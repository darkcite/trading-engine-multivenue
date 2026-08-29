// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the Bybit boot discovery parser
//! (WS9): `BybitDiscovery::ingest_body` + `next_page_cursor`.
//!
//! Boot-only REST surface, still untrusted bytes: the parser must
//! reject malformed pages with an error and never panic.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = ingress_bybit::discovery::BybitDiscovery::new();
    let _ = d.ingest_body(data);
    let _ = ingress_bybit::discovery::next_page_cursor(data);
});
