// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The integer arithmetic of the xsd member (statarb doc 08 §3.4):
//! a fixed-point natural log, the pairwise spread and the rolling
//! z-score law. Every function is allocation-free, panic-free in
//! release, float-free, and mirrored bit for bit by
//! `claude_worker.xsd_ref` — the parity fixture
//! (`crates/strategy-xsd/tests/parity.rs`) pins the two together.
//!
//! Division rule: every signed division is [`floor_div`] (Python `//`);
//! every unsigned one is a plain `/` or `>>` on a non-negative value,
//! where truncation and floor agree.

use core_regime::math::floor_div;
pub use core_regime::math::isqrt_i128;

/// Fixed-point scale of the log, the z-scores and every threshold.
pub const SCALE_1E9: i64 = 1_000_000_000;

/// `ln 2` in Q60 (`round(ln 2 · 2^60)`), the exponent's contribution.
const LN2_Q60: i128 = 799_144_290_325_165_979;
/// Q60 one.
const ONE_Q60: i128 = 1 << 60;
/// Last odd power of the atanh series (`u^21/21`); `|u| < 1/3`, so the
/// truncation error is below `2·3^-23/23 ≈ 9e-13` — far under the 1e-9
/// unit.
const SERIES_LAST: i128 = 21;

/// Natural log of a positive integer, ×1e9, rounded half up.
///
/// `v = m · 2^k` with `m ∈ [1, 2)` by the leading-zero count; `ln m`
/// by the atanh series `2 (u + u³/3 + … + u²¹/21)`, `u = (m−1)/(m+1)`,
/// in Q60 `i128` fixed point; `+ k · ln 2`. Accuracy law, pinned by the
/// tests here and in the Python mirror: `|ln1e9(v) − round(ln v · 1e9)|
/// ≤ 1` for every `v ∈ [1, 2^63)`.
///
/// The member feeds it prices ×1e6; the constant `ln 1e6` offset cancels
/// in every spread's mean, which is why no rescaling happens here.
/// `v == 0` (never a price) returns 0 in release and asserts in debug.
#[inline]
pub fn ln1e9(v: u64) -> i64 {
    debug_assert!(v > 0, "ln1e9 of zero");
    if v == 0 {
        return 0;
    }
    let k = 63 - v.leading_zeros() as i32;
    let m_q: i128 = if k <= 60 {
        (v as i128) << (60 - k)
    } else {
        (v >> (k - 60)) as i128
    };
    let u = ((m_q - ONE_Q60) << 60) / (m_q + ONE_Q60);
    let u2 = (u * u) >> 60;
    let mut term = u;
    let mut acc = u;
    let mut i: i128 = 3;
    while i <= SERIES_LAST {
        term = (term * u2) >> 60;
        acc += term / i;
        i += 2;
    }
    let ln_v = (acc << 1) + (k as i128) * LN2_Q60;
    ((ln_v * SCALE_1E9 as i128 + (1 << 59)) >> 60) as i64
}

/// The pairwise spread `s = ln a − β · ln b` (×1e9) from two logs
/// (×1e9) and a hedge ratio (×1e9), floor-divided.
#[inline(always)]
pub fn spread_1e9(ln_a_1e9: i64, ln_b_1e9: i64, beta_1e9: i64) -> i64 {
    ln_a_1e9 - floor_div(beta_1e9 as i128 * ln_b_1e9 as i128, SCALE_1E9 as i128) as i64
}

/// The minimum sample count a window must hold before a z is finite:
/// `max(30, W / 4)` — the research's `rolling_z` mask.
#[inline(always)]
pub const fn min_count(window_h: u32) -> u32 {
    let q = window_h / 4;
    if q > 30 {
        q
    } else {
        30
    }
}

/// Running window statistics of one pair: the sum, the sum of squares
/// and the count of PRESENT spreads over the trailing window, plus the
/// newest spread when present.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct WindowStats {
    /// Σ s (×1e9).
    pub sum: i128,
    /// Σ s² (×1e18).
    pub sum2: i128,
    /// Present spreads in the window.
    pub n: u32,
    /// The newest spread is present (`s0` meaningful).
    pub newest_present: bool,
    /// The newest spread (×1e9).
    pub s0: i64,
}

impl WindowStats {
    /// Empty window.
    pub const EMPTY: Self = Self {
        sum: 0,
        sum2: 0,
        n: 0,
        newest_present: false,
        s0: 0,
    };

    /// Fold one present spread in.
    #[inline(always)]
    pub fn push(&mut self, s: i64) {
        self.sum += s as i128;
        self.sum2 += s as i128 * s as i128;
        self.n += 1;
    }
}

