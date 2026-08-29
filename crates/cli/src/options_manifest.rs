// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `options-manifest.tsv` — per-run sidecar mapping boot-discovered
//! options SymbolIds to venue instrument names (M2 close, operator-ruled
//! 2026-08-22).
//!
//! WHY: option ordinals are allocated per boot in selection order and
//! RESHUFFLE across boots by design (chain roll; mvp-plan §6 /
//! PLAN §14 caveat), and options instruments are boot-discovered — the
//! worker's universe-file seeding lane can never name them
//! (docs/m2-progress.md M2.1 "recorded consequences"). Every offline
//! consumer that keys by venue+descriptor (§9.4 law: the §9.8 IV digest,
//! M4 shadow-P&L, M5 digest) therefore needs a PER-RUN sym→name map.
//! This file is that map: written once at boot, after discovery, into
//! the capture run directory the records it names land in.
//!
//! FORMAT (docs/wire-format.md "Capture files"): UTF-8 text, one line
//! per selected option instrument —
//! `<venue_label>\t<sym_u32_decimal>\t<instrument_name>\n` — where
//! `venue_label` is the venue's capture-file prefix (`deribit`, `okx`,
//! `bn`). No header line. The file exists only when the boot selected
//! ≥ 1 option instrument; absence = an options-less (or pre-M2-close)
//! run. Readers parse strictly and skip-and-count malformed lines
//! (worker labeling.py discipline).
//!
//! DOCTRINE: offline/boot path — this module allocates freely and is
//! never on the hot path.

use core_config::universe::AllocatedUniverse;
use core_types::SymbolId;

/// Manifest file name inside a capture run directory.
pub const OPTIONS_MANIFEST_FILE: &str = "options-manifest.tsv";

/// M4.2 (operator ruling D3): the FULL per-run instrument manifest —
/// EVERY allocated instrument on every venue, `<sym_u32>\t<descriptor>`
/// per line (descriptors are the §9.4 worker map-name convention,
/// baked engine-side: PM token ids bare; `binance:` / `binance-usdm:` /
/// `okx:` / `deribit:` / `hyperliquid:` from the allocation lane;
/// options `deribit:`/`okx:`/`binance-opt:` + instrument name). Written
/// on EVERY boot (a boot always has ≥ 1 instrument — venue-blind boots
/// refuse). [`OPTIONS_MANIFEST_FILE`] stays for one release for
/// pre-D3 readers; new consumers prefer this file.
pub const INSTRUMENT_MANIFEST_FILE: &str = "instrument-manifest.tsv";

/// Render the manifest body from the boot-discovery outcome vectors
/// (each already in allocation order). Empty when no options were
/// selected — callers skip the write then.
pub fn render(
    deribit: &[(String, SymbolId)],
    okx: &[(String, SymbolId)],
    bn: &[(String, SymbolId, u8)],
) -> String {
    let mut out = String::new();
    for (name, sym) in deribit {
        push_row(&mut out, "deribit", *sym, name);
    }
    for (name, sym) in okx {
        push_row(&mut out, "okx", *sym, name);
    }
    for (name, sym, _uly_idx) in bn {
        push_row(&mut out, "bn", *sym, name);
    }
    out
}

/// Render the FULL instrument manifest (D3): the allocated static
/// universe (descriptors pre-baked by `core-config::universe`) plus
/// the boot-discovered options chains (descriptors composed here with
/// the worker namespaces). Emission order = allocation order.
pub fn render_instruments(
    allocated: &AllocatedUniverse,
    deribit_opts: &[(String, SymbolId)],
    okx_opts: &[(String, SymbolId)],
    bn_opts: &[(String, SymbolId, u8)],
) -> String {
    let mut out = String::new();
    for t in &allocated.pm_tokens {
        push_desc_row(&mut out, t.sym, &t.token_id);
    }
    for i in &allocated.bn_spot {
        push_desc_row(&mut out, i.sym, &i.descriptor);
    }
    for i in &allocated.bn_usdm {
        push_desc_row(&mut out, i.sym, &i.descriptor);
    }
    // M5-onboarding fix (2026-08-29): the D3 law is EVERY instrument,
    // every boot — the WS5/WS6/WS9 additions below were silently
    // missing from the manifest until the first offline consumer
    // (carry_signal) needed a Bybit descriptor.
    for i in &allocated.bn_dated {
        push_desc_row(&mut out, i.sym, &i.descriptor);
    }
    for i in &allocated.okx {
        push_desc_row(&mut out, i.sym, &i.descriptor);
    }
    for i in &allocated.deribit {
        push_desc_row(&mut out, i.sym, &i.descriptor);
    }
    for i in &allocated.deribit_combos {
        push_desc_row(&mut out, i.sym, &i.descriptor);
    }
    for i in &allocated.hl {
        push_desc_row(&mut out, i.sym, &i.descriptor);
    }
    for i in &allocated.bybit_spot {
        push_desc_row(&mut out, i.sym, &i.descriptor);
    }
    for i in &allocated.bybit_linear {
        push_desc_row(&mut out, i.sym, &i.descriptor);
    }
    for (name, sym) in deribit_opts {
        let desc = format!("deribit:{name}");
        push_desc_row(&mut out, *sym, &desc);
    }
    for (name, sym) in okx_opts {
        let desc = format!("okx:{name}");
        push_desc_row(&mut out, *sym, &desc);
    }
    for (name, sym, _uly_idx) in bn_opts {
        let desc = format!("binance-opt:{name}");
        push_desc_row(&mut out, *sym, &desc);
    }
    out
}

