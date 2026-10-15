// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the AMM fill law (HYPARB H2, `core_fill::AmmBook`).
//!
//! The input is a script: each 48-byte step is a symbol selector, an
//! opcode, a judge's side / price / size, and a 40-byte pool-event
//! payload. Most payloads are the raw bytes (the decoder must refuse
//! garbage); the rest are re-encoded from fuzzed fields so the state
//! machine actually reaches live pools, swaps, fee events and positions.
//!
//! What must hold for every input:
//!
//! * no panic — the book and the judge are on the engine thread;
//! * a verdict is `Fill` or `Cancel`, never `Wait` (a swap never rests);
//! * a fill is strictly positive, never larger than the order, and its
//!   price is within the order's limit (a sell at or above it, a buy at
//!   or below it) — the conservative-by-construction law;
//! * a pool that is not live is never filled.

#![no_main]

use libfuzzer_sys::fuzz_target;

use core_amm::payload::{
    encode_fee, encode_gap, encode_head, encode_liquidity, encode_snapshot, encode_state,
    encode_swap, PAYLOAD_LEN,
};
use core_amm::{sqrt_at_tick, MAX_TICK, MIN_TICK};
use core_fill::{AmmBook, Verdict};
use core_types::{make_symbol_id, Side, VenueId, SYMBOL_ID_NONE};

const STEP: usize = 48;

fn i32_of(b: &[u8]) -> i32 {
    i32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn i64_of(b: &[u8]) -> i64 {
    i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn u128_of(b: &[u8]) -> u128 {
    let mut x = [0u8; 16];
    x.copy_from_slice(&b[..16]);
    u128::from_le_bytes(x)
}

fuzz_target!(|data: &[u8]| {
    let mut book = AmmBook::new();
    let mut i = 0usize;
    while i + STEP <= data.len() {
        let st = &data[i..i + STEP];
        i += STEP;
        // Three pools and the chain-wide id, plus a foreign venue.
        let sym = match st[0] % 5 {
            0 => SYMBOL_ID_NONE,
            1 => make_symbol_id(VenueId::HyperEvm, 1),
            2 => make_symbol_id(VenueId::HyperEvm, 2),
            3 => make_symbol_id(VenueId::HyperEvm, 128),
            _ => make_symbol_id(VenueId::Binance, 1),
        };
        let raw: [u8; PAYLOAD_LEN] = st[8..48].try_into().unwrap();
        let tick = (i32_of(&st[8..12]) % (MAX_TICK / 4)).clamp(MIN_TICK, MAX_TICK);
        let payload = match st[1] % 9 {
            0 => Some(raw),
            1 => encode_snapshot(
                1,
                st[2] % 3,
                MIN_TICK,
                MAX_TICK,
                0,
                u32::from_le_bytes([st[12], st[13], st[14], 0]) % 1_000_000,
                1 + (st[15] as i32) * 10,
                st[16] % 37,
                st[17] % 37,
            ),
            2 | 3 => {
                let (lo, hi) = sqrt_at_tick(tick);
                encode_state(
                    tick,
                    lo,
                    hi,
                    u128_of(&st[16..32]) >> (st[2] % 128),
                    st[1] % 9 == 2,
                )
            }
            4 => encode_swap(
                1,
                i64_of(&st[16..24]) as i128 * 1_000_000,
                i64_of(&st[24..32]) as i128,
            ),
            5 => encode_fee(
                1,
                1 + st[2] % 2,
                u32::from(st[16]) * 1_000,
                u32::from(st[17]) * 100,
            ),
            6 => {
                let lo = tick.min(MAX_TICK - 1);
                let hi = (lo as i64 + 1 + (st[18] as i64) * 60).min(MAX_TICK as i64) as i32;
                encode_liquidity(1, st[2] & 1 == 1, lo, hi, u128_of(&st[16..32]) >> 64)
            }
            7 => encode_head(u64::from(st[2]) + 1, 1, 1),
            _ => encode_gap(1),
        };
        if let Some(p) = payload {
            let _ = book.observe(sym, &p);
        }
        // Judge on every step.
        let idx = (st[3] % 3) as usize;
        let idx = if idx == 2 { 127 } else { idx };
        let side = if st[4] & 1 == 0 { Side::Bid } else { Side::Ask };
        let px = i64_of(&st[24..32]) >> (st[5] % 64);
        let qty = i64_of(&st[32..40]) >> (st[6] % 64);
        let live = book.is_live(idx);
        let v = book.judge(idx, side, px, qty);
        match v.verdict {
            Verdict::Fill { px_1e6, qty_1e6 } => {
                assert!(live, "a pool that is not live filled");
                assert!(qty_1e6 > 0 && qty_1e6 <= qty, "fill {qty_1e6} of {qty}");
                assert!(px_1e6 > 0);
                match side {
                    Side::Ask => assert!(px_1e6 >= px, "sell at {px_1e6} under limit {px}"),
                    Side::Bid => assert!(px_1e6 <= px, "buy at {px_1e6} over limit {px}"),
                }
            }
            Verdict::Cancel => {}
            Verdict::Wait => panic!("an AMM swap never rests"),
        }
    }
});
