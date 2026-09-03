// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! audit-pnl integration tests (M4.2): golden hand-computable modeled
//! fills through the REUSED §4 fill law, strategy_id attribution, the
//! vm RulesetCommit hash timeline, the §6 descriptor-keying law across
//! sym-reshuffled runs, the manifest-less per-run namespace arm, the
//! paper-fill fold, and byte-identical determinism. Fixtures ride the
//! real `PmlrWriter` (one layout law, no drift).

use std::path::{Path, PathBuf};

use core_io::{PmlrWriter, SlotKind};
use core_types::{
    AiCmd, AiCmdKind, Fill, Order, Price, Qty, Side, Tick, VenueId, AI_SIDE_NONE, SYMBOL_ID_NONE,
};

use cli::audit_pnl::{run, AuditPnlConfig};

const EPOCH_1: u64 = 1_000_000_000_000_000_000;
const EPOCH_2: u64 = 1_000_100_000_000_000_000; // +100 s wall

fn tmp_root(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("m4_audit_pnl_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_dir(root: &Path, epoch: u64) -> PathBuf {
    let d = root.join(format!("run-{epoch}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_ticks(dir: &Path, label: &str, epoch: u64, ticks: &[Tick]) {
    let mut w = PmlrWriter::open(
        dir.join(format!("{label}-ticks.pmlr")),
        SlotKind::Tick,
        epoch,
    )
    .unwrap();
    for t in ticks {
        w.append(t).unwrap();
    }
    w.flush().unwrap();
}

fn write_orders(dir: &Path, epoch: u64, orders: &[Order]) {
    let mut w = PmlrWriter::open(dir.join("engine-orders.pmlr"), SlotKind::Order, epoch).unwrap();
    for o in orders {
        w.append(o).unwrap();
    }
    w.flush().unwrap();
}

fn write_fills(dir: &Path, epoch: u64, fills: &[Fill]) {
    let mut w = PmlrWriter::open(dir.join("engine-fills.pmlr"), SlotKind::Fill, epoch).unwrap();
    for f in fills {
        w.append(f).unwrap();
    }
    w.flush().unwrap();
}

fn write_ai_cmds(dir: &Path, epoch: u64, cmds: &[AiCmd]) {
    let mut w = PmlrWriter::open(dir.join("ai-cmds.pmlr"), SlotKind::AiCmd, epoch).unwrap();
    for c in cmds {
        w.append(c).unwrap();
    }
    w.flush().unwrap();
}

fn manifest(dir: &Path, rows: &[(u32, &str)]) {
    let body: String = rows
        .iter()
        .map(|(sym, desc)| format!("{sym}\t{desc}\n"))
        .collect();
    std::fs::write(dir.join("instrument-manifest.tsv"), body).unwrap();
}

fn pm_tick(ts: u64, sym: u32, bid: i64, bq: i64, ask: i64, aq: i64) -> Tick {
    Tick::new(
        ts,
        VenueId::Polymarket,
        sym,
        1,
        Price::from_raw(bid),
        Qty::from_raw(bq),
        Price::from_raw(ask),
        Qty::from_raw(aq),
    )
}

fn order(ts: u64, sym: u32, side: Side, px: i64, qty: i64, oid: u64, strategy: u8) -> Order {
    let mut o = Order::new(
        ts,
        VenueId::Polymarket,
        sym,
        side,
        0,
        Price::from_raw(px),
        Qty::from_raw(qty),
        oid,
    );
    o.strategy_id = strategy;
    o
}

fn commit_cmd(ts: u64, seq: u32, hash: [u8; 16]) -> AiCmd {
    let px = i64::from_le_bytes(hash[0..8].try_into().unwrap());
    let qty = i64::from_le_bytes(hash[8..16].try_into().unwrap());
    AiCmd::new(
        ts,
        seq,
        SYMBOL_ID_NONE,
        px,
        qty,
        0,
        AiCmdKind::RulesetCommit,
        VenueId::Ai,
        5,
        AI_SIDE_NONE,
        0,
        0,
    )
}

fn run_report(root: &Path) -> (String, Vec<String>) {
    let cfg = AuditPnlConfig {
        replay_dir: root.to_path_buf(),
        ..AuditPnlConfig::default()
    };
    let mut lines = Vec::new();
    let json = run(&cfg, &mut |l: &str| lines.push(l.to_owned())).expect("report");
    (json, lines)
}

/// PM activation Δ default = 200 ms (§4.4 table).
const PM_DELTA: u64 = 200_000_000;

#[test]
fn golden_modeled_fill_attribution_and_markout() {
    let root = tmp_root("golden");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(42, "PMTOK")]);
    // Anchor tick at ts=1_000; order emitted ts=2_000 (activates at
    // 2_000+Δ); crossing tick after activation: ask 0.38 < px 0.50,
    // displayed 50 ⇒ full fill of 10 @ 0.50 (strict cross, maker at
    // P); final tick marks mid 0.60 ⇒ unreal = (0.60−0.50)×10 = +$1.
    write_ticks(
        &dir,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
            pm_tick(
                2_000 + PM_DELTA + 10,
                42,
                350_000,
                1_000_000,
                380_000,
                50_000_000,
            ),
            pm_tick(
                2_000 + PM_DELTA + 20,
                42,
                590_000,
                1_000_000,
                610_000,
                1_000_000,
            ),
        ],
    );
    write_orders(
        &dir,
        EPOCH_1,
        &[order(2_000, 42, Side::Bid, 500_000, 10_000_000, 7, 0)],
    );
    let (json, lines) = run_report(&root);
    assert!(json.contains("\"audit_pnl_version\":1"));
    assert!(json.contains(
        "\"strategy_id\":0,\"label\":\"latency-arb\",\"orders\":1,\"fills\":1,\"trades\":1"
    ));
    assert!(json.contains("\"net_usd\":\"1.0\""), "json: {json}");
    // Per-sym human row carries the DESCRIPTOR, never the bare sym.
    assert!(lines.iter().any(|l| l.contains("PMTOK: fills=1")));
    // Paper view honestly empty.
    assert!(json.contains("\"paper\":{\"fills\":0,\"net_usd\":\"0.0\"}"));
    let _ = std::fs::remove_dir_all(&root);
}

