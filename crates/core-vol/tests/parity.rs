// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Rust ↔ Python parity of the VRP V4 forecast law.
//!
//! Consumes `claude-worker/tests/fixtures/vol/parity-<n>.input.tsv` — a
//! tape of ops (close, seed pair, arm, settle, settle-from-ring,
//! disarm, emit, sigma-emit) — and asserts every
//! emitted state row against `parity-<n>.expected.tsv`, the SAME pair
//! `claude-worker/tests/test_vol_ref.py` checks. The expected file is
//! (re)written by THIS harness under
//! `CORE_VOL_PARITY_WRITE=<fixture name>` — or `=1` for ALL of them:
//! the engine's code is the law, and the worker follows it. A change in
//! either implementation shows up as a red on one side.
//!
//! **Regenerate ONE fixture, not all of them.** `parity-1` is the VRP
//! lane's 8 h tape and its rows are a standing bit-identity guard
//! (BIN15 O4a added a 15-minute HAR term and proved it additive by
//! leaving that file untouched and still green). `=1` exists for a
//! deliberate law change; a lane adding a fixture names its own.
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
const FIXTURES: [&str; 2] = ["parity-1", "parity-15m"];

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
    // R3: the regime's log-vol intercept in force, sticky until the
    // next `O`. Zero for every row the fixture wrote before P4.1, which
    // is what keeps those rows bit-identical.
    let mut off_1e9: i64 = 0;
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
                e.arm_hold_at_with_offset(0, tau_ns, f[1].parse().unwrap(), off_1e9);
            }
            "O" => off_1e9 = f[1].parse().unwrap(),
            "S" => e.observe_settlement(f[1].parse().unwrap()),
            // F1: settle from the engine's OWN realised window — the
            // law the member uses live. `0` when the window is absent,
            // which `observe_settlement` ignores by contract.
            "R" => {
                let rv = e.realised_since_arm_1e9().unwrap_or(0);
                e.observe_settlement(rv);
            }
            "D" => e.disarm(),
            // BIN15 O4a: σ̂ over τ, the raw per-τ vol the binary pricer
            // consumes. Its own row shape, so a tape without `G` —
            // `parity-1` — keeps rows the VRP lane already pinned.
            "G" => {
                out.push(format!(
                    "{row}\tG\t{}\t{}",
                    opt(e.sigma_hat_1e9(tau_ns)),
                    opt(e.ln_sigma_hat_1e9(tau_ns)),
                ));
                row += 1;
            }
            "Q" => {
                let (a, b) = match e.fit() {
                    Some((a, b)) => (Some(a), Some(b)),
                    None => (None, None),
                };
                let (lo, hi) = match e.bounds_with_offset(tau_ns, theta_1e9, off_1e9) {
                    Some((lo, hi)) => (Some(lo), Some(hi)),
                    None => (None, None),
                };
                let q = e.qlike_counters();
                out.push(format!(
                    "{row}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
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
                    opt(e.realised_since_arm_1e9()),
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
    let write = std::env::var("CORE_VOL_PARITY_WRITE").unwrap_or_default();
    if write == "1" || write == name {
        let mut text = format!(
            "# {name}.expected.tsv — WRITTEN by crates/core-vol/tests/parity.rs \
             (CORE_VOL_PARITY_WRITE={name}).\n\
             # row minutes n_pairs har x a b ln_sigma_hat iv_lo iv_hi \
             qlike_n qlike_iv qlike_har har_beats_iv armed realised\n\
             # a `G` row is instead: row G sigma_hat ln_sigma_hat\n"
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

/// BIN15 P1b (F5): the slope clamp must be BIT-INERT on healthy fits.
///
/// `core-vol` also serves the VRP lane (4 h / 8 h), and a clamp that
/// moved a single fitted `b` there would silently re-price two live
/// campaigns. The fixture rows above already prove bit-identity — they
/// carry `a` and `b` per emitted row and they are unchanged — so this
/// test states the OTHER half explicitly: every fit the shared tapes
/// produce already lives inside `[B_MIN_1E9, B_MAX_1E9]`, i.e. the
/// clamp never engages on real data.
///
/// A red here is NOT a test to fix. It means a fixture fit sits outside
/// the bound, which means the VRP lane has been trading a slope the
/// bound calls broken — an operator finding, and its own investigation.
#[test]
fn every_fitted_slope_on_the_shared_tapes_is_already_inside_the_clamp() {
    for name in FIXTURES {
        let mut seen = 0usize;
        for row in run(name) {
            let f: Vec<&str> = row.split('\t').collect();
            // `G` rows carry no fit; state rows put `b` in column 6.
            if f.len() < 7 || f[1] == "G" || f[6] == "-" {
                continue;
            }
            let b: i64 = f[6].parse().expect("b is an integer");
            assert!(
                (core_vol::B_MIN_1E9..=core_vol::B_MAX_1E9).contains(&b),
                "{name}: fitted slope {b} is outside [{}, {}] — the clamp is NOT \
                 bit-inert on this tape, which means the lane that trades it has \
                 been trading a broken fit. Surface to the operator; do not widen \
                 the bound to make this pass",
                core_vol::B_MIN_1E9,
                core_vol::B_MAX_1E9
            );
            seen += 1;
        }
        assert!(seen > 0, "{name}: no fitted row — the guard would be vacuous");
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
    // Column 14 is `armed` (13 is `har_beats_iv`) — this assertion read
    // the wrong column before the `realised` column was added, and both
    // happened to be "0".
    assert_eq!(cell(&rows[n - 1], 14), "0", "and must leave nothing armed");
    // R3: somewhere in the tape two rows share a fit and a forecast but
    // NOT a band — that pair is the regime intercept, and nothing else
    // in the law can produce it.
    let mut i = 1usize;
    let mut found = false;
    while i < n {
        let (a, b) = (&rows[i - 1], &rows[i]);
        if cell(a, 7) == cell(b, 7) && cell(a, 5) == cell(b, 5) && cell(a, 8) != cell(b, 8) {
            let lo_a: i64 = cell(a, 8).parse().unwrap();
            let lo_b: i64 = cell(b, 8).parse().unwrap();
            let hi_a: i64 = cell(a, 9).parse().unwrap();
            let hi_b: i64 = cell(b, 9).parse().unwrap();
            // exp(−0.099) ≈ 0.905743, to a part in 1e5, on BOTH edges:
            // the offset SCALES the band, it does not widen it.
            let k = |x: i64, y: i64| (x as i128 * 1_000_000_000) / y as i128;
            assert!((k(lo_b, lo_a) - 905_742_878).abs() <= 10_000, "lo scales");
            assert!((k(hi_b, hi_a) - 905_742_878).abs() <= 10_000, "hi scales");
            found = true;
            break;
        }
        i += 1;
    }
    assert!(found, "the tape must exercise a regime intercept (`O`)");

    // ---- F1/F4/F5: the tape must reach each new branch ----
    // A hold that ran its full tenor reports a realised vol...
    let full = rows
        .iter()
        .find(|r| cell(r, 14) == "1" && cell(r, 15) != "-")
        .expect("an armed row whose hold has completed");
    let rv: i64 = cell(full, 15).parse().unwrap();
    assert!(rv > 0);
    // ...and it is not the forecast. F1 in one assertion.
    assert_ne!(cell(full, 15), cell(full, 3), "y must not be the HAR");
    // A hold that has not run reports ABSENT, not a short window.
    assert!(
        rows.iter().any(|r| cell(r, 14) == "1" && cell(r, 15) == "-"),
        "an armed row whose hold is still open"
    );
    // Nothing armed ⇒ nothing realised, on every row.
    assert!(
        rows.iter().all(|r| cell(r, 14) == "1" || cell(r, 15) == "-"),
        "a disarmed engine must report no realised window"
    );
    // BIN15 P1b (F5) REPLACED WHAT THIS PINS, and the reason is on the
    // record. The tape carries six rows whose raw OLS slope is NEGATIVE
    // (−0.4286 and −0.3092): F5 put them there so `b·x` went negative
    // and the floored division could be told from the truncated one.
    // The slope is now clamped to `[B_MIN_1E9, B_MAX_1E9]`, so those
    // rows fit at exactly 0 — "the higher the forecast, the lower the
    // realised vol" is a fit to refuse, not a fit to floor correctly.
    //
    // Verified before the fixture was regenerated (operator ruling
    // 2026-09-13): the LIVE 8 h fit off `~/multivenue/vrp-state.tsv`
    // is b = 0.909931 on 90 pairs with an x-spread of 0.416 in log, so
    // the clamp and the spread floor are bit-inert on real VRP data and
    // no historical VRP number moves. The exposure was this synthetic
    // tape alone.
    //
    // What the tape must still contain is the CLAMP ENGAGING — a
    // downward cloud pinned at the floor rather than trading inverted.
    assert!(
        rows.iter().any(|r| cell(r, 6) == core_vol::B_MIN_1E9.to_string()),
        "the tape must still drive a downward cloud into the clamp"
    );
    // And every fit on it is inside the bound, which is the property
    // `every_fitted_slope_on_the_shared_tapes_is_already_inside_the_clamp`
    // states for both tapes.
    assert!(
        rows.iter()
            .filter(|r| cell(r, 6) != "-")
            .all(|r| {
                let b: i64 = cell(r, 6).parse().unwrap();
                (core_vol::B_MIN_1E9..=core_vol::B_MAX_1E9).contains(&b)
            }),
        "a fitted slope escaped the clamp"
    );
}
