// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → `ingress_binance::parse_book_ticker`.
//!
//! The byte scanner is expected to tolerate any input — returning
//! `false` on malformed frames and never panicking or reading past the
//! end of the slice. This target exercises that contract with random
//! and coverage-guided inputs from libFuzzer, plus the in-place one
//! (BX0): a frame that fails to parse leaves `out` untouched, and one
//! that parses carries the pinned symbol.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut f = ingress_binance::BookTickerFrame::ZERO;
    if ingress_binance::parse_book_ticker(data, 7, &mut f) {
        assert_eq!(f.sym, 7);
    } else {
        assert_eq!(f, ingress_binance::BookTickerFrame::ZERO, "a failed parse wrote the frame");
    }
});
