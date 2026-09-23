// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `backtest --member hyparb` (HYPARB H6): the harness replays the pool
//! tape (`hyperevm-signals.pmlr`) beside the Hyperliquid books, drives
//! the slot-0 member exactly as the engine does (the paper matcher's AMM
//! book first, then `on_signal`), judges its swap at the next HEAD by
//! the AMM fill law, fills the hedge IoC against the perp's book, and
//! charges the gas ledger against the OOS net.
//!
//! The capture: one run; the perp sits 1 % above a WHYPE/USDC-shaped
//! pool; the snapshot lands at 1 s (the member buys the pool), a HEAD at
//! 2 s fills the swap, the member hedges on the perp, and the perp tick
//! at 2.2 s fills the hedge. All OOS under `0/100`.
//!
//! Offline-path doctrine: this test allocates freely.

use std::path::{Path, PathBuf};

use cli::backtest::member::{run_member, MemberKind, MemberSpec};
use cli::backtest::regime::RegimeMode;
use cli::backtest::BacktestConfig;
use core_amm::payload::{encode_head, encode_snapshot, encode_state, encode_tick, FAMILY_V3};
use core_io::{PmlrWriter, SlotKind};
use core_types::{
    make_symbol_id, LatencyClass, Price, Qty, Signal, SignalSource, Tick, VenueId, SYMBOL_ID_NONE,
};

const EPOCH_NS: u64 = 1_700_000_010_000_000_000;
const POOL_ADDR: &str = "0x6c9a33e3b592c0d65b3ba59355d5be0d38259285";
const TICK0: i32 = -230_543;
const L: u128 = 50_000_000_000_000_000_000;

fn perp() -> u32 {
    make_symbol_id(VenueId::Hyperliquid, 1)
}

fn pool() -> u32 {
    make_symbol_id(VenueId::HyperEvm, 1)
}

fn unique_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("cli-hyparb-{tag}-{}-{nanos}", std::process::id()))
}

fn pool_mid_1e6() -> i64 {
    let (lo, hi) = core_amm::sqrt_at_tick(TICK0);
    (core_amm::price_1e18_from_sqrt(lo, hi, 18, 6) / 1_000_000_000_000) as i64
}

fn perp_tick(ts_s: f64, seq: u32) -> Tick {
    let mid = pool_mid_1e6() * 101 / 100;
    Tick::new(
        (ts_s * 1e9) as u64,
        VenueId::Hyperliquid,
        perp(),
        seq,
        Price::from_raw(mid - 1_000),
        Qty::from_raw(100_000_000),
        Price::from_raw(mid + 1_000),
        Qty::from_raw(100_000_000),
    )
}

fn sig(ts_s: f64, sym: u32, payload: [u8; 40]) -> Signal {
    Signal::new(
        (ts_s * 1e9) as u64,
        sym,
        LatencyClass::Warm,
        SignalSource::HyperEvm as u8,
        payload,
    )
}

fn hyparb_toml() -> String {
    include_str!("../../../hyparb.toml.example")
        .replace("min_net_bps_1e6 = 10000000", "min_net_bps_1e6 = 5000000")
        .replace("lag_ns = 500000000", "lag_ns = 900000000")
        // The fixture's 1 % gap is its first sample: with basis control
        // on, the member would (rightly) de-mean all of it away.
        .replace("basis_enabled = 1", "basis_enabled = 0")
}

fn build_capture(root: &Path, with_pool_tape: bool) -> (PathBuf, PathBuf, PathBuf) {
    let run = root.join(format!("run-{EPOCH_NS}"));
    std::fs::create_dir_all(&run).expect("mkdir run");
    std::fs::write(
        run.join("instrument-manifest.tsv"),
        format!("{}\thyperliquid:HYPE\n", perp()),
    )
    .expect("manifest");
    let mut w =
        PmlrWriter::open(run.join("hl-ticks.pmlr"), SlotKind::Tick, EPOCH_NS).expect("open ticks");
    for (i, ts) in [0.0, 2.2, 3.0, 4.0].iter().enumerate() {
        w.append(&perp_tick(*ts, i as u32 + 1))
            .expect("append tick");
    }
    w.flush().expect("flush ticks");
    if with_pool_tape {
        let (lo, hi) = core_amm::sqrt_at_tick(TICK0);
        let tape = [
            sig(
                1.0,
                pool(),
                encode_snapshot(7, FAMILY_V3, -240_000, -220_000, 2, 500, 10, 18, 6).unwrap(),
            ),
            sig(1.0, pool(), encode_tick(-240_000, L as i128, L).unwrap()),
            sig(1.0, pool(), encode_tick(-220_000, -(L as i128), L).unwrap()),
            sig(1.0, pool(), encode_state(TICK0, lo, hi, L, true).unwrap()),
            sig(2.0, SYMBOL_ID_NONE, encode_head(8, 2, 1).unwrap()),
            sig(3.0, SYMBOL_ID_NONE, encode_head(9, 3, 1).unwrap()),
        ];
        let mut w = PmlrWriter::open(
            run.join("hyperevm-signals.pmlr"),
            SlotKind::Signal,
            EPOCH_NS,
        )
        .expect("open signals");
        for s in &tape {
            w.append(s).expect("append signal");
        }
        w.flush().expect("flush signals");
    }
    let toml = root.join("hyparb.toml");
    std::fs::write(&toml, hyparb_toml()).expect("write hyparb.toml");
    let universe = root.join("universe.toml");
    std::fs::write(
        &universe,
        format!("[hyperevm]\npools = [\"{POOL_ADDR}:v3:18:6\"]\n"),
    )
    .expect("write universe.toml");
    (root.to_path_buf(), toml, universe)
}