fn push_desc_row(out: &mut String, sym: SymbolId, descriptor: &str) {
    debug_assert!(!descriptor.is_empty());
    debug_assert!(!descriptor.contains('\t') && !descriptor.contains('\n'));
    out.push_str(&sym.to_string());
    out.push('\t');
    out.push_str(descriptor);
    out.push('\n');
}

fn push_row(out: &mut String, label: &str, sym: SymbolId, name: &str) {
    // Venue instrument names are discovery-parsed symbol strings —
    // structurally free of tabs/newlines. Guard the invariant anyway.
    debug_assert!(!name.contains('\t') && !name.contains('\n'));
    out.push_str(label);
    out.push('\t');
    out.push_str(&sym.to_string());
    out.push('\t');
    out.push_str(name);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_three_venues_in_allocation_order() {
        let deribit = vec![
            ("BTC-27MAR26-100000-C".to_string(), 0x0300_0201u32),
            ("BTC-27MAR26-100000-P".to_string(), 0x0300_0202u32),
        ];
        let okx = vec![("BTC-USD-260327-100000-C".to_string(), 0x0200_0201u32)];
        let bn = vec![("BTC-260327-100000-C".to_string(), 0x0100_0401u32, 0u8)];
        let body = render(&deribit, &okx, &bn);
        let want = format!(
            "deribit\t{}\tBTC-27MAR26-100000-C\n\
             deribit\t{}\tBTC-27MAR26-100000-P\n\
             okx\t{}\tBTC-USD-260327-100000-C\n\
             bn\t{}\tBTC-260327-100000-C\n",
            0x0300_0201u32, 0x0300_0202u32, 0x0200_0201u32, 0x0100_0401u32
        );
        assert_eq!(body, want);
    }

    #[test]
    fn empty_outcome_renders_empty() {
        assert!(render(&[], &[], &[]).is_empty());
    }

    #[test]
    fn render_instruments_covers_every_lane_with_final_descriptors() {
        let mut alloc = AllocatedUniverse::default();
        alloc.pm_tokens.push(core_config::universe::PmToken {
            sym: 42,
            token_id: "2875608808".to_string(),
            market_index: 0,
            is_yes: true,
        });
        alloc.bn_spot.push(core_config::universe::Instrument {
            sym: 0x0100_0007,
            name: "btcusdt".to_string(),
            descriptor: "binance:btcusdt".to_string(),
        });
        alloc.deribit.push(core_config::universe::Instrument {
            sym: 0x0300_0001,
            name: "BTC-PERPETUAL".to_string(),
            descriptor: "deribit:BTC-PERPETUAL".to_string(),
        });
        // M5-onboarding fix: the WS5/WS6/WS9 lanes were silently
        // absent from the manifest — pin every block for good.
        alloc.bn_dated.push(core_config::universe::Instrument {
            sym: 0x0100_0301,
            name: "btcusdt_260925".to_string(),
            descriptor: "binance-usdm:btcusdt_260925".to_string(),
        });
        alloc.deribit_combos.push(core_config::universe::Instrument {
            sym: 0x0300_0101,
            name: "BTC-FS-27MAR26_PERP".to_string(),
            descriptor: "deribit:BTC-FS-27MAR26_PERP".to_string(),
        });
        alloc.bybit_spot.push(core_config::universe::Instrument {
            sym: 0x0600_0001,
            name: "BTCUSDT".to_string(),
            descriptor: "bybit:BTCUSDT".to_string(),
        });
        alloc.bybit_linear.push(core_config::universe::Instrument {
            sym: 0x0600_0201,
            name: "ADAUSDT".to_string(),
            descriptor: "bybit-linear:ADAUSDT".to_string(),
        });
        let deribit_opts = vec![("BTC-27MAR26-100000-C".to_string(), 0x0300_0201u32)];
        let bn_opts = vec![("BTC-260327-100000-C".to_string(), 0x0100_0401u32, 0u8)];
        let body = render_instruments(&alloc, &deribit_opts, &[], &bn_opts);
        let want = format!(
            "42\t2875608808\n\
             {}\tbinance:btcusdt\n\
             {}\tbinance-usdm:btcusdt_260925\n\
             {}\tderibit:BTC-PERPETUAL\n\
             {}\tderibit:BTC-FS-27MAR26_PERP\n\
             {}\tbybit:BTCUSDT\n\
             {}\tbybit-linear:ADAUSDT\n\
             {}\tderibit:BTC-27MAR26-100000-C\n\
             {}\tbinance-opt:BTC-260327-100000-C\n",
            0x0100_0007u32,
            0x0100_0301u32,
            0x0300_0001u32,
            0x0300_0101u32,
            0x0600_0001u32,
            0x0600_0201u32,
            0x0300_0201u32,
            0x0100_0401u32
        );
        assert_eq!(body, want);
    }
}
