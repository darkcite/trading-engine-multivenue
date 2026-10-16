// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.9 — the twelve scripts of §4.0 (ours) and their priors, and each screened one run on a
//! synthetic chart. The files sit in `crates/pine-lang/tests/scripts/` (impl-07 §2.4), beside
//! impl-01's fixtures, so pine-lang's round-trip and dialect tests cover them too.

mod common;

use common::*;
use pine_ir::ExecMode;
use pine_screen::{run_trial, CompileRefusal, TrialStatus, Workspace};
use std::path::{Path, PathBuf};

/// The eleven screened scripts (§4.0.1), table order, with the mode tier 1 compiles.
const SCREENED: [(&str, ExecMode); 11] = [
    ("pvm7_supertrend", ExecMode::BarClose),
    ("pvm7_macd_cross", ExecMode::BarClose),
    ("pvm7_bb_breakout", ExecMode::BarClose),
    ("pvm7_rsi_mr", ExecMode::BarClose),
    ("pvm7_donchian", ExecMode::BarClose),
    ("pvm7_ma_ribbon", ExecMode::BarClose),
    ("pvm7_keltner_squeeze", ExecMode::BarClose),
    ("pvm7_atr_trail", ExecMode::BarClose),
    ("pvm7_atr_trail_tick", ExecMode::Realtime),
    ("pvm7_orb", ExecMode::BarClose),
    ("pvm7_grid", ExecMode::Realtime),
];

/// Screened scripts that fault today, each an open request to another lane (named in the T2
/// report). The assertion is exact, so a fix turns this test red until the entry is removed.
// pvm7_grid: its five `strategy.exit("Ti", "Bi", limit = …)` re-issued each evaluation fill
// pine-broker's exit tables → `BrokerFault::Script(OrderTableFull)` at bar 201 (bar 301 without the
// `cancel_all`; without the exits it completes) — exit-rule / leg-set slots are not reclaimed.
// Request to the pine-broker lane.
const KNOWN_FAULTS: [&str; 1] = ["pvm7_grid"];

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../pine-lang/tests/scripts")
}

