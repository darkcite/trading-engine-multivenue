// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BIN15 H90 — the LATE-WINDOW pricer (ruling O-H90-5; the build plan is
//! the git-excluded vault's doc 33, the evidence its doc 32). Moved here
//! from `strategy_bin15::late` VERBATIM at PM15-4 (2026-10-17) —
//! `strategy_bin15::late` re-exports it, and `parity-3` pins it unchanged —
//! with one refactor that changes no arithmetic: the settlement law's tail
//! is [`price_twap`], which the Polymarket report grid ([`crate::report`])
//! prices with too.
//!
//! Inside the venue's settlement window the binary is decided by the 60-s
//! mark TWAP ending at expiry (LAW E-11), and part of that average is
//! already behind. This module prices the remainder from a SHORT horizon,
//! the way the out-of-sample study priced it ("P1b"):
//!
//! 1. a **1-s last-value grid** of the underlying's mark (`g(s)`, the last
//!    mark with wall ≤ `s`, fresh while under [`MARK_FRESH_NS`] old) and of
//!    the HL perp BBO mid (`gp(s)`, fresh under [`PERP_FRESH_NS`]);
//! 2. **σ** = the RMS of the 1-s mark returns over the trailing
//!    [`SIGMA_WINDOW_S`] grid seconds — a return counted only when both of
//!    its seconds are fresh, and no price under [`SIGMA_MIN_OBS`] of them;
//! 3. the **nowcast** `m_now = gp(now) · median(g/gp)` over the trailing
//!    [`BASIS_WINDOW_S`] grid seconds: the perp's book leads the mark it
//!    feeds;
//! 4. the **settlement** `S = mean g(s)` over `s = T−59 … T`: the seconds
//!    at or before now are observed, the `n` after them a driftless walk
//!    from `m_now` with per-second sd `k·σ·m_now`, so
//!    `Var S = (k σ m_now)² (a n² + n(n−1)(2n−1)/6) / 3600` with `a` the
//!    time to the first unobserved sample;
//! 5. `p_yes = Φ((E S − K) / sd)` read off [`LateLut`] — a Gaussian table
//!    on `z = 0 … 8.192`, an ARTIFACT (`late_phi_lut`), because Φ is data
//!    and near-certainty must be expressible: the S-law table stops at
//!    0.999979, which would refuse every 0.98 offer on a decided instance.
//!
//! Integer throughout, mirrored step for step by
//! `claude_worker.bin15_ref` (`late_*`) and pinned by the `parity-3`
//! fixture at tolerance zero. No allocation after [`LateGrid::new`], no
//! float, no transcendental, while-index loops.

use core_regime::math::{floor_div, isqrt_i128};

/// Points in the late Φ table: `z = 0, 0.002, …, 8.192`.
pub const LATE_PHI_POINTS: usize = 4097;

/// The late Φ table's step in `z` ×1e6.
pub const LATE_PHI_STEP_1E6: i64 = 2_000;

/// Widest `|z|` the table covers ×1e6; beyond it the last entry holds.
pub const LATE_Z_CLAMP_1E6: i64 = 8_192_000;

/// One second, ns — the grid's step.
pub const SEC_NS: u64 = 1_000_000_000;

/// A grid second's mark is fresh while the mark in force is younger than
/// this — the study's 5 s, one shelf life with the member's own gate.
pub const MARK_FRESH_NS: u64 = 5_000_000_000;

/// A grid second's perp mid is fresh while younger than this.
pub const PERP_FRESH_NS: u64 = 3_000_000_000;

/// σ's window, grid seconds.
pub const SIGMA_WINDOW_S: u64 = 900;

/// The fewest valid 1-s returns σ is computed from.
pub const SIGMA_MIN_OBS: u32 = 300;

/// The basis median's window, grid seconds.
pub const BASIS_WINDOW_S: u64 = 60;

/// Samples in the venue's 15 m settlement average (one a second over the
/// last minute).
pub const TWAP_SAMPLES: u64 = 60;

/// Seconds the return ring holds — a power of two over the σ window.
pub const R2_RING: usize = 1024;

