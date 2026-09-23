// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The paper judge's arithmetic — one swap against the ACTIVE RANGE,
//! human units in and out — and the effective-fee observation the judge
//! and the member share.
//!
//! `core-fill` owns the fill LAW (when an order may fill and what a fill
//! is); the 256-bit arithmetic lives here, beside the walk it calls, so
//! there is one implementation of "what would this swap have paid".
//!
//! ## Units
//!
//! Every AMM order is expressed in token0: `qty_1e6` is token0 in human
//! units × 1e6, `px_1e6` is token1 per token0 (human) × 1e6. The POOL FEE
//! IS IN THE PRICE — a pool charges it on the input, so it is part of the
//! execution price on chain, and `core_types::Fill` carries no fee.
//!
//! ## Conservative by construction
//!
//! * The walk never crosses the active range's boundary (no tick map), so
//!   a swap that would cross a tick is capped there: it fills LESS than
//!   the chain would, never more.
//! * The price limit is placed on the pool's MARGINAL price with the fee
//!   folded in, so every unit filled cost at most (bought) or earned at
//!   least (sold) the limit — the average is therefore within it too.
//!   The limit's sqrt price rounds toward stopping earlier.
//! * The reported quantity rounds DOWN, and the price rounds against us
//!   (a buy's up, a sell's down), computed against the REPORTED quantity
//!   so `px · qty` never overstates what the swap was worth.
//! * A result whose average price nevertheless breaches the limit (fee
//!   rounding on a dust swap) is refused, not clipped.

use crate::price::{scale10, sqrt_from_price_u};
use crate::swap::{swap_exact_in_range, swap_in_range};
use crate::tick_math::{MAX_SQRT, MIN_SQRT};
use crate::types::{
    PoolMeta, PoolState, SwapSpec, POOL_FLAG_EDGE, SWAP_FLAG_EDGE, SWAP_FLAG_MATH,
    SWAP_FLAG_REFUSED, SWAP_FLAG_SATURATED,
};
use crate::u256::{mul_div, mul_div_rounding_up, U256};

const PIPS: u128 = 1_000_000;
const E6: u128 = 1_000_000;
const E12: u128 = 1_000_000_000_000;

/// [`RangeFill::flags`]: nothing was filled (see the other bits for why).
pub const RFILL_NONE: u8 = 1;
/// [`RangeFill::flags`]: the pool was not live (stale, edge-limited,
/// unpriced or illiquid), or the order was malformed.
pub const RFILL_NOT_LIVE: u8 = 2;
/// [`RangeFill::flags`]: the limit was already breached at the pool's
/// current price, or the result's average price breached it.
pub const RFILL_LIMIT: u8 = 4;
/// [`RangeFill::flags`]: filled less than asked (the range boundary or
/// the limit stopped the walk).
pub const RFILL_PARTIAL: u8 = 8;
/// [`RangeFill::flags`]: the walk ended ON the range boundary — the pool
/// state after it carries `POOL_FLAG_EDGE` and is not walkable again
/// until a real update arrives.
pub const RFILL_EDGE: u8 = 16;
/// [`RangeFill::flags`]: an arithmetic domain error (would revert, or a
/// quantity that does not fit the i64 / u128 domain).
pub const RFILL_MATH: u8 = 32;

/// What one in-range swap filled, in human units.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct RangeFill {
    /// token0 filled × 1e6 (0 with [`RFILL_NONE`]).
    pub qty_1e6: i64,
    /// Average price, token1 per token0 × 1e6, pool fee included.
    pub px_1e6: i64,
    /// `RFILL_*`.
    pub flags: u8,
    _pad: [u8; 15],
    /// The pool after the swap — the caller carries it, or a standing
    /// gap is harvested twice. Equal to the input state with no fill.
    pub after: PoolState,
}

impl RangeFill {
    const fn none(state: &PoolState, flags: u8) -> Self {
        Self {
            qty_1e6: 0,
            px_1e6: 0,
            flags: flags | RFILL_NONE,
            _pad: [0; 15],
            after: *state,
        }
    }
}

#[inline(always)]
const fn to_i64(v: U256) -> Option<i64> {
    if v.hi != 0 || v.lo > i64::MAX as u128 {
        None
    } else {
        Some(v.lo as i64)
    }
}

