// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BX3 (O-BX17) — two live arms behind one router, split by VENUE.
//!
//! [`SlotSplit`](crate::SlotSplit) sends a whole slot to one arm.
//! [`VenueSplit`] sends each ORDER to the arm that trades its route
//! venue: `b` for `venue` (Binance), `a` for every other. The engine's
//! shape is `SlotSplit<VenueSplit<Hl, Bn>, HyparbLive>`; BX13's battery
//! runs `VenueSplit<Null, Bn>`.
//!
//! ## The route venue
//!
//! An order's venue byte, after the boot-fixed alias table
//! ([`RouteAliases`]): the M1 legacy anchor `binance:btcusdt` is the
//! flat id 7, whose byte reads 0 (Polymarket), and routes to Binance by
//! its alias. The router's `venue_allowed` check reads the same table,
//! so the arm an order reaches is always the venue its slot was allowed.
//!
//! ## What is per order, per slot, and shared
//!
//! * **Per order:** submit, cancel and modify, by route venue.
//! * **Per slot:** the halt signal. Each live slot's coverage is fixed at
//!   construction from its venue mask — `a` when the mask names any
//!   venue but `venue`, `b` when it names `venue`. A slot one arm serves
//!   reads that arm's signal; a slot both serve reads
//!   [`HaltSignal::merged`] (the worse of each observation). So a
//!   Binance lock or margin verdict reaches only the slots whose mask
//!   names Binance (O-BX8), never a Hyperliquid-only slot.
//! * **Shared:** fills and retirements (`a`'s, then `b`'s), idle work,
//!   ticks, pool events, venue events, booked fills and option
//!   summaries (each arm ignores what is not its own), `cancel_all`
//!   (both asked, O-BX8) and its confirmation (the least settled of the
//!   two: Working > Stranded > Clear), and shutdown.
//! * **Slot-agnostic readers** (`halt_signal`, `arm_counters`) answer
//!   `a`'s, so the flat `exec.arm_*` block stays Hyperliquid's.
//!
//! Zero-sized glue beyond 8 + 32 bytes of tables: one alias walk and one
//! byte compare per verb, no allocation, no `dyn`.

use crate::route::{ExecRoute, EXEC_SLOTS, EXEC_VENUES};
use crate::split::least_settled;
use clob_dispatcher::{
    CancelAllState, DispatchError, DispatchStats, HaltSignal, LiveArmCounters, OrderDispatch,
    Retired, RouteAliases,
};
use core_types::{
    CancelReq, ChannelEvent, Fill, ModifyReq, NsTs, OptSummary, Order, SymbolId, Tick,
};

/// A slot's coverage bit: arm `a` trades it.
pub const SERVES_A: u8 = 1;
/// A slot's coverage bit: arm `b` trades it.
pub const SERVES_B: u8 = 2;

/// Orders whose route venue is `venue` go to `b`; every other to `a`.
pub struct VenueSplit<A: OrderDispatch, B: OrderDispatch> {
    venue: u8,
    serves: [u8; EXEC_SLOTS],
    aliases: RouteAliases,
    a: A,
    b: B,
}

impl<A: OrderDispatch, B: OrderDispatch> VenueSplit<A, B> {
    /// `b` trades route venue `venue`, `a` every other. Each slot's
    /// coverage is read from `route`'s venue masks. The alias table starts
    /// empty and is the ROUTER's: `RoutedDispatcher::set_route_aliases`
    /// hands it down ([`OrderDispatch::set_route_aliases`]), so the two
    /// cannot disagree. Boot-only.
    #[must_use]
    pub fn new(venue: u8, a: A, b: B, route: &ExecRoute) -> Self {
        let bit_b: u16 = if venue < EXEC_VENUES {
            1u16 << venue
        } else {
            0
        };
        let mut serves = [0u8; EXEC_SLOTS];
        let mut s = 0usize;
        while s < EXEC_SLOTS {
            let mask = route.venue_mask_at(s).unwrap_or(0);
            let a_hit = (mask & !bit_b) != 0;
            let b_hit = (mask & bit_b) != 0;
            serves[s] = (a_hit as u8 * SERVES_A) | (b_hit as u8 * SERVES_B);
            s += 1;
        }
        Self {
            venue,
            serves,
            aliases: RouteAliases::NONE,
            a,
            b,
        }
    }

