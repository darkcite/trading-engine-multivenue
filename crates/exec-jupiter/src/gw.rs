// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # gw — jup-gw, slot 11's order thread, socket-free (PMJ J3)
//!
//! [`JupGateway::step`] is one pass; the loop that calls it only moves
//! time. One request per pass, so a pass never blocks for more than one
//! round trip — except an order's build and its execute, which run back
//! to back (the transaction's blockhash ages between them).
//!
//! **An order** ([`INTENT_BUY`]): its window's Gamma market id from the
//! table the 5 m discovery tees in → `POST /orders` (the body rendered in
//! the client's window) → the answer scanned in place
//! ([`crate::json::scan_build`]): a keeper order, `requiredSigners`
//! exactly our wallet, the server's `maxBuyPriceUsd` ≤ the member's limit
//! → the base64 transaction decoded into the fixed 1 232 B buffer → the
//! **guard** ([`crate::tx::guard`], mandatory: an empty allowlist refuses
//! everything) → our signature written into its slot →
//! `POST /execute` with the build's `context` → LANDED / REFUSED /
//! IN_DOUBT. A build creates nothing on chain, so every failure before the
//! execute is a refusal; an execute that may have left the host and did
//! not answer readably is IN_DOUBT (its blockhash still lets it land).
//!
//! **After the land**: `GET /orders/status/{orderPubkey}` every
//! [`POLL_EVERY_NS`] until it is terminal or the deadline (the window's
//! close + [`DEADLINE_AFTER_CLOSE_NS`]) passes, then **recon**: the
//! account's `GET /orders` pages, the order's `filledContracts` and
//! `avgFillPriceUsd` → FILL (cumulative) and DONE. An in-doubt order is
//! polled the same way: a status answer means it landed; recon not
//! finding it after the deadline means it never did (DONE, `aux` 1). A
//! landed order recon cannot read for [`RECON_TRIES_MAX`] tries is LOST
//! and the gateway halts (the operator reconciles).
//!
//! **E-5 for this venue**: the execute's `Success` is the ACK (LANDED);
//! the order record's fill numbers are the FILL. The venue reports no fee
//! per fill: the build's estimate rides the events, the position's
//! `feesPaidUsd` is J5's measure.
//!
//! **Idle work** (no order building, no row due): the wallet's balances
//! every [`BALANCE_EVERY_NS`] (the USDC and SOL gauges, the arm's worth);
//! the positions every [`POSITIONS_EVERY_NS`] (or at the arm's
//! [`INTENT_CLAIM`]), and each claimable one's claim — built, guarded
//! (no token may leave our account) and signed, then held: **the claim's
//! submission route is unverified until J0 records one** (`claims_held`).
//!
//! **Halts** (the kit's halt word, the first reason wins): a region
//! refusal, an authentication refusal, an order recon lost, a lost event,
//! and the operator's (`exec.HALT` on slot 11 reaches the arm's
//! `cancel_slot`). A halted gateway refuses every build before the wire —
//! a landed order is still polled and reconciled: it cannot be pulled.
//!
//! Zero allocation after [`JupGateway::new`]: every body is rendered in
//! the client's window, the transaction and the context live in fixed
//! buffers, the tables are fixed arrays.

use core_crypto::{base58_decode, base58_encode, base64_decode};
use core_net::{Header, Method, PostErr};
use core_ring::Consumer;
use core_types::pm_roll::pm5_instance_id;
use exec_gateway::stats::HALT_VENUE_BASE;
use exec_gateway::{GatewayEnd, HALT_NONE};
use ingress_polymarket::gamma15::PmWindowSpec;
use ingress_polymarket::pm15_loop::PM15_SPEC_RING;
use signer_ed25519::Ed25519Signer;

use crate::client::JupClient;
use crate::cmd::{
    reason, JupEvent, JupIntent, EVT_BALANCE, EVT_CLAIMED, EVT_CLAIM_READY, EVT_DONE, EVT_FILL,
    EVT_IN_DOUBT, EVT_LANDED, EVT_LOST, EVT_REFUSED, INTENT_BUY, INTENT_CLAIM, JUP_CMD_RING,
    JUP_EVT_RING,
};
use crate::held::{render_held, HeldLeg, HELD_CLAIM_READY, HELD_FILE_MAX, HELD_MAX, HELD_OPEN};
use crate::json::{
    scan_balances, scan_build, scan_claim, scan_error, scan_execute, scan_orders, scan_positions,
    scan_status, Balances, BuildAnswer, ClaimAnswer, ErrKind, ErrorAnswer, ExecStatus,
    ExecuteAnswer, OrderRecord, OrderState, StatusAnswer,
};
use crate::render::{
    claim_body, claim_path, execute_body, order_body, status_path, Outcome, B58_KEY_MAX,
    CONTEXT_MAX, EXECUTE_BODY_MAX, PATH_MAX,
};
use crate::stats::{word, JupExecStats, GW_WORDS, PUBLISH_EVERY_NS};
use crate::tx::{guard, SolTx, TxErr, TxGuard, KEY_LEN, TX_MAX};

/// The gateway's end of the engine duplex.
pub type JupGwEnd = GatewayEnd<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>;

/// Orders tracked at once (landed and polled, or in recon).
pub const GW_ROWS: usize = 16;
/// Windows the table holds: 8 families × the current window and two
/// ahead, with room.
pub const MARKETS_MAX: usize = 32;
/// The status poll's cadence, ns.
pub const POLL_EVERY_NS: u64 = 250_000_000;
/// The poll's deadline after the window's close, ns.
pub const DEADLINE_AFTER_CLOSE_NS: u64 = 30_000_000_000;
/// The least time between two recon requests of one order, ns.
pub const RECON_GAP_NS: u64 = 2_000_000_000;
/// Recon attempts before a landed order is LOST (and the gateway halts).
pub const RECON_TRIES_MAX: u8 = 30;
/// Pages one recon follows.
pub const RECON_PAGES_MAX: u32 = 4;
/// Orders a recon page asks for.
pub const RECON_PAGE: u32 = 50;
/// How often the balances are read, ns.
pub const BALANCE_EVERY_NS: u64 = 30_000_000_000;
/// How often the positions are read, ns.
pub const POSITIONS_EVERY_NS: u64 = 60_000_000_000;
/// Claims queued (or held, built and signed) at once.
pub const CLAIMS_MAX: usize = 8;
/// Event slots a pass may need (a refusal, or a fill and its done).
pub const EVT_ROOM: usize = 4;
/// The client's body window must hold the largest body.
pub const BODY_MIN: usize = EXECUTE_BODY_MAX;
/// The `x-api-key` value's bound (`core_net`'s header value bound).
pub const API_KEY_MAX: usize = 128;
/// The default lead a build needs before its window's end, ns.
pub const MIN_LEAD_NS_DEFAULT: u64 = 1_000_000_000;
/// The venue's least deposit, USDC micro units ($5).
pub const MIN_DEPOSIT_MICRO: u64 = 5_000_000;