#[inline(always)]
const fn to_u128(v: U256) -> Option<u128> {
    if v.hi != 0 {
        None
    } else {
        Some(v.lo)
    }
}

/// The limit on the pool's MARGINAL price (token1 per token0 × 1e18,
/// human) that keeps every unit within `px_limit_1e6` after the fee:
/// a sell stops at `px / (1 − f)` (rounded up — earlier), a buy at
/// `px · (1 − f)` (rounded down — earlier).
const fn marginal_limit_1e18(px_limit_1e6: i64, fee_pips: u32, sell: bool) -> Option<u128> {
    let px = U256::mul_u128(px_limit_1e6 as u128, E12);
    let keep = PIPS - fee_pips as u128;
    let v = if sell {
        mul_div_rounding_up(px, U256::from_u128(PIPS), U256::from_u128(keep))
    } else {
        mul_div(px, U256::from_u128(keep), U256::from_u128(PIPS))
    };
    match v {
        Some(x) => to_u128(x),
        None => None,
    }
}

/// Fill `qty_1e6` of token0 against the ACTIVE RANGE of `state`, within
/// the average-price limit `px_limit_1e6` — a SELL of token0 into the
/// pool (`sell = true`, exact input, price down) or a BUY of token0 from
/// it (exact output, price up). `meta.fee_pips` is the fee charged; the
/// caller picks it (the judge charges the worse of the fee in force and
/// the last one observed).
///
/// Conservative by construction — see the module doc. Never panics; any
/// domain problem is a [`RFILL_NONE`] result with the reason bit set.
#[must_use]
pub fn fill_in_range(
    state: &PoolState,
    meta: &PoolMeta,
    sell: bool,
    qty_1e6: i64,
    px_limit_1e6: i64,
) -> RangeFill {
    if !state.is_live() || qty_1e6 <= 0 || px_limit_1e6 <= 0 || meta.fee_pips as u128 >= PIPS {
        return RangeFill::none(state, RFILL_NOT_LIVE);
    }
    // token0 raw = qty · 10^(dec0 − 6), floored.
    let raw0 = match scale10(
        U256::from_u128(qty_1e6 as u128),
        meta.dec0 as i32 - 6,
        false,
    ) {
        Some(v) => match to_u128(v) {
            Some(x) if x > 0 => x,
            _ => return RangeFill::none(state, RFILL_MATH),
        },
        None => return RangeFill::none(state, RFILL_MATH),
    };
    let lim_1e18 = match marginal_limit_1e18(px_limit_1e6, meta.fee_pips, sell) {
        Some(v) if v > 0 => v,
        _ => return RangeFill::none(state, RFILL_MATH),
    };
    // Floor sqrt; a sell nudges it UP one unit so the walk stops no later
    // than the exact limit would.
    let mut lim = sqrt_from_price_u(lim_1e18, meta.dec0, meta.dec1);
    if sell && lim.lt(MAX_SQRT.wrapping_sub(U256::ONE)) {
        lim = lim.wrapping_add(U256::ONE);
    }
    let cur = U256::from_u160(state.sqrt_price_lo, state.sqrt_price_hi);
    // A sell needs room below the current price, a buy above it.
    let room = if sell { lim.lt(cur) } else { cur.lt(lim) };
    if !room || lim.lt(MIN_SQRT) {
        return RangeFill::none(state, RFILL_LIMIT);
    }
    let spec = SwapSpec {
        amount: raw0,
        limit_lo: lim.lo,
        limit_hi: lim.hi as u32,
        fee_pips: meta.fee_pips,
        zero_for_one: sell,
        exact_in: sell,
    };
    let r = swap_exact_in_range(state, meta, &spec);
    if r.flags & (SWAP_FLAG_REFUSED | SWAP_FLAG_MATH | SWAP_FLAG_SATURATED) != 0 {
        return RangeFill::none(state, RFILL_MATH);
    }
    let (t0, t1) = if sell {
        (r.amount_in, r.amount_out)
    } else {
        (r.amount_out, r.amount_in)
    };
    if t0 == 0 || t1 == 0 {
        return RangeFill::none(state, RFILL_LIMIT);
    }
    // Reported quantity: floored to 1e6 human units.
    let q = match scale10(U256::from_u128(t0), 6 - meta.dec0 as i32, false) {
        Some(v) => match to_i64(v) {
            Some(x) if x > 0 => x,
            _ => return RangeFill::none(state, RFILL_MATH),
        },
        None => return RangeFill::none(state, RFILL_MATH),
    };
    // px = t1 · 10^(12 − dec1) / q — token1 human × 1e6 per 1e6 token0 —
    // rounded against us, against the REPORTED quantity.
    let num = match scale10(U256::from_u128(t1), 12 - meta.dec1 as i32, !sell) {
        Some(v) => v,
        None => return RangeFill::none(state, RFILL_MATH),
    };
    let px = if sell {
        mul_div(num, U256::ONE, U256::from_u128(q as u128))
    } else {
        mul_div_rounding_up(num, U256::ONE, U256::from_u128(q as u128))
    };
    let px = match px {
        Some(v) => match to_i64(v) {
            Some(x) if x > 0 => x,
            _ => return RangeFill::none(state, RFILL_MATH),
        },
        None => return RangeFill::none(state, RFILL_MATH),
    };
    if (sell && px < px_limit_1e6) || (!sell && px > px_limit_1e6) {
        return RangeFill::none(state, RFILL_LIMIT);
    }
    let mut flags = 0u8;
    if q < qty_1e6 {
        flags |= RFILL_PARTIAL;
    }
    if r.flags & SWAP_FLAG_EDGE != 0 || r.after.flags & POOL_FLAG_EDGE != 0 {
        flags |= RFILL_EDGE;
    }
    RangeFill {
        qty_1e6: q,
        px_1e6: px,
        flags,
        _pad: [0; 15],
        after: r.after,
    }
}

