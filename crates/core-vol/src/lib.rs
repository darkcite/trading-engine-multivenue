// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `core-vol` — the integer volatility forecast (VRP V4)
//!
//! One rolling HAR over 1-minute returns, one OLS fit in log space, and
//! the two `i64` bounds the VRP member compares Deribit's `mark_iv_1e9`
//! against. That is the whole crate.
//!
//! ## Why it exists
//!
//! The lane's premise (edge spec §1, "E1") is that short-dated crypto
//! ATM implied vol is systematically beaten by a plain fitted HAR at
//! forecasting realised variance — measured at 4 h and 8 h, absent by
//! 12 h. Everything downstream is a way of monetising that one fact, so
//! the forecast has to be exact, reproducible, and auditable against the
//! Python that fitted it offline. Floats are none of those things across
//! two languages and two compilers, so there are none here.
//!
//! ## The pipeline, with every scaling pinned
//!
//! ```text
//! r_k        = ret_bps_1e9(prev_close, close)             bps ×1e9
//! rv_W       = isqrt( Σ_{k∈W} r_k² )                      W ∈ {60, 240, 1440}
//! har_τ      = isqrt( (rv60²/60 + rv240²/240 + rv1440²/1440) / 3 × τ_min )
//! x          = ln(har_τ)                                  ×1e9
//! y          = ln(realised vol over the hold)              ×1e9, same domain
//! b          = Σ(x−x̄)(y−ȳ) × 1e9 / Σ(x−x̄)²                ×1e9
//! a          = ȳ − b·x̄/1e9                                 ×1e9
//! ln σ̂       = a + b·x/1e9                                  ×1e9
//! iv_lo/hi   = exp(ln σ̂ ∓ θ) × ANNUALISE / 1e13            annualised fraction ×1e9
//! ```
//!
//! `x` and `y` are both `ln` of a RAW `bps×1e9` integer, so the constant
//! `ln(1e13)` they share is absorbed into the intercept `a` and cancels
//! out of every forecast. That is deliberate: it removes a
//! multiplication from the hot boundary and a whole class of
//! scaling-mismatch bug from the parity mirror.
//!
//! ## Doctrine
//!
//! * **No allocation after [`VolEngine::new`].** Every ring is an inline
//!   array; nothing here owns a `Vec`.
//! * **No floats.** `#[cfg(test)]` code may use them — that is where the
//!   integer path is proved against the reference it replaced.
//! * **Transcendentals happen at the per-expiry boundary, never per
//!   tick.** [`VolEngine::on_minute_close`] is an add, a subtract and
//!   three squares; the hot decision downstream is two `i64` compares.
//! * **ABSENT DATA HOLDS.** Fewer than [`MIN_PAIRS`] fitted pairs, or a
//!   minute ring not yet full, and [`VolEngine::bounds`] is `None`. It
//!   never extrapolates, never back-fills, never guesses.
//! * **The 12 h and 24 h cells do not exist.** Kill criterion 4 says
//!   they are not to be traded because E1 is absent there, so
//!   [`tenor_of`] refuses any τ but 4 h and 8 h. A future session that
//!   "extends the horizon" has to delete a line that says why, rather
//!   than pass a bigger number.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod fx;

/// 1-minute return ring. 1536 > 1440 by a whole 96 minutes, so the
/// sample leaving the longest window is always still resident when the
/// newest one is written — that headroom is what makes the rolling sums
/// exact instead of approximate.
pub const MINUTE_RING: usize = 1536;

/// Fitted (x, y) pair ring — one pair per settled expiry.
pub const PAIR_RING: usize = 128;

/// Pairs required before a fit is trusted (build card §2.3).
pub const MIN_PAIRS: usize = 60;

/// Trailing window of the QLIKE comparison — kill criterion 3 is
/// stated over "a trailing 60 expiries", so the ring is 60.
pub const QLIKE_RING: usize = 60;

/// The HAR component windows, in minutes.
pub const HAR_WINDOWS: [usize; 3] = [60, 240, 1440];

/// Nanoseconds in a minute.
pub const MINUTE_NS: u64 = 60_000_000_000;

/// bps ×1e9 per unit of fraction: `frac × 1e4 × 1e9`.
pub const BPS_1E9_PER_UNIT: i128 = 10_000_000_000_000;

/// `sqrt(365.25 × 6) × 1e9` — the 4 h annualiser.
pub const ANNUALISE_4H_1E9: i64 = 46_813_459_603;

