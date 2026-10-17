// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'3 T19b (impl-Bp §7.8, I26): the 8g member drive end to end on the
//! `hold.pine` fixture over a one-run window built here. The "paper" session
//! is a fresh boot driven by the plain loop; its own boot record and its
//! emissions (stamped `POP` after their record) are the capture the replay
//! is judged against (pin:Bp-b3-tests-in-crate).

use std::path::{Path, PathBuf};

use core_io::{PmlrWriter, SlotKind};
use core_types::{
    make_symbol_id, ChannelEvent, ChannelId, Order, Price, Qty, Side, Tick, VenueId,
    TICK_FLAG_STALE,
};
use strategy_set::SLOT_PINE;

use super::super::{check_inputs, ReplayArgv};
use super::{boot_set, drive_judged, load_window, virt_of, STRADDLE_NS};
use crate::backtest::fill::FillEngine;
use crate::backtest::{parse_model_params, ModelParams, RecPayload, VIRT_T0};

const EA: u64 = 1_767_225_600_000_000_000;
const SEC: u64 = 1_000_000_000;
const STEP: u64 = 500_000_000;
const SYM: u32 = make_symbol_id(VenueId::Hyperliquid, 0);
/// The paper pop delay of a captured stamp after its record's ingress.
const POP: u64 = 3_000_000;
const END_S: u64 = 330;

fn script_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pine-bp")
}

struct Fx {
    root: PathBuf,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run_dir(root: &Path) -> PathBuf {
    root.join(format!("run-{EA}"))
}

/// One run: an HL quote and a print every 500 ms over `[0, END_S)`; the
/// tick at `stale_k` carries the ingress's `TICK_FLAG_STALE` with a fresh
/// venue stamp (a re-judge would clear it).
fn write_run(dir: &Path, stale_k: u64) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("instrument-manifest.tsv"),
        format!("{SYM}\thyperliquid:BTC\n"),
    )
    .unwrap();
    let mut wt = PmlrWriter::open(dir.join("hl-ticks.pmlr"), SlotKind::Tick, EA).unwrap();
    let mut we = PmlrWriter::open(dir.join("hl-events.pmlr"), SlotKind::Event, EA).unwrap();
    let mut k = 0u64;
    while k * STEP < END_S * SEC {
        let ts = k * STEP;
        let px = 100_000_000 + k as i64 * 10_000;
        let mut t = Tick::new(
            ts + 1,
            VenueId::Hyperliquid,
            SYM,
            k as u32 + 1,
            Price::from_raw(px - 10_000),
            Qty::from_raw(50_000_000),
            Price::from_raw(px + 10_000),
            Qty::from_raw(50_000_000),
        );
        t.venue_time_ms = (EA + ts + 1) / 1_000_000;
        if k == stale_k {
            t.flags |= TICK_FLAG_STALE;
        }
        wt.append(&t).unwrap();
        we.append(&ChannelEvent::new(
            ts,
            VenueId::Hyperliquid,
            ChannelId::Trade,
            SYM,
            0,
            (EA + ts) / 1_000_000,
            px,
            1_000_000,
        ))
        .unwrap();
        k += 1;
    }
    wt.flush().unwrap();
    we.flush().unwrap();
}

