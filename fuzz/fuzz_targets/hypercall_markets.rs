// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the Hypercall boot discovery (HC4):
//! the one-pass `/markets` scan for a fixed configured list, then the
//! capped-chain selection on whatever it kept. Total over any input:
//! an `Err`, never a panic; a selection never exceeds its E × K × 2
//! bound per underlying.

#![no_main]

use libfuzzer_sys::fuzz_target;

use ingress_hypercall::discovery::{parse_markets, select_universe, DEFAULT_BLACKOUT_MS};

const UNDERLYINGS: [&[u8]; 3] = [b"BTC", b"BOT", b"SP500"];

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = parse_markets(data, &UNDERLYINGS, 1_790_350_000_000, DEFAULT_BLACKOUT_MS) {
        let u = select_universe(&m, UNDERLYINGS.len(), 3, 8);
        assert!(u.len() <= UNDERLYINGS.len() * 3 * 8 * 2);
        for r in &u {
            assert!(!r.name().is_empty());
        }
    }
});
