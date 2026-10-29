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
//! the client's window, plus `pmj.toml`'s `build_extra` members — J0's
//! limit-order request field, [`crate::render::BuildExtra`]) → the answer
//! scanned in place ([`crate::json::scan_build`]): a keeper order,
//! `requiredSigners` exactly our wallet (or, with the dormant co-signer
//! amendment pinned, our wallet and that key), the server's
//! `maxBuyPriceUsd` ≤ the member's limit → the base64 transaction decoded
//! into the fixed 1 232 B buffer → the **order guard**
//! ([`crate::tx::guard_order`], mandatory: an empty allowlist refuses
//! everything; the signed create-order must be a Limit buy at or under the
//! member's limit, depositing exactly what we asked, from our deposit
//! account, into the pinned vault) → our signature written into its slot →
//! `POST /execute` with the build's `context` → LANDED / REFUSED /
//! IN_DOUBT. A build creates nothing on chain, so every failure before the
//! execute is a refusal; an execute that may have left the host and did
//! not answer readably is IN_DOUBT (its blockhash still lets it land).
//!
//! **After the land**: `GET /orders/status/{orderPubkey}` every
//! [`POLL_EVERY_NS`] until it is terminal — its fill numbers in the answer
//! book it at once — or the deadline (the window's close +
//! [`DEADLINE_AFTER_CLOSE_NS`]) passes, then **recon by the order's
//! pubkey** (the build answer's `order.orderPubkey`): the PRIMARY read is
//! the same status answer, which answers after the close (J0's public
//! evidence: `GET /orders` lists OPEN orders only); the SECONDARY read is
//! the account's `GET /orders` pages (an order still open). A terminal
//! record with its numbers → FILL (cumulative) and DONE; without them it is
//! not read (never booked unfilled); fill numbers whose sources disagree
//! are not read either. The signed create-order's order and position
//! accounts are the answer's own keys (the order guard), so recon reads the
//! order that was signed. **An in-doubt order is released** (DONE, `aux` 1:
//! it never landed) only when (i) its status answers the API's OWN JSON 404
//! of kind `invalid_request_error` (any other kind — `api_error`, a rate
//! limit, an auth or region refusal — and a CDN's or proxy's 404 are no
//! evidence), (ii) twice, at least
//! [`RELEASE_GAP_NS`] apart, both at or after its blockhash's life
//! ([`BLOCKHASH_LIFE_NS`] after the execute), and (iii) no 200 of any kind
//! was ever read for it — pages of open orders are no evidence. Whether
//! Jupiter's 404 for an unknown order is its error object is a J0 fact: if
//! it is not, nothing releases and the order goes LOST (a halt, never a
//! guess). An order recon can read nothing about for [`RECON_TRIES_MAX`]
//! tries — landed, or in doubt with its status unread — is LOST and the
//! gateway halts (the operator reconciles).
//!
//! **The debit halt** (risk-policy J3 pre-dust (c)): the deposit moves by
//! Jupiter's CPI, so after an order lands the wallet's deposit-mint balance
//! is checked at every balance read: it must be at least the REFERENCE
//! minus the deposits of every order whose signed transaction may have
//! left the host since (landed or in doubt). The reference is the last
//! balance read taken with no order in flight — no row open and no execute
//! sent in the last [`DEBIT_SETTLE_NS`] — re-anchored at each such read
//! (after its own check). A refund or a claim payout only raises the
//! balance, so it never halts; a lower read halts the gateway and the arm
//! ([`HALT_DEBIT`], counted). No build is made before the first reference
//! (`reason::NO_REFERENCE`), when the last read is older than
//! [`REFERENCE_MAX_AGE_NS`] (`reason::STALE_REFERENCE`: failed reads would
//! blind the halt), or when the deposits allowed since the reference would
//! pass [`DEBIT_ALLOW_ORDERS`] per-order maxima (`reason::ALLOWANCE`: the
//! floor must not sink toward zero). These three are local refusals that
//! count toward the arm's reject-streak halt BY DESIGN (fail-closed: a
//! debit halt that cannot judge stops the slot rather than trading blind).
//! Its limits: a payout landing with a drain can
//! mask it by up to the payout; a balance answer staler than
//! [`DEBIT_SETTLE_NS`] at a re-anchor could halt a sound wallet (a halt,
//! never a loss); the read cadence is [`BALANCE_EVERY_NS`].
//!
//! **E-5 for this venue**: the execute's `Success` is the ACK (LANDED);
//! the order record's fill numbers are the FILL. The venue reports no fee
//! per fill: the build's estimate rides the events, the position's
//! `feesPaidUsd` is J5's measure.
//!
//! **Idle work** (no order building, no row due): the wallet's balances
//! every [`BALANCE_EVERY_NS`] (the USDC and SOL gauges, the arm's worth,
//! the debit halt); the positions every [`POSITIONS_EVERY_NS`] (or at the
//! arm's [`INTENT_CLAIM`]), and each claimable one's claim — built, guarded
//! ([`crate::tx::guard_claim`]: one `claim_payout` / `claim_payout2`, no
//! token may leave our account, the payout lands in our deposit account)
//! and signed, then held: **the claim's submission route is unverified
//! until J0 records one** (`claims_held`).
//!
//! **Halts** (the kit's halt word, the first reason wins): a region
//! refusal, an authentication refusal, an order recon lost, a lost event,
//! a debit beyond the deposits ([`HALT_DEBIT`]), a position no held leg
//! and no order in flight accounts for ([`HALT_UNKNOWN_POSITION`]; never
//! claimed), and the operator's
//! (`exec.HALT` on slot 11 reaches the arm's `cancel_slot`). A halted
//! gateway refuses every build before the wire — a landed order is still
//! polled and reconciled: it cannot be pulled.
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
    claim_body, claim_path, execute_body, order_body, status_path, BuildExtra, Outcome,
    B58_KEY_MAX, CONTEXT_MAX, EXECUTE_BODY_MAX, ORDER_BODY_MAX, PATH_MAX,
};
use crate::stats::{word, JupExecStats, GW_WORDS, PUBLISH_EVERY_NS};
use crate::tx::{guard_claim, guard_order, OrderAsk, SolTx, TxErr, TxGuard, KEY_LEN, TX_MAX};

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
/// Halt: the wallet's deposit-mint balance fell below the reference minus
/// the deposits of the orders sent since (module docs, "The debit halt").
pub const HALT_DEBIT: u64 = HALT_VENUE_BASE + 7;
/// Halt: the venue lists a position with contracts that no held leg and no
/// order in flight accounts for (the venue analogue of kill-switch trigger
/// 3: a position we did not open, or one an order we released as never
/// landed did open).
pub const HALT_UNKNOWN_POSITION: u64 = HALT_VENUE_BASE + 8;

