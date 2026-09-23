// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The single-block arb solve against one pool.
//!
//! The pool is traded to the price at which its MARGINAL price, pool fee
//! included, equals the hedge bound — the profit-maximising size for a
//! concave curve against a flat hedge:
//!
//! * buy token0 (price up) while `P / (1 − f) < eff_bid` ⇒ `P* = eff_bid · (1 − f)`
//! * sell token0 (price down) while `P · (1 − f) > eff_ask` ⇒ `P* = eff_ask / (1 − f)`
//!
//! The swap to `P*` is the contract's own exact arithmetic ([`walk`] with
//! the pool fee), walking the REAL tick map and stopping at its edge; the
//! caller's notional cap — already clamped to the hedge venue's live
//! top-of-book, the largest single correction in the model — bounds the
//! token0 leg. Revenue rounds down, cost rounds up. Gas is charged in
//! full on every quote: a quote that does not clear it is `None`.

use crate::price::{price_1e18_u, scale10, sqrt_from_price_u};
use crate::swap::walk;
use crate::types::{
    ArbParams, ArbQuote, ArbSide, PoolMeta, PoolState, SwapSpec, TickMap, ARB_FLAG_BELOW_GAS, ARB_FLAG_MAP_EDGE,
    ARB_FLAG_MATH, ARB_FLAG_NOT_LIVE, ARB_FLAG_SIZE_CAPPED, SWAP_FLAG_EDGE, SWAP_FLAG_MATH, SWAP_FLAG_REFUSED,
    SWAP_FLAG_SATURATED,
};
use crate::u256::{mul_div, mul_div_rounding_up, U256};

const PIPS: u128 = 1_000_000;

/// `amount0` raw token0 → raw token1 at `px_1e18` (token1 per token0,
/// human, ×1e18): `a0 · px · 10^dec1 / (10^dec0 · 1e18)`.
const fn t0_to_t1_raw(amount0: u128, px_1e18: u128, dec0: u8, dec1: u8, round_up: bool) -> Option<U256> {
    let x = U256::mul_u128(amount0, px_1e18);
    scale10(x, dec1 as i32 - dec0 as i32 - 18, round_up)
}

/// A signed token1 raw amount → USD × 1e6, valuing token1 through
/// token0: `raw / 10^dec1 · px0_usd_1e6 · 1e18 / P_1e18`. Saturates.
const fn t1_raw_to_usd_1e6(mag: U256, negative: bool, px0_usd_1e6: i64, p_1e18: U256, dec1: u8) -> i64 {
    let num = match scale10(mag, 18 - dec1 as i32, false) {
        Some(v) => v,
        None => U256::MAX,
    };
    let v = match mul_div(num, U256::from_u128(px0_usd_1e6 as u128), p_1e18) {
        Some(v) => v.saturating_u128(),
        None => u128::MAX,
    };
    let v = if v > i64::MAX as u128 { i64::MAX } else { v as i64 };
    if negative {
        -v
    } else {
        v
    }
}

