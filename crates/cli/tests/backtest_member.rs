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
        funding_seed: None,
        member: Some(MemberSpec {
            kind: MemberKind::Icdp,
            params: toml.to_path_buf(),
            table: None,
            seed: None,
            vrp_seed: None,
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

// ---------------------------------------------------------------
// XSD-3: `--member xsd` on the same arm.
// ---------------------------------------------------------------

const XSD_A: u32 = (1 << 24) | 512; // binance-usdm:aaausdt
const XSD_B: u32 = (1 << 24) | 513; // binance-usdm:bbbusdt
const HOUR_S: u64 = 3_600;
/// The wall hour the capture starts in (`EPOCH_NS` is 810 s into it).
const XSD_H0: u64 = EPOCH_NS / 1_000_000_000 / HOUR_S;

const XSD_TOML: &str = "[xsd]\n\
z_window_h = 96\n\
z_enter_1e9 = 3000000000\n\
z_exit_1e9 = 0\n\
z_stop_1e9 = 1000000000000\n\
consensus = 1\n\
grid_n = 1\n\
grid_step_1e9 = 500000000\n\
max_hold_s = 864000\n\
cooldown_s = 3600\n\
ttl_s = 300\n\
position_usd_1e6 = 1000000000\n\
max_positions = 82\n\
max_gross_usd_1e6 = 100000000000\n\
direction = 1\n";

fn xsd_tick(sym: u32, wall_s: u64, mid_1e6: i64, seq: u32) -> Tick {
    // Payload ts = mono; the harness maps the first record to EPOCH_NS.
    let ts_ns = (wall_s - EPOCH_NS / 1_000_000_000) * 1_000_000_000;
    Tick::new(
        ts_ns,
        VenueId::Binance,
        sym,
        seq,
        Price::from_raw(mid_1e6 - 10_000),
        Qty::from_raw(10_000_000),
        Price::from_raw(mid_1e6 + 10_000),
        Qty::from_raw(10_000_000),
    )
}

/// Two perps over four wall hours: A dislocates +5 % during hour H0 (a
/// long entry decided at the H0+1 boundary, priced by A's first tick of
/// that hour), collapses during H0+1 (a revert exit at H0+2). The seed
/// carries 95 wobbling hours so the window is exactly 96 deep at the
/// first roll.
fn build_xsd_capture(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let run = root.join(format!("run-{EPOCH_NS}"));
    std::fs::create_dir_all(&run).expect("mkdir run");
    std::fs::write(
        run.join("instrument-manifest.tsv"),
        format!("{XSD_A}\tbinance-usdm:aaausdt\n{XSD_B}\tbinance-usdm:bbbusdt\n"),
    )
    .expect("manifest");
    let h = |k: u64| (XSD_H0 + k) * HOUR_S;
    let t0 = EPOCH_NS / 1_000_000_000;
    let ticks = [
        xsd_tick(XSD_A, t0, 100_000_000, 1),
        xsd_tick(XSD_B, t0 + 1, 100_000_000, 2),
        xsd_tick(XSD_A, h(0) + 3_500, 105_000_000, 3), // H0 closes: A 105, B 100
        xsd_tick(XSD_B, h(0) + 3_501, 100_000_000, 4),
        xsd_tick(XSD_A, h(1) + 5, 105_100_000, 5), // roll → ENTER; priced at this ask
        xsd_tick(XSD_A, h(1) + 10, 105_100_000, 6), // the IoC fills at the touch
        xsd_tick(XSD_B, h(1) + 11, 100_000_000, 7),
        xsd_tick(XSD_A, h(1) + 3_500, 100_000_000, 8), // H0+1 closes: A back at 100
        xsd_tick(XSD_B, h(1) + 3_501, 100_000_000, 9),
        xsd_tick(XSD_A, h(2) + 5, 100_000_000, 10), // roll → EXIT revert; priced at this bid
        xsd_tick(XSD_A, h(2) + 10, 100_000_000, 11), // the exit fills
        xsd_tick(XSD_B, h(2) + 11, 100_000_000, 12),
        xsd_tick(XSD_A, h(3) + 5, 100_000_000, 13),
        xsd_tick(XSD_B, h(3) + 6, 100_000_000, 14),
    ];
    let path = run.join("bn-ticks.pmlr");
    let mut w = PmlrWriter::open(&path, SlotKind::Tick, EPOCH_NS).expect("open writer");
    for t in &ticks {
        w.append(t).expect("append");
    }
    w.flush().expect("flush");
    let toml = root.join("xsd.toml");
    std::fs::write(&toml, XSD_TOML).expect("write xsd.toml");
    let table = root.join("xsd-table.tsv");
    std::fs::write(
        &table,
        "# target\tpartner\tbeta_1e9\nbinance-usdm:aaausdt\tbinance-usdm:bbbusdt\t1000000000\n\
         binance-usdm:aaausdt\tbinance-usdm:zzzusdt\t1000000000\n",
    )
    .expect("write table");
    let seed = root.join("xsd-seed.tsv");
    let mut s = String::from("# descriptor\topen_ms\tclose_1e6\n");
    let mut k = 0u64;
    while k < 95 {
        let hour = XSD_H0 - 95 + k;
        let a = if k % 2 == 0 { 100_000_000 } else { 100_200_000 };
        s.push_str(&format!("binance-usdm:aaausdt\t{}\t{a}\n", hour * HOUR_S * 1_000));
        s.push_str(&format!("binance-usdm:bbbusdt\t{}\t100000000\n", hour * HOUR_S * 1_000));
        k += 1;
    }
    // A seed row AT the boot hour must be dropped, never trusted.
    s.push_str(&format!("binance-usdm:aaausdt\t{}\t999000000\n", XSD_H0 * HOUR_S * 1_000));
    std::fs::write(&seed, s).expect("write seed");
    (root.to_path_buf(), toml, table, seed)
}

#[test]
fn xsd_member_enters_and_reverts_on_the_wall_hour_grid() {
    let root = unique_root("xsd");
    let (replay, toml, table, seed) = build_xsd_capture(&root);
    let mut cfg = member_cfg(&replay, &toml);
    cfg.member = Some(MemberSpec {
        kind: MemberKind::Xsd,
        params: toml.clone(),
        table: Some(table),
        seed: Some(seed),
        vrp_seed: None,
    });
    let out = run_member(&cfg, cfg.member.as_ref().unwrap()).expect("member run");
    let hash = core_crypto::sha256(XSD_TOML.as_bytes());
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        out.schema1.starts_with(&format!("{{\"schema_version\":1,\"ruleset_hash\":\"{hex}\",\"split\":\"0/100\",")),
        "schema1: {}",
        out.schema1
    );
    assert!(out.summary.contains("member: xsd params="), "{}", out.summary);
    assert!(
        out.summary.contains(" targets=1 pairs=1 syms=2 rows_dropped=1 seed_rows=190 seed_dropped=1 "),
        "{}",
        out.summary
    );
    // Three rolls (H0+1, +2, +3): one entry, one revert exit, no stop.
    assert!(
        out.summary.contains(" rolls=3 decisions=3 entries=1 adds=0 exits_revert=1 exits_stop=0 "),
        "{}",
        out.summary
    );
    assert!(out.summary.contains(" positions_open=0 orders_emitted=2 "), "{}", out.summary);
    assert_eq!(out.stats.fills_total, 2, "{}", out.summary);
    assert_eq!(out.stats.ioc_fills, 2, "{}", out.summary);
    assert_eq!(out.stats.oos_round_trips, 1, "{}", out.summary);
    assert!(out.schema1.contains("\"round_trips\":1,\"legs\":2}"), "{}", out.schema1);
    // Bought ~9.5 units at 105.11, sold at 99.99: ≈ −$48.7 at zero fee.
    let key = "\"net_pnl_usd\":";
    let i = out.schema1.find(key).expect("net field") + key.len();
    let j = out.schema1[i..].find(',').expect("comma") + i;
    let net: f64 = out.schema1[i..j].parse().expect("json number");
    assert!((-49.5..=-48.0).contains(&net), "net {net}: {}", out.schema1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn xsd_member_without_a_table_is_refused_with_a_reason() {
    let root = unique_root("xsd-notable");
    let (replay, toml, _table, seed) = build_xsd_capture(&root);
    let mut cfg = member_cfg(&replay, &toml);
    cfg.member = Some(MemberSpec {
        kind: MemberKind::Xsd,
        params: toml.clone(),
        table: Some(root.join("absent-table.tsv")),
        seed: Some(seed),
        vrp_seed: None,
    });
    let msg = match run_member(&cfg, cfg.member.as_ref().unwrap()) {
        Ok(_) => panic!("an explicit absent table must refuse the run"),
        Err(e) => format!("{e}"),
    };
    assert!(msg.contains("does not exist"), "{msg}");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------
// VRP P2.3 (Q10) — `backtest --member vrp`
// ---------------------------------------------------------------
//
// A whole campaign on a synthetic root: 34 h of one-minute Deribit perp
// closes (the HAR's 24 h warm-up plus the 8 h hold), one option's
// ticker summaries and its REAL quote lane, and a chain of two expiries
// so the selection law has to choose. The member selects at
// `E − τ − selection/2`, decides at `E − τ` against a 500 % quoted IV
// it cannot possibly agree with, its entry IoC fills against the option
// book, the hedge goes out FROM that fill (X1), and the contract is
// still held at `E`, where it settles for cash.

/// `deribit:BTC-27MAR26-79000-C` expires 2026-03-27T08:00:00Z.
const VRP_EXPIRY_NS: u64 = 1_774_598_400_000_000_000;
/// The run's epoch: 34 h before the expiry, so the 24 h warm-up
/// finishes with two hours to spare before the selection window opens.
const VRP_EPOCH_NS: u64 = VRP_EXPIRY_NS - 122_400_000_000_000;
const VRP_PERP: u32 = (3 << 24) | 1;
const VRP_OPT: u32 = (3 << 24) | 700;
const VRP_OPT_FAR: u32 = (3 << 24) | 701;

const VRP_TOML: &str = "[vrp]\n\
theta_1e9             = 100000000\n\
tau_ns                = 28800000000000\n\
epsilon_ns            = 300000000000\n\
selection_ns          = 600000000000\n\
rebalance_ns          = 3600000000000\n\
qty_1e6               = 1000000\n\
band_qty_1e6          = 50000\n\
underlying_descriptor = \"deribit:BTC-PERPETUAL\"\n\
hedge_descriptor      = \"deribit:BTC-PERPETUAL\"\n\
sides                 = \"both\"\n";

/// A deep two-sided book — every leg of the campaign fills in one tick.
fn vrp_tick(ts_ns: u64, sym: u32, seq: u32, bid: i64, ask: i64) -> Tick {
    Tick::new(
        ts_ns,
        VenueId::Deribit,
        sym,
        seq,
        Price::from_raw(bid),
        Qty::from_raw(100_000_000_000_000),
        Price::from_raw(ask),
        Qty::from_raw(100_000_000_000_000),
    )
}

/// One ticker summary of `sym`: mark 0.0038 BTC, the quoted IV and
/// delta the caller wants, against an underlying of `index_1e9`.
fn vrp_summary(
    ts_ns: u64,
    sym: u32,
    iv_1e9: i64,
    delta_1e9: i64,
    index_1e9: i64,
) -> core_types::OptSummary {
    core_types::OptSummary::new(
        ts_ns,
        VenueId::Deribit,
        sym,
        core_types::OPT_SUMMARY_FLAG_MARK_PX,
        3_800_000, // 0.0038 BTC ⇒ ~$300 at $79,000
        iv_1e9,
        index_1e9,
        0,
        delta_1e9,
        1,
        1,
        -1,
    )
}

/// Build the campaign root. Returns `(replay_dir, vrp.toml, vrp-seed.tsv)`.
fn build_vrp_capture(root: &Path, with_chain: bool) -> (PathBuf, PathBuf, PathBuf) {
    let run = root.join(format!("run-{VRP_EPOCH_NS}"));
    std::fs::create_dir_all(&run).expect("mkdir run");
    let mut manifest = format!("{VRP_PERP}\tderibit:BTC-PERPETUAL\n");
    if with_chain {
        manifest.push_str(&format!("{VRP_OPT}\tderibit:BTC-27MAR26-79000-C\n"));
        manifest.push_str(&format!("{VRP_OPT_FAR}\tderibit:BTC-28MAR26-79000-C\n"));
    }
    std::fs::write(run.join("instrument-manifest.tsv"), manifest).expect("manifest");
    std::fs::write(
        run.join("options-manifest.tsv"),
        if with_chain {
            format!(
                "deribit\t{VRP_OPT}\tBTC-27MAR26-79000-C\nderibit\t{VRP_OPT_FAR}\tBTC-28MAR26-79000-C\n"
            )
        } else {
            String::new()
        },
    )
    .expect("options manifest");

    // ---- the perp lane: 2041 one-minute closes, EPOCH .. E ----
    //
    // The walk matters: a flat tape has zero realised variance, the HAR
    // is then 0 and the member reports `no_bounds` forever. The step is
    // ~$20/min on $79,000 — about 2.5 bps, which is the live scale.
    //
    // THREE ticks a minute, not one: every leg is an IoC with a 60 s
    // TTL, so a lane that prints once a minute gives a hedge emitted
    // just after a print a single chance to be judged before its own
    // TTL cancels it. The live Deribit perp prints many times a second.
    const STEP_NS: u64 = 20_000_000_000;
    let steps = 122_400_000_000_000 / STEP_NS + 15; // EPOCH .. E + 5 min
    let mut perp: Vec<Tick> = Vec::with_capacity(steps as usize + 1);
    let mut px = 79_000_000_000i64;
    let mut s = 20_260_912i64;
    let mut k = 0u64;
    while k <= steps {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        px = (px + ((s as u64 >> 32) % 14_000_000) as i64 - 7_000_000).max(1_000_000_000);
        perp.push(vrp_tick(
            500 + k * STEP_NS,
            VRP_PERP,
            k as u32,
            px - 500_000,
            px + 500_000,
        ));
        k += 1;
    }
    // The option's REAL quote lane (VRP V2a): COIN-denominated, 0.005 /
    // 0.006 BTC ⇒ $395 / $474 at $79,000. The entry IoC rests at the
    // MARK (~$300), so the bid crosses it and the leg fills.
    //
    // Only after the first summary: a quote with no underlying known at
    // or before its instant is dropped, by the denomination law.
    const SELECT_OFF: u64 = 122_400_000_000_000 - 28_800_000_000_000 - 300_000_000_000;
    const ENTRY_OFF: u64 = 122_400_000_000_000 - 28_800_000_000_000;
    let mut opt_ticks: Vec<Tick> = Vec::new();
    let mut j = 0u64;
    while j < 8 {
        opt_ticks.push(vrp_tick(
            500 + SELECT_OFF + 1_000_000_000 + j * 1_000_000_000,
            VRP_OPT,
            j as u32,
            5_000,
            6_000,
        ));
        j += 1;
    }
    j = 0;
    while j < 8 {
        opt_ticks.push(vrp_tick(
            500 + ENTRY_OFF + 1_000_000_000 + j * 1_000_000_000,
            VRP_OPT,
            100 + j as u32,
            5_000,
            6_000,
        ));
        j += 1;
    }
    let mut w = PmlrWriter::open(run.join("deribit-ticks.pmlr"), SlotKind::Tick, VRP_EPOCH_NS)
        .expect("open ticks");
    // §3.2 orders within a file by (ts, idx), so append in ts order.
    let mut all: Vec<Tick> = perp;
    all.extend(opt_ticks);
    all.sort_by_key(|t| t.ts_ns);
    for t in &all {
        w.append(t).expect("append tick");
    }
    w.flush().expect("flush ticks");

    // ---- the ticker lane: select, decide, settle ----
    let summaries = [
        // Inside `[E−τ−selection, E−τ]`: the member picks the strike.
        vrp_summary(
            500 + SELECT_OFF,
            VRP_OPT,
            700_000_000,
            500_000_000,
            79_000_000_000_000,
        ),
        // At E−τ: 500 % quoted IV against a ~20 % forecast ⇒ SELL vol.
        vrp_summary(
            500 + ENTRY_OFF,
            VRP_OPT,
            5_000_000_000,
            500_000_000,
            79_000_000_000_000,
        ),
        // Closing in: the index ends $1,500 above the 79,000 strike.
        // These have to land BEFORE the expiry instant — the member
        // settles on the first record at or after E, which is a perp
        // tick, and it settles at the last index it was told about.
        vrp_summary(
            500 + 122_400_000_000_000 - 10_000_000_000,
            VRP_OPT,
            5_000_000_000,
            500_000_000,
            80_500_000_000_000,
        ),
        vrp_summary(
            500 + 122_400_000_000_000 - 1_000_000_000,
            VRP_OPT,
            5_000_000_000,
            500_000_000,
            80_500_000_000_000,
        ),
        vrp_summary(
            500 + 122_400_000_000_000,
            VRP_OPT,
            5_000_000_000,
            500_000_000,
            80_500_000_000_000,
        ),
    ];
    let mut w = PmlrWriter::open(
        run.join("deribit-opt-summary.pmlr"),
        SlotKind::OptSummary,
        VRP_EPOCH_NS,
    )
    .expect("open opt");
    for o in &summaries {
        w.append(o).expect("append summary");
    }
    w.flush().expect("flush opt");

    // ---- the artifacts ----
    let toml = root.join("vrp.toml");
    std::fs::write(&toml, VRP_TOML).expect("write vrp.toml");
    // 60 fitted pairs ON the identity line y = x: the OLS fit is then
    // a = 0, b = 1, so `ln σ̂` IS `ln har` at any regressor — the band
    // is the live HAR itself and the fixture cannot drift into an
    // extrapolation nobody intended.
    let seed_path = run.join("vrp-seed.tsv");
    let mut seed = String::from("V\t2\n");
    let mut i = 0u64;
    while i < 60 {
        let x = 14_000_000_000i64 + i as i64 * 100_000_000;
        seed.push_str(&format!(
            "P\t{}\t{x}\t{x}\n",
            1_700_000_000_000u64 + i * 86_400_000
        ));
        i += 1;
    }
    std::fs::write(&seed_path, seed).expect("write vrp-seed.tsv");
    (root.to_path_buf(), toml, seed_path)
}

fn vrp_cfg(replay: &Path, toml: &Path, seed: Option<PathBuf>) -> BacktestConfig {
    let mut cfg = member_cfg(replay, toml);
    cfg.fee_bps = vec!["deribit:0:0".to_owned()];
    cfg.member = Some(MemberSpec {
        kind: MemberKind::Vrp,
        params: toml.to_path_buf(),
        table: None,
        seed: None,
        vrp_seed: seed,
    });
    cfg
}

#[test]
fn member_vrp_drives_a_campaign_on_a_synthetic_root() {
    let root = unique_root("vrp");
    let (replay, toml, seed) = build_vrp_capture(&root, true);
    let cfg = vrp_cfg(&replay, &toml, Some(seed));
    let out = run_member(&cfg, cfg.member.as_ref().unwrap()).expect("member run");

    // The boot line names the chain it resolved and the warm forecast.
    assert!(out.summary.contains("member: vrp params="), "{}", out.summary);
    assert!(out.summary.contains("chain_rows=2"), "{}", out.summary);
    assert!(out.summary.contains("pairs=60"), "{}", out.summary);
    // `warm=` on the boot line is the SEEDED window: this root carries
    // no `R` rows, so the member warms from the replay's own closes —
    // which the counters line reports after the drive.
    assert!(out.summary.contains("warm=false"), "{}", out.summary);
    assert!(out.summary.contains("vol_warm=true"), "{}", out.summary);
    assert!(out.summary.contains("sides=both"), "{}", out.summary);
    assert!(out.summary.contains("root=run-dirs (Q3)"), "{}", out.summary);

    let c = &out.summary;
    let field = |name: &str| -> u64 {
        let key = format!(" {name}=");
        let i = c.find(&key).unwrap_or_else(|| panic!("{name}: {c}")) + key.len();
        let j = c[i..]
            .find(|ch: char| !ch.is_ascii_digit())
            .map_or(c.len(), |k| k + i);
        c[i..j].parse().unwrap_or_else(|_| panic!("{name}: {c}"))
    };
    assert!(field("select_scans") >= 1, "{c}");
    assert_eq!(field("decisions"), 1, "one decision at E−τ: {c}");
    assert_eq!(field("no_bounds"), 0, "the forecast exists: {c}");
    assert_eq!(field("holds"), 0, "500 % IV is outside any sane band: {c}");
    assert!(field("entries_submitted") >= 1, "{c}");
    assert_eq!(field("entries"), 1, "and the campaign opened: {c}");
    assert_eq!(field("settlements"), 1, "the contract expired held: {c}");
    // The index closed $1,500 above the 79,000 strike.
    assert_eq!(field("settled_itm"), 1, "settled in the money: {c}");
    assert_eq!(field("settled_otm"), 0, "{c}");
    assert_eq!(field("settled_unpriced"), 0, "{c}");
    // X1: the hedge goes out FROM the option fill and completes — an
    // abandoned hedge is a naked perp, which is the F7 defect.
    assert!(field("hedges") >= 1, "the delta hedge went out: {c}");
    assert_eq!(field("hedge_abandoned"), 0, "{c}");
    assert!(field("fills") >= 2, "both legs filled: {c}");
    assert_eq!(field("fills_ignored"), 0, "{c}");
    assert!(field("vol_minutes") >= 1_440, "warmed from the replay: {c}");
    // The report is the VM's own shape.
    assert!(out.schema1.contains("\"schema_version\":1"), "{}", out.schema1);
    assert!(out.schema1.contains("\"position_rows\":1"), "{}", out.schema1);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn member_vrp_refuses_a_missing_chain() {
    let root = unique_root("vrp-nochain");
    let (replay, toml, seed) = build_vrp_capture(&root, false);
    let cfg = vrp_cfg(&replay, &toml, Some(seed));
    let msg = match run_member(&cfg, cfg.member.as_ref().unwrap()) {
        Ok(_) => panic!("a root whose manifest carries no option chain must refuse"),
        Err(e) => format!("{e}"),
    };
    assert!(msg.contains("options chain holds no BTC option"), "{msg}");
    let _ = std::fs::remove_dir_all(&root);
}
