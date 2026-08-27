// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-cross-arb
//!
//! Strategy C: cross-market arbitrage.
//!
//! When a group of mutually-exclusive Polymarket binaries (e.g.
//! all candidates in an election market) sums to a probability
//! that deviates from 1.0 by more than a threshold, emit one
//! order per group member that closes the gap and locks in the
//! implied edge.
//!
//! Compile-time monomorphized. State is fully inline:
//!
//! * `[MarketGroup<M>; N]` — up to `N` groups of up to `M`
//!   symbols each.
//! * `MultiBook<{ N * M }>` — top-of-book per registered symbol.
//! * `[u64; N]` per-group cooldown deadlines.
//!
//! Hot path: one `MultiBook::apply` + one group lookup + at most
//! `M` integer summations + `M` `ctx.submit` calls. Zero-alloc
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

use book_builder::{BookErr, MultiBook, TopOfBook};
use core_time::NsTs;
use core_types::{Fill, Order, Qty, Side, Signal, SymbolId, Tick, VenueId, SYMBOL_ID_NONE};
use strategy_core::{CooldownGate, Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

/// Probability "1.0" in 1e6 fixed-point.
pub const ONE_1E6: i64 = 1_000_000;

/// Default Phase 6 threshold: 0.02 (2 cents from sum=1).
pub const DEFAULT_THRESHOLD_1E6: i64 = 20_000;

/// Default per-leg order quantity.
pub const DEFAULT_QTY: Qty = Qty::from_raw(10_000_000);

/// Default cooldown (250 ms) between emit storms per group.
pub const DEFAULT_COOLDOWN_NS: u64 = 250_000_000;

const ORDER_KIND_POST_ONLY: u8 = 0;

// -----------------------------------------------------------------
// MarketGroup
// -----------------------------------------------------------------

/// A fixed-capacity, mutually-exclusive market partition.
///
/// `M` is the maximum number of members per group. Unused slots
/// hold `SYMBOL_ID_NONE`.
#[derive(Copy, Clone, Debug)]
pub struct MarketGroup<const M: usize> {
    members: [SymbolId; M],
    count: u32,
}

impl<const M: usize> MarketGroup<M> {
    /// Empty group.
    pub const fn empty() -> Self {
        Self {
            members: [SYMBOL_ID_NONE; M],
            count: 0,
        }
    }

    /// Member symbols, populated prefix only.
    #[inline]
    pub fn members(&self) -> &[SymbolId] {
        &self.members[..self.count as usize]
    }

    /// Length / population count.
    #[inline]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether this group has no members.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Opaque group handle returned from [`CrossArb::register_group`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GroupId(u32);

/// Why a `register_group` call rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GroupErr {
    /// Caller passed `SYMBOL_ID_NONE` as a member.
    ReservedSymbol,
    /// Member count > `M`.
    TooManyMembers,
    /// Strategy group table is full.
    Full,
    /// One of the members is already tracked in another group.
    Duplicate,
    /// Underlying book table can't track another symbol.
    Book(BookErr),
}

// -----------------------------------------------------------------
// CrossArb — Strategy C
// -----------------------------------------------------------------

/// Cross-market arbitrage strategy.
///
/// * `N` — max number of distinct groups.
/// * `M` — max symbols per group.
///
/// The internal book is `MultiBook<{N * M}>` in spirit, but Rust
/// 1.88 lacks `generic_const_exprs`. We pick a single `BOOK_CAP`
/// that's large enough for the small group sizes we care about
/// in v1 (≤ 8 groups × ≤ 8 members = 64 symbols).
pub struct CrossArb<const N: usize, const M: usize> {
    groups: [MarketGroup<M>; N],
    group_count: u32,
    book: MultiBook<BOOK_CAP>,
    threshold_1e6: i64,
    qty: Qty,
    cooldown: CooldownGate<N>,
    next_oid: u64,

    /// Cumulative counters (read by paper-mode UIs).
    pub pm_ticks_seen: u64,
    /// Orders emitted via `ctx.submit`.
    pub orders_emitted: u64,
    /// Orders the dispatcher rejected (ring-full).
    pub orders_dropped: u64,
    /// Groups whose emit fired some — but not all — legs (i.e. at
    /// least one leg was emitted AND at least one was dropped).
    /// Each occurrence leaves the operator with directional
    /// exposure on the legs that landed; the cli should log this
    /// loudly. Phase 7 will wire fill-tracking unwind logic.
    pub partial_fill_groups: u64,
}

/// Book capacity wired into [`CrossArb`]. Bump and recompile.
pub const BOOK_CAP: usize = 64;

impl<const N: usize, const M: usize> CrossArb<N, M> {
    /// Construct with default thresholds.
    pub fn new() -> Self {
        Self {
            groups: [MarketGroup::empty(); N],
            group_count: 0,
            book: MultiBook::empty(),
            threshold_1e6: DEFAULT_THRESHOLD_1E6,
            qty: DEFAULT_QTY,
            cooldown: CooldownGate::new(DEFAULT_COOLDOWN_NS),
            next_oid: 1,
            pm_ticks_seen: 0,
            orders_emitted: 0,
            orders_dropped: 0,
            partial_fill_groups: 0,
        }
    }

    /// Replace the sum-deviation threshold (1e6 units).
    #[inline]
    pub fn set_threshold(&mut self, threshold_1e6: i64) {
        self.threshold_1e6 = threshold_1e6;
    }

    /// Replace the per-leg order quantity.
    #[inline]
    pub fn set_qty(&mut self, qty: Qty) {
        self.qty = qty;
    }

    /// Replace the cooldown (ns).
    #[inline]
    pub fn set_cooldown_ns(&mut self, cooldown_ns: u64) {
        self.cooldown.set_cooldown_ns(cooldown_ns);
    }

    /// Number of registered groups.
    #[inline]
    pub fn len(&self) -> usize {
        self.group_count as usize
    }

    /// Whether any groups are registered.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.group_count == 0
    }

    /// Borrow the book table (for tests / dashboards).
    #[inline]
    pub fn book(&self) -> &MultiBook<BOOK_CAP> {
        &self.book
    }

    /// Register a market group. Boot-only.
    pub fn register_group(&mut self, members: &[SymbolId]) -> Result<GroupId, GroupErr> {
        if members.len() > M {
            return Err(GroupErr::TooManyMembers);
        }
        if (self.group_count as usize) >= N {
            return Err(GroupErr::Full);
        }
        // Cross-group duplicate check.
        let existing = self.group_count as usize;
        for &m in members {
            if m == SYMBOL_ID_NONE {
                return Err(GroupErr::ReservedSymbol);
            }
            let mut g = 0;
            while g < existing {
                let n = self.groups[g].count as usize;
                let mut i = 0;
                while i < n {
                    if self.groups[g].members[i] == m {
                        return Err(GroupErr::Duplicate);
                    }
                    i += 1;
                }
                g += 1;
            }
            // Track the symbol in the book.
            match self.book.track(m) {
                Ok(_) => {}
                Err(BookErr::AlreadyTracked) => return Err(GroupErr::Duplicate),
                Err(e) => return Err(GroupErr::Book(e)),
            }
        }
        let idx = self.group_count as usize;
        self.groups[idx].members[..members.len()].copy_from_slice(members);
        self.groups[idx].count = members.len() as u32;
        self.group_count = self.group_count.wrapping_add(1);
        Ok(GroupId(idx as u32))
    }

    /// Find the group containing `sym`. O(N * M).
    #[inline]
    fn group_of(&self, sym: SymbolId) -> Option<u32> {
        let n = self.group_count as usize;
        let mut g = 0u32;
        while (g as usize) < n {
            let group = &self.groups[g as usize];
            let count = group.count as usize;
            let mut i = 0;
            while i < count {
                if group.members[i] == sym {
                    return Some(g);
                }
                i += 1;
            }
            g += 1;
        }
        None
    }

    // ---- hot path ----

    #[inline(always)]
    fn maybe_emit<C: Ctx>(&mut self, group_idx: usize, ctx: &mut C) {
        let group = self.groups[group_idx];
        let count = group.count as usize;
        if count == 0 {
            return;
        }

        // Sum the per-member mids. All members must have quotes —
        // otherwise the partial state isn't actionable.
        let mut sum_1e6: i64 = 0;
        let mut tops = [TopOfBook::empty(SYMBOL_ID_NONE); 8];
        debug_assert!(count <= tops.len(), "M must be ≤ 8 in v1");
        let mut i = 0;
        while i < count {
            let m = group.members[i];
            let top = match self.book.snapshot(m) {
                Some(t) if t.has_quotes() => t,
                _ => return,
            };
            tops[i] = top;
            sum_1e6 = sum_1e6.saturating_add(top.mid().raw());
            i += 1;
        }

        let delta = sum_1e6 - ONE_1E6;
        let abs_delta = if delta >= 0 { delta } else { -delta };
        if abs_delta < self.threshold_1e6 {
            return;
        }

        let now = ctx.now_ns();
        if !self.cooldown.allow(group_idx, now) {
            return;
        }

        // delta > 0: sum overpriced → sell every member.
        // delta < 0: sum underpriced → buy every member.
        let side = if delta > 0 { Side::Ask } else { Side::Bid };

        // Per-leg qty allocation. Plain `qty / count` rounds toward
        // zero — for `count=3, qty=$10` that's `$3` per leg, sum
        // `$9` not `$10`. We distribute the remainder so the
        // **total notional matches `self.qty` exactly**:
        //
        //   base      = qty / count
        //   remainder = qty % count
        //   first `remainder` legs get `base + 1`; the rest get `base`.
        //
        // Phase 7 will replace this even allocation with deviation-
        // weighted legs; v1 is operator-predictable and conserves
        // notional.
        let total_qty = self.qty.raw();
        let count_i = count as i64;
        let base = if count_i > 0 { total_qty / count_i } else { 0 };
        let remainder = if count_i > 0 { (total_qty % count_i).max(0) as usize } else { 0 };
        if base == 0 && remainder == 0 {
            return;
        }

        // Emit all legs. Track BOTH any-emitted and any-dropped so
        // we can detect the partial-fill state where the operator
        // is left with directional exposure on the legs that
        // landed.
        let mut emitted_any = false;
        let mut dropped_any = false;
        let mut i = 0;
        while i < count {
            // First `remainder` legs get one extra unit so the sum
            // matches `self.qty` to the raw integer.
            let leg_raw = if i < remainder { base + 1 } else { base };
            if leg_raw <= 0 {
                i += 1;
                continue;
            }
            let per_leg = Qty::from_raw(leg_raw);
            let order = Order::new(
                now,
                VenueId::Polymarket,
                tops[i].sym,
                side,
                ORDER_KIND_POST_ONLY,
                tops[i].mid(),
                per_leg,
                self.next_oid,
            );
            self.next_oid = self.next_oid.wrapping_add(1);
            match ctx.submit(order) {
                Ok(()) => {
                    self.orders_emitted = self.orders_emitted.wrapping_add(1);
                    emitted_any = true;
                }
                Err(SubmitErr::RingFull) => {
                    self.orders_dropped = self.orders_dropped.wrapping_add(1);
                    dropped_any = true;
                }
            }
            i += 1;
        }

        // Partial-fill detection — surface loudly. Phase 7 will
        // wire unwind logic; until then this is the operator's
        // signal that they need to manually flatten exposure.
        if emitted_any && dropped_any {
            self.partial_fill_groups = self.partial_fill_groups.wrapping_add(1);
        }

        // Update cooldown only if at least one leg made it in —
        // otherwise the next tick should retry the whole group.
        if emitted_any {
            self.cooldown.record_emit(group_idx, now);
        }
    }
}