/// `sqrt(365.25 × 3) × 1e9` — the 8 h annualiser.
pub const ANNUALISE_8H_1E9: i64 = 33_102_114_736;

/// A tenor the crate will forecast for: its length in minutes and the
/// precomputed `1/sqrt(τ / 1 year)` that turns a realised vol over τ
/// into an annualised fraction. Precomputed because τ is fixed at boot
/// and a runtime `sqrt` on the decision path would be a float.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Tenor {
    /// τ in minutes.
    pub tau_min: i64,
    /// `1/sqrt(τ / 1 year) × 1e9`.
    pub annualise_1e9: i64,
}

/// The ONLY tenors this crate forecasts, and why: E1 is measured at 4 h
/// and 8 h and is gone by 12 h (edge spec §1), so kill criterion 4
/// forbids trading the longer cells. Anything else is `None`.
#[inline]
pub const fn tenor_of(tau_ns: u64) -> Option<Tenor> {
    match tau_ns {
        14_400_000_000_000 => Some(Tenor {
            tau_min: 240,
            annualise_1e9: ANNUALISE_4H_1E9,
        }),
        28_800_000_000_000 => Some(Tenor {
            tau_min: 480,
            annualise_1e9: ANNUALISE_8H_1E9,
        }),
        _ => None,
    }
}

/// The kill-criterion-3 live tell (edge spec §5.3): the trailing-60
/// mean QLIKE of the venue's implied vol against the trailing-60 mean
/// QLIKE of this engine's own forecast. `har_beats_iv == false` once
/// `n` has reached [`QLIKE_RING`] is the halt condition — the mechanism
/// the lane is built on has stopped holding, and the member must stop
/// rather than wait for a later session to notice in a report.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct QlikeCounters {
    /// Settled expiries in the trailing window (saturates at
    /// [`QLIKE_RING`]).
    pub n: u32,
    /// Mean QLIKE of the venue's implied vol ×1e9. Lower is better.
    pub iv_mean_1e9: i64,
    /// Mean QLIKE of this engine's forecast ×1e9. Lower is better.
    pub har_mean_1e9: i64,
    /// `har_mean < iv_mean` over a FULL window. False while the window
    /// is still filling — the tell is not armed until it can be trusted.
    pub har_beats_iv: bool,
}

/// The forecast engine. One instance per underlying; BTC only in v1.
///
/// Roughly 16 KiB of inline rings, so construct it once at boot (behind
/// a `Box` if the caller's frame is tight) and never again. The layout
/// puts the two hot sums and the fit next to each other and pads the
/// whole struct to a cache line.
#[repr(C, align(64))]
pub struct VolEngine {
    /// Rolling `Σ r²` over each of [`HAR_WINDOWS`], in step.
    sum_sq: [i128; 3],
    /// Fitted slope and intercept ×1e9.
    b_1e9: i64,
    a_1e9: i64,
    /// Minutes observed since construction (the global index).
    minutes: u64,
    /// Previous close ×1e6; `0` = no previous close yet.
    prev_px_1e6: i64,
    /// Per-minute returns, bps ×1e9.
    ret_1e9: [i64; MINUTE_RING],
    /// Fitted pairs, ×1e9.
    pair_x_1e9: [i64; PAIR_RING],
    pair_y_1e9: [i64; PAIR_RING],
    /// Trailing QLIKE, ×1e9.
    qlike_iv_1e9: [i64; QLIKE_RING],
    qlike_har_1e9: [i64; QLIKE_RING],
    /// Ring cursors and counts.
    pair_head: usize,
    n_pairs: usize,
    q_head: usize,
    q_n: usize,
    /// The armed hold: the `x` formed at entry, the forecast made from
    /// it, and the implied vol that was quoted against it.
    pend_x_1e9: i64,
    pend_ln_sigma_1e9: i64,
    pend_rv_iv_1e9: i64,
    /// Whether a hold is armed, and whether the fit is usable.
    armed: bool,
    fitted: bool,
}

