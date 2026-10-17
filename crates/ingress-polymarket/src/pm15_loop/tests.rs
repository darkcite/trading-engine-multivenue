// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The PM15 driver against the venue's own frames: the BTC window that
//! opened 2026-10-17 06:00Z, captured from 28 s before its end through its
//! resolution (`tests/fixtures/pm15/`, 914 frames of its market).
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `pm15_loop.rs`): every copy here builds a fixture or a fake wire.

use super::*;
use crate::gamma15::parse_window_spec;
use crate::rolling::PmSlotState;
use core_ring::Ring;
use core_types::pm_roll::{
    pm_instance_id, pm_mark_sym, pm_pair_of_window, pm_roll_family, pm_window_of_open, PM_LEG_DOWN,
    PM_SETTLED_UP,
};
use core_types::{roll_kind, unpack_roll_seq, ROLL_KIND_CREATED, ROLL_KIND_SETTLED};

const UP: &[u8] = b"74913418352376030655968323928940542465856871160803692353977404305259528321778";
const DOWN: &[u8] =
    b"108416957532714595532891587985594213692071846013834323276407586325741402897137";
const OPEN: u64 = 1_792_216_800;
const MS: u64 = 1_000_000;

#[derive(Default)]
struct Rec {
    ticks: Vec<Tick>,
    events: Vec<ChannelEvent>,
    rejects: usize,
    raw: usize,
}

impl Capture for Rec {
    fn tick(&mut self, t: &Tick) {
        self.ticks.push(*t);
    }
    fn event(&mut self, e: &ChannelEvent) {
        self.events.push(*e);
    }
    fn raw_frame(&mut self, _ts: NsTs, _p: &[u8]) {
        self.raw += 1;
    }
    fn parse_reject(&mut self, _ts: NsTs, _p: &[u8]) {
        self.rejects += 1;
    }
}

#[derive(Default)]
struct Wire {
    open: bool,
    frames: Vec<Vec<u8>>,
}

impl Pm15Wire for Wire {
    fn is_open(&self) -> bool {
        self.open
    }
    fn queue(&mut self, parts: &[&[u8]]) -> bool {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p);
        }
        self.frames.push(v);
        true
    }
}

struct Rig {
    tick: Producer<Tick, DEFAULT_TICK_RING_CAP>,
    tick_rx: Consumer<Tick, DEFAULT_TICK_RING_CAP>,
    event: Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_rx: Consumer<ChannelEvent, EVENT_RING_SIZE>,
    status: IngressStatus,
    counters: Pm15Counters,
    rec: Rec,
}

impl Rig {
    fn new() -> Self {
        let (tick, tick_rx) = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new().split();
        let (event, event_rx) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
        Self {
            tick,
            tick_rx,
            event,
            event_rx,
            status: IngressStatus::new(),
            counters: Pm15Counters::new(),
            rec: Rec::default(),
        }
    }