/// A balance read counts as "no order in flight" (a debit reference) only
/// this long after the last execute, ns — room for the balance answer to
/// show the last deposit's debit.
pub const DEBIT_SETTLE_NS: u64 = 30_000_000_000;

/// The deposits outstanding since the debit reference may reach this many
/// per-order maxima, no more (risk review W5-1, MINOR 2): without it, orders
/// that never leave a quiet balance read would sink the debit floor toward
/// zero (`saturating_sub`) and blind the halt. Four is the member's own
/// per-market order cap (`pmj.toml` `max_orders_market`, default 4); a
/// quiet read (no row open, [`DEBIT_SETTLE_NS`] after the last execute)
/// re-anchors and frees it. **Sized for the BTC-only dust / pilot frames
/// (one family live):** before more than one family arms live it must be
/// re-sized to families × the per-market cap, or the later families' orders
/// in a window are refused (`reason::ALLOWANCE`).
pub const DEBIT_ALLOW_ORDERS: u64 = 4;

/// The debit reference is stale — no build — when the last balance read
/// that read is older than this, ns (two missed reads: MINOR 3).
pub const REFERENCE_MAX_AGE_NS: u64 = 2 * BALANCE_EVERY_NS;

/// An in-doubt order is released only on two of Jupiter's own 404s at
/// least this far apart, both at or after its blockhash's life, ns.
pub const RELEASE_GAP_NS: u64 = 30_000_000_000;

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
    /// The build body's extra members (`pmj.toml` `build_extra`; none:
    /// today's body).
    pub build_extra: BuildExtra,
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
    /// Any 200 for this order was read (it then is never released as
    /// never-landed).
    seen_200: bool,
    /// The first of Jupiter's own 404s at or after the blockhash's life
    /// (0: none yet).
    first_404_ns: u64,
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
    seen_200: false,
    first_404_ns: 0,
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
    /// Balance reads below the debit floor (each one; the first halts).
    pub debit_halts: u64,
    /// In-doubt orders released as never landed (a 404 past the blockhash).
    pub released: u64,
    /// Positions read with contracts that no held leg and no order in
    /// flight accounts for (each; the first halts).
    pub unknown_positions: u64,
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

