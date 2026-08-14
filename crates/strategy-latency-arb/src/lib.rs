//! # strategy-latency-arb
//!
//! Strategy B: Binance → Polymarket crypto markets. The primary v1
//! edge on the free-tier stack. Compile-time monomorphized.
//!
//! ## Decision rule (Phase 2 v1)
//!
//! For each tracked Polymarket market `ps`, we hold a fast Binance
//! reference mid for the paired symbol `bs`. On every Polymarket
//! tick we update the per-market top-of-book; on every Binance
//! tick we refresh the reference mid. When the Polymarket mid
//! diverges from the Binance mid by more than `threshold_1e6`
//! fixed-point units, and the per-market cooldown has elapsed, we
//! emit one `Order` against Polymarket on the side that closes
//! the gap.
//!
//! ```text
//! delta = pm_mid - bn_mid
//! if abs(delta) < threshold:        do nothing
//! if (now - last_emit) < cooldown:  do nothing
//! side = delta > 0 ? Ask /* sell rich */ : Bid /* buy cheap */
//! order = Order::new(now, ps, side, post_only, pm_mid, qty, oid)
//! ctx.submit(order)
//! ```
//!
//! ## Zero-alloc design
//!
//! All state is inline:
//! * `MultiBook<N>` for Polymarket books (fixed `N` slot count),
//! * `SymbolPairTable<N>` for the cross-venue map,
//! * `[Price; N]` + `[bool; N]` for the Binance mid cache,
//! * `[u64; N]` for per-market cooldown deadlines.
//!
//! `on_tick` is one linear scan over `N` (≤ 64 in practice) plus
//! one integer compare-and-emit. No heap.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use book_builder::MultiBook;
use core_time::NsTs;
use core_types::{Fill, Order, Price, Qty, Side, Signal, SymbolId, Tick, SYMBOL_ID_NONE};
use strategy_core::{CooldownGate, Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

// ---------------------------------------------------------------
// SymbolPairTable — Polymarket SymbolId → Binance SymbolId
// ---------------------------------------------------------------

/// Fixed-capacity map from Polymarket symbol to Binance symbol.
/// Built at boot via [`SymbolPairTable::add`]; queried via
/// [`SymbolPairTable::binance_for`].
///
/// Linear scan because `N` is tiny. Cache-friendly.
#[derive(Debug)]
#[repr(C, align(64))]
pub struct SymbolPairTable<const N: usize> {
    pm_syms: [SymbolId; N],
    bn_syms: [SymbolId; N],
    count: u32,
    _pad: [u8; 60],
}

impl<const N: usize> SymbolPairTable<N> {
    /// Empty table — both halves filled with the sentinel.
    pub const fn empty() -> Self {
        Self {
            pm_syms: [SYMBOL_ID_NONE; N],
            bn_syms: [SYMBOL_ID_NONE; N],
            count: 0,
            _pad: [0; 60],
        }
    }

    /// Number of populated pairs.
    #[inline]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the table is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Register a cross-venue pair. Boot-only.
    pub fn add(&mut self, pm: SymbolId, bn: SymbolId) -> Result<(), PairError> {
        if pm == SYMBOL_ID_NONE || bn == SYMBOL_ID_NONE {
            return Err(PairError::ReservedSymbol);
        }
        if (self.count as usize) >= N {
            return Err(PairError::Full);
        }
        let n = self.count as usize;
        self.pm_syms[n] = pm;
        self.bn_syms[n] = bn;
        self.count = self.count.wrapping_add(1);
        Ok(())
    }

    /// Look up the Binance counterpart for a Polymarket symbol.
    #[inline]
    pub fn binance_for(&self, pm: SymbolId) -> Option<SymbolId> {
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            if self.pm_syms[i] == pm {
                return Some(self.bn_syms[i]);
            }
            i += 1;
        }
        None
    }

    /// Look up the Polymarket counterpart for a Binance symbol.
    #[inline]
    pub fn polymarket_for(&self, bn: SymbolId) -> Option<SymbolId> {
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            if self.bn_syms[i] == bn {
                return Some(self.pm_syms[i]);
            }
            i += 1;
        }
        None
    }

    /// Index of `bn` in the table, if present. Strategies that keep
    /// parallel per-pair arrays cache this for O(1) follow-up
    /// reads.
    #[inline]
    pub fn binance_index(&self, bn: SymbolId) -> Option<u32> {
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            if self.bn_syms[i] == bn {
                return Some(i as u32);
            }
            i += 1;
        }
        None
    }
}

