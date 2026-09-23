// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the AMM tick walk and the arb solve (HYPARB H1, `core-amm`).
//!
//! Arbitrary bytes become a pool state, a tick map (possibly
//! INCONSISTENT — nets that do not sum, liquidity that underflows on a
//! cross), a swap request and hedge bounds. The walk's failure mode is
//! not only a crash: an amount that exceeds what was asked, or a quote
//! with positive P&L and no side, would be booked as edge.
//!
//! What must hold for every input:
//!
//! * no panic on any path (every division is checked, every index is
//!   bounded by the loop that produced it);
//! * exact-in never spends more than offered, exact-out never delivers
//!   more than asked, the fee is part of the input;
//! * a refused swap moves nothing;
//! * `solve_arb` never returns positive P&L with `ArbSide::None`, and a
//!   `None` quote leaves the pool untouched.

#![no_main]

use libfuzzer_sys::fuzz_target;

use core_amm::{
    solve_arb, sqrt_at_tick, swap_exact, swap_exact_in_range, ArbParams, ArbSide, PoolMeta, PoolState, SwapSpec, TickMap,
    TickNode, MAX_TICK, MIN_TICK, SWAP_FLAG_REFUSED,
};

fn take<const K: usize>(d: &[u8], i: &mut usize) -> [u8; K] {
    let mut b = [0u8; K];
    let mut k = 0;
    while k < K {
        b[k] = d.get(*i + k).copied().unwrap_or(0);
        k += 1;
    }
    *i += K;
    b
}

fuzz_target!(|d: &[u8]| {
    let mut i = 0usize;
    let spacing = (u16::from_le_bytes(take::<2>(d, &mut i)) % 400) as i32 + 1;
    let tick = i32::from_le_bytes(take::<4>(d, &mut i)) % (MAX_TICK - 1);
    let liq = u128::from_le_bytes(take::<16>(d, &mut i)) >> (take::<1>(d, &mut i)[0] % 128);
    let flags = take::<1>(d, &mut i)[0] & 3;
    let (lo, hi) = sqrt_at_tick(tick);
    let mut state = PoolState::new(lo, hi, tick, liq);
    state.flags = flags;
    let mut meta = PoolMeta::ZERO;
    meta.tick_spacing = spacing;
    meta.fee_pips = u32::from_le_bytes(take::<4>(d, &mut i)) % 1_000_001;
    meta.dec0 = take::<1>(d, &mut i)[0] % 25;
    meta.dec1 = take::<1>(d, &mut i)[0] % 25;

    // Up to 32 nodes around the tick; ascending by construction, nets arbitrary.
    let n = (take::<1>(d, &mut i)[0] % 33) as usize;
    let mut nodes = [TickNode::ZERO; 32];
    let base = (tick / spacing) * spacing;
    let mut t = base - (n as i32 / 2) * spacing * 3;
    let mut k = 0;
    while k < n {
        t += spacing * ((take::<1>(d, &mut i)[0] % 4) as i32 + 1);
        let net = i128::from_le_bytes(take::<16>(d, &mut i)) >> (take::<1>(d, &mut i)[0] % 127);
        nodes[k] = TickNode::new(t, net);
        k += 1;
    }
    let lo_t = ((base - spacing * 200) / spacing) * spacing;
    let hi_t = ((base + spacing * 200) / spacing) * spacing;
    let mut map = TickMap::<32>::EMPTY;
    let loaded = lo_t > MIN_TICK && hi_t < MAX_TICK && map.load(&nodes[..n], lo_t, hi_t, spacing).is_ok();

    let amount = u128::from_le_bytes(take::<16>(d, &mut i)) >> (take::<1>(d, &mut i)[0] % 128);
    let dt = i32::from_le_bytes(take::<4>(d, &mut i)) % 20_000;
    let tgt = (tick + dt).clamp(MIN_TICK, MAX_TICK);
    let (llo, lhi) = sqrt_at_tick(tgt);
    let spec = SwapSpec {
        amount,
        limit_lo: llo,
        limit_hi: lhi,
        fee_pips: meta.fee_pips,
        zero_for_one: dt < 0,
        exact_in: take::<1>(d, &mut i)[0] & 1 == 0,
    };
    let r = if loaded { swap_exact(&state, &meta, &map, &spec) } else { swap_exact_in_range(&state, &meta, &spec) };
    if spec.exact_in {
        assert!(r.amount_in <= amount, "exact-in spent {} of {}", r.amount_in, amount);
    } else {
        assert!(r.amount_out <= amount, "exact-out delivered {} of {}", r.amount_out, amount);
    }
    assert!(r.fee <= r.amount_in);
    if r.flags & SWAP_FLAG_REFUSED != 0 {
        assert_eq!((r.amount_in, r.amount_out), (0, 0));
        assert_eq!(r.after, state);
    }

    if loaded {
        let p = ArbParams {
            eff_bid_1e18: u128::from_le_bytes(take::<16>(d, &mut i)) >> 40,
            eff_ask_1e18: u128::from_le_bytes(take::<16>(d, &mut i)) >> 40,
            px0_usd_1e6: i64::from_le_bytes(take::<8>(d, &mut i)),
            max_notional_usd_1e6: i64::from_le_bytes(take::<8>(d, &mut i)),
            gas_usd_1e6: i64::from_le_bytes(take::<8>(d, &mut i)),
        };
        let q = solve_arb(&state, &meta, &map, &p);
        if q.side == ArbSide::None {
            assert!(q.pnl_usd_1e6 <= 0);
            assert_eq!(q.after, state);
            assert_eq!((q.token0_raw, q.token1_raw), (0, 0));
        } else {
            assert!(q.pnl_usd_1e6 > 0);
        }
    }
});
