// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The only arithmetic the crate does on money (impl-05 §4.1, D18), all over `pine_ir::scale`.
//!
//! Laws (REVIEW §5, "cap bypass by overflow"): every notional is an `i128` ×1e12 from
//! [`value_1e12`]; an overflowing product saturates **away from zero**, so a saturated notional is
//! ≥ every cap and the order is refused (I6); no lossy cast (the cast lints are denied crate-wide);
//! truncation toward zero unless a rule says otherwise; quantities floor to a step, never up (I5).
//! Every function here is total: no input panics.

pub(crate) use pine_ir::scale::{e12_to_1e6_ceil, e12_to_1e6_floor, sat_i64, usd_1e6_to_1e12};

/// The ×1e6 scale as an `i128` factor.
pub(crate) const E6: i128 = 1_000_000;

/// The saturation bound: `±i128::MAX` (symmetric, so a saturated value always negates safely).
const SAT: i128 = i128::MAX;

/// `±SAT` by the sign of `a·b/d` (`0` when `a` or `b` is zero).
const fn sat_by_sign(a: i128, b: i128, d: i128) -> i128 {
    if a == 0 || b == 0 {
        return 0;
    }
    let neg = (a < 0) ^ (b < 0) ^ (d < 0);
    if neg {
        -SAT
    } else {
        SAT
    }
}

/// a·b/d, truncated toward zero; an overflowing product saturates to ±i128::MAX by sign.
///
/// A zero divisor saturates the same way (impl-05 §4.1 law 2, "saturation blocks"); the result is
/// always within `[-i128::MAX, i128::MAX]`.
pub(crate) const fn mul_div(a: i128, b: i128, d: i128) -> i128 {
    // Ruling Q1: a zero divisor is a caller defect — fail fast in tests and debug backtests; a
    // release build still blocks (saturates) rather than passes.
    debug_assert!(d != 0, "mul_div: zero divisor");
    let p = match a.checked_mul(b) {
        Some(p) => p,
        None => return sat_by_sign(a, b, d),
    };
    match p.checked_div(d) {
        // `checked_div` truncates toward zero; only `i128::MIN / 1` can land on `i128::MIN`.
        Some(q) if q == i128::MIN => -SAT,
        Some(q) => q,
        None => sat_by_sign(a, b, d),
    }
}

/// Value of `qty` at `px` with point value `pv`, ×1e12: mul_div(scale::notional_1e12(px, qty), pv, E6).
pub(crate) const fn value_1e12(px_1e6: i64, qty_1e6: i64, pv_1e6: i64) -> i128 {
    mul_div(
        pine_ir::scale::notional_1e12(px_1e6, qty_1e6),
        pv_1e6 as i128,
        E6,
    )
}

/// `pct` (×1e6) of `v`: mul_div(v, pct, 100·E6).
pub(crate) const fn pct_1e12(v_1e12: i128, pct_1e6: i64) -> i128 {
    mul_div(v_1e12, pct_1e6 as i128, 100 * E6)
}

/// A9 — `v` as a percentage of one trade's base, ×1e6: `v / (entry_px × |qty|) × 100`, i.e.
/// sat_i64(mul_div(v, 100·E6, entry_px·|qty|)), truncated toward zero; `None` on a zero base.
/// The base is `pine-std`'s per-trade percent accessors' (`profit_percent`, `max_runup_percent`,
/// `max_drawdown_percent`): entry price × |size|, no point value (`pin:04-trade-pct-provisional`).
pub(crate) const fn trade_pct_1e6(v_1e12: i128, entry_px_1e6: i64, qty_1e6: i64) -> Option<i64> {
    // `|i64| · |i64| < 2^126`: the product of two `i64` always fits an `i128`.
    let q = qty_1e6 as i128;
    let base = entry_px_1e6 as i128 * if q < 0 { -q } else { q };
    if base == 0 {
        return None;
    }
    Some(sat_i64(mul_div(v_1e12, 100 * E6, base)))
}