impl<const N: usize> Default for SymbolPairTable<N> {
    fn default() -> Self {
        Self::empty()
    }
}

/// Why a [`SymbolPairTable::add`] call rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PairError {
    /// Either `pm` or `bn` was `SYMBOL_ID_NONE` — the sentinel id.
    ReservedSymbol,
    /// The table is at capacity.
    Full,
}

// ---------------------------------------------------------------
// LatencyArb — the strategy
// ---------------------------------------------------------------

/// Mid-vs-mid cross-venue arbitrage strategy, generic over `N`
/// (the per-side slot count: at most `N` Polymarket symbols and at
/// most `N` paired Binance symbols).
///
/// State is fully inline; no heap after construction (no heap at
/// construction either, as long as the caller stack-allocates the
/// struct).
#[derive(Debug)]
pub struct LatencyArb<const N: usize> {
    /// Cross-venue symbol map.
    pairs: SymbolPairTable<N>,
    /// Polymarket books, one slot per tracked PM symbol.
    book: MultiBook<N>,
    /// Latest Binance mid by *Binance* table index (NOT SymbolId).
    /// Indexed via `pairs.binance_index(sym)`.
    binance_mid: [Price; N],
    /// Whether a Binance mid has ever been observed at index `i`.
    binance_seen: [bool; N],

    /// Trigger threshold in fixed-point 1e6 units. v1 single value;
    /// v2 may add per-pair thresholds.
    threshold_1e6: i64,
    /// Order qty (post-only limit). v1 single value.
    qty: Qty,
    /// Per-Polymarket-market cooldown gate. Shared helper from
    /// `strategy-core` so the same semantic ships across all four
    /// in-tree strategies.
    cooldown: CooldownGate<N>,

    /// Monotonic client_oid counter, used for idempotency.
    next_oid: u64,

    // ---- Paper-mode counters (read by cli/dashboards) ----
    /// Total Polymarket ticks observed since `on_start`.
    pub pm_ticks_seen: u64,
    /// Total Binance ticks observed since `on_start`.
    pub bn_ticks_seen: u64,
    /// Total signals observed.
    pub signals_seen: u64,
    /// Total fills observed.
    pub fills_seen: u64,
    /// Total orders emitted via `ctx.submit`.
    pub orders_emitted: u64,
    /// Total orders the dispatcher refused (ring-full).
    pub orders_dropped: u64,
}

/// Default Phase 2 threshold: $0.02 in 1e6 fixed-point.
pub const DEFAULT_THRESHOLD_1E6: i64 = 20_000;

/// Default Phase 2 order quantity: $10 notional in 1e6 fixed-point.
pub const DEFAULT_QTY: Qty = Qty::from_raw(10_000_000);

/// Default Phase 2 cooldown: 250 ms in nanoseconds.
pub const DEFAULT_COOLDOWN_NS: u64 = 250_000_000;

/// Order-type tag for the post-only limit order Phase 2 emits.
const ORDER_KIND_POST_ONLY: u8 = 0;

impl<const N: usize> LatencyArb<N> {
    /// Construct an empty strategy with default thresholds. The
    /// caller must register at least one pair via [`Self::add_pair`]
    /// before `on_start`.
    pub const fn new() -> Self {
        Self {
            pairs: SymbolPairTable::empty(),
            book: MultiBook::empty(),
            binance_mid: [Price::from_raw(0); N],
            binance_seen: [false; N],
            threshold_1e6: DEFAULT_THRESHOLD_1E6,
            qty: DEFAULT_QTY,
            cooldown: CooldownGate::new(DEFAULT_COOLDOWN_NS),
            next_oid: 1,
            pm_ticks_seen: 0,
            bn_ticks_seen: 0,
            signals_seen: 0,
            fills_seen: 0,
            orders_emitted: 0,
            orders_dropped: 0,
        }
    }

    /// Replace the trigger threshold (1e6 units).
    #[inline]
    pub fn set_threshold(&mut self, threshold_1e6: i64) {
        self.threshold_1e6 = threshold_1e6;
    }

