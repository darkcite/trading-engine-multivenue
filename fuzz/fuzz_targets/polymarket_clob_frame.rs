// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → `ingress_polymarket::parse_book_update`
//! and `ingress_polymarket::parse_price_change_row`.
//!
//! Invariant: the parsers must never panic, never read out of bounds,
//! never allocate-unboundedly, regardless of the input — and a failed
//! parse leaves its frame untouched.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/poison.rs"]
mod poison;

fuzz_target!(|data: &[u8]| {
    // Try both classification and full parse — either must terminate
    // without panic on any input.
    let _ = ingress_polymarket::classify(data);
    let mut f: core_types::Tick = poison::poisoned();
    if !ingress_polymarket::parse_book_update(data, 0, 0, &mut f) {
        assert_eq!(f, poison::poisoned::<core_types::Tick>(), "a failed parse wrote the frame");
    }
    let mut f: core_types::Tick = poison::poisoned();
    if !ingress_polymarket::parse_price_change_row(data, 0, 0, 0, &mut f) {
        assert_eq!(f, poison::poisoned::<core_types::Tick>(), "a failed parse wrote the frame");
    }
});
