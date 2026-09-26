// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Reconnect backoff — capped exponential with jitter (D8 fix)
//!
//! Replaces the flat 500 ms sleep every ingress used between
//! reconnect attempts. During a venue outage the flat delay hammers
//! the endpoint (and burns OKX's 3 connection-attempts/s budget);
//! the exponential schedule backs off 500 ms → 8 s with equal-jitter
//! so a fleet of ingress threads doesn't thundering-herd the venue
//! on recovery.
//!
//! Deterministic: jitter comes from an internal splitmix64 stream
//! seeded by the caller, so tests can assert exact schedules.
//!
//! ## When a session's end resets the schedule
//!
//! [`should_reset_backoff`] — the healthy-session law — is the one answer
//! for every reconnecting loop: the seven outer spawn loops in the cli
//! (one connection per thread) and, since ruling O-HC16 (2026-09-26), the
//! internal reconnects of `ingress-hypercall` (one connection) and
//! `ingress-mexc` (per slot). It moved here from `cli/src/paper.rs`
//! unchanged.

/// Default first-retry delay.
pub const BACKOFF_BASE_NS: u64 = 500_000_000; // 500 ms
/// Default cap.
pub const BACKOFF_CAP_NS: u64 = 8_000_000_000; // 8 s

/// Capped exponential backoff state. One per connection-owning
/// thread; not shared.
#[derive(Copy, Clone, Debug)]
pub struct Backoff {
    base_ns: u64,
    cap_ns: u64,
    /// Consecutive failures since the last [`Self::reset`].
    attempt: u32,
    /// splitmix64 state for jitter.
    rng: u64,
}

impl Backoff {
    /// Construct with explicit base/cap. `seed` decorrelates jitter
    /// across threads (pass the core id or a boot nonce).
    pub const fn new(base_ns: u64, cap_ns: u64, seed: u64) -> Self {
        Self {
            base_ns,
            cap_ns,
            attempt: 0,
            rng: seed,
        }
    }

    /// House defaults: 500 ms → 8 s.
    pub const fn default_for_ingress(seed: u64) -> Self {
        Self::new(BACKOFF_BASE_NS, BACKOFF_CAP_NS, seed)
    }

    /// Delay before the next reconnect attempt, advancing the
    /// schedule. Equal-jitter: uniformly in `[d/2, d)` where
    /// `d = min(cap, base << attempt)` — retains exponential spacing
    /// while decorrelating simultaneous reconnectors.
    pub fn next_delay_ns(&mut self) -> u64 {
        let shift = if self.attempt >= 31 { 31 } else { self.attempt };
        let d = shl_capped(self.base_ns, shift, self.cap_ns);
        self.attempt = self.attempt.saturating_add(1);
        let half = d / 2;
        half + self.next_rand() % half.max(1)
    }