/// The z-score of the newest spread over its window, ×1e9, or `None`
/// when the research's mask refuses it: fewer than [`min_count`]
/// present samples, an absent newest spread, or a zero standard
/// deviation. `mean = ⌊Σ/n⌋`, `var = ⌊Σ²/n⌋ − mean²`, `std =
/// isqrt(var)`, `z = ⌊(s0 − mean) · 1e9 / std⌋`.
#[inline]
pub fn z_1e9(w: &WindowStats, window_h: u32) -> Option<i64> {
    if !w.newest_present || w.n < min_count(window_h) {
        return None;
    }
    let n = w.n as i128;
    let mean = floor_div(w.sum, n);
    let var = floor_div(w.sum2, n) - mean * mean;
    let std = isqrt_i128(var);
    if std <= 0 {
        return None;
    }
    Some(floor_div((w.s0 as i128 - mean) * SCALE_1E9 as i128, std as i128) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(v: u64) -> i64 {
        ((v as f64).ln() * 1e9).round() as i64
    }

    #[test]
    fn ln1e9_within_one_unit_of_the_float_log() {
        let fixed: [u64; 14] = [
            1,
            2,
            3,
            7,
            10,
            999_999,
            1_000_000,
            1_000_001,
            65_000_000_000,
            200_000_000_000,
            1_000_000_000_000,
            123_456_789_012_345,
            1 << 62,
            u64::MAX >> 1,
        ];
        for v in fixed {
            let d = (ln1e9(v) - reference(v)).abs();
            assert!(d <= 1, "v={v} got {} want {} (Δ {d})", ln1e9(v), reference(v));
        }
        // A deterministic sweep across the whole exponent range.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut i = 0;
        while i < 20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let shift = (x % 63) as u32;
            let v = ((x >> 1) >> shift).max(1);
            let d = (ln1e9(v) - reference(v)).abs();
            assert!(d <= 1, "v={v} Δ {d}");
            i += 1;
        }
    }

    #[test]
    fn ln1e9_is_monotone_and_exact_at_powers_of_two() {
        assert_eq!(ln1e9(1), 0);
        assert_eq!(ln1e9(2), 693_147_181);
        assert_eq!(ln1e9(1 << 40), 27_725_887_222);
        let mut prev = ln1e9(1);
        let mut v = 2u64;
        while v < (1u64 << 63) {
            let cur = ln1e9(v);
            assert!(cur > prev, "not monotone at {v}");
            prev = cur;
            v = v.saturating_mul(3).max(v + 1);
        }
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn ln1e9_zero_is_zero_in_release() {
        assert_eq!(ln1e9(0), 0);
    }

    #[test]
    fn spread_floors_the_hedge_term() {
        // β = 0.5 on ln b = 3 (×1e9 units) → 1.5 floors to 1.
        assert_eq!(spread_1e9(10, 3, 500_000_000), 9);
        // Negative hedge term floors toward −∞.
        assert_eq!(spread_1e9(10, -3, 500_000_000), 12);
        assert_eq!(spread_1e9(0, 0, 0), 0);
    }

    #[test]
    fn min_count_is_thirty_or_a_quarter() {
        assert_eq!(min_count(720), 180);
        assert_eq!(min_count(100), 30);
        assert_eq!(min_count(120), 30);
        assert_eq!(min_count(124), 31);
    }

    #[test]
    fn z_matches_the_closed_form_and_refuses_the_mask() {
        // 0..=9 → mean 4.5, var 8.25 → std 2.87; window law needs n ≥ 30,
        // so use a 30-sample ramp: mean = 14.5 (floors to 14), var =
        // ⌊8555/30⌋ − 196 = 285 − 196 = 89 → std 9 (floor of 9.43).
        let mut w = WindowStats::EMPTY;
        let mut i = 0i64;
        while i < 30 {
            w.push(i);
            i += 1;
        }
        w.newest_present = true;
        w.s0 = 29;
        assert_eq!(z_1e9(&w, 100), Some((29 - 14) * SCALE_1E9 / 9));
        // Absent newest ⇒ None.
        w.newest_present = false;
        assert_eq!(z_1e9(&w, 100), None);
        // Too few samples ⇒ None.
        let mut short = WindowStats::EMPTY;
        short.push(1);
        short.newest_present = true;
        short.s0 = 1;
        assert_eq!(z_1e9(&short, 100), None);
        // Zero std ⇒ None.
        let mut flat = WindowStats::EMPTY;
        let mut i = 0;
        while i < 40 {
            flat.push(5);
            i += 1;
        }
        flat.newest_present = true;
        flat.s0 = 5;
        assert_eq!(z_1e9(&flat, 100), None);
    }
}
