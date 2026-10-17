// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.1 — the suite. This edition (T15) runs every fixture whose oracle is ours
//! (`authored`, `algebraic`, `declined`) through the real engine — pine-lang → pine-sema → verify →
//! VM → pine-std → TvBroker — against the syminfo stand-in (`pin:06-syminfo-standin`), and pins the
//! current pass set: a regression shows as a pass leaving the set, an engine fix as a gap leaving
//! the table. Engine gaps are findings for their owners, never expectation edits.

use std::path::PathBuf;

use pine_conformance::fixture::{Fixture, Oracle};
use pine_conformance::report::Verdict;
use pine_conformance::run::{Runner, RunnerOptions, SkipReason, SyminfoSource};
use pine_ir::Dialect;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The tree's options with the captured `syminfo.tsv` pointed at an explicitly absent file (a fresh,
/// empty temp dir), so the runner reads the stand-in whether or not the tree holds the captured
/// file (the Mac does once probe-00 ran) — `pin:06-runner-syminfo-path`.
fn standin_options(tag: &str) -> RunnerOptions {
    let dir =
        std::env::temp_dir().join(format!("pine-conf-no-syminfo-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut o = RunnerOptions::new(root());
    o.syminfo_tsv = dir.join("syminfo.tsv");
    assert!(!o.syminfo_tsv.exists(), "the captured syminfo is absent");
    o
}

/// Every `.fix` under `tests/conformance/fixtures/<group>/`, sorted by id.
fn load_all() -> Vec<Fixture> {
    let dir = root().join("tests/conformance/fixtures");
    let mut out = Vec::new();
    let mut groups: Vec<_> = std::fs::read_dir(&dir)
        .expect("fixtures dir")
        .map(|e| e.expect("entry").path())
        .collect();
    groups.sort();
    for g in groups {
        let mut files: Vec<_> = std::fs::read_dir(&g)
            .expect("group dir")
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "fix"))
            .collect();
        files.sort();
        for p in files {
            let text = std::fs::read_to_string(&p).expect("read .fix");
            let f = Fixture::parse(&text, &p.display().to_string()).expect("parse .fix");
            out.push(f);
        }
    }
    out
}

/// Ours: the oracle is not a TradingView capture.
fn ours(f: &Fixture) -> bool {
    matches!(
        f.oracle,
        Oracle::Authored | Oracle::Algebraic | Oracle::Declined
    )
}

/// The fixtures that pass today (T15, 2026-10-17, stand-in syminfo, `DEFAULT_LIMITS`).
const PASS: &[&str] = &[
    "A051", "A052", "A064", "A081", "A082", "A083", "A084", "A085", "A086", "A087", "A088", "A089",
    "A091", "A092", "A093", "A095", "A096", "C026", "C030", "C038", "D035", "D036", "D037", "D038",
    "D039", "D040", "D048", "D049", "D057", "D061", "D062", "E119", "F054", "F059", "F084",
];

/// Known gaps: `(id, what the run did, owner)` — findings for the lead, never test or expectation
/// edits. Remove a row when its owner lands the fix (the pass set then grows).
///
/// 2026-10-17: C030 (pine-sema 3005-before-3047, I9) and D062 (3255 → 3053) left the table when
/// pine-sema landed both; D051/D052 left it by pin:06-d051-live-refuse (see [`SMOKE`]).
const KNOWN_GAPS: &[(&str, &str)] = &[];

/// Skips the environment explains (§4.5.9): tier-3 fixtures under `DEFAULT_LIMITS` (`0b0011`).
const SKIPS: &[&str] = &["A094", "C027", "C043", "F065", "F067", "F069", "F071"];

/// Smoke fixtures (`pending` body, ours): `Verdict::Pending` where the bar files exist (the Mac),
/// `Skipped(NoBarsRoot)` here — pin:06-d051-live-refuse.
const SMOKE: &[&str] = &["D051", "D052"];

fn short(v: &Verdict) -> String {
    match v {
        Verdict::Fail(d) => d
            .error
            .clone()
            .unwrap_or_else(|| format!("{d:?}"))
            .chars()
            .take(160)
            .collect(),
        other => format!("{other:?}"),
    }
}

#[test]
fn standin_fixtures_hold_the_known_pass_set() {
    let mut runner = Runner::new(standin_options("pass-set")).expect("runner");
    assert_eq!(runner.syminfo_source(), SyminfoSource::StandIn);
    let all = load_all();
    assert_eq!(all.len(), 566, "fixture count");
    let (mut pass, mut gaps, mut skips) = (Vec::new(), Vec::new(), Vec::new());
    let mut smoke: Vec<String> = Vec::new();
    for f in all.iter().filter(|f| ours(f)) {
        let v = runner.check(f);
        let id = f.id.as_str().to_string();
        match &v {
            Verdict::Pass => pass.push(id),
            Verdict::Skipped(SkipReason::TierInactive { .. }) => skips.push(id),
            Verdict::Pending | Verdict::Skipped(SkipReason::NoBarsRoot)
                if SMOKE.contains(&id.as_str()) =>
            {
                smoke.push(id)
            }
            Verdict::DialectDivergence { .. } => {
                let mut w = short(&v);
                for d in [Dialect::V6, Dialect::V5] {
                    w.push_str(&format!(" | v{}: {:?}", d as u8, runner.run(f, d).err()));
                }
                gaps.push((id, w));
            }
            _ => gaps.push((id, short(&v))),
        }
    }
    println!(
        "ran {} ours: pass {} / gap {} / skip {}",
        pass.len() + gaps.len() + skips.len(),
        pass.len(),
        gaps.len(),
        skips.len()
    );
    for (id, w) in &gaps {
        println!("GAP {id}\t{w}");
    }
    println!("PASS {pass:?}");
    println!("SKIP {skips:?}");
    let want_pass: Vec<String> = PASS.iter().map(|s| s.to_string()).collect();
    let want_gaps: Vec<String> = KNOWN_GAPS.iter().map(|(id, _)| id.to_string()).collect();
    let got_gaps: Vec<String> = gaps.iter().map(|(id, _)| id.clone()).collect();
    let want_skips: Vec<String> = SKIPS.iter().map(|s| s.to_string()).collect();
    assert_eq!(pass, want_pass, "pass set moved");
    assert_eq!(got_gaps, want_gaps, "known-gaps table moved");
    assert_eq!(skips, want_skips, "skips moved");
    assert_eq!(smoke, SMOKE, "smoke set moved");
}

#[test]
fn tv_fixtures_refuse_without_the_captured_syminfo() {
    let mut runner = Runner::new(standin_options("refuse")).expect("runner");
    assert_eq!(runner.syminfo_source(), SyminfoSource::StandIn);
    let all = load_all();
    let f = all
        .iter()
        .find(|f| f.oracle == Oracle::Tv && f.driver == pine_conformance::fixture::Driver::Syn)
        .expect("a tv syn fixture");
    let r = runner.run(f, pine_conformance::run::primary_dialect(f.dialects));
    let e = r.expect_err("a tv fixture against the stand-in refuses");
    assert_eq!(e.code(), Some(pine_conformance::code::FIXTURE_PARSE));
    assert!(format!("{e:?}").contains("pin:06-syminfo-standin"));
}
