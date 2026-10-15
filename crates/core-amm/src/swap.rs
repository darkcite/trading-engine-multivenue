// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The tick walk — the pool contract's swap loop, over a local tick map.
//!
//! The walk reproduces the contract's STEP DECOMPOSITION, not just its
//! price path, because each step rounds its own amounts:
//!
//! * **V3 (`LINKED = false`)** — `UniswapV3Pool.swap` (and its Slipstream
//!   fork): `nextInitializedTickWithinOneWord` stops at every 256-tick
//!   bitmap-word boundary even when no tick there is initialised, and the
//!   target is clamped to `MIN/MAX_TICK`. Emulated from the sorted node
//!   list.
//! * **Algebra Integral (`LINKED = true`)** — `SwapCalculation`: every
//!   step targets the next INITIALISED tick of the linked list (the list
//!   is terminated by `MIN/MAX_TICK` markers, so there is no clamp and no
//!   word stop), and the loop ends after a step that moved the price
//!   without reaching its target.
//!
//! The per-step arithmetic (`movePriceTowardsTarget` ≡ `computeSwapStep`)
//! and the crossing rule are identical in both.
//!
//! Laws: no iterators (`while` + index); no floats; never extrapolate
//! past the coverage; clamp liquidity at zero on an inconsistent map
//! (and say so); bounded step count.

use crate::sqrt_price_math::compute_swap_step;
use crate::tick_math::{
    sqrt_ratio_at_tick, tick_at_sqrt_ratio, MAX_SQRT, MAX_TICK, MIN_SQRT, MIN_TICK,
};
use crate::types::{
    PoolMeta, PoolState, SwapResult, SwapSpec, TickMap, TickNode, AMM_KIND_ALGEBRA, POOL_FLAG_EDGE,
    SWAP_FLAG_EDGE, SWAP_FLAG_LIMIT, SWAP_FLAG_LIQ_CLAMP, SWAP_FLAG_MATH, SWAP_FLAG_SATURATED,
    SWAP_FLAG_STEP_CAP,
};
use crate::u256::U256;

/// Step budget per swap. One block's worth of flow crosses a handful of
/// ticks and word boundaries; hitting this is a flagged anomaly.
pub const MAX_STEPS: u16 = 512;

/// `floor(a / b)` for `b > 0` (Solidity's compressed-tick adjustment).
#[inline(always)]
const fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if (a % b != 0) && (a < 0) {
        q - 1
    } else {
        q
    }
}

/// `nextInitializedTickWithinOneWord` over the sorted node list.
///
/// `cursor` is, going up, the index of the first node ABOVE `tick`, and
/// going down, one past the last node AT OR BELOW `tick` (so `0` means
/// none). Returns `(next_tick, initialised)`, unclamped.
#[inline(always)]
fn next_tick(tick: i32, spacing: i32, lte: bool, nodes: &[TickNode], cursor: usize) -> (i64, bool) {
    let sp = spacing as i64;
    let compressed = floor_div(tick as i64, sp);
    if lte {
        let word_min = (compressed >> 8) << 8;
        if cursor > 0 && cursor <= nodes.len() {
            let c = (nodes[cursor - 1].tick as i64) / sp; // an exact multiple
            if c >= word_min {
                return (nodes[cursor - 1].tick as i64, true);
            }
        }
        (word_min * sp, false)
    } else {
        let c1 = compressed + 1;
        let word_max = ((c1 >> 8) << 8) + 255;
        if cursor < nodes.len() {
            let c = (nodes[cursor].tick as i64) / sp;
            if c <= word_max {
                return (nodes[cursor].tick as i64, true);
            }
        }
        (word_max * sp, false)
    }
}

