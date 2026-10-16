// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.11 — `multivenue-engine pine handoff | report`: the pre-registered selection (I24),
//! the ≤ 2 h windows over the v3 tape (I23), the manifest round trip (I25 — through a minimal
//! local parser until impl-08's `core_config::pine::parse_manifest` lands,
//! pin:07-manifest-parser-impl08), the cost, and the report's deliverables with tier 2 pending,
//! joined, and refused (I26).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use cli::pine::handoff::{parse_listing, plan_tape, tape, tape_bars_target, ListingStats, TapeRun};
use pine_ir::{barflag, trial_flag, Bar};
use pine_screen::bar_path;
use pine_screen::bars::{bar_source, write_bar_file};
use pine_screen::handoff::{read_handoff, MAX_WINDOW_NS};
use pine_screen::ids::{fee_digest, hex32};
use pine_screen::rows::{RowsFile, RowsWriter};
use pine_screen::{RowRec, RowsHdr};

const BIN: &str = env!("CARGO_BIN_EXE_multivenue-engine");
/// 2026-01-01T00:00:00Z.
const T0: i64 = 1_767_225_600_000;
const H: i64 = 3_600_000;
const D: i64 = 86_400_000;
const DAYS: i64 = 60;
const KEY: &str = "binance_usdm:btcusdt";
const NOTIONAL: i64 = 1_000_000_000;

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

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("pine_handoff_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn hex(b: &[u8; 32]) -> String {
    String::from_utf8_lossy(&hex32(b)).into_owned()
}