/// Halt: the venue refused this account's region.
pub const HALT_REGION: u64 = HALT_VENUE_BASE + 1;
/// Halt: the venue refused the API key or the account.
pub const HALT_AUTH: u64 = HALT_VENUE_BASE + 2;
/// Halt: recon lost a landed order.
pub const HALT_RECON: u64 = HALT_VENUE_BASE + 3;
/// Halt: an event could not be written (also the kit's own).
pub const HALT_EMIT: u64 = HALT_VENUE_BASE + 4;
/// Halt: the operator's (`exec.HALT` on the slot, via the arm).
pub const HALT_OPERATOR: u64 = HALT_VENUE_BASE + 5;
/// Halt: the arm's own (an event of no order, a full queue or ledger, a
/// fill price off the binary law).
pub const HALT_ARM: u64 = HALT_VENUE_BASE + 6;

const P_ORDERS: &[u8] = b"/prediction/v1/orders";
const P_EXECUTE: &[u8] = b"/prediction/v1/execute";
const P_ORDERS_Q: &[u8] = b"/prediction/v1/orders?ownerPubkey=";
const P_POSITIONS_Q: &[u8] = b"/prediction/v1/positions?ownerPubkey=";
const P_BALANCES: &[u8] = b"/ultra/v1/balances/";
/// A query target's bound.
const QUERY_MAX: usize = 160;

const R_FREE: u8 = 0;
/// Landed: polled.
const R_POLL: u8 = 1;
/// Sent, unanswered: polled until it shows.
const R_DOUBT: u8 = 2;
/// Terminal, or past the deadline: recon reads its numbers.
const R_RECON: u8 = 3;

/// jup-gw's configuration (boot).
#[derive(Copy, Clone, Debug)]
pub struct JupGwCfg {
    /// The wallet's public key (the fee payer, the owner).
    pub wallet: [u8; KEY_LEN],
    /// The deposit mint (USDC).
    pub usdc_mint: [u8; KEY_LEN],
    /// The transaction guard (the `pmj.toml` `guard_*` keys).
    pub guard: TxGuard,
    /// The per-order maximum deposit, USDC micro units.
    pub max_order_micro: u64,
    /// The lead a build needs before its window's end, ns.
    pub min_lead_ns: u64,
}

/// One market window jup-gw can build for.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Market {
    instance: u32,
    market_id: u64,
}

/// One order after its execute.
#[derive(Copy, Clone, Debug)]
struct Row {
    client_oid: u64,
    order_key: [u8; KEY_LEN],
    position_key: [u8; KEY_LEN],
    deadline_ns: u64,
    next_ns: u64,
    sent_ns: u64,
    filled_micro: u64,
    fee_micro: u64,
    cost_micro: u64,
    instance: u32,
    outcome: u8,
    state: u8,
    tries: u8,
    landed: bool,
}

const FREE_ROW: Row = Row {
    client_oid: 0,
    order_key: [0; KEY_LEN],
    position_key: [0; KEY_LEN],
    deadline_ns: 0,
    next_ns: 0,
    sent_ns: 0,
    filled_micro: 0,
    fee_micro: 0,
    cost_micro: 0,
    instance: 0,
    outcome: 0,
    state: R_FREE,
    tries: 0,
    landed: false,
};

/// A claimable position.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Claim {
    position: [u8; KEY_LEN],
    payout_micro: u64,
    /// 0 queued, 1 built and held.
    state: u8,
}

/// jup-gw's tallies.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GwCounters {
    /// Windows taken into the table.
    pub specs: u64,
    /// Windows the table refused (no instance, full).
    pub specs_refused: u64,
    /// Builds sent.
    pub builds: u64,
    /// Builds that answered a transaction.
    pub built: u64,
    /// Transactions the guard refused.
    pub guard_refused: u64,
    /// Builds refused on the server's price cap.
    pub price_refused: u64,
    /// Executes sent.
    pub executes: u64,
    /// Orders landed.
    pub landed: u64,
    /// Executes answered `Failed`.
    pub exec_failed: u64,
    /// Orders left in doubt.
    pub in_doubt: u64,
    /// Refusals made by jup-gw (any reason).
    pub refused: u64,
    /// Refusals the venue answered.
    pub venue_refused: u64,
    /// Region refusals.
    pub region_refused: u64,
    /// Status polls sent.
    pub polls: u64,
    /// Status polls that failed.
    pub poll_failures: u64,
    /// Recon requests sent.
    pub recons: u64,
    /// Recon requests that failed or did not find the order.
    pub recon_misses: u64,
    /// Fill events.
    pub fills: u64,
    /// Orders done.
    pub done: u64,
    /// Orders lost (recon gave up).
    pub lost: u64,
    /// Balance reads.
    pub balance_reads: u64,
    /// Balance reads that failed.
    pub balance_failures: u64,
    /// Position reads.
    pub position_reads: u64,
    /// Position reads that failed.
    pub position_failures: u64,
    /// Claims built, guarded and signed (held: the route is unverified).
    pub claims_held: u64,
    /// Claims refused (the build, the guard).
    pub claims_refused: u64,
    /// Held-file renders handed to the owner.
    pub held_writes: u64,
    /// Legs the held table could not hold.
    pub held_full: u64,
}

/// Why a prepared build was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Refusal {
    why: u8,
    aux: u64,
    halt: u64,
}

const fn refusal(why: u8, aux: u64) -> Refusal {
    Refusal { why, aux, halt: HALT_NONE }
}

/// What a build that passed brings to the execute.
#[derive(Copy, Clone, Debug)]
struct Built {
    tx_len: usize,
    ctx_len: usize,
    order_key: [u8; KEY_LEN],
    position_key: [u8; KEY_LEN],
    fee_micro: u64,
    cost_micro: u64,
}

/// A refusal read off a non-200 answer: the region and the key halt.
fn venue_refusal(http: u16, body: &[u8]) -> Refusal {
    let mut e = ErrorAnswer::default();
    let kind = if scan_error(body, &mut e).is_ok() {
        e.kind
    } else {
        ErrKind::Unknown
    };
    match kind {
        ErrKind::Region => Refusal {
            why: reason::REGION,
            aux: u64::from(http),
            halt: HALT_REGION,
        },
        ErrKind::Auth => Refusal {
            why: reason::VENUE,
            aux: (kind as u64) << 16 | u64::from(http),
            halt: HALT_AUTH,
        },
        _ => refusal(reason::VENUE, (kind as u64) << 16 | u64::from(http)),
    }
}

