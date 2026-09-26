// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The signing nonce.
//!
//! The venue wants a nonce **unique per signer** and **inside time
//! bounds of its own command clock, in milliseconds** — measured on
//! mainnet 2026-09-26 (the HC9 dust): `ms × 1000` was refused as
//! "nonce … is outside time bounds for signer … (command_ts=<ms>)",
//! the signature itself accepted. So the nonce IS the wall clock in ms,
//! bumped by one when a second nonce falls in the same millisecond (or
//! the clock steps back). The bump can run ahead of the clock only by
//! the burst it absorbs — the venue's own rate limit (≤ 600 writes a
//! minute per wallet) keeps that far below a millisecond a write, so a
//! restart seconds later still starts past every nonce the previous boot
//! issued. The SDK carries it as a JS `Number`: below 2^53 (~1.8e12
//! today — no ceiling in sight).
//!
//! Single writer: it lives in the arm and is `&mut`.

/// The JS safe-integer ceiling the SDK's `Number(nonce)` imposes.
pub const NONCE_MAX: u64 = (1u64 << 53) - 1;

/// A strictly increasing nonce: the wall clock in ms.
#[derive(Debug, Default, Copy, Clone)]
pub struct HcNonce {
    last: u64,
}

impl HcNonce {
    /// A fresh counter.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self { last: 0 }
    }

    /// The next nonce for wall time `now_ms`, or `None` on a clock at
    /// or before the epoch (a nonce the venue would read as ancient is
    /// never signed) or past the safe-integer ceiling.
    #[inline]
    pub fn next(&mut self, now_ms: u64) -> Option<u64> {
        if now_ms == 0 {
            return None;
        }
        let n = if now_ms > self.last {
            now_ms
        } else {
            self.last.checked_add(1)?
        };
        if n > NONCE_MAX {
            return None;
        }
        self.last = n;
        Some(n)
    }

    /// The last nonce issued (boot tells, `/state`).
    #[inline]
    #[must_use]
    pub const fn last(&self) -> u64 {
        self.last
    }
}

/// Wall time in ms since the epoch; 0 before it (see [`HcNonce::next`]).
#[inline]
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_the_clock_in_ms_and_bumps_inside_a_millisecond() {
        let mut n = HcNonce::new();
        assert_eq!(n.next(1_000), Some(1_000));
        assert_eq!(n.next(1_000), Some(1_001), "a second nonce in the same ms");
        assert_eq!(n.next(1_000), Some(1_002));
        assert_eq!(n.next(1_005), Some(1_005), "the clock again once it passes the bumps");
    }

    #[test]
    fn a_backwards_clock_never_repeats_one() {
        let mut n = HcNonce::new();
        assert_eq!(n.next(5_000), Some(5_000));
        assert_eq!(n.next(4_000), Some(5_001));
        assert_eq!(n.next(6_000), Some(6_000));
    }

    #[test]
    fn a_zero_clock_and_the_ceiling_refuse() {
        let mut n = HcNonce::new();
        assert_eq!(n.next(0), None);
        assert_eq!(n.next(NONCE_MAX + 1), None);
        assert_eq!(n.last(), 0, "a refusal issues nothing");
        assert_eq!(n.next(NONCE_MAX), Some(NONCE_MAX));
        assert_eq!(n.next(NONCE_MAX), None, "no bump past the ceiling");
    }

    #[test]
    fn today_the_nonce_is_the_venues_command_clock() {
        // The mainnet refusal named `command_ts` in ms: the nonce must sit
        // on that scale, within a clock skew of it.
        let ms = now_ms();
        assert!(ms > 1_767_225_600_000, "the clock reads {ms}");
        let mut n = HcNonce::new();
        let v = n.next(ms).unwrap();
        assert_eq!(v, ms);
        assert!(v < 10_000_000_000_000, "ms, not µs: {v}");
    }
}
