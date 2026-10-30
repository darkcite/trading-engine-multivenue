// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM B'1r T9 (impl-Bp §4.4, §7.3): the replay keep policy.
//!
//! I8 — legacy byte-identity: `backtest` without `--strategy pine` /
//! `--replay-from-boot` prints byte-identical stdout and stderr, and the
//! same `merged_records` / `boundary_virt_ns`, on every fixture root, as
//! the goldens committed under `tests/fixtures/pine-bp/legacy-goldens/`
//! (written by the pre-T9 harness; `regenerate_legacy_goldens` is the
//! `#[ignore]`d one-shot that rewrites them).
//!
//! pin:Bp-fixture-roots — "every fixture root under crates/cli/tests/":
//! the committed on-disk root (`claude-worker/tests/fixtures/backtest-real`)
//! and the roots built here (the two-run golden layout and an events root
//! carrying every event channel, Trade rows included, a print ahead of the
//! first tick). The builder-private roots of `backtest_harness.rs` /
//! `backtest_member.rs` keep their own exact goldens there.
//! pin:Bp-goldens-regime-off — the goldens pass `--regime off`: the
//! default `Auto` reads the operator's `~/multivenue/regime.toml`, which
//! would make the stderr golden host-dependent.
//!
//! Offline-path doctrine: this test allocates freely.

use std::path::{Path, PathBuf};

use core_io::{PmlrWriter, SlotKind};
use core_types::{ChannelEvent, ChannelId, Price, Qty, Tick, VenueId};

use cli::backtest::regime::RegimeMode;
use cli::backtest::BacktestConfig;

const GOLDEN_RULESET: &str = r#"{"rows":[{"name":"h1-golden","family":"crypto","trigger":{"type":"cross_deviation","ref":7},"sym":42,"side":"bid","edge_bps":80,"horizon_ms":1500,"max_risk_usd":50.0}]}"#;

const EPOCH_RUN_0: u64 = 1_000_000_000;
const EPOCH_RUN_1: u64 = 5_000_000_000;

fn goldens_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pine-bp/legacy-goldens")
}

fn committed_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../claude-worker/tests/fixtures/backtest-real")
}

fn scratch_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("cli-bpkeep-{tag}-{}-{nanos}", std::process::id()))
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

fn write_events(run_dir: &Path, label: &str, epoch_ns: u64, evs: &[ChannelEvent]) {
    let path = run_dir.join(format!("{label}-events.pmlr"));
    let mut w = PmlrWriter::open(&path, SlotKind::Event, epoch_ns).expect("open writer");
    for e in evs {
        w.append(e).expect("append event");
    }
    w.flush().expect("flush");
}

#[allow(clippy::too_many_arguments)]
fn ev(
    ts: u64,
    venue: VenueId,
    ch: ChannelId,
    sym: u32,
    seq: u64,
    t_ms: u64,
    v0: i64,
    v1: i64,
) -> ChannelEvent {
    ChannelEvent::new(ts, venue, ch, sym, seq, t_ms, v0, v1)
}

