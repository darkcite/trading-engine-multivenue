// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Property tests over the tick walk and the arb solve, on random but
//! CONSISTENT pools: the map is built from random positions, so the
//! in-range liquidity and every `liquidityNet` agree the way a real
//! pool's do.

use core_amm::{
    solve_arb, sqrt_at_tick, swap_exact, swap_in_range, swap_to_target, ArbParams, ArbSide, PoolMeta, PoolState,
    SwapSpec, TickMap, TickNode, SWAP_FLAG_LIQ_CLAMP, SWAP_FLAG_MATH, SWAP_FLAG_REFUSED,
};
use proptest::prelude::*;

const SPACING: i32 = 10;
const COVER: i32 = 4_000 * SPACING;

/// A pool around `tick0` from up to 12 positions `(lo_off, width, L)`.
fn build(tick0: i32, positions: &[(i32, i32, u128)]) -> (PoolState, PoolMeta, TickMap<64>) {
    let base = (tick0 / SPACING) * SPACING;
    let mut nets: Vec<(i32, i128)> = Vec::new();
    let mut active: u128 = 0;
    for &(off, w, l) in positions {
        let lo = base + off * SPACING;
        let hi = lo + w.max(1) * SPACING;
        if lo < base - COVER || hi > base + COVER {
            continue;
        }
        nets.push((lo, l as i128));
        nets.push((hi, -(l as i128)));
        if lo <= tick0 && tick0 < hi {
            active += l;
        }
    }
    nets.sort_by_key(|x| x.0);
    let mut merged: Vec<TickNode> = Vec::new();
    for (t, n) in nets {
        match merged.last_mut() {
            Some(last) if last.tick == t => last.liquidity_net += n,
            _ => merged.push(TickNode::new(t, n)),
        }
    }
    let mut map = TickMap::<64>::EMPTY;
    map.load(&merged, base - COVER, base + COVER, SPACING).unwrap();
    let (lo, hi) = sqrt_at_tick(tick0);
    let state = PoolState::new(lo, hi, tick0, active);
    let mut meta = PoolMeta::ZERO;
    meta.tick_spacing = SPACING;
    meta.fee_pips = 3_000;
    meta.dec0 = 18;
    meta.dec1 = 6;
    (state, meta, map)
}

