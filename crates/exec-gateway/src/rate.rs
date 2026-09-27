// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A one-minute sliding sum (XMM XH4e): what a gateway spent of a venue's
//! per-minute allowance — Hyperliquid's IP weight, its WebSocket message
//! count — in sixty one-second buckets and their running total, fixed at
//! construction. Monotonic time; no allocation; a minute of silence
//! empties it.

/// Seconds the window covers: the venue's minute.
pub const RATE_WINDOW_S: u64 = 60;

const NS_PER_S: u64 = 1_000_000_000;
const BUCKETS: usize = RATE_WINDOW_S as usize;

/// One minute of spend, per second. 256 B, four cache lines — not `Copy`:
/// a stray by-value use is a compile error, never a silent 256 B copy.
#[repr(C, align(64))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateWindow {
    /// Index = second mod 60: what that second spent.
    buckets: [u32; BUCKETS],
    /// The (monotonic) second the newest bucket belongs to.
    head_s: u64,
    /// Σ `buckets`, kept exact by every write.
    sum: u64,
}

const _: () = assert!(core::mem::size_of::<RateWindow>() == 256);

impl Default for RateWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl RateWindow {
    /// An empty window.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buckets: [0; BUCKETS],
            head_s: 0,
            sum: 0,
        }
    }

    /// Move the window to `now_ns`: the seconds that left it leave the
    /// sum. A clock that did not reach a new second moves nothing.
    #[inline]
    pub fn advance(&mut self, now_ns: u64) {
        let s = now_ns / NS_PER_S;
        if s <= self.head_s {
            return;
        }
        if s - self.head_s >= RATE_WINDOW_S {
            self.buckets = [0; BUCKETS];
            self.sum = 0;
        } else {
            let mut t = self.head_s + 1;
            while t <= s {
                let i = (t % RATE_WINDOW_S) as usize;
                self.sum -= u64::from(self.buckets[i]);
                self.buckets[i] = 0;
                t += 1;
            }
        }
        self.head_s = s;
    }

    /// Spend `n` at `now_ns`.
    #[inline]
    pub fn add(&mut self, now_ns: u64, n: u32) {
        self.advance(now_ns);
        let i = (self.head_s % RATE_WINDOW_S) as usize;
        let before = self.buckets[i];
        self.buckets[i] = before.saturating_add(n);
        self.sum += u64::from(self.buckets[i] - before);
    }

    /// What the last minute spent, as of the last `advance` or `add`.
    #[inline]
    #[must_use]
    pub const fn sum(&self) -> u64 {
        self.sum
    }

    /// Whether the last minute spent at least `pct` % of `cap` a minute
    /// (a `cap` of 0 always has).
    #[inline]
    #[must_use]
    pub const fn at_least(&self, cap: u64, pct: u64) -> bool {
        self.sum.saturating_mul(100) >= cap.saturating_mul(pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = NS_PER_S;

    #[test]
    fn spend_leaves_the_window_a_minute_later() {
        let mut w = RateWindow::new();
        w.add(100 * S, 5);
        w.add(130 * S + 7, 3);
        assert_eq!(w.sum(), 8);
        w.advance(159 * S + 999_999_999);
        assert_eq!(w.sum(), 8, "second 100 is still in the minute");
        w.advance(160 * S);
        assert_eq!(w.sum(), 3, "second 100 left it");
        w.advance(190 * S);
        assert_eq!(w.sum(), 0);
        assert!(w.at_least(0, 80), "a cap of 0 is always spent");
    }

    #[test]
    fn a_minute_of_silence_empties_it_and_a_clock_that_stands_still_moves_nothing() {
        let mut w = RateWindow::new();
        w.add(10 * S, 7);
        w.advance(10 * S - 1);
        assert_eq!(w.sum(), 7);
        w.advance(70 * S);
        assert_eq!(w.sum(), 0);
        w.add(70 * S, 2);
        w.add(70 * S, 2);
        assert_eq!(w.sum(), 4);
        assert!(w.at_least(5, 80) && !w.at_least(6, 80));
    }

    /// Against a log of every spend: the sum is always what the last 60
    /// whole seconds spent.
    #[test]
    fn the_sum_matches_a_recount() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut w = RateWindow::new();
        let mut log: Vec<(u64, u32)> = Vec::new();
        let mut first = 0usize;
        let mut now = 1_000 * S;
        let mut step = 0;
        while step < 20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            now += x % (3 * S);
            let n = ((x >> 40) % 50) as u32;
            w.add(now, n);
            let head = now / S;
            log.push((head, n));
            while log[first].0 + RATE_WINDOW_S <= head {
                first += 1;
            }
            let mut want = 0u64;
            let mut i = first;
            while i < log.len() {
                want += u64::from(log[i].1);
                i += 1;
            }
            assert_eq!(w.sum(), want, "step {step}");
            step += 1;
        }
    }
}
