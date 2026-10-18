// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **PM15-6d — redemption through the relayer** (plan D-10, spec H.2): a
//! market the arm holds a settled leg of is redeemed to pUSD by a Deposit
//! Wallet `WALLET` batch the owner signs and Polymarket's relayer executes
//! (gasless) — `CTF.setApprovalForAll(CtfCollateralAdapter, true)` first
//! in the boot's first batch, then the adapter's
//! `redeemPositions(pUSD, 0, condition, [1, 2])` per market (the adapter
//! redeems the wallet's whole balance of both outcomes).
//!
//! **Where it runs:** on pm-gw's idle path — no command waiting, no recon
//! due, the account read not in progress — one request per pass, and
//! never in a window's last [`crate::gw::QUIET_TAIL_S`] seconds: the late
//! law orders only inside them, so a redemption never stands between an
//! order and the wire. Its own keep-alive client talks to the relayer.
//!
//! **The cycle**, one request per step:
//!
//! 1. a market waits [`REDEEM_AFTER_NS`] after the arm asks (its roll
//!    settled on the venue; the chain's resolution can lag) — the queue
//!    is FIFO, so the ripe markets are its head;
//! 2. the wallet's nonce: `GET /v1/account/transactions/params`;
//! 3. up to [`REDEEM_BATCH`] markets signed into one batch (deadline now +
//!    [`DEADLINE_S`]) and `POST /submit`;
//! 4. its state, every [`POLL_GAP_NS`]: `GET /v1/account/transactions/<id>`.
//!    `STATE_MINED` / `STATE_CONFIRMED` redeem the batch's markets;
//!    `STATE_FAILED` / `STATE_INVALID`, a refused submit, or no terminal
//!    state in [`POLL_MAX`] polls cost each of its markets a try — a
//!    market is dropped after [`TRIES_MAX`] (`dropped`: redeem it by
//!    hand). A failed request, and every failed batch, waits
//!    [`REDEEM_EVERY_NS`].
//!
//! **Never a halt:** an unredeemed leg stays in the arm's ledger at its
//! cost (the conservative worth) and the account reads show it; no order
//! depends on it. Only the Deposit Wallet (signature type 3) is built.
//!
//! Zero allocation after [`Redeemer::new`]: the calldata, the body and
//! the targets are rendered in fixed buffers; the signature is a stack
//! array.

use core_net::{Header, Method};
use core_parse::{find_field, scan_u64, skip_ws};
use signer_eip712::enc_address;
use signer_eip712::pmv2::{
    deposit_batch_digest, deposit_call_hash, deposit_wallet_separator, eip55, sign,
    CONDITIONAL_TOKENS, CTF_COLLATERAL_ADAPTER, DEPOSIT_WALLET_FACTORY, PUSD,
};

use crate::account::PmAccount;
use crate::clob::PmClob;
use crate::num::{put, put_hex, put_u64};
use crate::render::is_plain_token;
use crate::response::string_field;

/// The relayer's host.
pub const RELAYER_HOST: &str = "relayer-v2.polymarket.com";
/// The relayer client's body window (a full batch is ≈ 5 KiB).
pub const RELAYER_BODY: usize = 8 * 1024;
/// The relayer client's answer buffer.
pub const RELAYER_RESP: usize = 16 * 1024;
/// Markets waiting.
pub const REDEEM_QUEUE: usize = 16;
/// Markets per batch.
pub const REDEEM_BATCH: usize = 8;
/// A market waits this long after the arm asks, ns.
pub const REDEEM_AFTER_NS: u64 = 180_000_000_000;
/// The wait after a failure, ns.
pub const REDEEM_EVERY_NS: u64 = 300_000_000_000;
/// Between two polls of a submitted batch, ns.
pub const POLL_GAP_NS: u64 = 2_000_000_000;
/// Polls before a batch is given up (3 min).
pub const POLL_MAX: u16 = 90;
/// Tries before a market is dropped.
pub const TRIES_MAX: u8 = 3;
/// The batch's deadline after its signing, s (the relayer's limit is 300).
pub const DEADLINE_S: u64 = 240;
/// `redeemPositions(address,bytes32,bytes32,uint256[])`'s calldata length.
pub const REDEEM_DATA_LEN: usize = 4 + 7 * 32;
/// `setApprovalForAll(address,bool)`'s calldata length.
pub const APPROVE_DATA_LEN: usize = 4 + 2 * 32;
/// The longest relayer API key taken.
pub const RELAYER_KEY_MAX: usize = 96;

