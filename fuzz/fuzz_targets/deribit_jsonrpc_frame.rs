// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the full ingress-deribit parse surface.
//!
//! One Deribit JSON-RPC connection multiplexes four public channels;
//! the run loop classifies each text frame, lifts the channel's
//! instrument name, then hands the payload to the per-channel parser.
//! This target drives every one of those byte scanners with the same
//! input:
//!
//! 1. The frame classifier (`classify`).
//! 2. The instrument extractor (`extract_instrument`), with both
//!    prefix shapes: `quote.{instr}` (instrument ends at `"`) and
//!    `book.{instr}.100ms` (instrument ends at `.`).
//! 3. All four channel parsers (`parse_quote`, `parse_ticker`,
//!    `parse_trade`, `parse_book_header`).
//!
//! None of these may panic, allocate, or read out of bounds on any
//! input. Additionally, an accepted `quote` frame can never carry a
//! double-empty book, and an accepted book header always carries a
//! valid action with a real `prev_change_id` on changes.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // --- Classifier -------------------------------------------------
    let kind = ingress_deribit::classify(data);
    std::hint::black_box(kind);

    // --- instrument extraction (both prefix shapes) -----------------
    for channel in [
        ingress_deribit::DeribitChannel::Quote,
        ingress_deribit::DeribitChannel::Book,
    ] {
        if let Some(inst) = ingress_deribit::extract_instrument(data, channel) {
            // Zero-copy contract: the value is a subslice of the payload.
            assert!(inst.len() <= data.len());
        }
    }

    // --- quote ------------------------------------------------------
    if let Some(f) = ingress_deribit::parse_quote(data, 0) {
        // A frame with both sides empty carries no information — the
        // parser must have rejected it before returning `Some`.
        assert!(!(f.bid_px_1e6 == 0 && f.ask_px_1e6 == 0));
    }

    // --- ticker -----------------------------------------------------
    let _ = ingress_deribit::parse_ticker(data, 0);

    // --- trades -----------------------------------------------------
    if let Some(t) = ingress_deribit::parse_trade(data, 0) {
        // Taker direction is 0 (buy) or 1 (sell) — nothing else.
        assert!(t.side <= 1);
    }

    // --- book header ------------------------------------------------
    if let Some(b) = ingress_deribit::parse_book_header(data, 0) {
        assert!(
            b.action == ingress_deribit::BOOK_ACTION_SNAPSHOT
                || b.action == ingress_deribit::BOOK_ACTION_CHANGE
        );
        // Only snapshots may omit `prev_change_id` (absent ⇒ -1); a
        // change without one is rejected, never invented.
        if b.action == ingress_deribit::BOOK_ACTION_CHANGE {
            assert!(b.prev_change_id != -1);
        }
    }
});
