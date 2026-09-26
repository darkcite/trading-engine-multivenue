// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-settle — the Hypercall settlement law, replicated (HC7)
//!
//! Hypercall settles an expiring series against the MEDIAN OF MEANS of
//! its underlying's oracle price over the last 30 minutes before expiry
//! `T`. The first measurement (2026-09-25 20:00Z, seven equity
//! underlyings, against the venue's posted `/settlement-payouts`) put the
//! plain mean up to 6.5 bp off on an underlying that trended through the
//! window, and median-of-means within 3.3 bp. This crate is that law and
//! the window that feeds it:
//!
//! * [`SettleWindow`] samples the oracle on a FIXED 1 s grid over
//!   `[T − 30 min, T]`, sample-and-hold: grid instant `g` takes the last
//!   price stamped at or before `g` (a price from before the window
//!   carries in). A quiet feed and a busy one yield the same 1 800
//!   points, so the estimate does not depend on how often the feed
//!   happened to publish — the measurement's residual looked like
//!   exactly that sampling effect (2 s REST polls).
//! * [`median_of_means`] — drop `⌊5 % · n⌋` values from each tail, cut
//!   the rest into `k = ⌊√n′⌋` buckets of `⌊n′ / k⌋` (the last takes the
//!   remainder), average each, and take the median of the averages (the
//!   mean of the middle two when `k` is even). The venue's docs do not
//!   say whether the buckets are cut from the SORTED trimmed values or
//!   in TIME order, and the first measurement did not separate them, so
//!   [`BucketOrder`] carries both until the HC7 gate chooses: median
//!   |err| ≤ 1 bp and max ≤ 3 bp over ≥ 20 consecutive expiries on ≥ 3
//!   underlyings, before any member books with it.
//!
//! **Doctrine:** stack-only and 0 B/op (bench gate 80). The window is one
//! fixed array; the law partitions a caller-owned scratch IN PLACE with
//! `select_nth_unstable` at each trim and bucket boundary — never a full
//! sort. Prices are ×1e6 integers; sums run in `i128`, so no input can
//! overflow them. Fixed-point means truncate: the law agrees with the
//! venue's float arithmetic to 1e-6 of a price unit.

#![forbid(unsafe_code)]

/// The settlement window: the last 30 minutes before expiry.
pub const SETTLE_WINDOW_MS: u64 = 30 * 60 * 1000;

/// The sampling grid's step.
pub const GRID_STEP_MS: u64 = 1000;

/// Grid points in one window (the last one sits exactly at `T`).
pub const GRID_POINTS: usize = (SETTLE_WINDOW_MS / GRID_STEP_MS) as usize;

/// Values dropped from EACH tail, per mille of the sample count.
pub const TRIM_PER_MILLE: usize = 50;

/// Bucket-count ceiling: `⌊√GRID_POINTS⌋` = 42 fits with room.
pub const MAX_BUCKETS: usize = 64;

const _: () = assert!(GRID_POINTS * GRID_STEP_MS as usize == SETTLE_WINDOW_MS as usize);
const _: () = assert!(MAX_BUCKETS * MAX_BUCKETS > GRID_POINTS);

/// How the trimmed values are cut into buckets (the open question the
/// HC7 gate settles — module doc).
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BucketOrder {
    /// Buckets of consecutive SORTED values (the robust-statistics
    /// textbook form: each bucket is a quantile band).
    Sorted = 0,
    /// Buckets of consecutive-in-TIME values, after dropping the values
    /// outside the trim band (each bucket is a sub-window).
    Time = 1,
}