const TX_ID_MAX: usize = 64;
const SEL_REDEEM: [u8; 4] = [0x01, 0xb7, 0x03, 0x7c];
const SEL_APPROVE_ALL: [u8; 4] = [0xa2, 0x2c, 0xb4, 0x65];
/// A full batch's body, at most: the fixed part (the keys, three
/// addresses, the nonce, the signature, the deadline), the approval call,
/// then every redemption call (each a target, a value and its hex).
const BATCH_BODY_MAX: usize = 200
    + 3 * 42
    + 20
    + 132
    + 20
    + (40 + 42 + 2 * APPROVE_DATA_LEN)
    + REDEEM_BATCH * (40 + 42 + 2 * REDEEM_DATA_LEN);
const _: () = assert!(
    BATCH_BODY_MAX <= RELAYER_BODY,
    "a full batch fits the relayer body window"
);

const P_IDLE: u8 = 0;
const P_NONCE: u8 = 1;
const P_SUBMIT: u8 = 2;
const P_POLL: u8 = 3;

/// The adapter's calldata redeeming `condition`:
/// `redeemPositions(pUSD, 0, condition, [1, 2])`.
#[must_use]
pub fn redeem_calldata(condition: &[u8; 32]) -> [u8; REDEEM_DATA_LEN] {
    let mut d = [0u8; REDEEM_DATA_LEN];
    put(&mut d, 0, &SEL_REDEEM);
    put(&mut d, 4, &enc_address(&PUSD));
    // 36..68: the parent collection, zero.
    put(&mut d, 68, condition);
    d[131] = 0x80; // the array's offset
    d[163] = 2; // its length
    d[195] = 1;
    d[227] = 2;
    d
}

/// The CTF's calldata approving the adapter for all of the wallet's
/// positions: `setApprovalForAll(CtfCollateralAdapter, true)`.
#[must_use]
pub fn approve_adapter_calldata() -> [u8; APPROVE_DATA_LEN] {
    let mut d = [0u8; APPROVE_DATA_LEN];
    put(&mut d, 0, &SEL_APPROVE_ALL);
    put(&mut d, 4, &enc_address(&CTF_COLLATERAL_ADAPTER));
    d[67] = 1;
    d
}

/// The relayer API key (`RELAYER_API_KEY`; its address is the owner's).
pub struct RelayerKey {
    b: [u8; RELAYER_KEY_MAX],
    len: usize,
}

impl RelayerKey {
    /// The key from its text — `None` when empty, too long or not a plain
    /// header token. Boot-only.
    #[must_use]
    pub fn new(text: &[u8]) -> Option<Self> {
        if text.is_empty() || text.len() > RELAYER_KEY_MAX || !is_plain_token(text) {
            return None;
        }
        let mut k = Self {
            b: [0; RELAYER_KEY_MAX],
            len: text.len(),
        };
        put(&mut k.b, 0, text);
        Some(k)
    }

    #[inline]
    fn as_bytes(&self) -> &[u8] {
        &self.b[..self.len]
    }
}

/// The redeemer's tallies.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RedeemCounters {
    /// Markets queued.
    pub queued: u64,
    /// Asks for a market already queued.
    pub dupes: u64,
    /// Asks refused: the queue was full.
    pub queue_full: u64,
    /// Batches submitted.
    pub batches: u64,
    /// Batches the chain executed.
    pub batches_done: u64,
    /// Markets redeemed.
    pub redeemed: u64,
    /// Batches failed (refused, failed on chain, or timed out).
    pub batches_failed: u64,
    /// Markets dropped after [`TRIES_MAX`] tries.
    pub dropped: u64,
    /// Requests that failed or did not read.
    pub request_failures: u64,
    /// Polls sent.
    pub polls: u64,
}

#[derive(Copy, Clone)]
struct Market {
    cond: [u8; 32],
    since_ns: u64,
    tries: u8,
}

const NO_MARKET: Market = Market {
    cond: [0; 32],
    since_ns: 0,
    tries: 0,
};

