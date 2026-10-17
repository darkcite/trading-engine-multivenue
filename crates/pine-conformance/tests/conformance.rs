// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.1 — the suite. This edition (T15) runs every fixture whose oracle is ours
//! (`authored`, `algebraic`, `declined`) through the real engine — pine-lang → pine-sema → verify →
//! VM → pine-std → TvBroker — against the syminfo stand-in (`pin:06-syminfo-standin`), and pins the
//! current pass set: a regression shows as a pass leaving the set, an engine fix as a gap leaving
//! the table. R-P1 (revised 2026-10-17): every divergence is documented in the issue register and
//! ruled by the lead before anything changes — the engine, the fixture (a generator defect: a
//! fixture can be wrong, and changes only through `gen` / `distil`) or the capture.

use std::path::PathBuf;

use pine_conformance::fixture::{Driver, Fixture, Oracle};
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

/// Skips the environment explains (§4.5.9): tier-3 fixtures under `DEFAULT_LIMITS` (`0b0011`);
/// r29 adds the UDT field rows' v5 twins F109, F111, F113 (authored `refuse 3005`, tier 3).
const SKIPS: &[&str] = &[
    "A094", "C027", "C043", "F065", "F067", "F069", "F071", "F109", "F111", "F113",
];

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
    // r29 (2026-10-17): 566 + E122–E132 + F099–F119 (pending, or the v5 twins' authored refusals).
    assert_eq!(all.len(), 598, "fixture count");
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

// ------------------------------------------------------------------------------------------------
// R-P1 (2026-10-17): TradingView is the oracle. Every captured fixture (`run::is_tv_captured`: oracle
// tv / tv_only, a captured body, not in `run::TV_EXCLUDED`) runs through the real engine against the
// captured `syminfo.tsv` under `run::TV_LIMITS` (tier 3 active), classified by
// `triage::classify_tv`. The pass set is pinned like the stand-in's: a regression is a pass leaving
// [`TV_PASS`]; an engine fix is a row leaving [`TV_GAPS`] (or changing class). `pine-conf report
// --tv` writes the same run as the divergence report (vault `conformance/run-r27.md`).
// ------------------------------------------------------------------------------------------------

/// TradingView passes (r33 part 5, 2026-10-18, no bars root): 343.
const TV_PASS: &[&str] = &[
    "A001", "A002", "A003", "A004", "A005", "A006", "A007", "A008", "A009", "A010", "A011", "A012",
    "A013", "A014", "A015", "A016", "A017", "A018", "A019", "A020", "A021", "A022", "A023", "A024",
    "A025", "A026", "A027", "A028", "A029", "A030", "A031", "A032", "A033", "A034", "A035", "A036",
    "A037", "A038", "A039", "A040", "A041", "A042", "A043", "A044", "A045", "A046", "A047", "A048",
    "A049", "A050", "A053", "A054", "A055", "A056", "A057", "A058", "A059", "A060", "A061", "A062",
    "A063", "A065", "A066", "A067", "A068", "A069", "A070", "A071", "A072", "A073", "A074", "A075",
    "A076", "A077", "A078", "A079", "A080", "A090", "B001", "B002", "B003", "B004", "B005", "B006",
    "B007", "B008", "B009", "B010", "B011", "B012", "B013", "B014", "B015", "B016", "B017", "B018",
    "B019", "B020", "B021", "B022", "B023", "B024", "B025", "B026", "B027", "B028", "B029", "B030",
    "B031", "B032", "B033", "B034", "B035", "B036", "B037", "B038", "B039", "B040", "B041", "B042",
    "B043", "B044", "B045", "B046", "B047", "B048", "B049", "B050", "B051", "B052", "B053", "B054",
    "B055", "B056", "B057", "B058", "B059", "B060", "B061", "B064", "B065", "B066", "B067", "B068",
    "B069", "B070", "B071", "B072", "B073", "B074", "B075", "B076", "B077", "B078", "B079", "B080",
    "B081", "B082", "B083", "B084", "B085", "B086", "B087", "B088", "B089", "B090", "B091", "B092",
    "B093", "B094", "B095", "B096", "B097", "B098", "B099", "B100", "B101", "B102", "B103", "C001",
    "C002", "C003", "C004", "C005", "C006", "C007", "C008", "C009", "C010", "C011", "C012", "C013",
    "C014", "C015", "C016", "C017", "C018", "C019", "C020", "C021", "C022", "C023", "C024", "C025",
    "C028", "C029", "C031", "C032", "C033", "C034", "C035", "C036", "C037", "C039", "C040", "C041",
    "C042", "C044", "C045", "C046", "D053", "D054", "E056", "E109", "E110", "E122", "E123", "E128",
    "F001", "F002", "F003", "F004", "F005", "F006", "F007", "F008", "F009", "F010", "F012", "F013",
    "F014", "F015", "F016", "F017", "F018", "F021", "F022", "F023", "F026", "F027", "F028", "F029",
    "F030", "F031", "F032", "F033", "F034", "F035", "F036", "F038", "F039", "F041", "F042", "F043",
    "F044", "F045", "F046", "F048", "F049", "F050", "F051", "F052", "F053", "F055", "F056", "F057",
    "F058", "F060", "F063", "F064", "F066", "F068", "F070", "F072", "F073", "F074", "F075", "F076",
    "F077", "F078", "F079", "F080", "F081", "F082", "F083", "F085", "F086", "F087", "F088", "F089",
    "F090", "F091", "F092", "F093", "F094", "F095", "F096", "F097", "F099", "F100", "F101", "F102",
    "F103", "F104", "F105", "F106", "F107", "F110", "F112", "F114", "F115", "F116", "F118", "F119",
    "Z001", "Z002", "Z003", "Z004", "Z005", "Z006", "Z007", "Z008", "Z009", "Z010", "Z011", "Z012",
    "Z013", "Z014", "Z015", "Z016", "Z017", "Z018", "Z019",
];

