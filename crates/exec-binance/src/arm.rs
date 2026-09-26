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
//!    `i128`: no division on the submit path);
//! 4. **the governors** ([`crate::gov`]): the ORDERS windows, the UM
//!    quantitative rules, the open orders per symbol and the breadth;
//! 5. **the push**: one [`BnCmd`] into the command ring. A full ring is
//!    `QueueFull` and the router books nothing.
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
//! counted, so a halt's `clear_resting` never drops an in-flight exit.
//!
//! Error mapping: not bound, not live or not ours → `NoLiveRoute`; a verb
//! or kind this product does not take → `Unsupported`; a BX-5 refusal →
//! `Unsupported`; a governor → `SlotDisabled`; the gateway not ready →
//! `Disconnected`; a full ring → `QueueFull`.

use clob_dispatcher::{
    CancelAllState, DispatchError, DispatchStats, HaltSignal, LiveArmCounters, OrderDispatch,
    Renamed, Retired, RETIRED_EXPIRED, RETIRED_REJECTED,
};
use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_ring::{Consumer, Producer};
use core_time::WallAnchor;
use core_types::{CancelReq, ChannelEvent, ChannelId, ModifyReq, Order, Side, SymbolId, VenueId};

use crate::cmd::*;
use crate::gov::{classify, CodeClass, FuturesOrders, Qtr, QtrRow, QTR_DUST_USD_1E6};
use crate::inst::{rx, InstTable, INST_INVERSE, INST_LIVE, INST_MAX, INST_NO_DEADMAN, ROW_NONE};
use crate::margin::{MarginBook, SLOTS};
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
}

/// Refusal reasons.
pub const REFUSALS: usize = 19;
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
    /// Sweeps requested that the ring could not take.
    pub cancel_all_unqueued: u64,
    /// Local refusals, by [`Refusal`].
    pub refused: [u64; REFUSALS],
    /// The gateway's tallies (fills booked, foreign, unowned, dropped,
    /// unresolved; recon ok / failed / drift legs / unseen legs; stream
    /// reconnects, connect failures; sweep orders left).
    pub gw: [u64; GW_TALLIES],
}

/// The gateway tallies mirrored in [`BnArmCounters::gw`] (groups of three,
/// one group per `EVT_TALLY`).
pub const GW_TALLIES: usize = 18;
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
    sweep_pending: bool,
    sweep_stranded: bool,
}

/// **The arm** (module docs).
pub struct BnArm {
    cmd: Producer<BnCmd, CMD_RING>,
    evt: Consumer<BnEvt, EVT_RING>,
    inst: InstTable,
    rt: Box<[RowRt; INST_MAX]>,
    qtr_rows: Box<[QtrRow; INST_MAX]>,
    fut: FuturesOrders,
    qtr: Qtr,
    knobs: ArmKnobs,
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
}

/// `⌊a · b / 1e6⌋ < m` for `a, b ≥ 0`, without the division:
/// `⌊x / d⌋ < m ⇔ x < m · d`.
#[inline(always)]
const fn scaled_lt(a: i64, b: i64, m: i64) -> bool {
    (a as i128) * (b as i128) < (m as i128) * 1_000_000
}

