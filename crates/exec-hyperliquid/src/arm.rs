// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The engine's half of a Hyperliquid gateway arm (XH4d): an
//! [`OrderDispatch`] that never touches a socket. Its verbs push
//! [`HlCmd`]s into the gateway's command ring; its pump reads the
//! gateway's [`ExecEvent`]s and speaks the paper matcher's vocabulary back
//! to the engine, so a member's state machine runs unchanged, live.
//!
//! **The vocabulary** (what xmm expects of any arm, paper or live): an
//! accepted place or modify gets `RESTING` or a final event; a modify is
//! the new order's `RESTING`, then the old one's `CANCELED(REPLACED)`; a
//! refused replacement is the new order's `REJECTED` (the member cancels
//! the predecessor itself — and when the venue already ended it, its
//! `CANCELED` follows); every fill reaches the engine BEFORE its order's
//! `FILLED` (the pump runs inside `try_next_fill`, which the engine drains
//! before the order events of each iteration, and `FILLED` is held until
//! the fills cover the order or a join window passes); every end but a
//! fill is also a retirement (the router's resting row). A post-only order
//! that landed without an answer yet (the stream, a read, a fill or a
//! cancel said so) gets its `RESTING` first.
//!
//! **Sticky cancel intent** (maker plan §8.3): a cancel is the arm's to
//! deliver once asked — a full ring or a refusal keeps it as intent and
//! re-sends it; a cancel that overtook its place ("not found" about an
//! unanswered order) is re-sent when the place lands; "not found" about a
//! resting order means filled or cancelled elsewhere: the orders are read
//! at once, and the stream has `gone_ms` to say which (else
//! `CANCELED(OTHER)`).
//!
//! **In doubt (E-5):** a place or modify the gateway could not settle can
//! land until its `expiresAfter` and a clock skew after the verdict; then
//! a cancel by cloid settles it — success: it rested (`RESTING`, then
//! `CANCELED`); not found: it never landed (`REJECTED(EXPIRED)`). A cancel
//! asked meanwhile goes at once, and a "not found" before the doubt ends
//! keeps it for the doubt's end.
//!
//! **XH-7:** a post-only cross is information, not a fault — it feeds a
//! window of the last [`ALO_WINDOW`] answers to ALO places and modifies
//! (reported as permille; the router's `AloRejectRate` judges it), never
//! `reject_streak`.
//!
//! **Reconciliation** (maker plan §8.5–8.6), through the gateway's reads
//! (the WebSocket post channel; nothing blocks either thread): at boot,
//! after the first `userFills` snapshot, a sweep of the slot's orders,
//! then the perp sheet (positions seeded from it — fills at or before its
//! time are in it) and the open orders; then the sheet and the orders
//! every `recon_ms`, and the orders at once after a stream gap or a
//! "gone" inference. **A boot whose sheet holds a position in a bound
//! perp is held** — never ready, never reconciled — until XH5 rules on
//! it: the member boots flat and the router's ledger would be seeded over
//! a position it never saw. Drift is |booked − venue| at the venue's mark
//! and must survive two sheets (a fill in flight). On the slot's own
//! account a position in a perp the arm does not bind leaves it
//! unreconciled; on a shared account (testnet) such positions are other
//! slots' and a bound perp must be the arm's alone. **An order carrying
//! the slot's byte that the arm does not know is cancelled and, after
//! boot, pins the drift unbounded** (the router's `ReconDrift` halts the
//! slot) — including one the arm closed by inference that a read issued
//! after that close still lists. **The account's worth (XH5b-1)** is the
//! perp sheet's `accountValue` plus the spot sheet's USDC `total` — spot
//! USDC margins the perps (XH4g measured `accountValue` at $0 on E3): on
//! the slot's own account the spot sheet (`spotClearinghouseState`) is
//! read right after each perp sheet, and the slot is reconciled, its
//! agreement fresh and its P&L judged only with both halves landed. The
//! session's P&L anchor is restored from the owner's state file (E7: a
//! bound re-anchored per boot is a bound on nothing) or taken at the
//! first complete worth and handed to its keeper on the gateway's thread
//! ([`HlGatewayArm::keep_anchor_in`], XH5b-2) — the slot is reconciled
//! only once the keeper has stored it; it is judged on the slot's own
//! account only.
//!
//! **Cancel-all** marks every order it holds for cancel and asks the
//! gateway's sweep; `Clear` means the venue read clear AND no order the
//! arm holds can still be on the book or land. **Shutdown** keeps the
//! dead man armed until then, clears it only after, and at its bound
//! returns with the switch still armed if the venue never read clear.
//!
//! **What reaches the router** ([`HaltSignal`]): the stream gap (0 until a
//! gateway socket was first up — the documented "no observation"; then
//! the time since one was last up with a fresh gateway heartbeat, so a
//! dead gateway thread cannot hide behind a frozen gauge), the drift high
//! water, the streaks, `reconciled`, the reconciliation age, the P&L, the
//! ALO rate, and the gateway's halt word (read from the stats block: an
//! event can be lost, the word cannot), and the budget floor.
//!
//! **The address budget (XH4e, maker plan §8.7)** is read from the venue
//! (`userRateLimit`) after the boot's orders read and every `limit_ms`;
//! between reads the arm counts one address request per item it pushes —
//! a place, a modify, a cancel, a control post (never exempted as a
//! cancel) — and per cancel of the gateway's own sweeps (its stats word),
//! and takes one back for each the gateway refused unsent. Every count
//! errs high: the requests unanswered when a read goes and every one
//! pushed while it is in flight are added on top of its answer (some may
//! count twice), and no fill is credited between reads (the next read
//! states the volume they earned). The arm is not ready before the first
//! answer, and growth is refused once no read has answered for
//! [`LIMIT_STALE_PERIODS`] periods. At or below `budget_floor` (at least
//! [`MIN_BUDGET_FLOOR`]) places and modifies are refused and the router's
//! `BudgetFloor` halts the slot; the dead man stays armed while the arm
//! holds an order or the halt's cancel-all is not read clear — the floor
//! is the headroom for exactly that cleanup — and parks once the arm is
//! flat and the venue reads clear (its re-arms alone cost ≈ 17 280
//! requests a quoting day).
//!
//! **Top-ups (XMM XH5c-2: S7-L1's policy, one governor for both arms)** —
//! on the slot's own account, armed at boot ([`HlGatewayArm::set_topup`]),
//! once the session's anchor is durable: at a budget read with no purchase
//! in flight the band rule decides a `reserveRequestWeight`
//! ([`TopupGovernor::take`]: inside `(floor, floor + weight]` and the
//! day's max — the day charged with the decision); the governor's state
//! goes to its keeper on the gateway's thread ([`crate::topup`]), and the
//! purchase leaves only once it is stored — built in place, pushed by
//! reference, one address request, its answer routed by its own client-id
//! range. Refused before it left: its request taken back and the day's
//! charge returned — never bought, decided again at a later read (one a
//! newer read leaves out of the band before it left is abandoned so).
//! Accepted or in doubt: the next budget read issued after the answer must
//! show half its weight apart from what volume earned; once a perp sheet
//! issued after the answer — after the read that showed it, for one in
//! doubt: the worth that carries the price — has landed too, its price is
//! the session's spend, which the session P&L adds back (Q-XH5-2: bounded
//! by the day's max instead). A purchase not seen, a venue refusal or a
//! state its keeper could not store stops the top-ups (handed over as the
//! day spent). A halted slot (its sweep asked) and the shutdown buy
//! nothing more; a purchase that left is still judged and charged.
//!
//! **The day's spend (XH4e, S7-L1 gap A)** is read once, after the first
//! budget read: the slot's own fills since 00:00Z (`userFillsByTime`,
//! page after page — each from the last one's latest row, a read period
//! later, never while the gateway's rates read throttled — until a page
//! adds nothing past its start: the venue's page size is not taken on
//! trust), summed as |px × sz| — an upper bound on the day's increasing
//! turnover — and handed to the router
//! ([`OrderDispatch::venue_day_bought`]); the arm reports `reconciled`
//! only once it is read, so the router's day cap never restarts at a full
//! allowance on a boot. Every page ends at the time the first page went
//! (taken again each time it is asked; later fills are the live ledger's),
//! so the pages converge. The venue serves an account's 10 000 most recent
//! fills, counted to the moment it answers: a read whose rows plus the
//! account's fills since its end (any slot's, as the stream brings them)
//! reach them — or that meets a stream gap which may have lost fills
//! while it holds its end, needs more than [`DAY_PAGES_MAX`] pages, meets
//! a full page that cannot move, or a page whose rows ran backward in time
//! (its next start could skip some) — reads the day spent (fail closed). A
//! failed budget or day read is asked again a read period later. A fill is
//! stamped with the venue's time (wall), as the blocking arm stamps it:
//! the router's day epoch is wall time.
//!
//! Engine thread, 0 B/op after boot (gate 98): fixed tables, fixed
//! queues, linear scans over at most [`ARM_ORDERS`] rows.

use std::sync::Arc;

use clob_dispatcher::{
    CancelAllState, DispatchError, DispatchStats, GatewayArmReport, HaltSignal, OrderDispatch,
    Renamed, Retired, RETIRED_CANCELED_MEMBER, RETIRED_CANCELED_TTL, RETIRED_CANCELED_VENUE,
    RETIRED_EXPIRED, RETIRED_FILLED, RETIRED_REJECTED,
};
use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_time::now_ns;
use core_types::{
    CancelReq, Fill, ModifyReq, Order, OrderEvent, Price, Qty, Side, VenueId, FILL_ORIGIN_VENUE,
    ORDER_EVENT_CANCELED, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_BAD_ALO_PX,
    ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_EVENT_REASON_EXPIRED, ORDER_EVENT_REASON_NONE,
    ORDER_EVENT_REASON_OTHER, ORDER_EVENT_REASON_REPLACED, ORDER_EVENT_REJECTED,
    ORDER_EVENT_RESTING,
};
use exec_gateway::stats::{CONGESTION_THROTTLED, SWEEP_CLEAR, SWEEP_STRANDED};
use exec_gateway::{boxed_array, HALT_NONE};

use crate::action::Tif;
use crate::anchor::AnchorSlot;
use crate::budget::{AddressBudget, TopupGovernor};
use crate::cmd::{HlCmd, CMD_CANCEL};
use crate::dayspend::{utc_day, DAY_MS, VENUE_PAGE_MAX};
use crate::deadman::{DeadMan, DeadManCfg};
use crate::event::{
    never_left, ExecEvent, ASSET_UNKNOWN, EVF_BUY, EVF_CROSSED, EVF_FILLED, EVF_POST_ITEM, EV_ACK,
    EV_CANCELED, EV_FILL, EV_FILLED, EV_FILL_META, EV_GAP, EV_HALT, EV_OPEN_ORDER, EV_POSITION,
    EV_READ_DONE, EV_READ_FAILED, EV_REJECTED, EV_RESTING, EV_SNAPSHOT, EV_UNANSWERED,
    GAP_UPDATES_UNREADABLE, READ_DAY, READ_LIMIT, READ_ORDERS, READ_SPOT, READ_STATE,
    REASON_IOC_MISS, REASON_NOT_FOUND, REASON_THROTTLED,
};
use crate::gateway::{pw, HlEngineEnd};
use crate::nonce::now_ms;
use crate::rules::{PerpRules, Round};
use crate::topup::{Keep, TopupSlot};
use crate::ttl::TTL_MAX_MS;

/// Orders one arm tracks at once (xmm rests at most 16: four perps, two
/// sides, and a modify's predecessor each).
pub const ARM_ORDERS: usize = 128;
/// Perps one arm binds.
pub const ARM_PERPS: usize = 16;
// Both index by mask (the checks fold).
const _: () = assert!(ARM_ORDERS.is_power_of_two() && ARM_PERPS.is_power_of_two());
/// XH-7: the ALO window — the last 200 answers to ALO places and modifies.
pub const ALO_WINDOW: usize = 200;
/// The window's rate is reported once it holds this many answers.
const ALO_MIN: u32 = 50;
/// Each out-queue (order events, fills, retirements); a power of two.
const OUT: usize = 1024;
const _: () = assert!(OUT.is_power_of_two());
/// Gateway events one pump takes.
const PUMP_MAX: usize = 256;
/// The most one gateway event or one row timer puts on an out-queue: a
/// modify's replacement that lands by its end (a fill, a `filled` row, a
/// cancel) is its `RESTING`, the predecessor's `CANCELED(REPLACED)` and
/// its own end — one rename (XH4f) and one retirement.
const STEP_ROOM: usize = 3;
/// Ids the arm finished lately (one `u64` holds whether each end was
/// inferred).
const FINISHED: usize = 64;
const _: () = assert!(FINISHED == 64);
/// How long a finished id is remembered.
const FINISHED_NS: u64 = 30_000_000_000;
/// Fills held while the perp sheet is not seeded yet.
const PRESEED: usize = 64;
/// A cancel the ring refused, or the venue refused for a reason other
/// than "not found", is re-sent after this.
const CANCEL_RETRY_NS: u64 = 500_000_000;
/// A read with no verdict this long after its post timeout is abandoned
/// (the gateway always gives one, LAW E-5: a guard, not a path).
const READ_GRACE_NS: u64 = 5_000_000_000;
/// The dead man stays wanted this long after the last order closed: an
/// arm between two quotes is not an arm with nothing to protect, and a
/// clear and a re-arm per requote would spend the address budget.
const WANT_HOLD_NS: u64 = 30_000_000_000;
/// A gateway heartbeat older than this is a gateway that stopped turning.
const ALIVE_NS: u64 = 2_000_000_000;
/// The largest clock skew a boot takes (the venue's 5 s lead).
const MAX_SKEW_MS: u64 = 5_000;
/// The largest post timeout, read period, join and gone window a boot
/// takes, ms (every `× NS_PER_MS` stays far from overflow).
const MAX_WINDOW_MS: u64 = 60_000;
/// The largest shutdown bound a boot takes, ms: the engine's stop never
/// waits on the venue longer.
const MAX_SHUTDOWN_MS: u64 = 30_000;
/// XH4e — the longest the budget goes unread, ms (between reads the arm
/// counts its own requests).
const MAX_LIMIT_MS: u64 = 600_000;
/// XH4e — the venue's page of `userFillsByTime` ([`VENUE_PAGE_MAX`]: as
/// documented, unmeasured) in a day page's row count: a full page that
/// cannot move past its start leaves rows unread.
const DAY_PAGE: u64 = VENUE_PAGE_MAX as u64;
/// XH4e — the venue serves an account's 10 000 most recent fills, counted
/// to the moment it answers: a day read whose rows plus the account's
/// fills since its end reach them may have lost the day's start, and the
/// day reads spent.
const DAY_ROWS_MAX: u32 = 10_000;
/// XH4e — the most pages a day read takes (its cost: up to 120 IP weight a
/// page); a read that needs more reads the day spent.
const DAY_PAGES_MAX: u8 = 20;
/// XH4e — what a day the arm could not read to its start reports, USD
/// ×1e8: spent (every sum the router adds it to saturates).
const DAY_SPENT_1E8: i64 = i64::MAX;
/// XH4e — a budget no read has answered for this many `limit_ms` is
/// stale: growth is refused until one does (the count alone drifts —
/// another user of the address, a request the venue counted unseen).
const LIMIT_STALE_PERIODS: u64 = 3;
/// XH4e — the fewest requests a `budget_floor` may keep: at the floor the
/// arm still cancels every order it can hold twice over (its own cancel
/// and the gateway sweep's) and keeps the dead man armed through the
/// cleanup — 64 re-arms, five minutes of them at the default period.
pub const MIN_BUDGET_FLOOR: u64 = 2 * ARM_ORDERS as u64 + 64;
/// Arm-internal client ids (the dead man's posts, the reads, the leverage
/// posts and the top-ups' purchases): never cloids; their post items carry
/// command bits 0, an order's never do.
const DEADMAN_OID_BASE: u64 = 1 << 63;
const READ_OID_BASE: u64 = (1 << 63) | (1 << 62);
/// XH4f (O-XH6): a perp's leverage post, the perp's index in the low bits.
/// Under `READ_OID_BASE` it reads as the arm's own control post (refunded
/// when refused unsent), and the dead man matches its own id exactly.
const LEVERAGE_OID_BASE: u64 = (1 << 63) | (1 << 61);
/// XMM XH5c-2: a top-up's purchase (`reserveRequestWeight`), the purchases'
/// count in the low bits ([`TOPUP_SEQ_MASK`]). Bit 63 set, bit 62 clear:
/// the arm's own control post (refunded when refused unsent); bit 60 its
/// range's own (the leverage range's bit 61 clear).
const TOPUP_OID_BASE: u64 = (1 << 63) | (1 << 60);
/// The low bits a purchase's count takes.
const TOPUP_SEQ_MASK: u64 = (1 << 60) - 1;
// The four ranges never meet: the dead man's ids stay far under bit 60.
const _: () = assert!(
    TOPUP_OID_BASE & !TOPUP_SEQ_MASK == TOPUP_OID_BASE
        && LEVERAGE_OID_BASE & !TOPUP_SEQ_MASK != TOPUP_OID_BASE
        && READ_OID_BASE & !TOPUP_SEQ_MASK != TOPUP_OID_BASE
        && TOPUP_OID_BASE & READ_OID_BASE == DEADMAN_OID_BASE
);
/// XH4f: the highest leverage an arm sets (the venue's own ceiling on its
/// largest perps).
pub const MAX_LEVERAGE: u8 = 50;
/// Wire 1e8 per engine 1e6.
const WIRE_PER_ENGINE: i64 = 100;
const NS_PER_MS: u64 = 1_000_000;

// XH4f: the member reads the gateway's congestion word as its own.
const _: () = assert!(
    exec_gateway::stats::CONGESTION_NONE == core_types::EXEC_CONGESTION_NONE as u64
        && exec_gateway::stats::CONGESTION_BUSY == core_types::EXEC_CONGESTION_BUSY as u64
        && exec_gateway::stats::CONGESTION_THROTTLED
            == core_types::EXEC_CONGESTION_THROTTLED as u64
        && exec_gateway::stats::CONGESTION_RATE_LIMITED
            == core_types::EXEC_CONGESTION_RATE_LIMITED as u64
);

// Row states.
/// Pushed; not answered yet.
const ST_SENT: u8 = 1;
/// On the venue's book.
const ST_RESTING: u8 = 2;
/// The gateway could not settle the place or modify (E-5).
const ST_DOUBT: u8 = 3;
/// The venue said "not found" about it resting, a read stopped listing
/// it, or the stream ended it while its modify was in flight.
const ST_GONE: u8 = 4;
/// Filled per the venue; `FILLED` waits for the fills.
const ST_FILLING: u8 = 5;
// Sticky cancel intent.
const CX_NONE: u8 = 0;
const CX_WANTED: u8 = 1;
const CX_SENT: u8 = 2;
// Row bits.
/// An ask.
const RB_ASK: u8 = 1;
/// Post-only: XH-7 counts its answers, and it rests before it fills.
const RB_ALO: u8 = 1 << 1;
/// The member asked to cancel it.
const RB_ASKED: u8 = 1 << 2;
/// A modify of it is in flight.
const RB_REPLACING: u8 = 1 << 3;
/// The stream ended it (`end_reason`) while its modify was in flight.
const RB_ENDED: u8 = 1 << 4;
/// Thought gone, and an orders read issued since did not list it: its
/// gone wait alone ends it.
const RB_UNLISTED: u8 = 1 << 5;
/// XMM XH5d: its RESTING went out with the venue's time (the `open` row's):
/// none repeats.
const RB_STAMPED: u8 = 1 << 6;
// XMM XH5c-2: a top-up's phases.
/// None in flight: a budget read may decide one.
const TU_IDLE: u8 = 0;
/// Decided, the day charged, its state handed to the keeper: it leaves
/// once stored.
const TU_STORING: u8 = 1;
/// Pushed: its answer awaited.
const TU_SENT: u8 = 2;
/// Accepted, or in doubt: the next budget read issued after the answer
/// judges it, and its price waits for a worth that carries it.
const TU_SEEN: u8 = 3;
// Boot phases.
const BOOT_SNAPSHOT: u8 = 0;
const BOOT_SWEEP: u8 = 1;
const BOOT_STATE: u8 = 2;
const BOOT_ORDERS: u8 = 3;
const BOOT_DONE: u8 = 4;

/// One perp the arm trades: the engine symbol, the venue asset, its rules.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PerpBind {
    /// The engine symbol.
    pub sym: u32,
    /// The venue asset index (`/info meta`'s universe position).
    pub asset: u32,
    /// Its price and size rules (LAW XH-4).
    pub rules: PerpRules,
}

/// An arm's configuration (XH4f composes it from `exec.toml`, the boot
/// binding and the owner's state files).
#[derive(Copy, Clone, Debug)]
pub struct ArmCfg {
    /// The strategy slot this arm serves (the gateway's slot mask holds it).
    pub slot: u8,
    /// The perps it trades; the first `n_perps` are bound.
    pub perps: [PerpBind; ARM_PERPS],
    /// How many of `perps` are bound.
    pub n_perps: usize,
    /// `expiresAfter` on every place and modify, ms (> 0: an order in
    /// doubt is settled once it can no longer land).
    pub expires_after_ms: u16,
    /// The gateway's post timeout, ms (= the dead man's `answer_ms`).
    pub post_timeout_ms: u64,
    /// The venue's clock against ours, ms (≤ 5 s).
    pub skew_ms: u64,
    /// The sheet and the orders are read this often, ms — and the spot
    /// sheet with them on an account of the slot's own (XH5b-1).
    pub recon_ms: u64,
    /// `FILLED` waits this long for its fills, ms.
    pub join_ms: u64,
    /// An order the venue says is gone waits this long for the stream to
    /// say how, ms.
    pub gone_ms: u64,
    /// The shutdown's bound, ms (≤ 30 s): the sweep confirmed, then the
    /// dead man cleared — or, at the bound, the switch left as it is
    /// (armed if the venue keeps a schedule; counted).
    pub shutdown_ms: u64,
    /// The account holds only this slot's orders and positions (slot 6's
    /// own master): the dead man needs no read to know it, unbound
    /// positions are strangers, and the account's worth — the perp sheet's
    /// value plus the spot sheet's USDC (XH5b-1) — is the slot's P&L.
    pub own_account: bool,
    /// The session's P&L anchor restored from the owner's state file, the
    /// account's worth USD ×1e6; 0 = none (the first complete worth
    /// anchors, and the arm hands it to its keeper:
    /// [`HlGatewayArm::keep_anchor_in`]).
    pub anchor_1e6: i64,
    /// The dead-man switch's timing.
    pub deadman: DeadManCfg,
    /// XH4e: the address budget's floor, requests (the exec.toml
    /// `request_budget_floor`, at least [`MIN_BUDGET_FLOOR`]): at or below
    /// it places and modifies are refused, the router halts the slot, and
    /// the dead man parks once nothing of ours can rest.
    pub budget_floor: u64,
    /// XH4e: the budget is read from the venue this often, ms (≤ 10 min).
    pub limit_ms: u64,
    /// XH4f (O-XH6): every bound perp's leverage, cross margin, set before
    /// the arm takes orders (≤ [`MAX_LEVERAGE`]); `0` = the venue's, unset.
    pub leverage: u8,
}

/// Why an arm cannot be built.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArmErr {
    /// A slot outside the engine's eight.
    Slot,
    /// No perps, more than [`ARM_PERPS`], or one bound twice.
    Perps,
    /// A zero expiry, post timeout, read period, join, gone, shutdown or
    /// budget period, or one above its bound (60 s; the shutdown 30 s; the
    /// budget 10 min); a skew above 5 s; a dead-man answer that is not the
    /// post timeout.
    Timing,
    /// The dead-man timing does not hold.
    DeadMan,
    /// XH4e: a budget floor below [`MIN_BUDGET_FLOOR`].
    Budget,
    /// XH4f: a leverage above [`MAX_LEVERAGE`].
    Leverage,
    /// XMM XH5c-2: top-ups armed on an account not the slot's own, or a
    /// weight past the purchase command's `u32` (refused, never truncated).
    Topup,
}

/// What the arm has seen (its `/metrics` block, XH4f).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ArmCounters {
    /// Places and modifies pushed.
    pub pushed: u64,
    /// Submits and modifies refused before the ring: another slot, an
    /// unbound symbol, a rule, a kind, a client id in use.
    pub refused_local: u64,
    /// Pushes the command ring refused (full).
    pub ring_full: u64,
    /// Order events sent to the engine.
    pub events: u64,
    /// Fills sent to the engine.
    pub fills: u64,
    /// Fills of another slot (tape only).
    pub fills_foreign: u64,
    /// Fills on an asset the arm does not bind.
    pub fills_unresolved: u64,
    /// Fees, USD ×1e8 (signed; rebates negative).
    pub fees_1e8: i64,
    /// Fills that crossed (taker).
    pub fills_crossed: u64,
    /// Answers about an order the arm does not hold.
    pub stray: u64,
    /// Post-only crosses (XH-7).
    pub alo_rejects: u64,
    /// Other venue refusals of places and modifies.
    pub rejects: u64,
    /// Orders in doubt settled by their cancel.
    pub doubt_settled: u64,
    /// Orders that ended unseen (`CANCELED(OTHER)` after the gone wait).
    pub closed_unseen: u64,
    /// `FILLED` sent before its fills covered the order (join timeout).
    pub join_timeouts: u64,
    /// Unknown orders of the slot a read found after boot (each
    /// cancelled; one closed unseen and listed again counts).
    pub unknown_orders: u64,
    /// Reads answered.
    pub reads: u64,
    /// Reads that failed (refused, unanswered, unreadable).
    pub reads_failed: u64,
    /// Fills before the seeding sheet that did not fit its hold.
    pub preseed_dropped: u64,
    /// Fills that arrived during the shutdown, after the engine stopped
    /// draining (the venue's sheet holds them for the next boot).
    pub shutdown_fills: u64,
    /// Shutdowns that reached their bound unconfirmed: the venue may
    /// still hold orders of ours, and only the dead man's schedule — if
    /// the venue keeps one ([`DeadMan::availability`],
    /// [`DeadMan::deadline_ms`]) — cancels them. XH4f logs and alerts.
    pub shutdown_unconfirmed: u64,
    /// Shutdowns that reached their bound with the sweep read clear but
    /// the switch's clear not confirmed: nothing of ours rests, but the
    /// venue may still fire the switch on an empty book (a trigger spent).
    pub shutdown_switch_held: u64,
    /// Out-queue entries lost to a full queue (the room checks make it a
    /// bug).
    pub dropped: u64,
    /// Places and modifies refused at the budget floor, or while the
    /// budget is stale (XH4e).
    pub refused_budget: u64,
    /// Places and modifies the gateway refused unsent over its rate
    /// shares (XH4e: `REASON_THROTTLED`; never a venue reject).
    pub throttled: u64,
    /// Budget reads answered (XH4e).
    pub limit_reads: u64,
    /// Spot sheets answered with their USDC row (XH5b-1).
    pub spot_reads: u64,
    /// Spot reads that failed — refused, unanswered, an end of another
    /// kind, a sheet that lists no USDC (XH5b-2).
    pub spot_failed: u64,
    /// Pages of the day's fills read (XH4e).
    pub day_pages: u64,
    /// Days read spent because their start may be unread (XH4e): the
    /// venue's 10 000 reached (the rows read plus the account's fills since
    /// the read's end), a stream gap while the read held its end, the page
    /// bound, a full page that cannot move, or rows out of time order.
    pub day_unreadable: u64,
    /// Requests taken back from the budget: the gateway refused them
    /// before they left (XH4e).
    pub refunded: u64,
    /// XH4f (O-XH6): perps whose leverage the venue refused or never
    /// answered — dark: growth on them is refused.
    pub leverage_dark: u64,
    /// Places and modifies refused on a dark perp (XH4f).
    pub refused_leverage: u64,
    /// XMM XH5c-2: top-ups seen on the venue — their price charged to the
    /// session's spend.
    pub topup_ok: u64,
    /// XMM XH5c-2: top-up stops — a purchase the venue refused or the next
    /// read did not show, a state its keeper could not store, an answer for
    /// no purchase in flight.
    pub topup_failed: u64,
    /// XMM XH5c-2: purchases that never left — refused unsent (halted, not
    /// ready, throttled), a full ring, or abandoned before they left (a
    /// newer read out of the band, the slot's halt, the shutdown): never
    /// bought, the day's charge returned.
    pub topup_unsent: u64,
}

/// XH4g: the reconciliation as the arm last read it ([`HlGatewayArm::recon_view`]).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconView {
    /// When the perp sheet was last answered (monotonic ns; 0 = never).
    pub sheet_ns: u64,
    /// When the open orders were last answered (monotonic ns; 0 = never).
    pub orders_ns: u64,
    /// The last sheet agreed with the book in every bound perp, and held no
    /// position the arm cannot place.
    pub state_ok: bool,
    /// The last orders read found no order of the slot the arm does not hold.
    pub orders_ok: bool,
    /// The drift high water, USD ×1e6 (a disagreement that survived two
    /// sheets).
    pub drift_hw_1e6: i64,
}

/// One tracked order. One cache line.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default)]
struct Row {
    client_oid: u64,
    /// A modify's predecessor until the modify lands or not; 0 = none.
    prev: u64,
    /// When the row's wait ends (a doubt, gone, the join, a cancel
    /// retry); 0 = none.
    due_ns: u64,
    /// The last state change (in doubt: the verdict).
    since_ns: u64,
    qty_1e8: i64,
    filled_1e8: i64,
    /// Index into the bound perps.
    perp: u8,
    state: u8,
    cancel: u8,
    /// `RB_*`.
    bits: u8,
    /// The read epoch that last listed it.
    seen: u8,
    /// The stream's reason, with `RB_ENDED`.
    end_reason: u8,
}

const _: () = assert!(core::mem::size_of::<Row>() == 64);
const _: () = assert!(core::mem::align_of::<Row>() == 64);

/// A fixed out-queue. XH4f: `ready` holds back what was pushed since the
/// last [`Out::release`] from [`Out::pop_released`] (the retirements a
/// poll behind their orders' fills).
struct Out<T: Copy> {
    buf: Box<[T; OUT]>,
    head: usize,
    len: usize,
    ready: usize,
}

impl<T: Copy> Out<T> {
    fn new(blank: T) -> Self {
        Self {
            buf: boxed_array(blank),
            head: 0,
            len: 0,
            ready: 0,
        }
    }

    /// Everything pushed so far becomes poppable by [`Out::pop_released`].
    #[inline(always)]
    fn release(&mut self) {
        self.ready = self.len;
    }

    /// The next released entry.
    #[inline(always)]
    fn pop_released(&mut self) -> Option<T> {
        if self.ready == 0 {
            return None;
        }
        self.ready -= 1;
        self.pop()
    }

    #[inline(always)]
    fn room(&self) -> usize {
        OUT - self.len
    }

    /// `false`: full.
    #[inline(always)]
    fn push(&mut self, v: T) -> bool {
        if self.len == OUT {
            return false;
        }
        // COPY: one entry (a 64 B order event or fill, a 32 B rename, a
        // 16 B retirement) into its slot — the queue IS the hand-over: one gateway event
        // makes up to three entries, and a FILLED waits across the
        // engine's separate fill and event drains (the join); rejected: a
        // queue of references (these are new values, not the gateway's).
        self.buf[(self.head + self.len) & (OUT - 1)] = v;
        self.len += 1;
        true
    }

    /// The next entry into `out`; `false`: empty.
    #[inline(always)]
    fn pop_into(&mut self, out: &mut T) -> bool {
        if self.len == 0 {
            return false;
        }
        // COPY: one 64 B order event into the engine's scratch — the
        // trait's out-parameter, one copy (no `Option` by value);
        // rejected: a borrowed peek-and-advance on `OrderDispatch`, a
        // trait change across every arm and the router for 64 B.
        *out = self.buf[self.head & (OUT - 1)];
        self.head = (self.head + 1) & (OUT - 1);
        self.len -= 1;
        true
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        // COPY: one entry (a 64 B fill, a 32 B rename, a 16 B retirement)
        // returned by value — the trait's `try_next_fill` /
        // `try_next_renamed` / `try_next_retired` (through `pop_released`)
        // return owned values (a fill's `Option` has a niche: 64 B).
        let v = self.buf[self.head & (OUT - 1)];
        self.head = (self.head + 1) & (OUT - 1);
        self.len -= 1;
        Some(v)
    }

    /// Drop every entry; how many there were.
    #[inline(always)]
    fn clear(&mut self) -> usize {
        let n = self.len;
        self.head = 0;
        self.len = 0;
        self.ready = 0;
        n
    }
}

/// One perp's reconciliation state.
#[derive(Copy, Clone, Debug, Default)]
struct PerpState {
    /// Signed position the arm booked, coin ×1e8.
    booked_1e8: i64,
    /// The last sheet's signed position, coin ×1e8.
    venue_1e8: i64,
    /// The last sheet's position value at the venue's mark, USD ×1e8.
    value_1e8: u64,
    /// The last fill's price (the mark when the sheet holds no position).
    last_px_1e8: i64,
}

