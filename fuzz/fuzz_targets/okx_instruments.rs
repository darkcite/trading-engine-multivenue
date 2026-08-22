//! Fuzz target: arbitrary bytes → `ingress_okx::discovery::OkxDiscovery`,
//! the boot-time `/api/v5/public/instruments` REST parser
//! (Phase 8e, plan §11 — same bar as the WS parsers).
//!
//! Exercises:
//!
//! 1. `ingest_body` on the raw input into a fresh table.
//! 2. On success, `find()` with a subslice of the input (the lookup
//!    scan must never panic on an arbitrary key) plus the count
//!    accessors — `universe_live() <= universe_total()` must always
//!    hold, and the returned added-count must equal `universe_total()`
//!    on a table that started empty.
//! 3. Multi-body accumulation: discovery ingests one REST page per
//!    fetched `instType`, so if the input is long enough it is split
//!    in half and both halves are ingested sequentially into a fresh
//!    table, re-checking the same invariants.
//!
//! None of this may panic or read out of bounds on any input. (The
//! module itself allocates by design — it runs at boot only.)

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = ingress_okx::discovery::OkxDiscovery::new();

    if let Ok(n) = d.ingest_body(data) {
        assert_eq!(n, d.universe_total());
        assert!(d.universe_live() <= d.universe_total());

        let key_len = data.len().min(64);
        let _ = d.find(&data[..key_len]);
    }

    // Multi-body accumulation: discovery ingests one REST page per
    // `instType` fetch (SPOT / SWAP / FUTURES); split the input and
    // feed both halves into the same fresh table, as the boot
    // sequence would across pages.
    if data.len() >= 2 {
        let mid = data.len() / 2;
        let mut d2 = ingress_okx::discovery::OkxDiscovery::new();
        let r1 = d2.ingest_body(&data[..mid]);
        let r2 = d2.ingest_body(&data[mid..]);
        assert!(d2.universe_live() <= d2.universe_total());
        if let (Ok(n1), Ok(n2)) = (r1, r2) {
            assert_eq!(n1 + n2, d2.universe_total());
        }

        let key_len = data.len().min(64);
        let _ = d2.find(&data[..key_len]);
    }

    // M2.2: the OPTION-page walker shares the row machinery but
    // carries its own contract (instType=OPTION + stk/expTime/optType
    // required); fuzz it on the same input, then drive the capped
    // selection over whatever parsed — ≤ E×K×2 and never panic.
    let mut od = ingress_okx::discovery::OkxDiscovery::new();
    if let Ok(n) = od.ingest_options_body(data) {
        assert_eq!(n, od.universe_total());
        assert!(od.universe_live() <= od.universe_total());
        let sel = ingress_okx::discovery::select_capped_chain(od.rows(), 1_000_000_000, 2, 8, 0);
        assert!(sel.len() as u32 <= 2 * 8 * 2);
    }

    // M2.2: the index-price parser also rides this corpus (its
    // dedicated target is okx_index_price; double coverage is free).
    let _ = ingress_okx::discovery::parse_index_price(data);
});