/// A base58 key of exactly 32 bytes.
fn key_of(b58: &[u8]) -> Option<[u8; KEY_LEN]> {
    let mut k = [0u8; KEY_LEN];
    match base58_decode(b58, &mut k) {
        Ok(KEY_LEN) => Some(k),
        _ => None,
    }
}

/// The guard's refusal as an event detail (`TxErr` discriminant + 1).
#[inline]
const fn tx_aux(e: TxErr) -> u64 {
    e as u64 + 1
}

/// Decode, parse, guard and sign the base64 transaction `b64` into `tx`;
/// its length. `deposit` bounds the token debit (0 for a claim).
fn guard_and_sign(
    b64: &[u8],
    tx: &mut [u8; TX_MAX],
    g: &TxGuard,
    deposit: u64,
    signer: &Ed25519Signer,
) -> Result<usize, TxErr> {
    // COPY: the server's base64 transaction decoded into the fixed
    // 1 232 B buffer the guard parses and our signature is written into —
    // the wire format (base64 text) is not the bytes we sign — rejected:
    // none (the plan's own "decode into a fixed buffer").
    let n = base64_decode(b64, &mut tx[..]).map_err(|_| TxErr::Length)?;
    let parsed = SolTx::parse(&tx[..n])?;
    let passed = guard(&tx[..n], &parsed, g, deposit)?;
    parsed.sign_into(&mut tx[..n], passed.signer_idx, signer)?;
    Ok(n)
}

/// The build's answer → a signed transaction and its context, or why not.
#[allow(clippy::too_many_arguments)]
fn prepare(
    http: u16,
    body: &[u8],
    it: &JupIntent,
    market_id: u64,
    cfg: &JupGwCfg,
    wallet_b58: &[u8],
    signer: &Ed25519Signer,
    tx: &mut [u8; TX_MAX],
    ctx: &mut [u8; CONTEXT_MAX],
) -> Result<Built, Refusal> {
    if http != 200 {
        return Err(venue_refusal(http, body));
    }
    let mut a = BuildAnswer::default();
    if scan_build(body, &mut a).is_err() {
        return Err(refusal(reason::BUILD_UNREADABLE, 0));
    }
    if !a.keeper_order {
        return Err(refusal(reason::NOT_KEEPER, 0));
    }
    if a.max_buy_price_micro > it.limit_px_1e6.max(0) as u64 {
        return Err(refusal(reason::PRICE_CAP, a.max_buy_price_micro));
    }
    if a.n_required != 1 || a.required[0].of(body) != wallet_b58 {
        return Err(refusal(reason::SIGNERS, u64::from(a.n_required)));
    }
    // The answer is the order we asked for: its market and outcome, a
    // buy, our wallet, a cost within the deposit.
    let mut want = [0u8; 32];
    let mut w = Put { dst: &mut want, n: 0 };
    w.put(b"POLY-");
    w.dec(market_id);
    w.put(if it.outcome == 0 { b"-0" } else { b"-1" });
    let wn = w.n;
    if a.market_id.of(body) != &want[..wn]
        || a.is_yes != Some(true)
        || a.is_buy != Some(true)
        || a.user_pubkey.of(body) != wallet_b58
        || !a.has_cost
        || a.cost_micro > it.deposit_micro
    {
        return Err(refusal(reason::MISMATCH, 0));
    }
    let Some(order_key) = key_of(a.order_pubkey.of(body)) else {
        return Err(refusal(reason::BUILD_UNREADABLE, 1));
    };
    let position_key = key_of(a.position_pubkey.of(body)).unwrap_or([0; KEY_LEN]);
    let c = a.context.of(body);
    if c.len() > CONTEXT_MAX {
        return Err(refusal(reason::BUILD_UNREADABLE, 2));
    }
    let tx_len = match guard_and_sign(a.transaction.of(body), tx, &cfg.guard, it.deposit_micro, signer) {
        Ok(n) => n,
        Err(e) => return Err(refusal(reason::GUARD, tx_aux(e))),
    };
    // COPY: the build's `execution.context` object (≤ CONTEXT_MAX) out of
    // the answer buffer — the execute's body is rendered in the same
    // client, whose next request overwrites this answer — rejected: a
    // split borrow of `core_net::HttpsReq`'s window and answer (a core-net
    // API change for one bounded copy per order).
    ctx[..c.len()].copy_from_slice(c);
    Ok(Built {
        tx_len,
        ctx_len: c.len(),
        order_key,
        position_key,
        fee_micro: a.fee_total_micro,
        cost_micro: a.cost_micro,
    })
}

/// What the execute's answer says.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Landing {
    Landed,
    /// Not believed until recon (a `Failed` may still land while its
    /// blockhash lives; an unreadable answer may have).
    Doubt(u8),
    Refused(Refusal),
}

/// The execute's answer → its verdict and the halt it asks (a region or a
/// key refusal). Only `Success` lands; every other answer came after the
/// signed transaction left the host, so it is in doubt until its blockhash
/// has expired and recon agrees it never landed.
fn landing(http: u16, body: &[u8]) -> (Landing, u64) {
    if http == 200 {
        let mut a = ExecuteAnswer::default();
        return match scan_execute(body, &mut a) {
            Ok(()) if a.status == ExecStatus::Success => (Landing::Landed, HALT_NONE),
            Ok(()) => (Landing::Doubt(reason::EXECUTE_FAILED), HALT_NONE),
            Err(_) => (Landing::Doubt(0), HALT_NONE),
        };
    }
    let f = venue_refusal(http, body);
    (Landing::Doubt(f.why), f.halt)
}

/// An in-doubt order still not found this long after its execute never
/// landed, ns: its blockhash lives ~150 blocks (≈ 60–90 s); 300 s bounds a
/// slow cluster with room (the guard refuses a durable nonce, which would
/// never expire).
pub const BLOCKHASH_LIFE_NS: u64 = 300_000_000_000;

/// jup-gw (module docs).
pub struct JupGateway<C: JupClient> {
    cfg: JupGwCfg,
    client: C,
    end: JupGwEnd,
    specs: Consumer<PmWindowSpec, PM15_SPEC_RING>,
    signer: Ed25519Signer,
    key: [u8; API_KEY_MAX],
    key_len: usize,
    wallet_b58: [u8; B58_KEY_MAX],
    wallet_len: usize,
    mint_b58: [u8; B58_KEY_MAX],
    mint_len: usize,
    markets: [Market; MARKETS_MAX],
    market_next: usize,
    rows: [Row; GW_ROWS],
    claims: [Claim; CLAIMS_MAX],
    held: [HeldLeg; HELD_MAX],
    held_dirty: bool,
    tx: [u8; TX_MAX],
    ctx: [u8; CONTEXT_MAX],
    path: [u8; QUERY_MAX],
    balance_due_ns: u64,
    positions_due_ns: u64,
    publish_ns: u64,
    usdc_micro: i64,
    sol_lamports: i64,
    counters: GwCounters,
    stats: Option<std::sync::Arc<JupExecStats>>,
}

