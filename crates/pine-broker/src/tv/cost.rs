// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Slippage and commission (impl-05 §4.8.6). Pure functions; no allocation.

use crate::money::{mul_div, pct_1e12, usd_1e6_to_1e12, value_1e12};
use crate::switch::CashPerOrder;
use pine_ir::{CommissionType, StrategyDecl};

/// `px` moved `slippage_ticks · mintick` in the adverse direction (up for a buy, down for a sell),
/// saturating, **not clamped** to the bar. Applied to market orders, stops and triggered trails —
/// never to a limit (the caller's rule).
pub(crate) fn slip(px_1e6: i64, buy: bool, slippage_ticks: u32, mintick_1e6: i64) -> i64 {
    let d = i64::from(slippage_ticks).saturating_mul(mintick_1e6);
    if buy {
        px_1e6.saturating_add(d)
    } else {
        px_1e6.saturating_sub(d)
    }
}

/// The commission of one fill of `|q|` at `px` ×1e12 (§4.8.6). `first_fill`: the fill is its
/// order's first (the `cash_per_order = PerOrder` reading charges only that one).
pub(crate) fn commission_1e12(
    d: &StrategyDecl,
    px_1e6: i64,
    q_abs_1e6: i64,
    pv_1e6: i64,
    first_fill: bool,
    per_order: CashPerOrder,
) -> i128 {
    let cv = d.commission_value_1e6;
    match d.commission_type {
        CommissionType::Percent => pct_1e12(value_1e12(px_1e6, q_abs_1e6, pv_1e6), cv),
        // contracts ×1e6 · USD ×1e6 = ×1e12.
        CommissionType::CashPerContract => mul_div(i128::from(q_abs_1e6), i128::from(cv), 1),
        CommissionType::CashPerOrder => {
            if per_order == CashPerOrder::PerFill || first_fill {
                usd_1e6_to_1e12(cv)
            } else {
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{commission_1e12, slip};
    use crate::switch::CashPerOrder;
    use pine_ir::{CommissionType, StrategyDecl};

    const E: i64 = 1_000_000;
    const E12: i128 = 1_000_000_000_000;

    #[test]
    fn slippage_is_adverse_and_unclamped() {
        // 20 ticks of 0.25 beyond 100: 105 for a buy, 95 for a sell.
        assert_eq!(slip(100 * E, true, 20, 250_000), 105 * E);
        assert_eq!(slip(100 * E, false, 20, 250_000), 95 * E);
        assert_eq!(slip(100 * E, true, 0, 250_000), 100 * E);
        assert_eq!(slip(i64::MAX, true, u32::MAX, i64::MAX), i64::MAX);
    }

    #[test]
    fn commission_per_type() {
        let pct = StrategyDecl {
            commission_type: CommissionType::Percent,
            commission_value_1e6: E,
            ..StrategyDecl::default()
        };
        // 1 % of 2 × 100 = 2 USD.
        assert_eq!(
            commission_1e12(&pct, 100 * E, 2 * E, E, true, CashPerOrder::PerFill),
            2 * E12
        );
        let cpc = StrategyDecl {
            commission_type: CommissionType::CashPerContract,
            commission_value_1e6: 500_000,
            ..StrategyDecl::default()
        };
        // 0.50 per contract × 3 = 1.50 USD.
        assert_eq!(
            commission_1e12(&cpc, 100 * E, 3 * E, E, true, CashPerOrder::PerFill),
            1_500_000 * 1_000_000
        );
        let cpo = StrategyDecl {
            commission_type: CommissionType::CashPerOrder,
            commission_value_1e6: 2 * E,
            ..StrategyDecl::default()
        };
        assert_eq!(
            commission_1e12(&cpo, 1, 1, E, false, CashPerOrder::PerFill),
            2 * E12
        );
        // The `PerOrder` alternative (`pin:05-cash-per-order`): only the first fill pays.
        assert_eq!(
            commission_1e12(&cpo, 1, 1, E, true, CashPerOrder::PerOrder),
            2 * E12
        );
        assert_eq!(
            commission_1e12(&cpo, 1, 1, E, false, CashPerOrder::PerOrder),
            0
        );
        // None when unspecified.
        assert_eq!(
            commission_1e12(
                &StrategyDecl::default(),
                100 * E,
                E,
                E,
                true,
                CashPerOrder::PerFill
            ),
            0
        );
    }
}
