// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Amount deltas and next-price solves, bit-exact with Uniswap V3
//! `SqrtPriceMath` and `SwapMath`.
//!
//! Every rounding direction is the contract's: amounts IN round up,
//! amounts OUT round down, and the next price moves the pool no further
//! than the amount pays for. `None` is the contract's `revert` — an
//! arithmetic domain error the caller stops on and counts.

use crate::u256::{div_rounding_up, mul_div, mul_div_rounding_up, U256};

/// 2^96, the `sqrtPriceX96` resolution.
const Q96: U256 = U256::pow2(96);
/// Fee denominator: fees are in pips (1e-6).
const PIPS: u128 = 1_000_000;

/// `SqrtPriceMath.getAmount0Delta(a, b, L, roundUp)`: the token0 needed
/// to move between two prices at constant liquidity.
pub(crate) const fn amount0_delta(
    sa: U256,
    sb: U256,
    liquidity: u128,
    round_up: bool,
) -> Option<U256> {
    let (lo, hi) = if sa.le(sb) { (sa, sb) } else { (sb, sa) };
    if lo.is_zero() {
        return None;
    }
    let num1 = U256::from_u128(liquidity).shl(96);
    let num2 = match hi.checked_sub(lo) {
        Some(v) => v,
        None => return None,
    };
    if round_up {
        match mul_div_rounding_up(num1, num2, hi) {
            Some(v) => div_rounding_up(v, lo),
            None => None,
        }
    } else {
        match mul_div(num1, num2, hi) {
            Some(v) => match v.checked_div_rem(lo) {
                Some((q, _)) => Some(q),
                None => None,
            },
            None => None,
        }
    }
}

/// `SqrtPriceMath.getAmount1Delta(a, b, L, roundUp)`: the token1 needed
/// to move between two prices at constant liquidity.
pub(crate) const fn amount1_delta(
    sa: U256,
    sb: U256,
    liquidity: u128,
    round_up: bool,
) -> Option<U256> {
    let (lo, hi) = if sa.le(sb) { (sa, sb) } else { (sb, sa) };
    let d = match hi.checked_sub(lo) {
        Some(v) => v,
        None => return None,
    };
    if round_up {
        mul_div_rounding_up(U256::from_u128(liquidity), d, Q96)
    } else {
        mul_div(U256::from_u128(liquidity), d, Q96)
    }
}

/// `getNextSqrtPriceFromAmount0RoundingUp`.
const fn next_from_amount0_up(sp: U256, liquidity: u128, amount: U256, add: bool) -> Option<U256> {
    if amount.is_zero() {
        return Some(sp);
    }
    let num1 = U256::from_u128(liquidity).shl(96);
    let product = amount.checked_mul(sp);
    if add {
        if let Some(p) = product {
            let (den, of) = num1.overflowing_add(p);
            if !of {
                return mul_div_rounding_up(num1, sp, den);
            }
        }
        // num1 / (num1/sp + amount), rounded up.
        let q = match num1.checked_div_rem(sp) {
            Some((q, _)) => q,
            None => return None,
        };
        match q.checked_add(amount) {
            Some(den) => div_rounding_up(num1, den),
            None => None,
        }
    } else {
        let p = match product {
            Some(p) => p,
            None => return None,
        };
        if num1.le(p) {
            return None;
        }
        let den = match num1.checked_sub(p) {
            Some(v) => v,
            None => return None,
        };
        match mul_div_rounding_up(num1, sp, den) {
            Some(v) => {
                if v.hi > u32::MAX as u128 {
                    None
                } else {
                    Some(v)
                }
            }
            None => None,
        }
    }
}

