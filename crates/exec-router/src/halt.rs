// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **E6 commit 3 — the sticky halt.**
//!
//! Five triggers, one latch per slot, and no way out but an operator.
//!
//! ## What halts, and why each one
//!
//! | trigger | threshold | what it means |
//! |---|---|---|
//! | consecutive venue rejects | `halt_on_reject_streak` | the venue understands us and keeps saying no |
//! | request budget floor | `request_budget_floor` (E4) | the address allowance is spent; the next order may not go |
//! | reconciliation drift | `halt_on_recon_drift_usd_1e6` | our position and the venue's disagree by real money |
//! | user-event stream gap | `halt_on_ws_gap_ms` | LAW E-5 — the WS is the FILL, so a gap is trading blind |
//! | asset-id refusal streak | `halt_on_asset_refusal_streak` | LAW E-4 — a member quoting an instance that has rolled |
//!
//! **The day cap is NOT here**, and the plan listed it. Reaching
//! `cap_day_usd` is the clamp working, and it clears itself at
//! 00:00Z; a sticky halt on it would stop the engine for good every
//! day it traded to its cap and need a human to restart. The other
//! five mean "something is wrong"; that one means "today is done".
//! Operator ruling, 2026-09-19.
//!
//! ## Venue-wide sensors, per-slot thresholds
//!
//! Every trigger above is a property of the ARM — the socket, the
//! budget, the reconciler — not of any one member. Each live slot
//! compares that one signal against ITS OWN numbers, so a slot with a
//! tighter threshold halts first and a slot on another venue is
//! untouched. That is what "per-slot refusal, venue-wide cancel"
//! means in code.
//!
//! ## Sticky
//!
//! Nothing clears a halt in-process. There is no timeout, no
//! half-open state and no automatic retry of the condition: a member
//! that halted because the venue rejected fifty orders must not
//! resume because the fifty-first would have been accepted. The
//! operator clears it, and E6 commit 3 writes `exec.HALT` so a
//! restart does not clear it either — the scheduled daily restart
//! would otherwise resume trading into the condition, overnight,
//! unattended.
//!
//! ## What a halted slot may still do
//!
//! Cancel. Nothing else. A modify places a new order and can raise
//! price and size — it is a submit wearing a different name — while a
//! cancel is the only way to get flat and must never be blocked.

use crate::route::{HaltLimits, EXEC_SLOTS};
use clob_dispatcher::HaltSignal;

/// Why a slot halted. `None` is the running state.
///
/// One reason per slot: the first trigger to fire latches, and the
/// ones after it describe the same incident. An operator reading
/// `/state` wants to know what went wrong FIRST.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum HaltReason {
    /// Running.
    #[default]
    None = 0,
    /// Consecutive venue rejections reached `halt_on_reject_streak`.
    RejectStreak = 1,
    /// The address request budget is at or below its floor.
    BudgetFloor = 2,
    /// Reconciliation drift reached `halt_on_recon_drift_usd_1e6`.
    ReconDrift = 3,
    /// The user-event stream has been quiet past `halt_on_ws_gap_ms`.
    WsGap = 4,
    /// Consecutive LAW E-4 refusals reached
    /// `halt_on_asset_refusal_streak`.
    AssetRefusals = 5,
    /// The operator asked for it — `exec.HALT`, or a future control
    /// path. Never decided by a trigger.
    Operator = 6,
}

impl HaltReason {
    /// The word that goes in `exec.HALT` and in `/state`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            HaltReason::None => "none",
            HaltReason::RejectStreak => "reject-streak",
            HaltReason::BudgetFloor => "budget-floor",
            HaltReason::ReconDrift => "recon-drift",
            HaltReason::WsGap => "ws-gap",
            HaltReason::AssetRefusals => "asset-refusals",
            HaltReason::Operator => "operator",
        }
    }

    #[inline]
    #[must_use]
    const fn is_halted(self) -> bool {
        !matches!(self, HaltReason::None)
    }
}

