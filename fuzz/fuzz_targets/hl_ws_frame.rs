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

#[path = "common/poison.rs"]
mod poison;

fuzz_target!(|data: &[u8]| {
    // --- Classifier -------------------------------------------------
    std::hint::black_box(ingress_hyperliquid::classify(data));

    // --- coin extraction --------------------------------------------
    std::hint::black_box(ingress_hyperliquid::extract_coin(data));

    // --- channel parsers --------------------------------------------
    let mut f: ingress_hyperliquid::HlBboFrame = poison::poisoned();
    if !ingress_hyperliquid::parse_bbo(data, 0, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_hyperliquid::HlBboFrame>(), "a failed parse wrote the frame");
    }
    std::hint::black_box(f);
    let mut f: ingress_hyperliquid::HlL2BookFrame = poison::poisoned();
    if !ingress_hyperliquid::parse_l2book_header(data, 0, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_hyperliquid::HlL2BookFrame>(), "a failed parse wrote the frame");
    }
    std::hint::black_box(f);
    // The one-walk depth parse: its header obeys the same untouched-on-
    // false rule; its level carrier may be partly filled then (by design).
    let mut depth = core_types::DepthTopK::EMPTY;
    let mut f: ingress_hyperliquid::HlL2BookFrame = poison::poisoned();
    if !ingress_hyperliquid::parse_l2book_depth(data, 0, 0, &mut depth, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_hyperliquid::HlL2BookFrame>(), "a failed parse wrote the header");
    }
    std::hint::black_box(f);
    std::hint::black_box(depth.bids[0].px_1e6);
    let mut f: ingress_hyperliquid::HlTradeFrame = poison::poisoned();
    if !ingress_hyperliquid::parse_trade(data, 0, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_hyperliquid::HlTradeFrame>(), "a failed parse wrote the frame");
    }
    std::hint::black_box(f);
    let mut f: ingress_hyperliquid::HlAssetCtxFrame = poison::poisoned();
    if !ingress_hyperliquid::parse_active_asset_ctx(data, 0, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_hyperliquid::HlAssetCtxFrame>(), "a failed parse wrote the frame");
    }
    std::hint::black_box(f);
    std::hint::black_box(ingress_hyperliquid::parse_all_mids(data));
    let mut f: ingress_hyperliquid::HlOutcomeMetaFrame = poison::poisoned();
    if !ingress_hyperliquid::parse_outcome_meta(data, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_hyperliquid::HlOutcomeMetaFrame>(), "a failed parse wrote the frame");
    }
    std::hint::black_box(f);

    // --- subscribe acks ---------------------------------------------
    std::hint::black_box(ingress_hyperliquid::parse_sub_response(data));
});
