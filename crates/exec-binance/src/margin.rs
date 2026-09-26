// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **Margin (BX-20; D6, O-BX18, O-BX26).**
//!
//! The arm computes each product's margin ratio — maintenance margin over
//! the margin balance — at every reconciliation and on every
//! `MARGIN_CALL` / `ACCOUNT_UPDATE`, and raises `MarginRisk` when:
//!
//! * a slot's ratio is at or past its `halt_on_margin_ratio_1e6` (the
//!   per-slot side table `[i64; 8]` handed to the arm at boot, O-BX18 — it
//!   sits outside the router's pinned route because it is judged at recon
//!   cadence, never on the submit path); or
//! * any `MARGIN_CALL` arrives — it reaches EVERY Binance slot.
//!
//! A cancel still passes and nothing is flattened (O-BX9): the operator
//! flattens by tool.
//!
//! **USDⓈ-M** (the product BX6 builds): `totalMaintMargin /
//! totalMarginBalance` from the account snapshot. A balance at or below
//! zero reads as an unbounded ratio — fail closed. The other products'
//! formulas (COIN-M per asset, PM `uniMMR`, options maintenance over
//! adjusted equity) arrive with their phases.

use crate::inst::PRODUCTS;

/// The slot count (`EXEC_SLOTS`).
pub const SLOTS: usize = 8;

/// The ratio of an unbounded or unreadable account: always at risk.
pub const RATIO_UNBOUNDED: i64 = i64::MAX;

/// **USDⓈ-M**: `maint / balance` ×1e6 (both USD ×1e6).
#[must_use]
pub const fn um_ratio_1e6(total_maint_1e6: i64, total_margin_balance_1e6: i64) -> i64 {
    if total_margin_balance_1e6 <= 0 || total_maint_1e6 < 0 {
        return RATIO_UNBOUNDED;
    }
    // maint ≤ 2^63/1e6 USD in practice; widen so no product overflows.
    let r = (total_maint_1e6 as i128 * 1_000_000) / total_margin_balance_1e6 as i128;
    if r > i64::MAX as i128 {
        RATIO_UNBOUNDED
    } else {
        r as i64
    }
}

/// **The O-BX18 side table and the arm's margin state.**
#[derive(Clone, Debug)]
pub struct MarginBook {
    /// `halt_on_margin_ratio_1e6` per slot (0 = the slot trades no
    /// Binance product with margin).
    limit_1e6: [i64; SLOTS],
    /// Which products each slot trades (bit `p` = `PRODUCT_*` p).
    products: [u8; SLOTS],
    /// The latest ratio per product ×1e6; 0 until sampled.
    ratio_1e6: [i64; PRODUCTS],
    /// A `MARGIN_CALL` arrived this boot (sticky, like the halt it raises).
    margin_call: bool,
}

impl MarginBook {
    /// The side table handed over at boot: per slot, the threshold and the
    /// products it trades.
    #[must_use]
    pub const fn new(limit_1e6: [i64; SLOTS], products: [u8; SLOTS]) -> Self {
        Self {
            limit_1e6,
            products,
            ratio_1e6: [0; PRODUCTS],
            margin_call: false,
        }
    }

    /// A ratio sample for `product`.
    #[inline]
    pub fn sample(&mut self, product: u8, ratio_1e6: i64) {
        if let Some(r) = self.ratio_1e6.get_mut(product as usize) {
            *r = ratio_1e6;
        }
    }

    /// A `MARGIN_CALL` arrived.
    #[inline]
    pub fn margin_call(&mut self) {
        self.margin_call = true;
    }

    /// The latest ratio of `product` (0 = not sampled).
    #[must_use]
    pub fn ratio(&self, product: u8) -> i64 {
        self.ratio_1e6.get(product as usize).copied().unwrap_or(0)
    }

    /// **`MarginRisk` for `slot`** (a slot trading any Binance product):
    /// any margin call, or a sampled ratio of a product the slot trades at
    /// or past its threshold.
    #[must_use]
    pub fn at_risk(&self, slot: usize) -> bool {
        let (Some(&lim), Some(&mask)) = (self.limit_1e6.get(slot), self.products.get(slot)) else {
            return false;
        };
        if mask == 0 {
            return false;
        }
        if self.margin_call {
            return true;
        }
        if lim <= 0 {
            return false;
        }
        let mut p = 0;
        while p < PRODUCTS {
            if mask & (1 << p) != 0 && self.ratio_1e6[p] >= lim {
                return true;
            }
            p += 1;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inst::{PRODUCT_SPOT, PRODUCT_USDM};

    #[test]
    fn the_um_ratio() {
        assert_eq!(um_ratio_1e6(50_000_000, 1_000_000_000), 50_000);
        assert_eq!(um_ratio_1e6(0, 1_000_000_000), 0);
        assert_eq!(um_ratio_1e6(1, 0), RATIO_UNBOUNDED);
        assert_eq!(um_ratio_1e6(1, -5), RATIO_UNBOUNDED);
        assert_eq!(um_ratio_1e6(i64::MAX, 1), RATIO_UNBOUNDED);
    }

    #[test]
    fn a_slot_reads_its_own_threshold_and_products() {
        let mut lim = [0i64; SLOTS];
        lim[2] = 600_000;
        lim[5] = 700_000;
        let mut prod = [0u8; SLOTS];
        prod[2] = 1 << PRODUCT_USDM;
        prod[5] = 1 << PRODUCT_SPOT;
        let mut m = MarginBook::new(lim, prod);
        assert!(!m.at_risk(2) && !m.at_risk(5));
        m.sample(PRODUCT_USDM, 650_000);
        assert!(m.at_risk(2), "slot 2 trades usdm at 65 % > 60 %");
        assert!(!m.at_risk(5), "slot 5 trades no usdm");
        assert!(!m.at_risk(0), "slot 0 has no threshold");
        m.sample(PRODUCT_USDM, 599_999);
        assert!(!m.at_risk(2));
        m.margin_call();
        assert!(m.at_risk(2) && m.at_risk(5), "a margin call reaches every Binance slot");
        assert!(!m.at_risk(0), "a slot trading no Binance product is not the arm's");
        assert!(!m.at_risk(99), "an out-of-range slot never panics");
    }
}