fn positions() -> impl Strategy<Value = Vec<(i32, i32, u128)>> {
    prop::collection::vec((-300i32..300, 1i32..400, 1_000_000u128..1_000_000_000_000_000_000), 1..12)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    /// Up to a target, then back down to the start price: the pool
    /// returns to within one tick, and the round trip never pays out.
    #[test]
    fn up_then_down_returns(tick0 in -250_000i32..-150_000, dt in 1i32..3_000, pos in positions()) {
        let (s, meta, map) = build(tick0, &pos);
        prop_assume!(s.liquidity > 0);
        let (tlo, thi) = sqrt_at_tick(tick0 + dt);
        let (t0_out, t1_in, up) = swap_to_target(&s, &meta, &map, tlo, thi, u128::MAX, true);
        prop_assume!(t0_out > 0);
        let (t0_in, t1_out, back) = swap_to_target(&up, &meta, &map, s.sqrt_price_lo, s.sqrt_price_hi, u128::MAX, false);
        prop_assert!((back.tick - s.tick).abs() <= 1, "start {} back {}", s.tick, back.tick);
        prop_assert!(back.liquidity == s.liquidity || (back.tick - s.tick).abs() <= 1);
        // Rounding favours the pool on both legs.
        prop_assert!(t0_in >= t0_out);
        prop_assert!(t1_out <= t1_in);
    }

    /// Farther target ⇒ at least as much traded.
    #[test]
    fn monotone_in_target(tick0 in -250_000i32..-150_000, d1 in 1i32..2_000, d2 in 1i32..2_000, pos in positions()) {
        let (s, meta, map) = build(tick0, &pos);
        let (a, b) = if d1 <= d2 { (d1, d2) } else { (d2, d1) };
        let (lo_a, hi_a) = sqrt_at_tick(tick0 + a);
        let (lo_b, hi_b) = sqrt_at_tick(tick0 + b);
        let (x0, x1, _) = swap_to_target(&s, &meta, &map, lo_a, hi_a, u128::MAX, true);
        let (y0, y1, _) = swap_to_target(&s, &meta, &map, lo_b, hi_b, u128::MAX, true);
        prop_assert!(y0 >= x0 && y1 >= x1);
        let (lo_a, hi_a) = sqrt_at_tick(tick0 - a);
        let (lo_b, hi_b) = sqrt_at_tick(tick0 - b);
        let (x0, x1, _) = swap_to_target(&s, &meta, &map, lo_a, hi_a, u128::MAX, false);
        let (y0, y1, _) = swap_to_target(&s, &meta, &map, lo_b, hi_b, u128::MAX, false);
        prop_assert!(y0 >= x0 && y1 >= x1);
    }

    /// A capped swap never trades more than an uncapped one, and never
    /// more token0 than the cap.
    #[test]
    fn cap_never_increases(tick0 in -250_000i32..-150_000, dt in 1i32..3_000, cap in 1u128..1_000_000_000_000_000_000, pos in positions()) {
        let (s, meta, map) = build(tick0, &pos);
        let (lo, hi) = sqrt_at_tick(tick0 + dt);
        let (c0, c1, _) = swap_to_target(&s, &meta, &map, lo, hi, cap, true);
        let (u0, u1, _) = swap_to_target(&s, &meta, &map, lo, hi, u128::MAX, true);
        prop_assert!(c0 <= u0 && c1 <= u1 && c0 <= cap);
    }

    /// The matcher's in-range judge never fills more than the full walk.
    #[test]
    fn in_range_never_exceeds_walk(tick0 in -250_000i32..-150_000, dt in -3_000i32..3_000, pos in positions()) {
        prop_assume!(dt != 0);
        let (s, meta, map) = build(tick0, &pos);
        let (lo, hi) = sqrt_at_tick(tick0 + dt);
        let up = dt > 0;
        let (r0, r1, _) = swap_in_range(&s, &meta, lo, hi, u128::MAX, up);
        let (w0, w1, _) = swap_to_target(&s, &meta, &map, lo, hi, u128::MAX, up);
        prop_assert!(r0 <= w0 && r1 <= w1);
    }

    /// On a consistent map the walk never needs the liquidity clamp and
    /// never hits a math error.
    #[test]
    fn consistent_map_is_clean(tick0 in -250_000i32..-150_000, dt in -3_000i32..3_000,
                               amt in 1u128..1_000_000_000_000_000_000_000, pos in positions(), exact_in in any::<bool>()) {
        prop_assume!(dt != 0);
        let (s, meta, map) = build(tick0, &pos);
        let (lo, hi) = sqrt_at_tick(tick0 + dt);
        let spec = SwapSpec { amount: amt, limit_lo: lo, limit_hi: hi, fee_pips: 3_000, zero_for_one: dt < 0, exact_in };
        let r = swap_exact(&s, &meta, &map, &spec);
        prop_assert_eq!(r.flags & (SWAP_FLAG_LIQ_CLAMP | SWAP_FLAG_MATH | SWAP_FLAG_REFUSED), 0);
        if exact_in { prop_assert!(r.amount_in <= amt); } else { prop_assert!(r.amount_out <= amt); }
        prop_assert!(r.fee <= r.amount_in);
    }

    /// Fee zero, exact-in, uncapped: identical to the pre-fee walk.
    #[test]
    fn fee_zero_matches_pre_fee(tick0 in -250_000i32..-150_000, dt in 1i32..3_000, pos in positions()) {
        let (s, meta, map) = build(tick0, &pos);
        let (lo, hi) = sqrt_at_tick(tick0 - dt);
        let (p0, p1, pa) = swap_to_target(&s, &meta, &map, lo, hi, u128::MAX, false);
        let spec = SwapSpec { amount: u128::MAX >> 2, limit_lo: lo, limit_hi: hi, fee_pips: 0, zero_for_one: true, exact_in: true };
        let r = swap_exact(&s, &meta, &map, &spec);
        prop_assert_eq!((r.amount_in, r.amount_out, r.after), (p0, p1, pa));
    }

    /// `side == None` never carries positive P&L, and a trade never
    /// reports a P&L it did not clear.
    #[test]
    fn arb_never_positive_without_a_side(tick0 in -240_000i32..-220_000, pos in positions(),
                                         bid_bps in -300i64..300, spread_bps in 0i64..50,
                                         cap in 1i64..100_000_000_000, gas in 0i64..5_000_000) {
        let (s, meta, map) = build(tick0, &pos);
        prop_assume!(s.liquidity > 0);
        let mid = core_amm::price_1e18_from_sqrt(s.sqrt_price_lo, s.sqrt_price_hi, 18, 6) as i128;
        let bid = mid + mid * bid_bps as i128 / 10_000;
        let ask = bid + bid * spread_bps as i128 / 10_000;
        prop_assume!(bid > 0);
        let px0 = (mid / 1_000_000_000_000) as i64; // USDC token1 ⇒ USD
        prop_assume!(px0 > 0);
        let q = solve_arb(&s, &meta, &map, &ArbParams {
            eff_bid_1e18: bid as u128, eff_ask_1e18: ask as u128,
            px0_usd_1e6: px0, max_notional_usd_1e6: cap, gas_usd_1e6: gas,
        });
        if q.side == ArbSide::None {
            prop_assert!(q.pnl_usd_1e6 <= 0);
            prop_assert_eq!(q.after, s);
            prop_assert_eq!((q.token0_raw, q.token1_raw), (0, 0));
        } else {
            prop_assert!(q.pnl_usd_1e6 > 0);
            prop_assert!(q.notional_usd_1e6 <= cap + 1);
            match q.side {
                ArbSide::BuyToken0 => prop_assert!(q.after.sqrt_price_lo > s.sqrt_price_lo || q.after.sqrt_price_hi > s.sqrt_price_hi),
                ArbSide::SellToken0 => prop_assert!(q.after.sqrt_price_lo < s.sqrt_price_lo || q.after.sqrt_price_hi < s.sqrt_price_hi),
                ArbSide::None => unreachable!(),
            }
        }
    }
}