    /// Replace the order qty.
    #[inline]
    pub fn set_qty(&mut self, qty: Qty) {
        self.qty = qty;
    }

    /// Replace the cooldown (ns).
    #[inline]
    pub fn set_cooldown_ns(&mut self, cooldown_ns: u64) {
        self.cooldown.set_cooldown_ns(cooldown_ns);
    }

    /// Register a Polymarket ⇄ Binance pair. Boot-only. Also tracks
    /// the Polymarket symbol in the internal book.
    pub fn add_pair(&mut self, pm: SymbolId, bn: SymbolId) -> Result<(), AddPairError> {
        self.pairs
            .add(pm, bn)
            .map_err(AddPairError::PairTable)?;
        self.book.track(pm).map_err(AddPairError::Book)?;
        Ok(())
    }

    /// Number of registered pairs.
    #[inline]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// Whether any pairs are registered.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Borrow the book table (used by tests + paper-mode UIs).
    #[inline]
    pub fn book(&self) -> &MultiBook<N> {
        &self.book
    }

    /// Latest Binance mid for `bn`, if any.
    #[inline]
    pub fn binance_mid(&self, bn: SymbolId) -> Option<Price> {
        let idx = self.pairs.binance_index(bn)?;
        let i = idx as usize;
        if self.binance_seen[i] {
            Some(self.binance_mid[i])
        } else {
            None
        }
    }

    // ---- hot-path internals --------------------------------------

    #[inline(always)]
    fn on_pm_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        self.pm_ticks_seen = self.pm_ticks_seen.wrapping_add(1);
        // Drop ticks for untracked Polymarket symbols silently.
        self.book.apply(tick);
        self.maybe_emit(tick.sym, ctx);
    }

    #[inline(always)]
    fn on_bn_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        self.bn_ticks_seen = self.bn_ticks_seen.wrapping_add(1);
        // Cache the Binance mid keyed by the table index.
        let idx = match self.pairs.binance_index(tick.sym) {
            Some(i) => i as usize,
            None => return,
        };
        let mid = mid_from_tick(tick);
        self.binance_mid[idx] = mid;
        self.binance_seen[idx] = true;

        // Recheck the trigger from the Binance side too — if
        // Binance moved first, we want to fire on its tick, not
        // wait for the next Polymarket tick.
        let pm = match self.pairs.polymarket_for(tick.sym) {
            Some(p) => p,
            None => return,
        };
        self.maybe_emit(pm, ctx);
    }

    #[inline(always)]
    fn maybe_emit<C: Ctx>(&mut self, pm: SymbolId, ctx: &mut C) {
        // Need a paired Binance symbol + a known Binance mid + a
        // populated Polymarket book.
        let bn = match self.pairs.binance_for(pm) {
            Some(b) => b,
            None => return,
        };
        let bn_idx = match self.pairs.binance_index(bn) {
            Some(i) => i as usize,
            None => return,
        };
        if !self.binance_seen[bn_idx] {
            return;
        }
        let pm_idx = match self.book.index_of(pm) {
            Some(i) => i as usize,
            None => return,
        };
        let top = self.book.slots()[pm_idx];
        if !top.has_quotes() {
            return;
        }

        let pm_mid = top.mid().raw();
        let bn_mid = self.binance_mid[bn_idx].raw();
        let delta = pm_mid - bn_mid;
        // Branchless absolute value — but we still need the sign
        // afterwards, so a single compare is fine here.
        let abs_delta = if delta >= 0 { delta } else { -delta };
        if abs_delta < self.threshold_1e6 {
            return;
        }

        let now = ctx.now_ns();
        if !self.cooldown.allow(pm_idx, now) {
            return;
        }

        // Polymarket is rich relative to Binance → sell at the bid
        // (Ask side from our POV on Polymarket). Cheap → buy at the
        // ask (Bid side).
        let side = if delta > 0 { Side::Ask } else { Side::Bid };
        let order = Order::new(
            now,
            pm,
            side,
            ORDER_KIND_POST_ONLY,
            top.mid(),
            self.qty,
            self.next_oid,
        );
        self.next_oid = self.next_oid.wrapping_add(1);

        match ctx.submit(order) {
            Ok(()) => {
                self.orders_emitted = self.orders_emitted.wrapping_add(1);
                self.cooldown.record_emit(pm_idx, now);
            }
            Err(SubmitErr::RingFull) => {
                self.orders_dropped = self.orders_dropped.wrapping_add(1);
            }
        }
    }
}

