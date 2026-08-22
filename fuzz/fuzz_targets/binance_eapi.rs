//! Fuzz target: arbitrary bytes → the M2.4 Binance eapi byte
//! scanners — exchangeInfo option rows, the index-price REST body,
//! the combined-stream splitter, and the ticker/index WS data
//! parsers (§21.4: every new byte scanner ships with a fuzz target).
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
    if let Some((_stream, tail)) = ingress_binance::eapi::split_combined(data) {
        let _ = ingress_binance::eapi::parse_eapi_ticker(tail);
        let _ = ingress_binance::eapi::parse_eapi_index(tail);
    }
    let _ = ingress_binance::eapi::parse_eapi_ticker(data);
    let _ = ingress_binance::eapi::parse_eapi_index(data);
});
