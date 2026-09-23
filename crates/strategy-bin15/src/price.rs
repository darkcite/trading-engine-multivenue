// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The integer pricer: log-moneyness → `d` → Φ → recalibration.
//!
//! Separated from the member so the Python mirror
//! (`claude_worker.bin15_ref`) has one named law to follow and the
//! parity fixture has one function to pin. Every step is integer, and
//! the two expensive transcendentals (`exp`, `ln`) are NOT here — they
//! run once per minute per underlying inside `core-vol` and arrive as
//! `sig2_min_1e18`.
//!
//! **BIN15 S3 — the venue's settlement, priced.** The venue settles on
//! the TWAP of the underlying's mark over `[T − W, T]` (LAW E-11,
//! `docs/risk-policy.md` "E8"). [`pricing_horizon_ns`] is the variance-time
//! of that average seen from `τ = T − t`, and [`fair_value_twap`] prices
//! the binary from anywhere in its life: [`fair_value`] at that horizon
//! while the window lies ahead, and the RUNNING-TWAP law inside it, where
//! part of the average is already decided (ruling O-2).

use core_regime::math::{floor_div, isqrt_i128};

/// Points in the Φ table: `d = 0, 0.001, …, 4.096`.
pub const PHI_POINTS: usize = 4097;

/// Points in each recalibration table: `p = 0, 1/64, …, 1`.
pub const RECAL_POINTS: usize = 65;

/// Φ-table step in `d` ×1e6 — the table is on a 1e-3 grid.
pub const PHI_STEP_1E6: i64 = 1_000;

/// Widest `|d|` the table covers ×1e6. Beyond it Φ is 1 to within the
/// table's own resolution, so the clamp costs nothing and removes an
/// unbounded index.
pub const D_CLAMP_1E6: i64 = 4_096_000;

/// Recalibration bucket width in `p` ×1e6: `1e6 / 64`.
pub const RECAL_STEP_1E6: i64 = 15_625;

/// Widest `|u| = |mark − K| / K` the pricer will square, ×1e9.
///
/// `1e11` is `u = 100`: the mark a hundred times the strike, or a
/// hundredth of it. Nothing inside that band can overflow the `i128`
/// square (`u² ≤ 1e22`), and nothing outside it is a 15-minute binary's
/// moneyness. See [`log_moneyness_1e9`] for why the answer beyond it is
/// `None` and not a clamp.
pub const U_CLAMP_1E9: i128 = 100_000_000_000;

/// τ at or above which the EARLY recalibration table applies, ns.
pub const PHASE_EARLY_NS: u64 = 600_000_000_000;

/// τ at or above which the MID table applies (below `PHASE_EARLY_NS`).
pub const PHASE_MID_NS: u64 = 240_000_000_000;

/// Recalibration phase: τ ≥ 10 min.
pub const PHASE_EARLY: usize = 0;
/// Recalibration phase: 4 min ≤ τ < 10 min.
pub const PHASE_MID: usize = 1;
/// Recalibration phase: τ < 4 min.
pub const PHASE_LATE: usize = 2;
/// How many phases exist.
pub const PHASES: usize = 3;

/// The lookup tables, boot-boxed by the member because 4 097 + 3 × 65
/// `u32`s is 17 KiB and a member's frame is not the place for it.
#[repr(C, align(64))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bin15Luts {
    /// `Φ(d) ×1e6` for `d = 0, 0.001, …, 4.096`.
    pub phi: [u32; PHI_POINTS],
    /// `p' ×1e6` at `p = 0, 1/64, …, 1`, per [`PHASES`].
    pub recal: [[u32; RECAL_POINTS]; PHASES],
}

impl Default for Bin15Luts {
    fn default() -> Self {
        Self::identity()
    }
}

impl Bin15Luts {
    /// Tables that do nothing: Φ saturated at `0.5` and each
    /// recalibration the identity. Test scaffolding and the shape a
    /// refused artifact must never be silently replaced by — the boot
    /// path REFUSES an absent artifact rather than falling back here.
    #[must_use]
    pub fn identity() -> Self {
        let mut phi = [0u32; PHI_POINTS];
        let mut i = 0usize;
        while i < PHI_POINTS {
            phi[i] = 500_000;
            i += 1;
        }
        let mut recal = [[0u32; RECAL_POINTS]; PHASES];
        let mut ph = 0usize;
        while ph < PHASES {
            let mut k = 0usize;
            while k < RECAL_POINTS {
                recal[ph][k] = (k as i64 * RECAL_STEP_1E6) as u32;
                k += 1;
            }
            recal[ph][RECAL_POINTS - 1] = 1_000_000;
            ph += 1;
        }
        Self { phi, recal }
    }

