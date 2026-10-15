// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the whole Hypercall WS surface (HC3):
//! `classify`, the in-place `IndicativeMarketData` parse (from a
//! POISONED frame — a failed parse must leave it untouched) and its
//! provider walk, the index walk, `Trade`, `MarketUpdate`,
//! `ClockSynced`, `Error` and the slow-consumer CLOSE reason. Every
//! scanner must tolerate any input — `None` on malformed frames, never
//! a panic, a loop or a read past the slice.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/poison.rs"]
mod poison;

use ingress_hypercall as hc;

fuzz_target!(|data: &[u8]| {
    let _ = hc::classify(data);
    let mut q: hc::HcQuote = poison::poisoned();
    match hc::parse_indicative(data, &mut q) {
        Some(meta) => {
            assert!(!hc::span_bytes(data, q.instrument).is_empty());
            assert!(meta.sides <= (hc::SIDE_BID | hc::SIDE_ASK));
            if meta.sides & hc::SIDE_BID == 0 {
                assert_eq!((q.bid_px_1e6, q.bid_qty_1e6), (0, 0), "a missing side is 0 / 0");
            }
            let mut ps = [hc::HcProvider::default(); hc::HC_MAX_PROVIDERS];
            if let Some((read, present)) = hc::walk_providers(data, q.providers, &mut ps) {
                assert!(read as usize <= hc::HC_MAX_PROVIDERS && read <= present);
            }
        }
        None => assert_eq!(q, poison::poisoned::<hc::HcQuote>(), "a failed parse wrote the frame"),
    }
    let mut xs = [hc::HcIndexEntry::default(); hc::HC_MAX_UNDERLYINGS];
    if let Some((read, present, _ts)) = hc::parse_index_update(data, &mut xs) {
        assert!(read as usize <= hc::HC_MAX_UNDERLYINGS && read <= present);
    }
    if let Some(t) = hc::parse_trade(data) {
        let _ = hc::span_bytes(data, t.symbol);
    }
    if let Some((_action, sym, _ts)) = hc::parse_market_update(data) {
        let _ = hc::span_bytes(data, sym);
    }
    let _ = hc::parse_clock_synced(data);
    if let Some(m) = hc::parse_error(data) {
        let _ = hc::span_bytes(data, m);
    }
    let _ = hc::parse_close_reason(data);
});
