// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the HIP-4 outcome description
//! grammar (`ingress_hyperliquid::discovery::parse_outcome_spec`,
//! `parse_hl_time_ns`) and the `outcomeMetaUpdates` lifecycle pair
//! (`parse_outcome_meta`, `outcome_meta_description`) — BIN15 O1,
//! §21.4 law (every ingress parser has a property test AND a fuzz
//! target).
//!
//! These four run on the ingress thread against venue bytes, so the
//! bar is the WS parsers' bar: no panic, no out-of-bounds read, no
//! allocation, on any input.
//!
//! Input layout: the whole buffer is fed to each entry point, and the
//! same buffer is also treated as a bare `description` value (the
//! grammar's real input is the VALUE of a JSON string, which the
//! accessor hands over borrowed — so the two must both survive
//! arbitrary bytes).
//!
//! Invariants checked beyond "does not panic":
//!
//! * `parse_outcome_spec` is TOTAL — it always returns a spec, whose
//!   `outcome` is the one passed in and whose `underlying_len` never
//!   exceeds the fixed field.
//! * A resolved grammar always carries an underlying (the key sets all
//!   require one), so a half-parsed row can never read as confident.
//! * `outcome_meta_description` returns a slice that lies INSIDE the
//!   input buffer — the zero-copy contract.
//! * `parse_hl_time_ns` accepts only whole minutes.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    use ingress_hyperliquid::discovery;

    // The grammar, on the raw buffer as a description value.
    let spec = discovery::parse_outcome_spec(7, data);
    assert_eq!(spec.outcome, 7);
    assert!(spec.underlying_len as usize <= discovery::HL_OUTCOME_UNDERLYING_MAX);
    assert_eq!(
        spec.underlying_bytes().len(),
        spec.underlying_len as usize
    );
    if spec.grammar != discovery::HlOutcomeGrammar::Unknown {
        assert!(spec.underlying_len > 0, "a resolved grammar has an underlying");
    }

    // The civil-date scanner.
    if let Some(ns) = discovery::parse_hl_time_ns(data) {
        assert_eq!(ns % (60 * 1_000_000_000), 0, "whole minutes only");
    }

    // The lifecycle pair, on the buffer as a WS frame.
    let _ = ingress_hyperliquid::parse_outcome_meta(data);
    if let Some((id, desc)) = ingress_hyperliquid::outcome_meta_description(data) {
        // Zero copy: the description borrows from the input.
        let base = data.as_ptr() as usize;
        let at = desc.as_ptr() as usize;
        assert!(at >= base && at + desc.len() <= base + data.len());
        // And it parses without panicking under the same grammar.
        let s = discovery::parse_outcome_spec(id, desc);
        assert_eq!(s.outcome, id);
    }
});