    /// `Φ(|d|) ×1e6` by linear interpolation on the 1e-3 grid.
    ///
    /// `d_1e6` must be in `[0, D_CLAMP_1E6]` — [`fair_value`] clamps
    /// before calling, and a debug build asserts it.
    #[inline(always)]
    #[must_use]
    pub fn phi_1e6(&self, d_1e6: i64) -> i64 {
        debug_assert!((0..=D_CLAMP_1E6).contains(&d_1e6));
        let idx = (d_1e6 / PHI_STEP_1E6) as usize;
        if idx >= PHI_POINTS - 1 {
            return i64::from(self.phi[PHI_POINTS - 1]);
        }
        let lo = i64::from(self.phi[idx]);
        let hi = i64::from(self.phi[idx + 1]);
        let frac = d_1e6 - idx as i64 * PHI_STEP_1E6;
        lo + floor_div(((hi - lo) as i128) * frac as i128, PHI_STEP_1E6 as i128) as i64
    }

    /// `p' ×1e6` by linear interpolation over one phase's 64 buckets.
    #[inline(always)]
    #[must_use]
    pub fn recal_1e6(&self, phase: usize, p_1e6: i64) -> i64 {
        debug_assert!(phase < PHASES);
        let p = p_1e6.clamp(0, 1_000_000);
        let t = &self.recal[phase.min(PHASES - 1)];
        let idx = (p / RECAL_STEP_1E6) as usize;
        if idx >= RECAL_POINTS - 1 {
            return i64::from(t[RECAL_POINTS - 1]);
        }
        let lo = i64::from(t[idx]);
        let hi = i64::from(t[idx + 1]);
        let frac = p - idx as i64 * RECAL_STEP_1E6;
        lo + floor_div(((hi - lo) as i128) * frac as i128, RECAL_STEP_1E6 as i128) as i64
    }
}

/// Which recalibration table τ falls in.
#[inline(always)]
#[must_use]
pub const fn phase_of(tau_ns: u64) -> usize {
    if tau_ns >= PHASE_EARLY_NS {
        PHASE_EARLY
    } else if tau_ns >= PHASE_MID_NS {
        PHASE_MID
    } else {
        PHASE_LATE
    }
}

/// `ln(mark / strike) ×1e9`, to second order.
///
/// `u = (mark − K)/K` and `ln(1+u) ≈ u − u²/2`. A 15-minute BTC binary
/// is struck within a few tens of bps of the mark, so `|u| < 2e-2` and
/// the third-order term is under `3e-6` — below the 1e-4 price tick
/// the venue quotes on, and two orders of magnitude below the vol
/// forecast's own error. A `ln` here would be a transcendental on the
/// decision path for no measurable gain.
#[inline(always)]
#[must_use]
pub fn log_moneyness_1e9(mark_1e6: i64, strike_1e6: i64) -> Option<i64> {
    if mark_1e6 <= 0 || strike_1e6 <= 0 {
        return None;
    }
    let u = floor_div((mark_1e6 - strike_1e6) as i128 * 1_000_000_000, strike_1e6 as i128);
    // REFUSE BEFORE SQUARING. `u * u` is an `i128` multiply and a strike
    // small enough relative to the mark overflows it: a mis-parsed
    // `threshold:` of 0.000001 against a BTC mark is `u ≈ 7.7e19`, whose
    // square is 6e39 against an `i128` ceiling of 1.7e38 — a debug panic
    // and, with release overflow checks off, a wrapped `u2` that prices
    // a binary off a number with no relation to the market. The
    // `try_from` below was meant to be the guard and cannot be: it runs
    // one line too late.
    //
    // The bound is not merely arithmetic. The second-order form is
    // defensible over a few tens of bps (see above); a mark outside
    // `[K/100, 101·K]` is not a moneyness at all, it is a strike that
    // does not describe this instrument. So the answer is the lane's
    // standing one for data that cannot be trusted — ABSENT DATA HOLDS —
    // rather than a saturated near-certainty the member would happily
    // cross a book for.
    if !(-U_CLAMP_1E9..=U_CLAMP_1E9).contains(&u) {
        return None;
    }
    // u²/(2·1e9), floored the same way so the mirror can follow it.
    let u2 = floor_div(u * u, 2_000_000_000);
    i64::try_from(u - u2).ok()
}

/// The result of one re-price.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Fair {
    /// Recalibrated fair value ×1e6, in `[0, 1e6]`.
    pub p_hat_1e6: i64,
    /// Pre-recalibration fair value ×1e6.
    pub p_raw_1e6: i64,
    /// The standardised distance to the strike ×1e6, clamped.
    pub d_1e6: i64,
    /// BIN15 O6: σ√τ ×1e9 — the denominator `d_1e6` was divided by.
    ///
    /// Carried out of the pricer so a LIVE reader can separate an
    /// overconfident forecast (a small denominator) from a genuine
    /// move (a large numerator). Without it, a `p̂` pinned at 0 or
    /// 1e6 is indistinguishable between the two, and telling them
    /// apart previously needed an offline replay of the run.
    pub den_1e9: i64,
    /// BIN15 S3: the pricing horizon this price was computed at, ns —
    /// `tau_ns` for [`fair_value`], [`pricing_horizon_ns`] for
    /// [`fair_value_twap`]. It picked the recalibration phase, and the
    /// member records it rather than re-deriving it (`last_tau_ns`, the
    /// ledger's `tau_ns`).
    pub horizon_ns: u64,
}

