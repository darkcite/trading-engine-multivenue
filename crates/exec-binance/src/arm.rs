// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **`BnArm` — the engine-thread half (plan §3.2).**
//!
//! `submit` only validates and pushes (target: under 200 ns):
//!
//! 1. **the row**, alias first ([`crate::inst::InstTable::row_of`]);
//! 2. **the refusals**, each counted by reason ([`Refusal`]): not bound,
//!    not live-tradable, owned by another slot (BX-7), a maker where no
//!    venue dead-man works (BX-17), the gateway not ready;
//! 3. **BX-5 quantization** by the row's magic reciprocals — a BUY price
//!    floors, a SELL price ceils, the quantity floors — then the minimum
//!    quantity and notional, the maximum quantity, and the percent-price
//!    band while a fresh mark exists (each judged by cross-multiplying in
//!    `i128`: no division on the submit path). **PB2 — a strict owner**
//!    ([`BnArm::set_strict_grid`], the Pine slot): an order the grid would
//!    move is refused ([`Refusal::OffGrid`]), never rounded; an on-grid
//!    order leaves exactly as before. **PB3** — a USDⓈ-M reduce-only PLACE
//!    of a strict owner is exempt from the minimum notional (`-4164`:
//!    "unless you choose reduce only") once the boot sets
//!    [`BnArm::set_ro_min_exempt`] (off by default: PB0.2 records the
//!    venue's answer first, OD-6); `minQty` still binds, and a modify is
//!    not exempt (`-5029` carries no such clause);
//! 4. **the governors** ([`crate::gov`]): the ORDERS windows (futures',
//!    spot's, options' or Binance Stocks' pool, by the row's product), the UM quantitative
//!    rules, the open orders per symbol and the breadth (futures' alone);
//!    **The writing law (BX9, BX-18)** rides this step: on an option row
//!    the account may not write ([`crate::inst::INST_WRITABLE`] clear) a
//!    SELL with no long of the owner's to reduce is refused
//!    ([`Refusal::NotWritable`]); any other SELL there leaves REDUCE-ONLY,
//!    so the venue itself refuses one that would open a short — the arm's
//!    view of the position is the gateway's booked one ([`EVT_POSITION`]),
//!    which lags orders in flight; the venue's flag does not.
//!    **The equity law (BX10)** rides it too: on a Binance Stocks row the
//!    side the venue takes (`tradability`, else [`Refusal::NotLive`]), a
//!    SELL only of a booked holding ([`Refusal::NotWritable`] — the router's
//!    spot law bounds its size), Stocks' own pool (200 / min per UID), and
//!    the mandatory TTL: every equity order is a DAY LIMIT whose deadline
//!    the arm stamps at `equity_ttl_ns` (none set: [`Refusal::Ttl`]) — an
//!    IoC by kind (no maker: no venue dead-man), resting until it fills or
//!    the gateway cancels it; its cancel verb is built, its modify not;
//! 5. **the push**: one [`BnCmd`] into the command ring (the kit's
//!    duplex, [`crate::cmd::duplex`]), stamped with its push time. A full
//!    ring is `QueueFull` and the router books nothing.
//!
//! A **modify** passes the same step 2 as a submit (it re-prices a resting
//! maker: the row live and the owner's, the dead-man working, the gateway
//! ready), then steps 3 and the ORDERS windows.
//!
//! The per-row tables are fixed arrays of [`INST_MAX`] indexed through a
//! mask: no bounds-check panic on the submit path.
//!
//! `on_idle` drains at most [`EVT_DRAIN_MAX`] [`BnEvt`]s and turns them into
//! the router's records and the halt observations. Two orderings are this
//! module's duty (risk-policy "BX3" obligations):
//!
//! * **fills before retirements (1).** The gateway pushes every fill of an
//!   order onto lane 4 BEFORE the event that retires it, and this arm hands
//!   a retirement to the router one idle poll AFTER it drained it: between
//!   two polls the engine's loop drains the fill lanes, so the fill is
//!   booked first. The residual: a lane-4 backlog deeper than the engine's
//!   per-pass drain delays a fill past that poll (the resting count and the
//!   exit test's working quantity are then early by that order, never the
//!   money ledgers).
//! * **confirm, then release (2).** A cancel's or modify's `Ok` means
//!   QUEUED ([`OrderDispatch::verbs_confirm_later`] is `true`): the router
//!   releases a cancelled row on the confirmed `RETIRED_CANCELED_MEMBER`
//!   and renames a modified one on the confirmed [`Renamed`].
//!
//! **`cancel_all_state` (3)** is `Clear` only when the sweep was confirmed
//! AND no order of ours is in flight — an IoC sent and not yet answered is
//! counted, so a halt's `clear_resting_slot` never drops an in-flight exit.
//!
//! **The halt's sweep is a word, not a command (BX7, XG-1):** `cancel_all`
//! bumps the stats block's sweep request — never refused, whatever the
//! command ring holds — and `cancel_all_state` reads the gateway's answer
//! in the same block (Working until the gateway has taken THIS request;
//! then Stranded or Clear). The halt word (XG-2, XG-5) rides the signal:
//! a gateway that lost an output, or whose deadline queue overflowed,
//! halts the slot ([`HaltSignal::with_gateway_halted`]).
//!
//! Error mapping: not bound, not live or not ours → `NoLiveRoute`; a verb,
//! kind or order flag this product does not take → `Unsupported`; a BX-5
//! refusal →
//! `Unsupported`; a governor → `SlotDisabled`; the gateway not ready →
//! `Disconnected`; a full ring → `QueueFull`.

use clob_dispatcher::{
    BinanceArmReport, CancelAllState, DispatchError, DispatchStats, HaltSignal, LiveArmCounters, OrderDispatch,
    Renamed, Retired, BINANCE_REFUSALS, RETIRED_EXPIRED, RETIRED_REJECTED,
};
use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_time::WallAnchor;
use core_types::{
    CancelReq, ChannelEvent, ChannelId, ModifyReq, Order, Side, SymbolId, VenueId, ORDER_FLAG_POST_ONLY,
    ORDER_FLAG_REDUCE_ONLY,
};

use exec_gateway::stats::{HALT_NONE, PUBLISHED_WORDS, SWEEP_STRANDED, SWEEP_WORKING};

use crate::cmd::*;
use crate::equity::{EQ_SIDE_BUY, EQ_SIDE_SELL};
use crate::gov::{
    classify, is_venue_code, CodeClass, EquityOrders, FuturesOrders, OptionOrders, Qtr, QtrRow, SpotOrders, ARM_WEIGHT_MAX_PER_MIN,
    OPT_ARM_WEIGHT_MAX_PER_MIN, QTR_DUST_USD_1E6, SPOT_ARM_WEIGHT_MAX_PER_MIN,
};
use crate::inst::{
    rx, InstTable, INST_INVERSE, INST_LIVE, INST_MAX, INST_NO_DEADMAN, INST_OWNED, INST_WRITABLE, PRODUCT_COINM, PRODUCT_EQUITY,
    PRODUCT_OPTIONS, PRODUCT_SPOT, PRODUCT_USDM, ROW_NONE,
};
use crate::margin::{MarginBook, SLOTS};
use crate::mode::AccountMode;
use crate::num::{ceil_to, floor_to};

/// Events drained per `on_idle`.
pub const EVT_DRAIN_MAX: usize = 64;
/// A mark older than this does not bound the band check.
pub const MARK_FRESH_NS: u64 = 10_000_000_000;
/// A gateway pulse older than this means the gateway is not ready.
pub const STATUS_FRESH_NS: u64 = 1_000_000_000;
/// The shutdown sweep's bound.
pub const SHUTDOWN_WAIT_NS: u64 = 3_000_000_000;
const RET_CAP: usize = 1_024;
const REN_CAP: usize = 256;

/// Why the arm refused an order locally. Counted per reason.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Refusal {
    /// No row: the instrument was never bound (BX-4).
    Unbound = 0,
    /// Bound but not live-tradable (§3.6), or closed by the venue this boot.
    NotLive = 1,
    /// Another slot owns the instrument (BX-7).
    NotOwner = 2,
    /// Neither a maker nor an IoC.
    Kind = 3,
    /// A maker where no venue dead-man works (BX-17).
    NoDeadman = 4,
    /// The gateway is not logged on, the clock unmeasured, or not reconciled.
    NotReady = 5,
    /// A non-positive price or quantity.
    Price = 6,
    /// Under `LOT_SIZE.minQty` after flooring.
    MinQty = 7,
    /// Under the minimum notional.
    MinNotional = 8,
    /// Over `LOT_SIZE.maxQty`.
    MaxQty = 9,
    /// Outside the percent-price band of a fresh mark.
    Band = 10,
    /// A maker TTL under the slot's `min_maker_ttl_ms` (BX-13).
    Ttl = 11,
    /// The ORDERS windows (§3.9).
    Orders = 12,
    /// The UM quantitative rules.
    Qtr = 13,
    /// `MAX_NUM_ORDERS` on the symbol.
    MaxOrders = 14,
    /// The slot's `max_symbols`.
    Breadth = 15,
    /// The command ring is full.
    QueueFull = 16,
    /// A cancel or modify on a product whose verbs are not built (§3.8).
    Verb = 17,
    /// A maker while the dead-man is not working NOW (the order session
    /// cannot cancel, a countdown is not current): transient, retryable.
    DeadmanDown = 18,
    /// Order flags this arm does not honour (an unknown bit), or that
    /// contradict the kind (a post-only IoC) — BX7, the operator's ruling.
    Flags = 19,
    /// BX9 (BX-18): a SELL on an option row this account may not write,
    /// with no long of the owner's to reduce — it could only open a short.
    NotWritable = 20,
    /// PB2 (I-PB2): a strict owner's price or quantity off the row's grid
    /// — refused, never rounded (the Pine slot read the same row).
    OffGrid = 21,
}

/// Refusal reasons.
pub const REFUSALS: usize = 22;
/// The `/state` words, by [`Refusal`].
pub const REFUSAL_WORDS: [&str; REFUSALS] = [
    "unbound",
    "not_live",
    "not_owner",
    "kind",
    "no_deadman",
    "not_ready",
    "price",
    "min_qty",
    "min_notional",
    "max_qty",
    "band",
    "ttl",
    "orders",
    "qtr",
    "max_orders",
    "breadth",
    "queue_full",
    "verb",
    "deadman_down",
    "flags",
    "not_writable",
    "off_grid",
];

/// The arm's counters (`/state` `exec.arms.binance`).
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct BnArmCounters {
    /// Orders pushed to the gateway.
    pub submitted: u64,
    /// Venue ACKs.
    pub acked: u64,
    /// Venue rejections.
    pub rejected: u64,
    /// IoCs that ended with nothing filled.
    pub ioc_missed: u64,
    /// Cancels pushed.
    pub cancels_sent: u64,
    /// Cancels the venue refused.
    pub cancels_refused: u64,
    /// Modifies pushed.
    pub modifies_sent: u64,
    /// Modifies the venue refused.
    pub modifies_refused: u64,
    /// Modifies the venue confirmed.
    pub modified: u64,
    /// Retirements the local ring could not hold (the row stays counted).
    pub retired_dropped: u64,
    /// Confirmed modifies the local ring could not hold.
    pub renamed_dropped: u64,
    /// Budget observations (`-1015`, 429, `-1003`).
    pub budget_obs: u64,
    /// Venue locks.
    pub locks: u64,
    /// Frames that did not scan (BX-15).
    pub scan_failed: u64,
    /// Events of no known kind.
    pub evt_unknown: u64,
    /// Shutdown verbs the command ring could not take (the sweep itself is
    /// the stats word and never refused; the gateway then leaves its loop
    /// on the stop flag's grace instead).
    pub cancel_all_unqueued: u64,
    /// Sweeps that ended stranded (BX11: `LiveArmCounters::sweep_stalled`).
    pub sweeps_stranded: u64,
    /// Local refusals, by [`Refusal`].
    pub refused: [u64; REFUSALS],
    /// The gateway's tallies (fills booked, foreign, unowned, dropped,
    /// unresolved; recon ok / failed / drift legs / unseen legs; stream
    /// reconnects, connect failures; sweep orders left).
    pub gw: [u64; GW_TALLIES],
}

// BX11: the refusals by reason cross the dispatch boundary as an array of
// the dispatcher's length.
const _: () = assert!(REFUSALS == BINANCE_REFUSALS);

/// The gateway tallies mirrored in [`BnArmCounters::gw`]: the gateway
/// publishes tally `i` as the stats block's word `i` (BX7) and the arm
/// copies them on each pulse ([`EVT_STATUS`]).
pub const GW_TALLIES: usize = 21;
// The tallies' words, then the gateway's weight word (`PW_WEIGHT`).
const _: () = assert!(GW_TALLIES < PUBLISHED_WORDS);
/// [`BnArmCounters::gw`] index: fills booked onto lane 4.
pub const GW_FILLS_BOOKED: usize = 0;
/// Fills on an owned instrument not ours (BX-8).
pub const GW_FILLS_FOREIGN: usize = 1;
/// Fills on an instrument not bound.
pub const GW_FILLS_UNOWNED: usize = 2;
/// Fills lane 4 could not take at once (held, then pushed).
pub const GW_FILLS_DEFERRED: usize = 3;
/// Fills whose order the table did not know.
pub const GW_FILLS_UNRESOLVED: usize = 4;
/// Reconciliations that agreed.
pub const GW_RECON_OK: usize = 5;
/// Reconciliations that failed to read.
pub const GW_RECON_FAILED: usize = 6;
/// Legs in drift.
pub const GW_RECON_DRIFT_LEGS: usize = 7;
/// Venue legs never booked.
pub const GW_RECON_UNSEEN_LEGS: usize = 8;
/// Stream reconnects.
pub const GW_RECONNECTS: usize = 9;
/// Stream connect failures.
pub const GW_CONNECT_FAILURES: usize = 10;
/// Orders a sweep left.
pub const GW_SWEEP_LEFT: usize = 11;
/// Fills neither lane 4 nor the held queue could take (lost; the pulse
/// raises unbounded drift).
pub const GW_FILLS_LOST: usize = 12;
/// REST jobs a full queue refused.
pub const GW_JOBS_DROPPED: usize = 13;
/// Working orders put in doubt by a stream reopen or an open-order
/// listing that no longer names them (BX-11).
pub const GW_IN_DOUBT_RAISED: usize = 14;
/// Journal records a full ring dropped.
pub const GW_JOURNAL_DROPPED: usize = 15;
/// Cancels owed (not sendable, lost or refused) and retried.
pub const GW_CANCELS_OWED: usize = 16;
/// Quiet periods begun (a budget or lock answer).
pub const GW_QUIET: usize = 17;
/// Hosts resolved again after a dial-failure streak, the new address
/// handed to their connections (BX7, [`crate::dns`]).
pub const GW_DNS_RERESOLVED: usize = 18;
/// User-stream events of a kind the arm does not act on
/// ([`crate::userstream::UE_OTHER`]: `STRATEGY_UPDATE`, `GRID_UPDATE`, a
/// kind newer than this build) — scanned, counted, never booked (BX11:
/// BX-15's "never a silent skip").
pub const GW_EVENTS_OTHER: usize = 19;
/// **BX13 (R3-1)** — reconciliation cycles held inconclusive: a leg whose
/// order's trades were still being read (`Verdict::held_legs`) — neither
/// drift nor reconciled; journaled with [`crate::journal::J_RECON_HELD`].
pub const GW_RECON_HELD: usize = 20;

