// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The compositing dispatcher: one `OrderDispatch` over two arms,
//! choosing between them on `Order.strategy_id`.
//!
//! ## The hot path
//!
//! One masked byte load from the route table, one three-way branch,
//! one delegated call. No `dyn`, no allocation, no lock, no syscall.
//! Both arms are concrete type parameters, so the whole thing
//! monomorphises and the branch predicts perfectly in the steady state
//! (a given slot's mode never changes within a boot).
//!
//! ## The two laws this type exists to enforce
//!
//! **LAW E-1 — a live slot never falls back to paper.** A `Live` slot
//! whose order names a venue it has no route to is *refused and
//! counted*, never quietly matched on paper. A modelled fill carries
//! `FILL_ORIGIN_PAPER`, but every downstream reader of a live slot's
//! tape is entitled to assume its fills happened; silently satisfying
//! a live order on paper would put a trade that never occurred into
//! the P&L with live semantics.
//!
//! **LAW E-2 — the matcher's counters describe the PAPER arm only.**
//! `matcher_counters()` and `open_paper_orders()` forward to the paper
//! arm and nothing else, so `/metrics` can never suggest the matcher
//! is modelling something the venue is really doing.
//!
//! ## Where live fills come from (plan §0.1-1)
//!
//! **Not from here.** `QueuedDispatcher::try_next_fill` returns `None`
//! by contract ("fills flow through a separate path (engine fill
//! ring)"), and the engine already owns per-venue fill lanes —
//! `engine::fill_lane_of(VenueId::Hyperliquid) == Some(3)`, whose
//! producer the live worker thread takes over in E4. The engine drains
//! fill lanes *before* the dispatcher fill pump, so "a real fill never
//! queues behind a modelled one" is already a structural property of
//! the loop and costs no code here. `try_next_fill` therefore forwards
//! the **paper arm only**.

use crate::counters::RouteCounters;
use crate::mode::ExecMode;
use crate::route::ExecRoute;
use clob_dispatcher::{
    DispatchError, DispatchStats, ExecCounters, MatcherCounters, OrderDispatch,
};
use core_types::{CancelReq, Fill, ModifyReq, NsTs, Order, Tick};

/// Routes each order to the paper matcher or the live arm according to
/// the boot-fixed [`ExecRoute`].
///
/// With an all-paper table this is observationally identical to the
/// bare paper arm — that equivalence is the E1 acceptance property and
/// is asserted in `tests::an_all_paper_table_is_indistinguishable_…`.
#[derive(Debug)]
pub struct RoutedDispatcher<P: OrderDispatch, L: OrderDispatch> {
    route: ExecRoute,
    paper: P,
    live: L,
    counters: RouteCounters,
}

impl<P: OrderDispatch, L: OrderDispatch> RoutedDispatcher<P, L> {
    /// Compose the two arms under `route`. Boot-only.
    #[inline]
    pub fn new(route: ExecRoute, paper: P, live: L) -> Self {
        Self {
            route,
            paper,
            live,
            counters: RouteCounters::new(),
        }
    }

    /// The table in force. Cold; boot tell, `/state`.
    #[inline]
    #[must_use]
    pub fn route(&self) -> &ExecRoute {
        &self.route
    }

    /// What the router did. Cold; the 5 s metrics tick.
    #[inline]
    #[must_use]
    pub fn counters(&self) -> RouteCounters {
        self.counters
    }

    /// The paper arm, for tests and for the cli's `/state` assembly.
    #[inline]
    pub fn paper(&self) -> &P {
        &self.paper
    }

    /// The live arm, same.
    #[inline]
    pub fn live(&self) -> &L {
        &self.live
    }
}