    fn out(&mut self) -> Pm15Out<'_, Rec> {
        Pm15Out {
            tick: &mut self.tick,
            event: &mut self.event,
            event_mask: PM15_EVENT_MASK,
            status: &self.status,
            counters: &self.counters,
            capture: &mut self.rec,
        }
    }

    fn lane_events(&mut self) -> Vec<ChannelEvent> {
        let mut v = Vec::new();
        while let Some(e) = self.event_rx.try_pop_ref() {
            v.push(*e);
        }
        v
    }

    fn lane_ticks(&mut self) -> usize {
        let mut n = 0;
        while self.tick_rx.try_pop_ref().is_some() {
            n += 1;
        }
        n
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let p = format!("{}/tests/fixtures/pm15/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

fn btc_spec() -> PmWindowSpec {
    let w = pm_window_of_open(OPEN).unwrap();
    let mut s = PmWindowSpec::EMPTY;
    parse_window_spec(&fixture("gamma_btc_1792216800.json"), 0, b"btc", w, &mut s)
        .expect("the live answer passes the law");
    assert_eq!(s.up.as_bytes(), UP);
    assert_eq!(s.down.as_bytes(), DOWN);
    s
}

/// The last row's `best_bid` / `best_ask` per asset of a `price_change`
/// frame, by an independent scan (string search, not the parser).
fn final_touch(p: &[u8], id: &[u8]) -> Option<(i64, i64)> {
    let s = std::str::from_utf8(p).unwrap();
    let mut got = None;
    for row in s.split("\"asset_id\":\"").skip(1) {
        if !row.starts_with(std::str::from_utf8(id).unwrap()) {
            continue;
        }
        let px = |k: &str| -> i64 {
            let at = row.find(k).unwrap() + k.len();
            let v = &row[at..];
            let v = v.trim_start().trim_start_matches('"');
            let end = v.find('"').unwrap();
            (v[..end].parse::<f64>().unwrap() * 1e6).round() as i64
        };
        got = Some((px("\"best_bid\":"), px("\"best_ask\":")));
    }
    got
}

#[test]
fn the_live_window_replays_into_ticks_rolls_and_its_settlement() {
    let spec = btc_spec();
    let w = spec.window;
    let s = pm_roll_family(0, pm_pair_of_window(w));
    let inst = pm_instance_id(w, 0).unwrap();
    let frames = fixture("market_btc_1792216800.frames");
    let lines: Vec<(u64, &[u8])> = frames
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            let tab = l.iter().position(|b| *b == b'\t').unwrap();
            (
                std::str::from_utf8(&l[..tab])
                    .unwrap()
                    .parse::<u64>()
                    .unwrap(),
                &l[tab + 1..],
            )
        })
        .collect();
    assert_eq!(lines.len(), 914);

    let mut rig = Rig::new();
    let mut wire = Wire::default();
    let mut d = Pm15Driver::new(&[b"btc"], 5_000);
    let t0 = lines[0].0 * MS;
    // The window is already open at the first frame: bound and created.
    d.offer(&spec, t0, t0, &mut rig.out(), &mut wire);
    assert!(
        wire.frames.is_empty(),
        "no session yet: the open subscribes"
    );
    let ev = rig.lane_events();
    assert_eq!(ev.len(), 3, "CREATED + the two legs' order law");
    assert_eq!(ev[0].channel, ChannelId::InstrumentRoll as u8);
    assert_eq!(
        ev[0].sym,
        pm_rolling_sym(0, pm_pair_of_window(w), PM_LEG_UP)
    );
    assert_eq!(roll_kind(ev[0].venue_seq), ROLL_KIND_CREATED);
    assert_eq!(
        unpack_roll_seq(ev[0].venue_seq),
        (inst, PM_TWAP_S, s, false)
    );
    assert_eq!((ev[0].v0, ev[0].v1), (0, pm_window_end_ns(w) as i64));
    assert_eq!(ev[1].channel, ChannelId::OutcomeMeta as u8);
    assert_eq!(
        (ev[1].v0, ev[1].v1, ev[1].venue_seq),
        (10_000, 5_000_000, u64::from(inst))
    );
    assert_eq!(
        ev[2].sym,
        pm_rolling_sym(0, pm_pair_of_window(w), PM_LEG_DOWN)
    );
    // The session opens: one subscribe naming both tokens.
    wire.open = true;
    assert!(d.on_market_open(&mut wire));
    let want = format!(
        "{{\"assets_ids\":[\"{}\",\"{}\"],\"type\":\"market\",\"custom_feature_enabled\":true}}",
        std::str::from_utf8(UP).unwrap(),
        std::str::from_utf8(DOWN).unwrap()
    );
    assert_eq!(wire.frames, vec![want.into_bytes()]);
    wire.frames.clear();

    let mut checked = 0usize;
    let mut ended_seen = false;
    for (ms, p) in &lines {
        let now = ms * MS;
        let mut out = rig.out();
        d.on_clock(now, now, &mut out, &mut wire);
        d.on_market_text(p, now, now, &mut out);
        d.perform(now, &mut out, &mut wire);
        if d.fams.slot(s).state == PmSlotState::Ended {
            ended_seen = true;
        }
        if memchr::memmem::find(p, b"\"price_change\"").is_some()
            && d.fams.slot(s).state != PmSlotState::Empty
        {
            // After every event, the ladder's touch is the venue's.
            let mut leg = 0u32;
            while leg < 2 {
                let id = if leg == PM_LEG_UP { UP } else { DOWN };
                if let Some((bb, ba)) = final_touch(p, id) {
                    let t = d.ladders[2 * s + leg as usize].touch();
                    assert_eq!(t.bid_px, if bb <= 0 { 0 } else { bb }, "bid at {ms}");
                    assert_eq!(
                        t.ask_px,
                        if ba >= 1_000_000 { 0 } else { ba },
                        "ask at {ms}"
                    );
                    checked += 1;
                }
                leg += 1;
            }
        }
    }
    assert!(checked > 800, "{checked} touches checked");
    assert!(
        ended_seen,
        "the window ended and kept ticking until resolved"
    );
    assert_eq!(rig.rec.rejects, 0);
    assert_eq!(rig.counters.of(Pm15Counter::BookRejects), 0);
    assert_eq!(rig.counters.of(Pm15Counter::RowRejects), 0);
    assert_eq!(rig.counters.of(Pm15Counter::TouchOffGrid), 0);
    assert_eq!(rig.rec.raw, 914);

    // Ticks: emitted only when a touch changed; the sizes are known.
    let n_ticks = rig.rec.ticks.len();
    assert_eq!(
        rig.lane_ticks(),
        n_ticks,
        "every captured tick reached the lane"
    );
    let ticks = &rig.rec.ticks;
    assert!(ticks.len() > 100, "{} ticks", ticks.len());
    let sized = ticks
        .iter()
        .filter(|t| t.ask_px.raw() == 0 || t.ask_qty.raw() > 0)
        .count();
    assert!(
        sized * 100 >= ticks.len() * 99,
        "{sized} of {} asks sized",
        ticks.len()
    );
    let mut k = 1usize;
    while k < ticks.len() {
        let (a, b) = (&ticks[k - 1], &ticks[k]);
        if a.sym == b.sym {
            assert!(
                (a.bid_px, a.bid_qty, a.ask_px, a.ask_qty)
                    != (b.bid_px, b.bid_qty, b.ask_px, b.ask_qty),
                "a repeated touch is not re-emitted"
            );
            assert!(b.venue_seq > a.venue_seq, "the arrival ordinal rises");
        }
        k += 1;
    }

    // The lane: the order law moved to 0.001 on both legs (4 venue frames,
    // 2 repeats), the trade stayed in the capture, the settlement is Up.
    let ev = rig.lane_events();
    let laws: Vec<_> = ev
        .iter()
        .filter(|e| e.channel == ChannelId::OutcomeMeta as u8)
        .collect();
    assert_eq!(laws.len(), 2, "one per leg, repeats dropped");
    assert!(laws.iter().all(|e| e.v0 == 1_000 && e.venue_time_ms > 0));
    assert!(!ev.iter().any(|e| e.channel == ChannelId::Trade as u8));
    assert_eq!(
        rig.rec
            .events
            .iter()
            .filter(|e| e.channel == ChannelId::Trade as u8)
            .count(),
        1
    );
    let settled: Vec<_> = ev
        .iter()
        .filter(|e| e.channel == ChannelId::InstrumentRoll as u8)
        .collect();
    assert_eq!(settled.len(), 1);
    let e = settled[0];
    assert_eq!(roll_kind(e.venue_seq), ROLL_KIND_SETTLED);
    assert_eq!(unpack_roll_seq(e.venue_seq).0, inst);
    assert_eq!((e.v0, e.v1), (PM_SETTLED_UP, pm_window_end_ns(w) as i64));
    assert_eq!(e.venue_time_ms, 1_792_217_816_055);
    assert_eq!(rig.counters.of(Pm15Counter::RollsSettled), 1);
    // …and its tokens were unsubscribed.
    let unsub = format!(
        "{{\"assets_ids\":[\"{}\",\"{}\"],\"operation\":\"unsubscribe\"}}",
        std::str::from_utf8(UP).unwrap(),
        std::str::from_utf8(DOWN).unwrap()
    );
    assert_eq!(wire.frames, vec![unsub.into_bytes()]);
    assert_eq!(d.fams.slot(s).state, PmSlotState::Empty);
    assert_eq!(d.bound(), 0);
}

