// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.6 — triage, signatures and the failure report (T17). Fixtures are ours
//! (`tests/fix/B011.fix` and rows built here); no TradingView value.

use std::path::PathBuf;

use pine_conformance::fixture::{Align, Cell, Driver, Expect, Fixture, Oracle, Row};
use pine_conformance::report::{render, render_with, Divergence, Verdict};
use pine_conformance::run::{RunError, RunOutcome, Runner, RunnerOptions, SkipReason};
use pine_conformance::sig::{match_signature, Kind};
use pine_conformance::triage::{triage, Bucket, Owner, TriageEnv};
use pine_ir::{DiagCode, Dialect, FaultKind, SymTable, VmCounters};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn b011_text() -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fix/B011.fix"))
        .expect("B011.fix")
}

fn b011() -> Fixture {
    Fixture::parse(&b011_text(), "tests/fix/B011.fix").expect("parse B011")
}

fn env_for(f: &Fixture) -> TriageEnv {
    TriageEnv {
        chart_sha256: f.chart_sha256,
        barhash_ok: None,
        active_tiers: 0b0011,
    }
}

/// An outcome over `keys` with two plots (isna, val), row-major.
fn outcome(keys: &[i64], isna: &[f64], val: &[f64]) -> RunOutcome {
    let mut plots = Vec::new();
    for i in 0..keys.len() {
        plots.push(isna[i]);
        plots.push(val[i]);
    }
    RunOutcome {
        dialect: Dialect::V6,
        plots,
        keys: keys.to_vec(),
        plot_ix: vec![0, 1],
        trades: Vec::new(),
        record: None,
        evicted: Default::default(),
        cum_net: Vec::new(),
        bars: keys.len() as u32,
        evaluations: keys.len() as u64,
        counters: VmCounters::default(),
        source_sha256: [0; 32],
        syms: SymTable::new(),
    }
}

fn num(v: f64) -> Cell {
    Cell::Val {
        v,
        text: format!("{v:?}"),
    }
}

/// Rows `0..n` with isna 0 and `val[i]`.
fn rows(val: &[f64]) -> Vec<Row> {
    val.iter()
        .enumerate()
        .map(|(i, v)| Row {
            key: i as i64,
            cells: vec![num(0.0), num(*v)],
        })
        .collect()
}

fn fail_at(plot: usize, row: i64) -> Verdict {
    Verdict::Fail(Divergence {
        plot: Some(plot),
        first_row: row,
        first_out_of_bound: Some(row),
        trade: None,
        error: None,
    })
}

fn diag(code: u16) -> pine_ir::Diag {
    pine_ir::Diag {
        code: DiagCode(code),
        sev: pine_ir::Severity::Error,
        span: pine_ir::Span::default(),
        msg: String::new(),
        notes: Vec::new(),
    }
}

#[test]
fn sig_matches_pin_reading() {
    // The every_bar reading with the na mask kept, so only the values move.
    let text = b011_text().replace("19\t0\t1e-7", "19\t1\tna");
    let f = Fixture::parse(&text, "B011-pin").unwrap();
    let Expect::Series(exp) = &f.expect else {
        panic!("series")
    };
    let o = outcome(
        &[17, 18, 19],
        &[0.0, 0.0, 1.0],
        &[1283.0, 1284.125, f64::NAN],
    );
    assert_eq!(match_signature(&f, exp, &o, 1), Some(Kind::PinReading));
}

#[test]
fn sig_matches_off_by_one_warmup() {
    let mut f = b011();
    f.readings.clear();
    let e: Vec<f64> = (0..20).map(|i| f64::from(i) * 1.5 + 0.25).collect();
    let mut a = vec![-7.0];
    a.extend_from_slice(&e[..19]);
    let exp = rows(&e);
    f.expect = Expect::Series(exp.clone());
    let keys: Vec<i64> = (0..20).collect();
    let o = outcome(&keys, &[0.0; 20], &a);
    assert_eq!(match_signature(&f, &exp, &o, 1), Some(Kind::OffByOneWarmup));
}

#[test]
fn sig_matches_scale_1e6() {
    let mut f = b011();
    f.readings.clear();
    let e: Vec<f64> = (0..10).map(|i| 60_000.0 + f64::from(i) * 13.37).collect();
    let a: Vec<f64> = e.iter().map(|x| x * 1e6).collect();
    let exp = rows(&e);
    let keys: Vec<i64> = (0..10).collect();
    let o = outcome(&keys, &[0.0; 10], &a);
    assert_eq!(match_signature(&f, &exp, &o, 1), Some(Kind::Scale1e6));
}

