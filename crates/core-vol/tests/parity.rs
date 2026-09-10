// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Rust ↔ Python parity of the VRP V4 forecast law.
//!
//! Consumes `claude-worker/tests/fixtures/vol/parity-<n>.input.tsv` — a
//! tape of ops (close, seed pair, arm, settle, emit) — and asserts every
//! emitted state row against `parity-<n>.expected.tsv`, the SAME pair
//! `claude-worker/tests/test_vol_ref.py` checks. The expected file is
//! (re)written by THIS harness under `CORE_VOL_PARITY_WRITE=1`: the
//! engine's code is the law, and the worker follows it. A change in
//! either implementation shows up as a red on one side.
//!
//! Why a shared fixture and not two independent test suites: the seed
//! the engine boots with (V5) is cut by the Python, and the pairs the
//! engine forms afterwards continue that same series. If the two
//! implementations disagree by one unit anywhere, the fitted line the
//! engine trades on is not the line the research measured, and nothing
//! else in the lane would notice.
//!
//! Test-only code: allocation and `unwrap` are fine here.

use core_vol::VolEngine;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../claude-worker/tests/fixtures/vol/"
);
const FIXTURES: [&str; 1] = ["parity-1"];

fn opt(v: Option<i64>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => "-".to_owned(),
    }
}

/// Replay one fixture, returning the emitted rows.
fn run(name: &str) -> Vec<String> {
    let src = std::fs::read_to_string(format!("{FIXTURE_DIR}{name}.input.tsv"))
        .unwrap_or_else(|e| panic!("{name}.input.tsv: {e}"));

    let mut tau_ns: u64 = 0;
    let mut theta_1e9: i64 = 0;
    let mut e = VolEngine::new();
    let mut out: Vec<String> = Vec::new();
    let mut row = 0usize;

    for line in src.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = l.split('\t').collect();
        if let Some(v) = f[0].strip_prefix("tau_ns=") {
            tau_ns = v.parse().unwrap();
            theta_1e9 = f[1].strip_prefix("theta_1e9=").unwrap().parse().unwrap();
            continue;
        }
        match f[0] {
            "C" => e.on_minute_close(f[1].parse().unwrap()),
            "P" => e.seed_pair(f[1].parse().unwrap(), f[2].parse().unwrap()),
            "A" => {
                e.arm_hold(tau_ns, f[1].parse().unwrap());
            }
            "S" => e.observe_settlement(f[1].parse().unwrap()),
            "Q" => {
                let (a, b) = match e.fit() {
                    Some((a, b)) => (Some(a), Some(b)),
                    None => (None, None),
                };
                let (lo, hi) = match e.bounds(tau_ns, theta_1e9) {
                    Some((lo, hi)) => (Some(lo), Some(hi)),
                    None => (None, None),
                };
                let q = e.qlike_counters();
                out.push(format!(
                    "{row}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    e.minutes(),
                    e.n_pairs(),
                    opt(e.har_1e9(tau_ns)),
                    opt(e.x_1e9(tau_ns)),
                    opt(a),
                    opt(b),
                    opt(e.ln_sigma_hat_1e9(tau_ns)),
                    opt(lo),
                    opt(hi),
                    q.n,
                    q.iv_mean_1e9,
                    q.har_mean_1e9,
                    u8::from(q.har_beats_iv),
                    u8::from(e.is_armed()),
                ));
                row += 1;
            }
            other => panic!("{name}: unknown op {other:?}"),
        }
    }
    assert!(!out.is_empty(), "{name}: the tape emitted no rows");
    out
}

fn check(name: &str) {
    let got = run(name);
    let expected = format!("{FIXTURE_DIR}{name}.expected.tsv");
    if std::env::var("CORE_VOL_PARITY_WRITE").as_deref() == Ok("1") {
        let mut text = format!(
            "# {name}.expected.tsv — WRITTEN by crates/core-vol/tests/parity.rs \
             (CORE_VOL_PARITY_WRITE=1).\n\
             # row minutes n_pairs har x a b ln_sigma_hat iv_lo iv_hi \
             qlike_n qlike_iv qlike_har har_beats_iv armed\n"
        );
        for l in &got {
            text.push_str(l);
            text.push('\n');
        }
        std::fs::write(&expected, text).unwrap();
    }
    let want = std::fs::read_to_string(&expected)
        .unwrap_or_else(|e| panic!("{expected}: {e} — run with CORE_VOL_PARITY_WRITE=1 once"));
    let want: Vec<&str> = want
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(got.len(), want.len(), "{name}: row count drifted");
    for (g, w) in got.iter().zip(want.iter()) {
        assert_eq!(g, w, "{name}: the forecast law drifted from the shared fixture");
    }
}

#[test]
fn forecast_law_matches_the_shared_fixtures() {
    for name in FIXTURES {
        check(name);
    }
}

/// The fixture has to be a real exercise of the law, not a tape that
/// happens to run. These are the properties the rows must show, checked
/// against the file itself so a regenerated fixture cannot quietly
/// become trivial.
#[test]
fn the_fixture_exercises_every_branch_it_claims_to() {
    let rows = run("parity-1");
    let cell = |r: &str, i: usize| r.split('\t').nth(i).unwrap().to_owned();

    // A cold row: no HAR, no fit, no bounds.
    assert_eq!(cell(&rows[0], 3), "-", "row 0 must be cold");
    assert_eq!(cell(&rows[0], 5), "-");
    assert_eq!(cell(&rows[0], 7), "-");
    // A warm ring but no pairs: the HAR exists, the forecast does not.
    let warm = rows
        .iter()
        .find(|r| cell(r, 3) != "-" && cell(r, 5) == "-")
        .expect("a warm-ring, unfitted row");
    assert_eq!(cell(warm, 8), "-", "no fit ⇒ no bounds");
    // A fitted row with bounds that bracket the forecast.
    let fitted = rows
        .iter()
        .find(|r| cell(r, 5) != "-" && cell(r, 8) != "-")
        .expect("a fitted row");
    let lo: i64 = cell(fitted, 8).parse().unwrap();
    let hi: i64 = cell(fitted, 9).parse().unwrap();
    assert!(0 < lo && lo < hi, "lo {lo} hi {hi}");
    // The ring wrapped: more minutes than MINUTE_RING.
    let last: u64 = cell(rows.last().unwrap(), 1).parse().unwrap();
    assert!(
        last > core_vol::MINUTE_RING as u64,
        "the tape must wrap the minute ring: {last}"
    );
    // QLIKE was scored on the settled expiries.
    let scored: u32 = cell(rows.last().unwrap(), 10).parse().unwrap();
    assert!(scored >= 5, "the tape must settle expiries: {scored}");
    // And the last op (a settlement with nothing armed) changed nothing.
    let n = rows.len();
    assert_eq!(
        cell(&rows[n - 1], 2),
        cell(&rows[n - 2], 2),
        "an unarmed settlement must not form a pair"
    );
    assert_eq!(cell(&rows[n - 1], 13), "0", "and must leave nothing armed");
}
