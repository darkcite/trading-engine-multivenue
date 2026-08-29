// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → `ingress_deribit::walk_book_levels`
//! (WS10-B — the `book.100ms` level walker feeding the depth ladder;
//! action-token rows with unquoted, possibly sci-notation numbers).
//!
//! The walker is expected to tolerate any input — returning `None` on
//! malformed frames, never panicking, never reading past the end of
//! the slice, and never growing the ladder past its cap. This target
//! exercises that contract with random and coverage-guided inputs.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut ladder = book_builder::ladder::DepthLadder::new();
    let _ = ingress_deribit::walk_book_levels(data, &mut ladder);
    assert!(ladder.bids.len() <= book_builder::ladder::DEPTH_LADDER_CAP);
    assert!(ladder.asks.len() <= book_builder::ladder::DEPTH_LADDER_CAP);
});
