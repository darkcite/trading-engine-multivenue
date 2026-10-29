// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PM15-4 — the TWAP lock on Polymarket's Chainlink REPORTS.
//!
//! A Polymarket 15-minute market settles on Chainlink's 60-second TWAP
//! stream; the engine sees Chainlink's 1 Hz reports (RTDS
//! `crypto_prices_chainlink`), each stamped on its own whole second and
//! arriving ≈ 0.3–1.3 s after it. Where the HIP-4 grid ([`crate::late`])
//! is a LAST-VALUE grid on the arrival clock — a mark is in force from
//! when it arrived — this grid keeps each report ON ITS OWN SECOND, so the
//! observed part of the settlement average is the venue's samples
//! themselves, not the engine's view of them.
//!
//! ## The law (measured 2026-10-17, vault doc 36 v2 / build plan §6)
//!
//! - **Samples**: the settlement is the mean of the reports of the seconds
//!   `[T − 60, T − 1]` ([`PM_SAMPLE_LAST_OFFSET_S`] = 1): of the windows
//!   tried, it is the closest to the published `finalPrice` (0.1–15 ppm;
//!   the venue averages a denser stream than the 1 Hz reports).
//! - **The strike** is the previous window's settlement — `priceToBeat` of
//!   `w + 1` equals `finalPrice` of `w` exactly — so the same average over
//!   `[open − 60, open − 1]` estimates it ([`ReportGrid::mean`]) until the
//!   venue publishes it.
//! - **Observed** samples are the seconds up to the newest REPORT, not up
//!   to the query's second: a report not yet arrived is a sample not yet
//!   known, and it is modelled ([`report_fair`]).
//! - A second without a report inside the observed span takes the last
//!   report at most [`REPORT_GAP_FILL_S`] seconds before it (≈ 1.6 % of
//!   the RTDS seconds were missing on the 2026-10-17 recording); a longer
//!   hole holds the price ([`LateHold::ObservedStale`]).
//! - **σ** = the RMS of the 1-s returns between consecutive reports over the
//!   trailing [`SIGMA_WINDOW_S`], at least [`SIGMA_MIN_OBS`] of them.
//! - **The nowcast** `m_now = perp(now) · median(report(s)/perp(s))` over
//!   the [`BASIS_WINDOW_S`] seconds ending at the newest report, `perp(s)`
//!   being the fast venue's BBO mid IN FORCE at the instant `s` (arrival
//!   clock), fresh under [`PERP_FRESH_NS`].
//! - The tail — variance of the unobserved part, `z`, Φ — is
//!   [`crate::late::price_twap`], the HIP-4 law's own.
//!
//! ## Law (iii), the trapezoid (PMJ J12, F4, R-PMJ-S61) — a second law
//!
//! Measured against 9 000+ windows (vault `docs/research/pmj/f4/`): the
//! venue's 60-s TWAP is the TRAPEZOID of the reports over the labels
//! `[T − 62, T − 2]` — weights ½, 1, …, 1, ½ (sum 60) — about 2 s behind the
//! labels the first law averages. `priceToBeat(w)` equals `finalPrice(w − 1)`
//! exactly, so the same trapezoid over `[open − 62, open − 2]` is the strike
//! ([`ReportGrid::trap`]); it matches the published strike at 5.7 ppm RMS on
//! days it was not fitted on, against 19.1 for [`ReportGrid::mean`].
//! [`report_fair_trap`] prices on those samples with the trapezoid's own
//! tail variance, plus a residual for what the estimate still misses: σ_res
//! = max(ρ · σ, floor), ρ fitted against the raw σ (F4 §11). The first law
//! ([`report_fair`], [`ReportGrid::mean`]) is unchanged, value for value; a
//! member picks one law at boot.
//!
//! Integer throughout, no allocation after [`ReportGrid::new`], no float,
//! while-index loops.

use core_regime::math::{floor_div, isqrt_i128};

use crate::late::{
    price_twap, LateFair, LateHold, LateLut, BASIS_WINDOW_S, LATE_Z_CLAMP_1E6, PERP_FRESH_NS,
    R2_RING, SEC_NS, SHORT_RING, SIGMA_MAX_1E9, SIGMA_MIN_OBS, SIGMA_WINDOW_S, TWAP_SAMPLES,
};

/// The last settlement sample's second is `T − 1` (module docs).
pub const PM_SAMPLE_LAST_OFFSET_S: u64 = 1;

/// A report-less second inside the observed span takes the last report
/// at most this many seconds older.
pub const REPORT_GAP_FILL_S: u64 = 3;

/// The newest report must be younger than this at the query, by its own
/// second.
pub const REPORT_FRESH_NS: u64 = 5_000_000_000;

/// The perp-mid ring's depth, seconds (a power of two over the basis
/// window and the freshness).
const PERP_RING: usize = 64;

/// A member's strike law: the first — the mean of `[open − 60, open − 1]`,
/// [`report_fair`] (PM15-4).
pub const STRIKE_LAW_MEAN60: u8 = 0;
/// Law (iii) — the trapezoid of `[open − 62, open − 2]`, [`report_fair_trap`]
/// (PMJ J12).
pub const STRIKE_LAW_TRAP62: u8 = 1;
/// Law (iii)'s grid keeps the reports ×1e9: v1 (×1e12) over this.
pub const FINE_PER_V1: i64 = 1_000;
/// … and a ×1e6 value (the perp's BBO) times this.
pub const FINE_PER_1E6: i64 = 1_000;