#[test]
fn a_hedge_inside_the_fee_band_never_trades() {
    let (s, meta, map) = build(-230_540, &[(-50, 100, 1_000_000_000_000_000_000)]);
    let mid = core_amm::price_1e18_from_sqrt(s.sqrt_price_lo, s.sqrt_price_hi, 18, 6);
    // ±25 bps around mid, inside the 30 bps pool fee.
    let q = solve_arb(&s, &meta, &map, &ArbParams {
        eff_bid_1e18: mid - mid / 400, eff_ask_1e18: mid + mid / 400,
        px0_usd_1e6: (mid / 1_000_000_000_000) as i64, max_notional_usd_1e6: 1_000_000_000, gas_usd_1e6: 0,
    });
    assert_eq!(q.side, ArbSide::None);
}

#[test]
fn a_wide_gap_trades_and_stops_at_the_optimum() {
    let (s, meta, map) = build(-230_540, &[(-500, 1_000, 1_000_000_000_000_000_000)]);
    let mid = core_amm::price_1e18_from_sqrt(s.sqrt_price_lo, s.sqrt_price_hi, 18, 6);
    let bid = mid + mid / 100; // hedge bid 100 bps over the pool
    let q = solve_arb(&s, &meta, &map, &ArbParams {
        eff_bid_1e18: bid, eff_ask_1e18: bid + bid / 10_000,
        px0_usd_1e6: (mid / 1_000_000_000_000) as i64, max_notional_usd_1e6: i64::MAX, gas_usd_1e6: 10_000,
    });
    assert_eq!(q.side, ArbSide::BuyToken0);
    assert!(q.pnl_usd_1e6 > 0);
    // After the trade the pool's marginal ask (fee in) sits at the hedge bid.
    let after = core_amm::price_1e18_from_sqrt(q.after.sqrt_price_lo, q.after.sqrt_price_hi, 18, 6);
    let want = bid - bid * 3_000 / 1_000_000;
    let err = after.abs_diff(want);
    assert!(err <= want / 1_000_000, "after {after} want {want}");
    // Re-solving from the carried state finds nothing left.
    let q2 = solve_arb(&q.after, &meta, &map, &ArbParams {
        eff_bid_1e18: bid, eff_ask_1e18: bid + bid / 10_000,
        px0_usd_1e6: (mid / 1_000_000_000_000) as i64, max_notional_usd_1e6: i64::MAX, gas_usd_1e6: 10_000,
    });
    assert_eq!(q2.side, ArbSide::None);
}