/// The window root, the manifest, the universe and the paper boot's own
/// record (`pine-boot.tsv`); the capture's orders are written by `capture`.
fn fixture(tag: &str) -> (Fx, ReplayArgv) {
    let root = std::env::temp_dir().join(format!("cli-pine-8g-drive-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    write_run(&run_dir(&root), 40);
    let src = std::fs::read(script_dir().join("hold.pine")).unwrap();
    let sha: String = core_crypto::sha256(&src)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(
        root.join("manifest.toml"),
        format!(
            "[[script]]\nsha256 = \"{sha}\"\nfile = \"hold.pine\"\ndialect = 6\nenabled = 1\n\
             mode = \"bar_close\"\nsymbol = \"hyperliquid:btc\"\ntimeframe_s = 60\n\
             venue = \"hyperliquid\"\nmax_bars_back = 5000\ncoalesce_ms = 250\n\
             coalesce_bps = 0\ninsn_budget = 2000000\nqty_notional_1e6 = 12000000\n\
             limit_primitive = \"ioc\"\ncross_ticks = 2\ncap_instance_1e6 = 12000000\n\
             cap_day_1e6 = 480000000\nmax_open = 4\ncollection_cap = 4096\n\
             str_persist_bytes = 4096\nstr_cell_bytes = 128\nstr_scratch_bytes = 8192\n\
             intent_cap = 32\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("screen-universe.tsv"),
        "hyperliquid:btc\t-\thyperliquid:BTC\t4\tperp\thl\t10000\t1000000\t1000\t10000000\t\
         BTC\tBTC\tUSDC\tcrypto\n",
    )
    .unwrap();
    let a = ReplayArgv {
        window_root: root.clone(),
        manifest: root.join("manifest.toml"),
        fills: "judged".to_owned(),
        pine_dir: Some(script_dir()),
        report_out: Some(root.join("out/8g.jsonl")),
        ..ReplayArgv::default()
    };
    // The paper boot: fresh, its own record rendered into the window root.
    let universe = crate::pine::universe::load_universe(&root.join("screen-universe.tsv"))
        .unwrap_or_else(|e| panic!("{}", e.message()));
    let (manifest, _) = core_config::pine::load_manifest(&a.manifest).unwrap();
    let facts = crate::pine_boot::manifest_facts(
        &manifest,
        |_| Some(SYM),
        |key, sym| crate::pine_boot::facts_from_screen_universe(key, sym, &universe),
    )
    .unwrap();
    let mut fills = FillEngine::new(model(), u64::MAX);
    let set =
        boot_set(&a, facts, [0; 32], None, 0, &mut fills).unwrap_or_else(|f| panic!("{}", f.err));
    let rec = set.pine().boot_record(EA, 0).expect("record");
    std::fs::write(root.join("pine-boot.tsv"), rec.render()).unwrap();
    (Fx { root }, a)
}

fn model() -> ModelParams {
    parse_model_params(&[], None, &[], &[], &[], None).unwrap()
}

/// The paper session: the plain loop over the window, every emission
/// stamped `POP` after its record's ingress, slot 8's.
fn paper_orders(a: &ReplayArgv) -> Vec<Order> {
    let x = check_inputs(a).unwrap_or_else(|f| panic!("{}", f.err));
    let mut fills = FillEngine::new(model(), u64::MAX);
    let mut set =
        super::boot_from_record(a, &x, 0, &mut fills).unwrap_or_else(|f| panic!("{}", f.err));
    let recs = load_window(&x, &[SYM]).unwrap_or_else(|f| panic!("{}", f.err));
    let run = drive_judged(&mut set, &mut fills, &recs, None, &mut None)
        .unwrap_or_else(|f| panic!("{}", f.err));
    assert!(run.fail.is_none());
    let first = x.span.first_ts_ns;
    run.log
        .emits
        .iter()
        .map(|(o, virt)| {
            let mut c = *o;
            c.strategy_id = SLOT_PINE;
            c.ts_ns = first + (virt - VIRT_T0) + POP;
            c
        })
        .collect()
}

fn write_orders(a: &ReplayArgv, orders: &[Order]) {
    let mut w = PmlrWriter::open(
        run_dir(&a.window_root).join("engine-orders.pmlr"),
        SlotKind::Order,
        EA,
    )
    .unwrap();
    for o in orders {
        w.append(o).unwrap();
    }
    w.flush().unwrap();
}

fn report(a: &ReplayArgv) -> Vec<String> {
    std::fs::read_to_string(a.report_out.as_ref().unwrap())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn field(line: &str, key: &str) -> String {
    let k = format!("\"{key}\":");
    let at = line.find(&k).unwrap_or_else(|| panic!("{key} in {line}")) + k.len();
    line[at..]
        .split([',', '}'])
        .next()
        .unwrap()
        .trim_matches('"')
        .to_owned()
}

/// R5: the replayed member boots from the record — its own boot record
/// (oid bases, facts, manifest) is the input record, line for line.
#[test]
fn replay_from_boot_seeds_the_member_from_the_record() {
    let (fx, a) = fixture("seed");
    let x = check_inputs(&a).unwrap_or_else(|f| panic!("{}", f.err));
    let mut fills = FillEngine::new(model(), u64::MAX);
    let set =
        super::boot_from_record(&a, &x, 0, &mut fills).unwrap_or_else(|f| panic!("{}", f.err));
    let again = set.pine().boot_record(EA, 0).expect("record");
    assert_eq!(again, x.record);
    assert_eq!(again.symbols[0].sym, SYM);
    drop(fx);
}

/// F1 + F5: each member place takes its captured stamp's virt, and its
/// `t_active` is that plus the paper law's Δ; the window passes (exit 0)
/// with every captured order matched.
#[test]
fn judged_mode_activates_on_the_captured_stamp_with_the_paper_venue_law() {
    let (_fx, a) = fixture("activate");
    let orders = paper_orders(&a);
    assert!(orders.len() >= 2, "hold.pine trades in the window");
    write_orders(&a, &orders);
    let x = super::super::replay_from_boot_main(&a);
    assert_eq!(x.code, 0, "{}", x.stderr);
    let lines = report(&a);
    assert_eq!(field(&lines[0], "orders"), orders.len().to_string());
    assert_eq!(field(&lines[0], "unmatched_captured"), "0");
    let delta = model().latency_ns[usize::from(core_types::symbol_venue_byte(SYM))];
    let places: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("\"kind\":\"place\""))
        .collect();
    assert!(!places.is_empty());
    let first = check_inputs(&a).unwrap().span.first_ts_ns;
    for p in places {
        let k: usize = field(p, "k").parse().unwrap();
        let emit: u64 = field(p, "emit_virt_ns").parse().unwrap();
        assert_eq!(emit, virt_of(first, orders[k].ts_ns), "{p}");
        assert_eq!(field(p, "t_active_ns"), (emit + delta).to_string(), "{p}");
    }
}

/// F5 (legacy byte-identical by default): on the legacy anchor sym 7 the
/// switch takes Polymarket's Δ (the wire byte), the default Binance's.
#[test]
fn the_paper_delta_switch_is_off_by_default_and_reads_the_wire_byte() {
    const BN_ANCHOR: u32 = core_config::universe::LEGACY_BN_ANCHOR_SYM;
    let mut p = ModelParams {
        stale_after_ms: VenueId::stale_after_ms_defaults(),
        ..ModelParams::default()
    };
    p.latency_ns = [0; core_types::VENUE_COUNT];
    p.latency_ns[VenueId::Polymarket as usize] = 1_000_000_000;
    for (paper, want) in [(false, 1usize), (true, 0)] {
        let mut e = FillEngine::new(p, 0);
        e.set_paper_delta_law(paper);
        e.intake(
            &Order::new(
                0,
                VenueId::Polymarket,
                BN_ANCHOR,
                Side::Bid,
                1,
                Price::from_raw(400_000),
                Qty::from_raw(70_000_000),
                1,
            ),
            0,
        );
        let mut out = Vec::new();
        let t = Tick::new(
            0,
            VenueId::Polymarket,
            BN_ANCHOR,
            1,
            Price::from_raw(350_000),
            Qty::from_raw(1),
            Price::from_raw(380_000),
            Qty::from_raw(70_000_000),
        );
        e.on_record(&t, 10, 0, &mut out);
        assert_eq!(out.len(), want, "paper law {paper}");
    }
}

/// REVIEW §6 (iv): other slots' orders share the member's model — four
/// resting slot-3 bids on the symbol, injected before the member's first
/// order, fill the per-symbol table and the member's place is refused.
#[test]
fn judged_mode_injects_other_slots_orders_into_the_same_model() {
    let (_fx, a) = fixture("inject");
    let mut orders = paper_orders(&a);
    let at = orders[0].ts_ns.saturating_sub(SEC);
    let x = check_inputs(&a).unwrap();
    let mut fills = FillEngine::new(model(), u64::MAX);
    fills.set_paper_delta_law(true);
    let mut set = super::boot_from_record(&a, &x, 0, &mut fills).unwrap();
    let recs = load_window(&x, &[SYM]).unwrap();
    let clean = super::CapturedOrders::new(&orders);
    let clock = super::JudgedClock::new(clean, x.span.first_ts_ns, model().latency_ns);
    let base = drive_judged(&mut set, &mut fills, &recs, Some(clock), &mut None).unwrap();
    assert!(base.fail.is_none());
    assert_eq!(base.out.intake_refused, 0);
    for k in 0..crate::backtest::fill::MAX_OPEN_PER_SYM as u64 {
        let mut o = Order::new(
            at,
            VenueId::Hyperliquid,
            SYM,
            Side::Bid,
            0,
            Price::from_raw(1_000_000),
            Qty::from_raw(1_000_000),
            9_000 + k,
        );
        o.strategy_id = 3;
        orders.insert(0, o);
    }
    let mut fills = FillEngine::new(model(), u64::MAX);
    fills.set_paper_delta_law(true);
    let mut set = super::boot_from_record(&a, &x, 0, &mut fills).unwrap();
    let clock = super::JudgedClock::new(
        super::CapturedOrders::new(&orders),
        x.span.first_ts_ns,
        model().latency_ns,
    );
    let run = drive_judged(&mut set, &mut fills, &recs, Some(clock), &mut None).unwrap();
    assert!(
        run.out.intake_refused >= 1,
        "the injected bids took the sym's table"
    );
}

/// F6: the window keeps the ingress's stale flag (`rejudge_stale: false`)
/// where the venue stamp alone would call the tick fresh.
#[test]
fn judged_mode_keeps_the_captured_stale_flags() {
    let (_fx, a) = fixture("stale");
    let x = check_inputs(&a).unwrap();
    let recs = load_window(&x, &[SYM]).unwrap();
    let flagged: Vec<u32> = recs
        .iter()
        .filter_map(|r| match &r.payload {
            RecPayload::Tick(t) if t.flags & TICK_FLAG_STALE != 0 => Some(t.venue_seq),
            _ => None,
        })
        .collect();
    assert_eq!(
        flagged,
        vec![41],
        "exactly the captured stale tick (k = 40)"
    );
}

/// I26: activation margins are reported per place, a straddle-exposed
/// place is named, and none of it gates the exit.
#[test]
fn straddle_margins_are_reported_not_gated() {
    let (_fx, a) = fixture("straddle");
    let delta = model().latency_ns[usize::from(core_types::symbol_venue_byte(SYM))];
    // Stamp every slot-8 order so its t_active lands 1 ms before the next
    // quote: margin 1 ms < STRADDLE_NS.
    let pop = (STEP - delta % STEP) % STEP;
    let pop = if pop > 1_000_000 {
        pop - 1_000_000
    } else {
        pop + STEP - 1_000_000
    };
    let orders: Vec<Order> = paper_orders(&a)
        .into_iter()
        .map(|mut o| {
            o.ts_ns = o.ts_ns - POP + pop;
            o
        })
        .collect();
    write_orders(&a, &orders);
    let x = super::super::replay_from_boot_main(&a);
    assert_eq!(x.code, 0, "straddle exposure never gates: {}", x.stderr);
    let lines = report(&a);
    let places: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("\"kind\":\"place\""))
        .collect();
    assert!(!places.is_empty());
    for p in &places {
        let m: u64 = field(p, "margin_ns")
            .parse()
            .unwrap_or_else(|_| panic!("{p}"));
        assert!(m < STRADDLE_NS, "{p}");
        assert_eq!(field(p, "straddle_exposed"), "true");
    }
    let s = &lines[0];
    let n: u64 = ["agree", "straddle_exposed", "disagree"]
        .iter()
        .map(|k| field(s, k).parse::<u64>().unwrap())
        .sum();
    assert_eq!(n, places.len() as u64);
}

/// 8g-B (impl-08 §4.9): `--tape-out` writes the replay's tapes under
/// `<dir>/<boot_utc_ms>/` — the label `pine-verify --cross` pairs on — with
/// the decision records, the sequencing records and one `set.tsv` line.
#[test]
fn tape_out_writes_the_replay_tapes_under_the_boot_label() {
    let (fx, mut a) = fixture("tape");
    write_orders(&a, &paper_orders(&a));
    a.tape_out = Some(fx.root.join("tape"));
    let x = super::super::replay_from_boot_main(&a);
    assert_eq!(x.code, 0, "{}", x.stderr);
    // pin:Bp-8g-boot-utc-ms: run epoch + boot_mono (0) − first ts (the
    // first print, at 0).
    let label = (EA / 1_000_000).to_string();
    assert_eq!(field(&x.stdout, "boot_utc_ms"), label);
    let dir = fx.root.join("tape").join(&label);
    let dec = std::fs::read(dir.join("decisions.bin")).unwrap();
    let seq = std::fs::read(dir.join("seq.bin")).unwrap();
    assert!(!dec.is_empty() && dec.len() % 64 == 0, "{}", dec.len());
    assert!(!seq.is_empty() && seq.len() % 128 == 0, "{}", seq.len());
    let set = std::fs::read_to_string(dir.join("set.tsv")).unwrap();
    let lines: Vec<&str> = set.lines().collect();
    assert_eq!(lines.len(), 2, "{set}");
    assert!(lines[0].starts_with("#utc_ms\tmask\t"));
    assert!(lines[1].starts_with(&format!("{label}\t")));
}

/// I47 end to end (impl-08 T22): a window's tapes and its replay-from-boot tapes, both written
/// by `--tape-out`, paired by label (`pine_verify::pair`) — a clean pairing has no divergence and
/// a non-empty prefix; a seeded one-field mutation of the recorded side is reported at its
/// index, unattributed (red).
#[test]
fn cross_pairs_replay_tapes_by_label_and_reports_a_seeded_divergence() {
    use crate::pine_verify::{cross, pair};
    let (fx, mut a) = fixture("cross");
    write_orders(&a, &paper_orders(&a));
    a.tape_out = Some(fx.root.join("live"));
    assert_eq!(super::super::replay_from_boot_main(&a).code, 0);
    a.tape_out = Some(fx.root.join("replay"));
    assert_eq!(super::super::replay_from_boot_main(&a).code, 0);
    let label = (EA / 1_000_000).to_string();
    let live = cross::read_tapes(&fx.root.join("live").join(&label)).expect("live tapes");
    let rep = pair(&fx.root.join("replay"), &label, &live.dec)
        .expect("readable")
        .expect("paired");
    // Rule 7: no replay tape under the label → ineligible, listed.
    let none = pair(&fx.root.join("replay"), "1", &live.dec).expect("readable");
    assert!(none.unwrap_err().contains("rule 7"));
    let facts = vec![cross::ScriptFacts {
        sym: SYM,
        syms: vec![SYM],
        delta_ns: model().latency_ns[usize::from(core_types::symbol_venue_byte(SYM))],
        ..cross::ScriptFacts::default()
    }];
    let o = cross::cross(&live, &rep, &facts);
    assert!(o.divergences.is_empty(), "{o:?}");
    assert!(o.prefix_orders >= 1, "{o:?}");
    // Seed: the first DISPATCH's px + 1 on the recorded side.
    let mut seeded = live.clone();
    let i = seeded
        .dec
        .iter()
        .position(|d| d.kind == strategy_core::pine_tape::DISPATCH)
        .expect("a dispatch");
    seeded.dec[i].x0 += 1;
    let o = cross::cross(&seeded, &rep, &facts);
    assert_eq!(o.divergences.len(), 1, "{o:?}");
    let d = &o.divergences[0];
    assert_eq!((d.index, d.class), (0, cross::Class::Unattributed), "{o:?}");
    assert_eq!(o.unattributed(), 1);
    assert!(d.detail.contains("live Some("), "{}", d.detail);
}