    /// Number of consecutive failures recorded so far.
    #[inline]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Call after a session was healthy (e.g. reached Steady and
    /// exchanged data) so the next failure starts from `base` again.
    #[inline]
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    #[inline]
    fn next_rand(&mut self) -> u64 {
        // splitmix64 — same generator the WS key seeding uses.
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A session must live this long, moving market data, before its end
/// resets the reconnect backoff ([`should_reset_backoff`]).
pub const HEALTHY_SESSION_MIN_NS: u64 = 30_000_000_000;

/// T1(b) — the D8 intent, restored (outage 2026-08-27 §5.3): the
/// reconnect schedule resets only when the session actually MOVED
/// MARKET DATA (`ticks_after > ticks_before`) for at least
/// [`HEALTHY_SESSION_MIN_NS`], or ended in a venue-quiet
/// idle/staleness trip (inherently rate-limited by the keepalive /
/// staleness budget, so it cannot hammer). A session that only
/// received its own subscribe rejection — the exact post-settlement
/// failure that reconnected at ~1 Hz for 16 h/day — keeps
/// escalating.
///
/// The lifetime clause (2026-09-26): a Hyperliquid session that
/// re-subscribed a settled HIP-4 coin got its snapshots and was then
/// dropped by the venue within a second — ticks moved, so the backoff
/// reset every time and the lane reconnected every ~1.2 s for hours
/// (~48 connects/min against the venue's 30/min per IP). Whatever the
/// cause, a session that dies young now escalates to the 8 s cap.
/// One definition for every reconnecting loop (module doc).
#[inline]
#[must_use]
pub const fn should_reset_backoff(
    ticks_after: u64,
    ticks_before: u64,
    session_ns: u64,
    venue_quiet_trip: bool,
) -> bool {
    (ticks_after > ticks_before && session_ns >= HEALTHY_SESSION_MIN_NS) || venue_quiet_trip
}

/// Saturating `base << shift`, clamped to `cap`.
#[inline]
fn shl_capped(base: u64, shift: u32, cap: u64) -> u64 {
    match base.checked_shl(shift) {
        Some(v) if v < cap => v,
        _ => cap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_is_exponential_then_capped() {
        let mut b = Backoff::default_for_ingress(7);
        // Expected midpoints: 500ms, 1s, 2s, 4s, 8s, 8s, ...
        let expected_full = [
            500_000_000u64,
            1_000_000_000,
            2_000_000_000,
            4_000_000_000,
            8_000_000_000,
            8_000_000_000,
        ];
        for want in expected_full {
            let d = b.next_delay_ns();
            // Equal jitter keeps d in [want/2, want).
            assert!(d >= want / 2, "delay {d} below {}", want / 2);
            assert!(d < want, "delay {d} not below {want}");
        }
        assert_eq!(b.attempt(), 6);
    }

    #[test]
    fn reset_restarts_from_base() {
        let mut b = Backoff::default_for_ingress(1);
        let _ = b.next_delay_ns();
        let _ = b.next_delay_ns();
        b.reset();
        let d = b.next_delay_ns();
        assert!(
            d < BACKOFF_BASE_NS,
            "post-reset delay {d} not in first bucket"
        );
        assert!(d >= BACKOFF_BASE_NS / 2);
    }

    #[test]
    fn huge_attempt_count_saturates_at_cap() {
        // Failure mode: attempt counter far past the cap must not
        // overflow the shift.
        let mut b = Backoff::new(500_000_000, 8_000_000_000, 3);
        for _ in 0..100 {
            let d = b.next_delay_ns();
            assert!(d < 8_000_000_000);
            assert!(d >= 250_000_000);
        }
    }

    /// T1(b) (outage 2026-08-27 §5.3): the predicate that replaces
    /// the msgs-based reset. Happy path: data moved for a healthy
    /// lifetime ⇒ reset. Failure modes: a rejection-only session (msgs
    /// moved, ticks did not) must keep escalating, and so must a session
    /// that moved data but died young (2026-09-26: HL snapshots, then
    /// the venue's drop, every ~1.2 s); a venue-quiet idle/staleness
    /// trip is rate-limited by construction and may reset.
    #[test]
    fn backoff_resets_only_on_a_healthy_session_or_quiet_trip() {
        let healthy = HEALTHY_SESSION_MIN_NS;
        // Data moved for a healthy lifetime ⇒ reset.
        assert!(should_reset_backoff(10, 3, healthy, false));
        assert!(should_reset_backoff(10, 3, u64::MAX, false));
        // Rejection-only session: ticks unchanged ⇒ keep escalating.
        assert!(!should_reset_backoff(3, 3, healthy, false));
        // The exact outage shape: rejection received every cycle,
        // never a tick — first cycle from zero included.
        assert!(!should_reset_backoff(0, 0, healthy, false));
        // The 2026-09-26 shape: snapshots moved, then the venue dropped
        // the socket ~1 s in ⇒ keep escalating, however many ticks.
        assert!(!should_reset_backoff(1_000, 3, 1_000_000_000, false));
        assert!(!should_reset_backoff(10, 3, healthy - 1, false));
        // Venue-quiet idle/staleness trip ⇒ reset (budget-limited).
        assert!(should_reset_backoff(3, 3, 0, true));
    }

    #[test]
    fn jitter_decorrelates_two_seeds() {
        let mut a = Backoff::default_for_ingress(1);
        let mut b = Backoff::default_for_ingress(2);
        // Same schedule bucket, different jitter draw.
        assert_ne!(a.next_delay_ns(), b.next_delay_ns());
    }
}
