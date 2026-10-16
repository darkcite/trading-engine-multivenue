// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.11 — `multivenue-engine pine run | hurdle | screen`: argv, refusals, exit codes, the
//! run-directory layout, the hurdle's drop, and thread invariance (I15).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use pine_ir::{barflag, Bar};
use pine_screen::bar_path;
use pine_screen::bars::{bar_source, write_bar_file};
use pine_screen::rows::RowsFile;

const BIN: &str = env!("CARGO_BIN_EXE_multivenue-engine");
/// 2026-01-01T00:00:00Z.
const T0: i64 = 1_767_225_600_000;
const H: i64 = 3_600_000;
const D: i64 = 86_400_000;
const DAYS: i64 = 60;
const KEYS: [&str; 3] = [
    "binance_usdm:btcusdt",
    "binance_usdm:ethusdt",
    "binance_usdm:solusdt",
];

const SMA_CROSS: &str = "//@version=6
strategy(\"sma cross\", overlay=true)
f = ta.sma(close, 10)
s = ta.sma(close, 30)
if ta.crossover(f, s)
    strategy.entry(\"L\", strategy.long)
if ta.crossunder(f, s)
    strategy.close(\"L\")
plot(f)
";

/// A loop of 10^8 iterations at bar_index 50 exhausts the 2 000 000 budget.
const BUDGET_AT_50: &str = "//@version=6
strategy(\"budget\")
var float x = 0.0
if bar_index == 50
    for i = 0 to 100000000
        x += 1
plot(x)
";

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("pine_cli_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Hourly sine bars (a phase per symbol) on the 0.01 grid for `DAYS` days.
fn write_bars(root: &Path, key: &str, phase: f64) {
    let n = (DAYS * 24) as usize;
    let mut rows = Vec::with_capacity(n);
    let mut prev = 0i64;
    for i in 0..n {
        let t = i as f64;
        let c = 100.0
            + 8.0 * (2.0 * std::f64::consts::PI * t / 120.0 + phase).sin()
            + 2.5 * (2.0 * std::f64::consts::PI * t / 31.0).sin();
        let c = (c * 100.0).round() as i64 * 10_000;
        let o = if i == 0 { c } else { prev };
        let open_ms = T0 + i as i64 * H;
        rows.push(Bar {
            open_1e6: o,
            high_1e6: o.max(c) + 50_000,
            low_1e6: o.min(c) - 50_000,
            close_1e6: c,
            volume_1e6: 1_000_000,
            open_ms,
            close_ms: open_ms + H,
            index: i as u32,
            flags: barflag::CONFIRMED,
        });
        prev = c;
    }
    let p = bar_path(root, key, 3_600).unwrap();
    write_bar_file(
        &p,
        &rows,
        3_600,
        pine_ir::symbol_hash(key.as_bytes()),
        1,
        bar_source::CANDLES,
        false,
    )
    .unwrap();
}

struct Fixture {
    dir: PathBuf,
    bars: PathBuf,
    universe: PathBuf,
    scripts: PathBuf,
    priors: PathBuf,
}

/// Bars for `n_keys` symbols, a universe naming them (fee venue `bn`, perp), the test scripts and
/// a priors table `(script, hold)`.
fn fixture(name: &str, n_keys: usize, priors: &[(&str, u32)]) -> Fixture {
    let dir = tmp(name);
    let bars = dir.join("bars");
    let mut uni = String::from("# test universe\n");
    for (i, key) in KEYS.iter().take(n_keys).enumerate() {
        write_bars(&bars, key, i as f64);
        let sym = key.split(':').nth(1).unwrap().to_uppercase();
        uni.push_str(&format!(
            "{key}\tbinance-usdm:{}\t-\t1\tperp\tbn\t10000\t1000000\t1000\t5000000\t{sym}\tX\tUSDT\tcrypto\n",
            key.split(':').nth(1).unwrap()
        ));
    }
    let universe = dir.join("screen-universe.tsv");
    std::fs::write(&universe, uni).unwrap();
    let scripts = dir.join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join("t_sma.pine"), SMA_CROSS).unwrap();
    std::fs::write(scripts.join("t_budget.pine"), BUDGET_AT_50).unwrap();
    let mut pr = String::from("# script\thold_bars\ttrades_per_1k_bars\tbuiltins\n");
    for (sc, hold) in priors {
        pr.push_str(&format!("# test prior\n{sc}\t{hold}\t10\tta.sma\n"));
    }
    let priors = dir.join("priors.tsv");
    std::fs::write(&priors, pr).unwrap();
    Fixture {
        dir,
        bars,
        universe,
        scripts,
        priors,
    }
}

fn pine(verb: &str, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("pine")
        .arg(verb)
        .args(args)
        .output()
        .unwrap()
}

fn from() -> String {
    (T0 + 10 * D).to_string()
}

fn to() -> String {
    (T0 + DAYS * D).to_string()
}

fn screen_args<'a>(f: &'a Fixture, out: &'a Path, from: &'a str, to: &'a str) -> Vec<&'a str> {
    vec![
        "--scripts",
        s(&f.scripts),
        "--priors",
        s(&f.priors),
        "--bars",
        s(&f.bars),
        "--universe",
        s(&f.universe),
        "--timeframes",
        "3600",
        "--from-ms",
        from,
        "--to-ms",
        to,
        "--fee-bps",
        "bn:2:5",
        "--out",
        s(out),
        "--lib-dir",
        s(&f.dir),
    ]
}

fn err(o: &Output) -> String {
    format!(
        "status {:?}\nstdout {}\nstderr {}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn pine_cli_parse_time_refusals_exit_2() {
    let f = fixture("argv", 1, &[("t_sma", 18)]);
    let out = f.dir.join("run");
    let (fr, t) = (from(), to());
    let base = screen_args(&f, &out, &fr, &t);
    let with = |pairs: &[(&str, &str)], extra: &[&str]| {
        let mut a: Vec<String> = base.iter().map(|x| (*x).to_owned()).collect();
        for (k, v) in pairs {
            let i = a.iter().position(|x| x == k).unwrap();
            a[i + 1] = (*v).to_owned();
        }
        a.extend(extra.iter().map(|x| (*x).to_owned()));
        a
    };
    let off_day = (T0 + 10 * D + H).to_string();
    let cases: Vec<Vec<String>> = vec![
        with(&[("--timeframes", "3601")], &[]),
        with(&[], &["--threads", "0"]),
        with(&[], &["--interleave", "0"]),
        with(&[], &["--interleave", "65"]),
        with(&[("--from-ms", &off_day)], &[]),
        with(&[("--to-ms", &fr)], &[]),
        with(&[], &["--notional-1e6", "0"]),
        with(&[("--fee-bps", "bn.perps:2:5")], &[]),
        with(&[], &["--hurdle-mode", "maybe"]),
        with(&[], &["--interleave-sweep", "1,0"]),
    ];
    for c in &cases {
        let refs: Vec<&str> = c.iter().map(String::as_str).collect();
        let o = pine("screen", &refs);
        assert_eq!(o.status.code(), Some(2), "{c:?}\n{}", err(&o));
        assert!(!out.exists(), "nothing written: {c:?}");
    }
    // `run` shares the grammar.
    let o = pine(
        "run",
        &[
            "--script",
            s(&f.scripts.join("t_sma.pine")),
            "--bars",
            s(&f.bars),
            "--universe",
            s(&f.universe),
            "--symbol",
            KEYS[0],
            "--timeframe",
            "3600",
            "--from-ms",
            &fr,
            "--to-ms",
            &t,
            "--fee-bps",
            "nope:1:2",
            "--out",
            s(&out),
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(!out.exists());
}

fn run_args<'a>(
    f: &'a Fixture,
    script: &'a Path,
    out: &'a Path,
    fr: &'a str,
    t: &'a str,
) -> Vec<&'a str> {
    vec![
        "--script",
        s(script),
        "--bars",
        s(&f.bars),
        "--universe",
        s(&f.universe),
        "--symbol",
        KEYS[0],
        "--timeframe",
        "3600",
        "--from-ms",
        fr,
        "--to-ms",
        t,
        "--fee-bps",
        "bn:2:5",
        "--lib-dir",
        s(&f.dir),
        "--out",
        s(out),
    ]
}

#[test]
fn pine_cli_run_writes_the_run_directory() {
    let f = fixture("run", 1, &[("t_sma", 18)]);
    let out = f.dir.join("run");
    let (fr, t) = (from(), to());
    let script = f.scripts.join("t_sma.pine");
    let o = pine("run", &run_args(&f, &script, &out, &fr, &t));
    assert!(o.status.success(), "{}", err(&o));
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(stdout.starts_with("trial "), "{stdout}");
    for p in [
        "run.tsv",
        "rows.t1",
        "ret-t1",
        "trials.tsv",
        "hurdle.tsv",
        "faults.log",
    ] {
        assert!(out.join(p).exists(), "{p}");
    }
    let run_tsv = std::fs::read_to_string(out.join("run.tsv")).unwrap();
    assert!(run_tsv.starts_with("schema\tpvm7-run-2\n"), "{run_tsv}");
    assert!(run_tsv.contains("fee_bps\tbn:2:5\n"));
    assert!(run_tsv.contains(&format!("bars\t{}\t3600\t", KEYS[0])));
    let rows = RowsFile::open(&out.join("rows.t1")).unwrap();
    assert_eq!(rows.hdr().tier, 1);
    assert_eq!(rows.recs().len(), 1);
    let id = rows.recs()[0].trial_id;
    let hexid = String::from_utf8_lossy(&pine_screen::ids::hex32(&id)).into_owned();
    assert!(stdout.contains(&hexid));
    // One series per Completed trial, its length the observed days.
    let ret =
        pine_screen::ret::read_ret(&pine_screen::ret::ret_path(&out.join("ret-t1"), &id)).unwrap();
    assert_eq!(ret.len() as u32, rows.recs()[0].summary.obs_days);
    let trials = std::fs::read_to_string(out.join("trials.tsv")).unwrap();
    assert!(trials.starts_with(&hexid), "{trials}");
    assert!(trials.contains("\tcompleted\t"), "{trials}");
    let hurdle = std::fs::read_to_string(out.join("hurdle.tsv")).unwrap();
    assert!(hurdle.ends_with("\tnot_gated\n"), "{hurdle}");
}

#[test]
fn pine_json_envelope_is_pvm7_2() {
    let f = fixture("json", 1, &[("t_sma", 18)]);
    let out = f.dir.join("run");
    let (fr, t) = (from(), to());
    let script = f.scripts.join("t_sma.pine");
    let mut a = run_args(&f, &script, &out, &fr, &t);
    a.push("--json");
    let o = pine("run", &a);
    assert!(o.status.success(), "{}", err(&o));
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        stdout.starts_with("{\"schema\":\"pvm7-2\",\"verb\":\"run\""),
        "{stdout}"
    );
    assert!(!stdout.contains("schema-1"));
}

#[test]
fn pine_cli_fault_exits_4_and_record_faults_continues() {
    let f = fixture("fault", 1, &[("t_sma", 18), ("t_budget", 18)]);
    let out = f.dir.join("run");
    let (fr, t) = (from(), to());
    // A single run of the faulting script: exit 4, the incident printed.
    let script = f.scripts.join("t_budget.pine");
    let o = pine(
        "run",
        &run_args(&f, &script, &f.dir.join("single"), &fr, &t),
    );
    assert_eq!(o.status.code(), Some(4), "{}", err(&o));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(
        e.contains("InsnBudget") && e.contains("bar_index=50") && e.contains("source_sha256="),
        "{e}"
    );
    // A screen without --record-faults: the first fault exits 4.
    let o = pine("screen", &screen_args(&f, &out, &fr, &t));
    assert_eq!(o.status.code(), Some(4), "{}", err(&o));
    // With it: a FAULTED row, the neighbour completed, faults.log carries the record.
    let _ = std::fs::remove_dir_all(&out);
    let mut a = screen_args(&f, &out, &fr, &t);
    a.push("--record-faults");
    let o = pine("screen", &a);
    assert!(o.status.success(), "{}", err(&o));
    let rows = RowsFile::open(&out.join("rows.t1")).unwrap();
    assert_eq!(rows.recs().len(), 2);
    assert_eq!(
        rows.recs()[0].summary.flags & pine_ir::trial_flag::FAULTED,
        0
    );
    assert_ne!(
        rows.recs()[1].summary.flags & pine_ir::trial_flag::FAULTED,
        0
    );
    let faults = std::fs::read_to_string(out.join("faults.log")).unwrap();
    assert_eq!(faults.lines().count(), 1);
    assert!(
        faults.contains("t_budget") && faults.contains("InsnBudget"),
        "{faults}"
    );
    // Only the Completed trial has a series.
    assert_eq!(std::fs::read_dir(out.join("ret-t1")).unwrap().count(), 1);
}

#[test]
fn pine_refuses_a_symbol_without_a_fee_row() {
    let f = fixture("nofee", 1, &[("t_sma", 18)]);
    let out = f.dir.join("run");
    let (fr, t) = (from(), to());
    let mut a = screen_args(&f, &out, &fr, &t);
    let i = a.iter().position(|x| *x == "bn:2:5").unwrap();
    a[i] = "okx:2:5"; // bn.perp is never written: None, never a zero fee
    let o = pine("screen", &a);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(String::from_utf8_lossy(&o.stderr).contains("no --fee-bps row for bn.perp"));
    assert!(!out.exists());
    // A class flag for another class does not cover it either.
    a[i] = "bn.spot:2:5";
    assert_eq!(pine("screen", &a).status.code(), Some(5));
}

#[test]
fn pine_cli_every_combination_dropped_exits_6() {
    let f = fixture("alldrop", 1, &[("t_sma", 18)]);
    let out = f.dir.join("run");
    let (fr, t) = (from(), to());
    let mut a = screen_args(&f, &out, &fr, &t);
    let i = a.iter().position(|x| *x == "bn:2:5").unwrap();
    a[i] = "bn:2:5000";
    let o = pine("screen", &a);
    assert_eq!(o.status.code(), Some(6), "{}", err(&o));
    // `pine hurdle` reads bar files only and says the same.
    let o = pine(
        "hurdle",
        &[
            "--scripts",
            s(&f.scripts),
            "--priors",
            s(&f.priors),
            "--bars",
            s(&f.bars),
            "--universe",
            s(&f.universe),
            "--timeframes",
            "3600",
            "--from-ms",
            &fr,
            "--to-ms",
            &t,
            "--fee-bps",
            "bn:2:5000",
        ],
    );
    assert_eq!(o.status.code(), Some(6), "{}", err(&o));
    assert!(String::from_utf8_lossy(&o.stdout).contains("\tdrop\n"));
}

#[test]
fn pine_hurdle_drop_steps_no_bar() {
    // hold 0 → gross 0 → DROP; hold 18 → clear.
    let f = fixture("drop", 1, &[("t_budget", 0), ("t_sma", 18)]);
    let out = f.dir.join("run");
    let (fr, t) = (from(), to());
    let o = pine("screen", &screen_args(&f, &out, &fr, &t));
    assert!(o.status.success(), "{}", err(&o));
    let trials = std::fs::read_to_string(out.join("trials.tsv")).unwrap();
    let dropped: Vec<&str> = trials
        .lines()
        .filter(|l| l.contains("\tdropped\t"))
        .collect();
    assert_eq!(dropped.len(), 1, "{trials}");
    assert!(
        dropped[0].contains("t_budget")
            && dropped[0].contains("hurdle=")
            && dropped[0].contains("ratio_1e4=0")
    );
    // No RowRec, no series for it; the meter stepped only the cleared script.
    let rows = RowsFile::open(&out.join("rows.t1")).unwrap();
    assert_eq!(rows.recs().len(), 1);
    assert_eq!(std::fs::read_dir(out.join("ret-t1")).unwrap().count(), 1);
    let tp = std::fs::read_to_string(out.join("throughput.tsv")).unwrap();
    let line = tp.lines().find(|l| l.starts_with("bar_close\t")).unwrap();
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(cols[1], "1", "K");
    assert_eq!(cols[2], "1", "one instance stepped: {tp}");
    assert_eq!(
        cols[4],
        (DAYS * 24).to_string(),
        "bar_steps of the one cleared trial"
    );
    let hurdle = std::fs::read_to_string(out.join("hurdle.tsv")).unwrap();
    assert_eq!(hurdle.lines().count(), 2);
}

#[test]
fn pine_screen_threads_do_not_change_the_rows() {
    let f = fixture("threads", 3, &[("t_sma", 18), ("t_budget", 18)]);
    let (fr, t) = (from(), to());
    let mut outs = Vec::new();
    for (n, k) in [("1", "1"), ("3", "2")] {
        let out = f.dir.join(format!("run-{n}"));
        let mut a = screen_args(&f, &out, &fr, &t);
        a.extend(["--threads", n, "--interleave", k, "--record-faults"]);
        let o = pine("screen", &a);
        assert!(o.status.success(), "{}", err(&o));
        outs.push(out);
    }
    let read = |p: &Path| std::fs::read(p).unwrap();
    assert_eq!(
        read(&outs[0].join("rows.t1")),
        read(&outs[1].join("rows.t1"))
    );
    assert_eq!(
        read(&outs[0].join("trials.tsv")),
        read(&outs[1].join("trials.tsv"))
    );
    let names = |p: &Path| {
        let mut v: Vec<_> = std::fs::read_dir(p.join("ret-t1"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    let (a, b) = (names(&outs[0]), names(&outs[1]));
    assert_eq!(a, b);
    assert_eq!(a.len(), 3);
    for n in &a {
        assert_eq!(
            read(&outs[0].join("ret-t1").join(n)),
            read(&outs[1].join("ret-t1").join(n))
        );
    }
    let rows = RowsFile::open(&outs[0].join("rows.t1")).unwrap();
    assert_eq!(rows.recs().len(), 6);
}