/// Price one binary at a pricing horizon.
///
/// * `mark_1e6` — the UNDERLYING's mark, ×1e6.
/// * `strike_1e6` — the instance's threshold, ×1e6.
/// * `tau_ns` — the pricing horizon, the variance-time the settlement is
///   still exposed to: [`pricing_horizon_ns`] for a TWAP-settled family
///   (BIN15 S3 — `τ − 2W/3` while the window lies ahead), the time to
///   expiry for one that settles at `T`. [`fair_value_twap`] is the
///   member's entry point; this is the law it reduces to outside the
///   window, and what the parity fixture pins.
/// * `sig2_min_1e18` — per-minute variance of log-price, `σ_min² ×1e18`,
///   from `core-vol`'s once-a-minute `sigma_hat_1e9`.
///
/// `None` when the inputs cannot produce a number: a non-positive
/// price, a zero horizon, or a zero variance (a cold forecast — ABSENT
/// DATA HOLDS, the same law `core-vol` applies one level down).
#[inline]
#[must_use]
pub fn fair_value(
    luts: &Bin15Luts,
    mark_1e6: i64,
    strike_1e6: i64,
    tau_ns: u64,
    sig2_min_1e18: i128,
) -> Option<Fair> {
    if tau_ns == 0 || sig2_min_1e18 <= 0 {
        return None;
    }
    let x_1e9 = log_moneyness_1e9(mark_1e6, strike_1e6)?;
    // σ_min·√τ ×1e9: the per-minute variance scaled to τ minutes, then
    // rooted. `tau_ns / 60e9` is τ in minutes and the multiply happens
    // FIRST so a sub-minute τ does not floor to zero.
    let var_1e18 = sig2_min_1e18.saturating_mul(tau_ns as i128) / 60_000_000_000;
    let den_1e9 = isqrt_i128(var_1e18);
    if den_1e9 <= 0 {
        return None;
    }
    let d_1e6 = floor_div(x_1e9 as i128 * 1_000_000, den_1e9 as i128) as i64;
    let d_1e6 = d_1e6.clamp(-D_CLAMP_1E6, D_CLAMP_1E6);
    Some(fair_of_d(luts, d_1e6, den_1e9, tau_ns))
}

/// `d` → Φ → recalibration at `horizon_ns`'s phase: the tail every
/// pricing law shares.
#[inline(always)]
fn fair_of_d(luts: &Bin15Luts, d_1e6: i64, den_1e9: i64, horizon_ns: u64) -> Fair {
    // Φ is tabulated on the non-negative half only; the other half is
    // its reflection, which is exact rather than an approximation.
    let p_raw_1e6 = if d_1e6 >= 0 {
        luts.phi_1e6(d_1e6)
    } else {
        1_000_000 - luts.phi_1e6(-d_1e6)
    };
    let p_hat_1e6 = luts.recal_1e6(phase_of(horizon_ns), p_raw_1e6);
    Fair {
        p_hat_1e6: p_hat_1e6.clamp(0, 1_000_000),
        p_raw_1e6,
        d_1e6,
        den_1e9,
        horizon_ns,
    }
}

/// BIN15 S3: the pricing horizon of a TWAP-settled binary, ns — the
/// variance-time its settlement is still exposed to, `to_expiry_ns`
/// before the expiry.
///
/// The venue settles on the mean mark over `[T − W, T]`. For a Brownian
/// log-price with variance σ² per unit time, the variance of that mean
/// seen from `t` (τ = T − t):
///
/// * τ ≥ W — the window lies wholly ahead: the mean is the price at
///   `T − W` plus the average of W more of the path, so
///   `Var = σ²(τ − W) + σ²·W/3 = σ²·(τ − 2W/3)`.
/// * τ < W — inside the window: only the part of the average still ahead
///   is uncertain, `(1/W)·∫_t^T`, so `Var = σ²·τ³/(3W²)`; the part
///   already behind shifts the MEAN, which [`fair_value_twap`] carries.
/// * W = 0 — a family that settles AT `T`: τ itself.
///
/// Continuous at τ = W (both W/3). Integer and exact to the nanosecond
/// the division floors to: `⌊2W/3⌋` is formed without `2W` (no overflow
/// for any `u64`), and the cube as `⌊⌊τ²/W⌋·τ / 3W⌋` in `u128` (no
/// intermediate past `τ²`, since τ < W). Until S3 the member priced
/// `τ + W/3` — a window AFTER the expiry — 60 s too long.
#[inline]
#[must_use]
pub const fn pricing_horizon_ns(to_expiry_ns: u64, twap_ns: u64) -> u64 {
    if twap_ns == 0 {
        return to_expiry_ns;
    }
    if to_expiry_ns >= twap_ns {
        let two_thirds = (twap_ns / 3) * 2 + ((twap_ns % 3) * 2) / 3;
        return to_expiry_ns - two_thirds;
    }
    let t = to_expiry_ns as u128;
    let w = twap_ns as u128;
    ((t * t / w) * t / (3 * w)) as u64
}