/// Seconds the short rings (marks, ratios) hold — over the settlement
/// average's 60 and the basis's 60.
pub const SHORT_RING: usize = 64;

/// The largest σ the pricer accepts ×1e9 per √s (10 % a second): past it
/// the mark is not a price series, and the `i128` product stays bounded.
pub const SIGMA_MAX_1E9: i64 = 100_000_000;

/// The late law's parameters, as the cli hands them in (`bin15.toml`
/// `late_*`, parsed and bounded by `core-config`). `enabled == 0` is the
/// member bit for bit as it was before H90.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LateParams {
    /// `1` = the late entry is on.
    pub enabled: u8,
    /// Consecutive distinct snapshots of the favoured leg the test must
    /// hold on (the S5 poll), in `[1, 8]`.
    pub persist_polls: u8,
    /// BIN15 fills B-1 (ruling F-5): `1` = after an UNFILLED late IoC the
    /// instance fires again only once the leg it bought on shows another
    /// touch (its ask price or size, an emptied side included); `0` = on
    /// the next passing reprice, the law to 2026-10-23.
    pub refire_fresh: u8,
    /// BIN15 fills B-3 (ruling F-2): `1` = the IoC's limit is the law's
    /// bound `p − e` floored to the grid, sized `usd / limit` under the
    /// caps, so it takes every resting level up to the bound; `0` = the
    /// seen touch ask, sized to the touch's own quantity.
    pub limit_bound: u8,
    /// BIN15 fills B-4 guard (G26): with a lead book on the underlying,
    /// the law fires only if the HL-perp nowcast (the pre-B-4 grid) favours
    /// the same side with at least this belief ×1e6; `0` = no guard (the
    /// lead alone decides, B-4 as it was). Ignored without a lead.
    pub lead_guard_1e6: u32,
    /// The window's floor: no late entry with less than this to expiry, ns.
    pub tau_min_ns: u64,
    /// The window's ceiling: the late law prices from this far out, ns.
    pub tau_max_ns: u64,
    /// The least belief on the favoured side ×1e6.
    pub p_min_1e6: i64,
    /// The margin the ask must leave under that belief ×1e6.
    pub e_1e6: i64,
    /// The most one late entry spends ×1e6 USD (the whole ask up to this).
    pub usd_1e6: i64,
    /// The σ scale ×1e3.
    pub sigma_k_1e3: i64,
    /// BIN15 fills B-2 (ruling F-5): the oldest favoured-leg ask the law
    /// fires on — engine ms since that leg's ask moved to its current price
    /// (the member's `TouchState::ask_since_ms`); `0` = no limit.
    pub max_quote_age_ms: u32,
    /// PR-BIN15-P2 (Anton, 2026-10-24): the dearest price ×1e6 the law
    /// pays. No ask over it is bought, and at the bound the IoC's limit is
    /// the lower of `p − e` and the cap. `0` = no cap. Below the 1 c margin's
    /// real discounts sit the maker's standing 0.9898 asks bought at p ≈ 1,
    /// for under 1 c a contract with the whole stake at risk; 0.985 drops them.
    pub limit_max_1e6: u32,
}

const _: () = assert!(::core::mem::size_of::<LateParams>() == 64);

impl Default for LateParams {
    /// The ruled values (O-H90-5..7) with the law OFF.
    fn default() -> Self {
        Self {
            enabled: 0,
            persist_polls: 2,
            refire_fresh: 0,
            limit_bound: 0,
            lead_guard_1e6: 0,
            tau_min_ns: 2_000_000_000,
            tau_max_ns: 30_000_000_000,
            p_min_1e6: 900_000,
            e_1e6: 20_000,
            usd_1e6: 100_000_000,
            sigma_k_1e3: 1_550,
            max_quote_age_ms: 0,
            limit_max_1e6: 0,
        }
    }
}

/// The late Φ table, boot-boxed by the member (4 097 `u32`s).
#[repr(C, align(64))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LateLut {
    /// `Φ(z) ×1e6` for `z = 0, 0.002, …, 8.192`.
    pub phi: [u32; LATE_PHI_POINTS],
}

impl Default for LateLut {
    fn default() -> Self {
        Self::flat()
    }
}

