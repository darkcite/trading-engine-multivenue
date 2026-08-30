// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # backtest::fill — the §4 fill / fee / latency model + accounting
//! (design §4, H-D1 LOCKED: strict-cross maker, zero RNG)
//!
//! Doctrine: **the backtest may under-promise; it must never
//! over-promise.** Every rounding direction and every modeling choice
//! in this module leans conservative:
//!
//! * **Strict-cross maker fills (§4.2):** a resting BID at `P` fills
//!   only when the opposite best trades STRICTLY THROUGH the level
//!   (`ask_px < P`), never on a touch — queue-ahead on a touched level
//!   is assumed infinite. Fill price is `P` (a maker never improves);
//!   fill qty is capped by the displayed opposite size, and that
//!   displayed size is a SHARED budget across all our resting orders
//!   of the sym at that tick (two of our orders can never double-claim
//!   one printed size), consumed in emit order (FIFO). Partials rest.
//! * **Activation Δ (§4.4/§4.1):** an order cannot fill before
//!   `t_active = t_emit + Δ_venue`. An order can never fill on its own
//!   emitting tick regardless of Δ (the fill pass for a record runs
//!   before the vm sees the tick and emits).
//! * **Open-order caps (§4.1):** the risk-policy structural caps
//!   (max 4 open per symbol, 32 total) are modeled even though
//!   `Engine::on_new_order` does not exist yet. A beyond-cap emit is
//!   counted and DROPPED — conservative vs today's paper engine,
//!   which has no such wall. The vm still sees `Ok` from `submit`
//!   (production-faithful: today's engine never refuses, so cooldown
//!   stamping must match production), the harness just refuses to let
//!   the dropped order ever fill.
//! * **Fees (§4.3):** maker bps on fill notional, rounded UP (a fee
//!   never rounds in our favor). All harness fills are maker fills by
//!   construction (post-only model); the taker column exists for §4.3
//!   table completeness and future models.
//! * **Fixed-point (§4.5):** prices/qtys are i64 ×1e6; notionals,
//!   costs, realized P&L, fees and equity are i128 ×1e12 (px×qty).
//!   Floats appear NOWHERE; ×1e12 → ×1e6 happens once at render, with
//!   conservative direction (net floors, drawdown/bounds ceil).
//! * **Per-venue independent fill clocks (§4.7, door-closers 16.3.2 /
//!   16.3.6):** everything is keyed by namespaced `SymbolId` (venue
//!   byte in bits 31..24); an order on ANY venue byte is accepted and
//!   evaluated against its own sym's ticks with its own venue Δ. No
//!   cross-venue serialization exists anywhere in this module.
//!
//! ## Split bucketing (§3.4)
//!
//! Replay is continuous; accounting is bucketed. Every order is tagged
//! at intake: `oos = (emit virt ts >= boundary)`. TWO position books
//! run over the same fill stream:
//!
//! * the **full book** (every fill, IS + OOS) exists for the §4.6
//!   `bounds` maxima — a breach anywhere in the window disqualifies;
//! * the **OOS book** (fills of OOS-emitted orders only) produces
//!   `oos.net_pnl / trades / trading_days / max_drawdown` — the P&L of
//!   a strategy that starts flat at the boundary. IS-emitted orders
//!   warm the vm and the books but never leak into the OOS verdict,
//!   even when their fills land after the boundary (§3.4: "orders
//!   emitted with virt ts in the OOS window, and their fills/marks").
//!
//! Equity(book) = realized − fees + Σ unrealized, sampled on every
//! fill of the book and on every tick of a sym the book holds; the
//! average-cost `removed` quantum is subtracted from the open cost AND
//! added to realized with the SAME truncated value, so
//! `realized + unrealized` is EXACTLY cash-conserving regardless of
//! the truncation (the §12 conservation proptest pins the identity).
//! End-of-replay mark-out at last mid is therefore literally the final
//! equity value (liquidation-at-mark, stated on stderr).
//!
//! ## Doctrine note — offline path
//!
//! `audit_replay.rs` doctrine: never loaded by the engine loop; the
//! books use `BTreeMap` (deterministic iteration) and may allocate.
//! The open-order table itself is a fixed `[OpenOrder; 32]` — the §4.1
//! preallocation promise — and the per-record scratch is reused.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use core_types::{symbol_venue_byte, Order, Side, Tick, SYMBOL_ID_NONE};

use crate::backtest::ModelParams;

/// §4.1 cap: max open orders per symbol (risk-policy mirror).
// Operator ruling 2026-08-29 ($50k tier): 4 -> 8.
pub const MAX_OPEN_PER_SYM: usize = 8;
/// §4.1 cap: max total open orders (risk-policy mirror).
// Operator ruling 2026-08-29 ($50k tier): 32 -> 64.
pub const MAX_OPEN_TOTAL: usize = 64;
/// WS9: number of TRADEABLE venues (pm, bn, okx, deribit, hl,
/// bybit). NOT a venue-byte bound any more — `Ai = 5` sits inside
/// the byte range while `Bybit = 6` trades; use
/// [`tradeable_venue_byte`] for the per-order gate.
pub const TRADEABLE_VENUES: usize = 6;

/// WS9: the per-order venue gate — venue bytes 0..=4 plus Bybit (6)
/// can execute; the Ai feed (5) and corrupt bytes cannot.
#[inline]
pub const fn tradeable_venue_byte(venue: usize) -> bool {
    venue <= 4 || venue == 6
}
/// ns per UTC day (§4.5 `trading_days` bucketing, §3.3 wall mapping).
pub const DAY_NS: u64 = 86_400_000_000_000;

// ---------------------------------------------------------------
// Fixed-point conversion helpers (×1e12 → ×1e6, direction explicit)
// ---------------------------------------------------------------

/// Floor an ×1e12 value to ×1e6 (toward −∞): the NET P&L direction —
/// profit is never overstated, a loss never understated.
#[inline]
pub fn usd_1e12_to_1e6_floor(v_1e12: i128) -> i64 {
    let q = v_1e12.div_euclid(1_000_000);
    debug_assert!(i64::try_from(q).is_ok(), "1e6-scaled USD out of i64");
    q as i64
}

/// Ceil an ×1e12 value to ×1e6 (toward +∞): the RISK direction —
/// drawdown and notional bounds are never understated.
#[inline]
pub fn usd_1e12_to_1e6_ceil(v_1e12: i128) -> i64 {
    let q = -((-v_1e12).div_euclid(1_000_000));
    debug_assert!(i64::try_from(q).is_ok(), "1e6-scaled USD out of i64");
    q as i64
}

/// §4.3 fee on a fill: `ceil(notional_1e12 × bps / 10_000)` — a fee
/// never rounds in our favor. `notional` is ≥ 0 by construction
/// (px > 0, qty > 0).
#[inline]
fn fee_ceil_1e12(notional_1e12: i128, bps: u32) -> i128 {
    debug_assert!(notional_1e12 >= 0);
    (notional_1e12 * bps as i128 + 9_999) / 10_000
}

// ---------------------------------------------------------------
// Open-order table (§4.1)
// ---------------------------------------------------------------

/// One resting order. POD; the table is a fixed array in emit order
/// (`seq` strictly increases with insertion, compaction preserves
/// order), which IS the FIFO fill priority.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct OpenOrder {
    /// Emit sequence (FIFO priority; unique).
    pub seq: u64,
    /// Virtual activation time: `t_emit + Δ_venue` (§4.4).
    pub t_active_ns: u64,
    /// Namespaced symbol (venue byte inside — door-closer 16.3.2).
    pub sym: u32,
    /// Resting side.
    pub side: Side,
    /// Venue byte (== `symbol_venue_byte(sym)`; kept for Δ/fee lookup).
    pub venue: u8,
    /// §3.4 bucket tag: emitted at/after the boundary.
    pub oos: bool,
    /// Resting limit price ×1e6.
    pub px_1e6: i64,
    /// Unfilled remainder ×1e6 (> 0 while resting).
    pub remaining_1e6: i64,
    /// The vm's idempotency key — echoed into synthesized fills.
    pub client_oid: u64,
}