/// The report ring's depth, seconds: a power of two over law (iii)'s 61
/// labels plus the gap fill and the lag of the newest report (J12; the
/// first law reads only its old [`REPORT_HORIZON_S`] of it).
pub const REPORT_RING: usize = 128;

/// How far behind the newest report the first law reads a second — the
/// horizon it had on a 64-s ring, kept so its results are unchanged.
pub const REPORT_HORIZON_S: u64 = SHORT_RING as u64 - REPORT_GAP_FILL_S;

/// How far behind the newest report law (iii) reads a second.
pub const TRAP_HORIZON_S: u64 = REPORT_RING as u64 - REPORT_GAP_FILL_S;

/// Law (iii)'s first settlement label is `T − 62`, its last `T − 2`.
pub const PM_TRAP_FIRST_OFFSET_S: u64 = 62;
/// Law (iii)'s last settlement label offset (module docs).
pub const PM_TRAP_LAST_OFFSET_S: u64 = 2;
/// Law (iii)'s labels: `[T − 62, T − 2]`.
pub const TRAP_LABELS: u64 = PM_TRAP_FIRST_OFFSET_S - PM_TRAP_LAST_OFFSET_S + 1;
/// Twice the trapezoid's weight sum: the weights run 1, 2, …, 2, 1.
const TRAP_W2_SUM: i128 = 2 * TWAP_SAMPLES as i128;

/// Law (iii)'s residual: ρ ×1e3 of the raw σ, with a floor in ppm.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrapResidual {
    /// ρ ×1e3 (F4 §11: 130).
    pub ratio_1e3: i64,
    /// The floor, ppm of the strike.
    pub min_ppm: i64,
}

impl TrapResidual {
    /// The residual, relative ×1e9, at σ ×1e9 per √s.
    #[inline(always)]
    #[must_use]
    pub const fn rel_1e9(self, sigma_1e9: i64) -> i64 {
        let v = floor_div_i64(self.ratio_1e3.saturating_mul(sigma_1e9), 1_000);
        let f = self.min_ppm.saturating_mul(1_000);
        if v > f { v } else { f }
    }

    /// True when the floor, not the ratio, sets the residual.
    #[inline(always)]
    #[must_use]
    pub const fn floor_binds(self, sigma_1e9: i64) -> bool {
        floor_div_i64(self.ratio_1e3.saturating_mul(sigma_1e9), 1_000)
            < self.min_ppm.saturating_mul(1_000)
    }
}

#[inline(always)]
const fn floor_div_i64(a: i64, b: i64) -> i64 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) { q - 1 } else { q }
}

/// One asset's Chainlink reports and fast-venue mids (module docs).
#[repr(C, align(64))]
#[derive(Clone, Debug)]
pub struct ReportGrid {
    /// `report(s)` by second (×1e6 under the first law, ×1e9 under law
    /// (iii): the member's scale); `mark_sec` says which second a slot holds.
    mark: [i64; REPORT_RING],
    mark_sec: [u64; REPORT_RING],
    /// The perp mid ×2 (×1e6) in force at the instant `s`, by second.
    perp: [i64; PERP_RING],
    perp_sec: [u64; PERP_RING],
    /// `r(s)²` (`r ×1e9`) of the return into second `s`.
    r2: [i64; R2_RING],
    /// Validity bits of `r2`.
    r_ok: [u64; R2_RING / 64],
    /// Σ `r2` over the valid returns inside the σ window.
    sum_r2: i128,
    /// Valid returns inside the σ window.
    n_r: u64,
    /// The newest report's second (0 = none) and value — ×1e6 under the
    /// first law, ×1e9 under law (iii) (the member's scale).
    last_sec: u64,
    last_1e6: i64,
    /// The perp BBO in force, `bid + ask` ×1e6, and its arrival wall.
    cur_perp_x2_1e6: i64,
    cur_perp_wall: u64,
}

impl Default for ReportGrid {
    fn default() -> Self {
        Self::new()
    }
}

impl ReportGrid {
    /// An empty grid — boot only (≈ 10 KiB; the member boxes them).
    #[must_use]
    pub fn new() -> Self {
        Self {
            mark: [0; REPORT_RING],
            mark_sec: [0; REPORT_RING],
            perp: [0; PERP_RING],
            perp_sec: [0; PERP_RING],
            r2: [0; R2_RING],
            r_ok: [0; R2_RING / 64],
            sum_r2: 0,
            n_r: 0,
            last_sec: 0,
            last_1e6: 0,
            cur_perp_x2_1e6: 0,
            cur_perp_wall: 0,
        }
    }

    /// The newest report's second (0 = none yet).
    #[inline(always)]
    #[must_use]
    pub const fn last_sec(&self) -> u64 {
        self.last_sec
    }

    /// The newest report ×1e6 (0 = none yet).
    #[inline(always)]
    #[must_use]
    pub const fn last_1e6(&self) -> i64 {
        self.last_1e6
    }

