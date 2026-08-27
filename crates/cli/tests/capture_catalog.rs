// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # capture-catalog integration tests (M3)
//!
//! Fixture-driven: synthetic `run-<epoch_ns>` capture dirs written
//! with the REAL `PmlrWriter`, judged by `cli::capture_catalog` —
//! continuity (gap-free days, streaks), the harness-view acceptance
//! mirror, the monitor-view trailing-window arithmetic, sizes, and
//! JSON determinism.
//!
//! Offline-path doctrine: this test allocates freely.

use std::path::{Path, PathBuf};

use cli::capture_catalog::{
    run_catalog, CatalogConfig, DEFAULT_GAP_TOLERANCE_NS, MONITOR_FLOOR_NS, NS_PER_DAY,
};
use core_io::{PmlrWriter, SlotKind};
use core_types::{Price, Qty, Tick, VenueId};

/// 1 s in ns.
const G: u64 = 1_000_000_000;
/// Fixture base day index: 20_684 = 2026-08-19 UTC.
const D0: u64 = 20_684;

fn unique_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("catalog-{tag}-{}-{nanos}", std::process::id()))
}

fn mk_tick(ts_ns: u64, venue: VenueId, sym: u32, seq: u32) -> Tick {
    Tick::new(
        ts_ns,
        venue,
        sym,
        seq,
        Price::from_raw(490_000),
        Qty::from_raw(10_000_000),
        Price::from_raw(510_000),
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

fn catalog(root: &Path) -> cli::capture_catalog::CatalogOutput {
    run_catalog(&CatalogConfig {
        dir: root.to_path_buf(),
        gap_tolerance_ns: DEFAULT_GAP_TOLERANCE_NS,
    })
    .expect("catalog runs")
}

/// Three synthetic full UTC days: one run per day, booted 10 s after
/// midnight, ticking until 10 s before the next midnight (dark 20 s
/// per day boundary — well under the 300 s tolerance).
fn build_three_full_days(root: &Path) {
    for i in 0..3u64 {
        let day_start = (D0 + i) * NS_PER_DAY;
        let epoch = day_start + 10 * G;
        let run = root.join(format!("run-{epoch}"));
        std::fs::create_dir_all(&run).expect("mkdir run");
        let ts_first = 1_000u64;
        let ts_last = ts_first + (NS_PER_DAY - 20 * G);
        write_ticks(
            &run,
            "pm",
            epoch,
            &[
                mk_tick(ts_first, VenueId::Polymarket, 42, 1),
                mk_tick(ts_last, VenueId::Polymarket, 42, 2),
            ],
        );
    }
}

#[test]
fn three_full_days_are_gap_free_and_whole_root_backtestable() {
    let root = unique_root("3days");
    std::fs::create_dir_all(&root).expect("mkdir root");
    build_three_full_days(&root);
    // A non-tick capture file rides along in run[0]: counted in
    // `other_files` + bytes, never parsed.
    let first_epoch = D0 * NS_PER_DAY + 10 * G;
    std::fs::write(
        root.join(format!("run-{first_epoch}")).join("pm-events.pmlr"),
        b"opaque-other-channel",
    )
    .expect("write other file");

    let out = catalog(&root);
    assert_eq!(out.facts.runs, 3);
    assert_eq!(out.facts.harness_ok_runs, 3);
    assert_eq!(out.facts.ticks, 6);
    assert_eq!(out.facts.capture_utc_days, 3);
    assert_eq!(out.facts.gap_free_days, 3);
    assert_eq!(out.facts.longest_streak, 3);
    assert_eq!(out.facts.trailing_streak, 3);
    assert_eq!(out.facts.gaps, 2, "two 20 s midnight gaps");
    assert_eq!(out.facts.overlaps, 0);
    assert!(out.facts.whole_root_backtestable);
    assert!(out.facts.days_gate_coverage_sufficient);
    // Monitor: last run alone covers ~24 h of the trailing window.
    assert!(out.facts.monitor_would_run);
    assert!(out.facts.monitor_coverage_ns >= MONITOR_FLOOR_NS);
    assert_eq!(out.facts.monitor_selected_runs, 1);

    assert!(out.json.contains("\"catalog_version\":1"));
    assert!(out.json.contains("\"date\":\"2026-08-19\""));
    assert!(out.json.contains("\"date\":\"2026-08-21\""));
    assert!(out.json.contains("\"streak_end_date\":\"2026-08-21\""));
    assert!(out.json.contains("\"other_files\":1"));
    assert!(out.json.contains("\"gap_free\":true"));
    assert!(!out.json.contains("\"gap_free\":false"));
    assert!(out.summary.contains("GAP-FREE"));
    assert!(out.summary.contains("monitor would RUN"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn catalog_json_is_deterministic() {
    let root = unique_root("determ");
    std::fs::create_dir_all(&root).expect("mkdir root");
    build_three_full_days(&root);
    let a = catalog(&root);
    let b = catalog(&root);
    assert_eq!(a.json, b.json, "byte-identical across invocations");
    assert_eq!(a.summary, b.summary);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn short_partial_day_is_gapped_and_monitor_skips() {
    let root = unique_root("partial");
    std::fs::create_dir_all(&root).expect("mkdir root");
    // One 2 h run at 01:00 UTC: day 22 h dark, monitor below floor.
    let epoch = D0 * NS_PER_DAY + 3_600 * G;
    let run = root.join(format!("run-{epoch}"));
    std::fs::create_dir_all(&run).expect("mkdir run");
    write_ticks(
        &run,
        "bn",
        epoch,
        &[
            mk_tick(500, VenueId::Binance, 7, 1),
            mk_tick(500 + 7_200 * G, VenueId::Binance, 7, 2),
        ],
    );

    let out = catalog(&root);
    assert_eq!(out.facts.runs, 1);
    assert_eq!(out.facts.capture_utc_days, 1);
    assert_eq!(out.facts.gap_free_days, 0);
    assert_eq!(out.facts.longest_streak, 0);
    assert_eq!(out.facts.trailing_streak, 0);
    assert!(!out.facts.days_gate_coverage_sufficient, "1 day < min 2");
    assert!(!out.facts.monitor_would_run, "2 h < 6 h floor");
    assert!(out.facts.whole_root_backtestable, "clean, just short");
    assert!(out.json.contains("\"streak_end_date\":null"));
    assert!(out.json.contains("\"gap_free\":false"));
    assert!(out.summary.contains("monitor would SKIP"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn harness_rejections_are_reported_per_file() {
    let root = unique_root("reject");
    std::fs::create_dir_all(&root).expect("mkdir root");
    // run[0]: pm written under a MISMATCHED header epoch + a junk bn
    // file — both §3.1 rejections the harness would refuse.
    let epoch_bad = D0 * NS_PER_DAY + 10 * G;
    let bad = root.join(format!("run-{epoch_bad}"));
    std::fs::create_dir_all(&bad).expect("mkdir bad");
    write_ticks(
        &bad,
        "pm",
        epoch_bad + 1, // header epoch != dir epoch
        &[mk_tick(1_000, VenueId::Polymarket, 42, 1)],
    );
    std::fs::write(bad.join("bn-ticks.pmlr"), b"not-a-pmlr-file").expect("junk");
    // run[1]: clean.
    let epoch_ok = epoch_bad + 3_600 * G;
    let ok = root.join(format!("run-{epoch_ok}"));
    std::fs::create_dir_all(&ok).expect("mkdir ok");
    write_ticks(
        &ok,
        "pm",
        epoch_ok,
        &[
            mk_tick(1_000, VenueId::Polymarket, 42, 1),
            mk_tick(1_000 + 600 * G, VenueId::Polymarket, 42, 2),
        ],
    );

    let out = catalog(&root);
    assert_eq!(out.facts.runs, 2);
    assert_eq!(out.facts.harness_ok_runs, 1);
    assert!(!out.facts.whole_root_backtestable);
    assert!(out.json.contains("\"note\":\"header-epoch-mismatch\""));
    assert!(out.json.contains("\"note\":\"unreadable-header\""));
    assert!(out.summary.contains("harness=REJECT"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn single_run_dir_argument_is_the_single_run() {
    let root = unique_root("single");
    std::fs::create_dir_all(&root).expect("mkdir root");
    build_three_full_days(&root);
    let first_epoch = D0 * NS_PER_DAY + 10 * G;
    let run = root.join(format!("run-{first_epoch}"));
    let out = run_catalog(&CatalogConfig {
        dir: run,
        gap_tolerance_ns: DEFAULT_GAP_TOLERANCE_NS,
    })
    .expect("single-run catalog");
    assert_eq!(out.facts.runs, 1);
    assert_eq!(out.facts.ticks, 2);
    assert_eq!(out.facts.capture_utc_days, 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn overlapping_runs_are_flagged_and_root_refused() {
    let root = unique_root("overlap");
    std::fs::create_dir_all(&root).expect("mkdir root");
    // run A: one hour of wall span.
    let epoch_a = D0 * NS_PER_DAY + 10 * G;
    let a = root.join(format!("run-{epoch_a}"));
    std::fs::create_dir_all(&a).expect("mkdir a");
    write_ticks(
        &a,
        "pm",
        epoch_a,
        &[
            mk_tick(100, VenueId::Polymarket, 42, 1),
            mk_tick(100 + 3_600 * G, VenueId::Polymarket, 42, 2),
        ],
    );
    // run B: boots 30 min into A's span — wall overlap.
    let epoch_b = epoch_a + 1_800 * G;
    let b = root.join(format!("run-{epoch_b}"));
    std::fs::create_dir_all(&b).expect("mkdir b");
    write_ticks(
        &b,
        "pm",
        epoch_b,
        &[
            mk_tick(100, VenueId::Polymarket, 42, 1),
            mk_tick(100 + 600 * G, VenueId::Polymarket, 42, 2),
        ],
    );

    let out = catalog(&root);
    assert_eq!(out.facts.runs, 2);
    assert_eq!(out.facts.overlaps, 1);
    assert!(!out.facts.whole_root_backtestable);
    assert!(out.summary.contains("OVERLAPS"));
    let _ = std::fs::remove_dir_all(&root);
}