fn read(name: &str) -> String {
    let p = dir().join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `script → (hold_bars, trades_per_1k_bars)` rows of the priors file, in file order.
fn priors() -> Vec<(String, u32, u32)> {
    let mut rows = Vec::new();
    let text = read("pvm7_priors.tsv");
    let mut prev_comment = false;
    for line in text.lines() {
        if line.starts_with('#') {
            prev_comment = true;
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        assert!(
            prev_comment,
            "a priors row without a comment line above it: {line}"
        );
        prev_comment = false;
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 4, "four tab-separated columns: {line}");
        let hold = f[1].parse().expect("hold_bars");
        let rate = f[2].parse().expect("trades_per_1k_bars");
        assert!(!f[3].is_empty(), "a builtins column: {line}");
        rows.push((f[0].to_string(), hold, rate));
    }
    rows
}

#[test]
fn scripts_are_ours_and_complete() {
    let mut files: Vec<String> = std::fs::read_dir(dir())
        .expect("scripts dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("pvm7_") && n.ends_with(".pine"))
        .collect();
    files.sort();
    assert_eq!(
        files.len(),
        12,
        "the eleven screened + pvm7_grid_array: {files:?}"
    );
    assert!(!dir().to_string_lossy().contains("corpus"));
    for f in &files {
        assert!(f.starts_with("pvm7_"), "{f}");
        let src = read(f);
        let lines: Vec<&str> = src.lines().collect();
        assert_eq!(lines[0], "// SPDX-License-Identifier: Apache-2.0", "{f}");
        assert_eq!(lines[1], "// Copyright 2026 Anton (darkcite)", "{f}");
        assert_eq!(lines[2], "//@version=6", "{f}");
        assert_eq!(lines[3], "//", "{f}");
        let rule: String = lines[4..]
            .iter()
            .take_while(|l| l.starts_with("//"))
            .map(|l| l.trim_start_matches("//"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            rule.contains("OUR CODE, typed from the public rule"),
            "{f}: the rule comment"
        );
    }
    for (s, _) in SCREENED {
        assert!(files.contains(&format!("{s}.pine")), "{s}");
    }
    assert!(files.contains(&"pvm7_grid_array.pine".to_string()));
    assert!(
        !read("pvm7_grid.pine").contains("array."),
        "pvm7_grid is array-free"
    );
    // One priors row per screened file; grid_array none.
    let p = priors();
    assert_eq!(p.len(), 11);
    for (s, _) in SCREENED {
        assert_eq!(p.iter().filter(|r| r.0 == s).count(), 1, "{s}");
    }
    assert!(p.iter().all(|r| r.0 != "pvm7_grid_array"));
    let row = |s: &str| p.iter().find(|r| r.0 == s).map(|r| (r.1, r.2));
    assert_eq!(row("pvm7_supertrend"), Some((18, 55)));
    assert_eq!(row("pvm7_rsi_mr"), Some((6, 165)));
    assert_eq!(row("pvm7_grid"), Some((4, 120)));
}

/// Each screened script compiled for the synthetic chart and, when it compiles, run through the
/// one runner. A refusal is reported (a missing pine-std row or sema feature is a request to that
/// lane), never worked around; a `ScreenErr` (ours) fails.
#[test]
fn scripts_screen_on_synthetic_bars() {
    // 2 000 hourly rows: 40 days of warm-up (the ribbon's EMA-55 asks 770 rows), 40 scored.
    let b = bars_of(&sine_closes(2_000));
    let cfg = cfg(40, 80);
    let mut ws = Workspace::new();
    let mut report = String::new();
    let mut ran = 0;
    for (s, mode) in SCREENED {
        let src = read(&format!("{s}.pine"));
        match compile(&src) {
            Ok(t) => {
                assert_eq!(t.mode(), mode, "{s}");
                let o = run_trial(&t, &b, &cfg, &mut ws)
                    .unwrap_or_else(|e| panic!("{s}: ScreenErr {e:?} (ours)"));
                let faulted = matches!(o.status, TrialStatus::Faulted(_));
                assert_eq!(
                    faulted,
                    KNOWN_FAULTS.contains(&s),
                    "{s}: a fault in one of our scripts is a bug to find: {:?}",
                    o.status
                );
                assert!(
                    !matches!(o.status, TrialStatus::Refused(_)),
                    "{s}: refused on a chart that covers it: {:?}",
                    o.status
                );
                // I21: the two calc_on_every_tick scripts' rows are INTRABAR_APPROX, the rest not.
                assert_eq!(
                    o.summary.flags & pine_ir::trial_flag::INTRABAR_APPROX != 0,
                    mode == ExecMode::Realtime,
                    "{s}"
                );
                ran += 1;
                report += &format!(
                    "{s}\tRUN\t{:?}\ttrades={} net_bps_1e4={}\n",
                    o.status, o.summary.trades, o.summary.net_bps_1e4
                );
            }
            Err(e) => {
                let first = match &e {
                    CompileRefusal::Parse(ds) | CompileRefusal::Compile(ds) => ds
                        .iter()
                        .find(|d| d.sev == pine_ir::Severity::Error)
                        .map(|d| d.msg.clone())
                        .unwrap_or_default(),
                    other => format!("{other:?}"),
                };
                assert!(
                    !matches!(e, CompileRefusal::Verify(_)),
                    "{s}: a verifier refusal is ours"
                );
                report += &format!("{s}\tREFUSED\t{}\t{first}\n", e.code().0);
            }
        }
    }
    eprintln!("{report}");
    assert!(ran >= 1, "no script ran:\n{report}");
}

/// I15 over the eleven: K = 1 × 11, K = 4 + 4 + 3 and K = 11 give identical outcomes (rows, status,
/// broker record) — the interleave never changes a trial.
#[test]
fn screen_rows_do_not_depend_on_k_or_grouping_eleven() {
    let b = bars_of(&sine_closes(2_000));
    let cfg = cfg(40, 80);
    let loaded: Vec<pine_screen::Loaded> = SCREENED
        .iter()
        .map(|(s, _)| compile(&read(&format!("{s}.pine"))).expect("compiles"))
        .collect();
    let refs: Vec<&pine_screen::Loaded> = loaded.iter().collect();
    let singles: Vec<pine_screen::TrialOutcome> = refs
        .iter()
        .map(|t| run_trial(t, &b, &cfg, &mut Workspace::new()).expect("runs"))
        .collect();
    let mut ws = Workspace::new();
    let run = |ts: &[&pine_screen::Loaded], ws: &mut Workspace| {
        let mut out = vec![pine_screen::TrialOutcome::default(); ts.len()];
        pine_screen::run_group(ts, &b, &cfg, ws, &mut out, &mut pine_screen::NoProbe)
            .expect("runs");
        out
    };
    let all = run(&refs, &mut ws);
    assert_eq!(all, singles, "K = 11");
    let mut split = run(&refs[0..4], &mut ws);
    split.extend(run(&refs[4..8], &mut ws));
    split.extend(run(&refs[8..11], &mut ws));
    assert_eq!(split, singles, "K = 4 + 4 + 3 (one reused Workspace)");
}
