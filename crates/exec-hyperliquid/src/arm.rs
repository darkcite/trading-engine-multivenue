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
//! after that close still lists. The session's P&L anchor is restored
//! from the owner's state file (E7: a bound re-anchored per boot is a
//! bound on nothing) or taken at the first sheet and offered to the owner
//! to keep; it is judged on the slot's own account only.
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
//! event can be lost, the word cannot). The budget floor is XH4e's.
//!
//! Engine thread, 0 B/op after boot (gate 98): fixed tables, fixed
//! queues, linear scans over at most [`ARM_ORDERS`] rows.

use clob_dispatcher::{
    CancelAllState, DispatchError, DispatchStats, HaltSignal, OrderDispatch, Retired,
    RETIRED_CANCELED_MEMBER, RETIRED_CANCELED_VENUE, RETIRED_EXPIRED, RETIRED_REJECTED,
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
use exec_gateway::stats::{SWEEP_CLEAR, SWEEP_STRANDED};
use exec_gateway::{boxed_array, HALT_NONE};

use crate::action::Tif;
use crate::cmd::{HlCmd, CMD_CANCEL};
use crate::deadman::{DeadMan, DeadManCfg};
use crate::event::{
    ExecEvent, ASSET_UNKNOWN, EVF_BUY, EVF_CROSSED, EVF_FILLED, EVF_POST_ITEM, EV_ACK, EV_CANCELED,
    EV_FILL, EV_FILLED, EV_FILL_META, EV_GAP, EV_HALT, EV_OPEN_ORDER, EV_POSITION, EV_READ_DONE,
    EV_READ_FAILED, EV_REJECTED, EV_RESTING, EV_SNAPSHOT, EV_UNANSWERED, READ_ORDERS, READ_STATE,
    REASON_HALTED, REASON_LOCAL, REASON_NOT_FOUND, REASON_NOT_READY, REASON_TRANSPORT,
};
use crate::gateway::HlEngineEnd;
use crate::nonce::now_ms;
use crate::rules::{PerpRules, Round};

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
/// its own end — and two retirements.
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
/// Arm-internal client ids (the dead man's posts and the reads): never
/// cloids; their post items carry command bits 0, an order's never do.
const DEADMAN_OID_BASE: u64 = 1 << 63;
const READ_OID_BASE: u64 = (1 << 63) | (1 << 62);
/// Wire 1e8 per engine 1e6.
const WIRE_PER_ENGINE: i64 = 100;
const NS_PER_MS: u64 = 1_000_000;

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
    /// The sheet and the orders are read this often, ms.
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
    /// positions are strangers, and the account value is the slot's P&L.
    pub own_account: bool,
    /// The session's P&L anchor restored from the owner's state file,
    /// account value USD ×1e6; 0 = none (the first sheet anchors, and
    /// [`HlGatewayArm::fresh_anchor_1e6`] offers it for keeping).
    pub anchor_1e6: i64,
    /// The dead-man switch's timing.
    pub deadman: DeadManCfg,
}

/// Why an arm cannot be built.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArmErr {
    /// A slot outside the engine's eight.
    Slot,
    /// No perps, more than [`ARM_PERPS`], or one bound twice.
    Perps,
    /// A zero expiry, post timeout, read period, join, gone or shutdown
    /// window, or one above its bound (60 s; the shutdown 30 s); a skew
    /// above 5 s; a dead-man answer that is not the post timeout.
    Timing,
    /// The dead-man timing does not hold.
    DeadMan,
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

/// A fixed out-queue.
struct Out<T: Copy> {
    buf: Box<[T; OUT]>,
    head: usize,
    len: usize,
}

