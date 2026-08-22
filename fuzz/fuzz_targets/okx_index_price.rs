//! Fuzz target: arbitrary bytes → `ingress_okx::discovery::parse_index_price`,
//! the M2.2 boot-time `/api/v5/market/index-tickers` REST parser (the
//! capped-chain ATM reference; docs/m2-progress.md design entry —
//! §21.4: every new byte scanner ships with a fuzz target).
//!
//! The parser must never panic or read out of bounds on any input;
//! every outcome is `Ok(px_1e9 > 0)` or a structured
//! `OkxDiscoveryErr`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(px) = ingress_okx::discovery::parse_index_price(data) {
        assert!(px > 0, "parse_index_price must reject nonpositive prices");
    }
});
