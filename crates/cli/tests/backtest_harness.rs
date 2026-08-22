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
    std::env::temp_dir().join(format!(
        "cli-backtest-{tag}-{}-{nanos}",
        std::process::id()
    ))
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
         \"max_drawdown_usd\":0.0}},\"bounds\":{{\"max_order_notional_usd\":50.0,\
         \"max_symbol_notional_usd\":0.0,\"max_total_notional_usd\":0.0}}}}"
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
    assert_eq!(s.first_virt_ns, VIRT_T0, "run-0 first tick rebases to VIRT_T0");
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
    // have errored), row evaluated on every pm tick, fired twice
    // (cooldown swallowed the middle tick), emitted twice at $50.
    assert_eq!(s.vm_evals, 3);
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
    assert_eq!(out.stats.vm_orders_emitted, 1, "second pm tick sits in cooldown");
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
    assert_eq!(a.schema1, b.schema1, "schema-1 stdout must be bit-identical");
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
