// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! HYPARB L4 — two live arms behind one router.
//!
//! [`RoutedDispatcher`](crate::RoutedDispatcher) holds ONE live arm.
//! Two slots can be live at once on different money: slot 3 (BIN15, the
//! operator's Hyperliquid account) and slot 0 (HYPARB, its own wallet on
//! HyperEVM and Hyperliquid, ruling O-HL3). [`SlotSplit`] is that one arm:
//! it sends every verb of slot `slot` to `b` and every other slot's to
//! `a`, by the `strategy_id` byte the router already routed on.
//!
//! ## What is per slot, and what is shared
//!
//! * **Per slot:** submit, cancel and modify; the halt signal
//!   ([`OrderDispatch::halt_signal_for`]), so slot 0's reconciler,
//!   stream and P&L bound never halt slot 3, nor the reverse. The
//!   router seeds each slot from its own arm's signal.
//! * **Shared:** fills and retirements (a's, then b's), idle work, venue events and
//!   ticks (each arm ignores what is not its own), and `cancel_all`.
//!   The router's cancel-all is venue-wide by design: a halt takes
//!   every live order off, the healthy slot re-quotes. Both arms are
//!   asked, and the state is the least settled of the two.
//! * **Slot-agnostic readers** (`halt_signal`, `arm_counters`) answer
//!   `a`'s — the surface that existed before L4.
//!
//! Zero-sized glue: one byte compare per verb, no allocation, no `dyn`.

use clob_dispatcher::{
    CancelAllState, DispatchError, DispatchStats, HaltSignal, LiveArmCounters, OrderDispatch,
};
use core_types::{CancelReq, ChannelEvent, Fill, ModifyReq, NsTs, Order, SymbolId, Tick};

/// Slot `slot`'s verbs go to `b`; every other slot's to `a`.
pub struct SlotSplit<A: OrderDispatch, B: OrderDispatch> {
    slot: u8,
    a: A,
    b: B,
}

impl<A: OrderDispatch, B: OrderDispatch> SlotSplit<A, B> {
    /// `b` trades slot `slot`; `a` every other live slot.
    #[inline]
    #[must_use]
    pub const fn new(slot: u8, a: A, b: B) -> Self {
        Self { slot, a, b }
    }

    /// The slot `b` trades.
    #[inline]
    #[must_use]
    pub const fn slot(&self) -> u8 {
        self.slot
    }

    /// The arm every other slot trades through.
    #[inline]
    #[must_use]
    pub const fn a(&self) -> &A {
        &self.a
    }

    /// The arm slot [`Self::slot`] trades through.
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
}

/// The least settled of two confirmations: a sweep still draining
/// waits; an arm that gave up is asked again; clear only when both are.
#[inline]
const fn least_settled(a: CancelAllState, b: CancelAllState) -> CancelAllState {
    match (a, b) {
        (CancelAllState::Working, _) | (_, CancelAllState::Working) => CancelAllState::Working,
        (CancelAllState::Stranded, _) | (_, CancelAllState::Stranded) => CancelAllState::Stranded,
        _ => CancelAllState::Clear,
    }
}

