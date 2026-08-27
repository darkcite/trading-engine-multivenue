// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-latency
//!
//! Log-linear latency histogram. Preallocated buckets, lock-free
//! record path, percentile reads, periodic textual dump.
//!
//! ## Layout
//!
//! `LatencyTracker<const ROWS: usize>` owns `ROWS * 64` atomic
//! buckets. A sample `ns` lands in:
//!
//! ```text
//! if ns == 0:
//!   row = 0, col = 0
//! else:
//!   row = floor(log2(ns))                       (capped at ROWS - 1)
//!   shift = row.saturating_sub(6)                # 64 sub-buckets per row
//!   col = ((ns - (1 << row)) >> shift) & 63
//! bucket = row * 64 + col
//! ```
//!
//! 16 rows covers 0 ns ≤ ns < 2^16 ns = ~65 µs with 1/64 = 1.56 %
//! relative error in the upper half of each row. For the latency
//! profiles in this codebase (~µs to ms), `ROWS = 24` covers
//! 0 ns → ~16 ms with 1 KiB of bucket storage per tracker.
//!
//! ## Operations
//!
//! * [`LatencyTracker::record(ns)`] — atomic increment of the
//!   target bucket + atomic add of `ns` to `sum_ns`. Zero alloc.
//! * [`LatencyTracker::percentile(p)`] — snapshot the bucket
//!   counts, find the bucket containing the cumulative percentile.
//!   O(ROWS * 64).
//! * [`LatencyTracker::write_hgrm(out)`] — textual dump: one line
//!   per percentile of interest, plus per-row counts. Allocation
//!   is allowed here (boot-time tool path only).

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use core::sync::atomic::{AtomicU64, Ordering};
use std::io;

/// Number of sub-buckets per row. Pinned at 64 (one cache line of
/// `AtomicU64`s, eight bytes each).
pub const COLS_PER_ROW: usize = 64;

/// Default percentile set used by [`LatencyTracker::write_hgrm`].
pub const DEFAULT_PERCENTILES: &[f64] = &[0.50, 0.90, 0.99, 0.999, 0.9999];

/// A fixed-capacity log-linear histogram of nanosecond latencies.
///
/// `ROWS` is the number of binary-exponent rows (covers 0 ns up to
/// `2^ROWS - 1` ns; samples above land in the top row).
#[repr(C, align(64))]
pub struct LatencyTracker<const ROWS: usize> {
    buckets: [AtomicU64; ROWS_MAX_CHECK],
    sum_ns: AtomicU64,
    count: AtomicU64,
}

// Helper constant for the bucket array size at compile time. We
// can't directly use `ROWS * 64` in the array field type expression
// without `generic_const_exprs`, so we cap at a comfortable upper
// bound and use a runtime invariant. 32 rows is plenty (covers
// ~4.3 seconds).
const ROWS_MAX_CHECK: usize = 32 * COLS_PER_ROW;