impl LateLut {
    /// A table that believes nothing: Φ ≡ 0.5, so no side ever reaches a
    /// `p_min` over one half. What an unconfigured member carries.
    #[must_use]
    pub fn flat() -> Self {
        Self {
            phi: [500_000; LATE_PHI_POINTS],
        }
    }

    /// `Φ(|z|) ×1e6` by linear interpolation; `z_abs_1e6` must be in
    /// `[0, LATE_Z_CLAMP_1E6]`.
    #[inline(always)]
    #[must_use]
    pub fn phi_1e6(&self, z_abs_1e6: i64) -> i64 {
        debug_assert!((0..=LATE_Z_CLAMP_1E6).contains(&z_abs_1e6));
        let idx = (z_abs_1e6 / LATE_PHI_STEP_1E6) as usize;
        if idx >= LATE_PHI_POINTS - 1 {
            return i64::from(self.phi[LATE_PHI_POINTS - 1]);
        }
        let lo = i64::from(self.phi[idx]);
        let hi = i64::from(self.phi[idx + 1]);
        let frac = z_abs_1e6 - idx as i64 * LATE_PHI_STEP_1E6;
        lo + floor_div(
            ((hi - lo) as i128) * frac as i128,
            LATE_PHI_STEP_1E6 as i128,
        ) as i64
    }

    /// `p_yes ×1e6` at a signed `z ×1e6` (clamped).
    #[inline(always)]
    #[must_use]
    pub fn p_of_z(&self, z_1e6: i64) -> i64 {
        let z = z_1e6.clamp(-LATE_Z_CLAMP_1E6, LATE_Z_CLAMP_1E6);
        if z >= 0 {
            self.phi_1e6(z)
        } else {
            1_000_000 - self.phi_1e6(-z)
        }
    }
}

/// Why the late pricer gave no price.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LateHold {
    /// Fewer than [`SIGMA_MIN_OBS`] valid 1-s returns in the window.
    NoSigma,
    /// No fresh perp mid, or no fresh mark/perp ratio in the basis window.
    NoNowcast,
    /// The mark in force is older than [`MARK_FRESH_NS`].
    MarkStale,
    /// An observed second of the settlement average had no fresh mark.
    ObservedStale,
    /// The query is past the settlement or the inputs are out of shape.
    Shape,
}

/// One late price and every input it was built from.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LateFair {
    /// `P(Yes) ×1e6`.
    pub p_yes_1e6: i64,
    /// The standardised distance ×1e6 (clamped to the table).
    pub z_1e6: i64,
    /// σ ×1e9 per √s (unscaled by `k`).
    pub sigma_1e9: i64,
    /// The mark/perp basis ×1e9.
    pub basis_1e9: i64,
    /// The nowcast mark ×1e6.
    pub m_now_1e6: i64,
    /// Observed samples of the settlement average.
    pub n_obs: u64,
}

/// One underlying's 1-s grid: the rings the late pricer reads.
#[repr(C, align(64))]
#[derive(Clone, Debug)]
pub struct LateGrid {
    /// `g(s) ×1e6` for the last [`SHORT_RING`] grid seconds; `0` = no fresh
    /// mark at that second.
    mark: [i64; SHORT_RING],
    /// `g(s)/gp(s) ×1e9` for the last [`SHORT_RING`] grid seconds; `0` = no
    /// fresh pair.
    ratio: [i64; SHORT_RING],
    /// `r(s)²` (`r ×1e9`) for the last [`R2_RING`] grid seconds.
    r2: [i64; R2_RING],
    /// Validity bits of `r2`.
    r_ok: [u64; R2_RING / 64],
    /// Σ `r2` over the valid returns inside the σ window.
    sum_r2: i128,
    /// Valid returns inside the σ window.
    n_r: u64,
    /// The last grid second written; `0` = none yet.
    sec: u64,
    /// The mark in force ×1e6 and its wall stamp.
    cur_mark_1e6: i64,
    cur_mark_wall: u64,
    /// The perp BBO in force, `bid + ask` ×1e6 (twice the mid, exact), and
    /// its wall stamp.
    cur_perp_x2_1e6: i64,
    cur_perp_wall: u64,
}

