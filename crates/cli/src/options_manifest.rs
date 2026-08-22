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

use core_types::SymbolId;

/// Manifest file name inside a capture run directory.
pub const OPTIONS_MANIFEST_FILE: &str = "options-manifest.tsv";

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
}