    /// One report: second `sec`, price ×1e6. Older or repeated seconds
    /// are ignored (the newest report is the clock).
    pub fn on_report(&mut self, sec: u64, px_1e6: i64) {
        if px_1e6 <= 0 || sec == 0 || sec <= self.last_sec {
            return;
        }
        if self.last_sec != 0 && sec - self.last_sec > R2_RING as u64 {
            self.r2 = [0; R2_RING];
            self.r_ok = [0; R2_RING / 64];
            self.sum_r2 = 0;
            self.n_r = 0;
        } else if self.last_sec != 0 {
            // Every second from the last report to this one leaves its
            // window edge and its own slot behind.
            let mut s = self.last_sec + 1;
            while s <= sec {
                if s >= SIGMA_WINDOW_S {
                    self.drop_return((s - SIGMA_WINDOW_S) % R2_RING as u64);
                }
                self.drop_return(s % R2_RING as u64);
                s += 1;
            }
        }
        // The 1-s return into `sec`: only from a report of `sec − 1`.
        if self.last_sec != 0 && self.last_sec + 1 == sec {
            let prev = self.last_1e6;
            let r = floor_div((px_1e6 - prev) as i128 * 1_000_000_000, prev as i128);
            let sq = (r * r).min(i128::from(i64::MAX)) as i64;
            let j = (sec % R2_RING as u64) as usize;
            self.r2[j] = sq;
            self.r_ok[j / 64] |= 1u64 << (j % 64);
            self.sum_r2 += i128::from(sq);
            self.n_r += 1;
        }
        let i = (sec % REPORT_RING as u64) as usize;
        self.mark[i] = px_1e6;
        self.mark_sec[i] = sec;
        self.last_sec = sec;
        self.last_1e6 = px_1e6;
    }

    #[inline]
    fn drop_return(&mut self, slot: u64) {
        let o = slot as usize % R2_RING;
        let bit = 1u64 << (o % 64);
        if self.r_ok[o / 64] & bit != 0 {
            self.sum_r2 -= i128::from(self.r2[o]);
            self.n_r -= 1;
            self.r_ok[o / 64] &= !bit;
        }
        self.r2[o] = 0;
    }

    /// One fast-venue BBO at arrival wall `wall_ns`, as `bid + ask` ×1e6:
    /// in force from the next whole second's instant on.
    pub fn on_perp(&mut self, wall_ns: u64, bid_plus_ask_1e6: i64) {
        if bid_plus_ask_1e6 <= 0 {
            return;
        }
        let s = wall_ns / SEC_NS + 1;
        let i = (s % PERP_RING as u64) as usize;
        self.perp[i] = bid_plus_ask_1e6;
        self.perp_sec[i] = s;
        self.cur_perp_x2_1e6 = bid_plus_ask_1e6;
        self.cur_perp_wall = wall_ns;
    }

    /// The report of second `s`, a gap filled from at most
    /// [`REPORT_GAP_FILL_S`] seconds before it; `None` beyond, or when the
    /// ring no longer holds `s`.
    #[inline]
    #[must_use]
    pub fn report_at(&self, s: u64) -> Option<i64> {
        self.report_within(s, REPORT_HORIZON_S)
    }

    /// [`Self::report_at`] at most `horizon` seconds behind the newest report
    /// (`horizon` ≤ [`TRAP_HORIZON_S`]).
    #[inline]
    #[must_use]
    fn report_within(&self, s: u64, horizon: u64) -> Option<i64> {
        debug_assert!(horizon <= TRAP_HORIZON_S);
        if s > self.last_sec || self.last_sec - s >= horizon {
            return None;
        }
        let mut back = 0u64;
        while back <= REPORT_GAP_FILL_S && back <= s {
            let t = s - back;
            let i = (t % REPORT_RING as u64) as usize;
            if self.mark_sec[i] == t && self.mark[i] > 0 {
                return Some(self.mark[i]);
            }
            back += 1;
        }
        None
    }

    /// The perp mid ×2 in force at the instant `s` (fresh under
    /// [`PERP_FRESH_NS`]).
    #[inline]
    fn perp_at(&self, s: u64) -> Option<i64> {
        let max_back = PERP_FRESH_NS / SEC_NS;
        let mut back = 0u64;
        while back < max_back && back < s {
            let t = s - back;
            let i = (t % PERP_RING as u64) as usize;
            if self.perp_sec[i] == t && self.perp[i] > 0 {
                return Some(self.perp[i]);
            }
            back += 1;
        }
        None
    }

    /// The mean of the reports of seconds `[first, last]` ×1e6 (the strike
    /// estimate over `[open − 60, open − 1]`), gaps filled as
    /// [`Self::report_at`]; `None` when a second is missing or outside the
    /// ring.
    #[must_use]
    pub fn mean(&self, first: u64, last: u64) -> Option<i64> {
        if last < first {
            return None;
        }
        let mut sum: i128 = 0;
        let mut s = first;
        while s <= last {
            sum += i128::from(self.report_at(s)?);
            s += 1;
        }
        Some(floor_div(sum, i128::from(last - first + 1)) as i64)
    }

    /// Law (iii)'s strike: the trapezoid of the reports of the labels
    /// `[open − 62, open − 2]` (weights ½, 1, …, 1, ½ over 60), floored,
    /// gaps filled as [`Self::report_at`] within [`TRAP_HORIZON_S`]; `None`
    /// when a label is missing or `open` is too early.
    #[must_use]
    pub fn trap(&self, open_s: u64) -> Option<i64> {
        if open_s < PM_TRAP_FIRST_OFFSET_S {
            return None;
        }
        let first = open_s - PM_TRAP_FIRST_OFFSET_S;
        let last = open_s - PM_TRAP_LAST_OFFSET_S;
        let mut sum: i128 = 0;
        let mut s = first;
        while s <= last {
            let v = i128::from(self.report_within(s, TRAP_HORIZON_S)?);
            sum += if s == first || s == last { v } else { 2 * v };
            s += 1;
        }
        Some(floor_div(sum, TRAP_W2_SUM) as i64)
    }

