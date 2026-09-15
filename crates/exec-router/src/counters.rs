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
    pub refused_off: u64,
    /// Refused because the slot is live but the order named a venue
    /// the slot has no live route to. **LAW E-1: refused, never
    /// downgraded to paper.**
    pub refused_no_route: u64,
    /// Per-slot live submits. Index = `strategy_id`.
    pub live_submits_by_slot: [u64; EXEC_SLOTS],
    /// Per-slot refusals (off + no-route). Index = `strategy_id`.
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