#[test]
fn sig_matches_na_mask_first() {
    let f = b011();
    let Expect::Series(exp) = &f.expect else {
        panic!("series")
    };
    // The every_bar reading as given: row 19's isna differs → NaMask, before PinReading.
    let o = outcome(&[17, 18, 19], &[0.0, 0.0, 0.0], &[1283.0, 1284.125, 1e-7]);
    assert_eq!(match_signature(&f, exp, &o, 1), Some(Kind::NaMask));
}

#[test]
fn sig_unmatched_reports_none() {
    let mut f = b011();
    f.readings.clear();
    let e = [1.0, 2.0, 3.0, 4.0, 5.0];
    let a = [1.5, 7.0, 2.0, 9.0, 4.4];
    let exp = rows(&e);
    let o = outcome(&[0, 1, 2, 3, 4], &[0.0; 5], &a);
    assert_eq!(match_signature(&f, &exp, &o, 1), None);
}

#[test]
fn triage_orders_chart_drift_first() {
    let mut f = b011();
    f.oracle = Oracle::Declined;
    let mut env = env_for(&f);
    env.chart_sha256[0] ^= 1;
    let r = Err(RunError::Skip(SkipReason::TierInactive { tier: 3 }));
    let v = Verdict::Skipped(SkipReason::TierInactive { tier: 3 });
    let t = triage(&f, &r, &v, &env);
    assert_eq!((t.bucket, t.owner), (Bucket::ChartDrift, Owner::Operator));
    // Without the drift, tier-inactive is Unimplemented before Declined.
    let t = triage(&f, &r, &v, &env_for(&f));
    assert_eq!(t.bucket, Bucket::Unimplemented);
    let r3024 = Err(RunError::Compile(vec![diag(3024)]));
    let t = triage(&f, &r3024, &fail_at(1, 17), &env_for(&f));
    assert_eq!(t.bucket, Bucket::Unimplemented);
}

#[test]
fn triage_declines_by_design_codes() {
    let f = b011(); // oracle tv
    for code in [3016u16, 3023, 3026, 3032, 3042, 3043, 3053, 5001, 5002] {
        let r = Err(RunError::Compile(vec![diag(code)]));
        let t = triage(&f, &r, &fail_at(1, 17), &env_for(&f));
        assert_eq!(t.bucket, Bucket::Declined, "{code}");
    }
    for code in [5100u16, 5109] {
        let r = Err(RunError::BrokerDecl(DiagCode(code)));
        assert_eq!(
            triage(&f, &r, &fail_at(1, 17), &env_for(&f)).bucket,
            Bucket::Declined
        );
    }
    // Retired and out-of-set codes are not by design.
    for code in [3025u16, 3033, 3015, 3054] {
        let r = Err(RunError::Compile(vec![diag(code)]));
        assert_eq!(
            triage(&f, &r, &fail_at(1, 17), &env_for(&f)).bucket,
            Bucket::SemanticBug,
            "{code}"
        );
    }
    let mut d = b011();
    d.oracle = Oracle::Declined;
    let t = triage(
        &d,
        &Ok(outcome(&[], &[], &[])),
        &Verdict::Pass,
        &env_for(&d),
    );
    assert_eq!(t.bucket, Bucket::Declined);
}

#[test]
fn triage_never_data_buckets_a_syn_fixture() {
    let mut f = b011();
    f.align = Align::Time;
    let mut env = env_for(&f);
    env.barhash_ok = Some(false);
    let o = outcome(&[17, 18, 19], &[0.0, 0.0, 1.0], &[1.0, 2.0, f64::NAN]);
    for drv in [Driver::Syn, Driver::Rt] {
        f.driver = drv;
        let t = triage(&f, &Ok(o.clone()), &fail_at(1, 17), &env);
        assert_ne!(t.bucket, Bucket::DataRevision, "{drv:?}");
    }
    f.driver = Driver::Chart;
    let t = triage(&f, &Ok(o), &fail_at(1, 17), &env);
    assert_eq!((t.bucket, t.owner), (Bucket::DataRevision, Owner::Operator));
}

