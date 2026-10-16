// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.11 — `multivenue-engine pine prepare`: the two sources end to end on synthetic
//! inputs, and the refusals. The `sqlite3` child itself runs on the Mac (no `sqlite3` binary in
//! the cloud box): these tests drive `--candles-tsv` and every `--candles-db` refusal that fires
//! before a child would run.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use core_io::{PmlrWriter, SlotKind};
use core_types::{ChannelEvent, ChannelId, VenueId};
use pine_ir::barflag;
use pine_screen::bars::bar_source;
use pine_screen::{bar_path, BarFile};

const BIN: &str = env!("CARGO_BIN_EXE_multivenue-engine");
/// Monday 2026-09-07T00:00:00Z.
const MON: i64 = 1_788_739_200_000;
const H: i64 = 3_600_000;
const D: i64 = 86_400_000;
const KEY: &str = "binance_usdm:btcusdt";
const DESC: &str = "binance-usdm:btcusdt";

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("pine_prepare_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn universe(dir: &Path, desc: &str, cdesc: &str) -> PathBuf {
    let p = dir.join("screen-universe.tsv");
    std::fs::write(
        &p,
        format!(
            "# key descriptor capture_descriptor venue class fee_venue mintick pointvalue qty_step min_notional ticker base currency type\n\
             {KEY}\t{desc}\t{cdesc}\t1\tperp\tbn\t100000\t1000000\t1000\t5000000\tBTCUSDT\tBTC\tUSDT\tcrypto\n"
        ),
    )
    .unwrap();
    p
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("pine")
        .arg("prepare")
        .args(args)
        .output()
        .unwrap()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// `n` rows of `step_ms` from `from`, skipping the row indices in `skip`.
fn candle_tsv(from: i64, step_ms: i64, n: i64, skip: &[i64]) -> String {
    let mut t = String::from("# open_ts\to\th\tl\tc\tv\n");
    for i in 0..n {
        if skip.contains(&i) {
            continue;
        }
        // An exponent-form REAL (the shell may print one) and a NULL-volume mid row that must
        // never become a bar (it sits at an open of its own, off the window's grid, below).
        t.push_str(&format!(
            "{}\t100.5\t101\t1.0e2\t100.25\t1.5\n",
            from + i * step_ms
        ));
    }
    t
}

fn write_tsv_tree(dir: &Path) {
    let d = dir.join(DESC);
    std::fs::create_dir_all(&d).unwrap();
    // 14 days of 1 h with one hole (row 5), 14 days of 4 h, 21 days of 1 d (the window cuts 7),
    // plus a mid row (empty volume) in the 1 d file.
    std::fs::write(d.join("1h.tsv"), candle_tsv(MON, H, 14 * 24, &[5])).unwrap();
    std::fs::write(d.join("4h.tsv"), candle_tsv(MON, 4 * H, 14 * 6, &[])).unwrap();
    let mut daily = candle_tsv(MON, D, 21, &[]);
    daily.push_str(&format!("{}\t1\t1\t1\t1\t\n", MON + 30 * D));
    std::fs::write(d.join("1d.tsv"), daily).unwrap();
}

#[test]
fn pine_prepare_candles_tsv_end_to_end() {
    let dir = tmp("tsv");
    write_tsv_tree(&dir.join("src"));
    let uni = universe(&dir, DESC, "-");
    let out = dir.join("bars");
    let to = (MON + 14 * D).to_string();
    let from = MON.to_string();
    let src = dir.join("src");
    let args = [
        "--out",
        s(&out),
        "--universe",
        s(&uni),
        "--timeframes",
        "3600,7200,14400,86400,604800",
        "--from-ms",
        &from,
        "--to-ms",
        &to,
        "--candles-tsv",
        s(&src),
        "--open-ts-unit",
        "ms",
    ];
    let o = run(&args);
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        o.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&o.stderr)
    );
    // The row counts are printed per (key, tf).
    assert!(
        stdout.contains(&format!(
            "prepare\t{KEY}\t3600\tsrc=1h\tsrc_rows=335\trows=335\tholes=1\t"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "prepare\t{KEY}\t7200\tsrc=1h\tsrc_rows=335\trows=167\tholes=1\tdropped_partial=1\t"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "prepare\t{KEY}\t14400\tsrc=4h\tsrc_rows=84\trows=84\t"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "prepare\t{KEY}\t86400\tsrc=1d\tsrc_rows=14\trows=14\t"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "prepare\t{KEY}\t604800\tsrc=1d\tsrc_rows=14\trows=2\tholes=0\tdropped_partial=0\t"
        )),
        "{stdout}"
    );

    // 1 w: Monday buckets, the sum of seven daily volumes, high/low/open/close folded.
    let w = BarFile::open(&bar_path(&out, KEY, 604_800).unwrap()).unwrap();
    assert_eq!(w.hdr().source, bar_source::CANDLES);
    assert_eq!(w.hdr().venue, 1);
    assert_eq!(w.hdr().symbol_hash, pine_ir::symbol_hash(KEY.as_bytes()));
    let r = w.rows();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].open_ms, MON);
    assert_eq!(r[1].open_ms, MON + 7 * D);
    assert_eq!(r[0].volume_1e6, 7 * 1_500_000);
    assert_eq!(
        (r[0].open_1e6, r[0].high_1e6, r[0].low_1e6, r[0].close_1e6),
        (100_500_000, 101_000_000, 100_000_000, 100_250_000)
    );
    assert!(r[0].flags & barflag::CONFIRMED != 0);
    // 2 h: the bucket holding the 1 h hole is dropped and becomes a hole.
    let h2 = BarFile::open(&bar_path(&out, KEY, 7_200).unwrap()).unwrap();
    assert_eq!(h2.rows()[2].open_ms, MON + 6 * H);
    assert!(h2.rows()[2].flags & barflag::GAP_BEFORE != 0);
    assert_eq!(h2.rows()[0].volume_1e6, 3_000_000);

    // Idempotence (I5): the second pass writes nothing.
    let before = std::fs::read(bar_path(&out, KEY, 3_600).unwrap()).unwrap();
    let o2 = run(&args);
    let stdout2 = String::from_utf8_lossy(&o2.stdout);
    assert!(o2.status.success());
    assert_eq!(stdout2.matches("\tunchanged").count(), 5, "{stdout2}");
    assert_eq!(
        std::fs::read(bar_path(&out, KEY, 3_600).unwrap()).unwrap(),
        before
    );

    // --json carries the pvm7-2 envelope.
    let mut jargs = args.to_vec();
    jargs.push("--json");
    let o3 = run(&jargs);
    assert!(String::from_utf8_lossy(&o3.stdout)
        .starts_with("{\"schema\":\"pvm7-2\",\"verb\":\"prepare\""));
}

