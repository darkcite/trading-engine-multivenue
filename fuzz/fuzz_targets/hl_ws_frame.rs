// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the full ingress-hyperliquid parse
//! surface.
//!
//! One Hyperliquid connection multiplexes six public channels; the
//! run loop classifies each text frame, lifts the `coin`, then hands
//! the payload to the per-channel parser. This target drives every
//! one of those byte scanners with the same input:
//!
//! 1. The frame classifier (`classify`).
//! 2. The `coin` extractor (`extract_coin`).
//! 3. All six channel parsers (`parse_bbo`, `parse_l2book_header`,
//!    `parse_trade`, `parse_active_asset_ctx`, `parse_all_mids`,
//!    `parse_outcome_meta`).
//! 4. The subscribe-ack parser (`parse_sub_response`).
//!
//! None of these may panic, allocate, or read out of bounds on any
//! input — results are consumed and otherwise ignored.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // --- Classifier -------------------------------------------------
    std::hint::black_box(ingress_hyperliquid::classify(data));

    // --- coin extraction --------------------------------------------
    std::hint::black_box(ingress_hyperliquid::extract_coin(data));

    // --- channel parsers --------------------------------------------
    std::hint::black_box(ingress_hyperliquid::parse_bbo(data, 0));
    std::hint::black_box(ingress_hyperliquid::parse_l2book_header(data, 0));
    std::hint::black_box(ingress_hyperliquid::parse_trade(data, 0));
    std::hint::black_box(ingress_hyperliquid::parse_active_asset_ctx(data, 0));
    std::hint::black_box(ingress_hyperliquid::parse_all_mids(data));
    std::hint::black_box(ingress_hyperliquid::parse_outcome_meta(data));

    // --- subscribe acks ---------------------------------------------
    std::hint::black_box(ingress_hyperliquid::parse_sub_response(data));
});
