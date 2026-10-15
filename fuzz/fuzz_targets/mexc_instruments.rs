// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the MEXC boot discovery surface
//! (MX5): `MexcSpotDiscovery::ingest_body` (`/api/v3/exchangeInfo`),
//! `MexcPerpDiscovery::ingest_body` (`/api/v1/contract/detail`) and
//! `parse_funding_rate` (`/api/v1/contract/funding_rate/{SYM}`).
//!
//! Boot-only REST surface, still untrusted bytes: every parser must
//! reject malformed bodies with a typed error, never panic, and roll a
//! failed ingest back (the counts stay consistent either way).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut s = ingress_mexc::discovery::MexcSpotDiscovery::new();
    match s.ingest_body(data) {
        Ok(n) => assert_eq!(n, s.universe_total()),
        Err(_) => assert_eq!(s.universe_total(), 0),
    }
    assert!(s.universe_trading() <= s.universe_total());

    let mut p = ingress_mexc::discovery::MexcPerpDiscovery::new();
    match p.ingest_body(data) {
        Ok(n) => assert_eq!(n, p.universe_total()),
        Err(_) => assert_eq!(p.universe_total(), 0),
    }
    assert!(p.universe_trading() <= p.universe_total());

    let _ = ingress_mexc::discovery::parse_funding_rate(data);
});
