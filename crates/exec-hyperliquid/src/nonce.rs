// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The action nonce.
//!
//! Hyperliquid requires a millisecond timestamp inside
//! `(T − 2 days, T + 1 day)`, unique, and **greater than the smallest
//! of the 100 most recent** for that signer. A monotonic counter
//! anchored on the wall clock satisfies all three trivially: it never
//! repeats, and it only ever moves forward.
//!
//! Single-writer by construction — it lives on the dispatcher worker
//! thread and is `&mut`, so there is no atomic on the hot path and no
//! question about ordering.
//!
//! **One agent wallet per sending process**, per the venue's own
//! guidance. If a second sender is ever added it gets its own agent
//! wallet; it never shares this counter, because two processes cannot
//! keep one monotonic sequence without coordination the hot path
//! cannot afford.

/// A monotonic millisecond nonce.
#[derive(Debug, Default, Copy, Clone)]
pub struct Nonce {
    last: u64,
}

impl Nonce {
    /// A fresh counter.
    #[inline(always)]
    #[must_use]
    pub const fn new() -> Self {
        Self { last: 0 }
    }

    /// The next nonce for `now_ms`.
    ///
    /// Takes the clock when it has moved and otherwise increments —
    /// so a burst inside one millisecond, or a clock that steps
    /// backwards, still yields a strictly increasing sequence.
    #[inline(always)]
    pub fn next(&mut self, now_ms: u64) -> u64 {
        self.last = if now_ms > self.last {
            now_ms
        } else {
            self.last.saturating_add(1)
        };
        self.last
    }

    /// The last nonce issued. Cold; the boot tell and `/state`.
    #[inline(always)]
    #[must_use]
    pub const fn last(&self) -> u64 {
        self.last
    }
}

/// The wall clock in milliseconds since the Unix epoch — the value the
/// venue window `(T − 2 days, T + 1 day)` is measured against.
///
/// Returns **0 when the clock is before the epoch**, which no caller
/// may feed to [`Nonce::next`] as a real time: `exchange::seal`
/// refuses to sign on a zero clock rather than emit a nonce the venue
/// would reject as ancient. One definition for the arm, the smoke and
/// the lifecycle gate, so all three read the same clock.
#[inline]
#[must_use]
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wall_clock_is_after_the_epoch_and_in_milliseconds() {
        let ms = now_ms();
        // 2020-01-01T00:00:00Z — any real clock on a build host is past it.
        assert!(ms > 1_577_836_800_000, "clock reads {ms} ms");
        // and not in nanoseconds / seconds by mistake (year 2100 bound).
        assert!(ms < 4_102_444_800_000, "clock reads {ms} ms");
    }

    #[test]
    fn it_takes_the_clock_when_the_clock_moves() {
        let mut n = Nonce::new();
        assert_eq!(n.next(1_000), 1_000);
        assert_eq!(n.next(1_001), 1_001);
        assert_eq!(n.next(2_000), 2_000);
    }

    #[test]
    fn a_burst_inside_one_millisecond_still_increases() {
        let mut n = Nonce::new();
        assert_eq!(n.next(1_000), 1_000);
        assert_eq!(n.next(1_000), 1_001);
        assert_eq!(n.next(1_000), 1_002);
        assert_eq!(n.next(1_000), 1_003);
    }

    /// NTP steps the clock backwards. A nonce that went backwards with
    /// it would be rejected by the venue and, worse, could repeat one
    /// the venue has already seen.
    #[test]
    fn a_backwards_clock_never_moves_the_nonce_backwards() {
        let mut n = Nonce::new();
        assert_eq!(n.next(5_000), 5_000);
        assert_eq!(n.next(1_000), 5_001, "clock stepped back; nonce did not");
        assert_eq!(n.next(1), 5_002);
        assert_eq!(n.next(5_003), 5_003, "and it recovers when the clock passes");
    }

    #[test]
    fn it_is_strictly_increasing_over_a_hostile_clock() {
        let mut n = Nonce::new();
        let mut prev = 0u64;
        let clocks = [10u64, 10, 9, 11, 11, 11, 1, 100, 100, 99];
        for c in clocks {
            let v = n.next(c);
            assert!(v > prev, "{v} !> {prev}");
            prev = v;
        }
        assert_eq!(n.last(), prev);
    }
}
