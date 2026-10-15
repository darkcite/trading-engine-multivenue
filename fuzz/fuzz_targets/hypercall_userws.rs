// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the Hypercall private-socket reader (HC9): `events::parse` over
//! any payload, and the JSON walker it stands on. The fill of record
//! comes from here (LAW E-5), so beyond "no panic": a `Fill` has a
//! positive size, a non-negative price and an in-bounds symbol span.

#![no_main]

use libfuzzer_sys::fuzz_target;

use exec_hypercall::events::{parse, Msg};

fuzz_target!(|data: &[u8]| {
    match parse(data) {
        Msg::Fill(f) => {
            assert!(f.qty_1e6 > 0);
            assert!(f.px_1e6 >= 0);
            assert!(f.symbol.start < f.symbol.end && f.symbol.end <= data.len());
        }
        Msg::Error(Some(r)) => assert!(r.end <= data.len()),
        _ => {}
    }
    if let Some(root) = exec_hypercall::json::root(data) {
        assert!(root.span.end <= data.len());
        let _ = exec_hypercall::json::field_unique_in(data, &root, b"status");
    }
});
