// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'3 T19 (impl-Bp §7.8, I25): the replay-from-boot refusals, each exit 6
//! before any record (pin:Bp-b3-tests-in-crate).

use std::path::PathBuf;

use core_io::{PmlrWriter, SlotKind};
use core_types::{make_symbol_id, Price, Qty, Tick, VenueId};
use strategy_pine::{BootRecord, SlotRoute};

use super::{check_inputs, FillSource, ReplayArgv, BOOT_SLACK_NS};

const EA: u64 = 1_767_225_600_000_000_000;
const SEC: u64 = 1_000_000_000;
const SYM: u32 = make_symbol_id(VenueId::Hyperliquid, 0);

struct Fx {
    root: PathBuf,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A window root holding `run-<EA>` whose first tick is at `first_s`, a
/// manifest, and a boot record naming `rec_epoch` / `boot_mono_ns` and the
/// manifest's hash (or `[0; 32]` when `hash_ok` is false).
fn fixture(
    tag: &str,
    first_s: u64,
    rec_epoch: u64,
    boot_mono_ns: u64,
    hash_ok: bool,
) -> (Fx, ReplayArgv) {
    let root = std::env::temp_dir().join(format!("cli-pine-8g-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let run = root.join(format!("run-{EA}"));
    std::fs::create_dir_all(&run).unwrap();
    let mut wt = PmlrWriter::open(run.join("hl-ticks.pmlr"), SlotKind::Tick, EA).unwrap();
    for k in 0..4u64 {
        let ts = first_s * SEC + k * SEC;
        wt.append(&Tick::new(
            ts,
            VenueId::Hyperliquid,
            SYM,
            k as u32 + 1,
            Price::from_raw(99_990_000),
            Qty::from_raw(50_000_000),
            Price::from_raw(100_010_000),
            Qty::from_raw(50_000_000),
        ))
        .unwrap();
    }
    wt.flush().unwrap();
    let manifest = b"[[script]]\n# the bytes only are hashed here\n";
    std::fs::write(root.join("manifest.toml"), manifest).unwrap();
    let rec = BootRecord {
        manifest_sha256: if hash_ok {
            core_crypto::sha256(manifest)
        } else {
            [0; 32]
        },
        run_epoch_ns: rec_epoch,
        boot_mono_ns,
        route: SlotRoute::Paper,
        scripts: Vec::new(),
        symbols: Vec::new(),
        queue_syms: Vec::new(),
        exec_sha256: [0; 32],
    };
    std::fs::write(root.join("pine-boot.tsv"), rec.render()).unwrap();
    let a = ReplayArgv {
        window_root: root.clone(),
        manifest: root.join("manifest.toml"),
        fills: "judged".to_owned(),
        ..ReplayArgv::default()
    };
    (Fx { root }, a)
}

fn refused(a: &ReplayArgv, why: &str) {
    match check_inputs(a) {
        Ok(_) => panic!("{why}: accepted"),
        Err(f) => {
            assert_eq!(f.exit, 6, "{why}: {}", f.err);
            assert!(f.err.to_string().contains("replay-from-boot"), "{}", f.err);
        }
    }
}

/// I25: a window anchored inside `BOOT_SLACK_NS` of the boot passes; one
/// anchored later is not "from boot" and is refused.
#[test]
fn replay_from_boot_refuses_a_window_not_starting_at_boot() {
    let (_fx, a) = fixture("atboot", 60, EA, 0, true);
    let x = check_inputs(&a).unwrap_or_else(|f| panic!("{}", f.err));
    assert_eq!(x.fills, FillSource::Judged);
    assert_eq!(x.span.first_ts_ns, 60 * SEC);
    assert_eq!((x.record.run_epoch_ns, x.run.epoch_ns), (EA, EA));
    assert!(x.record_path.ends_with("pine-boot.tsv"));
    assert!(x.journal.is_none());
    let late = BOOT_SLACK_NS / SEC + 1;
    let (_fx2, a2) = fixture("late", late, EA, 0, true);
    refused(&a2, "anchor later than boot + slack");
}

/// I25: a manifest that does not hash to the record is refused.
#[test]
fn replay_from_boot_refuses_a_manifest_mismatch() {
    let (_fx, a) = fixture("mf", 0, EA, 0, false);
    refused(&a, "manifest mismatch");
}

/// I25: `--fills recorded` without `--delivery-journal` is refused.
#[test]
fn recorded_fills_without_a_journal_is_refused() {
    let (_fx, mut a) = fixture("nojournal", 0, EA, 0, true);
    a.fills = "recorded".to_owned();
    refused(&a, "recorded without a journal");
    a.fills = String::new();
    refused(&a, "recorded is the default");
}

/// §4.12: a record naming another run, or a root of two runs, is refused.
#[test]
fn replay_from_boot_refuses_another_runs_record_and_a_two_run_root() {
    let (_fx, a) = fixture("epoch", 0, EA + 1, 0, true);
    refused(&a, "record epoch is not the run's");
    let (fx2, a2) = fixture("tworuns", 0, EA, 0, true);
    std::fs::create_dir_all(fx2.root.join(format!("run-{}", EA + 7))).unwrap();
    refused(&a2, "two runs");
}

// ---- T20 / T19 planners (§7.8; the member drive over them is T19b) ----

use core_types::{ChannelEvent, ChannelId, Fill, Order, Side};
use strategy_pine::delivery::{kind, PineDeliveryRec};
use strategy_set::SLOT_PINE;

use super::{plan_recorded, CapturedOrders, Delivery, EXIT_DIVERGED};
use crate::backtest::{MergedRec, RecPayload};

const HL: u8 = VenueId::Hyperliquid as u8;

fn jrec(kind: u8, ts_ns: u64, seq: u64, aux: u64) -> PineDeliveryRec {
    PineDeliveryRec {
        ts_ns,
        seq,
        sym: SYM,
        kind,
        venue: HL,
        aux,
        ..PineDeliveryRec::default()
    }
}

fn tick_rec(ts: u64) -> MergedRec {
    MergedRec {
        payload: RecPayload::Tick(Tick::new(
            ts,
            VenueId::Hyperliquid,
            SYM,
            1,
            Price::from_raw(99_990_000),
            Qty::from_raw(50_000_000),
            Price::from_raw(100_010_000),
            Qty::from_raw(50_000_000),
        )),
        virt_ns: ts,
        wall_ns: ts,
    }
}

fn print_rec(ts: u64, seq: u64) -> MergedRec {
    MergedRec {
        payload: RecPayload::Event(ChannelEvent::new(
            ts,
            VenueId::Hyperliquid,
            ChannelId::Trade,
            SYM,
            seq,
            ts / 1_000_000,
            100_000_000,
            1_000_000,
        )),
        virt_ns: ts,
        wall_ns: ts,
    }
}

fn fill(ts: u64, oid: u64, slot: u8) -> Fill {
    let mut f = Fill::new(
        ts,
        SYM,
        Side::Bid,
        Price::from_raw(100_010_000),
        Qty::from_raw(120_000),
        oid,
    );
    f.strategy_id = slot;
    f
}

/// T20: the planned delivery is the journal's sequence — its order, not
/// the capture's (a print the member saw before an earlier-ingressed tick
/// stays before it), equal keys consumed in capture order, fills from the
/// slot-8 rows only, dispositions and timers as journaled.
#[test]
fn recorded_mode_delivers_exactly_the_journal_sequence() {
    let ch = ChannelId::Trade as u8 as u64;
    let recs = [
        tick_rec(10),
        print_rec(11, 5),
        tick_rec(12),
        tick_rec(12),
        print_rec(13, 6),
    ];
    let fills = [fill(20, 77, 3), fill(20, 77, SLOT_PINE)];
    let journal = [
        jrec(kind::TIMER, 9, 0, 0),
        jrec(kind::EVENT, 11, 5, ch),
        jrec(kind::TICK, 10, 0, 0),
        jrec(kind::TICK, 12, 0, 0),
        jrec(kind::TICK, 12, 0, 0),
        jrec(kind::FILL, 20, 77, 0),
        jrec(kind::DONE, 20, 77, 0),
        jrec(kind::EVENT, 13, 6, ch),
    ];
    let plan = plan_recorded(&journal, &recs, &fills).unwrap_or_else(|f| panic!("{}", f.err));
    assert_eq!(
        plan,
        vec![
            Delivery::Timer(9),
            Delivery::Rec(1),
            Delivery::Rec(0),
            Delivery::Rec(2),
            Delivery::Rec(3),
            Delivery::Fill(1),
            Delivery::Done(77),
            Delivery::Rec(4),
        ]
    );
}

/// T20 (F14): a journal record without its capture input — a tick the
/// capture lacks, a print on another channel, a third tick at a key the
/// capture holds twice, a fill of another slot — exits 5 naming its index.
#[test]
fn a_journal_record_without_a_capture_match_exits_5() {
    let ch = ChannelId::Trade as u8 as u64;
    let recs = [tick_rec(10), tick_rec(12), tick_rec(12), print_rec(13, 6)];
    let fills = [fill(20, 77, 3)];
    let cases: [(&[PineDeliveryRec], usize); 4] = [
        (&[jrec(kind::TICK, 10, 0, 0), jrec(kind::TICK, 11, 0, 0)], 1),
        (&[jrec(kind::EVENT, 13, 6, ch + 1)], 0),
        (
            &[
                jrec(kind::TICK, 12, 0, 0),
                jrec(kind::TICK, 12, 0, 0),
                jrec(kind::TICK, 12, 0, 0),
            ],
            2,
        ),
        (
            &[jrec(kind::TIMER, 1, 0, 0), jrec(kind::FILL, 20, 77, 0)],
            1,
        ),
    ];
    for (journal, at) in cases {
        match plan_recorded(journal, &recs, &fills) {
            Ok(p) => panic!("index {at}: planned {p:?}"),
            Err(f) => {
                assert_eq!(f.exit, EXIT_DIVERGED, "{}", f.err);
                let msg = f.err.to_string();
                assert!(msg.contains(&format!("divergence at index {at}:")), "{msg}");
            }
        }
    }
}

fn order(ts: u64, oid: u64, px: i64, slot: u8) -> Order {
    let mut o = Order::new(
        ts,
        VenueId::Hyperliquid,
        SYM,
        Side::Bid,
        1,
        Price::from_raw(px),
        Qty::from_raw(120_000),
        oid,
    );
    o.strategy_id = slot;
    o
}

/// T19: the member's k-th order takes the k-th captured slot-8 order's
/// stamp; other slots' orders are set aside for injection; the first
/// mismatch (here k = 1, a price) exits 5 with its index, and an order
/// beyond the capture does too.
#[test]
fn an_order_mismatch_exits_5_with_the_first_index() {
    let captured = [
        order(1_000, 11, 100_000_000, SLOT_PINE),
        order(1_500, 99, 50_000_000, 3),
        order(2_000, 12, 100_010_000, SLOT_PINE),
    ];
    let mut c = CapturedOrders::new(&captured);
    assert_eq!(c.others.len(), 1);
    assert_eq!(c.others[0].client_oid, 99);
    // The member's own oid and stamp differ from paper's (D23 excludes both).
    let first = order(7, 501, 100_000_000, SLOT_PINE);
    assert_eq!(
        c.stamp(&first).unwrap_or_else(|f| panic!("{}", f.err)),
        1_000
    );
    assert_eq!(c.unmatched(), 1);
    let bad = order(8, 502, 100_020_000, SLOT_PINE);
    let Err(f) = c.stamp(&bad) else {
        panic!("a price mismatch is a divergence")
    };
    assert_eq!(f.exit, EXIT_DIVERGED);
    assert!(
        f.err.to_string().contains("divergence at index 1:"),
        "{}",
        f.err
    );
    let ok = order(9, 503, 100_010_000, SLOT_PINE);
    assert_eq!(c.stamp(&ok).unwrap_or_else(|f| panic!("{}", f.err)), 2_000);
    let Err(f) = c.stamp(&ok) else {
        panic!("beyond the capture")
    };
    assert_eq!(f.exit, EXIT_DIVERGED);
    assert!(
        f.err.to_string().contains("divergence at index 2:"),
        "{}",
        f.err
    );
}