fn cfg(replay: &Path, toml: &Path, universe: &Path) -> BacktestConfig {
    BacktestConfig {
        ruleset: PathBuf::new(),
        replay_dir: replay.to_path_buf(),
        split: "0/100".to_owned(),
        fee_bps: vec!["hl:0:0".to_owned()],
        latency_ns: Some(0),
        latency_ns_venue: Vec::new(),
        stale_after_ms: vec!["hl:0".to_owned()],
        opt_fee: Vec::new(),
        option_spread_frac_1e6: None,
        emit_detail: None,
        regime: RegimeMode::Off,
        regime_seed: None,
        funding_seed: None,
        member: Some(MemberSpec {
            kind: MemberKind::Hyparb,
            params: toml.to_path_buf(),
            table: None,
            seed: None,
            vrp_seed: None,
            bin15_seed_dir: None,
            hyparb_universe: Some(universe.to_path_buf()),
        }),
    }
}

fn field<'a>(line: &'a str, key: &str) -> &'a str {
    let at = line
        .find(&format!(" {key}="))
        .unwrap_or_else(|| panic!("{key} in {line}"))
        + key.len()
        + 2;
    let rest = &line[at..];
    &rest[..rest.find(' ').unwrap_or(rest.len())]
}

#[test]
fn the_member_arbs_the_pool_tape_hedges_on_the_perp_and_pays_gas() {
    let root = unique_root("arb");
    let (replay, toml, universe) = build_capture(&root, true);
    let c = cfg(&replay, &toml, &universe);
    let out = run_member(&c, c.member.as_ref().unwrap()).expect("member run");
    let hash = core_crypto::sha256(hyparb_toml().as_bytes());
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        out.schema1.contains(&format!("\"ruleset_hash\":\"{hex}\"")),
        "{}",
        out.schema1
    );
    let line = out
        .summary
        .lines()
        .find(|l| l.starts_with("member: hyparb pool_events="))
        .unwrap_or_else(|| panic!("counters line in {}", out.summary));
    assert_eq!(field(line, "maps_loaded"), "1", "{line}");
    assert_eq!(field(line, "arbs"), "1", "{line}");
    assert_eq!(field(line, "side_buy"), "1", "{line}");
    assert_eq!(field(line, "amm_judged_fills"), "1", "{line}");
    assert_eq!(field(line, "amm_fills"), "1", "{line}");
    assert_eq!(field(line, "hedges_perp"), "1", "{line}");
    assert_eq!(field(line, "hedge_fills"), "1", "{line}");
    assert_eq!(
        field(line, "gas_oos_usd"),
        field(line, "gas_usd"),
        "all OOS: {line}"
    );
    let head = out
        .summary
        .lines()
        .find(|l| l.starts_with("member: hyparb params="))
        .expect("the boot line");
    assert_eq!(field(head, "pool_signals"), "6", "{head}");
    // The AMM leg and the hedge are two OOS trades. The OOS net marks
    // each leg at its OWN venue's mid, so the captured basis is not yet
    // profit (it is realised on convergence): what it shows is the pool
    // fee, the hedge's half spread and the gas the ledger charged.
    assert!(out.schema1.contains("\"trades\":2"), "{}", out.schema1);
    assert_eq!(out.stats.fills_oos, 2);
    std::fs::remove_dir_all(&root).ok();
}

/// Without the pool tape the member never sees a pool: nothing trades,
/// and the merge carries no signal (the lane is hyparb-only).
#[test]
fn without_the_pool_tape_nothing_trades() {
    let root = unique_root("dry");
    let (replay, toml, universe) = build_capture(&root, false);
    let c = cfg(&replay, &toml, &universe);
    let out = run_member(&c, c.member.as_ref().unwrap()).expect("member run");
    let line = out
        .summary
        .lines()
        .find(|l| l.starts_with("member: hyparb pool_events="))
        .expect("counters line");
    assert_eq!(field(line, "pool_events"), "0", "{line}");
    assert_eq!(field(line, "arbs"), "0", "{line}");
    assert_eq!(out.stats.fills_total, 0);
    std::fs::remove_dir_all(&root).ok();
}

/// A pool the universe does not list refuses the run (usage error).
#[test]
fn a_pool_the_universe_does_not_list_refuses() {
    let root = unique_root("nopool");
    let (replay, toml, universe) = build_capture(&root, true);
    std::fs::write(&universe, "[hyperevm]\npools = []\n").expect("rewrite");
    let c = cfg(&replay, &toml, &universe);
    let e = run_member(&c, c.member.as_ref().unwrap()).unwrap_err();
    assert!(format!("{e:?}").contains("not in universe.toml"), "{e:?}");
    std::fs::remove_dir_all(&root).ok();
}