#[test]
fn triage_maps_owner_from_stage_and_group() {
    let mut f = b011();
    f.pins.clear();
    f.readings.clear();
    let env = env_for(&f);
    let v = fail_at(1, 17);
    let owner = |f: &Fixture, r: Result<RunOutcome, RunError>| triage(f, &r, &v, &env).owner;
    assert_eq!(
        owner(&f, Err(RunError::Parse(vec![diag(1001)]))),
        Owner::Pvm1
    );
    assert_eq!(
        owner(&f, Err(RunError::Compile(vec![diag(3001)]))),
        Owner::Pvm2
    );
    assert_eq!(
        owner(&f, Err(RunError::Compile(vec![diag(5004)]))),
        Owner::Pvm4
    );
    assert_eq!(
        owner(&f, Err(RunError::BrokerDecl(DiagCode(5150)))),
        Owner::Pvm5
    );
    let fault = || RunError::Fault {
        kind: FaultKind::Internal,
        pc: 0,
        bar_index: 0,
        sub: false,
    };
    assert_eq!(owner(&f, Err(fault())), Owner::Pvm4); // group B
    let o = outcome(&[17], &[0.0], &[1.0]);
    for (id, want) in [
        ("A001", Owner::Pvm3),
        ("B001", Owner::Pvm4),
        ("C001", Owner::Pvm2),
        ("D001", Owner::Pvm4),
        ("E001", Owner::Pvm5),
        ("F001", Owner::Pvm3),
        ("Z001", Owner::Pvm1),
    ] {
        let mut g = f.clone();
        g.id = pine_conformance::fixture::FixtureId::parse(id).unwrap();
        assert_eq!(owner(&g, Ok(o.clone())), want, "{id}");
    }
    // A pin fixture's owner is the pin's switch owner.
    let mut p = f.clone();
    p.pins = vec!["pin:02-cond-call".into()];
    assert_eq!(owner(&p, Ok(o)), Owner::Pvm2);
    // Dialect divergence carries its signature.
    let t = triage(
        &f,
        &Ok(outcome(&[], &[], &[])),
        &Verdict::DialectDivergence {
            first_row: 1,
            plot: 0,
        },
        &env,
    );
    assert_eq!(t.signature, Some(Kind::DialectDivergence));
}

#[test]
fn triage_is_deterministic_over_the_manifest() {
    let mut runner = Runner::new(RunnerOptions::new(root())).expect("runner");
    let dir = root().join("tests/conformance/fixtures");
    let mut n = 0usize;
    let mut groups: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    groups.sort();
    for g in groups {
        let mut files: Vec<_> = std::fs::read_dir(&g)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "fix"))
            .collect();
        files.sort();
        for p in files {
            let f = Fixture::parse(&std::fs::read_to_string(&p).unwrap(), "m").unwrap();
            let r = runner.run(&f, pine_conformance::run::primary_dialect(f.dialects));
            let v = runner.check(&f);
            let mut env = env_for(&f);
            env.barhash_ok = Some(false);
            let a = triage(&f, &r, &v, &env);
            let b = triage(&f, &r, &v, &env);
            assert_eq!(
                (a.bucket, a.owner, a.signature, &a.detail),
                (b.bucket, b.owner, b.signature, &b.detail),
                "{}",
                f.id.as_str()
            );
            if matches!(f.driver, Driver::Syn | Driver::Rt) {
                assert_ne!(a.bucket, Bucket::DataRevision, "{}", f.id.as_str());
            }
            n += 1;
        }
    }
    // r29 (2026-10-17): 566 + E122–E132 + F099–F119.
    assert_eq!(n, 598);
}

/// `tests/golden/report_B011.txt`; `PINE_CONF_BLESS=1` rewrites it (review the diff).
#[test]
fn report_golden() {
    let f = b011();
    let o = outcome(
        &[17, 18, 19],
        &[0.0, 0.0, 1.0],
        &[1284.5327181818184, 1287.0, f64::NAN],
    );
    let v = Verdict::Fail(Divergence {
        plot: Some(1),
        first_row: 17,
        first_out_of_bound: Some(18),
        trade: None,
        error: None,
    });
    let t = triage(&f, &Ok(o.clone()), &v, &env_for(&f));
    let mut got = render_with(&f, &v, &t, None, Some(&o));
    got.push_str("---\n");
    got.push_str(&render(&f, &v, &t, None));
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/report_B011.txt");
    if std::env::var_os("PINE_CONF_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&path).expect("golden file");
    assert_eq!(got, want);
    // Byte-stable: rendering twice is identical.
    assert_eq!(render(&f, &v, &t, None), render(&f, &v, &t, None));
}
