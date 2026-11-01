// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! TERM decimal entry: never panics, never rounds, and every accepted value
//! formats back to an input that parses to the same integer.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(v) = term_proto::dec::parse_1e6(data) {
        let mut b = [0u8; 28];
        let n = term_proto::dec::format_1e6(v, 6, &mut b);
        assert_eq!(term_proto::dec::parse_1e6(&b[..n]), Ok(v));
        let frac = data.iter().position(|&c| c == b'.').map_or(0, |p| data.len() - p - 1);
        assert!(frac <= 6);
    }
});