/// The median-of-means of `samples` (×1e6 prices, TIME order) under
/// `order`. `scratch` is the caller's work area, at least
/// `samples.len()` long (its contents are overwritten). `None` when
/// there are no samples or `scratch` is too short.
#[must_use]
pub fn median_of_means(samples: &[i64], order: BucketOrder, scratch: &mut [i64]) -> Option<i64> {
    let n = samples.len();
    if n == 0 || scratch.len() < n {
        return None;
    }
    let c = n * TRIM_PER_MILLE / 1000;
    let work = &mut scratch[..n];
    // COPY: the grid (≤ GRID_POINTS × 8 B = 14.4 KiB) into the caller's
    // scratch, once per expiry — the partitions reorder it and the Time
    // variant must still read the samples in time order — rejected:
    // partitioning the window in place (it would destroy the time order).
    work.copy_from_slice(samples);
    // `c <= n / 20`, so `c <= n - c - 1` for every n >= 1: the band is
    // never empty.
    let n2 = match order {
        BucketOrder::Sorted => {
            // Isolate the trimmed middle: ranks [c, n - c).
            if c > 0 {
                work.select_nth_unstable(c);
                work[c..].select_nth_unstable(n - 2 * c - 1);
            }
            // COPY: the trimmed band to the front of the scratch (≤ 14.4 KiB,
            // once per expiry) so both variants bucket `work[..n2]` —
            // rejected: offsetting every bucket index by `c`.
            work.copy_within(c..n - c, 0);
            n - 2 * c
        }
        BucketOrder::Time => {
            // The band's edges are the c-th and (n - c - 1)-th smallest;
            // the values inside it keep their time order (ties at an
            // edge all stay — the measured law's filter).
            let lo = *work.select_nth_unstable(c).1;
            let hi = *work.select_nth_unstable(n - c - 1).1;
            let mut m = 0usize;
            let mut i = 0usize;
            while i < n {
                let x = samples[i];
                if lo <= x && x <= hi {
                    work[m] = x;
                    m += 1;
                }
                i += 1;
            }
            m
        }
    };
    let band = &mut work[..n2];
    let k = isqrt(n2).max(1);
    let size = n2 / k;
    debug_assert!(k <= MAX_BUCKETS && size >= 1);
    if order == BucketOrder::Sorted {
        // Bucket i must hold ranks [i·size, (i+1)·size): partition at
        // each boundary, left to right, on the not-yet-cut suffix.
        let mut b = 1usize;
        while b < k {
            let lo = (b - 1) * size;
            band[lo..].select_nth_unstable(size);
            b += 1;
        }
    }
    let mut means = [0i64; MAX_BUCKETS];
    let mut b = 0usize;
    while b < k {
        let lo = b * size;
        let hi = if b + 1 < k { lo + size } else { n2 };
        let mut sum = 0i128;
        let mut i = lo;
        while i < hi {
            sum += i128::from(band[i]);
            i += 1;
        }
        means[b] = (sum / (hi - lo) as i128) as i64;
        b += 1;
    }
    let means = &mut means[..k];
    let mid = k / 2;
    let upper = *means.select_nth_unstable(mid).1;
    if k % 2 == 1 {
        return Some(upper);
    }
    let lower = *means[..mid].select_nth_unstable(mid - 1).1;
    Some(((i128::from(lower) + i128::from(upper)) / 2) as i64)
}

/// `⌊√n⌋` (the MSRV predates `usize::isqrt`); n ≤ [`GRID_POINTS`], so
/// at most 42 steps, once per expiry.
const fn isqrt(n: usize) -> usize {
    let mut k = 0usize;
    while (k + 1) * (k + 1) <= n {
        k += 1;
    }
    k
}

/// Why [`SettleWindow::push`] refused a price.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PushRefused {
    /// Zero or negative (an oracle price is positive).
    NonPositive = 0,
    /// Stamped before the last accepted price (the feed is time-ordered;
    /// a late print is not re-graded into the past).
    OutOfOrder = 1,
    /// Stamped after `T`: the settlement is already fixed.
    AfterExpiry = 2,
}

/// The 1 s sample-and-hold grid over `[T − 30 min, T]` (module doc).
/// One per (underlying, expiry); boot- or member-owned, reused by
/// [`SettleWindow::reset`]. 14.4 KiB.
#[repr(C, align(64))]
#[derive(Clone)]
pub struct SettleWindow {
    /// Grid prices ×1e6, time order; `[..n]` is valid.
    samples: [i64; GRID_POINTS],
    /// `T`, unix ms.
    t_end_ms: u64,
    /// Stamp of the last accepted price.
    last_ts_ms: u64,
    /// The last accepted price (0 = none yet).
    last_px: i64,
    /// Next grid index to fill.
    next: u32,
    /// Grid points written — `next` minus the leading points no price
    /// had reached yet.
    n: u32,
}

impl SettleWindow {
    /// An empty window for expiry `t_end_ms` (unix ms).
    #[must_use]
    pub const fn new(t_end_ms: u64) -> Self {
        Self {
            samples: [0; GRID_POINTS],
            t_end_ms,
            last_ts_ms: 0,
            last_px: 0,
            next: 0,
            n: 0,
        }
    }

    /// Reuse the window for another expiry (no allocation).
    pub fn reset(&mut self, t_end_ms: u64) {
        self.t_end_ms = t_end_ms;
        self.last_ts_ms = 0;
        self.last_px = 0;
        self.next = 0;
        self.n = 0;
    }

    /// The expiry this window settles.
    #[inline]
    #[must_use]
    pub const fn t_end_ms(&self) -> u64 {
        self.t_end_ms
    }

