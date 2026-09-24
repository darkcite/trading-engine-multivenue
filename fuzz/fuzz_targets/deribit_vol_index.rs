// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → `ingress_deribit::parse_vol_index`
//! (WS6 — the DVOL push scanner).
//!
//! The byte scanner is expected to tolerate any input — returning
//! `false` on malformed frames and never panicking or reading past the
//! end of the slice. This target exercises that contract with random
//! and coverage-guided inputs from libFuzzer.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/poison.rs"]
mod poison;

fuzz_target!(|data: &[u8]| {
    let mut f: ingress_deribit::DeribitVolIndexFrame = poison::poisoned();
    if ingress_deribit::parse_vol_index(data, &mut f) {
        // The name is a span of THIS payload, 1..=16 bytes.
        let name = f.index_name(data).expect("a parsed frame's name span lies inside its payload");
        assert!((1..=16).contains(&name.len()), "name length {}", name.len());
    } else {
        assert_eq!(f, poison::poisoned::<ingress_deribit::DeribitVolIndexFrame>(), "a failed parse wrote the frame");
    }
});