const EMPTY_OPEN: OpenOrder = OpenOrder {
    seq: 0,
    t_active_ns: 0,
    sym: SYMBOL_ID_NONE,
    side: Side::Bid,
    venue: 0,
    oos: false,
    px_1e6: 0,
    remaining_1e6: 0,
    client_oid: 0,
};

/// D-7 half-spread (VM2 V5): `max(0.5% of mark, 1 tick)` ×1e6 —
/// flat per venue by ruling; the 1-tick floor stands in for the
/// "1 IV-tick equivalent" at these mark scales (documented
/// implementation choice, printed with every report that used it).
#[inline]
pub fn mark_half_spread_1e6(mark_1e6: i64) -> i64 {
    (mark_1e6 / 200).max(1)
}

/// One synthesized fill (§3.6.4 feedback + accounting input).
#[derive(Copy, Clone, Debug)]
pub struct SynthFill {
    /// Filled symbol.
    pub sym: u32,
    /// OUR side of the trade (the resting order's side).
    pub side: Side,
    /// Fill price ×1e6 — always the resting price `P`.
    pub px_1e6: i64,
    /// Fill quantity ×1e6.
    pub qty_1e6: i64,
    /// Fee charged ×1e12 (§4.3).
    pub fee_1e12: i128,
    /// §3.4 bucket of the ORDER that filled.
    pub oos: bool,
    /// The filled order's `client_oid`.
    pub client_oid: u64,
}

// ---------------------------------------------------------------
// Position books (§4.5)
// ---------------------------------------------------------------

/// Per-sym position state. `cost_1e12` is the signed open cost
/// (long ⇒ ≥ 0 cash spent, short ⇒ ≤ 0 cash received); average cost
/// is `cost / qty` implicitly — never materialized, so no division
/// drift accumulates.
#[derive(Copy, Clone, Debug, Default)]
struct PosEntry {
    qty_1e6: i64,
    cost_1e12: i128,
    realized_1e12: i128,
    fees_1e12: i128,
    fills: u64,
}

/// One accounting scope (full-window or OOS). Running sums are
/// maintained incrementally with exact integer deltas.
#[derive(Debug, Default)]
struct Book {
    entries: BTreeMap<u32, PosEntry>,
    realized_sum_1e12: i128,
    fees_sum_1e12: i128,
    unreal_sum_1e12: i128,
}

impl Book {
    /// equity = realized − fees + Σ unrealized (§4.5).
    #[inline]
    fn equity_1e12(&self) -> i128 {
        self.realized_sum_1e12 - self.fees_sum_1e12 + self.unreal_sum_1e12
    }

    /// Mark move for `sym`: adjust the running unrealized sum.
    /// Returns true when the book holds the sym (caller samples).
    fn on_mark(&mut self, sym: u32, old_mark_1e6: i64, new_mark_1e6: i64) -> bool {
        match self.entries.get(&sym) {
            Some(e) if e.qty_1e6 != 0 => {
                self.unreal_sum_1e12 +=
                    e.qty_1e6 as i128 * (new_mark_1e6 as i128 - old_mark_1e6 as i128);
                true
            }
            _ => false,
        }
    }

    /// Apply one fill at the CURRENT mark of `sym` (§4.5): average-cost
    /// realization on reducing fills, exact open/extend otherwise; the
    /// truncated `removed` quantum leaves `cost` and enters `realized`
    /// as the SAME value, so `realized + unrealized` stays exactly
    /// cash-conserving (module docs).
    fn apply_fill(
        &mut self,
        sym: u32,
        side: Side,
        px_1e6: i64,
        qty_1e6: i64,
        fee_1e12: i128,
        mark_1e6: i64,
    ) {
        debug_assert!(px_1e6 > 0 && qty_1e6 > 0 && fee_1e12 >= 0);
        let e = self.entries.entry(sym).or_default();
        let pre_contrib = e.qty_1e6 as i128 * mark_1e6 as i128 - e.cost_1e12;

        let px = px_1e6 as i128;
        let mut q_signed: i128 = match side {
            Side::Bid => qty_1e6 as i128,
            Side::Ask => -(qty_1e6 as i128),
        };
        let mut realized_delta: i128 = 0;
        while q_signed != 0 {
            let pos = e.qty_1e6 as i128;
            if pos == 0 || (pos > 0) == (q_signed > 0) {
                // Open / extend: exact, no division.
                e.cost_1e12 += px * q_signed;
                e.qty_1e6 += q_signed as i64;
                q_signed = 0;
            } else {
                // Reduce (possibly through zero — loop handles the rest).
                let c = q_signed.abs().min(pos.abs());
                let qc = if q_signed > 0 { c } else { -c };
                let removed = e.cost_1e12 * c / pos.abs();
                realized_delta += -(px * qc) - removed;
                e.cost_1e12 -= removed;
                e.qty_1e6 += qc as i64;
                q_signed -= qc;
            }
        }
        e.realized_1e12 += realized_delta;
        e.fees_1e12 += fee_1e12;
        e.fills += 1;

        let post_contrib = e.qty_1e6 as i128 * mark_1e6 as i128 - e.cost_1e12;
        self.realized_sum_1e12 += realized_delta;
        self.fees_sum_1e12 += fee_1e12;
        self.unreal_sum_1e12 += post_contrib - pre_contrib;
    }
}

// ---------------------------------------------------------------
// OOS drawdown (§4.5) + §4.6 bounds trackers
// ---------------------------------------------------------------

/// Max peak-to-trough of the OOS equity curve. Peak starts at 0: the
/// OOS book starts flat at the boundary, so a strategy that only
/// loses shows the full loss as drawdown.
#[derive(Copy, Clone, Debug)]
struct DdTracker {
    peak_1e12: i128,
    max_dd_1e12: i128,
}

impl DdTracker {
    const fn new() -> Self {
        Self {
            peak_1e12: 0,
            max_dd_1e12: 0,
        }
    }

    #[inline]
    fn sample(&mut self, equity_1e12: i128) {
        if equity_1e12 > self.peak_1e12 {
            self.peak_1e12 = equity_1e12;
        } else {
            let dd = self.peak_1e12 - equity_1e12;
            if dd > self.max_dd_1e12 {
                self.max_dd_1e12 = dd;
            }
        }
    }
}

/// §4.6 observed maxima over the FULL window: peak per-sym
/// `|position| × mark` and peak Σ over syms, updated on every
/// full-book qty change and every mark move of a held sym.
#[derive(Debug, Default)]
struct BoundsTracker {
    per_sym_cur_1e12: BTreeMap<u32, i128>,
    total_cur_1e12: i128,
    max_symbol_1e12: i128,
    max_total_1e12: i128,
}

impl BoundsTracker {
    fn set_sym(&mut self, sym: u32, abs_notional_1e12: i128) {
        debug_assert!(abs_notional_1e12 >= 0);
        let old = self
            .per_sym_cur_1e12
            .insert(sym, abs_notional_1e12)
            .unwrap_or(0);
        self.total_cur_1e12 += abs_notional_1e12 - old;
        if abs_notional_1e12 > self.max_symbol_1e12 {
            self.max_symbol_1e12 = abs_notional_1e12;
        }
        if self.total_cur_1e12 > self.max_total_1e12 {
            self.max_total_1e12 = self.total_cur_1e12;
        }
    }
}

// ---------------------------------------------------------------
// Outcome surfaces
// ---------------------------------------------------------------