/// What a signed build is for: the guard it must pass.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Want<'a> {
    /// An order: [`guard_order`] against the request and the answer.
    Order(OrderAsk<'a>),
    /// A claim: [`guard_claim`].
    Claim,
}

/// Decode, parse, guard and sign the base64 transaction `b64` into `tx`;
/// its length. An order is signed only through [`guard_order`], a claim
/// only through [`guard_claim`].
fn guard_and_sign(
    b64: &[u8],
    tx: &mut [u8; TX_MAX],
    g: &TxGuard,
    want: &Want<'_>,
    signer: &Ed25519Signer,
) -> Result<usize, TxErr> {
    // COPY: the server's base64 transaction decoded into the fixed
    // 1 232 B buffer the guard parses and our signature is written into —
    // the wire format (base64 text) is not the bytes we sign — rejected:
    // none (the plan's own "decode into a fixed buffer").
    let n = base64_decode(b64, &mut tx[..]).map_err(|_| TxErr::Length)?;
    let parsed = SolTx::parse(&tx[..n])?;
    let passed = match want {
        Want::Order(ask) => guard_order(&tx[..n], &parsed, g, ask)?.0,
        Want::Claim => guard_claim(&tx[..n], &parsed, g)?,
    };
    parsed.sign_into(&mut tx[..n], passed.signer_idx, signer)?;
    Ok(n)
}

/// `requiredSigners` names our wallet alone — or, with the dormant
/// co-signer amendment pinned (G-A1), our wallet and the pinned key in
/// either order, no other key and no duplicate.
fn signers_ok(a: &BuildAnswer, body: &[u8], wallet_b58: &[u8], cosigner_b58: &[u8]) -> bool {
    let r0 = a.required[0].of(body);
    match a.n_required {
        1 => r0 == wallet_b58,
        2 if !cosigner_b58.is_empty() => {
            let r1 = a.required[1].of(body);
            (r0 == wallet_b58 && r1 == cosigner_b58) || (r0 == cosigner_b58 && r1 == wallet_b58)
        }
        _ => false,
    }
}