impl Default for VolEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl VolEngine {
    /// A zeroed engine: no closes, no pairs, no fit, nothing armed.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sum_sq: [0; 3],
            b_1e9: 0,
            a_1e9: 0,
            minutes: 0,
            prev_px_1e6: 0,
            ret_1e9: [0; MINUTE_RING],
            pair_x_1e9: [0; PAIR_RING],
            pair_y_1e9: [0; PAIR_RING],
            qlike_iv_1e9: [0; QLIKE_RING],
            qlike_har_1e9: [0; QLIKE_RING],
            pair_head: 0,
            n_pairs: 0,
            q_head: 0,
            q_n: 0,
            pend_x_1e9: 0,
            pend_ln_sigma_1e9: 0,
            pend_rv_iv_1e9: 0,
            armed: false,
            fitted: false,
        }
    }

    /// Feed one 1-minute close, ×1e6. The first call only seeds
    /// `prev_px`; every later one forms a return and rolls the three
    /// sums in O(1) — an add, three squares and (once the windows fill)
    /// three subtracts. No division, no transcendental, no branch on
    /// data beyond the window-warm tests.
    ///
    /// A non-positive close is IGNORED rather than trusted: it cannot
    /// be a price, and `ret_bps_1e9` divides by it.
    #[inline]
    pub fn on_minute_close(&mut self, px_1e6: i64) {
        if px_1e6 <= 0 {
            return;
        }
        if self.prev_px_1e6 <= 0 {
            self.prev_px_1e6 = px_1e6;
            return;
        }
        let r = core_regime::math::ret_bps_1e9(self.prev_px_1e6, px_1e6);
        self.prev_px_1e6 = px_1e6;

        let k = self.minutes; // this return's global index
        let slot = (k % MINUTE_RING as u64) as usize;
        self.ret_1e9[slot] = r;
        let sq = r as i128 * r as i128;

        let mut w = 0usize;
        while w < HAR_WINDOWS.len() {
            let win = HAR_WINDOWS[w] as u64;
            self.sum_sq[w] += sq;
            if k >= win {
                let out = self.ret_1e9[((k - win) % MINUTE_RING as u64) as usize];
                self.sum_sq[w] -= out as i128 * out as i128;
            }
            w += 1;
        }
        self.minutes = k + 1;
    }

    /// Minutes of return history observed.
    #[inline]
    #[must_use]
    pub const fn minutes(&self) -> u64 {
        self.minutes
    }

    /// Fitted pairs held.
    #[inline]
    #[must_use]
    pub const fn n_pairs(&self) -> usize {
        self.n_pairs
    }

    /// `(a_1e9, b_1e9)` once at least [`MIN_PAIRS`] pairs have been
    /// fitted; `None` before that (ABSENT DATA HOLDS).
    #[inline]
    #[must_use]
    pub const fn fit(&self) -> Option<(i64, i64)> {
        if self.fitted {
            Some((self.a_1e9, self.b_1e9))
        } else {
            None
        }
    }

    /// The HAR forecast of realised vol over τ, in the raw `bps ×1e9`
    /// domain. `None` until the longest window is full — a partial
    /// 1440-minute sum is not a day's realised vol, it is a smaller
    /// window wearing its name.
    #[must_use]
    pub fn har_1e9(&self, tau_ns: u64) -> Option<i64> {
        let t = tenor_of(tau_ns)?;
        let longest = HAR_WINDOWS[HAR_WINDOWS.len() - 1] as u64;
        if self.minutes < longest {
            return None;
        }
        let mut mean_sq: i128 = 0;
        let mut w = 0usize;
        while w < HAR_WINDOWS.len() {
            let rv = core_regime::math::isqrt_i128(self.sum_sq[w]) as i128;
            mean_sq += rv * rv / HAR_WINDOWS[w] as i128;
            w += 1;
        }
        let var_tau = mean_sq / 3 * t.tau_min as i128;
        let har = core_regime::math::isqrt_i128(var_tau);
        if har <= 0 {
            return None;
        }
        Some(har)
    }

    /// `ln σ̂` over τ, ×1e9 — the fitted forecast in the same log domain
    /// as `x` and `y`. `None` until both the ring and the fit are ready.
    #[must_use]
    pub fn ln_sigma_hat_1e9(&self, tau_ns: u64) -> Option<i64> {
        if !self.fitted {
            return None;
        }
        let x = self.x_1e9(tau_ns)?;
        Some(self.a_1e9 + ((self.b_1e9 as i128 * x as i128) / 1_000_000_000) as i64)
    }

    /// The regressor `x = ln(har_τ)` ×1e9. Public because the parity
    /// mirror and the seed cutter form the SAME number and must be able
    /// to prove it.
    #[must_use]
    pub fn x_1e9(&self, tau_ns: u64) -> Option<i64> {
        let har = self.har_1e9(tau_ns)?;
        let x = fx::ln_1e9(har as u64);
        if x == fx::LOG2_UNDEFINED {
            return None;
        }
        Some(x)
    }

    /// The decision bounds: `(iv_lo_1e9, iv_hi_1e9)` as ANNUALISED
    /// fractions ×1e9, directly comparable with Deribit's
    /// `OptSummary::mark_iv_1e9`. `θ` is the band half-width in log
    /// space, ×1e9 (θ = 0.10 ⇒ `100_000_000`).
    ///
    /// `None` whenever anything the forecast rests on is missing: an
    /// unsupported τ, a cold minute ring, or fewer than [`MIN_PAIRS`]
    /// fitted pairs.
    #[must_use]
    pub fn bounds(&self, tau_ns: u64, theta_1e9: i64) -> Option<(i64, i64)> {
        let t = tenor_of(tau_ns)?;
        let ln_sigma = self.ln_sigma_hat_1e9(tau_ns)?;
        let lo = Self::annualised_1e9(ln_sigma - theta_1e9, t.annualise_1e9)?;
        let hi = Self::annualised_1e9(ln_sigma + theta_1e9, t.annualise_1e9)?;
        Some((lo, hi))
    }

    /// `exp(ln_v) × annualise / 1e13` — a log-domain vol over τ turned
    /// into an annualised fraction ×1e9.
    #[inline]
    fn annualised_1e9(ln_v_1e9: i64, annualise_1e9: i64) -> Option<i64> {
        let rv = fx::exp_1e9(ln_v_1e9);
        if rv == 0 || rv == u64::MAX {
            return None;
        }
        let iv = (rv as i128 * annualise_1e9 as i128) / BPS_1E9_PER_UNIT;
        i64::try_from(iv).ok().filter(|v| *v > 0)
    }

    /// Arm the hold that is being entered now: stash the regressor, the
    /// forecast made from it, and the implied vol quoted against it, so
    /// that [`Self::observe_settlement`] can form the pair and score
    /// BOTH forecasts when the expiry settles.
    ///
    /// Returns the armed `x`, or `None` if the ring is not warm — in
    /// which case nothing is armed and the caller must hold.
    pub fn arm_hold(&mut self, tau_ns: u64, mark_iv_1e9: i64) -> Option<i64> {
        let x = self.x_1e9(tau_ns)?;
        let t = tenor_of(tau_ns)?;
        self.pend_x_1e9 = x;
        // The fit may not exist yet; the pair is still worth forming,
        // it just scores no QLIKE this expiry.
        self.pend_ln_sigma_1e9 = match self.ln_sigma_hat_1e9(tau_ns) {
            Some(v) => v,
            None => i64::MIN,
        };
        // Implied vol back into the realised-vol domain: the exact
        // inverse of `annualised_1e9`, so the two forecasts are scored
        // in one domain rather than compared across two.
        self.pend_rv_iv_1e9 = if mark_iv_1e9 > 0 {
            let rv = (mark_iv_1e9 as i128 * BPS_1E9_PER_UNIT) / t.annualise_1e9 as i128;
            i64::try_from(rv).unwrap_or(0)
        } else {
            0
        };
        self.armed = true;
        Some(x)
    }

    /// Whether a hold is armed.
    #[inline]
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        self.armed
    }

    /// Append a pre-formed `(x, y)` pair — the V5 boot seed, where the
    /// worker has already replayed history through the identical
    /// integer law. Refits.
    pub fn seed_pair(&mut self, x_1e9: i64, y_1e9: i64) {
        self.push_pair(x_1e9, y_1e9);
        self.refit();
    }

    /// The hold settled at `realised_rv_1e9` (realised vol over τ, raw
    /// `bps ×1e9`). Forms the pair from the armed `x`, appends it,
    /// refits, and scores both forecasts into the QLIKE window.
    ///
    /// A settlement with nothing armed is IGNORED — there is no `x` to
    /// pair it with, and inventing one is how a lookahead gets in.
    pub fn observe_settlement(&mut self, realised_rv_1e9: i64) {
        if !self.armed || realised_rv_1e9 <= 0 {
            self.armed = false;
            return;
        }
        let y = fx::ln_1e9(realised_rv_1e9 as u64);
        if y == fx::LOG2_UNDEFINED {
            self.armed = false;
            return;
        }
        if self.pend_ln_sigma_1e9 != i64::MIN && self.pend_rv_iv_1e9 > 0 {
            let ln_iv = fx::ln_1e9(self.pend_rv_iv_1e9 as u64);
            if ln_iv != fx::LOG2_UNDEFINED {
                let q_har = qlike_1e9(y, self.pend_ln_sigma_1e9);
                let q_iv = qlike_1e9(y, ln_iv);
                self.qlike_har_1e9[self.q_head] = q_har;
                self.qlike_iv_1e9[self.q_head] = q_iv;
                self.q_head = (self.q_head + 1) % QLIKE_RING;
                if self.q_n < QLIKE_RING {
                    self.q_n += 1;
                }
            }
        }
        self.push_pair(self.pend_x_1e9, y);
        self.refit();
        self.armed = false;
    }

    /// The kill-criterion-3 tell.
    #[must_use]
    pub fn qlike_counters(&self) -> QlikeCounters {
        if self.q_n == 0 {
            return QlikeCounters::default();
        }
        let n = self.q_n as i128;
        let mut si: i128 = 0;
        let mut sh: i128 = 0;
        let mut i = 0usize;
        while i < self.q_n {
            si += self.qlike_iv_1e9[i] as i128;
            sh += self.qlike_har_1e9[i] as i128;
            i += 1;
        }
        let iv = core_regime::math::floor_div(si, n) as i64;
        let har = core_regime::math::floor_div(sh, n) as i64;
        QlikeCounters {
            n: self.q_n as u32,
            iv_mean_1e9: iv,
            har_mean_1e9: har,
            // Armed only on a FULL window: a half-filled comparison is
            // not evidence that a mechanism has died.
            har_beats_iv: self.q_n == QLIKE_RING && har < iv,
        }
    }

    #[inline]
    fn push_pair(&mut self, x_1e9: i64, y_1e9: i64) {
        self.pair_x_1e9[self.pair_head] = x_1e9;
        self.pair_y_1e9[self.pair_head] = y_1e9;
        self.pair_head = (self.pair_head + 1) % PAIR_RING;
        if self.n_pairs < PAIR_RING {
            self.n_pairs += 1;
        }
    }

    /// Closed-form OLS over the pair ring, in `i128`. Cold path: once
    /// per settled expiry, O(128).
    fn refit(&mut self) {
        if self.n_pairs < MIN_PAIRS {
            self.fitted = false;
            return;
        }
        let n = self.n_pairs as i128;
        let mut sx: i128 = 0;
        let mut sy: i128 = 0;
        let mut i = 0usize;
        while i < self.n_pairs {
            sx += self.pair_x_1e9[i] as i128;
            sy += self.pair_y_1e9[i] as i128;
            i += 1;
        }
        let xbar = core_regime::math::floor_div(sx, n);
        let ybar = core_regime::math::floor_div(sy, n);
        let mut sxy: i128 = 0;
        let mut sxx: i128 = 0;
        i = 0;
        while i < self.n_pairs {
            let dx = self.pair_x_1e9[i] as i128 - xbar;
            let dy = self.pair_y_1e9[i] as i128 - ybar;
            sxy += dx * dy;
            sxx += dx * dx;
            i += 1;
        }
        if sxx == 0 {
            // Every regressor identical: the slope is undefined, and a
            // fit that cannot see x is not a forecast.
            self.fitted = false;
            return;
        }
        let b = core_regime::math::floor_div(sxy * 1_000_000_000, sxx);
        let a = ybar - core_regime::math::floor_div(b * xbar, 1_000_000_000);
        self.b_1e9 = i64::try_from(b).unwrap_or(0);
        self.a_1e9 = i64::try_from(a).unwrap_or(0);
        self.fitted = i128::from(self.b_1e9) == b && i128::from(self.a_1e9) == a;
    }
}