impl<T: Copy> Out<T> {
    fn new(blank: T) -> Self {
        Self {
            buf: boxed_array(blank),
            head: 0,
            len: 0,
        }
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
        // COPY: one entry (a 64 B order event or fill, a 16 B retirement)
        // into its slot — the queue IS the hand-over: one gateway event
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
        // COPY: one entry (a 64 B fill, a 16 B retirement) returned by
        // value — the trait's `try_next_fill` / `try_next_retired` return
        // owned values (a fill's `Option` has a niche: 64 B).
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
    anchored: bool,
    /// The anchor was taken this boot (the owner keeps it).
    fresh_anchor: bool,
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
    /// The dead man's command, built in place.
    scratch: HlCmd,
}

// The arm's inline state, 1 344 B — what `new` moves into its owner (the
// tables are boxed).
const _: () = assert!(core::mem::size_of::<HlGatewayArm>() == 1_344);

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
            || cfg.skew_ms > MAX_SKEW_MS
            || cfg.deadman.answer_ms != cfg.post_timeout_ms
        {
            return Err(ArmErr::Timing);
        }
        let deadman =
            DeadMan::new(cfg.deadman, cfg.slot, DEADMAN_OID_BASE).map_err(|_| ArmErr::DeadMan)?;
        // COPY: the arm's inline state — 1 344 B, const-asserted (the
        // configuration ≈ 0.3 KiB, the dead man 256 B, counters, the
        // stats, the ring end, the scratch command, the queue heads and
        // words) — moved at boot into the owner, cold;
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
            anchored: cfg.anchor_1e6 > 0,
            fresh_anchor: false,
            done_ids: boxed_array(0),
            done_ns: boxed_array(0),
            done_inferred: 0,
            done_at: 0,
            sweep_asked: 0,
            hold_until_ns: 0,
            deadman,
            scratch: HlCmd::noop(cfg.slot, 0),
        })
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
    /// every bound perp, and the dead man's boot clear answered: the arm
    /// takes orders.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.boot == BOOT_DONE && !self.boot_held && self.deadman.settled()
    }

    /// The seeding sheet held a position in a bound perp: the arm is held
    /// (XH5 rules on flattening at shutdown or refusing such a boot).
    #[must_use]
    pub const fn boot_position(&self) -> bool {
        self.boot_held
    }

    /// The session anchor this boot took (account value USD ×1e6), for the
    /// owner to keep off the engine thread (E7) in a state file of the
    /// slot's own; `None` when it was restored, not taken yet, or the
    /// account is shared (its P&L is never judged there).
    #[must_use]
    pub const fn fresh_anchor_1e6(&self) -> Option<i64> {
        if self.fresh_anchor && self.cfg.own_account {
            Some(self.anchor_1e8 / WIRE_PER_ENGINE)
        } else {
            None
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

    /// Room on every out-queue for one more step.
    #[inline]
    fn room(&self) -> bool {
        self.events.room() >= STEP_ROOM
            && self.fills.room() >= STEP_ROOM
            && self.retired.room() >= STEP_ROOM
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
        let ev = OrderEvent::new(
            now,
            VenueId::Hyperliquid,
            self.bind(i).sym,
            self.rows[i].client_oid,
            self.cfg.slot,
            kind,
            reason,
            0,
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

    fn filled(&mut self, i: usize, now: u64) {
        self.emit(i, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_NONE, now);
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
    /// once, then — a modify — its predecessor's `CANCELED(REPLACED)`.
    /// Returns the row's index, which closing the predecessor may move.
    fn landed(&mut self, i: usize, now: u64) -> usize {
        let st = self.rows[i].state;
        if st == ST_SENT || st == ST_DOUBT {
            self.rows[i].state = ST_RESTING;
            self.rows[i].since_ns = now;
            self.rows[i].due_ns = 0;
            if self.rows[i].bits & RB_ALO != 0 {
                self.emit(i, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, now);
            }
        }
        let prev = self.rows[i].prev;
        if prev == 0 {
            return i;
        }
        self.rows[i].prev = 0;
        let Some(p) = self.find(prev) else {
            return i;
        };
        self.close(p, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_REPLACED, now);
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
            self.cancel_done(i, now);
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

    /// A cancel succeeded: the order was on the venue until now.
    fn cancel_done(&mut self, i: usize, now: u64) {
        if self.rows[i].state == ST_FILLING {
            // Filled first; its FILLED comes.
            return;
        }
        self.counters.doubt_settled += u64::from(self.rows[i].state == ST_DOUBT);
        let reason = if self.rows[i].bits & RB_ASKED != 0 {
            ORDER_EVENT_REASON_CANCEL_REQUESTED
        } else {
            ORDER_EVENT_REASON_OTHER
        };
        let i = self.landed(i, now);
        self.close(i, ORDER_EVENT_CANCELED, reason, now);
    }

    /// A post item the gateway or the venue refused.
    fn on_refused(&mut self, e: &ExecEvent, now: u64) {
        let never_left = matches!(
            e.reason,
            REASON_LOCAL | REASON_HALTED | REASON_NOT_READY | REASON_TRANSPORT
        );
        if e.cmd_kind() == CMD_CANCEL {
            if let Some(i) = self.answered(e, now) {
                self.cancel_refused(i, e.reason, never_left, now);
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
        if !never_left && !alo_cross {
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

    fn cancel_refused(&mut self, i: usize, reason: u8, never_left: bool, now: u64) {
        if never_left || reason != REASON_NOT_FOUND {
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

    /// `orderUpdates`: open.
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
        let i = self.landed(i, now);
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
        let f = Fill::new(
            e.ts_ns,
            self.cfg.perps[p].sym,
            side,
            Price(e.px_1e8 / WIRE_PER_ENGINE),
            Qty(e.sz_1e8 / WIRE_PER_ENGINE),
            e.client_oid,
        )
        .with_attribution(self.cfg.slot, FILL_ORIGIN_VENUE);
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

    /// The arm's own posts (the dead man's and the reads): command bits 0.
    fn on_internal(&mut self, e: &ExecEvent, now: u64) {
        if self.deadman.on_event(e, now_ms()) {
            return;
        }
        if self.read_oid != 0 && e.slot == self.cfg.slot && e.client_oid == self.read_oid {
            // The read was refused or went unanswered.
            self.read_failed(now);
            return;
        }
        self.counters.stray += 1;
    }

    // --- reads ------------------------------------------------------

    fn issue_read(&mut self, kind: u8, now: u64) -> bool {
        let oid = READ_OID_BASE + self.read_seq + 1;
        let c = if kind == READ_ORDERS {
            HlCmd::read_orders(self.cfg.slot, oid)
        } else {
            HlCmd::read_state(self.cfg.slot, oid)
        };
        if !self.end.cmd.push(&c.at(now)) {
            self.counters.ring_full += 1;
            return false;
        }
        self.read_seq += 1;
        self.read_oid = oid;
        self.read_kind = kind;
        self.read_ns = now;
        if kind == READ_ORDERS {
            self.epoch = self.epoch.wrapping_add(1);
            self.reread = false;
            self.others = false;
            self.unknown_this_read = 0;
        } else {
            self.unknown_positions = 0;
            let n = self.cfg.n_perps.min(ARM_PERPS);
            let mut p = 0usize;
            while p < n {
                self.perps[p].venue_1e8 = 0;
                self.perps[p].value_1e8 = 0;
                p += 1;
            }
        }
        true
    }

    /// The read in flight failed: ask again after a skew.
    fn read_failed(&mut self, now: u64) {
        self.counters.reads_failed += 1;
        let retry = now.saturating_add(self.cfg.skew_ms * NS_PER_MS);
        if self.read_kind == READ_ORDERS {
            self.next_orders_ns = retry;
        } else {
            self.next_state_ns = retry;
        }
        self.read_oid = 0;
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
        if e.asset != ASSET_UNKNOWN
            && !self
                .end
                .cmd
                .push(&HlCmd::cancel(self.cfg.slot, e.client_oid, e.asset).at(now))
        {
            self.counters.ring_full += 1;
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
        if e.kind == EV_READ_FAILED {
            self.read_failed(now);
            return;
        }
        self.read_oid = 0;
        self.counters.reads += 1;
        let next = now.saturating_add(self.cfg.recon_ms * NS_PER_MS);
        if self.read_kind == READ_ORDERS {
            self.orders_done(now);
            self.next_orders_ns = next;
        } else {
            self.state_done(e.px_1e8, e.venue_ms, now);
            self.next_state_ns = next;
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
        if !self.anchored {
            self.anchor_1e8 = account_1e8;
            self.anchored = true;
            self.fresh_anchor = true;
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

    /// Both reads agree and each ended within two periods: the
    /// agreement's clock restarts. A read that stops coming (starved or
    /// failing) shows as `ReconStale`, never as an old agreement.
    fn note_agreement(&mut self, now: u64) {
        let fresh = 2 * self.cfg.recon_ms * NS_PER_MS;
        if self.state_ok
            && self.orders_ok
            && now.saturating_sub(self.sheet_ns) <= fresh
            && now.saturating_sub(self.orders_ns) <= fresh
        {
            self.last_agree_ns = now;
        }
    }

    // --- idle -------------------------------------------------------

    /// One idle pass at `now`.
    fn idle(&mut self, now: u64) -> bool {
        let mut worked = self.pump(now);
        if self.stream_alive(now) {
            self.up_ns = now;
        }
        worked |= self.boot_step(now);
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
    /// at boot, both after it.
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
    /// (a Stranded sweep may leave ours on the book).
    fn deadman_step(&mut self, now: u64) -> bool {
        let want = self.deadman_wanted(now);
        self.drive_deadman(want, now)
    }

    fn deadman_wanted(&mut self, now: u64) -> bool {
        // After boot (whose own sweep reads clear before it ends), a
        // cancel-all not read clear holds the switch like an order does.
        if self.n_rows > 0
            || (self.boot == BOOT_DONE && self.cancel_all_state() != CancelAllState::Clear)
        {
            self.hold_until_ns = now.saturating_add(WANT_HOLD_NS);
        }
        let alone = self.cfg.own_account || self.exclusive;
        self.boot == BOOT_DONE && alone && now < self.hold_until_ns
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
        let reconciled =
            self.boot == BOOT_DONE && !self.boot_held && self.state_ok && self.orders_ok;
        let age = if self.last_agree_ns == 0 {
            0
        } else {
            now.saturating_sub(self.last_agree_ns).max(1)
        };
        HaltSignal::new(
            gap,
            drift,
            self.reject_streak,
            self.asset_refusal_streak,
            false,
            reconciled,
            age,
        )
        .with_pnl(
            self.anchored && self.seeded && self.cfg.own_account,
            (self.account_1e8 - self.anchor_1e8) / WIRE_PER_ENGINE,
        )
        .with_alo_reject_permille(self.alo_reject_permille())
        .with_gateway_halted(self.end.stats.halt_reason() != HALT_NONE)
    }

    /// The command for `o` on perp `p` (a place, or a modify of `prev`),
    /// built in `out`; `false` when it breaks a rule the venue would (LAW
    /// XH-4) or is a kind the arm does not speak.
    fn order_cmd(&self, o: &Order, p: usize, prev: u64, out: &mut HlCmd) -> bool {
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
        let mut c = HlCmd::noop(self.cfg.slot, 0);
        if !self.order_cmd(order, p, 0, &mut c) {
            return self.refuse_local(DispatchError::Unroutable);
        }
        if !self.end.cmd.push(&c) {
            self.counters.ring_full += 1;
            return self.refuse(DispatchError::QueueFull);
        }
        self.track(&c, p, now_ns());
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
        self.rows[i].bits |= RB_REPLACING;
        self.track(&c, perp, now_ns());
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
        self.retired.pop()
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

    fn on_shutdown(&mut self) {
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
            // Nobody drains after the stop: fills are counted (the venue's
            // sheet holds them for the next boot), the rest dropped.
            self.counters.shutdown_fills += self.fills.clear() as u64;
            let _ = self.events.clear();
            let _ = self.retired.clear();
            let _ = self.row_timers(now);
            match self.cancel_all_state() {
                CancelAllState::Clear => {
                    let _ = self.drive_deadman(false, now);
                    if self.deadman.quiescent(now_ms()) {
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
                if self.cancel_all_state() == CancelAllState::Clear {
                    self.counters.shutdown_switch_held += 1;
                } else {
                    self.counters.shutdown_unconfirmed += 1;
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
    use crate::cmd::{CMD_MODIFY, CMD_PLACE, CMD_READ_ORDERS, CMD_READ_STATE, CMD_SCHEDULE_CANCEL};
    use crate::event::{EVF_CMD_SHIFT, EVF_RESTING, SLOT_FOREIGN};
    use crate::gateway::{hl_duplex, HlGatewayEnd};
    use exec_gateway::stats::SWEEP_WORKING;
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
        }
    }

    /// The gateway's end, scripted: commands pulled into a list the test
    /// picks from, events injected.
    struct Venue {
        gw: HlGatewayEnd,
        cmds: Vec<HlCmd>,
    }

    impl Venue {
        /// The first command of `kind` the arm pushed, taken.
        fn take(&mut self, kind: u8) -> Option<HlCmd> {
            while let Some(c) = self.gw.cmd.pop() {
                self.cmds.push(*c);
            }
            let i = self.cmds.iter().position(|c| c.kind == kind)?;
            Some(self.cmds.remove(i))
        }

        fn send(&mut self, e: ExecEvent) {
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
        let read = if c.kind == CMD_READ_ORDERS {
            READ_ORDERS
        } else {
            READ_STATE
        };
        ExecEvent {
            client_oid: c.client_oid,
            venue_ms,
            px_1e8: account_1e8,
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

    fn retired(arm: &mut HlGatewayArm) -> Vec<u64> {
        let mut out = Vec::new();
        while let Some(r) = arm.try_next_retired() {
            assert_eq!(r.slot, SLOT);
            out.push(r.client_oid);
        }
        out
    }

    /// Up to the first sheet: the dead man's boot clear answered, the
    /// snapshot, the sweep read clear; the sheet read issued.
    fn to_sheet(c: &ArmCfg) -> (HlGatewayArm, Venue, u64, HlCmd) {
        let (eng, gw) = hl_duplex();
        let mut arm = HlGatewayArm::new(c, eng).expect("arm");
        let mut v = Venue {
            gw,
            cmds: Vec::new(),
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

    /// Answer the sheet (account 1 000 USD at venue ms 1, flat) and the
    /// orders.
    fn booted_with(c: &ArmCfg) -> (HlGatewayArm, Venue, u64) {
        let (mut arm, mut v, t, st) = to_sheet(c);
        v.send(read_end(&st, 1_000 * E8, 1));
        let _ = arm.idle(t);
        let od = v.take(CMD_READ_ORDERS).expect("the orders after the sheet");
        v.send(read_end(&od, 0, 0));
        let _ = arm.idle(t);
        assert!(arm.ready());
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

    /// The periodic sheet then orders read at `at`, answered (the sheet
    /// with `pos`, the orders listing `listed`).
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
        assert!(
            retired(&mut arm).is_empty(),
            "a fill's end is no retirement"
        );
        assert_eq!(arm.open_orders(), 0);
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
        assert_eq!(retired(&mut arm), [1]);
        assert_eq!(arm.open_orders(), 1);
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
    fn a_restored_anchor_is_the_session_s_and_a_fresh_one_is_offered_for_keeping() {
        let mut c = cfg();
        c.anchor_1e6 = 900_000_000;
        let (arm, _v, t) = booted_with(&c);
        let sig = arm.signal(t);
        assert_eq!((sig.pnl_judged, sig.pnl_delta_usd_1e6), (1, 100_000_000));
        assert_eq!(arm.fresh_anchor_1e6(), None);
        let (arm, _v, t) = booted();
        assert_eq!(arm.fresh_anchor_1e6(), Some(1_000_000_000));
        assert_eq!(arm.signal(t).pnl_delta_usd_1e6, 0);
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
        assert_eq!(
            arm.fresh_anchor_1e6(),
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
    }
}