impl<const ROWS: usize> LatencyTracker<ROWS> {
    /// Construct an empty tracker.
    ///
    /// # Panics
    ///
    /// Panics at construction if `ROWS > 32`. (We cap the bucket
    /// array at 32 rows so the type fits a fixed-size array.)
    pub fn new() -> Self {
        assert!(
            ROWS <= 32,
            "core-latency: ROWS must be <= 32 (got {ROWS}); bump ROWS_MAX_CHECK to widen"
        );
        Self {
            buckets: core::array::from_fn(|_| AtomicU64::new(0)),
            sum_ns: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Record one nanosecond sample. Zero-alloc; relaxed ordering.
    #[inline]
    pub fn record(&self, ns: u64) {
        let idx = bucket_index::<ROWS>(ns);
        // SAFETY-equivalent comment: bucket_index returns an
        // index < ROWS * COLS_PER_ROW <= ROWS_MAX_CHECK.
        let slot = &self.buckets[idx];
        slot.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Total samples recorded.
    #[inline]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Mean latency in ns. Returns 0 if no samples.
    #[inline]
    pub fn mean_ns(&self) -> u64 {
        let n = self.count();
        if n == 0 {
            0
        } else {
            self.sum_ns.load(Ordering::Relaxed) / n
        }
    }

    /// `p`-th percentile, in nanoseconds. `p` is a fraction in
    /// `[0.0, 1.0]`. Returns 0 if no samples.
    pub fn percentile(&self, p: f64) -> u64 {
        let n = self.count() as f64;
        if n == 0.0 {
            return 0;
        }
        let p = p.clamp(0.0, 1.0);
        let target = (n * p).ceil() as u64;
        let mut cumulative: u64 = 0;
        let limit = ROWS * COLS_PER_ROW;
        let mut i = 0;
        while i < limit {
            cumulative = cumulative.saturating_add(self.buckets[i].load(Ordering::Relaxed));
            if cumulative >= target {
                return bucket_upper_bound::<ROWS>(i);
            }
            i += 1;
        }
        // Fell through — should never happen because cumulative
        // converges to count. Return the top bucket boundary.
        bucket_upper_bound::<ROWS>(limit - 1)
    }

    /// Reset all counters. Boot-only; not zero-alloc-friendly
    /// against concurrent recorders, so caller is responsible for
    /// quiescence.
    pub fn reset(&self) {
        let limit = ROWS * COLS_PER_ROW;
        let mut i = 0;
        while i < limit {
            self.buckets[i].store(0, Ordering::Relaxed);
            i += 1;
        }
        self.sum_ns.store(0, Ordering::Relaxed);
        self.count.store(0, Ordering::Relaxed);
    }

    /// Write a textual histogram dump to `out`. Allocation
    /// allowed — this is not on the hot path.
    pub fn write_hgrm(&self, out: &mut dyn io::Write, label: &str) -> io::Result<()> {
        writeln!(out, "# {label}")?;
        writeln!(out, "# count={} mean_ns={}", self.count(), self.mean_ns())?;
        for &p in DEFAULT_PERCENTILES {
            writeln!(out, "p{:.4} {}", p, self.percentile(p))?;
        }
        writeln!(out, "# bucket counts (row col count upper_ns):")?;
        let limit = ROWS * COLS_PER_ROW;
        let mut i = 0;
        while i < limit {
            let c = self.buckets[i].load(Ordering::Relaxed);
            if c > 0 {
                let row = i / COLS_PER_ROW;
                let col = i % COLS_PER_ROW;
                writeln!(
                    out,
                    "{} {} {} {}",
                    row,
                    col,
                    c,
                    bucket_upper_bound::<ROWS>(i)
                )?;
            }
            i += 1;
        }
        Ok(())
    }
}

impl<const ROWS: usize> Default for LatencyTracker<ROWS> {
    fn default() -> Self {
        Self::new()
    }
}

// -----------------------------------------------------------------
// Bucket math
// -----------------------------------------------------------------

/// Compute the bucket index for `ns`, given `ROWS` rows.
#[inline]
pub fn bucket_index<const ROWS: usize>(ns: u64) -> usize {
    if ns == 0 {
        return 0;
    }
    // ilog2: position of the highest set bit. 64-bit.
    let row_full = 63 - ns.leading_zeros() as usize;
    let row = if row_full >= ROWS {
        ROWS - 1
    } else {
        row_full
    };
    let shift = row.saturating_sub(6);
    let col = (((ns - (1u64 << row)) >> shift) as usize) & (COLS_PER_ROW - 1);
    row * COLS_PER_ROW + col
}

/// Inclusive upper bound (in ns) of the bucket at `idx`.
#[inline]
fn bucket_upper_bound<const ROWS: usize>(idx: usize) -> u64 {
    let row = idx / COLS_PER_ROW;
    let col = idx % COLS_PER_ROW;
    let base = 1u64.checked_shl(row as u32).unwrap_or(u64::MAX);
    let shift = row.saturating_sub(6) as u32;
    // upper ns for this sub-bucket = base + ((col + 1) << shift) - 1
    let span = ((col as u64) + 1).checked_shl(shift).unwrap_or(u64::MAX);
    base.saturating_add(span).saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    type T = LatencyTracker<24>;

    #[test]
    fn record_then_count_matches() {
        let t = T::new();
        for _ in 0..7 {
            t.record(1_000);
        }
        assert_eq!(t.count(), 7);
    }

    #[test]
    fn bucket_index_zero_is_first_slot() {
        assert_eq!(bucket_index::<24>(0), 0);
    }

    #[test]
    fn bucket_index_one_is_first_row() {
        // 1 ns → row 0, col 0.
        assert_eq!(bucket_index::<24>(1), 0);
    }

    #[test]
    fn bucket_index_2_to_64_stays_in_low_rows() {
        // 2 ns → row 1 (ilog2 = 1) → idx in [COLS, 2*COLS).
        // 63 ns → row 5 (ilog2 = 5) → idx in [5*COLS, 6*COLS).
        let i2 = bucket_index::<24>(2);
        assert!(
            (COLS_PER_ROW..2 * COLS_PER_ROW).contains(&i2),
            "i2={i2}"
        );
        let i63 = bucket_index::<24>(63);
        assert!(
            (5 * COLS_PER_ROW..6 * COLS_PER_ROW).contains(&i63),
            "i63={i63}"
        );
    }

    #[test]
    fn bucket_index_caps_above_rows() {
        // 2^30 ns is well above 2^24 — should land in the top row.
        let idx = bucket_index::<24>(1u64 << 30);
        assert!(
            idx >= (24 - 1) * COLS_PER_ROW,
            "idx={idx}, expected ≥ {}",
            (24 - 1) * COLS_PER_ROW
        );
    }

    #[test]
    fn percentile_returns_zero_on_empty() {
        let t = T::new();
        assert_eq!(t.percentile(0.5), 0);
    }

    #[test]
    fn percentile_returns_recorded_bucket_for_single_value() {
        let t = T::new();
        for _ in 0..100 {
            t.record(1000);
        }
        let p50 = t.percentile(0.50);
        // 1000 ns sits in row 9 (2^9 = 512 ≤ 1000 < 1024). The
        // upper bound of that bucket is < 2^10 - 1 = 1023. We can't
        // get exactly 1000 back from the histogram; instead assert
        // the bound bracket is correct.
        assert!((512..=1023).contains(&p50), "p50={p50}");
    }

    #[test]
    fn percentile_separates_two_clusters() {
        let t = T::new();
        for _ in 0..50 {
            t.record(100); // fast cluster
        }
        for _ in 0..50 {
            t.record(1_000_000); // slow cluster
        }
        let p50 = t.percentile(0.50);
        let p99 = t.percentile(0.99);
        assert!(p50 < 1024, "p50 should fall in fast cluster: {p50}");
        assert!(p99 >= 1_000_000, "p99 should reach slow cluster: {p99}");
    }

    #[test]
    fn mean_ns_matches_arithmetic() {
        let t = T::new();
        t.record(100);
        t.record(200);
        t.record(300);
        // sum = 600, count = 3 → mean = 200.
        assert_eq!(t.mean_ns(), 200);
    }

    #[test]
    fn reset_clears_state() {
        let t = T::new();
        for i in 0..10 {
            t.record(i * 100);
        }
        t.reset();
        assert_eq!(t.count(), 0);
        assert_eq!(t.mean_ns(), 0);
        assert_eq!(t.percentile(0.99), 0);
    }

    #[test]
    fn write_hgrm_emits_header_and_buckets() {
        let t = T::new();
        for _ in 0..10 {
            t.record(1_000);
        }
        let mut buf = Vec::new();
        t.write_hgrm(&mut buf, "test-stage").unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains("# test-stage"));
        assert!(s.contains("# count=10"));
        assert!(s.contains("p0.5000"));
        assert!(s.contains("# bucket counts"));
    }

    #[test]
    #[should_panic(expected = "ROWS must be <= 32")]
    fn new_panics_above_max_rows() {
        let _t: LatencyTracker<33> = LatencyTracker::new();
    }
}
