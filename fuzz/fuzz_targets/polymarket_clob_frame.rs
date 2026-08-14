//! Fuzz target: arbitrary bytes → `ingress_polymarket::parse_book_update`.
//!
//! Invariant: the parser must never panic, never read out of bounds,
//! never allocate-unboundedly, regardless of the input.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Try both classification and full parse — either must terminate
    // without panic on any input.
    let _ = ingress_polymarket::classify(data);
    let _ = ingress_polymarket::parse_book_update(data, 0, 0);
});