/// The knobs the arm is booted with.
#[derive(Clone, Debug)]
pub struct ArmKnobs {
    /// `orders_frac_1e6`.
    pub orders_frac_1e6: i64,
    /// `qtr_frac_1e6`.
    pub qtr_frac_1e6: i64,
    /// The one live Binance slot (BX-7 as built: it owns every row).
    pub owner_slot: u8,
    /// Its `max_symbols`.
    pub max_symbols: u32,
    /// Its `min_maker_ttl_ms`, in ns.
    pub min_maker_ttl_ns: u64,
    /// The O-BX18 side table.
    pub margin: MarginBook,
    /// The boot's wall anchor (the QTR cycle is wall time).
    pub anchor: WallAnchor,
    /// The account mode the gateway was booted in (BX11: the report).
    pub mode: AccountMode,
    /// Demo Mode (the battery; the engine trades mainnet only, BX-16).
    pub demo: bool,
    /// BX10 — Binance Stocks' mandatory gateway TTL, ns (BX-17's exception:
    /// every equity order is a DAY LIMIT the gateway cancels at this
    /// deadline). 0: none — an equity order is refused ([`Refusal::Ttl`]).
    pub equity_ttl_ns: u64,
}

/// A fixed FIFO on the engine thread. `mark` lets a producer hold back
/// what it pushed since the last [`Fifo::release`].
struct Fifo<T: Copy + Default, const N: usize> {
    buf: Box<[T; N]>,
    head: usize,
    tail: usize,
    mark: usize,
}

impl<T: Copy + Default, const N: usize> Fifo<T, N> {
    fn new() -> Self {
        Self {
            buf: Box::new([T::default(); N]),
            head: 0,
            tail: 0,
            mark: 0,
        }
    }

    #[inline(always)]
    fn push(&mut self, v: T) -> bool {
        if self.tail - self.head == N {
            return false;
        }
        self.buf[self.tail % N] = v;
        self.tail += 1;
        true
    }

    /// Everything pushed so far becomes poppable.
    #[inline(always)]
    fn release(&mut self) {
        self.mark = self.tail;
    }

    /// Pop a released entry.
    #[inline(always)]
    fn pop_released(&mut self) -> Option<T> {
        if self.head == self.mark {
            return None;
        }
        let v = self.buf[self.head % N];
        self.head += 1;
        Some(v)
    }

    /// Pop any entry.
    #[inline(always)]
    fn pop(&mut self) -> Option<T> {
        self.mark = self.tail;
        self.pop_released()
    }
}

/// A row's runtime on the engine thread.
#[repr(C, align(32))]
#[derive(Copy, Clone, Debug, Default)]
struct RowRt {
    mark_1e6: i64,
    mark_ns: u64,
    pos_1e6: i64,
    open: u32,
    _r: u32,
}

/// The gateway's latest pulse and verdicts.
#[derive(Copy, Clone, Debug, Default)]
struct Seen {
    status_ns: u64,
    gap_ns: u64,
    status_flags: u8,
    /// The last pulse's clock offset (ms) and orders in doubt (BX11).
    clock_offset_ms: i64,
    in_doubt: u64,
    reconciled: bool,
    reconciled_once: bool,
    recon_ok_ns: u64,
    drift_1e6: i64,
    budget_until_ns: u64,
    venue_lock: bool,
    scan_failed: bool,
    equity_1e6: i64,
    anchor_1e6: i64,
    day: u64,
    day_read: bool,
}

/// **The arm** (module docs).
pub struct BnArm {
    /// The engine's end of the duplex: commands out, events in, the stats
    /// block (the sweep and halt words, the published tallies).
    end: BnEngineEnd,
    /// The sweep request this arm last made (0: none yet) — the gateway's
    /// answer counts only once its sequence reaches it.
    sweep_asked: u64,
    inst: InstTable,
    rt: Box<[RowRt; INST_MAX]>,
    qtr_rows: Box<[QtrRow; INST_MAX]>,
    fut: FuturesOrders,
    /// Spot's ORDERS pool and its unfilled-order mirror (BX8).
    spot: SpotOrders,
    /// Options' ORDERS pool (BX9).
    opt: OptionOrders,
    /// Binance Stocks' ORDERS pool (BX10).
    eq: EquityOrders,
    qtr: Qtr,
    knobs: ArmKnobs,
    /// PB2: refuse, never round (the Pine slot's law); false keeps BX-5.
    strict_grid: bool,
    /// PB3: a strict owner's USDⓈ-M reduce-only place skips the minimum
    /// notional; false (the default) keeps the refusal for every owner.
    ro_min_exempt: bool,
    margin: MarginBook,
    /// Rows the owner slot holds a position or open orders in.
    active_rows: u32,
    /// Rows with open orders (the QTR `N`).
    rows_open: u32,
    /// Orders pushed and not yet terminal (obligation 3).
    in_flight: u32,
    reject_streak: u32,
    asset_refusal_streak: u32,
    day_bought_1e6: [i64; SLOTS],
    seen: Seen,
    retired: Fifo<Retired, RET_CAP>,
    renamed: Fifo<Renamed, REN_CAP>,
    c: BnArmCounters,
    /// BX11: the gateway's latency quantiles the report carries
    /// ([`LAT_QUANTILES`]), refreshed one a report, round robin.
    lat: [core::cell::Cell<u64>; LAT_QUANTILES],
    /// The next quantile the report refreshes.
    lat_next: core::cell::Cell<u8>,
}

/// The report's latency quantiles: the WS API's p50 and p99, the cancel's,
/// the fill's lag, the REST route's and the push-to-write's p99 — each a
/// 2 048-bucket walk (`Lat::quantile_ns`, "a /metrics read, never a
/// loop's"): one walk per report, so the engine thread pays about 1 µs a
/// second and the six refresh within six reports.
const LAT_QUANTILES: usize = 6;

/// The order flags this arm honours (BX7, the operator's ruling):
/// reduce-only on any kind (`reduceOnly=true`); post-only on a maker
/// (whose `GTX` already is post-only). An unknown bit, or a post-only IoC
/// (a contradiction), is refused — and a reduce-only on spot (BX8: the
/// venue has no such order; a spot sell is bounded by the holding, the
/// router's law).
#[inline(always)]
const fn admit_flags(flags: u8, maker: bool, spot: bool) -> Result<(), Refusal> {
    let unknown = flags & !(ORDER_FLAG_REDUCE_ONLY | ORDER_FLAG_POST_ONLY) != 0;
    let post_only_ioc = !maker && flags & ORDER_FLAG_POST_ONLY != 0;
    let spot_reduce = spot && flags & ORDER_FLAG_REDUCE_ONLY != 0;
    if unknown || post_only_ioc || spot_reduce {
        Err(Refusal::Flags)
    } else {
        Ok(())
    }
}

/// The futures products (the UM quantitative rules, the breadth and the
/// futures ORDERS pool are theirs alone).
#[inline(always)]
const fn is_futures(product: u8) -> bool {
    product == PRODUCT_USDM || product == PRODUCT_COINM
}

/// **BX9 (BX-18) — the writing law** (module docs, step 4): the flags the
/// order leaves with. On an option row not writable a SELL with no long to
/// reduce (`pos_1e6` ≤ 0, the gateway's booked position) is refused; any
/// other SELL there is sent reduce-only. Every other order keeps its flags.
#[inline(always)]
const fn writing_law(product: u8, row_flags: u8, side: Side, flags: u8, pos_1e6: i64) -> Result<u8, Refusal> {
    if product != PRODUCT_OPTIONS || row_flags & INST_WRITABLE != 0 || !matches!(side, Side::Ask) {
        return Ok(flags);
    }
    if pos_1e6 <= 0 {
        return Err(Refusal::NotWritable);
    }
    Ok(flags | ORDER_FLAG_REDUCE_ONLY)
}

/// **BX10 — the equity law** (module docs, step 4): the side the venue
/// takes on the ticker (`tradability`, `sides`: else [`Refusal::NotLive`]
/// — closed to that side), a SELL only of a holding (`pos_1e6`, the
/// gateway's booked position: none, and the SELL could only short —
/// [`Refusal::NotWritable`]; the router's spot law bounds its size), and
/// the mandatory TTL set (`ttl_ns`: else [`Refusal::Ttl`] — an equity
/// order rests until it is cancelled, BX-17's exception).
#[inline(always)]
const fn equity_law(sides: u8, buy: bool, pos_1e6: i64, ttl_ns: u64) -> Result<(), Refusal> {
    let side = if buy { EQ_SIDE_BUY } else { EQ_SIDE_SELL };
    if sides & side == 0 {
        return Err(Refusal::NotLive);
    }
    if !buy && pos_1e6 <= 0 {
        return Err(Refusal::NotWritable);
    }
    if ttl_ns == 0 {
        return Err(Refusal::Ttl);
    }
    Ok(())
}

/// `⌊a · b / 1e6⌋ < m` for `a, b ≥ 0`, without the division:
/// `⌊x / d⌋ < m ⇔ x < m · d`.
#[inline(always)]
const fn scaled_lt(a: i64, b: i64, m: i64) -> bool {
    (a as i128) * (b as i128) < (m as i128) * 1_000_000
}

impl BnArm {
    /// The arm over the engine's end of the duplex and its boot tables.
    #[must_use]
    pub fn new(end: BnEngineEnd, inst: InstTable, knobs: ArmKnobs) -> Self {
        let margin = knobs.margin.clone();
        Self {
            end,
            sweep_asked: 0,
            rt: Box::new([RowRt::default(); INST_MAX]),
            qtr_rows: Box::new([QtrRow::default(); INST_MAX]),
            inst,
            fut: FuturesOrders::new(knobs.orders_frac_1e6),
            spot: SpotOrders::new(knobs.orders_frac_1e6),
            opt: OptionOrders::new(knobs.orders_frac_1e6),
            eq: EquityOrders::new(knobs.orders_frac_1e6),
            qtr: Qtr {
                frac_1e6: knobs.qtr_frac_1e6,
            },
            margin,
            knobs,
            strict_grid: false,
            ro_min_exempt: false,
            active_rows: 0,
            rows_open: 0,
            in_flight: 0,
            reject_streak: 0,
            asset_refusal_streak: 0,
            day_bought_1e6: [0; SLOTS],
            seen: Seen::default(),
            retired: Fifo::new(),
            renamed: Fifo::new(),
            c: BnArmCounters::default(),
            lat: [const { core::cell::Cell::new(0) }; LAT_QUANTILES],
            lat_next: core::cell::Cell::new(0),
        }
    }

    /// **PB2 — the strict grid** (boot-only, before the first order): on,
    /// an order whose price or quantity the row's grid would move is
    /// refused ([`Refusal::OffGrid`]) instead of quantized (BX-5); off (the
    /// default: the batteries, every non-Pine owner), BX-5 unchanged.
    // pin:cli-pb2-strict-setter — the plan names `ArmKnobs::strict_grid`;
    // a new `ArmKnobs` field breaks the batteries' and the bench's struct
    // literals (files outside this task), so the knob is a boot-time setter
    // with the same default (false = BX-5).
    pub fn set_strict_grid(&mut self, on: bool) {
        self.strict_grid = on;
    }

    /// PB2: whether the grid is strict.
    #[must_use]
    pub const fn strict_grid(&self) -> bool {
        self.strict_grid
    }

    /// **PB3 — the reduce-only minimum exemption** (boot-only, before the
    /// first order; OD-6 "once confirmed"): on, a USDⓈ-M reduce-only PLACE
    /// of a STRICT owner ([`Self::set_strict_grid`]) is sent below the
    /// minimum notional (`-4164`: "unless you choose reduce only"); off (the
    /// default, every owner until PB0.2 is recorded), the minimum binds as
    /// BX-5 states it. r28 risk review B1: the exemption never reaches a
    /// non-Pine owner, and never before the venue's answer is on record.
    pub fn set_ro_min_exempt(&mut self, on: bool) {
        self.ro_min_exempt = on;
    }

    /// PB3: whether the reduce-only minimum exemption is on.
    #[must_use]
    pub const fn ro_min_exempt(&self) -> bool {
        self.ro_min_exempt
    }

    /// The counters.
    #[must_use]
    pub const fn counters(&self) -> &BnArmCounters {
        &self.c
    }

    /// The instrument table (the boot binds the ledger from it).
    #[must_use]
    pub const fn inst(&self) -> &InstTable {
        &self.inst
    }

    /// Orders pushed and not yet terminal.
    #[must_use]
    pub const fn in_flight(&self) -> u32 {
        self.in_flight
    }

    /// The gateway's shared block (cold: `/state`, `/metrics`, tests) — its
    /// halt, sweep and congestion words, its gauges, its published words
    /// ([`crate::gateway::PW_WEIGHT`]) and its latencies.
    #[must_use]
    pub fn gateway_stats(&self) -> &exec_gateway::GatewayStats {
        &self.end.stats
    }

    /// The E7 session anchor is not persisted yet (the gateway's last
    /// pulse, `EVT_F_ANCHOR_UNSAVED`; S8, F2): a restart now would
    /// re-anchor. Cold: the operator's arming check and `/state` (BX11).
    #[must_use]
    pub const fn anchor_unsaved(&self) -> bool {
        self.seen.status_flags & EVT_F_ANCHOR_UNSAVED != 0
    }

    #[inline(always)]
    fn refuse(&mut self, r: Refusal) {
        self.c.refused[r as usize] += 1;
    }

    /// The gateway is logged on, its clock measured, its pulse fresh, and
    /// the account reconciled at least once.
    #[inline(always)]
    fn ready(&self, now_ns: u64) -> bool {
        let need = EVT_F_ORDER_UP | EVT_F_USER_UP | EVT_F_CLOCK_OK;
        self.seen.status_flags & need == need
            && now_ns.saturating_sub(self.seen.status_ns) < STATUS_FRESH_NS
            && self.seen.reconciled_once
    }

    #[inline(always)]
    fn active(&self, row: u16) -> bool {
        let r = &self.rt[rx(row)];
        r.open > 0 || r.pos_1e6 != 0
    }

    /// Open-order count change on `row`, keeping the breadth and QTR `N`
    /// — futures rows' alone (BX8: neither is a spot rule; BX9: nor an
    /// options one).
    #[inline(always)]
    fn open_delta(&mut self, row: u16, up: bool) {
        let futures = is_futures(self.inst.hot(row).product);
        let was_active = self.active(row);
        let r = &mut self.rt[rx(row)];
        let was_open = r.open > 0;
        if up {
            r.open += 1;
        } else {
            debug_assert!(r.open > 0, "an order closed twice");
            r.open = r.open.saturating_sub(1);
        }
        let is_open = r.open > 0;
        let is_active = self.active(row);
        if futures {
            self.rows_open = (self.rows_open + is_open as u32).saturating_sub(was_open as u32);
            self.active_rows = (self.active_rows + is_active as u32).saturating_sub(was_active as u32);
        }
    }

