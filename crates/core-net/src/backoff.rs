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
        assert!(d < BACKOFF_BASE_NS, "post-reset delay {d} not in first bucket");
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

    #[test]
    fn jitter_decorrelates_two_seeds() {
        let mut a = Backoff::default_for_ingress(1);
        let mut b = Backoff::default_for_ingress(2);
        // Same schedule bucket, different jitter draw.
        assert_ne!(a.next_delay_ns(), b.next_delay_ns());
    }
}