/// Scalar results of the model after `finish` (×1e12 kept exact; the
/// render layer converts with explicit direction).
#[derive(Copy, Clone, Debug)]
pub struct ModelOutcome {
    /// OOS equity at end = realized − fees + mark-out (§4.5).
    pub oos_net_1e12: i128,
    /// OOS realized component (stderr/sidecar surface).
    pub oos_realized_1e12: i128,
    /// OOS fees paid.
    pub oos_fees_1e12: i128,
    /// OOS unrealized at last marks (== the end mark-out).
    pub oos_unreal_1e12: i128,
    /// OOS max peak-to-trough drawdown.
    pub oos_max_dd_1e12: i128,
    /// OOS fill count (`oos.trades`; a partial fill is one trade).
    pub oos_trades: u64,
    /// Distinct UTC days with ≥ 1 OOS fill (`oos.trading_days`).
    pub oos_trading_days: u64,
    /// Full-book realized / fees / unrealized (sidecar surfaces).
    pub full_realized_1e12: i128,
    /// Full-book fees.
    pub full_fees_1e12: i128,
    /// Full-book unrealized at last marks.
    pub full_unreal_1e12: i128,
    /// §4.6 peak per-sym |position|×mark, full window.
    pub max_symbol_notional_1e12: i128,
    /// §4.6 peak Σ|position|×mark, full window.
    pub max_total_notional_1e12: i128,
    /// Total synthesized fills (IS + OOS).
    pub fills_total: u64,
    /// Orders accepted into the table before the boundary.
    pub orders_is: u64,
    /// Orders accepted into the table at/after the boundary.
    pub orders_oos: u64,
    /// §4.1 beyond-cap drops (per-sym cap).
    pub rejected_sym_cap: u64,
    /// §4.1 beyond-cap drops (total cap).
    pub rejected_total_cap: u64,
    /// Orders whose venue byte has no Δ/fee row (cannot execute).
    pub unroutable: u64,
    /// Orders still resting at replay end (canceled, zero P&L).
    pub canceled_end: u64,
    /// Peak simultaneous open orders (total).
    pub peak_open_total: u64,
    /// VM2 V5 (D-7): fills executed under the OPTIONS MARK-FILL law
    /// — immediate execution at mark ± half-spread with the TAKER
    /// fee, for syms registered mark-filled (no real book exists to
    /// be maker in). > 0 obliges the caller to PRINT the assumption.
    pub mark_fills: u64,
    /// Peak simultaneous open orders on one sym.
    pub peak_open_per_sym: u64,
}

/// One per-sym row for the `--emit-detail` sidecar (full book, sorted
/// by sym — BTreeMap order).
#[derive(Copy, Clone, Debug)]
pub struct PerSymDetail {
    /// Namespaced sym.
    pub sym: u32,
    /// Venue byte of the sym.
    pub venue: u8,
    /// Final signed position ×1e6.
    pub pos_qty_1e6: i64,
    /// Last observed mid ×1e6 (0 = never marked — impossible for a
    /// sym with fills).
    pub last_mid_1e6: i64,
    /// Realized P&L ×1e12.
    pub realized_1e12: i128,
    /// Fees ×1e12.
    pub fees_1e12: i128,
    /// Fill count.
    pub fills: u64,
}

// ---------------------------------------------------------------
// The fill engine (§4)
// ---------------------------------------------------------------

/// The §4 model: open-order table + strict-cross matcher + fee/latency
/// tables + the two accounting books + trackers. Deterministic by
/// construction — zero RNG, fixed scan orders, BTree iteration only.
pub struct FillEngine {
    params: ModelParams,
    boundary_virt_ns: u64,
    seq_next: u64,
    open: [OpenOrder; MAX_OPEN_TOTAL],
    open_len: usize,
    marks_1e6: BTreeMap<u32, i64>,
    full: Book,
    oos: Book,
    bounds: BoundsTracker,
    dd: DdTracker,
    oos_days: BTreeSet<u64>,
    /// D-7 mark-fill sym class (options with a synthetic mark book).
    mark_fill_syms: BTreeSet<u32>,
    mark_fills: u64,
    fills_total: u64,
    oos_trades: u64,
    orders_is: u64,
    orders_oos: u64,
    rejected_sym_cap: u64,
    rejected_total_cap: u64,
    unroutable: u64,
    canceled_end: u64,
    peak_open_total: u64,
    peak_open_per_sym: u64,
}

impl FillEngine {
    /// A fresh model for one replay: §4.3/§4.4 tables + the §3.4
    /// boundary that buckets order intake.
    pub fn new(params: ModelParams, boundary_virt_ns: u64) -> Self {
        Self {
            params,
            boundary_virt_ns,
            seq_next: 0,
            open: [EMPTY_OPEN; MAX_OPEN_TOTAL],
            open_len: 0,
            marks_1e6: BTreeMap::new(),
            full: Book::default(),
            oos: Book::default(),
            bounds: BoundsTracker::default(),
            dd: DdTracker::new(),
            oos_days: BTreeSet::new(),
            mark_fill_syms: BTreeSet::new(),
            mark_fills: 0,
            fills_total: 0,
            oos_trades: 0,
            orders_is: 0,
            orders_oos: 0,
            rejected_sym_cap: 0,
            rejected_total_cap: 0,
            unroutable: 0,
            canceled_end: 0,
            peak_open_total: 0,
            peak_open_per_sym: 0,
        }
    }

    /// Open orders currently resting (test/inspection surface).
    pub fn open_orders(&self) -> &[OpenOrder] {
        &self.open[..self.open_len]
    }

    /// Current OOS-book equity ×1e12 (realized − fees + unrealized).
    /// M4.2 `audit-pnl` surface: with `boundary_virt_ns == 0` the OOS
    /// book IS the whole-window book, and per-UTC-day net buckets are
    /// day-boundary snapshots of this value. Additive accessor — no
    /// model behavior change.
    #[inline]
    pub fn oos_equity_1e12(&self) -> i128 {
        self.oos.equity_1e12()
    }

    /// §4.1 intake of one vm-emitted order at `emit_virt`. Applies the
    /// venue routability check, the 4/32 caps (per-sym first, then
    /// total — the risk-policy table order), tags the §3.4 bucket and
    /// stamps `t_active = emit + Δ_venue`. Rejections are counted,
    /// never surfaced to the vm (module docs: the vm sees `Ok`).
    pub fn intake(&mut self, order: &Order, emit_virt: u64) {
        let venue = order.venue as usize;
        debug_assert_eq!(
            order.venue,
            symbol_venue_byte(order.sym),
            "vm derives Order.venue from the sym namespace"
        );
        let px = order.px.raw();
        let qty = order.qty.raw();
        // The real vm cannot emit an untradeable venue byte
        // (VenueId-derived), nor a non-positive px/qty (two-sided-
        // book + cap guards). Fail closed on hand-built inputs:
        // count as unroutable, drop. WS9: the gate is the explicit
        // predicate — Ai (5) sits inside the byte range, Bybit (6)
        // trades.
        debug_assert!(px > 0 && qty > 0, "vm emits positive px/qty only");
        if !tradeable_venue_byte(venue) || px <= 0 || qty <= 0 {
            self.unroutable += 1;
            return;
        }
        let mut sym_count = 0usize;
        let mut i = 0usize;
        while i < self.open_len {
            if self.open[i].sym == order.sym {
                sym_count += 1;
            }
            i += 1;
        }
        if sym_count >= MAX_OPEN_PER_SYM {
            self.rejected_sym_cap += 1;
            return;
        }
        if self.open_len >= MAX_OPEN_TOTAL {
            self.rejected_total_cap += 1;
            return;
        }
        let oos = emit_virt >= self.boundary_virt_ns;
        if oos {
            self.orders_oos += 1;
        } else {
            self.orders_is += 1;
        }
        self.open[self.open_len] = OpenOrder {
            seq: self.seq_next,
            t_active_ns: emit_virt.saturating_add(self.params.latency_ns[venue]),
            sym: order.sym,
            side: order.side,
            venue: order.venue,
            oos,
            px_1e6: px,
            remaining_1e6: qty,
            client_oid: order.client_oid,
        };
        self.seq_next += 1;
        self.open_len += 1;
        if self.open_len as u64 > self.peak_open_total {
            self.peak_open_total = self.open_len as u64;
        }
        let sym_now = (sym_count + 1) as u64;
        if sym_now > self.peak_open_per_sym {
            self.peak_open_per_sym = sym_now;
        }
    }