/// The build's answer → a signed transaction and its context, or why not
/// (`cosigner_b58` empty: no co-signer pinned).
#[allow(clippy::too_many_arguments)]
fn prepare(
    http: u16,
    body: &[u8],
    it: &JupIntent,
    market_id: u64,
    cfg: &JupGwCfg,
    wallet_b58: &[u8],
    cosigner_b58: &[u8],
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
    if !signers_ok(&a, body, wallet_b58, cosigner_b58) {
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
    // The order and position keys are what the gateway polls, reconciles
    // and releases by: both must read, and the guard holds the signed
    // create-order to them (risk review W5-1, MAJOR 1).
    let Some(order_key) = key_of(a.order_pubkey.of(body)) else {
        return Err(refusal(reason::BUILD_UNREADABLE, 1));
    };
    let Some(position_key) = key_of(a.position_pubkey.of(body)) else {
        return Err(refusal(reason::BUILD_UNREADABLE, 3));
    };
    let c = a.context.of(body);
    if c.len() > CONTEXT_MAX {
        return Err(refusal(reason::BUILD_UNREADABLE, 2));
    }
    // The signed bytes must be the order we asked: a Limit buy at or under
    // the member's limit, depositing exactly `deposit_micro` (whatever the
    // answer's words and the request's `build_extra` said), on the answer's
    // own order and position accounts, market hash and contracts.
    let ask = OrderAsk {
        deposit: it.deposit_micro,
        limit_micro: it.limit_px_1e6.max(0) as u64,
        order: order_key,
        position: position_key,
        market_hash: a.market_id_hash.of(body),
        contracts: a.contracts_micro,
    };
    let want = Want::Order(ask);
    let tx_len = match guard_and_sign(a.transaction.of(body), tx, &cfg.guard, &want, signer) {
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

/// One status read of an order ([`JupGateway`]'s recon by pubkey).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum StatusRead {
    /// A 200 that reads.
    Read(StatusAnswer),
    /// A 404: the venue does not know the order.
    NotFound,
    /// Nothing that reads: a failed request, another status, an unreadable
    /// body.
    Unread,
}

/// An in-doubt order whose status still answers 404 this long after its
/// execute never landed, ns: its blockhash lives ~150 blocks (≈ 60–90 s); 300 s bounds a
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
    /// The pinned co-signer, base58 (`cosigner_len` 0: none, LAW PMJ-G).
    cosigner_b58: [u8; B58_KEY_MAX],
    cosigner_len: usize,
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
    /// The debit halt's reference balance (-1: none yet — no build).
    debit_ref_micro: i64,
    /// The deposits sent since the reference (landed or in doubt).
    debit_allow_micro: u64,
    /// No balance read before this counts as "no order in flight".
    quiet_after_ns: u64,
    /// When the last balance read that read the deposit mint was taken.
    debit_read_ns: u64,
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
    /// (The dormant amendment) the pinned co-signer is the wallet itself,
    /// or the zero key.
    Cosigner,
    /// No vault is pinned (`guard_vault`): every order would be refused.
    Vault,
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
        let cosigner = cfg.guard.cosigner;
        if cfg.guard.has_cosigner && (cosigner == cfg.wallet || cosigner == [0; KEY_LEN]) {
            return Err(GwBootErr::Cosigner);
        }
        if cfg.guard.vault == [0; KEY_LEN] {
            return Err(GwBootErr::Vault);
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
        let mut cosigner_b58 = [0u8; B58_KEY_MAX];
        let cosigner_len = if cfg.guard.has_cosigner {
            base58_encode(&cfg.guard.cosigner, &mut cosigner_b58).map_err(|_| GwBootErr::Cosigner)?
        } else {
            0
        };
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
            cosigner_b58,
            cosigner_len,
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
            debit_ref_micro: -1,
            debit_allow_micro: 0,
            quiet_after_ns: 0,
            debit_read_ns: 0,
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

    /// The debit halt's state: the reference balance (-1: none yet) and
    /// the deposits sent since it.
    #[must_use]
    pub const fn debit_state(&self) -> (i64, u64) {
        (self.debit_ref_micro, self.debit_allow_micro)
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
        // The debit halt needs its reference before any deposit can leave,
        // a fresh one (failed reads would blind it), and room under the
        // deposits it allows before a quiet read re-anchors it.
        if self.debit_ref_micro < 0 {
            return self.refuse(oid, reason::NO_REFERENCE, 0, now);
        }
        if now > self.debit_read_ns.saturating_add(REFERENCE_MAX_AGE_NS) {
            return self.refuse(oid, reason::STALE_REFERENCE, now - self.debit_read_ns, now);
        }
        let allow_max = self.cfg.max_order_micro.saturating_mul(DEBIT_ALLOW_ORDERS);
        if self.debit_allow_micro.saturating_add(it.deposit_micro) > allow_max {
            return self.refuse(oid, reason::ALLOWANCE, self.debit_allow_micro, now);
        }
        let outcome = if it.outcome == 0 { Outcome::Up } else { Outcome::Down };

        // The build.
        let Some(n) = order_body(
            self.client.body_mut(),
            &self.wallet_b58[..self.wallet_len],
            market_id,
            outcome,
            it.deposit_micro,
            &self.mint_b58[..self.mint_len],
            &self.cfg.build_extra,
            it.limit_px_1e6.max(0) as u64,
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
                &self.cosigner_b58[..self.cosigner_len],
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
            seen_200: false,
            first_404_ns: 0,
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
        if !matches!(verdict, Landing::Refused(_)) {
            // The signed transaction may have left the host: its deposit may
            // leave the wallet (the debit halt's allowance), and no balance
            // read counts as quiet until it has settled.
            self.debit_allow_micro = self.debit_allow_micro.saturating_add(it.deposit_micro);
            self.quiet_after_ns = now.saturating_add(DEBIT_SETTLE_NS);
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

    /// One `GET /orders/status/{pk}` read of row `i`'s order (the build
    /// answer's `order.orderPubkey`).
    fn read_status(&mut self, i: usize) -> StatusRead {
        let r = self.rows[i];
        let mut k = [0u8; B58_KEY_MAX];
        let Ok(kl) = base58_encode(&r.order_key, &mut k) else {
            return StatusRead::Unread;
        };
        let Some(pn) = status_path(&mut self.path[..PATH_MAX], &k[..kl]) else {
            return StatusRead::Unread;
        };
        let t0 = core_time::now_ns();
        let key: Header<'_> = (b"x-api-key", &self.key[..self.key_len]);
        let res = self.client.request(Method::Get, &self.path[..pn], &[key], 0);
        match res {
            Ok((200, range)) => {
                // Any 200 for the order — readable or not — is the venue
                // answering about it: never again "never created".
                self.rows[i].seen_200 = true;
                self.end.stats.cancel_rtt.record(core_time::now_ns().saturating_sub(t0));
                let body = self.client.resp().get(range).unwrap_or(&[]);
                let mut a = StatusAnswer::default();
                match scan_status(body, &mut a) {
                    Ok(()) => StatusRead::Read(a),
                    Err(_) => StatusRead::Unread,
                }
            }
            Ok((404, range)) => {
                // Only the API's own `invalid_request_error` says "not
                // found". Every other kind (`api_error`, `rate_limit_error`,
                // `authentication_error`, `permission_error`,
                // `idempotency_error`, a region refusal) and a CDN's or
                // proxy's 404 (HTML, another shape) say nothing about the
                // order: in doubt still. The spec gives the error object no
                // fixed message, so the kind alone decides (a J0 fact to
                // tighten: the message Jupiter answers for an unknown order).
                let body = self.client.resp().get(range).unwrap_or(&[]);
                let mut e = ErrorAnswer::default();
                if scan_error(body, &mut e).is_ok() && e.kind == ErrKind::InvalidRequest {
                    StatusRead::NotFound
                } else {
                    StatusRead::Unread
                }
            }
            _ => StatusRead::Unread,
        }
    }

    /// The poll: a terminal status with its numbers books the order at
    /// once; a terminal one without them goes to recon.
    fn status(&mut self, i: usize, now: u64) {
        self.counters.polls += 1;
        let read = self.read_status(i);
        self.rows[i].next_ns = now.saturating_add(POLL_EVERY_NS);
        let a = match read {
            StatusRead::Read(a) => a,
            // Unknown to the venue (yet): polled again.
            StatusRead::NotFound => return,
            StatusRead::Unread => {
                self.counters.poll_failures += 1;
                return;
            }
        };
        match a.state {
            s if s.terminal() => {
                self.landed_late(i, now);
                if let Some(rec) = a.record() {
                    return self.finish(i, &rec, now);
                }
                self.rows[i].state = R_RECON;
                self.rows[i].next_ns = now;
            }
            OrderState::Pending | OrderState::Partial => {
                self.landed_late(i, now);
                self.rows[i].state = R_POLL;
            }
            _ => {}
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

    /// The SECONDARY read: the account's open-order pages
    /// ([`RECON_PAGES_MAX`] at most) for row `i`'s order — `None` when no
    /// page read it (pages left unread, or a page that failed, are no
    /// evidence of anything).
    fn recon_pages(&mut self, i: usize) -> Option<OrderRecord> {
        let r = self.rows[i];
        let mut k = [0u8; B58_KEY_MAX];
        let kl = base58_encode(&r.order_key, &mut k).unwrap_or(0);
        let mut page = 0u32;
        while page < RECON_PAGES_MAX {
            let pn = self.recon_target(page);
            self.counters.recons += 1;
            let key: Header<'_> = (b"x-api-key", &self.key[..self.key_len]);
            let res = self.client.request(Method::Get, &self.path[..pn], &[key], 0);
            let Ok((200, range)) = res else {
                return None;
            };
            let body = self.client.resp().get(range).unwrap_or(&[]);
            let mut rec = OrderRecord::default();
            match scan_orders(body, &k[..kl], &mut rec) {
                Ok((true, _)) => return Some(rec),
                Ok((false, true)) => page += 1,
                _ => return None,
            }
        }
        None
    }

    /// Recon by the order's pubkey (module docs): the status answer first,
    /// the open-order pages second; an in-doubt order is released only on
    /// a 404 past its blockhash's life.
    fn recon(&mut self, i: usize, now: u64) {
        let r = self.rows[i];
        self.counters.recons += 1;
        let read = self.read_status(i);
        match read {
            StatusRead::Read(a) => {
                if let Some(rec) = a.record() {
                    self.landed_late(i, now);
                    return self.finish(i, &rec, now);
                }
                match a.state {
                    // Known and still open past its deadline: read again
                    // later (a try spent; the pages add nothing).
                    OrderState::Pending | OrderState::Partial => {
                        self.landed_late(i, now);
                        return self.retry_recon(i, now, false);
                    }
                    // Terminal without its numbers: the venue knows it.
                    s if s.terminal() => self.landed_late(i, now),
                    // A word this reader does not know: no evidence.
                    _ => {}
                }
            }
            StatusRead::NotFound if !r.landed && !r.seen_200 => {
                if now < r.sent_ns.saturating_add(BLOCKHASH_LIFE_NS) {
                    // Its blockhash may still live: wait, spending no tries.
                    return self.retry_recon(i, now, true);
                }
                if r.first_404_ns == 0 {
                    self.rows[i].first_404_ns = now;
                    return self.retry_recon(i, now, true);
                }
                if now < r.first_404_ns.saturating_add(RELEASE_GAP_NS) {
                    return self.retry_recon(i, now, true);
                }
                // In doubt, never answered with a 200, and the API's own
                // 404 twice, RELEASE_GAP_NS apart, both past its
                // blockhash's life: it never landed.
                self.counters.done += 1;
                self.counters.released += 1;
                let mut ev = JupEvent::new(EVT_DONE, r.client_oid, now);
                ev.aux = 1;
                self.rows[i] = FREE_ROW;
                self.emit(&ev);
                return;
            }
            // A landed (or once answered) order the venue does not know, or
            // nothing read: the pages may still show it.
            StatusRead::NotFound | StatusRead::Unread => {}
        }
        match self.recon_pages(i) {
            Some(rec)
                if rec.state.terminal() && rec.has_fill && (rec.filled_micro == 0 || rec.has_avg) =>
            {
                self.landed_late(i, now);
                return self.finish(i, &rec, now);
            }
            // Listed, so it landed; terminal without its numbers, or still
            // open: not read — never booked unfilled.
            Some(_) => self.landed_late(i, now),
            None => {}
        }
        self.counters.recon_misses += 1;
        self.retry_recon(i, now, false);
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
        // anchor at 0 (that would hide every later loss) — nor the debit
        // halt's reference.
        if let (true, Some(usdc)) = (ok, b.usdc_micro) {
            self.usdc_micro = word(usdc);
            self.sol_lamports = b.sol_lamports.map_or(-1, word);
            ev.aux = usdc;
            ev.cost_micro = b.sol_lamports.unwrap_or(0);
            self.debit_check(now, usdc);
        } else {
            self.counters.balance_failures += 1;
            ev.reason = 1;
        }
        self.emit(&ev);
    }

    /// The debit halt at a balance read of `usdc` (the deposit mint, base
    /// units; module docs): checked against the reference minus the
    /// deposits sent since, first; then re-anchored when no order is in
    /// flight. A breach halts and never re-anchors.
    fn debit_check(&mut self, now: u64, usdc: u64) {
        self.debit_read_ns = now;
        if self.debit_ref_micro >= 0 {
            let floor = (self.debit_ref_micro as u64).saturating_sub(self.debit_allow_micro);
            if usdc < floor {
                self.counters.debit_halts += 1;
                self.end.stats.halt(HALT_DEBIT);
                return;
            }
        }
        if self.open_rows() == 0 && now >= self.quiet_after_ns {
            self.debit_ref_micro = word(usdc);
            self.debit_allow_micro = 0;
        }
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
        let rows = &self.rows;
        let mut gone = [0usize; HELD_MAX];
        let mut n_gone = 0usize;
        let mut unknown = 0u64;
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
            if p.contracts_micro == 0 {
                return;
            }
            // Contracts we must account for: a held leg on this position,
            // or an order in flight (landed, in doubt, in recon) on it. The
            // fill race cannot trip this: a row exists from before its
            // execute until the pass that books its fill, which inserts the
            // held leg before it frees the row. An unknown position is
            // never claimed.
            let mut known = false;
            let mut j = 0usize;
            while j < HELD_MAX {
                known |= held[j].state != 0 && held[j].position == k;
                j += 1;
            }
            let mut r = 0usize;
            while r < GW_ROWS {
                known |= rows[r].state != R_FREE && rows[r].position_key == k;
                r += 1;
            }
            if !known {
                unknown += 1;
                return;
            }
            if !p.claimable {
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
        if unknown > 0 {
            self.counters.unknown_positions += unknown;
            self.end.stats.halt(HALT_UNKNOWN_POSITION);
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
                        let b64 = a.transaction.of(body);
                        guard_and_sign(b64, &mut self.tx, &self.cfg.guard, &Want::Claim, &self.signer)
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
            word(c.debit_halts),
            word(c.released),
            word(c.unknown_positions),
            word(self.end.stats.halt_reason()),
            self.usdc_micro,
            self.sol_lamports,
            self.debit_ref_micro,
            word(self.debit_allow_micro),
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
const _: () = assert!(BODY_MIN >= ORDER_BODY_MAX);

#[cfg(test)]
mod tests;
