// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The fixed-point scales of ruling D18 — the pine boundary's units match
//! the engine's exactly, so there is no conversion layer, only these
//! helpers (contract §3.4, §3.15):
//!
//! | quantity | type | scale | engine counterpart |
//! |---|---|---|---|
//! | price | `i64` | ×1e6 | `core_types::Price` |
//! | quantity, volume | `i64` | ×1e6 | `core_types::Qty` |
//! | money: notional, fee, P&L, equity (computation) | `i128` | ×1e12 | `cli::backtest::fill` §4.5 |
//! | money: configuration and report fields | `i64` | USD ×1e6 | `exec.toml` caps, `TrialSummary` |
//! | percent | `i64` | ×1e6 | — |
//! | Pine-visible time | `i64` | UTC ms | — (engine events are ns) |
//!
//! The VM computes in `f64` internally; every f64 ↔ fixed crossing goes
//! through [`to_1e6`] / [`from_1e6`] / [`from_1e12`] / [`to_tick_band_1e6`],
//! and nowhere else.
//!
//! The directional ×1e12 → ×1e6 renderings reproduce
//! `cli::backtest::fill::usd_1e12_to_1e6_floor` / `_ceil` exactly (a
//! `pine-*` crate may not depend on `cli`; `tests/contract.rs` pins the
//! same vectors): **floor** for P&L (the net direction), **ceil** for risk
//! (caps, fees).

/// Price scale.
pub const PX: i64 = 1_000_000;
/// Quantity / volume scale.
pub const QTY: i64 = 1_000_000;
/// Configuration / report USD scale.
pub const USD: i64 = 1_000_000;
/// Percent scale.
pub const PCT: i64 = 1_000_000;
/// Computation money scale (px ×1e6 · qty ×1e6).
pub const MONEY: i128 = 1_000_000_000_000;
/// Nanoseconds per millisecond.
pub const NS_PER_MS: i64 = 1_000_000;

/// f64 → ×1e6, **round half away from zero**, saturating at the `i64`
/// bounds; `None` for NaN (`na`). This is the one f64 → fixed crossing of
/// the lane: every price, quantity and percent an intent carries passes
/// through it.
#[inline]
#[must_use]
pub fn to_1e6(x: f64) -> Option<i64> {
    if x.is_nan() {
        return None;
    }
    // `f64::round` is half-away-from-zero; `as i64` saturates (±inf too).
    Some((x * 1e6).round() as i64)
}

/// ×1e6 → f64. Correctly rounded (one division).
#[inline]
#[must_use]
pub fn from_1e6(v: i64) -> f64 {
    v as f64 / 1e6
}

/// ×1e12 → f64 (a money read-back for a script). Correctly rounded for
/// magnitudes below 2^53 ×1e-12 USD.
#[inline]
#[must_use]
pub fn from_1e12(v: i128) -> f64 {
    v as f64 / 1e12
}

/// Notional of `qty_1e6` at `px_1e6`, ×1e12. Exact: the product of two
/// `i64` always fits an `i128`.
#[inline]
#[must_use]
pub const fn notional_1e12(px_1e6: i64, qty_1e6: i64) -> i128 {
    px_1e6 as i128 * qty_1e6 as i128
}

/// USD ×1e6 → ×1e12. Exact.
#[inline]
#[must_use]
pub const fn usd_1e6_to_1e12(v: i64) -> i128 {
    v as i128 * 1_000_000
}

/// ×1e12 → USD ×1e6, rounded toward −∞ (the net P&L direction),
/// saturating.
#[inline]
#[must_use]
pub const fn e12_to_1e6_floor(v: i128) -> i64 {
    sat_i64(v.div_euclid(1_000_000))
}

/// ×1e12 → USD ×1e6, rounded toward +∞ (the risk direction), saturating.
#[inline]
#[must_use]
pub const fn e12_to_1e6_ceil(v: i128) -> i64 {
    if v == i128::MIN {
        return i64::MIN;
    }
    sat_i64(-((-v).div_euclid(1_000_000)))
}

/// Saturate an `i128` into `i64`.
#[inline]
#[must_use]
pub const fn sat_i64(x: i128) -> i64 {
    if x > i64::MAX as i128 {
        i64::MAX
    } else if x < i64::MIN as i128 {
        i64::MIN
    } else {
        x as i64
    }
}