/// Contracts ×1e6 that `v` buys at `px` with `pv`: sat_i64(mul_div(v, E6, px·pv)); 0 if px·pv ≤ 0.
pub(crate) const fn qty_for_value(v_1e12: i128, px_1e6: i64, pv_1e6: i64) -> i64 {
    // The product of two `i64` always fits an `i128`.
    let den = px_1e6 as i128 * pv_1e6 as i128;
    if den <= 0 {
        return 0;
    }
    sat_i64(mul_div(v_1e12, E6, den))
}

/// `ticks` (×1e6) of `mintick` as a price distance: sat_i64(mul_div(ticks, mintick, E6)).
pub(crate) const fn ticks_px(ticks_1e6: i64, mintick_1e6: i64) -> i64 {
    sat_i64(mul_div(ticks_1e6 as i128, mintick_1e6 as i128, E6))
}

/// Largest multiple of `step` not larger in magnitude, sign kept; identity for step ≤ 0.
pub(crate) const fn floor_step(q_1e6: i64, step_1e6: i64) -> i64 {
    if step_1e6 <= 0 {
        return q_1e6;
    }
    // `%` truncates toward zero, so the remainder has `q`'s sign and `|q − r| ≤ |q|`; with
    // `step > 0` neither operation can overflow.
    q_1e6 - q_1e6 % step_1e6
}

/// Round a price to `tick`, half away from zero; identity for tick ≤ 0. Saturates at the `i64`
/// bounds.
pub(crate) const fn round_tick(px_1e6: i64, tick_1e6: i64) -> i64 {
    if tick_1e6 <= 0 {
        return px_1e6;
    }
    let px = px_1e6 as i128;
    let t = tick_1e6 as i128;
    let r = px % t;
    let base = px - r;
    let twice = if r < 0 { -2 * r } else { 2 * r };
    if twice < t {
        sat_i64(base)
    } else if px < 0 {
        sat_i64(base - t)
    } else {
        sat_i64(base + t)
    }
}

/// Round a price down (toward −∞) to `tick`; identity for tick ≤ 0. Saturates at the `i64` bounds.
pub(crate) const fn floor_tick(px_1e6: i64, tick_1e6: i64) -> i64 {
    if tick_1e6 <= 0 {
        return px_1e6;
    }
    let t = tick_1e6 as i128;
    sat_i64((px_1e6 as i128).div_euclid(t) * t)
}

/// Round a price up (toward +∞) to `tick`; identity for tick ≤ 0. Saturates at the `i64` bounds.
pub(crate) const fn ceil_tick(px_1e6: i64, tick_1e6: i64) -> i64 {
    if tick_1e6 <= 0 {
        return px_1e6;
    }
    let t = tick_1e6 as i128;
    sat_i64(-((-(px_1e6 as i128)).div_euclid(t) * t))
}

