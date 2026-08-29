// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! 8h H1 harness-substrate integration tests (design §12, H1 slice):
//! golden multi-venue multi-run fixture through the REAL `cli::backtest`
//! pipeline (real `validate_ruleset`, real `VmStrategy` flip + emit
//! paths), byte-identical determinism, the frozen worker argv against
//! the real binary (`CARGO_BIN_EXE`), and the §5 nonzero-exit paths.
//!
//! Offline-path doctrine: this test allocates freely; fixtures are
//! written with the real `PmlrWriter` into unique temp dirs.

use std::path::{Path, PathBuf};

use core_io::{PmlrWriter, SlotKind};
use core_types::{Price, Qty, Tick, VenueId};
use ingress_ai::RulesetReject;

use cli::backtest::{BacktestConfig, HarnessError, VIRT_T0};

// ---------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------

/// The golden candidate: one §4.1 cross_deviation row, action sym 42
/// (PM namespace) against reference sym 7 (the boot-default BN id),
/// 80 bps edge, 1500 ms horizon, $50 row cap.
const GOLDEN_RULESET: &str = r#"{"rows":[{"name":"h1-golden","family":"crypto","trigger":{"type":"cross_deviation","ref":7},"sym":42,"side":"bid","edge_bps":80,"horizon_ms":1500,"max_risk_usd":50.0}]}"#;

/// A candidate whose action leg (99) the capture never observed —
/// §3.5: "you cannot evaluate what you did not capture".
const ABSENT_SYM_RULESET: &str = r#"{"rows":[{"name":"h1-absent","family":"crypto","trigger":{"type":"cross_deviation","ref":7},"sym":99,"side":"bid","edge_bps":80,"horizon_ms":1500,"max_risk_usd":50.0}]}"#;

/// Wall epochs of the two golden runs (disjoint; gap 4 s dwarfs the
/// 1500 ms row horizon, so the second run re-fires).
const EPOCH_RUN_0: u64 = 1_000_000_000;
const EPOCH_RUN_1: u64 = 5_000_000_000;

fn unique_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("cli-backtest-{tag}-{}-{nanos}", std::process::id()))
}

fn mk_tick(ts_ns: u64, venue: VenueId, sym: u32, seq: u32, bid: i64, ask: i64) -> Tick {
    Tick::new(
        ts_ns,
        venue,
        sym,
        seq,
        Price::from_raw(bid),
        Qty::from_raw(10_000_000),
        Price::from_raw(ask),
        Qty::from_raw(10_000_000),
    )
}

fn write_ticks(run_dir: &Path, label: &str, epoch_ns: u64, ticks: &[Tick]) {
    let path = run_dir.join(format!("{label}-ticks.pmlr"));
    let mut w = PmlrWriter::open(&path, SlotKind::Tick, epoch_ns).expect("open writer");
    for t in ticks {
        w.append(t).expect("append tick");
    }
    w.flush().expect("flush");
}

/// Build the golden two-run, two-venue capture under `root` and
/// return the ruleset path. Layout (monotonic ts per run are
/// arbitrary-based on purpose — the §3.3 rebase must erase them):
///
/// * run-1000000000: bn(sym 7) mid 0.56 @ ts 1000; pm(sym 42)
///   mid 0.50 @ ts 2000 (fires, $50) and @ ts 3000 (cooldown).
/// * run-5000000000: bn @ ts 500; pm @ ts 700 (re-fires, $50 —
///   virt gap 4 s > 1500 ms horizon).
fn build_golden_capture(root: &Path) -> PathBuf {
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));
    let run1 = root.join(format!("run-{EPOCH_RUN_1}"));
    std::fs::create_dir_all(&run0).expect("mkdir run0");
    std::fs::create_dir_all(&run1).expect("mkdir run1");

    write_ticks(
        &run0,
        "bn",
        EPOCH_RUN_0,
        &[mk_tick(1_000, VenueId::Binance, 7, 1, 550_000, 570_000)],
    );
    write_ticks(
        &run0,
        "pm",
        EPOCH_RUN_0,
        &[
            mk_tick(2_000, VenueId::Polymarket, 42, 1, 490_000, 510_000),
            mk_tick(3_000, VenueId::Polymarket, 42, 2, 490_000, 510_000),
        ],
    );
    write_ticks(
        &run1,
        "bn",
        EPOCH_RUN_1,
        &[mk_tick(500, VenueId::Binance, 7, 2, 550_000, 570_000)],
    );
    write_ticks(
        &run1,
        "pm",
        EPOCH_RUN_1,
        &[mk_tick(700, VenueId::Polymarket, 42, 3, 490_000, 510_000)],
    );

    let ruleset = root.join("golden-ruleset.json");
    std::fs::write(&ruleset, GOLDEN_RULESET).expect("write ruleset");
    ruleset
}

fn cfg(ruleset: &Path, replay_dir: &Path, split: &str) -> BacktestConfig {
    BacktestConfig {
        ruleset: ruleset.to_path_buf(),
        replay_dir: replay_dir.to_path_buf(),
        split: split.to_owned(),
        fee_bps: Vec::new(),
        latency_ns: None,
        latency_ns_venue: Vec::new(),
        emit_detail: None,
    }
}