impl Default for LateGrid {
    fn default() -> Self {
        Self::new()
    }
}

impl LateGrid {
    /// An empty grid — boot only (≈ 9.4 KiB; the member boxes them).
    #[must_use]
    pub fn new() -> Self {
        Self {
            mark: [0; SHORT_RING],
            ratio: [0; SHORT_RING],
            r2: [0; R2_RING],
            r_ok: [0; R2_RING / 64],
            sum_r2: 0,
            n_r: 0,
            sec: 0,
            cur_mark_1e6: 0,
            cur_mark_wall: 0,
            cur_perp_x2_1e6: 0,
            cur_perp_wall: 0,
        }
    }

    /// Forget the rings (a gap longer than they hold); the values in force
    /// survive.
    fn clear_rings(&mut self) {
        self.mark = [0; SHORT_RING];
        self.ratio = [0; SHORT_RING];
        self.r2 = [0; R2_RING];
        self.r_ok = [0; R2_RING / 64];
        self.sum_r2 = 0;
        self.n_r = 0;
    }

    /// Write grid second `s` from the values in force (all stamped at or
    /// before `s`, which [`Self::advance_to`]'s callers guarantee).
    #[inline]
    fn fill_second(&mut self, s: u64) {
        let t = s.saturating_mul(SEC_NS);
        let m = if self.cur_mark_1e6 > 0
            && self.cur_mark_wall <= t
            && t - self.cur_mark_wall < MARK_FRESH_NS
        {
            self.cur_mark_1e6
        } else {
            0
        };
        let p = if self.cur_perp_x2_1e6 > 0
            && self.cur_perp_wall <= t
            && t - self.cur_perp_wall < PERP_FRESH_NS
        {
            self.cur_perp_x2_1e6
        } else {
            0
        };
        let i = (s % SHORT_RING as u64) as usize;
        let prev = self.mark[((s + SHORT_RING as u64 - 1) % SHORT_RING as u64) as usize];
        self.mark[i] = m;
        self.ratio[i] = if m > 0 && p > 0 {
            floor_div(m as i128 * 2_000_000_000, p as i128) as i64
        } else {
            0
        };
        // The return leaving the window: second `s − SIGMA_WINDOW_S`.
        if s >= SIGMA_WINDOW_S {
            let o = ((s - SIGMA_WINDOW_S) % R2_RING as u64) as usize;
            let bit = 1u64 << (o % 64);
            if self.r_ok[o / 64] & bit != 0 {
                self.sum_r2 -= i128::from(self.r2[o]);
                self.n_r -= 1;
                self.r_ok[o / 64] &= !bit;
            }
        }
        let j = (s % R2_RING as u64) as usize;
        let bit = 1u64 << (j % 64);
        // Slot `j` last held second `s − R2_RING`, already out of the window.
        debug_assert!(self.r_ok[j / 64] & bit == 0);
        self.r_ok[j / 64] &= !bit;
        self.r2[j] = 0;
        if m > 0 && prev > 0 {
            let r = floor_div((m - prev) as i128 * 1_000_000_000, prev as i128);
            let sq = (r * r).min(i128::from(i64::MAX)) as i64;
            self.r2[j] = sq;
            self.r_ok[j / 64] |= bit;
            self.sum_r2 += i128::from(sq);
            self.n_r += 1;
        }
    }

    /// Fill every grid second up to `target` from the values in force.
    #[inline]
    pub fn advance_to(&mut self, target: u64) {
        if self.sec == 0 {
            if self.cur_mark_1e6 <= 0 {
                return;
            }
            self.sec = target.saturating_sub(1);
        }
        if target <= self.sec {
            return;
        }
        if target - self.sec > R2_RING as u64 {
            self.clear_rings();
            self.sec = target - 1;
        }
        let mut s = self.sec + 1;
        while s <= target {
            self.fill_second(s);
            s += 1;
        }
        self.sec = target;
    }

    /// A mark at wall `w`: the seconds before it are written with the
    /// mark it replaces, then it takes over.
    #[inline]
    pub fn on_mark(&mut self, wall_ns: u64, mark_1e6: i64) {
        if mark_1e6 <= 0 {
            return;
        }
        self.advance_to(wall_ns.saturating_sub(1) / SEC_NS);
        self.cur_mark_1e6 = mark_1e6;
        self.cur_mark_wall = wall_ns;
    }

