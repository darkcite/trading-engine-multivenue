//! Fuzz target: arbitrary bytes → `ingress_deribit::parse_option_ticker`,
//! the M2.3 OPTION `ticker.{instr}.100ms` payload parser feeding the
//! `OptSummary` capture channel (docs/m2-progress.md; §21.4: every new
//! byte scanner ships with a fuzz target).
//!
//! The parser must never panic or read out of bounds on any input;
//! every outcome is `Some(frame)` or `None`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ingress_deribit::parse_option_ticker(data);
});