impl<C: JupClient> Drop for JupGateway<C> {
    fn drop(&mut self) {
        // The API key leaves no copy behind (the signing key is
        // signer-ed25519's: mlock'd and wiped there).
        zeroize::Zeroize::zeroize(&mut self.key);
    }
}

/// Why [`JupGateway::new`] refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GwBootErr {
    /// The API key is empty, too long, or carries a byte a header cannot.
    ApiKey,
    /// The signer's public key is not the configured wallet.
    Signer,
    /// The client's body window is under [`BODY_MIN`].
    Window,
    /// The per-order maximum is under the venue's $5 minimum.
    MaxOrder,
    /// The guard's wallet is not the configured wallet.
    Guard,
    /// The guard's deposit mint is not the configured USDC mint, or no
    /// deposit account is configured.
    Deposit,
}

impl<C: JupClient> JupGateway<C> {
    /// Boot jup-gw. **Boot-only** — nothing allocates after this.
    ///
    /// # Errors
    /// [`GwBootErr`].
    pub fn new(
        cfg: JupGwCfg,
        mut client: C,
        end: JupGwEnd,
        specs: Consumer<PmWindowSpec, PM15_SPEC_RING>,
        signer: Ed25519Signer,
        api_key: &[u8],
    ) -> Result<Self, GwBootErr> {
        if api_key.is_empty()
            || api_key.len() > API_KEY_MAX
            || api_key.iter().any(|b| !b.is_ascii_graphic())
        {
            return Err(GwBootErr::ApiKey);
        }
        if signer.public_key() != cfg.wallet.as_slice() {
            return Err(GwBootErr::Signer);
        }
        if client.body_mut().len() < BODY_MIN {
            return Err(GwBootErr::Window);
        }
        if cfg.max_order_micro < MIN_DEPOSIT_MICRO {
            return Err(GwBootErr::MaxOrder);
        }
        if cfg.guard.wallet != cfg.wallet {
            return Err(GwBootErr::Guard);
        }
        if cfg.guard.deposit_mint != cfg.usdc_mint || cfg.guard.deposit_account == [0; KEY_LEN] {
            return Err(GwBootErr::Deposit);
        }
        let mut key = [0u8; API_KEY_MAX];
        // COPY: the API key into the gateway's header buffer (boot, once,
        // ≤ 128 B) — rejected: none, the header must outlive the caller's
        // environment read.
        key[..api_key.len()].copy_from_slice(api_key);
        let mut wallet_b58 = [0u8; B58_KEY_MAX];
        let wallet_len = base58_encode(&cfg.wallet, &mut wallet_b58).map_err(|_| GwBootErr::Signer)?;
        let mut mint_b58 = [0u8; B58_KEY_MAX];
        let mint_len = base58_encode(&cfg.usdc_mint, &mut mint_b58).map_err(|_| GwBootErr::Signer)?;
        Ok(Self {
            cfg,
            client,
            end,
            specs,
            signer,
            key,
            key_len: api_key.len(),
            wallet_b58,
            wallet_len,
            mint_b58,
            mint_len,
            markets: [Market::default(); MARKETS_MAX],
            market_next: 0,
            rows: [FREE_ROW; GW_ROWS],
            claims: [Claim::default(); CLAIMS_MAX],
            held: [HeldLeg::EMPTY; HELD_MAX],
            held_dirty: false,
            tx: [0; TX_MAX],
            ctx: [0; CONTEXT_MAX],
            path: [0; QUERY_MAX],
            balance_due_ns: 0,
            positions_due_ns: 0,
            publish_ns: 0,
            usdc_micro: -1,
            sol_lamports: -1,
            counters: GwCounters::default(),
            stats: None,
        })
    }

    /// Publish tallies into `stats` (the cli's `/metrics` mirror).
    pub fn set_stats(&mut self, stats: std::sync::Arc<JupExecStats>) {
        self.stats = Some(stats);
    }

    /// When the first balance and position reads are due (engine ns):
    /// boot sets them a little after the first pass; the rig defers them.
    pub fn set_idle_schedule(&mut self, balance_due_ns: u64, positions_due_ns: u64) {
        self.balance_due_ns = balance_due_ns;
        self.positions_due_ns = positions_due_ns;
    }

    /// The tallies.
    #[must_use]
    pub const fn counters(&self) -> GwCounters {
        self.counters
    }

    /// The client (tests read its record; the run loop closes it).
    pub fn client_mut(&mut self) -> &mut C {
        &mut self.client
    }

