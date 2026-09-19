// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! What the router did, per boot. Mirrored to `/metrics` on the 5 s
//! tick and into `/state`; never read on the hot path.
//!
//! Cache-line aligned and `#[repr(C)]` because the dispatcher owns one
//! by value and the hot path bumps it on every routed submit — a
//! counter sharing a line with the route table's hot bytes would make
//! every submit dirty the line the next submit wants to read.

use crate::route::EXEC_SLOTS;

/// **E6 — which of the four clamps refused an order.**
///
/// A plain enum rather than a bitset: exactly one clamp refuses any
/// given request, because the gate returns on the first that fires and
/// the order never reaches the venue to breach a second.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RiskRefusal {
    /// `max_order_usd` — this one order's notional.
    MaxOrder,
    /// `cap_instance_usd` — the slot's projected net exposure.
    CapInstance,
    /// `cap_day_usd` — the slot's projected day buy turnover.
    CapDay,
    /// `max_open_orders` — the slot's resting count.
    OpenOrders,
    /// The ledger has never been reconciled against the venue, so
    /// none of the three ledger-fed numbers can be trusted.
    Unseeded,
}

/// Router counters. `Copy` POD; every field saturates rather than
/// wraps, because an operator reading `live_submits` after an incident
/// needs a number that is monotone, not one that has been around the
/// horn.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteCounters {
    /// Orders sent to the live arm (accepted into it, not necessarily
    /// acked by the venue — the arm's own stats carry acks).
    pub live_submits: u64,
    /// Orders sent to the paper matcher.
    pub paper_submits: u64,
    /// Refused because the slot is [`crate::ExecMode::Off`].
    ///
    /// E5: counts refusals of ANY verb — submit, cancel or modify.
    /// The counter is named for the CONDITION (the slot is off),
    /// which is the same condition whichever verb ran into it, and
    /// which is the thing an operator acts on.
    pub refused_off: u64,
    /// Refused because the slot is live but the request named a venue
    /// the slot has no live route to. **LAW E-1: refused, never
    /// downgraded to paper.**
    ///
    /// E5: any verb, as with `refused_off`.
    pub refused_no_route: u64,
    /// **E6: refused by the RISK GATE** — the SUM of the four
    /// breakdown fields below.
    ///
    /// **Its meaning changed in E6 commit 2 while its name did not.**
    /// In commit 1 this counter had one contributor, `max_order_usd`,
    /// and a non-zero value meant "the member and the operator
    /// disagreed" — an alarm. Commit 2 added three ledger-fed clamps
    /// whose refusals mean "the operator's ceiling was reached", which
    /// is the clamp working, not an alarm.
    ///
    /// The number is kept as the sum so a dashboard built against
    /// commit 1 keeps parsing. **An ALERT wired to it should be
    /// re-pointed at [`Self::refused_max_order`]**, which is the one
    /// field that still carries the commit-1 meaning: bin15 sizes
    /// against its own `cap_instance`/`cap_day` ledger, and that field
    /// is computed from the request in front of it, so a non-zero
    /// value there means a member asked for something its own caps
    /// should already have stopped.
    pub refused_risk: u64,
    /// **E6: which clamp fired**, as a breakdown of `refused_risk`.
    ///
    /// The aggregate above is what says "the member and the operator
    /// disagreed"; these say WHICH disagreement, and the four are very
    /// different operator actions. `refused_risk` stays the sum, so a
    /// dashboard built against E6 commit 1 keeps reading what it read.
    ///
    /// Exceeded the slot's `max_order_usd` — one order, too big.
    pub refused_max_order: u64,
    /// Would have taken the slot's NET EXPOSURE past
    /// `cap_instance_usd`. The position, not the order.
    pub refused_cap_instance: u64,
    /// Would have taken the slot's day BUY TURNOVER past
    /// `cap_day_usd`. Resets at 00:00Z with the ledger's epoch.
    pub refused_cap_day: u64,
    /// The slot already has `max_open_orders` working. Counted only
    /// for a PLACE: LAW E-7 says a requote replaces in place, so a
    /// modify cannot be the order that takes the count over.
    pub refused_open_orders: u64,
    /// **The ledger has not been reconciled against the venue.**
    ///
    /// Its own field rather than a share of `refused_cap_instance`,
    /// and the distinction is the whole point. Nothing in production
    /// calls `RoutedDispatcher::mark_ledger_seeded` on this commit —
    /// the reconciler wiring is E6 commit 3 — so on a live boot
    /// **every** refusal is this one. Folded into
    /// `refused_cap_instance`, it would send an operator to look at a
    /// `cap_instance_usd` number that has nothing to do with why
    /// their orders are being refused, which is the same defect as
    /// `refused_risk`'s changed meaning above, made twice.
    pub refused_unseeded: u64,
    /// Per-slot live submits. Index = `strategy_id`.
    pub live_submits_by_slot: [u64; EXEC_SLOTS],
    /// Per-slot refusals — EVERY reason (off, no-route, and E6's risk
    /// gate). Index = `strategy_id`.
    ///
    /// The total rather than a breakdown, because what an operator
    /// reads it for is "which slot is being refused"; the aggregate
    /// fields above say why.
    pub refused_by_slot: [u64; EXEC_SLOTS],
}

