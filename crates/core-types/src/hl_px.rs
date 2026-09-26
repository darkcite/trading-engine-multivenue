// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Hyperliquid's perp price and size law (HC10), at engine scale (×1e6).
//!
//! The venue refuses a perp order whose price has more than **five
//! significant figures** or more than **`6 − szDecimals` decimals**; an
//! INTEGER price is always legal, whatever its significant figures
//! (venue docs, "Tick and lot size"). Sizes step by `10^-szDecimals`.
//! `szDecimals` is the coin's own, from `meta` — for a HIP-3 builder-dex
//! coin (`xyz:NVDA`) from that dex's `meta` (`ingress_hyperliquid::
//! discovery`). One law, shared by the member that sizes a hedge and the
//! arm that sends it, so the two cannot disagree about a legal price.
//!
//! Pure integer arithmetic; `const fn`; no allocation.

/// Engine price scale.
const SCALE: i64 = 1_000_000;

/// `10^k` for `k ≤ 18`.
const fn pow10(k: u32) -> i64 {
    let mut v = 1i64;
    let mut i = 0u32;
    while i < k {
        v *= 10;
        i += 1;
    }
    v
}

/// Decimal digits of `v > 0`.
const fn digits(v: i64) -> u32 {
    let mut n = 1u32;
    let mut x = v;
    while x >= 10 {
        x /= 10;
        n += 1;
    }
    n
}

/// The price quantum, ×1e6, for a price near `px_1e6` (`> 0`): the
/// coarser of the decimal limit (`10^szDecimals` at ×1e6) and the
/// five-significant-figure limit (`10^(digits − 5)`).
const fn quantum(px_1e6: i64, sz_decimals: u8) -> i64 {
    let dec = pow10(sz_decimals as u32);
    let n = digits(px_1e6);
    let sig = if n > 5 { pow10(n - 5) } else { 1 };
    if dec > sig {
        dec
    } else {
        sig
    }
}

/// Is `px_1e6` a price the venue accepts for a perp with `sz_decimals`?
/// (`sz_decimals > 6` is no perp the venue lists: never legal.)
#[must_use]
pub const fn hl_perp_px_legal(px_1e6: i64, sz_decimals: u8) -> bool {
    if px_1e6 <= 0 || sz_decimals > 6 {
        return false;
    }
    px_1e6 % SCALE == 0 || px_1e6 % quantum(px_1e6, sz_decimals) == 0
}

/// `px_1e6` moved to the nearest legal price — up (`up`, a buy that
/// must reach an ask) or down (a sell that must reach a bid). `0` for a
/// non-positive price or `sz_decimals > 6`.
#[must_use]
pub const fn hl_perp_round_px(px_1e6: i64, sz_decimals: u8, up: bool) -> i64 {
    if px_1e6 <= 0 || sz_decimals > 6 {
        return 0;
    }
    if hl_perp_px_legal(px_1e6, sz_decimals) {
        return px_1e6;
    }
    let q = quantum(px_1e6, sz_decimals);
    let down = px_1e6 - px_1e6 % q;
    let r = if up { down + q } else { down };
    // Rounding up can add a digit (99_999.95 → 100_000.0): the new
    // number's quantum is coarser, and an integer is always legal.
    if r > 0 && !hl_perp_px_legal(r, sz_decimals) {
        let q2 = quantum(r, sz_decimals);
        let d2 = r - r % q2;
        return if up { d2 + q2 } else { d2 };
    }
    r
}

/// The size step, contracts ×1e6: `10^(6 − szDecimals)`. `0` for
/// `sz_decimals > 6`.
#[must_use]
pub const fn hl_perp_lot_1e6(sz_decimals: u8) -> i64 {
    if sz_decimals > 6 {
        return 0;
    }
    pow10(6 - sz_decimals as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_venue_examples_hold() {
        // BTC (szDecimals 5): one decimal, five significant figures.
        assert!(hl_perp_px_legal(109_876_000_000, 5), "109876 — an integer is always legal");
        assert!(!hl_perp_px_legal(109_876_500_000, 5), "109876.5 has six significant figures");
        assert!(hl_perp_px_legal(9_876_500_000, 5), "9876.5");
        assert!(!hl_perp_px_legal(9_876_550_000, 5), "9876.55 has two decimals");
        // A small coin (szDecimals 0): six decimals, five figures.
        assert!(hl_perp_px_legal(12_345, 0), "0.012345");
        assert!(!hl_perp_px_legal(123_456, 0), "0.123456 has six figures");
        assert!(hl_perp_px_legal(123_450, 0), "0.12345");
        // xyz equities (szDecimals 3): three decimals AND five figures —
        // above $100 the figures bind first.
        assert!(hl_perp_px_legal(187_120_000, 3), "187.12");
        assert!(!hl_perp_px_legal(187_123_000, 3), "187.123 has six figures");
        assert!(hl_perp_px_legal(18_712_000, 3), "18.712");
        assert!(!hl_perp_px_legal(18_712_400, 3), "18.7124 has four decimals");
        assert!(!hl_perp_px_legal(0, 3));
        assert!(!hl_perp_px_legal(1_000_000, 7));
    }

    #[test]
    fn rounding_moves_toward_the_side_that_must_reach() {
        assert_eq!(hl_perp_round_px(9_876_550_000, 5, true), 9_876_600_000);
        assert_eq!(hl_perp_round_px(9_876_550_000, 5, false), 9_876_500_000);
        assert_eq!(hl_perp_round_px(123_456, 0, true), 123_460);
        assert_eq!(hl_perp_round_px(123_456, 0, false), 123_450);
        // Up across a digit boundary lands on the integer.
        assert_eq!(hl_perp_round_px(99_999_950_000, 5, true), 100_000_000_000);
        // A legal price is its own rounding; a figure too many moves.
        assert_eq!(hl_perp_round_px(187_120_000, 3, true), 187_120_000);
        assert_eq!(hl_perp_round_px(187_123_000, 3, true), 187_130_000);
        assert_eq!(hl_perp_round_px(-1, 3, true), 0);
    }

    #[test]
    fn every_rounding_is_legal_and_on_the_right_side() {
        let mut px = 1i64;
        while px < 200_000_000_000 {
            let mut sz = 0u8;
            while sz <= 6 {
                let u = hl_perp_round_px(px, sz, true);
                let d = hl_perp_round_px(px, sz, false);
                assert!(hl_perp_px_legal(u, sz) && u >= px, "{px} sz {sz} up {u}");
                assert!(d == 0 || (hl_perp_px_legal(d, sz) && d <= px), "{px} sz {sz} down {d}");
                sz += 1;
            }
            px = px * 7 / 5 + 13;
        }
    }

    #[test]
    fn the_lot_is_the_size_step() {
        assert_eq!(hl_perp_lot_1e6(5), 10, "0.00001 BTC");
        assert_eq!(hl_perp_lot_1e6(0), 1_000_000, "whole units");
        assert_eq!(hl_perp_lot_1e6(6), 1);
        assert_eq!(hl_perp_lot_1e6(7), 0);
    }
}
