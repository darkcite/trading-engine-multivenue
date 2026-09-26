// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the Hypercall REST options-summary
//! scan (HC3's poller): every row it visits has a name span inside the
//! input, and the scan is total — `None` or a count, never a panic.

#![no_main]

use libfuzzer_sys::fuzz_target;

use ingress_hypercall::rest::{parse_summary_rows, to_opt_summary};

fuzz_target!(|data: &[u8]| {
    let mut n = 0usize;
    let rows = parse_summary_rows(data, |r| {
        assert!(r.name.1 as usize <= data.len() && r.name.0 <= r.name.1);
        let _ = to_opt_summary(0, 0, r);
        n += 1;
    });
    if let Some(k) = rows {
        assert_eq!(k, n);
    }
});
