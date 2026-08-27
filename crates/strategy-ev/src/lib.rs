// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-ev
//!
//! Strategy A: mispricing vs model-derived true probability.
//!
//! Compile-time monomorphized. State is fully inline:
//!
//! * `MultiBook<N>` for per-Polymarket-symbol top-of-book,
//! * `ArtifactTable<N>` of `(asset_id → model_p_1e6, family, impact)`
//!   loaded at boot from `claude-worker`'s NDJSON output,
//! * `[u8; N * KEY_LEN]` of per-symbol asset id bytes so the
//!   strategy can look up the artifact without holding any borrows.
//!
//! ## Decision rule
//!
//! For each tracked Polymarket symbol `ps`:
//!
//! ```text
//! tag         = table.lookup(asset_id_for(ps))?    // None → skip
//! book.apply(tick)
//! mid_1e6     = book.snapshot(ps).mid()            // 0..1_000_000
//! delta       = mid_1e6 - tag.model_p_1e6          // signed 1e6
//! if abs(delta) < threshold:    skip
//! if (now - last_emit_ns) < cooldown: skip
//! side = delta > 0 ? Ask  /* market rich → sell */
//!                  : Bid  /* market cheap → buy  */
//! ctx.submit(Order { px: book.snapshot(ps).mid(), qty: self.qty, ... })
//! last_emit_ns[ps] = now
//! ```
//!
//! Hot path: one `MultiBook::apply` + one `ArtifactTable::lookup`
//! (both O(N), N ≤ 64) + a handful of integer compares. Zero alloc
//! verified by the bench harness.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use book_builder::{BookErr, MultiBook};
use core_time::NsTs;
use core_types::{Fill, Order, Price, Qty, Side, Signal, SymbolId, Tick, VenueId, SYMBOL_ID_NONE};
use research_artifacts::{ArtifactTable, KEY_LEN};
use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

/// Default Phase 5 threshold: 0.02 (2 cents on a 0..1 binary).
pub const DEFAULT_THRESHOLD_1E6: i64 = 20_000;

/// Default order quantity (1e6 fixed-point).
pub const DEFAULT_QTY: Qty = Qty::from_raw(10_000_000);

/// Default cooldown (250 ms).
pub const DEFAULT_COOLDOWN_NS: u64 = 250_000_000;

const ORDER_KIND_POST_ONLY: u8 = 0;

/// Per-symbol asset-id slot used to look up the artifact table.
#[derive(Copy, Clone, Debug)]
struct AssetKey {
    bytes: [u8; KEY_LEN],
    len: u8,
}