    /// Quantize and check the venue law for an order on `row` (BX-5).
    /// Returns (price, quantity, dust: the notional under the QTR's dust
    /// line). Every notional and band test cross-multiplies in `i128`.
    /// PB2: a strict arm refuses what the grid would move. PB3: `min_exempt`
    /// (a USDⓈ-M reduce-only place) skips the minimum notional alone.
    #[inline(always)]
    fn quantize(&mut self, row: u16, buy: bool, px: i64, qty: i64, min_exempt: bool, now_ns: u64) -> Result<(i64, i64, bool), Refusal> {
        let h = self.inst.hot(row);
        let tick = h.tick_1e6 as i64;
        let step = h.step_1e6 as i64;
        if px <= 0 || qty <= 0 || px > i64::MAX - tick {
            return Err(Refusal::Price);
        }
        let px_q = if buy {
            floor_to(px, tick, h.tick_magic())
        } else {
            ceil_to(px, tick, h.tick_magic())
        };
        let qty_q = floor_to(qty, step, h.step_magic());
        // PB2 (I-PB2): refuse, never round — judged on the order as sent,
        // before any minimum (an off-grid order is a defect, not a size).
        if self.strict_grid && (px_q != px || qty_q != qty) {
            return Err(Refusal::OffGrid);
        }
        if px_q <= 0 {
            return Err(Refusal::Price);
        }
        if qty_q <= 0 || qty_q < h.min_qty_1e6 {
            return Err(Refusal::MinQty);
        }
        // The notional is qty × unit (COIN-M) or px × qty, ×1e6 each.
        let (na, nb) = if h.has(INST_INVERSE) { (qty_q, h.unit_1e6) } else { (px_q, qty_q) };
        if !min_exempt && scaled_lt(na, nb, h.min_notional_1e6) {
            return Err(Refusal::MinNotional);
        }
        let dust = scaled_lt(na, nb, QTR_DUST_USD_1E6);
        let c = self.inst.cold(row);
        if c.max_qty_1e6 > 0 && qty_q > c.max_qty_1e6 {
            return Err(Refusal::MaxQty);
        }
        let rt = &self.rt[rx(row)];
        if rt.mark_1e6 > 0 && now_ns.saturating_sub(rt.mark_ns) < MARK_FRESH_NS {
            let (up, down) = if buy {
                (c.bid_up_1e6, c.bid_down_1e6)
            } else {
                (c.ask_up_1e6, c.ask_down_1e6)
            };
            // Above ⌊mark · up / 1e6⌋ ⇔ mark · up < px · 1e6; below
            // ⌊mark · down / 1e6⌋ ⇔ (px + 1) · 1e6 ≤ mark · down.
            let mark = rt.mark_1e6 as i128;
            let above = up != 0 && mark * (up as i128) < (px_q as i128) * 1_000_000;
            let below = down != 0 && (px_q as i128 + 1) * 1_000_000 <= mark * (down as i128);
            if above || below {
                return Err(Refusal::Band);
            }
        }
        Ok((px_q, qty_q, dust))
    }

    /// The checks every order verb passes before the venue law (module
    /// docs, step 2): bound, live, the owner's, a kind the row takes, the
    /// dead-man for a maker, a ready gateway. The row.
    #[inline(always)]
    fn admit(&self, sym: SymbolId, strategy_id: u8, kind: u8, now: u64) -> Result<u16, Refusal> {
        let row = self.inst.row_of(sym);
        if row == ROW_NONE {
            return Err(Refusal::Unbound);
        }
        let h = self.inst.hot(row);
        if !h.has(INST_LIVE) {
            return Err(Refusal::NotLive);
        }
        if strategy_id != self.knobs.owner_slot {
            return Err(Refusal::NotOwner);
        }
        let maker = kind == ORDER_KIND_MAKER;
        if !maker && kind != ORDER_KIND_IOC {
            return Err(Refusal::Kind);
        }
        if maker && h.has(INST_NO_DEADMAN) {
            return Err(Refusal::NoDeadman);
        }
        if maker && self.seen.status_flags & EVT_F_DEADMAN_OK == 0 {
            return Err(Refusal::DeadmanDown);
        }
        if !self.ready(now) {
            return Err(Refusal::NotReady);
        }
        Ok(row)
    }

    /// Count a refusal and map it to the router's error (module docs).
    #[inline]
    fn refused(&mut self, r: Refusal) -> DispatchError {
        self.refuse(r);
        match r {
            Refusal::Unbound | Refusal::NotLive => {
                self.asset_refusal_streak += 1;
                DispatchError::NoLiveRoute
            }
            Refusal::NotOwner => DispatchError::NoLiveRoute,
            Refusal::Kind | Refusal::NoDeadman | Refusal::Verb | Refusal::Flags | Refusal::NotWritable => DispatchError::Unsupported,
            Refusal::NotReady | Refusal::DeadmanDown => DispatchError::Disconnected,
            Refusal::Price | Refusal::MinQty | Refusal::MinNotional | Refusal::MaxQty | Refusal::Band | Refusal::Ttl | Refusal::OffGrid => {
                DispatchError::Unsupported
            }
            Refusal::Orders | Refusal::Qtr | Refusal::MaxOrders | Refusal::Breadth => DispatchError::SlotDisabled,
            Refusal::QueueFull => DispatchError::QueueFull,
        }
    }

    #[inline(always)]
    fn submit_inner(&mut self, o: &Order) -> Result<(), Refusal> {
        let now = core_time::now_ns();
        let row = self.admit(o.sym, o.strategy_id, o.kind, now)?;
        let maker = o.kind == ORDER_KIND_MAKER;
        let product = self.inst.hot(row).product;
        let spot = product == PRODUCT_SPOT;
        let equity = product == PRODUCT_EQUITY;
        // A cash holding takes no reduce-only (spot, BX8; Binance Stocks,
        // BX10): a SELL is bounded by the holding instead.
        admit_flags(o.flags, maker, spot || equity)?;
        // BX9 (BX-18) — the writing law (module docs, step 4).
        let flags = writing_law(product, self.inst.hot(row).flags, o.side, o.flags, self.rt[rx(row)].pos_1e6)?;
        if maker && o.ttl_ns != 0 && o.ttl_ns < self.knobs.min_maker_ttl_ns {
            return Err(Refusal::Ttl);
        }
        let buy = o.side == Side::Bid;
        // BX10 — the equity law (module docs, step 4).
        if equity {
            equity_law(self.inst.cold(row).eq_sides, buy, self.rt[rx(row)].pos_1e6, self.knobs.equity_ttl_ns)?;
        }
        // PB3 (OD-6; Binance USDⓈ-M `-4164` MIN_NOTIONAL: "Order's notional
        // must be no smaller than %s (unless you choose reduce only)"): a
        // strict owner's USDⓈ-M place carrying reduce-only (validated above)
        // is exempt from the minimum notional while the boot set the knob
        // (B1: off until PB0.2 is recorded); its `minQty` still binds.
        let min_exempt = self.ro_min_exempt
            && self.strict_grid
            && product == PRODUCT_USDM
            && flags & ORDER_FLAG_REDUCE_ONLY != 0;
        let (px, qty, dust) = self.quantize(row, buy, o.px.raw(), o.qty.raw(), min_exempt, now)?;
        // ---- governors ----------------------------------------------------
        let r = rx(row);
        let wall_ms = self.knobs.anchor.wall_of(now) / 1_000_000;
        if spot {
            // BX8: spot's own pool and its unfilled-order mirror — the UM
            // quantitative rules and the breadth are futures' alone. BX13
            // (R2-2): nothing before the venue's own count is read (a
            // restart's 0 is not the venue's); a SELL to its reserve.
            if !self.spot.seeded() {
                return Err(Refusal::NotReady);
            }
            self.spot.roll(wall_ms);
            if !self.spot.admits(now, !buy) {
                return Err(Refusal::Orders);
            }
        } else if product == PRODUCT_OPTIONS {
            // BX9: options' own pool; no quantitative rule, no breadth.
            if !self.opt.admits(now) {
                return Err(Refusal::Orders);
            }
        } else if equity {
            // BX10: Stocks' own pool (200 / min per UID).
            if !self.eq.admits(now) {
                return Err(Refusal::Orders);
            }
        } else {
            if !self.fut.admits(now) {
                return Err(Refusal::Orders);
            }
            Qtr::roll(&mut self.qtr_rows[r], wall_ms);
            let n = self.rows_open + (self.rt[r].open == 0) as u32;
            if !self.qtr.admits(&self.qtr_rows[r], n, maker, dust) {
                return Err(Refusal::Qtr);
            }
        }
        let max_orders = self.inst.cold(row).max_orders as i64;
        if max_orders > 0 && (self.rt[r].open as i64 + 1) * 1_000_000 > max_orders * self.knobs.orders_frac_1e6 {
            return Err(Refusal::MaxOrders);
        }
        if is_futures(product) && !self.active(row) && self.active_rows >= self.knobs.max_symbols {
            return Err(Refusal::Breadth);
        }
        // ---- push -----------------------------------------------------------
        // BX10: every equity order rests (DAY, no IoC exists) under the
        // mandatory TTL — the gateway cancels it at the deadline (BX-17's
        // exception).
        let deadline = if equity {
            now + self.knobs.equity_ttl_ns
        } else if maker && o.ttl_ns != 0 {
            now + o.ttl_ns
        } else {
            0
        };
        let cmd = BnCmd::place(o.client_oid, px, qty, deadline, row, o.kind, o.side, o.strategy_id, flags).at(now);
        if !self.end.cmd.push(&cmd) {
            return Err(Refusal::QueueFull);
        }
        if spot {
            self.spot.commit(now);
        } else if product == PRODUCT_OPTIONS {
            self.opt.commit(now);
        } else if equity {
            self.eq.commit(now);
        } else {
            self.fut.commit(now);
            Qtr::on_place(&mut self.qtr_rows[r], maker, dust);
        }
        self.open_delta(row, true);
        self.in_flight += 1;
        self.c.submitted += 1;
        Ok(())
    }

    /// Queue a retirement for the router (held one poll, obligation 1).
    #[inline(always)]
    fn retire(&mut self, client_oid: u64, slot: u8, why: u8) {
        if !self.retired.push(Retired::new(client_oid, slot, why)) {
            self.c.retired_dropped += 1;
        }
    }

    /// An order of ours ended (REJECT or RETIRED).
    #[inline(always)]
    fn ended(&mut self, e: &BnEvt, why: u8, filled: bool) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if (e.row as usize) < self.inst.len() {
            self.open_delta(e.row, false);
            let ioc = self.seen_kind_ioc(e);
            let product = self.inst.hot(e.row).product;
            if product == PRODUCT_SPOT {
                // BX8: the venue takes an order that filled off the day's
                // unfilled count.
                if filled {
                    self.spot.filled(self.knobs.anchor.wall_of(e.ts_ns) / 1_000_000);
                }
            } else if is_futures(product) {
                let icr = e.flags & EVT_F_ICR != 0;
                Qtr::on_end(&mut self.qtr_rows[rx(e.row)], filled, ioc, icr);
            }
            if ioc && !filled {
                self.c.ioc_missed += 1;
            }
        }
        self.retire(e.client_oid, e.slot, why);
    }

    #[inline(always)]
    fn seen_kind_ioc(&self, e: &BnEvt) -> bool {
        e.flags & EVT_F_IOC != 0
    }

    /// One gateway event.
    fn apply(&mut self, e: &BnEvt) {
        match e.kind {
            EVT_ACK => {
                self.c.acked += 1;
                self.reject_streak = 0;
            }
            EVT_REJECT => {
                self.c.rejected += 1;
                let class = classify(e.code, 0);
                let why = match class {
                    CodeClass::WouldTake => RETIRED_EXPIRED,
                    _ => RETIRED_REJECTED,
                };
                // Only the VENUE's refusals (negative codes; Binance
                // Stocks' six-digit positives, BX10 — `is_venue_code`) move
                // the streak; the gateway's local ones (9 0xx) are races
                // the arm's own readiness check already counts.
                match class {
                    CodeClass::WouldTake | CodeClass::Budget | CodeClass::Lock => {}
                    CodeClass::RowFatal => {
                        self.inst.retire_row(e.row);
                        self.reject_streak += 1;
                    }
                    CodeClass::WriteFatal => {
                        // BX9 (BX-18): no writing on the underlying, for
                        // every row of it; its longs still trade.
                        if (e.row as usize) < self.inst.len() {
                            let uly = self.inst.hot(e.row).uly;
                            self.inst.unwrite_underlying(uly);
                        }
                        self.reject_streak += 1;
                    }
                    CodeClass::Reject | CodeClass::Clock | CodeClass::Unknown => {
                        self.reject_streak += is_venue_code(e.code) as u32;
                    }
                }
                self.ended(e, why, false);
            }
            EVT_RETIRED => {
                let filled = e.flags & EVT_F_HAD_FILL != 0;
                self.reject_streak = 0;
                self.ended(e, e.why, filled);
            }
            EVT_CANCEL_FAILED => {
                self.c.cancels_refused += 1;
            }
            EVT_MODIFIED => {
                self.c.modified += 1;
                let sym = if (e.row as usize) < self.inst.len() {
                    self.inst.hot(e.row).sym
                } else {
                    core_types::SYMBOL_ID_NONE
                };
                let r = Renamed::new(e.a as u64, e.client_oid, e.b, sym, e.slot, e.side == Side::Bid as u8);
                if !self.renamed.push(r) {
                    self.c.renamed_dropped += 1;
                }
            }
            EVT_MODIFY_FAILED => {
                self.c.modifies_refused += 1;
                if is_venue_code(e.code) && classify(e.code, 0) == CodeClass::Reject {
                    self.reject_streak += 1;
                }
            }
            EVT_STATUS => {
                self.seen.status_ns = e.ts_ns;
                self.seen.gap_ns = e.a as u64;
                self.seen.status_flags = e.flags;
                self.seen.clock_offset_ms = e.b;
                self.seen.in_doubt = e.c as u64;
                // The tallies the gateway published just before this pulse.
                let s = &*self.end.stats;
                let mut i = 0;
                while i < GW_TALLIES {
                    self.c.gw[i] = s.published(i);
                    i += 1;
                }
                // An output the rings could not take (a fill, or the event
                // that ends an order): the book may be wrong — unbounded
                // drift until the operator restarts (BX-15's rule).
                if e.flags & EVT_F_LOST != 0 {
                    self.seen.scan_failed = true;
                }
            }
            EVT_RECON => {
                let ok = e.flags & EVT_F_RECONCILED != 0;
                self.seen.reconciled = ok;
                self.seen.drift_1e6 = e.a;
                if ok {
                    self.seen.reconciled_once = true;
                    self.seen.recon_ok_ns = e.ts_ns;
                }
            }
            EVT_MARGIN => {
                self.margin.sample(e.product, e.a);
                if e.flags & EVT_F_MARGIN_CALL != 0 {
                    self.margin.margin_call();
                }
                self.seen.equity_1e6 = e.b;
                self.seen.anchor_1e6 = e.c;
            }
            EVT_BUDGET => {
                self.c.budget_obs += 1;
                self.seen.budget_until_ns = self.seen.budget_until_ns.max(e.a as u64);
            }
            EVT_LOCK => {
                self.c.locks += 1;
                self.seen.venue_lock = true;
            }
            EVT_SWEEP => {
                // The outcome is the sweep word (`cancel_all_state`); the
                // report carries the count at once.
                self.c.gw[GW_SWEEP_LEFT] = e.a as u64;
                self.c.sweeps_stranded += u64::from(e.flags & EVT_F_SWEEP_STRANDED != 0);
            }
            EVT_DAY => {
                // A router slot (`SLOTS` = `EXEC_SLOTS`, 16 since S16-4).
                if (e.slot as usize) < SLOTS {
                    self.day_bought_1e6[e.slot as usize] = e.b;
                    self.seen.day = e.a as u64;
                    self.seen.day_read = true;
                }
            }
            EVT_SCAN_FAIL => {
                self.c.scan_failed += 1;
                self.seen.scan_failed = true;
            }
            // BX13 (R2-2): the venue's count of spot's day — seeded into the
            // day the read was asked on (`c`).
            EVT_SPOT_DAY => {
                let count = u32::try_from(e.a).unwrap_or(u32::MAX);
                let limit = u32::try_from(e.b).unwrap_or(0);
                let day = u64::try_from(e.c).unwrap_or(0);
                // The day asked on is the gateway's venue clock's; one more
                // than a day from the arm's own is no day this boot can
                // seed (the risk review's round 2: a first seed of day 0
                // would be rolled away, a far one would freeze the roll) —
                // taken for nothing, the lane stays `NotReady`.
                let today = self.knobs.anchor.wall_of(e.ts_ns) / 1_000_000 / crate::gov::DAY_MS;
                if day.abs_diff(today) <= 1 {
                    self.spot.seed(day.saturating_mul(crate::gov::DAY_MS), count, limit);
                }
            }
            EVT_POSITION => {
                if (e.row as usize) < self.inst.len() {
                    let was = self.active(e.row);
                    self.rt[rx(e.row)].pos_1e6 = e.a;
                    let is = self.active(e.row);
                    // The breadth is futures' alone (BX8, BX9).
                    if is_futures(self.inst.hot(e.row).product) {
                        self.active_rows = (self.active_rows + is as u32).saturating_sub(was as u32);
                    }
                }
            }
            _ => self.c.evt_unknown += 1,
        }
    }

    /// The signal for `slot` at `now_ns`.
    fn signal(&self, slot: u8, now_ns: u64) -> HaltSignal {
        let s = &self.seen;
        let gap = if s.status_ns == 0 {
            0
        } else {
            s.gap_ns.max(now_ns.saturating_sub(s.status_ns))
        };
        // BX-15: a frame that did not scan may have been a fill the book
        // never saw — unbounded drift until the operator restarts.
        let drift = if s.scan_failed { i64::MAX } else { s.drift_1e6 };
        let age = if s.recon_ok_ns == 0 {
            0
        } else {
            now_ns.saturating_sub(s.recon_ok_ns)
        };
        HaltSignal::new(
            gap,
            drift,
            self.reject_streak,
            self.asset_refusal_streak,
            now_ns < s.budget_until_ns,
            // Obligation 8: never reconciled before the day's spend is read.
            s.reconciled && s.day_read,
            age,
        )
        .with_pnl(s.anchor_1e6 > 0, s.equity_1e6.saturating_sub(s.anchor_1e6))
        .with_venue(s.venue_lock, self.margin.at_risk(slot as usize))
        // XG-2 / XG-5: the gateway lost an output or overflowed a table.
        .with_gateway_halted(self.end.stats.halt_reason() != HALT_NONE)
    }

    /// The sweep this arm asked for is still being worked: the gateway has
    /// not taken the request yet, or has and not finished.
    #[inline]
    fn sweep_working(&self) -> bool {
        if self.sweep_asked == 0 {
            return false;
        }
        let (seq, st) = self.end.stats.sweep();
        seq < self.sweep_asked || st == SWEEP_WORKING
    }
}