#[inline(always)]
fn mid_from_tick(tick: &Tick) -> Price {
    Price::from_raw((tick.bid_px.raw() + tick.ask_px.raw()) / 2)
}

impl<const N: usize> Default for LatencyArb<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Why an [`LatencyArb::add_pair`] call rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AddPairError {
    /// The internal pair table rejected.
    PairTable(PairError),
    /// The internal book table rejected.
    Book(book_builder::BookErr),
}

impl<const N: usize> StrategyCounters for LatencyArb<N> {
    #[inline]
    fn orders_emitted(&self) -> u64 {
        self.orders_emitted
    }
    #[inline]
    fn orders_dropped(&self) -> u64 {
        self.orders_dropped
    }
    #[inline]
    fn strategy_kind(&self) -> &'static str {
        "latency-arb"
    }
}

impl<const N: usize> Strategy for LatencyArb<N> {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if self.pairs.is_empty() {
            return Err(StrategyError::Config(
                "strategy-latency-arb: no pairs registered",
            ));
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        // Polymarket ticks always update the book; Binance ticks
        // update the mid cache. We dispatch by *who tracks* the
        // symbol, not by a venue tag — same machinery either way.
        if self.book.index_of(tick.sym).is_some() {
            self.on_pm_tick(tick, ctx);
        } else if self.pairs.binance_index(tick.sym).is_some() {
            self.on_bn_tick(tick, ctx);
        }
        // Symbols neither tracked as Polymarket nor Binance are
        // dropped silently.
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {
        self.signals_seen = self.signals_seen.wrapping_add(1);
        // Phase 2 doesn't react to signals — RPC newHeads and RSS
        // news arrive too slowly to drive trade decisions for
        // strategy B. The counter exists so paper-mode UIs can
        // surface ingest health.
    }

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {
        self.fills_seen = self.fills_seen.wrapping_add(1);
    }

    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}

    #[inline(always)]
    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::Qty;

    /// Test-only `Ctx` that records the last submitted order and
    /// advances a monotonic clock the caller can step.
    struct TestCtx {
        now: NsTs,
        submitted: Vec<Order>,
        next_err: Option<SubmitErr>,
    }

    impl TestCtx {
        fn new() -> Self {
            Self {
                now: 0,
                submitted: Vec::new(),
                next_err: None,
            }
        }
    }

    impl Ctx for TestCtx {
        fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
            if let Some(e) = self.next_err.take() {
                return Err(e);
            }
            self.submitted.push(order);
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    const PM: SymbolId = 100;
    const BN: SymbolId = 200;

    fn mk_strat() -> LatencyArb<4> {
        let mut s: LatencyArb<4> = LatencyArb::new();
        s.add_pair(PM, BN).unwrap();
        s.set_threshold(20_000); // $0.02 fixed-point
        s.set_qty(Qty::from_raw(1_000_000));
        s.set_cooldown_ns(100); // tiny in tests
        s
    }

    fn mk_tick(sym: SymbolId, bid: i64, ask: i64) -> Tick {
        Tick::new(
            0,
            sym,
            1,
            Price::from_raw(bid),
            Qty::from_raw(10),
            Price::from_raw(ask),
            Qty::from_raw(10),
        )
    }

    #[test]
    fn on_start_fails_without_pairs() {
        let mut s: LatencyArb<4> = LatencyArb::new();
        let mut ctx = TestCtx::new();
        assert!(matches!(
            s.on_start(&mut ctx),
            Err(StrategyError::Config(_))
        ));
    }

    #[test]
    fn on_start_passes_with_pair() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        assert!(s.on_start(&mut ctx).is_ok());
    }