impl<const N: usize, const M: usize> Default for CrossArb<N, M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const M: usize> StrategyCounters for CrossArb<N, M> {
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
        "cross-arb"
    }
}

impl<const N: usize, const M: usize> Strategy for CrossArb<N, M> {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if self.group_count == 0 {
            return Err(StrategyError::Config(
                "strategy-cross-arb: no groups registered",
            ));
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        let group_idx = match self.group_of(tick.sym) {
            Some(i) => i as usize,
            None => return,
        };
        self.pm_ticks_seen = self.pm_ticks_seen.wrapping_add(1);
        self.book.apply(tick);
        self.maybe_emit(group_idx, ctx);
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}

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
    use core_types::Price;

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

    fn mk_strat() -> CrossArb<4, 3> {
        let mut s: CrossArb<4, 3> = CrossArb::new();
        s.set_threshold(20_000);
        s.set_qty(Qty::from_raw(3_000_000));
        s.set_cooldown_ns(100);
        s.register_group(&[10, 11, 12]).unwrap();
        s
    }

    #[test]
    fn on_start_fails_without_groups() {
        let mut s: CrossArb<4, 3> = CrossArb::new();
        let mut ctx = TestCtx::new();
        assert!(matches!(
            s.on_start(&mut ctx),
            Err(StrategyError::Config(_))
        ));
    }