impl BnArm {
    /// The arm over its two ring ends and its boot tables.
    #[must_use]
    pub fn new(
        cmd: Producer<BnCmd, CMD_RING>,
        evt: Consumer<BnEvt, EVT_RING>,
        inst: InstTable,
        knobs: ArmKnobs,
    ) -> Self {
        let margin = knobs.margin.clone();
        Self {
            cmd,
            evt,
            rt: Box::new([RowRt::default(); INST_MAX]),
            qtr_rows: Box::new([QtrRow::default(); INST_MAX]),
            inst,
            fut: FuturesOrders::new(knobs.orders_frac_1e6),
            qtr: Qtr {
                frac_1e6: knobs.qtr_frac_1e6,
            },
            margin,
            knobs,
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
        }
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

    /// Open-order count change on `row`, keeping the breadth and QTR `N`.
    #[inline(always)]
    fn open_delta(&mut self, row: u16, up: bool) {
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
        self.rows_open = (self.rows_open + is_open as u32).saturating_sub(was_open as u32);
        let is_active = self.active(row);
        self.active_rows = (self.active_rows + is_active as u32).saturating_sub(was_active as u32);
    }

    /// Quantize and check the venue law for an order on `row` (BX-5).
    /// Returns (price, quantity, dust: the notional under the QTR's dust
    /// line). Every notional and band test cross-multiplies in `i128`.
    #[inline(always)]
    fn quantize(&mut self, row: u16, buy: bool, px: i64, qty: i64, now_ns: u64) -> Result<(i64, i64, bool), Refusal> {
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
        if px_q <= 0 {
            return Err(Refusal::Price);
        }
        if qty_q <= 0 || qty_q < h.min_qty_1e6 {
            return Err(Refusal::MinQty);
        }
        // The notional is qty × unit (COIN-M) or px × qty, ×1e6 each.
        let (na, nb) = if h.has(INST_INVERSE) { (qty_q, h.unit_1e6) } else { (px_q, qty_q) };
        if scaled_lt(na, nb, h.min_notional_1e6) {
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
            Refusal::Kind | Refusal::NoDeadman | Refusal::Verb => DispatchError::Unsupported,
            Refusal::NotReady | Refusal::DeadmanDown => DispatchError::Disconnected,
            Refusal::Price | Refusal::MinQty | Refusal::MinNotional | Refusal::MaxQty | Refusal::Band | Refusal::Ttl => {
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
        if maker && o.ttl_ns != 0 && o.ttl_ns < self.knobs.min_maker_ttl_ns {
            return Err(Refusal::Ttl);
        }
        let buy = o.side == Side::Bid;
        let (px, qty, dust) = self.quantize(row, buy, o.px.raw(), o.qty.raw(), now)?;
        // ---- governors ----------------------------------------------------
        if !self.fut.admits(now) {
            return Err(Refusal::Orders);
        }
        let r = rx(row);
        let wall_ms = self.knobs.anchor.wall_of(now) / 1_000_000;
        Qtr::roll(&mut self.qtr_rows[r], wall_ms);
        let n = self.rows_open + (self.rt[r].open == 0) as u32;
        if !self.qtr.admits(&self.qtr_rows[r], n, maker, dust) {
            return Err(Refusal::Qtr);
        }
        let max_orders = self.inst.cold(row).max_orders as i64;
        if max_orders > 0 && (self.rt[r].open as i64 + 1) * 1_000_000 > max_orders * self.knobs.orders_frac_1e6 {
            return Err(Refusal::MaxOrders);
        }
        if !self.active(row) && self.active_rows >= self.knobs.max_symbols {
            return Err(Refusal::Breadth);
        }
        // ---- push -----------------------------------------------------------
        let deadline = if maker && o.ttl_ns != 0 { now + o.ttl_ns } else { 0 };
        let cmd = BnCmd::place(o.client_oid, px, qty, deadline, row, o.kind, o.side, o.strategy_id);
        if !self.cmd.try_push_ref(&cmd) {
            return Err(Refusal::QueueFull);
        }
        self.fut.commit(now);
        Qtr::on_place(&mut self.qtr_rows[r], maker, dust);
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
            let icr = e.flags & EVT_F_ICR != 0;
            Qtr::on_end(&mut self.qtr_rows[rx(e.row)], filled, ioc, icr);
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
                // Only the VENUE's refusals (negative codes) move the
                // streak; the gateway's local ones (positive) are races the
                // arm's own readiness check already counts.
                match class {
                    CodeClass::WouldTake | CodeClass::Budget | CodeClass::Lock => {}
                    CodeClass::RowFatal => {
                        self.inst.retire_row(e.row);
                        self.reject_streak += 1;
                    }
                    CodeClass::Reject | CodeClass::Clock | CodeClass::Unknown => {
                        self.reject_streak += (e.code < 0) as u32;
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
                if e.code < 0 && classify(e.code, 0) == CodeClass::Reject {
                    self.reject_streak += 1;
                }
            }
            EVT_STATUS => {
                self.seen.status_ns = e.ts_ns;
                self.seen.gap_ns = e.a as u64;
                self.seen.status_flags = e.flags;
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
                self.seen.sweep_pending = false;
                self.seen.sweep_stranded = e.flags & EVT_F_SWEEP_STRANDED != 0;
                self.c.gw[GW_SWEEP_LEFT] = e.a as u64;
            }
            EVT_DAY => {
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
            EVT_POSITION => {
                if (e.row as usize) < self.inst.len() {
                    let was = self.active(e.row);
                    self.rt[rx(e.row)].pos_1e6 = e.a;
                    let is = self.active(e.row);
                    self.active_rows = (self.active_rows + is as u32).saturating_sub(was as u32);
                }
            }
            EVT_TALLY => {
                let base = e.why as usize * 3;
                let v = [e.a, e.b, e.c];
                let mut i = 0;
                while i < 3 {
                    if base + i < GW_TALLIES {
                        self.c.gw[base + i] = v[i] as u64;
                    }
                    i += 1;
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
        if self.inst.hot(row).has(INST_NO_DEADMAN) {
            // IoC-only: nothing rests, so no cancel verb is built (§3.8).
            self.refuse(Refusal::Verb);
            return Err(DispatchError::Unsupported);
        }
        if !self.cmd.try_push_ref(&BnCmd::cancel(req.client_oid, row, req.strategy_id)) {
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
        let buy = o.side == Side::Bid;
        let (px, qty, _) = match self.quantize(row, buy, o.px.raw(), o.qty.raw(), now) {
            Ok(v) => v,
            Err(r) => return Err(self.refused(r)),
        };
        // A modify is an order to the venue's ORDERS count: governed alike.
        if !self.fut.admits(now) {
            return Err(self.refused(Refusal::Orders));
        }
        let cmd = BnCmd::modify(o.client_oid, req.prev_client_oid(), px, qty, 0, row, o.side, o.strategy_id);
        if !self.cmd.try_push_ref(&cmd) {
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
            let Some(g) = self.evt.try_pop_ref() else {
                break;
            };
            // COPY: one event (64 B) off the ring — the slot is released
            // before `apply` borrows the whole arm; applying in place would
            // hold `self.evt` across it.
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

    fn cancel_all(&mut self) -> Result<(), DispatchError> {
        if !self.cmd.try_push_ref(&BnCmd::sweep(VERB_CANCEL_ALL)) {
            self.c.cancel_all_unqueued += 1;
            return Err(DispatchError::QueueFull);
        }
        self.seen.sweep_pending = true;
        self.seen.sweep_stranded = false;
        Ok(())
    }

    fn cancel_all_state(&self) -> CancelAllState {
        // (N-b) Stranded wins over orders in flight: see
        // `a_stranded_sweep_is_reported_with_orders_in_flight`.
        // A stranded sweep is reported as such even with orders in flight
        // (the router asks again; each ask is a fresh sweep, its REST
        // fallback included); Clear still needs nothing in flight.
        if self.seen.sweep_pending {
            CancelAllState::Working
        } else if self.seen.sweep_stranded {
            CancelAllState::Stranded
        } else if self.in_flight > 0 {
            CancelAllState::Working
        } else {
            CancelAllState::Clear
        }
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
            refused_stale: c.refused[Refusal::Unbound as usize] + c.refused[Refusal::NotLive as usize],
            sent_unanswered: self.in_flight as u64,
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
            cancel_all_unqueued: c.cancel_all_unqueued,
            ws_reconnects: c.gw[GW_RECONNECTS],
            ws_connect_failures: c.gw[GW_CONNECT_FAILURES],
            pnl_anchor_usd_1e6: self.seen.anchor_1e6,
            session_pnl_usd_1e6: self.seen.equity_1e6.saturating_sub(self.seen.anchor_1e6),
            ..LiveArmCounters::default()
        }
    }

    /// BX6: `/state` `exec.arms.binance`.
    fn venue_arm_counters(&self, venue: u8) -> Option<LiveArmCounters> {
        (venue == VenueId::Binance as u8).then(|| self.arm_counters())
    }

    fn on_shutdown(&mut self) {
        if !self.cmd.try_push_ref(&BnCmd::sweep(VERB_SHUTDOWN)) {
            self.c.cancel_all_unqueued += 1;
            return;
        }
        self.seen.sweep_pending = true;
        let start = core_time::now_ns();
        while self.seen.sweep_pending && core_time::now_ns().saturating_sub(start) < SHUTDOWN_WAIT_NS {
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
    use crate::inst::{BindSpec, PRODUCT_USDM};
    use core_ring::Ring;
    use core_types::{make_symbol_id, Price, Qty};

    pub(crate) const SLOT: u8 = 2;

    pub(crate) fn btc() -> core_types::SymbolId {
        make_symbol_id(VenueId::Binance, 513)
    }

    pub(crate) struct Rig {
        pub arm: BnArm,
        pub cmd_rx: Consumer<BnCmd, CMD_RING>,
        pub evt_tx: Producer<BnEvt, EVT_RING>,
    }

    pub(crate) fn rig_with(maker_ok: bool) -> Rig {
        let d = fapi_rows(FAPI);
        let (mut t, mut w) = InstTable::new(7);
        let row = d.find(b"BTCUSDT").unwrap();
        t.bind(&mut w, &BindSpec { sym: btc(), product: PRODUCT_USDM, row, owned: true, maker_ok }).unwrap();
        let (ctx, crx) = Ring::<BnCmd, CMD_RING>::new().split();
        let (etx, erx) = Ring::<BnEvt, EVT_RING>::new().split();
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
        };
        Rig { arm: BnArm::new(ctx, erx, t, knobs), cmd_rx: crx, evt_tx: etx }
    }

    impl Rig {
        pub(crate) fn send(&mut self, e: BnEvt) {
            assert!(self.evt_tx.try_push_ref(&e));
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
            self.cmd_rx.try_pop_ref().map(|g| *g)
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
        let mut s = BnEvt::new(EVT_SWEEP, 1);
        s.flags = EVT_F_SWEEP_DONE;
        r.send(s);
        r.arm.on_idle();
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Working, "still in flight");
        let mut e = BnEvt::order(EVT_RETIRED, 1, 5, 0, SLOT);
        e.why = RETIRED_EXPIRED;
        e.flags = EVT_F_IOC;
        r.send(e);
        r.arm.on_idle();
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Clear);
        r.arm.cancel_all().unwrap();
        let mut s = BnEvt::new(EVT_SWEEP, 1);
        s.flags = EVT_F_SWEEP_STRANDED;
        s.a = 2;
        r.send(s);
        r.arm.on_idle();
        assert_eq!(r.arm.cancel_all_state(), CancelAllState::Stranded);
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
        let mut s = BnEvt::new(EVT_SWEEP, 1);
        s.flags = EVT_F_SWEEP_STRANDED;
        s.a = 1;
        r.send(s);
        r.arm.on_idle();
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
}