    /// Orders the gateway still tracks.
    #[must_use]
    pub fn open_rows(&self) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < GW_ROWS {
            n += usize::from(self.rows[i].state != R_FREE);
            i += 1;
        }
        n
    }

    /// The held legs the boot restored (`pmj-held.state`).
    pub fn restore_held(&mut self, legs: &[HeldLeg]) {
        let mut i = 0usize;
        while i < legs.len() && i < HELD_MAX {
            self.held[i] = legs[i];
            i += 1;
        }
    }

    /// The held file, when the table changed since the last call:
    /// rendered into `dst` (≥ [`HELD_FILE_MAX`]); the owner writes it.
    pub fn take_held(&mut self, dst: &mut [u8]) -> Option<usize> {
        if !self.held_dirty || dst.len() < HELD_FILE_MAX {
            return None;
        }
        self.held_dirty = false;
        self.counters.held_writes += 1;
        Some(render_held(dst, &self.wallet_b58[..self.wallet_len], &self.held))
    }

    /// One pass at engine time `now` and wall time `wall` (Unix ns):
    /// whether it did anything.
    pub fn step(&mut self, now: u64, wall: u64) -> bool {
        let mut worked = self.take_specs();
        if self.end.evt.room() >= EVT_ROOM {
            // A 64 B POD by value: the slot is released at once.
            let next = self.end.cmd.pop().map(|c| *c);
            if let Some(it) = next {
                self.on_intent(&it, now, wall);
                worked = true;
            } else if self.poll_due(now) || self.idle_work(now) {
                worked = true;
            }
        }
        self.end.stats.beat(now);
        self.publish(now);
        worked
    }

    // --- the window table --------------------------------------------

    fn take_specs(&mut self) -> bool {
        let mut any = false;
        while let Some(s) = self.specs.try_pop_ref() {
            any = true;
            let (family, window, market_id) = (s.family, s.window, s.market_id);
            drop(s);
            let Some(instance) = pm5_instance_id(window, family as usize) else {
                self.counters.specs_refused += 1;
                continue;
            };
            if market_id == 0 {
                self.counters.specs_refused += 1;
                continue;
            }
            self.counters.specs += 1;
            let mut at = MARKETS_MAX;
            let mut i = 0usize;
            while i < MARKETS_MAX {
                if self.markets[i].instance == instance {
                    at = i;
                    break;
                }
                if at == MARKETS_MAX && self.markets[i].instance == 0 {
                    at = i;
                }
                i += 1;
            }
            if at == MARKETS_MAX {
                at = self.market_next;
                self.market_next = (self.market_next + 1) % MARKETS_MAX;
            }
            self.markets[at] = Market { instance, market_id };
        }
        any
    }

    fn market_of(&self, instance: u32) -> Option<u64> {
        if instance == 0 {
            return None;
        }
        let mut i = 0usize;
        while i < MARKETS_MAX {
            if self.markets[i].instance == instance {
                return Some(self.markets[i].market_id);
            }
            i += 1;
        }
        None
    }

    // --- events ------------------------------------------------------

    fn emit(&mut self, ev: &JupEvent) {
        if !self.end.evt.emit(ev) {
            self.end.stats.halt(HALT_EMIT);
        }
    }

    fn refuse(&mut self, oid: u64, why: u8, aux: u64, now: u64) {
        self.counters.refused += 1;
        let mut ev = JupEvent::new(EVT_REFUSED, oid, now);
        ev.reason = why;
        ev.aux = aux;
        self.emit(&ev);
    }

    fn free_row(&self) -> Option<usize> {
        let mut i = 0usize;
        while i < GW_ROWS {
            if self.rows[i].state == R_FREE {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    // --- an order ----------------------------------------------------

    fn on_intent(&mut self, it: &JupIntent, now: u64, wall: u64) {
        if it.kind == INTENT_CLAIM {
            self.positions_due_ns = now;
            return;
        }
        let oid = it.client_oid;
        if it.kind != INTENT_BUY {
            return self.refuse(oid, reason::LOCAL, 0, now);
        }
        let halt = self.end.stats.halt_reason();
        if halt != HALT_NONE {
            return self.refuse(oid, reason::HALTED, halt, now);
        }
        let Some(market_id) = self.market_of(it.instance) else {
            return self.refuse(oid, reason::NO_MARKET, u64::from(it.instance), now);
        };
        if wall.saturating_add(self.cfg.min_lead_ns) >= it.window_end_ns {
            return self.refuse(oid, reason::LATE, 0, now);
        }
        if it.deposit_micro < MIN_DEPOSIT_MICRO || it.deposit_micro > self.cfg.max_order_micro {
            return self.refuse(oid, reason::SIZE, it.deposit_micro, now);
        }
        if it.limit_px_1e6 <= 0 || it.limit_px_1e6 >= 1_000_000 {
            return self.refuse(oid, reason::LOCAL, 1, now);
        }
        let Some(r) = self.free_row() else {
            return self.refuse(oid, reason::FULL, 0, now);
        };
        let outcome = if it.outcome == 0 { Outcome::Up } else { Outcome::Down };

        // The build.
        let Some(n) = order_body(
            self.client.body_mut(),
            &self.wallet_b58[..self.wallet_len],
            market_id,
            outcome,
            it.deposit_micro,
            &self.mint_b58[..self.mint_len],
        ) else {
            return self.refuse(oid, reason::LOCAL, 2, now);
        };
        self.end.stats.push_to_write.record(now.saturating_sub(it.decision_ns));
        self.counters.builds += 1;
        let t0 = core_time::now_ns();
        let key: Header<'_> = (b"x-api-key", &self.key[..self.key_len]);
        let res = self.client.request(Method::Post, P_ORDERS, &[key], n);
        let (http, range) = match res {
            Ok(x) => x,
            Err(_) => return self.refuse(oid, reason::NETWORK, 0, now),
        };
        self.end.stats.post_rtt.record(core_time::now_ns().saturating_sub(t0));
        let prepared = {
            let body = self.client.resp().get(range).unwrap_or(&[]);
            prepare(
                http,
                body,
                it,
                market_id,
                &self.cfg,
                &self.wallet_b58[..self.wallet_len],
                &self.signer,
                &mut self.tx,
                &mut self.ctx,
            )
        };
        let b = match prepared {
            Ok(b) => b,
            Err(f) => {
                match f.why {
                    reason::GUARD => self.counters.guard_refused += 1,
                    reason::PRICE_CAP => self.counters.price_refused += 1,
                    reason::REGION => self.counters.region_refused += 1,
                    reason::VENUE => self.counters.venue_refused += 1,
                    _ => {}
                }
                if f.halt != HALT_NONE {
                    self.end.stats.halt(f.halt);
                }
                return self.refuse(oid, f.why, f.aux, now);
            }
        };
        self.counters.built += 1;

        // The execute.
        let Some(n) = execute_body(
            self.client.body_mut(),
            &self.tx[..b.tx_len],
            &self.ctx[..b.ctx_len],
            oid,
        ) else {
            return self.refuse(oid, reason::LOCAL, 3, now);
        };
        self.counters.executes += 1;
        let t1 = core_time::now_ns();
        let key: Header<'_> = (b"x-api-key", &self.key[..self.key_len]);
        let res = self.client.request(Method::Post, P_EXECUTE, &[key], n);
        let rtt = core_time::now_ns().saturating_sub(t1);
        let left_close = it.window_end_ns.saturating_sub(wall);
        self.rows[r] = Row {
            client_oid: oid,
            order_key: b.order_key,
            position_key: b.position_key,
            deadline_ns: now.saturating_add(left_close).saturating_add(DEADLINE_AFTER_CLOSE_NS),
            next_ns: now.saturating_add(POLL_EVERY_NS),
            sent_ns: now,
            filled_micro: 0,
            fee_micro: b.fee_micro,
            cost_micro: b.cost_micro,
            instance: it.instance,
            outcome: it.outcome & 1,
            state: R_DOUBT,
            tries: 0,
            landed: false,
        };
        let (verdict, halt) = match res {
            Err(PostErr { left_host: false, .. }) => {
                (Landing::Refused(refusal(reason::NETWORK, 1)), HALT_NONE)
            }
            Err(_) => (Landing::Doubt(0), HALT_NONE),
            Ok((http, range)) => {
                self.end.stats.rest_rtt.record(rtt);
                let body = self.client.resp().get(range).unwrap_or(&[]);
                landing(http, body)
            }
        };
        if halt != HALT_NONE {
            self.end.stats.halt(halt);
        }
        match verdict {
            Landing::Landed => {
                self.counters.landed += 1;
                self.rows[r].state = R_POLL;
                self.rows[r].landed = true;
                let mut ev = JupEvent::new(EVT_LANDED, oid, now);
                ev.fee_micro = b.fee_micro;
                ev.cost_micro = b.cost_micro;
                ev.filled_micro = 0;
                ev.aux = rtt / 1_000;
                self.emit(&ev);
            }
            Landing::Doubt(why) => {
                match why {
                    reason::EXECUTE_FAILED => self.counters.exec_failed += 1,
                    reason::REGION => self.counters.region_refused += 1,
                    reason::VENUE => self.counters.venue_refused += 1,
                    _ => {}
                }
                self.counters.in_doubt += 1;
                let mut ev = JupEvent::new(EVT_IN_DOUBT, oid, now);
                ev.reason = why;
                self.emit(&ev);
            }
            Landing::Refused(f) => {
                // Nothing left the host: the signed transaction dies here.
                self.rows[r] = FREE_ROW;
                self.refuse(oid, f.why, f.aux, now);
            }
        }
    }

    // --- after the land ----------------------------------------------

    /// One due row's request (or its move to recon): whether one was due.
    fn poll_due(&mut self, now: u64) -> bool {
        let mut i = 0usize;
        while i < GW_ROWS {
            let r = self.rows[i];
            if r.state != R_FREE && r.next_ns <= now {
                if r.state == R_RECON {
                    self.recon(i, now);
                } else if now >= r.deadline_ns {
                    self.rows[i].state = R_RECON;
                    self.rows[i].next_ns = now;
                } else {
                    self.status(i, now);
                }
                return true;
            }
            i += 1;
        }
        false
    }

    /// The order landed after all (an in-doubt row the venue knows).
    fn landed_late(&mut self, i: usize, now: u64) {
        if self.rows[i].landed {
            return;
        }
        self.rows[i].landed = true;
        self.counters.landed += 1;
        let r = self.rows[i];
        let mut ev = JupEvent::new(EVT_LANDED, r.client_oid, now);
        ev.fee_micro = r.fee_micro;
        ev.cost_micro = r.cost_micro;
        self.emit(&ev);
    }

    fn status(&mut self, i: usize, now: u64) {
        let r = self.rows[i];
        let mut k = [0u8; B58_KEY_MAX];
        let Ok(kl) = base58_encode(&r.order_key, &mut k) else {
            self.rows[i].state = R_RECON;
            return;
        };
        let Some(pn) = status_path(&mut self.path[..PATH_MAX], &k[..kl]) else {
            self.rows[i].state = R_RECON;
            return;
        };
        self.counters.polls += 1;
        let t0 = core_time::now_ns();
        let key: Header<'_> = (b"x-api-key", &self.key[..self.key_len]);
        let res = self.client.request(Method::Get, &self.path[..pn], &[key], 0);
        self.rows[i].next_ns = now.saturating_add(POLL_EVERY_NS);
        let state = match res {
            Ok((200, range)) => {
                self.end.stats.cancel_rtt.record(core_time::now_ns().saturating_sub(t0));
                let body = self.client.resp().get(range).unwrap_or(&[]);
                let mut a = StatusAnswer::default();
                match scan_status(body, &mut a) {
                    Ok(()) => Some(a.state),
                    Err(_) => None,
                }
            }
            Ok((404, _)) => return,
            _ => None,
        };
        let Some(state) = state else {
            self.counters.poll_failures += 1;
            return;
        };
        match state {
            OrderState::Filled | OrderState::Failed => {
                self.landed_late(i, now);
                self.rows[i].state = R_RECON;
                self.rows[i].next_ns = now;
            }
            OrderState::Pending | OrderState::Partial => {
                self.landed_late(i, now);
                self.rows[i].state = R_POLL;
            }
            OrderState::Unknown => {}
        }
    }

    /// Render the recon query for page `page` into `self.path`.
    fn recon_target(&mut self, page: u32) -> usize {
        let mut w = Put { dst: &mut self.path, n: 0 };
        w.put(P_ORDERS_Q);
        w.put(&self.wallet_b58[..self.wallet_len]);
        w.put(b"&start=");
        w.dec(u64::from(page * RECON_PAGE));
        w.put(b"&end=");
        w.dec(u64::from((page + 1) * RECON_PAGE));
        w.n
    }

    fn recon(&mut self, i: usize, now: u64) {
        let r = self.rows[i];
        let mut k = [0u8; B58_KEY_MAX];
        let kl = base58_encode(&r.order_key, &mut k).unwrap_or(0);
        let mut found: Option<OrderRecord> = None;
        let mut read_ok = true;
        let mut ran_out = false;
        let mut page = 0u32;
        while page < RECON_PAGES_MAX {
            let pn = self.recon_target(page);
            self.counters.recons += 1;
            let key: Header<'_> = (b"x-api-key", &self.key[..self.key_len]);
            let res = self.client.request(Method::Get, &self.path[..pn], &[key], 0);
            let Ok((200, range)) = res else {
                read_ok = false;
                break;
            };
            let body = self.client.resp().get(range).unwrap_or(&[]);
            let mut rec = OrderRecord::default();
            match scan_orders(body, &k[..kl], &mut rec) {
                Ok((true, _)) => {
                    found = Some(rec);
                    break;
                }
                Ok((false, true)) => {
                    page += 1;
                    ran_out = page == RECON_PAGES_MAX;
                }
                Ok((false, false)) => break,
                Err(_) => {
                    read_ok = false;
                    break;
                }
            }
        }
        // Pages left unread are no evidence the order is absent.
        if found.is_none() && ran_out {
            read_ok = false;
        }
        let terminal = |r: &OrderRecord| r.state == OrderState::Filled || r.state == OrderState::Failed;
        match found {
            Some(rec) if terminal(&rec) && rec.has_fill && (rec.filled_micro == 0 || rec.has_avg) => {
                self.landed_late(i, now);
                self.finish(i, &rec, now);
            }
            Some(rec) if terminal(&rec) => {
                // Terminal without its numbers: not read (retried, then
                // LOST) — never booked as unfilled.
                self.landed_late(i, now);
                self.counters.recon_misses += 1;
                self.retry_recon(i, now, false);
            }
            Some(_) => {
                // Still pending past its deadline: read again later.
                self.landed_late(i, now);
                self.retry_recon(i, now, false);
            }
            None if read_ok && !r.landed && now >= r.sent_ns.saturating_add(BLOCKHASH_LIFE_NS) => {
                // In doubt, never found, its blockhash long expired: it
                // never landed.
                self.counters.done += 1;
                let mut ev = JupEvent::new(EVT_DONE, r.client_oid, now);
                ev.aux = 1;
                self.rows[i] = FREE_ROW;
                self.emit(&ev);
            }
            None => {
                self.counters.recon_misses += 1;
                self.retry_recon(i, now, !r.landed && read_ok);
            }
        }
    }

    /// Recon again in [`RECON_GAP_NS`]; a landed order that exhausts its
    /// tries is LOST (`doubt_wait`: an in-doubt order waiting out its
    /// blockhash does not spend tries).
    fn retry_recon(&mut self, i: usize, now: u64, doubt_wait: bool) {
        self.rows[i].next_ns = now.saturating_add(RECON_GAP_NS);
        if doubt_wait {
            return;
        }
        self.rows[i].tries = self.rows[i].tries.saturating_add(1);
        if self.rows[i].tries >= RECON_TRIES_MAX {
            let oid = self.rows[i].client_oid;
            self.counters.lost += 1;
            self.rows[i] = FREE_ROW;
            self.end.stats.halt(HALT_RECON);
            let ev = JupEvent::new(EVT_LOST, oid, now);
            self.emit(&ev);
        }
    }

    /// A terminal order: its fill (cumulative), the held leg, DONE.
    fn finish(&mut self, i: usize, rec: &OrderRecord, now: u64) {
        let r = self.rows[i];
        if rec.filled_micro > r.filled_micro {
            let cost = mul_div(rec.filled_micro, rec.avg_px_micro, 1_000_000);
            let mut ev = JupEvent::new(EVT_FILL, r.client_oid, now);
            ev.filled_micro = rec.filled_micro;
            ev.avg_px_micro = rec.avg_px_micro;
            ev.fee_micro = r.fee_micro;
            ev.cost_micro = cost;
            self.counters.fills += 1;
            self.end.stats.fill_venue_to_local.record(now.saturating_sub(r.sent_ns));
            self.emit(&ev);
            let prev_cost = mul_div(r.filled_micro, rec.avg_px_micro, 1_000_000);
            self.hold(&r, rec.filled_micro - r.filled_micro, cost.saturating_sub(prev_cost));
        }
        self.counters.done += 1;
        let mut ev = JupEvent::new(EVT_DONE, r.client_oid, now);
        ev.filled_micro = rec.filled_micro.max(r.filled_micro);
        self.rows[i] = FREE_ROW;
        self.emit(&ev);
    }

    /// Book `contracts` (and their `cost`) into the held table.
    fn hold(&mut self, r: &Row, contracts: u64, cost: u64) {
        let mut at = HELD_MAX;
        let mut free = HELD_MAX;
        let mut j = 0usize;
        while j < HELD_MAX {
            let l = &self.held[j];
            if l.state != 0
                && l.instance == r.instance
                && l.outcome == r.outcome
                && l.position == r.position_key
            {
                at = j;
                break;
            }
            if free == HELD_MAX && l.state == 0 {
                free = j;
            }
            j += 1;
        }
        if at == HELD_MAX {
            if free == HELD_MAX {
                self.counters.held_full += 1;
                return;
            }
            at = free;
            self.held[at] = HeldLeg {
                position: r.position_key,
                instance: r.instance,
                outcome: r.outcome,
                state: HELD_OPEN,
                ..HeldLeg::EMPTY
            };
        }
        let l = &mut self.held[at];
        l.contracts_micro = l.contracts_micro.saturating_add(contracts);
        l.cost_micro = l.cost_micro.saturating_add(cost);
        self.held_dirty = true;
    }

    // --- idle work ---------------------------------------------------

    fn idle_work(&mut self, now: u64) -> bool {
        if now >= self.balance_due_ns {
            self.balance_due_ns = now.saturating_add(BALANCE_EVERY_NS);
            self.read_balances(now);
            return true;
        }
        if now >= self.positions_due_ns {
            self.positions_due_ns = now.saturating_add(POSITIONS_EVERY_NS);
            self.read_positions();
            return true;
        }
        if self.end.stats.halt_reason() != HALT_NONE {
            return false;
        }
        let mut c = 0usize;
        while c < CLAIMS_MAX {
            if self.claims[c].position != [0; KEY_LEN] && self.claims[c].state == 0 {
                self.build_claim(c, now);
                return true;
            }
            c += 1;
        }
        false
    }

    fn read_balances(&mut self, now: u64) {
        let mut w = Put { dst: &mut self.path, n: 0 };
        w.put(P_BALANCES);
        w.put(&self.wallet_b58[..self.wallet_len]);
        let pn = w.n;
        self.counters.balance_reads += 1;
        let key: Header<'_> = (b"x-api-key", &self.key[..self.key_len]);
        let res = self.client.request(Method::Get, &self.path[..pn], &[key], 0);
        let mut b = Balances::default();
        let ok = match res {
            Ok((200, range)) => {
                let body = self.client.resp().get(range).unwrap_or(&[]);
                scan_balances(body, &self.mint_b58[..self.mint_len], &mut b).is_ok()
            }
            _ => false,
        };
        let mut ev = JupEvent::new(EVT_BALANCE, 0, now);
        // No USDC entry is no reading: it must never seed the session
        // anchor at 0 (that would hide every later loss).
        if let (true, Some(usdc)) = (ok, b.usdc_micro) {
            self.usdc_micro = word(usdc);
            self.sol_lamports = b.sol_lamports.map_or(-1, word);
            ev.aux = usdc;
            ev.cost_micro = b.sol_lamports.unwrap_or(0);
        } else {
            self.counters.balance_failures += 1;
            ev.reason = 1;
        }
        self.emit(&ev);
    }

    fn read_positions(&mut self) {
        let mut w = Put { dst: &mut self.path, n: 0 };
        w.put(P_POSITIONS_Q);
        w.put(&self.wallet_b58[..self.wallet_len]);
        let pn = w.n;
        self.counters.position_reads += 1;
        let key: Header<'_> = (b"x-api-key", &self.key[..self.key_len]);
        let res = self.client.request(Method::Get, &self.path[..pn], &[key], 0);
        let Ok((200, range)) = res else {
            self.counters.position_failures += 1;
            return;
        };
        let body = self.client.resp().get(range).unwrap_or(&[]);
        let claims = &mut self.claims;
        let held = &self.held;
        let mut gone = [0usize; HELD_MAX];
        let mut n_gone = 0usize;
        let read = scan_positions(body, &mut |p| {
            let Some(k) = key_of(p.pubkey.of(body)) else {
                return;
            };
            if p.claimed {
                // Claimed (or settled by the venue on its own): the leg's
                // payout is in the wallet — it leaves the held table once
                // its event is written (below).
                let mut j = 0usize;
                while j < HELD_MAX {
                    if held[j].state != 0 && held[j].position == k && n_gone < HELD_MAX {
                        gone[n_gone] = j;
                        n_gone += 1;
                    }
                    j += 1;
                }
                return;
            }
            if !p.claimable || p.contracts_micro == 0 {
                return;
            }
            let mut free = CLAIMS_MAX;
            let mut c = 0usize;
            while c < CLAIMS_MAX {
                if claims[c].position == k {
                    return;
                }
                if free == CLAIMS_MAX && claims[c].position == [0; KEY_LEN] {
                    free = c;
                }
                c += 1;
            }
            if free < CLAIMS_MAX {
                claims[free] = Claim {
                    position: k,
                    payout_micro: p.payout_micro,
                    state: 0,
                };
            }
        });
        if read.is_err() {
            self.counters.position_failures += 1;
        }
        // Within the ring's room only: a leg not told this pass stays in
        // the table and the next read tells it.
        let mut j = 0usize;
        while j < n_gone && self.end.evt.room() > 0 {
            let at = gone[j];
            j += 1;
            let l = self.held[at];
            let mut ev = JupEvent::new(EVT_CLAIMED, 0, core_time::now_ns());
            ev.aux = u64::from(l.instance);
            ev.reason = l.outcome;
            ev.cost_micro = l.cost_micro;
            self.emit(&ev);
            self.held[at] = HeldLeg::EMPTY;
            self.held_dirty = true;
        }
    }

    /// Build, guard and sign claim `c`; held (the route is J0's).
    fn build_claim(&mut self, c: usize, now: u64) {
        let pos = self.claims[c].position;
        let mut k = [0u8; B58_KEY_MAX];
        let Ok(kl) = base58_encode(&pos, &mut k) else {
            self.claims[c].state = 2;
            return;
        };
        let Some(pn) = claim_path(&mut self.path[..PATH_MAX], &k[..kl]) else {
            self.claims[c].state = 2;
            return;
        };
        let Some(n) = claim_body(self.client.body_mut(), &self.wallet_b58[..self.wallet_len]) else {
            self.claims[c].state = 2;
            return;
        };
        let key: Header<'_> = (b"x-api-key", &self.key[..self.key_len]);
        let res = self.client.request(Method::Post, &self.path[..pn], &[key], n);
        let signed = match res {
            Ok((200, range)) => {
                let body = self.client.resp().get(range).unwrap_or(&[]);
                let mut a = ClaimAnswer::default();
                match scan_claim(body, &mut a) {
                    Ok(()) => {
                        guard_and_sign(a.transaction.of(body), &mut self.tx, &self.cfg.guard, 0, &self.signer)
                            .map(|_| a.payout_micro)
                            .ok()
                    }
                    Err(_) => None,
                }
            }
            _ => None,
        };
        let Some(payout) = signed else {
            self.counters.claims_refused += 1;
            self.claims[c].state = 2;
            return;
        };
        self.claims[c].state = 1;
        self.claims[c].payout_micro = payout;
        self.counters.claims_held += 1;
        let mut j = 0usize;
        while j < HELD_MAX {
            if self.held[j].state == HELD_OPEN && self.held[j].position == pos {
                self.held[j].state = HELD_CLAIM_READY;
                self.held_dirty = true;
            }
            j += 1;
        }
        let mut ev = JupEvent::new(EVT_CLAIM_READY, 0, now);
        ev.cost_micro = payout;
        self.emit(&ev);
    }

    // --- tallies -----------------------------------------------------

    fn publish(&mut self, now: u64) {
        let Some(stats) = self.stats.as_ref() else {
            return;
        };
        if now < self.publish_ns {
            return;
        }
        self.publish_ns = now.saturating_add(PUBLISH_EVERY_NS);
        let c = &self.counters;
        let v: [i64; GW_WORDS] = [
            word(c.specs),
            word(c.specs_refused),
            word(c.builds),
            word(c.built),
            word(c.guard_refused),
            word(c.price_refused),
            word(c.executes),
            word(c.landed),
            word(c.exec_failed),
            word(c.in_doubt),
            word(c.refused),
            word(c.venue_refused),
            word(c.region_refused),
            word(c.polls),
            word(c.poll_failures),
            word(c.recons),
            word(c.recon_misses),
            word(c.fills),
            word(c.done),
            word(c.lost),
            word(c.balance_reads),
            word(c.balance_failures),
            word(c.position_reads),
            word(c.position_failures),
            word(c.claims_held),
            word(c.claims_refused),
            word(c.held_writes),
            word(c.held_full),
            word(self.end.stats.halt_reason()),
            self.usdc_micro,
            self.sol_lamports,
        ];
        stats.set_gw(&v);
    }
}

/// `a × b / d` without overflow (u128), saturating.
#[inline]
fn mul_div(a: u64, b: u64, d: u64) -> u64 {
    if d == 0 {
        return 0;
    }
    u64::try_from(u128::from(a) * u128::from(b) / u128::from(d)).unwrap_or(u64::MAX)
}

/// A bounded writer into a target buffer (a write past it is dropped:
/// every target here is sized by `QUERY_MAX`).
struct Put<'a> {
    dst: &'a mut [u8],
    n: usize,
}

impl Put<'_> {
    #[inline]
    fn put(&mut self, b: &[u8]) {
        let end = self.n + b.len();
        if end > self.dst.len() {
            return;
        }
        // COPY: a target part into the request's target buffer (the
        // render itself, ≤ QUERY_MAX) — rejected: none.
        self.dst[self.n..end].copy_from_slice(b);
        self.n = end;
    }

    #[inline]
    fn dec(&mut self, v: u64) {
        let mut d = [0u8; 20];
        let mut x = v;
        let mut k = 0usize;
        loop {
            d[19 - k] = b'0' + (x % 10) as u8;
            k += 1;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        self.put(&d[20 - k..]);
    }
}

const _: () = assert!(QUERY_MAX >= P_ORDERS_Q.len() + B58_KEY_MAX + 7 + 10 + 5 + 10);
const _: () = assert!(QUERY_MAX >= P_POSITIONS_Q.len() + B58_KEY_MAX);
const _: () = assert!(QUERY_MAX >= P_BALANCES.len() + B58_KEY_MAX);
const _: () = assert!(PATH_MAX <= QUERY_MAX);

#[cfg(test)]
mod tests;