/// The two-run, two-venue golden layout (as `backtest_harness.rs`).
fn build_golden(root: &Path) -> PathBuf {
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

/// The golden layout plus event files carrying every channel the keep
/// test reads — Funding, AssetCtx, Mark (no binary underlying: dropped),
/// InstrumentRoll — and Trade rows on bn and hl, one print ahead of the
/// run's first tick (the §4.4 hazard the legacy keep must not see).
fn build_events(root: &Path) -> PathBuf {
    let ruleset = build_golden(root);
    let run0 = root.join(format!("run-{EPOCH_RUN_0}"));
    let run1 = root.join(format!("run-{EPOCH_RUN_1}"));
    write_events(
        &run0,
        "bn",
        EPOCH_RUN_0,
        &[
            ev(
                400,
                VenueId::Binance,
                ChannelId::Trade,
                7,
                11,
                1,
                560_000,
                5_000_000,
            ),
            ev(
                1_500,
                VenueId::Binance,
                ChannelId::Funding,
                7,
                0,
                2,
                100_000,
                0,
            ),
            ev(
                2_500,
                VenueId::Binance,
                ChannelId::Trade,
                7,
                12,
                3,
                561_000,
                -2_000_000,
            ),
            ev(
                3_500,
                VenueId::Binance,
                ChannelId::Mark,
                7,
                0,
                4,
                560_500,
                560_400,
            ),
        ],
    );
    write_events(
        &run0,
        "hl",
        EPOCH_RUN_0,
        &[
            ev(
                300,
                VenueId::Hyperliquid,
                ChannelId::Trade,
                9,
                71,
                1,
                64_437_500_000,
                250_000,
            ),
            ev(
                2_200,
                VenueId::Hyperliquid,
                ChannelId::AssetCtx,
                9,
                0,
                0,
                12_500,
                1_000_000,
            ),
            ev(
                2_300,
                VenueId::Hyperliquid,
                ChannelId::Mark,
                9,
                0,
                0,
                64_400_000_000,
                64_390_000_000,
            ),
        ],
    );
    write_events(
        &run1,
        "bn",
        EPOCH_RUN_1,
        &[
            ev(
                600,
                VenueId::Binance,
                ChannelId::Funding,
                7,
                0,
                5,
                110_000,
                0,
            ),
            ev(
                900,
                VenueId::Binance,
                ChannelId::Trade,
                7,
                13,
                6,
                562_000,
                1_000_000,
            ),
        ],
    );
    ruleset
}

fn cfg(ruleset: &Path, replay_dir: &Path) -> BacktestConfig {
    BacktestConfig {
        ruleset: ruleset.to_path_buf(),
        replay_dir: replay_dir.to_path_buf(),
        split: "70/30".to_owned(),
        fee_bps: Vec::new(),
        latency_ns: None,
        latency_ns_venue: Vec::new(),
        stale_after_ms: Vec::new(),
        opt_fee: Vec::new(),
        option_spread_frac_1e6: None,
        emit_detail: None,
        regime: RegimeMode::Off,
        regime_seed: None,
        funding_seed: None,
        member: None,
    }
}

/// `(name, root, ruleset, scratch?)` for every fixture root.
fn roots() -> Vec<(&'static str, PathBuf, PathBuf, bool)> {
    let committed = committed_root();
    let committed_rs = committed.join("golden-ruleset.json");
    let golden = scratch_root("golden");
    let golden_rs = build_golden(&golden);
    let events = scratch_root("events");
    let events_rs = build_events(&events);
    vec![
        ("committed-backtest-real", committed, committed_rs, false),
        ("golden-two-run", golden, golden_rs, true),
        ("events-every-channel", events, events_rs, true),
    ]
}

/// The binary on the frozen argv (+ `--regime off`); stdout, and stderr
/// with the root path normalised.
fn run_binary(root: &Path, ruleset: &Path) -> (String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_multivenue-engine"))
        .args([
            "backtest",
            "--ruleset",
            ruleset.to_str().expect("utf8"),
            "--replay-dir",
            root.to_str().expect("utf8"),
            "--split",
            "70/30",
            "--regime",
            "off",
        ])
        .output()
        .expect("spawn harness");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let root_s = root.to_str().expect("utf8");
    let stdout = String::from_utf8(out.stdout)
        .expect("utf8")
        .replace(root_s, "<ROOT>");
    let stderr = String::from_utf8(out.stderr)
        .expect("utf8")
        .replace(root_s, "<ROOT>");
    (stdout, stderr)
}

fn stats_line(root: &Path, ruleset: &Path) -> String {
    let s = cli::backtest::run(&cfg(ruleset, root))
        .expect("harness ok")
        .stats;
    format!(
        "runs={} merged_records={} first_virt_ns={} last_virt_ns={} boundary_virt_ns={} oos_records={}\n",
        s.runs, s.merged_records, s.first_virt_ns, s.last_virt_ns, s.boundary_virt_ns, s.oos_records
    )
}

fn golden(name: &str, ext: &str) -> String {
    let p = goldens_dir().join(format!("{name}.{ext}"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "{} missing ({e}) — run regenerate_legacy_goldens once",
            p.display()
        )
    })
}

fn cleanup(rows: &[(&'static str, PathBuf, PathBuf, bool)]) {
    for (_, root, _, scratch) in rows {
        if *scratch {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

#[test]
fn legacy_backtest_is_byte_identical_on_every_fixture_root() {
    let rows = roots();
    for (name, root, rs, _) in &rows {
        let (stdout, stderr) = run_binary(root, rs);
        assert_eq!(stdout, golden(name, "stdout"), "{name}: stdout");
        assert_eq!(stderr, golden(name, "stderr"), "{name}: stderr");
    }
    cleanup(&rows);
}

#[test]
fn legacy_keep_policy_reproduces_merged_records_and_boundary() {
    let rows = roots();
    for (name, root, rs, _) in &rows {
        assert_eq!(stats_line(root, rs), golden(name, "stats"), "{name}");
    }
    cleanup(&rows);
}

/// One-shot: rewrite the goldens from the CURRENT harness. Run only
/// before a deliberate legacy change (never to make a red green):
/// `cargo test -p cli --test backtest_keep regenerate -- --ignored`.
#[test]
#[ignore]
fn regenerate_legacy_goldens() {
    let dir = goldens_dir();
    std::fs::create_dir_all(&dir).expect("mkdir goldens");
    let rows = roots();
    for (name, root, rs, _) in &rows {
        let (stdout, stderr) = run_binary(root, rs);
        std::fs::write(dir.join(format!("{name}.stdout")), stdout).expect("write");
        std::fs::write(dir.join(format!("{name}.stderr")), stderr).expect("write");
        std::fs::write(dir.join(format!("{name}.stats")), stats_line(root, rs)).expect("write");
    }
    cleanup(&rows);
}