impl OrderDispatch for BnArm {
    #[inline]
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        match self.submit_inner(order) {
            Ok(()) => {
                self.asset_refusal_streak = 0;
                Ok(())
            }
            Err(r) => Err(self.refused(r)),
        }
    }

    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        let row = self.inst.row_of(req.sym);
        if row == ROW_NONE {
            self.refuse(Refusal::Unbound);
            return Err(DispatchError::NoLiveRoute);
        }
        let h = self.inst.hot(row);
        if h.has(INST_NO_DEADMAN) && h.product != PRODUCT_EQUITY {
            // IoC-only: nothing rests, so no cancel verb is built (§3.8).
            // BX10: an equity order rests (DAY under the TTL) — its cancel
            // is built.
            self.refuse(Refusal::Verb);
            return Err(DispatchError::Unsupported);
        }
        let cmd = BnCmd::cancel(req.client_oid, row, req.strategy_id).at(core_time::now_ns());
        if !self.end.cmd.push(&cmd) {
            self.refuse(Refusal::QueueFull);
            return Err(DispatchError::QueueFull);
        }
        self.c.cancels_sent += 1;
        Ok(())
    }

    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        let o = req.order();
        let row = self.inst.row_of(o.sym);
        if row != ROW_NONE && self.inst.hot(row).has(INST_NO_DEADMAN) {
            // IoC-only: nothing rests, so no modify verb is built (§3.8).
            return Err(self.refused(Refusal::Verb));
        }
        let now = core_time::now_ns();
        // A modify re-prices a resting maker: the submit's step 2 as one.
        let row = match self.admit(o.sym, o.strategy_id, ORDER_KIND_MAKER, now) {
            Ok(row) => row,
            Err(r) => return Err(self.refused(r)),
        };
        // A modify keeps the resting order's own flags at the venue (UM
        // `order.modify` takes none): only an unknown bit is refused here
        // (a spot row has no modify: refused above, `Verb`).
        if let Err(r) = admit_flags(o.flags, true, false) {
            return Err(self.refused(r));
        }
        let buy = o.side == Side::Bid;
        // PB3: a modify is never exempt from the minimum — the venue's
        // `-5029` MODIFICATION_MIN_NOTIONAL carries no reduce-only clause.
        // pin:cli-pb3-modify-not-exempt
        let (px, qty, _) = match self.quantize(row, buy, o.px.raw(), o.qty.raw(), false, now) {
            Ok(v) => v,
            Err(r) => return Err(self.refused(r)),
        };
        // A modify is an order to the venue's ORDERS count: governed alike.
        if !self.fut.admits(now) {
            return Err(self.refused(Refusal::Orders));
        }
        let cmd = BnCmd::modify(o.client_oid, req.prev_client_oid(), px, qty, 0, row, o.side, o.strategy_id).at(now);
        if !self.end.cmd.push(&cmd) {
            return Err(self.refused(Refusal::QueueFull));
        }
        self.fut.commit(now);
        self.c.modifies_sent += 1;
        Ok(())
    }

    #[inline]
    fn try_next_fill(&mut self) -> Option<core_types::Fill> {
        // Fills cross on engine fill lane 4, never through the arm.
        None
    }

    #[inline]
    fn try_next_retired(&mut self) -> Option<Retired> {
        self.retired.pop_released()
    }

    #[inline]
    fn verbs_confirm_later(&self, _sym: core_types::SymbolId, _venue: u8, _strategy_id: u8) -> bool {
        true
    }

    #[inline]
    fn try_next_renamed(&mut self) -> Option<Renamed> {
        self.renamed.pop()
    }

    fn stats(&self) -> DispatchStats {
        let refused: u64 = {
            let mut t = 0;
            let mut i = 0;
            while i < REFUSALS {
                t += self.c.refused[i];
                i += 1;
            }
            t
        };
        DispatchStats {
            accepted: self.c.submitted,
            rejected: self.c.rejected + refused,
            rejected_queue_full: self.c.refused[Refusal::QueueFull as usize],
            fills_seen: self.c.gw[GW_FILLS_BOOKED],
            ..DispatchStats::default()
        }
    }

    fn on_idle(&mut self) -> bool {
        // Obligation 1: what the last poll drained goes to the router now.
        self.retired.release();
        let mut n = 0;
        while n < EVT_DRAIN_MAX {
            let Some(g) = self.end.evt.pop() else {
                break;
            };
            // COPY: one event (64 B) off the ring — the slot is released
            // before `apply` borrows the whole arm; applying in place would
            // hold `self.end.evt` across it.
            let e: BnEvt = *g;
            drop(g);
            self.apply(&e);
            n += 1;
        }
        n > 0 || self.retired.head != self.retired.mark || self.renamed.head != self.renamed.tail
    }

    fn on_venue_event(&mut self, event: &ChannelEvent) {
        // O-BX29: Binance marks reach the arm through the dispatcher only.
        if event.channel != ChannelId::Mark as u8 || event.venue != VenueId::Binance as u8 || event.v0 <= 0 {
            return;
        }
        let row = self.inst.row_of(event.sym);
        if row != ROW_NONE {
            let r = &mut self.rt[rx(row)];
            r.mark_1e6 = event.v0;
            r.mark_ns = event.ts_ns;
        }
    }

    fn halt_signal(&self) -> HaltSignal {
        self.signal(self.knobs.owner_slot, core_time::now_ns())
    }

    fn halt_signal_for(&self, slot: u8) -> HaltSignal {
        self.signal(slot, core_time::now_ns())
    }

    /// XG-1: the sweep is the stats word — never refused, never behind the
    /// command ring. A newer request restarts the gateway's sweep.
    fn cancel_all(&mut self) -> Result<(), DispatchError> {
        self.sweep_asked = self.end.stats.request_sweep();
        Ok(())
    }

    fn cancel_all_state(&self) -> CancelAllState {
        if self.sweep_working() {
            return CancelAllState::Working;
        }
        // (N-b) Stranded wins over orders in flight: see
        // `a_stranded_sweep_is_reported_with_orders_in_flight`. A stranded
        // sweep is reported as such even with orders in flight (the router
        // asks again; each ask is a fresh sweep, its REST fallback
        // included); Clear still needs nothing in flight.
        if self.sweep_asked != 0 && self.end.stats.sweep().1 == SWEEP_STRANDED {
            CancelAllState::Stranded
        } else if self.in_flight > 0 {
            CancelAllState::Working
        } else {
            CancelAllState::Clear
        }
    }

    /// XMM XH5a: one owner slot (BX-7) — its cancel is the arm's whole
    /// cancel; another slot has nothing on this account.
    fn cancel_slot(&mut self, slot: u8) -> Result<(), DispatchError> {
        if slot == self.knobs.owner_slot {
            return self.cancel_all();
        }
        Ok(())
    }

    fn cancel_slot_state(&self, slot: u8) -> CancelAllState {
        if slot == self.knobs.owner_slot {
            return self.cancel_all_state();
        }
        CancelAllState::Clear
    }

    fn arm_counters(&self) -> LiveArmCounters {
        let c = &self.c;
        let refused_local = {
            let mut t = 0;
            let mut i = 0;
            while i < REFUSALS {
                t += c.refused[i];
                i += 1;
            }
            t
        };
        LiveArmCounters {
            submitted: c.submitted,
            rejected: c.rejected,
            ioc_missed: c.ioc_missed,
            refused_local,
            // BX11 (the published traps): a row bound but no longer live
            // is LAW E-4's "an instance the table has rolled past"; an
            // order whose answer never came is one put in doubt (BX-11) —
            // not every order in flight; a stranded sweep is one that gave
            // up; the headroom is the request weight's in its minute.
            refused_stale: c.refused[Refusal::NotLive as usize],
            sent_unanswered: c.gw[GW_IN_DOUBT_RAISED],
            fills_booked: c.gw[GW_FILLS_BOOKED],
            fills_unresolved: c.gw[GW_FILLS_UNRESOLVED],
            fills_foreign: c.gw[GW_FILLS_FOREIGN],
            fills_scan_failed: c.scan_failed,
            fills_unowned: c.gw[GW_FILLS_UNOWNED],
            fills_dropped: c.gw[GW_FILLS_LOST],
            recon_ok: c.gw[GW_RECON_OK],
            recon_failed: c.gw[GW_RECON_FAILED],
            recon_drift_legs: c.gw[GW_RECON_DRIFT_LEGS],
            recon_unseen_legs: c.gw[GW_RECON_UNSEEN_LEGS],
            sweep_left: c.gw[GW_SWEEP_LEFT],
            sweep_stalled: c.sweeps_stranded,
            cancel_all_unqueued: c.cancel_all_unqueued,
            ws_reconnects: c.gw[GW_RECONNECTS],
            ws_connect_failures: c.gw[GW_CONNECT_FAILURES],
            budget_remaining: ARM_WEIGHT_MAX_PER_MIN as i64 - self.end.stats.published(crate::gateway::PW_WEIGHT) as i64,
            pnl_anchor_usd_1e6: self.seen.anchor_1e6,
            session_pnl_usd_1e6: self.seen.equity_1e6.saturating_sub(self.seen.anchor_1e6),
            ..LiveArmCounters::default()
        }
    }

    /// BX6: `/state` `exec.arms.binance`.
    fn venue_arm_counters(&self, venue: u8) -> Option<LiveArmCounters> {
        (venue == VenueId::Binance as u8).then(|| self.arm_counters())
    }

    /// BX11: `/state` `exec.arms.binance_gw`, `/metrics` `engine_exec_bn_*`
    /// (the trait's doc). Cold: the counters copied, the gateway's stats
    /// block read, the owned rows counted (≤ 256), the round trips'
    /// quantiles taken (`/metrics`' cost, once a second at most).
    fn binance_arm_report(&self, now_ns: u64, out: &mut BinanceArmReport) -> bool {
        let (c, st, seen) = (&self.c, &*self.end.stats, &self.seen);
        let flag = |f: u8| u64::from(seen.status_flags & f != 0);
        let mut owned = 0u64;
        let mut row = 0u16;
        while (row as usize) < self.inst.len() {
            owned += u64::from(self.inst.hot(row).has(INST_OWNED));
            row += 1;
        }
        let hb = st.heartbeat_ns();
        let k = usize::from(self.lat_next.get()) % LAT_QUANTILES;
        self.lat[k].set(match k {
            0 => st.post_rtt.quantile_ns(0.5),
            1 => st.post_rtt.quantile_ns(0.99),
            2 => st.cancel_rtt.quantile_ns(0.99),
            3 => st.fill_venue_to_local.quantile_ns(0.99),
            4 => st.rest_rtt.quantile_ns(0.99),
            _ => st.push_to_write.quantile_ns(0.99),
        });
        self.lat_next.set(((k + 1) % LAT_QUANTILES) as u8);
        // COPY: the report (688 B) into the caller's — cold, once a second;
        // built whole, so no field is left from an earlier read — rejected:
        // field-by-field writes into `out` (the same bytes, less legible).
        *out = BinanceArmReport {
            present: 1,
            slot: u64::from(self.knobs.owner_slot),
            acked: c.acked,
            cancels_sent: c.cancels_sent,
            cancels_refused: c.cancels_refused,
            modifies_sent: c.modifies_sent,
            modifies_refused: c.modifies_refused,
            modified: c.modified,
            retired_dropped: c.retired_dropped,
            renamed_dropped: c.renamed_dropped,
            budget_obs: c.budget_obs,
            locks: c.locks,
            scan_failed: c.scan_failed,
            evt_unknown: c.evt_unknown,
            sweeps_stranded: c.sweeps_stranded,
            fills_deferred: c.gw[GW_FILLS_DEFERRED],
            jobs_dropped: c.gw[GW_JOBS_DROPPED],
            in_doubt_raised: c.gw[GW_IN_DOUBT_RAISED],
            journal_dropped: c.gw[GW_JOURNAL_DROPPED],
            cancels_owed: c.gw[GW_CANCELS_OWED],
            quiet: c.gw[GW_QUIET],
            dns_reresolved: c.gw[GW_DNS_RERESOLVED],
            events_other: c.gw[GW_EVENTS_OTHER],
            refused: c.refused,
            ready: u64::from(self.ready(now_ns)),
            order_up: flag(EVT_F_ORDER_UP),
            user_up: flag(EVT_F_USER_UP),
            deadman_ok: flag(EVT_F_DEADMAN_OK),
            clock_ok: flag(EVT_F_CLOCK_OK),
            anchor_unsaved: flag(EVT_F_ANCHOR_UNSAVED),
            in_flight: u64::from(self.in_flight),
            in_doubt: seen.in_doubt,
            clock_offset_ms: seen.clock_offset_ms,
            heartbeat_age_ms: if hb == 0 { u64::MAX } else { now_ns.saturating_sub(hb) / 1_000_000 },
            congestion: st.congestion(),
            halt_reason: st.halt_reason(),
            sweep: self.cancel_all_state() as u64,
            weight_used: st.published(crate::gateway::PW_WEIGHT),
            weight_max: ARM_WEIGHT_MAX_PER_MIN,
            opt_weight_used: st.published(crate::gateway::PW_OPT_WEIGHT),
            opt_weight_max: OPT_ARM_WEIGHT_MAX_PER_MIN,
            spot_weight_used: st.published(crate::gateway::PW_SPOT_WEIGHT),
            spot_weight_max: SPOT_ARM_WEIGHT_MAX_PER_MIN,
            spot_day_unfilled: u64::from(self.spot.unfilled_on(self.knobs.anchor.wall_of(now_ns) / 1_000_000)),
            spot_day_cap: u64::from(self.spot.caps().0),
            spot_sell_cap: u64::from(self.spot.caps().1),
            spot_day_seeded: u64::from(self.spot.seeded()),
            recon_held: c.gw[GW_RECON_HELD],
            orders_10s_used: self.fut.w10s.used(now_ns) as u64,
            orders_10s_cap: self.fut.w10s.cap() as u64,
            orders_1m_used: self.fut.w1m.used(now_ns) as u64,
            orders_1m_cap: self.fut.w1m.cap() as u64,
            quiet_left_ms: seen.budget_until_ns.saturating_sub(now_ns) / 1_000_000,
            margin_usdm_1e6: self.margin.ratio(PRODUCT_USDM),
            margin_coinm_1e6: self.margin.ratio(PRODUCT_COINM),
            margin_options_1e6: self.margin.ratio(PRODUCT_OPTIONS),
            owned_rows: owned,
            account_mode: self.knobs.mode as u64,
            demo: u64::from(self.knobs.demo),
            post_rtt_p50_ns: self.lat[0].get(),
            post_rtt_p99_ns: self.lat[1].get(),
            post_rtt_max_ns: st.post_rtt.max_ns(),
            cancel_rtt_p99_ns: self.lat[2].get(),
            fill_lag_p99_ns: self.lat[3].get(),
            rest_rtt_p99_ns: self.lat[4].get(),
            push_to_write_p99_ns: self.lat[5].get(),
        };
        true
    }

    /// The sweep (the word: it runs whatever the ring holds), then the
    /// verb that ends the gateway's loop once it has; waited for up to
    /// [`SHUTDOWN_WAIT_NS`].
    fn on_shutdown(&mut self) {
        self.sweep_asked = self.end.stats.request_sweep();
        if !self.end.cmd.push(&BnCmd::shutdown().at(core_time::now_ns())) {
            // The sweep still runs; the gateway leaves on the stop flag's
            // grace instead of on the verb.
            self.c.cancel_all_unqueued += 1;
        }
        let start = core_time::now_ns();
        while self.sweep_working() && core_time::now_ns().saturating_sub(start) < SHUTDOWN_WAIT_NS {
            if !self.on_idle() {
                core::hint::spin_loop();
            }
        }
    }

    fn venue_day_bought(&self, slot: usize) -> Option<(u64, i64)> {
        if !self.seen.day_read {
            return None;
        }
        self.day_bought_1e6.get(slot).map(|&v| (self.seen.day, v))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::inst::tests::{fapi_rows, FAPI};
    use crate::inst::{BindSpec, PRODUCT_SPOT, PRODUCT_USDM};
    use core_types::{make_symbol_id, Price, Qty};
    use exec_gateway::stats::{HALT_VENUE_BASE, SWEEP_CLEAR};

    pub(crate) const SLOT: u8 = 2;

    pub(crate) fn btc() -> core_types::SymbolId {
        make_symbol_id(VenueId::Binance, 513)
    }

    pub(crate) struct Rig {
        pub arm: BnArm,
        /// The gateway's end, played by the test.
        pub gw: BnGatewayEnd,
    }

    pub(crate) fn rig_with(maker_ok: bool) -> Rig {
        let d = fapi_rows(FAPI);
        let (mut t, mut w) = InstTable::new(7);
        let row = d.find(b"BTCUSDT").unwrap();
        t.bind(&mut w, &BindSpec { sym: btc(), product: PRODUCT_USDM, row, owned: true, maker_ok }).unwrap();
        let (eng, gw) = duplex();
        let mut lim = [0i64; SLOTS];
        lim[SLOT as usize] = 600_000;
        let mut prod = [0u8; SLOTS];
        prod[SLOT as usize] = 1 << PRODUCT_USDM;
        let knobs = ArmKnobs {
            orders_frac_1e6: 800_000,
            qtr_frac_1e6: 800_000,
            owner_slot: SLOT,
            max_symbols: 20,
            min_maker_ttl_ns: 5_000_000_000,
            margin: MarginBook::new(lim, prod),
            anchor: WallAnchor::new(0, 1_790_000_000_000_000_000),
            mode: AccountMode::Classic,
            demo: false,
            equity_ttl_ns: 0,
        };
        Rig { arm: BnArm::new(eng, t, knobs), gw }
    }

    impl Rig {
        pub(crate) fn send(&mut self, e: BnEvt) {
            assert!(self.gw.evt.emit(&e));
        }

        /// The gateway's answer to the arm's latest sweep request.
        pub(crate) fn sweep_done(&mut self, state: u64) {
            let req = self.gw.stats.sweep_requested();
            self.gw.stats.set_sweep(req, state);
        }

        /// A healthy gateway: logged on, clock measured, dead-man armed,
        /// reconciled, the day read.
        pub(crate) fn healthy(&mut self) {
            let now = core_time::now_ns();
            let mut s = BnEvt::new(EVT_STATUS, now);
            s.flags = EVT_F_ORDER_UP | EVT_F_USER_UP | EVT_F_CLOCK_OK | EVT_F_DEADMAN_OK;
            self.send(s);
            let mut r = BnEvt::new(EVT_RECON, now);
            r.flags = EVT_F_RECONCILED;
            self.send(r);
            let mut d = BnEvt::new(EVT_DAY, now);
            d.slot = SLOT;
            d.a = 20_000;
            self.send(d);
            self.arm.on_idle();
        }

        pub(crate) fn pop_cmd(&mut self) -> Option<BnCmd> {
            self.gw.cmd.pop().map(|g| *g)
        }
    }

    pub(crate) fn order(kind: u8, side: Side, px: i64, qty: i64, oid: u64) -> Order {
        let mut o = Order::new(0, VenueId::Binance, btc(), side, kind, Price(px), Qty(qty), oid);
        o.strategy_id = SLOT;
        o
    }

    #[test]
    fn a_healthy_arm_quantizes_and_pushes() {
        let mut r = rig_with(true);
        r.healthy();
        // BUY 65 000.15 → floors to 65 000.1; qty 0.0123 → 0.012.
        let o = order(ORDER_KIND_IOC, Side::Bid, 65_000_150_000, 12_300, 9);
        r.arm.submit(&o).unwrap();
        let c = r.pop_cmd().unwrap();
        assert_eq!((c.verb, c.px_1e6, c.qty_1e6, c.ttl_deadline_ns), (VERB_PLACE, 65_000_100_000, 12_000, 0));
        assert_eq!((c.slot, c.row, c.client_oid), (SLOT, 0, 9));
        // SELL ceils.
        r.arm.submit(&order(ORDER_KIND_IOC, Side::Ask, 65_000_150_000, 12_300, 10)).unwrap();
        assert_eq!(r.pop_cmd().unwrap().px_1e6, 65_000_200_000);
        assert_eq!(r.arm.in_flight(), 2);
        assert_eq!(r.arm.counters().submitted, 2);
    }

    #[test]
    fn not_ready_until_the_gateway_says_so() {
        let mut r = rig_with(true);
        let o = order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 1);
        assert_eq!(r.arm.submit(&o), Err(DispatchError::Disconnected));
        assert_eq!(r.arm.counters().refused[Refusal::NotReady as usize], 1);
        r.healthy();
        assert!(r.arm.submit(&o).is_ok());
    }

    /// BX8: the rig with a spot row (the anchor, under the alias) and a
    /// USDⓈ-M row, healthy; BX13: spot's day not yet read.
    fn spot_rig() -> Rig {
        let d = fapi_rows(crate::inst::tests::SPOT);
        let f = fapi_rows(FAPI);
        let (mut t, mut w) = InstTable::new(7);
        t.bind(&mut w, &BindSpec { sym: 7, product: PRODUCT_SPOT, row: d.find(b"BTCUSDT").unwrap(), owned: true, maker_ok: false }).unwrap();
        t.bind(&mut w, &BindSpec { sym: btc(), product: PRODUCT_USDM, row: f.find(b"BTCUSDT").unwrap(), owned: true, maker_ok: true }).unwrap();
        let (eng, gw) = duplex();
        let mut prod = [0u8; SLOTS];
        prod[SLOT as usize] = 1 << PRODUCT_USDM;
        let knobs = ArmKnobs {
            orders_frac_1e6: 800_000,
            qtr_frac_1e6: 800_000,
            owner_slot: SLOT,
            max_symbols: 1,
            min_maker_ttl_ns: 5_000_000_000,
            margin: MarginBook::new([600_000; SLOTS], prod),
            anchor: WallAnchor::new(0, 1_790_000_000_000_000_000),
            mode: AccountMode::Classic,
            demo: false,
            equity_ttl_ns: 0,
        };
        let mut r = Rig { arm: BnArm::new(eng, t, knobs), gw };
        r.healthy();
        r
    }

    /// An order on the spot row (sym 7).
    fn spot(kind: u8, side: Side, flags: u8, oid: u64) -> Order {
        let mut o = Order::new(0, VenueId::Binance, 7, side, kind, Price(65_000_000_000), Qty(1_000), oid);
        o.strategy_id = SLOT;
        o.flags = flags;
        o
    }

    /// BX13 (R2-2): spot's day as the venue counts it, sent to the arm.
    fn spot_day(r: &mut Rig, count: i64, limit: i64) {
        let now = core_time::now_ns();
        let mut d = BnEvt::new(EVT_SPOT_DAY, now);
        d.a = count;
        d.b = limit;
        // The day asked on: the arm's own (the rig's anchor).
        d.c = (r.arm.knobs.anchor.wall_of(now) / 1_000_000 / crate::gov::DAY_MS) as i64;
        r.send(d);
        r.arm.on_idle();
    }

    /// BX8: a spot row (here the anchor, under the alias) trades IoC only
    /// — a maker is refused (no dead-man), and so is a reduce-only order —
    /// on spot's own ORDERS pool: 80 IoCs in 10 s (80 % of 100) while the
    /// futures' pool stays untouched; a spot order counts toward neither
    /// the breadth nor the UM rules; an order that filled leaves the day's
    /// unfilled count. BX13 (R2-2): nothing before the venue's day is read.
    #[test]
    fn a_spot_row_is_ioc_only_on_its_own_pool() {
        let mut r = spot_rig();
        assert_eq!(r.arm.submit(&spot(ORDER_KIND_IOC, Side::Bid, 0, 3)), Err(DispatchError::Disconnected), "the day unread");
        assert_eq!(r.arm.counters().refused[Refusal::NotReady as usize], 1);
        spot_day(&mut r, 0, 200_000);
        assert!(r.arm.spot.seeded());
        let mut m = spot(ORDER_KIND_MAKER, Side::Bid, 0, 1);
        m.ttl_ns = 10_000_000_000;
        assert_eq!(r.arm.submit(&m), Err(DispatchError::Unsupported));
        assert_eq!(r.arm.submit(&spot(ORDER_KIND_IOC, Side::Ask, ORDER_FLAG_REDUCE_ONLY, 2)), Err(DispatchError::Unsupported));
        let c = r.arm.counters();
        assert_eq!((c.refused[Refusal::NoDeadman as usize], c.refused[Refusal::Flags as usize]), (1, 1));
        let mut placed = 0u32;
        let mut oid = 10;
        loop {
            match r.arm.submit(&spot(ORDER_KIND_IOC, Side::Bid, 0, oid)) {
                Ok(()) => placed += 1,
                Err(e) => {
                    assert_eq!(e, DispatchError::SlotDisabled);
                    break;
                }
            }
            oid += 1;
            let c = r.pop_cmd().expect("the IoC reached the ring");
            assert_eq!((c.kind, c.flags, c.qty_1e6), (ORDER_KIND_IOC, 0, 1_000));
        }
        assert_eq!(placed, 80, "spot's 10 s window at 80 % of 100");
        assert_eq!(r.arm.counters().refused[Refusal::Orders as usize], 1);
        assert_eq!(r.arm.fut.w10s.used(core_time::now_ns()), 0, "spot drew nothing from the futures' pool");
        assert_eq!((r.arm.active_rows, r.arm.rows_open), (0, 0), "no breadth, no UM rule");
        // The futures' pool is its own: the USDⓈ-M row still trades — the
        // one row the breadth (max_symbols 1) allows.
        assert_eq!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 500)), Ok(()));
        assert_eq!(r.arm.rows_open, 1);
        // An order that filled is off the day's unfilled count.
        assert_eq!(r.arm.spot.unfilled(), 80);
        let mut e = BnEvt::order(EVT_RETIRED, core_time::now_ns(), 10, 0, SLOT);
        e.why = clob_dispatcher::RETIRED_FILLED;
        e.flags = EVT_F_HAD_FILL | EVT_F_IOC;
        r.send(e);
        let mut x = BnEvt::order(EVT_RETIRED, core_time::now_ns(), 11, 0, SLOT);
        x.why = clob_dispatcher::RETIRED_EXPIRED;
        x.flags = EVT_F_IOC;
        r.send(x);
        r.arm.on_idle();
        assert_eq!(r.arm.spot.unfilled(), 79, "the unfilled IoC stays counted");
    }

    /// BX13 (the risk review's round 2): a read asked more than a day from
    /// the arm's own day seeds nothing — day 0 (rolled away at once) and a
    /// far day (the roll frozen) alike; the lane stays `NotReady` until a
    /// read of the day comes. Break-and-watch: unbounded, day 0 seeds.
    #[test]
    fn a_day_far_from_the_arms_own_seeds_nothing() {
        let mut r = spot_rig();
        let now = core_time::now_ns();
        let today = (r.arm.knobs.anchor.wall_of(now) / 1_000_000 / crate::gov::DAY_MS) as i64;
        for c in [0, today - 2, today + 2] {
            let mut d = BnEvt::new(EVT_SPOT_DAY, now);
            (d.a, d.b, d.c) = (1, 200_000, c);
            r.send(d);
            r.arm.on_idle();
            assert!(!r.arm.spot.seeded(), "day {c} of {today}");
        }
        assert_eq!(r.arm.submit(&spot(ORDER_KIND_IOC, Side::Bid, 0, 1)), Err(DispatchError::Disconnected));
        assert_eq!(r.arm.counters().refused[Refusal::NotReady as usize], 1);
        spot_day(&mut r, 1, 200_000);
        assert!(r.arm.spot.seeded(), "today's read");
    }

    /// BX13 (R2-2): the venue's count seeds the day — a BUY stops at the
    /// share (160 000 of 200 000 at 80 %), a SELL goes on into its reserve
    /// (to 170 000) and stops there; a later read never lowers the count.
    #[test]
    fn a_spot_sell_keeps_its_reserve_past_the_days_share() {
        let mut r = spot_rig();
        spot_day(&mut r, 159_999, 200_000);
        assert_eq!(r.arm.spot.unfilled(), 159_999, "the venue's count, not 0");
        assert_eq!(r.arm.submit(&spot(ORDER_KIND_IOC, Side::Bid, 0, 1)), Ok(()));
        r.pop_cmd().expect("the BUY");
        assert_eq!(r.arm.submit(&spot(ORDER_KIND_IOC, Side::Bid, 0, 2)), Err(DispatchError::SlotDisabled), "the BUYs' share");
        assert_eq!(r.arm.counters().refused[Refusal::Orders as usize], 1);
        assert_eq!(r.arm.submit(&spot(ORDER_KIND_IOC, Side::Ask, 0, 3)), Ok(()), "a SELL into the reserve");
        assert_eq!(r.pop_cmd().expect("the SELL").side, Side::Ask as u8);
        spot_day(&mut r, 169_999, 0);
        assert_eq!(r.arm.submit(&spot(ORDER_KIND_IOC, Side::Ask, 0, 4)), Ok(()));
        r.pop_cmd().expect("the last SELL the reserve takes");
        assert_eq!(r.arm.submit(&spot(ORDER_KIND_IOC, Side::Ask, 0, 5)), Err(DispatchError::SlotDisabled), "the reserve spent");
        spot_day(&mut r, 5, 0);
        assert_eq!(r.arm.spot.unfilled(), 170_000, "a read never lowers the mirror");
    }

    /// BX10: a Binance Stocks row — an IoC by kind, sent as a DAY LIMIT
    /// under the mandatory TTL (the arm stamps the deadline); no maker (no
    /// dead-man), no reduce-only, no post-only; the venue's sides; a SELL
    /// only of a booked holding; Stocks' own pool; its cancel verb built,
    /// its modify not. Break-and-watch: dropping the holding check sends
    /// the first SELL.
    #[test]
    fn an_equity_row_rests_under_the_ttl_long_only() {
        let info = br#"{"symbols":[{"symbol":"AAPL","tradability":"BUY_SELL","fractionable":true,"stepSize":"0.000000001","minNotional":"5","maxQty":"1000","maxNumOrders":200},{"symbol":"SPY","tradability":"SELL","fractionable":false,"stepSize":"1","minNotional":"5"}]}"#;
        let (aapl, spy) = (make_symbol_id(VenueId::Binance, 4_097), make_symbol_id(VenueId::Binance, 4_098));
        let (mut t, mut w) = InstTable::new(7);
        let a = crate::equity::scan_eq_symbol(info, b"AAPL").unwrap();
        let b = crate::equity::scan_eq_symbol(info, b"SPY").unwrap();
        let ra = t.bind_equity(&mut w, &crate::inst::EqBindSpec { sym: aapl, row: &a, owned: true, bstock: b"AAPLB" }).unwrap();
        t.bind_equity(&mut w, &crate::inst::EqBindSpec { sym: spy, row: &b, owned: true, bstock: b"" }).unwrap();
        let h = t.hot(ra);
        assert_eq!((h.tick_1e6, h.step_1e6, h.px_dec, h.qty_dec, h.min_notional_1e6), (10_000, 1, 2, 6, 5_000_000), "0.01 and a millionth");
        assert!(h.has(INST_LIVE) && h.has(INST_NO_DEADMAN) && h.product == PRODUCT_EQUITY);
        assert_eq!(t.hot(ra + 1).step_1e6, 1_000_000, "SPY trades whole shares");
        assert_eq!(w.eq(ra).bstock.get(), b"AAPLB");
        let (eng, gw) = duplex();
        let mut prod = [0u8; SLOTS];
        prod[SLOT as usize] = 1 << PRODUCT_EQUITY;
        let knobs = |ttl: u64| ArmKnobs {
            orders_frac_1e6: 800_000,
            qtr_frac_1e6: 800_000,
            owner_slot: SLOT,
            max_symbols: 1,
            min_maker_ttl_ns: 5_000_000_000,
            margin: MarginBook::new([600_000; SLOTS], prod),
            anchor: WallAnchor::new(0, 1_790_000_000_000_000_000),
            mode: AccountMode::Classic,
            demo: false,
            equity_ttl_ns: ttl,
        };
        let mut r = Rig { arm: BnArm::new(eng, t, knobs(30_000_000_000)), gw };
        r.healthy();
        let eq = |sym, kind: u8, side: Side, flags: u8, qty: i64, oid: u64| {
            let mut o = Order::new(0, VenueId::Binance, sym, side, kind, Price(336_097_000), Qty(qty), oid);
            o.strategy_id = SLOT;
            o.flags = flags;
            o
        };
        // A BUY of 0.03 at 336.097 → 336.09 (floored): a DAY order whose
        // deadline is the TTL's.
        let t0 = core_time::now_ns();
        assert_eq!(r.arm.submit(&eq(aapl, ORDER_KIND_IOC, Side::Bid, 0, 30_000, 1)), Ok(()));
        let c = r.pop_cmd().expect("pushed");
        assert_eq!((c.px_1e6, c.qty_1e6, c.kind), (336_090_000, 30_000, ORDER_KIND_IOC));
        assert!(c.ttl_deadline_ns >= t0 + 30_000_000_000 && c.ttl_deadline_ns < t0 + 31_000_000_000, "the TTL stamped");
        let mut m = eq(aapl, ORDER_KIND_MAKER, Side::Bid, 0, 30_000, 2);
        m.ttl_ns = 10_000_000_000;
        assert_eq!(r.arm.submit(&m), Err(DispatchError::Unsupported), "no maker: no dead-man");
        assert_eq!(r.arm.submit(&eq(aapl, ORDER_KIND_IOC, Side::Bid, ORDER_FLAG_REDUCE_ONLY, 30_000, 3)), Err(DispatchError::Unsupported));
        assert_eq!(r.arm.submit(&eq(aapl, ORDER_KIND_IOC, Side::Ask, 0, 30_000, 4)), Err(DispatchError::Unsupported), "no holding: no SELL");
        assert_eq!(r.arm.submit(&eq(aapl, ORDER_KIND_IOC, Side::Bid, 0, 10_000, 5)), Err(DispatchError::Unsupported), "$3.36 < $5");
        assert_eq!(r.arm.submit(&eq(spy, ORDER_KIND_IOC, Side::Bid, 0, 1_000_000, 6)), Err(DispatchError::NoLiveRoute), "SPY is SELL-only");
        let k = r.arm.counters().refused;
        assert_eq!(
            (k[Refusal::NoDeadman as usize], k[Refusal::Flags as usize], k[Refusal::NotWritable as usize], k[Refusal::MinNotional as usize], k[Refusal::NotLive as usize]),
            (1, 1, 1, 1, 1)
        );
        // A holding booked: the SELL goes.
        let mut p = BnEvt::new(EVT_POSITION, core_time::now_ns());
        p.row = ra;
        p.a = 30_000;
        r.send(p);
        r.arm.on_idle();
        assert_eq!(r.arm.submit(&eq(aapl, ORDER_KIND_IOC, Side::Ask, 0, 30_000, 7)), Ok(()));
        assert!(r.pop_cmd().is_some());
        // The cancel verb is built; the modify is not.
        let cr = CancelReq::of(&eq(aapl, ORDER_KIND_IOC, Side::Bid, 0, 30_000, 1), 0);
        assert_eq!(r.arm.cancel(&cr), Ok(()));
        assert_eq!(r.pop_cmd().map(|c| c.verb), Some(VERB_CANCEL));
        let mo = ModifyReq::new(1, eq(aapl, ORDER_KIND_MAKER, Side::Bid, 0, 30_000, 8));
        assert_eq!(r.arm.modify(&mo), Err(DispatchError::Unsupported));
        // Stocks' own pool: 160 a minute at 80 % of 200, the others' untouched.
        let mut placed = 2u32;
        let mut oid = 100;
        while r.arm.submit(&eq(aapl, ORDER_KIND_IOC, Side::Bid, 0, 30_000, oid)).is_ok() {
            placed += 1;
            oid += 1;
            let _ = r.pop_cmd();
        }
        assert_eq!(placed, 160);
        assert_eq!((r.arm.fut.w10s.used(core_time::now_ns()), r.arm.spot.w10s.used(core_time::now_ns())), (0, 0));
        // No TTL configured: refused (an equity order would rest unbounded).
        let (eng, gw) = duplex();
        let (mut t2, mut w2) = InstTable::new(7);
        t2.bind_equity(&mut w2, &crate::inst::EqBindSpec { sym: aapl, row: &a, owned: true, bstock: b"" }).unwrap();
        let mut r2 = Rig { arm: BnArm::new(eng, t2, knobs(0)), gw };
        r2.healthy();
        assert_eq!(r2.arm.submit(&eq(aapl, ORDER_KIND_IOC, Side::Bid, 0, 30_000, 1)), Err(DispatchError::Unsupported));
        assert_eq!(r2.arm.counters().refused[Refusal::Ttl as usize], 1);
    }

    #[test]
    fn the_refusals_are_counted_by_reason() {
        let mut r = rig_with(true);
        r.healthy();
        let mut o = order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 1);
        o.sym = make_symbol_id(VenueId::Binance, 999);
        assert_eq!(r.arm.submit(&o), Err(DispatchError::NoLiveRoute));
        let mut o = order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 1);
        o.strategy_id = 3;
        assert_eq!(r.arm.submit(&o), Err(DispatchError::NoLiveRoute));
        assert_eq!(r.arm.submit(&order(7, Side::Bid, 65_000_000_000, 10_000, 1)), Err(DispatchError::Unsupported));
        // Qty 0.0009 floors to 0: under minQty.
        assert_eq!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 900, 1)), Err(DispatchError::Unsupported));
        // 0.001 × 65 000 = 65 < 100 USD min notional.
        assert_eq!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 1_000, 1)), Err(DispatchError::Unsupported));
        // A maker TTL under 5 s.
        let mut m = order(ORDER_KIND_MAKER, Side::Bid, 65_000_000_000, 10_000, 1);
        m.ttl_ns = 1_000_000_000;
        assert_eq!(r.arm.submit(&m), Err(DispatchError::Unsupported));
        let c = r.arm.counters();
        assert_eq!(c.refused[Refusal::Unbound as usize], 1);
        assert_eq!(c.refused[Refusal::NotOwner as usize], 1);
        assert_eq!(c.refused[Refusal::Kind as usize], 1);
        assert_eq!(c.refused[Refusal::MinQty as usize], 1);
        assert_eq!(c.refused[Refusal::MinNotional as usize], 1);
        assert_eq!(c.refused[Refusal::Ttl as usize], 1);
        assert!(r.pop_cmd().is_none(), "nothing refused reached the ring");
    }

    #[test]
    fn a_maker_needs_a_working_dead_man() {
        let mut r = rig_with(false);
        r.healthy();
        let m = order(ORDER_KIND_MAKER, Side::Bid, 65_000_000_000, 10_000, 1);
        assert_eq!(r.arm.submit(&m), Err(DispatchError::Unsupported), "IoC-only product");
        let mut r = rig_with(true);
        r.healthy();
        let mut s = BnEvt::new(EVT_STATUS, core_time::now_ns());
        s.flags = EVT_F_ORDER_UP | EVT_F_USER_UP | EVT_F_CLOCK_OK; // countdown not answering
        r.send(s);
        r.arm.on_idle();
        // N6: a dead-man down NOW is transient — retryable, not Unsupported.
        assert_eq!(r.arm.submit(&m), Err(DispatchError::Disconnected));
        assert_eq!(r.arm.counters().refused[Refusal::DeadmanDown as usize], 1);
        assert_eq!(r.arm.counters().refused[Refusal::NoDeadman as usize], 0);
    }

    #[test]
    fn the_band_binds_only_with_a_fresh_mark() {
        let mut r = rig_with(true);
        r.healthy();
        // No mark: a far price passes the arm (the router's gate is its own).
        assert!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 80_000_000_000, 10_000, 1)).is_ok());
        let mark = ChannelEvent::new(core_time::now_ns(), VenueId::Binance, ChannelId::Mark, btc(), 0, 0, 65_000_000_000, 65_000_000_000);
        r.arm.on_venue_event(&mark);
        // 1.05 × 65 000 = 68 250.
        assert!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 68_250_000_000, 10_000, 2)).is_ok());
        assert_eq!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 68_250_100_000, 10_000, 3)), Err(DispatchError::Unsupported));
        // 0.95 × 65 000 = 61 750 for a sell.
        assert_eq!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Ask, 61_749_900_000, 10_000, 4)), Err(DispatchError::Unsupported));
        assert_eq!(r.arm.counters().refused[Refusal::Band as usize], 2);
    }

    #[test]
    fn retirements_reach_the_router_one_poll_late() {
        let mut r = rig_with(true);
        r.healthy();
        r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 5)).unwrap();
        let mut e = BnEvt::order(EVT_RETIRED, 1, 5, 0, SLOT);
        e.why = clob_dispatcher::RETIRED_FILLED;
        e.flags = EVT_F_HAD_FILL | EVT_F_IOC;
        r.send(e);
        r.arm.on_idle();
        assert_eq!(r.arm.try_next_retired(), None, "held back: the fill books first");
        assert_eq!(r.arm.in_flight(), 0);
        r.arm.on_idle();
        let got = r.arm.try_next_retired().unwrap();
        assert_eq!((got.client_oid, got.slot, got.why), (5, SLOT, clob_dispatcher::RETIRED_FILLED));
        assert_eq!(r.arm.try_next_retired(), None);
    }

    #[test]
    fn a_reject_retires_and_moves_the_streak_a_would_take_does_not() {
        let mut r = rig_with(true);
        r.healthy();
        r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 5)).unwrap();
        r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 6)).unwrap();
        let mut a = BnEvt::order(EVT_REJECT, 1, 5, 0, SLOT);
        a.code = -2019;
        r.send(a);
        let mut b = BnEvt::order(EVT_REJECT, 1, 6, 0, SLOT);
        b.code = -5022;
        r.send(b);
        r.arm.on_idle();
        r.arm.on_idle();
        assert_eq!(r.arm.halt_signal().reject_streak, 1);
        assert_eq!(r.arm.try_next_retired().unwrap().why, RETIRED_REJECTED);
        assert_eq!(r.arm.try_next_retired().unwrap().why, RETIRED_EXPIRED);
    }

    #[test]
    fn cancel_and_modify_are_queued_and_confirmed_later() {
        let mut r = rig_with(true);
        r.healthy();
        assert!(r.arm.verbs_confirm_later(btc(), VenueId::Binance as u8, SLOT));
        r.arm.cancel(&CancelReq::new(0, VenueId::Binance, btc(), 5)).unwrap();
        assert_eq!(r.pop_cmd().unwrap().verb, VERB_CANCEL);
        let o = order(ORDER_KIND_MAKER, Side::Bid, 64_000_050_000, 12_345, 8);
        r.arm.modify(&ModifyReq::new(7, o)).unwrap();
        let c = r.pop_cmd().unwrap();
        assert_eq!((c.verb, c.client_oid, c.prev_client_oid, c.px_1e6, c.qty_1e6), (VERB_MODIFY, 8, 7, 64_000_000_000, 12_000));
        let mut e = BnEvt::order(EVT_MODIFIED, 1, 8, 0, SLOT);
        e.a = 7;
        e.b = 12_000;
        e.side = Side::Bid as u8;
        r.send(e);
        r.arm.on_idle();
        let n = r.arm.try_next_renamed().unwrap();
        assert_eq!((n.prev_client_oid, n.client_oid, n.qty_1e6, n.sym, n.slot, n.buy), (7, 8, 12_000, btc(), SLOT, 1));
    }

    /// A modify re-prices a resting maker: it passes the submit's gate.
    /// Break-and-watch: without `admit` in `modify`, each of these is
    /// queued — a size raise while the stream is down, on another slot's
    /// row, or with no dead-man.
    #[test]
    fn a_modify_passes_the_submits_gate() {
        let m = order(ORDER_KIND_MAKER, Side::Bid, 64_000_000_000, 12_000, 8);
        // Not ready: the dead-man answers, but never reconciled yet.
        let mut r = rig_with(true);
        let mut s = BnEvt::new(EVT_STATUS, core_time::now_ns());
        s.flags = EVT_F_ORDER_UP | EVT_F_USER_UP | EVT_F_CLOCK_OK | EVT_F_DEADMAN_OK;
        r.send(s);
        r.arm.on_idle();
        assert_eq!(r.arm.modify(&ModifyReq::new(7, m)), Err(DispatchError::Disconnected));
        // Another slot's.
        r.healthy();
        let mut other = m;
        other.strategy_id = 3;
        assert_eq!(r.arm.modify(&ModifyReq::new(7, other)), Err(DispatchError::NoLiveRoute));
        // The dead-man stopped answering (N6: transient, retryable).
        let mut s = BnEvt::new(EVT_STATUS, core_time::now_ns());
        s.flags = EVT_F_ORDER_UP | EVT_F_USER_UP | EVT_F_CLOCK_OK;
        r.send(s);
        r.arm.on_idle();
        assert_eq!(r.arm.modify(&ModifyReq::new(7, m)), Err(DispatchError::Disconnected));
        // A row the venue closed for good (RowFatal retires it).
        let mut r = rig_with(true);
        r.healthy();
        r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 5)).unwrap();
        let mut e = BnEvt::order(EVT_REJECT, 1, 5, 0, SLOT);
        e.code = -4411;
        r.send(e);
        r.arm.on_idle();
        assert_eq!(r.arm.modify(&ModifyReq::new(7, m)), Err(DispatchError::NoLiveRoute));
        let c = r.arm.counters();
        assert_eq!((c.refused[Refusal::NotLive as usize], c.modifies_sent), (1, 0));
        assert_eq!(r.pop_cmd().map(|c| c.verb), Some(VERB_PLACE), "only the IoC reached the ring");
        assert!(r.pop_cmd().is_none());
    }

    /// The venue law without a divide: the notional and band edges land
    /// exactly where the floored products put them.
    #[test]
    fn the_notional_and_band_edges_are_exact() {
        // ⌊a·b/1e6⌋ < m at the edge: 100 USD at 100 000.0 × 0.001.
        assert!(!scaled_lt(100_000_000_000, 1_000, 100_000_000));
        assert!(scaled_lt(99_999_900_000, 1_000, 100_000_000));
        let mut r = rig_with(true);
        r.healthy();
        // 0.002 × 50 000 = 100 USD: exactly the minimum passes.
        assert!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 50_000_000_000, 2_000, 1)).is_ok());
        // 0.002 × 49 999.9 = 99.9998: under it.
        assert_eq!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 49_999_900_000, 2_000, 2)), Err(DispatchError::Unsupported));
        assert_eq!(r.arm.counters().refused[Refusal::MinNotional as usize], 1);
    }

    /// The rig with the strict grid on (PB2, the Pine slot's law), healthy.
    fn strict_rig() -> Rig {
        let mut r = rig_with(true);
        r.arm.set_strict_grid(true);
        r.healthy();
        r
    }

    /// **PB2 (I-PB2)** — a strict owner's order off the row's grid
    /// (BTCUSDT: tick 0.10, step 0.001) is refused `OffGrid`, counted,
    /// mapped `Unsupported` as a price refusal is, and nothing reaches the
    /// ring: an off-tick price (either side), an off-step quantity, and a
    /// modify off the tick. Break-and-watch: without the check the BUY
    /// floors to 65 000.1 and is pushed (`a_healthy_arm_quantizes_and_pushes`).
    #[test]
    fn off_grid_price_or_size_is_refused_never_rounded_for_a_strict_owner() {
        let mut r = strict_rig();
        assert!(r.arm.strict_grid());
        let off_px = order(ORDER_KIND_IOC, Side::Bid, 65_000_150_000, 12_000, 1);
        assert_eq!(r.arm.submit(&off_px), Err(DispatchError::Unsupported));
        let off_px_sell = order(ORDER_KIND_IOC, Side::Ask, 65_000_150_000, 12_000, 2);
        assert_eq!(r.arm.submit(&off_px_sell), Err(DispatchError::Unsupported));
        let off_qty = order(ORDER_KIND_IOC, Side::Bid, 65_000_100_000, 12_300, 3);
        assert_eq!(r.arm.submit(&off_qty), Err(DispatchError::Unsupported));
        // Off the step and under minQty at once: the grid is judged first.
        assert_eq!(
            r.arm
                .submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_100_000, 900, 4)),
            Err(DispatchError::Unsupported)
        );
        let mut m = order(ORDER_KIND_MAKER, Side::Bid, 65_000_050_000, 12_000, 5);
        m.ttl_ns = 10_000_000_000;
        assert_eq!(
            r.arm.modify(&ModifyReq::new(9, m)),
            Err(DispatchError::Unsupported)
        );
        let c = r.arm.counters();
        assert_eq!(c.refused[Refusal::OffGrid as usize], 5);
        assert_eq!(
            (
                c.refused[Refusal::MinQty as usize],
                c.refused[Refusal::Price as usize]
            ),
            (0, 0)
        );
        assert_eq!((c.submitted, c.modifies_sent), (0, 0));
        assert!(
            r.pop_cmd().is_none(),
            "nothing off the grid reached the ring"
        );
    }

    /// **PB2 (I-PB2)** — an on-grid order leaves a strict arm exactly as it
    /// leaves a BX-5 arm: every byte of the command but the push stamp.
    #[test]
    fn on_grid_orders_pass_byte_identical() {
        let mut strict = strict_rig();
        let mut bx5 = rig_with(true);
        bx5.healthy();
        let mut m = order(ORDER_KIND_MAKER, Side::Ask, 65_000_100_000, 12_000, 8);
        m.ttl_ns = 10_000_000_000;
        let mut ro = order(ORDER_KIND_IOC, Side::Ask, 64_000_000_000, 2_000, 9);
        ro.flags = ORDER_FLAG_REDUCE_ONLY;
        for o in [
            order(ORDER_KIND_IOC, Side::Bid, 65_000_100_000, 12_000, 7),
            m,
            ro,
        ] {
            strict.arm.submit(&o).unwrap();
            bx5.arm.submit(&o).unwrap();
            let (mut a, mut b) = (strict.pop_cmd().unwrap(), bx5.pop_cmd().unwrap());
            (a.pushed_ns, b.pushed_ns) = (0, 0);
            a.ttl_deadline_ns = a.ttl_deadline_ns.min(1);
            b.ttl_deadline_ns = b.ttl_deadline_ns.min(1);
            assert_eq!(a, b, "oid {}", o.client_oid);
        }
        assert_eq!(strict.arm.counters().refused, bx5.arm.counters().refused);
        assert_eq!(strict.arm.counters().submitted, 3);
    }

    /// **PB2 (I-PB6)** — a non-strict owner (the default: the batteries,
    /// every other slot) still quantizes by BX-5: a BUY floors, a SELL
    /// ceils, the quantity floors; `OffGrid` is never counted.
    #[test]
    fn a_non_strict_owner_still_quantizes_bx5() {
        let mut r = rig_with(true);
        r.healthy();
        assert!(!r.arm.strict_grid());
        r.arm
            .submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_150_000, 12_300, 1))
            .unwrap();
        let c = r.pop_cmd().unwrap();
        assert_eq!((c.px_1e6, c.qty_1e6), (65_000_100_000, 12_000));
        r.arm
            .submit(&order(ORDER_KIND_IOC, Side::Ask, 65_000_150_000, 12_300, 2))
            .unwrap();
        assert_eq!(r.pop_cmd().unwrap().px_1e6, 65_000_200_000);
        assert_eq!(r.arm.counters().refused[Refusal::OffGrid as usize], 0);
        // Switched off again, a strict arm is BX-5's.
        let mut s = strict_rig();
        s.arm.set_strict_grid(false);
        s.arm
            .submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_150_000, 12_300, 3))
            .unwrap();
        assert_eq!(s.pop_cmd().unwrap().px_1e6, 65_000_100_000);
    }

    /// **PB2** — the refusal words, the count and the dispatcher's length
    /// agree: one word per reason, `off_grid` last, every word unique.
    #[test]
    fn the_refusal_words_and_count_agree() {
        assert_eq!(REFUSALS, 22);
        assert_eq!(REFUSAL_WORDS.len(), REFUSALS);
        assert_eq!(BINANCE_REFUSALS, REFUSALS);
        assert_eq!(Refusal::OffGrid as usize, REFUSALS - 1);
        assert_eq!(REFUSAL_WORDS[Refusal::OffGrid as usize], "off_grid");
        assert_eq!(REFUSAL_WORDS[Refusal::NotWritable as usize], "not_writable");
        let mut i = 0;
        while i < REFUSALS {
            let mut j = i + 1;
            while j < REFUSALS {
                assert_ne!(REFUSAL_WORDS[i], REFUSAL_WORDS[j]);
                j += 1;
            }
            i += 1;
        }
        let k = BnArmCounters::default();
        assert_eq!(k.refused.len(), REFUSALS);
    }

    /// **PB3 (I-PB3, OD-6)** — with the exemption on, a strict owner's
    /// USDⓈ-M reduce-only order below the minimum notional is sent (`-4164`
    /// exempts it: "unless you choose reduce only"), reduce-only on the
    /// wire; its `minQty` still binds; the strict grid still binds.
    /// 0.001 × 65 000 = 65 < 100 USDT.
    #[test]
    fn a_reduce_only_order_below_min_notional_passes_on_usdm() {
        let mut r = rig_with(true);
        r.arm.set_strict_grid(true);
        r.arm.set_ro_min_exempt(true);
        r.healthy();
        let mut o = order(ORDER_KIND_IOC, Side::Ask, 65_000_000_000, 1_000, 1);
        o.flags = ORDER_FLAG_REDUCE_ONLY;
        r.arm.submit(&o).unwrap();
        let c = r.pop_cmd().unwrap();
        assert_eq!(
            (c.verb, c.px_1e6, c.qty_1e6, c.flags),
            (VERB_PLACE, 65_000_000_000, 1_000, ORDER_FLAG_REDUCE_ONLY)
        );
        // A reduce-only maker too (the exit that rests).
        let mut m = order(ORDER_KIND_MAKER, Side::Bid, 64_000_000_000, 1_000, 2);
        m.flags = ORDER_FLAG_REDUCE_ONLY;
        m.ttl_ns = 10_000_000_000;
        r.arm.submit(&m).unwrap();
        assert_eq!(r.pop_cmd().unwrap().flags, ORDER_FLAG_REDUCE_ONLY);
        // minQty binds: 0.0009 is under the step, so the strict arm refuses
        // it off the grid before any minimum.
        let mut dust = order(ORDER_KIND_IOC, Side::Ask, 65_000_000_000, 900, 3);
        dust.flags = ORDER_FLAG_REDUCE_ONLY;
        assert_eq!(r.arm.submit(&dust), Err(DispatchError::Unsupported));
        let k = r.arm.counters();
        assert_eq!(k.refused[Refusal::OffGrid as usize], 1);
        assert_eq!(k.refused[Refusal::MinNotional as usize], 0);
        assert_eq!(k.submitted, 2);
    }

    /// **PB3 (r28 risk review B1)** — the exemption needs BOTH the knob and
    /// a strict owner: off by default (PB0.2 unrecorded), and never for a
    /// non-strict owner (the batteries, every non-Pine slot) even with the
    /// knob on. Each refusal is `MinNotional`, nothing reaches the wire.
    #[test]
    fn the_minimum_exemption_needs_the_knob_and_a_strict_owner() {
        for (strict, knob) in [(true, false), (false, true), (false, false)] {
            let mut r = rig_with(true);
            r.arm.set_strict_grid(strict);
            r.arm.set_ro_min_exempt(knob);
            assert_eq!(r.arm.ro_min_exempt(), knob);
            r.healthy();
            let mut o = order(ORDER_KIND_IOC, Side::Ask, 65_000_000_000, 1_000, 1);
            o.flags = ORDER_FLAG_REDUCE_ONLY;
            assert_eq!(r.arm.submit(&o), Err(DispatchError::Unsupported), "strict {strict} knob {knob}");
            assert_eq!(r.arm.counters().refused[Refusal::MinNotional as usize], 1);
            assert!(r.pop_cmd().is_none());
        }
        let r = rig_with(true);
        assert!(!r.arm.ro_min_exempt(), "off by default");
    }

    /// **PB3 (I-PB3)** — an opening order below the minimum is still
    /// refused, and so is a modify below it even when it names reduce-only
    /// (`-5029` MODIFICATION_MIN_NOTIONAL has no reduce-only clause;
    /// pin:cli-pb3-modify-not-exempt).
    #[test]
    fn an_opening_order_below_min_notional_is_still_refused() {
        let mut r = rig_with(true);
        r.healthy();
        assert_eq!(
            r.arm
                .submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 1_000, 1)),
            Err(DispatchError::Unsupported)
        );
        let mut m = order(ORDER_KIND_MAKER, Side::Bid, 65_000_000_000, 1_000, 2);
        m.flags = ORDER_FLAG_REDUCE_ONLY;
        m.ttl_ns = 10_000_000_000;
        assert_eq!(
            r.arm.modify(&ModifyReq::new(9, m)),
            Err(DispatchError::Unsupported)
        );
        assert_eq!(r.arm.counters().refused[Refusal::MinNotional as usize], 2);
        assert!(r.pop_cmd().is_none());
    }

    #[test]
    fn an_ioc_only_product_builds_no_cancel_verb() {
        let mut r = rig_with(false);
        r.healthy();
        assert_eq!(r.arm.cancel(&CancelReq::new(0, VenueId::Binance, btc(), 5)), Err(DispatchError::Unsupported));
        assert_eq!(r.arm.counters().refused[Refusal::Verb as usize], 1);
    }

    #[test]
    fn clear_needs_the_sweep_and_nothing_in_flight() {
        let mut r = rig_with(true);
        r.healthy();
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Clear);
        r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 5)).unwrap();
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Working, "an IoC in flight");
        r.arm.cancel_all().unwrap();
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Working, "the gateway has not taken it");
        assert!(r.pop_cmd().map(|c| c.verb) == Some(VERB_PLACE), "XG-1: the sweep is no command");
        assert!(r.pop_cmd().is_none());
        r.sweep_done(SWEEP_CLEAR);
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Working, "still in flight");
        let mut e = BnEvt::order(EVT_RETIRED, 1, 5, 0, SLOT);
        e.why = RETIRED_EXPIRED;
        e.flags = EVT_F_IOC;
        r.send(e);
        r.arm.on_idle();
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Clear);
        r.arm.cancel_all().unwrap();
        // An older answer does not answer the new request.
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Working);
        r.sweep_done(SWEEP_STRANDED);
        let mut s = BnEvt::new(EVT_SWEEP, 1);
        s.flags = EVT_F_SWEEP_STRANDED;
        s.a = 2;
        r.send(s);
        r.arm.on_idle();
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Stranded);
        assert_eq!(r.arm.counters().gw[GW_SWEEP_LEFT], 2, "the report's count");
    }

    /// N-b: a stranded sweep is reported as Stranded even while orders of
    /// ours are still in flight (the router asks again); Clear still needs
    /// none. Break-and-watch: testing `in_flight` before `sweep_stranded`
    /// reports Working and fails the assertion.
    #[test]
    fn a_stranded_sweep_is_reported_with_orders_in_flight() {
        let mut r = rig_with(true);
        r.healthy();
        r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 5)).unwrap();
        r.arm.cancel_all().unwrap();
        r.sweep_done(SWEEP_STRANDED);
        assert_eq!(r.arm.in_flight(), 1);
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Stranded);
        // The anchor flag rides the pulse (F2).
        assert!(!r.arm.anchor_unsaved());
        let mut p = BnEvt::new(EVT_STATUS, core_time::now_ns());
        p.flags = EVT_F_ORDER_UP | EVT_F_USER_UP | EVT_F_CLOCK_OK | EVT_F_DEADMAN_OK | EVT_F_ANCHOR_UNSAVED;
        r.send(p);
        r.arm.on_idle();
        assert!(r.arm.anchor_unsaved());
    }

    #[test]
    fn the_signal_carries_every_observation() {
        let mut r = rig_with(true);
        r.healthy();
        let s = r.arm.halt_signal();
        assert!(s.reconciled == 1 && s.venue_lock == 0 && s.margin_risk == 0);
        let now = core_time::now_ns();
        let mut m = BnEvt::new(EVT_MARGIN, now);
        m.product = PRODUCT_USDM;
        m.a = 650_000;
        m.b = 1_010_000_000;
        m.c = 1_000_000_000;
        r.send(m);
        let mut b = BnEvt::new(EVT_BUDGET, now);
        b.a = (now + 60_000_000_000) as i64;
        r.send(b);
        r.send(BnEvt::new(EVT_LOCK, now));
        r.arm.on_idle();
        let s = r.arm.halt_signal_for(SLOT);
        assert_eq!((s.margin_risk, s.venue_lock, s.budget_floor_breached), (1, 1, 1));
        assert_eq!((s.pnl_judged, s.pnl_delta_usd_1e6), (1, 10_000_000));
        r.send(BnEvt::new(EVT_SCAN_FAIL, now));
        r.arm.on_idle();
        assert_eq!(r.arm.halt_signal().recon_drift_usd_1e6, i64::MAX, "BX-15: unbounded drift");
    }

    #[test]
    fn not_reconciled_before_the_day_is_read() {
        let mut r = rig_with(true);
        let now = core_time::now_ns();
        let mut rc = BnEvt::new(EVT_RECON, now);
        rc.flags = EVT_F_RECONCILED;
        r.send(rc);
        r.arm.on_idle();
        assert_eq!(r.arm.halt_signal().reconciled, 0);
        assert_eq!(r.arm.venue_day_bought(SLOT as usize), None);
        let mut d = BnEvt::new(EVT_DAY, now);
        d.slot = SLOT;
        d.a = 20_700;
        d.b = 55_000_000;
        r.send(d);
        r.arm.on_idle();
        assert_eq!(r.arm.halt_signal().reconciled, 1);
        assert_eq!(r.arm.venue_day_bought(SLOT as usize), Some((20_700, 55_000_000)));
    }

    #[test]
    fn breadth_counts_symbols_with_orders_or_positions() {
        let mut r = rig_with(true);
        r.arm.knobs.max_symbols = 0;
        r.healthy();
        assert_eq!(r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 1)), Err(DispatchError::SlotDisabled));
        assert_eq!(r.arm.counters().refused[Refusal::Breadth as usize], 1);
    }

    /// The command ring filled to the brim (cancels: no governor stops
    /// them first).
    fn fill_the_ring(r: &mut Rig) {
        let mut oid = 100u64;
        while r.arm.cancel(&CancelReq::new(0, VenueId::Binance, btc(), oid)).is_ok() {
            oid += 1;
        }
        assert_eq!(oid - 100, CMD_RING as u64, "the ring is full");
    }

    /// The operator's ruling on the order flags: reduce-only passes (and
    /// rides the command), post-only passes on a maker, a post-only IoC and
    /// an unknown bit are refused and counted. Break-and-watch: without
    /// `admit_flags` the unknown bit reaches the ring.
    #[test]
    fn the_order_flags_are_honoured_or_refused() {
        let mut r = rig_with(true);
        r.healthy();
        let mut ro = order(ORDER_KIND_IOC, Side::Ask, 65_000_000_000, 10_000, 1);
        ro.flags = ORDER_FLAG_REDUCE_ONLY;
        assert_eq!(r.arm.submit(&ro), Ok(()));
        assert_eq!(r.pop_cmd().map(|c| c.flags), Some(ORDER_FLAG_REDUCE_ONLY));
        let mut po = order(ORDER_KIND_MAKER, Side::Bid, 64_000_000_000, 10_000, 2);
        po.flags = ORDER_FLAG_POST_ONLY | ORDER_FLAG_REDUCE_ONLY;
        assert_eq!(r.arm.submit(&po), Ok(()));
        assert_eq!(r.pop_cmd().map(|c| c.flags), Some(ORDER_FLAG_POST_ONLY | ORDER_FLAG_REDUCE_ONLY));
        let mut po_ioc = order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 3);
        po_ioc.flags = ORDER_FLAG_POST_ONLY;
        assert_eq!(r.arm.submit(&po_ioc), Err(DispatchError::Unsupported), "a post-only IoC");
        let mut unknown = order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 4);
        unknown.flags = 1 << 5;
        assert_eq!(r.arm.submit(&unknown), Err(DispatchError::Unsupported), "an unknown bit");
        let mut repl = order(ORDER_KIND_MAKER, Side::Bid, 64_000_000_000, 10_000, 5);
        repl.flags = 1 << 7;
        assert_eq!(r.arm.modify(&ModifyReq::new(2, repl)), Err(DispatchError::Unsupported), "a modify's unknown bit");
        assert_eq!(r.arm.counters().refused[Refusal::Flags as usize], 3);
        assert!(r.pop_cmd().is_none(), "nothing refused reached the ring");
    }

    /// XG-1: a halt's sweep never waits on ring room — a full command
    /// ring refuses orders, never the sweep. Break-and-watch: a sweep
    /// pushed as a command is refused here and never reaches the gateway.
    #[test]
    fn a_full_command_ring_still_takes_the_sweep() {
        let mut r = rig_with(true);
        r.healthy();
        fill_the_ring(&mut r);
        assert_eq!(r.arm.counters().refused[Refusal::QueueFull as usize], 1);
        let asked = r.gw.stats.sweep_requested();
        assert_eq!(r.arm.cancel_all(), Ok(()));
        assert_eq!(r.gw.stats.sweep_requested(), asked + 1, "the request is in the word");
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Working);
    }

    /// XG-2 / XG-5: the gateway's halt word halts the arm's slot. Break-
    /// and-watch: without `with_gateway_halted`, the signal stays clean.
    #[test]
    fn the_gateways_halt_word_rides_the_signal() {
        let mut r = rig_with(true);
        r.healthy();
        assert_eq!(r.arm.halt_signal().gateway_halted, 0);
        r.gw.stats.halt(HALT_VENUE_BASE);
        assert_eq!(r.arm.halt_signal().gateway_halted, 1);
        assert_eq!(r.arm.halt_signal_for(SLOT).gateway_halted, 1);
    }

    /// The tallies are the gateway's published words, read on its pulse;
    /// and every command carries its push time (`push_to_write`).
    #[test]
    fn the_tallies_arrive_with_the_pulse_and_commands_carry_their_push_time() {
        let mut r = rig_with(true);
        r.gw.stats.publish(GW_FILLS_BOOKED, 7);
        r.gw.stats.publish(GW_QUIET, 2);
        assert_eq!(r.arm.counters().gw[GW_FILLS_BOOKED], 0, "not before a pulse");
        r.healthy();
        assert_eq!((r.arm.counters().gw[GW_FILLS_BOOKED], r.arm.counters().gw[GW_QUIET]), (7, 2));
        let t0 = core_time::now_ns();
        r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 1)).unwrap();
        r.arm.cancel(&CancelReq::new(0, VenueId::Binance, btc(), 1)).unwrap();
        let (p, c) = (r.pop_cmd().unwrap(), r.pop_cmd().unwrap());
        assert!(p.pushed_ns >= t0 && c.pushed_ns >= p.pushed_ns, "{} {}", p.pushed_ns, c.pushed_ns);
    }

    /// The shutdown asks for the sweep through the word and ends the
    /// gateway with the verb; with the ring full, the sweep still goes
    /// (counted). Break-and-watch: a shutdown that only pushes the verb
    /// leaves the word untouched here.
    #[test]
    fn the_shutdown_sweeps_through_the_word_even_with_a_full_ring() {
        let mut r = rig_with(true);
        r.healthy();
        fill_the_ring(&mut r);
        let asked = r.gw.stats.sweep_requested();
        // The gateway's answer to the request on_shutdown is about to make
        // (it would come from the gateway thread): the wait ends at once.
        r.gw.stats.set_sweep(asked + 1, SWEEP_CLEAR);
        let t0 = core_time::now_ns();
        r.arm.on_shutdown();
        assert!(core_time::now_ns() - t0 < SHUTDOWN_WAIT_NS, "answered: no wait");
        assert_eq!(r.gw.stats.sweep_requested(), asked + 1);
        assert_eq!(r.arm.counters().cancel_all_unqueued, 1, "the verb found no room");
    }

    /// BX11: the arm's report reads its counters, the last pulse (its
    /// flags, its clock offset and the orders in doubt), the gateway's
    /// published tallies and weight, the governor's windows, the margin
    /// book and the boot's knobs; and the published traps read what they
    /// say — `sent_unanswered` the orders put in doubt (not every order in
    /// flight), `sweep_stalled` the stranded sweeps, `budget_remaining` the
    /// weight's headroom, `refused_stale` a row bound but not live (never
    /// an unbound one). Break-and-watch: `sent_unanswered` from `in_flight`
    /// reads 1, not 2; `refused_stale` with `Unbound` reads 1, not 0.
    #[test]
    fn the_report_reads_the_arm_the_pulse_and_the_gateway() {
        let mut r = rig_with(true);
        r.healthy();
        r.arm.submit(&order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 5)).unwrap();
        let mut stray = order(ORDER_KIND_IOC, Side::Bid, 65_000_000_000, 10_000, 6);
        stray.sym = make_symbol_id(VenueId::Binance, 999);
        assert_eq!(r.arm.submit(&stray), Err(DispatchError::NoLiveRoute));
        r.gw.stats.publish(GW_IN_DOUBT_RAISED, 2);
        r.gw.stats.publish(GW_JOURNAL_DROPPED, 3);
        r.gw.stats.publish(crate::gateway::PW_WEIGHT, 100);
        let now = core_time::now_ns();
        let mut p = BnEvt::new(EVT_STATUS, now);
        p.flags = EVT_F_ORDER_UP | EVT_F_USER_UP | EVT_F_CLOCK_OK | EVT_F_DEADMAN_OK | EVT_F_ANCHOR_UNSAVED;
        p.b = -12;
        p.c = 1;
        r.send(p);
        let mut m = BnEvt::new(EVT_MARGIN, now);
        m.product = PRODUCT_USDM;
        m.a = 250_000;
        r.send(m);
        r.arm.on_idle();
        r.arm.cancel_all().unwrap();
        r.sweep_done(SWEEP_STRANDED);
        let mut sw = BnEvt::new(EVT_SWEEP, now);
        sw.flags = EVT_F_SWEEP_STRANDED;
        r.send(sw);
        r.arm.on_idle();
        let mut rep = BinanceArmReport::default();
        assert!(r.arm.binance_arm_report(core_time::now_ns(), &mut rep));
        assert_eq!((rep.present, rep.slot), (1, u64::from(SLOT)));
        assert_eq!((rep.in_doubt_raised, rep.journal_dropped, rep.in_doubt, rep.in_flight), (2, 3, 1, 1));
        assert_eq!((rep.order_up, rep.user_up, rep.deadman_ok, rep.clock_ok, rep.anchor_unsaved), (1, 1, 1, 1, 1));
        assert_eq!(rep.clock_offset_ms, -12);
        assert_eq!((rep.weight_used, rep.weight_max), (100, ARM_WEIGHT_MAX_PER_MIN));
        assert_eq!((rep.orders_10s_used, rep.orders_1m_used), (1, 1), "the IoC placed");
        assert!(rep.orders_10s_cap > 0 && rep.orders_1m_cap > rep.orders_10s_cap);
        assert_eq!(rep.margin_usdm_1e6, 250_000);
        assert_eq!((rep.sweep, rep.sweeps_stranded), (CancelAllState::Stranded as u64, 1));
        assert_eq!((rep.owned_rows, rep.account_mode, rep.demo), (1, AccountMode::Classic as u64, 0));
        assert_eq!(rep.refused[Refusal::Unbound as usize], 1);
        assert!(rep.heartbeat_age_ms == u64::MAX, "the test's gateway never beat");
        let lc = r.arm.arm_counters();
        assert_eq!(lc.sent_unanswered, 2, "the orders put in doubt, not the one in flight");
        assert_eq!((lc.sweep_stalled, lc.refused_stale, lc.refused_local), (1, 0, 1));
        assert_eq!(lc.budget_remaining, ARM_WEIGHT_MAX_PER_MIN as i64 - 100);
        // The alloc review's AL-5: one latency walk a report, round robin —
        // the six quantiles all read within six reports, never seven walks
        // in one.
        r.gw.stats.post_rtt.record(2_000_000);
        r.gw.stats.cancel_rtt.record(3_000_000);
        r.gw.stats.fill_venue_to_local.record(4_000_000);
        r.gw.stats.rest_rtt.record(5_000_000);
        r.gw.stats.push_to_write.record(6_000);
        let mut i = 0;
        while i < LAT_QUANTILES {
            assert!(r.arm.binance_arm_report(core_time::now_ns(), &mut rep));
            i += 1;
        }
        let lat = [rep.post_rtt_p50_ns, rep.post_rtt_p99_ns, rep.cancel_rtt_p99_ns, rep.fill_lag_p99_ns, rep.rest_rtt_p99_ns, rep.push_to_write_p99_ns];
        assert!(lat.iter().all(|&q| q > 0), "every quantile read: {lat:?}");
        assert!(rep.post_rtt_p99_ns >= 2_000_000 && rep.cancel_rtt_p99_ns >= 3_000_000, "{lat:?}");
    }
}