    /// σ ×1e9 per √s, or `None` under [`SIGMA_MIN_OBS`] valid returns.
    #[inline]
    #[must_use]
    pub fn sigma_1e9(&self) -> Option<i64> {
        if self.n_r < u64::from(SIGMA_MIN_OBS) {
            return None;
        }
        let v = isqrt_i128(floor_div(self.sum_r2, i128::from(self.n_r)));
        Some(v.min(SIGMA_MAX_1E9))
    }

    /// The report/perp basis ×1e9: the median of `report(s)·2/perp(s)` over
    /// the [`BASIS_WINDOW_S`] seconds ending at the newest report (an even
    /// count takes the floored mean of the middle two).
    #[must_use]
    pub fn basis_1e9(&self) -> Option<i64> {
        let mut buf = [0i64; SHORT_RING];
        let mut k = 0usize;
        let mut back = 0u64;
        while back < BASIS_WINDOW_S && back < self.last_sec {
            let s = self.last_sec - back;
            back += 1;
            let i = (s % REPORT_RING as u64) as usize;
            if self.mark_sec[i] != s || self.mark[i] <= 0 {
                continue;
            }
            if let Some(p) = self.perp_at(s) {
                buf[k] = floor_div(i128::from(self.mark[i]) * 2_000_000_000, i128::from(p)) as i64;
                k += 1;
            }
        }
        if k == 0 {
            return None;
        }
        let mut i = 1usize;
        while i < k {
            let x = buf[i];
            let mut j = i;
            while j > 0 && buf[j - 1] > x {
                buf[j] = buf[j - 1];
                j -= 1;
            }
            buf[j] = x;
            i += 1;
        }
        Some(if k % 2 == 1 {
            buf[k / 2]
        } else {
            floor_div(i128::from(buf[k / 2 - 1]) + i128::from(buf[k / 2]), 2) as i64
        })
    }
}

/// The price of a Polymarket 15-minute binary struck at `strike_1e6`
/// whose window ends at `end_ns` (a whole second), seen at `wall_ns`, σ
/// scaled by `k_1e3` (module docs).
///
/// # Errors
/// [`LateHold`] — the input that is missing.
pub fn report_fair(
    grid: &ReportGrid,
    lut: &LateLut,
    strike_1e6: i64,
    end_ns: u64,
    wall_ns: u64,
    k_1e3: i64,
) -> Result<LateFair, LateHold> {
    let end_s = end_ns / SEC_NS;
    if strike_1e6 <= 0
        || k_1e3 <= 0
        || end_s < TWAP_SAMPLES + PM_SAMPLE_LAST_OFFSET_S
        || wall_ns > end_ns
    {
        return Err(LateHold::Shape);
    }
    let (m_now, sigma, basis) = nowcast(grid, wall_ns)?;
    let last = end_s - PM_SAMPLE_LAST_OFFSET_S;
    let first = last + 1 - TWAP_SAMPLES;
    let obs_last = if grid.last_sec < last {
        grid.last_sec
    } else {
        last
    };
    let n_obs = if obs_last < first {
        0
    } else {
        obs_last - first + 1
    };
    let mut obs: i128 = 0;
    let mut s = first;
    while s < first + n_obs {
        let Some(v) = grid.report_at(s) else {
            return Err(LateHold::ObservedStale);
        };
        obs += i128::from(v);
        s += 1;
    }
    let first_fut = if obs_last + 1 > first {
        obs_last + 1
    } else {
        first
    };
    let fut_ns = i128::from(first_fut * SEC_NS);
    let a_ns = if fut_ns > i128::from(wall_ns) {
        fut_ns - i128::from(wall_ns)
    } else {
        0
    };
    Ok(price_twap(
        lut, obs, n_obs, a_ns, m_now, sigma, basis, k_1e3, strike_1e6,
    ))
}

/// The nowcast and σ both laws share, after the same holds: `(m_now, σ,
/// basis)`, `m_now` in the grid's scale.
#[inline]
fn nowcast(grid: &ReportGrid, wall_ns: u64) -> Result<(i128, i64, i64), LateHold> {
    if grid.last_sec == 0 || grid.last_sec.saturating_mul(SEC_NS) > wall_ns {
        return Err(LateHold::MarkStale);
    }
    if wall_ns - grid.last_sec * SEC_NS >= REPORT_FRESH_NS {
        return Err(LateHold::MarkStale);
    }
    if grid.cur_perp_x2_1e6 <= 0
        || grid.cur_perp_wall > wall_ns
        || wall_ns - grid.cur_perp_wall >= PERP_FRESH_NS
    {
        return Err(LateHold::NoNowcast);
    }
    let Some(sigma) = grid.sigma_1e9() else {
        return Err(LateHold::NoSigma);
    };
    let Some(basis) = grid.basis_1e9() else {
        return Err(LateHold::NoNowcast);
    };
    let m_now = floor_div(
        i128::from(grid.cur_perp_x2_1e6) * i128::from(basis),
        2_000_000_000,
    );
    if m_now <= 0 || m_now > i128::from(i64::MAX) {
        return Err(LateHold::Shape);
    }
    Ok((m_now, sigma, basis))
}