impl RouteCounters {
    /// A zeroed set.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            live_submits: 0,
            paper_submits: 0,
            refused_off: 0,
            refused_no_route: 0,
            refused_risk: 0,
            refused_max_order: 0,
            refused_cap_instance: 0,
            refused_cap_day: 0,
            refused_open_orders: 0,
            refused_unseeded: 0,
            live_submits_by_slot: [0; EXEC_SLOTS],
            refused_by_slot: [0; EXEC_SLOTS],
        }
    }

    /// Bump the per-slot live counter. Hot; the index is masked, so an
    /// out-of-range id lands in a real bucket rather than reading past
    /// the array — but an out-of-range id can never be `Live` (see
    /// [`crate::ExecRoute::mode`]), so this is belt-and-braces.
    #[inline(always)]
    fn bump_slot(arr: &mut [u64; EXEC_SLOTS], strategy_id: u8) {
        let idx = (strategy_id as usize) & (EXEC_SLOTS - 1);
        // SAFETY: `idx` is masked with `EXEC_SLOTS - 1`, a power-of-two
        // mask, so `idx < EXEC_SLOTS` for every `u8`.
        let slot = unsafe { arr.get_unchecked_mut(idx) };
        *slot = slot.saturating_add(1);
    }

    /// Record a live submit for `strategy_id`.
    #[inline(always)]
    pub fn on_live_submit(&mut self, strategy_id: u8) {
        self.live_submits = self.live_submits.saturating_add(1);
        Self::bump_slot(&mut self.live_submits_by_slot, strategy_id);
    }

    /// Record a paper submit.
    #[inline(always)]
    pub fn on_paper_submit(&mut self) {
        self.paper_submits = self.paper_submits.saturating_add(1);
    }

    /// Record a refusal because the slot is `Off`.
    #[inline(always)]
    pub fn on_refused_off(&mut self, strategy_id: u8) {
        self.refused_off = self.refused_off.saturating_add(1);
        Self::bump_slot(&mut self.refused_by_slot, strategy_id);
    }

    /// Record a refusal because the live slot has no route to the
    /// order's venue.
    #[inline(always)]
    pub fn on_refused_no_route(&mut self, strategy_id: u8) {
        self.refused_no_route = self.refused_no_route.saturating_add(1);
        Self::bump_slot(&mut self.refused_by_slot, strategy_id);
    }

    /// E6: record a refusal by the risk gate, and which clamp did it.
    ///
    /// Takes the reason rather than defaulting it, because a refusal
    /// counted only in the aggregate is a refusal an operator has to
    /// guess the cause of at exactly the moment guessing is expensive.
    #[inline(always)]
    pub fn on_refused_risk(&mut self, strategy_id: u8, why: RiskRefusal) {
        self.refused_risk = self.refused_risk.saturating_add(1);
        let field = match why {
            RiskRefusal::MaxOrder => &mut self.refused_max_order,
            RiskRefusal::CapInstance => &mut self.refused_cap_instance,
            RiskRefusal::CapDay => &mut self.refused_cap_day,
            RiskRefusal::OpenOrders => &mut self.refused_open_orders,
            RiskRefusal::Unseeded => &mut self.refused_unseeded,
        };
        *field = field.saturating_add(1);
        Self::bump_slot(&mut self.refused_by_slot, strategy_id);
    }

    /// Live submits recorded for one slot. Cold.
    #[inline]
    #[must_use]
    pub fn live_submits_at(&self, slot: usize) -> Option<u64> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(self.live_submits_by_slot[slot])
    }

    /// Refusals recorded for one slot. Cold.
    #[inline]
    #[must_use]
    pub fn refused_at(&self, slot: usize) -> Option<u64> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(self.refused_by_slot[slot])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::STRATEGY_ID_NONE;

    #[test]
    fn counters_are_cache_line_aligned() {
        assert_eq!(core::mem::align_of::<RouteCounters>(), 64);
    }

    #[test]
    fn new_is_zero_and_equals_default() {
        assert_eq!(RouteCounters::new(), RouteCounters::default());
        assert_eq!(RouteCounters::new().live_submits, 0);
    }

    #[test]
    fn per_slot_and_aggregate_move_together() {
        let mut c = RouteCounters::new();
        c.on_live_submit(3);
        c.on_live_submit(3);
        c.on_paper_submit();
        c.on_refused_off(6);
        c.on_refused_no_route(3);

        assert_eq!(c.live_submits, 2);
        assert_eq!(c.paper_submits, 1);
        assert_eq!(c.refused_off, 1);
        assert_eq!(c.refused_no_route, 1);
        assert_eq!(c.live_submits_at(3), Some(2));
        assert_eq!(c.refused_at(3), Some(1));
        assert_eq!(c.refused_at(6), Some(1));
        assert_eq!(c.live_submits_at(EXEC_SLOTS), None);
        assert_eq!(c.refused_at(EXEC_SLOTS), None);
    }

    #[test]
    fn an_out_of_range_id_never_reads_past_the_array() {
        let mut c = RouteCounters::new();
        // 0xFF & 7 == 7 — lands in a real bucket, no UB.
        c.on_refused_off(STRATEGY_ID_NONE);
        assert_eq!(c.refused_off, 1);
        assert_eq!(c.refused_at(7), Some(1));
    }

    #[test]
    fn counters_saturate_rather_than_wrap() {
        let mut c = RouteCounters::new();
        c.live_submits = u64::MAX;
        c.live_submits_by_slot[3] = u64::MAX;
        c.on_live_submit(3);
        assert_eq!(c.live_submits, u64::MAX, "saturating, not 0");
        assert_eq!(c.live_submits_at(3), Some(u64::MAX));
    }
}
