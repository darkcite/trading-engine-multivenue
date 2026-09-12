// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `backtest --member icdp` (Tier 3, statarb doc 08 §6.2): the harness
//! drives the coded icdp member on the WALL clock through the frozen
//! fill law and reports it in the VM's report shape.
//!
//! The capture is one synthetic Binance run whose wall epoch sits on a
//! 15 s bar boundary; the ticks walk icdp through: bar 0 opens on the
//! first sighting → the roll at 15 s makes it the "previous bar" → a
//! +2 bps early return at 19 s clears a 1 bps threshold → a long IoC
//! at ask + 10 bps fills on the next two-sided tick → the roll at 30 s
//! exits at bid − 10 bps → the exit fills ⇒ ONE round trip, all OOS
//! under `0/100`.
//!
//! Offline-path doctrine: this test allocates freely.

use std::path::{Path, PathBuf};

use cli::backtest::member::{run_member, MemberKind, MemberSpec};
use cli::backtest::regime::RegimeMode;
use cli::backtest::BacktestConfig;
use core_io::{PmlrWriter, SlotKind};
use core_types::{Price, Qty, Tick, VenueId};

/// 2023-11-14T22:13:30Z — a multiple of 15 s, so wall bars align.
const EPOCH_NS: u64 = 1_700_000_010_000_000_000;
const BN_BTC: u32 = 7; // `binance:btcusdt`, the M1 anchor sym

const ICDP_TOML: &str = "[icdp]\n\
tf_ms = 15000\n\
delta_ms = 3750\n\
\n\
[[instrument]]\n\
descriptor = \"binance:btcusdt\"\n\
mu = [0, 0, 0, 0, 0]\n\
inv_sd = [1000000000, 1000000000, 1000000000, 1000000000, 1000000000]\n\
w = [1000000000, 0, 0, 0, 0]\n\
b = 0\n\
thr = 1000000000\n\
notional_usd_1e6 = 500000000\n\
spread_cap_1e9 = 5000000000\n\
entry_slip_1e9 = 10000000000\n\
exit_slip_1e9 = 10000000000\n";

fn unique_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("cli-member-{tag}-{}-{nanos}", std::process::id()))
}

fn tick(ts_s: f64, bid_1e6: i64, ask_1e6: i64, seq: u32) -> Tick {
    Tick::new(
        (ts_s * 1e9) as u64,
        VenueId::Binance,
        BN_BTC,
        seq,
        Price::from_raw(bid_1e6),
        Qty::from_raw(10_000_000),
        Price::from_raw(ask_1e6),
        Qty::from_raw(10_000_000),
    )
}

fn build_capture(root: &Path) -> (PathBuf, PathBuf) {
    let run = root.join(format!("run-{EPOCH_NS}"));
    std::fs::create_dir_all(&run).expect("mkdir run");
    std::fs::write(run.join("instrument-manifest.tsv"), "7\tbinance:btcusdt\n").expect("manifest");
    let ticks = [
        tick(0.0, 99_990_000, 100_010_000, 1),
        tick(5.0, 99_990_000, 100_010_000, 2),
        tick(10.0, 99_990_000, 100_010_000, 3),
        tick(15.5, 99_990_000, 100_010_000, 4), // roll → bar 1 opens at 100.00
        tick(19.0, 100_010_000, 100_030_000, 5), // decision: +2 bps early return ⇒ long IoC @ ask+10 bps
        tick(20.0, 100_010_000, 100_030_000, 6), // the IoC fills at the touch
        tick(25.0, 100_010_000, 100_030_000, 7),
        tick(30.5, 100_010_000, 100_030_000, 8), // roll → exit IoC @ bid−10 bps
        tick(31.0, 100_010_000, 100_030_000, 9), // the exit fills
        tick(32.0, 100_010_000, 100_030_000, 10),
    ];
    let path = run.join("bn-ticks.pmlr");
    let mut w = PmlrWriter::open(&path, SlotKind::Tick, EPOCH_NS).expect("open writer");
    for t in &ticks {
        w.append(t).expect("append");
    }
    w.flush().expect("flush");
    let toml = root.join("icdp.toml");
    std::fs::write(&toml, ICDP_TOML).expect("write icdp.toml");
    (root.to_path_buf(), toml)
}

fn member_cfg(replay: &Path, toml: &Path) -> BacktestConfig {
    BacktestConfig {
        ruleset: PathBuf::new(),
        replay_dir: replay.to_path_buf(),
        split: "0/100".to_owned(),
        fee_bps: vec!["bn:0:0".to_owned()],
        latency_ns: Some(0),
        latency_ns_venue: Vec::new(),
        stale_after_ms: vec!["bn:0".to_owned()],
        opt_fee: Vec::new(),
        option_spread_frac_1e6: None,
        emit_detail: None,
        regime: RegimeMode::Off,
        regime_seed: None,
        vrp_seed: None,
        funding_seed: None,
        member: Some(MemberSpec {
            kind: MemberKind::Icdp,
            params: toml.to_path_buf(),
        }),
    }
}