    /// The route venue `b` trades.
    #[inline]
    #[must_use]
    pub const fn venue(&self) -> u8 {
        self.venue
    }

    /// Which arms trade `slot` ([`SERVES_A`] | [`SERVES_B`]); 0 for a
    /// slot neither does or an out-of-range one. Cold; boot tell, tests.
    #[inline]
    #[must_use]
    pub fn serves(&self, slot: usize) -> u8 {
        if slot < EXEC_SLOTS {
            self.serves[slot]
        } else {
            0
        }
    }

    /// The alias table this split routes by — the router's.
    #[inline]
    #[must_use]
    pub const fn aliases(&self) -> &RouteAliases {
        &self.aliases
    }

    /// The arm every other venue trades through.
    #[inline]
    #[must_use]
    pub const fn a(&self) -> &A {
        &self.a
    }

    /// The arm route venue [`Self::venue`] trades through.
    #[inline]
    #[must_use]
    pub const fn b(&self) -> &B {
        &self.b
    }

    #[cfg(test)]
    pub(crate) fn a_mut(&mut self) -> &mut A {
        &mut self.a
    }

    #[cfg(test)]
    pub(crate) fn b_mut(&mut self) -> &mut B {
        &mut self.b
    }

    /// Does an order on `sym` stamped `venue` belong to `b`?
    #[inline(always)]
    fn to_b(&self, sym: SymbolId, venue: u8) -> bool {
        self.aliases.route_venue(sym, venue) == self.venue
    }
}