/// Event ns → Pine ms, floor (a bar boundary never rounds up into the next
/// millisecond).
#[inline]
#[must_use]
pub const fn ms_from_ns(ns: i64) -> i64 {
    ns.div_euclid(NS_PER_MS)
}

/// Pine ms → ns, saturating.
#[inline]
#[must_use]
pub const fn ns_from_ms(ms: i64) -> i64 {
    ms.saturating_mul(NS_PER_MS)
}

/// R-band's edge, as a denominator: a value within `1 / BAND_DEN` of a tick below the half-tick
/// tie rounds as the tie (`1e-10` ticks, CAR-R33-7's constant).
pub const BAND_DEN: u128 = 10_000_000_000;

/// f64 → the nearest multiple of `tick_1e6`, ×1e6, by **R-band** (TradingView F123, capture 08,
/// r39): computed on the **exact** binary value of `|x|` — no f64 step — `|x|` rounds away from
/// zero iff it is at or above the half-tick tie, or below it by at most `1e-10` ticks
/// ([`BAND_DEN`]); otherwise it rounds toward zero. `neg_tie_toward_zero` is U04's `half_up`
/// reading for a negative `x` — ties toward +inf, so the band mirrors: away from zero iff above
/// the tie by more than `1e-10` ticks; U04's decided `half_away` passes `false`.
/// `None` for NaN, ±inf, `tick_1e6 < 1`, and any `x` whose ×1e6 leaves `i64` (the domain of
/// [`to_1e6`]).
///
/// No f64 formula fits F123's fourteen readings (`round(x / mintick) · mintick`, `to_1e6` then
/// an integer tie and a 1e-10 additive nudge each miss at least one); this exact rational one
/// fits all of them (`pin:06-r34-band-formation`).
#[must_use]
pub fn to_tick_band_1e6(x: f64, tick_1e6: i64, neg_tie_toward_zero: bool) -> Option<i64> {
    if !x.is_finite() || tick_1e6 < 1 || x.abs() * 1e6 >= 9.223_372_036_854_775e18 {
        return None;
    }
    if x == 0.0 {
        return Some(0);
    }
    // |x| = m · 2^-s exactly; in the domain above s ≥ 9 always (|x| < 2^44, m ≥ 2^52 or
    // subnormal).
    let bits = x.abs().to_bits();
    let (exp, frac) = ((bits >> 52) & 0x7ff, bits & ((1u64 << 52) - 1));
    let (m, e) = if exp == 0 {
        (u128::from(frac), -1074i64)
    } else {
        (u128::from(frac | (1u64 << 52)), exp as i64 - 1075)
    };
    debug_assert!(e < 0, "to_tick_band_1e6: |x| in the domain is below 2^44");
    let s = (-e) as u32;
    // ticks = |x| · 1e6 / tick = a / d, a = m · 1e6 < 2^73, d = tick · 2^s.
    let tick = tick_1e6 as u128;
    let tick_bits = 128 - tick.leading_zeros();
    if tick_bits + s > 100 {
        // d ≥ 2^100 > 2^27 · a: |x| is below 2^-27 ticks, half a tick from any tie.
        return Some(0);
    }
    let (a, d) = (m * 1_000_000, tick << s);
    let (q, r) = (a / d, a % d);
    // The fraction f = r / d of a tick; the tie is f = 1/2, a gap of |2r − d| / 2d ticks.
    let within = |gap: u128| gap.checked_mul(BAND_DEN).is_some_and(|g| g <= 2 * d);
    let up = if r == 0 {
        false
    } else if neg_tie_toward_zero && x < 0.0 {
        // The mirror (ties toward +inf): away from zero iff f > 1/2 + 1e-10.
        2 * r > d && !within(2 * r - d)
    } else {
        // Away from zero iff f ≥ 1/2 − 1e-10.
        2 * r >= d || within(d - 2 * r)
    };
    let q = if up { q + 1 } else { q };
    let v = i64::try_from(q).ok()?.checked_mul(tick_1e6)?;
    Some(if x < 0.0 { -v } else { v })
}
