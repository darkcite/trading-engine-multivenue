// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The pool-event payload codec (`core_amm::payload`): every event
//! round-trips bit-exactly, and the decoder is CANONICAL — any 40 bytes
//! it accepts re-encode to the same 40 bytes, so there is exactly one
//! wire form per event and a corrupted payload cannot alias a valid one
//! with different bytes.

use core_amm::payload::{
    decode, encode_fee, encode_gap, encode_head, encode_liquidity, encode_snapshot, encode_state,
    encode_swap, encode_tick, Payload, PoolEvent, FAMILY_ALGEBRA, FEE_SRC_ALGEBRA_V10,
    FEE_SRC_ALGEBRA_V12, MAX_PAYLOAD_BLOCK, PAYLOAD_LEN,
};
use core_amm::MAX_TICK;
use proptest::prelude::*;

fn encode(ev: &PoolEvent) -> Option<Payload> {
    match *ev {
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
    }
}

fn tick() -> impl Strategy<Value = i32> {
    -MAX_TICK..=MAX_TICK
}

fn block() -> impl Strategy<Value = u64> {
    0..=MAX_PAYLOAD_BLOCK
}

fn event() -> impl Strategy<Value = PoolEvent> {
    prop_oneof![
        (
            tick(),
            any::<u128>(),
            any::<u32>(),
            any::<u128>(),
            any::<bool>()
        )
            .prop_map(
                |(tick, sqrt_price_lo, sqrt_price_hi, liquidity, snapshot)| {
                    PoolEvent::State {
                        tick,
                        sqrt_price_lo,
                        sqrt_price_hi,
                        liquidity,
                        snapshot,
                    }
                }
            ),
        (block(), any::<i128>(), any::<i128>()).prop_map(|(block, amount0, amount1)| {
            PoolEvent::Swap {
                block,
                amount0,
                amount1,
            }
        }),
        (
            block(),
            prop_oneof![Just(FEE_SRC_ALGEBRA_V10), Just(FEE_SRC_ALGEBRA_V12)],
            any::<u32>(),
            any::<u32>()
        )
            .prop_map(|(block, source, a, b)| PoolEvent::Fee {
                block,
                source,
                a,
                b
            }),
        (block(), any::<bool>(), tick(), tick(), any::<u128>()).prop_filter_map(
            "lower < upper",
            |(block, burn, x, y, amount)| {
                (x != y).then(|| PoolEvent::Liquidity {
                    block,
                    burn,
                    tick_lower: x.min(y),
                    tick_upper: x.max(y),
                    amount,
                })
            }
        ),
        (block(), any::<u64>(), any::<u128>()).prop_map(|(block, timestamp, base_fee)| {
            PoolEvent::Head {
                block,
                timestamp,
                base_fee,
            }
        }),
        block().prop_map(|block| PoolEvent::Gap { block }),
        (
            block(),
            0u8..=FAMILY_ALGEBRA,
            tick(),
            tick(),
            any::<u16>(),
            any::<u32>(),
            1i32..=MAX_TICK
        )
            .prop_map(
                |(block, family, x, y, nodes, fee, spacing)| PoolEvent::Snapshot {
                    block,
                    family,
                    lo: x.min(y),
                    hi: x.max(y),
                    nodes,
                    fee,
                    spacing
                }
            ),
        (tick(), any::<i128>(), any::<u128>()).prop_map(|(tick, net, gross)| PoolEvent::Tick {
            tick,
            net,
            gross
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    #[test]
    fn every_event_round_trips(ev in event()) {
        let p = encode(&ev).expect("in-domain event encodes");
        prop_assert_eq!(decode(&p), Some(ev));
    }

    #[test]
    fn the_decoder_is_canonical(bytes in prop::array::uniform32(any::<u8>()), tail in prop::array::uniform8(any::<u8>()), kind in 0u8..10, sub in 0u8..4) {
        let mut p = [0u8; PAYLOAD_LEN];
        p[..32].copy_from_slice(&bytes);
        p[32..].copy_from_slice(&tail);
        p[0] = kind | (sub << 4);
        if let Some(ev) = decode(&p) {
            prop_assert_eq!(encode(&ev), Some(p));
        }
    }

    #[test]
    fn sparse_payloads_are_canonical_too(kind in 1u8..9, sub in 0u8..4, block in block(), mask in any::<u64>()) {
        // Mostly-zero payloads reach the accepting paths far more often.
        let mut p = [0u8; PAYLOAD_LEN];
        p[0] = kind | (sub << 4);
        p[1..8].copy_from_slice(&block.to_le_bytes()[..7]);
        let mut i = 8;
        while i < PAYLOAD_LEN {
            if mask >> (i % 64) & 1 == 1 {
                p[i] = (block >> (i % 7 * 8)) as u8 | 1;
            }
            i += 1;
        }
        if let Some(ev) = decode(&p) {
            prop_assert_eq!(encode(&ev), Some(p));
        }
    }
}