#[test]
fn chainlink_reports_become_one_mark_per_second_per_family() {
    let mut rig = Rig::new();
    let mut d = Pm15Driver::new(&[b"eth", b"btc"], 5_000);
    let f = |sym: &str, ts: u64, v: &str| {
        format!(
            "{{\"connection_id\":\"x\",\"payload\":{{\"full_accuracy_value\":\"{v}\",\"symbol\":\"{sym}\",\"timestamp\":{ts},\"value\":1.0}},\"timestamp\":{},\"topic\":\"crypto_prices_chainlink\",\"type\":\"update\"}}",
            ts + 1_300
        )
    };
    let now = 1_792_216_681_300 * MS;
    let frames = [
        f("btc/usd", 1_792_216_680_000, "81474326921005000000000"),
        f("btc/usd", 1_792_216_680_000, "81474326921005000000000"),
        f("sol/usd", 1_792_216_680_000, "150000000000000000000"),
        f("eth/usd", 1_792_216_680_000, "2500000000000000000000"),
        f("btc/usd", 1_792_216_681_000, "81475000000000000000000"),
    ];
    for fr in &frames {
        d.on_rtds_text(fr.as_bytes(), now, &mut rig.out());
    }
    d.on_rtds_text(b"PONG", now, &mut rig.out());
    d.on_rtds_text(
        br#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"btc/usd"}}"#,
        now,
        &mut rig.out(),
    );
    let ev = rig.lane_events();
    assert_eq!(
        ev.len(),
        3,
        "btc twice (two seconds), eth once; sol is no family; the repeat dropped"
    );
    assert_eq!(
        (ev[0].sym, ev[0].v0, ev[0].v1),
        (pm_mark_sym(1), 81_474_326_921, 81_474_326_921_005_000)
    );
    assert_eq!(
        (ev[0].venue_seq, ev[0].venue_time_ms),
        (1_792_216_680, 1_792_216_680_000)
    );
    assert_eq!((ev[1].sym, ev[1].v0), (pm_mark_sym(0), 2_500_000_000));
    assert_eq!((ev[2].sym, ev[2].v0), (pm_mark_sym(1), 81_475_000_000));
    assert!(ev
        .iter()
        .all(|e| e.channel == ChannelId::Mark as u8 && e.venue == VenueId::Polymarket as u8));
    assert_eq!(rig.counters.of(Pm15Counter::Marks), 3);
    assert_eq!(rig.counters.of(Pm15Counter::MarksDuplicate), 1);
    assert_eq!(rig.counters.of(Pm15Counter::RtdsRejects), 1);
    assert_eq!(rig.rec.rejects, 1);
}

