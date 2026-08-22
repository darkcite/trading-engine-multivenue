//! Fuzz the M1 Binance exchangeInfo discovery parser
//! (`ingress_binance::discovery::BnDiscovery::ingest_body`) — spot
//! single-symbol and full USDS-M page bodies share one walker. House
//! rule §21.3/§21.4: REST discovery parses venue-controlled bytes at
//! boot; it must never panic, loop, or misindex on arbitrary input.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = ingress_binance::discovery::BnDiscovery::new();
    let _ = d.ingest_body(data);
});
