// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → `ingress_hyperliquid::discovery::HlDiscovery`,
//! the boot-time `POST /info` REST parsers (`meta`, `spotMeta`,
//! `perpDexs`, `outcomeMeta`) plus the `resolve()` coin-string
//! resolver (Phase 8e, plan §11 — same bar as the WS parsers).
//!
//! Input layout:
//!
//! * `data[0] % 4` selects which of the four ingest fns receives
//!   `data[1..]` — the four wire shapes (an object keyed on
//!   `"universe":[`, a bare top-level array, an object keyed on
//!   `"outcomes":[`) share little structure, so driving all four with
//!   the same byte stream would mostly starve three of them.
//! * `resolve()` is then called on a subslice of the remainder —
//!   this exercises the `BTC` / `@N` / `dex:COIN` / `#enc` coin-string
//!   forms against whatever the ingest call populated.
//! * HC10: a builder dex's own meta (`ingest_dex_meta`) over a fixed
//!   `perpDexs`: every id it derives stays on the builder scheme.
//! * `counts()` / `universe_total()` must stay consistent:
//!   `perps + spots + outcomes*2 == universe_total()` always holds by
//!   construction (builder dexs are excluded from the total — see the
//!   method docs) — checked whether or not ingestion succeeded.
//! * Multi-body accumulation: discovery ingests one body per boot
//!   fetch (`meta` once, `spotMeta` once, `perpDexs` once,
//!   `outcomeMeta` once), so if the remainder is long enough it is
//!   split in half and both halves are fed to the same selected
//!   ingest fn on a fresh table, re-checking the same invariants.
//!
//! None of this may panic or read out of bounds on any input. (The
//! module itself allocates by design — it runs at boot only.)

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Feed `body` to the ingest fn selected by `selector % 4`.
fn ingest_selected(
    d: &mut ingress_hyperliquid::discovery::HlDiscovery,
    selector: u8,
    body: &[u8],
) -> Result<u32, ingress_hyperliquid::discovery::HlDiscoveryErr> {
    match selector % 4 {
        0 => d.ingest_meta(body),
        1 => d.ingest_spot_meta(body),
        2 => d.ingest_perp_dexs(body),
        _ => d.ingest_outcome_meta(body),
    }
}

/// `perps + spots + outcomes*2 == universe_total()` always holds —
/// builder dexs are excluded from the total (method docs).
fn assert_consistent(d: &ingress_hyperliquid::discovery::HlDiscovery) {
    let (perps, spots, _dexs, outcomes) = d.counts();
    assert_eq!(d.universe_total(), perps + spots + outcomes * 2);
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let selector = data[0];
    let rest = &data[1..];
    let key_len = rest.len().min(64);

    let mut d = ingress_hyperliquid::discovery::HlDiscovery::new();
    let _ = ingest_selected(&mut d, selector, rest);
    let _ = d.resolve(&rest[..key_len]);
    assert_consistent(&d);

    // HC10: a dex's own meta over a known `perpDexs` — any builder id it
    // derives is on the scheme (a builder perp, never a HIP-4 or spot
    // id), and a dex the venue did not list is refused.
    let mut d3 = ingress_hyperliquid::discovery::HlDiscovery::new();
    if d3.ingest_perp_dexs(br#"[null,{"name":"xyz"}]"#).is_ok() {
        let _ = d3.ingest_dex_meta(b"xyz", rest);
        if let Some(info) = d3.resolve(&rest[..key_len]) {
            if info.kind == ingress_hyperliquid::discovery::HlAssetKind::BuilderDex && info.asset_id != 0 {
                assert!((110_000..120_000).contains(&info.asset_id), "{}", info.asset_id);
            }
        }
        assert!(d3.ingest_dex_meta(b"nope", rest).is_err());
    }

    // Multi-body accumulation: split the remainder and feed both
    // halves to the same selected ingest fn on a fresh table.
    if rest.len() >= 2 {
        let mid = rest.len() / 2;
        let mut d2 = ingress_hyperliquid::discovery::HlDiscovery::new();
        let _ = ingest_selected(&mut d2, selector, &rest[..mid]);
        let _ = ingest_selected(&mut d2, selector, &rest[mid..]);
        let _ = d2.resolve(&rest[..key_len]);
        assert_consistent(&d2);
    }
});