/// `getNextSqrtPriceFromAmount1RoundingDown`. The contract's two
/// branches (`amount <= type(uint160).max` shift vs `mulDiv`) compute the
/// same exact quotient, so both collapse into `mul_div` here.
const fn next_from_amount1_down(
    sp: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Option<U256> {
    let l = U256::from_u128(liquidity);
    if add {
        let q = match mul_div(amount, Q96, l) {
            Some(v) => v,
            None => return None,
        };
        match sp.checked_add(q) {
            Some(v) => {
                if v.hi > u32::MAX as u128 {
                    None
                } else {
                    Some(v)
                }
            }
            None => None,
        }
    } else {
        let q = match mul_div_rounding_up(amount, Q96, l) {
            Some(v) => v,
            None => return None,
        };
        if sp.le(q) {
            return None;
        }
        sp.checked_sub(q)
    }
}

/// `getNextSqrtPriceFromInput`.
const fn next_from_input(
    sp: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Option<U256> {
    if sp.is_zero() || liquidity == 0 {
        return None;
    }
    if zero_for_one {
        next_from_amount0_up(sp, liquidity, amount_in, true)
    } else {
        next_from_amount1_down(sp, liquidity, amount_in, true)
    }
}

/// `getNextSqrtPriceFromOutput`.
const fn next_from_output(
    sp: U256,
    liquidity: u128,
    amount_out: U256,
    zero_for_one: bool,
) -> Option<U256> {
    if sp.is_zero() || liquidity == 0 {
        return None;
    }
    if zero_for_one {
        next_from_amount1_down(sp, liquidity, amount_out, false)
    } else {
        next_from_amount0_up(sp, liquidity, amount_out, false)
    }
}

/// One `SwapMath.computeSwapStep` result.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) sqrt_next: U256,
    pub(crate) amount_in: U256,
    pub(crate) amount_out: U256,
    pub(crate) fee: U256,
}

/// `SwapMath.computeSwapStep`. `remaining` is the magnitude of
/// `amountRemaining`; `exact_in` is its sign (`>= 0`).
pub(crate) const fn compute_swap_step(
    current: U256,
    target: U256,
    liquidity: u128,
    remaining: U256,
    exact_in: bool,
    fee_pips: u32,
) -> Option<Step> {
    if fee_pips as u128 >= PIPS {
        return None;
    }
    let zero_for_one = target.le(current);
    let fee_c = U256::from_u128(PIPS - fee_pips as u128);
    let pips = U256::from_u128(PIPS);
    let mut amount_in = U256::ZERO;
    let mut amount_out = U256::ZERO;
    let sqrt_next;
    if exact_in {
        let rem_less_fee = match mul_div(remaining, fee_c, pips) {
            Some(v) => v,
            None => return None,
        };
        amount_in = match if zero_for_one {
            amount0_delta(target, current, liquidity, true)
        } else {
            amount1_delta(current, target, liquidity, true)
        } {
            Some(v) => v,
            None => return None,
        };
        if amount_in.le(rem_less_fee) {
            sqrt_next = target;
        } else {
            sqrt_next = match next_from_input(current, liquidity, rem_less_fee, zero_for_one) {
                Some(v) => v,
                None => return None,
            };
        }
    } else {
        amount_out = match if zero_for_one {
            amount1_delta(target, current, liquidity, false)
        } else {
            amount0_delta(current, target, liquidity, false)
        } {
            Some(v) => v,
            None => return None,
        };
        if amount_out.le(remaining) {
            sqrt_next = target;
        } else {
            sqrt_next = match next_from_output(current, liquidity, remaining, zero_for_one) {
                Some(v) => v,
                None => return None,
            };
        }
    }
    let max = target.same_as(sqrt_next);
    if zero_for_one {
        if !max || !exact_in {
            amount_in = match amount0_delta(sqrt_next, current, liquidity, true) {
                Some(v) => v,
                None => return None,
            };
        }
        if !max || exact_in {
            amount_out = match amount1_delta(sqrt_next, current, liquidity, false) {
                Some(v) => v,
                None => return None,
            };
        }
    } else {
        if !max || !exact_in {
            amount_in = match amount1_delta(current, sqrt_next, liquidity, true) {
                Some(v) => v,
                None => return None,
            };
        }
        if !max || exact_in {
            amount_out = match amount0_delta(current, sqrt_next, liquidity, false) {
                Some(v) => v,
                None => return None,
            };
        }
    }
    // Cap the output at the requested amount (exact-output only).
    if !exact_in && remaining.lt(amount_out) {
        amount_out = remaining;
    }
    let fee = if exact_in && !max {
        match remaining.checked_sub(amount_in) {
            Some(v) => v,
            None => return None,
        }
    } else {
        match mul_div_rounding_up(amount_in, U256::from_u128(fee_pips as u128), fee_c) {
            Some(v) => v,
            None => return None,
        }
    };
    Some(Step {
        sqrt_next,
        amount_in,
        amount_out,
        fee,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_math::sqrt_ratio_at_tick;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(20_000))]

        /// Exact-in: input + fee never exceeds what was offered, and the
        /// price moves toward the target but never past it.
        #[test]
        fn exact_in_is_bounded(t0 in -200_000i32..200_000, dt in -5_000i32..5_000,
                               liq in 1u128..(1u128 << 100), amt in 1u128..(1u128 << 100),
                               fee in prop::sample::select(vec![0u32, 100, 500, 3_000, 10_000])) {
            let c = sqrt_ratio_at_tick(t0);
            let t = sqrt_ratio_at_tick(t0 + dt);
            let s = compute_swap_step(c, t, liq, U256::from_u128(amt), true, fee).unwrap();
            let spent = s.amount_in.checked_add(s.fee).unwrap();
            prop_assert!(spent <= U256::from_u128(amt));
            if dt >= 0 { prop_assert!(s.sqrt_next >= c && s.sqrt_next <= t); }
            else { prop_assert!(s.sqrt_next <= c && s.sqrt_next >= t); }
        }

        /// Exact-out never delivers more than asked.
        #[test]
        fn exact_out_is_capped(t0 in -200_000i32..200_000, dt in -5_000i32..5_000,
                               liq in 1u128..(1u128 << 100), amt in 1u128..(1u128 << 100)) {
            let c = sqrt_ratio_at_tick(t0);
            let t = sqrt_ratio_at_tick(t0 + dt);
            let s = compute_swap_step(c, t, liq, U256::from_u128(amt), false, 3_000).unwrap();
            prop_assert!(s.amount_out <= U256::from_u128(amt));
        }

        /// Up and back at constant liquidity costs at least what it returns.
        #[test]
        fn no_free_round_trip(t0 in -200_000i32..200_000, dt in 1i32..5_000, liq in 1u128..(1u128 << 100)) {
            let a = sqrt_ratio_at_tick(t0);
            let b = sqrt_ratio_at_tick(t0 + dt);
            let in1 = amount1_delta(a, b, liq, true).unwrap();
            let out1 = amount1_delta(a, b, liq, false).unwrap();
            let in0 = amount0_delta(a, b, liq, true).unwrap();
            let out0 = amount0_delta(a, b, liq, false).unwrap();
            prop_assert!(in1 >= out1 && in0 >= out0);
        }
    }
}