    /// Refresh the §4.6 bounds contribution of `sym` from the full
    /// book at `mark`.
    fn bounds_refresh(&mut self, sym: u32, mark_1e6: i64) {
        let qty = self.full.entries.get(&sym).map(|e| e.qty_1e6).unwrap_or(0);
        let abs_notional = (qty as i128).abs() * mark_1e6 as i128;
        self.bounds.set_sym(sym, abs_notional);
    }

    /// VM2 V5 (D-7): register `sym` as MARK-FILLED — an options
    /// instrument whose "book" is a synthetic zero-spread mark tick.
    /// Orders on such syms execute IMMEDIATELY (post-latency) at
    /// `mark ± half-spread` with the venue's TAKER fee — the D-7 law:
    /// no real displayed book exists to be maker in, so the model
    /// charges the assumed spread + taker economics instead. Callers
    /// that ever see `mark_fills > 0` MUST print the assumption.
    pub fn set_mark_fill_sym(&mut self, sym: u32) {
        self.mark_fill_syms.insert(sym);
    }

    /// One merged record (§4.2): mark update first (a fill at this
    /// tick marks against THIS tick's mid — the adverse-selection-
    /// honest direction), then the strict-cross fill pass over the
    /// resting orders of `tick.sym`. Fills are appended to `out`
    /// (cleared here) for the §3.6.4 vm feedback.
    ///
    /// One-sided ticks (bid or ask ≤ 0) never mark and never fill:
    /// an absent ask must not read as "ask below our bid".
    pub fn on_record(&mut self, tick: &Tick, virt_ns: u64, wall_ns: u64, out: &mut Vec<SynthFill>) {
        out.clear();
        let sym = tick.sym;
        if sym == SYMBOL_ID_NONE {
            return;
        }
        let bid = tick.bid_px.raw();
        let ask = tick.ask_px.raw();
        let two_sided = bid > 0 && ask > 0;

        // ---- (a) mark update (§4.5: "every tick of a held sym") ----
        if two_sided {
            let mid = tick.mid().raw();
            let old = self.marks_1e6.insert(sym, mid).unwrap_or(mid);
            if self.full.on_mark(sym, old, mid) {
                self.bounds_refresh(sym, mid);
            }
            if self.oos.on_mark(sym, old, mid) {
                self.dd.sample(self.oos.equity_1e12());
            }
        }

        // ---- (b′) D-7 mark-fill pass (VM2 V5) ----
        // Registered options syms: every active resting order fills
        // in FULL at mark ± half-spread with the TAKER fee — buys pay
        // `mark + h`, sells receive `mark − h`
        // ([`mark_half_spread_1e6`]). Runs INSTEAD of the
        // strict-cross pass for these syms (their ticks are synthetic
        // zero-spread marks; displayed-size budgets are meaningless).
        if two_sided && self.mark_fill_syms.contains(&sym) {
            let mark = *self.marks_1e6.get(&sym).expect("mark just written");
            let h = mark_half_spread_1e6(mark);
            let mut i = 0usize;
            while i < self.open_len {
                let o = self.open[i];
                if o.sym != sym || virt_ns < o.t_active_ns {
                    i += 1;
                    continue;
                }
                let (fill_px, fill_qty) = match o.side {
                    Side::Bid => (mark + h, o.remaining_1e6),
                    Side::Ask => ((mark - h).max(1), o.remaining_1e6),
                };
                let notional_1e12 = fill_px as i128 * fill_qty as i128;
                let taker_bps = self.params.fee_bps[o.venue as usize].1;
                let fee_1e12 = fee_ceil_1e12(notional_1e12, taker_bps);
                self.full
                    .apply_fill(sym, o.side, fill_px, fill_qty, fee_1e12, mark);
                self.bounds_refresh(sym, mark);
                if o.oos {
                    self.oos
                        .apply_fill(sym, o.side, fill_px, fill_qty, fee_1e12, mark);
                    self.dd.sample(self.oos.equity_1e12());
                    self.oos_trades += 1;
                    self.oos_days.insert(wall_ns / DAY_NS);
                }
                self.fills_total += 1;
                self.mark_fills += 1;
                out.push(SynthFill {
                    sym,
                    side: o.side,
                    px_1e6: fill_px,
                    qty_1e6: fill_qty,
                    fee_1e12,
                    oos: o.oos,
                    client_oid: o.client_oid,
                });
                // Full fill: compact (FIFO preserved).
                let mut j = i;
                while j + 1 < self.open_len {
                    self.open[j] = self.open[j + 1];
                    j += 1;
                }
                self.open_len -= 1;
            }
            return;
        }

        // ---- (b) strict-cross fill pass (§4.2) ----
        // Two-sided ticks only, same as the mark: a one-sided book is
        // not trusted as fill evidence (the 8e preopen lesson; also
        // every fill needs the mark this record just wrote). Fewer
        // fills = conservative.
        if !two_sided {
            return;
        }
        // Shared displayed budgets: our BIDs consume the printed ask
        // size, our ASKs the printed bid size — FIFO in emit order
        // (the table IS emit-ordered by construction).
        let mut ask_budget = tick.ask_qty.raw().max(0);
        let mut bid_budget = tick.bid_qty.raw().max(0);
        let mut i = 0usize;
        while i < self.open_len {
            let o = self.open[i];
            if o.sym != sym || virt_ns < o.t_active_ns {
                i += 1;
                continue;
            }
            let fill_qty = match o.side {
                // Strict cross only: `<`, never `<=` (touch ⇒ infinite
                // queue ahead, §4.2). `ask/bid > 0` held by `two_sided`.
                Side::Bid if ask < o.px_1e6 && ask_budget > 0 => {
                    let q = o.remaining_1e6.min(ask_budget);
                    ask_budget -= q;
                    q
                }
                Side::Ask if bid > o.px_1e6 && bid_budget > 0 => {
                    let q = o.remaining_1e6.min(bid_budget);
                    bid_budget -= q;
                    q
                }
                _ => 0,
            };
            if fill_qty <= 0 {
                i += 1;
                continue;
            }
            let notional_1e12 = o.px_1e6 as i128 * fill_qty as i128;
            let maker_bps = self.params.fee_bps[o.venue as usize].0;
            let fee_1e12 = fee_ceil_1e12(notional_1e12, maker_bps);
            // Fills only happen at a two-sided tick of the sym, which
            // just refreshed the mark above.
            let mark = *self.marks_1e6.get(&sym).expect("fill implies a mark");
            self.full
                .apply_fill(sym, o.side, o.px_1e6, fill_qty, fee_1e12, mark);
            self.bounds_refresh(sym, mark);
            if o.oos {
                self.oos
                    .apply_fill(sym, o.side, o.px_1e6, fill_qty, fee_1e12, mark);
                self.dd.sample(self.oos.equity_1e12());
                self.oos_trades += 1;
                self.oos_days.insert(wall_ns / DAY_NS);
            }
            self.fills_total += 1;
            out.push(SynthFill {
                sym,
                side: o.side,
                px_1e6: o.px_1e6,
                qty_1e6: fill_qty,
                fee_1e12,
                oos: o.oos,
                client_oid: o.client_oid,
            });
            let remaining = o.remaining_1e6 - fill_qty;
            if remaining > 0 {
                self.open[i].remaining_1e6 = remaining;
                i += 1;
            } else {
                // Compact: shift left, preserving emit order (FIFO).
                let mut j = i;
                while j + 1 < self.open_len {
                    self.open[j] = self.open[j + 1];
                    j += 1;
                }
                self.open_len -= 1;
                // Do not advance `i`: the next order slid into slot i.
            }
        }
    }