    /// Grid instant `i`: `T − 30 min + (i + 1) s` (the last is `T`).
    #[inline]
    const fn grid_ms(&self, i: u32) -> u64 {
        self.t_end_ms - SETTLE_WINDOW_MS + (i as u64 + 1) * GRID_STEP_MS
    }

    /// Fill every grid point stamped before `ts_ms` with the held price.
    fn advance_to(&mut self, ts_ms: u64) {
        while (self.next as usize) < GRID_POINTS && self.grid_ms(self.next) < ts_ms {
            if self.last_px > 0 {
                self.samples[self.n as usize] = self.last_px;
                self.n += 1;
            }
            self.next += 1;
        }
    }

    /// One oracle print. Prints before the window only set the price
    /// that carries in; prints after `T` are refused.
    ///
    /// # Errors
    /// [`PushRefused`] — the window is unchanged.
    pub fn push(&mut self, ts_ms: u64, px_1e6: i64) -> Result<(), PushRefused> {
        if px_1e6 <= 0 {
            return Err(PushRefused::NonPositive);
        }
        if ts_ms < self.last_ts_ms {
            return Err(PushRefused::OutOfOrder);
        }
        if ts_ms > self.t_end_ms {
            return Err(PushRefused::AfterExpiry);
        }
        self.advance_to(ts_ms);
        self.last_px = px_1e6;
        self.last_ts_ms = ts_ms;
        Ok(())
    }

    /// Close the window at `T` (hold the last price through the end) and
    /// lend the grid. Idempotent.
    pub fn finish(&mut self) -> &[i64] {
        self.advance_to(self.t_end_ms + 1);
        &self.samples[..self.n as usize]
    }

    /// Grid points holding a price, of [`GRID_POINTS`] — the coverage a
    /// consumer judges before trusting the estimate (a feed that was
    /// down for most of the window yields few).
    #[inline]
    #[must_use]
    pub const fn points(&self) -> usize {
        self.n as usize
    }

    /// HC11b: the grid points written so far, in time order — what a state
    /// file keeps of an open window.
    #[inline]
    #[must_use]
    pub fn samples(&self) -> &[i64] {
        &self.samples[..self.n as usize]
    }

    /// HC11b: the next grid index to fill ([`GRID_POINTS`] once the window
    /// has run to `T`).
    #[inline]
    #[must_use]
    pub const fn next_index(&self) -> u32 {
        self.next
    }

    /// HC11b: the stamp of the last accepted print (0 = none yet).
    #[inline]
    #[must_use]
    pub const fn last_ts_ms(&self) -> u64 {
        self.last_ts_ms
    }

    /// HC11b: rebuild the window for expiry `t_end_ms` from what a state
    /// file kept of it — the grid points written, the next grid index and
    /// the last print's stamp.
    ///
    /// The held price does NOT carry: the process was down between the last
    /// print and the next, and sample-and-hold holds what was seen, never
    /// across an outage. The grid instants in between stay empty — they
    /// count against [`Self::points`], which is how a consumer sees the gap.
    ///
    /// # Errors
    ///
    /// What is inconsistent; the window is unchanged.
    pub fn restore(&mut self, t_end_ms: u64, next: u32, last_ts_ms: u64, samples: &[i64]) -> Result<(), &'static str> {
        let n = samples.len();
        if next as usize > GRID_POINTS || n > next as usize {
            return Err("settle window: more points than grid instants");
        }
        if t_end_ms < SETTLE_WINDOW_MS || last_ts_ms > t_end_ms {
            return Err("settle window: no window ends there, or a print after it");
        }
        let mut i = 0usize;
        while i < n {
            if samples[i] <= 0 {
                return Err("settle window: a non-positive price");
            }
            i += 1;
        }
        // COPY: ≤ GRID_POINTS × 8 B (14.4 KiB) of persisted grid, once per
        // window at boot — the window owns its fixed array — rejected: a
        // window borrowing the state file's parse (it outlives the boot).
        self.samples[..n].copy_from_slice(samples);
        self.t_end_ms = t_end_ms;
        self.last_ts_ms = last_ts_ms;
        self.last_px = 0;
        self.next = next;
        self.n = n as u32;
        Ok(())
    }

    /// Close the window and apply the law. `None` when no price reached
    /// the window.
    pub fn settle_1e6(&mut self, order: BucketOrder, scratch: &mut [i64; GRID_POINTS]) -> Option<i64> {
        self.finish();
        median_of_means(&self.samples[..self.n as usize], order, scratch)
    }
}

impl core::fmt::Debug for SettleWindow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SettleWindow")
            .field("t_end_ms", &self.t_end_ms)
            .field("points", &self.n)
            .field("next", &self.next)
            .field("last_px", &self.last_px)
            .finish()
    }
}

#[cfg(test)]
mod tests;
