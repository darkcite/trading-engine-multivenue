// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the M2.3 OKX `opt-summary` byte
//! scanners — `parse_opt_summary_row` (one data row → the OptSummary
//! field set) and `extract_inst_family` (the family-keyed subscribe
//! ack arg). §21.4: every new byte scanner ships with a fuzz target.
//!
//! Neither may panic or read out of bounds on any input.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ingress_okx::parse_opt_summary_row(data);
    let _ = ingress_okx::extract_inst_family(data);
});
