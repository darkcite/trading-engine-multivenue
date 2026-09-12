// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Rust ↔ Python parity of the BIN15 integer pricer (O4b).
//!
//! Consumes `claude-worker/tests/fixtures/bin15/parity-<n>.input.tsv` — a
//! tape of pricer ops — and asserts every emitted row against
//! `parity-<n>.expected.tsv`, the SAME pair
//! `claude-worker/tests/test_bin15_ref.py` checks through
//! `claude_worker.bin15_ref`. The expected file is (re)written by THIS
//! harness under `BIN15_PARITY_WRITE=<fixture name>` — or `=1` for ALL
//! of them: the engine's code is the law, and the worker follows it.
//!
//! **Regenerate ONE fixture, not all of them.** This is O4a's lesson
//! taken verbatim: a blanket `=1` lets a lane re-bless a tape it never
//! read, and `parity-1` here is the shipped table's proof. `=1` exists
//! for a deliberate law change; a lane adding a fixture names its own.
//!
//! **Tolerance is zero.** Every step of the pricer is integer, so there
//! is no tolerance to set. A one-unit disagreement means the fair value
//! the engine crosses a book on is not the fair value the calibration
//! ledger scores, and the O5 desk gate would be measuring a model that
//! never traded.
//!
//! The tables travel IN the fixture (`PHI` / `RECAL` records) rather
//! than being rebuilt on each side. That is deliberate: it pins the
//! SHIPPED numbers — the ones `bin15.toml.example` carries and the
//! engine hashes into its boot tell — instead of a test-local ramp that
//! both sides could agree on while the artifact held something else.
//! It is also why this test takes no `core-config` dependency: the
//! member must not link the config crate (spec §6.4), not even in dev.
//!
//! Fixture grammar (one record per line, `#` comments and blanks
//! skipped), mirrored exactly by `claude_worker.bin15_ref.replay`:
//!
//! | record | meaning | emits |
//! |---|---|---|
//! | `PHI v0 … v4096` | load Φ | — |
//! | `RECAL phase v0 … v64` | load one recal table | — |
//! | `F mark strike tau sig2` | `fair_value` | `F p_hat p_raw d` or `F - - -` |
//! | `X mark strike` | `log_moneyness_1e9` | `X x_1e9` or `X -` |
//! | `P d_1e6` | `phi_1e6` | `P v` |
//! | `R phase p_1e6` | `recal_1e6` | `R v` |
//! | `H tau_ns` | `phase_of` | `H phase` |
//! | `G px_1e6 tick_1e6` | the grid rounders | `G floor ceil` |
//!
//! Test-only code: allocation and `unwrap` are fine here.

use strategy_bin15::price::{
    ceil_grid_1e6, fair_value, floor_grid_1e6, log_moneyness_1e9, phase_of, Bin15Luts, PHASES,
    PHI_POINTS, RECAL_POINTS,
};

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../claude-worker/tests/fixtures/bin15/"
);
const FIXTURES: [&str; 1] = ["parity-1"];

/// Replay one fixture, returning the emitted rows.
fn run(name: &str) -> Vec<String> {
    let src = std::fs::read_to_string(format!("{FIXTURE_DIR}{name}.input.tsv"))
        .unwrap_or_else(|e| panic!("{name}.input.tsv: {e}"));

    // Boxed for the same reason the member boxes it: 4 097 + 3 × 65
    // `u32`s is 17 KiB and a stack frame is not the place for it.
    let mut luts = Box::new(Bin15Luts::identity());
    let mut out: Vec<String> = Vec::new();

    for line in src.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = l.split('\t').collect();
        match f[0] {
            "PHI" => {
                assert_eq!(f.len() - 1, PHI_POINTS, "PHI record length");
                let mut i = 0usize;
                while i < PHI_POINTS {
                    luts.phi[i] = f[i + 1].parse().unwrap();
                    i += 1;
                }
            }
            "RECAL" => {
                assert_eq!(f.len() - 2, RECAL_POINTS, "RECAL record length");
                let ph: usize = f[1].parse().unwrap();
                assert!(ph < PHASES, "RECAL phase {ph}");
                let mut k = 0usize;
                while k < RECAL_POINTS {
                    luts.recal[ph][k] = f[k + 2].parse().unwrap();
                    k += 1;
                }
            }
            "F" => {
                let row = match fair_value(
                    &luts,
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                    f[3].parse().unwrap(),
                    f[4].parse().unwrap(),
                ) {
                    Some(v) => format!("F\t{}\t{}\t{}", v.p_hat_1e6, v.p_raw_1e6, v.d_1e6),
                    None => "F\t-\t-\t-".to_owned(),
                };
                out.push(row);
            }
            "X" => {
                let x = log_moneyness_1e9(f[1].parse().unwrap(), f[2].parse().unwrap());
                out.push(match x {
                    Some(v) => format!("X\t{v}"),
                    None => "X\t-".to_owned(),
                });
            }
            "P" => out.push(format!("P\t{}", luts.phi_1e6(f[1].parse().unwrap()))),
            "R" => out.push(format!(
                "R\t{}",
                luts.recal_1e6(f[1].parse().unwrap(), f[2].parse().unwrap())
            )),
            "H" => out.push(format!("H\t{}", phase_of(f[1].parse().unwrap()))),
            "G" => {
                let px: i64 = f[1].parse().unwrap();
                let tick: i64 = f[2].parse().unwrap();
                out.push(format!(
                    "G\t{}\t{}",
                    floor_grid_1e6(px, tick),
                    ceil_grid_1e6(px, tick)
                ));
            }
            other => panic!("unknown fixture record `{other}`"),
        }
    }
    out
}

fn check(name: &str) {
    let got = run(name);
    assert!(
        got.len() > 1_000,
        "{name}: the fixture produced almost nothing ({} rows)",
        got.len()
    );
    let expected = format!("{FIXTURE_DIR}{name}.expected.tsv");
    let write = std::env::var("BIN15_PARITY_WRITE").unwrap_or_default();
    if write == "1" || write == name {
        let mut text = format!(
            "# {name}.expected.tsv — WRITTEN by crates/strategy-bin15/tests/parity.rs \
             (BIN15_PARITY_WRITE={name}).\n\
             # F p_hat_1e6 p_raw_1e6 d_1e6   (`-` = the pricer refused the inputs)\n\
             # X x_1e9 · P phi_1e6 · R recal_1e6 · H phase · G floor ceil\n"
        );
        for l in &got {
            text.push_str(l);
            text.push('\n');
        }
        std::fs::write(&expected, text).unwrap();
        eprintln!("wrote {expected} ({} rows)", got.len());
        return;
    }
    let want = std::fs::read_to_string(&expected)
        .unwrap_or_else(|e| panic!("{expected}: {e} (run with BIN15_PARITY_WRITE={name})"));
    let want: Vec<&str> = want
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(
        got.len(),
        want.len(),
        "{name}: row count moved — the tape changed, not the law"
    );
    let mut i = 0usize;
    while i < want.len() {
        assert_eq!(
            got[i], want[i],
            "{name}: row {} disagrees with the Python mirror (tolerance is 0)",
            i + 1
        );
        i += 1;
    }
}

#[test]
fn parity_fixtures_match_the_python_mirror() {
    for name in FIXTURES {
        check(name);
    }
}