    #[test]
    fn on_start_passes_after_register() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        assert!(s.on_start(&mut ctx).is_ok());
    }

    #[test]
    fn register_rejects_oversized_member_list() {
        let mut s: CrossArb<4, 3> = CrossArb::new();
        assert_eq!(
            s.register_group(&[1, 2, 3, 4]),
            Err(GroupErr::TooManyMembers)
        );
    }

    #[test]
    fn register_rejects_sentinel_member() {
        let mut s: CrossArb<4, 3> = CrossArb::new();
        assert_eq!(
            s.register_group(&[1, SYMBOL_ID_NONE]),
            Err(GroupErr::ReservedSymbol)
        );
    }

    #[test]
    fn register_rejects_duplicate_across_groups() {
        let mut s: CrossArb<4, 3> = CrossArb::new();
        s.register_group(&[1, 2]).unwrap();
        assert_eq!(s.register_group(&[2, 3]), Err(GroupErr::Duplicate));
    }

    #[test]
    fn no_fire_when_sum_balanced() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        // mids: 333_333 + 333_333 + 333_334 ≈ 1_000_000.
        s.on_tick(&mk_tick(10, 333_330, 333_336), &mut ctx);
        s.on_tick(&mk_tick(11, 333_330, 333_336), &mut ctx);
        s.on_tick(&mk_tick(12, 333_331, 333_337), &mut ctx);
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn fire_asks_when_sum_overpriced() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        // 400_000 + 400_000 + 300_000 = 1_100_000 → +100_000 ≥ threshold.
        s.on_tick(&mk_tick(10, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(11, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(12, 295_000, 305_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 3);
        let mut i = 0;
        while i < 3 {
            assert_eq!(ctx.submitted[i].side, Side::Ask);
            i += 1;
        }
    }

    #[test]
    fn fire_bids_when_sum_underpriced() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        // 200_000 + 250_000 + 250_000 = 700_000 → -300_000.
        s.on_tick(&mk_tick(10, 195_000, 205_000), &mut ctx);
        s.on_tick(&mk_tick(11, 245_000, 255_000), &mut ctx);
        s.on_tick(&mk_tick(12, 245_000, 255_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 3);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
    }

    #[test]
    fn no_fire_until_all_members_have_quotes() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        // Only two of three members have ticked.
        s.on_tick(&mk_tick(10, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(11, 395_000, 405_000), &mut ctx);
        assert!(ctx.submitted.is_empty(), "partial state should not fire");
    }

    #[test]
    fn cooldown_suppresses_repeated_fire() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        // Prime + fire.
        s.on_tick(&mk_tick(10, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(11, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(12, 295_000, 305_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 3);
        // Within cooldown.
        ctx.now = 1_050;
        s.on_tick(&mk_tick(12, 295_000, 305_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 3, "cooldown should suppress");
        // Past cooldown.
        ctx.now = 1_200;
        s.on_tick(&mk_tick(12, 295_000, 305_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 6);
    }

    #[test]
    fn ring_full_increments_dropped_and_partial_counters() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        // Prime two members.
        s.on_tick(&mk_tick(10, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(11, 395_000, 405_000), &mut ctx);
        // Inject error on the FIRST leg submit; legs 2/3 land.
        ctx.next_err = Some(SubmitErr::RingFull);
        s.on_tick(&mk_tick(12, 295_000, 305_000), &mut ctx);
        // One drop + two emits ⇒ partial fill.
        assert_eq!(s.orders_dropped, 1);
        assert_eq!(s.orders_emitted, 2);
        assert_eq!(
            s.partial_fill_groups, 1,
            "partial-fill counter must bump when some legs land and others drop"
        );
    }

    #[test]
    fn full_success_does_not_bump_partial_counter() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(10, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(11, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(12, 295_000, 305_000), &mut ctx);
        assert_eq!(s.orders_emitted, 3);
        assert_eq!(s.orders_dropped, 0);
        assert_eq!(s.partial_fill_groups, 0);
    }

    #[test]
    fn per_leg_qty_sums_to_total_qty_even_with_remainder() {
        // qty=10, count=3 → legs of (4, 3, 3). Sum = 10.
        let mut s: CrossArb<4, 3> = CrossArb::new();
        s.set_threshold(20_000);
        s.set_qty(Qty::from_raw(10));
        s.set_cooldown_ns(100);
        s.register_group(&[10, 11, 12]).unwrap();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(10, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(11, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(12, 295_000, 305_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 3);
        let sum: i64 = ctx.submitted.iter().map(|o| o.qty.raw()).sum();
        assert_eq!(sum, 10, "per-leg total must equal configured qty");
        // First `remainder = 10 % 3 = 1` leg gets base+1 = 4; the
        // remaining 2 legs get base = 3.
        assert_eq!(ctx.submitted[0].qty.raw(), 4);
        assert_eq!(ctx.submitted[1].qty.raw(), 3);
        assert_eq!(ctx.submitted[2].qty.raw(), 3);
    }

    #[test]
    fn per_leg_qty_evenly_divides_when_qty_is_multiple_of_count() {
        // qty=9, count=3 → legs of (3, 3, 3). No remainder.
        let mut s: CrossArb<4, 3> = CrossArb::new();
        s.set_threshold(20_000);
        s.set_qty(Qty::from_raw(9));
        s.set_cooldown_ns(100);
        s.register_group(&[10, 11, 12]).unwrap();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(10, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(11, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(12, 295_000, 305_000), &mut ctx);
        let sum: i64 = ctx.submitted.iter().map(|o| o.qty.raw()).sum();
        assert_eq!(sum, 9);
        for o in &ctx.submitted {
            assert_eq!(o.qty.raw(), 3);
        }
    }

    #[test]
    fn per_leg_qty_does_not_emit_when_total_is_zero() {
        let mut s: CrossArb<4, 3> = CrossArb::new();
        s.set_threshold(20_000);
        s.set_qty(Qty::from_raw(0));
        s.set_cooldown_ns(100);
        s.register_group(&[10, 11, 12]).unwrap();
        let mut ctx = TestCtx::new();
        ctx.now = 1_000;
        s.on_tick(&mk_tick(10, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(11, 395_000, 405_000), &mut ctx);
        s.on_tick(&mk_tick(12, 295_000, 305_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn unknown_symbol_dropped_silently() {
        let mut s = mk_strat();
        let mut ctx = TestCtx::new();
        s.on_tick(&mk_tick(99, 500_000, 500_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(s.pm_ticks_seen, 0);
    }
}
