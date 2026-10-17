// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! pm-gw — the order thread's core (PM15-6b, plan §5–§6), socket-free:
//! [`PmGateway::step`] is one pass; the loop that calls it only moves time.
//!
//! **A submit** (`CMD_SUBMIT`): the window's row ([`crate::tokens`]) → the
//! amounts law (D-3) and the market's floors → the V2 order, signed (6a) →
//! the body in the request's own window: the token's prefix, then the
//! order's tail rendered in place after it → the L2 MAC over
//! `ts ‖ POST ‖ /order ‖ body` → **SENT** (before the request's first
//! byte: a trade can only exist after the venue read it) → the blocking
//! request → the answer, read fail-closed → ACK / REFUSED / IN_DOUBT.
//!
//! **The retry law (D-12):** a request that failed before any answer, or
//! answered `order timed out` (rejected before the book), is sent once more
//! with the same bytes — the same salt and timestamp, so a copy the venue
//! already took is refused `Duplicated` and never fills twice; a second
//! failure, or that `Duplicated`, is IN_DOUBT.
//!
//! **Recon** (`CMD_RECON`, the arm's, at an order's deadline or after
//! IN_DOUBT or a channel GAP): `GET /data/trades?market=<condition>&after=`
//! — the account's own trades — paged; every TAKER trade of the order is a
//! FILL event (the arm books a trade id once), then DONE. At most one
//! request per [`RECON_GAP_NS`]; a failed read is retried up to
//! [`RECON_TRIES_MAX`] times, then DONE with `b = -1` and the halt.
//!
//! **Halts** (the kit's halt word, the first reason wins): an answer whose
//! `orderID` is not the digest signed (D-5), a refusal that names the
//! account or the protocol (`Version`, `Auth`, `Payload`, `Closed`), recon
//! given up. A halted gateway refuses every submit before the wire.
//!
//! Zero allocation after [`PmGateway::new`]: the body is rendered in the
//! client's window, the signature into a fixed buffer, recon's page into a
//! boxed record table.

use core_net::{Header, Method, PostErr};
use core_ring::Consumer;
use exec_gateway::stats::HALT_VENUE_BASE;
use exec_gateway::GatewayEnd;
use ingress_polymarket::gamma15::PmWindowSpec;
use ingress_polymarket::pm15_loop::PM15_SPEC_RING;
use signer_eip712::pmv2::{OrderV2, SIDE_BUY};

use crate::account::PmAccount;
use crate::amounts::{clears_floors, fak_buy};
use crate::clob::PmClob;
use crate::cmd::{
    PmCmd, PmEvt, CMD_RECON, CMD_SUBMIT, EVT_ACK, EVT_DONE, EVT_IN_DOUBT, EVT_REFUSED, EVT_SENT,
    LOCAL_AMOUNTS, LOCAL_FLOOR, LOCAL_NO_TOKEN, LOCAL_REQUEST, LOCAL_SIGN, PM_CMD_RING,
    PM_EVT_RING,
};
use crate::num::{put, put_u64};
use crate::render::{render_order_tail, render_ts, PREFIX_MAX, TAIL_MAX};
use crate::response::{read_order_answer, Refusal, Verdict};
use crate::sign::{sign_order, SIG_MAX};
use crate::tokens::PmTokens;
use crate::userws::{fnv1a, scan_trades_page, UserEvt, KIND_TRADE, ROLE_MAKER};

/// The gateway's end of the engine duplex.
pub type PmGwEnd = GatewayEnd<PmCmd, PmEvt, PM_CMD_RING, PM_EVT_RING>;

/// The least time between two recon requests, ns.
pub const RECON_GAP_NS: u64 = 2_000_000_000;
/// Recon attempts of one order before the gateway gives up and halts.
pub const RECON_TRIES_MAX: u8 = 30;
/// Recon reads the trades of this far back, s (a FAK's trades exist
/// within a second of its send; the arm asks within a minute).
pub const RECON_LOOKBACK_S: u64 = 300;
/// Recon commands held while they wait for their turn.
pub const RECON_QUEUE: usize = 16;
/// Pages one recon follows (`next_cursor`).
pub const RECON_PAGES_MAX: usize = 4;
/// Trades one page may carry (the venue's default page is 100).
pub const RECON_PAGE_TRADES: usize = 128;
/// The body window a client must offer: the longest prefix and tail.
pub const BODY_MIN: usize = PREFIX_MAX + TAIL_MAX;