/// I1 golden: mixed kind-0 / kind-1 intents through one run. The maker
/// (kind 0) fills at P on the strict cross exactly as the golden
/// above; the IoC (kind 1) at the same limit fills at the activation
/// TOUCH (0.38, not 0.50), pays the TAKER column of `--fee-bps
/// pm:1:5`, and a second IoC whose limit is below the touch cancels;
/// a third IoC emitted with a TTL that elapses before any fresh tick
/// expires. The fee ladder re-prices the same fills at 0 / 1 / 2 bps.
#[test]
fn ioc_intents_fill_at_the_touch_with_the_taker_fee_and_the_ladder_prints() {
    let root = tmp_root("ioc");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(42, "PMTOK")]);
    write_ticks(
        &dir,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
            pm_tick(
                2_000 + PM_DELTA + 10,
                42,
                350_000,
                1_000_000,
                380_000,
                50_000_000,
            ),
            pm_tick(
                2_000 + PM_DELTA + 20,
                42,
                590_000,
                1_000_000,
                610_000,
                1_000_000,
            ),
        ],
    );
    let ioc = |ts: u64, px: i64, oid: u64, ttl: u64| {
        let mut o = order(ts, 42, Side::Bid, px, 10_000_000, oid, 0);
        o.kind = 1;
        o.with_ttl_ns(ttl)
    };
    write_orders(
        &dir,
        EPOCH_1,
        &[
            order(2_000, 42, Side::Bid, 500_000, 10_000_000, 7, 0), // maker @0.50
            ioc(2_000, 500_000, 8, 0),                              // IoC limit 0.50 ⇒ pays 0.38
            ioc(2_000, 370_000, 9, 0),                              // IoC limit 0.37 < 0.38 ⇒ cancel
            ioc(2_000, 500_000, 10, PM_DELTA), // expires at 2_000+Δ, before the 2_000+Δ+10 tick
        ],
    );
    let cfg = AuditPnlConfig {
        replay_dir: root.clone(),
        fee_bps: vec!["pm:1:5".to_owned()],
        ..AuditPnlConfig::default()
    };
    let mut lines = Vec::new();
    let json = run(&cfg, &mut |l: &str| lines.push(l.to_owned())).expect("report");
    // 4 orders; 2 fills (maker 10 @ 0.50, IoC 10 @ 0.38); final mark 0.60:
    //   maker unreal (0.60−0.50)×10 = +1.0, fee 1 bps × $5 = $0.0005
    //   IoC   unreal (0.60−0.38)×10 = +2.2, fee 5 bps × $3.8 = $0.0019
    //   net = 3.2 − 0.0024 = 3.1976
    assert!(
        json.contains("\"orders\":4,\"fills\":2,\"trades\":2"),
        "json: {json}"
    );
    assert!(json.contains("\"net_usd\":\"3.1976\""), "json: {json}");
    assert!(json.contains("\"fees_usd\":\"0.0024\""), "json: {json}");
    assert!(
        json.contains("\"ioc_fills\":1,\"ioc_canceled\":1,\"ttl_expired\":1,"),
        "json: {json}"
    );
    // Ladder: 0 ⇒ 3.2; 1 bps/side on $8.8 notional ⇒ 3.2 − 0.00088 =
    // 3.19912; 2 bps ⇒ 3.19824.
    assert!(
        json.contains("\"fee_ladder_net_usd\":[\"3.2\",\"3.19912\",\"3.19824\"]"),
        "json: {json}"
    );
    assert!(
        lines.iter().any(|l| l.contains("ioc_fills=1 ioc_canceled=1 ttl_expired=1 | fee ladder")),
        "lines: {lines:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// VT4: the golden script with the crossing tick STAMPED stale — the
/// order never fills (no fill, no mark on a stale tick), the run line
/// says `pm=1/3`, and `pm:0` restores the golden fill (a threshold
/// change is a replay).
#[test]
fn stale_tick_neither_fills_nor_marks_in_audit_pnl_and_is_reported() {
    let root = tmp_root("stale");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(42, "PMTOK")]);
    let stamped = |ts: u64, bid: i64, ask: i64, aq: i64, venue_ms: u64| {
        Tick::new_stamped(
            ts,
            VenueId::Polymarket,
            42,
            1,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(aq),
            venue_ms,
            0,
        )
    };
    // Offsets: the anchor learns venue≈mono; the crossing tick's stamp
    // sits 2 s behind ⇒ delay ≈ 2200 ms > pm's 1000 ⇒ STALE; the last
    // tick is on time again (10 ns later: stale time rounds to 0 bps).
    write_ticks(
        &dir,
        "pm",
        EPOCH_1,
        &[
            stamped(1_000, 400_000, 420_000, 1_000_000, 1_000_000),
            stamped(2_000 + PM_DELTA + 10, 350_000, 380_000, 50_000_000, 998_000),
            stamped(2_000 + PM_DELTA + 20, 590_000, 610_000, 1_000_000, 1_000_200),
        ],
    );
    write_orders(
        &dir,
        EPOCH_1,
        &[order(2_000, 42, Side::Bid, 500_000, 10_000_000, 7, 0)],
    );
    let (json, lines) = run_report(&root);
    assert!(
        json.contains("\"strategy_id\":0,\"label\":\"latency-arb\",\"orders\":1,\"fills\":0,"),
        "json: {json}"
    );
    assert!(
        lines.iter().any(|l| l.ends_with(" stale: pm=1/3 (0bps)")),
        "lines: {lines:?}"
    );
    // Judgement OFF for pm ⇒ the golden fill and +$1 markout return.
    let cfg = AuditPnlConfig {
        replay_dir: root.clone(),
        stale_after_ms: vec!["pm:0".to_owned()],
        ..AuditPnlConfig::default()
    };
    let mut lines = Vec::new();
    let json = run(&cfg, &mut |l: &str| lines.push(l.to_owned())).expect("report");
    assert!(json.contains("\"net_usd\":\"1.0\""), "json: {json}");
    assert!(
        lines.iter().any(|l| l.ends_with(" stale: pm=0/3 (0bps)")),
        "lines: {lines:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn vm_hash_timeline_buckets_orders_after_commit_only() {
    let root = tmp_root("vmhash");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(42, "PMTOK")]);
    write_ticks(
        &dir,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
            pm_tick(5_000_000_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
        ],
    );
    let hash = [0xAB; 16];
    write_ai_cmds(&dir, EPOCH_1, &[commit_cmd(3_000, 1, hash)]);
    write_orders(
        &dir,
        EPOCH_1,
        &[
            // BEFORE the commit: slot-5 aggregate only.
            order(2_000, 42, Side::Bid, 100_000, 1_000_000, 1, 5),
            // AFTER: bucketed under the hash too.
            order(4_000, 42, Side::Bid, 100_000, 1_000_000, 2, 5),
        ],
    );
    let (json, _lines) = run_report(&root);
    assert!(json.contains("\"strategy_id\":5,\"label\":\"vm\",\"orders\":2"));
    let hex = "ab".repeat(16);
    assert!(
        json.contains(&format!("\"hash128\":\"{hex}\",\"orders\":1")),
        "json: {json}"
    );
    assert!(json.contains("\"vm_orders_no_hash\":1"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn descriptor_keying_survives_sym_reshuffle_across_runs() {
    // §6 law: run 2 reshuffles ordinals (42 ⇒ OTHER, 43 ⇒ PMTOK).
    // The PMTOK position opened in run 1 must mark against run 2's
    // sym-43 ticks, and a run-2 order on sym 42 must land in OTHER.
    let root = tmp_root("reshuffle");
    let d1 = run_dir(&root, EPOCH_1);
    manifest(&d1, &[(42, "PMTOK")]);
    write_ticks(
        &d1,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
            pm_tick(
                2_000 + PM_DELTA + 10,
                42,
                350_000,
                1_000_000,
                380_000,
                50_000_000,
            ),
        ],
    );
    write_orders(
        &d1,
        EPOCH_1,
        &[order(2_000, 42, Side::Bid, 500_000, 10_000_000, 1, 0)],
    );

    let d2 = run_dir(&root, EPOCH_2);
    manifest(&d2, &[(42, "OTHER"), (43, "PMTOK")]);
    write_ticks(
        &d2,
        "pm",
        EPOCH_2,
        &[
            // PMTOK now rides sym 43: mark to 0.60.
            pm_tick(1_000, 43, 590_000, 1_000_000, 610_000, 1_000_000),
            // OTHER (sym 42) ticks + crossing for the run-2 order.
            pm_tick(2_000, 42, 200_000, 1_000_000, 220_000, 1_000_000),
            pm_tick(
                3_000 + PM_DELTA + 10,
                42,
                150_000,
                1_000_000,
                180_000,
                50_000_000,
            ),
        ],
    );
    write_orders(
        &d2,
        EPOCH_2,
        &[order(3_000, 42, Side::Bid, 200_000, 5_000_000, 2, 0)],
    );

    let (json, lines) = run_report(&root);
    // PMTOK: filled 10 @ 0.50 in run 1, marked 0.60 by run 2's sym 43
    // ⇒ +$1. OTHER: filled 5 @ 0.20, last mark = its fill-tick mid
    // 0.165 ⇒ −$0.175. Net floor: 1.0 − 0.175 = 0.825.
    assert!(json.contains("\"orders\":2,\"fills\":2,\"trades\":2"));
    assert!(json.contains("\"net_usd\":\"0.825\""), "json: {json}");
    let pmtok = lines.iter().filter(|l| l.contains("PMTOK: fills=")).count();
    assert_eq!(pmtok, 1, "ONE continuous PMTOK row across the reshuffle");
    assert!(lines.iter().any(|l| l.contains("OTHER: fills=1")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn manifestless_run_is_namespaced_per_run() {
    let root = tmp_root("nomanifest");
    let dir = run_dir(&root, EPOCH_1);
    write_ticks(
        &dir,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
            pm_tick(
                2_000 + PM_DELTA + 10,
                42,
                350_000,
                1_000_000,
                380_000,
                50_000_000,
            ),
        ],
    );
    write_orders(
        &dir,
        EPOCH_1,
        &[order(2_000, 42, Side::Bid, 500_000, 10_000_000, 1, 0xFF)],
    );
    let (json, lines) = run_report(&root);
    assert!(json.contains("\"strategy_id\":255,\"label\":\"unattributed\""));
    assert!(lines
        .iter()
        .any(|l| l.contains(&format!("run-{EPOCH_1}/sym-"))));
    assert!(lines.iter().any(|l| l.contains("manifest=NO")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn paper_fills_fold_into_cash_plus_markout() {
    let root = tmp_root("paper");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(42, "PMTOK")]);
    write_ticks(
        &dir,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
            pm_tick(9_000, 42, 590_000, 1_000_000, 610_000, 1_000_000),
        ],
    );
    // Paper fill: bought 10 @ 0.50 ⇒ cash −5; mark-out 10 × 0.60 = 6
    // ⇒ net +1 (no fees in paper).
    write_fills(
        &dir,
        EPOCH_1,
        &[Fill::new(
            5_000,
            42,
            Side::Bid,
            Price::from_raw(500_000),
            Qty::from_raw(10_000_000),
            77,
        )],
    );
    let (json, _lines) = run_report(&root);
    assert!(
        json.contains("\"paper\":{\"fills\":1,\"net_usd\":\"1.0\"}"),
        "json: {json}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rerun_is_byte_identical() {
    let root = tmp_root("determinism");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(42, "PMTOK")]);
    write_ticks(
        &dir,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
            pm_tick(
                2_000 + PM_DELTA + 10,
                42,
                350_000,
                1_000_000,
                380_000,
                50_000_000,
            ),
        ],
    );
    write_orders(
        &dir,
        EPOCH_1,
        &[order(2_000, 42, Side::Bid, 500_000, 10_000_000, 1, 0)],
    );
    let (json1, lines1) = run_report(&root);
    let (json2, lines2) = run_report(&root);
    assert_eq!(json1, json2, "stdout JSON byte-identical");
    assert_eq!(lines1, lines2, "stderr summary byte-identical");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tickless_root_is_refused() {
    let root = tmp_root("tickless");
    let dir = run_dir(&root, EPOCH_1);
    write_orders(
        &dir,
        EPOCH_1,
        &[order(2_000, 42, Side::Bid, 500_000, 10_000_000, 1, 0)],
    );
    let cfg = AuditPnlConfig {
        replay_dir: root.clone(),
        ..AuditPnlConfig::default()
    };
    assert!(run(&cfg, &mut |_l: &str| {}).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

/// VM2 V5 (D-7): an option intent with NO tick lane executes under
/// the mark-fill law — the opt-summary marks synthesize the book, the
/// fill lands at mark ± h with taker economics, and the assumption is
/// PRINTED.
#[test]
fn option_intents_mark_fill_and_print_the_d7_assumption() {
    let root = tmp_root("d7opt");
    let dir = run_dir(&root, EPOCH_1);
    let opt_sym: u32 = (3 << 24) | 700;
    let anchor_sym: u32 = (1 << 24) | 600;

    std::fs::write(
        dir.join("instrument-manifest.tsv"),
        format!("{anchor_sym}\tbinance-usdm:btcusdt\n{opt_sym}\tderibit:BTC-27MAR26-60000-C\n"),
    )
    .unwrap();

    // Tick anchor (runs need one) + two mark-bearing opt records.
    write_ticks(
        &dir,
        "bn",
        EPOCH_1,
        &[Tick::new(
            1_000,
            VenueId::Binance,
            anchor_sym,
            1,
            Price::from_raw(490_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(510_000),
            Qty::from_raw(1_000_000),
        )],
    );
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
    {
        let mut w = PmlrWriter::open(
            dir.join("deribit-opt-summary.pmlr"),
            SlotKind::OptSummary,
            EPOCH_1,
        )
        .unwrap();
        w.append(&mk_opt(2_000, 50_000_000)).unwrap();
        w.append(&mk_opt(3_000, 40_000_000)).unwrap();
        w.flush().unwrap();
    }
    // One vm option intent between the marks: Bid 100 units @ 0.05.
    let mut o = Order::new(
        2_500,
        VenueId::Deribit,
        opt_sym,
        Side::Bid,
        0,
        Price::from_raw(50_000),
        Qty::from_raw(100_000_000),
        1,
    );
    o.strategy_id = 5;
    write_orders(&dir, EPOCH_1, &[o]);

    let mut lines: Vec<String> = Vec::new();
    let json = run(
        &AuditPnlConfig {
            replay_dir: root.clone(),
            fee_bps: Vec::new(),
            latency_ns: Some(0),
            latency_ns_venue: Vec::new(),
            stale_after_ms: Vec::new(),
        },
        &mut |l: &str| lines.push(l.to_owned()),
    )
    .expect("audit ok");

    assert!(
        lines.iter().any(|l| l.contains("OPTIONS MARK-FILL LAW (D-7)")),
        "the assumption must be PRINTED: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("opt-synth-ticks=2")),
        "synthetic mark ticks reported: {lines:?}"
    );
    // The vm strategy row shows exactly one fill (the mark fill at
    // the second record).
    assert!(
        lines
            .iter()
            .any(|l| l.contains("strategy 5 (vm)") && l.contains("fills=1")),
        "one mark fill on the vm row: {lines:?}"
    );
    assert!(json.contains("\"strategy_id\":5"));

    let _ = std::fs::remove_dir_all(&root);
}