/// The redemption cycle (module docs).
pub struct Redeemer {
    key: RelayerKey,
    wallet: [u8; 20],
    sep: [u8; 32],
    factory_text: [u8; 42],
    adapter_text: [u8; 42],
    ctf_text: [u8; 42],
    q: [Market; REDEEM_QUEUE],
    n: usize,
    phase: u8,
    next_ns: u64,
    batch_n: usize,
    with_approve: bool,
    approved: bool,
    nonce: u64,
    tx: [u8; TX_ID_MAX],
    tx_len: usize,
    polls: u16,
    target: [u8; 96],
    counters: RedeemCounters,
}

impl Redeemer {
    /// The redeemer of `wallet` (the Deposit Wallet) on `chain_id`.
    /// Boot-only.
    #[must_use]
    pub fn new(key: RelayerKey, wallet: [u8; 20], chain_id: u64) -> Self {
        Self {
            key,
            wallet,
            sep: deposit_wallet_separator(chain_id, wallet),
            factory_text: eip55(&DEPOSIT_WALLET_FACTORY),
            adapter_text: eip55(&CTF_COLLATERAL_ADAPTER),
            ctf_text: eip55(&CONDITIONAL_TOKENS),
            q: [NO_MARKET; REDEEM_QUEUE],
            n: 0,
            phase: P_IDLE,
            next_ns: 0,
            batch_n: 0,
            with_approve: false,
            approved: false,
            nonce: 0,
            tx: [0; TX_ID_MAX],
            tx_len: 0,
            polls: 0,
            target: [0; 96],
            counters: RedeemCounters::default(),
        }
    }