/// The engine's half of one Hyperliquid gateway arm.
pub struct HlGatewayArm {
    cfg: ArmCfg,
    end: HlEngineEnd,
    rows: Box<[Row; ARM_ORDERS]>,
    n_rows: usize,
    events: Out<OrderEvent>,
    fills: Out<Fill>,
    retired: Out<Retired>,
    /// XH4f: the modifies that landed, for the router's rename.
    renamed: Out<Renamed>,
    counters: ArmCounters,
    stats: DispatchStats,
    // XH-7 and the streaks.
    alo: Box<[u8; ALO_WINDOW]>,
    alo_at: usize,
    alo_n: u32,
    alo_bad: u32,
    reject_streak: u32,
    asset_refusal_streak: u32,
    // Liveness: when the stream was last seen alive (0 = never).
    up_ns: u64,
    snapshot: bool,
    // Boot and reads.
    boot: u8,
    /// The seeding sheet held a position in a bound perp: held for XH5.
    boot_held: bool,
    read_seq: u64,
    /// The read in flight (0 = none), its kind, when it went.
    read_oid: u64,
    read_kind: u8,
    read_ns: u64,
    next_state_ns: u64,
    next_orders_ns: u64,
    epoch: u8,
    /// An order was inferred gone: read the orders at once (a listing
    /// brings it back before its gone wait ends).
    reread: bool,
    /// This orders read saw another slot's (or a stranger's) orders.
    others: bool,
    /// No other slot's order was seen since the last orders read that saw
    /// none (a gap or another slot's resting row ends it).
    exclusive: bool,
    /// When exclusivity was last lost: only an orders read issued since
    /// restores it.
    excl_lost_ns: u64,
    unknown_this_read: u32,
    // Positions.
    perps: Box<[PerpState; ARM_PERPS]>,
    seeded: bool,
    seed_ms: u64,
    preseed: Box<[(u8, i64, u64); PRESEED]>,
    n_preseed: usize,
    unknown_positions: u32,
    worst_prev_1e6: i64,
    drift_hw_1e6: i64,
    state_ok: bool,
    orders_ok: bool,
    /// When the last sheet and the last orders read ended: an agreement
    /// needs both fresh.
    sheet_ns: u64,
    orders_ns: u64,
    last_agree_ns: u64,
    anchor_1e8: i64,
    account_1e8: i64,
    /// XH5b-1: the spot sheet's USDC `total` — the worth's second half on
    /// the slot's own account — and when it was last read (0 = never); the
    /// next spot read (`u64::MAX`: none due — one goes right after each
    /// perp sheet).
    spot_1e8: i64,
    spot_ns: u64,
    next_spot_ns: u64,
    anchored: bool,
    /// The anchor was taken this boot (its keeper stores it).
    fresh_anchor: bool,
    /// XH5b-2: the anchor is durable — restored from the state file, or
    /// read back stored by its keeper (on the slot's own account the first
    /// reconciliation waits for it).
    anchor_kept: bool,
    /// XH5b-2: where a fresh anchor is handed to its keeper (the gateway's
    /// thread); `None` — no keeper: a fresh anchor is never kept.
    keep: Option<Arc<AnchorSlot>>,
    // The arm's own ends: the client ids, when, whether inferred (bit k),
    // the next slot.
    done_ids: Box<[u64; FINISHED]>,
    done_ns: Box<[u64; FINISHED]>,
    done_inferred: u64,
    done_at: usize,
    // The sweep and the dead man.
    sweep_asked: u64,
    hold_until_ns: u64,
    deadman: DeadMan,
    /// The dead man's command, and a top-up's purchase (XMM XH5c-2), each
    /// built in place — one at a time, on the engine thread — and pushed
    /// by reference.
    scratch: HlCmd,
    // XH4e: the address budget (the venue's figures at the last read and
    // every request since), whether one was read, the next read, the
    // requests pushed while one is in flight, the sweep cancels charged.
    budget: AddressBudget,
    limit_ok: bool,
    limit_due_ns: u64,
    limit_sent: u64,
    sweep_charged: u64,
    // XH4e: the day's spend — every page's end (venue ms, taken at each
    // ask of the first page; its UTC day is the day read), the next page's
    // start, when it goes, the sum so far (USD ×1e8), done.
    day_until_ms: u64,
    day_from_ms: u64,
    day_due_ns: u64,
    day_bought_1e8: i64,
    day_done: bool,
    /// Day pages read, and the rows toward the venue's 10 000: the rows
    /// read plus the account's fills since the end (the bounds end the
    /// read spent).
    day_pages_read: u8,
    day_rows: u32,
    /// When the budget goes stale: three periods after the last answer.
    limit_stale_ns: u64,
    // XH4f (O-XH6): each bound perp's leverage (bit = perp) — asked, set,
    // dark — and when a leverage refused unsent is asked again.
    lev_sent: u16,
    lev_ok: u16,
    lev_dark: u16,
    lev_due_ns: u64,
    // XMM XH5c-2: the top-ups — the governor (the policy, the day's count,
    // the session's spend), the keeper's hand-over (`None`: not armed), the
    // phase, the hand-over's version, the purchases' count (the id's low
    // bits), the headroom apart from volume a purchase was decided at, its
    // read mark (the reads issued when it was answered — a later one judges
    // it — or, answered in doubt, the read that showed it: a sheet issued
    // later carries it), the last perp sheet's read number (its spot half
    // pairs with it); whether the purchase in flight was answered in doubt,
    // was seen on the venue, is inside the worth; whether a hand-over is
    // owed to the keeper; whether the halt or the shutdown ended the buying
    // (the accounting goes on).
    topup: TopupGovernor,
    topup_keep: Option<Arc<TopupSlot>>,
    topup_phase: u8,
    topup_ver: u64,
    topup_seq: u64,
    topup_before: i64,
    topup_mark: u64,
    topup_sheet: u64,
    topup_doubt: bool,
    topup_shown: bool,
    topup_in_worth: bool,
    topup_owed: bool,
    topup_off: bool,
    // XMM XH5d: the ALO priority's HYPE — the session's cap
    // (`set_priority_cap`; 0, the default: no place bids one) and what the
    // places sent committed, USD ×1e6.
    priority_cap_1e6: u32,
    priority_spent_1e6: u32,
}

// The arm's inline state, 1 792 B — what `new` moves into its owner (the
// tables are boxed); XH5b-1's spot half and its counter (32 B) took one more
// 64 B line; XH5b-2's kept flag, its keeper's handle (the anchor word itself
// lives behind it) and `spot_failed` (17 B) fit that line's padding; XMM
// XH5c-2's top-ups — the governor (40 B), the keeper's handle, the phase,
// five words, five flags and three counters (118 B) — took two more lines;
// XMM XH5d-3a's priority cap and tally (two u32s, 8 B) fit their padding.
const _: () = assert!(core::mem::size_of::<HlGatewayArm>() == 1_792);

/// XMM XH5d: what a place bidding ALO priority `p` (×1e8 of resting
/// notional; 0 none) commits — `sz` at `px` (wire units, ×1e8) — in USD
/// ×1e6, rounded up; every step saturating, past `u32` saturated (no cap
/// admits it); a non-positive notional, none.
#[inline]
const fn priority_fee_1e6(p: u32, px_1e8: i64, sz_1e8: i64) -> u32 {
    let n = (p as i128)
        .saturating_mul(px_1e8 as i128)
        .saturating_mul(sz_1e8 as i128);
    if n <= 0 {
        return 0;
    }
    let fee = n.saturating_add(999_999_999_999_999_999) / 1_000_000_000_000_000_000;
    if fee > u32::MAX as i128 {
        u32::MAX
    } else {
        fee as u32
    }
}

/// The router's retirement code (BX3's `RETIRED_*`) for an end the arm
/// reports: a post-only cross, or an order in doubt that never landed,
/// expired; any other refusal was rejected; the member's cancel — or its
/// modify, which ended the original — is the member's; every other end is
/// the venue's cancel.
#[inline]
const fn retired_why(kind: u8, reason: u8) -> u8 {
    if kind == ORDER_EVENT_REJECTED {
        if reason == ORDER_EVENT_REASON_BAD_ALO_PX || reason == ORDER_EVENT_REASON_EXPIRED {
            RETIRED_EXPIRED
        } else {
            RETIRED_REJECTED
        }
    } else if reason == ORDER_EVENT_REASON_CANCEL_REQUESTED || reason == ORDER_EVENT_REASON_REPLACED
    {
        RETIRED_CANCELED_MEMBER
    } else if reason == ORDER_EVENT_REASON_EXPIRED {
        // XH4f (D9): the gateway's TTL cancelled it.
        RETIRED_CANCELED_TTL
    } else {
        RETIRED_CANCELED_VENUE
    }
}

impl HlGatewayArm {
    /// Boot: every table allocated here.
    ///
    /// # Errors
    /// [`ArmErr`].
    pub fn new(cfg: &ArmCfg, end: HlEngineEnd) -> Result<Self, ArmErr> {
        if cfg.slot >= 8 {
            return Err(ArmErr::Slot);
        }
        if cfg.n_perps == 0 || cfg.n_perps > ARM_PERPS {
            return Err(ArmErr::Perps);
        }
        let mut i = 0usize;
        while i < cfg.n_perps {
            let mut j = i + 1;
            while j < cfg.n_perps {
                if cfg.perps[i].sym == cfg.perps[j].sym || cfg.perps[i].asset == cfg.perps[j].asset
                {
                    return Err(ArmErr::Perps);
                }
                j += 1;
            }
            i += 1;
        }
        let window = |ms: u64| ms == 0 || ms > MAX_WINDOW_MS;
        if cfg.expires_after_ms == 0
            || window(cfg.post_timeout_ms)
            || window(cfg.recon_ms)
            || window(cfg.join_ms)
            || window(cfg.gone_ms)
            || cfg.shutdown_ms == 0
            || cfg.shutdown_ms > MAX_SHUTDOWN_MS
            || cfg.limit_ms == 0
            || cfg.limit_ms > MAX_LIMIT_MS
            || cfg.skew_ms > MAX_SKEW_MS
            || cfg.deadman.answer_ms != cfg.post_timeout_ms
        {
            return Err(ArmErr::Timing);
        }
        if cfg.budget_floor < MIN_BUDGET_FLOOR {
            return Err(ArmErr::Budget);
        }
        if cfg.leverage > MAX_LEVERAGE {
            return Err(ArmErr::Leverage);
        }
        let deadman =
            DeadMan::new(cfg.deadman, cfg.slot, DEADMAN_OID_BASE).map_err(|_| ArmErr::DeadMan)?;
        // COPY: the arm's inline state — 1 792 B, const-asserted (the
        // configuration ≈ 0.3 KiB, the dead man 256 B, the address budget
        // 64 B, the top-up governor 40 B, counters, the stats, the ring end,
        // the scratch command, the queue heads and words) — moved at boot
        // into the owner, cold;
        // rejected: boxing the arm (every verb would chase a pointer). Its
        // tables are boxed here, never moved.
        Ok(Self {
            cfg: *cfg,
            end,
            rows: boxed_array(Row::default()),
            n_rows: 0,
            events: Out::new(OrderEvent::ZERO),
            fills: Out::new(Fill::new(0, 0, Side::Bid, Price(0), Qty(0), 0)),
            retired: Out::new(Retired::default()),
            renamed: Out::new(Renamed::default()),
            counters: ArmCounters::default(),
            stats: DispatchStats::default(),
            alo: boxed_array(0),
            alo_at: 0,
            alo_n: 0,
            alo_bad: 0,
            reject_streak: 0,
            asset_refusal_streak: 0,
            up_ns: 0,
            snapshot: false,
            boot: BOOT_SNAPSHOT,
            boot_held: false,
            read_seq: 0,
            read_oid: 0,
            read_kind: 0,
            read_ns: 0,
            next_state_ns: 0,
            next_orders_ns: 0,
            epoch: 0,
            reread: false,
            others: false,
            exclusive: false,
            excl_lost_ns: 0,
            unknown_this_read: 0,
            perps: boxed_array(PerpState::default()),
            seeded: false,
            seed_ms: 0,
            preseed: boxed_array((0, 0, 0)),
            n_preseed: 0,
            unknown_positions: 0,
            worst_prev_1e6: 0,
            drift_hw_1e6: 0,
            state_ok: false,
            orders_ok: false,
            sheet_ns: 0,
            orders_ns: 0,
            last_agree_ns: 0,
            anchor_1e8: cfg.anchor_1e6.saturating_mul(WIRE_PER_ENGINE),
            account_1e8: 0,
            spot_1e8: 0,
            spot_ns: 0,
            next_spot_ns: u64::MAX,
            anchored: cfg.anchor_1e6 > 0,
            fresh_anchor: false,
            anchor_kept: cfg.anchor_1e6 > 0,
            keep: None,
            done_ids: boxed_array(0),
            done_ns: boxed_array(0),
            done_inferred: 0,
            done_at: 0,
            sweep_asked: 0,
            hold_until_ns: 0,
            deadman,
            scratch: HlCmd::noop(cfg.slot, 0),
            // The arm keeps no budget state file — the venue's read is the
            // budget's only source — so the address a file is checked
            // against is unused.
            budget: AddressBudget::cold([0; 20], cfg.budget_floor),
            limit_ok: false,
            limit_due_ns: 0,
            limit_sent: 0,
            sweep_charged: 0,
            day_until_ms: 0,
            day_from_ms: 0,
            day_due_ns: 0,
            day_bought_1e8: 0,
            day_done: false,
            day_pages_read: 0,
            day_rows: 0,
            limit_stale_ns: 0,
            lev_sent: 0,
            lev_ok: 0,
            lev_dark: 0,
            lev_due_ns: 0,
            // Off until the boot arms it (`set_topup`).
            topup: TopupGovernor::default(),
            topup_keep: None,
            topup_phase: TU_IDLE,
            topup_ver: 0,
            topup_seq: 0,
            topup_before: 0,
            topup_mark: 0,
            topup_sheet: 0,
            topup_doubt: false,
            topup_shown: false,
            topup_in_worth: false,
            topup_owed: false,
            topup_off: false,
            priority_cap_1e6: 0,
            priority_spent_1e6: 0,
        })
    }

    /// XMM XH5d: arm the ALO priority — at most `usd_1e6` of HYPE committed
    /// this session (boot-only; 0, the default, keeps every place bidding a
    /// priority refused unsent).
    pub fn set_priority_cap(&mut self, usd_1e6: u32) {
        self.priority_cap_1e6 = usd_1e6;
    }

    /// What the arm has seen.
    #[must_use]
    pub const fn counters(&self) -> &ArmCounters {
        &self.counters
    }

    /// The dead-man switch (its counters for `/metrics`).
    #[must_use]
    pub const fn deadman(&self) -> &DeadMan {
        &self.deadman
    }

    /// Orders tracked.
    #[must_use]
    pub const fn open_orders(&self) -> usize {
        self.n_rows
    }

