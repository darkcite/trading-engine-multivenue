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
    AiCmd, AiCmdKind, Fill, Order, Price, Qty, Side, Tick, VenueId, AI_SIDE_NONE, FILL_FLAG_ADOPTED,
    SYMBOL_ID_NONE,
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
    // 2 since §6.4: `origin` is required on every strategy and
    // ruleset row, not tolerated when absent like `regime` and
    // `binary_fills` were, so the version has to say so.
    assert!(json.contains("\"audit_pnl_version\":2"));
    // §6.4: the accounting word rides between the label and the
    // numbers, so a consumer cannot read a figure off this row without
    // also reading which accounting produced it. `audit-pnl` REPLAYS,
    // so it is always PAPER here — pinned because the Python reader
    // (`claude_worker.pnl_report`) now REFUSES a row without it, and
    // this string is the only thing connecting the two.
    assert!(json.contains(
        "\"strategy_id\":0,\"label\":\"hyparb\",\"origin\":1,\"orders\":1,\"fills\":1,\"trades\":1"
    ));
    assert!(
        !json.contains("\"origin\":0"),
        "a replay cannot produce a VENUE row: {json}"
    );
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
        json.contains(
            "\"strategy_id\":0,\"label\":\"hyparb\",\"origin\":1,\"orders\":1,\"fills\":0,"
        ),
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
    assert!(json.contains("\"strategy_id\":5,\"label\":\"vm\",\"origin\":1,\"orders\":2"));
    let hex = "ab".repeat(16);
    assert!(
        json.contains(&format!("\"hash128\":\"{hex}\",\"origin\":1,\"orders\":1")),
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

/// BX7 (the risk review's R6): a Binance restart adopts the position the
/// last run left — opening inventory, not a second buy. Run 1 bought 10 @
/// 0.50; run 2's boot adopts the same 10 at its mark (0.55): the paper view
/// holds 10, marked 0.60 ⇒ net +1 (cash −5, mark-out 6), one fill traded.
/// Break-and-watch: folded as a trade it holds 20 and nets +1.5.
#[test]
fn an_adopted_fill_is_opening_inventory_not_a_second_buy() {
    let root = tmp_root("adopted");
    let d1 = run_dir(&root, EPOCH_1);
    manifest(&d1, &[(42, "PMTOK")]);
    write_ticks(&d1, "pm", EPOCH_1, &[pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000)]);
    write_fills(&d1, EPOCH_1, &[Fill::new(5_000, 42, Side::Bid, Price::from_raw(500_000), Qty::from_raw(10_000_000), 77)]);
    let d2 = run_dir(&root, EPOCH_2);
    manifest(&d2, &[(42, "PMTOK")]);
    write_ticks(&d2, "pm", EPOCH_2, &[pm_tick(1_000, 42, 590_000, 1_000_000, 610_000, 1_000_000)]);
    write_fills(
        &d2,
        EPOCH_2,
        &[Fill::new(2_000, 42, Side::Bid, Price::from_raw(550_000), Qty::from_raw(10_000_000), 0).with_flags(FILL_FLAG_ADOPTED)],
    );
    let (json, lines) = run_report(&root);
    assert!(json.contains("\"paper\":{\"fills\":1,\"net_usd\":\"1.0\"}"), "json: {json}");
    assert!(lines.iter().any(|l| l.contains("paper view: fills=1 adopted=1 ")), "{lines:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// BX7 (the risk review's N7): an adopted fill without a price (an
/// inverse leg's: its face carries none) opens at its symbol's last mark —
/// or, before any, at its next — never at 0, which read the exit's whole
/// flow as paper P&L. 42 adopts 10 after a 0.41 mark; 43 adopts 10 before
/// its first mark (0.30): both open where they are marked ⇒ net 0.
/// Break-and-watch: opened at 0, the paper view nets +7.1.
#[test]
fn an_unpriced_adopted_fill_opens_at_the_mark() {
    let root = tmp_root("adopted-unpriced");
    let d = run_dir(&root, EPOCH_1);
    manifest(&d, &[(42, "PMTOK"), (43, "PMTWO")]);
    write_ticks(
        &d,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
            pm_tick(3_000_000_000, 43, 290_000, 1_000_000, 310_000, 1_000_000),
        ],
    );
    let adopted = |sym: u32| Fill::new(2_000, sym, Side::Bid, Price::from_raw(0), Qty::from_raw(10_000_000), 0).with_flags(FILL_FLAG_ADOPTED);
    write_fills(&d, EPOCH_1, &[adopted(42), adopted(43)]);
    let (json, lines) = run_report(&root);
    assert!(json.contains("\"paper\":{\"fills\":0,\"net_usd\":\"0.0\"}"), "json: {json}");
    assert!(lines.iter().any(|l| l.contains("paper view: fills=0 adopted=2 ") && l.contains("; 0 adopted leg(s) never marked")), "{lines:?}");
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
            ..AuditPnlConfig::default()
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

// ---------------------------------------------------------------
// BIN15 O5a — the HIP-4 settlement surface
// ---------------------------------------------------------------
//
// O3 shipped the per-instance binary settlement on both harness arms
// and named THIS surface's absence as the O5 prerequisite: nothing
// produced a binary fill for it to score until slot 3 could trade. It
// can now, and a position held through its expiry has to become CASH —
// the venue clears the book at `T`, so marking it out at the last price
// the tape carried is marking it against a book that no longer exists.
//
// The collector here is not the backtest's. This surface INTERNS by
// descriptor, so the No leg is paired by NAME and the underlying is
// found by name; only the LAW (`backtest::binary::apply_binary_settlements`)
// is shared. That split is the thing these tests pin.

/// Hyperliquid activation Δ default = 600 ms (§4.4 table).
const HL_DELTA: u64 = 600_000_000;

/// The rolling pool's reserved ordinals, WITH the venue byte: the fill
/// model asserts `order.venue == symbol_venue_byte(order.sym)`, because
/// the engine derives one from the other and a capture that disagreed
/// would be a capture of orders no engine could have emitted.
const HL_YES: u32 = core_types::make_symbol_id(VenueId::Hyperliquid, 900);
const HL_NO: u32 = core_types::make_symbol_id(VenueId::Hyperliquid, 901);
const HL_BTC: u32 = core_types::make_symbol_id(VenueId::Hyperliquid, 3);
const BIN_OUTCOME: u32 = 2650;
/// A strike far below the mark: the Yes leg settles at 1.0.
const BIN_STRIKE_1E6: i64 = 70_000_000_000;
const BIN_MARK_1E6: i64 = 79_000_000_000;
/// The instance expires 40 s into the run and TWAPs over the 10 s
/// BEFORE that (BIN15 S1).
const BIN_EXPIRY_OFF: u64 = 40_000_000_000;
const BIN_TWAP_S: u16 = 10;

fn hl_tick(ts: u64, sym: u32, bid: i64, ask: i64) -> Tick {
    Tick::new(
        ts,
        VenueId::Hyperliquid,
        sym,
        1,
        Price::from_raw(bid),
        Qty::from_raw(100_000_000),
        Price::from_raw(ask),
        Qty::from_raw(100_000_000),
    )
}

fn hl_order(ts: u64, sym: u32, side: Side, px: i64, qty: i64, oid: u64) -> Order {
    let mut o = Order::new(
        ts,
        VenueId::Hyperliquid,
        sym,
        side,
        0,
        Price::from_raw(px),
        Qty::from_raw(qty),
        oid,
    );
    o.strategy_id = 3; // bin15
    o
}

fn write_events(dir: &Path, epoch: u64, evs: &[core_types::ChannelEvent]) {
    let mut w = PmlrWriter::open(dir.join("hl-events.pmlr"), SlotKind::Event, epoch).unwrap();
    for e in evs {
        w.append(e).unwrap();
    }
    w.flush().unwrap();
}

fn bin_roll(ts: u64, sym: u32, settled: bool) -> core_types::ChannelEvent {
    // Family index 0 occupies bits 48..56 and is left implicit: the
    // `rolling` list has one entry in these fixtures.
    let seq = u64::from(BIN_OUTCOME)
        | (u64::from(BIN_TWAP_S) << 32)
        | (u64::from(settled) << 56);
    core_types::ChannelEvent::new(
        ts,
        VenueId::Hyperliquid,
        core_types::ChannelId::InstrumentRoll,
        sym,
        seq,
        0,
        BIN_STRIKE_1E6,
        (EPOCH_1 + BIN_EXPIRY_OFF) as i64,
    )
}

fn bin_mark(ts: u64, sym: u32, px_1e6: i64) -> core_types::ChannelEvent {
    core_types::ChannelEvent::new(
        ts,
        VenueId::Hyperliquid,
        core_types::ChannelId::Mark,
        sym,
        0,
        0,
        px_1e6,
        0,
    )
}

/// The manifest rows a live HIP-4 family writes (O2's convention).
///
/// `with_underlying` is the one that matters: the No leg's descriptor
/// is DERIVED from the Yes leg's by name, so its manifest row is
/// optional, but without the underlying there is no mark series and
/// nothing to settle against.
fn bin_manifest(dir: &Path, with_underlying: bool) {
    let mut rows: Vec<(u32, &str)> = vec![
        (HL_YES, "hyperliquid:out:BTC:15m[yes]"),
        (HL_NO, "hyperliquid:out:BTC:15m[no]"),
    ];
    if with_underlying {
        rows.push((HL_BTC, "hyperliquid:BTC"));
    }
    manifest(dir, &rows);
}

#[test]
fn a_binary_held_through_expiry_settles_at_the_payout_not_the_last_book() {
    let root = tmp_root("bin15-settle");
    let dir = run_dir(&root, EPOCH_1);
    bin_manifest(&dir, true);
    // The roll binds the instance at the run's start; the marks in the
    // TWAP window ENDING at the expiry (BIN15 S1, LAW E-11:
    // `[expiry − twap, expiry]`) are the settlement evidence
    // (>= SETTLE_MIN_MARKS of them). The one after the expiry must not
    // be read.
    write_events(
        &dir,
        EPOCH_1,
        &[
            bin_roll(1_000, HL_YES, false),
            bin_mark(2_000, HL_BTC, BIN_MARK_1E6),
            // In force at the window's open (the evidence must cover it).
            bin_mark(BIN_EXPIRY_OFF - 10_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF - 7_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF - 4_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF - 1_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF + 3_000_000_000, HL_BTC, 1_000_000),
        ],
    );
    // The Yes book: an ask at 0.40 that the order crosses, then a LAST
    // tick at 0.20/0.22 — deliberately far below the payout, so a
    // mark-out and a settlement cannot be confused for each other.
    write_ticks(
        &dir,
        "hl",
        EPOCH_1,
        &[
            hl_tick(1_000, HL_YES, 390_000, 410_000),
            // STRICTLY below the 0.40 limit: the §4 cross is strict, so
            // an ask EQUAL to the limit does not fill — and the order
            // would then fill on the last tick instead, after the
            // settlement pass, which reads exactly like a settlement
            // that did not happen.
            hl_tick(2_000 + HL_DELTA + 10, HL_YES, 380_000, 390_000),
            hl_tick(BIN_EXPIRY_OFF + 12_000_000_000, HL_YES, 200_000, 220_000),
        ],
    );
    // 100 contracts bought at 0.40.
    write_orders(
        &dir,
        EPOCH_1,
        &[hl_order(2_000, HL_YES, Side::Bid, 400_000, 100_000_000, 11)],
    );
    let (json, lines) = run_report(&root);

    // The schedule was built, from THIS surface's own collector.
    let sched = lines
        .iter()
        .find(|l| l.contains("bin15 settlement table"))
        .unwrap_or_else(|| panic!("no schedule line: {lines:#?}"));
    assert!(sched.contains("1 of 1 instance(s) settleable"), "{sched}");
    assert!(sched.contains("(1 reaching their instant)"), "{sched}");
    assert!(sched.contains("unpaired_rolls=0"), "{sched}");

    // The P&L: 100 contracts at 0.40 settling at 1.0 = +$60. A
    // mark-out against that last 0.21 mid would have been −$19.
    assert!(
        json.contains("\"strategy_id\":3,\"label\":\"bin15\",\"origin\":1,\"orders\":1,\"fills\":1"),
        "json: {json}"
    );
    assert!(json.contains("\"net_usd\":\"60.0\""), "json: {json}");
    let _ = std::fs::remove_dir_all(&root);
}

/// BIN15 S2: the same instance on a v3 root whose run directory was
/// named 23 s before its first tick (as a boot names it) and whose HL
/// ticks carry the venue's own clock. On the old anchor law every wall
/// instant would sit 23 s early and the minute before the expiry would
/// hold none of the settlement marks; on the venue clock the audit
/// settles it exactly as the identity fixture above does — and says
/// which clock it used.
#[test]
fn a_v3_root_settles_on_the_venue_clock_in_the_audit_too() {
    const EARLY: u64 = 23_000_000_000;
    let root = tmp_root("bin15-venue-clock");
    let epoch = EPOCH_1 - EARLY;
    let dir = run_dir(&root, epoch);
    bin_manifest(&dir, true);
    write_events(
        &dir,
        epoch,
        &[
            bin_roll(1_000, HL_YES, false),
            bin_mark(2_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF - 10_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF - 7_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF - 4_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF - 1_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF, HL_BTC, BIN_MARK_1E6),
        ],
    );
    // The venue's clock: the first tick (ts 1 000) IS `EPOCH_1`.
    let stamp = |t: Tick| -> Tick {
        Tick::new_stamped(
            t.ts_ns,
            VenueId::Hyperliquid,
            t.sym,
            t.venue_seq,
            t.bid_px,
            t.bid_qty,
            t.ask_px,
            t.ask_qty,
            (EPOCH_1 + t.ts_ns - 1_000) / 1_000_000,
            0,
        )
    };
    write_ticks(
        &dir,
        "hl",
        epoch,
        &[
            stamp(hl_tick(1_000, HL_YES, 390_000, 410_000)),
            stamp(hl_tick(2_000 + HL_DELTA + 10, HL_YES, 380_000, 390_000)),
            stamp(hl_tick(BIN_EXPIRY_OFF + 12_000_000_000, HL_YES, 200_000, 220_000)),
        ],
    );
    write_orders(
        &dir,
        epoch,
        &[hl_order(2_000, HL_YES, Side::Bid, 400_000, 100_000_000, 11)],
    );
    let (json, lines) = run_report(&root);
    let tell = lines
        .iter()
        .find(|l| l.contains("wall=venue"))
        .unwrap_or_else(|| panic!("no clock tell: {lines:#?}"));
    // The stamps are whole milliseconds, so the fitted offset is off the
    // exact 23 s by under a millisecond.
    assert!(
        tell.contains("(the anchor law ran 22.999 s early)")
            || tell.contains("(the anchor law ran 23.000 s early)"),
        "{tell}"
    );
    let sched = lines
        .iter()
        .find(|l| l.contains("bin15 settlement table"))
        .unwrap_or_else(|| panic!("no schedule line: {lines:#?}"));
    assert!(sched.contains("1 of 1 instance(s) settleable"), "{sched}");
    assert!(json.contains("\"net_usd\":\"60.0\""), "json: {json}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_roll_whose_underlying_is_not_in_the_manifest_is_counted_not_guessed() {
    // Without the underlying's own row there is no mark series to
    // settle against, so the instance cannot be scheduled at all. It is
    // COUNTED and the positions mark out — the honest outcome, and loud.
    let root = tmp_root("bin15-unpaired");
    let dir = run_dir(&root, EPOCH_1);
    bin_manifest(&dir, false);
    write_events(
        &dir,
        EPOCH_1,
        &[
            bin_roll(1_000, HL_YES, false),
            bin_mark(2_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF - 9_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF - 5_000_000_000, HL_BTC, BIN_MARK_1E6),
            bin_mark(BIN_EXPIRY_OFF, HL_BTC, BIN_MARK_1E6),
        ],
    );
    write_ticks(
        &dir,
        "hl",
        EPOCH_1,
        &[
            hl_tick(1_000, HL_YES, 390_000, 410_000),
            // STRICTLY below the 0.40 limit: the §4 cross is strict, so
            // an ask EQUAL to the limit does not fill — and the order
            // would then fill on the last tick instead, after the
            // settlement pass, which reads exactly like a settlement
            // that did not happen.
            hl_tick(2_000 + HL_DELTA + 10, HL_YES, 380_000, 390_000),
            hl_tick(BIN_EXPIRY_OFF + 12_000_000_000, HL_YES, 200_000, 220_000),
        ],
    );
    write_orders(
        &dir,
        EPOCH_1,
        &[hl_order(2_000, HL_YES, Side::Bid, 400_000, 100_000_000, 11)],
    );
    let (json, lines) = run_report(&root);
    let sched = lines
        .iter()
        .find(|l| l.contains("bin15 settlement table"))
        .unwrap_or_else(|| panic!("no schedule line: {lines:#?}"));
    assert!(sched.contains("unpaired_rolls=1"), "{sched}");
    assert!(sched.contains("UNPAIRED rolls mark out"), "{sched}");
    assert!(sched.contains("0 of 0 instance(s) settleable"), "{sched}");
    // It marked out against the last mid (0.21) instead of settling:
    // (0.21 − 0.40) x 100 = −$19.
    assert!(json.contains("\"net_usd\":\"-19.0\""), "json: {json}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_root_with_no_hip4_instrument_says_nothing_and_changes_nothing() {
    // The regression guard in test form: every root captured before
    // slot 3 goes live has no HIP-4 family, so the surface must be
    // SILENT about binaries and its numbers must be what they were.
    // (The 8-window pool's own JSON was verified byte-identical across
    // this lane at `725d1d2748de79df`.)
    let root = tmp_root("bin15-absent");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(42, "PMTOK")]);
    write_ticks(
        &dir,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, 42, 400_000, 1_000_000, 420_000, 1_000_000),
            pm_tick(2_000 + PM_DELTA + 10, 42, 350_000, 1_000_000, 380_000, 50_000_000),
            pm_tick(2_000 + PM_DELTA + 20, 42, 590_000, 1_000_000, 610_000, 1_000_000),
        ],
    );
    write_orders(
        &dir,
        EPOCH_1,
        &[order(2_000, 42, Side::Bid, 500_000, 10_000_000, 7, 0)],
    );
    let (json, lines) = run_report(&root);
    assert!(
        !lines.iter().any(|l| l.contains("bin15")),
        "a root with no HIP-4 instrument must not mention bin15: {lines:#?}"
    );
    // Byte for byte the golden above.
    assert!(json.contains("\"net_usd\":\"1.0\""), "json: {json}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A sell print on Hyperliquid (`v1` negative = the seller aggressed).
fn hl_sell_print(ts: u64, sym: u32, tid: u64, px_1e6: i64, qty_1e6: i64) -> core_types::ChannelEvent {
    core_types::ChannelEvent::new(
        ts,
        VenueId::Hyperliquid,
        core_types::ChannelId::Trade,
        sym,
        tid,
        0,
        px_1e6,
        -qty_1e6,
    )
}

/// XMM XH3: a post-only maker on Hyperliquid (slot 6) fills in the audit
/// from the prints AFTER the displayed queue ahead of it — before XH3
/// audit-pnl read no prints, so such an order could never fill and slot
/// 6's shadow P&L read flat. The queue line fires only for a run with a
/// post-only maker in it.
#[test]
fn a_post_only_maker_fills_from_prints_after_the_displayed_queue() {
    let root = tmp_root("xmm-queue");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(HL_BTC, "hyperliquid:BTC")]);
    let land = 2_000 + HL_DELTA + 10;
    // 100.00 / 100.01 with 100 displayed on each side; a later book for
    // the mark-out.
    write_ticks(
        &dir,
        "hl",
        EPOCH_1,
        &[
            hl_tick(1_000, HL_BTC, 100_000_000, 100_010_000),
            hl_tick(land, HL_BTC, 100_000_000, 100_010_000),
            hl_tick(land + 5_000_000_000, HL_BTC, 100_000_000, 100_010_000),
        ],
    );
    // One post-only bid joining the touch: 100 ahead of it.
    let mut o = hl_order(2_000, HL_BTC, Side::Bid, 100_000_000, 1_000_000, 21).with_post_only();
    o.strategy_id = 6;
    write_orders(&dir, EPOCH_1, &[o]);
    // 60 traded at our price: the queue ahead shrinks to 40, nothing for
    // us. Then 41: the last 40 ahead, and 1 — all of ours.
    write_events(
        &dir,
        EPOCH_1,
        &[
            hl_sell_print(land + 1_000_000, HL_BTC, 1, 100_000_000, 60_000_000),
            hl_sell_print(land + 2_000_000, HL_BTC, 2, 100_000_000, 41_000_000),
        ],
    );
    let (json, lines) = run_report(&root);
    let q = lines
        .iter()
        .find(|l| l.contains("queue-orders="))
        .unwrap_or_else(|| panic!("no queue line: {lines:#?}"));
    assert!(q.contains("queue-orders=1 queue-prints=2"), "{q}");
    assert!(
        json.contains("\"strategy_id\":6,\"label\":\"xmm\",\"origin\":1,\"orders\":1,\"fills\":1"),
        "json: {json}"
    );
    let _ = std::fs::remove_dir_all(&root);

    // The same run with prints that never clear the queue ahead: no fill.
    let root = tmp_root("xmm-queue-short");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(HL_BTC, "hyperliquid:BTC")]);
    write_ticks(
        &dir,
        "hl",
        EPOCH_1,
        &[
            hl_tick(1_000, HL_BTC, 100_000_000, 100_010_000),
            hl_tick(land, HL_BTC, 100_000_000, 100_010_000),
            hl_tick(land + 5_000_000_000, HL_BTC, 100_000_000, 100_010_000),
        ],
    );
    write_orders(&dir, EPOCH_1, &[o]);
    write_events(
        &dir,
        EPOCH_1,
        &[hl_sell_print(land + 1_000_000, HL_BTC, 1, 100_000_000, 99_000_000)],
    );
    let (json, _) = run_report(&root);
    assert!(
        json.contains("\"strategy_id\":6,\"label\":\"xmm\",\"origin\":1,\"orders\":1,\"fills\":0"),
        "json: {json}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// XMM XH3: prints load ONLY for a run with a post-only maker, and only
/// inside its ticks — a run without one (here slot 3's plain order and a
/// print 5 s after the last tick) reports byte-for-byte as it did.
#[test]
fn prints_change_nothing_for_a_run_without_a_post_only_maker() {
    let report = |with_print: bool| {
        let root = tmp_root(if with_print { "xmm-noq-print" } else { "xmm-noq-bare" });
        let dir = run_dir(&root, EPOCH_1);
        manifest(&dir, &[(HL_BTC, "hyperliquid:BTC")]);
        write_ticks(
            &dir,
            "hl",
            EPOCH_1,
            &[
                hl_tick(1_000, HL_BTC, 100_000_000, 100_010_000),
                hl_tick(2_000 + HL_DELTA + 10, HL_BTC, 99_990_000, 100_000_000),
                hl_tick(3_000_000_000, HL_BTC, 100_000_000, 100_010_000),
            ],
        );
        write_orders(
            &dir,
            EPOCH_1,
            &[hl_order(2_000, HL_BTC, Side::Bid, 100_000_000, 1_000_000, 31)],
        );
        if with_print {
            write_events(
                &dir,
                EPOCH_1,
                &[
                    hl_sell_print(1_500, HL_BTC, 1, 100_000_000, 5_000_000),
                    hl_sell_print(8_000_000_000, HL_BTC, 2, 100_000_000, 5_000_000),
                ],
            );
        }
        let (json, lines) = run_report(&root);
        let _ = std::fs::remove_dir_all(&root);
        (json, lines)
    };
    let (bare, bare_lines) = report(false);
    let (with, with_lines) = report(true);
    assert_eq!(bare, with, "the JSON (window, days, rows) must not move");
    assert_eq!(bare_lines, with_lines, "nor the stderr surface");
    assert!(!with_lines.iter().any(|l| l.contains("queue-orders=")));
}

// ---------------------------------------------------------------
// PMJ J10 (R-PMJ-S42): Polymarket's rolling windows settle on the
// venue's own result, and their taker fills pay the p-dependent fee.
// ---------------------------------------------------------------

/// BTC 15m, family 0, pair 0 — the pool syms the ingress binds.
const PM15_UP: u32 = core_types::pm_roll::pm_rolling_sym(0, 0, 0);
const PM15_DOWN: u32 = core_types::pm_roll::pm_rolling_sym(0, 0, 1);
/// BTC 5m, family 0, pair 0.
const PM5_UP: u32 = core_types::pm_roll::pm5_rolling_sym(0, 0, 0);
const PM5_DOWN: u32 = core_types::pm_roll::pm5_rolling_sym(0, 0, 1);
/// The static anchor lane every run needs (a run with no `VENUE_LABELS`
/// tick has no anchor and contributes nothing).
const PM_STATIC: u32 = 42;
/// The settled row, 120 s into the run; the run's last tick after it.
const PM_SETTLE_TS: u64 = 120_000_000_000;
const PM_END_TS: u64 = 130_000_000_000;

fn pm_roll(label_pool5: bool, ts: u64, up_sym: u32, settled: bool, winner: i64) -> core_types::ChannelEvent {
    let instance: u32 = if label_pool5 {
        core_types::pm_roll::PM5_INSTANCE_FLAG | 5_000
    } else {
        5_000
    };
    core_types::ChannelEvent::new(
        ts,
        VenueId::Polymarket,
        core_types::ChannelId::InstrumentRoll,
        up_sym,
        core_types::pack_roll_seq(instance, 60, 0, settled),
        0,
        if settled { winner } else { 0 },
        0,
    )
}

fn write_events_as(dir: &Path, label: &str, epoch: u64, evs: &[core_types::ChannelEvent]) {
    let mut w = PmlrWriter::open(dir.join(format!("{label}-events.pmlr")), SlotKind::Event, epoch)
        .unwrap();
    for e in evs {
        w.append(e).unwrap();
    }
    w.flush().unwrap();
}

fn pm_ioc(ts: u64, venue: VenueId, sym: u32, px: i64, qty: i64, strategy: u8) -> Order {
    let mut o = Order::new(
        ts,
        venue,
        sym,
        Side::Bid,
        core_fill::ORDER_KIND_IOC,
        Price::from_raw(px),
        Qty::from_raw(qty),
        77,
    );
    o.strategy_id = strategy;
    o
}

/// One run: the static anchor lane at the start and the end, the rolling
/// leg's book, one IoC buy on `leg`, and the window's roll rows.
#[allow(clippy::too_many_arguments)]
fn pm_root(
    tag: &str,
    pool5: bool,
    leg: u32,
    venue: VenueId,
    strategy: u8,
    ask: i64,
    qty: i64,
    winner: i64,
) -> PathBuf {
    let root = tmp_root(tag);
    let dir = run_dir(&root, EPOCH_1);
    let (label, up, down, period) = if pool5 {
        ("pm5", PM5_UP, PM5_DOWN, "5m")
    } else {
        ("pm15", PM15_UP, PM15_DOWN, "15m")
    };
    let up_desc = format!("polymarket-rolling:updown:btc:{period}[up0]");
    let down_desc = format!("polymarket-rolling:updown:btc:{period}[down0]");
    manifest(&dir, &[(PM_STATIC, "PMTOK"), (up, &up_desc), (down, &down_desc)]);
    write_ticks(
        &dir,
        "pm",
        EPOCH_1,
        &[
            pm_tick(1_000, PM_STATIC, 400_000, 1, 600_000, 1),
            pm_tick(PM_END_TS, PM_STATIC, 400_000, 1, 600_000, 1),
        ],
    );
    write_ticks(
        &dir,
        label,
        EPOCH_1,
        &[
            pm_tick(1_500, leg, ask - 20_000, 1_000_000, ask, 100_000_000),
            pm_tick(2_000 + PM_DELTA + 10, leg, ask - 20_000, 1_000_000, ask, 100_000_000),
            // A last book far from either payout: a mark-out cannot pass
            // for the settlement.
            pm_tick(PM_SETTLE_TS + 5_000_000_000, leg, 400_000, 1_000_000, 600_000, 1_000_000),
        ],
    );
    write_orders(&dir, EPOCH_1, &[pm_ioc(2_000, venue, leg, 970_000, qty, strategy)]);
    write_events_as(
        &dir,
        label,
        EPOCH_1,
        &[pm_roll(pool5, 500, up, false, 0), pm_roll(pool5, PM_SETTLE_TS, up, true, winner)],
    );
    root
}

fn strategy_row<'a>(json: &'a str, label: &str) -> &'a str {
    let at = json
        .find(&format!("\"label\":\"{label}\""))
        .unwrap_or_else(|| panic!("no {label} row: {json}"));
    let end = json[at..].find('}').map_or(json.len(), |e| at + e);
    &json[at..end]
}