    /// The tallies.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> RedeemCounters {
        self.counters
    }

    /// Markets waiting (in flight included).
    #[inline]
    #[must_use]
    pub const fn waiting(&self) -> usize {
        self.n
    }

    /// Queue `cond` for redemption at monotonic `now_ns`.
    pub fn ask(&mut self, cond: &[u8; 32], now_ns: u64) {
        let mut i = 0usize;
        while i < self.n {
            if self.q[i].cond == *cond {
                self.counters.dupes += 1;
                return;
            }
            i += 1;
        }
        if self.n == REDEEM_QUEUE {
            self.counters.queue_full += 1;
            return;
        }
        self.q[self.n] = Market {
            cond: *cond,
            since_ns: now_ns,
            tries: 0,
        };
        self.n += 1;
        self.counters.queued += 1;
    }

    /// Whether a step is due at `now_ns`.
    #[inline]
    #[must_use]
    pub fn due(&self, now_ns: u64) -> bool {
        if now_ns < self.next_ns {
            return false;
        }
        self.phase != P_IDLE
            || (self.n > 0 && now_ns >= self.q[0].since_ns.saturating_add(REDEEM_AFTER_NS))
    }

    /// One request of the cycle on `rly` (call when [`Redeemer::due`]).
    pub fn step<W: PmClob>(&mut self, rly: &mut W, acct: &PmAccount, now_ns: u64, wall_s: u64) {
        match self.phase {
            P_IDLE => {
                let mut k = 0usize;
                while k < self.n.min(REDEEM_BATCH)
                    && now_ns >= self.q[k].since_ns.saturating_add(REDEEM_AFTER_NS)
                {
                    k += 1;
                }
                if k == 0 {
                    return;
                }
                self.batch_n = k;
                self.with_approve = !self.approved;
                self.phase = P_NONCE;
                self.nonce_one(rly, acct, now_ns);
            }
            P_NONCE => self.nonce_one(rly, acct, now_ns),
            P_SUBMIT => self.submit_one(rly, acct, now_ns, wall_s),
            _ => self.poll_one(rly, acct, now_ns),
        }
    }

    fn nonce_one<W: PmClob>(&mut self, rly: &mut W, acct: &PmAccount, now_ns: u64) {
        let Ok(nonce) = wallet_nonce(rly, acct, &self.key) else {
            return self.wait_failed(now_ns);
        };
        self.nonce = nonce;
        self.phase = P_SUBMIT;
        self.next_ns = now_ns;
    }

    fn submit_one<W: PmClob>(&mut self, rly: &mut W, acct: &PmAccount, now_ns: u64, wall_s: u64) {
        let deadline = wall_s + DEADLINE_S;
        let mut data = [[0u8; REDEEM_DATA_LEN]; REDEEM_BATCH];
        let mut hashes = [[0u8; 32]; REDEEM_BATCH + 1];
        let approve = approve_adapter_calldata();
        let mut h = 0usize;
        if self.with_approve {
            hashes[0] = deposit_call_hash(&CONDITIONAL_TOKENS, 0, &approve);
            h = 1;
        }
        let mut i = 0usize;
        while i < self.batch_n {
            data[i] = redeem_calldata(&self.q[i].cond);
            hashes[h] = deposit_call_hash(&CTF_COLLATERAL_ADAPTER, 0, &data[i]);
            h += 1;
            i += 1;
        }
        let digest =
            deposit_batch_digest(&self.sep, &self.wallet, self.nonce, deadline, &hashes[..h]);
        let Ok(sig) = sign(&acct.sk, &digest) else {
            return self.batch_failed(now_ns);
        };
        let body = rly.body_mut();
        let mut at = put(body, 0, br#"{"type":"WALLET","from":""#);
        at = put(body, at, &acct.poly_address_text);
        at = put(body, at, br#"","to":""#);
        at = put(body, at, &self.factory_text);
        at = put(body, at, br#"","nonce":""#);
        at = put_u64(body, at, self.nonce);
        at = put(body, at, br#"","signature":"0x"#);
        at = put_hex(body, at, &sig);
        at = put(
            body,
            at,
            br#"","metadata":"Redeem position","depositWalletParams":{"depositWallet":""#,
        );
        at = put(body, at, &acct.maker_text);
        at = put(body, at, br#"","deadline":""#);
        at = put_u64(body, at, deadline);
        at = put(body, at, br#"","calls":["#);
        if self.with_approve {
            at = put_call(body, at, &self.ctf_text, &approve);
        }
        let mut i = 0usize;
        while i < self.batch_n {
            if i > 0 || self.with_approve {
                at = put(body, at, b",");
            }
            at = put_call(body, at, &self.adapter_text, &data[i]);
            i += 1;
        }
        at = put(body, at, b"]}}");
        debug_assert!(at <= BATCH_BODY_MAX, "the batch body's bound");
        self.counters.batches += 1;
        let res = rly.request(Method::Post, b"/submit", &headers(&self.key, acct), at);
        match res {
            Ok((200, range)) => match string_field(&rly.resp()[range], b"\"transactionID\":") {
                Some(id) if !id.is_empty() && id.len() <= TX_ID_MAX && is_plain_token(id) => {
                    self.tx_len = id.len();
                    put(&mut self.tx, 0, id);
                    self.polls = 0;
                    self.phase = P_POLL;
                    self.next_ns = now_ns.saturating_add(POLL_GAP_NS);
                }
                _ => {
                    self.counters.request_failures += 1;
                    self.batch_failed(now_ns);
                }
            },
            // The quota (25 submits a minute): nothing executed, no try.
            Ok((429, _)) => self.wait_failed(now_ns),
            // Refused (the payload, the auth) or no answer: a try of each
            // market.
            _ => {
                self.counters.request_failures += 1;
                self.batch_failed(now_ns);
            }
        }
    }

    fn poll_one<W: PmClob>(&mut self, rly: &mut W, acct: &PmAccount, now_ns: u64) {
        let mut at = put(&mut self.target, 0, b"/v1/account/transactions/");
        let tx_len = self.tx_len;
        at = put(&mut self.target, at, &self.tx[..tx_len]);
        self.counters.polls += 1;
        self.polls += 1;
        let res = rly.request(
            Method::Get,
            &self.target[..at],
            &headers(&self.key, acct),
            0,
        );
        let state = match res {
            Ok((200, range)) => string_field(&rly.resp()[range], b"\"state\":").map(tx_state),
            _ => None,
        };
        match state {
            Some(S_DONE) => {
                self.counters.batches_done += 1;
                self.counters.redeemed += self.batch_n as u64;
                self.approved |= self.with_approve;
                self.remove_head(self.batch_n);
                self.phase = P_IDLE;
                self.next_ns = now_ns;
            }
            Some(S_FAILED) => self.batch_failed(now_ns),
            Some(_) | None => {
                if state.is_none() {
                    self.counters.request_failures += 1;
                }
                if self.polls >= POLL_MAX {
                    self.batch_failed(now_ns);
                } else {
                    self.next_ns = now_ns.saturating_add(POLL_GAP_NS);
                }
            }
        }
    }

    /// A request before the submit failed: nothing was sent to the chain.
    fn wait_failed(&mut self, now_ns: u64) {
        self.counters.request_failures += 1;
        self.phase = P_IDLE;
        self.next_ns = now_ns.saturating_add(REDEEM_EVERY_NS);
    }

    /// The batch did not redeem: a try of each of its markets.
    fn batch_failed(&mut self, now_ns: u64) {
        self.counters.batches_failed += 1;
        let mut i = 0usize;
        let mut kept = 0usize;
        while i < self.n {
            let mut m = self.q[i];
            if i < self.batch_n {
                m.tries += 1;
            }
            if m.tries >= TRIES_MAX {
                self.counters.dropped += 1;
            } else {
                self.q[kept] = m;
                kept += 1;
            }
            i += 1;
        }
        self.n = kept;
        self.batch_n = 0;
        self.phase = P_IDLE;
        self.next_ns = now_ns.saturating_add(REDEEM_EVERY_NS);
    }

    fn remove_head(&mut self, k: usize) {
        let mut i = k;
        while i < self.n {
            self.q[i - k] = self.q[i];
            i += 1;
        }
        self.n -= k;
        self.batch_n = 0;
    }
}

const S_PENDING: u8 = 0;
const S_DONE: u8 = 1;
const S_FAILED: u8 = 2;

/// A relayer transaction's state: executed on chain, failed, or pending.
fn tx_state(s: &[u8]) -> u8 {
    match s {
        b"STATE_MINED" | b"STATE_CONFIRMED" => S_DONE,
        b"STATE_FAILED" | b"STATE_INVALID" => S_FAILED,
        _ => S_PENDING,
    }
}

/// The relayer's two auth lines: the key, and the owner's address.
fn headers<'a>(key: &'a RelayerKey, acct: &'a PmAccount) -> [Header<'a>; 2] {
    [
        (b"RELAYER_API_KEY", key.as_bytes()),
        (b"RELAYER_API_KEY_ADDRESS", &acct.poly_address_text),
    ]
}

/// The Deposit Wallet's relayer nonce — `GET /v1/account/transactions/
/// params?address=<owner>&type=WALLET` (the redeemer's step 2, and the
/// boot's proof that the relayer key authenticates).
///
/// # Errors
/// The answer's HTTP status, or 0 when it did not arrive or read.
pub fn wallet_nonce<W: PmClob>(
    rly: &mut W,
    acct: &PmAccount,
    key: &RelayerKey,
) -> Result<u64, u16> {
    let mut target = [0u8; 128];
    let mut at = put(&mut target, 0, b"/v1/account/transactions/params?address=");
    at = put(&mut target, at, &acct.poly_address_text);
    at = put(&mut target, at, b"&type=WALLET");
    match rly.request(Method::Get, &target[..at], &headers(key, acct), 0) {
        Ok((200, range)) => read_nonce(&rly.resp()[range]).ok_or(0),
        Ok((http, _)) => Err(http),
        Err(_) => Err(0),
    }
}

/// The wallet's nonce, quoted or bare.
fn read_nonce(body: &[u8]) -> Option<u64> {
    let mut p = skip_ws(body, find_field(body, b"\"nonce\":")?);
    let quoted = p < body.len() && body[p] == b'"';
    if quoted {
        p += 1;
    }
    let (v, end) = scan_u64(body, p)?;
    if end == p || (quoted && (end >= body.len() || body[end] != b'"')) {
        return None;
    }
    Some(v)
}

/// One call's JSON: `{"target":"…","value":"0","data":"0x…"}`.
fn put_call(body: &mut [u8], at: usize, target: &[u8; 42], data: &[u8]) -> usize {
    let mut at = put(body, at, br#"{"target":""#);
    at = put(body, at, target);
    at = put(body, at, br#"","value":"0","data":"0x"#);
    at = put_hex(body, at, data);
    put(body, at, br#""}"#)
}

#[cfg(test)]
mod tests;