#[test]
fn pine_prepare_refuses_the_zero_byte_database() {
    let dir = tmp("zero");
    let db = dir.join("worker/candles.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, b"").unwrap();
    let uni = universe(&dir, DESC, "-");
    let out = dir.join("bars");
    let o = run(&[
        "--out",
        s(&out),
        "--universe",
        s(&uni),
        "--timeframes",
        "3600",
        "--to-ms",
        &(MON + D).to_string(),
        "--candles-db",
        s(&db),
        "--open-ts-unit",
        "ms",
    ]);
    assert_eq!(o.status.code(), Some(5));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("7001 zero-byte source"), "{err}");
    assert!(!out.exists(), "nothing written");
}

#[test]
fn pine_prepare_refuses_a_path_outside_worker_candles_db() {
    let dir = tmp("path");
    let db = dir.join("other/candles.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, b"SQLite format 3\0").unwrap();
    let uni = universe(&dir, DESC, "-");
    let out = dir.join("bars");
    let o = run(&[
        "--out",
        s(&out),
        "--universe",
        s(&uni),
        "--timeframes",
        "3600",
        "--to-ms",
        &(MON + D).to_string(),
        "--candles-db",
        s(&db),
        "--open-ts-unit",
        "ms",
    ]);
    assert_eq!(o.status.code(), Some(5));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("not a worker/candles.db path"), "{err}");
    assert!(!out.exists());
}