/// Expected schema-1 line for the golden fixture: hold-model zeros,
/// live `max_order_notional_usd` ($50 exactly: qty = 50e6·1e6/mid,
/// mid 0.50 ⇒ qty 1e8 ⇒ notional 50e6 with zero flooring loss).
fn golden_schema1(split: &str) -> String {
    let digest = core_crypto::sha256(GOLDEN_RULESET.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{{\"schema_version\":1,\"ruleset_hash\":\"{hex}\",\"split\":\"{split}\",\
         \"oos\":{{\"net_pnl_usd\":0.0,\"trades\":0,\"trading_days\":0,\
         \"max_drawdown_usd\":0.0,\"round_trips\":0,\"legs\":0}},\
         \"bounds\":{{\"max_order_notional_usd\":50.0,\
         \"max_symbol_notional_usd\":0.0,\"max_total_notional_usd\":0.0}},\
         \"position_rows\":0}}"
    )
}

// ---------------------------------------------------------------
// Golden fixture (§12: "golden replay fixture with known P&L" — the
// hold-model slice of it: known bounds, known zeros, known counters)
// ---------------------------------------------------------------

#[test]
fn golden_two_run_two_venue_capture_hold_model_report() {
    let root = unique_root("golden");
    let ruleset = build_golden_capture(&root);

    let out = cli::backtest::run(&cfg(&ruleset, &root, "70/30")).expect("harness ok");

    // The frozen stdout contract, byte for byte.
    assert_eq!(out.schema1, golden_schema1("70/30"));

    // Substrate facts: discovery, merge, rebase, universe, vm drive.
    let s = out.stats;
    assert_eq!(s.runs, 2);
    assert_eq!(s.merged_records, 5);
    assert_eq!(s.universe_syms, 2, "capture-observed universe = {{7, 42}}");
    assert_eq!(
        s.first_virt_ns, VIRT_T0,
        "run-0 first tick rebases to VIRT_T0"
    );
    assert_eq!(
        s.last_virt_ns,
        VIRT_T0 + (EPOCH_RUN_1 - EPOCH_RUN_0) + 200,
        "inter-run gap = epoch delta; intra-run delta (700-500) exact"
    );
    // §3.4 boundary: 70% of the 4_000_000_200 ns span.
    assert_eq!(s.boundary_virt_ns, VIRT_T0 + 2_800_000_140);
    assert_eq!(s.oos_records, 2, "the whole second run is OOS");
    assert_eq!(s.capture_utc_days, 1, "both wall epochs land on 1970-01-01");

    // vm drive through the REAL paths: commit flipped (or `run` would
    // have errored), row evaluated on every tick of EITHER leg (VM2
    // V3 two-legged freshness: the two bn ref ticks evaluate too —
    // run-1's fire lands on its ref tick, 200 ns EARLIER than v1's,
    // on fresher data; fires/emits/bounds unchanged), fired twice
    // (cooldown swallowed the middle pm tick), emitted twice at $50.
    assert_eq!(s.vm_evals, 5);
    assert_eq!(s.vm_fires, 2);
    assert_eq!(s.vm_orders_emitted, 2);
    assert_eq!(s.vm_orders_dropped, 0);
    assert_eq!(s.vm_book_track_failed, 0);
    assert_eq!(s.max_order_notional_1e6, 50_000_000);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn golden_capture_single_run_dir_form() {
    // §3.1: --replay-dir may point straight at one run-<ns> dir.
    let root = unique_root("single");
    let ruleset = build_golden_capture(&root);
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));

    let out = cli::backtest::run(&cfg(&ruleset, &run0, "70/30")).expect("harness ok");
    assert_eq!(out.stats.runs, 1);
    assert_eq!(out.stats.merged_records, 3);
    assert_eq!(
        out.stats.vm_orders_emitted, 1,
        "second pm tick sits in cooldown"
    );
    assert_eq!(out.stats.max_order_notional_1e6, 50_000_000);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn carved_all_oos_split_is_accepted_and_echoed_verbatim() {
    // §3.4's one carved degenerate form — the §8.3 monitor's scoring
    // mode. Boundary collapses to the first record; everything is OOS.
    let root = unique_root("alloos");
    let ruleset = build_golden_capture(&root);

    let out = cli::backtest::run(&cfg(&ruleset, &root, "0/100")).expect("harness ok");
    assert_eq!(out.schema1, golden_schema1("0/100"));
    assert_eq!(out.stats.boundary_virt_ns, out.stats.first_virt_ns);
    assert_eq!(out.stats.oos_records, out.stats.merged_records);

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------
// Determinism (§12 / plan §11: "same log ⇒ bit-identical report")
// ---------------------------------------------------------------

#[test]
fn reruns_are_byte_identical() {
    let root = unique_root("determinism");
    let ruleset = build_golden_capture(&root);
    let c = cfg(&ruleset, &root, "70/30");

    let a = cli::backtest::run(&c).expect("first run ok");
    let b = cli::backtest::run(&c).expect("second run ok");
    assert_eq!(
        a.schema1, b.schema1,
        "schema-1 stdout must be bit-identical"
    );
    assert_eq!(a.summary, b.summary, "stderr summary is deterministic too");

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------
// The frozen argv against the REAL binary (worker contract shape)
// ---------------------------------------------------------------

#[test]
fn real_binary_frozen_argv_prints_schema1_only() {
    let root = unique_root("binary");
    let ruleset = build_golden_capture(&root);

    // EXACTLY the argv `backtest.py::run_backtest` builds (after the
    // binary name): no extra flags, worker defaults ARE the contract.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_multivenue-engine"))
        .args([
            "backtest",
            "--ruleset",
            ruleset.to_str().expect("utf8 path"),
            "--replay-dir",
            root.to_str().expect("utf8 path"),
            "--split",
            "70/30",
        ])
        .output()
        .expect("spawn harness binary");

    assert!(
        out.status.success(),
        "harness must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert_eq!(
        stdout,
        format!("{}\n", golden_schema1("70/30")),
        "stdout must carry the schema-1 line and NOTHING else"
    );
    assert!(
        !out.stderr.is_empty(),
        "human summary belongs on stderr (§10)"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn real_binary_validator_reject_is_nonzero_with_empty_stdout() {
    let root = unique_root("binreject");
    let _ = build_golden_capture(&root);
    let absent = root.join("absent-ruleset.json");
    std::fs::write(&absent, ABSENT_SYM_RULESET).expect("write ruleset");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_multivenue-engine"))
        .args([
            "backtest",
            "--ruleset",
            absent.to_str().expect("utf8 path"),
            "--replay-dir",
            root.to_str().expect("utf8 path"),
            "--split",
            "70/30",
        ])
        .output()
        .expect("spawn harness binary");

    assert!(!out.status.success(), "validator reject must exit nonzero");
    assert!(
        out.stdout.is_empty(),
        "NO schema-1 output on reject (§3.5/§5)"
    );
    assert!(!out.stderr.is_empty(), "reject reason belongs on stderr");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn real_binary_bad_split_is_nonzero_with_empty_stdout() {
    let root = unique_root("binsplit");
    let ruleset = build_golden_capture(&root);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_multivenue-engine"))
        .args([
            "backtest",
            "--ruleset",
            ruleset.to_str().expect("utf8 path"),
            "--replay-dir",
            root.to_str().expect("utf8 path"),
            "--split",
            "70/40",
        ])
        .output()
        .expect("spawn harness binary");

    assert!(!out.status.success(), "70/40 must be a usage error (§3.4)");
    assert!(out.stdout.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------
// Failure modes through the library surface
// ---------------------------------------------------------------

#[test]
fn absent_leg_sym_is_a_symbol_reject() {
    let root = unique_root("reject");
    let _ = build_golden_capture(&root);
    let absent = root.join("absent-ruleset.json");
    std::fs::write(&absent, ABSENT_SYM_RULESET).expect("write ruleset");

    let err = cli::backtest::run(&cfg(&absent, &root, "70/30")).unwrap_err();
    match err {
        HarnessError::Reject(r) => assert_eq!(r, RulesetReject::Symbol),
        other => panic!("expected Reject(Symbol), got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn empty_log_root_is_a_capture_error() {
    let root = unique_root("emptyroot");
    std::fs::create_dir_all(&root).expect("mkdir");
    let ruleset = root.join("r.json");
    std::fs::write(&ruleset, GOLDEN_RULESET).expect("write ruleset");

    let err = cli::backtest::run(&cfg(&ruleset, &root, "70/30")).unwrap_err();
    assert!(matches!(err, HarnessError::Capture(_)), "got {err:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn header_only_capture_is_an_empty_merge_error() {
    // Runs exist, files exist, but nothing was ever captured — no
    // trustworthy report can describe an empty stream (§5).
    let root = unique_root("headeronly");
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));
    std::fs::create_dir_all(&run0).expect("mkdir");
    write_ticks(&run0, "pm", EPOCH_RUN_0, &[]);
    write_ticks(&run0, "bn", EPOCH_RUN_0, &[]);
    let ruleset = root.join("r.json");
    std::fs::write(&ruleset, GOLDEN_RULESET).expect("write ruleset");

    let err = cli::backtest::run(&cfg(&ruleset, &root, "70/30")).unwrap_err();
    assert!(matches!(err, HarnessError::Capture(_)), "got {err:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn header_epoch_mismatch_fails_the_cross_check() {
    // §3.1: dir-name epoch and PMLR header epoch must agree.
    let root = unique_root("epochxs");
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));
    std::fs::create_dir_all(&run0).expect("mkdir");
    write_ticks(
        &run0,
        "pm",
        EPOCH_RUN_0 + 1, // header disagrees with the dir name
        &[mk_tick(1_000, VenueId::Polymarket, 42, 1, 490_000, 510_000)],
    );
    let ruleset = root.join("r.json");
    std::fs::write(&ruleset, GOLDEN_RULESET).expect("write ruleset");

    let err = cli::backtest::run(&cfg(&ruleset, &root, "70/30")).unwrap_err();
    match err {
        HarnessError::Capture(msg) => {
            assert!(msg.contains("epoch_ns"), "cross-check message: {msg}")
        }
        other => panic!("expected Capture, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn v1_tick_capture_is_refused() {
    // The §3.2 merge keys on the venue byte, which v1 slots leave
    // undefined — a v1 file must be refused, not silently mis-merged.
    let root = unique_root("v1file");
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));
    std::fs::create_dir_all(&run0).expect("mkdir");
    let mut header = vec![0u8; 64];
    header[0..4].copy_from_slice(b"PMLR");
    header[4..6].copy_from_slice(&1u16.to_le_bytes()); // v1
    header[6] = 0; // SlotKind::Tick
    header[8..16].copy_from_slice(&EPOCH_RUN_0.to_le_bytes());
    std::fs::write(run0.join("pm-ticks.pmlr"), &header).expect("write v1 file");
    let ruleset = root.join("r.json");
    std::fs::write(&ruleset, GOLDEN_RULESET).expect("write ruleset");

    let err = cli::backtest::run(&cfg(&ruleset, &root, "70/30")).unwrap_err();
    match err {
        HarnessError::Capture(msg) => assert!(msg.contains("v2"), "message: {msg}"),
        other => panic!("expected Capture, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unreadable_ruleset_is_a_usage_error() {
    let root = unique_root("noruleset");
    let _ = build_golden_capture(&root);
    let missing = root.join("does-not-exist.json");

    let err = cli::backtest::run(&cfg(&missing, &root, "70/30")).unwrap_err();
    assert!(matches!(err, HarnessError::Usage(_)), "got {err:?}");

    let _ = std::fs::remove_dir_all(&root);
}

// ===============================================================
// H2 — the §4 model: known-P&L golden fixture (design §12 / plan §11
// "golden replay fixture with known P&L"), sidecar, fee override,
// determinism, and the COMMITTED copy the Python real-harness test
// (claude-worker/tests/test_backtest_real.py) replays.
// ===============================================================

/// The P&L candidate: two `level_breach` rows on PM sym 42 —
/// buy-at-mid when ask ≤ 0.42, sell-at-mid when bid ≥ 0.60, $50 caps,
/// 1500 ms cooldowns. BN sym 7 rides along as merge ballast (the rows
/// never reference it), keeping the fixture two-venue (§16.3.1).
const PNL_RULESET: &str = r#"{"rows":[{"name":"h2-buy-low","family":"crypto","trigger":{"type":"level_breach","level":0.42},"sym":42,"side":"bid","edge_bps":80,"horizon_ms":1500,"max_risk_usd":50.0},{"name":"h2-sell-high","family":"crypto","trigger":{"type":"level_breach","level":0.6},"sym":42,"side":"ask","edge_bps":80,"horizon_ms":1500,"max_risk_usd":50.0}]}"#;

/// Wall epochs: run 0 on 1970-01-01; run 1 starts 1970-01-02
/// 23:59:59.2 so its OOS fills straddle a UTC midnight (2 trading
/// days). The 70/30 boundary lands inside the inter-run gap: run 0 is
/// all IS, run 1 all OOS.
const PNL_EPOCH_RUN_0: u64 = 1_000_000_000;
const PNL_EPOCH_RUN_1: u64 = 172_799_200_000_000;

fn mk_tick_q(
    ts_ns: u64,
    venue: VenueId,
    sym: u32,
    seq: u32,
    bid: i64,
    bid_q: i64,
    ask: i64,
    ask_q: i64,
) -> Tick {
    Tick::new(
        ts_ns,
        venue,
        sym,
        seq,
        Price::from_raw(bid),
        Qty::from_raw(bid_q),
        Price::from_raw(ask),
        Qty::from_raw(ask_q),
    )
}

/// Build the known-P&L capture under `root`; returns the ruleset path.
///
/// Hand-computed script (Δ_pm = 200 ms default; virt per §3.3):
///
/// run 0 (ALL IS — warms the vm and the full book only):
/// * bn @1000 (ballast) · pm @2000 0.38/0.42 ⇒ row-1 fires, O1 = BID
///   0.40 × 125 ($50) · pm @0.5s 0.36/0.39 ask-size 200 ⇒ O1 fills
///   125 @0.40 (IS) · pm @2.2s 0.60/0.65 ⇒ row-2 fires, O2 = ASK
///   0.625 × 80 ($50) · pm @2.6s 0.66(size 30)/0.70 ⇒ O2 partial 30
///   @0.625 (IS; realized +$6.75 in the FULL book only).
///
/// run 1 (ALL OOS — the schema-1 verdict):
/// * bn @500 (ballast) · pm @700 0.38/0.42 ⇒ row-1 re-fires, O3 = BID
///   0.40 × 125 ($50, OOS) · pm @+0.3s 0.35/0.38 ask-size 70 ⇒ O3
///   partial 70 @0.40 — trade #1, day 1970-01-02; equity −$2.45 ·
///   pm @+0.9s 0.35/0.38 ask-size 200 ⇒ O3 fills 55 — trade #2, day
///   1970-01-03 (midnight crossed); equity trough −$4.375 (row-1
///   cooldown 0.9 s < 1.5 s: no re-fire) · pm @+1.4s 0.43/0.45 ⇒ mark
///   0.44, equity +$5.00; O2 (IS, 50 left) rests to the end (canceled).
///
/// Expected exactly: net +5.0, trades 2, days 2, DD 4.375; bounds
/// 50.0 / 96.8 / 96.8 (peak = 220 held × 0.44 last mark).
fn build_pnl_capture(root: &Path) -> PathBuf {
    let run0 = root.join(format!("run-{PNL_EPOCH_RUN_0}"));
    let run1 = root.join(format!("run-{PNL_EPOCH_RUN_1}"));
    std::fs::create_dir_all(&run0).expect("mkdir run0");
    std::fs::create_dir_all(&run1).expect("mkdir run1");

    write_ticks(
        &run0,
        "bn",
        PNL_EPOCH_RUN_0,
        &[mk_tick_q(
            1_000,
            VenueId::Binance,
            7,
            1,
            550_000,
            10_000_000,
            570_000,
            10_000_000,
        )],
    );
    write_ticks(
        &run0,
        "pm",
        PNL_EPOCH_RUN_0,
        &[
            mk_tick_q(
                2_000,
                VenueId::Polymarket,
                42,
                1,
                380_000,
                10_000_000,
                420_000,
                10_000_000,
            ),
            mk_tick_q(
                500_000_000,
                VenueId::Polymarket,
                42,
                2,
                360_000,
                10_000_000,
                390_000,
                200_000_000,
            ),
            mk_tick_q(
                2_200_000_000,
                VenueId::Polymarket,
                42,
                3,
                600_000,
                10_000_000,
                650_000,
                10_000_000,
            ),
            mk_tick_q(
                2_600_000_000,
                VenueId::Polymarket,
                42,
                4,
                660_000,
                30_000_000,
                700_000,
                10_000_000,
            ),
        ],
    );
    write_ticks(
        &run1,
        "bn",
        PNL_EPOCH_RUN_1,
        &[mk_tick_q(
            500,
            VenueId::Binance,
            7,
            2,
            550_000,
            10_000_000,
            570_000,
            10_000_000,
        )],
    );
    write_ticks(
        &run1,
        "pm",
        PNL_EPOCH_RUN_1,
        &[
            mk_tick_q(
                700,
                VenueId::Polymarket,
                42,
                5,
                380_000,
                10_000_000,
                420_000,
                10_000_000,
            ),
            mk_tick_q(
                300_000_700,
                VenueId::Polymarket,
                42,
                6,
                350_000,
                10_000_000,
                380_000,
                70_000_000,
            ),
            mk_tick_q(
                900_000_700,
                VenueId::Polymarket,
                42,
                7,
                350_000,
                10_000_000,
                380_000,
                200_000_000,
            ),
            mk_tick_q(
                1_400_000_700,
                VenueId::Polymarket,
                42,
                8,
                430_000,
                10_000_000,
                450_000,
                10_000_000,
            ),
        ],
    );

    let ruleset = root.join("golden-ruleset.json");
    std::fs::write(&ruleset, PNL_RULESET).expect("write ruleset");
    ruleset
}

/// The expected schema-1 line for the P&L fixture (defaults, 70/30).
fn pnl_schema1() -> String {
    let digest = core_crypto::sha256(PNL_RULESET.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{{\"schema_version\":1,\"ruleset_hash\":\"{hex}\",\"split\":\"70/30\",\
         \"oos\":{{\"net_pnl_usd\":5.0,\"trades\":2,\"trading_days\":2,\
         \"max_drawdown_usd\":4.375,\"round_trips\":0,\"legs\":2}},\
         \"bounds\":{{\"max_order_notional_usd\":50.0,\
         \"max_symbol_notional_usd\":96.8,\"max_total_notional_usd\":96.8}},\
         \"position_rows\":0}}"
    )
}

#[test]
fn golden_pnl_fixture_hand_computed_accounting_exact() {
    let root = unique_root("pnl");
    let ruleset = build_pnl_capture(&root);

    let out = cli::backtest::run(&cfg(&ruleset, &root, "70/30")).expect("harness ok");
    assert_eq!(
        out.schema1,
        pnl_schema1(),
        "known P&L, byte for byte (plan §11)"
    );

    let s = out.stats;
    assert_eq!(s.runs, 2);
    assert_eq!(s.merged_records, 10);
    assert_eq!(s.universe_syms, 2);
    assert_eq!(s.oos_records, 5, "all of run 1 is OOS");
    assert_eq!(s.capture_utc_days, 3, "days 0, 1, 2 spanned");
    // vm drive: 2 rows × 8 pm ticks; fires at run0 @2000 (bid row),
    // run0 @2.2s (ask row), run1 @700 (bid row re-armed).
    assert_eq!(s.vm_evals, 16);
    assert_eq!(s.vm_fires, 3);
    assert_eq!(s.vm_orders_emitted, 3);
    assert_eq!(s.vm_orders_dropped, 0);
    // §4 model facts.
    assert_eq!(s.orders_is, 2);
    assert_eq!(s.orders_oos, 1);
    assert_eq!(s.orders_rejected_sym_cap, 0);
    assert_eq!(s.orders_rejected_total_cap, 0);
    assert_eq!(s.orders_unroutable, 0);
    assert_eq!(s.orders_canceled_end, 1, "O2's 50 rests to the end");
    assert_eq!(s.peak_open_total, 2, "O2 (IS) + O3 (OOS) coexist");
    assert_eq!(s.peak_open_per_sym, 2);
    assert_eq!(s.fills_total, 4, "2 IS fills + 2 OOS fills");
    assert_eq!(s.fills_oos, 2);
    assert_eq!(s.oos_trading_days, 2, "midnight crossed inside run 1");
    // §4.5 components: no OOS reducing fill, zero fees ⇒ net == markout.
    assert_eq!(s.oos_realized_1e6, 0);
    assert_eq!(s.oos_fees_1e6, 0);
    assert_eq!(s.oos_unreal_1e6, 5_000_000);
    assert_eq!(s.oos_net_pnl_1e6, 5_000_000);
    assert_eq!(s.oos_max_drawdown_1e6, 4_375_000);
    assert_eq!(s.max_order_notional_1e6, 50_000_000);
    assert_eq!(s.max_symbol_notional_1e6, 96_800_000);
    assert_eq!(s.max_total_notional_1e6, 96_800_000);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn golden_pnl_fee_override_charges_maker_bps_exactly() {
    // Same fixture, PM maker 50 bps: OOS fees = ceil(28e12×50/1e4) +
    // ceil(22e12×50/1e4) = $0.14 + $0.11 = $0.25 ⇒ net 4.75, and the
    // fee-laden trough deepens the drawdown to 4.625.
    let root = unique_root("pnlfee");
    let ruleset = build_pnl_capture(&root);
    let mut c = cfg(&ruleset, &root, "70/30");
    c.fee_bps = vec!["pm:50:50".to_owned()];

    let out = cli::backtest::run(&c).expect("harness ok");
    let s = out.stats;
    assert_eq!(s.oos_fees_1e6, 250_000);
    assert_eq!(s.oos_net_pnl_1e6, 4_750_000);
    assert_eq!(s.oos_max_drawdown_1e6, 4_625_000);
    assert_eq!(s.fills_oos, 2, "fees never change what fills");
    assert_eq!(s.max_order_notional_1e6, 50_000_000);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn emit_detail_sidecar_is_written_versioned_and_deterministic() {
    let root = unique_root("pnldetail");
    let ruleset = build_pnl_capture(&root);
    let detail_a = root.join("detail-a.json");
    let detail_b = root.join("detail-b.json");

    let mut c = cfg(&ruleset, &root, "70/30");
    c.emit_detail = Some(detail_a.clone());
    let out_a = cli::backtest::run(&c).expect("harness ok");
    c.emit_detail = Some(detail_b.clone());
    let out_b = cli::backtest::run(&c).expect("harness ok");

    let a = std::fs::read_to_string(&detail_a).expect("sidecar a");
    let b = std::fs::read_to_string(&detail_b).expect("sidecar b");
    assert_eq!(a, b, "sidecar is deterministic");
    assert_eq!(out_a.schema1, out_b.schema1);
    // Versioned separately from schema-1; carries the operator detail
    // the frozen stdout must NOT carry (§5).
    assert!(a.starts_with("{\"detail_version\":1,"));
    assert!(a.contains("\"canceled_end\":1"));
    assert!(
        a.contains("\"full\":{\"realized_usd\":6.75,"),
        "IS sell realized $6.75: {a}"
    );
    assert!(a.contains("\"per_sym\":[{\"sym\":42,\"venue\":0,\"pos_qty\":220.0,\"last_mid\":0.44,"));
    // The stdout line itself never grows keys (§5 pinned).
    assert!(!out_a.schema1.contains("detail_version"));
    // The summary names the sidecar path.
    assert!(out_a.summary.contains("--emit-detail: sidecar written to"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pnl_reruns_are_byte_identical() {
    let root = unique_root("pnldet");
    let ruleset = build_pnl_capture(&root);
    let c = cfg(&ruleset, &root, "70/30");

    let a = cli::backtest::run(&c).expect("first run ok");
    let b = cli::backtest::run(&c).expect("second run ok");
    assert_eq!(a.schema1, b.schema1);
    assert_eq!(a.summary, b.summary);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn real_binary_pnl_fixture_frozen_argv() {
    // The worker argv over the P&L fixture: exit 0, stdout = the
    // known-P&L schema-1 line alone.
    let root = unique_root("pnlbin");
    let ruleset = build_pnl_capture(&root);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_multivenue-engine"))
        .args([
            "backtest",
            "--ruleset",
            ruleset.to_str().expect("utf8 path"),
            "--replay-dir",
            root.to_str().expect("utf8 path"),
            "--split",
            "70/30",
        ])
        .output()
        .expect("spawn harness binary");

    assert!(
        out.status.success(),
        "harness must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("stdout utf8"),
        format!("{}\n", pnl_schema1())
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------
// The COMMITTED fixture for the Python real-harness test (§12 H-D8).
// claude-worker/tests/fixtures/backtest-real/ holds byte-exact copies
// of the P&L capture + ruleset; `test_backtest_real.py` replays them
// through the frozen `run_backtest` against the release binary.
// ---------------------------------------------------------------

fn committed_fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../claude-worker/tests/fixtures/backtest-real")
}

/// Every file of the P&L fixture, relative to its root.
const PNL_FIXTURE_FILES: [&str; 5] = [
    "run-1000000000/bn-ticks.pmlr",
    "run-1000000000/pm-ticks.pmlr",
    "run-172799200000000/bn-ticks.pmlr",
    "run-172799200000000/pm-ticks.pmlr",
    "golden-ruleset.json",
];

#[test]
fn committed_python_fixture_matches_the_generator_byte_for_byte() {
    // Drift guard: the committed copy must equal what
    // `build_pnl_capture` writes (PmlrWriter output is deterministic).
    // If this fails after an intentional fixture change, rerun
    // `regenerate_committed_python_fixture -- --ignored`.
    let committed = committed_fixture_dir();
    let root = unique_root("pnlcommit");
    let _ = build_pnl_capture(&root);

    for rel in PNL_FIXTURE_FILES {
        let generated = std::fs::read(root.join(rel)).expect("generated file");
        let checked_in = std::fs::read(committed.join(rel)).unwrap_or_else(|e| {
            panic!(
                "committed fixture {} missing/unreadable ({e}) — run the ignored \
                 regenerate_committed_python_fixture test once",
                committed.join(rel).display()
            )
        });
        assert_eq!(generated, checked_in, "{rel} drifted from the generator");
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// One-shot generator for the committed copy. `#[ignore]`d: run
/// manually after an intentional fixture change —
/// `cargo test -p cli --test backtest_harness regenerate_committed -- --ignored`.
#[test]
#[ignore]
fn regenerate_committed_python_fixture() {
    let committed = committed_fixture_dir();
    std::fs::create_dir_all(&committed).expect("mkdir committed fixture dir");
    let _ = build_pnl_capture(&committed);
    eprintln!("committed fixture regenerated at {}", committed.display());
}

// ---------------------------------------------------------------
// VM2 V5: multi-channel replay, warmup, positions, D-7 mark fills
// ---------------------------------------------------------------

/// okx-namespaced pair syms + their §9.4 descriptors (the v2 rows
/// resolve through the fixture manifest).
const V5_SYM: u32 = (2 << 24) | 1; // okx:BTC-USDT-SWAP
const V5_REF: u32 = (1 << 24) | 600; // binance-usdm:btcusdt

fn v5_write_manifest(run: &Path) {
    std::fs::write(
        run.join("instrument-manifest.tsv"),
        format!(
            "{V5_SYM}\tokx:BTC-USDT-SWAP\n{V5_REF}\tbinance-usdm:btcusdt\n{}\tderibit:BTC-27MAR26-60000-C\n",
            (3u32 << 24) | 700
        ),
    )
    .expect("manifest");
}

/// Deep displayed sizes: the hand-computed fixtures fill each leg in
/// ONE crossing tick (the shared `mk_tick`'s 10-unit book would
/// partial-fill the $9 900 legs).
fn v5_tick(ts_ns: u64, venue: VenueId, sym: u32, seq: u32, bid: i64, ask: i64) -> Tick {
    Tick::new(
        ts_ns,
        venue,
        sym,
        seq,
        Price::from_raw(bid),
        Qty::from_raw(100_000_000_000_000),
        Price::from_raw(ask),
        Qty::from_raw(100_000_000_000_000),
    )
}

fn v5_cfg(ruleset: &Path, replay: &Path, split: &str) -> BacktestConfig {
    BacktestConfig {
        ruleset: ruleset.to_path_buf(),
        replay_dir: replay.to_path_buf(),
        split: split.to_owned(),
        fee_bps: vec![
            "okx:0:0".to_owned(),
            "bn:0:0".to_owned(),
            "deribit:0:0".to_owned(),
        ],
        latency_ns: Some(0),
        latency_ns_venue: Vec::new(),
        emit_detail: None,
    }
}

/// The V5 golden: one v2 POSITION pair row (xv shape) driven through
/// entry (two legs), reversion and exit (two closers) with zero fees,
/// zero latency and crossed fill fixtures placed so every fill lands
/// at its resting price with mark == fill px — the WHOLE accounting
/// is hand-computed: net = (0.75 − 0.505) × 13 200 = $3 234 exactly,
/// dd 0, legs 4, round_trips 1.
#[test]
fn v5_position_round_trip_hand_computed_exact() {
    let root = unique_root("v5-rt");
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));
    std::fs::create_dir_all(&run0).expect("mkdir");
    v5_write_manifest(&run0);

    let ruleset = root.join("xv2.json");
    std::fs::write(
        &ruleset,
        r#"{"rows":[{"name":"xv2","instrument":"okx:BTC-USDT-SWAP","ref":"binance-usdm:btcusdt","feature":"mid","combine":"diff_bps","enter":4000.0,"abs":true,"exit":100.0,"horizon_ms":10,"max_risk_usd":9900.0}]}"#,
    )
    .expect("ruleset");

    // Interleaved pair ticks (one file per venue; ts strictly
    // ordered):
    //  t1 ref  .49/.51        (mid .50 — books live)
    //  t2 sym  .74/.76        (mid .75, dev +5000 bps ⇒ ENTER:
    //                          Ask sym@.75 ×13200, Bid ref@.50 ×19800)
    //  t3 sym  .755/.745      (crossed; fills sym Ask @.75, mark .75)
    //  t4 ref  .505/.495      (crossed; fills ref Bid @.50, mark .50)
    //  t5 sym  .51/.50        (mid .505, dev +100 bps ⇒ EXIT: emits
    //                          Bid sym@.505, Ask ref@.50)
    //  t6 sym  .51/.50        (fills sym closer @.505)
    //  t7 ref  .505/.495      (fills ref closer @.50)
    write_ticks(
        &run0,
        "okx",
        EPOCH_RUN_0,
        &[
            v5_tick(2_000, VenueId::Okx, V5_SYM, 1, 740_000, 760_000),
            v5_tick(3_000, VenueId::Okx, V5_SYM, 2, 755_000, 745_000),
            v5_tick(5_000, VenueId::Okx, V5_SYM, 3, 510_000, 500_000),
            v5_tick(6_000, VenueId::Okx, V5_SYM, 4, 510_000, 500_000),
        ],
    );
    write_ticks(
        &run0,
        "bn",
        EPOCH_RUN_0,
        &[
            v5_tick(1_000, VenueId::Binance, V5_REF, 1, 490_000, 510_000),
            v5_tick(4_000, VenueId::Binance, V5_REF, 2, 505_000, 495_000),
            v5_tick(7_000, VenueId::Binance, V5_REF, 3, 505_000, 495_000),
        ],
    );

    let out = cli::backtest::run(&v5_cfg(&ruleset, &root, "0/100")).expect("harness ok");
    let s = out.stats;
    assert_eq!(s.position_rows, 1);
    assert_eq!(s.vm_orders_emitted, 4, "two entry legs + two closers");
    assert_eq!(s.fills_total, 4);
    assert_eq!(s.oos_round_trips, 1);
    assert_eq!(s.oos_net_pnl_1e6, 3_234_000_000, "hand-computed $3 234");
    assert_eq!(s.oos_max_drawdown_1e6, 0, "marks pinned to fills ⇒ dd 0");
    assert_eq!(s.max_order_notional_1e6, 9_900_000_000);
    assert_eq!(s.max_symbol_notional_1e6, 9_900_000_000);
    assert_eq!(s.max_total_notional_1e6, 19_800_000_000);
    // The frozen stdout line, byte for byte (additive keys included).
    let digest = core_crypto::sha256(
        std::fs::read(&ruleset).expect("ruleset bytes").as_slice(),
    );
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        out.schema1,
        format!(
            "{{\"schema_version\":1,\"ruleset_hash\":\"{hex}\",\"split\":\"0/100\",\
             \"oos\":{{\"net_pnl_usd\":3234.0,\"trades\":4,\"trading_days\":1,\
             \"max_drawdown_usd\":0.0,\"round_trips\":1,\"legs\":4}},\
             \"bounds\":{{\"max_order_notional_usd\":9900.0,\
             \"max_symbol_notional_usd\":9900.0,\"max_total_notional_usd\":19800.0}},\
             \"position_rows\":1}}"
        )
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Warmup (§1.5 as refined at V5): a windowed row trades NOTHING
/// until its longest referenced window has filled — the same
/// condition inside the warmup fires zero times.
#[test]
fn v5_warmup_gates_entries_until_window_filled() {
    let root = unique_root("v5-warm");
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));
    std::fs::create_dir_all(&run0).expect("mkdir");
    v5_write_manifest(&run0);

    // roll_mean(10 min) ≥ 0 refire row: fires on ANY present mean.
    let ruleset = root.join("roll.json");
    std::fs::write(
        &ruleset,
        r#"{"rows":[{"name":"roll","instrument":"okx:BTC-USDT-SWAP","feature":"roll_mean","window_min":10,"enter":0.0,"horizon_ms":86400000,"max_risk_usd":100.0,"side":"bid"}]}"#,
    )
    .expect("ruleset");

    // One funding event teaches the wall offset (rolling windows are
    // wall-minute concepts); a depth snapshot rides along to pin the
    // multi-channel merge count.
    let ev = core_types::ChannelEvent::new(
        500,
        VenueId::Okx,
        core_types::ChannelId::Funding,
        V5_SYM,
        0,
        1_787_961_600_000,
        100_000_000,
        1_787_990_400_000,
    );
    let path = run0.join("okx-events.pmlr");
    let mut w =
        PmlrWriter::open(&path, SlotKind::Event, EPOCH_RUN_0).expect("open events");
    w.append(&ev).expect("append");
    w.flush().expect("flush");
    let mut bids = [core_types::DepthLevel::EMPTY; core_types::DEPTH_K];
    bids[0] = core_types::DepthLevel {
        px_1e6: 500_000,
        qty_1e6: 1_000_000,
    };
    let mut asks = [core_types::DepthLevel::EMPTY; core_types::DEPTH_K];
    asks[0] = core_types::DepthLevel {
        px_1e6: 502_000,
        qty_1e6: 1_000_000,
    };
    let d = core_types::DepthTopK::new(600, VenueId::Okx, V5_SYM, 0, bids, asks);
    let dpath = run0.join("okx-depth.pmlr");
    let mut w = PmlrWriter::open(&dpath, SlotKind::Depth, EPOCH_RUN_0).expect("open depth");
    w.append(&d).expect("append");
    w.flush().expect("flush");

    // Ticks: inside the 10-min warmup (condition true — mean would
    // exist) and past it.
    write_ticks(
        &run0,
        "okx",
        EPOCH_RUN_0,
        &[
            mk_tick(1_000, VenueId::Okx, V5_SYM, 1, 499_000, 501_000),
            mk_tick(60_000_000_000, VenueId::Okx, V5_SYM, 2, 499_000, 501_000),
            mk_tick(660_000_000_000, VenueId::Okx, V5_SYM, 3, 499_000, 501_000),
        ],
    );

    let out = cli::backtest::run(&v5_cfg(&ruleset, &root, "0/100")).expect("harness ok");
    let s = out.stats;
    assert_eq!(s.merged_events, 1);
    assert_eq!(s.merged_depths, 1);
    assert_eq!(
        s.warmup_end_virt_ns,
        s.first_virt_ns + 600_000_000_000,
        "warmup = the longest referenced window (10 min)"
    );
    assert_eq!(
        s.vm_fires, 1,
        "the in-warmup condition (ticks 1–2) fired nothing; tick 3 fired"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// D-7: an options row (no tick lane, mark-bearing OptSummary) prices
/// at mark and FILLS under the mark law — taker at mark ± h.
#[test]
fn v5_option_mark_fill_law_executes_and_counts() {
    let root = unique_root("v5-opt");
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));
    std::fs::create_dir_all(&run0).expect("mkdir");
    v5_write_manifest(&run0);
    let opt_sym = (3u32 << 24) | 700;

    let ruleset = root.join("iv.json");
    std::fs::write(
        &ruleset,
        r#"{"rows":[{"name":"iv","instrument":"deribit:BTC-27MAR26-60000-C","feature":"mark_iv","enter":0.0,"horizon_ms":86400000,"max_risk_usd":100.0,"side":"bid"}]}"#,
    )
    .expect("ruleset");

    // A tick anchor on another venue (runs need ≥ 1 tick file) …
    write_ticks(
        &run0,
        "bn",
        EPOCH_RUN_0,
        &[mk_tick(500, VenueId::Binance, V5_REF, 1, 490_000, 510_000)],
    );
    // … and two mark-bearing option records: the row fires at the
    // first (Mid = mark 0.05), rests Bid @50 000, and mark-fills at
    // the second (mark 0.04 ⇒ fill @ 40 000 + h, h = max(0.5%, 1) =
    // 200).
    let mk_opt = |ts: u64, mark_1e9: i64| {
        core_types::OptSummary::new(
            ts,
            VenueId::Deribit,
            opt_sym,
            core_types::OPT_SUMMARY_FLAG_MARK_PX,
            mark_1e9,
            650_000_000,
            65_000_000_000_000,
            0,
            500_000_000,
            1,
            1,
            -1,
        )
    };
    let opath = run0.join("deribit-opt-summary.pmlr");
    let mut w =
        PmlrWriter::open(&opath, SlotKind::OptSummary, EPOCH_RUN_0).expect("open opt");
    w.append(&mk_opt(1_000, 50_000_000)).expect("append");
    w.append(&mk_opt(2_000, 40_000_000)).expect("append");
    w.flush().expect("flush");

    let out = cli::backtest::run(&v5_cfg(&ruleset, &root, "0/100")).expect("harness ok");
    let s = out.stats;
    assert_eq!(s.merged_opts, 2);
    assert_eq!(s.opt_synth_ticks, 2, "both marks synthesized ticks");
    assert_eq!(s.vm_orders_emitted, 1, "priced at Mid = mark");
    assert_eq!(s.mark_fills, 1, "the D-7 law executed");
    assert_eq!(s.fills_total, 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// The §6 replay half: option/instrument ordinals that reshuffle
/// across runs evaluate as ONE instrument — run-0 syms remap through
/// the manifest join onto the newest run's ids.
#[test]
fn v5_cross_run_manifest_rebind_unifies_syms() {
    let root = unique_root("v5-rebind");
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));
    let run1 = root.join(format!("run-{EPOCH_RUN_1}"));
    std::fs::create_dir_all(&run0).expect("mkdir");
    std::fs::create_dir_all(&run1).expect("mkdir");
    // Same descriptor, RESHUFFLED sym across the runs.
    let old_sym = (2u32 << 24) | 9;
    std::fs::write(
        run0.join("instrument-manifest.tsv"),
        format!("{old_sym}\tokx:BTC-USDT-SWAP\n"),
    )
    .expect("m0");
    std::fs::write(
        run1.join("instrument-manifest.tsv"),
        format!("{V5_SYM}\tokx:BTC-USDT-SWAP\n"),
    )
    .expect("m1");

    let ruleset = root.join("lvl.json");
    std::fs::write(
        &ruleset,
        r#"{"rows":[{"name":"lvl","instrument":"okx:BTC-USDT-SWAP","feature":"mid","enter":0.0,"horizon_ms":10,"max_risk_usd":100.0,"side":"bid"}]}"#,
    )
    .expect("ruleset");

    write_ticks(
        &run0,
        "okx",
        EPOCH_RUN_0,
        &[mk_tick(1_000, VenueId::Okx, old_sym, 1, 499_000, 501_000)],
    );
    write_ticks(
        &run1,
        "okx",
        EPOCH_RUN_1,
        &[mk_tick(500, VenueId::Okx, V5_SYM, 1, 499_000, 501_000)],
    );

    let out = cli::backtest::run(&v5_cfg(&ruleset, &root, "0/100")).expect("harness ok");
    let s = out.stats;
    assert_eq!(s.universe_syms, 1, "one instrument across both runs");
    assert_eq!(s.remapped_syms, 1, "run-0's old ordinal remapped");
    assert_eq!(
        s.vm_fires, 2,
        "the row fires in BOTH runs — run-0's tick reached it via the rebind"
    );
    let _ = std::fs::remove_dir_all(&root);
}