/// Halt: an answer's `orderID` is not the digest signed (D-5).
pub const HALT_ORDER_ID: u64 = HALT_VENUE_BASE + 1;
/// Halt: a refusal that names the account or the protocol.
pub const HALT_REFUSAL: u64 = HALT_VENUE_BASE + 2;
/// Halt: recon could not read an order's trades.
pub const HALT_RECON: u64 = HALT_VENUE_BASE + 3;
/// Halt: the event ring had no room for an event (also the kit's own).
pub const HALT_EMIT: u64 = HALT_VENUE_BASE + 4;

/// The gateway's tallies (read by its owner; the cli publishes them).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GwCounters {
    /// Windows taken into the token table.
    pub specs: u64,
    /// Windows refused by the table.
    pub specs_refused: u64,
    /// Orders written to the wire (first attempts).
    pub posts: u64,
    /// Second attempts (D-12).
    pub resends: u64,
    /// Answers accepted.
    pub acks: u64,
    /// Answers refused by the venue.
    pub refused: u64,
    /// Refusals made before the wire.
    pub refused_local: u64,
    /// Orders left in doubt.
    pub in_doubt: u64,
    /// Recon requests sent.
    pub recon_requests: u64,
    /// Recon requests that failed.
    pub recon_failures: u64,
    /// Recon fills emitted.
    pub recon_fills: u64,
    /// Recons finished.
    pub recon_done: u64,
}

/// pm-gw's core (module docs).
pub struct PmGateway<W: PmClob> {
    acct: PmAccount,
    clob: W,
    tokens: PmTokens,
    end: PmGwEnd,
    specs: Consumer<PmWindowSpec, PM15_SPEC_RING>,
    sig: [u8; SIG_MAX],
    salt: u64,
    recon_q: [PmCmd; RECON_QUEUE],
    recon_n: usize,
    recon_next_ns: u64,
    page: Box<[UserEvt; RECON_PAGE_TRADES]>,
    target: [u8; 256],
    halted: bool,
    counters: GwCounters,
}

impl<W: PmClob> PmGateway<W> {
    /// **Boot-only** (the token table and the recon page are boxed here).
    /// `seed` seeds the salt sequence (any value; 0 is mapped away).
    ///
    /// # Panics
    /// When `clob`'s body window is under [`BODY_MIN`] (a boot sizing
    /// bug).
    pub fn new(
        acct: PmAccount,
        mut clob: W,
        end: PmGwEnd,
        specs: Consumer<PmWindowSpec, PM15_SPEC_RING>,
        seed: u64,
    ) -> Self {
        assert!(clob.body_mut().len() >= BODY_MIN, "pm-gw: the body window is too small");
        Self {
            acct,
            clob,
            tokens: PmTokens::new(),
            end,
            specs,
            sig: [0; SIG_MAX],
            salt: seed | 1,
            recon_q: [PmCmd::new(0, 0); RECON_QUEUE],
            recon_n: 0,
            recon_next_ns: 0,
            page: exec_gateway::boxed_array(UserEvt::BLANK),
            target: [0; 256],
            halted: false,
            counters: GwCounters::default(),
        }
    }