impl AssetKey {
    const fn empty() -> Self {
        Self {
            bytes: [0u8; KEY_LEN],
            len: 0,
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// Why an [`EvStrategy::register`] call rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// Caller passed `SYMBOL_ID_NONE`.
    ReservedSymbol,
    /// `asset_id` exceeds [`KEY_LEN`].
    AssetIdTooLong,
    /// Underlying book table is full.
    Book(BookErr),
    /// Strategy's asset-id slot table is full (per-strategy `N` cap).
    Full,
    /// Same Polymarket symbol already registered.
    Duplicate,
}

/// Model-vs-market mispricing strategy.
///
/// `N` is the slot capacity for both the book table and the
/// parallel asset-id array. Set at boot.
pub struct EvStrategy<const N: usize> {
    table: ArtifactTable<N>,
    book: MultiBook<N>,
    /// Per-symbol asset id used as the lookup key into the table.
    asset_ids: [AssetKey; N],
    /// Per-symbol cooldown deadlines (ns).
    last_emit_ns: [u64; N],

    threshold_1e6: i64,
    qty: Qty,
    cooldown_ns: u64,
    next_oid: u64,

    /// Cumulative counters (read by paper-mode UIs).
    pub pm_ticks_seen: u64,
    /// Total signals observed.
    pub signals_seen: u64,
    /// Total fills observed.
    pub fills_seen: u64,
    /// Orders the strategy emitted via `ctx.submit`.
    pub orders_emitted: u64,
    /// Orders the dispatcher rejected (ring-full).
    pub orders_dropped: u64,
}

impl<const N: usize> EvStrategy<N> {
    /// Construct with default thresholds. Boot-only.
    pub fn new() -> Self {
        Self {
            table: ArtifactTable::empty(),
            book: MultiBook::empty(),
            asset_ids: [AssetKey::empty(); N],
            last_emit_ns: [0u64; N],
            threshold_1e6: DEFAULT_THRESHOLD_1E6,
            qty: DEFAULT_QTY,
            cooldown_ns: DEFAULT_COOLDOWN_NS,
            next_oid: 1,
            pm_ticks_seen: 0,
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
        self.cooldown_ns = cooldown_ns;
    }

    /// Borrow the underlying artifact table (for tests + dashboards).
    #[inline]
    pub fn table(&self) -> &ArtifactTable<N> {
        &self.table
    }

    /// Mutable handle to the artifact table — call once at boot to
    /// populate it.
    #[inline]
    pub fn table_mut(&mut self) -> &mut ArtifactTable<N> {
        &mut self.table
    }

    /// Borrow the book table.
    #[inline]
    pub fn book(&self) -> &MultiBook<N> {
        &self.book
    }

    /// Number of registered symbols.
    #[inline]
    pub fn len(&self) -> usize {
        self.book.len()
    }

    /// Whether any symbols are registered.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.book.is_empty()
    }

    /// Register a Polymarket symbol with its asset-id key. The key
    /// must match the artifact-table entry the strategy will look
    /// up. Boot-only.
    pub fn register(&mut self, sym: SymbolId, asset_id: &[u8]) -> Result<(), RegisterError> {
        if sym == SYMBOL_ID_NONE {
            return Err(RegisterError::ReservedSymbol);
        }
        if asset_id.len() > KEY_LEN {
            return Err(RegisterError::AssetIdTooLong);
        }
        let idx = self.book.track(sym).map_err(map_book_err)?;
        let i = idx as usize;
        if i >= N {
            return Err(RegisterError::Full);
        }
        self.asset_ids[i].bytes[..asset_id.len()].copy_from_slice(asset_id);
        self.asset_ids[i].len = asset_id.len() as u8;
        Ok(())
    }

    // ---- hot path ----

    #[inline(always)]
    fn maybe_emit<C: Ctx>(&mut self, idx: usize, ctx: &mut C) {
        let top = self.book.slots()[idx];
        if !top.has_quotes() {
            return;
        }
        let asset = self.asset_ids[idx].as_slice();
        let tag = match self.table.lookup(asset) {
            Some(t) => t,
            None => return,
        };

        let mid_1e6 = top.mid().raw();
        let p_1e6 = tag.model_p_1e6 as i64;
        let delta = mid_1e6 - p_1e6;
        let abs_delta = if delta >= 0 { delta } else { -delta };
        if abs_delta < self.threshold_1e6 {
            return;
        }

        let now = ctx.now_ns();
        if now < self.last_emit_ns[idx].saturating_add(self.cooldown_ns) {
            return;
        }

        let side = if delta > 0 { Side::Ask } else { Side::Bid };
        let order = Order::new(
            now,
            VenueId::Polymarket,
            top.sym,
            side,
            ORDER_KIND_POST_ONLY,
            Price::from_raw(mid_1e6),
            self.qty,
            self.next_oid,
        );
        self.next_oid = self.next_oid.wrapping_add(1);

        match ctx.submit(order) {
            Ok(()) => {
                self.orders_emitted = self.orders_emitted.wrapping_add(1);
                self.last_emit_ns[idx] = now;
            }
            Err(SubmitErr::RingFull) => {
                self.orders_dropped = self.orders_dropped.wrapping_add(1);
            }
        }
    }
}

impl<const N: usize> Default for EvStrategy<N> {
    fn default() -> Self {
        Self::new()
    }
}

fn map_book_err(e: BookErr) -> RegisterError {
    match e {
        BookErr::Full => RegisterError::Full,
        BookErr::ReservedSymbol => RegisterError::ReservedSymbol,
        BookErr::AlreadyTracked => RegisterError::Duplicate,
    }
}

impl<const N: usize> StrategyCounters for EvStrategy<N> {
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
        "ev"
    }
}

impl<const N: usize> Strategy for EvStrategy<N> {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if self.book.is_empty() {
            return Err(StrategyError::Config("strategy-ev: no symbols registered"));
        }
        if self.table.is_empty() {
            return Err(StrategyError::Config("strategy-ev: artifact table empty"));
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        // Only Polymarket ticks drive this strategy. Look up by
        // SymbolId in the book; unknown symbols (e.g. Binance) are
        // dropped silently.
        let idx = match self.book.index_of(tick.sym) {
            Some(i) => i as usize,
            None => return,
        };
        self.pm_ticks_seen = self.pm_ticks_seen.wrapping_add(1);
        self.book.apply(tick);
        self.maybe_emit(idx, ctx);
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {
        self.signals_seen = self.signals_seen.wrapping_add(1);
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
    use research_artifacts::{Family, Impact};

    /// Recording context — captures submits + drives a fake clock.
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
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            if let Some(e) = self.next_err.take() {
                return Err(e);
            }
            self.submitted.push(o);
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    const PM: SymbolId = 42;
    const ASSET: &[u8] = b"0xabc";

    fn mk_strat() -> EvStrategy<4> {
        let mut s: EvStrategy<4> = EvStrategy::new();
        s.set_threshold(20_000);
        s.set_qty(Qty::from_raw(1_000_000));
        s.set_cooldown_ns(100);
        s.register(PM, ASSET).unwrap();
        s.table_mut()
            .insert(ASSET, 500_000, Family::Crypto, Impact::High)
            .unwrap();
        s
    }

    fn mk_tick(sym: SymbolId, bid: i64, ask: i64) -> Tick {
        Tick::new(
            0,
            VenueId::Polymarket,
            sym,
            1,
            Price::from_raw(bid),
            Qty::from_raw(10),
            Price::from_raw(ask),
            Qty::from_raw(10),
        )
    }

    #[test]
    fn on_start_fails_without_symbols() {
        let mut s: EvStrategy<4> = EvStrategy::new();
        let mut ctx = TestCtx::new();
        assert!(matches!(
            s.on_start(&mut ctx),
            Err(StrategyError::Config(_))
        ));
    }

    #[test]
    fn on_start_fails_without_artifact_table() {
        let mut s: EvStrategy<4> = EvStrategy::new();
        s.register(PM, ASSET).unwrap();
        let mut ctx = TestCtx::new();
        assert!(matches!(
            s.on_start(&mut ctx),
            Err(StrategyError::Config(_))
        ));
    }

    #[test]
    fn on_start_passes_with_full_config() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        assert!(s.on_start(&mut ctx).is_ok());
    }

    #[test]
    fn unknown_symbol_is_dropped_silently() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        s.on_tick(&mk_tick(999, 400_000, 600_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(s.pm_ticks_seen, 0);
    }

    #[test]
    fn no_order_when_under_threshold() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        // mid = 500_005, model = 500_000 → delta = 5 < 20_000.
        s.on_tick(&mk_tick(PM, 500_000, 500_010), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(s.pm_ticks_seen, 1);
    }

    #[test]
    fn rich_market_triggers_ask_order() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        // mid = 700_000, model = 500_000 → delta = +200_000.
        s.on_tick(&mk_tick(PM, 690_000, 710_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        let o = ctx.submitted[0];
        assert_eq!(o.sym, PM);
        assert_eq!(o.side, Side::Ask);
        assert_eq!(o.px.raw(), 700_000);
    }

    #[test]
    fn cheap_market_triggers_bid_order() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        // mid = 300_000, model = 500_000 → delta = -200_000.
        s.on_tick(&mk_tick(PM, 290_000, 310_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
    }

    #[test]
    fn cooldown_suppresses_duplicate_emit() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(PM, 690_000, 710_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        ctx.now = 1_050; // still within 100 ns cooldown
        s.on_tick(&mk_tick(PM, 690_000, 710_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        ctx.now = 1_200; // past cooldown
        s.on_tick(&mk_tick(PM, 690_000, 710_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 2);
    }

    #[test]
    fn ring_full_increments_dropped_counter() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        ctx.next_err = Some(SubmitErr::RingFull);
        s.on_tick(&mk_tick(PM, 690_000, 710_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(s.orders_dropped, 1);
        assert_eq!(s.orders_emitted, 0);
        // Cooldown should NOT have been updated.
        ctx.now = 1_050;
        s.on_tick(&mk_tick(PM, 690_000, 710_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
    }

    #[test]
    fn register_rejects_sentinel() {
        let mut s: EvStrategy<4> = EvStrategy::new();
        assert_eq!(s.register(SYMBOL_ID_NONE, b"x"), Err(RegisterError::ReservedSymbol));
    }

    #[test]
    fn register_rejects_duplicates() {
        let mut s: EvStrategy<4> = EvStrategy::new();
        s.register(PM, ASSET).unwrap();
        assert_eq!(s.register(PM, ASSET), Err(RegisterError::Duplicate));
    }

    #[test]
    fn register_rejects_oversized_asset() {
        let mut s: EvStrategy<4> = EvStrategy::new();
        let big = [b'x'; KEY_LEN + 1];
        assert_eq!(
            s.register(PM, &big),
            Err(RegisterError::AssetIdTooLong)
        );
    }
}
