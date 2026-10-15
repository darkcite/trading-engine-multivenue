// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Human prices ↔ `sqrtPriceX96`, and the active range.
//!
//! A "price ×1e18" is token1 per token0 in HUMAN units (decimals
//! applied), scaled by 1e18 — the unit the hedge bounds arrive in.

use crate::tick_math::{MAX_SQRT, MAX_TICK, MIN_SQRT, MIN_TICK};
use crate::u256::{mul_div, mul_div_rounding_up, U256};

/// `10^e` for `e <= 77`; `None` beyond (does not fit 256 bits).
pub(crate) const fn pow10(e: u32) -> Option<U256> {
    if e > 77 {
        return None;
    }
    let mut r = U256::ONE;
    let ten = U256::from_u128(10);
    let mut i = 0;
    while i < e {
        r = match r.checked_mul(ten) {
            Some(v) => v,
            None => return None,
        };
        i += 1;
    }
    Some(r)
}

/// `x · 10^e` (e ≥ 0) or `x / 10^-e` (e < 0), rounding as asked.
pub(crate) const fn scale10(x: U256, e: i32, round_up: bool) -> Option<U256> {
    if e >= 0 {
        match pow10(e as u32) {
            Some(p) => x.checked_mul(p),
            None => None,
        }
    } else {
        match pow10(e.unsigned_abs()) {
            Some(p) => {
                if round_up {
                    mul_div_rounding_up(x, U256::ONE, p)
                } else {
                    mul_div(x, U256::ONE, p)
                }
            }
            None => Some(U256::ZERO),
        }
    }
}

/// The active range `[lo, hi)` containing `tick`: `lo` is the greatest
/// multiple of `tick_spacing` not above `tick`, `hi = lo + tick_spacing`,
/// both clamped into `[MIN_TICK, MAX_TICK]`. Liquidity is constant
/// inside it by construction — ticks can only be initialised on
/// multiples of the spacing.
#[must_use]
pub fn range_bounds(tick: i32, tick_spacing: i32) -> (i32, i32) {
    if tick_spacing <= 0 {
        return (tick, tick);
    }
    let sp = tick_spacing as i64;
    let t = tick as i64;
    let q = t / sp;
    let q = if t % sp != 0 && t < 0 { q - 1 } else { q };
    let lo = q * sp;
    (clamp_tick(lo), clamp_tick(lo + sp))
}

#[inline(always)]
const fn clamp_tick(v: i64) -> i32 {
    if v < MIN_TICK as i64 {
        MIN_TICK
    } else if v > MAX_TICK as i64 {
        MAX_TICK
    } else {
        v as i32
    }
}

/// token1 per token0 (human) × 1e18 from a `uint160` sqrt price:
/// `(s / 2^96)^2 · 10^(dec0 − dec1) · 1e18`, floored, saturating at
/// `u128::MAX` (a price above 3.4e20 — never a real pool).
#[must_use]
pub fn price_1e18_from_sqrt(lo: u128, hi: u32, dec0: u8, dec1: u8) -> u128 {
    price_1e18_u(U256::from_u160(lo, hi), dec0, dec1).saturating_u128()
}

pub(crate) const fn price_1e18_u(s: U256, dec0: u8, dec1: u8) -> U256 {
    // s^2 / 2^96 fits: s < 2^160 ⇒ < 2^224.
    let p96 = match mul_div(s, s, U256::pow2(96)) {
        Some(v) => v,
        None => return U256::MAX,
    };
    let e = 18 + dec0 as i32 - dec1 as i32;
    if e >= 0 {
        match pow10(e as u32) {
            Some(p) => match mul_div(p96, p, U256::pow2(96)) {
                Some(v) => v,
                None => U256::MAX,
            },
            None => U256::MAX,
        }
    } else {
        match pow10(e.unsigned_abs()) {
            Some(p) => match p.checked_mul(U256::pow2(96)) {
                Some(d) => match mul_div(p96, U256::ONE, d) {
                    Some(v) => v,
                    None => U256::MAX,
                },
                None => U256::ZERO,
            },
            None => U256::ZERO,
        }
    }
}

/// The `uint160` sqrt price (floored) for a token1-per-token0 human
/// price × 1e18: `sqrt(px · 2^192 / 10^(18 + dec0 − dec1))`, clamped
/// into `[MIN_SQRT, MAX_SQRT − 1]`.
#[must_use]
pub fn sqrt_from_price_1e18(px_1e18: u128, dec0: u8, dec1: u8) -> (u128, u32) {
    let s = sqrt_from_price_u(px_1e18, dec0, dec1);
    (s.lo, s.hi as u32)
}

pub(crate) const fn sqrt_from_price_u(px_1e18: u128, dec0: u8, dec1: u8) -> U256 {
    let e = 18 + dec0 as i32 - dec1 as i32;
    let x = if e >= 0 {
        match pow10(e as u32) {
            Some(p) => mul_div(U256::from_u128(px_1e18), U256::pow2(192), p),
            None => Some(U256::ZERO),
        }
    } else {
        match scale10(U256::from_u128(px_1e18), -e, false) {
            Some(v) => v.checked_mul(U256::pow2(192)),
            None => None,
        }
    };
    let s = match x {
        Some(v) => v.isqrt(),
        None => MAX_SQRT,
    };
    if s.lt(MIN_SQRT) {
        MIN_SQRT
    } else if MAX_SQRT.le(s) {
        MAX_SQRT.wrapping_sub(U256::ONE)
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_math::sqrt_at_tick;

    #[test]
    fn range_bounds_floor_negative_ticks() {
        assert_eq!(range_bounds(0, 10), (0, 10));
        assert_eq!(range_bounds(9, 10), (0, 10));
        assert_eq!(range_bounds(-1, 10), (-10, 0));
        assert_eq!(range_bounds(-10, 10), (-10, 0));
        assert_eq!(range_bounds(-230_543, 10), (-230_550, -230_540));
        assert_eq!(range_bounds(MAX_TICK, 60).1, MAX_TICK);
    }

    #[test]
    fn whype_usdc_price_round_trip() {
        // A live WHYPE(18)/USDC(6) state: tick -230543 ⇒ ~ $97.7.
        let (lo, hi) = sqrt_at_tick(-230_543);
        let px = price_1e18_from_sqrt(lo, hi, 18, 6);
        assert!(
            px > 90 * 1_000_000_000_000_000_000 && px < 110 * 1_000_000_000_000_000_000,
            "px {px}"
        );
        let (slo, shi) = sqrt_from_price_1e18(px, 18, 6);
        let back = price_1e18_from_sqrt(slo, shi, 18, 6);
        // floor(sqrt) then square: within a few 1e-18 relative.
        assert!(px - back <= px / 1_000_000_000_000_000 + 1);
    }

    #[test]
    fn pow10_limits() {
        assert!(pow10(77).is_some());
        assert!(pow10(78).is_none());
    }
}