/// **Which trigger, if any, fires for one slot.**
///
/// A free function over `(signal, limits)` and nothing else, so the
/// decision is testable without an arm, a socket or a router. Every
/// threshold is `>=`: a limit of 5 rejections means the fifth halts,
/// because an operator who wrote 5 asked for five and not six.
///
/// A threshold of `0` is UNSET and never fires.
/// `core_config::exec` refuses a live slot that leaves one at zero,
/// so an unset threshold cannot reach a live boot — this is the
/// belt, and it fails SILENT rather than halting everything, because
/// a zero that halted would take down every paper slot in the table.
///
/// **Order is the reporting order**, not a priority: at most one
/// reason is latched and the rest describe the same incident, so the
/// cheapest and most specific comes first.
#[must_use]
pub fn trigger_for(sig: &HaltSignal, lim: &HaltLimits) -> HaltReason {
    if lim.reject_streak > 0 && sig.reject_streak >= lim.reject_streak {
        return HaltReason::RejectStreak;
    }
    if lim.asset_refusal_streak > 0 && sig.asset_refusal_streak >= lim.asset_refusal_streak {
        return HaltReason::AssetRefusals;
    }
    // The budget floor has no per-slot threshold of its own — it is
    // `request_budget_floor`, which the ARM already compares against
    // and reports as a flag. A slot cannot opt out of it: an address
    // with no allowance left cannot place for anybody.
    if sig.budget_floor_breached != 0 {
        return HaltReason::BudgetFloor;
    }
    if lim.recon_drift_usd_1e6 > 0 && sig.recon_drift_usd_1e6 >= lim.recon_drift_usd_1e6 {
        return HaltReason::ReconDrift;
    }
    // `ws_gap_ns == 0` is "no observation", not "no gap": an arm that
    // has never connected is not an arm that has gone quiet, and
    // halting before the first connect would make the engine
    // unstartable.
    if lim.ws_gap_ms > 0 && sig.ws_gap_ns > 0 {
        let gap_ms = (sig.ws_gap_ns / 1_000_000) as i64;
        if gap_ms >= lim.ws_gap_ms {
            return HaltReason::WsGap;
        }
    }
    HaltReason::None
}

/// No cancel-all owed for this slot.
const CANCEL_NONE: u8 = 0;
/// Latched, and the arm has not been asked yet.
const CANCEL_WANTED: u8 = 1;
/// Asked; awaiting the venue's confirmation.
const CANCEL_ASKED: u8 = 2;

/// **What the router owes the live arm on this poll.**
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelPhase {
    /// Nothing outstanding.
    None,
    /// Ask the arm to cancel everything.
    Wanted,
    /// Already asked; poll for confirmation.
    Asked,
}

/// **The per-slot latch.**
///
/// `Copy` POD, owned by the router, mutated only on the engine
/// thread.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HaltState {
    /// Index = `strategy_id`. Why each slot halted.
    reasons: [HaltReason; EXEC_SLOTS],
    /// **Where this slot is in the cancel-all cycle.**
    ///
    /// Index = `strategy_id`, one of [`CANCEL_NONE`],
    /// [`CANCEL_WANTED`], [`CANCEL_ASKED`]. Three-valued because a
    /// cancel-all on a real venue is a request and a confirmation
    /// separated by minutes, and the caller's job differs between
    /// them: `WANTED` means ask the arm, `ASKED` means poll it.
    ///
    /// The halt latches immediately whatever this says — waiting for
    /// a successful cancel before refusing would keep submitting into
    /// the condition that tripped the halt.
    pending_cancel: [u8; EXEC_SLOTS],
    /// Halt edges, total. One per slot that ever halted.
    pub halts: u64,
    /// Cancel-all REQUESTS the arm would not accept.
    pub cancel_all_failures: u64,
    /// **Polls on which the arm reported it had given up** — a sweep
    /// abandoned, or a leg it could never queue — and the router
    /// asked again. Distinct from `cancel_all_failures`, which counts
    /// requests that never landed: this one counts requests that
    /// landed and still left orders working at the venue, which is
    /// the number an operator actually wants after an incident.
    pub cancel_all_stranded: u64,
}