    /// End of replay (§4.5): cancel unfilled remainders (zero P&L
    /// effect), read the books at last marks (the mark-out is exactly
    /// the standing unrealized sum) and hand back every scalar.
    pub fn finish(&mut self) -> ModelOutcome {
        self.canceled_end = self.open_len as u64;
        self.open_len = 0;
        ModelOutcome {
            oos_net_1e12: self.oos.equity_1e12(),
            oos_realized_1e12: self.oos.realized_sum_1e12,
            oos_fees_1e12: self.oos.fees_sum_1e12,
            oos_unreal_1e12: self.oos.unreal_sum_1e12,
            oos_max_dd_1e12: self.dd.max_dd_1e12,
            oos_trades: self.oos_trades,
            oos_trading_days: self.oos_days.len() as u64,
            full_realized_1e12: self.full.realized_sum_1e12,
            full_fees_1e12: self.full.fees_sum_1e12,
            full_unreal_1e12: self.full.unreal_sum_1e12,
            max_symbol_notional_1e12: self.bounds.max_symbol_1e12,
            max_total_notional_1e12: self.bounds.max_total_1e12,
            fills_total: self.fills_total,
            orders_is: self.orders_is,
            orders_oos: self.orders_oos,
            rejected_sym_cap: self.rejected_sym_cap,
            rejected_total_cap: self.rejected_total_cap,
            unroutable: self.unroutable,
            canceled_end: self.canceled_end,
            peak_open_total: self.peak_open_total,
            peak_open_per_sym: self.peak_open_per_sym,
            mark_fills: self.mark_fills,
        }
    }

    /// Per-sym sidecar rows (full book, sorted by sym).
    pub fn per_sym_detail(&self) -> Vec<PerSymDetail> {
        let mut rows = Vec::with_capacity(self.full.entries.len());
        for (sym, e) in &self.full.entries {
            rows.push(PerSymDetail {
                sym: *sym,
                venue: symbol_venue_byte(*sym),
                pos_qty_1e6: e.qty_1e6,
                last_mid_1e6: *self.marks_1e6.get(sym).unwrap_or(&0),
                realized_1e12: e.realized_1e12,
                fees_1e12: e.fees_1e12,
                fills: e.fills,
            });
        }
        rows
    }
}

