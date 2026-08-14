//! Fuzz target: arbitrary bytes → `ingress_polymarket::discovery::PmDiscovery`,
//! the boot-time Gamma `/markets?clob_token_ids=` REST parser
//! (Phase 8e, plan §11 — same bar as the WS parsers).
//!
//! Exercises:
//!
//! 1. `ingest_body` on the raw input into a fresh table.
//! 2. On success, `find_by_token()` and `sibling_of()` with a
//!    subslice of the input — both linear-scan lookups must never
//!    panic on arbitrary token bytes — plus the added-count vs.
//!    `universe_total()` consistency check on a table that started
//!    empty.
//! 3. Multi-body accumulation: discovery ingests one REST body per
//!    fetched token id, so if the input is long enough it is split in
//!    half and both halves are ingested sequentially into a fresh
//!    table, re-checking the same invariant.
//!
//! None of this may panic or read out of bounds on any input. (The
//! module itself allocates by design — it runs at boot only.)

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = ingress_polymarket::discovery::PmDiscovery::new();

    if let Ok(n) = d.ingest_body(data) {
        assert_eq!(n, d.universe_total());

        let key_len = data.len().min(80);
        let _ = d.find_by_token(&data[..key_len]);
        let _ = d.sibling_of(&data[..key_len]);
    }

    // Multi-body accumulation: discovery ingests one REST body per
    // fetched token id; split the input and feed both halves into
    // the same fresh table, as the boot sequence would across
    // requests.
    if data.len() >= 2 {
        let mid = data.len() / 2;
        let mut d2 = ingress_polymarket::discovery::PmDiscovery::new();
        let r1 = d2.ingest_body(&data[..mid]);
        let r2 = d2.ingest_body(&data[mid..]);
        if let (Ok(n1), Ok(n2)) = (r1, r2) {
            assert_eq!(n1 + n2, d2.universe_total());
        }

        let key_len = data.len().min(80);
        let _ = d2.find_by_token(&data[..key_len]);
        let _ = d2.sibling_of(&data[..key_len]);
    }
});