/// BIN15 S3: the running-TWAP moneyness ×1e9 — the excess of the
/// settlement average over the strike, in units of the current mark,
/// when `a_known_px_ns` of the window is already behind.
///
/// `X = A_known + τ·S − W·K` (mark ×1e6 · ns) is how far the average's
/// integral would clear `W·K` if the mark stayed where it is;
/// `m = X / (S·W)`. At the window's open (A = 0, τ = W) this is
/// `(S − K)/S` — the log-moneyness to first order — so the two sides of
/// [`fair_value_twap`] meet. `None` on a non-positive price or window, on
/// arithmetic that would overflow `i128` (a nonsense `A`), or when
/// `|m|` leaves [`U_CLAMP_1E9`] (the same band [`log_moneyness_1e9`]
/// refuses outside — ABSENT DATA HOLDS).
#[inline]
#[must_use]
pub fn twap_moneyness_1e9(
    a_known_px_ns: i128,
    mark_1e6: i64,
    strike_1e6: i64,
    to_expiry_ns: u64,
    twap_ns: u64,
) -> Option<i64> {
    if mark_1e6 <= 0 || strike_1e6 <= 0 || twap_ns == 0 {
        return None;
    }
    let s = mark_1e6 as i128;
    let w = twap_ns as i128;
    let x = a_known_px_ns
        .checked_add((to_expiry_ns as i128).checked_mul(s)?)?
        .checked_sub(w.checked_mul(strike_1e6 as i128)?)?;
    let m = floor_div(x.checked_mul(1_000_000_000)?, s.checked_mul(w)?);
    if !(-U_CLAMP_1E9..=U_CLAMP_1E9).contains(&m) {
        return None;
    }
    i64::try_from(m).ok()
}

/// BIN15 S3 (ruling O-2): price one binary on the venue's own settlement
/// — the TWAP of the underlying's mark over `[T − W, T]` — from anywhere
/// in its life.
///
/// * The window lies ahead (`to_expiry_ns ≥ twap_ns`, or `twap_ns == 0`):
///   [`fair_value`] at [`pricing_horizon_ns`] (`τ − 2W/3`).
/// * Inside the window: `a_known_px_ns = ∫_{T−W}^{t} S du` (mark ×1e6 ·
///   ns, the member's running sum, last mark carried forward) has decided
///   part of the average. ITM ⇔ `A + ∫_t^T S du ≥ W·K`; with
///   `∫_t^T S du ≈ τ·S + S·σ·∫_0^τ B_s ds` (variance `S²σ²τ³/3`) the
///   standardised distance is `d = X / (S·σ·√(τ³/3))`, computed as the
///   moneyness [`twap_moneyness_1e9`] over `σ_min·√(horizon)` with the
///   horizon `τ³/(3W²)` — the same variance stated per unit of the
///   average. As τ → 0 the horizon → 0 and `d` → the clamp on the side of
///   `m` (`>=` settles ITM): the known part IS the answer.
///
/// Outside the window `None` exactly where [`fair_value`] refuses. Inside
/// it `None` on a cold forecast (σ² ≤ 0) or where [`twap_moneyness_1e9`]
/// refuses; a variance that floors to zero is NOT a refusal there —
/// nothing is left uncertain, and `d` is the clamp on `m`'s side.
#[inline]
#[must_use]
pub fn fair_value_twap(
    luts: &Bin15Luts,
    mark_1e6: i64,
    strike_1e6: i64,
    to_expiry_ns: u64,
    twap_ns: u64,
    a_known_px_ns: i128,
    sig2_min_1e18: i128,
) -> Option<Fair> {
    if twap_ns == 0 || to_expiry_ns >= twap_ns {
        return fair_value(
            luts,
            mark_1e6,
            strike_1e6,
            pricing_horizon_ns(to_expiry_ns, twap_ns),
            sig2_min_1e18,
        );
    }
    if sig2_min_1e18 <= 0 {
        return None;
    }
    let m_1e9 = twap_moneyness_1e9(a_known_px_ns, mark_1e6, strike_1e6, to_expiry_ns, twap_ns)?;
    let horizon = pricing_horizon_ns(to_expiry_ns, twap_ns);
    let var_1e18 = sig2_min_1e18.saturating_mul(horizon as i128) / 60_000_000_000;
    let den_1e9 = isqrt_i128(var_1e18);
    let d_1e6 = if den_1e9 <= 0 {
        if m_1e9 >= 0 {
            D_CLAMP_1E6
        } else {
            -D_CLAMP_1E6
        }
    } else {
        (floor_div(m_1e9 as i128 * 1_000_000, den_1e9 as i128) as i64)
            .clamp(-D_CLAMP_1E6, D_CLAMP_1E6)
    };
    Some(fair_of_d(luts, d_1e6, den_1e9, horizon))
}