impl<P: OrderDispatch, L: OrderDispatch> OrderDispatch for RoutedDispatcher<P, L> {
    /// Route one order. **Hot path.**
    #[inline]
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        match self.route.mode(order.strategy_id) {
            ExecMode::Paper => {
                self.counters.on_paper_submit();
                self.paper.submit(order)
            }
            ExecMode::Off => {
                self.counters.on_refused_off(order.strategy_id);
                Err(DispatchError::SlotDisabled)
            }
            ExecMode::Live => {
                if !self.route.venue_allowed(order.strategy_id, order.venue) {
                    // LAW E-1. NOT `self.paper.submit(order)`.
                    self.counters.on_refused_no_route(order.strategy_id);
                    return Err(DispatchError::NoLiveRoute);
                }
                self.counters.on_live_submit(order.strategy_id);
                self.live.submit(order)
            }
        }
    }

    /// Route one cancel. **LAW E-1 applies to a cancel too, and
    /// harder.**
    ///
    /// A live slot's cancel satisfied by the paper matcher would take
    /// a modelled order out of a modelled book and report success,
    /// while the real quote stays resting at the venue — the strategy
    /// then believes it has no exposure and the venue disagrees. A
    /// mis-routed submit invents a fill; a mis-routed cancel invents
    /// the ABSENCE of one, which nothing downstream can detect.
    ///
    /// Same three-way branch as `submit`, on the cancel's own
    /// `strategy_id`/`venue` — which `StampCtx` stamps exactly as it
    /// stamps an order's.
    #[inline]
    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        match self.route.mode(req.strategy_id) {
            ExecMode::Paper => self.paper.cancel(req),
            ExecMode::Off => {
                self.counters.on_refused_off(req.strategy_id);
                Err(DispatchError::SlotDisabled)
            }
            ExecMode::Live => {
                if !self.route.venue_allowed(req.strategy_id, req.venue) {
                    self.counters.on_refused_no_route(req.strategy_id);
                    return Err(DispatchError::NoLiveRoute);
                }
                self.live.cancel(req)
            }
        }
    }

    /// Route one modify — LAW E-1 and LAW E-7 together. Identical
    /// branch to `cancel`, on the REPLACEMENT's routing fields: a
    /// modify carries a whole `Order`, and the slot/venue that own
    /// the resting order are the slot/venue that own its replacement
    /// (a modify may not change either — [`core_types::OrderIdentity`]).
    #[inline]
    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        match self.route.mode(req.order.strategy_id) {
            ExecMode::Paper => self.paper.modify(req),
            ExecMode::Off => {
                self.counters.on_refused_off(req.order.strategy_id);
                Err(DispatchError::SlotDisabled)
            }
            ExecMode::Live => {
                if !self.route.venue_allowed(req.order.strategy_id, req.order.venue) {
                    self.counters.on_refused_no_route(req.order.strategy_id);
                    return Err(DispatchError::NoLiveRoute);
                }
                self.live.modify(req)
            }
        }
    }

    /// Paper fills only — see the module note on §0.1-1. Venue fills
    /// ride the engine's own fill lane 3, which the engine drains
    /// before it ever pumps this.
    #[inline]
    fn try_next_fill(&mut self) -> Option<Fill> {
        self.paper.try_next_fill()
    }

    /// Both arms, summed (plan §0.1-3). Cold: the 5 s tick only.
    fn stats(&self) -> DispatchStats {
        self.paper.stats().merged(self.live.stats())
    }

    /// Every tick reaches the paper matcher, **always and ungated**.
    ///
    /// Even with slot 3 live, the other seven slots' paper orders
    /// still need judging against the book. Gating this on "is any
    /// slot live" would silently freeze the paper matcher for every
    /// member that is still modelling.
    #[inline]
    fn observe_tick(&mut self, tick: &Tick, now_ns: NsTs) {
        self.paper.observe_tick(tick, now_ns);
    }

    /// LAW E-2 — the paper arm's numbers, never the live arm's.
    #[inline]
    fn matcher_counters(&self) -> MatcherCounters {
        self.paper.matcher_counters()
    }

    /// LAW E-2 — likewise.
    #[inline]
    fn open_paper_orders(&self) -> usize {
        self.paper.open_paper_orders()
    }

    /// E1: hand the router's counters and the live route map across the
    /// trait boundary, so the engine loop can mirror them to `/metrics`
    /// without knowing this type. **Cold** — the 5 s tick only.
    /// E4: both arms get the idle moment.
    ///
    /// `|` and NOT `||`: short-circuiting would starve the second arm
    /// every time the first reported work, and the second arm is the
    /// one that owns a venue socket. A hook that runs only when the
    /// other arm is quiet is a hook that stops running exactly when
    /// the engine is busiest.
    ///
    /// This forwarding is why the hook exists at all — `RoutedDispatcher`
    /// is what the `--exec` path wires, so a default `false` here would
    /// leave a live arm's user-event socket unpumped and its budget
    /// state file unwritten, which is the "valid, empty and silent"
    /// failure the user-event module names as the worst one.
    #[inline]
    fn on_idle(&mut self) -> bool {
        let a = self.paper.on_idle();
        let b = self.live.on_idle();
        a | b
    }

    /// Both arms, unconditionally. No short-circuit subtlety here —
    /// this returns nothing, so there is no `|` vs `||` trap the way
    /// there is in `on_idle`; the only requirement is that the LIVE
    /// arm is never skipped, because it is the one that binds.
    #[inline]
    fn on_venue_event(&mut self, event: &core_types::ChannelEvent) {
        self.paper.on_venue_event(event);
        self.live.on_venue_event(event);
    }

    fn exec_counters(&self) -> ExecCounters {
        let c = self.counters;
        let mut modes = [0u8; clob_dispatcher::EXEC_COUNTER_SLOTS];
        for (slot, m) in modes.iter_mut().enumerate() {
            *m = self
                .route
                .mode_at(slot)
                .unwrap_or(ExecMode::Paper)
                .as_u8();
        }
        ExecCounters {
            configured: 1,
            modes,
            live_submits: c.live_submits,
            paper_submits: c.paper_submits,
            refused_off: c.refused_off,
            refused_no_route: c.refused_no_route,
            live_submits_by_slot: c.live_submits_by_slot,
            refused_by_slot: c.refused_by_slot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two crates each name their own slot count (the dependency
    /// runs one way, so neither can import the other's). If they ever
    /// disagree, the per-slot arrays crossing the trait boundary would
    /// silently truncate.
    #[test]
    fn the_slot_counts_of_the_two_crates_agree() {
        assert_eq!(crate::route::EXEC_SLOTS, clob_dispatcher::EXEC_COUNTER_SLOTS);
    }
    use crate::null::NullLiveDispatcher;
    use clob_dispatcher::PaperDispatcher;
    use core_types::{Price, Qty, Side, VenueId, STRATEGY_ID_NONE, STRATEGY_SLOT_BIN15};

    /// A live arm that ACCEPTS, so the tests can tell "routed to live"
    /// apart from "refused". Records what it saw.
    #[derive(Debug, Default)]
    struct SpyLive {
        seen: Vec<u64>,
        cancelled: Vec<u64>,
        modified: Vec<(u64, u64)>,
    }

    impl OrderDispatch for SpyLive {
        fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
            self.seen.push(order.client_oid);
            Ok(())
        }
        fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
            self.cancelled.push(req.client_oid);
            Ok(())
        }
        fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
            self.modified.push((req.prev_client_oid, req.order.client_oid));
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> DispatchStats {
            DispatchStats {
                accepted: self.seen.len() as u64,
                ..DispatchStats::default()
            }
        }
    }

    fn order(slot: u8, venue: VenueId, oid: u64) -> Order {
        let mut o = Order::new(
            1_000,
            venue,
            42,
            Side::Bid,
            0,
            Price::from_raw(500_000),
            Qty::from_raw(1_000_000),
            oid,
        );
        o.strategy_id = slot;
        o
    }

    fn bin15_live_table() -> ExecRoute {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            100_000_000,
            64,
        )
        .unwrap();
        r
    }

    #[test]
    fn a_live_slot_reaches_the_live_arm_and_no_other_slot_does() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
        );
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok());
        for slot in [0u8, 1, 2, 4, 5, 6, 7] {
            assert!(d.submit(&order(slot, VenueId::Hyperliquid, 100 + slot as u64)).is_ok());
        }
        assert_eq!(d.live().seen, vec![1], "only slot 3 went live");
        assert_eq!(d.counters().live_submits, 1);
        assert_eq!(d.counters().paper_submits, 7);
        assert_eq!(d.counters().live_submits_at(3), Some(1));
    }

    /// LAW E-1, the load-bearing test.
    #[test]
    fn a_live_slot_on_a_wrong_venue_is_refused_and_never_falls_back_to_paper() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
        );
        let before = d.paper().open_paper_orders();
        // Slot 3 is live for Hyperliquid only; Binance has no route.
        let r = d.submit(&order(3, VenueId::Binance, 9));
        assert_eq!(r, Err(DispatchError::NoLiveRoute));
        assert!(d.live().seen.is_empty(), "never reached the live arm");
        assert_eq!(
            d.paper().open_paper_orders(),
            before,
            "LAW E-1: the paper matcher must not have taken it either"
        );
        assert_eq!(d.counters().refused_no_route, 1);
        assert_eq!(d.counters().paper_submits, 0);
    }

    #[test]
    fn an_off_slot_is_refused_by_both_arms() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(6, ExecMode::Off, &[], 0, 0).unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default());
        assert_eq!(
            d.submit(&order(6, VenueId::Hyperliquid, 5)),
            Err(DispatchError::SlotDisabled)
        );
        assert!(d.live().seen.is_empty());
        assert_eq!(d.paper().open_paper_orders(), 0);
        assert_eq!(d.counters().refused_off, 1);
        assert_eq!(d.counters().refused_at(6), Some(1));
    }

    #[test]
    fn an_unstamped_order_goes_to_paper_even_under_a_live_table() {
        // STRATEGY_ID_NONE == 0xFF; 0xFF & 7 == 7. Arm slot 7 live to
        // make the aliasing maximally tempting.
        let mut r = ExecRoute::all_paper();
        r.set_slot(7, ExecMode::Live, &[VenueId::Hyperliquid.to_u8()], 0, 0)
            .unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default());
        assert!(d
            .submit(&order(STRATEGY_ID_NONE, VenueId::Hyperliquid, 11))
            .is_ok());
        assert!(d.live().seen.is_empty(), "un-stamped must never go live");
        assert_eq!(d.counters().paper_submits, 1);
    }

    /// The E1 acceptance property: with an all-paper table the router
    /// is indistinguishable from the bare paper dispatcher on every
    /// observable the trait exposes.
    #[test]
    fn an_all_paper_table_is_indistinguishable_from_a_bare_paper_dispatcher() {
        let mut bare = PaperDispatcher::new();
        let mut routed = RoutedDispatcher::new(
            ExecRoute::all_paper(),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
        );

        // A scripted stream over every slot, both venues, both sides.
        let mut oid = 0u64;
        for round in 0..64u64 {
            for slot in [0u8, 1, 2, 3, 4, 5, 6, 7, STRATEGY_ID_NONE] {
                oid += 1;
                let venue = if round % 2 == 0 {
                    VenueId::Hyperliquid
                } else {
                    VenueId::Polymarket
                };
                let o = order(slot, venue, oid);
                assert_eq!(
                    bare.submit(&o),
                    routed.submit(&o),
                    "submit disagreed at oid {oid}"
                );
            }
            let t = Tick::new(
                1_000 + round,
                VenueId::Hyperliquid,
                42,
                round as u32,
                Price::from_raw(499_000),
                Qty::from_raw(1_000_000),
                Price::from_raw(501_000),
                Qty::from_raw(1_000_000),
            );
            bare.observe_tick(&t, 1_000 + round);
            routed.observe_tick(&t, 1_000 + round);

            assert_eq!(
                bare.try_next_fill().map(|f| (f.order_id, f.origin)),
                routed.try_next_fill().map(|f| (f.order_id, f.origin)),
                "fill stream diverged at round {round}"
            );
            assert_eq!(
                bare.matcher_counters(),
                routed.matcher_counters(),
                "matcher counters diverged at round {round}"
            );
            assert_eq!(
                bare.open_paper_orders(),
                routed.open_paper_orders(),
                "open orders diverged at round {round}"
            );
        }

        // Stats: the null live arm contributes nothing at all, so the
        // merged total is the bare total field for field.
        let b = bare.stats();
        let r = routed.stats();
        assert_eq!(b.accepted, r.accepted);
        assert_eq!(b.rejected, r.rejected);
        assert_eq!(b.rejected_queue_full, r.rejected_queue_full);
        assert_eq!(b.rejected_routing, r.rejected_routing);
        assert_eq!(b.fills_seen, r.fills_seen);
        assert_eq!(routed.counters().live_submits, 0);
        assert_eq!(routed.counters().refused_off, 0);
        assert_eq!(routed.counters().refused_no_route, 0);
    }

    // ---------------- E5: routing the lifecycle verbs ----------------

    fn cancel_of(o: &Order) -> CancelReq {
        CancelReq::of(o, 2_000)
    }

    /// **LAW E-1 for a cancel, the load-bearing test.**
    ///
    /// A mis-routed submit invents a fill. A mis-routed cancel
    /// invents the ABSENCE of one: the paper matcher would remove a
    /// modelled order and report success while the real quote stays
    /// resting at the venue, and nothing downstream can detect the
    /// difference.
    #[test]
    fn a_live_slots_cancel_on_a_wrong_venue_never_reaches_the_paper_matcher() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
        );
        // Slot 3 is live for Hyperliquid only. Park a paper order on
        // the matcher so there is a book for a fall-through to reach.
        let decoy = order(0, VenueId::Polymarket, 9);
        assert!(d.submit(&decoy).is_ok());
        assert_eq!(d.paper().open_paper_orders(), 1);

        let mut c = cancel_of(&order(3, VenueId::Binance, 9));
        c.strategy_id = 3;
        assert_eq!(d.cancel(&c), Err(DispatchError::NoLiveRoute));
        assert!(d.live().cancelled.is_empty(), "never reached the live arm");
        assert_eq!(
            d.paper().open_paper_orders(),
            1,
            "LAW E-1: the paper matcher must not have taken the cancel either"
        );
        // The open count alone would NOT prove that: a fall-through
        // would have been refused by the matcher's own lookup and
        // left the count at 1 anyway. These are what prove the
        // matcher was never asked — every path through
        // `PaperMatcher::cancel` moves exactly one of them.
        let mc = d.paper().matcher_counters();
        assert_eq!(mc.cancels, 0, "the matcher performed no cancel");
        assert_eq!(mc.no_such_order, 0, "the matcher was never even asked");
        assert_eq!(mc.identity_mismatch, 0);
        assert_eq!(mc.ambiguous_order, 0);
        assert_eq!(d.counters().refused_no_route, 1);
    }

    #[test]
    fn a_live_slots_cancel_and_modify_reach_the_live_arm_and_no_other_slots_do() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
        );
        let live = order(3, VenueId::Hyperliquid, 1);
        let mut c = cancel_of(&live);
        c.strategy_id = 3;
        assert_eq!(d.cancel(&c), Ok(()));
        assert_eq!(d.live().cancelled, vec![1]);

        let mut repl = order(3, VenueId::Hyperliquid, 2);
        repl.strategy_id = 3;
        assert_eq!(d.modify(&ModifyReq::new(1, repl)), Ok(()));
        assert_eq!(d.live().modified, vec![(1, 2)]);

        // A paper slot's verbs must not touch the live arm.
        let paper = order(0, VenueId::Polymarket, 5);
        assert!(d.submit(&paper).is_ok());
        assert_eq!(d.cancel(&cancel_of(&paper)), Ok(()));
        assert_eq!(d.live().cancelled, vec![1], "still only the live one");
        assert_eq!(d.paper().matcher_counters().cancels, 1);
    }

    #[test]
    fn an_off_slots_cancel_and_modify_are_refused_by_both_arms() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(6, ExecMode::Off, &[], 0, 0).unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default());
        let o = order(6, VenueId::Hyperliquid, 5);
        assert_eq!(d.cancel(&cancel_of(&o)), Err(DispatchError::SlotDisabled));
        assert_eq!(
            d.modify(&ModifyReq::new(4, o)),
            Err(DispatchError::SlotDisabled)
        );
        assert!(d.live().cancelled.is_empty());
        assert!(d.live().modified.is_empty());
        assert_eq!(d.paper().matcher_counters().cancels, 0);
        assert_eq!(d.paper().matcher_counters().no_such_order, 0);
        assert_eq!(d.paper().matcher_counters().identity_mismatch, 0);
        assert_eq!(d.counters().refused_off, 2);
    }

    /// The stub live arm must refuse a lifecycle verb exactly as it
    /// refuses a submit. `Ok` here would be a quote the strategy
    /// stops tracking and the venue never had.
    #[test]
    fn the_null_live_arm_refuses_a_lifecycle_verb_rather_than_swallowing_it() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
        );
        let o = order(3, VenueId::Hyperliquid, 1);
        let mut c = cancel_of(&o);
        c.strategy_id = 3;
        assert_eq!(d.cancel(&c), Err(DispatchError::NoLiveRoute));
        assert_eq!(d.modify(&ModifyReq::new(1, o)), Err(DispatchError::NoLiveRoute));
        assert_eq!(d.live().refused(), 2);
        assert_eq!(d.paper().open_paper_orders(), 0);
    }

    #[test]
    fn law_e2_matcher_numbers_are_the_paper_arms_even_with_a_live_slot() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
        );
        // Three live orders; the matcher must stay at zero intake.
        for i in 0..3 {
            assert!(d.submit(&order(3, VenueId::Hyperliquid, i)).is_ok());
        }
        assert_eq!(d.matcher_counters().intake, 0, "LAW E-2");
        assert_eq!(d.open_paper_orders(), 0, "LAW E-2");
        assert_eq!(d.live().seen.len(), 3);
    }

    #[test]
    fn exec_counters_cross_the_trait_boundary_intact() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
        );
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok());
        assert!(d.submit(&order(0, VenueId::Polymarket, 2)).is_ok());
        assert_eq!(
            d.submit(&order(3, VenueId::Binance, 3)),
            Err(DispatchError::NoLiveRoute)
        );
        let e = d.exec_counters();
        assert_eq!(e.configured, 1);
        assert_eq!(e.live_submits, 1);
        assert_eq!(e.paper_submits, 1);
        assert_eq!(e.refused_no_route, 1);
        assert_eq!(e.refused_off, 0);
        assert_eq!(e.live_submits_by_slot[3], 1);
        assert_eq!(e.refused_by_slot[3], 1);
        assert_eq!(e.modes[3], ExecMode::Live.as_u8());
        assert_eq!(e.modes[0], ExecMode::Paper.as_u8());
    }

    /// A plain paper dispatcher reports NO router — this is what the
    /// cli reads at boot to decide whether `/metrics` grows the
    /// `engine_exec_*` family at all.
    #[test]
    fn a_bare_paper_dispatcher_reports_no_router() {
        let d = PaperDispatcher::new();
        assert_eq!(d.exec_counters().configured, 0);
        assert_eq!(d.exec_counters(), Default::default());
    }

    #[test]
    fn stats_sum_both_arms() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
        );
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok()); // live
        assert!(d.submit(&order(0, VenueId::Polymarket, 2)).is_ok()); // paper
        let s = d.stats();
        assert_eq!(s.accepted, d.paper().stats().accepted + 1);
    }

    #[test]
    fn the_null_live_arm_makes_a_stray_live_order_a_refusal_not_a_paper_fill() {
        // Belt and braces: even if config validation were bypassed and
        // a Live slot pointed at the stub, nothing gets modelled.
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
        );
        assert_eq!(
            d.submit(&order(3, VenueId::Hyperliquid, 1)),
            Err(DispatchError::NoLiveRoute)
        );
        assert_eq!(d.paper().open_paper_orders(), 0);
        assert_eq!(d.live().refused(), 1);
    }
}