/// A raw token amount as human units × 1e6, floored: `raw / 10^(dec − 6)`.
/// `None` when it does not fit `i64`.
#[must_use]
pub fn qty_1e6_from_raw(raw: u128, dec: u8) -> Option<i64> {
    match scale10(U256::from_u128(raw), 6 - dec as i32, false) {
        Some(v) => to_i64(v),
        None => None,
    }
}

/// The average price of a swap, token1 per token0 (human) × 1e6:
/// `token1_raw · 10^(dec0 − dec1 + 6) / token0_raw`, rounded as asked
/// (a buy's limit rounds UP, a sell's DOWN — the side that accepts the
/// quote). `None` for a zero leg or a price beyond `i64`.
#[must_use]
pub fn avg_px_1e6(
    token0_raw: u128,
    token1_raw: u128,
    dec0: u8,
    dec1: u8,
    round_up: bool,
) -> Option<i64> {
    if token0_raw == 0 || token1_raw == 0 {
        return None;
    }
    let num = scale10(
        U256::from_u128(token1_raw),
        dec0 as i32 - dec1 as i32 + 6,
        round_up,
    )?;
    let px = if round_up {
        mul_div_rounding_up(num, U256::ONE, U256::from_u128(token0_raw))
    } else {
        mul_div(num, U256::ONE, U256::from_u128(token0_raw))
    }?;
    match to_i64(px) {
        Some(x) if x > 0 => Some(x),
        _ => None,
    }
}