/// One slot's day spend from two arms that both trade it: the same day
/// sums, a newer day wins, one answer stands alone.
#[inline]
fn merged_day(a: Option<(u64, i64)>, b: Option<(u64, i64)>) -> Option<(u64, i64)> {
    match (a, b) {
        (Some((da, xa)), Some((db, xb))) => {
            if da == db {
                Some((da, xa.saturating_add(xb)))
            } else if da > db {
                Some((da, xa))
            } else {
                Some((db, xb))
            }
        }
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

impl<A: OrderDispatch, B: OrderDispatch> OrderDispatch for VenueSplit<A, B> {
    #[inline]
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        if self.to_b(order.sym, order.venue) {
            self.b.submit(order)
        } else {
            self.a.submit(order)
        }
    }

    #[inline]
    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        if self.to_b(req.sym, req.venue) {
            self.b.cancel(req)
        } else {
            self.a.cancel(req)
        }
    }

    /// Routed on the REPLACEMENT's fields, which a modify may not change
    /// ([`core_types::OrderIdentity`]), so it reaches the arm holding the
    /// original.
    #[inline]
    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        let o = req.order();
        if self.to_b(o.sym, o.venue) {
            self.b.modify(req)
        } else {
            self.a.modify(req)
        }
    }

    #[inline]
    fn try_next_fill(&mut self) -> Option<Fill> {
        match self.a.try_next_fill() {
            Some(f) => Some(f),
            None => self.b.try_next_fill(),
        }
    }

    #[inline]
    fn try_next_retired(&mut self) -> Option<Retired> {
        match self.a.try_next_retired() {
            Some(r) => Some(r),
            None => self.b.try_next_retired(),
        }
    }

    /// BX6 (obligation 2): the arm the order routes to answers — by the
    /// route venue, exactly as `cancel` and `modify` route.
    #[inline]
    fn verbs_confirm_later(&self, sym: SymbolId, venue: u8, strategy_id: u8) -> bool {
        if self.to_b(sym, venue) {
            self.b.verbs_confirm_later(sym, venue, strategy_id)
        } else {
            self.a.verbs_confirm_later(sym, venue, strategy_id)
        }
    }

    fn try_next_renamed(&mut self) -> Option<clob_dispatcher::Renamed> {
        match self.a.try_next_renamed() {
            Some(r) => Some(r),
            None => self.b.try_next_renamed(),
        }
    }

    fn stats(&self) -> DispatchStats {
        self.a.stats().merged(self.b.stats())
    }

    #[inline]
    fn observe_tick(&mut self, tick: &Tick, now_ns: NsTs) {
        self.a.observe_tick(tick, now_ns);
        self.b.observe_tick(tick, now_ns);
    }

    #[inline]
    fn observe_amm(&mut self, sym: SymbolId, payload: &[u8; 40], now_ns: NsTs) {
        self.a.observe_amm(sym, payload, now_ns);
        self.b.observe_amm(sym, payload, now_ns);
    }

    /// XMM XH2 (met at the 2026-09-26 merge, as main's `SlotSplit` has
    /// it): both arms see every print, like every tick.
    #[inline]
    fn observe_trade(&mut self, print: &core_types::TradePrint, now_ns: NsTs) {
        self.a.observe_trade(print, now_ns);
        self.b.observe_trade(print, now_ns);
    }

    /// XMM XH2: arm `a`'s order events first, then `b`'s — the fill
    /// order; forwarded so an arm's events cannot vanish into the
    /// defaulted `false` behind this wrapper.
    #[inline]
    fn try_next_order_event(&mut self, out: &mut core_types::OrderEvent) -> bool {
        self.a.try_next_order_event(out) || self.b.try_next_order_event(out)
    }

    /// XMM XH2: both arms learn the queue law's instruments.
    #[inline]
    fn track_queue_sym(&mut self, sym: SymbolId) {
        self.a.track_queue_sym(sym);
        self.b.track_queue_sym(sym);
    }

    /// Both arms, unconditionally — neither may starve the other.
    #[inline]
    fn on_idle(&mut self) -> bool {
        let a = self.a.on_idle();
        let b = self.b.on_idle();
        a | b
    }

    #[inline]
    fn on_venue_event(&mut self, event: &ChannelEvent) {
        self.a.on_venue_event(event);
        self.b.on_venue_event(event);
    }

    #[inline]
    fn on_opt_summary(&mut self, summary: &OptSummary) {
        self.a.on_opt_summary(summary);
        self.b.on_opt_summary(summary);
    }

    /// The router's table, kept and handed on to both arms.
    fn set_route_aliases(&mut self, aliases: RouteAliases) {
        self.aliases = aliases;
        self.a.set_route_aliases(aliases);
        self.b.set_route_aliases(aliases);
    }

    #[inline]
    fn on_fill_booked(&mut self, fill: &Fill) {
        self.a.on_fill_booked(fill);
        self.b.on_fill_booked(fill);
    }

    #[inline]
    fn halt_signal(&self) -> HaltSignal {
        self.a.halt_signal()
    }

    /// Each slot reads the arm(s) that trade it (module docs).
    #[inline]
    fn halt_signal_for(&self, slot: u8) -> HaltSignal {
        match self.serves(slot as usize) {
            SERVES_A => self.a.halt_signal_for(slot),
            SERVES_B => self.b.halt_signal_for(slot),
            3 => self
                .a
                .halt_signal_for(slot)
                .merged(self.b.halt_signal_for(slot)),
            // No arm trades it: nothing observed, and never seeded.
            _ => HaltSignal::default(),
        }
    }

    /// Both arms are asked, whatever the first answered (O-BX8).
    fn cancel_all(&mut self) -> Result<(), DispatchError> {
        let a = self.a.cancel_all();
        let b = self.b.cancel_all();
        a.and(b)
    }

    fn cancel_all_state(&self) -> CancelAllState {
        least_settled(self.a.cancel_all_state(), self.b.cancel_all_state())
    }

    fn arm_counters(&self) -> LiveArmCounters {
        self.a.arm_counters()
    }

    /// BX6: route venue [`Self::venue`]'s arm answers for it; every other
    /// venue is asked of `a`.
    fn venue_arm_counters(&self, venue: u8) -> Option<LiveArmCounters> {
        if venue == self.venue {
            self.b.venue_arm_counters(venue)
        } else {
            self.a.venue_arm_counters(venue)
        }
    }

    /// Both arms take their own orders off the venue on the way out.
    fn on_shutdown(&mut self) {
        self.a.on_shutdown();
        self.b.on_shutdown();
    }

    /// Each slot's day, from the arm(s) that trade it.
    #[inline]
    fn venue_day_bought(&self, slot: usize) -> Option<(u64, i64)> {
        match self.serves(slot) {
            SERVES_A => self.a.venue_day_bought(slot),
            SERVES_B => self.b.venue_day_bought(slot),
            3 => merged_day(self.a.venue_day_bought(slot), self.b.venue_day_bought(slot)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::ExecMode;
    use crate::route::{HaltLimits, SlotCaps};
    use core_types::{make_symbol_id, Price, Qty, Side, VenueId};

    const BN: u8 = VenueId::Binance as u8;
    const HL: u8 = VenueId::Hyperliquid as u8;
    /// The M1 legacy anchor, whose venue byte reads Polymarket.
    const ANCHOR: SymbolId = 7;

    /// An arm that records what reached it and answers what the test sets.
    #[derive(Default)]
    struct Arm {
        seen: Vec<u64>,
        cancelled: Vec<u64>,
        modified: Vec<u64>,
        opt: u32,
        idles: u32,
        sig: HaltSignal,
        day: Option<(u64, i64)>,
        cancel_all_calls: u32,
        state: u8,
        busy: bool,
        prints: u32,
        queue_syms: u32,
        /// Order events to hand out, by client id (popped from the back).
        events: Vec<u64>,
    }

    impl OrderDispatch for Arm {
        fn submit(&mut self, o: &Order) -> Result<(), DispatchError> {
            self.seen.push(o.client_oid);
            Ok(())
        }
        fn cancel(&mut self, r: &CancelReq) -> Result<(), DispatchError> {
            self.cancelled.push(r.client_oid);
            Ok(())
        }
        fn modify(&mut self, r: &ModifyReq) -> Result<(), DispatchError> {
            self.modified.push(r.order().client_oid);
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> DispatchStats {
            DispatchStats::default()
        }
        fn on_idle(&mut self) -> bool {
            self.idles += 1;
            self.busy
        }
        fn on_opt_summary(&mut self, _s: &OptSummary) {
            self.opt += 1;
        }
        fn halt_signal(&self) -> HaltSignal {
            self.sig
        }
        fn venue_day_bought(&self, _slot: usize) -> Option<(u64, i64)> {
            self.day
        }
        fn cancel_all(&mut self) -> Result<(), DispatchError> {
            self.cancel_all_calls += 1;
            Ok(())
        }
        fn cancel_all_state(&self) -> CancelAllState {
            match self.state {
                1 => CancelAllState::Working,
                2 => CancelAllState::Stranded,
                _ => CancelAllState::Clear,
            }
        }
        fn observe_trade(&mut self, _p: &core_types::TradePrint, _now_ns: NsTs) {
            self.prints += 1;
        }
        fn track_queue_sym(&mut self, _sym: SymbolId) {
            self.queue_syms += 1;
        }
        fn try_next_order_event(&mut self, out: &mut core_types::OrderEvent) -> bool {
            let Some(oid) = self.events.pop() else {
                return false;
            };
            out.client_oid = oid;
            true
        }
    }

    /// Slot 3 on Hyperliquid, slot 2 on Binance, slot 5 on both.
    fn route() -> ExecRoute {
        let mut r = ExecRoute::all_paper();
        let caps = SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64);
        let lim = HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000);
        r.set_slot(3, ExecMode::Live, &[HL], caps, lim).unwrap();
        r.set_slot(2, ExecMode::Live, &[BN], caps, lim).unwrap();
        r.set_slot(5, ExecMode::Live, &[HL, BN], caps, lim).unwrap();
        r
    }

    fn split() -> VenueSplit<Arm, Arm> {
        let mut s = VenueSplit::new(BN, Arm::default(), Arm::default(), &route());
        s.set_route_aliases(RouteAliases::NONE.with(ANCHOR, BN).unwrap());
        s
    }

    fn order(slot: u8, venue: u8, sym: SymbolId, oid: u64) -> Order {
        let mut o = Order::new(
            1,
            VenueId::Hyperliquid,
            sym,
            Side::Bid,
            1,
            Price::from_raw(1),
            Qty::from_raw(1),
            oid,
        );
        o.venue = venue;
        o.strategy_id = slot;
        o
    }

    #[test]
    fn each_order_reaches_the_arm_of_its_route_venue() {
        let mut s = split();
        let bn_sym = make_symbol_id(VenueId::Binance, 3073);
        s.submit(&order(2, BN, bn_sym, 1)).unwrap();
        s.submit(&order(3, HL, 4096, 2)).unwrap();
        // The anchor stamps Polymarket's byte and routes to Binance.
        s.submit(&order(2, 0, ANCHOR, 3)).unwrap();
        // Its neighbour keeps its own byte.
        s.submit(&order(2, 0, 8, 4)).unwrap();
        s.cancel(&CancelReq::of(&order(2, 0, ANCHOR, 5), 1))
            .unwrap();
        s.cancel(&CancelReq::of(&order(3, HL, 4096, 6), 1)).unwrap();
        s.modify(&ModifyReq::new(1, order(2, BN, bn_sym, 7)))
            .unwrap();
        s.modify(&ModifyReq::new(2, order(3, HL, 4096, 8))).unwrap();
        assert_eq!(s.b().seen, [1, 3]);
        assert_eq!(s.a().seen, [2, 4]);
        assert_eq!(s.b().cancelled, [5]);
        assert_eq!(s.a().cancelled, [6]);
        assert_eq!(s.b().modified, [7]);
        assert_eq!(s.a().modified, [8]);
    }

    #[test]
    fn coverage_follows_each_slots_venue_mask() {
        let s = split();
        assert_eq!(s.serves(3), SERVES_A);
        assert_eq!(s.serves(2), SERVES_B);
        assert_eq!(s.serves(5), SERVES_A | SERVES_B);
        assert_eq!(s.serves(0), 0, "a paper slot with no venue");
        assert_eq!(s.serves(99), 0);
        assert_eq!(s.venue(), BN);
        assert_eq!(s.aliases().route_venue(ANCHOR, 0), BN);
    }

    /// O-BX8 by construction. Break-and-watch: answering the merged
    /// signal for every slot halts slot 3 on a Binance verdict.
    #[test]
    fn a_binance_verdict_reaches_only_the_slots_that_trade_binance() {
        let mut s = split();
        s.a_mut().sig = HaltSignal::new(1, 0, 0, 0, false, true, 1);
        s.b_mut().sig = HaltSignal::new(1, 0, 0, 0, false, true, 1).with_venue(false, true);
        assert_eq!(s.halt_signal_for(3).margin_risk, 0, "Hyperliquid-only slot");
        assert_eq!(s.halt_signal_for(2).margin_risk, 1);
        assert_eq!(s.halt_signal_for(5).margin_risk, 1, "a slot on both venues");
        assert_eq!(s.halt_signal_for(0), HaltSignal::default());
        assert_eq!(s.halt_signal().margin_risk, 0, "slot-agnostic = a");
    }

    /// Each merge class, on a slot both arms trade.
    #[test]
    fn a_slot_on_both_venues_reads_the_worse_of_each_observation() {
        let mut s = split();
        s.a_mut().sig = HaltSignal::new(10, 7, 4, 0, false, true, 3).with_pnl(true, -5);
        s.b_mut().sig = HaltSignal::new(2, 9, 1, 2, true, false, 8)
            .with_pnl(true, 2)
            .with_venue(true, false);
        let m = s.halt_signal_for(5);
        assert_eq!(
            (m.ws_gap_ns, m.recon_drift_usd_1e6, m.recon_age_ns),
            (10, 9, 8)
        );
        assert_eq!((m.reject_streak, m.asset_refusal_streak), (4, 2));
        assert_eq!((m.budget_floor_breached, m.venue_lock), (1, 1));
        assert_eq!(
            m.reconciled, 0,
            "seeded only when both arms have reconciled"
        );
        assert_eq!((m.pnl_judged, m.pnl_delta_usd_1e6), (1, -3));
    }

    #[test]
    fn cancel_all_asks_both_and_confirms_only_when_both_are_clear() {
        let mut s = split();
        assert!(s.cancel_all().is_ok());
        assert_eq!((s.a().cancel_all_calls, s.b().cancel_all_calls), (1, 1));
        assert_eq!(s.cancel_all_state(), CancelAllState::Clear);
        s.b_mut().state = 2;
        assert_eq!(s.cancel_all_state(), CancelAllState::Stranded);
        s.a_mut().state = 1;
        assert_eq!(s.cancel_all_state(), CancelAllState::Working, "wait first");
    }

    /// `|`, not `||`: a busy `a` must not starve `b`'s sockets.
    #[test]
    fn idle_work_reaches_both_arms_whatever_the_first_reports() {
        let mut s = split();
        s.a_mut().busy = true;
        assert!(s.on_idle());
        assert_eq!((s.a().idles, s.b().idles), (1, 1));
        let summary = OptSummary::new(1, VenueId::Binance, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0);
        s.on_opt_summary(&summary);
        assert_eq!((s.a().opt, s.b().opt), (1, 1));
    }

    #[test]
    fn each_slot_reads_its_own_day_and_a_shared_slot_merges_them() {
        let mut s = split();
        s.a_mut().day = Some((20_000, 7));
        s.b_mut().day = Some((20_000, 9));
        assert_eq!(s.venue_day_bought(3), Some((20_000, 7)));
        assert_eq!(s.venue_day_bought(2), Some((20_000, 9)));
        assert_eq!(s.venue_day_bought(5), Some((20_000, 16)), "same day sums");
        s.b_mut().day = Some((20_001, 1));
        assert_eq!(s.venue_day_bought(5), Some((20_001, 1)), "a newer day wins");
        s.a_mut().day = None;
        assert_eq!(s.venue_day_bought(5), Some((20_001, 1)));
        assert_eq!(s.venue_day_bought(0), None);
    }

    /// XMM XH2 through this wrapper (the forwards the merge added): both
    /// arms see every print and every queue instrument, and each arm's
    /// order events come out — `a`'s first — instead of vanishing into
    /// the trait's defaulted `false`.
    #[test]
    fn prints_queue_syms_and_order_events_reach_both_arms() {
        let mut s = split();
        let print = core_types::TradePrint::new(1, VenueId::Hyperliquid, 4096, 9, 1, 1_000_000, 1_000_000, 0);
        s.observe_trade(&print, 2);
        s.track_queue_sym(4096);
        assert_eq!((s.a().prints, s.b().prints), (1, 1));
        assert_eq!((s.a().queue_syms, s.b().queue_syms), (1, 1));
        s.a_mut().events.push(11);
        s.b_mut().events.push(22);
        let mut ev = core_types::OrderEvent::ZERO;
        assert!(s.try_next_order_event(&mut ev));
        assert_eq!(ev.client_oid, 11, "arm a first");
        assert!(s.try_next_order_event(&mut ev));
        assert_eq!(ev.client_oid, 22, "then arm b");
        assert!(!s.try_next_order_event(&mut ev));
    }
}
