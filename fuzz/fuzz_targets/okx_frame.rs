// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the full ingress-okx parse surface.
//!
//! One OKX connection multiplexes five public channels; the run loop
//! classifies each text frame, lifts the arg `instId`, then hands the
//! payload to the per-channel parser. This target drives every one of
//! those byte scanners with the same input:
//!
//! 1. The frame classifier (`classify`).
//! 2. The arg `instId` extractor (`extract_inst_id`).
//! 3. All five channel parsers (`parse_bbo`, `parse_trade`,
//!    `parse_mark_price`, `parse_funding_rate`, `parse_book_header`).
//!
//! None of these may panic, allocate, or read out of bounds on any
//! input. Additionally, an accepted `bbo-tbt` frame can never carry
//! a double-empty book — the parser rejects those.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/poison.rs"]
mod poison;

fuzz_target!(|data: &[u8]| {
    // --- Classifier -------------------------------------------------
    let kind = ingress_okx::classify(data);
    std::hint::black_box(kind);

    // --- instId extraction ------------------------------------------
    if let Some(inst) = ingress_okx::extract_inst_id(data) {
        // Zero-copy contract: the value is a subslice of the payload.
        assert!(inst.len() <= data.len());
    }

    // --- bbo-tbt ----------------------------------------------------
    let mut f: ingress_okx::OkxBboFrame = poison::poisoned();
    let ok = ingress_okx::parse_bbo(data, 0, &mut f);
    if !ok {
        assert_eq!(f, poison::poisoned::<ingress_okx::OkxBboFrame>(), "a failed parse wrote the frame");
    }
    if ok {
        // A frame with both sides empty carries no information — the
        // parser must have rejected it before returning `Some`.
        assert!(!(f.bid_px_1e6 == 0 && f.ask_px_1e6 == 0));
    }

    // --- trades -----------------------------------------------------
    let mut t: ingress_okx::OkxTradeFrame = poison::poisoned();
    let ok = ingress_okx::parse_trade(data, 0, &mut t);
    if !ok {
        assert_eq!(t, poison::poisoned::<ingress_okx::OkxTradeFrame>(), "a failed parse wrote the frame");
    }
    if ok {
        // Taker direction is 0 (buy) or 1 (sell) — nothing else.
        assert!(t.side <= 1);
    }

    // --- mark-price / funding-rate ----------------------------------
    let mut f: ingress_okx::OkxMarkPriceFrame = poison::poisoned();
    if !ingress_okx::parse_mark_price(data, 0, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_okx::OkxMarkPriceFrame>(), "a failed parse wrote the frame");
    }
    let mut f: ingress_okx::OkxFundingFrame = poison::poisoned();
    if !ingress_okx::parse_funding_rate(data, 0, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_okx::OkxFundingFrame>(), "a failed parse wrote the frame");
    }

    // --- books header -----------------------------------------------
    let mut b: ingress_okx::OkxBookFrame = poison::poisoned();
    let ok = ingress_okx::parse_book_header(data, 0, &mut b);
    if !ok {
        assert_eq!(b, poison::poisoned::<ingress_okx::OkxBookFrame>(), "a failed parse wrote the frame");
    }
    if ok {
        assert!(
            b.action == ingress_okx::BOOK_ACTION_SNAPSHOT
                || b.action == ingress_okx::BOOK_ACTION_UPDATE
        );
    }
});