/// The fee, in pips, a swap observed on the tape ACTUALLY paid: the
/// event's input minus the pre-fee input that moves `pre` to the
/// post-swap price, over the event's input, rounded UP.
///
/// `amount0` / `amount1` are the `Swap` event's signed deltas (pool's
/// view: positive = into the pool). In-range only — `None` when the
/// post-swap price is not reachable from `pre` without crossing its
/// active range (the judge holds no map), when the event is not a
/// one-in/one-out swap, or when the arithmetic does not close.
///
/// This is how a pool whose `fee()` does not report the fee it charges
/// (plan findings 15 and 20) is priced honestly: the judge charges the
/// worse of the fee in force and the last one observed.
#[must_use]
pub fn observed_fee_pips(
    pre: &PoolState,
    meta: &PoolMeta,
    amount0: i128,
    amount1: i128,
    post_lo: u128,
    post_hi: u32,
) -> Option<u32> {
    let zero_for_one = amount0 > 0 && amount1 < 0;
    let one_for_zero = amount1 > 0 && amount0 < 0;
    if !(zero_for_one || one_for_zero) {
        return None;
    }
    let amt_in = if zero_for_one {
        amount0 as u128
    } else {
        amount1 as u128
    };
    let (t0, t1, after) = swap_in_range(pre, meta, post_lo, post_hi, u128::MAX, one_for_zero);
    if after.sqrt_price_lo != post_lo || after.sqrt_price_hi != post_hi {
        return None;
    }
    let pf_in = if zero_for_one { t0 } else { t1 };
    if pf_in == 0 || pf_in > amt_in {
        return None;
    }
    let fee = U256::from_u128(amt_in - pf_in);
    let pips = mul_div_rounding_up(fee, U256::from_u128(E6), U256::from_u128(amt_in))?;
    match to_u128(pips) {
        Some(p) if p < PIPS => Some(p as u32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::price::price_1e18_from_sqrt;
    use crate::tick_math::sqrt_at_tick;
    use crate::types::AMM_KIND_V3;

    /// WHYPE (18) / USDC (6) around $97.7, spacing 10, 0.05 %.
    fn pool() -> (PoolState, PoolMeta) {
        let tick = -230_543;
        let (lo, hi) = sqrt_at_tick(tick);
        let st = PoolState::new(lo, hi, tick, 50_000_000_000_000_000_000u128);
        let mut m = PoolMeta::ZERO;
        m.fee_pips = 500;
        m.tick_spacing = 10;
        m.dec0 = 18;
        m.dec1 = 6;
        m.kind = AMM_KIND_V3;
        (st, m)
    }

    fn mid_1e6(st: &PoolState, m: &PoolMeta) -> i64 {
        (price_1e18_from_sqrt(st.sqrt_price_lo, st.sqrt_price_hi, m.dec0, m.dec1) / E12) as i64
    }

    #[test]
    fn a_small_sell_fills_whole_below_the_mid_by_about_the_fee() {
        let (st, m) = pool();
        let mid = mid_1e6(&st, &m);
        let r = fill_in_range(&st, &m, true, 1_000_000, mid * 99 / 100);
        assert_eq!(r.flags, 0, "{r:?}");
        assert_eq!(r.qty_1e6, 1_000_000);
        assert!(r.px_1e6 < mid, "a sell never beats the mid");
        // fee 5 bps + a tiny impact.
        let gap_bps = (mid - r.px_1e6) as i128 * 10_000 / mid as i128;
        assert!((4..=7).contains(&gap_bps), "gap {gap_bps} bps");
        assert!(r.after.sqrt_price_lo < st.sqrt_price_lo, "price moved down");
    }

    #[test]
    fn a_small_buy_pays_above_the_mid_and_moves_the_price_up() {
        let (st, m) = pool();
        let mid = mid_1e6(&st, &m);
        let r = fill_in_range(&st, &m, false, 1_000_000, mid * 101 / 100);
        assert_eq!(r.flags, 0, "{r:?}");
        assert_eq!(r.qty_1e6, 1_000_000);
        assert!(r.px_1e6 > mid);
        assert!(r.after.sqrt_price_lo > st.sqrt_price_lo);
    }

    #[test]
    fn a_limit_already_breached_fills_nothing() {
        let (st, m) = pool();
        let mid = mid_1e6(&st, &m);
        let r = fill_in_range(&st, &m, true, 1_000_000, mid);
        assert!(
            r.flags & RFILL_NONE != 0 && r.flags & RFILL_LIMIT != 0,
            "{r:?}"
        );
        assert_eq!(r.after, st, "no fill, no impact");
        let r = fill_in_range(&st, &m, false, 1_000_000, mid);
        assert!(r.flags & RFILL_LIMIT != 0, "{r:?}");
    }

    #[test]
    fn a_swap_that_would_cross_the_range_is_capped_at_its_boundary() {
        let (st, m) = pool();
        let mid = mid_1e6(&st, &m);
        // 10 M HYPE would walk far past a 10-tick range.
        let r = fill_in_range(&st, &m, true, 10_000_000_000_000, mid / 2);
        assert!(r.flags & RFILL_PARTIAL != 0, "{r:?}");
        assert!(r.flags & RFILL_EDGE != 0, "{r:?}");
        assert!(r.qty_1e6 > 0 && r.qty_1e6 < 10_000_000_000_000);
        assert!(
            !r.after.is_live(),
            "an edge-ended state is not walkable again"
        );
        // And a second order against that state fills nothing.
        let again = fill_in_range(&r.after, &m, true, 1_000_000, mid / 2);
        assert!(again.flags & RFILL_NOT_LIVE != 0);
    }

    #[test]
    fn a_tight_limit_stops_the_walk_inside_the_range_and_holds_the_average() {
        let (st, m) = pool();
        let mid = mid_1e6(&st, &m);
        // Just below mid·(1 − fee): a little room, then the limit binds.
        let lim = mid - mid * 6 / 10_000;
        let r = fill_in_range(&st, &m, true, 10_000_000_000_000, lim);
        assert!(r.flags & RFILL_PARTIAL != 0, "{r:?}");
        assert!(
            r.flags & RFILL_EDGE == 0,
            "the limit, not the edge, stopped it"
        );
        assert!(r.px_1e6 >= lim, "average {} under limit {lim}", r.px_1e6);
    }

    #[test]
    fn not_live_or_malformed_is_refused() {
        let (st, m) = pool();
        let mut stale = st;
        stale.flags = crate::types::POOL_FLAG_STALE;
        assert!(fill_in_range(&stale, &m, true, 1, 1).flags & RFILL_NOT_LIVE != 0);
        assert!(fill_in_range(&st, &m, true, 0, 1).flags & RFILL_NOT_LIVE != 0);
        assert!(fill_in_range(&st, &m, true, 1, -5).flags & RFILL_NOT_LIVE != 0);
        let mut bad = m;
        bad.fee_pips = 1_000_000;
        assert!(fill_in_range(&st, &bad, true, 1, 1).flags & RFILL_NOT_LIVE != 0);
    }

    #[test]
    fn human_unit_helpers_floor_and_round_as_asked() {
        assert_eq!(
            qty_1e6_from_raw(1_500_000_000_000_000_000, 18),
            Some(1_500_000)
        );
        assert_eq!(
            qty_1e6_from_raw(999_999_999_999, 18),
            Some(0),
            "dust floors to zero"
        );
        assert_eq!(qty_1e6_from_raw(123, 2), Some(1_230_000));
        // 2 WHYPE for 195.4 USDC ⇒ 97.7 USDC per WHYPE.
        let t0 = 2_000_000_000_000_000_000u128;
        let t1 = 195_400_000u128;
        assert_eq!(avg_px_1e6(t0, t1, 18, 6, false), Some(97_700_000));
        assert_eq!(avg_px_1e6(t0 + 1, t1, 18, 6, false), Some(97_699_999));
        assert_eq!(avg_px_1e6(t0 + 1, t1, 18, 6, true), Some(97_700_000));
        assert_eq!(avg_px_1e6(0, t1, 18, 6, true), None);
    }

    #[test]
    fn the_observed_fee_recovers_the_fee_a_swap_paid() {
        let (st, mut m) = pool();
        // Charge 2,995 pips (the worst finding-15 pool) while `fee()`
        // would say 500.
        m.fee_pips = 2_995;
        let spec = SwapSpec {
            amount: 3_000_000_000_000_000_000,
            limit_lo: crate::tick_math::MIN_SQRT_LO + 1,
            limit_hi: 0,
            fee_pips: 2_995,
            zero_for_one: true,
            exact_in: true,
        };
        let r = swap_exact_in_range(&st, &m, &spec);
        assert_eq!(r.flags & SWAP_FLAG_EDGE, 0, "stay in range for the fixture");
        let a0 = r.amount_in as i128;
        let a1 = -(r.amount_out as i128);
        m.fee_pips = 500; // what the judge would otherwise believe
        let f = observed_fee_pips(
            &st,
            &m,
            a0,
            a1,
            r.after.sqrt_price_lo,
            r.after.sqrt_price_hi,
        )
        .expect("in range");
        assert!((2_995..=2_996).contains(&f), "solved {f}");
    }

    #[test]
    fn the_observed_fee_refuses_what_it_cannot_solve() {
        let (st, m) = pool();
        assert_eq!(
            observed_fee_pips(&st, &m, 5, 5, st.sqrt_price_lo, 0),
            None,
            "two inputs"
        );
        assert_eq!(observed_fee_pips(&st, &m, 0, 0, st.sqrt_price_lo, 0), None);
        // A post price far outside the active range: not solvable in range.
        let (far, _) = sqrt_at_tick(-231_000);
        assert_eq!(observed_fee_pips(&st, &m, 1_000, -1, far, 0), None);
    }
}
