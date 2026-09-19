// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **E6 commit 3 — the sticky halt.**
//!
//! Six triggers, one latch per slot, and no way out but an operator.
//!
//! ## What halts, and why each one (in `trigger_for`'s order)
//!
//! | trigger | threshold | what it means |
//! |---|---|---|
//! | consecutive venue rejects | `halt_on_reject_streak` | the venue understands us and keeps saying no |
//! | asset-id refusal streak | `halt_on_asset_refusal_streak` | LAW E-4 — a member quoting an instance that has rolled |
//! | request budget floor | `request_budget_floor` (E4) | the address allowance is spent; the next order may not go |
//! | reconciliation drift | `halt_on_recon_drift_usd_1e6` | our position and the venue's disagree by real money |
//! | reconciliation stale | `halt_on_recon_stale_ms` | the reconciler has not AGREED with the venue for this long — the safety net is dark |
//! | user-event stream gap | `halt_on_ws_gap_ms` | LAW E-5 — the WS is the FILL, so a gap is trading blind |
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
    /// No reconciliation has AGREED with the venue for
    /// `halt_on_recon_stale_ms`.
    ReconStale = 7,
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
            HaltReason::ReconStale => "recon-stale",
        }
    }

    /// The inverse of [`Self::as_str`]. Named `from_word` rather than
    /// `from_str` so it cannot be confused with `std::str::FromStr`,
    /// which it deliberately is not: that trait returns a `Result`,
    /// and this must never have a failure case.
    ///
    /// An unrecognised word is
    /// `Operator`, not `None`: a halt file naming a reason this
    /// binary does not know is still a halt, and the safe reading of
    /// "I cannot tell why" is "stay stopped".
    #[must_use]
    pub fn from_word(word: &str) -> Self {
        match word {
            "reject-streak" => HaltReason::RejectStreak,
            "budget-floor" => HaltReason::BudgetFloor,
            "recon-drift" => HaltReason::ReconDrift,
            "ws-gap" => HaltReason::WsGap,
            "asset-refusals" => HaltReason::AssetRefusals,
            "recon-stale" => HaltReason::ReconStale,
            _ => HaltReason::Operator,
        }
    }

    /// Anything but [`HaltReason::None`].
    #[inline]
    #[must_use]
    pub const fn is_halted(self) -> bool {
        !matches!(self, HaltReason::None)
    }
}

/// Bytes a rendered halt file can take: the two header lines plus one
/// `slot=NN reason=<longest word>` line per slot.
pub const HALT_FILE_MAX: usize = 512;

/// **The render must not be able to truncate.** `put` clamps to the
/// buffer, so a table that outgrew it would silently write a SHORT
/// halt file — naming some halted slots and not others, which is the
/// exact defect commit 4 exists to fix. Growing `EXEC_SLOTS` past
/// what fits is a compile error instead.
///
/// `128` is the two header lines (125 B today). `30` is the longest
/// line: `slot=` (5) + two digits (2) + ` reason=` (8) +
/// `asset-refusals` (14) + `\n` (1); `recon-stale` is shorter.
/// Written out rather than named, because a `const` used only by a
/// `const _` assert reads as dead code to the lint.
const _: () = assert!(128 + EXEC_SLOTS * 30 <= HALT_FILE_MAX);

/// **Render the whole halt state.**
///
/// Every halted slot, not just the last one to trip. The single-slot
/// version this replaces would have made the boot read-back resume a
/// slot that was halted, because the file only ever named the most
/// recent — and the read-back is the entire reason the file exists.
///
/// Returns the used length of `buf`. ASCII by construction: digits,
/// two literals and [`HaltReason::as_str`].
pub fn render_halt_file(buf: &mut [u8; HALT_FILE_MAX], reasons: &[HaltReason; EXEC_SLOTS]) -> usize {
    let mut n = 0usize;
    {
        let mut put = |bytes: &[u8]| {
            let room = buf.len().saturating_sub(n);
            let take = bytes.len().min(room);
            // COPY: ≤ HALT_FILE_MAX B of literals + reason words into
            // the file image — the RENDER of `exec.HALT`, once per halt
            // edge (cold); the bytes must be contiguous for one write.
            buf[n..n + take].copy_from_slice(&bytes[..take]);
            n += take;
        };
        put(b"# exec.HALT - engine-written. Delete it and restart to clear.\n");
        put(b"# A line is `slot=<n> reason=<word>`; a bare number works too.\n");
        let mut slot = 0usize;
        while slot < EXEC_SLOTS {
            let why = reasons[slot];
            if why.is_halted() {
                put(b"slot=");
                if slot >= 10 {
                    put(&[b'0' + (slot / 10) as u8]);
                }
                put(&[b'0' + (slot % 10) as u8]);
                put(b" reason=");
                put(why.as_str().as_bytes());
                put(b"\n");
            }
            slot += 1;
        }
    }
    n
}