#[test]
fn a_slot_9_take_settles_at_the_venues_result_with_the_p_law_fee() {
    // 20 shares at the 0.96 touch (limit 0.97); Up wins → $20.
    // Fee 0.07 × 0.96 × 0.04 × 20 = $0.05376 (the flat 350 bps would be
    // $0.672); cost $19.20 → net $0.74624. A mark-out at the last 0.50
    // mid would read −$9.25376.
    let root = pm_root("pm15-settle", false, PM15_UP, VenueId::Polymarket, 9, 960_000, 20_000_000,
                       core_types::pm_roll::PM_SETTLED_UP);
    let (json, lines) = run_report(&root);
    let row = strategy_row(&json, "pm15");
    assert!(row.contains("\"fills\":1"), "{row}");
    assert!(row.contains("\"net_usd\":\"0.74624\""), "{row}");
    assert!(json.contains("\"pm_settlement\":{\"windows\":1,\"up\":1,\"down\":0,\"unknown\":0"), "{json}");
    let table = lines
        .iter()
        .find(|l| l.contains("pm settlement table"))
        .unwrap_or_else(|| panic!("no table: {lines:#?}"));
    assert!(table.contains("1 window(s) on traded slots"), "{table}");
    assert!(lines.iter().any(|l| l.contains("pm rolling orders=1 ticks=3")), "{lines:#?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_slot_11_take_through_jupiter_pays_twice_the_fee_and_settles() {
    // Slot 11 routes through Jupiter (`VenueId::Jupiter` on a Polymarket
    // sym). 10 shares of Down at 0.94; Down wins → $10. Fee at twice the
    // rate: 0.14 × 0.94 × 0.06 × 10 = $0.07896; net $10 − $9.40 − $0.07896.
    let root = pm_root("pm5-settle", true, PM5_DOWN, VenueId::Jupiter, 11, 940_000, 10_000_000,
                       core_types::pm_roll::PM_SETTLED_DOWN);
    let (json, _) = run_report(&root);
    let row = strategy_row(&json, "pmj");
    assert!(row.contains("\"fills\":1"), "{row}");
    assert!(row.contains("\"net_usd\":\"0.52104\""), "{row}");
    assert!(json.contains("\"pm_settlement\":{\"windows\":1,\"up\":0,\"down\":1,\"unknown\":0"), "{json}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_window_retired_unresolved_pays_nothing_as_the_member_books_it() {
    let root = pm_root("pm15-retired", false, PM15_UP, VenueId::Polymarket, 9, 960_000, 20_000_000,
                       core_types::pm_roll::PM_SETTLED_UNKNOWN);
    let (json, _) = run_report(&root);
    // −$19.20 cost − $0.05376 fee.
    assert!(strategy_row(&json, "pm15").contains("\"net_usd\":\"-19.25376\""), "{json}");
    assert!(json.contains("\"unknown\":1,\"retired\":1,\"flattened\":0"), "{json}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_paper_view_settles_a_rolling_paper_fill_at_the_payout() {
    let root = pm_root("pm15-paper", false, PM15_UP, VenueId::Polymarket, 9, 960_000, 20_000_000,
                       core_types::pm_roll::PM_SETTLED_UP);
    let dir = root.join(format!("run-{EPOCH_1}"));
    // The engine's own paper fill of the same take (no fees in this view).
    let f = Fill::new(
        2_000 + PM_DELTA + 10,
        PM15_UP,
        Side::Bid,
        Price::from_raw(960_000),
        Qty::from_raw(20_000_000),
        77,
    )
    .with_attribution(9, core_types::FILL_ORIGIN_PAPER);
    write_fills(&dir, EPOCH_1, &[f]);
    let (json, lines) = run_report(&root);
    // 20 × (1.00 − 0.96) = $0.80, not the 0.50 mark's −$9.20.
    assert!(json.contains("\"paper\":{\"fills\":1,\"net_usd\":\"0.8\"}"), "{json}");
    assert!(lines.iter().any(|l| l.contains("paper view: 1 position(s) settled")), "{lines:#?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_root_without_a_rolling_trade_carries_no_pm_settlement() {
    // The rolls are there, but no slot traded the legs: nothing is
    // scheduled, nothing is printed, the JSON has no key.
    let root = tmp_root("pm15-untraded");
    let dir = run_dir(&root, EPOCH_1);
    manifest(&dir, &[(PM_STATIC, "PMTOK"),
                     (PM15_UP, "polymarket-rolling:updown:btc:15m[up0]"),
                     (PM15_DOWN, "polymarket-rolling:updown:btc:15m[down0]")]);
    write_ticks(&dir, "pm", EPOCH_1, &[pm_tick(1_000, PM_STATIC, 400_000, 1, 600_000, 1),
                                       pm_tick(PM_END_TS, PM_STATIC, 400_000, 1, 600_000, 1)]);
    write_events_as(&dir, "pm15", EPOCH_1, &[pm_roll(false, 500, PM15_UP, false, 0),
        pm_roll(false, PM_SETTLE_TS, PM15_UP, true, core_types::pm_roll::PM_SETTLED_UP)]);
    let (json, lines) = run_report(&root);
    assert!(!json.contains("pm_settlement"), "{json}");
    assert!(!lines.iter().any(|l| l.contains("pm settlement table") || l.contains("pm rolling")), "{lines:#?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A paper fill of slot 9's take (no fees in the paper view).
fn pm_paper_fill(ts: u64, sym: u32) -> Fill {
    Fill::new(ts, sym, Side::Bid, Price::from_raw(960_000), Qty::from_raw(20_000_000), 77)
        .with_attribution(9, core_types::FILL_ORIGIN_PAPER)
}

/// Review r1 #1: a window run A ended without a result for sits on a slot
/// sym run B reuses for a later window. B's settlement of THAT window must
/// not close A's position at its winner: A's is flattened at 0 at A's own
/// last instant (the member's unknown law), in the strategy row and in the
/// paper view alike.
#[test]
fn a_window_a_run_ended_without_a_result_is_flattened_at_its_runs_end() {
    let root = tmp_root("pm15-two-runs");
    let up_desc = "polymarket-rolling:updown:btc:15m[up0]";
    let down_desc = "polymarket-rolling:updown:btc:15m[down0]";
    // Run A: buys 20 Up of window 5000, and ends before its result.
    let a = run_dir(&root, EPOCH_1);
    manifest(&a, &[(PM_STATIC, "PMTOK"), (PM15_UP, up_desc), (PM15_DOWN, down_desc)]);
    write_ticks(&a, "pm", EPOCH_1, &[pm_tick(1_000, PM_STATIC, 400_000, 1, 600_000, 1),
                                     pm_tick(90_000_000_000, PM_STATIC, 400_000, 1, 600_000, 1)]);
    write_ticks(&a, "pm15", EPOCH_1, &[
        pm_tick(1_500, PM15_UP, 940_000, 1_000_000, 960_000, 100_000_000),
        pm_tick(2_000 + PM_DELTA + 10, PM15_UP, 940_000, 1_000_000, 960_000, 100_000_000),
    ]);
    write_orders(&a, EPOCH_1, &[pm_ioc(2_000, VenueId::Polymarket, PM15_UP, 970_000, 20_000_000, 9)]);
    write_fills(&a, EPOCH_1, &[pm_paper_fill(2_000 + PM_DELTA + 10, PM15_UP)]);
    write_events_as(&a, "pm15", EPOCH_1, &[pm_roll(false, 500, PM15_UP, false, 0)]);
    // Run B: the pair's later window (instance 5016) is created and
    // settles UP — the winner A's Up leg would wrongly be paid at.
    let b = run_dir(&root, EPOCH_2);
    manifest(&b, &[(PM_STATIC, "PMTOK"), (PM15_UP, up_desc), (PM15_DOWN, down_desc)]);
    write_ticks(&b, "pm", EPOCH_2, &[pm_tick(1_000, PM_STATIC, 400_000, 1, 600_000, 1),
                                     pm_tick(60_000_000_000, PM_STATIC, 400_000, 1, 600_000, 1)]);
    let later = |settled: bool| core_types::ChannelEvent::new(
        if settled { 50_000_000_000 } else { 2_000 },
        VenueId::Polymarket,
        core_types::ChannelId::InstrumentRoll,
        PM15_UP,
        core_types::pack_roll_seq(5_016, 60, 0, settled),
        0,
        if settled { core_types::pm_roll::PM_SETTLED_UP } else { 0 },
        0,
    );
    write_events_as(&b, "pm15", EPOCH_2, &[later(false), later(true)]);
    let (json, lines) = run_report(&root);
    // −$19.20 − $0.05376: paid 0, not the later window's $20.
    assert!(strategy_row(&json, "pm15").contains("\"net_usd\":\"-19.25376\""), "{json}");
    assert!(json.contains("\"flattened_run_end\":1"), "{json}");
    assert!(json.contains("\"paper\":{\"fills\":1,\"net_usd\":\"-19.2\"}"), "{json}");
    assert!(lines.iter().any(|l| l.contains("1 flattened at their run's end")), "{lines:#?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Within one run: a window with no settled row whose pair is re-created
/// is flattened at 0 at that instant; the re-created one, open at the
/// root's end, marks out (counted).
#[test]
fn a_window_whose_pair_is_re_created_with_no_result_is_flattened_there() {
    let root = pm_root("pm15-flatten", false, PM15_UP, VenueId::Polymarket, 9, 960_000, 20_000_000,
                       core_types::pm_roll::PM_SETTLED_UP);
    let dir = root.join(format!("run-{EPOCH_1}"));
    let next = core_types::ChannelEvent::new(
        60_000_000_000,
        VenueId::Polymarket,
        core_types::ChannelId::InstrumentRoll,
        PM15_UP,
        core_types::pack_roll_seq(5_016, 60, 0, false),
        0,
        0,
        0,
    );
    // No settled row for 5000; the pair's next window created at 60 s.
    write_events_as(&dir, "pm15", EPOCH_1, &[pm_roll(false, 500, PM15_UP, false, 0), next]);
    let (json, _) = run_report(&root);
    assert!(strategy_row(&json, "pm15").contains("\"net_usd\":\"-19.25376\""), "{json}");
    assert!(json.contains("\"unknown\":1,\"retired\":0,\"flattened\":1,\"flattened_run_end\":0"), "{json}");
    assert!(json.contains("\"open_at_end\":1"), "{json}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A settled row past the window's last instant is never applied: the
/// position marks out at the last mid, and the row is counted.
#[test]
fn a_settled_row_after_the_window_is_counted_and_never_applied() {
    let root = pm_root("pm15-after", false, PM15_UP, VenueId::Polymarket, 9, 960_000, 20_000_000,
                       core_types::pm_roll::PM_SETTLED_UP);
    let dir = root.join(format!("run-{EPOCH_1}"));
    let late = pm_roll(false, PM_END_TS + 10_000_000_000, PM15_UP, true,
                       core_types::pm_roll::PM_SETTLED_UP);
    write_events_as(&dir, "pm15", EPOCH_1, &[pm_roll(false, 500, PM15_UP, false, 0), late]);
    let (json, _) = run_report(&root);
    // 20 × (0.50 mid − 0.96) − $0.05376: the mark-out, not the $20 payout.
    assert!(strategy_row(&json, "pm15").contains("\"net_usd\":\"-9.25376\""), "{json}");
    assert!(json.contains("\"windows\":0,"), "{json}");
    assert!(json.contains("\"after_window\":1,"), "{json}");
    let _ = std::fs::remove_dir_all(&root);
}
