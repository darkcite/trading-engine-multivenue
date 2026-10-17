// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! I47 (§7.9 `cross_predicates_classify_constructed_divergences`): one constructed live /
//! replay tape pair per class, plus an unexplained one and a clean pair.

use core_types::{
    make_symbol_id, ChannelEvent, ChannelId, Price, Qty, Tick, VenueId, TICK_FLAG_STALE,
};
use strategy_core::{pine_seq, pine_tape, PineSeqRec, PineTapeRec};

use std::path::Path;

use strategy_pine::{HostKind, SlotRoute};

use super::super::pair;
use super::{cross, replay_fills, Class, ScriptFacts, Tapes, CROSS_FILLS};

const T0_MS: i64 = 1_767_225_600_000;
const EPOCH: u32 = 7;

fn sym() -> u32 {
    make_symbol_id(VenueId::Hyperliquid, 1)
}

fn tick(ts: u64, stale: bool) -> Tick {
    let mut t = Tick::new(
        ts,
        VenueId::Hyperliquid,
        sym(),
        u32::try_from(ts).unwrap_or(0),
        Price::from_raw(99_000_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(101_000_000),
        Qty::from_raw(1_000_000),
    );
    if stale {
        t.flags |= TICK_FLAG_STALE;
    }
    t
}

fn rec_tick(seq: u64, t: &Tick, pop: u64) -> PineSeqRec {
    PineSeqRec {
        seq,
        pop_ns: pop,
        e_ns: pop,
        kind: pine_seq::TICK,
        payload: super::super::words(t),
        ..PineSeqRec::default()
    }
}

fn rec_event(seq: u64, ts: u64) -> PineSeqRec {
    let ev = ChannelEvent::new(
        ts,
        VenueId::Hyperliquid,
        ChannelId::Trade,
        sym(),
        0,
        0,
        100_000_000,
        1_000_000,
    );
    PineSeqRec {
        seq,
        pop_ns: ts,
        e_ns: ts,
        kind: pine_seq::EVENT,
        payload: super::super::words(&ev),
        ..PineSeqRec::default()
    }
}

fn hdr() -> PineTapeRec {
    PineTapeRec {
        kind: pine_tape::BOOT_HDR,
        x0: 0,
        x1: T0_MS * 1_000_000,
        x2: u64::from(EPOCH),
        aux: T0_MS as u64,
        ..PineTapeRec::default()
    }
}

fn dispatch(seq_ref: u64, px: i64) -> PineTapeRec {
    PineTapeRec {
        kind: pine_tape::DISPATCH,
        seq_ref,
        a: pine_ir::EngineVerb::Submit as u8,
        b: pine_ir::Primitive::TakerIoc as u8,
        oid: 1,
        x0: px,
        x1: 1_000_000,
        ..PineTapeRec::default()
    }
}

fn fill(seq_ref: u64, px: i64, time_ms: u64) -> PineTapeRec {
    PineTapeRec {
        kind: pine_tape::FILL,
        seq_ref,
        oid: 1,
        x0: px,
        x1: 1_000_000,
        aux: time_ms,
        ..PineTapeRec::default()
    }
}

fn tapes(seq: Vec<PineSeqRec>, mut dec: Vec<PineTapeRec>) -> Tapes {
    dec.insert(0, hdr());
    for (i, d) in dec.iter_mut().enumerate() {
        d.seq = i as u64;
    }
    Tapes { seq, dec }
}

fn facts(delta_ns: u64, ttl_ns: u64, trade_time: bool) -> Vec<ScriptFacts> {
    vec![ScriptFacts {
        sym: sym(),
        syms: vec![sym()],
        delta_ns,
        ttl_ns,
        trade_time,
    }]
}

/// One divergence of `class` at `index` (the FILL / DISPATCH under test).
fn assert_one(o: &super::CrossOut, class: Class, index: usize) {
    assert_eq!(o.divergences.len(), 1, "{o:?}");
    let d = &o.divergences[0];
    assert_eq!((d.class, d.index), (class, index), "{o:?}");
    assert_eq!(o.counts[class as usize], 1);
    assert_eq!(o.counts.iter().sum::<u64>(), 1);
}

const FAR: u64 = 1 << 50;

#[test]
fn cross_predicates_classify_constructed_divergences() {
    // Clean: the same tapes → no divergence; the prefix counts the submit and the fill.
    let seq = vec![
        rec_tick(1, &tick(1_000, false), 1_000),
        rec_tick(2, &tick(2_000, false), 2_000),
    ];
    let dec = vec![dispatch(1, 101_000_000), fill(2, 101_000_000, T0_MS as u64)];
    let live = tapes(seq.clone(), dec.clone());
    let o = cross(&live, &live.clone(), &facts(FAR, 0, false));
    assert!(o.divergences.is_empty(), "{o:?}");
    assert_eq!((o.prefix_orders, o.prefix_fills), (1, 1));

    // Straddle: E 1 000 + Δ 500 = t_active 1 500; the judging tick T is ingress 1 400, pop
    // 1 600 — paper judged at T, the record-time replay later at another price.
    let seq = vec![
        rec_tick(1, &tick(1_000, false), 1_000),
        rec_tick(2, &tick(1_400, false), 1_600),
    ];
    let live = tapes(
        seq.clone(),
        vec![dispatch(1, 101_000_000), fill(2, 101_000_000, 1)],
    );
    let rep = tapes(
        seq.clone(),
        vec![dispatch(1, 101_000_000), fill(2, 101_500_000, 1)],
    );
    let o = cross(&live, &rep, &facts(500, 0, false));
    assert_one(&o, Class::Straddle, 1);
    assert_eq!((o.prefix_orders, o.prefix_fills), (1, 0));

    // Expiry-straddle: Δ out of reach, TTL 300 → expiry 1 300 between T's ingress 1 200 and pop
    // 1 350.
    let seq = vec![
        rec_tick(1, &tick(1_000, false), 1_000),
        rec_tick(2, &tick(1_200, false), 1_350),
    ];
    let live = tapes(
        seq.clone(),
        vec![dispatch(1, 101_000_000), fill(2, 101_000_000, 1)],
    );
    let rep = tapes(seq, vec![dispatch(1, 101_000_000), fill(2, 101_500_000, 1)]);
    assert_one(
        &cross(&live, &rep, &facts(FAR, 300, false)),
        Class::ExpiryStraddle,
        1,
    );

    // Interleave: the tick and the print in the other order; the dispatch after both differs.
    let (t, e) = (tick(1_000, false), 1_100);
    let live = tapes(
        vec![rec_tick(1, &t, 1_000), rec_event(2, e)],
        vec![dispatch(2, 101_000_000)],
    );
    let rep = tapes(
        vec![rec_event(1, e), rec_tick(2, &t, 1_000)],
        vec![dispatch(2, 100_000_000)],
    );
    assert_one(
        &cross(&live, &rep, &facts(FAR, 0, false)),
        Class::Interleave,
        0,
    );

    // Stale-rejudge: the same tick, its stale flag re-judged by the harness.
    let live = tapes(
        vec![rec_tick(1, &tick(1_000, true), 1_000)],
        vec![dispatch(1, 101_000_000)],
    );
    let rep = tapes(
        vec![rec_tick(1, &tick(1_000, false), 1_000)],
        vec![dispatch(1, 100_000_000)],
    );
    assert_one(
        &cross(&live, &rep, &facts(FAR, 0, false)),
        Class::StaleRejudge,
        0,
    );

    // Fill-stamp: a fill equal but for `time_ms`, the program reads trade times.
    let seq = vec![
        rec_tick(1, &tick(1_000, false), 1_000),
        rec_tick(2, &tick(2_000, false), 2_000),
    ];
    let live = tapes(
        seq.clone(),
        vec![dispatch(1, 101_000_000), fill(2, 101_000_000, 5)],
    );
    let rep = tapes(
        seq.clone(),
        vec![dispatch(1, 101_000_000), fill(2, 101_000_000, 6)],
    );
    assert_one(
        &cross(&live, &rep, &facts(FAR, 0, true)),
        Class::FillStamp,
        1,
    );

    // Unexplained: the same pair, the program reads no trade time → unattributed (red).
    let o = cross(&live, &rep, &facts(FAR, 0, false));
    assert_one(&o, Class::Unattributed, 1);
    assert_eq!(o.unattributed(), 1);

    // A record missing on one side is the divergence point too.
    let short = tapes(seq, vec![dispatch(1, 101_000_000)]);
    let o = cross(&live, &short, &facts(FAR, 0, false));
    assert_one(&o, Class::Unattributed, 1);
}

// ---------------------------------------------------------------- pairing (risk r22 B1, N1)

/// A boot's header: route, host, the live epoch / label.
fn boot_hdr(route: SlotRoute, host: HostKind) -> PineTapeRec {
    PineTapeRec {
        a: route as u8,
        b: host as u8,
        ..hdr()
    }
}

/// Writes `<root>/<label>/` with an empty `seq.bin`, `dec` and, when `fills` is set, the
/// replay's `replay.tsv` naming it.
fn write_tape(root: &Path, dec: &[PineTapeRec], fills: Option<&str>) -> String {
    let label = T0_MS.to_string();
    let dir = root.join(&label);
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("seq.bin"), b"").expect("seq");
    let mut b = Vec::new();
    for (i, r) in dec.iter().enumerate() {
        let mut r = *r;
        r.seq = i as u64;
        b.extend_from_slice(&crate::pine_tape::dec_bytes(&r));
    }
    std::fs::write(dir.join("decisions.bin"), b).expect("dec");
    if let Some(m) = fills {
        std::fs::write(
            dir.join(crate::pine_tape::REPLAY_FILE),
            format!("{}\nfills\t{m}\n", crate::pine_tape::REPLAY_MAGIC),
        )
        .expect("replay.tsv");
    }
    label
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pine-cross-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn script_rec() -> PineTapeRec {
    PineTapeRec {
        kind: pine_tape::BOOT_SCRIPT,
        a: 1,
        x0: 11,
        ..PineTapeRec::default()
    }
}

#[test]
fn cross_refuses_the_live_tape_as_its_own_replay() {
    let live = vec![boot_hdr(SlotRoute::Paper, HostKind::Engine), script_rec()];
    let want = Err("8g-B: paired tape is not this boot's replay-from-boot tape".to_string());
    // The live tape itself under the cross root (even with a replay.tsv beside it).
    let root = tmp("self");
    let label = write_tape(&root, &live, Some(CROSS_FILLS));
    assert_eq!(pair(&root, &label, &live).expect("readable"), want);
    // A replay header of another boot (epoch, label) or another route is refused too.
    let other = tmp("other");
    for bad in [
        PineTapeRec {
            x2: u64::from(EPOCH) + 1,
            ..boot_hdr(SlotRoute::Paper, HostKind::Replay)
        },
        PineTapeRec {
            aux: T0_MS as u64 + 1,
            ..boot_hdr(SlotRoute::Paper, HostKind::Replay)
        },
        boot_hdr(SlotRoute::Off, HostKind::Replay),
    ] {
        let _ = std::fs::remove_dir_all(&other);
        let label = write_tape(&other, &[bad, script_rec()], Some(CROSS_FILLS));
        assert_eq!(pair(&other, &label, &live).expect("readable"), want);
    }
    // A live side that is not the Engine host has no replay.
    let host = tmp("host");
    let rep = [boot_hdr(SlotRoute::Paper, HostKind::Replay), script_rec()];
    let label = write_tape(&host, &rep, Some(CROSS_FILLS));
    let not_engine = vec![boot_hdr(SlotRoute::Paper, HostKind::Replay), script_rec()];
    assert_eq!(pair(&host, &label, &not_engine).expect("readable"), want);
    // This boot's replay: Paper, Replay, the live epoch and label (the anchor words differ).
    let mut ok = boot_hdr(SlotRoute::Paper, HostKind::Replay);
    ok.x1 = 0;
    let good = tmp("ok");
    let label = write_tape(&good, &[ok, script_rec()], Some(CROSS_FILLS));
    let got = pair(&good, &label, &live)
        .expect("readable")
        .expect("paired");
    assert_eq!(got.dec.len(), 2);
    for d in [root, other, host, good] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn cross_refuses_a_replay_made_in_the_wrong_fills_mode() {
    let live = vec![boot_hdr(SlotRoute::Paper, HostKind::Engine), script_rec()];
    let rep = [boot_hdr(SlotRoute::Paper, HostKind::Replay), script_rec()];
    assert_eq!(CROSS_FILLS, "recorded");
    for (tag, fills, want) in [
        ("judged", Some("judged"), "fill source is judged"),
        // pin:08-replay-file-after-clean-drive (risk r23 B2).
        ("none", None, "unrecorded or unfinished replay"),
    ] {
        let root = tmp(tag);
        let label = write_tape(&root, &rep, fills);
        let e = pair(&root, &label, &live)
            .expect("readable")
            .expect_err("refused");
        assert!(e.contains(want), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }
    // A malformed replay.tsv (no magic line) names no mode.
    let root = tmp("torn");
    let label = write_tape(&root, &rep, None);
    std::fs::write(
        root.join(&label).join(crate::pine_tape::REPLAY_FILE),
        "fills\trecorded\n",
    )
    .expect("torn");
    assert_eq!(replay_fills(&root.join(&label)), None);
    assert!(pair(&root, &label, &live).expect("readable").is_err());
    let _ = std::fs::remove_dir_all(&root);
}

/// pin:08-cross-fields-listed (risk r22 N2): a pair is equal iff the fields §4.9.1 lists for its
/// kind are — the four unlisted tape fields (`DISPOSITION.b`, `FILL.a`, `FILL.b`,
/// `FILL.bar_index`) never make a divergence; every listed one does.
#[test]
fn cross_compares_only_the_listed_fields() {
    use super::same;
    let disp = PineTapeRec {
        kind: pine_tape::DISPOSITION,
        oid: 1,
        a: 2,
        b: 1,
        ..PineTapeRec::default()
    };
    let f = fill(1, 100, 5);
    // The four fields §4.9.1 does not list.
    assert!(same(&disp, &PineTapeRec { b: 3, ..disp }));
    assert!(same(&f, &PineTapeRec { a: 9, ..f }));
    assert!(same(&f, &PineTapeRec { b: 1, ..f }));
    assert!(same(&f, &PineTapeRec { bar_index: 4, ..f }));
    // Every listed field.
    assert!(!same(&disp, &PineTapeRec { a: 3, ..disp }));
    assert!(!same(&disp, &PineTapeRec { oid: 2, ..disp }));
    let fills = [
        PineTapeRec { oid: 2, ..f },
        PineTapeRec { x0: 101, ..f },
        PineTapeRec {
            x1: -1_000_000,
            ..f
        },
        PineTapeRec { x2: 1, ..f },
        PineTapeRec { aux: 6, ..f },
    ];
    for (i, g) in fills.iter().enumerate() {
        assert!(!same(&f, g), "FILL field {i}");
    }
    let d = dispatch(1, 100);
    let dispatches = [
        PineTapeRec {
            a: d.a.wrapping_add(1),
            ..d
        },
        PineTapeRec {
            b: d.b.wrapping_add(1),
            ..d
        },
        PineTapeRec { c: 1, ..d },
        PineTapeRec { bar_index: 1, ..d },
        PineTapeRec { oid: 2, ..d },
        PineTapeRec { x0: 101, ..d },
        PineTapeRec { x1: 2, ..d },
        PineTapeRec { x2: 1, ..d },
        PineTapeRec { aux: 1, ..d },
    ];
    for (i, g) in dispatches.iter().enumerate() {
        assert!(!same(&d, g), "DISPATCH field {i}");
    }
}