/// One cache line, and it must stay that way: the router reads it on
/// every dispatch (`risk_check` step 0a) and on every idle poll, so a
/// second line here is a second miss on the hot path.
const _: () = assert!(core::mem::size_of::<HaltState>() == 64);
const _: () = assert!(core::mem::align_of::<HaltState>() == 64);

impl Default for HaltState {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl HaltState {
    /// Nothing halted.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            reasons: [HaltReason::None; EXEC_SLOTS],
            pending_cancel: [0; EXEC_SLOTS],
            halts: 0,
            cancel_all_failures: 0,
            cancel_all_stranded: 0,
        }
    }

    /// Why this slot halted, or [`HaltReason::None`].
    #[inline]
    #[must_use]
    pub fn reason(&self, slot: usize) -> HaltReason {
        if slot >= EXEC_SLOTS {
            return HaltReason::None;
        }
        self.reasons[slot]
    }

    /// Is this slot halted?
    #[inline]
    #[must_use]
    pub fn is_halted(&self, slot: usize) -> bool {
        self.reason(slot).is_halted()
    }

    /// Is ANY slot halted? Decides whether the idle path has a
    /// cancel-all to retry.
    #[inline]
    #[must_use]
    pub fn any_halted(&self) -> bool {
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            if self.reasons[i].is_halted() {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Does any halted slot still have orders the venue was not told
    /// to remove?
    #[inline]
    #[must_use]
    pub fn cancel_outstanding(&self) -> bool {
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            if self.pending_cancel[i] != 0 {
                return true;
            }
            i += 1;
        }
        false
    }

    /// **Latch a halt. Returns `true` on the EDGE** — the first time
    /// this slot halts — so the caller fires cancel-all once per
    /// incident rather than once per poll.
    ///
    /// Sticky: a slot already halted keeps its FIRST reason. The ones
    /// after it describe the same incident, and an operator wants to
    /// know what went wrong first.
    pub fn latch(&mut self, slot: usize, why: HaltReason) -> bool {
        if slot >= EXEC_SLOTS || !why.is_halted() || self.reasons[slot].is_halted() {
            return false;
        }
        self.reasons[slot] = why;
        // WANTED, not ASKED: a fresh edge always gets a fresh
        // request, even while another slot's cancel is mid-flight.
        // This slot's legs were not in that sweep.
        self.pending_cancel[slot] = CANCEL_WANTED;
        self.halts = self.halts.saturating_add(1);
        true
    }

    /// The venue confirmed it holds nothing for us.
    #[inline]
    pub fn cancel_cleared(&mut self) {
        self.pending_cancel = [CANCEL_NONE; EXEC_SLOTS];
    }

    /// The request has gone to the arm; from here the caller polls
    /// rather than asks.
    #[inline]
    pub fn cancel_requested(&mut self) {
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            if self.pending_cancel[i] == CANCEL_WANTED {
                self.pending_cancel[i] = CANCEL_ASKED;
            }
            i += 1;
        }
    }

    /// **What the caller owes the arm right now.**
    ///
    /// `Wanted` beats `Asked`: a slot that halted while another
    /// slot's sweep was already draining needs its own legs
    /// requested, and a cancel-all is venue-wide, so one fresh
    /// request covers both.
    #[inline]
    #[must_use]
    pub fn cancel_phase(&self) -> CancelPhase {
        let mut asked = false;
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            match self.pending_cancel[i] {
                CANCEL_WANTED => return CancelPhase::Wanted,
                CANCEL_ASKED => asked = true,
                _ => {}
            }
            i += 1;
        }
        if asked {
            CancelPhase::Asked
        } else {
            CancelPhase::None
        }
    }

    /// A `cancel_all` REQUEST did not land. It stays `ASKED`: the
    /// poll that follows reports `Stranded` and asks again, so the
    /// retry has one shape whether the request errored or the sweep
    /// was abandoned.
    #[inline]
    pub fn cancel_failed(&mut self) {
        self.cancel_all_failures = self.cancel_all_failures.saturating_add(1);
    }

    /// The arm reported it had stopped with the venue unconfirmed.
    #[inline]
    pub fn cancel_stranded(&mut self) {
        self.cancel_all_stranded = self.cancel_all_stranded.saturating_add(1);
    }

}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Thresholds an operator might really write.
    fn lim() -> HaltLimits {
        HaltLimits::new(5, 5_000_000, 30_000, 3)
    }

    fn healthy() -> HaltSignal {
        HaltSignal::new(1_000_000, 0, 0, 0, false, true)
    }

    // -----------------------------------------------------------------
    // one test per trigger — the plan's E6 exit gate
    // -----------------------------------------------------------------

    #[test]
    fn a_reject_streak_halts_at_the_number_the_operator_wrote() {
        let mut s = healthy();
        s.reject_streak = 4;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::None, "four is not five");
        s.reject_streak = 5;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::RejectStreak);
    }

    #[test]
    fn a_breached_budget_floor_halts() {
        let mut s = healthy();
        s.budget_floor_breached = 1;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::BudgetFloor);
    }

    #[test]
    fn reconciliation_drift_halts_at_the_money_threshold() {
        let mut s = healthy();
        s.recon_drift_usd_1e6 = 4_999_999;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::None);
        s.recon_drift_usd_1e6 = 5_000_000;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::ReconDrift);
    }

    #[test]
    fn a_user_stream_gap_halts() {
        let mut s = healthy();
        s.ws_gap_ns = 29_999_000_000; // 29.999 s
        assert_eq!(trigger_for(&s, &lim()), HaltReason::None);
        s.ws_gap_ns = 30_000_000_000; // 30 s
        assert_eq!(trigger_for(&s, &lim()), HaltReason::WsGap);
    }

    #[test]
    fn an_asset_refusal_streak_halts() {
        let mut s = healthy();
        s.asset_refusal_streak = 2;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::None);
        s.asset_refusal_streak = 3;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::AssetRefusals);
    }

    // -----------------------------------------------------------------
    // the edges around them
    // -----------------------------------------------------------------

    #[test]
    fn a_healthy_arm_halts_on_nothing() {
        assert_eq!(trigger_for(&healthy(), &lim()), HaltReason::None);
    }

    /// **An arm that has never connected is not an arm that has gone
    /// quiet.** `ws_gap_ns == 0` is "no observation", and reading it
    /// as an infinite gap would halt every boot before its first
    /// connect — an engine that cannot start.
    #[test]
    fn a_stream_that_has_never_been_up_does_not_halt() {
        let mut s = healthy();
        s.ws_gap_ns = 0;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::None);
    }

    /// A threshold of `0` is UNSET. `core_config::exec` refuses a live
    /// slot that leaves one at zero, so this is the belt — and it
    /// fails SILENT rather than halting, because a zero that halted
    /// would take down every slot in an all-paper table.
    #[test]
    fn an_unset_threshold_never_fires() {
        let none = HaltLimits::none();
        let s = HaltSignal::new(u64::MAX, i64::MAX, u32::MAX, u32::MAX, false, true);
        assert_eq!(trigger_for(&s, &none), HaltReason::None);
    }

    /// The budget floor is the ONE trigger with no per-slot threshold:
    /// an address with no allowance left cannot place for anybody, so
    /// a slot cannot opt out of it.
    #[test]
    fn the_budget_floor_fires_even_with_every_threshold_unset() {
        let mut s = healthy();
        s.budget_floor_breached = 1;
        assert_eq!(
            trigger_for(&s, &HaltLimits::none()),
            HaltReason::BudgetFloor
        );
    }

    #[test]
    fn two_slots_with_different_thresholds_reach_different_conclusions() {
        // The point of venue-wide sensors and per-slot thresholds: one
        // signal, two answers.
        let mut s = healthy();
        s.reject_streak = 3;
        let tight = HaltLimits::new(3, 5_000_000, 30_000, 3);
        let loose = HaltLimits::new(50, 5_000_000, 30_000, 3);
        assert_eq!(trigger_for(&s, &tight), HaltReason::RejectStreak);
        assert_eq!(trigger_for(&s, &loose), HaltReason::None);
    }

    // -----------------------------------------------------------------
    // the latch
    // -----------------------------------------------------------------

    #[test]
    fn the_first_halt_is_an_edge_and_the_rest_are_not() {
        let mut h = HaltState::new();
        assert!(h.latch(3, HaltReason::WsGap), "the first is the edge");
        assert!(!h.latch(3, HaltReason::ReconDrift), "and the rest are not");
        assert_eq!(h.halts, 1, "one incident, one edge");
    }

    #[test]
    fn a_halt_keeps_its_first_reason() {
        // What went wrong FIRST is what an operator needs. The
        // triggers that follow describe the same incident — a dead
        // socket produces a reject streak and a drift soon after.
        let mut h = HaltState::new();
        h.latch(3, HaltReason::WsGap);
        h.latch(3, HaltReason::RejectStreak);
        assert_eq!(h.reason(3), HaltReason::WsGap);
    }

    #[test]
    fn halting_one_slot_leaves_the_others_running() {
        let mut h = HaltState::new();
        h.latch(3, HaltReason::WsGap);
        assert!(h.is_halted(3));
        assert!(!h.is_halted(5));
        assert!(h.any_halted());
    }

    #[test]
    fn nothing_clears_a_halt() {
        // Sticky. There is no timeout, no half-open state and no
        // `resume` on this type at all — a member that halted because
        // the venue rejected fifty orders must not resume because the
        // fifty-first would have been accepted.
        let mut h = HaltState::new();
        h.latch(3, HaltReason::RejectStreak);
        for _ in 0..1_000 {
            assert!(!h.latch(3, HaltReason::None), "None is not a halt");
            assert!(h.is_halted(3));
        }
        assert_eq!(h.reason(3), HaltReason::RejectStreak);
    }

    #[test]
    fn a_halt_edge_leaves_the_venue_marked_uncleared_until_it_is() {
        let mut h = HaltState::new();
        assert!(!h.cancel_outstanding());
        h.latch(3, HaltReason::WsGap);
        assert!(h.cancel_outstanding(), "the venue still holds our orders");
        h.cancel_cleared();
        assert!(!h.cancel_outstanding());
    }

    #[test]
    fn an_out_of_range_slot_is_not_halted_and_cannot_be() {
        let mut h = HaltState::new();
        assert!(!h.latch(EXEC_SLOTS, HaltReason::WsGap));
        assert!(!h.is_halted(EXEC_SLOTS));
        assert_eq!(h.reason(EXEC_SLOTS), HaltReason::None);
        assert_eq!(h.halts, 0);
    }

    #[test]
    fn every_reason_has_a_distinct_word_for_the_halt_file() {
        let all = [
            HaltReason::None,
            HaltReason::RejectStreak,
            HaltReason::BudgetFloor,
            HaltReason::ReconDrift,
            HaltReason::WsGap,
            HaltReason::AssetRefusals,
            HaltReason::Operator,
        ];
        let mut i = 0usize;
        while i < all.len() {
            let mut j = i + 1;
            while j < all.len() {
                assert_ne!(
                    all[i].as_str(),
                    all[j].as_str(),
                    "two reasons share a word in exec.HALT"
                );
                j += 1;
            }
            assert!(!all[i].as_str().is_empty());
            i += 1;
        }
    }
}
