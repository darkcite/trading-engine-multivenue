// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The signing nonce.
//!
//! The venue wants a nonce **unique per wallet**; gaps are allowed and
//! order is not required (plan §1.5). The SDK carries it as a JS
//! `Number`, so it must stay below 2^53. `ms × 1000` plus a per-ms
//! counter is both: ~1.8e15 today, a thousand nonces per millisecond,
//! and a restart — seconds later on the wall clock — starts past every
//! nonce the previous boot issued. A clock that steps backwards cannot
//! repeat one: the counter only moves forward within a process.
//!
//! Single writer: it lives in the arm and is `&mut`.

/// Nonces per millisecond before the counter borrows from the next.
pub const PER_MS: u64 = 1_000;

/// The JS safe-integer ceiling the SDK's `Number(nonce)` imposes.
pub const NONCE_MAX: u64 = (1u64 << 53) - 1;

/// A strictly increasing nonce anchored on the wall clock.
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
        let base = now_ms.checked_mul(PER_MS)?;
        let n = if base > self.last {
            base
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
    fn it_takes_the_clock_and_counts_inside_a_millisecond() {
        let mut n = HcNonce::new();
        assert_eq!(n.next(1_000), Some(1_000_000));
        assert_eq!(n.next(1_000), Some(1_000_001));
        assert_eq!(n.next(1_001), Some(1_001_000));
    }

    #[test]
    fn a_backwards_clock_never_repeats_one() {
        let mut n = HcNonce::new();
        assert_eq!(n.next(5_000), Some(5_000_000));
        assert_eq!(n.next(4_000), Some(5_000_001));
        assert_eq!(n.next(6_000), Some(6_000_000));
    }

    #[test]
    fn a_zero_clock_and_the_ceiling_refuse() {
        let mut n = HcNonce::new();
        assert_eq!(n.next(0), None);
        assert_eq!(n.next(NONCE_MAX / PER_MS + 1), None);
        assert_eq!(n.last(), 0, "a refusal issues nothing");
    }

    #[test]
    fn today_is_well_inside_the_safe_integer_range() {
        let ms = now_ms();
        assert!(ms > 1_767_225_600_000, "the clock reads {ms}");
        let mut n = HcNonce::new();
        let v = n.next(ms).unwrap();
        assert!(v < NONCE_MAX / 2, "{v}");
    }
}