#[test]
fn a_session_lost_restarts_every_ladder_and_resubscribes_what_is_bound() {
    let spec = btc_spec();
    let mut rig = Rig::new();
    let mut wire = Wire {
        open: true,
        frames: Vec::new(),
    };
    let mut d = Pm15Driver::new(&[b"btc"], 5_000);
    let now = (OPEN + 100) * 1_000_000_000;
    d.offer(&spec, now, now, &mut rig.out(), &mut wire);
    // A dynamic subscribe went out on the open session.
    assert_eq!(wire.frames.len(), 1);
    assert!(
        wire.frames[0].ends_with(b"\"operation\":\"subscribe\",\"custom_feature_enabled\":true}")
    );
    let s = pm_roll_family(0, pm_pair_of_window(spec.window));
    // A snapshot arrives; then the session drops.
    let book = format!(
        "[{{\"market\":\"0x70\",\"asset_id\":\"{}\",\"timestamp\":\"1792216900000\",\"hash\":\"h\",\"bids\":[{{\"price\":\"0.5\",\"size\":\"10\"}}],\"asks\":[{{\"price\":\"0.52\",\"size\":\"7\"}}],\"event_type\":\"book\"}}]",
        std::str::from_utf8(UP).unwrap()
    );
    d.on_market_text(book.as_bytes(), now, now, &mut rig.out());
    assert!(d.ladders[2 * s].is_ready());
    assert_eq!(rig.rec.ticks.len(), 1);
    // PM15-5c: a PONG re-asserts the touch unchanged, on the next ordinal.
    d.on_market_text(b"PONG", now + 5, now + 5, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 2);
    let (a, b) = (rig.rec.ticks[0], rig.rec.ticks[1]);
    assert_eq!(
        (b.bid_px, b.ask_px, b.ask_qty),
        (a.bid_px, a.ask_px, a.ask_qty)
    );
    assert_eq!(
        (b.ts_ns, b.venue_seq, b.is_stale()),
        (now + 5, a.venue_seq + 1, false)
    );
    assert_eq!(rig.counters.of(Pm15Counter::TouchesReasserted), 1);
    // The session drops: the touch is withdrawn — stale, both sides zero.
    d.on_market_down(now + 6, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 3);
    let w = rig.rec.ticks[2];
    assert!(w.is_stale());
    assert_eq!(
        (w.bid_px.raw(), w.ask_px.raw(), w.venue_seq),
        (0, 0, a.venue_seq + 2)
    );
    assert_eq!(rig.counters.of(Pm15Counter::TouchesWithdrawn), 1);
    assert!(!d.ladders[2 * s].is_ready());
    // Down, nothing is re-asserted (no ladder is ready).
    d.on_market_text(b"PONG", now + 7, now + 7, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 3);
    // The next open subscribes the bound tokens again.
    wire.frames.clear();
    assert!(d.on_market_open(&mut wire));
    assert_eq!(wire.frames.len(), 1);
    assert!(memchr::memmem::find(&wire.frames[0], UP).is_some());
    assert!(memchr::memmem::find(&wire.frames[0], DOWN).is_some());
    // The same snapshot again is a touch the lane has not seen since the
    // session restarted: emitted.
    d.on_market_text(book.as_bytes(), now, now, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 4);
}