/// `QLIKE(σ̂², rv²) = u − ln u − 1` where `u = rv²/σ̂²`, in ×1e9.
///
/// Both arguments are natural logs of a vol in the SAME raw domain, so
/// `ln u = 2(ln rv − ln σ̂)` and the domain's constant offset cancels —
/// which is why this needs one exponential and no division.
#[inline]
#[must_use]
pub fn qlike_1e9(ln_rv_1e9: i64, ln_sigma_1e9: i64) -> i64 {
    let ln_u = (ln_rv_1e9 - ln_sigma_1e9).saturating_mul(2);
    // ×1e9 output via the scaling identity in `fx`.
    let arg = (ln_u as i128 * 1_000_000_000) / fx::LN2_1E9 as i128 + fx::LOG2_1E9_1E9 as i128;
    let u = if arg < 0 {
        0
    } else if arg > i64::MAX as i128 {
        return i64::MAX;
    } else {
        fx::exp2_1e9(arg as i64)
    };
    let u = i64::try_from(u).unwrap_or(i64::MAX);
    u.saturating_sub(ln_u).saturating_sub(1_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAU_8H: u64 = 28_800_000_000_000;
    const TAU_4H: u64 = 14_400_000_000_000;
    const THETA: i64 = 100_000_000; // θ = 0.10

    /// A deterministic integer walk — no rng, no float, reproducible on
    /// any host. `step` cycles a small pattern so the three windows see
    /// genuinely different variance.
    fn walk(e: &mut VolEngine, minutes: usize, seed: i64) {
        let mut px = 79_000_000_000i64; // $79,000 ×1e6
        let mut s = seed;
        let mut i = 0usize;
        while i < minutes {
            // A 64-bit LCG's high bits, used as an integer step in
            // ×1e6 dollars. Test-only; nothing here is a model.
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let step = ((s >> 40) % 40_000_000) - 20_000_000;
            px += step;
            if px < 1_000_000_000 {
                px = 1_000_000_000;
            }
            e.on_minute_close(px);
            i += 1;
        }
    }

    #[test]
    fn only_the_measured_tenors_exist() {
        // E1 lives at 4 h and 8 h and is gone by 12 h (edge spec §1);
        // kill criterion 4 forbids the longer cells. The crate refuses
        // them rather than trusting a caller.
        assert!(tenor_of(TAU_4H).is_some());
        assert!(tenor_of(TAU_8H).is_some());
        assert!(tenor_of(43_200_000_000_000).is_none(), "12 h is not traded");
        assert!(tenor_of(86_400_000_000_000).is_none(), "24 h is not traded");
        assert!(tenor_of(0).is_none());
        assert_eq!(tenor_of(TAU_8H).unwrap().tau_min, 480);
        assert_eq!(tenor_of(TAU_4H).unwrap().tau_min, 240);
    }

    #[test]
    fn absent_data_holds() {
        let mut e = VolEngine::new();
        assert_eq!(e.bounds(TAU_8H, THETA), None, "cold engine");
        assert_eq!(e.har_1e9(TAU_8H), None);
        assert_eq!(e.fit(), None);

        // Ring warm, no pairs: still no bounds.
        walk(&mut e, 1_500, 1);
        assert!(e.har_1e9(TAU_8H).is_some(), "the HAR itself is ready");
        assert_eq!(e.bounds(TAU_8H, THETA), None, "no fit yet");

        // MIN_PAIRS − 1 pairs: still nothing.
        let x = e.x_1e9(TAU_8H).expect("x");
        let mut i = 0usize;
        while i < MIN_PAIRS - 1 {
            e.seed_pair(x + i as i64 * 1_000_000, x + i as i64 * 900_000);
            i += 1;
        }
        assert_eq!(e.n_pairs(), MIN_PAIRS - 1);
        assert_eq!(e.fit(), None, "59 pairs is not 60");
        assert_eq!(e.bounds(TAU_8H, THETA), None);

        // The 60th arms it.
        e.seed_pair(x + 59_000_000, x + 53_100_000);
        assert!(e.fit().is_some(), "60 pairs fits");
        assert!(e.bounds(TAU_8H, THETA).is_some());
    }

    #[test]
    fn rolling_sums_match_a_brute_force_recompute() {
        // The whole O(1) roll is worth nothing if it drifts from the
        // definition, and drift only shows up after the ring wraps —
        // so walk well past MINUTE_RING.
        let mut e = VolEngine::new();
        let mut rets: Vec<i64> = Vec::new();
        let mut px = 79_000_000_000i64;
        let mut s = 7i64;
        e.on_minute_close(px); // seeds prev_px, forms no return
        let mut i = 0usize;
        while i < 4_000 {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let step = ((s >> 40) % 40_000_000) - 20_000_000;
            let next = (px + step).max(1_000_000_000);
            rets.push(core_regime::math::ret_bps_1e9(px, next));
            e.on_minute_close(next);
            px = next;
            i += 1;
        }
        assert_eq!(e.minutes() as usize, rets.len());
        let mut w = 0usize;
        while w < HAR_WINDOWS.len() {
            let win = HAR_WINDOWS[w];
            let want: i128 = rets[rets.len() - win..]
                .iter()
                .map(|r| *r as i128 * *r as i128)
                .sum();
            assert_eq!(e.sum_sq[w], want, "window {win} drifted after a wrap");
            w += 1;
        }
    }

    #[test]
    fn the_fit_recovers_a_known_line() {
        // y = 0.8x + 2e9 exactly: the closed form must find it back to
        // the fixed point's own resolution.
        let mut e = VolEngine::new();
        let mut i = 0i64;
        while i < 80 {
            let x = 30_000_000_000 + i * 25_000_000;
            let y = 2_000_000_000 + (x * 8) / 10;
            e.seed_pair(x, y);
            i += 1;
        }
        let (a, b) = e.fit().expect("fitted");
        assert!((b - 800_000_000).abs() <= 10, "b = 0.8: got {b}");
        assert!((a - 2_000_000_000).abs() <= 400, "a = 2.0: got {a}");
    }

    #[test]
    fn a_degenerate_regressor_refuses_to_fit() {
        // Every x identical ⇒ the slope is undefined. A forecast that
        // cannot see x is not a forecast, so it holds.
        let mut e = VolEngine::new();
        let mut i = 0usize;
        while i < 70 {
            e.seed_pair(30_000_000_000, 25_000_000_000 + i as i64);
            i += 1;
        }
        assert_eq!(e.fit(), None);
    }

    #[test]
    fn a_settlement_with_nothing_armed_is_ignored() {
        // The lookahead guard: a pair can only ever be formed from an x
        // that was armed BEFORE the hold. No arm, no pair.
        let mut e = VolEngine::new();
        walk(&mut e, 1_500, 3);
        assert!(!e.is_armed());
        e.observe_settlement(1_200_000_000_000);
        assert_eq!(e.n_pairs(), 0, "no x to pair it with");

        let x = e.arm_hold(TAU_8H, 600_000_000).expect("armed");
        assert!(e.is_armed());
        e.observe_settlement(1_200_000_000_000);
        assert_eq!(e.n_pairs(), 1);
        assert!(!e.is_armed(), "settlement disarms");
        assert_eq!(e.pair_x_1e9[0], x);
    }

    #[test]
    fn arming_a_cold_engine_arms_nothing() {
        let mut e = VolEngine::new();
        assert_eq!(e.arm_hold(TAU_8H, 600_000_000), None);
        assert!(!e.is_armed());
        // And an unsupported tenor never arms either.
        walk(&mut e, 1_500, 5);
        assert_eq!(e.arm_hold(86_400_000_000_000, 600_000_000), None);
        assert!(!e.is_armed());
    }

    #[test]
    fn bounds_bracket_the_forecast_and_widen_with_theta() {
        let mut e = VolEngine::new();
        walk(&mut e, 1_600, 11);
        let x = e.x_1e9(TAU_8H).expect("x");
        let mut i = 0usize;
        while i < MIN_PAIRS {
            e.seed_pair(x - 200_000_000 + i as i64 * 7_000_000, x + i as i64 * 6_000_000);
            i += 1;
        }
        let (lo, hi) = e.bounds(TAU_8H, THETA).expect("bounds");
        assert!(lo > 0 && hi > lo, "lo {lo} hi {hi}");
        let (lo2, hi2) = e.bounds(TAU_8H, 2 * THETA).expect("bounds");
        assert!(lo2 < lo && hi2 > hi, "a wider θ widens the band");
        // θ = 0 collapses the band onto the forecast itself.
        let (lo0, hi0) = e.bounds(TAU_8H, 0).expect("bounds");
        assert_eq!(lo0, hi0);
        assert!(lo0 > lo && lo0 < hi);
        // The band is multiplicative in log space: hi/lo = e^{2θ}.
        let ratio_1e9 = (hi as i128 * 1_000_000_000 / lo as i128) as i64;
        let e2t = fx::exp2_1e9(
            ((2 * THETA) as i128 * 1_000_000_000 / fx::LN2_1E9 as i128) as i64
                + fx::LOG2_1E9_1E9,
        ) as i64;
        let drift = (ratio_1e9 - e2t).abs();
        assert!(drift * 10_000 < e2t, "hi/lo must be e^{{2θ}}: {ratio_1e9} vs {e2t}");
    }

    #[test]
    fn an_8h_forecast_reads_as_an_annualised_fraction() {
        // Sanity of the whole scaling chain: a real-looking BTC tape
        // must produce an annualised IV in the tens of percent, not
        // 0.02 (tenor-scaled) and not 40 (bps left unconverted).
        let mut e = VolEngine::new();
        walk(&mut e, 1_600, 21);
        let x = e.x_1e9(TAU_8H).expect("x");
        let mut i = 0usize;
        while i < MIN_PAIRS {
            // y ≈ x: the forecast is the HAR itself, unshrunk.
            e.seed_pair(x + i as i64 * 3_000_000, x + i as i64 * 3_000_000);
            i += 1;
        }
        let (lo, hi) = e.bounds(TAU_8H, THETA).expect("bounds");
        assert!(
            (30_000_000..3_000_000_000).contains(&lo),
            "lo {lo} is not a plausible annualised IV ×1e9"
        );
        assert!((30_000_000..3_000_000_000).contains(&hi), "hi {hi}");
    }

    #[test]
    fn qlike_scores_the_better_forecast_lower() {
        // QLIKE is a loss: a forecast that lands on the realisation
        // scores 0, and error in EITHER direction scores positive.
        let rv = fx::ln_1e9(1_200_000_000_000);
        // QLIKE has its minimum 0 at u = 1 and is quadratic there, so
        // the table's ~7e-7 residue on `u` shows up as a few hundred
        // parts in 1e9 rather than an exact zero. Pin the residue as a
        // bound: if it ever grows, the interpolation changed.
        let exact = qlike_1e9(rv, rv);
        assert!((0..1_000).contains(&exact), "an exact forecast is ~free: {exact}");
        let over = qlike_1e9(rv, rv + 200_000_000);
        let under = qlike_1e9(rv, rv - 200_000_000);
        assert!(over > exact && under > exact, "over {over} under {under}");
        let worse = qlike_1e9(rv, rv + 600_000_000);
        assert!(worse > over, "a worse forecast scores higher");
    }

    #[test]
    fn the_kill_tell_arms_only_on_a_full_window() {
        let mut e = VolEngine::new();
        walk(&mut e, 1_600, 31);
        let c0 = e.qlike_counters();
        assert_eq!(c0.n, 0);
        assert!(!c0.har_beats_iv, "an empty window is not evidence");

        // Seed a fit, then settle a window's worth of expiries where
        // the HAR forecast is deliberately closer than the IV.
        let x = e.x_1e9(TAU_8H).expect("x");
        let mut i = 0usize;
        while i < MIN_PAIRS {
            e.seed_pair(x + i as i64 * 2_000_000, x + i as i64 * 2_000_000);
            i += 1;
        }
        let mut settled = 0usize;
        while settled < QLIKE_RING - 1 {
            // Quote an implied vol far above the forecast, then settle
            // near the forecast: HAR wins every expiry.
            e.arm_hold(TAU_8H, 2_000_000_000).expect("armed");
            let rv = fx::exp_1e9(e.ln_sigma_hat_1e9(TAU_8H).expect("fit")) as i64;
            e.observe_settlement(rv);
            settled += 1;
        }
        let c1 = e.qlike_counters();
        assert_eq!(c1.n as usize, QLIKE_RING - 1);
        assert!(!c1.har_beats_iv, "59 of 60 is not a trailing 60");
        assert!(c1.har_mean_1e9 < c1.iv_mean_1e9, "HAR is in fact better");

        e.arm_hold(TAU_8H, 2_000_000_000).expect("armed");
        let rv = fx::exp_1e9(e.ln_sigma_hat_1e9(TAU_8H).expect("fit")) as i64;
        e.observe_settlement(rv);
        let c2 = e.qlike_counters();
        assert_eq!(c2.n as usize, QLIKE_RING);
        assert!(c2.har_beats_iv, "a full window of wins arms the tell");
    }

    #[test]
    fn a_non_positive_close_is_ignored_not_trusted() {
        let mut e = VolEngine::new();
        e.on_minute_close(0);
        e.on_minute_close(-5);
        assert_eq!(e.minutes(), 0);
        e.on_minute_close(79_000_000_000);
        assert_eq!(e.minutes(), 0, "the first close only seeds prev_px");
        e.on_minute_close(79_100_000_000);
        assert_eq!(e.minutes(), 1);
        e.on_minute_close(0);
        assert_eq!(e.minutes(), 1, "a bad close cannot advance the ring");
    }
}
