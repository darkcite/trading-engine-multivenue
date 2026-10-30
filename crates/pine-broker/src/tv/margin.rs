// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Margin — TV mode only (impl-05 §4.9): the funds check at an entry fill, the documented margin
//! call, and `strategy.margin_liquidation_price`. A zero margin is inert. Pure functions.

use crate::money::{floor_step, mul_div, pct_1e12, qty_for_value, sat_i64, value_1e12, E6};
use crate::switch::MarginFunds;

/// The opening quantity an entry may take (§4.9.1): `q_open` when the funds cover it; under
/// `Reject` `None` otherwise; under `Reduce` the affordable quantity floored to `step` (`None` at
/// zero). `equity` is taken after the fill's reducing part; `pos_r_abs` the position that part
/// leaves.
pub(crate) fn funds_check(
    q_open_1e6: i64,
    pos_r_abs_1e6: i64,
    px_1e6: i64,
    pv_1e6: i64,
    margin_pct_1e6: i64,
    equity_1e12: i128,
    step_1e6: i64,
    mode: MarginFunds,
) -> Option<i64> {
    if margin_pct_1e6 <= 0 {
        return Some(q_open_1e6);
    }
    let required = pct_1e12(value_1e12(px_1e6, q_open_1e6, pv_1e6), margin_pct_1e6);
    let available = equity_1e12.saturating_sub(pct_1e12(
        value_1e12(px_1e6, pos_r_abs_1e6, pv_1e6),
        margin_pct_1e6,
    ));
    if required <= available {
        return Some(q_open_1e6);
    }
    match mode {
        MarginFunds::Reject => None,
        MarginFunds::Reduce => {
            let v = mul_div(available.max(0), 100 * E6, i128::from(margin_pct_1e6));
            let q = floor_step(qty_for_value(v, px_1e6, pv_1e6), step_1e6).min(q_open_1e6);
            if q > 0 {
                Some(q)
            } else {
                None
            }
        }
    }
}

/// The documented margin call (§4.9.2): the unsigned quantity to liquidate at `px`, or 0.
pub(crate) fn margin_call_qty(
    pos_1e6: i64,
    avg_px_1e6: i64,
    px_1e6: i64,
    initial_1e12: i128,
    net_1e12: i128,
    margin_pct_1e6: i64,
    pv_1e6: i64,
    step_1e6: i64,
) -> i64 {
    if margin_pct_1e6 <= 0 || pos_1e6 == 0 {
        return 0;
    }
    let q = pos_1e6.saturating_abs();
    let spent = value_1e12(avg_px_1e6, q, pv_1e6);
    let mvs = value_1e12(px_1e6, q, pv_1e6);
    let open = if pos_1e6 > 0 {
        mvs.saturating_sub(spent)
    } else {
        spent.saturating_sub(mvs)
    };
    let equity = initial_1e12.saturating_add(net_1e12).saturating_add(open);
    let available = equity.saturating_sub(pct_1e12(mvs, margin_pct_1e6));
    if available >= 0 {
        return 0;
    }
    let loss = mul_div(available, 100 * E6, i128::from(margin_pct_1e6));
    let den = i128::from(px_1e6) * i128::from(pv_1e6);
    if den <= 0 {
        return 0;
    }
    let cover = floor_step(sat_i64(mul_div(loss, E6, den)), step_1e6);
    cover.saturating_abs().saturating_mul(4).min(q)
}

/// `strategy.margin_liquidation_price` ×1e6 (§4.9.3); 0 = `na`.
pub(crate) fn liquidation_price(
    pos_1e6: i64,
    avg_px_1e6: i64,
    initial_1e12: i128,
    net_1e12: i128,
    margin_pct_1e6: i64,
    pv_1e6: i64,
) -> i64 {
    let full = 100 * E6;
    let m = i128::from(margin_pct_1e6);
    if pos_1e6 == 0 || m <= 0 || (pos_1e6 > 0 && m >= full) {
        return 0;
    }
    let q = pos_1e6.saturating_abs();
    let spent = value_1e12(avg_px_1e6, q, pv_1e6);
    let mvs = if pos_1e6 > 0 {
        mul_div(
            spent.saturating_sub(initial_1e12).saturating_sub(net_1e12),
            full,
            full - m,
        )
    } else {
        mul_div(
            initial_1e12.saturating_add(net_1e12).saturating_add(spent),
            full,
            full + m,
        )
    };
    let den = i128::from(q) * i128::from(pv_1e6);
    if den <= 0 {
        return 0;
    }
    let p = sat_i64(mul_div(mvs, E6, den));
    p.max(0)
}

#[cfg(test)]
mod tests {
    use super::{funds_check, liquidation_price, margin_call_qty};
    use crate::switch::MarginFunds;

    const E: i64 = 1_000_000;
    const E12: i128 = 1_000_000_000_000;

    #[test]
    fn margin_call_reproduces_the_documented_example() {
        // 682 438 shares at 4.43, price 3.90, capital 1 000 000, margin 25 % → liquidate 111 052.
        let q = margin_call_qty(
            682_438 * E,
            4_430_000,
            3_900_000,
            1_000_000 * E12,
            0,
            25 * E,
            E,
            E,
        );
        assert_eq!(q, 111_052 * E);
    }

    #[test]
    fn margin_zero_is_inert() {
        assert_eq!(margin_call_qty(E, 100 * E, E, 0, 0, 0, E, 0), 0);
        assert_eq!(
            funds_check(5 * E, 0, 100 * E, E, 0, 0, 0, MarginFunds::Reject),
            Some(5 * E)
        );
        assert_eq!(liquidation_price(E, 100 * E, 0, 0, 0, E), 0);
    }

    #[test]
    fn funds_check_rejects_or_reduces() {
        // 1 000 USD of equity, margin 100 %: 20 contracts at 100 need 2 000.
        let eq = 1_000 * E12;
        let r = funds_check(20 * E, 0, 100 * E, E, 100 * E, eq, E, MarginFunds::Reject);
        assert_eq!(r, None);
        let r = funds_check(20 * E, 0, 100 * E, E, 100 * E, eq, E, MarginFunds::Reduce);
        assert_eq!(r, Some(10 * E));
        assert_eq!(
            funds_check(5 * E, 0, 100 * E, E, 100 * E, eq, E, MarginFunds::Reject),
            Some(5 * E)
        );
    }

    #[test]
    fn liquidation_price_long_and_short() {
        // Long 10 at 100 (spent 1 000), capital 1 000, 50 %: mvs* = 0 → na.
        assert_eq!(
            liquidation_price(10 * E, 100 * E, 1_000 * E12, 0, 50 * E, E),
            0
        );
        // Long 20 at 100 (spent 2 000), capital 1 000, 50 %: mvs* = 1 000 / 0.5 = 2 000 → 100.
        assert_eq!(
            liquidation_price(20 * E, 100 * E, 1_000 * E12, 0, 50 * E, E),
            100 * E
        );
        // Short 10 at 100, capital 1 000, margin 100 %: mvs* = 2000 / 2 = 1000 → 100.
        assert_eq!(
            liquidation_price(-10 * E, 100 * E, 1_000 * E12, 0, 100 * E, E),
            100 * E
        );
    }
}