/// Profit-maximising single-block arb.
///
/// `p.eff_bid_1e18` / `p.eff_ask_1e18` price token0 in token1 × 1e18
/// with the HEDGE venue's taker fees ALREADY folded in by the caller;
/// only the pool fee is applied here. `p.max_notional_usd_1e6` is the
/// caller's cap AFTER the hedge venue's top-of-book has been applied.
///
/// Never returns positive P&L with `side == None`. A `None` quote leaves
/// `after` equal to `state` (no impact to carry).
#[must_use]
pub fn solve_arb<const N: usize>(state: &PoolState, meta: &PoolMeta, map: &TickMap<N>, p: &ArbParams) -> ArbQuote {
    if !state.is_live() || !map.has_coverage() || p.px0_usd_1e6 <= 0 || p.max_notional_usd_1e6 <= 0 {
        return ArbQuote::none(state, ARB_FLAG_NOT_LIVE, 0);
    }
    let f = meta.fee_pips as u128;
    if f >= PIPS {
        return ArbQuote::none(state, ARB_FLAG_MATH, 0);
    }
    let s = U256::from_u160(state.sqrt_price_lo, state.sqrt_price_hi);
    let p_mid = price_1e18_u(s, meta.dec0, meta.dec1);
    if p_mid.is_zero() {
        return ArbQuote::none(state, ARB_FLAG_NOT_LIVE, 0);
    }
    let one_minus_f = U256::from_u128(PIPS - f);
    let pips = U256::from_u128(PIPS);
    // Marginal comparisons in (price × 1e18 × 1e6) units.
    let bid_net = match U256::from_u128(p.eff_bid_1e18).checked_mul(one_minus_f) {
        Some(v) => v,
        None => return ArbQuote::none(state, ARB_FLAG_MATH, 0),
    };
    let mid_x = match p_mid.checked_mul(pips) {
        Some(v) => v,
        None => return ArbQuote::none(state, ARB_FLAG_MATH, 0),
    };
    let mid_net = match p_mid.checked_mul(one_minus_f) {
        Some(v) => v,
        None => return ArbQuote::none(state, ARB_FLAG_MATH, 0),
    };
    let ask_x = match U256::from_u128(p.eff_ask_1e18).checked_mul(pips) {
        Some(v) => v,
        None => return ArbQuote::none(state, ARB_FLAG_MATH, 0),
    };
    let buy = bid_net > mid_x;
    let sell = !buy && mid_net > ask_x;
    if !buy && !sell {
        return ArbQuote::none(state, 0, 0);
    }
    // Edge at the pre-trade price, bps × 1e6 = (num − den) · 1e10 / den.
    let (num, den) = if buy { (bid_net, mid_x) } else { (mid_net, ask_x) };
    let gross = match num.checked_sub(den) {
        Some(d) => match mul_div(d, U256::from_u128(10_000_000_000), den) {
            Some(v) => {
                let v = v.saturating_u128();
                if v > i64::MAX as u128 {
                    i64::MAX
                } else {
                    v as i64
                }
            }
            None => i64::MAX,
        },
        None => 0,
    };
    // Target price P*.
    let target_px = if buy {
        mul_div(U256::from_u128(p.eff_bid_1e18), one_minus_f, pips)
    } else {
        mul_div_rounding_up(U256::from_u128(p.eff_ask_1e18), pips, one_minus_f)
    };
    let target_px = match target_px {
        Some(v) => v.saturating_u128(),
        None => return ArbQuote::none(state, ARB_FLAG_MATH, 0),
    };
    let target = sqrt_from_price_u(target_px, meta.dec0, meta.dec1);
    // token0 cap from the notional cap.
    let max_t0 = match scale10(U256::from_u128(p.max_notional_usd_1e6 as u128), meta.dec0 as i32, false) {
        Some(v) => match mul_div(v, U256::ONE, U256::from_u128(p.px0_usd_1e6 as u128)) {
            Some(q) => q.saturating_u128(),
            None => 0,
        },
        None => u128::MAX,
    };
    if max_t0 == 0 {
        return ArbQuote::none(state, ARB_FLAG_SIZE_CAPPED, 0);
    }
    let spec = SwapSpec {
        amount: max_t0,
        limit_lo: target.lo,
        limit_hi: target.hi as u32,
        fee_pips: meta.fee_pips,
        zero_for_one: sell,
        // Buying token0: exact OUTPUT of token0 (capped). Selling: exact INPUT.
        exact_in: sell,
    };
    let r = walk(state, meta.tick_spacing, map.nodes(), map.lo_tick, map.hi_tick, true, &spec);
    if r.flags & (SWAP_FLAG_REFUSED | SWAP_FLAG_MATH | SWAP_FLAG_SATURATED) != 0 {
        return ArbQuote::none(state, ARB_FLAG_MATH, 0);
    }
    let mut flags = 0u8;
    if r.flags & SWAP_FLAG_EDGE != 0 {
        flags |= ARB_FLAG_MAP_EDGE;
    }
    let reached = r.after.sqrt_price_lo == target.lo && r.after.sqrt_price_hi == target.hi as u32;
    if !reached && r.flags & SWAP_FLAG_EDGE == 0 {
        flags |= ARB_FLAG_SIZE_CAPPED;
    }
    let (token0, token1) = if buy { (r.amount_out, r.amount_in) } else { (r.amount_in, r.amount_out) };
    if token0 == 0 || token1 == 0 {
        return ArbQuote::none(state, flags, 0);
    }
    // P&L in raw token1: buy sells token0 on the hedge at eff_bid
    // (revenue, rounded down); sell buys it back at eff_ask (cost, up).
    let (plus, minus) = if buy {
        match t0_to_t1_raw(token0, p.eff_bid_1e18, meta.dec0, meta.dec1, false) {
            Some(rev) => (rev, U256::from_u128(token1)),
            None => return ArbQuote::none(state, ARB_FLAG_MATH, 0),
        }
    } else {
        match t0_to_t1_raw(token0, p.eff_ask_1e18, meta.dec0, meta.dec1, true) {
            Some(cost) => (U256::from_u128(token1), cost),
            None => return ArbQuote::none(state, ARB_FLAG_MATH, 0),
        }
    };
    let (mag, negative) = match plus.checked_sub(minus) {
        Some(v) => (v, false),
        None => (minus.wrapping_sub(plus), true),
    };
    let pnl_pre_gas = t1_raw_to_usd_1e6(mag, negative, p.px0_usd_1e6, p_mid, meta.dec1);
    let pnl = pnl_pre_gas.saturating_sub(if p.gas_usd_1e6 > 0 { p.gas_usd_1e6 } else { 0 });
    if pnl <= 0 {
        let fl = if pnl_pre_gas > 0 { flags | ARB_FLAG_BELOW_GAS } else { flags };
        return ArbQuote::none(state, fl, pnl);
    }
    let notional = match scale10(U256::mul_u128(token0, p.px0_usd_1e6 as u128), -(meta.dec0 as i32), false) {
        Some(v) => {
            let v = v.saturating_u128();
            if v > i64::MAX as u128 {
                i64::MAX
            } else {
                v as i64
            }
        }
        None => i64::MAX,
    };
    ArbQuote {
        side: if buy { ArbSide::BuyToken0 } else { ArbSide::SellToken0 },
        flags,
        _pad: [0; 2],
        gross_bps_1e6: gross,
        token0_raw: token0,
        token1_raw: token1,
        pnl_usd_1e6: pnl,
        notional_usd_1e6: notional,
        after: r.after,
    }
}