fn write_bars(root: &Path) {
    let n = (DAYS * 24) as usize;
    let mut rows = Vec::with_capacity(n);
    let mut prev = 0i64;
    for i in 0..n {
        let t = i as f64;
        let c = 100.0
            + 8.0 * (2.0 * std::f64::consts::PI * t / 120.0).sin()
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
    let p = bar_path(root, KEY, 3_600).unwrap();
    write_bar_file(
        &p,
        &rows,
        3_600,
        pine_ir::symbol_hash(KEY.as_bytes()),
        1,
        bar_source::CANDLES,
        false,
    )
    .unwrap();
}

struct Fx {
    dir: PathBuf,
    run: PathBuf,
}

fn pine(verb: &str, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("pine")
        .arg(verb)
        .args(args)
        .output()
        .unwrap()
}

fn err(o: &Output) -> String {
    format!(
        "status {:?}\nstdout {}\nstderr {}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// One symbol, hourly bars, `t_sma` (prior 10 trades / 1k bars) screened at 1 h.
fn screened(name: &str) -> Fx {
    let dir = tmp(name);
    let bars = dir.join("bars");
    write_bars(&bars);
    let universe = dir.join("screen-universe.tsv");
    std::fs::write(
        &universe,
        format!("# test\n{KEY}\tbinance-usdm:btcusdt\t-\t1\tperp\tbn\t10000\t1000000\t1000\t5000000\tBTCUSDT\tBTC\tUSDT\tcrypto\n"),
    )
    .unwrap();
    let scripts = dir.join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join("t_sma.pine"), SMA_CROSS).unwrap();
    let priors = dir.join("priors.tsv");
    std::fs::write(&priors, "# why\nt_sma\t18\t10\tta.sma\n").unwrap();
    let run = dir.join("screen");
    let (fr, to) = ((T0 + 10 * D).to_string(), (T0 + DAYS * D).to_string());
    let o = pine(
        "screen",
        &[
            "--scripts",
            s(&scripts),
            "--priors",
            s(&priors),
            "--bars",
            s(&bars),
            "--universe",
            s(&universe),
            "--timeframes",
            "3600",
            "--from-ms",
            &fr,
            "--to-ms",
            &to,
            "--fee-bps",
            "bn:2:5",
            "--out",
            s(&run),
            "--lib-dir",
            s(&dir),
        ],
    );
    assert!(o.status.success(), "{}", err(&o));
    let trials = std::fs::read_to_string(run.join("trials.tsv")).unwrap();
    assert!(trials.contains("\tclear\tcompleted\t"), "{trials}");
    Fx { dir, run }
}

/// A JSON line of the recon's shape (own-clock span).
fn row(epoch_ms: i64, dur_ns: i64, ver: u32, local: bool, size: u64) -> String {
    let e = epoch_ms * 1_000_000;
    let a = 2_700_000_000_000_000i64;
    format!(
        "{{\"epoch_ns\":{e},\"host_id\":\"mbp-m4\",\"local_path\":{},\"location\":\"s3\",\"pmlr_version\":{ver},\"run\":\"run-{e}\",\"size_bytes\":{size},\"span_ns\":[{a},{}],\"stored_bytes\":1,\"uploaded_at_ns\":1,\"venues\":[\"bn\"],\"windows_2h_complete\":11}}\n",
        if local { "\"/cache/x\"" } else { "null" },
        a + dur_ns
    )
}

const DUR23: i64 = 23 * 3_600_000_000_000;

/// Three v3 runs of 23 h at T0+50/51/52 d (the middle one in the archive cache), one v2 row.
fn listing(dir: &Path) -> PathBuf {
    let mut t = String::new();
    t.push_str(&row(T0 + 40 * D, DUR23, 2, false, 5));
    t.push_str(&row(T0 + 50 * D, DUR23, 3, false, 1_000));
    t.push('\n');
    t.push_str(&row(T0 + 51 * D, DUR23, 3, true, 2_000));
    t.push_str(&row(T0 + 52 * D, DUR23, 3, false, 3_000));
    let p = dir.join("archive-list.json");
    std::fs::write(&p, t).unwrap();
    p
}

fn handoff(fx: &Fx, run: &Path, listing: &Path, extra: &[&str]) -> Output {
    let mut a = vec![
        "--run",
        s(run),
        "--symbol",
        KEY,
        "--select",
        "lowest-clearing-tf",
        "--listing",
        s(listing),
    ];
    a.extend_from_slice(extra);
    let _ = &fx.dir;
    pine("handoff", &a)
}

fn handoff_dirs(run: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(run.join("handoff"))
        .map(|r| r.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn pine_handoff_prints_the_cost() {
    let fx = screened("cost");
    let l = listing(&fx.dir);
    let o = handoff(&fx, &fx.run, &l, &[]);
    assert!(o.status.success(), "{}", err(&o));
    let out = String::from_utf8_lossy(&o.stdout);
    // 3 runs × 23 h = 3 × 12 windows; one run local (archive cache) → 2 to pull; bytes = sizes.
    assert!(
        out.contains("\twindows=36\truns=3\truns_to_pull=2\test_bytes=6000\t"),
        "{out}"
    );
    assert!(
        out.contains(
            "rows=4\tnot_v3=1\tspan_refused=0\tempty=0\tspan_epoch_scale=0\tspan_own_clock=3"
        ),
        "{out}"
    );
    assert!(
        out.contains("tape_bars_target=3000\twindows_needed=1500"),
        "{out}"
    );
    assert!(out.contains("25 GiB pull floor"), "{out}");
    let dirs = handoff_dirs(&fx.run);
    assert_eq!(dirs.len(), 1);
    let h = read_handoff(&dirs[0]).unwrap();
    assert_eq!(h.windows.len(), 36);
    assert_eq!((h.score_from_ms, h.score_to_ms), (T0 + 50 * D, T0 + 52 * D));
    assert_eq!(h.warmup_to_ms, T0 + 50 * D);
    assert_eq!(h.rows_out, fx.run.join("rows.t2"));
    for w in &h.windows {
        assert!(w.off_to_ns - w.off_from_ns <= MAX_WINDOW_NS);
    }
    assert_eq!(h.windows.iter().filter(|w| w.local).count(), 12);
    for f in [
        "cutplan.tsv",
        "b3.argv",
        "manifest.toml",
        "t_sma.pine",
        "cost.tsv",
    ] {
        assert!(dirs[0].join(f).is_file(), "{f}");
    }
    let argv = std::fs::read_to_string(dirs[0].join("b3.argv")).unwrap();
    assert!(argv.starts_with("backtest\n--strategy\npine\n"), "{argv}");
    // JSON.
    let o = handoff(&fx, &fx.run, &l, &["--json"]);
    assert!(o.status.success(), "{}", err(&o));
    let j = String::from_utf8_lossy(&o.stdout);
    assert!(
        j.starts_with("{\"schema\":\"pvm7-2\",\"verb\":\"handoff\""),
        "{j}"
    );
    assert!(
        j.contains("\"windows\":36,\"runs\":3,\"runs_to_pull\":2,\"est_bytes\":6000"),
        "{j}"
    );
}

/// A minimal TOML reader for the manifest's own subset: `[[script]]`, `[script.inputs]`,
/// `[script.symbols]`, `key = <int | "basic string">` (pin:07-manifest-parser-impl08).
fn parse_manifest_min(t: &str) -> Vec<(String, String, String)> {
    let mut table = String::new();
    let mut out = Vec::new();
    for raw in t.lines() {
        let l = raw.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.starts_with('[') {
            assert!(
                ["[[script]]", "[script.inputs]", "[script.symbols]"].contains(&l),
                "table {l}"
            );
            table = l.to_owned();
            continue;
        }
        let (k, v) = l.split_once('=').expect("key = value");
        let (k, v) = (k.trim(), v.trim());
        let ok_int = v.parse::<i64>().is_ok();
        let ok_str = v.len() >= 2 && v.starts_with('"') && v.ends_with('"');
        assert!(
            ok_int || ok_str,
            "{k} = {v}: integer TOML or a basic string"
        );
        out.push((table.clone(), k.to_owned(), v.trim_matches('"').to_owned()));
    }
    out
}

#[test]
fn pine_handoff_manifest_round_trips() {
    let fx = screened("manifest");
    let l = listing(&fx.dir);
    let o = handoff(&fx, &fx.run, &l, &[]);
    assert!(o.status.success(), "{}", err(&o));
    let d = &handoff_dirs(&fx.run)[0];
    let h = read_handoff(d).unwrap();
    let kv = parse_manifest_min(&std::fs::read_to_string(d.join("manifest.toml")).unwrap());
    let get = |k: &str| {
        kv.iter()
            .find(|x| x.0 == "[[script]]" && x.1 == k)
            .unwrap_or_else(|| panic!("no {k}"))
            .2
            .clone()
    };
    for k in [
        "sha256",
        "file",
        "dialect",
        "enabled",
        "mode",
        "symbol",
        "timeframe_s",
        "venue",
        "max_bars_back",
        "coalesce_ms",
        "coalesce_bps",
        "insn_budget",
        "qty_notional_1e6",
        "limit_primitive",
        "cross_ticks",
        "cap_instance_1e6",
        "cap_day_1e6",
        "max_open",
        "collection_cap",
        "str_persist_bytes",
        "str_cell_bytes",
        "str_scratch_bytes",
        "intent_cap",
    ] {
        let _ = get(k);
    }
    let script = std::fs::read(d.join(get("file"))).unwrap();
    assert_eq!(get("sha256"), hex(&core_crypto::sha256(&script)));
    assert_eq!(script, SMA_CROSS.as_bytes());
    assert_eq!(get("dialect"), "6");
    assert_eq!(get("enabled"), "1");
    assert_eq!(get("mode"), "bar_close");
    assert_eq!(get("symbol"), KEY);
    assert_eq!(get("timeframe_s"), "3600");
    assert_eq!(get("venue"), "binance"); // pin:07-manifest-venue
    assert_eq!(get("qty_notional_1e6"), NOTIONAL.to_string());
    assert_eq!(get("limit_primitive"), "ioc");
    assert_eq!(get("cross_ticks"), "2");
    assert_eq!(get("max_open"), "8");
    let cap_i: i64 = get("cap_instance_1e6").parse().unwrap();
    let cap_d: i64 = get("cap_day_1e6").parse().unwrap();
    assert!(cap_i >= NOTIONAL && cap_d >= 4 * NOTIONAL);
    // The manifest names the hand-off's chart and program.
    assert_eq!(h.manifest, "manifest.toml");
    assert_eq!(h.symbol_key, KEY);
    // No overrides in a screen: the two sub-tables are present and empty.
    assert!(kv.iter().all(|x| x.0 == "[[script]]"));
}

#[test]
fn pine_handoff_selection_reads_no_pnl_column() {
    let fx = screened("select");
    let l = listing(&fx.dir);
    // A second run directory: the same screen with every *_bps_1e4 field of rows.t1 scrambled.
    let b = fx.dir.join("screen-b");
    std::fs::create_dir_all(&b).unwrap();
    for e in std::fs::read_dir(&fx.run).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            std::fs::copy(&p, b.join(p.file_name().unwrap())).unwrap();
        }
    }
    let rows = RowsFile::open(&b.join("rows.t1")).unwrap();
    let hdr = *rows.hdr();
    let mut recs: Vec<RowRec> = rows.recs().to_vec();
    for (i, r) in recs.iter_mut().enumerate() {
        r.summary.net_bps_1e4 = -7_777_777 - i as i64;
        r.summary.gross_bps_1e4 = 123_456;
        r.summary.fee_bps_1e4 = -999;
    }
    drop(rows);
    std::fs::remove_file(b.join("rows.t1")).unwrap();
    let mut w = RowsWriter::open(&b.join("rows.t1"), &hdr).unwrap();
    for r in &recs {
        w.append(r).unwrap();
    }
    w.finish().unwrap();
    for run in [&fx.run, &b] {
        let o = handoff(&fx, run, &l, &[]);
        assert!(o.status.success(), "{}", err(&o));
    }
    let (da, db) = (handoff_dirs(&fx.run), handoff_dirs(&b));
    assert_eq!(da.len(), 1);
    assert_eq!(
        da.iter()
            .map(|p| p.file_name().unwrap().to_owned())
            .collect::<Vec<_>>(),
        db.iter()
            .map(|p| p.file_name().unwrap().to_owned())
            .collect::<Vec<_>>()
    );
    let (ha, hb) = (read_handoff(&da[0]).unwrap(), read_handoff(&db[0]).unwrap());
    assert_eq!(ha.t1_trial_id, hb.t1_trial_id);
    assert_eq!(ha.t2_trial_id, hb.t2_trial_id);
    assert_eq!(ha.timeframe_s, hb.timeframe_s);
    for f in ["windows.tsv", "cutplan.tsv", "manifest.toml"] {
        assert_eq!(
            std::fs::read(da[0].join(f)).unwrap(),
            std::fs::read(db[0].join(f)).unwrap(),
            "{f}"
        );
    }
}

#[test]
fn pine_handoff_refuses_without_a_v3_window_exit_7() {
    let fx = screened("nov3");
    // Only a v2 row.
    let p = fx.dir.join("v2.json");
    std::fs::write(&p, row(T0 + 50 * D, DUR23, 2, false, 1)).unwrap();
    let o = handoff(&fx, &fx.run, &p, &[]);
    assert_eq!(o.status.code(), Some(7), "{}", err(&o));
    assert!(handoff_dirs(&fx.run).is_empty());
    // v3, but under two whole UTC days.
    let p = fx.dir.join("short.json");
    std::fs::write(&p, row(T0 + 50 * D, DUR23, 3, false, 1)).unwrap();
    let o = handoff(&fx, &fx.run, &p, &[]);
    assert_eq!(o.status.code(), Some(7), "{}", err(&o));
    assert!(handoff_dirs(&fx.run).is_empty());
}

#[test]
fn pine_handoff_argv_and_data_refusals() {
    let fx = screened("argv");
    let l = listing(&fx.dir);
    // --select other than lowest-clearing-tf: exit 2, nothing written (I24).
    let o = pine(
        "handoff",
        &[
            "--run",
            s(&fx.run),
            "--symbol",
            KEY,
            "--select",
            "best-net",
            "--listing",
            s(&l),
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    // No tape source at all: exit 2.
    let o = pine(
        "handoff",
        &[
            "--run",
            s(&fx.run),
            "--symbol",
            KEY,
            "--select",
            "lowest-clearing-tf",
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(handoff_dirs(&fx.run).is_empty());
    // Overlapping runs: the harness's own rule, exit 5.
    let p = fx.dir.join("overlap.json");
    let mut t = row(T0 + 50 * D, DUR23, 3, false, 1);
    t.push_str(&row(T0 + 50 * D + 22 * H, DUR23, 3, false, 1));
    std::fs::write(&p, t).unwrap();
    let o = handoff(&fx, &fx.run, &p, &[]);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    // A symbol that was not screened: no survivor, exit 7.
    let o = pine(
        "handoff",
        &[
            "--run",
            s(&fx.run),
            "--symbol",
            "binance_usdm:ethusdt",
            "--select",
            "lowest-clearing-tf",
            "--listing",
            s(&l),
        ],
    );
    assert_eq!(o.status.code(), Some(7), "{}", err(&o));
    // Not JSON lines: exit 5.
    let p = fx.dir.join("array.json");
    std::fs::write(&p, "[{\"run\":\"run-1\"}]\n").unwrap();
    let o = handoff(&fx, &fx.run, &p, &[]);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(handoff_dirs(&fx.run).is_empty());
}

#[test]
fn pine_handoff_tape_walk_and_listing_fields() {
    // The recon row's shape: own-clock span, only its difference read.
    let l = r#"{"epoch_ns":1787961633277915000,"host_id":"mbp-m4","local_path":null,"location":"s3","pmlr_version":3,"run":"run-1787961633277915000","size_bytes":123,"span_ns":[2700000000000000,2700007200000000],"stored_bytes":7,"uploaded_at_ns":1,"venues":[],"windows_2h_complete":1}"#;
    let mut st = ListingStats::default();
    let r = parse_listing(l, &mut st).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].dur_ns, 7_200_000_000);
    assert_eq!((r[0].local, r[0].bytes), (false, Some(123)));
    assert_eq!(st.span_own_clock, 1);
    // A reversed span is refused, never windowed; a run id not matching its epoch refuses.
    let bad = l.replace("[2700000000000000,2700007200000000]", "[5,4]");
    let mut st = ListingStats::default();
    assert!(parse_listing(&bad, &mut st).unwrap().is_empty());
    assert_eq!(st.span_refused, 1);
    let bad = l.replace("\"run-1787961633277915000\"", "\"run-1\"");
    assert!(parse_listing(&bad, &mut ListingStats::default()).is_err());
    // §4.8.3's arithmetic.
    assert_eq!(tape_bars_target(165), 182);
    assert_eq!(tape_bars_target(55), 546);
    assert_eq!(tape_bars_target(15), 2_000);
    assert_eq!(
        cli::pine::handoff::manifest_venue("okx_perp:btc-usdt-swap"),
        "okx"
    );
    // The walk stops at the newest runs once the target AND two whole days hold.
    let run = |d: i64| TapeRun {
        run: format!("run-{}", (T0 + d * D) * 1_000_000),
        epoch_ns: (T0 + d * D) * 1_000_000,
        dur_ns: DUR23,
        local: false,
        bytes: None,
    };
    let runs = tape(
        vec![run(1), run(2), run(3), run(4), run(5)],
        None,
        &mut ListingStats::default(),
    )
    .unwrap();
    // 182 1-m bars need ~3 h of tape; the two-day condition takes three runs.
    let p = plan_tape(&runs, 182, 60).unwrap();
    assert_eq!(p.runs.len(), 3);
    assert_eq!((p.score_from_ms, p.score_to_ms), (T0 + 3 * D, T0 + 5 * D));
    let (pull, bytes) = cli::pine::handoff::cost_of(&p);
    assert_eq!(pull, 3);
    assert_eq!(bytes, 36 * cli::pine::handoff::WINDOW_BYTES_FALLBACK);
    // Chains: one per run, contiguous inside it.
    assert_eq!(p.windows.first().unwrap().chain, 0);
    assert_eq!(p.windows.last().unwrap().chain, 2);
}

// ---------------------------------------------------------------------------------------------
// pine report
// ---------------------------------------------------------------------------------------------

fn handed_off(name: &str) -> Fx {
    let fx = screened(name);
    let l = listing(&fx.dir);
    let o = handoff(&fx, &fx.run, &l, &[]);
    assert!(o.status.success(), "{}", err(&o));
    fx
}

fn report(run: &Path, extra: &[&str]) -> Output {
    let mut a = vec!["--run", s(run)];
    a.extend_from_slice(extra);
    pine("report", &a)
}

#[test]
fn pine_report_t2_pending_writes_the_three_deliverables() {
    let fx = handed_off("rep_pending");
    let o = report(&fx.run, &["--text"]);
    assert!(o.status.success(), "{}", err(&o));
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("PVM-7 tier-1 -> tier-2 delta"), "{out}");
    assert!(out.contains("T2_PENDING"), "{out}");
    assert!(out.contains("NO RANKING IS PRINTED."), "{out}");
    for f in [
        "report/delta.json",
        "report/delta.txt",
        "report/wiring.json",
        "report/throughput.json",
        "rows.t1shadow",
    ] {
        assert!(fx.run.join(f).is_file(), "{f}");
    }
    let h = read_handoff(&handoff_dirs(&fx.run)[0]).unwrap();
    let shadow = RowsFile::open(&fx.run.join("rows.t1shadow")).unwrap();
    assert_eq!(shadow.hdr().tier, 1);
    assert_eq!(shadow.recs().len(), 1);
    assert_eq!(shadow.recs()[0].trial_id, h.t2_trial_id);
    assert!(pine_screen::ret::ret_path(&fx.run.join("ret-t1shadow"), &h.t2_trial_id).is_file());
    let dj = std::fs::read_to_string(fx.run.join("report/delta.json")).unwrap();
    assert!(dj.starts_with("{\"schema\":\"pvm7-2\""), "{dj}");
    assert!(dj.contains("\"t2\":null"), "{dj}");
    assert!(
        dj.contains("\"t2_net_bps_1e4\":null,\"d_net_bps_1e4\":null"),
        "{dj}"
    );
    assert!(dj.contains("\"flags\":[\"T2_PENDING\"]"), "{dj}");
    assert!(dj.contains("\"ranking\":null"), "{dj}");
    let wj = std::fs::read_to_string(fx.run.join("report/wiring.json")).unwrap();
    for e in [
        "source_is_ours",
        "bars_prepared",
        "hurdle",
        "tier1",
        "handoff",
        "tier2",
        "report",
    ] {
        assert!(wj.contains(&format!("\"edge\":\"{e}\"")), "{e}: {wj}");
    }
    assert!(wj.contains("rows.t2 absent"), "{wj}");
    let tj = std::fs::read_to_string(fx.run.join("report/throughput.json")).unwrap();
    assert!(tj.contains("\"deliverable\":\"throughput\""), "{tj}");
}

/// A tier-2 rows file under the run's notional and fee set with B'3-shaped rows.
fn write_t2(path: &Path, hdr: RowsHdr, recs: &[RowRec]) {
    let _ = std::fs::remove_file(path);
    let mut w = RowsWriter::open(path, &hdr).unwrap();
    for r in recs {
        w.append(r).unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn pine_report_joins_b3_rows() {
    let fx = handed_off("rep_join");
    let o = report(&fx.run, &[]);
    assert!(o.status.success(), "{}", err(&o));
    let h = read_handoff(&handoff_dirs(&fx.run)[0]).unwrap();
    let t1 = RowsFile::open(&fx.run.join("rows.t1shadow"))
        .unwrap()
        .recs()[0];
    let mut t2 = t1;
    t2.trial_id = h.t2_trial_id;
    t2.summary.closed = 25;
    t2.summary.net_bps_1e4 = t1.summary.net_bps_1e4 - 50_000;
    t2.summary.flags = trial_flag::FIXED_NOTIONAL;
    let mut null = t2;
    null.trial_id = h.null_trial_id;
    null.summary.net_bps_1e4 = 1_234;
    let hdr = RowsHdr::new(2, NOTIONAL, fee_digest(&["bn:2:5"]));
    write_t2(&fx.run.join("rows.t2"), hdr, &[t2, null]);
    let o = report(&fx.run, &[]);
    assert!(o.status.success(), "{}", err(&o));
    let dj = std::fs::read_to_string(fx.run.join("report/delta.json")).unwrap();
    assert!(dj.contains("\"d_net_bps_1e4\":-50000"), "{dj}");
    assert!(dj.contains("\"null_net_bps_1e4\":1234"), "{dj}");
    assert!(!dj.contains("T2_PENDING"), "{dj}");
    let wj = std::fs::read_to_string(fx.run.join("report/wiring.json")).unwrap();
    assert!(wj.contains("rows.t2 present, 1 of 1 rows found"), "{wj}");
    assert!(wj.contains("1 shadow rows, 1 joined, 0 orphans"), "{wj}");
}

#[test]
fn pine_report_refuses_rows_of_another_notional_or_fee_set() {
    let fx = handed_off("rep_refuse");
    let digest = fee_digest(&["bn:2:5"]);
    for hdr in [
        RowsHdr::new(2, 2 * NOTIONAL, digest),
        RowsHdr::new(2, NOTIONAL, fee_digest(&["bn:2:6"])),
        RowsHdr::new(1, NOTIONAL, digest),
    ] {
        let p = fx.run.join("rows.t2");
        write_t2(&p, hdr, &[]);
        let o = report(&fx.run, &[]);
        assert_eq!(o.status.code(), Some(5), "{}", err(&o));
        assert!(String::from_utf8_lossy(&o.stderr).contains("not comparable"));
    }
    // --trade-bars naming the candles root is an argv refusal.
    let _ = std::fs::remove_file(fx.run.join("rows.t2"));
    let o = report(&fx.run, &["--trade-bars", s(&fx.dir.join("bars"))]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
}

#[test]
fn pine_report_has_no_sort_flag() {
    let o = pine("report", &["--help"]);
    assert!(o.status.success(), "{}", err(&o));
    let help = String::from_utf8_lossy(&o.stdout).to_lowercase();
    for w in ["--sort", "--order", "--rank", "--top", "--best"] {
        assert!(!help.contains(w), "{w}: {help}");
    }
    let o = pine("report", &["--run", "/nonexistent", "--sort", "net"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    // No hand-off: exit 7.
    let fx = screened("rep_none");
    let o = report(&fx.run, &[]);
    assert_eq!(o.status.code(), Some(7), "{}", err(&o));
}

/// A local `run-<epoch_ns>` directory with a v3 tick file spanning `dur_ns` of its own clock.
fn local_run(root: &Path, epoch_ms: i64, dur_ns: i64) {
    let epoch_ns = (epoch_ms * 1_000_000) as u64;
    let run = root.join(format!("run-{epoch_ns}"));
    std::fs::create_dir_all(&run).unwrap();
    let mut w =
        core_io::PmlrWriter::open(run.join("bn-ticks.pmlr"), core_io::SlotKind::Tick, epoch_ns)
            .unwrap();
    let base = 9_000_000_000u64;
    for k in 0..=4u64 {
        let mut t = core_types::Tick::ZERO;
        t.ts_ns = base + k * (dur_ns as u64) / 4;
        t.sym = 1;
        t.venue = 1;
        w.append(&t).unwrap();
    }
    w.flush().unwrap();
}

#[test]
fn pine_handoff_reads_local_runs_through_the_capture_catalog() {
    let fx = screened("local");
    let logs = fx.dir.join("logs");
    local_run(&logs, T0 + 50 * D, DUR23);
    local_run(&logs, T0 + 51 * D, DUR23);
    // The archive lists 51 d (not cached) and 52 d: the local copy of 51 d wins.
    let p = fx.dir.join("list.json");
    let mut t = row(T0 + 51 * D, DUR23, 3, false, 2_000);
    t.push_str(&row(T0 + 52 * D, DUR23, 3, false, 3_000));
    std::fs::write(&p, t).unwrap();
    let o = handoff(&fx, &fx.run, &p, &["--local-root", s(&logs)]);
    assert!(o.status.success(), "{}", err(&o));
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("local_v3=2\tv3_runs=3"), "{out}");
    assert!(
        out.contains("\twindows=36\truns=3\truns_to_pull=1\t"),
        "{out}"
    );
    let h = read_handoff(&handoff_dirs(&fx.run)[0]).unwrap();
    assert_eq!(h.windows.iter().filter(|w| w.local).count(), 24);
    // The local root alone (no listing) is a tape source too: two runs, under two score days.
    let o = pine(
        "handoff",
        &[
            "--run",
            s(&fx.run),
            "--symbol",
            KEY,
            "--select",
            "lowest-clearing-tf",
            "--local-root",
            s(&logs),
        ],
    );
    assert_eq!(o.status.code(), Some(7), "{}", err(&o));
}