/// Algebra's step target: the neighbouring INITIALISED tick of the list
/// (same cursor convention as [`next_tick`]); past the map's last node,
/// the coverage edge, uninitialised (the limit is already clamped to it).
#[inline(always)]
fn next_tick_linked(
    lte: bool,
    nodes: &[TickNode],
    cursor: usize,
    lo_edge: i32,
    hi_edge: i32,
) -> (i64, bool) {
    if lte {
        if cursor > 0 && cursor <= nodes.len() {
            return (nodes[cursor - 1].tick as i64, true);
        }
        (lo_edge as i64, false)
    } else {
        if cursor < nodes.len() {
            return (nodes[cursor].tick as i64, true);
        }
        (hi_edge as i64, false)
    }
}

/// First index whose tick is `> tick` (binary search, no iterator).
#[inline]
fn upper_bound(nodes: &[TickNode], tick: i32) -> usize {
    let mut lo = 0usize;
    let mut hi = nodes.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if nodes[mid].tick <= tick {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

#[inline(always)]
const fn min_u(a: U256, b: U256) -> U256 {
    if a.le(b) {
        a
    } else {
        b
    }
}

#[inline(always)]
const fn max_u(a: U256, b: U256) -> U256 {
    if b.le(a) {
        a
    } else {
        b
    }
}

/// The walk. `edges_known`: the map lists every initialised tick inside
/// `[lo_edge, hi_edge]` INCLUSIVE (a fetched map), so ending exactly on
/// an edge is an ordinary cross; `false` (the in-range judge) marks such
/// an ending `POOL_FLAG_EDGE`, because the liquidity beyond is unknown.
/// `LINKED` selects the Algebra loop (module docs); monomorphised, so
/// neither loop pays for the other's branches.
pub(crate) fn walk<const LINKED: bool>(
    state: &PoolState,
    spacing: i32,
    nodes: &[TickNode],
    lo_edge: i32,
    hi_edge: i32,
    edges_known: bool,
    spec: &SwapSpec,
) -> SwapResult {
    let start_sqrt = U256::from_u160(state.sqrt_price_lo, state.sqrt_price_hi);
    if spacing <= 0
        || lo_edge > hi_edge
        || state.tick < lo_edge
        || state.tick > hi_edge
        || start_sqrt < MIN_SQRT
        || start_sqrt >= MAX_SQRT
        || spec.fee_pips >= 1_000_000
    {
        return SwapResult::refused(state);
    }
    let zero_for_one = spec.zero_for_one;
    let lo_e = if lo_edge < MIN_TICK {
        MIN_TICK
    } else {
        lo_edge
    };
    let hi_e = if hi_edge > MAX_TICK {
        MAX_TICK
    } else {
        hi_edge
    };
    let edge_sqrt = if zero_for_one {
        sqrt_ratio_at_tick(lo_e)
    } else {
        sqrt_ratio_at_tick(hi_e)
    };
    let req_limit = U256::from_u160(spec.limit_lo, spec.limit_hi);
    // The requested limit must lie on the trading side of the price.
    if (zero_for_one && req_limit >= start_sqrt) || (!zero_for_one && req_limit <= start_sqrt) {
        return SwapResult::refused(state);
    }
    // Never past the coverage (law — never extrapolate).
    let (limit, edge_binds) = if zero_for_one {
        let l = max_u(req_limit, edge_sqrt);
        (l, l == edge_sqrt && edge_sqrt > req_limit)
    } else {
        let l = min_u(req_limit, edge_sqrt);
        (l, l == edge_sqrt && edge_sqrt < req_limit)
    };

    let mut sqrt = start_sqrt;
    let mut tick = state.tick;
    let mut liq = state.liquidity;
    let mut remaining = U256::from_u128(spec.amount);
    let mut total_in = U256::ZERO;
    let mut total_out = U256::ZERO;
    let mut total_fee = U256::ZERO;
    let mut flags: u8 = 0;
    let mut steps: u16 = 0;
    let mut after_flags: u8 = state.flags & !POOL_FLAG_EDGE;
    let mut cursor = upper_bound(nodes, tick);

    while !remaining.is_zero() && sqrt != limit {
        if steps == MAX_STEPS {
            flags |= SWAP_FLAG_STEP_CAP;
            break;
        }
        steps += 1;
        let step_start = sqrt;
        let (nt, initialised) = if LINKED {
            next_tick_linked(zero_for_one, nodes, cursor, lo_e, hi_e)
        } else {
            next_tick(tick, spacing, zero_for_one, nodes, cursor)
        };
        let nt = if nt < MIN_TICK as i64 {
            MIN_TICK
        } else if nt > MAX_TICK as i64 {
            MAX_TICK
        } else {
            nt as i32
        };
        let sqrt_next = sqrt_ratio_at_tick(nt);
        let target = if zero_for_one {
            max_u(sqrt_next, limit)
        } else {
            min_u(sqrt_next, limit)
        };
        let st = match compute_swap_step(sqrt, target, liq, remaining, spec.exact_in, spec.fee_pips)
        {
            Some(s) => s,
            None => {
                flags |= SWAP_FLAG_MATH;
                break;
            }
        };
        sqrt = st.sqrt_next;
        let spent = st.amount_in.wrapping_add(st.fee);
        if spec.exact_in {
            remaining = match remaining.checked_sub(spent) {
                Some(v) => v,
                None => U256::ZERO,
            };
        } else {
            remaining = match remaining.checked_sub(st.amount_out) {
                Some(v) => v,
                None => U256::ZERO,
            };
        }
        total_in = total_in.wrapping_add(spent);
        total_out = total_out.wrapping_add(st.amount_out);
        total_fee = total_fee.wrapping_add(st.fee);

        if sqrt == sqrt_next {
            if initialised {
                // Up: cross nodes[cursor]; down: cross nodes[cursor - 1].
                let idx = if zero_for_one { cursor - 1 } else { cursor };
                let net = nodes[idx].liquidity_net;
                let net = if zero_for_one {
                    net.wrapping_neg()
                } else {
                    net
                };
                if net >= 0 {
                    liq = liq.saturating_add(net as u128);
                } else {
                    let d = net.unsigned_abs();
                    if d > liq {
                        flags |= SWAP_FLAG_LIQ_CLAMP;
                        liq = 0;
                    } else {
                        liq -= d;
                    }
                }
                cursor = if zero_for_one { cursor - 1 } else { cursor + 1 };
            }
            tick = if zero_for_one { nt - 1 } else { nt };
        } else if sqrt != step_start {
            tick = tick_at_sqrt_ratio(sqrt);
            if LINKED {
                // Algebra: a step that stopped short of its target ends
                // the swap (the remainder was absorbed by the step).
                break;
            }
        }
    }
    if sqrt == limit && !remaining.is_zero() {
        flags |= if edge_binds {
            SWAP_FLAG_EDGE
        } else {
            SWAP_FLAG_LIMIT
        };
    }
    if !edges_known && sqrt == edge_sqrt {
        // Landed on an edge whose far side is unknown: this state's
        // liquidity must not be walked further.
        after_flags |= POOL_FLAG_EDGE;
    }
    let (sp_lo, sp_hi) = match sqrt.to_u160() {
        Some(v) => v,
        None => (state.sqrt_price_lo, state.sqrt_price_hi),
    };
    let mut after = *state;
    after.sqrt_price_lo = sp_lo;
    after.sqrt_price_hi = sp_hi;
    after.tick = tick;
    after.liquidity = liq;
    after.flags = after_flags;
    if total_in.hi != 0 || total_out.hi != 0 || total_fee.hi != 0 {
        flags |= SWAP_FLAG_SATURATED;
    }
    SwapResult::new(
        total_in.saturating_u128(),
        total_out.saturating_u128(),
        total_fee.saturating_u128(),
        steps,
        flags,
        after,
    )
}

/// [`walk`] in the loop `meta.kind` selects.
#[inline(always)]
pub(crate) fn walk_kind(
    meta: &PoolMeta,
    state: &PoolState,
    nodes: &[TickNode],
    lo_edge: i32,
    hi_edge: i32,
    edges_known: bool,
    spec: &SwapSpec,
) -> SwapResult {
    if meta.kind == AMM_KIND_ALGEBRA {
        walk::<true>(
            state,
            meta.tick_spacing,
            nodes,
            lo_edge,
            hi_edge,
            edges_known,
            spec,
        )
    } else {
        walk::<false>(
            state,
            meta.tick_spacing,
            nodes,
            lo_edge,
            hi_edge,
            edges_known,
            spec,
        )
    }
}

/// One swap exactly as the pool contract would execute it, walking the
/// map's initialised ticks (and, for V3, its bitmap-word boundaries) with
/// the contract's rounding, in the loop of `meta.kind`. `spec.fee_pips`
/// is the fee in force (a dynamic-fee pool passes the fee of that swap).
#[must_use]
pub fn swap_exact<const N: usize>(
    state: &PoolState,
    meta: &PoolMeta,
    map: &TickMap<N>,
    spec: &SwapSpec,
) -> SwapResult {
    if !map.has_coverage() {
        return SwapResult::refused(state);
    }
    walk_kind(
        meta,
        state,
        map.nodes(),
        map.lo_tick,
        map.hi_tick,
        true,
        spec,
    )
}

/// Walk the tick map from `state` toward the target price, exact-input,
/// and return `(token0_raw, token1_raw, state_after)` **PRE-FEE**.
///
/// Upward (`up`): token0 is OUTPUT, token1 INPUT; `max_token0_raw` caps
/// the output. Downward: token0 is INPUT, token1 OUTPUT; the cap is on
/// the input. `u128::MAX` means uncapped; `0` moves nothing.
///
/// **Law — never extrapolate.** The target is clamped to the map's
/// `[lo_tick, hi_tick]`; the walk stops at the edge.
#[must_use]
pub fn swap_to_target<const N: usize>(
    state: &PoolState,
    meta: &PoolMeta,
    map: &TickMap<N>,
    sqrt_target_lo: u128,
    sqrt_target_hi: u32,
    max_token0_raw: u128,
    up: bool,
) -> (u128, u128, PoolState) {
    if max_token0_raw == 0 || !map.has_coverage() {
        return (0, 0, *state);
    }
    let spec = SwapSpec {
        amount: max_token0_raw,
        limit_lo: sqrt_target_lo,
        limit_hi: sqrt_target_hi,
        fee_pips: 0,
        zero_for_one: !up,
        exact_in: !up,
    };
    let r = walk_kind(
        meta,
        state,
        map.nodes(),
        map.lo_tick,
        map.hi_tick,
        true,
        &spec,
    );
    if up {
        (r.amount_out, r.amount_in, r.after)
    } else {
        (r.amount_in, r.amount_out, r.after)
    }
}

/// Exact-input swap WITHIN the active tick range only (constant `L`),
/// `(token0_raw, token1_raw, state_after)` **PRE-FEE**, same direction
/// and cap conventions as [`swap_to_target`].
///
/// For the paper matcher, which holds no tick map. **Conservative by
/// construction:** a swap that would cross a tick is capped at the range
/// boundary, so this fills LESS than the chain would, never more. A walk
/// that ends on the boundary marks `after` with `POOL_FLAG_EDGE`: the
/// liquidity on the far side is unknown until a real update arrives.
#[must_use]
pub fn swap_in_range(
    state: &PoolState,
    meta: &PoolMeta,
    sqrt_target_lo: u128,
    sqrt_target_hi: u32,
    max_token0_raw: u128,
    up: bool,
) -> (u128, u128, PoolState) {
    if max_token0_raw == 0 {
        return (0, 0, *state);
    }
    let (lo, hi) = crate::price::range_bounds(state.tick, meta.tick_spacing);
    let spec = SwapSpec {
        amount: max_token0_raw,
        limit_lo: sqrt_target_lo,
        limit_hi: sqrt_target_hi,
        fee_pips: 0,
        zero_for_one: !up,
        exact_in: !up,
    };
    let r = walk_kind(meta, state, &[], lo, hi, false, &spec);
    if up {
        (r.amount_out, r.amount_in, r.after)
    } else {
        (r.amount_in, r.amount_out, r.after)
    }
}

/// [`swap_exact`] within the active range only, for a judge with no tick
/// map: the contract's exact fee-inclusive arithmetic, capped at the
/// range boundary (conservative — never fills more than the chain).
#[must_use]
pub fn swap_exact_in_range(state: &PoolState, meta: &PoolMeta, spec: &SwapSpec) -> SwapResult {
    let (lo, hi) = crate::price::range_bounds(state.tick, meta.tick_spacing);
    walk_kind(meta, state, &[], lo, hi, false, spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_math::sqrt_at_tick;

    #[test]
    fn next_tick_matches_bitmap_semantics() {
        // spacing 10, nodes at -100, 0, 50
        let nodes = [
            TickNode::new(-100, 5),
            TickNode::new(0, 7),
            TickNode::new(50, -12),
        ];
        // up from 5: next initialised 50 (same word)
        let c = upper_bound(&nodes, 5);
        assert_eq!(next_tick(5, 10, false, &nodes, c), (50, true));
        // down from 5: 0 is <= 5
        assert_eq!(next_tick(5, 10, true, &nodes, c), (0, true));
        // down from -1: compressed -1 is in word -1 → [-256, -1]; -100 is there
        let c = upper_bound(&nodes, -1);
        assert_eq!(next_tick(-1, 10, true, &nodes, c), (-100, true));
        // up from 60, nothing above in word 0 → word max 255·10
        let c = upper_bound(&nodes, 60);
        assert_eq!(next_tick(60, 10, false, &nodes, c), (2550, false));
        // down from -2570 (compressed -257, word -2) with no node there
        let c = upper_bound(&nodes, -2570);
        assert_eq!(next_tick(-2570, 10, true, &nodes, c), (-5120, false));
    }

    #[test]
    fn refuses_limit_on_wrong_side_and_out_of_coverage() {
        let (lo, hi) = sqrt_at_tick(0);
        let s = PoolState::new(lo, hi, 0, 1_000_000_000_000);
        let mut m = TickMap::<4>::EMPTY;
        m.load(&[], -600, 600, 60).unwrap();
        let mut meta = PoolMeta::ZERO;
        meta.tick_spacing = 60;
        let (llo, lhi) = sqrt_at_tick(10);
        let spec = SwapSpec {
            amount: 1_000,
            limit_lo: llo,
            limit_hi: lhi,
            fee_pips: 500,
            zero_for_one: true,
            exact_in: true,
        };
        assert_eq!(
            swap_exact(&s, &meta, &m, &spec).flags,
            crate::SWAP_FLAG_REFUSED
        );
        let far = PoolState::new(lo, hi, 900, 1);
        let spec = SwapSpec {
            zero_for_one: false,
            ..spec
        };
        assert_eq!(
            swap_exact(&far, &meta, &m, &spec).flags,
            crate::SWAP_FLAG_REFUSED
        );
    }

    #[test]
    fn stops_at_the_map_edge_and_says_so() {
        let (lo, hi) = sqrt_at_tick(0);
        let s = PoolState::new(lo, hi, 0, 1_000_000_000_000_000);
        let mut m = TickMap::<4>::EMPTY;
        m.load(&[], -60, 60, 60).unwrap();
        let mut meta = PoolMeta::ZERO;
        meta.tick_spacing = 60;
        let (llo, lhi) = sqrt_at_tick(6000);
        let spec = SwapSpec {
            amount: u128::MAX >> 1,
            limit_lo: llo,
            limit_hi: lhi,
            fee_pips: 3_000,
            zero_for_one: false,
            exact_in: true,
        };
        let r = swap_exact(&s, &meta, &m, &spec);
        assert_eq!(r.flags & SWAP_FLAG_EDGE, SWAP_FLAG_EDGE);
        assert_eq!(r.after.tick, 60);
        assert_eq!(
            (r.after.sqrt_price_lo, r.after.sqrt_price_hi),
            sqrt_at_tick(60)
        );
    }
}
