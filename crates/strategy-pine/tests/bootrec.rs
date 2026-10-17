// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-Bp §3.9 R5 / §3.10, adopted by impl-08 §3.7: `PineBootCfg { replay, boot_record,
//! warmup }` and the boot record `pine-boot.tsv` — rendered from a started member, read back by a
//! replay's `configure` / `supply_warm`.

mod common;

use common::*;
use strategy_core::Strategy;
use strategy_pine::{BootRecord, PineBootCfg, PineStrategy, SlotRoute, WarmSlice};

fn cfg() -> PineBootCfg {
    let mut b = boot(vec![
        script("sma_always.pine", 0),
        script("sma_always.pine", 1),
    ]);
    b.manifest_sha256 = [7; 32];
    b.oid_epoch = 1_767_225_600;
    b
}

fn warm_with(d0: u8) -> Vec<WarmSlice> {
    let mut a = slice(0, 60, warm_bars(60, 50, 0));
    a.digest[..8].copy_from_slice(&[d0; 8]);
    vec![a, slice(1, 60, warm_bars(60, 50, 0))]
}

/// A started paper member and its record (written to a temp file).
fn recorded(tag: &str) -> (BootRecord, std::path::PathBuf) {
    let p = start(cfg(), warm_with(0x5a));
    let rec = p
        .boot_record(1_767_225_600_000_000_000, 123)
        .expect("started");
    let dir = std::env::temp_dir().join(format!("pvm-bootrec-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join(strategy_pine::BOOT_RECORD_FILE);
    std::fs::write(&path, rec.render()).expect("write");
    (rec, path)
}

fn replay_cfg(path: &std::path::Path) -> PineBootCfg {
    let mut b = cfg();
    b.replay = true;
    b.boot_record = Some(path.to_path_buf());
    // The replay's own epoch is overridden by the record's.
    b.oid_epoch = 9;
    b
}

#[test]
fn boot_record_is_read_back_from_a_started_member() {
    let mut fresh = PineStrategy::new();
    assert!(fresh.boot_record(1, 2).is_none(), "None before on_start");
    fresh.configure(cfg()).expect("configure");
    assert!(fresh.boot_record(1, 2).is_none(), "None before on_start");
    let (rec, _) = recorded("readback");
    assert_eq!(rec.manifest_sha256, [7; 32]);
    assert_eq!(
        (rec.run_epoch_ns, rec.boot_mono_ns),
        (1_767_225_600_000_000_000, 123)
    );
    assert_eq!(rec.route, SlotRoute::Paper);
    assert_eq!(rec.scripts.len(), 2);
    let sha = core_crypto::sha256(&std::fs::read(dir().join("sma_always.pine")).expect("src"));
    for (i, s) in rec.scripts.iter().enumerate() {
        assert_eq!(usize::from(s.idx), i);
        assert_eq!(s.sha256, sha);
        assert_eq!(
            (s.position_1e6, s.avg_px_1e6, s.day_notional_1e6),
            (0, 0, 0)
        );
        assert_eq!(
            s.client_oid_base,
            (1_767_225_600u64 << 32) | ((i as u64) << 29)
        );
    }
    // pin:08-boot-record-warm-digest-chart-row: the chart row's digest word (LE), 0 for none.
    assert_eq!(rec.scripts[0].warmup_digest, u64::from_le_bytes([0x5a; 8]));
    assert_eq!(rec.scripts[1].warmup_digest, 0);
    assert_eq!(BootRecord::parse(&rec.render()).expect("parse"), rec);
}

#[test]
fn replay_boots_from_the_record_with_its_epoch() {
    let (rec, path) = recorded("replay");
    let mut p = PineStrategy::new();
    p.configure(replay_cfg(&path)).expect("configure");
    p.supply_warm(warm_with(0x5a)).expect("warm digests match");
    p.on_start(&mut NoCtx).expect("on_start");
    // Same record out (the epoch is the record's, not the cfg's 9).
    let again = p
        .boot_record(rec.run_epoch_ns, rec.boot_mono_ns)
        .expect("started");
    assert_eq!(again, rec);
}

#[test]
fn replay_refuses_a_different_warm_up() {
    let (_, path) = recorded("warmdiff");
    let mut p = PineStrategy::new();
    p.configure(replay_cfg(&path)).expect("configure");
    let e = p.supply_warm(warm_with(0x5b)).expect_err("digest differs");
    assert_eq!(
        (e.code.0, e.key, e.script),
        (6013, "warmup_digest", Some(0))
    );
    assert_eq!(p.last_boot_error().map(|e| e.code.0), Some(6013));
}

#[test]
fn replay_refuses_a_manifest_or_script_mismatch() {
    let (rec, path) = recorded("mismatch");
    let mut b = replay_cfg(&path);
    b.manifest_sha256 = [8; 32];
    let e = PineStrategy::new().configure(b).expect_err("manifest");
    assert_eq!((e.code.0, e.key), (6001, "boot_record"));

    let mut r2 = rec.clone();
    r2.scripts[1].sha256 = [0; 32];
    std::fs::write(&path, r2.render()).expect("write");
    let e = PineStrategy::new()
        .configure(replay_cfg(&path))
        .expect_err("script");
    assert_eq!((e.code.0, e.script), (6001, Some(1)));

    let mut r3 = rec.clone();
    r3.scripts.pop();
    std::fs::write(&path, r3.render()).expect("write");
    let e = PineStrategy::new()
        .configure(replay_cfg(&path))
        .expect_err("count");
    assert_eq!(e.code.0, 6001);
}

#[test]
fn replay_refuses_a_non_flat_or_inconsistent_record() {
    let (rec, path) = recorded("flat");
    let mut r = rec.clone();
    r.scripts[0].position_1e6 = 1_000_000;
    std::fs::write(&path, r.render()).expect("write");
    let e = PineStrategy::new()
        .configure(replay_cfg(&path))
        .expect_err("non-flat");
    assert_eq!((e.code.0, e.script), (6013, Some(0)));

    let mut r = rec.clone();
    r.scripts[1].client_oid_base += 1;
    std::fs::write(&path, r.render()).expect("write");
    let e = PineStrategy::new()
        .configure(replay_cfg(&path))
        .expect_err("base");
    assert_eq!((e.code.0, e.script), (6013, Some(1)));

    std::fs::write(&path, "# pvm-boot-0\n").expect("write");
    let e = PineStrategy::new()
        .configure(replay_cfg(&path))
        .expect_err("malformed");
    assert_eq!((e.code.0, e.key), (6006, "boot_record"));
    let missing = path.with_extension("absent");
    let e = PineStrategy::new()
        .configure(replay_cfg(&missing))
        .expect_err("unreadable");
    assert_eq!(e.code.0, 6006);
}

#[test]
fn replay_inputs_are_refused_off_replay() {
    let mut b = cfg();
    b.boot_record = Some("x".into());
    let e = PineStrategy::new().configure(b).expect_err("record");
    assert_eq!((e.code.0, e.key), (6006, "replay"));
    let mut b = cfg();
    b.warmup = Some(("bars".into(), T0));
    let e = PineStrategy::new().configure(b).expect_err("warmup");
    assert_eq!((e.code.0, e.key), (6006, "replay"));
    // A replay without a record is a plain replay (B'3's tier 2).
    let mut b = cfg();
    b.replay = true;
    PineStrategy::new().configure(b).expect("plain replay");
}

#[test]
fn warmup_cut_makes_later_bars_cold() {
    // pin:08-r5-warmup-gate: the warm-up must end by the named close; bars past it make the row
    // cold (the digest reads 0), bars ending by it keep it warm.
    let bars = warm_bars(60, 50, 0);
    let last_close = bars.last().expect("bars").close_ms;
    for (cut, warm) in [(last_close, true), (last_close - 60_000, false)] {
        let mut b = cfg();
        b.replay = true;
        b.warmup = Some(("bars".into(), cut));
        let mut a = slice(0, 60, bars.clone());
        a.digest[..8].copy_from_slice(&[1; 8]);
        let mut p = PineStrategy::new();
        p.configure(b).expect("configure");
        p.supply_warm(vec![a]).expect("warm");
        p.on_start(&mut NoCtx).expect("on_start");
        let d = p.boot_record(0, 0).expect("rec").scripts[0].warmup_digest;
        assert_eq!(d != 0, warm, "cut {cut}");
    }
}
