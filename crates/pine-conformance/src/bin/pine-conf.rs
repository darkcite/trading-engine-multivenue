// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pine-conf` — the operator binary (impl-06 §3.12). Argv parsed by hand (no `clap`); exit 0 =
//! green, 1 = a divergence or a gate failure, 2 = usage or an unreadable input. No clock.
//!
//! This edition implements `gen [--check]`, `barhash --bars DIR [--write]` (§4.7.4) and `pins` (the §4.9.4 `pins.tsv` rows on stdout;
//! `--decide` lands with `cmp.rs`, T16); every other subcommand is a usage error until its task
//! lands. `pin:06-conf-root` — the workspace root is the current directory (DOCTRINE: commands run
//! from the lane root); it must hold `tests/conformance/chart.json`.
//!
//! COPY-DOCTRINE: an offline operator tool; it reads fixture files and never runs a bar step.

use std::path::Path;
use std::process::ExitCode;

const USAGE: &str =
    "usage: pine-conf gen [--check] | pine-conf pins | pine-conf barhash --bars DIR [--write]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = Path::new(".");
    if !root.join(pine_conformance::chart::CHART_JSON).is_file() {
        eprintln!("pine-conf: run from the workspace root (no tests/conformance/chart.json here)");
        return ExitCode::from(2);
    }
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["gen"] => match pine_conformance::gen::write(root) {
            Ok(n) => {
                let (w, c) = pine_conformance::gen::group_b::derived_counts();
                println!("pine-conf gen: wrote {n} fixture(s); B-wide {w}, B-chart {c}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("pine-conf gen: {e}");
                ExitCode::from(2)
            }
        },
        ["gen", "--check"] => {
            let diffs = pine_conformance::gen::check(root);
            for d in &diffs {
                println!("{d}");
            }
            if diffs.is_empty() {
                println!("pine-conf gen --check: byte-stable");
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        ["pins"] => {
            // §4.9.4's `pins.tsv` rows on stdout, from the `.fix` files on disk.
            let mut fixtures = Vec::new();
            for g in pine_conformance::gen::generate() {
                let parsed = std::fs::read_to_string(root.join(&g.fix_path))
                    .map_err(|e| format!("{}: {e}", g.fix_path))
                    .and_then(|t| {
                        pine_conformance::fixture::Fixture::parse(&t, &g.fix_path)
                            .map_err(|e| e.to_string())
                    });
                match parsed {
                    Ok(f) => fixtures.push(f),
                    Err(e) => {
                        eprintln!("pine-conf pins: {e}");
                        return ExitCode::from(2);
                    }
                }
            }
            print!("{}", pine_conformance::pins::tsv(&fixtures));
            ExitCode::SUCCESS
        }
        ["barhash", "--bars", dir] => barhash(root, dir, false),
        ["barhash", "--bars", dir, "--write"] => barhash(root, dir, true),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `pine-conf barhash`: the §4.7.4 checkpoints over the chart's prepared rows in the replay range
/// `[REPLAY_FROM_MS, TO_MS)`, printed; `--write` also writes `tests/conformance/barhash.tsv`.
fn barhash(root: &Path, dir: &str, write: bool) -> ExitCode {
    use pine_conformance::chart::spec;
    let mut src = pine_conformance::bars::ChartSource::new(Some(std::path::PathBuf::from(dir)));
    let clamp = pine_screen::Clamp {
        from_open_ms: spec::REPLAY_FROM_MS,
        to_close_ms: spec::TO_MS,
    };
    let rows = match src.rows(spec::SYMBOL_KEY, spec::TIMEFRAME_S, clamp) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("pine-conf barhash: {e:?}");
            return ExitCode::from(2);
        }
    };
    let cps = pine_conformance::capture::barhash(rows, spec::REPLAY_FROM_MS, spec::TO_MS);
    let n = pine_conformance::capture::window_rows(rows, spec::REPLAY_FROM_MS, spec::TO_MS);
    let tsv = pine_conformance::capture::barhash_tsv(&cps, n);
    print!("{tsv}");
    if write {
        let p = root.join("tests/conformance/barhash.tsv");
        if let Err(e) = std::fs::write(&p, tsv) {
            eprintln!("pine-conf barhash: {}: {e}", p.display());
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
