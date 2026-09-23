// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the M2.4 Binance eapi byte
//! scanners — exchangeInfo option rows, the index-price REST body,
//! the combined-stream splitter and the mark-element scanners on a
//! bare input (§21.4: every new byte scanner ships with a fuzz
//! target). The array walk has its own target,
//! `binance_eapi_mark_array` (BX0-F2).
//!
//! None may panic or read out of bounds on any input. On a
//! successful exchangeInfo parse the capped selection runs too and
//! must uphold its ≤ E×K×2 law.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = ingress_binance::eapi::EapiDiscovery::new();
    if d.ingest_exchange_info(data).is_ok() {
        let sel = ingress_binance::eapi::select_capped_chain(
            d.rows(),
            b"BTCUSDT",
            1_000_000_000,
            2,
            8,
            0,
        );
        assert!(sel.len() as u32 <= 2 * 8 * 2);
    }
    let _ = ingress_binance::eapi::parse_index_price(data);
    let _ = ingress_binance::eapi::split_combined(data);
    let _ = ingress_binance::eapi::eapi_elem_symbol(data);
    let mut f = ingress_binance::eapi::EapiMarkFrame::ZERO;
    if ingress_binance::eapi::parse_eapi_mark(data, &mut f) {
        assert!(f.index_px_1e9 > 0, "a parsed element always carries a positive index");
    }
});