#[test]
fn icdp_member_round_trips_on_the_wall_clock_and_reports_schema1() {
    let root = unique_root("icdp");
    let (replay, toml) = build_capture(&root);
    let cfg = member_cfg(&replay, &toml);
    let out = run_member(&cfg, cfg.member.as_ref().unwrap()).expect("member run");
    // Identity = the parameter file's sha256.
    let hash = core_crypto::sha256(ICDP_TOML.as_bytes());
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        out.schema1.starts_with(&format!("{{\"schema_version\":1,\"ruleset_hash\":\"{hex}\",\"split\":\"0/100\",")),
        "schema1: {}",
        out.schema1
    );
    // The member walked the whole path: a decision, a signal, an intent, an exit.
    assert!(out.summary.contains("member: icdp params="), "{}", out.summary);
    // Bar 0's decision has no previous bar (skipped_prev); bar 1's fires.
    assert!(
        out.summary.contains(" decisions=2 signals=1 intents=1 exits=1 "),
        "{}",
        out.summary
    );
    assert!(out.summary.contains(" skipped_prev=1 "), "{}", out.summary);
    assert!(out.summary.contains("clock=wall"), "{}", out.summary);
    // Two IoC fills (entry + exit) ⇒ one round trip, all OOS.
    assert_eq!(out.stats.fills_total, 2, "{}", out.summary);
    assert_eq!(out.stats.ioc_fills, 2, "{}", out.summary);
    assert_eq!(out.stats.oos_round_trips, 1, "{}", out.summary);
    assert_eq!(out.stats.vm_orders_emitted, 2, "{}", out.summary);
    assert!(out.schema1.contains("\"round_trips\":1,\"legs\":2}"), "{}", out.schema1);
    assert!(out.schema1.contains("\"position_rows\":1}"), "{}", out.schema1);
    // The entry paid the ask (100.03) and the exit hit the bid (100.01) on
    // ~5 units ($500 / 100.04 = 4.998): realized ≈ −$0.0999 at zero fee.
    let key = "\"net_pnl_usd\":";
    let i = out.schema1.find(key).expect("net field") + key.len();
    let j = out.schema1[i..].find(',').expect("comma") + i;
    let net: f64 = out.schema1[i..j].parse().expect("json number");
    assert!((-0.101..=-0.099).contains(&net), "net {net}: {}", out.schema1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn member_detail_sidecar_is_written_in_the_shared_shape() {
    let root = unique_root("icdp-detail");
    let (replay, toml) = build_capture(&root);
    let mut cfg = member_cfg(&replay, &toml);
    let detail = root.join("detail.json");
    cfg.emit_detail = Some(detail.clone());
    run_member(&cfg, cfg.member.as_ref().unwrap()).expect("member run");
    let d = std::fs::read_to_string(&detail).expect("sidecar written");
    assert!(d.starts_with("{\"detail_version\":7,"), "{d}");
    assert!(d.contains("\"fee_classes\":{"), "{d}");
    assert!(d.contains("\"fills\":{\"total\":2,"), "{d}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unresolvable_instrument_refuses_the_run() {
    let root = unique_root("icdp-unresolved");
    let (replay, toml) = build_capture(&root);
    std::fs::write(&toml, ICDP_TOML.replace("binance:btcusdt", "binance:nosuchusdt")).unwrap();
    let cfg = member_cfg(&replay, &toml);
    let msg = match run_member(&cfg, cfg.member.as_ref().unwrap()) {
        Ok(_) => panic!("an unresolvable instrument must refuse the run"),
        Err(e) => format!("{e}"),
    };
    assert!(msg.contains("not in the capture's manifest"), "{msg}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The bin grammar: `--member` without `--ruleset` is accepted, an
/// unknown member is refused before anything runs, and `--ruleset` with
/// `--member` conflicts (clap). The frozen argv shape is untouched.
#[test]
fn real_binary_member_grammar() {
    let root = unique_root("icdp-bin");
    let (replay, toml) = build_capture(&root);
    let bin = env!("CARGO_BIN_EXE_multivenue-engine");
    let ok = std::process::Command::new(bin)
        .args([
            "backtest",
            "--member",
            "icdp",
            "--icdp",
            toml.to_str().unwrap(),
            "--replay-dir",
            replay.to_str().unwrap(),
            "--split",
            "0/100",
            "--fee-bps",
            "bn:0:0",
            "--latency-ns",
            "0",
            "--stale-after-ms",
            "bn:0",
            "--regime",
            "off",
        ])
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&ok.stdout);
    let stderr = String::from_utf8_lossy(&ok.stderr);
    assert!(ok.status.success(), "stdout={stdout} stderr={stderr}");
    assert!(stdout.starts_with("{\"schema_version\":1,"), "{stdout}");
    assert!(stderr.contains("member: icdp"), "{stderr}");
    let bogus = std::process::Command::new(bin)
        .args([
            "backtest",
            "--member",
            "bogus",
            "--replay-dir",
            replay.to_str().unwrap(),
            "--split",
            "0/100",
        ])
        .output()
        .expect("spawn");
    assert!(!bogus.status.success());
    assert!(String::from_utf8_lossy(&bogus.stderr).contains("unknown --member"));
    assert!(bogus.stdout.is_empty());
    let conflict = std::process::Command::new(bin)
        .args([
            "backtest",
            "--member",
            "icdp",
            "--ruleset",
            "x.json",
            "--replay-dir",
            replay.to_str().unwrap(),
            "--split",
            "0/100",
        ])
        .output()
        .expect("spawn");
    assert!(!conflict.status.success());
    assert!(conflict.stdout.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}