    /// A perp BBO at wall `w`, as `bid + ask` ×1e6.
    #[inline]
    pub fn on_perp(&mut self, wall_ns: u64, bid_plus_ask_1e6: i64) {
        if bid_plus_ask_1e6 <= 0 {
            return;
        }
        self.advance_to(wall_ns.saturating_sub(1) / SEC_NS);
        self.cur_perp_x2_1e6 = bid_plus_ask_1e6;
        self.cur_perp_wall = wall_ns;
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

    /// The mark/perp basis ×1e9 at grid second `sec`: the median of the
    /// fresh ratios over the trailing [`BASIS_WINDOW_S`] seconds (an even
    /// count takes the floored mean of the middle two).
    #[must_use]
    pub fn basis_1e9(&self, sec: u64) -> Option<i64> {
        let mut buf = [0i64; SHORT_RING];
        let mut k = 0usize;
        let mut back = 0u64;
        while back < BASIS_WINDOW_S && back <= sec {
            let v = self.ratio[((sec - back) % SHORT_RING as u64) as usize];
            if v > 0 {
                buf[k] = v;
                k += 1;
            }
            back += 1;
        }
        if k == 0 {
            return None;
        }
        // Insertion sort over at most 60 values: no allocation, no branch
        // the mirror cannot follow.
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

    /// The last grid second written.
    #[inline]
    #[must_use]
    pub const fn sec(&self) -> u64 {
        self.sec
    }
}

/// The late price of a binary struck at `strike_1e6`, expiring at
/// `expiry_ns` (a whole second), seen at `wall_ns`, σ scaled by
/// `k_1e3`. The grid must already be advanced to `wall_ns / SEC_NS`.
pub fn late_fair(
    grid: &LateGrid,
    lut: &LateLut,
    strike_1e6: i64,
    expiry_ns: u64,
    wall_ns: u64,
    k_1e3: i64,
) -> Result<LateFair, LateHold> {
    if strike_1e6 <= 0 || k_1e3 <= 0 || expiry_ns < TWAP_SAMPLES * SEC_NS {
        return Err(LateHold::Shape);
    }
    let sec_d = wall_ns / SEC_NS;
    let last = expiry_ns / SEC_NS;
    if sec_d > last || grid.sec < sec_d {
        return Err(LateHold::Shape);
    }
    if grid.cur_mark_1e6 <= 0
        || grid.cur_mark_wall > wall_ns
        || wall_ns - grid.cur_mark_wall >= MARK_FRESH_NS
    {
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
    let Some(basis) = grid.basis_1e9(sec_d) else {
        return Err(LateHold::NoNowcast);
    };
    let m_now = floor_div(
        i128::from(grid.cur_perp_x2_1e6) * i128::from(basis),
        2_000_000_000,
    );
    if m_now <= 0 || m_now > i128::from(i64::MAX) {
        return Err(LateHold::Shape);
    }
    let first = last + 1 - TWAP_SAMPLES;
    let n_obs = if sec_d < first {
        0
    } else {
        (sec_d - first + 1).min(TWAP_SAMPLES)
    };
    let mut obs: i128 = 0;
    let mut s = first;
    while s < first + n_obs {
        let v = grid.mark[(s % SHORT_RING as u64) as usize];
        if v <= 0 {
            return Err(LateHold::ObservedStale);
        }
        obs += i128::from(v);
        s += 1;
    }
    let first_fut = if sec_d + 1 > first { sec_d + 1 } else { first };
    let a_ns = i128::from(first_fut * SEC_NS - wall_ns);
    Ok(price_twap(
        lut, obs, n_obs, a_ns, m_now, sigma, basis, k_1e3, strike_1e6,
    ))
}

/// The settlement law's tail (module docs, step 4–5): the average of
/// [`TWAP_SAMPLES`] samples whose first `n_obs` sum to `obs` (×1e6 each)
/// and whose rest are a driftless walk from `m_now` (×1e6) with per-second
/// sd `k·σ·m_now`, the first unobserved sample `a_ns` after the query —
/// priced against `strike_1e6` through `lut`. Shared by [`late_fair`] (the
/// HIP-4 last-value grid) and `crate::report::report_fair` (Polymarket's
/// Chainlink reports); the statements are `late_fair`'s own, moved, so
/// `parity-3` holds bit for bit.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn price_twap(
    lut: &LateLut,
    obs: i128,
    n_obs: u64,
    a_ns: i128,
    m_now: i128,
    sigma: i64,
    basis: i64,
    k_1e3: i64,
    strike_1e6: i64,
) -> LateFair {
    let n = i128::from(TWAP_SAMPLES - n_obs);
    // Var in s ×1e9: (a n² + n(n−1)(2n−1)/6) / 3600.
    let var_1e9 = floor_div(
        a_ns * n * n + (n * (n - 1) * (2 * n - 1) / 6) * 1_000_000_000,
        3_600,
    );
    let sqrt_v_1e9 = i128::from(isqrt_i128(var_1e9 * 1_000_000_000));
    let sd_1e6 = floor_div(
        i128::from(k_1e3) * i128::from(sigma) * m_now * sqrt_v_1e9,
        1_000_000_000_000_000_000_000,
    );
    let num = obs + n * m_now - i128::from(TWAP_SAMPLES) * i128::from(strike_1e6);
    let z = if sd_1e6 > 0 {
        floor_div(num * 1_000_000, i128::from(TWAP_SAMPLES) * sd_1e6)
            .clamp(-i128::from(LATE_Z_CLAMP_1E6), i128::from(LATE_Z_CLAMP_1E6)) as i64
    } else if num >= 0 {
        LATE_Z_CLAMP_1E6
    } else {
        -LATE_Z_CLAMP_1E6
    };
    LateFair {
        p_yes_1e6: lut.p_of_z(z),
        z_1e6: z,
        sigma_1e9: sigma,
        basis_1e9: basis,
        m_now_1e6: m_now as i64,
        n_obs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = SEC_NS;

    /// A Gaussian-shaped ramp good enough for the arithmetic tests: the
    /// shipped table is an artifact, cut by `claude_worker.bin15_fit`.
    fn ramp() -> LateLut {
        let mut l = LateLut::flat();
        let mut i = 0usize;
        while i < LATE_PHI_POINTS {
            // 0.5 at z = 0 rising linearly to 1.0 at z = 4, then flat.
            let z = i as i64 * LATE_PHI_STEP_1E6;
            let v = 500_000 + (z.min(4_000_000) / 8);
            l.phi[i] = v as u32;
            i += 1;
        }
        l
    }

    /// A grid fed one mark a second from `t0` for `secs` seconds, the mark
    /// stepping by `step` (alternating sign: a flat series with spread), and
    /// a perp mid equal to the mark.
    fn fed(t0: u64, secs: u64, base: i64, step: i64) -> LateGrid {
        let mut g = LateGrid::new();
        let mut s = 0u64;
        while s < secs {
            let m = base + if s % 2 == 0 { step } else { -step };
            let w = (t0 + s) * S + 300_000_000;
            g.on_mark(w, m);
            g.on_perp(w + 1, 2 * m);
            s += 1;
        }
        g
    }

    #[test]
    fn a_lut_is_symmetric_and_clamped() {
        let l = ramp();
        assert_eq!(l.p_of_z(0), 500_000);
        assert_eq!(l.p_of_z(2_000_000) + l.p_of_z(-2_000_000), 1_000_000);
        assert_eq!(l.p_of_z(50_000_000), i64::from(l.phi[LATE_PHI_POINTS - 1]));
        assert_eq!(
            LateLut::flat().p_of_z(7_000_000),
            500_000,
            "a flat table believes nothing"
        );
    }

    #[test]
    fn the_grid_carries_the_last_value_and_marks_staleness() {
        let mut g = LateGrid::new();
        g.on_mark(100 * S + 500_000_000, 80_000_000_000);
        g.on_perp(100 * S + 600_000_000, 160_002_000_000);
        // A query at 103.2 s writes 101, 102, 103 from the values in force.
        g.advance_to(103);
        assert_eq!(g.sec(), 103);
        assert_eq!(g.mark[(101 % 64) as usize], 80_000_000_000);
        assert_eq!(g.mark[(103 % 64) as usize], 80_000_000_000);
        // The ratio is mark / mid: 80 000 / 80 001.
        assert!(g.ratio[(103 % 64) as usize] < 1_000_000_000);
        // Six seconds later the mark has gone stale: second 106 is 5.5 s
        // after it (fresh), 107 is 6.5 s after it (stale).
        g.advance_to(106);
        assert_eq!(g.mark[(105 % 64) as usize], 80_000_000_000);
        g.advance_to(107);
        assert_eq!(
            g.mark[(106 % 64) as usize],
            0,
            "5.5 s after the mark at 100.5"
        );
    }

    #[test]
    fn sigma_needs_its_minimum_and_is_the_rms_of_the_returns() {
        let g = fed(1_000, 200, 50_000_000_000, 5_000_000);
        assert_eq!(g.sigma_1e9(), None, "under 300 returns");
        let g = fed(1_000, 500, 50_000_000_000, 5_000_000);
        let s = g.sigma_1e9().expect("over 300 returns");
        // Each return is ±10 000 000 / ~50 000 000 000 ×1e9 ≈ 200 000.
        assert!((199_000..=201_000).contains(&s), "sigma {s}");
    }

    #[test]
    fn a_gap_longer_than_the_ring_restarts_it() {
        let mut g = fed(1_000, 500, 50_000_000_000, 5_000_000);
        assert!(g.sigma_1e9().is_some());
        g.on_mark(5_000 * S, 50_000_000_000);
        assert_eq!(g.sigma_1e9(), None, "the window emptied");
    }

    #[test]
    fn a_decided_settlement_prices_near_certainty_and_an_open_one_near_half() {
        // 600 s of a quiet series around 50 000, then the query inside the
        // last minute of an instance expiring at 1 620 s.
        let g = fed(1_000, 610, 50_000_000_000, 1_000_000);
        let lut = ramp();
        let expiry = 1_620 * S;
        let wall = 1_609 * S + 400_000_000; // 49 observed samples
        let mut gg = g.clone();
        gg.advance_to(wall / S);
        // Strike $50 under the average: Yes is decided.
        let f = late_fair(&gg, &lut, 49_950_000_000, expiry, wall, 1_550).expect("priced");
        assert_eq!(f.n_obs, 49, "seconds 1 561 … 1 609 are observed");
        assert!(f.p_yes_1e6 > 990_000, "p {}", f.p_yes_1e6);
        // Strike AT the average: a coin.
        let f = late_fair(&gg, &lut, 50_000_000_000, expiry, wall, 1_550).expect("priced");
        assert!(
            (450_000..=550_000).contains(&f.p_yes_1e6),
            "p {}",
            f.p_yes_1e6
        );
    }

    #[test]
    fn every_missing_input_holds() {
        let lut = ramp();
        let expiry = 1_620 * S;
        let wall = 1_609 * S + 400_000_000;
        let mut g = LateGrid::new();
        g.advance_to(wall / S);
        assert_eq!(
            late_fair(&g, &lut, 1, expiry, wall, 1_550),
            Err(LateHold::Shape)
        );
        let mut g = fed(1_000, 610, 50_000_000_000, 1_000_000);
        g.advance_to(wall / S);
        assert_eq!(
            late_fair(&g, &lut, 50_000_000_000, expiry, wall + 30 * S, 1_550),
            Err(LateHold::Shape)
        );
        let mut g2 = g.clone();
        g2.advance_to((wall + 6 * S) / S);
        assert_eq!(
            late_fair(&g2, &lut, 50_000_000_000, expiry, wall + 6 * S, 1_550),
            Err(LateHold::MarkStale)
        );
        let short = fed(1_400, 210, 50_000_000_000, 1_000_000);
        let mut s2 = short.clone();
        s2.advance_to(wall / S);
        assert_eq!(
            late_fair(&s2, &lut, 50_000_000_000, expiry, wall, 1_550),
            Err(LateHold::NoSigma)
        );
    }
}
