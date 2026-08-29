// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → `ingress_deribit::parse_vol_index`
//! (WS6 — the DVOL push scanner).
//!
//! The byte scanner is expected to tolerate any input — returning
//! `None` on malformed frames and never panicking or reading past the
//! end of the slice. This target exercises that contract with random
//! and coverage-guided inputs from libFuzzer.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ingress_deribit::parse_vol_index(data);
});
