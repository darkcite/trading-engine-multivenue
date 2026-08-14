//! Fuzz target: arbitrary bytes → `ingress_binance::parse_book_ticker`.
//!
//! The byte scanner is expected to tolerate any input — returning
//! `None` on malformed frames and never panicking or reading past the
//! end of the slice. This target exercises that contract with random
//! and coverage-guided inputs from libFuzzer.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ingress_binance::parse_book_ticker(data, 0);
});