impl<A: OrderDispatch, B: OrderDispatch> OrderDispatch for SlotSplit<A, B> {
    #[inline]
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        if order.strategy_id == self.slot {
            self.b.submit(order)
        } else {
            self.a.submit(order)
        }
    }

    #[inline]
    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        if req.strategy_id == self.slot {
            self.b.cancel(req)
        } else {
            self.a.cancel(req)
        }
    }

    #[inline]
    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        if req.order().strategy_id == self.slot {
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
    fn try_next_retired(&mut self) -> Option<(u64, u8)> {
        match self.a.try_next_retired() {
            Some(r) => Some(r),
            None => self.b.try_next_retired(),
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
    fn on_fill_booked(&mut self, fill: &Fill) {
        self.a.on_fill_booked(fill);
        self.b.on_fill_booked(fill);
    }

    #[inline]
    fn halt_signal(&self) -> HaltSignal {
        self.a.halt_signal()
    }

    #[inline]
    fn halt_signal_for(&self, slot: u8) -> HaltSignal {
        if slot == self.slot {
            self.b.halt_signal()
        } else {
            self.a.halt_signal_for(slot)
        }
    }

    /// Both arms are asked, whatever the first answered.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty, Side, VenueId};

    /// An arm that records what reached it and answers what the test
    /// sets.
    #[derive(Default)]
    struct Arm {
        seen: Vec<u64>,
        cancelled: Vec<u64>,
        fills: Vec<Fill>,
        retired: Vec<(u64, u8)>,
        sig: HaltSignal,
        cancel_all_calls: u32,
        cancel_all_err: bool,
        state: u8,
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
            self.seen.push(r.order().client_oid);
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            self.fills.pop()
        }
        fn try_next_retired(&mut self) -> Option<(u64, u8)> {
            self.retired.pop()
        }
        fn stats(&self) -> DispatchStats {
            DispatchStats::default()
        }
        fn halt_signal(&self) -> HaltSignal {
            self.sig
        }
        fn cancel_all(&mut self) -> Result<(), DispatchError> {
            self.cancel_all_calls += 1;
            if self.cancel_all_err {
                Err(DispatchError::Disconnected)
            } else {
                Ok(())
            }
        }
        fn cancel_all_state(&self) -> CancelAllState {
            match self.state {
                1 => CancelAllState::Working,
                2 => CancelAllState::Stranded,
                _ => CancelAllState::Clear,
            }
        }
    }

    fn order(slot: u8, oid: u64) -> Order {
        let mut o = Order::new(
            1,
            VenueId::Hyperliquid,
            1,
            Side::Bid,
            1,
            Price::from_raw(1),
            Qty::from_raw(1),
            oid,
        );
        o.strategy_id = slot;
        o
    }

    fn split() -> SlotSplit<Arm, Arm> {
        SlotSplit::new(0, Arm::default(), Arm::default())
    }

    #[test]
    fn each_slots_verbs_reach_its_own_arm_only() {
        let mut s = split();
        s.submit(&order(0, 10)).unwrap();
        s.submit(&order(3, 30)).unwrap();
        s.cancel(&CancelReq::of(&order(0, 11), 1)).unwrap();
        s.cancel(&CancelReq::of(&order(3, 31), 1)).unwrap();
        s.modify(&ModifyReq::new(30, order(3, 32))).unwrap();
        assert_eq!(s.b().seen, [10]);
        assert_eq!(s.b().cancelled, [11]);
        assert_eq!(s.a().seen, [30, 32]);
        assert_eq!(s.a().cancelled, [31]);
        assert_eq!(s.slot(), 0);
    }

    #[test]
    fn each_slot_reads_its_own_arms_signal() {
        let mut s = split();
        s.a_mut().sig = HaltSignal::new(0, 0, 7, 0, false, true, 0);
        s.b_mut().sig = HaltSignal::new(0, 0, 1, 0, false, false, 0).with_pnl(true, -9);
        assert_eq!(s.halt_signal_for(3).reject_streak, 7);
        assert_eq!(s.halt_signal_for(0).reject_streak, 1);
        assert_eq!(s.halt_signal_for(0).pnl_delta_usd_1e6, -9);
        assert_eq!(s.halt_signal().reject_streak, 7, "slot-agnostic = a");
    }

    #[test]
    fn fills_come_from_both_arms_a_first() {
        let mut s = split();
        let f = |oid| Fill::new(1, 1, Side::Bid, Price::from_raw(1), Qty::from_raw(1), oid);
        s.a_mut().fills.push(f(1));
        s.b_mut().fills.push(f(2));
        assert_eq!(s.try_next_fill().map(|x| x.order_id), Some(1));
        assert_eq!(s.try_next_fill().map(|x| x.order_id), Some(2));
        assert!(s.try_next_fill().is_none());
    }

    #[test]
    fn retirements_come_from_both_arms() {
        let mut s = split();
        s.a_mut().retired.push((5, 3));
        s.b_mut().retired.push((6, 0));
        assert_eq!(s.try_next_retired(), Some((5, 3)));
        assert_eq!(s.try_next_retired(), Some((6, 0)));
        assert_eq!(s.try_next_retired(), None);
    }

    #[test]
    fn cancel_all_asks_both_and_confirms_only_when_both_are_clear() {
        let mut s = split();
        s.a_mut().cancel_all_err = true;
        assert!(s.cancel_all().is_err());
        assert_eq!((s.a().cancel_all_calls, s.b().cancel_all_calls), (1, 1));
        assert_eq!(s.cancel_all_state(), CancelAllState::Clear);
        s.b_mut().state = 2;
        assert_eq!(s.cancel_all_state(), CancelAllState::Stranded);
        s.a_mut().state = 1;
        assert_eq!(s.cancel_all_state(), CancelAllState::Working, "wait first");
    }
}