// ---------------------------------------------------------------
// Tests (§12: unit + proptest; offline path — allocates freely)
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, Price, Qty, VenueId};

    const PM_SYM: u32 = 42; // venue byte 0 (ordinal 42)
    const BN_SYM: u32 = 0x0100_0007; // venue byte 1 (Binance ordinal 7)

    fn order(sym: u32, side: Side, px: i64, qty: i64, oid: u64) -> Order {
        let venue = core_types::VenueId::from_u8(symbol_venue_byte(sym)).expect("venue");
        Order::new(
            0,
            venue,
            sym,
            side,
            0,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        )
    }

    fn tick(sym: u32, bid: i64, bq: i64, ask: i64, aq: i64) -> Tick {
        let venue = core_types::VenueId::from_u8(symbol_venue_byte(sym)).expect("venue");
        Tick::new(
            0,
            venue,
            sym,
            0,
            Price::from_raw(bid),
            Qty::from_raw(bq),
            Price::from_raw(ask),
            Qty::from_raw(aq),
        )
    }

    fn engine(boundary: u64) -> FillEngine {
        FillEngine::new(ModelParams::default(), boundary)
    }

    /// Zero-latency params: every order activates at its emit tick
    /// (still never fills on it — the pass precedes the emit).
    fn engine_zero_delta(boundary: u64) -> FillEngine {
        let p = ModelParams {
            fee_bps: [(0, 0); 7],
            latency_ns: [0; 7],
        };
        FillEngine::new(p, boundary)
    }

    // -------------- conversion helpers --------------

    #[test]
    fn fixed_point_conversions_floor_and_ceil() {
        assert_eq!(usd_1e12_to_1e6_floor(5_000_000_000_000), 5_000_000);
        assert_eq!(usd_1e12_to_1e6_ceil(5_000_000_000_000), 5_000_000);
        // floor is toward −∞ (a loss never understated)…
        assert_eq!(usd_1e12_to_1e6_floor(-4_374_999_999_999), -4_375_000);
        assert_eq!(usd_1e12_to_1e6_floor(4_374_999_999_999), 4_374_999);
        // …ceil toward +∞ (risk never understated).
        assert_eq!(usd_1e12_to_1e6_ceil(96_800_000_000_001), 96_800_001);
        assert_eq!(usd_1e12_to_1e6_ceil(-1), 0);
        assert_eq!(usd_1e12_to_1e6_floor(0), 0);
        assert_eq!(usd_1e12_to_1e6_ceil(0), 0);
    }

    #[test]
    fn fee_rounds_up_never_in_our_favor() {
        assert_eq!(fee_ceil_1e12(28_000_000_000_000, 50), 140_000_000_000); // $0.14
        assert_eq!(fee_ceil_1e12(1, 1), 1); // ceil(1/10_000) = 1
        assert_eq!(fee_ceil_1e12(10_000, 1), 1);
        assert_eq!(fee_ceil_1e12(10_001, 1), 2);
        assert_eq!(fee_ceil_1e12(123, 0), 0);
    }

    // -------------- activation Δ (§4.4) --------------

    #[test]
    fn delta_gates_the_first_fillable_tick_inclusive() {
        let mut e = engine(u64::MAX); // everything IS
        let mut out = Vec::new();
        // PM Δ default = 200 ms.
        e.intake(&order(PM_SYM, Side::Bid, 500_000, 10_000_000, 1), 1_000);
        let t_active = 1_000 + 200_000_000;
        assert_eq!(e.open_orders()[0].t_active_ns, t_active);
        // Crossing tick 1 ns early: no fill.
        e.on_record(
            &tick(PM_SYM, 400_000, 1, 450_000, 100_000_000),
            t_active - 1,
            0,
            &mut out,
        );
        assert!(out.is_empty());
        // At t_active exactly (≥, inclusive): fills.
        e.on_record(
            &tick(PM_SYM, 400_000, 1, 450_000, 100_000_000),
            t_active,
            0,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].px_1e6, 500_000, "maker fills at P, never better");
        assert_eq!(out[0].qty_1e6, 10_000_000);
    }

    // -------------- strict-cross (§4.2) --------------

    #[test]
    fn touch_never_fills_strict_cross_only() {
        let mut e = engine_zero_delta(u64::MAX);
        let mut out = Vec::new();
        e.intake(&order(PM_SYM, Side::Bid, 500_000, 10_000_000, 1), 0);
        e.intake(&order(PM_SYM, Side::Ask, 600_000, 10_000_000, 2), 0);
        // Touch on both sides: ask == bid px, bid == ask px — nothing.
        e.on_record(
            &tick(PM_SYM, 600_000, 50_000_000, 500_000, 50_000_000),
            10,
            0,
            &mut out,
        );
        assert!(out.is_empty(), "touch ⇒ infinite queue ahead ⇒ no fill");
        // Strict cross on both: ask < 0.50, bid > 0.60.
        e.on_record(
            &tick(PM_SYM, 610_000, 50_000_000, 490_000, 50_000_000),
            20,
            0,
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].px_1e6, 500_000);
        assert_eq!(out[1].px_1e6, 600_000);
    }

    #[test]
    fn one_sided_tick_never_fills_and_never_marks() {
        let mut e = engine_zero_delta(u64::MAX);
        let mut out = Vec::new();
        e.intake(&order(PM_SYM, Side::Bid, 500_000, 10_000_000, 1), 0);
        // ask = 0 (absent) must NOT read as "ask below our bid".
        e.on_record(
            &tick(PM_SYM, 400_000, 1_000_000, 0, 1_000_000),
            10,
            0,
            &mut out,
        );
        assert!(out.is_empty());
        assert!(!e.marks_1e6.contains_key(&PM_SYM), "one-sided ⇒ no mark");
        // bid = 0 with a resting ask: 0 > px is false, no fill; still no mark.
        e.intake(&order(PM_SYM, Side::Ask, 300_000, 10_000_000, 2), 10);
        e.on_record(
            &tick(PM_SYM, 0, 1_000_000, 700_000, 1_000_000),
            20,
            0,
            &mut out,
        );
        assert!(out.is_empty());
        assert!(!e.marks_1e6.contains_key(&PM_SYM));
    }

    #[test]
    fn partial_fill_rests_and_completes_two_trades() {
        let mut e = engine_zero_delta(0); // everything OOS
        let mut out = Vec::new();
        e.intake(&order(PM_SYM, Side::Bid, 400_000, 125_000_000, 1), 0);
        // Displayed 70 < remaining 125 ⇒ partial; remainder rests.
        e.on_record(
            &tick(PM_SYM, 350_000, 1, 380_000, 70_000_000),
            10,
            0,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qty_1e6, 70_000_000);
        assert_eq!(e.open_orders().len(), 1);
        assert_eq!(e.open_orders()[0].remaining_1e6, 55_000_000);
        // Remainder fills; slot frees.
        e.on_record(
            &tick(PM_SYM, 350_000, 1, 380_000, 200_000_000),
            20,
            0,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qty_1e6, 55_000_000);
        assert!(e.open_orders().is_empty());
        let o = e.finish();
        assert_eq!(o.oos_trades, 2, "a partial fill is one trade (§4.5)");
        assert_eq!(o.fills_total, 2);
        assert_eq!(o.canceled_end, 0);
    }

    #[test]
    fn shared_displayed_budget_is_fifo_by_emit_order() {
        let mut e = engine_zero_delta(u64::MAX);
        let mut out = Vec::new();
        e.intake(&order(PM_SYM, Side::Bid, 500_000, 100_000_000, 1), 0);
        e.intake(&order(PM_SYM, Side::Bid, 450_000, 100_000_000, 2), 0);
        // Ask 0.40 crosses BOTH; displayed 60 — the older order eats
        // it all, the younger gets none (no double-claimed liquidity).
        e.on_record(
            &tick(PM_SYM, 350_000, 1, 400_000, 60_000_000),
            10,
            0,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].client_oid, 1);
        assert_eq!(out[0].qty_1e6, 60_000_000);
        assert_eq!(e.open_orders().len(), 2);
        assert_eq!(e.open_orders()[0].remaining_1e6, 40_000_000);
        assert_eq!(e.open_orders()[1].remaining_1e6, 100_000_000);
    }

    // -------------- caps (§4.1) --------------

    #[test]
    fn per_sym_cap_rejects_the_ninth_open_order() {
        // Operator ruling 2026-08-29 ($50k research tier): 4 -> 8.
        let mut e = engine(u64::MAX);
        for oid in 0..9u64 {
            e.intake(
                &order(PM_SYM, Side::Bid, 100_000 + oid as i64, 1_000_000, oid),
                0,
            );
        }
        assert_eq!(e.open_orders().len(), 8);
        let o = e.finish();
        assert_eq!(o.rejected_sym_cap, 1);
        assert_eq!(o.rejected_total_cap, 0);
        assert_eq!(o.peak_open_per_sym, 8);
        assert_eq!(o.canceled_end, 8);
    }

    #[test]
    fn total_cap_rejects_the_sixty_fifth_open_order() {
        // Operator ruling 2026-08-29 ($50k research tier): 32 -> 64.
        let mut e = engine(u64::MAX);
        // 8 syms × 8 orders = 64 (per-sym cap never trips).
        for s in 0..8u32 {
            let sym = make_symbol_id(VenueId::Polymarket, 100 + s);
            for k in 0..8u64 {
                e.intake(
                    &order(sym, Side::Bid, 100_000, 1_000_000, (s as u64) * 8 + k),
                    0,
                );
            }
        }
        assert_eq!(e.open_orders().len(), 64);
        let extra_sym = make_symbol_id(VenueId::Polymarket, 999);
        e.intake(&order(extra_sym, Side::Bid, 100_000, 1_000_000, 777), 0);
        let o = e.finish();
        assert_eq!(o.rejected_total_cap, 1);
        assert_eq!(o.rejected_sym_cap, 0);
        assert_eq!(o.peak_open_total, 64);
    }

    #[test]
    fn unroutable_venue_is_counted_and_dropped() {
        let mut e = engine(u64::MAX);
        let ai_sym = make_symbol_id(VenueId::Ai, 1);
        e.intake(&order(ai_sym, Side::Bid, 100_000, 1_000_000, 1), 0);
        assert!(e.open_orders().is_empty());
        let o = e.finish();
        assert_eq!(o.unroutable, 1);
    }

    // -------------- fees (§4.3) --------------

    #[test]
    fn maker_fee_charges_on_fill_notional() {
        let p = ModelParams {
            fee_bps: [(50, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0)], // PM maker 50 bps
            latency_ns: [0; 7],
        };
        let mut e = FillEngine::new(p, 0); // all OOS
        let mut out = Vec::new();
        e.intake(&order(PM_SYM, Side::Bid, 400_000, 70_000_000, 1), 0);
        e.on_record(
            &tick(PM_SYM, 350_000, 1, 380_000, 70_000_000),
            10,
            0,
            &mut out,
        );
        // notional = 0.40 × 70 = $28 ⇒ fee = 28e12 × 50 / 1e4 = $0.14.
        assert_eq!(out[0].fee_1e12, 140_000_000_000);
        let o = e.finish();
        assert_eq!(o.oos_fees_1e12, 140_000_000_000);
        // Fee is inside equity: net = unreal − fee at the same mark.
        let mark = 365_000i128; // (350+380)/2
        let unreal = 70_000_000i128 * mark - 28_000_000_000_000;
        assert_eq!(o.oos_net_1e12, unreal - 140_000_000_000);
    }

    // -------------- §3.4 bucketing --------------

    #[test]
    fn is_emitted_order_filling_after_boundary_stays_out_of_oos() {
        let boundary = 1_000_000;
        let mut e = engine_zero_delta(boundary);
        let mut out = Vec::new();
        // Emitted BEFORE the boundary…
        e.intake(
            &order(PM_SYM, Side::Bid, 500_000, 10_000_000, 1),
            boundary - 1,
        );
        // …fills AFTER it: full book takes it, OOS book does not.
        e.on_record(
            &tick(PM_SYM, 400_000, 1, 450_000, 50_000_000),
            boundary + 10,
            0,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(!out[0].oos);
        // OOS-emitted order on the same sym fills next tick: counted.
        e.intake(
            &order(PM_SYM, Side::Bid, 500_000, 10_000_000, 2),
            boundary + 10,
        );
        e.on_record(
            &tick(PM_SYM, 400_000, 1, 450_000, 50_000_000),
            boundary + 20,
            DAY_NS * 3 + 5,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].oos);
        let o = e.finish();
        assert_eq!(o.fills_total, 2);
        assert_eq!(o.oos_trades, 1, "IS-emitted fills never enter oos (§3.4)");
        assert_eq!(o.orders_is, 1);
        assert_eq!(o.orders_oos, 1);
        assert_eq!(o.oos_trading_days, 1, "day 3 only");
    }

    #[test]
    fn mark_fill_fills_exactly_once_across_many_marks() {
        // VM2 V7 pin (the iv-proof finding): a mark-registered sym's
        // resting order fills ONCE in FULL and leaves the book — a
        // stream of subsequent marks must never re-fill it.
        let mut e = engine_zero_delta(0);
        e.set_mark_fill_sym(PM_SYM);
        let mut out = Vec::new();
        e.intake(&order(PM_SYM, Side::Bid, 500_000, 10_000_000, 1), 0);
        let mut fills = 0u64;
        let mut k = 0u64;
        while k < 5 {
            e.on_record(
                &tick(PM_SYM, 400_000, 1_000_000, 400_000, 1_000_000),
                10 + k,
                0,
                &mut out,
            );
            fills += out.len() as u64;
            k += 1;
        }
        assert_eq!(fills, 1, "one full mark-fill only");
        let s = e.finish();
        assert_eq!(s.mark_fills, 1);
        assert_eq!(s.fills_total, 1);
    }

    // -------------- §4.5 accounting --------------

    #[test]
    fn reducing_fill_realizes_average_cost_and_crosses_zero() {
        // Book contract, engine-faithful: every mark CHANGE flows
        // through `on_mark` before fills use the new mark (the engine
        // does exactly this — mark update precedes the fill pass).
        let mut b = Book::default();
        // Long 10 @ 0.40 (cost 4e12), mark 0.40.
        b.apply_fill(PM_SYM, Side::Bid, 400_000, 10_000_000, 0, 400_000);
        assert_eq!(b.entries[&PM_SYM].qty_1e6, 10_000_000);
        assert_eq!(b.entries[&PM_SYM].cost_1e12, 4_000_000_000_000);
        assert_eq!(b.unreal_sum_1e12, 0);
        // Mark rallies to 0.50: unrealized +$1.
        assert!(b.on_mark(PM_SYM, 400_000, 500_000));
        assert_eq!(b.unreal_sum_1e12, 1_000_000_000_000);
        // Sell 25 @ 0.50: realize (0.5−0.4)×10 = $1, open short 15 @ 0.50.
        b.apply_fill(PM_SYM, Side::Ask, 500_000, 25_000_000, 0, 500_000);
        let e = b.entries[&PM_SYM];
        assert_eq!(e.qty_1e6, -15_000_000);
        assert_eq!(e.cost_1e12, -7_500_000_000_000);
        assert_eq!(e.realized_1e12, 1_000_000_000_000);
        assert_eq!(b.unreal_sum_1e12, 0, "flat-to-mark short right after entry");
        // Mark drops to 0.30: the short is up $3 unrealized.
        assert!(b.on_mark(PM_SYM, 500_000, 300_000));
        assert_eq!(b.unreal_sum_1e12, 3_000_000_000_000);
        // Buy back 15 @ 0.30: realize (0.5−0.3)×15 = $3 more.
        b.apply_fill(PM_SYM, Side::Bid, 300_000, 15_000_000, 0, 300_000);
        let e = b.entries[&PM_SYM];
        assert_eq!(e.qty_1e6, 0);
        assert_eq!(e.cost_1e12, 0, "full close leaves zero cost exactly");
        assert_eq!(e.realized_1e12, 4_000_000_000_000);
        assert_eq!(b.unreal_sum_1e12, 0);
        assert_eq!(b.equity_1e12(), 4_000_000_000_000);
    }

    #[test]
    fn liquidation_markout_at_last_mid_enters_net() {
        let mut e = engine_zero_delta(0); // all OOS
        let mut out = Vec::new();
        e.intake(&order(PM_SYM, Side::Bid, 400_000, 125_000_000, 1), 0);
        e.on_record(
            &tick(PM_SYM, 350_000, 1, 380_000, 125_000_000),
            10,
            0,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        // Last tick moves the mark to 0.44: unreal = (0.44−0.40)×125 = $5.
        e.on_record(&tick(PM_SYM, 430_000, 1, 450_000, 1), 20, 0, &mut out);
        assert!(out.is_empty());
        let o = e.finish();
        assert_eq!(o.oos_realized_1e12, 0);
        assert_eq!(o.oos_unreal_1e12, 5_000_000_000_000);
        assert_eq!(o.oos_net_1e12, 5_000_000_000_000, "mark-out IS the net");
        assert_eq!(o.canceled_end, 0);
    }

    #[test]
    fn drawdown_tracks_peak_to_trough_of_oos_equity() {
        let mut d = DdTracker::new();
        d.sample(5_000_000_000_000); // peak 5
        d.sample(-3_000_000_000_000); // trough −3 ⇒ dd 8
        d.sample(1_000_000_000_000);
        assert_eq!(d.max_dd_1e12, 8_000_000_000_000);
        // Starting flat: a pure loss is its own drawdown.
        let mut d2 = DdTracker::new();
        d2.sample(-2_450_000_000_000);
        assert_eq!(d2.max_dd_1e12, 2_450_000_000_000);
    }

    #[test]
    fn deeper_buy_into_falling_mark_deepens_the_trough() {
        // The golden-fixture shape: buy 70 @ 0.40, mark 0.365 (−2.45),
        // buy 55 more @ 0.40 at the same mark (−4.375), then rally.
        let mut e = engine_zero_delta(0);
        let mut out = Vec::new();
        e.intake(&order(PM_SYM, Side::Bid, 400_000, 70_000_000, 1), 0);
        e.on_record(
            &tick(PM_SYM, 350_000, 1, 380_000, 70_000_000),
            10,
            0,
            &mut out,
        );
        e.intake(&order(PM_SYM, Side::Bid, 400_000, 55_000_000, 2), 10);
        e.on_record(
            &tick(PM_SYM, 350_000, 1, 380_000, 200_000_000),
            20,
            0,
            &mut out,
        );
        e.on_record(&tick(PM_SYM, 430_000, 1, 450_000, 1), 30, 0, &mut out);
        let o = e.finish();
        assert_eq!(o.oos_max_dd_1e12, 4_375_000_000_000);
        assert_eq!(o.oos_net_1e12, 5_000_000_000_000);
        assert_eq!(o.oos_trades, 2);
    }

    // -------------- §4.6 bounds --------------

    #[test]
    fn bounds_track_peak_symbol_and_total_across_marks_and_fills() {
        let mut e = engine_zero_delta(u64::MAX);
        let mut out = Vec::new();
        e.intake(&order(PM_SYM, Side::Bid, 400_000, 125_000_000, 1), 0);
        e.intake(&order(BN_SYM, Side::Bid, 400_000, 50_000_000, 2), 0);
        e.on_record(
            &tick(PM_SYM, 350_000, 1, 380_000, 200_000_000),
            10,
            0,
            &mut out,
        );
        e.on_record(
            &tick(BN_SYM, 350_000, 1, 380_000, 200_000_000),
            20,
            0,
            &mut out,
        );
        // Marks rally on both: PM 0.625, BN 0.625.
        e.on_record(&tick(PM_SYM, 600_000, 1, 650_000, 1), 30, 0, &mut out);
        e.on_record(&tick(BN_SYM, 600_000, 1, 650_000, 1), 40, 0, &mut out);
        let o = e.finish();
        // Peak per-sym: PM 125 × 0.625 = $78.125.
        assert_eq!(o.max_symbol_notional_1e12, 78_125_000_000_000);
        // Peak total: 125×0.625 + 50×0.625 = $109.375 (venue-agnostic
        // composition across namespaced syms — door-closer 16.3.2).
        assert_eq!(o.max_total_notional_1e12, 109_375_000_000_000);
    }

    // -------------- proptests (§12) --------------

    /// Scenario event: either a tick or a vm-style order emission.
    #[derive(Clone, Debug)]
    enum Ev {
        Tick {
            sym_i: usize,
            bid: i64,
            bq: i64,
            ask_gap: i64,
            aq: i64,
            one_sided: u8,
        },
        Emit {
            sym_i: usize,
            side_bid: bool,
            px: i64,
            qty: i64,
        },
    }

    fn syms() -> [u32; 4] {
        [
            make_symbol_id(VenueId::Polymarket, 1),
            make_symbol_id(VenueId::Polymarket, 2),
            make_symbol_id(VenueId::Binance, 1),
            make_symbol_id(VenueId::Hyperliquid, 3),
        ]
    }

    fn ev_strategy() -> impl proptest::strategy::Strategy<Value = Ev> {
        use proptest::prelude::any;
        use proptest::strategy::Strategy;
        proptest::prop_oneof![
            (
                0usize..4,
                1i64..999_000,
                0i64..300_000_000,
                1i64..50_000,
                0i64..300_000_000,
                any::<u8>()
            )
                .prop_map(|(sym_i, bid, bq, ask_gap, aq, one_sided)| Ev::Tick {
                    sym_i,
                    bid,
                    bq,
                    ask_gap,
                    aq,
                    one_sided,
                }),
            (0usize..4, any::<bool>(), 1i64..1_000_000, 1i64..200_000_000).prop_map(
                |(sym_i, side_bid, px, qty)| Ev::Emit {
                    sym_i,
                    side_bid,
                    px,
                    qty
                }
            ),
        ]
    }

    proptest::proptest! {
        /// §12 fill invariants: a fill's price is never better than the
        /// crossing book demands (px == resting P, opposite strictly
        /// through), per-tick filled qty never exceeds the displayed
        /// size (shared budget, per side), no fill before activation,
        /// fees ≥ 0.
        #[test]
        fn fill_invariants_hold_for_arbitrary_scenarios(
            evs in proptest::collection::vec(ev_strategy(), 1..120),
            maker_bps in 0u32..200,
        ) {
            let params = ModelParams {
                fee_bps: [(maker_bps, 0); 7],
                latency_ns: [200_000_000, 100_000_000, 100_000_000, 100_000_000, 600_000_000, 0, 100_000_000],
            };
            let mut e = FillEngine::new(params, u64::MAX / 2);
            let mut out = Vec::new();
            let mut virt = 0u64;
            let mut oid = 0u64;
            // oid → (t_active, px, side) for the activation/price checks.
            let mut by_oid: BTreeMap<u64, (u64, i64, Side)> = BTreeMap::new();
            let table = syms();
            for ev in &evs {
                virt += 1_000_003; // strictly increasing virtual clock
                match ev {
                    Ev::Emit { sym_i, side_bid, px, qty } => {
                        oid += 1;
                        let side = if *side_bid { Side::Bid } else { Side::Ask };
                        let o = order(table[*sym_i], side, *px, *qty, oid);
                        e.intake(&o, virt);
                        let venue = symbol_venue_byte(table[*sym_i]) as usize;
                        by_oid.insert(oid, (virt + params.latency_ns[venue], *px, side));
                    }
                    Ev::Tick { sym_i, bid, bq, ask_gap, aq, one_sided } => {
                        let (b, a) = match one_sided % 8 {
                            0 => (0, bid + ask_gap),
                            1 => (*bid, 0),
                            _ => (*bid, bid + ask_gap),
                        };
                        let t = tick(table[*sym_i], b, *bq, a, *aq);
                        e.on_record(&t, virt, virt, &mut out);
                        let mut bid_side_qty = 0i64; // our bids (vs displayed ask)
                        let mut ask_side_qty = 0i64; // our asks (vs displayed bid)
                        for f in &out {
                            proptest::prop_assert!(f.qty_1e6 > 0);
                            proptest::prop_assert!(f.fee_1e12 >= 0);
                            let (t_active, px, side) =
                                *by_oid.get(&f.client_oid).expect("known oid");
                            proptest::prop_assert!(virt >= t_active, "no fill before activation");
                            proptest::prop_assert_eq!(f.px_1e6, px, "maker fills at P exactly");
                            proptest::prop_assert_eq!(f.side as u8, side as u8);
                            match f.side {
                                Side::Bid => {
                                    proptest::prop_assert!(a > 0 && a < px, "strict cross only");
                                    bid_side_qty += f.qty_1e6;
                                }
                                Side::Ask => {
                                    proptest::prop_assert!(b > px, "strict cross only");
                                    ask_side_qty += f.qty_1e6;
                                }
                            }
                        }
                        proptest::prop_assert!(bid_side_qty <= *aq, "≤ displayed ask size");
                        proptest::prop_assert!(ask_side_qty <= *bq, "≤ displayed bid size");
                    }
                }
            }
        }

        /// §12 conservation identity, EXACT: for the full book,
        /// realized − fees + unrealized == cash − fees + Σ pos×mark,
        /// where cash is the naive signed fill flow — and the
        /// incremental unrealized sum equals its from-scratch
        /// recomputation.
        #[test]
        fn cash_and_position_are_conserved_exactly(
            evs in proptest::collection::vec(ev_strategy(), 1..150),
            maker_bps in 0u32..200,
        ) {
            let params = ModelParams {
                fee_bps: [(maker_bps, 0); 7],
                latency_ns: [0; 7],
            };
            let mut e = FillEngine::new(params, 0);
            let mut out = Vec::new();
            let mut virt = 0u64;
            let mut oid = 0u64;
            let mut cash_1e12: i128 = 0; // signed fill flow, fees excluded
            let mut fees_1e12: i128 = 0;
            let table = syms();
            for ev in &evs {
                virt += 7;
                match ev {
                    Ev::Emit { sym_i, side_bid, px, qty } => {
                        oid += 1;
                        let side = if *side_bid { Side::Bid } else { Side::Ask };
                        e.intake(&order(table[*sym_i], side, *px, *qty, oid), virt);
                    }
                    Ev::Tick { sym_i, bid, bq, ask_gap, aq, one_sided } => {
                        let (b, a) = match one_sided % 8 {
                            0 => (0, bid + ask_gap),
                            1 => (*bid, 0),
                            _ => (*bid, bid + ask_gap),
                        };
                        e.on_record(&tick(table[*sym_i], b, *bq, a, *aq), virt, virt, &mut out);
                        for f in &out {
                            let flow = f.px_1e6 as i128 * f.qty_1e6 as i128;
                            match f.side {
                                Side::Bid => cash_1e12 -= flow,
                                Side::Ask => cash_1e12 += flow,
                            }
                            fees_1e12 += f.fee_1e12;
                        }
                    }
                }
            }
            // Independent recomputation from entries + marks.
            let mut mark_value_1e12: i128 = 0;
            let mut unreal_recomputed: i128 = 0;
            for (sym, entry) in &e.full.entries {
                if entry.qty_1e6 != 0 {
                    let mark = *e.marks_1e6.get(sym).expect("held sym has a mark");
                    mark_value_1e12 += entry.qty_1e6 as i128 * mark as i128;
                }
                let mark = *e.marks_1e6.get(sym).unwrap_or(&0);
                unreal_recomputed += entry.qty_1e6 as i128 * mark as i128 - entry.cost_1e12;
            }
            proptest::prop_assert_eq!(
                e.full.unreal_sum_1e12, unreal_recomputed,
                "incremental unrealized == from-scratch"
            );
            proptest::prop_assert_eq!(e.full.fees_sum_1e12, fees_1e12);
            proptest::prop_assert_eq!(
                e.full.equity_1e12(),
                cash_1e12 - fees_1e12 + mark_value_1e12,
                "realized − fees + unrealized == cash − fees + Σ pos×mark"
            );
        }
    }
}