    /// The tallies.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> GwCounters {
        self.counters
    }

    /// Whether the gateway refuses every submit.
    #[inline]
    #[must_use]
    pub const fn halted(&self) -> bool {
        self.halted
    }

    /// The token table (tests, the cli's tells).
    #[inline]
    #[must_use]
    pub fn tokens(&self) -> &PmTokens {
        &self.tokens
    }

    /// The client (tests: the rig's script).
    #[inline]
    pub fn clob_mut(&mut self) -> &mut W {
        &mut self.clob
    }

    /// One pass at monotonic `now_ns` and wall `wall_ns`: the windows
    /// discovery found, at most one command, at most one recon request.
    /// `true` when it did something.
    pub fn step(&mut self, now_ns: u64, wall_ns: u64) -> bool {
        let mut did = false;
        while let Some(spec) = self.specs.try_pop_ref() {
            match self.tokens.offer(&spec, &self.acct) {
                Ok(()) => self.counters.specs += 1,
                Err(_) => self.counters.specs_refused += 1,
            }
            did = true;
        }
        // Back-pressure: a command may emit a few events and a recon one
        // per trade; a full event ring would lose a fill (XG-2).
        if self.end.evt.room() >= RECON_PAGE_TRADES / 2 {
            // COPY: the 64 B command off the ring — its fields are read
            // across the blocking request below (the guard is released at
            // once).
            let next = self.end.cmd.pop().map(|c| *c);
            if let Some(cmd) = next {
                did = true;
                match cmd.kind {
                    CMD_SUBMIT => self.submit(&cmd, wall_ns),
                    CMD_RECON => self.queue_recon(cmd),
                    _ => {}
                }
            }
            if self.recon_n > 0 && now_ns >= self.recon_next_ns {
                self.recon_next_ns = now_ns.saturating_add(RECON_GAP_NS);
                self.recon_one(wall_ns);
                did = true;
            }
        }
        did
    }

    fn emit(&mut self, e: &PmEvt) {
        if !self.end.evt.emit(e) {
            self.halt(HALT_EMIT);
        }
    }

    fn halt(&mut self, reason: u64) {
        self.halted = true;
        self.end.stats.halt(reason);
    }

    fn refuse_local(&mut self, client_oid: u64, why: u8) {
        self.counters.refused_local += 1;
        let mut e = PmEvt::about(EVT_REFUSED, client_oid);
        e.why = why;
        self.emit(&e);
    }

    #[inline]
    fn next_salt(&mut self) -> u64 {
        // xorshift64 — uniqueness, not secrecy (spec §C.5); ≤ 2^53 − 1.
        let mut x = self.salt;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.salt = x;
        let s = x & ((1u64 << 53) - 1);
        s | u64::from(s == 0)
    }

    /// The L2 MAC (`POLY_SIGNATURE`) over `ts ‖ method ‖ path ‖ body`.
    fn l2_mac(
        &self,
        ts: &[u8],
        method: &[u8],
        path: &[u8],
        body: &[u8],
    ) -> [u8; crate::l2::L2_SIG_LEN] {
        self.acct.l2.sign(&[ts, method, path, body])
    }

    fn submit(&mut self, cmd: &PmCmd, wall_ns: u64) {
        let cid = cmd.client_oid;
        if self.halted {
            return self.refuse_local(cid, LOCAL_REQUEST);
        }
        let leg = usize::from(cmd.leg & 1);
        let Some(row) = self.tokens.find(cmd.instance) else {
            return self.refuse_local(cid, LOCAL_NO_TOKEN);
        };
        let min_size = row.min_size_1e6.max(0) as u64;
        let token_id = row.legs[leg].id_be;
        let plen = row.legs[leg].prefix_len;
        let a = match fak_buy(
            u64::from(cmd.usd_1e6),
            u64::from(cmd.px_1e6),
            u64::from(cmd.tick_1e6),
        ) {
            Ok(a) => a,
            Err(_) => return self.refuse_local(cid, LOCAL_AMOUNTS),
        };
        if !clears_floors(&a, min_size) {
            return self.refuse_local(cid, LOCAL_FLOOR);
        }
        let salt = self.next_salt();
        let ts_ms = wall_ns / 1_000_000;
        let o = OrderV2 {
            salt,
            maker: self.acct.maker,
            signer: self.acct.signer,
            token_id,
            maker_amount: a.maker,
            taker_amount: a.taker,
            timestamp_ms: ts_ms,
            metadata: [0; 32],
            builder: [0; 32],
            side: SIDE_BUY,
            signature_type: self.acct.sig_type,
            _pad: [0; 6],
        };
        let Ok(signed) = sign_order(
            &self.acct.sk,
            &self.acct.exchange_sep,
            self.acct.chain_id,
            &o,
            &mut self.sig,
        ) else {
            return self.refuse_local(cid, LOCAL_SIGN);
        };
        // The body, in place: the token's prefix, then the tail after it.
        let body_len = {
            let Some(row) = self.tokens.find(cmd.instance) else {
                return self.refuse_local(cid, LOCAL_NO_TOKEN);
            };
            let b = self.clob.body_mut();
            // COPY: the token's prefix (≤ 422 B) into the request window,
            // per order — the rejected alternative, prefix and tail as two
            // writes, seals two TLS records (plan §5, "The request").
            b[..plen].copy_from_slice(&row.legs[leg].prefix[..plen]);
            let Ok(tail) = <&mut [u8; TAIL_MAX]>::try_from(&mut b[plen..plen + TAIL_MAX]) else {
                return self.refuse_local(cid, LOCAL_REQUEST);
            };
            plen + render_order_tail(
                salt,
                &a,
                ts_ms,
                &self.sig[..signed.sig_len],
                self.acct.api_key(),
                tail,
            )
        };
        let mut ts = [0u8; 20];
        let tlen = render_ts(wall_ns / 1_000_000_000, &mut ts);
        let mac = {
            let body = &self.clob.body_mut()[..body_len];
            self.acct.l2.sign(&[&ts[..tlen], b"POST", b"/order", body])
        };
        let mut sent = PmEvt::about(EVT_SENT, cid);
        sent.order_id = signed.order_id;
        self.emit(&sent);
        self.counters.posts += 1;
        let mut attempt = 0u8;
        loop {
            let headers: [Header<'_>; 5] = [
                (b"POLY_ADDRESS", &self.acct.poly_address_text),
                (b"POLY_API_KEY", self.acct.api_key()),
                (b"POLY_PASSPHRASE", self.acct.passphrase()),
                (b"POLY_SIGNATURE", &mac),
                (b"POLY_TIMESTAMP", &ts[..tlen]),
            ];
            let res = self.clob.request(Method::Post, b"/order", &headers, body_len);
            let retry = match res {
                Ok((http, range)) => {
                    let ans = read_order_answer(http, &self.clob.resp()[range]);
                    if ans.order_id != [0; 32] && ans.order_id != signed.order_id {
                        // D-5: the venue's id is not the digest signed — the
                        // in-flight law is void; the order may exist.
                        self.halt(HALT_ORDER_ID);
                        self.in_doubt(cid, signed.order_id);
                        return;
                    }
                    match ans.verdict {
                        Verdict::Accepted => {
                            self.counters.acks += 1;
                            let mut e = PmEvt::about(EVT_ACK, cid);
                            e.order_id = signed.order_id;
                            e.b = i64::from(ans.trades);
                            e.why = ans.status as u8;
                            self.emit(&e);
                            return;
                        }
                        Verdict::Refused if ans.refusal == Refusal::TimedOut && attempt == 0 => true,
                        Verdict::Refused if ans.refusal == Refusal::Duplicated => {
                            // The first copy's fate stands: recon reads it.
                            self.in_doubt(cid, signed.order_id);
                            return;
                        }
                        Verdict::Refused => {
                            self.counters.refused += 1;
                            if matches!(
                                ans.refusal,
                                Refusal::Version | Refusal::Auth | Refusal::Payload | Refusal::Closed
                            ) {
                                self.halt(HALT_REFUSAL);
                            }
                            let mut e = PmEvt::about(EVT_REFUSED, cid);
                            e.order_id = signed.order_id;
                            e.why = ans.refusal as u8;
                            e.b = i64::from(http);
                            e.c = i64::from(ans.retry_after_s);
                            self.emit(&e);
                            return;
                        }
                        Verdict::InDoubt => {
                            self.in_doubt(cid, signed.order_id);
                            return;
                        }
                    }
                }
                Err(PostErr { .. }) => attempt == 0,
            };
            if !retry {
                self.in_doubt(cid, signed.order_id);
                return;
            }
            attempt += 1;
            self.counters.resends += 1;
        }
    }

    fn in_doubt(&mut self, cid: u64, order_id: [u8; 32]) {
        self.counters.in_doubt += 1;
        let mut e = PmEvt::about(EVT_IN_DOUBT, cid);
        e.order_id = order_id;
        self.emit(&e);
    }

    fn queue_recon(&mut self, cmd: PmCmd) {
        if self.recon_n >= RECON_QUEUE {
            // More recons than orders can be open: a sizing bug.
            self.halt(HALT_RECON);
            let mut e = PmEvt::about(EVT_DONE, cmd.client_oid);
            e.b = -1;
            self.emit(&e);
            return;
        }
        self.recon_q[self.recon_n] = cmd;
        self.recon_n += 1;
    }

    /// The oldest queued recon: read the account's trades of its market,
    /// emit every TAKER trade of the order, then DONE — or, on a failed
    /// read, back to the queue's end.
    fn recon_one(&mut self, wall_ns: u64) {
        let cmd = self.recon_q[0];
        let mut k = 1usize;
        while k < self.recon_n {
            self.recon_q[k - 1] = self.recon_q[k];
            k += 1;
        }
        self.recon_n -= 1;
        let cid = cmd.client_oid;
        let Some(row) = self.tokens.find(cmd.instance) else {
            // The window left the table (two windows on): its trades are
            // unreadable here — fail closed.
            self.halt(HALT_RECON);
            let mut e = PmEvt::about(EVT_DONE, cid);
            e.b = -1;
            return self.emit(&e);
        };
        let mut cond = [0u8; 66];
        let cond_len = row.condition_len.min(66);
        // COPY: the condition id (66 B) out of the table for the query's
        // render — once per recon request, cold.
        cond[..cond_len].copy_from_slice(&row.condition_id[..cond_len]);
        let after = (wall_ns / 1_000_000_000).saturating_sub(RECON_LOOKBACK_S);
        let mut cursor = [0u8; 64];
        let mut cursor_len = 0usize;
        let mut found = 0i64;
        let mut page = 0usize;
        while page < RECON_PAGES_MAX {
            page += 1;
            let mut at = put(&mut self.target, 0, b"/data/trades?market=");
            at = put(&mut self.target, at, &cond[..cond_len]);
            at = put(&mut self.target, at, b"&after=");
            at = put_u64(&mut self.target, at, after);
            if cursor_len > 0 {
                at = put(&mut self.target, at, b"&next_cursor=");
                at = put(&mut self.target, at, &cursor[..cursor_len]);
            }
            let tlen_target = at;
            let mut ts = [0u8; 20];
            let tlen = render_ts(wall_ns / 1_000_000_000, &mut ts);
            // The query is not signed (spec §B.1): the path only.
            let mac = self.l2_mac(&ts[..tlen], b"GET", b"/data/trades", b"");
            self.counters.recon_requests += 1;
            let headers: [Header<'_>; 5] = [
                (b"POLY_ADDRESS", &self.acct.poly_address_text),
                (b"POLY_API_KEY", self.acct.api_key()),
                (b"POLY_PASSPHRASE", self.acct.passphrase()),
                (b"POLY_SIGNATURE", &mac),
                (b"POLY_TIMESTAMP", &ts[..tlen]),
            ];
            let target = &self.target[..tlen_target];
            let res = self.clob.request(Method::Get, target, &headers, 0);
            let range = match res {
                Ok((200, range)) => range,
                _ => return self.recon_failed(cmd),
            };
            let scanned = scan_trades_page(&self.clob.resp()[range.clone()], &mut self.page[..]);
            let Ok((s, next)) = scanned else {
                return self.recon_failed(cmd);
            };
            let mut i = 0usize;
            while i < s.events {
                let t = self.page[i];
                if t.kind == KIND_TRADE && t.role != ROLE_MAKER && t.order_id == cmd.order_id {
                    let key = fnv1a(&t.trade_id[..usize::from(t.trade_id_len)]);
                    let e = PmEvt::fill(t.order_id, key, t.price_1e6, t.size_1e6, t.status);
                    self.emit(&e);
                    self.counters.recon_fills += 1;
                    found += 1;
                }
                i += 1;
            }
            match next {
                Some((cs, ce)) if ce > cs && ce - cs <= cursor.len() => {
                    let resp = self.clob.resp();
                    let c = &resp[range.start + cs..range.start + ce];
                    // COPY: the next page's cursor (≤ 64 B) out of the
                    // answer before the next request reuses its buffer.
                    cursor[..c.len()].copy_from_slice(c);
                    cursor_len = c.len();
                }
                _ => break,
            }
        }
        self.counters.recon_done += 1;
        let mut e = PmEvt::about(EVT_DONE, cid);
        e.order_id = cmd.order_id;
        e.b = found;
        self.emit(&e);
    }

    fn recon_failed(&mut self, mut cmd: PmCmd) {
        self.counters.recon_failures += 1;
        cmd.tries = cmd.tries.saturating_add(1);
        if cmd.tries >= RECON_TRIES_MAX {
            self.halt(HALT_RECON);
            let mut e = PmEvt::about(EVT_DONE, cmd.client_oid);
            e.order_id = cmd.order_id;
            e.b = -1;
            return self.emit(&e);
        }
        self.queue_recon(cmd);
    }
}

#[cfg(test)]
mod tests;