/// **Parse a halt file — engine-written or hand-written.**
///
/// Ignores blank lines and `#` comments. A line may be
/// `slot=<n> reason=<word>` or a bare slot number, because an
/// operator reaching for this in a hurry should be able to write
/// `echo 3 > exec.HALT` and have it mean something.
///
/// A slot out of range is skipped rather than refusing the file: one
/// unreadable line must not discard the halts that parsed, and this
/// is the direction where discarding is the dangerous outcome.
#[must_use]
pub fn parse_halt_file(text: &str) -> [HaltReason; EXEC_SLOTS] {
    let mut out = [HaltReason::None; EXEC_SLOTS];
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut slot: Option<usize> = None;
        let mut why = HaltReason::Operator;
        for field in line.split_whitespace() {
            if let Some(v) = field.strip_prefix("slot=") {
                slot = v.parse().ok();
            } else if let Some(v) = field.strip_prefix("reason=") {
                why = HaltReason::from_word(v);
            } else if slot.is_none() {
                // A bare number, hand-written.
                slot = field.parse().ok();
            }
        }
        if let Some(i) = slot {
            if i < EXEC_SLOTS {
                out[i] = why;
            }
        }
    }
    out
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
    // `recon_age_ns == 0` is "never agreed", which the seeding
    // interlock already refuses on — the same sentinel rule as
    // `ws_gap_ns`. Once it HAS agreed, silence from the reconciler is
    // the safety net going dark and is measured here.
    if lim.recon_stale_ms > 0 && sig.recon_age_ns > 0 {
        let age_ms = (sig.recon_age_ns / 1_000_000) as i64;
        if age_ms >= lim.recon_stale_ms {
            return HaltReason::ReconStale;
        }
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
/// Asked, and the request was refused or only partly queued. Ask
/// again — once the arm has room.
const CANCEL_RETRY: u8 = 3;

/// **What the router owes the live arm on this poll.**
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelPhase {
    /// Nothing outstanding.
    None,
    /// Ask the arm to cancel everything.
    Wanted,
    /// Already asked; poll for confirmation.
    Asked,
    /// A request did not land. Ask again — but a request the arm
    /// refused for want of sweep-table room would be refused again
    /// while it is still draining, so the router waits out `Working`
    /// first rather than asking 500 times a second.
    Retry,
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

    /// Every slot's reason, for the halt file and `/state`.
    #[inline]
    #[must_use]
    pub const fn reasons(&self) -> &[HaltReason; EXEC_SLOTS] {
        &self.reasons
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
            if self.pending_cancel[i] == CANCEL_WANTED || self.pending_cancel[i] == CANCEL_RETRY {
                self.pending_cancel[i] = CANCEL_ASKED;
            }
            i += 1;
        }
    }

    /// **What the caller owes the arm right now.**
    ///
    /// `Wanted` beats `Retry` beats `Asked`: a slot that halted while
    /// another slot's sweep was already draining needs its own legs
    /// requested, and a cancel-all is venue-wide, so one fresh
    /// request covers both.
    #[inline]
    #[must_use]
    pub fn cancel_phase(&self) -> CancelPhase {
        let mut asked = false;
        let mut retry = false;
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            match self.pending_cancel[i] {
                CANCEL_WANTED => return CancelPhase::Wanted,
                CANCEL_RETRY => retry = true,
                CANCEL_ASKED => asked = true,
                _ => {}
            }
            i += 1;
        }
        if retry {
            CancelPhase::Retry
        } else if asked {
            CancelPhase::Asked
        } else {
            CancelPhase::None
        }
    }

    /// A `cancel_all` REQUEST did not land (refused outright, or only
    /// partially queued). Every slot that was `ASKED` goes back to
    /// `WANTED`, so the NEXT poll asks again rather than confirming.
    ///
    /// The first cut left them `ASKED` on the premise that the next
    /// poll would read `Stranded` and re-ask — but the arm reported
    /// `Clear` once the legs it DID queue drained, and the router then
    /// zeroed every slot's resting count over quotes still live at the
    /// venue: the commit-3 fail-open, back through the `QueueFull`
    /// door (E7 review, 2026-09-19). A refused request is re-requested
    /// by construction now, whatever the arm's state machine says.
    #[inline]
    pub fn cancel_failed(&mut self) {
        self.cancel_all_failures = self.cancel_all_failures.saturating_add(1);
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            if self.pending_cancel[i] == CANCEL_ASKED {
                self.pending_cancel[i] = CANCEL_RETRY;
            }
            i += 1;
        }
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
        HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000)
    }

    fn healthy() -> HaltSignal {
        HaltSignal::new(1_000_000, 0, 0, 0, false, true, 1_000_000)
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
    fn a_stale_reconciliation_halts_only_after_it_has_agreed_once() {
        let mut s = healthy();
        s.recon_age_ns = 0; // never agreed: the interlock's case, not this trigger's
        assert_eq!(trigger_for(&s, &lim()), HaltReason::None);
        s.recon_age_ns = 299_999_000_000;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::None);
        s.recon_age_ns = 300_000_000_000;
        assert_eq!(trigger_for(&s, &lim()), HaltReason::ReconStale);
        assert_eq!(HaltReason::from_word("recon-stale"), HaltReason::ReconStale);
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
        let s = HaltSignal::new(u64::MAX, i64::MAX, u32::MAX, u32::MAX, false, true, u64::MAX);
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
        let tight = HaltLimits::new(3, 5_000_000, 30_000, 3, 300_000);
        let loose = HaltLimits::new(50, 5_000_000, 30_000, 3, 300_000);
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

    /// The property the boot read-back rests on: what the engine
    /// wrote is what the next boot reads.
    #[test]
    fn the_halt_file_round_trips_every_halted_slot() {
        let mut reasons = [HaltReason::None; EXEC_SLOTS];
        reasons[3] = HaltReason::WsGap;
        reasons[5] = HaltReason::Operator;
        reasons[7] = HaltReason::ReconDrift;

        let mut buf = [0u8; HALT_FILE_MAX];
        let n = render_halt_file(&mut buf, &reasons);
        let text = core::str::from_utf8(&buf[..n]).expect("ascii by construction");
        assert_eq!(
            parse_halt_file(text),
            reasons,
            "three halted slots, three read back — not just the last\n{text}"
        );
    }

    /// The single-slot writer this replaced would have let the boot
    /// resume a slot that was halted.
    #[test]
    fn a_second_halted_slot_is_not_lost_to_the_first() {
        let mut reasons = [HaltReason::None; EXEC_SLOTS];
        reasons[0] = HaltReason::RejectStreak;
        reasons[1] = HaltReason::BudgetFloor;
        let mut buf = [0u8; HALT_FILE_MAX];
        let n = render_halt_file(&mut buf, &reasons);
        let back = parse_halt_file(core::str::from_utf8(&buf[..n]).unwrap());
        assert!(back[0].is_halted() && back[1].is_halted());
    }

    #[test]
    fn nothing_halted_renders_a_file_that_halts_nothing() {
        let mut buf = [0u8; HALT_FILE_MAX];
        let n = render_halt_file(&mut buf, &[HaltReason::None; EXEC_SLOTS]);
        let back = parse_halt_file(core::str::from_utf8(&buf[..n]).unwrap());
        assert_eq!(back, [HaltReason::None; EXEC_SLOTS], "comments only");
    }

    /// An operator reaching for this in a hurry writes the shortest
    /// thing that could work, and it has to work.
    #[test]
    fn a_hand_written_halt_file_is_understood() {
        let back = parse_halt_file("3\n");
        assert_eq!(back[3], HaltReason::Operator);
        assert_eq!(back[2], HaltReason::None);

        // With whitespace, comments and blank lines around it.
        let back = parse_halt_file("# stop bin15\n\n  5  \n");
        assert_eq!(back[5], HaltReason::Operator);
    }

    /// A reason this binary does not know still halts. "I cannot tell
    /// why" reads as "stay stopped", never as "carry on".
    #[test]
    fn an_unknown_reason_still_halts() {
        let back = parse_halt_file("slot=3 reason=something-from-the-future\n");
        assert!(back[3].is_halted());
        assert_eq!(back[3], HaltReason::Operator);
    }

    /// One unreadable line must not discard the halts that parsed.
    #[test]
    fn a_bad_line_does_not_throw_away_the_good_ones() {
        let back = parse_halt_file("slot=99 reason=ws-gap\nnonsense\nslot=3 reason=ws-gap\n");
        assert_eq!(back[3], HaltReason::WsGap, "the readable line survived");
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