    /// Booted (the sweep, the sheet, the orders), the seeding sheet flat in
    /// every bound perp, the dead man's boot clear answered and the budget
    /// read (XH4e): the arm takes orders.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.boot == BOOT_DONE
            && !self.boot_held
            && self.deadman.settled()
            && self.limit_ok
            && self.leverage_settled()
    }

    /// XH4f (O-XH6): every bound perp's leverage set or dark — or none is
    /// asked.
    #[must_use]
    const fn leverage_settled(&self) -> bool {
        self.cfg.leverage == 0 || (self.lev_ok | self.lev_dark) == self.perps_mask()
    }

    /// One bit per bound perp.
    #[inline]
    const fn perps_mask(&self) -> u16 {
        ((1u32 << self.cfg.n_perps) - 1) as u16
    }

    /// XH4e: the address budget as the arm models it — the venue's figures
    /// at the last read plus every request since; `None` before the first
    /// read.
    #[must_use]
    pub const fn budget(&self) -> Option<&AddressBudget> {
        if self.limit_ok {
            Some(&self.budget)
        } else {
            None
        }
    }

    /// XH4e: the budget at or below its floor — the router's `BudgetFloor`
    /// halt; places and modifies refused; the dead man parked once nothing
    /// of ours can rest. Never before the first read: nothing is known, and
    /// nothing is sent.
    #[must_use]
    pub fn budget_floor_breached(&self) -> bool {
        self.limit_ok && self.budget.may_submit().is_err()
    }

    /// XH4e: no budget read has answered for [`LIMIT_STALE_PERIODS`]
    /// periods since the first — growth is refused (no halt: the next read
    /// that answers lifts it).
    #[inline]
    fn budget_stale(&self, now: u64) -> bool {
        self.limit_ok && now > self.limit_stale_ns
    }

    /// XH4e: growth refused — the budget at its floor, or stale.
    #[inline]
    fn budget_refuses(&self, now: u64) -> bool {
        self.budget_floor_breached() || self.budget_stale(now)
    }

    /// XH4e: the gateway's congestion word (`exec_gateway::stats::
    /// CONGESTION_*`) — the member's pull reads it through
    /// [`OrderDispatch::exec_health`] (XH4f).
    #[must_use]
    pub fn congestion(&self) -> u64 {
        self.end.stats.congestion()
    }

    /// The seeding sheet held a position in a bound perp: the arm is held
    /// (XH5 rules on flattening at shutdown or refusing such a boot).
    #[must_use]
    pub const fn boot_position(&self) -> bool {
        self.boot_held
    }

    /// **XH5b-2 — boot: where the arm hands a fresh anchor to its keeper**
    /// (`anchor::AnchorKeeper`, on the gateway's thread), which stores it
    /// off the engine thread (E7). On the slot's own account the first
    /// reconciliation waits until the keeper has stored it; with no keeper
    /// a fresh anchor is never kept, and the slot never reconciled (the
    /// safe direction). A restored anchor needs none.
    pub fn keep_anchor_in(&mut self, slot: Arc<AnchorSlot>) {
        self.keep = Some(slot);
    }

    /// **XMM XH5c-2 — boot: the top-ups' governor** `gov` — restored from
    /// the slot's state file ([`crate::budget::restore_topup`]) and armed
    /// with the artifact's weight and day max — and, armed (a weight above
    /// 0), `keep`: where their state goes to its keeper
    /// (`topup::TopupKeeper`, on the gateway's thread). Off (weight 0), no
    /// keeper: the governor carries the restored spend's add-back alone. On
    /// the slot's own account alone: the shared one's requests are its
    /// blocking arm's to buy. A weight past the purchase command's `u32` is
    /// refused, never truncated; armed with no keeper, refused.
    ///
    /// # Errors
    /// [`ArmErr::Topup`].
    pub fn set_topup(
        &mut self,
        gov: TopupGovernor,
        keep: Option<Arc<TopupSlot>>,
    ) -> Result<(), ArmErr> {
        if !self.cfg.own_account
            || u32::try_from(gov.weight()).is_err()
            || (gov.weight() > 0 && keep.is_none())
        {
            return Err(ArmErr::Topup);
        }
        self.topup = gov;
        self.topup_keep = keep;
        Ok(())
    }

    /// XH4g: the reconciliation as last read — when each read was answered,
    /// whether each agreed, the drift high water. The testnet battery's view
    /// (a read answered a post timeout after an acknowledgement was issued
    /// after it); the router reads [`HaltSignal`].
    #[must_use]
    pub const fn recon_view(&self) -> ReconView {
        ReconView {
            sheet_ns: self.sheet_ns,
            orders_ns: self.orders_ns,
            state_ok: self.state_ok,
            orders_ok: self.orders_ok,
            drift_hw_1e6: self.drift_hw_1e6,
        }
    }

    /// XH-7: the ALO window's refusal rate, permille; 0 until it holds
    /// enough answers to mean something.
    #[must_use]
    pub fn alo_reject_permille(&self) -> u16 {
        if self.alo_n < ALO_MIN {
            return 0;
        }
        // ≤ 1000 by construction.
        (self.alo_bad * 1000 / self.alo_n) as u16
    }

    // --- the table --------------------------------------------------

    #[inline]
    fn find(&self, client_oid: u64) -> Option<usize> {
        let n = self.n_rows.min(ARM_ORDERS);
        let mut i = 0usize;
        while i < n {
            if self.rows[i].client_oid == client_oid {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    #[inline]
    fn perp_of_sym(&self, sym: u32) -> Option<usize> {
        let n = self.cfg.n_perps.min(ARM_PERPS);
        let mut i = 0usize;
        while i < n {
            if self.cfg.perps[i].sym == sym {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    #[inline]
    fn perp_of_asset(&self, asset: u32) -> Option<usize> {
        let n = self.cfg.n_perps.min(ARM_PERPS);
        let mut i = 0usize;
        while i < n {
            if self.cfg.perps[i].asset == asset {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Our row an event names (a stream row may name a finished one).
    #[inline]
    fn held(&self, e: &ExecEvent) -> Option<usize> {
        if e.slot != self.cfg.slot {
            return None;
        }
        self.find(e.client_oid)
    }

    /// Our row a post item answers; a missing one that did not just
    /// finish (a fill or the stream can beat the answer) is counted.
    #[inline]
    fn answered(&mut self, e: &ExecEvent, now: u64) -> Option<usize> {
        let i = self.held(e);
        if i.is_none() && e.slot == self.cfg.slot && self.finished_end(e.client_oid, now).is_none()
        {
            self.counters.stray += 1;
        }
        i
    }

    /// Remove row `i` (swap-remove) and remember its end.
    fn remove(&mut self, i: usize, now: u64, inferred: bool) {
        let k = self.done_at & (FINISHED - 1);
        self.done_ids[k] = self.rows[i].client_oid;
        self.done_ns[k] = now;
        self.done_inferred = (self.done_inferred & !(1u64 << k)) | (u64::from(inferred) << k);
        self.done_at = (k + 1) & (FINISHED - 1);
        debug_assert!(i < self.n_rows);
        self.n_rows -= 1;
        // COPY: the last row (64 B) into the freed one — the table stays
        // dense, so `find` and every scan walk `n_rows`; rejected:
        // tombstones (every scan would walk the high-water mark).
        self.rows[i] = self.rows[self.n_rows & (ARM_ORDERS - 1)];
    }

    /// The arm's own end of `client_oid` lately: when, and whether it was
    /// inferred (closed unseen) rather than said by the venue. The ids
    /// alone are scanned (512 B).
    fn finished_end(&self, client_oid: u64, now: u64) -> Option<(u64, bool)> {
        let mut k = 0usize;
        while k < FINISHED {
            if self.done_ids[k] == client_oid {
                let at = self.done_ns[k];
                if at != 0 && now.saturating_sub(at) < FINISHED_NS {
                    return Some((at, (self.done_inferred >> k) & 1 != 0));
                }
            }
            k += 1;
        }
        None
    }

    /// Row `i`'s bound perp (its index is below [`ARM_PERPS`] by
    /// construction; the mask folds the check).
    #[inline(always)]
    fn bind(&self, i: usize) -> &PerpBind {
        &self.cfg.perps[usize::from(self.rows[i].perp) & (ARM_PERPS - 1)]
    }

    /// Orders the arm holds that may be on the book or may land.
    fn live_rows(&self) -> usize {
        let n = self.n_rows.min(ARM_ORDERS);
        let mut live = 0usize;
        let mut i = 0usize;
        while i < n {
            live += usize::from(self.rows[i].state != ST_FILLING);
            i += 1;
        }
        live
    }

    /// XH4e: address requests pushed and not answered yet — places and
    /// modifies sent or in doubt, cancels sent, the dead man's post, a
    /// top-up's purchase sent, or answered in doubt and not seen yet (XMM
    /// XH5c-2): each may or may not be in the figure a budget read about to
    /// go states.
    fn unanswered(&self) -> u64 {
        let n = self.n_rows.min(ARM_ORDERS);
        let purchase = self.topup_phase == TU_SENT
            || (self.topup_phase == TU_SEEN && self.topup_doubt && !self.topup_shown);
        let mut k = u64::from(self.deadman.in_flight()) + u64::from(purchase);
        let mut i = 0usize;
        while i < n {
            let r = &self.rows[i];
            k += u64::from(r.state == ST_SENT || r.state == ST_DOUBT)
                + u64::from(r.cancel == CX_SENT);
            i += 1;
        }
        k
    }

    /// Room on every out-queue for one more step.
    #[inline]
    fn room(&self) -> bool {
        self.events.room() >= STEP_ROOM
            && self.fills.room() >= STEP_ROOM
            && self.retired.room() >= STEP_ROOM
            && self.renamed.room() >= STEP_ROOM
    }

    #[inline]
    fn doubt_due(&self, i: usize) -> u64 {
        let wait_ms = u64::from(self.cfg.expires_after_ms) + self.cfg.skew_ms;
        self.rows[i].since_ns.saturating_add(wait_ms * NS_PER_MS)
    }

    /// A socket up and a fresh gateway heartbeat.
    #[inline]
    fn stream_alive(&self, now: u64) -> bool {
        self.end.stats.ws_up() && now.saturating_sub(self.end.stats.heartbeat_ns()) < ALIVE_NS
    }

    // --- the vocabulary ---------------------------------------------

    fn emit(&mut self, i: usize, kind: u8, reason: u8, now: u64) {
        self.emit_at(i, kind, reason, now, 0);
    }

    /// An event with the venue's time of the change, ms — 0 = unknown (a
    /// post's answer carries none; XMM XH5d).
    fn emit_at(&mut self, i: usize, kind: u8, reason: u8, now: u64, venue_ms: u64) {
        let ev = OrderEvent::new(
            now,
            VenueId::Hyperliquid,
            self.bind(i).sym,
            self.rows[i].client_oid,
            self.cfg.slot,
            kind,
            reason,
            venue_ms,
        );
        if self.events.push(ev) {
            self.counters.events += 1;
        } else {
            self.counters.dropped += 1;
        }
    }

    /// A final event other than `FILLED`: the event, the retirement (with
    /// the router's `why`), the row gone.
    fn close_as(&mut self, i: usize, kind: u8, reason: u8, now: u64, inferred: bool) {
        self.emit(i, kind, reason, now);
        let r = Retired::new(
            self.rows[i].client_oid,
            self.cfg.slot,
            retired_why(kind, reason),
        );
        if !self.retired.push(r) {
            self.counters.dropped += 1;
        }
        self.remove(i, now, inferred);
    }

    fn close(&mut self, i: usize, kind: u8, reason: u8, now: u64) {
        self.close_as(i, kind, reason, now, false);
    }

    /// XH4f: the router's row ends with its fills — a retirement as
    /// filled, which the router takes a poll after them.
    fn filled(&mut self, i: usize, now: u64) {
        self.emit(i, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_NONE, now);
        let r = Retired::new(self.rows[i].client_oid, self.cfg.slot, RETIRED_FILLED);
        if !self.retired.push(r) {
            self.counters.dropped += 1;
        }
        self.remove(i, now, false);
    }

    /// `FILLED` once the fills cover the order. `true`: it is gone.
    fn maybe_filled(&mut self, i: usize, now: u64) -> bool {
        if self.rows[i].filled_1e8 >= self.rows[i].qty_1e8 {
            self.filled(i, now);
            return true;
        }
        false
    }

    /// The order is (or was) on the venue: a post-only order's `RESTING`
    /// once, then — a modify — its predecessor's `CANCELED(REPLACED)` and
    /// (XH4f, the queued-arm contract) the router's rename of its row at
    /// the replacement's size: no retirement of the predecessor. Returns
    /// the row's index, which removing the predecessor may move.
    fn landed(&mut self, i: usize, now: u64) -> usize {
        self.landed_at(i, now, 0)
    }

    /// [`Self::landed`], with the venue's time of the landing when the
    /// `open` row brings it — XMM XH5d: the measurement's ACK time, on
    /// the RESTING.
    fn landed_at(&mut self, i: usize, now: u64, venue_ms: u64) -> usize {
        let st = self.rows[i].state;
        if st == ST_SENT || st == ST_DOUBT {
            self.rows[i].state = ST_RESTING;
            self.rows[i].since_ns = now;
            self.rows[i].due_ns = 0;
            if self.rows[i].bits & RB_ALO != 0 {
                self.emit_at(
                    i,
                    ORDER_EVENT_RESTING,
                    ORDER_EVENT_REASON_NONE,
                    now,
                    venue_ms,
                );
                if venue_ms != 0 {
                    self.rows[i].bits |= RB_STAMPED;
                }
            }
        }
        let prev = self.rows[i].prev;
        if prev == 0 {
            return i;
        }
        self.rows[i].prev = 0;
        // The rename goes even when the predecessor ended first: the
        // router then tracks the replacement (it IS working).
        let n = Renamed::new(
            prev,
            self.rows[i].client_oid,
            self.rows[i].qty_1e8 / WIRE_PER_ENGINE,
            self.bind(i).sym,
            self.cfg.slot,
            self.rows[i].bits & RB_ASK == 0,
        );
        if !self.renamed.push(n) {
            self.counters.dropped += 1;
        }
        let Some(p) = self.find(prev) else {
            return i;
        };
        self.emit(p, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_REPLACED, now);
        self.remove(p, now, false);
        // The swap-remove moved the last row into `p`.
        if i == self.n_rows {
            p
        } else {
            i
        }
    }

    /// A modify that did not land ended (its row already closed): its
    /// predecessor rests on — or, if the stream ended it meanwhile (the
    /// venue drops the original of a refused modify), closes as the
    /// stream said.
    fn release(&mut self, prev: u64, now: u64) {
        if prev == 0 {
            return;
        }
        let Some(p) = self.find(prev) else {
            return;
        };
        self.rows[p].bits &= !RB_REPLACING;
        if self.rows[p].state != ST_GONE {
            return;
        }
        if self.rows[p].bits & RB_ENDED != 0 {
            let reason = self.rows[p].end_reason;
            self.close(p, ORDER_EVENT_CANCELED, reason, now);
        } else {
            // "Not found" or a read's silence, held for the modify: the
            // gone wait starts now, and a read confirms it.
            self.rows[p].due_ns = now.saturating_add(self.cfg.gone_ms * NS_PER_MS);
            self.reread = true;
        }
    }

    fn push_cancel(&mut self, i: usize, now: u64) {
        let asset = self.bind(i).asset;
        if self
            .end
            .cmd
            .push(&HlCmd::cancel(self.cfg.slot, self.rows[i].client_oid, asset).at(now))
        {
            self.charge(1);
            self.rows[i].cancel = CX_SENT;
            // A row thought gone keeps its gone wait (or its hold for a
            // modify): its cancel is best effort, the wait ends it.
            if self.rows[i].state != ST_GONE {
                self.rows[i].due_ns = 0;
            }
        } else {
            self.counters.ring_full += 1;
            self.want_cancel(i, now.saturating_add(CANCEL_RETRY_NS));
        }
    }

    /// Keep row `i`'s cancel as intent, re-sent at `due` — every path
    /// that keeps one (a full ring, a refusal, no answer): a row thought
    /// gone keeps its gone wait (or its hold for a modify) instead, the
    /// wait ends it (a listing that revives it re-sends the cancel).
    #[inline]
    fn want_cancel(&mut self, i: usize, due: u64) {
        self.rows[i].cancel = CX_WANTED;
        if self.rows[i].state != ST_GONE {
            self.rows[i].due_ns = due;
        }
    }

    /// XH4e: `n` address requests left the arm — each place, modify,
    /// cancel and control post is one, a control post never exempted as a
    /// cancel — counted against the budget and, while a budget read is in
    /// flight, added on top of its answer.
    #[inline]
    fn charge(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        self.budget
            .on_action_sent(u32::try_from(n).unwrap_or(u32::MAX));
        self.limit_sent = self.limit_sent.saturating_add(n);
    }

    /// XH4e: `n` requests the arm charged never left — the gateway refused
    /// them unsent: taken back, from a budget read's carry too.
    #[inline]
    fn refund(&mut self, n: u64) {
        self.budget
            .on_action_unsent(u32::try_from(n).unwrap_or(u32::MAX));
        self.limit_sent = self.limit_sent.saturating_sub(n);
        self.counters.refunded += n;
    }

    fn alo_note(&mut self, bad: bool) {
        let at = self.alo_at % ALO_WINDOW;
        if self.alo_n as usize == ALO_WINDOW {
            self.alo_bad -= u32::from(self.alo[at]);
        } else {
            self.alo_n += 1;
        }
        self.alo[at] = u8::from(bad);
        self.alo_bad += u32::from(bad);
        self.alo_at = (at + 1) % ALO_WINDOW;
    }

    // --- the pump ---------------------------------------------------

    /// Take up to [`PUMP_MAX`] gateway events into the out-queues.
    fn pump(&mut self, now: u64) -> bool {
        let mut n = 0usize;
        while n < PUMP_MAX && self.room() {
            let ev = match self.end.evt.pop() {
                // COPY: the 64 B event out of its ring slot — `on_event`
                // borrows the whole arm mutably, the guard borrows the
                // ring; rejected: splitting the arm's state around the
                // ring for one line's copy.
                Some(e) => *e,
                None => break,
            };
            self.on_event(&ev, now);
            n += 1;
        }
        n > 0
    }

    fn on_event(&mut self, e: &ExecEvent, now: u64) {
        let post = e.flags & EVF_POST_ITEM != 0;
        if post
            && e.kind == EV_REJECTED
            && e.slot == self.cfg.slot
            && never_left(e.reason)
            && (e.cmd_kind() != 0 || e.client_oid & READ_OID_BASE == DEADMAN_OID_BASE)
        {
            // XH4e: a request this arm charged — a place, a modify, a
            // cancel, the dead man's post, a leverage post (XH4f), a
            // top-up's purchase (XMM XH5c-2) — refused unsent. A read is no
            // address request, and no other control post is the arm's.
            self.refund(1);
        }
        if post && e.cmd_kind() == 0 {
            self.on_internal(e, now);
            return;
        }
        match e.kind {
            EV_ACK => self.on_ack(e, now),
            EV_REJECTED if post => self.on_refused(e, now),
            EV_REJECTED => self.on_row_rejected(e, now),
            EV_UNANSWERED => self.on_unanswered(e, now),
            EV_RESTING => self.on_row_resting(e, now),
            EV_CANCELED => self.on_row_canceled(e, now),
            EV_FILLED => self.on_row_filled(e, now),
            EV_FILL => self.on_fill(e, now),
            EV_FILL_META => {
                if e.slot == self.cfg.slot {
                    self.counters.fees_1e8 += e.px_1e8;
                    self.counters.fills_crossed += u64::from(e.flags & EVF_CROSSED != 0);
                }
            }
            EV_SNAPSHOT => self.snapshot = true,
            EV_GAP => {
                // A stream may have lost rows — another slot's resting
                // order among them: re-read the orders at once (a read in
                // flight went before the gap: it cannot say).
                self.reread = true;
                self.lose_exclusive(now);
                // XH4e: fills past a day read's end among them — while the
                // read holds its end, its count toward the venue's 10 000
                // may be short: spent (an `orderUpdates` frame carries no
                // fill; a first page asked again takes a new end).
                if e.aux != GAP_UPDATES_UNREADABLE && self.day_holds_end() {
                    self.day_spent();
                }
            }
            EV_OPEN_ORDER => self.on_open_order(e, now),
            EV_POSITION => self.on_position(e),
            EV_READ_DONE | EV_READ_FAILED => self.on_read_end(e, now),
            // The halt word reaches the signal from the stats block.
            EV_HALT => {}
            _ => self.counters.stray += 1,
        }
    }

    /// A place, modify or cancel was accepted.
    fn on_ack(&mut self, e: &ExecEvent, now: u64) {
        let Some(i) = self.answered(e, now) else {
            return;
        };
        if e.cmd_kind() == CMD_CANCEL {
            self.cancel_done(i, e.reason, now);
            return;
        }
        self.reject_streak = 0;
        self.asset_refusal_streak = 0;
        self.stats.accepted += 1;
        if self.rows[i].bits & RB_ALO != 0 {
            self.alo_note(false);
        }
        let i = self.landed(i, now);
        if e.flags & EVF_FILLED != 0 {
            // Traded on arrival (`sz` = the total filled; an IoC's
            // remainder died): FILLED once the fills cover it.
            if e.sz_1e8 < self.rows[i].qty_1e8 {
                self.rows[i].qty_1e8 = e.sz_1e8;
            }
            self.rows[i].state = ST_FILLING;
            self.rows[i].since_ns = now;
            self.rows[i].due_ns = now.saturating_add(self.cfg.join_ms * NS_PER_MS);
            self.rows[i].cancel = CX_NONE;
            let _ = self.maybe_filled(i, now);
        } else if self.rows[i].cancel == CX_WANTED {
            self.push_cancel(i, now);
        }
    }

    /// A cancel succeeded: the order was on the venue until now. `why`:
    /// the answer's reason — `EXPIRED` for the gateway's TTL cancel (XH4f).
    fn cancel_done(&mut self, i: usize, why: u8, now: u64) {
        if self.rows[i].state == ST_FILLING {
            // Filled first; its FILLED comes.
            return;
        }
        self.counters.doubt_settled += u64::from(self.rows[i].state == ST_DOUBT);
        let reason = if why == ORDER_EVENT_REASON_EXPIRED {
            ORDER_EVENT_REASON_EXPIRED
        } else if self.rows[i].bits & RB_ASKED != 0 {
            ORDER_EVENT_REASON_CANCEL_REQUESTED
        } else {
            ORDER_EVENT_REASON_OTHER
        };
        let i = self.landed(i, now);
        self.close(i, ORDER_EVENT_CANCELED, reason, now);
    }

    /// A post item the gateway or the venue refused.
    fn on_refused(&mut self, e: &ExecEvent, now: u64) {
        let unsent = never_left(e.reason);
        if e.reason == REASON_THROTTLED {
            self.counters.throttled += 1;
        }
        if e.cmd_kind() == CMD_CANCEL {
            if let Some(i) = self.answered(e, now) {
                self.cancel_refused(i, e.reason, unsent, now);
            }
            return;
        }
        let alo_cross = e.reason == ORDER_EVENT_REASON_BAD_ALO_PX;
        if alo_cross && e.slot == self.cfg.slot {
            // XH-7 counts the reply item, whether or not the stream's
            // `badAloPxRejected` row closed the order first.
            self.counters.alo_rejects += 1;
            self.alo_note(true);
        }
        let Some(i) = self.answered(e, now) else {
            return;
        };
        // E7-F2, as the blocking arm (`response.rs`): an IoC that found
        // nothing to take is a miss, not a venue fault — a run of them
        // must not latch the reject-streak halt (XMM XH4h-3).
        let ioc_miss = e.reason == REASON_IOC_MISS;
        if !unsent && !alo_cross && !ioc_miss {
            self.counters.rejects += 1;
            self.reject_streak += 1;
            if self.rows[i].bits & RB_ALO != 0 {
                self.alo_note(false);
            }
        }
        let prev = self.rows[i].prev;
        self.close(i, ORDER_EVENT_REJECTED, e.reason, now);
        self.release(prev, now);
    }

    fn cancel_refused(&mut self, i: usize, reason: u8, unsent: bool, now: u64) {
        if unsent || reason != REASON_NOT_FOUND {
            self.want_cancel(i, now.saturating_add(CANCEL_RETRY_NS));
            return;
        }
        match self.rows[i].state {
            // The place may still be ahead of its cancel: re-sent when it
            // lands.
            ST_SENT => {
                self.rows[i].cancel = CX_WANTED;
                self.rows[i].due_ns = 0;
            }
            ST_DOUBT if now >= self.doubt_due(i) => {
                // Past its expiry and a skew: it never landed.
                self.counters.doubt_settled += 1;
                let prev = self.rows[i].prev;
                self.close(i, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_EXPIRED, now);
                self.release(prev, now);
            }
            // It may still land: the cancel goes again at the doubt's end.
            ST_DOUBT => {
                self.rows[i].cancel = CX_WANTED;
                self.rows[i].due_ns = self.doubt_due(i);
            }
            ST_RESTING => {
                self.rows[i].state = ST_GONE;
                self.rows[i].since_ns = now;
                self.rows[i].cancel = CX_NONE;
                self.rows[i].bits &= !RB_UNLISTED;
                // Held while its modify is in flight (the modify's answer
                // says how it ended); otherwise a read confirms it.
                if self.rows[i].bits & RB_REPLACING != 0 {
                    self.rows[i].due_ns = 0;
                } else {
                    self.rows[i].due_ns = now.saturating_add(self.cfg.gone_ms * NS_PER_MS);
                    self.reread = true;
                }
            }
            // Filling or gone already: its end is coming.
            _ => self.rows[i].cancel = CX_NONE,
        }
    }

    /// A post item the gateway could not settle (E-5).
    fn on_unanswered(&mut self, e: &ExecEvent, now: u64) {
        let Some(i) = self.answered(e, now) else {
            return;
        };
        if e.cmd_kind() == CMD_CANCEL {
            // A cancel is idempotent: send it again after a skew.
            self.want_cancel(i, now.saturating_add(self.cfg.skew_ms * NS_PER_MS));
            return;
        }
        if self.rows[i].state == ST_SENT {
            self.rows[i].state = ST_DOUBT;
            self.rows[i].since_ns = now;
            self.rows[i].due_ns = self.doubt_due(i);
        }
    }

    /// An `orderUpdates` refusal (a post-only cross arrives here too; XH-7
    /// counts the reply item only).
    fn on_row_rejected(&mut self, e: &ExecEvent, now: u64) {
        let Some(i) = self.held(e) else {
            return;
        };
        let st = self.rows[i].state;
        if st == ST_SENT || st == ST_DOUBT {
            let prev = self.rows[i].prev;
            self.close(i, ORDER_EVENT_REJECTED, e.reason, now);
            self.release(prev, now);
        }
    }

    /// `orderUpdates`: open — the venue's time of the landing with it
    /// (XMM XH5d).
    fn on_row_resting(&mut self, e: &ExecEvent, now: u64) {
        if e.slot != self.cfg.slot {
            // Another slot's (or a stranger's) order rests: the account is
            // not the slot's alone until a read says so again.
            self.lose_exclusive(now);
            return;
        }
        let Some(i) = self.find(e.client_oid) else {
            return;
        };
        let i = self.landed_at(i, now, e.venue_ms);
        // XMM XH5d: the post's answer came first and its RESTING carried no
        // venue time — once more, with the `open` row's (a member takes the
        // repeat as a no-op; the capture keeps the time).
        if self.rows[i].state == ST_RESTING
            && self.rows[i].bits & (RB_ALO | RB_STAMPED) == RB_ALO
            && e.venue_ms != 0
        {
            self.rows[i].bits |= RB_STAMPED;
            self.emit_at(
                i,
                ORDER_EVENT_RESTING,
                ORDER_EVENT_REASON_NONE,
                now,
                e.venue_ms,
            );
        }
        if self.rows[i].cancel == CX_WANTED && self.rows[i].state == ST_RESTING {
            self.push_cancel(i, now);
        }
    }

    /// `orderUpdates`: canceled (by us, the venue, the dead man, a sweep).
    fn on_row_canceled(&mut self, e: &ExecEvent, now: u64) {
        let Some(i) = self.held(e) else {
            return;
        };
        if self.rows[i].state == ST_FILLING {
            // An IoC's dead remainder: its FILLED comes.
            return;
        }
        if self.rows[i].bits & RB_REPLACING != 0 {
            // Its modify is in flight: the modify's answer says whether
            // this was the replacement (REPLACED) or the venue's own end.
            self.rows[i].state = ST_GONE;
            self.rows[i].since_ns = now;
            self.rows[i].due_ns = 0;
            self.rows[i].bits |= RB_ENDED;
            self.rows[i].end_reason = e.reason;
            return;
        }
        let i = self.landed(i, now);
        self.close(i, ORDER_EVENT_CANCELED, e.reason, now);
    }

    /// `orderUpdates`: filled.
    fn on_row_filled(&mut self, e: &ExecEvent, now: u64) {
        let Some(i) = self.held(e) else {
            return;
        };
        let i = self.landed(i, now);
        if !self.maybe_filled(i, now) {
            self.rows[i].state = ST_FILLING;
            self.rows[i].since_ns = now;
            self.rows[i].due_ns = now.saturating_add(self.cfg.join_ms * NS_PER_MS);
            self.rows[i].cancel = CX_NONE;
        }
    }

    /// `userFills`: one trade. Booked, handed to the engine, joined with
    /// its order.
    fn on_fill(&mut self, e: &ExecEvent, now: u64) {
        // XH4e: the volume a fill earns the address is not credited here —
        // the last budget read may already count a fill that arrives after
        // it; the next read states it. A fill past a day read's end — any
        // slot's, a stranger's, one with no time — takes one of the venue's
        // 10 000 places: it counts with the day's rows.
        self.day_rows = self.day_rows.saturating_add(u32::from(
            self.day_until_ms != 0
                && !self.day_done
                && (e.venue_ms == 0 || e.venue_ms > self.day_until_ms),
        ));
        if e.slot != self.cfg.slot {
            self.counters.fills_foreign += 1;
            return;
        }
        let Some(p) = self.perp_of_asset(e.asset) else {
            self.counters.fills_unresolved += 1;
            return;
        };
        let buy = e.flags & EVF_BUY != 0;
        self.book(p, if buy { e.sz_1e8 } else { -e.sz_1e8 }, e.venue_ms);
        self.perps[p].last_px_1e8 = e.px_1e8;
        let side = if buy { Side::Bid } else { Side::Ask };
        // XH4e: the venue's time, wall — as the blocking arm stamps a venue
        // fill; the router's day epoch reads it as wall time.
        let f = Fill::new(
            e.venue_ms.saturating_mul(1_000_000),
            self.cfg.perps[p].sym,
            side,
            Price(e.px_1e8 / WIRE_PER_ENGINE),
            Qty(e.sz_1e8 / WIRE_PER_ENGINE),
            e.client_oid,
        )
        .with_attribution(self.cfg.slot, FILL_ORIGIN_VENUE)
        // XMM XH5d: the venue's trade id — the report's key against the
        // public prints.
        .with_tid(e.aux);
        if self.fills.push(f) {
            self.counters.fills += 1;
            self.stats.fills_seen += 1;
        } else {
            self.counters.dropped += 1;
        }
        if let Some(i) = self.find(e.client_oid) {
            let i = self.landed(i, now);
            self.rows[i].filled_1e8 += e.sz_1e8;
            let _ = self.maybe_filled(i, now);
        }
    }

    /// Book a fill against the sheet: after its time once seeded, held
    /// until the first sheet otherwise.
    fn book(&mut self, p: usize, signed_1e8: i64, venue_ms: u64) {
        if self.seeded {
            if venue_ms > self.seed_ms {
                self.perps[p].booked_1e8 += signed_1e8;
            }
        } else if self.n_preseed < PRESEED {
            self.preseed[self.n_preseed] = (p as u8, signed_1e8, venue_ms);
            self.n_preseed += 1;
        } else {
            self.counters.preseed_dropped += 1;
        }
    }

    /// The arm's own posts (the dead man's, the reads, the leverage posts and
    /// the top-ups' purchases): command bits 0.
    fn on_internal(&mut self, e: &ExecEvent, now: u64) {
        if self.deadman.on_event(e, now_ms()) {
            return;
        }
        if self.read_oid != 0 && e.slot == self.cfg.slot && e.client_oid == self.read_oid {
            // The read was refused or went unanswered.
            self.read_failed(now);
            return;
        }
        if e.slot == self.cfg.slot && e.client_oid & !(ARM_PERPS as u64 - 1) == LEVERAGE_OID_BASE {
            self.leverage_answer(e, now);
            return;
        }
        // XMM XH5c-2: a purchase's answer, by its own range — never stray.
        if e.slot == self.cfg.slot && e.client_oid & !TOPUP_SEQ_MASK == TOPUP_OID_BASE {
            self.topup_answer(e);
            return;
        }
        self.counters.stray += 1;
    }

    /// XH4f (O-XH6): once the seeding sheet has read every bound perp flat,
    /// each one's leverage goes (cross, charged as an address request); one
    /// refused unsent goes again a read period later. Not before: on a
    /// shared account a bound perp may hold another slot's position — the
    /// boot holds then, and a leverage change would reach that position.
    fn leverage_step(&mut self, now: u64) -> bool {
        if self.cfg.leverage == 0
            || self.boot < BOOT_ORDERS
            || self.boot_held
            || now < self.lev_due_ns
            || (self.lev_sent | self.lev_ok | self.lev_dark) == self.perps_mask()
        {
            return false;
        }
        let mut worked = false;
        let mut p = 0usize;
        while p < self.cfg.n_perps.min(ARM_PERPS) {
            let bit = 1u16 << p;
            if (self.lev_sent | self.lev_ok | self.lev_dark) & bit == 0 {
                let c = HlCmd::leverage(
                    self.cfg.slot,
                    LEVERAGE_OID_BASE | p as u64,
                    self.cfg.perps[p].asset,
                    true,
                    u32::from(self.cfg.leverage),
                );
                if !self.end.cmd.push(&c.at(now)) {
                    self.counters.ring_full += 1;
                    return worked;
                }
                self.charge(1);
                self.lev_sent |= bit;
                worked = true;
            }
            p += 1;
        }
        worked
    }

    /// XH4f (O-XH6): a perp's leverage answered. Set; refused unsent (never
    /// left: asked again a read period later); refused by the venue or
    /// unanswered: dark — growth on that perp is refused, since the venue's
    /// leverage is not the one the caps were sized for.
    fn leverage_answer(&mut self, e: &ExecEvent, now: u64) {
        let bit = 1u16 << (e.client_oid & (ARM_PERPS as u64 - 1));
        if self.lev_sent & bit == 0 {
            self.counters.stray += 1;
            return;
        }
        self.lev_sent &= !bit;
        match e.kind {
            EV_ACK => self.lev_ok |= bit,
            EV_REJECTED if never_left(e.reason) => {
                self.lev_due_ns = now.saturating_add(self.cfg.recon_ms * NS_PER_MS);
            }
            _ => {
                self.lev_dark |= bit;
                self.counters.leverage_dark += 1;
            }
        }
    }

    // --- top-ups (XMM XH5c-2) ---------------------------------------

    /// The purchase in flight's client id: its own range, its count.
    #[inline]
    const fn topup_oid(&self) -> u64 {
        TOPUP_OID_BASE | (self.topup_seq & TOPUP_SEQ_MASK)
    }

    /// **A budget read's top-up step** (`read`: the read's number, counted
    /// from the boot). A purchase answered before this read went is judged
    /// on it — on the headroom apart from what volume earned
    /// ([`AddressBudget::remaining_unearned`]), so a minute's volume is never
    /// read as a purchase: seen — half its weight shows over the figure it
    /// was decided at ([`TopupGovernor::confirms`]) — its price waits for a
    /// worth that carries it ([`Self::topup_charge`]): for one answered in
    /// doubt, which may have landed after a sheet issued since its answer,
    /// a sheet issued after THIS read (served after it landed); not seen,
    /// the top-ups stop. A purchase decided and not sent yet (its keeper
    /// slow) that this newer read leaves out of the band never leaves. With
    /// none in flight, the band rule decides ([`Self::topup_decide`]). One
    /// step a read.
    fn topup_on_read(&mut self, read: u64) {
        match self.topup_phase {
            TU_SEEN if !self.topup_shown && read > self.topup_mark => {
                if self
                    .topup
                    .confirms(self.topup_before, self.budget.remaining_unearned())
                {
                    self.topup_shown = true;
                    if self.topup_doubt {
                        self.topup_mark = read;
                        self.topup_in_worth = false;
                    }
                    self.topup_charge();
                } else {
                    self.stop_topups();
                }
            }
            TU_STORING if !self.topup_in_band() => self.topup_unsent(),
            TU_IDLE => self.topup_decide(),
            _ => {}
        }
    }

    /// The headroom a purchase is for: over the floor, at most one weight
    /// above it (the band rule's own figures, [`TopupGovernor::above_band`]).
    fn topup_in_band(&self) -> bool {
        let (remaining, floor) = (self.budget.remaining(), self.budget.floor());
        remaining > i64::try_from(floor).unwrap_or(i64::MAX)
            && !self.topup.above_band(remaining, floor)
    }

    /// A complete worth landed — the spot half of perp sheet `sheet` (its
    /// read number). Issued past a purchase's mark (its answer — or, in
    /// doubt, the read that showed it), the sheet carries the price: the
    /// add-back may follow ([`Self::topup_charge`]).
    fn topup_on_worth(&mut self, sheet: u64) {
        if self.topup_phase == TU_SEEN && sheet > self.topup_mark {
            self.topup_in_worth = true;
            self.topup_charge();
        }
    }

    /// The purchase seen on the venue AND inside the worth: only then is its
    /// price the session's spend — the add-back (Q-XH5-2) never lands on a
    /// worth that does not carry the price — and the state handed over for
    /// the next boot. Before, the price reads as a loss (the safe way; the
    /// boot keeps one price under a quarter of the loss bound).
    fn topup_charge(&mut self) {
        if !(self.topup_shown && self.topup_in_worth) {
            return;
        }
        self.topup.charge_cost();
        self.counters.topup_ok += 1;
        self.topup_phase = TU_IDLE;
        self.topup_hand_over();
    }

    /// Decide a purchase — armed, on the slot's own account, its anchor
    /// durable (the price is added back against THAT anchor: one paid
    /// before it would be in it already), the buying not ended (the halt,
    /// the shutdown: [`Self::topup_end`]), inside the band and the day's
    /// max ([`TopupGovernor::take`], the day charged with the decision) —
    /// and hand the state to the keeper: the purchase leaves once it is
    /// stored ([`Self::topup_step`]). The keeper still storing the last
    /// state: the day's charge returned, decided again at the next read; a
    /// keeper that failed: the top-ups stop.
    fn topup_decide(&mut self) {
        let Some(keep) = &self.topup_keep else {
            return;
        };
        if !self.cfg.own_account || !self.anchor_kept || self.topup_off {
            return;
        }
        if !self
            .topup
            .take(self.budget.remaining(), self.budget.floor(), utc_day())
        {
            return;
        }
        let handed = keep.publish(&self.topup.state(0));
        match handed {
            Ok(v) => {
                self.topup_ver = v;
                self.topup_before = self.budget.remaining_unearned();
                // This state is the newest: it supersedes one owed.
                self.topup_owed = false;
                self.topup_phase = TU_STORING;
            }
            Err(Keep::Failed) => self.stop_topups(),
            Err(_) => self.topup.refund_day(),
        }
    }

    /// A purchase handed over leaves once its keeper has stored the state —
    /// built in place in the scratch command (the dead man's; one at a time
    /// on this thread) and pushed by reference (the ring slot its one copy),
    /// charged as one address request. A keeper that failed stops the
    /// top-ups: nothing is bought unrecorded. A full ring leaves it unsent
    /// ([`Self::topup_unsent`]). A hand-over owed (a spend or a stop the
    /// keeper could not take at once) goes again, a pass at a time.
    fn topup_step(&mut self, now: u64) -> bool {
        if self.topup_owed && self.topup_phase != TU_STORING {
            self.topup_hand_over();
        }
        if self.topup_phase != TU_STORING {
            return false;
        }
        let kept = match &self.topup_keep {
            Some(k) => k.kept(self.topup_ver),
            None => Keep::Failed,
        };
        match kept {
            Keep::Pending => false,
            Keep::Failed => {
                self.stop_topups();
                true
            }
            Keep::Stored => {
                // Narrowed by refusal, never truncated (the arming refused a
                // weight past `u32`).
                let Ok(weight) = u32::try_from(self.topup.weight()) else {
                    self.stop_topups();
                    return true;
                };
                self.topup_seq = self.topup_seq.wrapping_add(1);
                self.scratch = HlCmd::reserve(self.cfg.slot, self.topup_oid(), weight).at(now);
                if self.end.cmd.push(&self.scratch) {
                    self.charge(1);
                    self.topup_phase = TU_SENT;
                } else {
                    self.counters.ring_full += 1;
                    self.topup_unsent();
                }
                true
            }
        }
    }

    /// The purchase's answer (its own client-id range; never `stray`).
    /// Refused before it left — its request already taken back by the
    /// never-left rule: [`Self::topup_unsent`]. Accepted, or in doubt (its
    /// request carried until it is seen): judged at the next budget read
    /// issued after this answer, charged once a worth carries it. Refused by
    /// the venue: the top-ups stop. An answer in the range for no purchase
    /// in flight is the arm's own count gone wrong: they stop too. After a
    /// halt or once the shutdown began, the same: only the buying ended
    /// ([`Self::topup_end`]) — a purchase that left is still judged and
    /// charged, and its spend handed over.
    fn topup_answer(&mut self, e: &ExecEvent) {
        if self.topup_phase != TU_SENT || e.client_oid != self.topup_oid() {
            self.stop_topups();
            return;
        }
        match e.kind {
            EV_REJECTED if never_left(e.reason) => self.topup_unsent(),
            EV_ACK | EV_UNANSWERED => {
                self.topup_mark = self.read_seq;
                self.topup_doubt = e.kind == EV_UNANSWERED;
                self.topup_shown = false;
                self.topup_in_worth = false;
                self.topup_phase = TU_SEEN;
            }
            _ => self.stop_topups(),
        }
    }

    /// The purchase never left (refused unsent, a full ring, abandoned — a
    /// newer read out of the band, the halt, the shutdown): never bought,
    /// its price never charged — the day's charge returned, decided again
    /// at a later read unless the buying ended.
    fn topup_unsent(&mut self) {
        self.topup.refund_day();
        self.counters.topup_unsent += 1;
        self.topup_phase = TU_IDLE;
    }

    /// **The buying ends** — the router halted the slot (its sweep asked)
    /// or the shutdown began: nothing more decided; a purchase decided and
    /// not sent never leaves ([`Self::topup_unsent`]). The accounting goes
    /// on: a purchase sent or seen is still judged and charged, its spend
    /// or a stop handed over.
    fn topup_end(&mut self) {
        self.topup_off = true;
        if self.topup_phase == TU_STORING {
            self.topup_unsent();
        }
    }

    /// No more top-ups from this process — nor, once the state is stored,
    /// for the rest of the UTC day it happens on ([`TopupGovernor::roll`],
    /// then [`TopupGovernor::stop`]): counted, and handed over.
    fn stop_topups(&mut self) {
        self.topup.roll(utc_day());
        self.topup.stop();
        self.topup_phase = TU_IDLE;
        self.counters.topup_failed += 1;
        self.topup_hand_over();
    }

    /// The state handed to the keeper with no wait — a spend seen, a stop —
    /// for the next boot. A keeper still storing the last hand-over takes
    /// nothing yet: the state is owed, and goes again at the next pass
    /// ([`Self::topup_step`]); until it does, the file is a step behind — the
    /// day's usage at its last pre-charge, the spend short (the safe way). A
    /// keeper that failed takes nothing, ever.
    fn topup_hand_over(&mut self) {
        if let Some(k) = &self.topup_keep {
            self.topup_owed = k.publish(&self.topup.state(0)) == Err(Keep::Pending);
        }
    }

    // --- reads ------------------------------------------------------

    fn issue_read(&mut self, kind: u8, now: u64) -> bool {
        let oid = READ_OID_BASE + self.read_seq + 1;
        let c = match kind {
            READ_ORDERS => HlCmd::read_orders(self.cfg.slot, oid),
            READ_LIMIT => HlCmd::read_limit(self.cfg.slot, oid),
            READ_DAY => HlCmd::read_day(self.cfg.slot, oid, self.day_from_ms, self.day_until_ms),
            READ_SPOT => HlCmd::read_spot(self.cfg.slot, oid),
            _ => HlCmd::read_state(self.cfg.slot, oid),
        };
        if !self.end.cmd.push(&c.at(now)) {
            self.counters.ring_full += 1;
            return false;
        }
        self.read_seq += 1;
        self.read_oid = oid;
        self.read_kind = kind;
        self.read_ns = now;
        match kind {
            READ_ORDERS => {
                self.epoch = self.epoch.wrapping_add(1);
                self.reread = false;
                self.others = false;
                self.unknown_this_read = 0;
            }
            READ_STATE => {
                self.unknown_positions = 0;
                let n = self.cfg.n_perps.min(ARM_PERPS);
                let mut p = 0usize;
                while p < n {
                    self.perps[p].venue_1e8 = 0;
                    self.perps[p].value_1e8 = 0;
                    p += 1;
                }
            }
            // XH4e: what the venue may not have counted yet goes on top of
            // its answer.
            READ_LIMIT => self.limit_sent = self.unanswered(),
            _ => {}
        }
        true
    }

    /// The read in flight failed: the sheet and the orders are asked again
    /// after a skew; the spot sheet with the next perp sheet (XH5b-1: one a
    /// sheet, so it never storms, whatever the skew); the budget and the
    /// day (XH4e: 20 IP weight or more each) a read period later.
    fn read_failed(&mut self, now: u64) {
        self.counters.reads_failed += 1;
        let retry = now.saturating_add(self.cfg.skew_ms * NS_PER_MS);
        let slow = now.saturating_add(self.cfg.recon_ms.max(self.cfg.skew_ms) * NS_PER_MS);
        match self.read_kind {
            READ_ORDERS => self.next_orders_ns = retry,
            READ_LIMIT => self.limit_due_ns = slow,
            READ_DAY => self.day_due_ns = slow,
            READ_SPOT => {
                self.next_spot_ns = u64::MAX;
                self.counters.spot_failed += 1;
            }
            _ => self.next_state_ns = retry,
        }
        self.read_oid = 0;
    }

    /// XH4e: the budget as the venue states it (`nRequestsUsed`, `cumVlm`,
    /// `nRequestsSurplus`), plus every request unanswered when the read
    /// went and every one pushed since — some may be counted twice, the
    /// conservative way.
    fn limit_done(&mut self, e: &ExecEvent, now: u64) {
        self.budget = AddressBudget::from_venue(
            [0; 20],
            self.cfg.budget_floor,
            e.aux,
            e.px_1e8 / WIRE_PER_ENGINE,
            u64::try_from(e.sz_1e8).unwrap_or(0),
        );
        let since = self.limit_sent;
        self.charge(since);
        self.limit_ok = true;
        self.limit_stale_ns =
            now.saturating_add(LIMIT_STALE_PERIODS * self.cfg.limit_ms * NS_PER_MS);
        self.counters.limit_reads += 1;
        self.limit_due_ns = now.saturating_add(self.cfg.limit_ms * NS_PER_MS);
        // XMM XH5c-2: the venue's own figure in hand — the top-up's step.
        self.topup_on_read(e.client_oid.wrapping_sub(READ_OID_BASE));
    }

    /// XH4e: one page of the day's fills — the read slot's |px × sz| (USD
    /// ×1e8) added. A page that adds nothing past its start ends the read
    /// (a short page is not the end: the venue's page size is unmeasured);
    /// any other is followed a read period later by the next, from its
    /// latest row's time, inclusive — a row on the boundary counts twice,
    /// the conservative way. The read ends spent at rows out of time order,
    /// a full page that cannot move, [`DAY_PAGES_MAX`] pages, or rows plus
    /// the account's fills since its end (`on_fill`) at the venue's 10 000
    /// — the page that adds nothing judged too: it answers a read period
    /// after the last page with rows, so the fills that raced that one are
    /// in. A page answered after a gap spent the day adds nothing.
    fn day_page(&mut self, e: &ExecEvent, now: u64) {
        self.counters.day_pages += 1;
        if self.day_done {
            return;
        }
        self.day_bought_1e8 = self.day_bought_1e8.saturating_add(e.px_1e8.max(0));
        self.day_pages_read += 1;
        self.day_rows = self
            .day_rows
            .saturating_add(u32::try_from(e.aux).unwrap_or(u32::MAX));
        let moved = e.venue_ms > self.day_from_ms;
        let ordered = e.sz_1e8 == 0;
        let reached = self.day_rows >= DAY_ROWS_MAX;
        if ordered && !moved && e.aux < DAY_PAGE && !reached {
            // Nothing past its start: the day is read.
            self.day_done = true;
            return;
        }
        if !ordered || !moved || reached || self.day_pages_read >= DAY_PAGES_MAX {
            // Rows out of time order (the next start could skip some), a
            // full page that cannot move, the venue's 10 000 reached, or
            // the read's page bound: the day's start may be past reach.
            self.day_spent();
            return;
        }
        self.day_from_ms = e.venue_ms;
        self.day_due_ns = now.saturating_add(self.cfg.recon_ms * NS_PER_MS);
    }

    /// XH4e: the day's start may be past reach — the day reads spent
    /// (every sum the router adds it to saturates: fail closed), and the
    /// read ends.
    fn day_spent(&mut self) {
        self.counters.day_unreadable += 1;
        self.day_bought_1e8 = DAY_SPENT_1E8;
        self.day_done = true;
    }

    /// XH4e: the day read holds its end — a page answered, or the first in
    /// flight. Before that, what a gap lost comes back as rows: the first
    /// page, asked again, takes a new end (the clock-skew residual aside).
    fn day_holds_end(&self) -> bool {
        !self.day_done
            && (self.day_pages_read > 0 || (self.read_oid != 0 && self.read_kind == READ_DAY))
    }

    /// One resting order of the account.
    fn on_open_order(&mut self, e: &ExecEvent, now: u64) {
        if self.read_oid == 0 || e.aux != self.read_oid {
            return;
        }
        if e.slot != self.cfg.slot {
            // Another slot's order, or a stranger's: the account is shared.
            self.others = true;
            return;
        }
        let Some(i) = self.find(e.client_oid) else {
            self.unlisted_order(e, now);
            return;
        };
        self.rows[i].seen = self.epoch;
        match self.rows[i].state {
            ST_SENT | ST_DOUBT => {
                let i = self.landed(i, now);
                if self.rows[i].cancel == CX_WANTED {
                    self.push_cancel(i, now);
                }
            }
            ST_GONE
                if self.rows[i].since_ns <= self.read_ns && self.rows[i].bits & RB_ENDED == 0 =>
            {
                // Listed by a read issued after it was thought gone: it
                // rests — and a cancel asked or wanted, none in flight,
                // goes again.
                self.rows[i].state = ST_RESTING;
                self.rows[i].since_ns = now;
                self.rows[i].due_ns = 0;
                let cx = self.rows[i].cancel;
                if cx == CX_WANTED || (cx == CX_NONE && self.rows[i].bits & RB_ASKED != 0) {
                    self.push_cancel(i, now);
                }
            }
            _ => {}
        }
    }

    /// A read lists an order of this slot the arm does not hold.
    fn unlisted_order(&mut self, e: &ExecEvent, now: u64) {
        let raced = match self.finished_end(e.client_oid, now) {
            // The venue said it ended: the listing is a view behind that.
            Some((_, false)) => true,
            // The arm inferred the end: only a read issued before that
            // inference may still list it; one issued after lists a live
            // order.
            Some((at, true)) => self.read_ns < at,
            None => false,
        };
        if raced {
            return;
        }
        // Cancelled; after boot it pins the drift (the slot halts).
        self.unknown_this_read += 1;
        if self.boot == BOOT_DONE {
            self.counters.unknown_orders += 1;
        }
        if e.asset != ASSET_UNKNOWN {
            if self
                .end
                .cmd
                .push(&HlCmd::cancel(self.cfg.slot, e.client_oid, e.asset).at(now))
            {
                self.charge(1);
            } else {
                self.counters.ring_full += 1;
            }
        }
    }

    /// One perp position of the sheet.
    fn on_position(&mut self, e: &ExecEvent) {
        if self.read_oid == 0 || e.client_oid != self.read_oid {
            return;
        }
        match self.perp_of_asset(e.asset) {
            Some(p) => {
                self.perps[p].venue_1e8 = e.sz_1e8;
                self.perps[p].value_1e8 = e.aux;
            }
            None if e.sz_1e8 != 0 && self.cfg.own_account => self.unknown_positions += 1,
            None => {}
        }
    }

    /// A read's end: `EV_READ_DONE` or `EV_READ_FAILED`.
    fn on_read_end(&mut self, e: &ExecEvent, now: u64) {
        if self.read_oid == 0 || e.slot != self.cfg.slot || e.client_oid != self.read_oid {
            return;
        }
        // XH5b-1: a spot sheet that lists no USDC answers nothing — nothing
        // to anchor or judge (`recon::account_view`'s rule), never a $0 half.
        let no_worth = self.read_kind == READ_SPOT && e.sz_1e8 == 0;
        if e.kind == EV_READ_FAILED || e.reason != self.read_kind || no_worth {
            // An end of another kind answers no read of this one.
            self.read_failed(now);
            return;
        }
        self.read_oid = 0;
        self.counters.reads += 1;
        let next = now.saturating_add(self.cfg.recon_ms * NS_PER_MS);
        match self.read_kind {
            READ_ORDERS => {
                self.orders_done(now);
                self.next_orders_ns = next;
            }
            READ_LIMIT => self.limit_done(e, now),
            READ_DAY => self.day_page(e, now),
            READ_SPOT => self.spot_done(e.px_1e8, now),
            _ => {
                // XMM XH5c-2: the sheet's number — its spot half pairs with
                // it.
                self.topup_sheet = e.client_oid.wrapping_sub(READ_OID_BASE);
                self.state_done(e.px_1e8, e.venue_ms, now);
                self.next_state_ns = next;
            }
        }
    }

    fn orders_done(&mut self, now: u64) {
        // A resting order the read did not list, resting since the read
        // went or before: it ended while the arm did not see it — a read
        // confirms it at once, and the stream has the gone wait to say
        // how. Held while its modify is in flight. One already thought
        // gone that this read — issued since — did not list is confirmed:
        // its gone wait alone ends it.
        let n = self.n_rows.min(ARM_ORDERS);
        let mut i = 0usize;
        while i < n {
            let r = &self.rows[i];
            let unlisted = r.seen != self.epoch && r.since_ns <= self.read_ns;
            if unlisted && r.state == ST_RESTING && r.bits & RB_REPLACING == 0 {
                self.rows[i].state = ST_GONE;
                self.rows[i].since_ns = now;
                self.rows[i].due_ns = now.saturating_add(self.cfg.gone_ms * NS_PER_MS);
                self.rows[i].bits &= !RB_UNLISTED;
                self.reread = true;
            } else if unlisted && r.state == ST_GONE {
                self.rows[i].bits |= RB_UNLISTED;
            }
            i += 1;
        }
        // Only a read issued since exclusivity was lost restores it.
        self.exclusive = !self.others && self.read_ns >= self.excl_lost_ns;
        self.orders_ok = self.unknown_this_read == 0 || self.boot != BOOT_DONE;
        self.orders_ns = now;
        self.note_agreement(now);
        if self.boot == BOOT_ORDERS {
            self.boot = BOOT_DONE;
        }
    }

    fn state_done(&mut self, account_1e8: i64, venue_ms: u64, now: u64) {
        self.account_1e8 = account_1e8;
        self.sheet_ns = now;
        // XH5b-1: the worth's spot half is read next, and the anchor waits
        // for it; a shared account's worth is every slot's — never read,
        // never anchored.
        if self.cfg.own_account {
            self.next_spot_ns = now;
        }
        let n = self.cfg.n_perps.min(ARM_PERPS);
        if !self.seeded {
            // Seed: the sheet at `venue_ms` holds every fill up to it; held
            // fills after it are booked on top. A position in a bound perp
            // holds the boot (the member boots flat).
            let mut p = 0usize;
            while p < n {
                self.perps[p].booked_1e8 = self.perps[p].venue_1e8;
                self.boot_held |= self.perps[p].venue_1e8 != 0;
                p += 1;
            }
            let mut k = 0usize;
            while k < self.n_preseed.min(PRESEED) {
                let (pp, signed, ms) = self.preseed[k];
                if ms > venue_ms {
                    self.perps[usize::from(pp) % ARM_PERPS].booked_1e8 += signed;
                }
                k += 1;
            }
            self.n_preseed = 0;
            self.seed_ms = venue_ms;
            self.seeded = true;
        }
        let mut worst = 0i64;
        let mut p = 0usize;
        while p < n {
            let s = &self.perps[p];
            let diff = (s.booked_1e8 - s.venue_1e8).unsigned_abs();
            if diff != 0 {
                let mark = if s.venue_1e8 != 0 {
                    let m = i128::from(s.value_1e8) * 100_000_000
                        / i128::from(s.venue_1e8.unsigned_abs());
                    i64::try_from(m).unwrap_or(i64::MAX)
                } else {
                    s.last_px_1e8
                };
                let usd_1e6 = if mark > 0 {
                    // coin ×1e8 × USD ×1e8 / 1e10 → USD ×1e6.
                    i64::try_from(i128::from(diff) * i128::from(mark) / 10_000_000_000)
                        .unwrap_or(i64::MAX)
                } else {
                    i64::MAX
                };
                worst = worst.max(usd_1e6.max(1));
            }
            p += 1;
        }
        // A disagreement must survive two sheets (a fill in flight).
        let noted = worst.min(self.worst_prev_1e6);
        self.worst_prev_1e6 = worst;
        self.drift_hw_1e6 = self.drift_hw_1e6.max(noted);
        self.state_ok = worst == 0 && self.unknown_positions == 0;
        self.note_agreement(now);
        if self.boot == BOOT_STATE {
            self.boot = BOOT_ORDERS;
            self.next_orders_ns = now;
        }
    }

    /// XH5b-1 — the spot sheet answered: the worth's USDC half. The first
    /// complete worth anchors the session when no anchor was restored — a
    /// positive one only: an unfunded account has nothing to anchor (the
    /// blocking arm's E7 rule), and the first reconciliation waits.
    fn spot_done(&mut self, usdc_1e8: i64, now: u64) {
        self.spot_1e8 = usdc_1e8;
        self.spot_ns = now;
        self.next_spot_ns = u64::MAX;
        self.counters.spot_reads += 1;
        // Positive in the engine's units: a worth under $1e-6 anchors
        // nothing (its keeper's word could not carry it).
        if !self.anchored && self.worth_1e8() / WIRE_PER_ENGINE > 0 {
            self.anchor_1e8 = self.worth_1e8();
            self.anchored = true;
            self.fresh_anchor = true;
            // XH5b-2: handed to its keeper, once; durable when it says so.
            if let Some(k) = &self.keep {
                k.publish(self.anchor_1e8 / WIRE_PER_ENGINE);
            }
        }
        self.note_agreement(now);
        // XMM XH5c-2: a complete worth — a purchase's price may be in it.
        self.topup_on_worth(self.topup_sheet);
    }

    /// XH5b-1 — the account's worth, USD ×1e8: the perp sheet's value plus
    /// the spot sheet's USDC (saturating: a hostile body scans to
    /// ±`i64::MAX`).
    #[inline(always)]
    const fn worth_1e8(&self) -> i64 {
        self.account_1e8.saturating_add(self.spot_1e8)
    }

    /// Both reads agree and each ended within two periods — on the slot's
    /// own account the worth's spot half too (XH5b-1): the agreement's
    /// clock restarts. A read that stops coming (starved or failing) shows
    /// as `ReconStale`, never as an old agreement.
    fn note_agreement(&mut self, now: u64) {
        let fresh = 2 * self.cfg.recon_ms * NS_PER_MS;
        let spot_fresh = !self.cfg.own_account
            || (self.spot_ns != 0 && now.saturating_sub(self.spot_ns) <= fresh);
        if self.state_ok
            && self.orders_ok
            && spot_fresh
            && now.saturating_sub(self.sheet_ns) <= fresh
            && now.saturating_sub(self.orders_ns) <= fresh
        {
            self.last_agree_ns = now;
        }
    }

    // --- idle -------------------------------------------------------

    /// One idle pass at `now`.
    fn idle(&mut self, now: u64) -> bool {
        // XH4f (the queued-arm contract): what the last pass retired goes
        // to the router now — a poll after the fills it pushed with it, and
        // only once the engine has taken every fill queued (one left past
        // the engine's drain batch would book after its row was released).
        if self.fills.len == 0 {
            self.retired.release();
        }
        let mut worked = self.pump(now);
        if self.stream_alive(now) {
            self.up_ns = now;
        }
        // XH5b-2: a fresh anchor is durable once its keeper says so (one
        // load a pass until then).
        if self.fresh_anchor && !self.anchor_kept {
            if let Some(k) = &self.keep {
                self.anchor_kept = k.is_kept(self.anchor_1e8 / WIRE_PER_ENGINE);
            }
        }
        // XH4e: the gateway's own sweep cancels are this address's too.
        let swept = self.end.stats.sweep_cancels();
        if swept > self.sweep_charged {
            self.charge(swept - self.sweep_charged);
            self.sweep_charged = swept;
        }
        worked |= self.boot_step(now);
        worked |= self.leverage_step(now);
        worked |= self.topup_step(now);
        worked |= self.recon_step(now);
        worked |= self.row_timers(now);
        worked |= self.deadman_step(now);
        worked
    }

    /// Boot: the snapshot, then the sweep (the sheet and the orders
    /// follow on the read cadence).
    fn boot_step(&mut self, now: u64) -> bool {
        match self.boot {
            BOOT_SNAPSHOT if self.snapshot => {
                self.sweep_asked = self.end.stats.request_sweep();
                self.boot = BOOT_SWEEP;
                true
            }
            BOOT_SWEEP => match self.cancel_all_state() {
                CancelAllState::Clear => {
                    self.boot = BOOT_STATE;
                    self.next_state_ns = now;
                    true
                }
                CancelAllState::Stranded => {
                    // Never read clear: ask again.
                    self.sweep_asked = self.end.stats.request_sweep();
                    true
                }
                CancelAllState::Working => false,
            },
            _ => false,
        }
    }

    /// Reads on their cadence, one at a time: the sheet then the orders
    /// at boot, both after it; on the slot's own account the spot sheet
    /// right after each perp sheet (XH5b-1).
    fn recon_step(&mut self, now: u64) -> bool {
        if self.boot < BOOT_STATE {
            return false;
        }
        if self.read_oid != 0 {
            let limit = (self.cfg.post_timeout_ms * NS_PER_MS).saturating_add(READ_GRACE_NS);
            if now.saturating_sub(self.read_ns) > limit {
                self.read_failed(now);
                return true;
            }
            return false;
        }
        // XH5b-1: the worth's spot half, one answer after the sheet it pairs
        // with: once a sheet — a failed one waits for the next sheet — so it
        // starves no other read.
        if now >= self.next_spot_ns {
            return self.issue_read(READ_SPOT, now);
        }
        // A re-read (a "gone" inference, a gap) goes before a periodic
        // sheet — it decides orders thought gone — unless the sheet is a
        // whole period overdue: no storm of re-reads starves the drift
        // and the P&L.
        let sheet_due = self.boot != BOOT_ORDERS && now >= self.next_state_ns;
        let starved = sheet_due && now - self.next_state_ns >= self.cfg.recon_ms * NS_PER_MS;
        if self.boot != BOOT_STATE && self.reread && !starved {
            return self.issue_read(READ_ORDERS, now);
        }
        if sheet_due {
            return self.issue_read(READ_STATE, now);
        }
        if self.boot != BOOT_STATE && now >= self.next_orders_ns {
            return self.issue_read(READ_ORDERS, now);
        }
        // XH4e: after the boot's sheet and orders, the budget (and every
        // `limit_ms`), then the day's spend, page by page, once.
        if self.boot == BOOT_DONE {
            if now >= self.limit_due_ns {
                return self.issue_read(READ_LIMIT, now);
            }
            // A page — 20 IP weight and one more per 20 rows — never goes
            // while the gateway's rates read throttled.
            if self.limit_ok
                && !self.day_done
                && now >= self.day_due_ns
                && self.end.stats.congestion() < CONGESTION_THROTTLED
            {
                if self.day_pages_read == 0 {
                    // Every page ends here: fills after it are the live
                    // ledger's, and the pages converge. Taken at each ask
                    // of the first page: a retry reads the fills since the
                    // failed ask as rows, and the count past the old end
                    // goes.
                    self.day_until_ms = now_ms();
                    self.day_from_ms = self.day_until_ms / DAY_MS * DAY_MS;
                    self.day_rows = 0;
                }
                return self.issue_read(READ_DAY, now);
            }
        }
        false
    }

    /// Rows whose wait ended.
    fn row_timers(&mut self, now: u64) -> bool {
        let mut worked = false;
        let mut i = 0usize;
        while i < self.n_rows.min(ARM_ORDERS) && self.room() {
            let (due, state, cancel, bits) = {
                let r = &self.rows[i];
                (r.due_ns, r.state, r.cancel, r.bits)
            };
            if due == 0 || now < due {
                i += 1;
                continue;
            }
            if state == ST_GONE && bits & RB_UNLISTED == 0 && self.confirming(i) {
                // An orders read issued since it was thought gone is in
                // flight or asked: its listing revives the row, its
                // silence ends it.
                i += 1;
                continue;
            }
            worked = true;
            match state {
                ST_GONE => {
                    self.counters.closed_unseen += 1;
                    self.close_as(i, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER, now, true);
                    continue;
                }
                ST_FILLING => {
                    self.counters.join_timeouts += 1;
                    self.filled(i, now);
                    continue;
                }
                // In doubt past its expiry and a skew — or a wanted
                // cancel's retry: the cancel goes (in doubt, it settles).
                ST_DOUBT => self.push_cancel(i, now),
                _ if cancel == CX_WANTED => self.push_cancel(i, now),
                _ => self.rows[i].due_ns = 0,
            }
            i += 1;
        }
        worked
    }

    /// The dead man: wanted while the arm is booted, the account is the
    /// slot's (or read to hold no other slot's orders since), and the arm
    /// holds or lately held an order — or a cancel-all is not read clear
    /// (a Stranded sweep may leave ours on the book). At the budget floor
    /// (XH4e) only while it holds one or the cancel-all is not clear.
    fn deadman_step(&mut self, now: u64) -> bool {
        let want = self.deadman_wanted(now);
        self.drive_deadman(want, now)
    }

    fn deadman_wanted(&mut self, now: u64) -> bool {
        // After boot (whose own sweep reads clear before it ends), a
        // cancel-all not read clear holds the switch like an order does.
        let holding = self.n_rows > 0
            || (self.boot == BOOT_DONE && self.cancel_all_state() != CancelAllState::Clear);
        if holding {
            self.hold_until_ns = now.saturating_add(WANT_HOLD_NS);
        }
        let alone = self.cfg.own_account || self.exclusive;
        // XH4e: at the budget floor the switch parks — its re-arms would
        // spend what is left — but only once nothing of ours can rest:
        // while an order is held or the halt's cancel-all is not read clear
        // it stays armed (the floor keeps the headroom for exactly that
        // cleanup); flat and read clear, it parks at once, hold or not.
        let parked = !holding && self.budget_floor_breached();
        self.boot == BOOT_DONE && alone && now < self.hold_until_ns && !parked
    }

    /// The account may hold another slot's order from `now`: not the
    /// slot's alone until an orders read issued since says so.
    #[inline]
    fn lose_exclusive(&mut self, now: u64) {
        self.exclusive = false;
        self.excl_lost_ns = now;
    }

    /// An orders read that decides row `i`, thought gone: asked and not
    /// issued yet (it goes before any sheet), or in flight and issued
    /// since the row went gone.
    #[inline]
    fn confirming(&self, i: usize) -> bool {
        self.reread
            || (self.read_oid != 0
                && self.read_kind == READ_ORDERS
                && self.read_ns >= self.rows[i].since_ns)
    }

    fn drive_deadman(&mut self, want: bool, now: u64) -> bool {
        let ms = now_ms();
        if !self.deadman.poll(ms, now, want, &mut self.scratch) {
            return false;
        }
        if self.end.cmd.push(&self.scratch) {
            self.charge(1);
            return true;
        }
        self.counters.ring_full += 1;
        self.deadman.unsent(ms);
        false
    }

    fn signal(&self, now: u64) -> HaltSignal {
        // 0 = no observation until the stream was first alive (the
        // router's documented sentinel); then the time since it was.
        let gap = if self.up_ns == 0 || self.stream_alive(now) {
            0
        } else {
            now.saturating_sub(self.up_ns).max(1)
        };
        let drift = if self.counters.unknown_orders > 0 {
            i64::MAX
        } else {
            self.drift_hw_1e6
        };
        // XH4e: and the day's spend read (S7-L1 gap A: the router seeds
        // from this — its day cap must not restart at a full allowance).
        // XH5b-1: and, on the slot's own account, the worth's spot half and
        // an anchor — the session bound is judged from the first
        // reconciliation on — XH5b-2: a durable one (restored, or stored by
        // its keeper): the slot never quotes on an anchor a restart would
        // lose.
        let worth_known =
            !self.cfg.own_account || (self.spot_ns != 0 && self.anchored && self.anchor_kept);
        let reconciled = self.boot == BOOT_DONE
            && !self.boot_held
            && self.state_ok
            && self.orders_ok
            && self.day_done
            && worth_known;
        let age = if self.last_agree_ns == 0 {
            0
        } else {
            now.saturating_sub(self.last_agree_ns).max(1)
        };
        let (judged, delta) = self.session_pnl();
        HaltSignal::new(
            gap,
            drift,
            self.reject_streak,
            self.asset_refusal_streak,
            self.budget_floor_breached(),
            reconciled,
            age,
        )
        .with_pnl(judged, delta)
        .with_alo_reject_permille(self.alo_reject_permille())
        .with_gateway_halted(self.end.stats.halt_reason() != HALT_NONE)
    }

    /// E7: the session's P&L, USD ×1e6 — judged on the slot's own account
    /// once its worth is complete (XH5b-1); unjudged, it reads 0. XMM
    /// XH5c-2 (Q-XH5-2): the worth less the anchor PLUS the session's
    /// top-up spend — a purchase seen on the venue is the day's max's to
    /// bound, not the loss bound's; one not seen is never added back.
    #[inline]
    const fn session_pnl(&self) -> (bool, i64) {
        let judged = self.anchored && self.seeded && self.cfg.own_account && self.spot_ns != 0;
        let delta = if judged {
            (self.worth_1e8().saturating_sub(self.anchor_1e8) / WIRE_PER_ENGINE)
                .saturating_add(self.topup.cost_1e6())
        } else {
            0
        };
        (judged, delta)
    }

    /// The command for `o` on perp `p` (a place, or a modify of `prev`),
    /// built in `out`; `false` when it breaks a rule the venue would (LAW
    /// XH-4) or is a kind the arm does not speak.
    fn order_cmd(&self, o: &Order, p: usize, prev: u64, out: &mut HlCmd) -> bool {
        if o.alo_priority_1e8 > HlCmd::PRIORITY_MAX_1E8 {
            return false;
        }
        let tif = match o.kind {
            ORDER_KIND_MAKER if o.is_post_only() => Tif::Alo,
            ORDER_KIND_IOC => Tif::Ioc,
            _ => return false,
        };
        let (Some(px), Some(sz)) = (
            o.px.0.checked_mul(WIRE_PER_ENGINE),
            o.qty.0.checked_mul(WIRE_PER_ENGINE),
        ) else {
            return false;
        };
        let rules = self.cfg.perps[p].rules;
        let buy = o.side == Side::Bid;
        // LAW XH-4: never a worse price, never a larger size.
        let Some(px) = rules.round_px(px, if buy { Round::Down } else { Round::Up }) else {
            return false;
        };
        let sz = rules.round_sz_down(sz);
        if rules.check(px, sz).is_err() {
            return false;
        }
        *out = HlCmd::place(
            self.cfg.slot,
            o.client_oid,
            self.cfg.perps[p].asset,
            buy,
            px,
            sz,
            tif,
        )
        .expiring_after(self.cfg.expires_after_ms)
        .at(o.ts_ns);
        if o.is_reduce_only() {
            out.set_reduce_only();
        }
        if prev != 0 {
            out.set_replacing(prev);
        } else if tif == Tif::Alo {
            // XMM XH5d: an ALO place bids its priority (a modify's
            // `batchModify` carries no grouping, A-S7-27).
            out.arg = o.alo_priority_1e8;
        }
        true
    }

    fn track(&mut self, c: &HlCmd, p: usize, now: u64) {
        let mut bits = 0u8;
        if c.is_buy == 0 {
            bits |= RB_ASK;
        }
        if c.tif == Tif::Alo as u8 {
            bits |= RB_ALO;
        }
        debug_assert!(self.n_rows < ARM_ORDERS, "the callers refuse a full table");
        self.rows[self.n_rows & (ARM_ORDERS - 1)] = Row {
            client_oid: c.client_oid,
            prev: c.prev_client_oid,
            since_ns: now,
            qty_1e8: c.sz_1e8,
            perp: p as u8,
            state: ST_SENT,
            bits,
            seen: self.epoch.wrapping_sub(1),
            ..Row::default()
        };
        self.n_rows += 1;
        self.counters.pushed += 1;
    }

    fn refuse(&mut self, e: DispatchError) -> Result<(), DispatchError> {
        self.stats.record_rejection(e);
        Err(e)
    }

    fn refuse_local(&mut self, e: DispatchError) -> Result<(), DispatchError> {
        self.counters.refused_local += 1;
        self.refuse(e)
    }
}

impl OrderDispatch for HlGatewayArm {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        if order.strategy_id != self.cfg.slot {
            // The router routes by slot: another slot's order would carry
            // this slot's cloid.
            return self.refuse_local(DispatchError::Unroutable);
        }
        if !self.ready() {
            return self.refuse(DispatchError::Disconnected);
        }
        let now = now_ns();
        if self.budget_refuses(now) {
            // XH4e: the blocking arm's answer at its floor.
            self.counters.refused_budget += 1;
            return self.refuse_local(DispatchError::SlotDisabled);
        }
        if self.n_rows >= ARM_ORDERS {
            return self.refuse(DispatchError::QueueFull);
        }
        if self.find(order.client_oid).is_some() {
            // A cloid names one order at the venue.
            return self.refuse_local(DispatchError::Unroutable);
        }
        let Some(p) = self.perp_of_sym(order.sym) else {
            self.asset_refusal_streak += 1;
            return self.refuse_local(DispatchError::Unroutable);
        };
        if self.lev_dark & (1u16 << p) != 0 {
            self.counters.refused_leverage += 1;
            return self.refuse_local(DispatchError::SlotDisabled);
        }
        let mut c = HlCmd::noop(self.cfg.slot, 0);
        if !self.order_cmd(order, p, 0, &mut c) {
            return self.refuse_local(DispatchError::Unroutable);
        }
        // XMM XH5d: the HYPE a priority commits is outside the worth — the
        // session's cap bounds it; with none armed no place bids one.
        let fee = priority_fee_1e6(c.arg, c.px_1e8, c.sz_1e8);
        if fee
            > self
                .priority_cap_1e6
                .saturating_sub(self.priority_spent_1e6)
        {
            return self.refuse_local(DispatchError::Unroutable);
        }
        // XH4f (D9): the order's TTL (LAW E-8), which the gateway keeps.
        c.time_ms = (order.ttl_ns / NS_PER_MS).min(TTL_MAX_MS);
        if !self.end.cmd.push(&c) {
            self.counters.ring_full += 1;
            return self.refuse(DispatchError::QueueFull);
        }
        self.priority_spent_1e6 += fee;
        self.charge(1);
        self.track(&c, p, now);
        Ok(())
    }

    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        if req.strategy_id != self.cfg.slot {
            return self.refuse(DispatchError::NoSuchOrder);
        }
        let Some(i) = self.find(req.client_oid) else {
            return self.refuse(DispatchError::NoSuchOrder);
        };
        if self.bind(i).sym != req.sym {
            return self.refuse(DispatchError::IdentityMismatch);
        }
        self.rows[i].bits |= RB_ASKED;
        // Sent, resting, in doubt or thought gone: the cancel goes now (a
        // "not found" about an order that may still land is kept as
        // intent). A full ring keeps it as intent: the arm re-sends it.
        if self.rows[i].state != ST_FILLING && self.rows[i].cancel != CX_SENT {
            self.push_cancel(i, now_ns());
        }
        Ok(())
    }

    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        let prev = req.prev_client_oid();
        let o = req.order();
        let Some(i) = self.find(prev) else {
            return self.refuse(DispatchError::NoSuchOrder);
        };
        let (state, cancel, bits, perp) = {
            let r = &self.rows[i];
            (
                r.state,
                r.cancel,
                r.bits,
                usize::from(r.perp) & (ARM_PERPS - 1),
            )
        };
        if state != ST_RESTING || cancel != CX_NONE || bits & RB_REPLACING != 0 {
            return self.refuse(DispatchError::NoSuchOrder);
        }
        if o.strategy_id != self.cfg.slot
            || self.cfg.perps[perp].sym != o.sym
            || (bits & RB_ASK != 0) != (o.side != Side::Bid)
            || (bits & RB_ALO != 0) != (o.kind == ORDER_KIND_MAKER)
        {
            return self.refuse(DispatchError::IdentityMismatch);
        }
        let now = now_ns();
        if self.budget_refuses(now) {
            self.counters.refused_budget += 1;
            return self.refuse_local(DispatchError::SlotDisabled);
        }
        if self.lev_dark & (1u16 << perp) != 0 {
            self.counters.refused_leverage += 1;
            return self.refuse_local(DispatchError::SlotDisabled);
        }
        if self.n_rows >= ARM_ORDERS {
            return self.refuse(DispatchError::QueueFull);
        }
        if self.find(o.client_oid).is_some() {
            return self.refuse_local(DispatchError::Unroutable);
        }
        let mut c = HlCmd::noop(self.cfg.slot, 0);
        if !self.order_cmd(o, perp, prev, &mut c) {
            return self.refuse_local(DispatchError::Unroutable);
        }
        if !self.end.cmd.push(&c) {
            self.counters.ring_full += 1;
            return self.refuse(DispatchError::QueueFull);
        }
        self.charge(1);
        self.rows[i].bits |= RB_REPLACING;
        self.track(&c, perp, now);
        Ok(())
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        // The engine asks for fills first in each iteration: the pump runs
        // here (and when idle), never while it drains order events, so a
        // fill always reaches it before its order's FILLED.
        let _ = self.pump(now_ns());
        self.fills.pop()
    }

    fn try_next_order_event(&mut self, out: &mut OrderEvent) -> bool {
        self.events.pop_into(out)
    }

    fn try_next_retired(&mut self) -> Option<Retired> {
        self.retired.pop_released()
    }

    /// XH4f (the queued-arm contract): the gateway arm's cancel and modify
    /// `Ok` mean queued — the router releases a row on its retirement and
    /// renames it on its `Renamed`. The arm serves one slot.
    #[inline]
    fn verbs_confirm_later(&self, _sym: core_types::SymbolId, _venue: u8, strategy_id: u8) -> bool {
        strategy_id == self.cfg.slot
    }

    #[inline]
    fn try_next_renamed(&mut self) -> Option<Renamed> {
        self.renamed.pop()
    }

    fn stats(&self) -> DispatchStats {
        self.stats
    }

    fn on_idle(&mut self) -> bool {
        self.idle(now_ns())
    }

    fn halt_signal(&self) -> HaltSignal {
        self.signal(now_ns())
    }

    fn halt_signal_for(&self, slot: u8) -> HaltSignal {
        if slot == self.cfg.slot {
            self.signal(now_ns())
        } else {
            HaltSignal::default()
        }
    }

    /// XH4e (S7-L1 gap A): the slot's fills since 00:00Z as the venue
    /// lists them, once read — `(UTC day, Σ |px × sz| USD ×1e6)`.
    fn venue_day_bought(&self, slot: usize) -> Option<(u64, i64)> {
        if self.day_done && slot == usize::from(self.cfg.slot) {
            Some((
                self.day_until_ms / DAY_MS,
                self.day_bought_1e8 / WIRE_PER_ENGINE,
            ))
        } else {
            None
        }
    }

    /// XH4f (D7): slot 6's health as the arm knows it — not booted, its
    /// budget refusing growth, the gateway's congestion word and its recent
    /// answers' worst round trip. Another slot's is not the arm's to judge.
    fn exec_health(&self, slot: u8, now_ns: u64) -> core_types::ExecHealth {
        if slot != self.cfg.slot {
            return core_types::ExecHealth::HEALTHY;
        }
        let blocked = (u8::from(!self.ready()) * core_types::EXEC_BLOCKED_NOT_READY)
            | (u8::from(self.budget_refuses(now_ns)) * core_types::EXEC_BLOCKED_BUDGET);
        core_types::ExecHealth::new(
            blocked,
            u8::try_from(self.congestion()).unwrap_or(u8::MAX),
            u32::try_from(self.end.stats.rtt_worst_ns() / NS_PER_MS).unwrap_or(u32::MAX),
        )
    }

    /// XH4f (D11): the arm's numbers — its counters and the dead man's, the
    /// gateway's shared words and the ones it publishes at its heartbeat,
    /// the budget, the latencies. The router adds its words for the slot.
    /// Cold: once a second (the quantiles walk their trackers).
    fn gateway_arm_report(&self, now_ns: u64, out: &mut GatewayArmReport) -> bool {
        let c = &self.counters;
        let d = self.deadman.counters();
        let s = &*self.end.stats;
        let [actions_sent, replies, replies_unmatched, post_timeouts, rest_fallback_posts, reconnects, in_flight, ws_up] =
            s.io();
        let beat = s.heartbeat_ns();
        let (budget_known, budget_remaining, budget_cancel_remaining) = match self.budget() {
            Some(b) => (1, b.remaining(), b.cancel_remaining()),
            None => (0, 0, 0),
        };
        let (_, session_pnl) = self.session_pnl();
        let worth = if self.cfg.own_account && self.spot_ns != 0 {
            self.worth_1e8() / WIRE_PER_ENGINE
        } else {
            0
        };
        // COPY: the 880 B report written once into the caller's block —
        // cold, once a second; the out-param keeps it off the return path.
        *out = GatewayArmReport {
            present: 1,
            slot: u64::from(self.cfg.slot),
            pushed: c.pushed,
            events: c.events,
            refused_local: c.refused_local,
            ring_full: c.ring_full,
            fills: c.fills,
            fills_foreign: c.fills_foreign,
            fills_unresolved: c.fills_unresolved,
            fills_crossed: c.fills_crossed,
            stray: c.stray,
            alo_rejects: c.alo_rejects,
            rejects: c.rejects,
            doubt_settled: c.doubt_settled,
            closed_unseen: c.closed_unseen,
            join_timeouts: c.join_timeouts,
            unknown_orders: c.unknown_orders,
            reads: c.reads,
            reads_failed: c.reads_failed,
            preseed_dropped: c.preseed_dropped,
            shutdown_fills: c.shutdown_fills,
            shutdown_unconfirmed: c.shutdown_unconfirmed,
            shutdown_switch_held: c.shutdown_switch_held,
            dropped: c.dropped,
            refused_budget: c.refused_budget,
            throttled: c.throttled,
            limit_reads: c.limit_reads,
            day_pages: c.day_pages,
            day_unreadable: c.day_unreadable,
            refunded: c.refunded,
            refused_leverage: c.refused_leverage,
            spot_reads: c.spot_reads,
            spot_failed: c.spot_failed,
            topup_ok: c.topup_ok,
            topup_failed: c.topup_failed,
            topup_unsent: c.topup_unsent,
            dm_arms: d.arms,
            dm_clears: d.clears,
            dm_accepted: d.accepted,
            dm_refused: d.refused,
            dm_rate_limited: d.rate_limited,
            dm_in_doubt: d.in_doubt,
            dm_unsent: d.unsent,
            dm_triggers: d.triggers,
            dm_stray: d.stray,
            actions_sent,
            replies,
            replies_unmatched,
            post_timeouts,
            rest_fallback_posts,
            reconnects,
            evt_ring_full: s.gateway_ring().1,
            own_cancels: s.sweep_cancels(),
            gw_fills: s.published(pw::FILLS),
            gw_fills_scan_failed: s.published(pw::FILLS_SCAN_FAILED),
            gw_updates: s.published(pw::UPDATES),
            gw_updates_ignored: s.published(pw::UPDATES_IGNORED),
            gw_updates_scan_failed: s.published(pw::UPDATES_SCAN_FAILED),
            gw_answers_unreadable: s.published(pw::ANSWERS_UNREADABLE),
            gw_answers_mismatched: s.published(pw::ANSWERS_MISMATCHED),
            gw_ws_errors: s.published(pw::WS_ERRORS),
            gw_local_refusals: s.published(pw::LOCAL_REFUSALS),
            gw_cancels_untimed: s.published(pw::CANCELS_UNTIMED),
            gw_dials: s.published(pw::DIALS),
            gw_conn_drops: s.published(pw::CONN_DROPS),
            gw_sweep_rounds: s.published(pw::SWEEP_ROUNDS),
            gw_sweep_unmapped: s.published(pw::SWEEP_UNMAPPED),
            gw_throttled: s.published(pw::THROTTLED),
            gw_ttl_cancels: s.published(pw::TTL_CANCELS),
            gw_ttl_full: s.published(pw::TTL_FULL),
            // The router's (`RoutedDispatcher` adds them).
            refused_cap_perp: 0,
            refused_max_resting: 0,
            refused_foreign_row: 0,
            ready: u64::from(self.ready()),
            open_orders: self.n_rows as u64,
            in_flight,
            ws_up,
            halt_reason: s.halt_reason(),
            heartbeat_age_ms: if beat == 0 {
                u64::MAX
            } else {
                now_ns.saturating_sub(beat) / NS_PER_MS
            },
            congestion: s.congestion(),
            rtt_worst_ns: s.rtt_worst_ns(),
            post_rtt_p50_ns: s.post_rtt.quantile_ns(0.5),
            post_rtt_p99_ns: s.post_rtt.quantile_ns(0.99),
            post_rtt_max_ns: s.post_rtt.max_ns(),
            rest_rtt_p99_ns: s.rest_rtt.quantile_ns(0.99),
            rest_rtt_max_ns: s.rest_rtt.max_ns(),
            push_to_write_p99_ns: s.push_to_write.quantile_ns(0.99),
            cancel_rtt_p99_ns: s.cancel_rtt.quantile_ns(0.99),
            fill_lag_p99_ns: s.fill_venue_to_local.quantile_ns(0.99),
            budget_known,
            budget_remaining,
            budget_cancel_remaining,
            rate_ip_sum: s.published(pw::RATE_IP),
            rate_ws_sum: s.published(pw::RATE_WS),
            fees_1e8: c.fees_1e8,
            leverage_dark: u64::from(self.lev_dark.count_ones()),
            // `Availability` is `repr(u8)`: 0 unknown … 3 exhausted.
            dm_availability: self.deadman.availability() as u64,
            dm_deadline_ms: self.deadman.deadline_ms(),
            dm_triggers_today: u64::from(self.deadman.triggers_today()),
            health_blocked: u64::from(self.exec_health(self.cfg.slot, now_ns).blocked),
            // The router's.
            unmarked_positions: 0,
            resting_usd_1e6: 0,
            own_account: u64::from(self.cfg.own_account),
            pnl_anchor_usd_1e6: self.anchor_1e8 / WIRE_PER_ENGINE,
            anchor_kept: u64::from(self.anchor_kept),
            worth_usd_1e6: worth,
            session_pnl_usd_1e6: session_pnl,
            // The buying ended (the halt, the shutdown): 0, as off.
            topup_weight: if self.topup_off {
                0
            } else {
                self.topup.weight()
            },
            topup_day_used: self.topup.used(),
            topup_cost_usd_1e6: self.topup.cost_1e6(),
        };
        true
    }

    fn cancel_all(&mut self) -> Result<(), DispatchError> {
        // The sweep for what the venue holds; a cancel for every order the
        // arm holds (one still in flight is cancelled when it lands).
        self.sweep_asked = self.end.stats.request_sweep();
        let now = now_ns();
        let n = self.n_rows.min(ARM_ORDERS);
        let mut i = 0usize;
        while i < n {
            if self.rows[i].state != ST_FILLING && self.rows[i].cancel != CX_SENT {
                self.push_cancel(i, now);
            }
            i += 1;
        }
        Ok(())
    }

    fn cancel_all_state(&self) -> CancelAllState {
        if self.sweep_asked != 0 {
            let (seq, st) = self.end.stats.sweep();
            if seq < self.sweep_asked {
                return CancelAllState::Working;
            }
            match st {
                SWEEP_STRANDED => return CancelAllState::Stranded,
                SWEEP_CLEAR => {}
                _ => return CancelAllState::Working,
            }
        }
        // The venue read clear; an order the arm still holds may be on the
        // book or may yet land (a read races an in-flight place).
        if self.live_rows() > 0 {
            CancelAllState::Working
        } else {
            CancelAllState::Clear
        }
    }

    /// XMM XH5a: one slot per gateway arm (`cfg.slot`) — that slot's
    /// cancel is the arm's whole cancel (the gateway's sweep takes only
    /// its slots' cloids, XG-1); another slot has nothing here. A gateway
    /// serving several slots (XH4i) carries the slot into its sweep
    /// request.
    fn cancel_slot(&mut self, slot: u8) -> Result<(), DispatchError> {
        if slot == self.cfg.slot {
            // XMM XH5c-2: the router asks this of a HALTED slot alone (its
            // sweep, XH5a) — a halted slot buys nothing more (a purchase
            // that left is still judged and charged).
            self.topup_end();
            return self.cancel_all();
        }
        Ok(())
    }

    fn cancel_slot_state(&self, slot: u8) -> CancelAllState {
        if slot == self.cfg.slot {
            return self.cancel_all_state();
        }
        CancelAllState::Clear
    }

    fn on_shutdown(&mut self) {
        // XMM XH5c-2: the shutdown buys nothing — no top-up decided at a
        // read answered during it, none sent; a purchase that left is still
        // judged and charged, and a hand-over owed goes again each pass —
        // the shutdown waits for it, within its bound (the keeper turns
        // until the gateway is released).
        self.topup_end();
        // The dead man stays armed until nothing of ours can rest; only
        // then is it cleared. At the bound, not confirmed clear: return
        // with it armed — the venue cancels at its deadline.
        let deadline = now_ns().saturating_add(self.cfg.shutdown_ms * NS_PER_MS);
        let _ = self.cancel_all();
        loop {
            let now = now_ns();
            let _ = self.pump(now);
            // The reads go on (a re-read decides orders thought gone).
            let _ = self.recon_step(now);
            if self.topup_owed {
                self.topup_hand_over();
            }
            // Nobody drains after the stop: fills are counted (the venue's
            // sheet holds them for the next boot), the rest dropped.
            self.counters.shutdown_fills += self.fills.clear() as u64;
            let _ = self.events.clear();
            let _ = self.retired.clear();
            let _ = self.renamed.clear();
            let _ = self.row_timers(now);
            match self.cancel_all_state() {
                CancelAllState::Clear => {
                    let _ = self.drive_deadman(false, now);
                    if self.deadman.quiescent(now_ms()) && !self.topup_owed {
                        return;
                    }
                }
                CancelAllState::Stranded => {
                    let _ = self.cancel_all();
                    let _ = self.deadman_step(now);
                }
                CancelAllState::Working => {
                    let _ = self.deadman_step(now);
                }
            }
            if now >= deadline {
                // Clear with the switch parked: only a hand-over still owed
                // held the bound (the top-up file lags a step).
                if self.cancel_all_state() != CancelAllState::Clear {
                    self.counters.shutdown_unconfirmed += 1;
                } else if !self.deadman.quiescent(now_ms()) {
                    self.counters.shutdown_switch_held += 1;
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::budget::{TopupState, TOPUP_PRICE_1E6};
    use crate::cmd::{
        CMD_FLAG_CROSS, CMD_LEVERAGE, CMD_MODIFY, CMD_PLACE, CMD_READ_DAY, CMD_READ_LIMIT,
        CMD_READ_ORDERS, CMD_READ_SPOT, CMD_READ_STATE, CMD_RESERVE, CMD_SCHEDULE_CANCEL,
    };
    use crate::event::{
        EVF_CMD_SHIFT, EVF_RESTING, GAP_FILLS_UNREADABLE, GAP_RECONNECT, SLOT_FOREIGN,
    };
    use crate::gateway::{hl_duplex, HlGatewayEnd};
    use exec_gateway::stats::{CONGESTION_NONE, SWEEP_WORKING};
    use exec_gateway::HALT_EVENT_RING_FULL;

    const SLOT: u8 = 6;
    const SYM: u32 = 42;
    const ASSET: u32 = 3;
    const E8: i64 = 100_000_000;
    const S: u64 = 1_000_000_000;

    fn cfg() -> ArmCfg {
        let bind = PerpBind {
            sym: SYM,
            asset: ASSET,
            rules: PerpRules::new(4).expect("rules"),
        };
        ArmCfg {
            slot: SLOT,
            perps: [bind; ARM_PERPS],
            n_perps: 1,
            expires_after_ms: 5_000,
            post_timeout_ms: 5_000,
            skew_ms: 2_000,
            recon_ms: 5_000,
            join_ms: 1_000,
            gone_ms: 3_000,
            shutdown_ms: 12_000,
            own_account: true,
            anchor_1e6: 0,
            deadman: DeadManCfg::DEFAULT,
            budget_floor: 2_000,
            limit_ms: 600_000,
            leverage: 0,
        }
    }

    /// The gateway's end, scripted: commands pulled into a list the test
    /// picks from, events injected.
    struct Venue {
        gw: HlGatewayEnd,
        cmds: Vec<HlCmd>,
        /// XH5b-2: the arm's anchor hand-over, and a keeper that stores what
        /// it is handed at once while `keeps` (the gateway thread's).
        anchor: Arc<AnchorSlot>,
        keeps: bool,
        kept: Option<i64>,
    }

    impl Venue {
        /// The keeper's pass: the anchor it has stored (`None`: none yet).
        fn keeper(&mut self) -> Option<i64> {
            if self.keeps {
                if let Some(v) = self.anchor.pending() {
                    self.anchor.mark_kept(v);
                    self.kept = Some(v);
                }
            }
            self.kept
        }

        /// The first command of `kind` the arm pushed, taken.
        fn take(&mut self, kind: u8) -> Option<HlCmd> {
            let _ = self.keeper();
            while let Some(c) = self.gw.cmd.pop() {
                self.cmds.push(*c);
            }
            let i = self.cmds.iter().position(|c| c.kind == kind)?;
            Some(self.cmds.remove(i))
        }

        fn send(&mut self, e: ExecEvent) {
            let _ = self.keeper();
            assert!(self.gw.evt.emit(&e), "event ring full");
        }
    }

    /// A post item's verdict, as the gateway builds it.
    fn item(c: &HlCmd, kind: u8, reason: u8, flags: u8) -> ExecEvent {
        let bits = if c.kind <= CMD_CANCEL { c.kind } else { 0 };
        ExecEvent {
            client_oid: c.client_oid,
            asset: c.asset,
            kind,
            reason,
            slot: c.slot,
            flags: EVF_POST_ITEM | (bits << EVF_CMD_SHIFT) | flags,
            ..ExecEvent::default()
        }
    }

    /// An `orderUpdates` row.
    fn row(kind: u8, cid: u64, reason: u8) -> ExecEvent {
        ExecEvent {
            client_oid: cid,
            asset: ASSET,
            kind,
            reason,
            slot: SLOT,
            ..ExecEvent::default()
        }
    }

    fn fill(cid: u64, buy: bool, px_1e8: i64, sz_1e8: i64, venue_ms: u64) -> ExecEvent {
        ExecEvent {
            client_oid: cid,
            venue_ms,
            px_1e8,
            sz_1e8,
            asset: ASSET,
            kind: EV_FILL,
            slot: SLOT,
            flags: if buy { EVF_BUY } else { 0 },
            ..ExecEvent::default()
        }
    }

    fn read_end(c: &HlCmd, account_1e8: i64, venue_ms: u64) -> ExecEvent {
        let read = match c.kind {
            CMD_READ_ORDERS => READ_ORDERS,
            CMD_READ_LIMIT => READ_LIMIT,
            CMD_READ_DAY => READ_DAY,
            CMD_READ_SPOT => READ_SPOT,
            _ => READ_STATE,
        };
        ExecEvent {
            client_oid: c.client_oid,
            venue_ms,
            px_1e8: account_1e8,
            // XH5b-1: a spot sheet that lists USDC.
            sz_1e8: i64::from(c.kind == CMD_READ_SPOT),
            kind: EV_READ_DONE,
            reason: read,
            slot: c.slot,
            ..ExecEvent::default()
        }
    }

    fn open_order(read: &HlCmd, slot: u8, cid: u64) -> ExecEvent {
        ExecEvent {
            client_oid: cid,
            aux: read.client_oid,
            asset: ASSET,
            kind: EV_OPEN_ORDER,
            slot,
            ..ExecEvent::default()
        }
    }

    fn position(read: &HlCmd, sz_1e8: i64, value_1e8: u64) -> ExecEvent {
        ExecEvent {
            client_oid: read.client_oid,
            sz_1e8,
            aux: value_1e8,
            asset: ASSET,
            kind: EV_POSITION,
            slot: read.slot,
            ..ExecEvent::default()
        }
    }

    fn quote(cid: u64, side: Side, px_1e6: i64, qty_1e6: i64) -> Order {
        let mut o = Order::new(
            0,
            VenueId::Hyperliquid,
            SYM,
            side,
            ORDER_KIND_MAKER,
            Price(px_1e6),
            Qty(qty_1e6),
            cid,
        )
        .with_post_only();
        o.strategy_id = SLOT;
        o
    }

    fn cancel_req(cid: u64) -> CancelReq {
        let mut r = CancelReq::new(0, VenueId::Hyperliquid, SYM, cid);
        r.strategy_id = SLOT;
        r
    }

    fn events(arm: &mut HlGatewayArm) -> Vec<(u64, u8, u8)> {
        let mut out = Vec::new();
        let mut e = OrderEvent::ZERO;
        while arm.try_next_order_event(&mut e) {
            out.push((e.client_oid, e.kind, e.reason));
        }
        out
    }

    /// The retirements, as the router's next poll takes them (XH4f: the
    /// arm holds each a poll behind its order's fills).
    fn retired(arm: &mut HlGatewayArm) -> Vec<u64> {
        arm.retired.release();
        let mut out = Vec::new();
        while let Some(r) = arm.try_next_retired() {
            assert_eq!(r.slot, SLOT);
            out.push(r.client_oid);
        }
        out
    }

    /// XH4f: the renames, `(prev, new)`.
    fn renamed(arm: &mut HlGatewayArm) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        while let Some(n) = arm.try_next_renamed() {
            assert_eq!(n.slot, SLOT);
            out.push((n.prev_client_oid, n.client_oid));
        }
        out
    }

    /// Up to the first sheet: the dead man's boot clear answered, the
    /// snapshot, the sweep read clear; the sheet read issued.
    fn to_sheet(c: &ArmCfg) -> (HlGatewayArm, Venue, u64, HlCmd) {
        let (eng, gw) = hl_duplex();
        let mut arm = HlGatewayArm::new(c, eng).expect("arm");
        let anchor = Arc::new(AnchorSlot::new());
        arm.keep_anchor_in(Arc::clone(&anchor));
        let mut v = Venue {
            gw,
            cmds: Vec::new(),
            anchor,
            keeps: true,
            kept: None,
        };
        let t = now_ns();
        v.gw.stats.set_gauges(0, true);
        v.gw.stats.beat(t);
        let _ = arm.idle(t);
        let clear = v
            .take(CMD_SCHEDULE_CANCEL)
            .expect("the boot clear goes first");
        v.send(item(&clear, EV_ACK, 0, 0));
        assert_eq!(
            arm.submit(&quote(1, Side::Bid, 100_000_000, 1_000_000)),
            Err(DispatchError::Disconnected),
            "no orders before the boot"
        );
        v.send(ExecEvent {
            kind: EV_SNAPSHOT,
            ..ExecEvent::default()
        });
        let _ = arm.idle(t);
        assert_eq!(
            arm.cancel_all_state(),
            CancelAllState::Working,
            "the boot sweep"
        );
        v.gw.stats
            .set_sweep(v.gw.stats.sweep_requested(), SWEEP_CLEAR);
        let _ = arm.idle(t);
        let st = v.take(CMD_READ_STATE).expect("the sheet after the sweep");
        (arm, v, t, st)
    }

    /// The budget read's end: `used` requests of a fresh address's 10 000.
    fn limit_end(c: &HlCmd, used: u64) -> ExecEvent {
        ExecEvent {
            aux: used,
            ..read_end(c, 0, 0)
        }
    }

    /// A day page's end: `rows` rows summing `sum_1e8`, the last at
    /// `last_ms`.
    fn day_end(c: &HlCmd, rows: u64, sum_1e8: i64, last_ms: u64) -> ExecEvent {
        ExecEvent {
            aux: rows,
            ..read_end(c, sum_1e8, last_ms)
        }
    }

    /// XH5b-1: the worth's spot half, read right after a sheet on the
    /// slot's own account — answered with `usdc_1e8` (none on a shared
    /// account).
    fn spot(arm: &mut HlGatewayArm, v: &mut Venue, at: u64, usdc_1e8: i64) {
        if !arm.cfg.own_account {
            return;
        }
        let _ = arm.idle(at);
        let sp = v
            .take(CMD_READ_SPOT)
            .expect("the spot sheet after the perp one");
        v.send(read_end(&sp, usdc_1e8, 0));
        let _ = arm.idle(at);
    }

    /// Answer the sheet (account 1 000 USD at venue ms 1, flat), its spot
    /// half on the slot's own account ($0 USDC) and the orders; up to the
    /// budget read, issued.
    fn to_budget(c: &ArmCfg) -> (HlGatewayArm, Venue, u64, HlCmd) {
        let (mut arm, mut v, t, st) = to_sheet(c);
        v.send(read_end(&st, 1_000 * E8, 1));
        let _ = arm.idle(t);
        spot(&mut arm, &mut v, t, 0);
        let od = v.take(CMD_READ_ORDERS).expect("the orders after the sheet");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        assert!(!arm.ready(), "not before the budget");
        let lim = v.take(CMD_READ_LIMIT).expect("the budget after the orders");
        (arm, v, t, lim)
    }

    /// Booted: the budget answered (100 requests used), the day read (no
    /// fills).
    fn booted_with(c: &ArmCfg) -> (HlGatewayArm, Venue, u64) {
        let (mut arm, mut v, t, lim) = to_budget(c);
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        assert!(arm.ready());
        let day = v
            .take(CMD_READ_DAY)
            .expect("the day's spend after the budget");
        v.send(day_end(&day, 0, 0, 0));
        let _ = arm.idle(t);
        (arm, v, t)
    }

    fn booted() -> (HlGatewayArm, Venue, u64) {
        booted_with(&cfg())
    }

    /// Place `cid` and answer it resting.
    fn rested(arm: &mut HlGatewayArm, v: &mut Venue, t: u64, cid: u64) -> HlCmd {
        arm.submit(&quote(cid, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let c = v.take(CMD_PLACE).expect("the place");
        v.send(item(&c, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        assert_eq!(
            events(arm),
            [(cid, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE)]
        );
        c
    }

    /// The periodic sheet, its spot half ($0 USDC: the worth unchanged)
    /// and the orders read at `at`, answered (the sheet with `pos`, the
    /// orders listing `listed`).
    fn recon(
        arm: &mut HlGatewayArm,
        v: &mut Venue,
        at: u64,
        pos: Option<(i64, u64)>,
        listed: &[u64],
    ) {
        let _ = arm.idle(at);
        let st = v.take(CMD_READ_STATE).expect("the sheet");
        if let Some((sz, value)) = pos {
            v.send(position(&st, sz, value));
        }
        v.send(read_end(&st, 1_000 * E8, 2));
        let _ = arm.idle(at);
        spot(arm, v, at, 0);
        let od = v.take(CMD_READ_ORDERS).expect("the orders");
        let mut k = 0usize;
        while k < listed.len() {
            v.send(open_order(&od, SLOT, listed[k]));
            k += 1;
        }
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(at);
    }

    /// Answer the arm's commands from another thread while `on_shutdown`
    /// blocks: cancels accepted, the dead man's posts accepted when
    /// `switch`, the sweep word set to `sweep(request)`.
    fn responder(
        mut v: Venue,
        sweep: fn(u64) -> u64,
        switch: bool,
        stop: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<Venue> {
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                loop {
                    let c = match v.gw.cmd.pop() {
                        Some(p) => *p,
                        None => break,
                    };
                    v.cmds.push(c);
                    if c.kind == CMD_CANCEL || (switch && c.kind == CMD_SCHEDULE_CANCEL) {
                        assert!(v.gw.evt.emit(&item(&c, EV_ACK, 0, 0)));
                    }
                }
                let asked = v.gw.stats.sweep_requested();
                v.gw.stats.set_sweep(asked, sweep(asked));
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            v
        })
    }

    /// A configuration whose dead man re-arms every 10 ms (so a test sees
    /// it armed) and whose shutdown is bounded at 300 ms.
    fn quick() -> ArmCfg {
        let mut c = cfg();
        c.deadman.rearm_ms = 10;
        c.shutdown_ms = 300;
        c
    }

    /// Booted with `c`, one order resting, the dead man armed and its arm
    /// accepted.
    fn armed(c: &ArmCfg) -> (HlGatewayArm, Venue) {
        let (mut arm, mut v, t) = booted_with(c);
        let _ = rested(&mut arm, &mut v, t, 1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = arm.idle(t);
        let set = v.take(CMD_SCHEDULE_CANCEL).expect("the switch arms");
        assert_ne!(set.time_ms, 0, "an arm, not a clear");
        v.send(item(&set, EV_ACK, 0, 0));
        let _ = arm.idle(t);
        assert_ne!(arm.deadman().deadline_ms(), 0);
        (arm, v)
    }

    #[test]
    fn a_quote_is_rounded_rests_fills_and_ends_filled_after_its_fill() {
        let (mut arm, mut v, t) = booted();
        arm.submit(&quote(7, Side::Bid, 100_123_456, 500_000))
            .expect("submit");
        let c = v.take(CMD_PLACE).expect("the place");
        // LAW XH-4: a bid rounds down to the tick, never up.
        assert_eq!(
            (
                c.px_1e8,
                c.sz_1e8,
                c.tif,
                c.expires_after_ms,
                c.slot,
                c.asset
            ),
            (
                10_012_000_000,
                50_000_000,
                Tif::Alo as u8,
                5_000,
                SLOT,
                ASSET
            )
        );
        v.send(item(&c, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [(7, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE)]
        );
        v.send(row(EV_FILLED, 7, 0));
        let _ = arm.idle(t);
        assert!(events(&mut arm).is_empty(), "FILLED waits for its fill");
        v.send(fill(7, true, 10_012_000_000, 50_000_000, 2));
        let _ = arm.idle(t);
        let f = arm.try_next_fill().expect("the fill");
        assert_eq!(
            (f.order_id, f.strategy_id, f.origin, f.px.0, f.qty.0, f.side),
            (7, SLOT, FILL_ORIGIN_VENUE, 100_120_000, 500_000, Side::Bid)
        );
        assert_eq!(
            events(&mut arm),
            [(7, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_NONE)]
        );
        assert_eq!(retired(&mut arm), [7], "a fill's end retires its row");
        assert_eq!(arm.open_orders(), 0);
    }

    /// XH4f (O-XH6): with a leverage configured, each bound perp's goes
    /// once the seeding sheet read it flat — cross, charged — and the arm
    /// takes no order before it is answered; one refused unsent goes again
    /// a read period later; one the venue refuses leaves the perp dark and
    /// its growth refused.
    #[test]
    fn each_perp_s_leverage_is_set_before_the_arm_takes_orders() {
        let mut c = cfg();
        c.leverage = 3;
        let (mut arm, mut v, t, lim) = to_budget(&c);
        let lev = v
            .take(CMD_LEVERAGE)
            .expect("the leverage, after the seeding sheet");
        assert_eq!(
            (lev.asset, lev.arg, lev.flags & CMD_FLAG_CROSS),
            (ASSET, 3, CMD_FLAG_CROSS)
        );
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        assert!(!arm.ready(), "not before its leverage is answered");
        v.send(item(&lev, EV_REJECTED, REASON_THROTTLED, 0));
        let _ = arm.idle(t);
        assert!(v.take(CMD_LEVERAGE).is_none(), "not at once");
        assert_eq!(arm.counters().refunded, 1, "it never left");
        let _ = arm.idle(t + 5 * S);
        let lev = v.take(CMD_LEVERAGE).expect("a read period later");
        v.send(item(&lev, EV_ACK, 0, 0));
        let _ = arm.idle(t + 5 * S);
        assert!(arm.ready());
        assert_eq!(arm.counters().stray, 0);
        // A venue refusal: dark.
        let (mut arm, mut v, t, lim) = to_budget(&c);
        let lev = v.take(CMD_LEVERAGE).expect("the leverage");
        v.send(limit_end(&lim, 100));
        v.send(item(&lev, EV_REJECTED, ORDER_EVENT_REASON_OTHER, 0));
        let _ = arm.idle(t);
        assert!(arm.ready(), "settled: dark");
        assert_eq!(
            arm.submit(&quote(1, Side::Bid, 100_000_000, 500_000)),
            Err(DispatchError::SlotDisabled)
        );
        assert_eq!(
            (
                arm.counters().leverage_dark,
                arm.counters().refused_leverage
            ),
            (1, 1)
        );
        assert_eq!(
            HlGatewayArm::new(
                &ArmCfg {
                    leverage: MAX_LEVERAGE + 1,
                    ..cfg()
                },
                hl_duplex().0
            )
            .err(),
            Some(ArmErr::Leverage)
        );
    }

    /// XH4f (the queued-arm contract): slot 6's verbs confirm later; a
    /// retirement reaches the router a poll behind the fills of its order,
    /// as filled; a landed modify renames the router's row at the
    /// replacement's size instead of retiring the original.
    #[test]
    fn the_arm_keeps_the_queued_arm_contract() {
        let (mut arm, mut v, t) = booted();
        let hl = VenueId::Hyperliquid as u8;
        assert!(arm.verbs_confirm_later(SYM, hl, SLOT));
        assert!(!arm.verbs_confirm_later(SYM, hl, 3), "one slot's arm");
        let _ = rested(&mut arm, &mut v, t, 7);
        v.send(fill(7, true, 10_012_000_000, 50_000_000, 2));
        v.send(row(EV_FILLED, 7, 0));
        let _ = arm.idle(t);
        assert!(arm.try_next_fill().is_some());
        assert!(arm.try_next_retired().is_none(), "a poll behind its fills");
        let _ = arm.idle(t);
        let r = arm.try_next_retired().expect("the next poll");
        assert_eq!((r.client_oid, r.why), (7, RETIRED_FILLED));
        assert_eq!(
            events(&mut arm),
            [(7, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_NONE)]
        );
        let _ = rested(&mut arm, &mut v, t, 1);
        arm.modify(&ModifyReq::new(1, quote(2, Side::Bid, 99_000_000, 700_000)))
            .expect("modify");
        let m = v.take(CMD_MODIFY).expect("the modify");
        v.send(item(&m, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        let n = arm.try_next_renamed().expect("the rename");
        assert_eq!(
            (
                n.prev_client_oid,
                n.client_oid,
                n.qty_1e6,
                n.sym,
                n.slot,
                n.buy
            ),
            (1, 2, 700_000, SYM, SLOT, 1)
        );
        assert!(
            retired(&mut arm).is_empty(),
            "the original is renamed, not retired"
        );
    }

    /// XH4f (D9): the order's TTL rides to the gateway (clamped at 30 s),
    /// and the gateway's TTL cancel closes it `CANCELED(EXPIRED)`, retired
    /// as the TTL's.
    #[test]
    fn a_place_carries_its_ttl_and_the_gateway_s_ttl_cancel_retires_it_as_the_ttl_s() {
        let (mut arm, mut v, t) = booted();
        arm.submit(&quote(7, Side::Bid, 100_000_000, 500_000).with_ttl_ns(90 * S))
            .expect("submit");
        let c = v.take(CMD_PLACE).expect("the place");
        assert_eq!(c.time_ms, crate::ttl::TTL_MAX_MS, "clamped at 30 s");
        v.send(item(&c, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        let _ = events(&mut arm);
        let ttl_cancel = HlCmd::cancel(SLOT, 7, ASSET);
        v.send(item(&ttl_cancel, EV_ACK, ORDER_EVENT_REASON_EXPIRED, 0));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [(7, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_EXPIRED)]
        );
        let _ = arm.idle(t);
        let r = arm.try_next_retired().expect("retired");
        assert_eq!((r.client_oid, r.why), (7, RETIRED_CANCELED_TTL));
        arm.submit(&quote(8, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        assert_eq!(
            v.take(CMD_PLACE).map(|c| c.time_ms),
            Some(0),
            "no TTL: none"
        );
    }

    /// XH4f (D7): the arm's word on slot 6 — not booted until the budget
    /// is read, the budget below its floor, the gateway's congestion and
    /// its answers' worst round trip; another slot's is not its to judge.
    #[test]
    fn the_arm_s_health_is_its_boot_its_budget_and_the_gateway_s_words() {
        use core_types::{
            ExecHealth, EXEC_BLOCKED_BUDGET, EXEC_BLOCKED_NOT_READY, EXEC_CONGESTION_THROTTLED,
        };
        let (mut arm, mut v, t, lim) = to_budget(&cfg());
        assert_eq!(arm.exec_health(SLOT, t).blocked, EXEC_BLOCKED_NOT_READY);
        // 9 000 of 10 000 used: 1 000 left, under the 2 000 floor.
        v.send(limit_end(&lim, 9_000));
        let _ = arm.idle(t);
        assert_eq!(arm.exec_health(SLOT, t).blocked, EXEC_BLOCKED_BUDGET);
        let (arm, v, t) = booted();
        assert_eq!(arm.exec_health(SLOT, t), ExecHealth::HEALTHY);
        v.gw.stats.set_congestion(CONGESTION_THROTTLED);
        v.gw.stats.set_rtt_worst(1_700 * NS_PER_MS + 1);
        assert_eq!(
            arm.exec_health(SLOT, t),
            ExecHealth::new(0, EXEC_CONGESTION_THROTTLED, 1_700)
        );
        assert_eq!(
            arm.exec_health(3, t),
            ExecHealth::HEALTHY,
            "slot 3's is not ours"
        );
    }

    /// XH4f (D11): the report reads the arm's counters, its budget and the
    /// gateway's words — shared, and published at its heartbeat; the
    /// router's are left to the router.
    #[test]
    fn the_arm_s_report_reads_its_counters_its_budget_and_the_gateway_s_words() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 8);
        v.gw.stats.publish(pw::THROTTLED, 3);
        v.gw.stats.publish(pw::RATE_WS, 9);
        v.gw.stats.note_post_timeout();
        v.gw.stats.rest_rtt.record(2_500 * NS_PER_MS);
        let mut r = GatewayArmReport::default();
        assert!(arm.gateway_arm_report(t + 5 * NS_PER_MS, &mut r));
        assert_eq!((r.present, r.slot, r.ready), (1, u64::from(SLOT), 1));
        assert!(r.pushed >= 1 && r.pushed == arm.counters().pushed);
        assert_eq!(r.open_orders, 1);
        let left = arm.budget().map_or(i64::MIN, AddressBudget::remaining);
        assert_eq!((r.budget_known, r.budget_remaining), (1, left));
        assert_eq!(
            (r.gw_throttled, r.rate_ws_sum, r.post_timeouts),
            (3, 9, 1),
            "the throttled word, the WebSocket window, a shared counter"
        );
        assert_eq!((r.ws_up, r.heartbeat_age_ms), (1, 5));
        assert_eq!(
            (r.rest_rtt_max_ns, r.post_rtt_max_ns),
            (2_500 * NS_PER_MS, 0),
            "XMM XH5c-3: the REST fallback's own series"
        );
        assert_eq!(
            (r.refused_cap_perp, r.unmarked_positions, r.resting_usd_1e6),
            (0, 0, 0),
            "the router's words"
        );
        // XH5b-2: the own account's worth, its anchor kept, the session.
        assert_eq!((r.spot_reads, r.spot_failed), (1, 0));
        assert_eq!(
            (r.own_account, r.pnl_anchor_usd_1e6, r.anchor_kept),
            (1, 1_000_000_000, 1)
        );
        assert_eq!((r.worth_usd_1e6, r.session_pnl_usd_1e6), (1_000_000_000, 0));
    }

    /// XH4f (risk r1): the leverage waits for the seeding sheet to read the
    /// bound perps flat — on a shared account one may hold another slot's
    /// position, and a held boot changes nothing on it.
    #[test]
    fn the_leverage_waits_for_the_seeding_sheet_to_read_the_perps_flat() {
        let mut c = cfg();
        c.leverage = 3;
        let (mut arm, mut v, t, st) = to_sheet(&c);
        assert!(v.take(CMD_LEVERAGE).is_none(), "not before the sheet");
        v.send(position(&st, E8 / 2, 50 * E8 as u64));
        v.send(read_end(&st, 1_000 * E8, 1));
        let _ = arm.idle(t);
        assert!(arm.boot_position());
        let _ = arm.idle(t + 60 * S);
        assert!(v.take(CMD_LEVERAGE).is_none(), "a held boot sends none");
    }

    /// XH4f (risk r1): a retirement waits while fills are queued — the
    /// engine books an order's fills before its row is released.
    #[test]
    fn a_retirement_waits_while_fills_are_queued() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 8);
        v.send(ExecEvent {
            client_oid: 8,
            venue_ms: 1_790_000_000_000,
            px_1e8: 100 * E8,
            sz_1e8: E8 / 200,
            asset: ASSET,
            kind: EV_FILL,
            slot: SLOT,
            flags: EVF_BUY,
            ..ExecEvent::default()
        });
        v.send(row(EV_CANCELED, 8, ORDER_EVENT_REASON_CANCEL_REQUESTED));
        let _ = arm.idle(t);
        let _ = arm.idle(t);
        assert!(arm.try_next_retired().is_none(), "a fill is still queued");
        assert!(arm.try_next_fill().is_some());
        let _ = arm.idle(t);
        assert_eq!(arm.try_next_retired().map(|r| r.client_oid), Some(8));
    }

    #[test]
    fn filled_goes_after_the_join_window_without_its_fills() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 8);
        v.send(row(EV_FILLED, 8, 0));
        let _ = arm.idle(t);
        let _ = arm.idle(t + S / 2);
        assert!(events(&mut arm).is_empty());
        let _ = arm.idle(t + S + 1);
        assert_eq!(
            events(&mut arm),
            [(8, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_NONE)]
        );
        assert_eq!(arm.counters().join_timeouts, 1);
    }

    /// XMM XH5d: an ALO place bids its order's priority in the command's
    /// `arg` — none until the session's cap is armed, and none past it,
    /// each refused unsent; a modify bids none (`batchModify` carries no
    /// grouping); a priority over 1 % of resting notional is refused unsent.
    #[test]
    fn an_alo_place_bids_its_priority_within_the_cap_and_a_modify_none() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        let bid = quote(2, Side::Bid, 99_000_000, 500_000).with_alo_priority(10_000);
        arm.modify(&ModifyReq::new(1, bid)).expect("modify");
        assert_eq!(
            v.take(CMD_MODIFY).expect("the modify").arg,
            0,
            "a modify bids none"
        );
        let ask =
            |cid: u64, p: u32| quote(cid, Side::Ask, 100_123_456, 500_000).with_alo_priority(p);
        let refused = arm.counters().refused_local;
        assert!(arm.submit(&ask(7, 10_000)).is_err(), "unarmed: no priority");
        assert!(v.take(CMD_PLACE).is_none(), "and never sent");
        assert_eq!(arm.counters().refused_local, refused + 1);
        // 1 bp of 0.5 at 100.13 commits $0.005007: two fit $0.015, a third
        // does not.
        arm.set_priority_cap(15_000);
        arm.submit(&ask(8, 10_000)).expect("the first");
        assert_eq!(v.take(CMD_PLACE).expect("the place").arg, 10_000);
        arm.submit(&ask(9, 10_000)).expect("the second");
        assert!(v.take(CMD_PLACE).is_some());
        assert!(
            arm.submit(&ask(10, 10_000)).is_err(),
            "past the cap: refused"
        );
        assert!(v.take(CMD_PLACE).is_none(), "and never sent");
        arm.submit(&ask(11, 0))
            .expect("a place bidding none still goes");
        assert_eq!(v.take(CMD_PLACE).expect("the place").arg, 0);
        assert!(
            arm.submit(&ask(12, 1_000_001)).is_err(),
            "over 1 %: refused"
        );
        assert!(v.take(CMD_PLACE).is_none(), "and never sent");
    }

    #[test]
    fn a_modify_is_the_new_resting_then_the_old_replaced() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        arm.modify(&ModifyReq::new(1, quote(2, Side::Bid, 99_000_000, 500_000)))
            .expect("modify");
        let m = v.take(CMD_MODIFY).expect("the modify");
        assert_eq!((m.client_oid, m.prev_client_oid), (2, 1));
        assert_eq!(
            arm.modify(&ModifyReq::new(1, quote(3, Side::Bid, 98_000_000, 500_000))),
            Err(DispatchError::NoSuchOrder),
            "one modify in flight per order"
        );
        v.send(item(&m, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [
                (2, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE),
                (1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_REPLACED)
            ]
        );
        assert!(retired(&mut arm).is_empty());
        assert_eq!(renamed(&mut arm), [(1, 2)]);
        assert_eq!(arm.open_orders(), 1);
    }

    /// XH4h-3 (E7-F2): an IoC that could not match ends REJECTED with its
    /// reason, retired — and moves neither `rejects` nor the streak.
    #[test]
    fn an_ioc_miss_is_no_reject_streak() {
        let (mut arm, mut v, t) = booted();
        let mut o = quote(9, Side::Bid, 100_000_000, 500_000);
        o.kind = ORDER_KIND_IOC;
        o.flags = 0;
        arm.submit(&o).expect("submit");
        let c = v.take(CMD_PLACE).expect("the IoC");
        v.send(item(&c, EV_REJECTED, REASON_IOC_MISS, 0));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [(9, ORDER_EVENT_REJECTED, REASON_IOC_MISS)]
        );
        assert_eq!(retired(&mut arm), [9]);
        assert_eq!((arm.counters().rejects, arm.reject_streak), (0, 0));
    }

    #[test]
    fn a_refused_modify_is_the_new_rejected_alone_and_counts_as_an_alo_cross() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        arm.modify(&ModifyReq::new(
            1,
            quote(2, Side::Bid, 101_000_000, 500_000),
        ))
        .expect("modify");
        let m = v.take(CMD_MODIFY).expect("the modify");
        v.send(item(&m, EV_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX, 0));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [(2, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX)]
        );
        assert_eq!(retired(&mut arm), [2]);
        assert_eq!((arm.counters().alo_rejects, arm.reject_streak), (1, 0));
        // The predecessor rests on: the member cancels it.
        arm.cancel(&cancel_req(1)).expect("cancel");
        assert_eq!(v.take(CMD_CANCEL).map(|c| c.client_oid), Some(1));
    }

    #[test]
    fn the_venue_s_end_of_an_original_waits_for_its_modify_s_answer() {
        // Refused: the new REJECTED, then the old one's end as the stream
        // said.
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        arm.modify(&ModifyReq::new(
            1,
            quote(2, Side::Bid, 101_000_000, 500_000),
        ))
        .expect("modify");
        let m = v.take(CMD_MODIFY).expect("the modify");
        v.send(row(EV_CANCELED, 1, ORDER_EVENT_REASON_OTHER));
        let _ = arm.idle(t);
        assert!(events(&mut arm).is_empty(), "held for the modify's answer");
        v.send(item(&m, EV_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX, 0));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [
                (2, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX),
                (1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER)
            ]
        );
        // Accepted: the new RESTING, then the old one REPLACED.
        let _ = rested(&mut arm, &mut v, t, 3);
        arm.modify(&ModifyReq::new(3, quote(4, Side::Bid, 99_000_000, 500_000)))
            .expect("modify");
        let m = v.take(CMD_MODIFY).expect("the modify");
        v.send(row(EV_CANCELED, 3, ORDER_EVENT_REASON_OTHER));
        v.send(item(&m, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [
                (4, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE),
                (3, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_REPLACED)
            ]
        );
    }

    #[test]
    fn a_modify_s_replacement_that_lands_by_a_fill_makes_three_events_in_order() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        arm.modify(&ModifyReq::new(1, quote(2, Side::Bid, 99_000_000, 500_000)))
            .expect("modify");
        let _ = v.take(CMD_MODIFY).expect("the modify");
        v.send(fill(2, true, 9_900_000_000, 50_000_000, 3));
        let _ = arm.idle(t);
        assert!(arm.try_next_fill().is_some(), "the fill first");
        assert_eq!(
            events(&mut arm),
            [
                (2, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE),
                (1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_REPLACED),
                (2, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_NONE)
            ]
        );
    }

    #[test]
    fn a_cancel_that_overtook_its_place_is_kept_and_sent_again() {
        let (mut arm, mut v, t) = booted();
        arm.submit(&quote(1, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let p = v.take(CMD_PLACE).expect("the place");
        arm.cancel(&cancel_req(1)).expect("cancel");
        let x = v.take(CMD_CANCEL).expect("the cancel");
        v.send(item(&x, EV_REJECTED, REASON_NOT_FOUND, 0));
        let _ = arm.idle(t);
        assert!(
            events(&mut arm).is_empty(),
            "not found about an unanswered place is no end"
        );
        v.send(item(&p, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [(1, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE)]
        );
        let x = v.take(CMD_CANCEL).expect("the cancel again, once it rests");
        v.send(item(&x, EV_ACK, 0, 0));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [(1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_CANCEL_REQUESTED)]
        );
        assert_eq!(retired(&mut arm), [1]);
    }

    #[test]
    fn a_place_in_doubt_is_settled_by_a_cancel_after_its_expiry_and_a_skew() {
        let (mut arm, mut v, t) = booted();
        // Never landed: REJECTED(EXPIRED).
        arm.submit(&quote(1, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let p = v.take(CMD_PLACE).expect("the place");
        v.send(item(&p, EV_UNANSWERED, 0, 0));
        let _ = arm.idle(t);
        let _ = arm.idle(t + 7 * S - 1);
        assert!(v.take(CMD_CANCEL).is_none(), "it may still land");
        let _ = arm.idle(t + 7 * S);
        let x = v.take(CMD_CANCEL).expect("the settling cancel");
        v.send(item(&x, EV_REJECTED, REASON_NOT_FOUND, 0));
        let _ = arm.idle(t + 7 * S);
        assert_eq!(
            events(&mut arm),
            [(1, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_EXPIRED)]
        );
        // Landed: its RESTING, then CANCELED(OTHER) — nobody asked.
        arm.submit(&quote(2, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let p = v.take(CMD_PLACE).expect("the place");
        v.send(item(&p, EV_UNANSWERED, 0, 0));
        let _ = arm.idle(t + 8 * S);
        let _ = arm.idle(t + 15 * S);
        let x = v.take(CMD_CANCEL).expect("the settling cancel");
        v.send(item(&x, EV_ACK, 0, 0));
        let _ = arm.idle(t + 15 * S);
        assert_eq!(
            events(&mut arm),
            [
                (2, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE),
                (2, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER)
            ]
        );
        assert_eq!(arm.counters().doubt_settled, 2);
    }

    #[test]
    fn a_cancel_asked_in_doubt_goes_at_once_and_is_kept_for_the_doubt_s_end() {
        let (mut arm, mut v, t) = booted();
        arm.submit(&quote(1, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let p = v.take(CMD_PLACE).expect("the place");
        v.send(item(&p, EV_UNANSWERED, 0, 0));
        let _ = arm.idle(t);
        arm.cancel(&cancel_req(1)).expect("cancel");
        let x = v.take(CMD_CANCEL).expect("at once");
        v.send(item(&x, EV_REJECTED, REASON_NOT_FOUND, 0));
        let _ = arm.idle(t + 1);
        assert!(events(&mut arm).is_empty(), "it may still land");
        let _ = arm.idle(t + 7 * S);
        let x = v.take(CMD_CANCEL).expect("again at the doubt's end");
        v.send(item(&x, EV_REJECTED, REASON_NOT_FOUND, 0));
        let _ = arm.idle(t + 7 * S);
        assert_eq!(
            events(&mut arm),
            [(1, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_EXPIRED)]
        );
    }

    #[test]
    fn a_resting_order_a_read_stops_listing_is_read_again_then_closed() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        let _ = rested(&mut arm, &mut v, t, 2);
        recon(&mut arm, &mut v, t + 5 * S, None, &[2]);
        let od = v.take(CMD_READ_ORDERS).expect("a confirming read at once");
        v.send(open_order(&od, SLOT, 2));
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 6 * S);
        assert!(events(&mut arm).is_empty());
        let _ = arm.idle(t + 8 * S);
        assert_eq!(
            events(&mut arm),
            [(1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER)]
        );
        assert_eq!(retired(&mut arm), [1]);
        assert_eq!((arm.counters().closed_unseen, arm.open_orders()), (1, 1));
    }

    #[test]
    fn not_found_about_a_resting_order_reads_the_orders_and_a_listing_brings_it_back() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        arm.cancel(&cancel_req(1)).expect("cancel");
        let x = v.take(CMD_CANCEL).expect("the cancel");
        v.send(item(&x, EV_REJECTED, REASON_NOT_FOUND, 0));
        let _ = arm.idle(t + 1);
        let od = v
            .take(CMD_READ_ORDERS)
            .expect("a read at once, not at the cadence");
        v.send(open_order(&od, SLOT, 1));
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 2);
        let _ = arm.idle(t + 4 * S);
        assert!(
            events(&mut arm).is_empty(),
            "listed: it rests, its gone wait is over"
        );
        let x = v.take(CMD_CANCEL).expect("the asked cancel goes again");
        v.send(item(&x, EV_ACK, 0, 0));
        let _ = arm.idle(t + 4 * S);
        assert_eq!(
            events(&mut arm),
            [(1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_CANCEL_REQUESTED)]
        );
    }

    #[test]
    fn an_order_closed_unseen_that_a_later_read_lists_is_cancelled_and_pins_the_drift() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        recon(&mut arm, &mut v, t + 5 * S, None, &[]);
        let od = v.take(CMD_READ_ORDERS).expect("the confirming read");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 5 * S);
        let _ = arm.idle(t + 8 * S);
        assert_eq!(
            events(&mut arm),
            [(1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER)]
        );
        recon(&mut arm, &mut v, t + 10 * S, None, &[1]);
        assert_eq!(v.take(CMD_CANCEL).map(|c| c.client_oid), Some(1));
        assert_eq!(arm.counters().unknown_orders, 1);
        assert_eq!(arm.signal(t + 10 * S).recon_drift_usd_1e6, i64::MAX);
    }

    #[test]
    fn an_unknown_order_of_the_slot_is_cancelled_and_pins_the_drift() {
        let (mut arm, mut v, t) = booted();
        assert!(arm.exclusive);
        let _ = arm.idle(t + 5 * S);
        let st = v.take(CMD_READ_STATE).expect("the sheet");
        v.send(read_end(&st, 1_000 * E8, 2));
        let _ = arm.idle(t + 5 * S);
        spot(&mut arm, &mut v, t + 5 * S, 0);
        let od = v.take(CMD_READ_ORDERS).expect("the orders");
        v.send(open_order(&od, SLOT, 999));
        v.send(open_order(&od, SLOT_FOREIGN, 0));
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 5 * S);
        assert_eq!(v.take(CMD_CANCEL).map(|c| c.client_oid), Some(999));
        assert_eq!(arm.counters().unknown_orders, 1);
        let sig = arm.signal(t + 5 * S);
        assert_eq!((sig.recon_drift_usd_1e6, sig.reconciled), (i64::MAX, 0));
        assert!(
            !arm.exclusive,
            "another slot's order: the account is shared"
        );
    }

    #[test]
    fn a_gap_or_another_slot_s_resting_order_ends_the_account_s_exclusivity() {
        let (mut arm, mut v, t) = booted();
        assert!(arm.exclusive);
        v.send(ExecEvent {
            kind: EV_GAP,
            ..ExecEvent::default()
        });
        let _ = arm.idle(t);
        assert!(!arm.exclusive, "a gap may have hidden another slot's order");
        let od = v.take(CMD_READ_ORDERS).expect("a read at once");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        assert!(arm.exclusive, "read clear again");
        v.send(ExecEvent {
            slot: 3,
            ..row(EV_RESTING, 5, 0)
        });
        let _ = arm.idle(t);
        assert!(!arm.exclusive);
    }

    #[test]
    fn drift_is_noted_only_when_two_sheets_disagree() {
        let (mut arm, mut v, t) = booted();
        // A fill of ours after the seeding sheet: booked +0.5 at 100.
        v.send(fill(55, true, 100 * E8, E8 / 2, 10));
        let _ = arm.idle(t);
        recon(&mut arm, &mut v, t + 5 * S, None, &[]);
        assert_eq!(
            arm.signal(t + 5 * S).recon_drift_usd_1e6,
            0,
            "one sheet is a race"
        );
        recon(&mut arm, &mut v, t + 10 * S, None, &[]);
        assert_eq!(
            arm.signal(t + 10 * S).recon_drift_usd_1e6,
            50_000_000,
            "0.5 × 100 USD"
        );
        // The sheet catches up: agreement again, the high water stays.
        recon(
            &mut arm,
            &mut v,
            t + 15 * S,
            Some((E8 / 2, 50 * E8 as u64)),
            &[],
        );
        let sig = arm.signal(t + 15 * S);
        assert_eq!((sig.recon_drift_usd_1e6, sig.reconciled), (50_000_000, 1));
    }

    #[test]
    fn fills_before_the_seeding_sheet_are_booked_after_its_time_only() {
        let (mut arm, mut v, t, st) = to_sheet(&cfg());
        v.send(fill(0, true, 100 * E8, E8 / 2, 5));
        v.send(fill(0, true, 100 * E8, 3 * E8 / 10, 20));
        v.send(read_end(&st, 1_000 * E8, 10));
        let _ = arm.idle(t);
        assert_eq!(arm.perps[0].booked_1e8, 3 * E8 / 10, "the later 0.3 only");
    }

    #[test]
    fn a_boot_whose_sheet_holds_a_bound_position_is_held() {
        let (mut arm, mut v, t, st) = to_sheet(&cfg());
        v.send(position(&st, E8 / 2, 50 * E8 as u64));
        v.send(read_end(&st, 1_000 * E8, 1));
        let _ = arm.idle(t);
        spot(&mut arm, &mut v, t, 0);
        let od = v.take(CMD_READ_ORDERS).expect("the orders");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        assert!(arm.boot_position() && !arm.ready());
        assert_eq!(
            arm.submit(&quote(1, Side::Bid, 100_000_000, 500_000)),
            Err(DispatchError::Disconnected)
        );
        assert_eq!(
            arm.signal(t).reconciled,
            0,
            "the ledger is not seeded over it"
        );
    }

    #[test]
    fn a_restored_anchor_is_the_session_s_and_a_fresh_one_goes_to_its_keeper() {
        let mut c = cfg();
        c.anchor_1e6 = 900_000_000;
        let (arm, mut v, t) = booted_with(&c);
        let sig = arm.signal(t);
        assert_eq!((sig.pnl_judged, sig.pnl_delta_usd_1e6), (1, 100_000_000));
        assert_eq!(v.keeper(), None, "restored: nothing to keep");
        assert!(arm.anchor_kept, "a restored anchor is durable");
        assert_eq!(sig.reconciled, 1);
        let (arm, mut v, t) = booted();
        assert_eq!(v.keeper(), Some(1_000_000_000));
        assert_eq!(arm.signal(t).pnl_delta_usd_1e6, 0);
    }

    /// XH5b-2: the arm's first complete worth on an own account, its keeper
    /// silent; the sheet, the spot half, the orders, the budget and the day
    /// all read.
    fn booted_unkept() -> (HlGatewayArm, Venue, u64) {
        let (mut arm, mut v, t, st) = to_sheet(&cfg());
        v.keeps = false;
        v.send(read_end(&st, 0, 1));
        let _ = arm.idle(t);
        let sp = v.take(CMD_READ_SPOT).expect("the spot sheet");
        v.send(read_end(&sp, 1_000 * E8, 0));
        let _ = arm.idle(t);
        assert_eq!(v.anchor.pending(), Some(1_000_000_000), "handed over");
        let od = v.take(CMD_READ_ORDERS).expect("the orders");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget");
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        let day = v.take(CMD_READ_DAY).expect("the day");
        v.send(day_end(&day, 0, 0, 0));
        let _ = arm.idle(t);
        (arm, v, t)
    }

    /// XH5b-2: a fresh anchor is handed to its keeper, and the slot reports
    /// nothing reconciled until the keeper has stored it — the router's
    /// first-reconciliation latch holds every quote until the anchor is
    /// durable; a store that fails halts the gateway (the keeper's word)
    /// and the slot stays unreconciled.
    #[test]
    fn the_first_reconciliation_waits_for_the_anchor_to_be_kept() {
        let (arm, mut v, t) = booted_unkept();
        assert!(arm.ready(), "the arm is booted");
        let sig = arm.signal(t);
        assert_eq!(
            (sig.reconciled, sig.pnl_judged),
            (0, 1),
            "judged, and nothing reconciled: no quote before the anchor is durable"
        );
        assert_eq!(v.keeper(), None, "its keeper is silent");
        v.gw.stats.halt(crate::gateway::HALT_ANCHOR_UNKEPT);
        let sig = arm.signal(t);
        assert_eq!((sig.reconciled, sig.gateway_halted), (0, 1));
    }

    /// XH5b-2: once its keeper has stored the anchor, the next pass reads
    /// it durable and the slot reconciles.
    #[test]
    fn a_kept_anchor_lets_the_slot_reconcile() {
        let (mut arm, mut v, t) = booted_unkept();
        v.keeps = true;
        assert_eq!(v.keeper(), Some(1_000_000_000));
        assert_eq!(arm.signal(t).reconciled, 0, "not before the arm reads it");
        let _ = arm.idle(t);
        assert!(arm.anchor_kept);
        assert_eq!(arm.signal(t).reconciled, 1);
    }

    /// XH5b-2: an arm with no keeper never reads a fresh anchor kept — its
    /// slot never reconciles on its own account (the safe direction).
    #[test]
    fn with_no_keeper_a_fresh_anchor_is_never_kept() {
        let (mut arm, mut v, t, st) = to_sheet(&cfg());
        arm.keep = None;
        v.send(read_end(&st, 0, 1));
        let _ = arm.idle(t);
        let sp = v.take(CMD_READ_SPOT).expect("the spot sheet");
        v.send(read_end(&sp, 1_000 * E8, 0));
        let _ = arm.idle(t);
        assert!(arm.anchored && !arm.anchor_kept);
        assert_eq!(v.keeper(), None, "nothing handed over");
        let od = v.take(CMD_READ_ORDERS).expect("the orders");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget");
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        let day = v.take(CMD_READ_DAY).expect("the day");
        v.send(day_end(&day, 0, 0, 0));
        let _ = arm.idle(t + S);
        assert_eq!(arm.signal(t + S).reconciled, 0);
    }

    #[test]
    fn the_alo_window_reports_permille_once_it_means_something() {
        let (mut arm, mut v, t) = booted();
        let mut k = 0u64;
        while k < 50 {
            arm.submit(&quote(100 + k, Side::Ask, 100_000_000, 500_000))
                .expect("submit");
            let c = v.take(CMD_PLACE).expect("the place");
            if k < 10 {
                v.send(item(&c, EV_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX, 0));
            } else {
                v.send(item(&c, EV_ACK, 0, EVF_RESTING));
            }
            let _ = arm.idle(t);
            if k == 48 {
                assert_eq!(arm.alo_reject_permille(), 0, "49 answers mean nothing yet");
            }
            k += 1;
        }
        assert_eq!(arm.alo_reject_permille(), 200);
        assert_eq!(arm.signal(t).alo_reject_permille, 200);
        assert_eq!(arm.signal(t).reject_streak, 0, "a cross is no venue fault");
    }

    #[test]
    fn cancel_all_follows_the_gateway_s_sweep_word() {
        let (mut arm, v, _t) = booted();
        arm.cancel_all().expect("cancel all");
        assert_eq!(arm.cancel_all_state(), CancelAllState::Working);
        let asked = v.gw.stats.sweep_requested();
        v.gw.stats.set_sweep(asked, SWEEP_WORKING);
        assert_eq!(arm.cancel_all_state(), CancelAllState::Working);
        v.gw.stats.set_sweep(asked, SWEEP_STRANDED);
        assert_eq!(arm.cancel_all_state(), CancelAllState::Stranded);
        arm.cancel_all().expect("asked again");
        assert_eq!(
            arm.cancel_all_state(),
            CancelAllState::Working,
            "a new request"
        );
        v.gw.stats
            .set_sweep(v.gw.stats.sweep_requested(), SWEEP_CLEAR);
        assert_eq!(arm.cancel_all_state(), CancelAllState::Clear);
    }

    #[test]
    fn cancel_all_is_clear_only_once_no_order_the_arm_holds_can_land() {
        let (mut arm, mut v, t) = booted();
        arm.submit(&quote(1, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let p = v.take(CMD_PLACE).expect("the place");
        arm.cancel_all().expect("cancel all");
        v.gw.stats
            .set_sweep(v.gw.stats.sweep_requested(), SWEEP_CLEAR);
        assert_eq!(
            arm.cancel_all_state(),
            CancelAllState::Working,
            "the venue read clear, but the place may still land"
        );
        let x = v.take(CMD_CANCEL).expect("its cancel");
        v.send(item(&x, EV_REJECTED, REASON_NOT_FOUND, 0));
        v.send(item(&p, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        let x = v.take(CMD_CANCEL).expect("cancelled once it lands");
        assert_eq!(arm.cancel_all_state(), CancelAllState::Working);
        v.send(item(&x, EV_ACK, 0, 0));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [
                (1, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE),
                (1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER)
            ]
        );
        assert_eq!(arm.cancel_all_state(), CancelAllState::Clear);
    }

    #[test]
    fn shutdown_sweeps_then_clears_the_switch() {
        let (mut arm, v) = armed(&quick());
        let stop = Arc::new(AtomicBool::new(false));
        let h = responder(v, |_| SWEEP_CLEAR, true, Arc::clone(&stop));
        let t0 = now_ns();
        arm.on_shutdown();
        stop.store(true, Ordering::Relaxed);
        let v = h.join().expect("the responder");
        assert!(now_ns() - t0 < 250_000_000, "confirmed before the bound");
        assert_eq!(arm.open_orders(), 0);
        assert_eq!(arm.counters().shutdown_unconfirmed, 0);
        let last = v
            .cmds
            .iter()
            .rev()
            .find(|c| c.kind == CMD_SCHEDULE_CANCEL)
            .expect("the clear");
        assert_eq!(last.time_ms, 0, "cleared last, after the sweep");
        assert!(arm.deadman().quiescent(now_ms()));
    }

    #[test]
    fn a_shutdown_the_venue_never_confirms_leaves_the_switch_armed() {
        let (mut arm, v) = armed(&quick());
        let stop = Arc::new(AtomicBool::new(false));
        let h = responder(v, |_| SWEEP_WORKING, true, Arc::clone(&stop));
        arm.on_shutdown();
        stop.store(true, Ordering::Relaxed);
        let v = h.join().expect("the responder");
        assert!(
            v.cmds
                .iter()
                .all(|c| c.kind != CMD_SCHEDULE_CANCEL || c.time_ms != 0),
            "never cleared"
        );
        assert_ne!(
            arm.deadman().deadline_ms(),
            0,
            "the venue still holds the deadline"
        );
        assert_eq!(
            (
                arm.counters().shutdown_unconfirmed,
                arm.counters().shutdown_switch_held
            ),
            (1, 0),
            "counted: the owner logs and alerts"
        );
    }

    #[test]
    fn a_confirmed_sweep_whose_switch_clear_goes_unanswered_is_counted_apart() {
        let (mut arm, v) = armed(&quick());
        let stop = Arc::new(AtomicBool::new(false));
        let h = responder(v, |_| SWEEP_CLEAR, false, Arc::clone(&stop));
        arm.on_shutdown();
        stop.store(true, Ordering::Relaxed);
        let _ = h.join().expect("the responder");
        assert_eq!(arm.open_orders(), 0, "nothing of ours rests");
        assert_eq!(
            (
                arm.counters().shutdown_unconfirmed,
                arm.counters().shutdown_switch_held
            ),
            (0, 1),
            "the switch may still fire on an empty book"
        );
    }

    #[test]
    fn a_stranded_shutdown_sweep_is_asked_again() {
        let (mut arm, v, _t) = booted_with(&quick());
        let stop = Arc::new(AtomicBool::new(false));
        let h = responder(
            v,
            |asked| {
                if asked <= 2 {
                    SWEEP_STRANDED
                } else {
                    SWEEP_CLEAR
                }
            },
            true,
            Arc::clone(&stop),
        );
        arm.on_shutdown();
        stop.store(true, Ordering::Relaxed);
        let v = h.join().expect("the responder");
        assert!(
            v.gw.stats.sweep_requested() >= 3,
            "asked again after Stranded"
        );
        assert_eq!(arm.cancel_all_state(), CancelAllState::Clear);
    }

    #[test]
    fn the_signal_carries_the_gap_the_gateway_halt_and_the_session_bound() {
        let (mut arm, mut v, t) = booted();
        let sig = arm.signal(t);
        assert_eq!(
            (sig.ws_gap_ns, sig.gateway_halted, sig.pnl_judged),
            (0, 0, 1)
        );
        assert_eq!(
            arm.signal(t + 3 * S).ws_gap_ns,
            3 * S,
            "a socket up but a heartbeat 3 s old: the gateway stopped"
        );
        v.gw.stats.beat(t + 3 * S);
        assert_eq!(arm.signal(t + 3 * S).ws_gap_ns, 0);
        v.gw.stats.set_gauges(0, false);
        assert_eq!(
            arm.signal(t + 3 * S).ws_gap_ns,
            3 * S,
            "since it was last alive"
        );
        v.gw.stats.halt(HALT_EVENT_RING_FULL);
        assert_eq!(arm.signal(t).gateway_halted, 1);
        // The account fell 10 USD from the anchor.
        let _ = arm.idle(t + 5 * S);
        let st = v.take(CMD_READ_STATE).expect("the sheet");
        v.send(read_end(&st, 990 * E8, 3));
        let _ = arm.idle(t + 5 * S);
        assert_eq!(arm.signal(t + 5 * S).pnl_delta_usd_1e6, -10_000_000);
    }

    #[test]
    fn a_shared_account_judges_no_session_bound_and_ignores_unbound_positions() {
        let mut c = cfg();
        c.own_account = false;
        let (mut arm, mut v, t, st) = to_sheet(&c);
        v.send(ExecEvent {
            asset: ASSET + 1,
            ..position(&st, E8, 10 * E8 as u64)
        });
        v.send(read_end(&st, 1_000 * E8, 1));
        let _ = arm.idle(t);
        assert_eq!(arm.unknown_positions, 0, "another slot's perp");
        assert_eq!(arm.signal(t).pnl_judged, 0);
        assert!(
            v.take(CMD_READ_SPOT).is_none(),
            "XH5b-1: a shared account's worth is every slot's — never read"
        );
        assert!(v.take(CMD_READ_ORDERS).is_some(), "the orders at once");
        assert_eq!(
            v.keeper(),
            None,
            "a shared account's value is every slot's: nothing to keep"
        );
    }

    #[test]
    fn a_read_that_fails_is_asked_again_after_a_skew() {
        let (mut arm, mut v, t) = booted();
        let _ = arm.idle(t + 5 * S);
        let st = v.take(CMD_READ_STATE).expect("the sheet");
        v.send(item(&st, EV_UNANSWERED, 0, 0));
        let _ = arm.idle(t + 5 * S);
        // The orders read goes meanwhile.
        let od = v.take(CMD_READ_ORDERS).expect("the orders");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 6 * S);
        assert!(v.take(CMD_READ_STATE).is_none(), "a skew first");
        let _ = arm.idle(t + 7 * S);
        let st = v.take(CMD_READ_STATE).expect("asked again");
        v.send(ExecEvent {
            kind: EV_READ_FAILED,
            ..read_end(&st, 0, 0)
        });
        let _ = arm.idle(t + 7 * S);
        assert_eq!(arm.counters().reads_failed, 2);
    }

    #[test]
    fn orders_the_venue_would_refuse_never_reach_the_ring() {
        let (mut arm, mut v, _t) = booted();
        let mut unbound = quote(1, Side::Bid, 100_000_000, 500_000);
        unbound.sym = SYM + 1;
        assert_eq!(arm.submit(&unbound), Err(DispatchError::Unroutable));
        assert_eq!(arm.signal(now_ns()).asset_refusal_streak, 1);
        let mut resting_kind = quote(2, Side::Bid, 100_000_000, 500_000);
        resting_kind.flags = 0;
        assert_eq!(
            arm.submit(&resting_kind),
            Err(DispatchError::Unroutable),
            "a maker needs ALO"
        );
        assert_eq!(
            arm.submit(&quote(3, Side::Bid, 100_000_000, 10_000)),
            Err(DispatchError::Unroutable),
            "1 USD is under the venue's minimum"
        );
        let mut other = quote(5, Side::Bid, 100_000_000, 500_000);
        other.strategy_id = 3;
        assert_eq!(
            arm.submit(&other),
            Err(DispatchError::Unroutable),
            "another slot's order"
        );
        arm.submit(&quote(4, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        assert_eq!(
            arm.submit(&quote(4, Side::Bid, 100_000_000, 500_000)),
            Err(DispatchError::Unroutable),
            "one order per client id"
        );
        assert_eq!(arm.counters().refused_local, 5);
        assert_eq!(v.take(CMD_PLACE).map(|c| c.client_oid), Some(4));
        assert!(v.take(CMD_PLACE).is_none());
    }

    #[test]
    fn the_stream_can_beat_the_answer_and_the_late_answer_is_no_stray() {
        let (mut arm, mut v, t) = booted();
        arm.submit(&quote(1, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let p = v.take(CMD_PLACE).expect("the place");
        v.send(row(EV_CANCELED, 1, ORDER_EVENT_REASON_OTHER));
        v.send(item(&p, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [
                (1, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE),
                (1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER)
            ]
        );
        assert_eq!(retired(&mut arm), [1]);
        assert_eq!(arm.counters().stray, 0);
    }

    #[test]
    fn a_cancel_of_an_order_thought_gone_keeps_its_gone_wait() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        arm.cancel(&cancel_req(1)).expect("cancel");
        let x = v.take(CMD_CANCEL).expect("the cancel");
        v.send(item(&x, EV_REJECTED, REASON_NOT_FOUND, 0));
        let _ = arm.idle(t + 1);
        let od = v.take(CMD_READ_ORDERS).expect("the read");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 2);
        // Thought gone, not listed; the cancel-all word cancels it again
        // and the venue finds nothing: the gone wait still ends it.
        arm.cancel_all().expect("cancel all");
        let x = v.take(CMD_CANCEL).expect("the cancel goes again");
        v.send(item(&x, EV_REJECTED, REASON_NOT_FOUND, 0));
        let _ = arm.idle(t + 3);
        assert!(events(&mut arm).is_empty(), "still in its gone wait");
        let _ = arm.idle(t + 4 * S);
        assert_eq!(
            events(&mut arm),
            [(1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER)]
        );
        assert_eq!(retired(&mut arm), [1]);
        assert_eq!((arm.counters().closed_unseen, arm.open_orders()), (1, 0));
    }

    #[test]
    fn a_slow_confirming_read_decides_orders_thought_gone() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        let _ = rested(&mut arm, &mut v, t, 2);
        // The periodic read lists neither: both thought gone, a read at once.
        recon(&mut arm, &mut v, t + 5 * S, None, &[]);
        let od = v
            .take(CMD_READ_ORDERS)
            .expect("the confirming read, at once");
        // Its answer is slow: the gone wait passes while it is in flight.
        let _ = arm.idle(t + 9 * S);
        assert!(
            events(&mut arm).is_empty(),
            "the read in flight decides first"
        );
        v.send(open_order(&od, SLOT, 1));
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 9 * S + 1);
        assert_eq!(
            events(&mut arm),
            [(2, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER)],
            "1 listed: it rests; 2 unlisted: it ended"
        );
        assert_eq!(
            (
                arm.open_orders(),
                arm.counters().closed_unseen,
                arm.counters().unknown_orders
            ),
            (1, 1, 0)
        );
    }

    #[test]
    fn an_unanswered_cancel_of_an_order_thought_gone_keeps_its_gone_wait() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        arm.cancel(&cancel_req(1)).expect("cancel");
        let x = v.take(CMD_CANCEL).expect("the cancel");
        v.send(item(&x, EV_REJECTED, REASON_NOT_FOUND, 0));
        let _ = arm.idle(t + 1);
        let od = v.take(CMD_READ_ORDERS).expect("the read");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 2);
        arm.cancel_all().expect("cancel all");
        let x = v.take(CMD_CANCEL).expect("the cancel goes again");
        v.send(item(&x, EV_UNANSWERED, 0, 0));
        let _ = arm.idle(t + 3);
        // The skew (2 s) would have ended it here; its gone wait (3 s from
        // the "not found") has not.
        let _ = arm.idle(t + 5 * S / 2);
        assert!(events(&mut arm).is_empty(), "the gone wait, not the skew");
        let _ = arm.idle(t + 4 * S);
        assert_eq!(
            events(&mut arm),
            [(1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_OTHER)]
        );
    }

    #[test]
    fn a_gap_during_an_orders_read_asks_another_and_only_that_read_restores_exclusivity() {
        let (mut arm, mut v, t) = booted();
        let _ = arm.idle(t + 5 * S);
        let st = v.take(CMD_READ_STATE).expect("the sheet");
        v.send(read_end(&st, 1_000 * E8, 2));
        let _ = arm.idle(t + 5 * S);
        spot(&mut arm, &mut v, t + 5 * S, 0);
        let od = v.take(CMD_READ_ORDERS).expect("the orders");
        v.send(ExecEvent {
            kind: EV_GAP,
            ..ExecEvent::default()
        });
        let _ = arm.idle(t + 5 * S + 1);
        assert!(!arm.exclusive);
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 5 * S + 2);
        assert!(
            !arm.exclusive,
            "a read issued before the gap cannot restore it"
        );
        let od = v
            .take(CMD_READ_ORDERS)
            .expect("another read at once, not at the cadence");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 5 * S + 3);
        assert!(arm.exclusive, "a read issued after the gap does");
    }

    #[test]
    fn a_cancel_all_not_read_clear_keeps_the_switch_wanted_past_the_hold() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 1);
        arm.cancel_all().expect("cancel all");
        let x = v.take(CMD_CANCEL).expect("the cancel");
        v.send(item(&x, EV_ACK, 0, 0));
        let _ = arm.idle(t + 1);
        assert_eq!(arm.open_orders(), 0);
        assert!(
            arm.deadman_wanted(t + 31 * S),
            "the sweep still works: armed"
        );
        v.gw.stats
            .set_sweep(v.gw.stats.sweep_requested(), SWEEP_STRANDED);
        assert!(arm.deadman_wanted(t + 62 * S), "stranded: armed");
        v.gw.stats
            .set_sweep(v.gw.stats.sweep_requested(), SWEEP_CLEAR);
        assert!(
            arm.deadman_wanted(t + 63 * S),
            "read clear: the hold runs out from here"
        );
        assert!(!arm.deadman_wanted(t + 93 * S), "then it is cleared");
    }

    #[test]
    fn a_storm_of_re_reads_never_starves_the_sheet() {
        let (mut arm, mut v, t) = booted();
        // A gap lands during every orders read: a re-read is always
        // pending once the first one went.
        let mut sheets = 0u32;
        let mut at = t + 1;
        while at < t + 16 * S {
            let _ = arm.idle(at);
            if let Some(st) = v.take(CMD_READ_STATE) {
                sheets += 1;
                v.send(read_end(&st, 1_000 * E8, 2));
            }
            if let Some(sp) = v.take(CMD_READ_SPOT) {
                v.send(read_end(&sp, 0, 0));
            }
            if let Some(od) = v.take(CMD_READ_ORDERS) {
                v.send(ExecEvent {
                    kind: EV_GAP,
                    ..ExecEvent::default()
                });
                v.send(read_end(&od, 0, 0));
            }
            at += S / 2;
        }
        assert_eq!(
            sheets, 2,
            "the periodic sheet, then one a whole period overdue"
        );
    }

    #[test]
    fn an_agreement_needs_a_fresh_sheet() {
        let (mut arm, mut v, t) = booted();
        // Every sheet fails from here; the orders reads go on.
        let mut at = t + 5 * S;
        while at < t + 30 * S {
            let _ = arm.idle(at);
            if let Some(st) = v.take(CMD_READ_STATE) {
                v.send(ExecEvent {
                    kind: EV_READ_FAILED,
                    ..read_end(&st, 0, 0)
                });
            }
            if let Some(od) = v.take(CMD_READ_ORDERS) {
                v.send(read_end(&od, 0, 0));
            }
            at += S;
        }
        assert!(
            arm.signal(at).recon_age_ns >= 15 * S,
            "the age runs from the last agreement with a fresh sheet"
        );
    }

    #[test]
    fn an_agreement_needs_fresh_orders_reads() {
        let (mut arm, mut v, t) = booted();
        // Every orders read fails from here; the sheets go on.
        let mut at = t + 5 * S;
        while at < t + 30 * S {
            let _ = arm.idle(at);
            if let Some(st) = v.take(CMD_READ_STATE) {
                v.send(read_end(&st, 1_000 * E8, 2));
            }
            if let Some(sp) = v.take(CMD_READ_SPOT) {
                v.send(read_end(&sp, 0, 0));
            }
            if let Some(od) = v.take(CMD_READ_ORDERS) {
                v.send(ExecEvent {
                    kind: EV_READ_FAILED,
                    ..read_end(&od, 0, 0)
                });
            }
            at += S;
        }
        assert!(
            arm.signal(at).recon_age_ns >= 15 * S,
            "the age runs from the last agreement with a fresh orders read"
        );
    }

    #[test]
    fn a_bad_configuration_is_refused() {
        let mut c = cfg();
        c.slot = 8;
        assert_eq!(
            HlGatewayArm::new(&c, hl_duplex().0).err(),
            Some(ArmErr::Slot)
        );
        let mut c = cfg();
        c.n_perps = 2;
        assert_eq!(
            HlGatewayArm::new(&c, hl_duplex().0).err(),
            Some(ArmErr::Perps),
            "bound twice"
        );
        let mut c = cfg();
        c.expires_after_ms = 0;
        assert_eq!(
            HlGatewayArm::new(&c, hl_duplex().0).err(),
            Some(ArmErr::Timing)
        );
        let mut c = cfg();
        c.skew_ms = 5_001;
        assert_eq!(
            HlGatewayArm::new(&c, hl_duplex().0).err(),
            Some(ArmErr::Timing)
        );
        let mut c = cfg();
        c.post_timeout_ms = 4_000;
        assert_eq!(
            HlGatewayArm::new(&c, hl_duplex().0).err(),
            Some(ArmErr::Timing),
            "the dead man's answer is the post timeout"
        );
        let mut c = cfg();
        c.shutdown_ms = 30_001;
        assert_eq!(
            HlGatewayArm::new(&c, hl_duplex().0).err(),
            Some(ArmErr::Timing),
            "the stop never waits on the venue longer than 30 s"
        );
        let mut c = cfg();
        c.gone_ms = 60_001;
        assert_eq!(
            HlGatewayArm::new(&c, hl_duplex().0).err(),
            Some(ArmErr::Timing)
        );
        let mut c = cfg();
        c.limit_ms = 600_001;
        assert_eq!(
            HlGatewayArm::new(&c, hl_duplex().0).err(),
            Some(ArmErr::Timing),
            "the budget is read at least every ten minutes"
        );
        let mut c = cfg();
        c.budget_floor = MIN_BUDGET_FLOOR - 1;
        assert_eq!(
            HlGatewayArm::new(&c, hl_duplex().0).err(),
            Some(ArmErr::Budget),
            "the floor keeps a cleanup's headroom"
        );
    }

    // --- XH4e -------------------------------------------------------

    /// The budget is read after the boot's orders and every `limit_ms`;
    /// between reads each item the arm pushes is one request (a place, a
    /// modify, a cancel, a control post), a sweep's cancels are charged by
    /// the gateway's word, one refused unsent is taken back, and no fill is
    /// credited (the next read states the volume); the requests unanswered
    /// when a read goes and those pushed while it is in flight are added on
    /// top of its answer.
    #[test]
    fn the_budget_is_read_charged_per_item_and_read_again() {
        let (mut arm, mut v, t) = booted();
        assert_eq!(arm.budget().expect("read").remaining(), 9_900);
        v.gw.stats.note_sweep_cancels(2);
        let _ = arm.idle(t);
        assert_eq!(arm.budget().unwrap().remaining(), 9_898, "the sweep's two");
        // $1 000 traded: nothing credited until a read states it.
        v.send(ExecEvent {
            slot: 3,
            ..fill(99, true, 1_000 * E8, E8, 3)
        });
        let _ = arm.pump(t);
        assert_eq!(arm.budget().unwrap().remaining(), 9_898, "no fill credit");
        arm.submit(&quote(1, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        arm.cancel(&cancel_req(1)).expect("cancel");
        assert_eq!(
            arm.budget().unwrap().remaining(),
            9_896,
            "a place and a cancel"
        );
        // A place the gateway refused unsent is taken back.
        arm.submit(&quote(2, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let _ = v.take(CMD_PLACE);
        let p = v.take(CMD_PLACE).expect("the second place");
        assert_eq!(arm.budget().unwrap().remaining(), 9_895);
        v.send(item(&p, EV_REJECTED, REASON_THROTTLED, 0));
        let _ = arm.pump(t);
        assert_eq!(
            (arm.budget().unwrap().remaining(), arm.counters().refunded),
            (9_896, 1),
            "it never left"
        );
        // Ten minutes on, after the periodic reads: the budget again. A
        // place unanswered when it goes, and one pushed while it is in
        // flight, count on top of its answer.
        let (mut arm, mut v, t) = booted();
        let later = t + 600 * S;
        arm.submit(&quote(2, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        recon(&mut arm, &mut v, later, None, &[]);
        let lim = v.take(CMD_READ_LIMIT).expect("read again");
        arm.submit(&quote(3, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let carried = arm.limit_sent;
        assert!(carried >= 2, "the place unanswered and the one in flight");
        v.send(limit_end(&lim, 500));
        let _ = arm.pump(later);
        assert_eq!(
            arm.budget().unwrap().remaining(),
            10_000 - 500 - carried as i64
        );
        assert_eq!(arm.counters().limit_reads, 2);
    }

    /// At or below the floor: places and modifies are refused (the blocking
    /// arm's `SlotDisabled`), the signal breaches the floor (the router's
    /// `BudgetFloor`), cancels still go — and the dead man stays armed while
    /// an order is held or the halt's cancel-all is not read clear, then
    /// parks at once.
    #[test]
    fn at_the_budget_floor_growth_stops_and_the_dead_man_parks_once_flat_and_clear() {
        let (mut arm, mut v, t, lim) = to_budget(&cfg());
        assert!(!arm.budget_floor_breached(), "nothing known is no breach");
        assert_eq!(arm.signal(t).budget_floor_breached, 0);
        v.send(limit_end(&lim, 7_000));
        let _ = arm.idle(t);
        let day = v.take(CMD_READ_DAY).expect("the day");
        v.send(day_end(&day, 0, 0, 0));
        let _ = arm.idle(t);
        let _ = rested(&mut arm, &mut v, t, 1);
        assert!(arm.deadman_wanted(t), "an order rests");
        // 999 more requests: 10 000 − 7 000 − 1 − 999 = 2 000, the floor.
        arm.charge(999);
        assert!(arm.budget_floor_breached());
        assert_eq!(arm.signal(t).budget_floor_breached, 1);
        assert_eq!(
            arm.submit(&quote(2, Side::Bid, 100_000_000, 500_000)),
            Err(DispatchError::SlotDisabled)
        );
        assert_eq!(
            arm.modify(&ModifyReq::new(1, quote(3, Side::Bid, 99_000_000, 500_000))),
            Err(DispatchError::SlotDisabled)
        );
        assert_eq!(arm.counters().refused_budget, 2);
        assert!(arm.deadman_wanted(t), "an order still rests: armed");
        // The router's halt: cancel-all — working until the venue reads
        // clear and the order's cancel is answered.
        arm.cancel_all().expect("cancel-all");
        let x = v.take(CMD_CANCEL).expect("a cancel always goes");
        assert!(arm.deadman_wanted(t), "the sweep is working");
        v.gw.stats
            .set_sweep(v.gw.stats.sweep_requested(), SWEEP_CLEAR);
        assert!(arm.deadman_wanted(t), "read clear, but ours is still held");
        v.send(item(&x, EV_ACK, 0, 0));
        let _ = arm.idle(t);
        assert_eq!(
            (arm.open_orders(), arm.cancel_all_state()),
            (0, CancelAllState::Clear)
        );
        assert!(
            !arm.deadman_wanted(t),
            "flat and read clear: parked at once, inside the hold"
        );
    }

    /// XH4e: at the floor with nothing held, a cancel-all the venue has not
    /// read clear — working or stranded — keeps the switch armed; only a
    /// clear read parks it.
    #[test]
    fn at_the_floor_a_cancel_all_not_read_clear_keeps_the_switch_armed() {
        let (mut arm, v, t) = booted();
        arm.charge(8_000);
        assert!(arm.budget_floor_breached());
        assert!(!arm.deadman_wanted(t), "flat and clear: parked");
        arm.cancel_all().expect("cancel-all");
        assert!(arm.deadman_wanted(t), "the sweep is working");
        v.gw.stats
            .set_sweep(v.gw.stats.sweep_requested(), SWEEP_STRANDED);
        assert!(arm.deadman_wanted(t), "stranded: ours may rest");
        v.gw.stats
            .set_sweep(v.gw.stats.sweep_requested(), SWEEP_CLEAR);
        assert!(!arm.deadman_wanted(t), "read clear: parked");
    }

    /// XH4e: a shutdown at the budget floor keeps the switch armed while
    /// the venue never reads clear — the floor never clears it early.
    #[test]
    fn a_shutdown_at_the_floor_the_venue_never_confirms_leaves_the_switch_armed() {
        let (mut arm, v) = armed(&quick());
        arm.charge(8_000);
        assert!(arm.budget_floor_breached());
        let stop = Arc::new(AtomicBool::new(false));
        let h = responder(v, |_| SWEEP_WORKING, true, Arc::clone(&stop));
        arm.on_shutdown();
        stop.store(true, Ordering::Relaxed);
        let v = h.join().expect("the responder");
        assert!(
            v.cmds
                .iter()
                .all(|c| c.kind != CMD_SCHEDULE_CANCEL || c.time_ms != 0),
            "never cleared"
        );
        assert_ne!(arm.deadman().deadline_ms(), 0, "the venue holds a deadline");
        assert_eq!(arm.counters().shutdown_unconfirmed, 1);
    }

    /// The day's spend is read after the budget, page by page — a full
    /// page followed from its last row's time — and only then does the
    /// arm report `reconciled` and the router get the day: the slot's
    /// fills since 00:00Z, ×1e6.
    #[test]
    fn the_day_is_read_page_by_page_before_the_arm_reconciles() {
        let (mut arm, mut v, t, lim) = to_budget(&cfg());
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        assert!(arm.ready());
        let p1 = v.take(CMD_READ_DAY).expect("the first page");
        let midnight = now_ms() / DAY_MS * DAY_MS;
        assert_eq!(p1.time_ms, midnight, "from 00:00Z");
        assert!(
            p1.prev_client_oid >= midnight && p1.prev_client_oid <= now_ms(),
            "to the time the read began"
        );
        assert_eq!(arm.signal(t).reconciled, 0, "not before the day is read");
        assert_eq!(arm.venue_day_bought(usize::from(SLOT)), None);
        v.send(day_end(&p1, DAY_PAGE, 300 * E8, midnight + 5_000));
        let _ = arm.idle(t);
        assert!(
            v.take(CMD_READ_DAY).is_none(),
            "the next page a read period on"
        );
        v.gw.stats.set_congestion(CONGESTION_THROTTLED);
        recon(&mut arm, &mut v, t + 5 * S, None, &[]);
        assert!(
            v.take(CMD_READ_DAY).is_none(),
            "never while the rates read throttled"
        );
        v.gw.stats.set_congestion(CONGESTION_NONE);
        let _ = arm.idle(t + 5 * S);
        let p2 = v.take(CMD_READ_DAY).expect("a full page asks for the next");
        assert_eq!(p2.time_ms, midnight + 5_000, "from its latest row");
        assert_eq!(
            p2.prev_client_oid, p1.prev_client_oid,
            "every page ends where the first did"
        );
        // A short page is not the end: it moved past its start.
        v.send(day_end(&p2, 3, 20 * E8, midnight + 6_000));
        let _ = arm.idle(t + 5 * S);
        assert_eq!(arm.signal(t).reconciled, 0, "not yet");
        recon(&mut arm, &mut v, t + 10 * S, None, &[]);
        let p3 = v.take(CMD_READ_DAY).expect("until a page adds nothing");
        assert_eq!(p3.time_ms, midnight + 6_000);
        // The boundary row again (counted twice: the conservative way).
        v.send(day_end(&p3, 1, 5 * E8, midnight + 6_000));
        let _ = arm.idle(t + 10 * S);
        assert_eq!(arm.signal(t).reconciled, 1);
        assert_eq!(
            arm.venue_day_bought(usize::from(SLOT)),
            Some((midnight / DAY_MS, 325_000_000))
        );
        assert_eq!(
            arm.venue_day_bought(3),
            None,
            "another slot's day is its own"
        );
        assert_eq!(
            (arm.counters().day_pages, arm.counters().day_unreadable),
            (3, 0)
        );
        assert!(v.take(CMD_READ_DAY).is_none(), "read once");
    }

    /// A venue fill is stamped with the venue's time (wall), as the
    /// blocking arm stamps one: the router's day epoch reads it as wall —
    /// and (XMM XH5d) carries the venue's trade id.
    #[test]
    fn a_fill_is_stamped_with_the_venue_s_time() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 5);
        v.send(ExecEvent {
            aux: 4_242,
            ..fill(5, true, 100 * E8, E8 / 2, 1_790_000_000_123)
        });
        let _ = arm.idle(t);
        let f = arm.try_next_fill().expect("the fill");
        assert_eq!((f.ts_ns, f.tid), (1_790_000_000_123_000_000, 4_242));
    }

    /// XMM XH5d: a post's answer carries no venue time, so its RESTING
    /// goes without one and the `open` row's time follows once, in a
    /// repeated RESTING; an `open` row that lands the order carries it in
    /// the first, and the answer after it repeats nothing.
    #[test]
    fn a_resting_order_s_venue_time_rides_the_open_row() {
        let (mut arm, mut v, t) = booted();
        let _ = rested(&mut arm, &mut v, t, 8);
        v.send(ExecEvent {
            venue_ms: 1_791_000_000_123,
            ..row(EV_RESTING, 8, 0)
        });
        let _ = arm.idle(t);
        let mut e = OrderEvent::ZERO;
        assert!(arm.try_next_order_event(&mut e));
        assert_eq!(
            (e.client_oid, e.kind, e.venue_time_ms),
            (8, ORDER_EVENT_RESTING, 1_791_000_000_123)
        );
        v.send(ExecEvent {
            venue_ms: 1_791_000_000_999,
            ..row(EV_RESTING, 8, 0)
        });
        let _ = arm.idle(t);
        assert!(events(&mut arm).is_empty(), "once");
        arm.submit(&quote(9, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let c = v.take(CMD_PLACE).expect("the place");
        v.send(ExecEvent {
            venue_ms: 1_791_000_001_000,
            ..row(EV_RESTING, 9, 0)
        });
        let _ = arm.idle(t);
        assert!(arm.try_next_order_event(&mut e));
        assert_eq!(
            (e.client_oid, e.kind, e.venue_time_ms),
            (9, ORDER_EVENT_RESTING, 1_791_000_001_000)
        );
        v.send(item(&c, EV_ACK, 0, EVF_RESTING));
        let _ = arm.idle(t);
        assert!(
            events(&mut arm).is_empty(),
            "the answer after it repeats nothing"
        );
    }

    /// A place the gateway refused over its rate shares never left: no
    /// venue reject, no streak — and it is counted.
    #[test]
    fn a_throttled_place_is_no_reject() {
        let (mut arm, mut v, t) = booted();
        arm.submit(&quote(9, Side::Bid, 100_000_000, 500_000))
            .expect("submit");
        let c = v.take(CMD_PLACE).expect("the place");
        v.send(item(&c, EV_REJECTED, REASON_THROTTLED, 0));
        let _ = arm.idle(t);
        assert_eq!(
            events(&mut arm),
            [(9, ORDER_EVENT_REJECTED, REASON_THROTTLED)]
        );
        assert_eq!(
            (
                arm.counters().throttled,
                arm.reject_streak,
                arm.counters().refunded
            ),
            (1, 0, 1)
        );
    }

    /// XH4e: a budget no read has answered for three periods refuses growth
    /// — no halt, no floor — until a read answers.
    #[test]
    fn a_budget_unread_for_three_periods_refuses_growth_until_a_read_answers() {
        let mut c = cfg();
        c.limit_ms = 10_000;
        let (mut arm, mut v, t) = booted_with(&c);
        assert!(
            !arm.budget_stale(t + 30 * S),
            "three periods is not past them"
        );
        assert!(arm.budget_stale(t + 30 * S + 1));
        // Three periods past the last answer (every read failed since).
        arm.limit_stale_ns = now_ns().saturating_sub(S);
        assert_eq!(
            arm.submit(&quote(1, Side::Bid, 100_000_000, 500_000)),
            Err(DispatchError::SlotDisabled)
        );
        assert_eq!(arm.counters().refused_budget, 1);
        assert_eq!(arm.signal(t).budget_floor_breached, 0, "stale is no floor");
        // The next read answers: growth again.
        recon(&mut arm, &mut v, t + 10 * S, None, &[]);
        let lim = v.take(CMD_READ_LIMIT).expect("the periodic budget read");
        v.send(limit_end(&lim, 200));
        let _ = arm.idle(t + 10 * S);
        arm.submit(&quote(2, Side::Bid, 100_000_000, 500_000))
            .expect("growth once a read answers");
    }

    /// XH4e: a failed budget or day read is asked again a read period
    /// later, not after a skew — each is 20 IP weight or more.
    #[test]
    fn a_failed_budget_or_day_read_is_asked_again_a_read_period_later() {
        let (mut arm, mut v, t, lim) = to_budget(&cfg());
        v.send(ExecEvent {
            kind: EV_READ_FAILED,
            ..limit_end(&lim, 0)
        });
        let _ = arm.idle(t);
        let _ = arm.idle(t + 4 * S);
        assert!(v.take(CMD_READ_LIMIT).is_none(), "not after a skew");
        recon(&mut arm, &mut v, t + 5 * S, None, &[]);
        let lim = v.take(CMD_READ_LIMIT).expect("a read period later");
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t + 5 * S);
        let day = v.take(CMD_READ_DAY).expect("the day after the budget");
        v.send(ExecEvent {
            kind: EV_READ_FAILED,
            ..day_end(&day, 0, 0, 0)
        });
        let _ = arm.idle(t + 5 * S);
        let _ = arm.idle(t + 9 * S);
        assert!(v.take(CMD_READ_DAY).is_none(), "not after a skew");
        recon(&mut arm, &mut v, t + 10 * S, None, &[]);
        assert!(v.take(CMD_READ_DAY).is_some(), "a read period later");
        assert_eq!(arm.counters().reads_failed, 2);
    }

    /// XH4e: the venue serves an account's 10 000 most recent fills — a
    /// read that reaches them may have lost the day's start: the day reads
    /// spent (fail closed), and the arm reconciles on it. So does a full
    /// page that cannot move past its start.
    #[test]
    fn a_day_past_the_venue_s_ten_thousand_fills_reads_spent() {
        let (mut arm, mut v, t, lim) = to_budget(&cfg());
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        let mut at = t;
        let mut k = 0u32;
        while k < DAY_ROWS_MAX / DAY_PAGE as u32 {
            if k > 0 {
                at += 5 * S;
                recon(&mut arm, &mut v, at, None, &[]);
            }
            let p = v.take(CMD_READ_DAY).expect("a page");
            v.send(day_end(&p, DAY_PAGE, E8, p.time_ms + 1_000));
            let _ = arm.idle(at);
            k += 1;
        }
        assert_eq!(
            (arm.counters().day_pages, arm.counters().day_unreadable),
            (5, 1)
        );
        assert_eq!(arm.signal(at).reconciled, 1);
        assert_eq!(
            arm.venue_day_bought(usize::from(SLOT)).map(|d| d.1),
            Some(DAY_SPENT_1E8 / WIRE_PER_ENGINE)
        );
        assert!(v.take(CMD_READ_DAY).is_none(), "read no further");
        // A page whose rows ran backward in time: its next start could skip
        // rows — spent.
        let (mut arm, mut v, t, lim) = to_budget(&cfg());
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        let p = v.take(CMD_READ_DAY).expect("the first page");
        v.send(ExecEvent {
            sz_1e8: 1,
            ..day_end(&p, 40, E8, p.time_ms + 1_000)
        });
        let _ = arm.idle(t);
        assert_eq!(arm.counters().day_unreadable, 1);
        assert_eq!(
            arm.venue_day_bought(usize::from(SLOT)).map(|d| d.1),
            Some(DAY_SPENT_1E8 / WIRE_PER_ENGINE)
        );
        // A full page all at its start cannot move: spent.
        let (mut arm, mut v, t, lim) = to_budget(&cfg());
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        let p = v.take(CMD_READ_DAY).expect("the first page");
        v.send(day_end(&p, DAY_PAGE, E8, p.time_ms));
        let _ = arm.idle(t);
        assert_eq!(arm.counters().day_unreadable, 1);
        assert_eq!(
            arm.venue_day_bought(usize::from(SLOT)).map(|d| d.1),
            Some(DAY_SPENT_1E8 / WIRE_PER_ENGINE)
        );
    }

    /// XH4e (risk r3 N1): the venue keeps an account's 10 000 most recent
    /// fills, counted to the moment it answers — past the read's end. Every
    /// fill the stream brings after the end, any slot's, counts with the
    /// rows read, judged at the page that adds nothing too: rows and later
    /// fills at 10 000 read spent, one fewer reads done. A fill at or
    /// before the end is the pages' own; one with no time counts.
    #[test]
    fn the_account_s_fills_after_the_read_s_end_count_toward_the_venue_s_ten_thousand() {
        let mut later = 1u64;
        while later <= 2 {
            let (mut arm, mut v, t, lim) = to_budget(&cfg());
            v.send(limit_end(&lim, 100));
            let _ = arm.idle(t);
            let mut p = v.take(CMD_READ_DAY).expect("the first page");
            let end = p.prev_client_oid;
            let mut k = 0u64;
            while k < 2 + later {
                // Another slot's fills: before the end, at it, then past
                // it — the first with no time (counted: fail closed).
                let at_ms = match k {
                    0 => end - 1,
                    1 => end,
                    2 => 0,
                    _ => end + 1,
                };
                v.send(ExecEvent {
                    slot: 3,
                    ..fill(70 + k, k % 2 == 0, E8, E8, at_ms)
                });
                k += 1;
            }
            // 9 997 rows in five pages, then the boundary row again.
            let mut at = t;
            k = 0;
            while k < 6 {
                if k > 0 {
                    at += 5 * S;
                    recon(&mut arm, &mut v, at, None, &[]);
                    p = v.take(CMD_READ_DAY).expect("a page");
                }
                let (rows, last) = match k {
                    0..=3 => (DAY_PAGE, p.time_ms + 1_000),
                    4 => (DAY_PAGE - 3, p.time_ms + 1_000),
                    _ => (1, p.time_ms),
                };
                v.send(day_end(&p, rows, E8, last));
                let _ = arm.idle(at);
                k += 1;
            }
            let spent = later == 2;
            let want = if spent { DAY_SPENT_1E8 } else { 6 * E8 };
            assert_eq!(arm.signal(at).reconciled, 1);
            assert_eq!(
                (arm.counters().day_pages, arm.counters().day_unreadable),
                (6, u64::from(spent)),
                "{later} fill(s) past the end"
            );
            assert_eq!(
                arm.venue_day_bought(usize::from(SLOT)).map(|d| d.1),
                Some(want / WIRE_PER_ENGINE)
            );
            later += 1;
        }
    }

    /// XH4e (risk r3 N1, r4): a first page asked again takes a new end —
    /// the fills since the failed ask are rows of it, the count past the
    /// old end goes, and a gap before it loses nothing the read counts —
    /// while a stream gap once the read holds its end (a page answered, or
    /// the first in flight) reads the day spent: fills past the end may be
    /// lost. An `orderUpdates` frame that did not scan carries no fill. A
    /// page answered after the spend adds nothing.
    #[test]
    fn a_first_page_asked_again_takes_a_new_end_and_a_gap_spends_the_day() {
        let (mut arm, mut v, t, lim) = to_budget(&cfg());
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        let p = v.take(CMD_READ_DAY).expect("the first page");
        let first_end = p.prev_client_oid;
        v.send(ExecEvent {
            slot: 3,
            ..fill(70, true, E8, E8, first_end + 1)
        });
        v.send(ExecEvent {
            kind: EV_READ_FAILED,
            ..day_end(&p, 0, 0, 0)
        });
        let _ = arm.idle(t);
        assert_eq!(arm.day_rows, 1, "a fill past the end counts");
        // A gap before the first page is asked again: not spent.
        v.send(ExecEvent {
            kind: EV_GAP,
            aux: GAP_RECONNECT,
            ..ExecEvent::default()
        });
        let _ = arm.idle(t);
        let od = v.take(CMD_READ_ORDERS).expect("the gap's re-read");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        assert_eq!(arm.counters().day_unreadable, 0, "nothing counted lost");
        std::thread::sleep(std::time::Duration::from_millis(2));
        recon(&mut arm, &mut v, t + 5 * S, None, &[]);
        let p = v
            .take(CMD_READ_DAY)
            .expect("asked again a read period later");
        assert!(p.prev_client_oid > first_end, "a new end");
        assert_eq!(
            p.time_ms,
            p.prev_client_oid / DAY_MS * DAY_MS,
            "from 00:00Z"
        );
        assert_eq!(arm.day_rows, 0, "that fill is a row of it now");
        v.send(day_end(&p, 3, 20 * E8, p.time_ms + 1_000));
        let _ = arm.idle(t + 5 * S);
        // An `orderUpdates` frame that did not scan: no fill lost.
        v.send(ExecEvent {
            kind: EV_GAP,
            aux: GAP_UPDATES_UNREADABLE,
            ..ExecEvent::default()
        });
        let _ = arm.idle(t + 5 * S);
        let od = v.take(CMD_READ_ORDERS).expect("the gap's re-read");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t + 5 * S);
        assert_eq!(arm.counters().day_unreadable, 0, "no fill lost");
        recon(&mut arm, &mut v, t + 10 * S, None, &[]);
        let p = v.take(CMD_READ_DAY).expect("the next page");
        v.send(ExecEvent {
            kind: EV_GAP,
            aux: GAP_RECONNECT,
            ..ExecEvent::default()
        });
        let _ = arm.idle(t + 10 * S);
        assert_eq!(
            arm.venue_day_bought(usize::from(SLOT)).map(|d| d.1),
            Some(DAY_SPENT_1E8 / WIRE_PER_ENGINE),
            "a gap while the read holds its end: spent"
        );
        // The page in flight across the gap answers — one that would read
        // spent itself (full, all at its start): it adds nothing.
        v.send(day_end(&p, DAY_PAGE, 5 * E8, p.time_ms));
        let _ = arm.idle(t + 10 * S);
        assert_eq!(
            (
                arm.counters().day_pages,
                arm.counters().day_unreadable,
                arm.counters().reads_failed
            ),
            (2, 1, 1)
        );
    }

    /// XH4e (r5): the read holds its end from the first page's ask to its
    /// own end — the first page in flight, between pages, a later page in
    /// flight — and a gap that may have lost fills spends the day at each:
    /// a `userFills` frame that did not scan, a reconnect, one of no stated
    /// kind. The page in flight then answers done-shaped: nothing changes.
    #[test]
    fn a_gap_spends_the_day_wherever_the_read_holds_its_end() {
        let mut case = 0usize;
        while case < 3 {
            let (mut arm, mut v, t, lim) = to_budget(&cfg());
            v.send(limit_end(&lim, 100));
            let _ = arm.idle(t);
            let mut p = v.take(CMD_READ_DAY).expect("the first page");
            let mut at = t;
            if case > 0 {
                v.send(day_end(&p, 3, 20 * E8, p.time_ms + 1_000));
                let _ = arm.idle(t);
            }
            if case == 2 {
                at += 5 * S;
                recon(&mut arm, &mut v, at, None, &[]);
                p = v.take(CMD_READ_DAY).expect("the next page");
            }
            v.send(ExecEvent {
                kind: EV_GAP,
                aux: [GAP_FILLS_UNREADABLE, GAP_RECONNECT, 0][case],
                ..ExecEvent::default()
            });
            let _ = arm.idle(at);
            if case != 1 {
                v.send(day_end(&p, 0, 0, 0));
                let _ = arm.idle(at);
            }
            assert_eq!(arm.counters().day_unreadable, 1, "case {case}");
            assert_eq!(
                arm.venue_day_bought(usize::from(SLOT)).map(|d| d.1),
                Some(DAY_SPENT_1E8 / WIRE_PER_ENGINE),
                "case {case}"
            );
            case += 1;
        }
    }

    /// XH4g: the reconciliation's view — nothing before the first sheet;
    /// then each read's answer time and verdict; a disagreement that
    /// survives two sheets is the drift high water.
    #[test]
    fn recon_view_follows_each_read_and_the_drift() {
        let c = cfg();
        let (mut arm, mut v, t, st) = to_sheet(&c);
        assert_eq!(arm.recon_view(), ReconView::default(), "nothing read yet");
        v.send(read_end(&st, 1_000 * E8, 1));
        let _ = arm.idle(t);
        let r = arm.recon_view();
        assert!(r.state_ok && r.sheet_ns == t, "{r:?}");
        assert!(
            !r.orders_ok && r.orders_ns == 0,
            "the orders not read yet: {r:?}"
        );
        spot(&mut arm, &mut v, t, 0);
        let od = v.take(CMD_READ_ORDERS).expect("the orders after the sheet");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        let r = arm.recon_view();
        assert!(
            r.orders_ok && r.orders_ns == t && r.drift_hw_1e6 == 0,
            "{r:?}"
        );

        // A position the book never booked: one sheet disagrees, a second
        // makes it the drift.
        let (mut arm, mut v, t) = booted();
        recon(
            &mut arm,
            &mut v,
            t + 5 * S,
            Some((E8, 100 * E8 as u64)),
            &[],
        );
        let r = arm.recon_view();
        assert!(!r.state_ok && r.drift_hw_1e6 == 0, "one sheet: {r:?}");
        recon(
            &mut arm,
            &mut v,
            t + 10 * S,
            Some((E8, 100 * E8 as u64)),
            &[],
        );
        let r = arm.recon_view();
        assert!(!r.state_ok && r.drift_hw_1e6 > 0, "two sheets: {r:?}");
    }

    /// XH5b-1: on the slot's own account the worth is the perp sheet's
    /// value plus the spot sheet's USDC. The spot read goes right after
    /// the sheet, before the orders; the anchor waits for it, and so does
    /// the reconciliation. A later period's P&L moves with both halves.
    #[test]
    fn the_worth_adds_the_spot_usdc_and_the_anchor_waits_for_it() {
        let (mut arm, mut v, t, st) = to_sheet(&cfg());
        v.send(read_end(&st, 0, 1));
        let _ = arm.idle(t);
        assert!(!arm.anchored, "half a worth anchors nothing");
        assert!(v.take(CMD_READ_ORDERS).is_none(), "the spot half first");
        let sp = v
            .take(CMD_READ_SPOT)
            .expect("the spot sheet after the perp one");
        assert_eq!(sp.slot, SLOT);
        v.send(read_end(&sp, 1_000 * E8, 0));
        let _ = arm.idle(t);
        assert_eq!(
            v.keeper(),
            Some(1_000_000_000),
            "$0 on the perps (spot margins them) and $1 000 USDC"
        );
        let od = v.take(CMD_READ_ORDERS).expect("the orders after it");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget");
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        let day = v.take(CMD_READ_DAY).expect("the day");
        v.send(day_end(&day, 0, 0, 0));
        let _ = arm.idle(t);
        let sig = arm.signal(t);
        assert_eq!(
            (sig.reconciled, sig.pnl_judged, sig.pnl_delta_usd_1e6),
            (1, 1, 0)
        );
        // A period on: $2 of unrealised gain on the perp sheet, then $3
        // less USDC (a loss settled to spot).
        let at = t + 5 * S;
        let _ = arm.idle(at);
        let st = v.take(CMD_READ_STATE).expect("the sheet");
        v.send(read_end(&st, 2 * E8, 2));
        let _ = arm.idle(at);
        assert_eq!(
            arm.signal(at).pnl_delta_usd_1e6,
            2_000_000,
            "the last spot half until the next one lands"
        );
        let sp = v.take(CMD_READ_SPOT).expect("the spot half again");
        v.send(read_end(&sp, 997 * E8, 0));
        let _ = arm.idle(at);
        assert_eq!(arm.signal(at).pnl_delta_usd_1e6, -1_000_000);
    }

    /// XH5b-1: a restored anchor is never judged against half a worth —
    /// the perp half alone would read the whole spot balance as a loss.
    #[test]
    fn a_restored_anchor_is_not_judged_on_half_a_worth() {
        let mut c = cfg();
        c.anchor_1e6 = 1_000_000_000;
        let (mut arm, mut v, t, st) = to_sheet(&c);
        v.send(read_end(&st, 0, 1));
        let _ = arm.idle(t);
        assert_eq!(
            arm.signal(t).pnl_judged,
            0,
            "the perp half alone would read −$1 000"
        );
        let sp = v.take(CMD_READ_SPOT).expect("the spot sheet");
        v.send(read_end(&sp, 990 * E8, 0));
        let _ = arm.idle(t);
        let sig = arm.signal(t);
        assert_eq!((sig.pnl_judged, sig.pnl_delta_usd_1e6), (1, -10_000_000));
        assert_eq!(v.keeper(), None, "restored, not taken");
    }

    /// XH5b-1: a failed spot read — an end of another kind, or no answer
    /// past the post timeout and its grace — waits for the next perp
    /// sheet: one spot read a sheet, so it never storms, at skew 0 too; the
    /// orders go meanwhile.
    #[test]
    fn a_failed_spot_read_waits_for_the_next_sheet() {
        let mut c = cfg();
        c.skew_ms = 0;
        let (mut arm, mut v, t) = booted_with(&c);
        let at = t + 5 * S;
        let _ = arm.idle(at);
        let st = v.take(CMD_READ_STATE).expect("the sheet");
        v.send(read_end(&st, 1_000 * E8, 2));
        let _ = arm.idle(at);
        let sp = v.take(CMD_READ_SPOT).expect("the spot half");
        v.send(ExecEvent {
            reason: READ_STATE,
            ..read_end(&sp, 0, 0)
        });
        let _ = arm.idle(at);
        let od = v.take(CMD_READ_ORDERS).expect("the orders meanwhile");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(at + 1);
        assert!(
            v.take(CMD_READ_SPOT).is_none(),
            "not before the next sheet, skew 0 or not"
        );
        let at = at + 5 * S;
        let _ = arm.idle(at);
        let st = v.take(CMD_READ_STATE).expect("the next sheet");
        v.send(read_end(&st, 1_000 * E8, 3));
        let _ = arm.idle(at);
        assert!(v.take(CMD_READ_SPOT).is_some(), "asked again with it");
        // Unanswered past the post timeout and its grace.
        let _ = arm.idle(at + 5 * S + READ_GRACE_NS + 1);
        assert_eq!(arm.counters().reads_failed, 2);
        assert_eq!(arm.counters().spot_failed, 2, "XH5b-2: on /metrics");
        assert!(
            v.take(CMD_READ_SPOT).is_none(),
            "again only with the sheet after"
        );
    }

    /// XH5b-1: a spot sheet that lists no USDC answers nothing — never a
    /// $0 half to anchor on (`account_view`'s rule): a failed read.
    #[test]
    fn a_spot_sheet_without_usdc_is_a_failed_read() {
        let (mut arm, mut v, t, st) = to_sheet(&cfg());
        v.send(read_end(&st, 600 * E8, 1));
        let _ = arm.idle(t);
        let sp = v.take(CMD_READ_SPOT).expect("the spot sheet");
        v.send(ExecEvent {
            sz_1e8: 0,
            ..read_end(&sp, 0, 0)
        });
        let _ = arm.idle(t);
        assert!(!arm.anchored, "never a $600 anchor on half a worth");
        let c = arm.counters();
        assert_eq!((c.reads_failed, c.spot_reads, c.spot_failed), (1, 0, 1));
        assert_eq!(v.keeper(), None);
    }

    /// XH5b-1: the router's first-reconciliation latch waits for the worth
    /// — a boot whose spot read fails reads the sheet, the orders, the
    /// budget and the day, and still reports nothing reconciled, no
    /// agreement and nothing judged until a spot half lands.
    #[test]
    fn the_first_reconciliation_waits_for_the_spot_half() {
        let (mut arm, mut v, t, st) = to_sheet(&cfg());
        v.send(read_end(&st, 0, 1));
        let _ = arm.idle(t);
        let sp = v.take(CMD_READ_SPOT).expect("the spot sheet");
        v.send(ExecEvent {
            kind: EV_READ_FAILED,
            ..read_end(&sp, 0, 0)
        });
        let _ = arm.idle(t);
        let od = v.take(CMD_READ_ORDERS).expect("the orders");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget");
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        let day = v.take(CMD_READ_DAY).expect("the day");
        v.send(day_end(&day, 0, 0, 0));
        let _ = arm.idle(t);
        let sig = arm.signal(t);
        assert_eq!(
            (
                sig.reconciled,
                sig.recon_age_ns,
                sig.pnl_judged,
                sig.pnl_delta_usd_1e6
            ),
            (0, 0, 0, 0),
            "no worth: nothing reconciled, no agreement, nothing judged"
        );
        assert_eq!(v.keeper(), None);
        let at = t + 5 * S;
        let _ = arm.idle(at);
        let st = v.take(CMD_READ_STATE).expect("the next sheet");
        v.send(read_end(&st, 0, 2));
        let _ = arm.idle(at);
        let sp = v.take(CMD_READ_SPOT).expect("the spot half with it");
        v.send(read_end(&sp, 1_000 * E8, 0));
        let _ = arm.idle(at);
        assert_eq!(v.keeper(), Some(1_000_000_000));
        let _ = arm.idle(at);
        let sig = arm.signal(at);
        assert_eq!((sig.reconciled, sig.pnl_judged), (1, 1));
    }

    /// XH5b-1: a $0 worth anchors nothing (an unfunded account, the
    /// blocking arm's E7 rule) — no first reconciliation, nothing judged or
    /// offered — until a positive worth lands.
    #[test]
    fn a_zero_worth_anchors_nothing_until_the_account_is_funded() {
        let (mut arm, mut v, t, st) = to_sheet(&cfg());
        v.send(read_end(&st, 0, 1));
        let _ = arm.idle(t);
        let sp = v.take(CMD_READ_SPOT).expect("the spot sheet");
        v.send(read_end(&sp, 0, 0));
        let _ = arm.idle(t);
        assert!(!arm.anchored, "$0 and $0: nothing to anchor");
        assert_eq!(v.keeper(), None);
        let od = v.take(CMD_READ_ORDERS).expect("the orders");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget");
        v.send(limit_end(&lim, 100));
        let _ = arm.idle(t);
        let day = v.take(CMD_READ_DAY).expect("the day");
        v.send(day_end(&day, 0, 0, 0));
        let _ = arm.idle(t);
        let sig = arm.signal(t);
        assert_eq!(
            (sig.reconciled, sig.pnl_judged),
            (0, 0),
            "no anchor: no first reconciliation"
        );
        // Funded: the next complete worth anchors.
        let at = t + 5 * S;
        let _ = arm.idle(at);
        let st = v.take(CMD_READ_STATE).expect("the next sheet");
        v.send(read_end(&st, 0, 2));
        let _ = arm.idle(at);
        let sp = v.take(CMD_READ_SPOT).expect("the spot half");
        v.send(read_end(&sp, 500 * E8, 0));
        let _ = arm.idle(at);
        assert_eq!(v.keeper(), Some(500_000_000));
        let _ = arm.idle(at);
        let sig = arm.signal(at);
        assert_eq!(
            (sig.reconciled, sig.pnl_judged, sig.pnl_delta_usd_1e6),
            (1, 1, 0)
        );
    }

    /// XH5b-1: the spot half goes ahead of a pending re-read, which goes
    /// one answer later.
    #[test]
    fn the_spot_half_goes_ahead_of_a_pending_re_read() {
        let (mut arm, mut v, t) = booted();
        let at = t + 5 * S;
        let _ = arm.idle(at);
        let st = v.take(CMD_READ_STATE).expect("the sheet");
        v.send(ExecEvent {
            kind: EV_GAP,
            ..ExecEvent::default()
        });
        v.send(read_end(&st, 1_000 * E8, 2));
        let _ = arm.idle(at);
        assert!(arm.reread, "the gap asks for a re-read");
        assert!(v.take(CMD_READ_ORDERS).is_none(), "not yet");
        let sp = v.take(CMD_READ_SPOT).expect("the spot half first");
        v.send(read_end(&sp, 0, 0));
        let _ = arm.idle(at);
        assert!(v.take(CMD_READ_ORDERS).is_some(), "then the re-read");
    }

    /// XH5b-1: the agreement needs the worth's spot half fresh, as it
    /// needs the sheet and the orders — a spot read that keeps failing
    /// ages it (the router's `ReconStale`), while the P&L stays judged on
    /// the last worth read.
    #[test]
    fn an_agreement_needs_a_fresh_spot_half() {
        let (mut arm, mut v, t) = booted();
        // Every spot read fails from here; the sheets and the orders go on.
        let mut at = t + 5 * S;
        while at < t + 30 * S {
            let _ = arm.idle(at);
            if let Some(st) = v.take(CMD_READ_STATE) {
                v.send(read_end(&st, 1_000 * E8, 2));
            }
            if let Some(sp) = v.take(CMD_READ_SPOT) {
                v.send(ExecEvent {
                    kind: EV_READ_FAILED,
                    ..read_end(&sp, 0, 0)
                });
            }
            if let Some(od) = v.take(CMD_READ_ORDERS) {
                v.send(read_end(&od, 0, 0));
            }
            at += S;
        }
        let sig = arm.signal(at);
        assert!(
            sig.recon_age_ns >= 15 * S,
            "the age runs from the last agreement with a fresh spot half"
        );
        assert_eq!(sig.pnl_judged, 1, "judged on the last worth read");
    }

    // --- XMM XH5c-2: the top-ups ----------------------------------------

    /// Top-ups of 1 000 requests a purchase, at most `day_max` a UTC day.
    fn gov(day_max: u64) -> TopupGovernor {
        TopupGovernor::restored(now_ms() / DAY_MS, 0, 0).armed(1_000, day_max)
    }

    /// The next budget read at `at` — after the periodic reads — answered
    /// with `used` requests of a fresh address's 10 000 and `surplus`
    /// reserved beyond use (the venue's own netting of a purchase).
    fn budget_at(arm: &mut HlGatewayArm, v: &mut Venue, at: u64, used: u64, surplus: i64) {
        recon(arm, v, at, None, &[]);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget read");
        v.send(ExecEvent {
            sz_1e8: surplus,
            ..limit_end(&lim, used)
        });
        let _ = arm.idle(at);
    }

    /// Booted with top-ups armed (`day_max`), a budget read inside the band
    /// `(2 000, 3 000]` answered (2 600 left), its hand-over stored by the
    /// keeper (emulated) and the purchase pushed — still in the ring.
    fn topped_up(day_max: u64) -> (HlGatewayArm, Venue, Arc<TopupSlot>, u64) {
        let (mut arm, mut v, t) = booted();
        let slot = Arc::new(TopupSlot::new());
        arm.set_topup(gov(day_max), Some(Arc::clone(&slot)))
            .expect("armed");
        let at = t + 600 * S;
        budget_at(&mut arm, &mut v, at, 7_400, 0);
        let (ver, _) = slot.pending().expect("handed over");
        slot.mark_kept(ver);
        let _ = arm.idle(at);
        (arm, v, slot, at)
    }

    /// A top-up is decided at a budget read inside the band — the day
    /// charged and handed to the keeper — leaves only once stored (one
    /// `reserveRequestWeight`, its own client id, the weight, one address
    /// request) and is judged at the next read issued after its answer:
    /// seen, its price is the session's spend, added back to the P&L
    /// (Q-XH5-2), and the state handed over again; never `stray`.
    #[test]
    fn a_top_up_is_decided_stored_then_bought_and_seen_at_the_next_read() {
        let (mut arm, mut v, t) = booted();
        let slot = Arc::new(TopupSlot::new());
        arm.set_topup(gov(10_000), Some(Arc::clone(&slot)))
            .expect("armed");
        let at = t + 600 * S;
        budget_at(&mut arm, &mut v, at, 5_000, 0);
        assert!(slot.pending().is_none(), "above the band: nothing decided");
        let at = at + 600 * S;
        budget_at(&mut arm, &mut v, at, 7_400, 0);
        assert!(
            v.take(CMD_RESERVE).is_none(),
            "never before its state is stored"
        );
        let (ver, st) = slot.pending().expect("decided and handed over");
        assert_eq!(
            (st.day, st.used, st.cost_1e6),
            (now_ms() / DAY_MS, 1_000, 0),
            "the day charged with the decision"
        );
        slot.mark_kept(ver);
        let before = arm.budget().expect("read").remaining();
        let _ = arm.idle(at);
        let c = v.take(CMD_RESERVE).expect("the purchase, once stored");
        assert_eq!(
            (c.slot, c.arg, c.client_oid & !TOPUP_SEQ_MASK),
            (SLOT, 1_000, TOPUP_OID_BASE)
        );
        assert_eq!(
            arm.budget().expect("read").remaining(),
            before - 1,
            "one address request"
        );
        let _ = arm.idle(at);
        assert!(v.take(CMD_RESERVE).is_none(), "one purchase");
        v.send(item(&c, EV_ACK, 0, 0));
        let _ = arm.idle(at);
        budget_at(&mut arm, &mut v, at + 600 * S, 7_400, 1_000);
        let k = arm.counters();
        assert_eq!(
            (k.topup_ok, k.topup_failed, k.topup_unsent, k.stray),
            (1, 0, 0, 0)
        );
        assert_eq!(arm.topup.cost_1e6(), 1_000 * TOPUP_PRICE_1E6);
        let (_, st) = slot.pending().expect("the spend handed over");
        assert_eq!((st.used, st.cost_1e6), (1_000, 500_000));
        assert_eq!(
            arm.session_pnl(),
            (true, 500_000),
            "the worth unchanged here: the spend added back"
        );
        let mut r = GatewayArmReport::default();
        assert!(arm.gateway_arm_report(at, &mut r));
        assert_eq!(
            (
                r.topup_ok,
                r.topup_weight,
                r.topup_day_used,
                r.topup_cost_usd_1e6,
                r.session_pnl_usd_1e6
            ),
            (1, 1_000, 1_000, 500_000, 500_000)
        );
    }

    /// A purchase refused before it left (throttled) is never bought — its
    /// request taken back, the day's charge returned, no spend — and the
    /// next read inside the band decides again; never `stray`.
    #[test]
    fn a_purchase_refused_before_it_left_is_never_bought() {
        let (mut arm, mut v, slot, at) = topped_up(10_000);
        let c = v.take(CMD_RESERVE).expect("the purchase");
        let refunded = arm.counters().refunded;
        let before = arm.budget().expect("read").remaining();
        v.send(item(&c, EV_REJECTED, REASON_THROTTLED, 0));
        let _ = arm.idle(at);
        let k = arm.counters();
        assert_eq!(k.refunded, refunded + 1, "its request taken back");
        assert_eq!(arm.budget().expect("read").remaining(), before + 1);
        assert_eq!(
            (k.topup_unsent, k.topup_ok, k.topup_failed, k.stray),
            (1, 0, 0, 0)
        );
        assert_eq!(
            (arm.topup.used(), arm.topup.cost_1e6()),
            (0, 0),
            "the day's charge returned, nothing spent"
        );
        let later = at + 600 * S;
        budget_at(&mut arm, &mut v, later, 7_400, 0);
        let (ver, st) = slot.pending().expect("decided again");
        assert_eq!(st.used, 1_000);
        slot.mark_kept(ver);
        let _ = arm.idle(later);
        assert!(v.take(CMD_RESERVE).is_some(), "bought this time");
    }

    /// An accepted purchase the next read does not show stops the top-ups
    /// (seen on the venue, or no more): nothing charged, the stop handed
    /// over as the day spent, and no later read decides one.
    #[test]
    fn a_purchase_the_next_read_does_not_show_stops_the_top_ups() {
        let (mut arm, mut v, slot, at) = topped_up(10_000);
        let c = v.take(CMD_RESERVE).expect("the purchase");
        v.send(item(&c, EV_ACK, 0, 0));
        let _ = arm.idle(at);
        budget_at(&mut arm, &mut v, at + 600 * S, 7_400, 0);
        let k = arm.counters();
        assert_eq!((k.topup_ok, k.topup_failed, k.stray), (0, 1, 0));
        assert_eq!(
            (arm.topup.weight(), arm.topup.cost_1e6()),
            (0, 0),
            "stopped, nothing charged"
        );
        let (ver, st) = slot.pending().expect("the stop handed over");
        assert_eq!(st.used, 10_000, "the day reads as spent");
        slot.mark_kept(ver);
        budget_at(&mut arm, &mut v, at + 1_200 * S, 7_400, 0);
        assert!(slot.pending().is_none(), "nothing more decided");
        assert!(v.take(CMD_RESERVE).is_none());
    }

    /// A purchase the venue refused stops the top-ups (its request stays
    /// charged: it left); one in doubt is carried as a request unanswered
    /// until the next read judges it — seen, its price charged once a sheet
    /// issued after THAT read lands: it may have landed after the sheet
    /// issued since its answer.
    #[test]
    fn a_venue_refusal_stops_and_a_purchase_in_doubt_is_judged_by_the_next_read() {
        let (mut arm, mut v, _slot, at) = topped_up(10_000);
        let c = v.take(CMD_RESERVE).expect("the purchase");
        let refunded = arm.counters().refunded;
        v.send(item(&c, EV_REJECTED, ORDER_EVENT_REASON_OTHER, 0));
        let _ = arm.idle(at);
        assert_eq!(
            (
                arm.counters().topup_failed,
                arm.topup.weight(),
                arm.counters().refunded
            ),
            (1, 0, refunded)
        );
        let (mut arm, mut v, _slot, at) = topped_up(10_000);
        let c = v.take(CMD_RESERVE).expect("the purchase");
        let refunded = arm.counters().refunded;
        v.send(item(&c, EV_UNANSWERED, 0, 0));
        let _ = arm.idle(at);
        assert_eq!(
            arm.counters().refunded,
            refunded,
            "in doubt: it may have left"
        );
        let later = at + 600 * S;
        recon(&mut arm, &mut v, later, None, &[]);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget read");
        assert_eq!(arm.limit_sent, 1, "carried while in doubt");
        v.send(ExecEvent {
            sz_1e8: 1_000,
            ..limit_end(&lim, 7_400)
        });
        let _ = arm.idle(later);
        assert_eq!(
            (arm.counters().topup_ok, arm.topup.cost_1e6()),
            (0, 0),
            "seen — the sheet issued since its answer may predate a late landing"
        );
        assert!(arm.topup_shown && !arm.topup_in_worth);
        recon(&mut arm, &mut v, later + 6 * S, None, &[]);
        assert_eq!(
            (arm.counters().topup_ok, arm.topup.cost_1e6()),
            (1, 500_000),
            "the sheet issued after the read that showed it, with its spot half"
        );
    }

    /// A budget read that went before the purchase's answer judges nothing
    /// — its figure may predate the purchase, which it carries as one
    /// request unanswered — and the next one does.
    #[test]
    fn a_read_issued_before_the_answer_judges_nothing() {
        let (mut arm, mut v, _slot, at) = topped_up(10_000);
        let c = v.take(CMD_RESERVE).expect("the purchase");
        let later = at + 600 * S;
        recon(&mut arm, &mut v, later, None, &[]);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget read, in flight");
        assert_eq!(arm.limit_sent, 1, "the purchase carried");
        v.send(item(&c, EV_ACK, 0, 0));
        v.send(ExecEvent {
            sz_1e8: 1_000,
            ..limit_end(&lim, 7_400)
        });
        let _ = arm.idle(later);
        assert_eq!(
            (arm.counters().topup_ok, arm.counters().topup_failed),
            (0, 0),
            "not judged"
        );
        budget_at(&mut arm, &mut v, later + 600 * S, 7_400, 1_000);
        assert_eq!(arm.counters().topup_ok, 1, "the next read judges it");
    }

    /// A state its keeper could not store is never bought: the top-ups stop.
    #[test]
    fn a_state_its_keeper_could_not_store_is_never_bought() {
        let (mut arm, mut v, t) = booted();
        let slot = Arc::new(TopupSlot::new());
        arm.set_topup(gov(10_000), Some(Arc::clone(&slot)))
            .expect("armed");
        let at = t + 600 * S;
        budget_at(&mut arm, &mut v, at, 7_400, 0);
        assert!(slot.pending().is_some(), "handed over");
        slot.mark_failed();
        let _ = arm.idle(at);
        assert!(v.take(CMD_RESERVE).is_none(), "nothing bought unrecorded");
        assert_eq!((arm.counters().topup_failed, arm.topup.weight()), (1, 0));
    }

    /// An answer in the purchase range for no purchase in flight is the
    /// arm's own count gone wrong: the top-ups stop; never `stray`.
    #[test]
    fn an_answer_for_no_purchase_in_flight_stops_the_top_ups() {
        let (mut arm, mut v, _slot, at) = topped_up(10_000);
        let c = v.take(CMD_RESERVE).expect("the purchase");
        let mut other = c;
        other.client_oid = c.client_oid + 1;
        v.send(item(&other, EV_ACK, 0, 0));
        let _ = arm.idle(at);
        assert_eq!(
            (
                arm.counters().topup_failed,
                arm.counters().stray,
                arm.topup.weight()
            ),
            (1, 0, 0)
        );
    }

    /// The day's max binds: at a max of one purchase, the next read inside
    /// the band decides nothing.
    #[test]
    fn the_day_s_max_binds() {
        let (mut arm, mut v, slot, at) = topped_up(1_000);
        let c = v.take(CMD_RESERVE).expect("the day's one purchase");
        v.send(item(&c, EV_ACK, 0, 0));
        let _ = arm.idle(at);
        budget_at(&mut arm, &mut v, at + 600 * S, 7_400, 1_000);
        assert_eq!(arm.counters().topup_ok, 1);
        let (ver, _) = slot.pending().expect("the spend handed over");
        slot.mark_kept(ver);
        budget_at(&mut arm, &mut v, at + 1_200 * S, 8_400, 1_000);
        assert!(slot.pending().is_none(), "the day's max");
        assert!(v.take(CMD_RESERVE).is_none());
    }

    /// Top-ups arm on the slot's own account alone, with a weight the
    /// purchase command carries (narrowed by refusal, never truncated) and a
    /// keeper (off, the governor alone), and decide nothing before the
    /// session's anchor is durable.
    #[test]
    fn top_ups_arm_on_an_own_account_alone_and_wait_for_the_anchor() {
        let slot = Arc::new(TopupSlot::new());
        let mut shared = cfg();
        shared.own_account = false;
        let (eng, _gw) = hl_duplex();
        let mut arm = HlGatewayArm::new(&shared, eng).expect("arm");
        assert_eq!(
            arm.set_topup(gov(10_000), Some(Arc::clone(&slot))),
            Err(ArmErr::Topup),
            "the shared account's requests are its blocking arm's to buy"
        );
        let (eng, _gw) = hl_duplex();
        let mut arm = HlGatewayArm::new(&cfg(), eng).expect("arm");
        let wide = TopupGovernor::restored(0, 0, 0);
        assert_eq!(
            arm.set_topup(
                wide.armed(u64::from(u32::MAX) + 1, u64::MAX),
                Some(Arc::clone(&slot))
            ),
            Err(ArmErr::Topup),
            "past u32: refused, never truncated"
        );
        assert!(arm
            .set_topup(
                wide.armed(u64::from(u32::MAX), u64::MAX),
                Some(Arc::clone(&slot))
            )
            .is_ok());
        assert_eq!(
            arm.set_topup(gov(10_000), None),
            Err(ArmErr::Topup),
            "armed with no keeper"
        );
        assert!(
            arm.set_topup(TopupGovernor::restored(0, 0, 500_000), None)
                .is_ok(),
            "off: the governor alone, for the restored spend's add-back"
        );
        let (mut arm, mut v, t) = booted_unkept();
        arm.set_topup(gov(10_000), Some(Arc::clone(&slot)))
            .expect("armed");
        budget_at(&mut arm, &mut v, t + 600 * S, 7_400, 0);
        assert!(!arm.anchor_kept);
        assert!(
            slot.pending().is_none(),
            "no price before the anchor it is added back against is durable"
        );
    }

    /// The shutdown buys nothing: a budget read inside the band answered
    /// during it decides nothing, and nothing is handed over or sent.
    #[test]
    fn the_shutdown_buys_nothing() {
        let (mut arm, mut v, t) = booted();
        let slot = Arc::new(TopupSlot::new());
        arm.set_topup(gov(10_000), Some(Arc::clone(&slot)))
            .expect("armed");
        let at = t + 600 * S;
        recon(&mut arm, &mut v, at, None, &[]);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget read");
        v.send(limit_end(&lim, 7_400));
        let stop = Arc::new(AtomicBool::new(false));
        let h = responder(v, |_| SWEEP_CLEAR, true, Arc::clone(&stop));
        arm.on_shutdown();
        stop.store(true, Ordering::Relaxed);
        let v = h.join().expect("the responder");
        assert_eq!(arm.counters().limit_reads, 2, "the read was answered");
        assert!(slot.pending().is_none(), "nothing decided");
        assert!(
            v.cmds.iter().all(|c| c.kind != CMD_RESERVE),
            "nothing bought"
        );
    }

    /// The add-back waits for a worth that carries the price: a budget read
    /// that shows the purchase before any perp sheet issued after it charges
    /// nothing; the next sheet and its spot half do.
    #[test]
    fn the_add_back_waits_for_a_worth_that_carries_the_price() {
        let mut cf = cfg();
        cf.limit_ms = 1_000;
        let (mut arm, mut v, t) = booted_with(&cf);
        let slot = Arc::new(TopupSlot::new());
        arm.set_topup(gov(10_000), Some(Arc::clone(&slot)))
            .expect("armed");
        let at = t + S;
        let _ = arm.idle(at);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget read");
        v.send(limit_end(&lim, 7_400));
        let _ = arm.idle(at);
        let (ver, _) = slot.pending().expect("decided");
        slot.mark_kept(ver);
        let _ = arm.idle(at);
        let buy = v.take(CMD_RESERVE).expect("the purchase");
        v.send(item(&buy, EV_ACK, 0, 0));
        let _ = arm.idle(at);
        let at2 = at + S;
        let _ = arm.idle(at2);
        let lim = v.take(CMD_READ_LIMIT).expect("the next budget read");
        v.send(ExecEvent {
            sz_1e8: 1_000,
            ..limit_end(&lim, 7_400)
        });
        let _ = arm.idle(at2);
        assert_eq!(
            (arm.counters().topup_ok, arm.topup.cost_1e6()),
            (0, 0),
            "seen on the venue, not inside the worth yet"
        );
        assert_eq!(arm.session_pnl(), (true, 0), "no add-back yet");
        recon(&mut arm, &mut v, at2 + 5 * S, None, &[]);
        assert_eq!(
            (arm.counters().topup_ok, arm.topup.cost_1e6()),
            (1, 500_000),
            "the sheet after it, with its spot half"
        );
    }

    /// A halted slot buys nothing more: the router's sweep of the slot
    /// (`cancel_slot`) ends the buying — a purchase decided is never sent
    /// (the day's charge returned), a later read inside the band decides
    /// none — while a purchase that left is still judged and charged, its
    /// spend handed over.
    #[test]
    fn a_halted_slot_buys_nothing_more() {
        let (mut arm, mut v, t) = booted();
        let slot = Arc::new(TopupSlot::new());
        arm.set_topup(gov(10_000), Some(Arc::clone(&slot)))
            .expect("armed");
        let at = t + 600 * S;
        budget_at(&mut arm, &mut v, at, 7_400, 0);
        let (ver, _) = slot.pending().expect("decided");
        arm.cancel_slot(SLOT).expect("the halt's sweep");
        assert_eq!(
            (arm.counters().topup_unsent, arm.topup.used()),
            (1, 0),
            "abandoned, the day returned"
        );
        slot.mark_kept(ver);
        let _ = arm.idle(at);
        assert!(v.take(CMD_RESERVE).is_none(), "never sent");
        budget_at(&mut arm, &mut v, at + 600 * S, 7_400, 0);
        assert!(slot.pending().is_none(), "nothing decided");
        assert!(v.take(CMD_RESERVE).is_none());
        // A purchase that left before the halt: judged and charged.
        let (mut arm, mut v, slot, at) = topped_up(10_000);
        let c = v.take(CMD_RESERVE).expect("the purchase");
        arm.cancel_slot(SLOT).expect("the halt's sweep");
        v.send(item(&c, EV_ACK, 0, 0));
        let _ = arm.idle(at);
        budget_at(&mut arm, &mut v, at + 600 * S, 7_400, 1_000);
        assert_eq!(
            (arm.counters().topup_ok, arm.topup.cost_1e6()),
            (1, 500_000),
            "its price is the session's spend"
        );
        let (ver, st) = slot.pending().expect("the spend handed over");
        assert_eq!(st.cost_1e6, 500_000);
        slot.mark_kept(ver);
        budget_at(&mut arm, &mut v, at + 1_200 * S, 8_400, 1_000);
        assert!(slot.pending().is_none(), "nothing more decided");
        assert!(v.take(CMD_RESERVE).is_none());
        let mut r = GatewayArmReport::default();
        assert!(arm.gateway_arm_report(at, &mut r));
        assert_eq!(
            (r.topup_ok, r.topup_weight, r.topup_cost_usd_1e6),
            (1, 0, 500_000),
            "the buying ended: the weight reads 0, the spend stays"
        );
    }

    /// A purchase decided whose keeper is slow, and that a newer budget read
    /// leaves out of the band, never leaves: the day's charge returned.
    #[test]
    fn a_decision_a_newer_read_leaves_out_of_the_band_never_leaves() {
        let mut cf = cfg();
        cf.limit_ms = 1_000;
        let (mut arm, mut v, t) = booted_with(&cf);
        let slot = Arc::new(TopupSlot::new());
        arm.set_topup(gov(10_000), Some(Arc::clone(&slot)))
            .expect("armed");
        let at = t + S;
        let _ = arm.idle(at);
        let lim = v.take(CMD_READ_LIMIT).expect("the budget read");
        v.send(limit_end(&lim, 7_400));
        let _ = arm.idle(at);
        let (ver, _) = slot.pending().expect("decided, its keeper slow");
        let at2 = at + S;
        let _ = arm.idle(at2);
        let lim = v.take(CMD_READ_LIMIT).expect("a newer budget read");
        v.send(limit_end(&lim, 5_000));
        let _ = arm.idle(at2);
        assert_eq!(
            (arm.counters().topup_unsent, arm.topup.used()),
            (1, 0),
            "abandoned, the day returned"
        );
        slot.mark_kept(ver);
        let _ = arm.idle(at2);
        assert!(v.take(CMD_RESERVE).is_none(), "never sent");
    }

    /// A spend the keeper cannot take at once (it still stores another
    /// hand-over) is owed, and goes at a later pass.
    #[test]
    fn a_hand_over_the_keeper_cannot_take_at_once_goes_later() {
        let (mut arm, mut v, slot, at) = topped_up(10_000);
        let c = v.take(CMD_RESERVE).expect("the purchase");
        v.send(item(&c, EV_ACK, 0, 0));
        let _ = arm.idle(at);
        let busy = slot
            .publish(&TopupState::default())
            .expect("another hand-over takes the keeper");
        let later = at + 600 * S;
        budget_at(&mut arm, &mut v, later, 7_400, 1_000);
        assert_eq!(arm.counters().topup_ok, 1, "seen, inside the worth");
        assert!(arm.topup_owed, "its spend is owed");
        slot.mark_kept(busy);
        let _ = arm.idle(later);
        let (_, st) = slot.pending().expect("the spend, handed over");
        assert_eq!(st.cost_1e6, 500_000);
        assert!(!arm.topup_owed);
    }

    /// A hand-over owed when the shutdown begins goes during it — the
    /// shutdown waits for its keeper (busy a while here), within its bound.
    #[test]
    fn a_hand_over_owed_goes_during_the_shutdown() {
        let (mut arm, mut v, slot, at) = topped_up(10_000);
        let c = v.take(CMD_RESERVE).expect("the purchase");
        v.send(item(&c, EV_ACK, 0, 0));
        let _ = arm.idle(at);
        let busy = slot
            .publish(&TopupState::default())
            .expect("another hand-over takes the keeper");
        budget_at(&mut arm, &mut v, at + 600 * S, 7_400, 1_000);
        assert!(arm.topup_owed, "its spend is owed");
        let keeper = Arc::clone(&slot);
        let k = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            keeper.mark_kept(busy);
        });
        let stop = Arc::new(AtomicBool::new(false));
        let h = responder(v, |_| SWEEP_CLEAR, true, Arc::clone(&stop));
        arm.on_shutdown();
        stop.store(true, Ordering::Relaxed);
        let _ = h.join().expect("the responder");
        k.join().expect("the keeper");
        assert!(!arm.topup_owed, "handed over during the shutdown");
        let (_, st) = slot.pending().expect("the spend, handed over");
        assert_eq!(st.cost_1e6, 500_000);
        let n = arm.counters();
        assert_eq!(
            (n.shutdown_switch_held, n.shutdown_unconfirmed),
            (0, 0),
            "nothing held at the bound"
        );
    }

    /// A stop spends the UTC day it happens on — not the day the governor
    /// counted last.
    #[test]
    fn a_stop_spends_the_day_it_happens_on() {
        let (mut arm, mut v, t) = booted();
        let slot = Arc::new(TopupSlot::new());
        let today = utc_day();
        arm.set_topup(
            TopupGovernor::restored(today - 1, 5_000, 0).armed(1_000, 10_000),
            Some(Arc::clone(&slot)),
        )
        .expect("armed");
        let none = HlCmd::reserve(SLOT, TOPUP_OID_BASE | 7, 1_000);
        v.send(item(&none, EV_ACK, 0, 0));
        let _ = arm.idle(t);
        let (_, st) = slot.pending().expect("the stop, handed over");
        assert_eq!((st.day, st.used), (today, 10_000));
        assert_eq!((arm.counters().topup_failed, arm.counters().stray), (1, 0));
    }
}
