// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 (lead rulings 2026-10-17): slot 8's typed loop hook on the concrete
//! `StrategySet` — the boot record `<run dir>/pine-boot.tsv` written once right after start
//! (R5, impl-Bp §3.10), and the R6 delivery journal `<run dir>/pine-delivery.bin` drained only
//! with `--pine-tape`.

use std::path::{Path, PathBuf};

use cli::paper::LoopHook;
use cli::pine_boot::{build, finish_at_loop, DiscoveredFacts, PineFlags};
use cli::pine_delivery::{read_journal, PineHook, DELIVERY_FILE};
use core_types::{ChannelEvent, ChannelId, Price, Qty, Tick, VenueId};
use ingress_hyperliquid::discovery::HlDiscovery;
use strategy_core::{Strategy, StrategyCounters};
use strategy_pine::delivery::kind;
use strategy_pine::{BootRecord, BOOT_RECORD_FILE};
use strategy_set::{StrategySet, BIT_PINE};

const T0_MS: i64 = 1_767_225_600_000;
const BTC: u32 = 4096;
const RUN_EPOCH_NS: u64 = 1_767_225_590_123_456_789;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pine")
}

struct Clocked;
impl strategy_core::Ctx for Clocked {
    fn submit(&mut self, _o: core_types::Order) -> Result<(), strategy_core::SubmitErr> {
        Ok(())
    }
    fn cancel(&mut self, _r: core_types::CancelReq) -> Result<(), strategy_core::SubmitErr> {
        Ok(())
    }
    fn modify(&mut self, _p: u64, _o: core_types::Order) -> Result<(), strategy_core::SubmitErr> {
        Ok(())
    }
    fn now_ns(&self) -> core_time::NsTs {
        5
    }
}

/// A started set with slot 8 configured from the fixture manifest (paper, cold).
fn started_set(dir: &Path, tape: bool) -> (StrategySet, [u8; 32]) {
    let mut hl = HlDiscovery::new();
    hl.ingest_meta(&std::fs::read(fixtures().join("hl_meta.json")).expect("hl"))
        .expect("hl meta");
    let hl_sz: Vec<(String, u8)> = ["BTC", "ETH"]
        .iter()
        .map(|c| {
            (
                (*c).to_string(),
                hl.resolve(c.as_bytes()).expect("c").sz_decimals,
            )
        })
        .collect();
    let d = DiscoveredFacts {
        hl_sz_decimals: &hl_sz,
        bn_usdm: &[],
        bn_spot: &[],
    };
    let flags = PineFlags {
        manifest: fixtures().join("manifest.toml"),
        tape,
        bars_root: dir.join("bars"),
        pine_dir: dir.to_path_buf(),
    };
    let mut pb = build(
        &flags,
        None,
        |k| (k == "hyperliquid:btc").then_some(BTC),
        &d,
    )
    .expect("build");
    assert!(pb.run_dir.is_none(), "build leaves the run dir to the bin");
    finish_at_loop(
        &mut pb.cfg,
        core_time::WallAnchor::new(0, u64::try_from(T0_MS).expect("t0") * 1_000_000),
    );
    let sha = pb.cfg.manifest_sha256;
    let mut set = StrategySet::new(BIT_PINE);
    let needs = set.pine_mut().configure(pb.cfg).expect("configure");
    let (slices, _cold) = cli::pine_boot::warm(&needs, &pb.bars_root, T0_MS);
    set.pine_mut().supply_warm(slices).expect("warm");
    set.on_start(&mut Clocked).expect("on_start");
    (set, sha)
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pine-hook-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("dir");
    d
}

fn quote(t_ms: i64) -> Tick {
    Tick::new(
        u64::try_from(t_ms).expect("t") * 1_000_000,
        VenueId::Hyperliquid,
        BTC,
        1,
        Price::from_raw(99_000_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(101_000_000),
        Qty::from_raw(1_000_000),
    )
}

fn print(t_ms: i64) -> ChannelEvent {
    ChannelEvent::new(
        u64::try_from(t_ms).expect("t") * 1_000_000,
        VenueId::Hyperliquid,
        ChannelId::Trade,
        BTC,
        0,
        u64::try_from(t_ms).expect("t"),
        100_000_000,
        1_000_000,
    )
}

/// R5: the hook writes `pine-boot.tsv` once, right after start, with the run's epoch and the
/// engine clock handed to `started`; it parses back to the member's own record. R6: with
/// `--pine-tape` the journal is opened beside it and every drained record lands, in order.
#[test]
fn hook_writes_the_boot_record_and_drains_the_journal() {
    let dir = tmp("tape");
    let run = dir.join("run-1");
    std::fs::create_dir_all(&run).expect("run");
    let (mut set, sha) = started_set(&dir, true);
    let mut hook = Some(PineHook::new(run.clone(), RUN_EPOCH_NS, true));
    LoopHook::started(&mut hook, &set, 777);
    let text = std::fs::read_to_string(run.join(BOOT_RECORD_FILE)).expect("record");
    let rec = BootRecord::parse(&text).expect("parse");
    assert_eq!(rec.run_epoch_ns, RUN_EPOCH_NS);
    assert_eq!(rec.boot_mono_ns, 777);
    assert_eq!(rec.manifest_sha256, sha);
    assert_eq!(
        Some(rec),
        set.pine().boot_record(RUN_EPOCH_NS, 777),
        "the member's own record, verbatim"
    );
    // Inputs: prints and quotes of the pine symbol, drained twice (5 s cadence + the end).
    let mut ctx = Clocked;
    let mut t = T0_MS + 10;
    while t < T0_MS + 30_000 {
        set.on_venue_event(&print(t), &mut ctx);
        set.on_tick(&quote(t + 5), &mut ctx);
        t += 1_000;
    }
    LoopHook::drain(&mut hook, &set);
    set.on_tick(&quote(t), &mut ctx);
    LoopHook::drain(&mut hook, &set);
    let j = read_journal(&run.join(DELIVERY_FILE)).expect("journal");
    let k = set.pine().pine_counters();
    assert_eq!(
        j.len() as u64,
        k.seq_records + k.cancelled_reported,
        "one record per sequencing point plus one per cancel"
    );
    assert_eq!(j[0].kind, kind::EVENT);
    assert_eq!(j[0].ts_ns, print(T0_MS + 10).ts_ns);
    assert_eq!(j[1].kind, kind::TICK);
    assert_eq!(j.last().map(|r| r.kind), Some(kind::TICK));
    assert_eq!(set.pine().delivery_dropped(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without `--pine-tape` the record is still written; no journal file exists.
#[test]
fn no_tape_record_only() {
    let dir = tmp("notape");
    let (set, _) = started_set(&dir, false);
    let mut hook = PineHook::new(dir.clone(), RUN_EPOCH_NS, false);
    assert!(hook.on_started(set.pine(), 1).is_some());
    assert!(hook.on_drain(set.pine()).is_none());
    assert!(dir.join(BOOT_RECORD_FILE).exists());
    assert!(!dir.join(DELIVERY_FILE).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// pin:08-boot-record-write-logged: an unwritable run dir is logged, never a panic or a refusal.
#[test]
fn unwritable_run_dir_is_logged_not_fatal() {
    let dir = tmp("gone");
    let (set, _) = started_set(&dir, true);
    let mut hook = PineHook::new(dir.join("absent/deeper"), RUN_EPOCH_NS, true);
    assert!(hook.on_started(set.pine(), 1).is_none());
    assert!(hook.journal_path().is_none());
    assert!(hook.on_drain(set.pine()).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