/// Round `px_1e6` DOWN to the venue's 1e-4 grid.
#[inline(always)]
#[must_use]
pub const fn floor_grid_1e6(px_1e6: i64, tick_1e6: i64) -> i64 {
    px_1e6 - px_1e6.rem_euclid(tick_1e6)
}

/// Round `px_1e6` UP to the venue's 1e-4 grid.
#[inline(always)]
#[must_use]
pub const fn ceil_grid_1e6(px_1e6: i64, tick_1e6: i64) -> i64 {
    let r = px_1e6.rem_euclid(tick_1e6);
    if r == 0 {
        px_1e6
    } else {
        px_1e6 + (tick_1e6 - r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real standard-normal Φ table, built here with floats because
    /// the POINT of these tests is to check the integer path against
    /// the reference it replaced. The shipped table comes from
    /// `claude_worker.bin15_fit` and is pinned by the parity fixture.
    fn normal_luts() -> Bin15Luts {
        let mut l = Bin15Luts::identity();
        let mut i = 0usize;
        while i < PHI_POINTS {
            let d = i as f64 / 1_000.0;
            // Φ(d) = (1 + erf(d/√2)) / 2, erf via Abramowitz–Stegun 7.1.26.
            let x = d / core::f64::consts::SQRT_2;
            let t = 1.0 / (1.0 + 0.327_591_1 * x);
            let poly = t
                * (0.254_829_592
                    + t * (-0.284_496_736
                        + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
            let erf = 1.0 - poly * (-x * x).exp();
            l.phi[i] = ((1.0 + erf) / 2.0 * 1e6).round() as u32;
            i += 1;
        }
        l
    }

    #[test]
    fn the_identity_tables_are_the_identity() {
        let l = Bin15Luts::identity();
        assert_eq!(l.phi_1e6(0), 500_000);
        assert_eq!(l.phi_1e6(D_CLAMP_1E6), 500_000);
        let mut p = 0i64;
        while p <= 1_000_000 {
            assert_eq!(l.recal_1e6(PHASE_EARLY, p), p, "identity at p={p}");
            p += RECAL_STEP_1E6;
        }
        assert_eq!(l.recal_1e6(PHASE_LATE, 1_000_000), 1_000_000);
    }

    #[test]
    fn phi_is_monotone_and_interpolates_on_the_grid() {
        let l = normal_luts();
        assert_eq!(l.phi_1e6(0), 500_000, "Φ(0) = 0.5 exactly");
        // Known values, to the table's own 1e-6 resolution.
        assert!((l.phi_1e6(1_000_000) - 841_345).abs() <= 40, "Φ(1) = 0.8413");
        assert!((l.phi_1e6(1_960_000) - 975_002).abs() <= 40, "Φ(1.96) = 0.975");
        assert!((l.phi_1e6(3_000_000) - 998_650).abs() <= 40, "Φ(3) = 0.99865");
        // Monotone across every grid point AND between them.
        let mut d = 0i64;
        let mut prev = l.phi_1e6(0);
        while d <= D_CLAMP_1E6 {
            let v = l.phi_1e6(d);
            assert!(v >= prev, "Φ fell at d={d}: {prev} -> {v}");
            prev = v;
            d += 137; // deliberately off-grid, so interpolation is exercised
        }
        // Halfway between two grid points is halfway between two values.
        let lo = l.phi_1e6(1_000_000);
        let hi = l.phi_1e6(1_001_000);
        assert_eq!(l.phi_1e6(1_000_500), lo + (hi - lo) / 2);
    }

    #[test]
    fn log_moneyness_is_second_order_and_refuses_nonsense() {
        // At the money the log-moneyness is exactly zero.
        assert_eq!(log_moneyness_1e9(79_000_000_000, 79_000_000_000), Some(0));
        // 100 bps up: ln(1.01) = 0.00995033; the 2nd-order form gives
        // 0.01 - 0.00005 = 0.00995, inside 3.4e-7.
        let x = log_moneyness_1e9(79_790_000_000, 79_000_000_000).expect("x");
        assert!((x - 9_950_331).abs() < 400, "ln(1.01) x1e9: got {x}");
        // 100 bps down: ln(0.99) = -0.01005034; 2nd order -0.01005.
        let x = log_moneyness_1e9(78_210_000_000, 79_000_000_000).expect("x");
        assert!((x + 10_050_336).abs() < 400, "ln(0.99) x1e9: got {x}");
        // The sign is the direction, and nonsense is refused rather
        // than priced.
        assert!(log_moneyness_1e9(80_000_000_000, 79_000_000_000).expect("x") > 0);
        assert!(log_moneyness_1e9(78_000_000_000, 79_000_000_000).expect("x") < 0);
        assert_eq!(log_moneyness_1e9(0, 79_000_000_000), None);
        assert_eq!(log_moneyness_1e9(79_000_000_000, 0), None);
        assert_eq!(log_moneyness_1e9(-1, 1), None);
    }

    /// O4b: the square came BEFORE the range check, so a strike small
    /// enough relative to the mark overflowed `i128` — a debug panic and
    /// a wrapped result in release. Found by the parity fixture, which
    /// feeds it deliberately.
    #[test]
    fn an_absurd_strike_is_refused_rather_than_overflowing() {
        // The parity tape's own case: 1e18 against a strike of 1.
        assert_eq!(log_moneyness_1e9(1_000_000_000_000_000_000, 1), None);
        // The realistic one: a `threshold:` that lost its decimal point
        // entirely, against a live BTC mark.
        assert_eq!(log_moneyness_1e9(77_177_000_000, 1), None);
        // The band's own edges: `u = ±U_CLAMP` prices, one unit past it
        // does not. `u = 1e11` is a mark 101x the strike.
        let k = 1_000_000i64;
        assert!(log_moneyness_1e9(k * 101, k).is_some(), "u = 1e11 is inside");
        assert_eq!(log_moneyness_1e9(k * 102, k), None, "u > 1e11 is outside");
        // And the shape the member actually sees is untouched: a mark a
        // few tens of bps from the strike, and the doubling/halving the
        // clamp tests rely on.
        assert!(log_moneyness_1e9(79_790_000_000, 79_000_000_000).is_some());
        assert!(log_moneyness_1e9(158_000_000_000, 79_000_000_000).is_some());
        assert!(log_moneyness_1e9(39_500_000_000, 79_000_000_000).is_some());
    }

    #[test]
    fn the_phase_boundaries_are_where_the_spec_says() {
        assert_eq!(phase_of(900_000_000_000), PHASE_EARLY);
        assert_eq!(phase_of(PHASE_EARLY_NS), PHASE_EARLY, "10 min is early");
        assert_eq!(phase_of(PHASE_EARLY_NS - 1), PHASE_MID);
        assert_eq!(phase_of(PHASE_MID_NS), PHASE_MID, "4 min is mid");
        assert_eq!(phase_of(PHASE_MID_NS - 1), PHASE_LATE);
        assert_eq!(phase_of(0), PHASE_LATE);
    }

    #[test]
    fn fair_value_is_a_half_at_the_money_and_monotone_in_the_mark() {
        let l = normal_luts();
        // σ_min = 10 bps of log-price per minute ⇒ σ² ×1e18 = 1e12.
        let sig2 = 1_000_000_000_000i128;
        let k = 79_000_000_000i64;
        let f = fair_value(&l, k, k, 600_000_000_000, sig2).expect("fair");
        assert_eq!(f.d_1e6, 0);
        assert_eq!(f.p_raw_1e6, 500_000, "at the money a binary is a coin flip");
        assert_eq!(f.p_hat_1e6, 500_000, "identity recal leaves it alone");
        // Monotone in the mark: a higher underlying makes "above the
        // strike" more likely, always.
        let mut prev = 0i64;
        let mut bump = -400_000_000i64;
        while bump <= 400_000_000 {
            let f = fair_value(&l, k + bump, k, 600_000_000_000, sig2).expect("fair");
            assert!(f.p_hat_1e6 >= prev, "p fell at bump={bump}");
            prev = f.p_hat_1e6;
            bump += 10_000_000;
        }
        assert!(prev > 500_000, "and it did move");
    }

    #[test]
    fn a_longer_horizon_pulls_the_price_toward_a_half() {
        let l = normal_luts();
        let sig2 = 1_000_000_000_000i128;
        let k = 79_000_000_000i64;
        let mark = k + 200_000_000; // ~25 bps above
        let near = fair_value(&l, mark, k, 60_000_000_000, sig2).expect("near");
        let far = fair_value(&l, mark, k, 900_000_000_000, sig2).expect("far");
        assert!(
            near.p_hat_1e6 > far.p_hat_1e6,
            "close to expiry a 25 bps lead is nearly settled: near {} far {}",
            near.p_hat_1e6,
            far.p_hat_1e6
        );
        assert!(far.p_hat_1e6 > 500_000, "but still above a coin flip");
    }

    #[test]
    fn absent_data_holds_and_the_clamp_is_symmetric() {
        let l = normal_luts();
        let k = 79_000_000_000i64;
        assert_eq!(fair_value(&l, k, k, 0, 1_000_000_000_000), None, "no horizon");
        assert_eq!(fair_value(&l, k, k, 600_000_000_000, 0), None, "cold forecast");
        assert_eq!(fair_value(&l, k, k, 600_000_000_000, -1), None);
        assert_eq!(fair_value(&l, 0, k, 600_000_000_000, 1), None, "no mark");
        // A mark far above the strike with a tiny vol saturates the
        // clamp rather than indexing past the table.
        let hi = fair_value(&l, k * 2, k, 600_000_000_000, 1).expect("hi");
        assert_eq!(hi.d_1e6, D_CLAMP_1E6);
        let lo = fair_value(&l, k / 2, k, 600_000_000_000, 1).expect("lo");
        assert_eq!(lo.d_1e6, -D_CLAMP_1E6);
        // And the two ends are reflections: p(+d) + p(-d) = 1.
        assert_eq!(hi.p_raw_1e6 + lo.p_raw_1e6, 1_000_000);
    }

    #[test]
    fn the_grid_rounders_go_the_right_way_on_both_signs() {
        assert_eq!(floor_grid_1e6(400_050, 100), 400_000);
        assert_eq!(ceil_grid_1e6(400_050, 100), 400_100);
        assert_eq!(floor_grid_1e6(400_000, 100), 400_000, "already on grid");
        assert_eq!(ceil_grid_1e6(400_000, 100), 400_000, "already on grid");
        // A negative centre can arise from a skew larger than p̂; the
        // rounders must stay on the grid rather than wander off it.
        assert_eq!(floor_grid_1e6(-50, 100) % 100, 0);
        assert_eq!(ceil_grid_1e6(-50, 100) % 100, 0);
        assert!(floor_grid_1e6(-50, 100) <= -50);
        assert!(ceil_grid_1e6(-50, 100) >= -50);
    }

    /// BIN15 S3: the horizon of a TWAP ENDING at `T`.
    #[test]
    fn the_pricing_horizon_is_the_variance_time_of_a_twap_ending_at_expiry() {
        const S: u64 = 1_000_000_000;
        const W: u64 = 60 * S;
        // The window ahead: τ − 2W/3 = τ − 40 s at W = 60 s.
        assert_eq!(pricing_horizon_ns(900 * S, W), 860 * S);
        assert_eq!(pricing_horizon_ns(120 * S, W), 80 * S);
        assert_eq!(pricing_horizon_ns(W, W), 20 * S, "W/3 at the open");
        // Inside: the cubic τ³/(3W²) — 30 s left ⇒ 30³/(3·60²) = 2.5 s.
        assert_eq!(pricing_horizon_ns(30 * S, W), 2_500_000_000);
        // Continuous at the open to within the floor of the division.
        let just_inside = pricing_horizon_ns(W - 1, W);
        assert!((20 * S).abs_diff(just_inside) < 2, "{just_inside}");
        // Nothing left, nothing uncertain.
        assert_eq!(pricing_horizon_ns(0, W), 0);
        // A family settling AT `T` keeps τ.
        assert_eq!(pricing_horizon_ns(123_456, 0), 123_456);
        // No overflow at the extremes the wire allows (u16 seconds).
        let w_max = 65_535 * S;
        assert_eq!(pricing_horizon_ns(u64::MAX, w_max), u64::MAX - (2 * w_max) / 3);
        assert!(pricing_horizon_ns(w_max - 1, w_max) <= w_max / 3);
    }

    /// BIN15 S3: at τ = 120 s the old horizon (τ + W/3 = 140 s) and the
    /// venue's (τ − 2W/3 = 80 s) differ by √(140/80) in `d` — the factor
    /// the late-life belief was too timid by. Asserted by formula.
    #[test]
    fn the_horizon_moves_d_by_the_ratio_of_the_two_variance_times() {
        let l = normal_luts();
        let sig2 = 1_000_000_000_000i128;
        let k = 79_000_000_000i64;
        let mark = k + 100_000_000; // ~12.7 bps above
        let old = fair_value(&l, mark, k, 140_000_000_000, sig2).expect("old");
        let new = fair_value_twap(&l, mark, k, 120_000_000_000, 60_000_000_000, 0, sig2)
            .expect("new");
        let want = old.d_1e6 as f64 * (140.0f64 / 80.0).sqrt();
        assert!(
            (new.d_1e6 as f64 - want).abs() <= 2.0,
            "d {} vs {} × √(140/80) = {want}",
            new.d_1e6,
            old.d_1e6
        );
        assert!(new.p_raw_1e6 > old.p_raw_1e6, "the same lead is surer, later");
        assert_eq!(old.horizon_ns, 140_000_000_000, "fair_value keeps the τ it was given");
        assert_eq!(new.horizon_ns, 80_000_000_000, "and the TWAP law reports its horizon");
    }

    /// BIN15 S3 (O-2): inside the window the part of the average already
    /// behind decides — a TWAP known well above the strike wins even when
    /// the mark has just dipped under it, and vice versa.
    #[test]
    fn inside_the_window_the_known_average_moves_the_price() {
        let l = normal_luts();
        let sig2 = 1_000_000_000_000i128;
        let k = 79_000_000_000i64;
        const S: u64 = 1_000_000_000;
        let w = 60 * S;
        let tau = 20 * S;
        // 40 s behind at +0.5 % over the strike; the mark now 0.1 % under.
        let above = (k + k / 200) as i128 * (40 * S) as i128;
        let mark = k - k / 1_000;
        let f = fair_value_twap(&l, mark, k, tau, w, above, sig2).expect("f");
        assert!(f.p_hat_1e6 > 990_000, "the known part wins: {}", f.p_hat_1e6);
        // The mirror: 40 s behind at −0.5 %, the mark 0.1 % over.
        let below = (k - k / 200) as i128 * (40 * S) as i128;
        let g = fair_value_twap(&l, k + k / 1_000, k, tau, w, below, sig2).expect("g");
        assert!(g.p_hat_1e6 < 10_000, "the known part loses: {}", g.p_hat_1e6);
        // At the open (nothing behind) the two branches meet: the
        // running-TWAP moneyness is the log-moneyness to first order.
        let at_open_in = fair_value_twap(&l, mark, k, w - 1, w, 0, sig2).expect("in");
        let at_open_out = fair_value_twap(&l, mark, k, w, w, 0, sig2).expect("out");
        assert!(
            at_open_in.d_1e6.abs_diff(at_open_out.d_1e6) < 2_000,
            "continuous at τ = W: {} vs {}",
            at_open_in.d_1e6,
            at_open_out.d_1e6
        );
        // Nothing left: the known average IS the settlement.
        let known_above = (k + 1) as i128 * w as i128;
        let done = fair_value_twap(&l, k - 1_000, k, 0, w, known_above, sig2).expect("done");
        assert_eq!(done.d_1e6, D_CLAMP_1E6);
        let known_at = k as i128 * w as i128;
        let tie = fair_value_twap(&l, k, k, 0, w, known_at, sig2).expect("tie");
        assert_eq!(tie.d_1e6, D_CLAMP_1E6, "`>=` settles ITM");
        // Absent data still holds.
        assert_eq!(fair_value_twap(&l, mark, k, tau, w, above, 0), None);
        assert_eq!(fair_value_twap(&l, 0, k, tau, w, above, sig2), None);
        assert_eq!(fair_value_twap(&l, mark, k, tau, w, i128::MAX, sig2), None, "overflow");
    }

    #[test]
    fn recalibration_is_monotone_and_pins_both_ends() {
        // The shipped shape: a slope > 1 through the middle, anchored
        // at 0 and 1 (`bin15_fit` builds it; this checks the reader).
        let mut l = Bin15Luts::identity();
        let mut k = 0usize;
        while k < RECAL_POINTS {
            let p = k as i64 * RECAL_STEP_1E6;
            // p' = clamp(0.5 + 1.104 (p - 0.5)), the EARLY slope.
            let v = 500_000 + (p - 500_000) * 1_104 / 1_000;
            l.recal[PHASE_EARLY][k] = v.clamp(0, 1_000_000) as u32;
            k += 1;
        }
        assert_eq!(l.recal_1e6(PHASE_EARLY, 0), 0);
        assert_eq!(l.recal_1e6(PHASE_EARLY, 1_000_000), 1_000_000);
        assert_eq!(l.recal_1e6(PHASE_EARLY, 500_000), 500_000, "the middle is fixed");
        assert!(
            l.recal_1e6(PHASE_EARLY, 750_000) > 750_000,
            "a slope over 1 pushes confidence outward"
        );
        let mut prev = -1i64;
        let mut p = 0i64;
        while p <= 1_000_000 {
            let v = l.recal_1e6(PHASE_EARLY, p);
            assert!(v >= prev, "recal fell at p={p}");
            prev = v;
            p += 211;
        }
        // Out of range clamps rather than panicking.
        assert_eq!(l.recal_1e6(PHASE_EARLY, -5), 0);
        assert_eq!(l.recal_1e6(PHASE_EARLY, 2_000_000), 1_000_000);
    }
}
