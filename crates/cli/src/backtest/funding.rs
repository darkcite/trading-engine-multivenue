// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The harness FUNDING seed (RG4's carry blocker, landed 2026-09-05 —
//! `docs/regime-and-dashboard-plan.md` §12): the per-window mirror of
//! the live seed lane's `FundingSeed` frames (VM2 V6, D-1).
//!
//! Under the ≤ 2 h capture-window law a funding-carry row (`apr24` /
//! `apr72`) could never be evidenced: the harness warm-up is
//! table-global and needs 24 h / 72 h of prints inside the window, so
//! a 2 h window emitted 0 orders. Live, the same row is warm from its
//! first minute because `python -m claude_worker.seeds` pushes the
//! `funding` table's last 73 h of prints as kind-10 frames at boot.
//! This module gives the replay the SAME warm-up: a `funding-seed.tsv`
//! (`descriptor \t ts_ms \t rate_1e9`, `#` comments — the shape of
//! `regime-seed.tsv`, written per window by `claude_worker.window_root`)
//! becomes synthesized `AiCmdKind::FundingSeed` commands applied through
//! `VmStrategy::on_ai` before the first replayed record — the live code
//! path, dedup law included (`FeatureState::funding_seed`). The warm-up
//! then drops the funding features' 24 h / 72 h requirement (the seed
//! IS their history; a seed shorter than a feature's window leaves that
//! feature ABSENT by the feature law — honest by construction).
//!
//! Offline path: allocation is fine here (doctrine header of the parent
//! module).

use std::path::Path;

use core_types::{AiCmd, AiCmdKind, SymbolId, VenueId, AI_SIDE_NONE, STRATEGY_SLOT_VM};

use crate::backtest::HarnessError;

/// One seed print, descriptor unresolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingSeedLine {
    /// §9.4 descriptor (`binance-usdm:btcusdt`, `deribit:BTC-PERPETUAL`, …).
    pub descriptor: String,
    /// Venue print time, wall ms (> 0).
    pub ts_ms: i64,
    /// Rate ×1e9 RAW as the venue printed it (the engine owns any ÷8
    /// law — the live lane's convention).
    pub rate_1e9: i64,
}

/// Parse `descriptor \t ts_ms \t rate_1e9` per line (`#` comments and
/// blank lines ignored). A malformed line is FATAL — the file is
/// generated, so a bad line means a bad generator.
pub fn parse_funding_seed(src: &str) -> Result<Vec<FundingSeedLine>, HarnessError> {
    let mut out = Vec::new();
    for (idx, raw) in src.lines().enumerate() {
        let ln = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split('\t');
        let descriptor = it.next().unwrap_or("").trim().to_owned();
        let ts = it
            .next()
            .ok_or_else(|| HarnessError::Usage(format!("funding seed line {ln}: missing ts_ms")))?;
        let rate = it.next().ok_or_else(|| {
            HarnessError::Usage(format!("funding seed line {ln}: missing rate_1e9"))
        })?;
        if it.next().is_some() {
            return Err(HarnessError::Usage(format!(
                "funding seed line {ln}: too many columns"
            )));
        }
        let ts_ms: i64 = ts.trim().parse().map_err(|_| {
            HarnessError::Usage(format!("funding seed line {ln}: bad ts_ms `{ts}`"))
        })?;
        let rate_1e9: i64 = rate.trim().parse().map_err(|_| {
            HarnessError::Usage(format!("funding seed line {ln}: bad rate_1e9 `{rate}`"))
        })?;
        if descriptor.is_empty() || ts_ms <= 0 {
            return Err(HarnessError::Usage(format!(
                "funding seed line {ln}: bad row `{line}`"
            )));
        }
        out.push(FundingSeedLine {
            descriptor,
            ts_ms,
            rate_1e9,
        });
    }
    Ok(out)
}

/// Read + parse the seed file.
pub fn load_funding_seed(path: &Path) -> Result<Vec<FundingSeedLine>, HarnessError> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| HarnessError::Usage(format!("{}: {e}", path.display())))?;
    parse_funding_seed(&src)
}

/// The synthesized kind-10 commands for every line whose descriptor
/// resolves on this root (`(cmds, dropped)`), stamped `ts_ns` — the
/// live frame shape: `px` = rate ×1e9, `qty` = venue print ms,
/// `strategy_id` = the vm slot, no side, no TTL.
pub fn seed_cmds(
    lines: &[FundingSeedLine],
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    ts_ns: u64,
) -> (Vec<AiCmd>, usize) {
    let mut out = Vec::with_capacity(lines.len());
    let mut dropped = 0usize;
    for (i, l) in lines.iter().enumerate() {
        let Some(sym) = resolve(&l.descriptor) else {
            dropped += 1;
            continue;
        };
        let cmd = AiCmd::new(
            ts_ns,
            i as u32,
            sym,
            l.rate_1e9,
            l.ts_ms,
            0,
            AiCmdKind::FundingSeed,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            AI_SIDE_NONE,
            0,
            0,
        );
        debug_assert!(cmd.validate_shape().is_ok(), "synthesized funding seed shape");
        out.push(cmd);
    }
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_blanks_and_rows_and_refuses_malformed() {
        let src = "# funding-seed.tsv\n\nbinance-usdm:btcusdt\t1700000000000\t100000\n\
                   deribit:BTC-PERPETUAL\t1700000001000\t-2500\n";
        let rows = parse_funding_seed(src).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].descriptor, "binance-usdm:btcusdt");
        assert_eq!(rows[0].ts_ms, 1_700_000_000_000);
        assert_eq!(rows[1].rate_1e9, -2_500);
        for bad in [
            "x\t1\n",
            "x\t1\t2\t3\n",
            "x\tabc\t2\n",
            "x\t1\tabc\n",
            "\t1\t2\n",
            "x\t0\t2\n",
        ] {
            assert!(parse_funding_seed(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn seed_cmds_resolve_drop_and_carry_the_live_frame_shape() {
        let lines = vec![
            FundingSeedLine {
                descriptor: "a".into(),
                ts_ms: 5,
                rate_1e9: 7,
            },
            FundingSeedLine {
                descriptor: "nope".into(),
                ts_ms: 6,
                rate_1e9: 8,
            },
        ];
        let resolve = |d: &str| if d == "a" { Some(42u32) } else { None };
        let (cmds, dropped) = seed_cmds(&lines, &resolve, 99);
        assert_eq!(dropped, 1);
        assert_eq!(cmds.len(), 1);
        let c = cmds[0];
        assert_eq!(c.kind(), Some(AiCmdKind::FundingSeed));
        assert_eq!((c.sym, c.px, c.qty, c.ts_ns), (42, 7, 5, 99));
        assert_eq!(c.strategy_id, STRATEGY_SLOT_VM);
        assert!(c.validate_shape().is_ok());
    }
}