    #[test]
    fn no_order_before_binance_seen() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        // PM tick with no Binance reference — should NOT trigger.
        s.on_tick(&mk_tick(PM, 500_000, 510_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn no_order_when_under_threshold() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        // Binance mid = 500_000. PM mid = 500_005. Δ = 5 < 20_000.
        s.on_tick(&mk_tick(BN, 499_000, 501_000), &mut ctx);
        s.on_tick(&mk_tick(PM, 500_000, 500_010), &mut ctx);
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn rich_polymarket_triggers_ask_order() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        // Binance mid = 500_000. PM mid = 600_000. Δ = +100_000 ≥ threshold.
        s.on_tick(&mk_tick(BN, 499_000, 501_000), &mut ctx);
        s.on_tick(&mk_tick(PM, 599_000, 601_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        let o = ctx.submitted[0];
        assert_eq!(o.sym, PM);
        assert_eq!(o.side, Side::Ask, "rich PM → sell on Polymarket");
        assert_eq!(o.px.raw(), 600_000);
    }

    #[test]
    fn cheap_polymarket_triggers_bid_order() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        // Binance mid = 500_000. PM mid = 400_000. Δ = -100_000.
        s.on_tick(&mk_tick(BN, 499_000, 501_000), &mut ctx);
        s.on_tick(&mk_tick(PM, 399_000, 401_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
    }

    #[test]
    fn cooldown_suppresses_duplicate_emit() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(BN, 499_000, 501_000), &mut ctx);
        s.on_tick(&mk_tick(PM, 599_000, 601_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        // Same tick stream a moment later — within cooldown (100 ns).
        ctx.now = 1_050;
        s.on_tick(&mk_tick(PM, 599_000, 601_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1, "still 1 — cooldown active");
        // Past cooldown — emits again.
        ctx.now = 1_200;
        s.on_tick(&mk_tick(PM, 599_000, 601_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 2);
    }

    #[test]
    fn untracked_symbol_dropped_silently() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        // Neither in the PM book nor in the BN map.
        s.on_tick(&mk_tick(999, 1_000_000, 1_000_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(s.pm_ticks_seen, 0);
        assert_eq!(s.bn_ticks_seen, 0);
    }

    #[test]
    fn binance_arrival_can_trigger_emit() {
        // PM tick first (with Polymarket only); then Binance moves
        // far away — emit must fire on the Binance tick, not wait
        // for the next PM tick.
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(PM, 599_000, 601_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
        s.on_tick(&mk_tick(BN, 499_000, 501_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
    }

    #[test]
    fn ring_full_increments_dropped_counter() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(BN, 499_000, 501_000), &mut ctx);
        ctx.next_err = Some(SubmitErr::RingFull);
        s.on_tick(&mk_tick(PM, 599_000, 601_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(s.orders_dropped, 1);
        assert_eq!(s.orders_emitted, 0);
        // Cooldown is NOT updated on a dropped order, so the next
        // tick should retry.
        ctx.now = 1_050;
        s.on_tick(&mk_tick(PM, 599_000, 601_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
    }

    #[test]
    fn add_pair_rejects_overflow() {
        let mut s: LatencyArb<2> = LatencyArb::new();
        s.add_pair(1, 11).unwrap();
        s.add_pair(2, 12).unwrap();
        let err = s.add_pair(3, 13).unwrap_err();
        assert!(matches!(err, AddPairError::PairTable(PairError::Full)));
    }

    #[test]
    fn pair_table_rejects_sentinel() {
        let mut t: SymbolPairTable<4> = SymbolPairTable::empty();
        assert_eq!(t.add(SYMBOL_ID_NONE, 1), Err(PairError::ReservedSymbol));
        assert_eq!(t.add(1, SYMBOL_ID_NONE), Err(PairError::ReservedSymbol));
    }

    #[test]
    fn binance_mid_getter_works() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        s.on_tick(&mk_tick(BN, 499_000, 501_000), &mut ctx);
        assert_eq!(s.binance_mid(BN), Some(Price::from_raw(500_000)));
        assert_eq!(s.binance_mid(999), None);
    }

    #[test]
    fn counters_advance_on_each_callback() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        s.on_tick(&mk_tick(PM, 500_000, 510_000), &mut ctx);
        assert_eq!(s.pm_ticks_seen, 1);
        s.on_tick(&mk_tick(BN, 499_000, 501_000), &mut ctx);
        assert_eq!(s.bn_ticks_seen, 1);
    }
}
