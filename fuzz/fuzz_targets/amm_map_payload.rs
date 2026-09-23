// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the pool-event payload codec and tick-map maintenance (HYPARB,
//! `core-amm::payload`, `TickMap::apply_position`).
//!
//! The first 40 bytes are a payload straight off the ring; the rest is a
//! sequence of `Mint`/`Burn` position changes against a small map. Both
//! failure modes are silent: a decoder that accepts two byte forms of one
//! event (or half-validates a corrupted one) feeds the member a state
//! that never happened; a mutation that fails half-way leaves a map that
//! walks through liquidity the chain does not have.
//!
//! What must hold for every input:
//!
//! * no panic;
//! * `decode` is CANONICAL — anything it accepts re-encodes to the same
//!   40 bytes;
//! * `apply_position` is all-or-nothing — on `Err` the map is
//!   byte-for-byte unchanged;
//! * after every accepted change the map is well-formed: strictly
//!   ascending, inside its coverage, every node initialised
//!   (`gross > 0`) with `gross >= |net|`, never over capacity;
//! * the pool's liquidity moves only when the position spans its tick.

#![no_main]

use libfuzzer_sys::fuzz_target;

use core_amm::payload::{
    decode, encode_fee, encode_gap, encode_head, encode_liquidity, encode_snapshot, encode_state,
    encode_swap, encode_tick, PoolEvent, PAYLOAD_LEN,
};
use core_amm::{PoolState, TickMap, TickNode, MAX_TICK, MIN_TICK};

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
    let p = take::<PAYLOAD_LEN>(d, &mut i);
    if let Some(ev) = decode(&p) {
        let back = match ev {
            PoolEvent::State {
                tick,
                sqrt_price_lo,
                sqrt_price_hi,
                liquidity,
                snapshot,
            } => encode_state(tick, sqrt_price_lo, sqrt_price_hi, liquidity, snapshot),
            PoolEvent::Swap {
                block,
                amount0,
                amount1,
            } => encode_swap(block, amount0, amount1),
            PoolEvent::Fee {
                block,
                source,
                a,
                b,
            } => encode_fee(block, source, a, b),
            PoolEvent::Liquidity {
                block,
                burn,
                tick_lower,
                tick_upper,
                amount,
            } => encode_liquidity(block, burn, tick_lower, tick_upper, amount),
            PoolEvent::Head {
                block,
                timestamp,
                base_fee,
            } => encode_head(block, timestamp, base_fee),
            PoolEvent::Gap { block } => encode_gap(block),
            PoolEvent::Snapshot {
                block,
                family,
                lo,
                hi,
                nodes,
                fee,
                spacing,
            } => encode_snapshot(block, family, lo, hi, nodes, fee, spacing),
            PoolEvent::Tick { tick, net, gross } => encode_tick(tick, net, gross),
        };
        assert_eq!(back, Some(p), "decode accepted a non-canonical payload");
    }

    let lo = (i16::from_le_bytes(take::<2>(d, &mut i)) as i32) * 4;
    let hi = lo + (u16::from_le_bytes(take::<2>(d, &mut i)) as i32 % 4_000);
    let mut map = TickMap::<16>::EMPTY;
    if map.load(&[], lo, hi, 1).is_err() {
        return;
    }
    let tick = lo + (i16::from_le_bytes(take::<2>(d, &mut i)) as i32);
    let mut state = PoolState::new(
        1,
        0,
        tick.clamp(MIN_TICK, MAX_TICK),
        u128::from_le_bytes(take::<16>(d, &mut i)) >> 64,
    );
    let mut snap = [TickNode::ZERO; 16];
    while i < d.len() {
        let a = (i16::from_le_bytes(take::<2>(d, &mut i)) as i32) * 3;
        let w = (u16::from_le_bytes(take::<2>(d, &mut i)) as i32) % 3_000 - 5;
        let raw = i64::from_le_bytes(take::<8>(d, &mut i));
        let delta = (raw >> (take::<1>(d, &mut i)[0] % 64)) as i128;
        let n = map.len();
        snap[..n].copy_from_slice(map.nodes());
        match map.apply_position(a, a + w, delta) {
            Err(_) => assert_eq!(map.nodes(), &snap[..n], "a refused change moved the map"),
            Ok(()) => {
                let nodes = map.nodes();
                assert!(nodes.len() <= 16);
                let mut k = 0;
                while k < nodes.len() {
                    let t = nodes[k];
                    assert!(
                        t.tick >= map.lo_tick && t.tick <= map.hi_tick,
                        "node outside coverage"
                    );
                    assert!(
                        k == 0 || nodes[k - 1].tick < t.tick,
                        "map not strictly ascending"
                    );
                    assert!(
                        t.liquidity_gross() > 0,
                        "a de-initialised tick stayed in the map"
                    );
                    assert!(
                        t.liquidity_gross() >= t.liquidity_net.unsigned_abs(),
                        "gross below |net|"
                    );
                    k += 1;
                }
            }
        }
        let before = state;
        match state.apply_position(a, a + w, delta) {
            Err(_) => assert_eq!(state, before),
            Ok(()) => {
                if state.tick < a || state.tick >= a + w {
                    assert_eq!(state, before, "liquidity moved for a position off the tick");
                }
            }
        }
    }
});