/// `4 · Σ_ij w_i w_j min(t_i, t_j)` in ns, for `n` unobserved labels at
/// `t_k = a + k` s (`k < n`), weights 1 except the last's ½ (F4 §7 (a); the
/// closed form is pinned against the double sum in the tests).
#[inline]
#[must_use]
pub fn trap_tail_var4_ns(n: i128, a_ns: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    let m = n - 1;
    let s9 = 1_000_000_000i128;
    4 * (a_ns * m * m + (m * (m - 1) * (2 * m - 1) / 6) * s9 + m * a_ns + (m * (m - 1) / 2) * s9)
        + a_ns
        + m * s9
}

/// Law (iii)'s price of a Polymarket rolling binary struck at `strike`
/// (the grid's scale) whose window ends at `end_ns`, seen at `wall_ns`, σ
/// scaled by `k_1e3`, with the residual `res` (module docs). The holds are
/// [`report_fair`]'s. Returns the fair value and whether the residual's
/// floor bound.
///
/// # Errors
/// [`LateHold`] — the input that is missing.
pub fn report_fair_trap(
    grid: &ReportGrid,
    lut: &LateLut,
    strike: i64,
    end_ns: u64,
    wall_ns: u64,
    k_1e3: i64,
    res: TrapResidual,
) -> Result<(LateFair, bool), LateHold> {
    let end_s = end_ns / SEC_NS;
    if strike <= 0
        || k_1e3 <= 0
        || res.ratio_1e3 < 0
        || res.min_ppm < 0
        || end_s < PM_TRAP_FIRST_OFFSET_S + 1
        || wall_ns > end_ns
    {
        return Err(LateHold::Shape);
    }
    let (m_now, sigma, basis) = nowcast(grid, wall_ns)?;
    let first = end_s - PM_TRAP_FIRST_OFFSET_S;
    let last = end_s - PM_TRAP_LAST_OFFSET_S;
    let obs_last = if grid.last_sec < last { grid.last_sec } else { last };
    // The first label must be observed: the tail's closed form assumes it.
    // A late window (τ ≤ 20 s today) always has it; a window queried ~60 s
    // or more before its end may not, and holds here as `Shape`.
    if obs_last < first {
        return Err(LateHold::Shape);
    }
    let mut obs2: i128 = 0;
    let mut s = first;
    while s <= obs_last {
        let Some(v) = grid.report_within(s, TRAP_HORIZON_S) else {
            return Err(LateHold::ObservedStale);
        };
        let v = i128::from(v);
        obs2 += if s == first || s == last { v } else { 2 * v };
        s += 1;
    }
    let n = i128::from(last - obs_last);
    let first_fut_ns = i128::from((obs_last + 1) * SEC_NS);
    let a_ns = if first_fut_ns > i128::from(wall_ns) {
        first_fut_ns - i128::from(wall_ns)
    } else {
        0
    };
    // Var of the TWAP (weights /60) in s ×1e9: Σ w w min / 3600.
    let var_1e9 = floor_div(trap_tail_var4_ns(n, a_ns), 4 * 3_600);
    let sqrt_v_1e9 = i128::from(isqrt_i128(var_1e9 * 1_000_000_000));
    let sd = floor_div(
        i128::from(k_1e3) * i128::from(sigma) * m_now * sqrt_v_1e9,
        1_000_000_000_000_000_000_000,
    );
    let rel = res.rel_1e9(sigma);
    let r = floor_div(i128::from(rel) * i128::from(strike), 1_000_000_000);
    let sd_res = i128::from(isqrt_i128(sd * sd + r * r));
    // 120 · (E[S] − K): the ×2 weights' expected sum against 120 · K.
    let num2 = obs2 + (2 * n - 1).max(0) * m_now - TRAP_W2_SUM * i128::from(strike);
    let z = if sd_res > 0 {
        floor_div(num2 * 1_000_000, TRAP_W2_SUM * sd_res)
            .clamp(-i128::from(LATE_Z_CLAMP_1E6), i128::from(LATE_Z_CLAMP_1E6)) as i64
    } else if num2 >= 0 {
        LATE_Z_CLAMP_1E6
    } else {
        -LATE_Z_CLAMP_1E6
    };
    Ok((
        LateFair {
            p_yes_1e6: lut.p_of_z(z),
            z_1e6: z,
            sigma_1e9: sigma,
            basis_1e9: basis,
            m_now_1e6: m_now as i64,
            n_obs: TRAP_LABELS - n as u64,
        },
        res.floor_binds(sigma),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::late::{LATE_PHI_POINTS, LATE_PHI_STEP_1E6};

    const S: u64 = SEC_NS;

    fn ramp() -> LateLut {
        let mut l = LateLut::flat();
        let mut i = 0usize;
        while i < LATE_PHI_POINTS {
            let z = i as i64 * LATE_PHI_STEP_1E6;
            l.phi[i] = (500_000 + (z.min(4_000_000) / 8)) as u32;
            i += 1;
        }
        l
    }

    /// Reports every second `t0 .. t0 + secs` alternating ±`step` around
    /// `base`, each arriving 1.3 s late with a perp BBO equal to it.
    fn fed(t0: u64, secs: u64, base: i64, step: i64) -> ReportGrid {
        let mut g = ReportGrid::new();
        let mut s = 0u64;
        while s < secs {
            let m = base + if s % 2 == 0 { step } else { -step };
            g.on_perp((t0 + s) * S - 200_000_000, 2 * m);
            g.on_report(t0 + s, m);
            s += 1;
        }
        g
    }

    #[test]
    fn reports_stay_on_their_second_and_gaps_fill_three_back() {
        let mut g = ReportGrid::new();
        g.on_report(100, 50_000_000_000);
        g.on_report(101, 50_001_000_000);
        g.on_report(101, 1);
        g.on_report(99, 1);
        assert_eq!(
            g.last_sec(),
            101,
            "repeats and older seconds change nothing"
        );
        g.on_report(105, 50_002_000_000);
        assert_eq!(g.report_at(101), Some(50_001_000_000));
        assert_eq!(g.report_at(104), Some(50_001_000_000), "three back");
        assert_eq!(g.report_at(105), Some(50_002_000_000));
        assert_eq!(g.report_at(106), None, "not reported yet");
        g.on_report(110, 50_003_000_000);
        assert_eq!(g.report_at(109), None, "four back is a hole");
        assert_eq!(g.mean(100, 101), Some(50_000_500_000));
        assert_eq!(g.mean(100, 109), None);
    }

    #[test]
    fn sigma_counts_only_consecutive_reports() {
        let g = fed(1_000, 200, 50_000_000_000, 5_000_000);
        assert_eq!(g.sigma_1e9(), None);
        let g = fed(1_000, 500, 50_000_000_000, 5_000_000);
        let s = g.sigma_1e9().expect("over 300 returns");
        assert!((199_000..=201_000).contains(&s), "sigma {s}");
        // Every other second missing: no 1-s return at all.
        let mut h = ReportGrid::new();
        let mut t = 0u64;
        while t < 1_000 {
            h.on_report(5_000 + 2 * t, 50_000_000_000 + (t as i64 % 2) * 1_000_000);
            t += 1;
        }
        assert_eq!(h.sigma_1e9(), None);
        // σ forgets what leaves its window.
        let mut g = fed(1_000, 500, 50_000_000_000, 5_000_000);
        g.on_report(3_000, 50_000_000_000);
        assert_eq!(g.sigma_1e9(), None, "a long gap empties the window");
    }

    #[test]
    fn the_basis_reads_the_perp_in_force_at_each_reported_second() {
        let mut g = ReportGrid::new();
        let mut s = 0u64;
        while s < 70 {
            // The perp sits 0.1 % over the reports.
            g.on_perp((2_000 + s) * S - 400_000_000, 2 * 50_050_000_000);
            g.on_report(2_000 + s, 50_000_000_000);
            s += 1;
        }
        let b = g.basis_1e9().expect("60 pairs");
        assert_eq!(
            b,
            floor_div(50_000_000_000i128 * 2_000_000_000, 2 * 50_050_000_000) as i64
        );
    }

    #[test]
    fn a_decided_window_prices_near_certainty_and_an_open_one_near_half() {
        // Window ends at 3 600 s; reports through 3 580 (20 s to go), the
        // query 1.3 s after the newest report.
        let g = fed(2_970, 611, 50_000_000_000, 1_000_000);
        assert_eq!(g.last_sec(), 3_580);
        let lut = ramp();
        let end = 3_600 * S;
        let wall = 3_580 * S + 1_300_000_000;
        let f = report_fair(&g, &lut, 49_950_000_000, end, wall, 1_550).expect("priced");
        // Samples 3 540 … 3 599; observed through 3 580.
        assert_eq!(f.n_obs, 41);
        assert!(f.p_yes_1e6 > 990_000, "p {}", f.p_yes_1e6);
        let f = report_fair(&g, &lut, 50_000_000_000, end, wall, 1_550).expect("priced");
        assert!(
            (450_000..=550_000).contains(&f.p_yes_1e6),
            "p {}",
            f.p_yes_1e6
        );
        // The strike estimate over [open − 60, open − 1] of a flat series.
        let m = g.mean(3_521, 3_580).expect("in the ring");
        assert_eq!(m, 50_000_000_000);
    }

    #[test]
    fn every_missing_input_holds() {
        let lut = ramp();
        let end = 3_600 * S;
        let g = fed(2_970, 611, 50_000_000_000, 1_000_000);
        let wall = 3_580 * S + 1_300_000_000;
        assert_eq!(
            report_fair(&g, &lut, 0, end, wall, 1_550),
            Err(LateHold::Shape)
        );
        assert_eq!(
            report_fair(&g, &lut, 1, end, end + 1, 1_550),
            Err(LateHold::Shape)
        );
        assert_eq!(
            report_fair(&g, &lut, 50_000_000_000, end, 3_585 * S + 1, 1_550),
            Err(LateHold::MarkStale),
            "the newest report is 5 s old"
        );
        assert_eq!(
            report_fair(&ReportGrid::new(), &lut, 1, end, wall, 1_550),
            Err(LateHold::MarkStale)
        );
        let short = fed(3_300, 281, 50_000_000_000, 1_000_000);
        assert_eq!(
            report_fair(&short, &lut, 50_000_000_000, end, wall, 1_550),
            Err(LateHold::NoSigma)
        );
        // A perp gone quiet: no nowcast.
        let mut q = fed(2_970, 611, 50_000_000_000, 1_000_000);
        q.on_report(3_581, 50_000_000_000);
        assert_eq!(
            report_fair(
                &q,
                &lut,
                50_000_000_000,
                end,
                3_581 * S + 2_900_000_000,
                1_550
            ),
            Err(LateHold::NoNowcast)
        );
    }

    // ---- law (iii) (J12) ------------------------------------------------

    /// `4 · Σ_ij w_i w_j min(t_i, t_j)` by brute force, ns.
    fn brute_var4_ns(n: i128, a_ns: i128) -> i128 {
        let mut acc: i128 = 0;
        let w2 = |k: i128| if k == n - 1 { 1i128 } else { 2i128 };
        let mut i = 0i128;
        while i < n {
            let mut j = 0i128;
            while j < n {
                let ti = a_ns + i * 1_000_000_000;
                let tj = a_ns + j * 1_000_000_000;
                acc += w2(i) * w2(j) * ti.min(tj);
                j += 1;
            }
            i += 1;
        }
        acc
    }

    #[test]
    fn the_trapezoid_tail_variance_is_the_double_sum() {
        for a in [0i128, 300_000_000, 1_000_000_000, 2_700_000_000, 999_999_999] {
            let mut n = 0i128;
            while n <= 61 {
                assert_eq!(trap_tail_var4_ns(n, a), brute_var4_ns(n, a), "n {n} a {a}");
                n += 1;
            }
        }
    }

    #[test]
    fn the_trapezoid_strike_weights_the_ends_by_half() {
        // Reports 1 000 … 1 100: r(s) = base + s (a ramp, so the weights show).
        let mut g = ReportGrid::new();
        let base = 50_000_000_000i64;
        let mut s = 1_000u64;
        while s <= 1_100 {
            g.on_report(s, base + s as i64);
            s += 1;
        }
        let open = 1_090u64;
        // ½·r(1028) + Σ r(1029 … 1087) + ½·r(1088), over 60.
        let mut want: i128 = 0;
        let mut l = open - 62;
        while l <= open - 2 {
            let w = if l == open - 62 || l == open - 2 { 1 } else { 2 };
            want += w * i128::from(base + l as i64);
            l += 1;
        }
        assert_eq!(g.trap(open), Some(floor_div(want, 120) as i64));
        // A flat series is its own trapezoid; the plain mean agrees.
        let flat = fed(2_000, 152, 50_000_000_000, 0);
        assert_eq!(flat.trap(2_152), Some(50_000_000_000));
        assert_eq!(flat.mean(2_092, 2_151), Some(50_000_000_000));
        // A hole of four seconds is no strike; too early an open is none.
        let mut h = ReportGrid::new();
        let mut s = 1_000u64;
        while s <= 1_100 {
            if !(1_040..1_044).contains(&s) {
                h.on_report(s, base);
            }
            s += 1;
        }
        assert_eq!(h.trap(1_090), None);
        assert_eq!(g.trap(61), None);
    }

    #[test]
    fn the_first_law_keeps_its_old_horizon_on_the_longer_ring() {
        let mut g = ReportGrid::new();
        let mut s = 1_000u64;
        while s <= 1_200 {
            g.on_report(s, 50_000_000_000 + s as i64);
            s += 1;
        }
        // 61 s behind the newest report: out for the first law (as on the
        // 64-s ring), still in for law (iii).
        assert_eq!(g.report_at(1_200 - 61), None);
        assert_eq!(g.report_at(1_200 - 60), Some(50_000_000_000 + 1_140));
        assert_eq!(g.report_within(1_200 - 61, TRAP_HORIZON_S), Some(50_000_000_000 + 1_139));
        assert_eq!(g.report_within(1_200 - 125, TRAP_HORIZON_S), None);
        // The trapezoid of an open 1 s after the newest report reaches
        // o − 62 = 61 s back: the old ring could not hold it.
        assert!(g.trap(1_201).is_some());
        assert_eq!(g.report_at(1_201 - 62), None, "o − 62 is past the first law's horizon");
    }

    fn res(ratio_1e3: i64, min_ppm: i64) -> TrapResidual {
        TrapResidual { ratio_1e3, min_ppm }
    }

    #[test]
    fn law_iii_prices_a_decided_window_and_an_open_one() {
        let g = fed(2_970, 611, 50_000_000_000, 1_000_000);
        assert_eq!(g.last_sec(), 3_580);
        let lut = ramp();
        let end = 3_600 * S;
        let wall = 3_580 * S + 1_300_000_000;
        let r = res(130, 3);
        let (f, _) =
            report_fair_trap(&g, &lut, 49_950_000_000, end, wall, 1_550, r).expect("priced");
        // Labels 3 538 … 3 598; observed through 3 580: 43 of 61.
        assert_eq!(f.n_obs, 43);
        assert!(f.p_yes_1e6 > 990_000, "p {}", f.p_yes_1e6);
        let (f, _) =
            report_fair_trap(&g, &lut, 50_000_000_000, end, wall, 1_550, r).expect("priced");
        assert!((450_000..=550_000).contains(&f.p_yes_1e6), "p {}", f.p_yes_1e6);
    }

    #[test]
    fn the_residual_widens_the_belief_and_its_floor_binds_on_quiet_tape() {
        // σ of a ±1e6 alternation on 5e10: |r| ≈ 4e-5 → σ_1e9 ≈ 40 000.
        let g = fed(2_970, 611, 50_000_000_000, 1_000_000);
        let lut = ramp();
        let end = 3_600 * S;
        let wall = 3_580 * S + 1_300_000_000;
        let strike = 49_999_000_000; // 20 ppm under
        let (no_res, _) =
            report_fair_trap(&g, &lut, strike, end, wall, 1_550, res(0, 0)).expect("priced");
        let (with, floor) =
            report_fair_trap(&g, &lut, strike, end, wall, 1_550, res(130, 3)).expect("priced");
        assert!(with.z_1e6 < no_res.z_1e6, "a residual only widens: {} {}", with.z_1e6, no_res.z_1e6);
        assert!(!floor, "130 · σ (≈ 5 ppm) is over a 3 ppm floor");
        let (_, floor) =
            report_fair_trap(&g, &lut, strike, end, wall, 1_550, res(130, 50)).expect("priced");
        assert!(floor, "a 50 ppm floor binds");
        let sigma = g.sigma_1e9().expect("σ");
        assert_eq!(res(130, 3).rel_1e9(sigma), floor_div_i64(130 * sigma, 1_000));
        assert_eq!(res(130, 50).rel_1e9(sigma), 50_000);
    }

    #[test]
    fn a_fully_observed_settlement_prices_on_the_residual_alone() {
        // Window end 3 600; labels end at 3 598; reports through 3 599.
        let g = fed(2_990, 610, 50_000_000_000, 0);
        assert_eq!(g.last_sec(), 3_599);
        let lut = ramp();
        let end = 3_600 * S;
        let wall = 3_599 * S + 500_000_000;
        // A flat 5e10 settlement against a strike 10 ppm under: z = 10 / ρσ-or-floor.
        let strike = 49_999_500_000;
        let (f, floor) =
            report_fair_trap(&g, &lut, strike, end, wall, 1_550, res(0, 5)).expect("priced");
        assert_eq!(f.n_obs, TRAP_LABELS);
        assert!(floor);
        // num = 120 · 500 000; r = 5e-6 · strike ≈ 249 997; z = 500 000 / 249 997 ≈ 2.0.
        assert!((1_999_000..=2_001_000).contains(&f.z_1e6), "z {}", f.z_1e6);
        // With no residual at all, a decided settlement is the clamp.
        let (f, _) =
            report_fair_trap(&g, &lut, strike, end, wall, 1_550, res(0, 0)).expect("priced");
        assert_eq!(f.z_1e6, LATE_Z_CLAMP_1E6);
    }

    #[test]
    fn law_iii_holds_as_the_first_law_does() {
        let lut = ramp();
        let end = 3_600 * S;
        let g = fed(2_970, 611, 50_000_000_000, 1_000_000);
        let wall = 3_580 * S + 1_300_000_000;
        let r = res(130, 3);
        assert_eq!(report_fair_trap(&g, &lut, 0, end, wall, 1_550, r).map(|x| x.0), Err(LateHold::Shape));
        assert_eq!(report_fair_trap(&g, &lut, 1, end, wall, 0, r).map(|x| x.0), Err(LateHold::Shape));
        assert_eq!(
            report_fair_trap(&g, &lut, 1, end, wall, 1_550, res(-1, 3)).map(|x| x.0),
            Err(LateHold::Shape)
        );
        assert_eq!(
            report_fair_trap(&g, &lut, 1, end, end + 1, 1_550, r).map(|x| x.0),
            Err(LateHold::Shape)
        );
        assert_eq!(
            report_fair_trap(&g, &lut, 1, end, 3_585 * S + 1, 1_550, r).map(|x| x.0),
            Err(LateHold::MarkStale)
        );
        let short = fed(3_300, 281, 50_000_000_000, 1_000_000);
        assert_eq!(
            report_fair_trap(&short, &lut, 50_000_000_000, end, wall, 1_550, r).map(|x| x.0),
            Err(LateHold::NoSigma)
        );
        // 70 s before the end: the first label is not reported yet.
        let early = fed(2_900, 631, 50_000_000_000, 1_000_000);
        assert_eq!(early.last_sec(), 3_530);
        assert_eq!(
            report_fair_trap(&early, &lut, 50_000_000_000, end, 3_530 * S + 500_000_000, 1_550, r)
                .map(|x| x.0),
            Err(LateHold::Shape)
        );
    }

    #[test]
    fn law_iii_survives_btc_at_a_million_dollars_in_the_fine_scale() {
        // ×1e9 scale (law (iii)'s), BTC at 1e6 USD, σ at its cap, k at 10.
        let g = fed(2_970, 611, 1_000_000_000_000_000, 50_000_000_000_000);
        assert_eq!(g.sigma_1e9(), Some(SIGMA_MAX_1E9), "σ at its cap");
        let lut = ramp();
        let end = 3_600 * S;
        let wall = 3_580 * S + 1_300_000_000;
        let (f, _) = report_fair_trap(&g, &lut, 999_000_000_000_000, end, wall, 10_000, res(1_000, 100))
            .expect("priced");
        assert!(f.p_yes_1e6 >= 500_000);
    }

    #[test]
    fn the_fine_scale_keeps_dogs_sub_ppm_steps() {
        // DOGE near 0.0796: ×1e6 rounds to 12.6 ppm steps, ×1e9 keeps 0.0126.
        let mut g = ReportGrid::new();
        let mut s = 1_000u64;
        while s <= 1_100 {
            g.on_report(s, 79_600_000 + (s % 3) as i64 * 7); // steps of 0.09 ppm
            s += 1;
        }
        let k = g.trap(1_090).expect("strike");
        assert!((79_600_000..=79_600_014).contains(&k), "k {k}");
        assert_ne!(k, 79_600_000, "the sub-ppm steps survive");
    }
}