#[cfg(test)]
mod tests {
    use super::{
        ceil_tick, e12_to_1e6_ceil, e12_to_1e6_floor, floor_step, floor_tick, mul_div, pct_1e12,
        qty_for_value, round_tick, sat_i64, ticks_px, trade_pct_1e6, usd_1e6_to_1e12, value_1e12,
        E6, SAT,
    };
    use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest, ProptestConfig};

    #[test]
    fn trade_pct_is_value_over_entry_notional_truncated() {
        // $2.50 on 2 × $100 = 1.25 %; a loss keeps its sign; a short's base is |qty|.
        let v = 2_500_000_000_000;
        assert_eq!(
            trade_pct_1e6(v, 100 * 1_000_000, 2_000_000),
            Some(1_250_000)
        );
        assert_eq!(
            trade_pct_1e6(-v, 100 * 1_000_000, -2_000_000),
            Some(-1_250_000)
        );
        // 1/3 % truncates toward zero.
        assert_eq!(
            trade_pct_1e6(1_000_000_000_000, 300 * 1_000_000, 1_000_000),
            Some(333_333)
        );
        assert_eq!(
            trade_pct_1e6(-1_000_000_000_000, 300 * 1_000_000, 1_000_000),
            Some(-333_333)
        );
        assert_eq!(trade_pct_1e6(v, 0, 2_000_000), None);
        assert_eq!(trade_pct_1e6(i128::MAX, 1, 1), Some(i64::MAX));
    }

    #[test]
    fn mul_div_truncates_toward_zero() {
        assert_eq!(mul_div(7, 1, 2), 3);
        assert_eq!(mul_div(-7, 1, 2), -3);
        assert_eq!(mul_div(7, -1, 2), -3);
        assert_eq!(mul_div(7, 1, -2), -3);
        assert_eq!(mul_div(-7, -1, -2), -3);
        assert_eq!(mul_div(1, 1, 3), 0);
        assert_eq!(mul_div(-1, 1, 3), 0);
    }

    #[test]
    fn mul_div_saturates_by_sign() {
        assert_eq!(mul_div(i128::MAX, 2, 1), SAT);
        assert_eq!(mul_div(i128::MAX, -2, 1), -SAT);
        assert_eq!(mul_div(i128::MAX, 2, -1), -SAT);
        assert_eq!(mul_div(i128::MIN, 2, 7), -SAT);
        assert_eq!(mul_div(i128::MIN, -2, 7), SAT);
        // `MIN / −1` and `MIN / 1` stay within the symmetric bound.
        assert_eq!(mul_div(i128::MIN, 1, -1), SAT);
        assert_eq!(mul_div(i128::MIN, 1, 1), -SAT);
        // A saturated notional is above every cap a configuration can state.
        assert!(value_1e12(i64::MAX, i64::MAX, i64::MAX) >= usd_1e6_to_1e12(i64::MAX));
        assert!(value_1e12(i64::MAX, i64::MIN, i64::MAX) <= -usd_1e6_to_1e12(i64::MAX));
    }

    /// Ruling Q1: debug builds fail fast on a zero divisor.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "mul_div: zero divisor")]
    fn mul_div_zero_divisor_fails_fast_in_debug() {
        let _ = mul_div(5, 3, 0);
    }

    /// Release builds block (saturate by sign) rather than pass (law 2).
    #[cfg(not(debug_assertions))]
    #[test]
    fn mul_div_zero_divisor_blocks_in_release() {
        assert_eq!(mul_div(5, 3, 0), SAT);
        assert_eq!(mul_div(-5, 3, 0), -SAT);
        assert_eq!(mul_div(0, 3, 0), 0);
    }

    #[test]
    fn value_is_exact_below_overflow() {
        // 2.5 contracts at 40 000.123456 with point value 1 → 100 000.30864 USD.
        let v = value_1e12(40_000_123_456, 2_500_000, 1_000_000);
        assert_eq!(v, 40_000_123_456_i128 * 2_500_000);
        // Point value 50 (an index future): 1 contract at 5 000 → 250 000 USD.
        assert_eq!(
            value_1e12(5_000_000_000, 1_000_000, 50_000_000),
            250_000 * E6 * E6
        );
        // 12.5 % of 1 000 USD.
        assert_eq!(pct_1e12(1_000 * E6 * E6, 12_500_000), 125 * E6 * E6);
        // 1 000 USD at 40 000 with pv 1 buys 0.025 contracts.
        assert_eq!(
            qty_for_value(1_000 * E6 * E6, 40_000_000_000, 1_000_000),
            25_000
        );
        assert_eq!(qty_for_value(1_000 * E6 * E6, 0, 1_000_000), 0);
        assert_eq!(qty_for_value(1_000 * E6 * E6, 40_000_000_000, -1), 0);
        // 3.5 ticks of 0.25.
        assert_eq!(ticks_px(3_500_000, 250_000), 875_000);
        // The scale re-exports agree with the computation domain.
        assert_eq!(usd_1e6_to_1e12(1_500_000), 1_500_000 * E6);
        assert_eq!(e12_to_1e6_floor(-1), -1);
        assert_eq!(e12_to_1e6_ceil(1), 1);
        assert_eq!(sat_i64(i128::MAX), i64::MAX);
    }

    #[test]
    fn floor_step_never_increases_magnitude() {
        assert_eq!(floor_step(1_999_999, 1_000_000), 1_000_000);
        assert_eq!(floor_step(-1_999_999, 1_000_000), -1_000_000);
        assert_eq!(floor_step(999_999, 1_000_000), 0);
        assert_eq!(floor_step(3_000_000, 1_000_000), 3_000_000);
        assert_eq!(floor_step(i64::MIN, 7), i64::MIN - i64::MIN % 7);
        assert_eq!(floor_step(5, 0), 5);
        assert_eq!(floor_step(5, -3), 5);
    }

    #[test]
    fn floor_step_keeps_sign() {
        assert!(floor_step(-2_500_000, 1_000_000) < 0);
        assert!(floor_step(2_500_000, 1_000_000) > 0);
        assert_eq!(floor_step(-1, 1_000_000), 0);
    }

    #[test]
    fn round_tick_is_half_away_from_zero() {
        assert_eq!(round_tick(1_125_000, 250_000), 1_250_000);
        assert_eq!(round_tick(1_124_999, 250_000), 1_000_000);
        assert_eq!(round_tick(-1_125_000, 250_000), -1_250_000);
        assert_eq!(round_tick(-1_124_999, 250_000), -1_000_000);
        assert_eq!(round_tick(1_000_000, 250_000), 1_000_000);
        assert_eq!(round_tick(i64::MAX, 10), i64::MAX);
        assert_eq!(round_tick(7, 0), 7);
        assert_eq!(floor_tick(1_249_999, 250_000), 1_000_000);
        assert_eq!(floor_tick(-1, 250_000), -250_000);
        assert_eq!(ceil_tick(1_000_001, 250_000), 1_250_000);
        assert_eq!(ceil_tick(-249_999, 250_000), 0);
        assert_eq!(ceil_tick(i64::MAX, 10), i64::MAX);
        assert_eq!(floor_tick(i64::MIN, 10), i64::MIN);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100_000))]

        #[test]
        fn prop_floor_step_is_a_multiple(q in any::<i64>(), s in 1_i64..=i64::MAX) {
            let r = floor_step(q, s);
            prop_assert!(r.unsigned_abs() <= q.unsigned_abs());
            prop_assert!(r == 0 || (r < 0) == (q < 0));
            prop_assert_eq!(r % s, 0);
            // No quantity is ever rounded up: the residual is below one step.
            prop_assert!((q.unsigned_abs() - r.unsigned_abs()) < s.unsigned_abs());
        }

        #[test]
        fn prop_money_ops_never_panic(
            a in any::<i128>(), b in any::<i128>(), d in any::<i128>(),
            x in any::<i64>(), y in any::<i64>(), z in any::<i64>(),
        ) {
            let d = if d == 0 { 1 } else { d };
            let m = mul_div(a, b, d);
            prop_assert!(m != i128::MIN);
            if a != 0 && b != 0 && a.checked_mul(b).is_none() {
                prop_assert_eq!(m.abs(), SAT);
                prop_assert_eq!(m < 0, (a < 0) ^ (b < 0) ^ (d < 0));
            }
            let _ = value_1e12(x, y, z);
            let _ = pct_1e12(a, x);
            let _ = qty_for_value(a, x, y);
            let _ = ticks_px(x, y);
            let _ = floor_step(x, y);
            let rt = round_tick(x, y);
            let ft = floor_tick(x, y);
            let ct = ceil_tick(x, y);
            if y > 0 {
                prop_assert!(ft <= x && ct >= x);
                let diff = (i128::from(rt) - i128::from(x)).unsigned_abs();
                // Within half a tick, unless saturation pinned it at a bound.
                prop_assert!(
                    diff * 2 <= y.unsigned_abs().into() || rt == i64::MAX || rt == i64::MIN
                );
            }
        }
    }
}