/// Known TradingView divergences `(id, class)` — engine items for their crates (R-P1), never
/// expectation edits (vault `conformance/run-r27.md` has the first difference of each). The
/// Chart / SynSig fixtures are not listed: without bar files every one is `harness-gap`
/// (`NoBarsRoot`), asserted by rule ([`TV_NO_BARS`]).
const TV_GAPS: &[(&str, &str)] = &[
    ("E106", "differ"),
    ("F011", "differ"),
    ("F019", "differ"),
    ("F020", "differ"),
    ("F024", "differ"),
    ("F025", "differ"),
    ("F037", "differ"),
    ("F040", "differ"),
    ("F047", "differ"),
    ("F061", "differ"),
    ("F062", "differ"),
    ("F098", "differ"),
    ("F117", "differ"),
];

/// Chart / SynSig fixtures of the run: all `harness-gap · NoBarsRoot` without a bars root (the
/// Mac runs them with `pine-conf report --tv --bars DIR`).
const TV_NO_BARS: usize = 194;
/// The captured TradingView fixtures the ratchet runs (r31 capture session 07).
const TV_CAPTURED: usize = 550;

fn tv_options() -> RunnerOptions {
    let mut o = RunnerOptions::new(root());
    o.limits = pine_conformance::run::TV_LIMITS;
    o.run_tv_only = true;
    o
}

#[test]
fn tv_fixtures_hold_the_known_pass_set() {
    use pine_conformance::triage::{classify_tv, tv_class_name, TvClass};
    let mut runner = Runner::new(tv_options()).expect("runner");
    assert_eq!(
        runner.syminfo_source(),
        SyminfoSource::Captured,
        "the captured syminfo.tsv is in the tree"
    );
    let all = load_all();
    let tv: Vec<&Fixture> = all
        .iter()
        .filter(|f| pine_conformance::run::is_tv_captured(f))
        .collect();
    println!("TVCOUNT {}", tv.len());
    assert_eq!(
        tv.len(),
        TV_CAPTURED,
        "captured fixtures run (r31: 550 — F108 escalated, uncapturable)"
    );
    let (mut pass, mut gaps) = (Vec::new(), Vec::new());
    let mut no_bars = 0usize;
    for f in tv {
        let t = classify_tv(f, &runner.check_full(f));
        let id = f.id.as_str().to_string();
        let charted = matches!(f.driver, Driver::Chart | Driver::SynSig);
        // A Chart / SynSig fixture needs bars — except one whose TradingView outcome is a
        // compile / run refusal (a paste, R-P2: E056 E110 E128), judged without running bars.
        if charted && t.cluster == "harness-gap · NoBarsRoot" {
            no_bars += 1;
        } else if t.class == TvClass::Pass {
            pass.push(id);
        } else {
            println!("TVGAP {id}\t{}\t{}", tv_class_name(t.class), t.first);
            gaps.push((id, tv_class_name(t.class).to_string()));
        }
    }
    println!(
        "tv: pass {} / gap {} / no bars {no_bars}",
        pass.len(),
        gaps.len()
    );
    assert_eq!(no_bars, TV_NO_BARS, "Chart / SynSig fixture count");
    println!("TVPASS {pass:?}");
    let want_pass: Vec<String> = TV_PASS.iter().map(|s| s.to_string()).collect();
    let want_gaps: Vec<(String, String)> = TV_GAPS
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    let lost: Vec<&String> = want_pass.iter().filter(|p| !pass.contains(p)).collect();
    assert!(
        lost.is_empty(),
        "TradingView regressions (left the pass set): {lost:?}"
    );
    assert_eq!(
        pass, want_pass,
        "TradingView pass set moved (an engine fix: update TV_PASS / TV_GAPS)"
    );
    assert_eq!(gaps, want_gaps, "TradingView gap table moved");
}