#[test]
fn pine_prepare_refuses_open_ts_of_the_wrong_unit() {
    let dir = tmp("unit");
    write_tsv_tree(&dir.join("src"));
    let uni = universe(&dir, DESC, "-");
    let out = dir.join("bars");
    let o = run(&[
        "--out",
        s(&out),
        "--universe",
        s(&uni),
        "--timeframes",
        "3600",
        "--to-ms",
        &(MON + D).to_string(),
        "--candles-tsv",
        s(&dir.join("src")),
        "--open-ts-unit",
        "s",
    ]);
    assert_eq!(o.status.code(), Some(5));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("7001 open_ms off the barclock grid row"),
        "{err}"
    );
    assert!(!out.exists());
    // And an argv refusal (exit 2) without the unit, two sources, a timeframe outside the list.
    for bad in [
        vec!["--candles-tsv", "x"],
        vec![
            "--candles-tsv",
            "x",
            "--trades",
            "y",
            "--open-ts-unit",
            "ms",
        ],
    ] {
        let mut a = vec![
            "--out",
            s(&out),
            "--universe",
            s(&uni),
            "--timeframes",
            "3600",
            "--to-ms",
            "1788825600000",
        ];
        a.extend(bad);
        assert_eq!(run(&a).status.code(), Some(2));
    }
    let o = run(&[
        "--out",
        s(&out),
        "--universe",
        s(&uni),
        "--timeframes",
        "3601",
        "--to-ms",
        "1788825600000",
        "--candles-tsv",
        "x",
        "--open-ts-unit",
        "ms",
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert!(!out.exists());
}

#[test]
fn pine_prepare_sqlite_is_readonly_and_inlined_safely() {
    let dir = tmp("quote");
    let db = dir.join("worker/candles.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, b"SQLite format 3\0").unwrap();
    let uni = universe(&dir, "binance-usdm:btc'usdt", "-");
    let out = dir.join("bars");
    let o = run(&[
        "--out",
        s(&out),
        "--universe",
        s(&uni),
        "--timeframes",
        "3600",
        "--to-ms",
        &(MON + D).to_string(),
        "--candles-db",
        s(&db),
        "--open-ts-unit",
        "ms",
    ]);
    assert_eq!(o.status.code(), Some(5));
    let err = String::from_utf8_lossy(&o.stderr);
    // Refused before any child runs: no spawn error, no sqlite3 exit status.
    assert!(err.contains("outside [a-z0-9:_./-]+"), "{err}");
    assert!(!err.contains("sqlite3"), "{err}");
    assert!(!out.exists());
}

/// Two runs of a capture: `bn-events.pmlr` trade prints at 1 per 10 s with venue time, the
/// second run carrying a few prints with `venue_time_ms == 0` (the wall fallback).
fn write_capture(root: &Path, sym: u32) -> (i64, i64) {
    let t0 = MON + 9 * H; // run 1 wall start
    let t1 = t0 + 20 * 60_000 + 15_000; // run 2 starts 20 min 15 s later
    for (k, (start, n)) in [(t0, 61i64), (t1, 60i64)].into_iter().enumerate() {
        let epoch_ns = (start as u64) * 1_000_000;
        let run = root.join(format!("run-{epoch_ns}"));
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(
            run.join("instrument-manifest.tsv"),
            format!("{sym}\t{DESC}\n3\tbinance-usdm:ethusdt\n"),
        )
        .unwrap();
        let mut w =
            PmlrWriter::open(run.join("bn-events.pmlr"), SlotKind::Event, epoch_ns).unwrap();
        let ts_base = 5_000_000_000u64; // the run's monotonic base: rebased to the epoch
        for i in 0..n {
            let wall_ms = start + i * 10_000;
            let ts = ts_base + (i as u64) * 10_000_000_000;
            let vt = if k == 1 && i % 20 == 3 {
                0
            } else {
                wall_ms as u64
            };
            let ev = ChannelEvent::new(
                ts,
                VenueId::Binance,
                ChannelId::Trade,
                sym,
                1_000 + i as u64,
                vt,
                100_000_000 + i * 1_000,
                2_000_000,
            );
            w.append(&ev).unwrap();
            // Another instrument's print and a non-trade row: never in the bars.
            w.append(&ChannelEvent::new(
                ts,
                VenueId::Binance,
                ChannelId::Trade,
                3,
                0,
                wall_ms as u64,
                1,
                1,
            ))
            .unwrap();
        }
        w.flush().unwrap();
    }
    (t0, t1)
}

#[test]
fn pine_prepare_trades_from_a_synthetic_capture() {
    let dir = tmp("trades");
    let (t0, t1) = write_capture(&dir.join("cap"), 7);
    let uni = universe(&dir, "-", DESC);
    let out = dir.join("tbars");
    let o = run(&[
        "--out",
        s(&out),
        "--universe",
        s(&uni),
        "--timeframes",
        "60",
        "--to-ms",
        &(MON + D).to_string(),
        "--trades",
        s(&dir.join("cap")),
        "--trades-label",
        "bn",
    ]);
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        o.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&o.stderr)
    );
    // Run 1 covers [t0, t0 + 600 s]: 10 full minutes; run 2 starts 15 s into a minute, so its
    // first bucket straddles the restart and is dropped; it covers to t1 + 590 s: 9 full minutes.
    let f = BarFile::open(&bar_path(&out, KEY, 60).unwrap()).unwrap();
    assert_eq!(f.hdr().source, bar_source::TRADES);
    let r = f.rows();
    assert_eq!(r.len(), 10 + 9, "{stdout}");
    assert_eq!(r[0].open_ms, t0);
    assert_eq!(r[9].open_ms, t0 + 9 * 60_000);
    assert_eq!(r[10].open_ms, t1 - 15_000 + 60_000);
    assert!(r[10].flags & barflag::GAP_BEFORE != 0);
    // Six prints a minute, |qty| 2 each; open/close the first/last print of the bucket.
    assert_eq!(r[0].volume_1e6, 6 * 2_000_000);
    assert_eq!(r[0].open_1e6, 100_000_000);
    assert_eq!(r[0].close_1e6, 100_005_000);
    assert!(stdout.contains("trades\truns=2\truns_pre_v3=0\truns_without_instrument=0\twall_fallbacks=3\tlate_prints=0"), "{stdout}");
    assert!(stdout.contains("\tdropped_partial="), "{stdout}");
}

#[test]
fn pine_prepare_keeps_one_source_per_root() {
    let dir = tmp("onesrc");
    // Candles first, at 60 s, into `bars`.
    let d = dir.join("src").join(DESC);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("1m.tsv"), candle_tsv(MON + 9 * H, 60_000, 30, &[])).unwrap();
    let uni = universe(&dir, DESC, DESC);
    let out = dir.join("bars");
    let to = (MON + D).to_string();
    let o = run(&[
        "--out",
        s(&out),
        "--universe",
        s(&uni),
        "--timeframes",
        "60",
        "--to-ms",
        &to,
        "--candles-tsv",
        s(&dir.join("src")),
        "--open-ts-unit",
        "ms",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let path = bar_path(&out, KEY, 60).unwrap();
    let before = std::fs::read(&path).unwrap();
    // Trades into the same root → exit 5, the file untouched.
    write_capture(&dir.join("cap"), 7);
    let o = run(&[
        "--out",
        s(&out),
        "--universe",
        s(&uni),
        "--timeframes",
        "60",
        "--to-ms",
        &to,
        "--trades",
        s(&dir.join("cap")),
        "--trades-label",
        "bn",
    ]);
    assert_eq!(o.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&o.stderr).contains("one source per --out root"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
