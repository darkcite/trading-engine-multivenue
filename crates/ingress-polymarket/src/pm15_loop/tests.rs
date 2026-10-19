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
    pm_instance_id, pm_mark_sym, pm_pair_of_window, pm_roll_family, pm_rolling_sym,
    pm_window_end_ns, pm_window_of_open, PM_LEG_DOWN, PM_SETTLED_UP,
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
    depths: Vec<core_types::DepthTopK>,
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
    fn depth(&mut self, d: &core_types::DepthTopK) {
        self.depths.push(*d);
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
    /// PMJ J7: bit `k` set = shard `k`'s session is down.
    down: u32,
    frames: Vec<Vec<u8>>,
    /// PMJ J7: the shard each frame went out on.
    on: Vec<usize>,
}

impl Pm15Wires for Wire {
    fn is_open(&self, k: usize) -> bool {
        self.open && self.down & (1 << k) == 0
    }
    fn queue(&mut self, k: usize, parts: &[&[u8]]) -> bool {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p);
        }
        self.frames.push(v);
        self.on.push(k);
        true
    }
}

impl Wire {
    /// PMJ J7: every frame sent so far, with its shard; the wire empties.
    fn take(&mut self) -> Vec<(usize, Vec<u8>)> {
        let frames = std::mem::take(&mut self.frames);
        std::mem::take(&mut self.on).into_iter().zip(frames).collect()
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

/// PMJ J7: an ETH family's tokens (the BTC answer reshaped, [`spec_as`]).
const UP_ETH: &[u8] =
    b"11111111111111111111111111111111111111111111111111111111111111111111111111111";
const DOWN_ETH: &[u8] =
    b"22222222222222222222222222222222222222222222222222222222222222222222222222222";

fn utf8(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap()
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(hay, needle).is_some()
}

/// PMJ J7: the live answer reshaped to `asset` (family `f`), the window
/// opening at `open`, on the tokens `up` / `down`.
fn spec_as(f: usize, asset: &str, open: u64, up: &[u8], down: &[u8]) -> PmWindowSpec {
    let w = pm_window_of_open(open).unwrap();
    let body = String::from_utf8(fixture("gamma_btc_1792216800.json"))
        .unwrap()
        .replace("btc-updown-15m-1792216800", &format!("{asset}-updown-15m-{open}"))
        .replace(r#""asset":"btc""#, &format!(r#""asset":"{asset}""#))
        .replace(utf8(UP), utf8(up))
        .replace(utf8(DOWN), utf8(down));
    let mut s = PmWindowSpec::EMPTY;
    parse_window_spec(body.as_bytes(), f, asset.as_bytes(), w, &mut s)
        .expect("the reshaped answer passes the law");
    assert_eq!((s.up.as_bytes(), s.down.as_bytes()), (up, down));
    s
}

/// The fixture's frames: `(arrival ms, frame)`.
fn lines_of(frames: &[u8]) -> Vec<(u64, &[u8])> {
    frames
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            let tab = l.iter().position(|b| *b == b'\t').unwrap();
            (
                std::str::from_utf8(&l[..tab]).unwrap().parse::<u64>().unwrap(),
                &l[tab + 1..],
            )
        })
        .collect()
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
    assert!(d.on_market_open(0, &mut wire));
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
        d.on_market_text(0, p, now, now, &mut out);
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
        ..Wire::default()
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
    d.on_market_text(0, book.as_bytes(), now, now, &mut rig.out());
    assert!(d.ladders[2 * s].is_ready());
    assert_eq!(rig.rec.ticks.len(), 1);
    // PM15-5c: a PONG re-asserts the touch unchanged, on the next ordinal.
    d.on_market_text(0, b"PONG", now + 5, now + 5, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 2);
    let (a, b) = (rig.rec.ticks[0], rig.rec.ticks[1]);
    assert_eq!(
        (b.bid_px, b.ask_px, b.ask_qty),
        (a.bid_px, a.ask_px, a.ask_qty)
    );
    assert_eq!(
        (b.ts_ns, b.venue_seq, b.is_stale(), b.venue_time_ms),
        (now + 5, a.venue_seq + 1, false, 0)
    );
    assert_eq!(rig.counters.of(Pm15Counter::TouchesReasserted), 1);
    // The session drops: the touch is withdrawn — stale, both sides zero.
    d.on_market_down(0, now + 6, &mut rig.out());
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
    d.on_market_text(0, b"PONG", now + 7, now + 7, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 3);
    // The next open subscribes the bound tokens again.
    wire.frames.clear();
    assert!(d.on_market_open(0, &mut wire));
    assert_eq!(wire.frames.len(), 1);
    assert!(memchr::memmem::find(&wire.frames[0], UP).is_some());
    assert!(memchr::memmem::find(&wire.frames[0], DOWN).is_some());
    // The same snapshot again is a touch the lane has not seen since the
    // session restarted: emitted.
    d.on_market_text(0, book.as_bytes(), now, now, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 4);
    // PM15-5d: an hour on, the book still quiet — the re-assertion is not
    // judged by the touch's old stamp.
    d.on_market_text(0, b"PONG", now + 3_600_000_000_000, now, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 5);
    assert!(!rig.rec.ticks[4].is_stale(), "a quiet book is not a stale one");
}

/// PMJ J1b: the 5-minute pipeline's driver. The same answer reshaped to a
/// 5 m window binds on the 5 m pool: its roll names the 5 m leg sym, the
/// family byte `16 + s`, the flagged instance and the 300 s window's end;
/// a Chainlink report becomes a mark on the 5 m family's own mark slot;
/// an unresolved window retires at 540 s.
#[test]
fn the_5m_driver_rolls_marks_and_retires_on_the_5m_pool() {
    use crate::gamma15::parse_window_spec_for;
    use core_types::pm_roll::{
        pm5_instance_id, pm5_mark_sym, pm5_pair_of_window, pm5_roll_family, pm5_rolling_sym,
        pm5_window_end_ns, pm5_window_of_open, PmPeriod, PM5_INSTANCE_FLAG,
    };
    const OPEN5: u64 = 1_792_308_000;
    let body = String::from_utf8(fixture("gamma_btc_1792216800.json"))
        .unwrap()
        .replace("btc-updown-15m-1792216800", "btc-updown-5m-1792308000")
        .replace(r#""duration":"15m""#, r#""duration":"5m""#);
    let w = pm5_window_of_open(OPEN5).unwrap();
    let mut spec = PmWindowSpec::EMPTY;
    parse_window_spec_for(PmPeriod::M5, body.as_bytes(), 0, b"btc", w, &mut spec)
        .expect("the 5 m answer passes the 5 m law");
    let mut rig = Rig::new();
    let mut wire = Wire::default();
    let mut d = Pm15Driver::new_for(PmPeriod::M5, &[b"btc"], 5_000);
    let t0 = OPEN5 * 1_000 * MS + 1;
    d.offer(&spec, t0, t0, &mut rig.out(), &mut wire);
    let ev = rig.lane_events();
    assert_eq!(ev.len(), 3, "CREATED + the two legs' order law");
    let pair = pm5_pair_of_window(w);
    let inst = pm5_instance_id(w, 0).unwrap();
    assert_ne!(inst & PM5_INSTANCE_FLAG, 0);
    assert_eq!(ev[0].sym, pm5_rolling_sym(0, pair, PM_LEG_UP));
    assert_eq!(
        unpack_roll_seq(ev[0].venue_seq),
        (inst, PM_TWAP_S, pm5_roll_family(0, pair), false)
    );
    assert_eq!(ev[0].v1, pm5_window_end_ns(w) as i64);
    assert_eq!(ev[2].sym, pm5_rolling_sym(0, pair, PM_LEG_DOWN));
    // A Chainlink report: the 5 m family's mark slot, not the 15 m one.
    let rep = format!(
        "{{\"connection_id\":\"x\",\"payload\":{{\"full_accuracy_value\":\"81474326921005000000000\",\"symbol\":\"btc/usd\",\"timestamp\":{},\"value\":1.0}},\"timestamp\":{},\"topic\":\"crypto_prices_chainlink\",\"type\":\"update\"}}",
        OPEN5 * 1_000,
        OPEN5 * 1_000 + 1_300
    );
    d.on_rtds_text(rep.as_bytes(), t0, &mut rig.out());
    let ev = rig.lane_events();
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0].sym, ev[0].v0), (pm5_mark_sym(0), 81_474_326_921));
    assert_ne!(ev[0].sym, pm_mark_sym(0));
    // Unresolved: held at 539 s past the end, retired UNKNOWN at 540 s.
    let end = pm5_window_end_ns(w);
    d.on_clock(end + 539_000 * MS, end + 539_000 * MS, &mut rig.out(), &mut wire);
    assert!(rig
        .lane_events()
        .iter()
        .all(|e| roll_kind(e.venue_seq) != ROLL_KIND_SETTLED));
    d.on_clock(end + 540_000 * MS, end + 540_000 * MS, &mut rig.out(), &mut wire);
    let ev = rig.lane_events();
    let settled: Vec<&ChannelEvent> = ev
        .iter()
        .filter(|e| {
            e.channel == ChannelId::InstrumentRoll as u8 && roll_kind(e.venue_seq) == ROLL_KIND_SETTLED
        })
        .collect();
    assert_eq!(settled.len(), 1);
    assert_eq!(
        unpack_roll_seq(settled[0].venue_seq),
        (inst, PM_TWAP_S, pm5_roll_family(0, pair), true)
    );
    assert_eq!(settled[0].v0, 0, "retired unknown");
    assert_eq!(rig.counters.of(Pm15Counter::RollsUnknown), 1);
}

/// PMJ J1b-3: the live window's book, replayed, fills the depth capture —
/// the file that was empty in every run (month results, caveat 3). Every
/// snapshot names a leg of the bound slot, is best-first on both sides,
/// and differs from the leg's previous one (the change gate).
#[test]
fn the_live_window_fills_the_depth_capture_change_gated() {
    let spec = btc_spec();
    let w = spec.window;
    let frames = fixture("market_btc_1792216800.frames");
    let lines: Vec<(u64, &[u8])> = frames
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            let tab = l.iter().position(|b| *b == b'\t').unwrap();
            (
                std::str::from_utf8(&l[..tab]).unwrap().parse::<u64>().unwrap(),
                &l[tab + 1..],
            )
        })
        .collect();
    let mut rig = Rig::new();
    let mut wire = Wire::default();
    let mut d = Pm15Driver::new(&[b"btc"], 5_000);
    let t0 = lines[0].0 * MS;
    d.offer(&spec, t0, t0, &mut rig.out(), &mut wire);
    wire.open = true;
    assert!(d.on_market_open(0, &mut wire));
    let mut events = 0usize;
    for (ms, p) in &lines {
        let now = ms * MS;
        let mut out = rig.out();
        d.on_clock(now, now, &mut out, &mut wire);
        d.on_market_text(0, p, now, now, &mut out);
        d.perform(now, &mut out, &mut wire);
        events += usize::from(memchr::memmem::find(p, b"\"price_change\"").is_some());
    }
    let up = pm_rolling_sym(0, pm_pair_of_window(w), PM_LEG_UP);
    let down = pm_rolling_sym(0, pm_pair_of_window(w), PM_LEG_DOWN);
    let ds = &rig.rec.depths;
    assert!(ds.len() > 100, "{} depth snapshots", ds.len());
    assert!(ds.len() < 2 * events + 4, "change-gated, not one per row");
    let mut last_up: Option<&core_types::DepthTopK> = None;
    let mut last_down: Option<&core_types::DepthTopK> = None;
    for x in ds {
        assert!(x.sym == up || x.sym == down);
        assert_eq!(x.venue, VenueId::Polymarket as u8);
        let mut k = 1usize;
        while k < core_types::DEPTH_K {
            if x.bids[k].qty_1e6 > 0 {
                assert!(x.bids[k].px_1e6 < x.bids[k - 1].px_1e6, "bids best first");
            }
            if x.asks[k].qty_1e6 > 0 {
                assert!(x.asks[k].px_1e6 > x.asks[k - 1].px_1e6, "asks best first");
            }
            k += 1;
        }
        if x.bids[0].qty_1e6 > 0 && x.asks[0].qty_1e6 > 0 {
            assert!(x.bids[0].px_1e6 < x.asks[0].px_1e6, "uncrossed");
        }
        let prev = if x.sym == up { &mut last_up } else { &mut last_down };
        if let Some(p) = prev {
            assert!(p.bids != x.bids || p.asks != x.asks, "a repeat passed the gate");
        }
        *prev = Some(x);
    }
}

/// PMJ (R-PMJ-S15): the same replay under the deployed policy — at most one
/// record per leg per second (a window's first book not held by the spacing), far fewer than
/// every change; a change the spacing held is written by the first `PONG`
/// sweep at least the spacing after the leg's last record; `rolling_depth =
/// false` records none; and the policy is capture-only — the ticks and
/// events are the same under every policy.
#[test]
fn the_depth_capture_keeps_one_record_per_leg_per_second_and_the_sweep_catches_up() {
    let spec = btc_spec();
    let w = spec.window;
    let frames = fixture("market_btc_1792216800.frames");
    let lines: Vec<(u64, &[u8])> = frames
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            let tab = l.iter().position(|b| *b == b'\t').unwrap();
            (
                std::str::from_utf8(&l[..tab]).unwrap().parse::<u64>().unwrap(),
                &l[tab + 1..],
            )
        })
        .collect();
    // Replay `lines[..cut]`, then (`sweep_after` ns later, if any) a PONG.
    let replay = |on: bool, min_ms: u32, cut: usize, sweep_after: Option<u64>| {
        let mut rig = Rig::new();
        let mut wire = Wire::default();
        let mut d = Pm15Driver::new(&[b"btc"], 5_000).with_depth_policy(on, min_ms);
        let t0 = lines[0].0 * MS;
        d.offer(&spec, t0, t0, &mut rig.out(), &mut wire);
        wire.open = true;
        assert!(d.on_market_open(0, &mut wire));
        let mut last = t0;
        for (ms, p) in &lines[..cut] {
            let now = ms * MS;
            let mut out = rig.out();
            d.on_clock(now, now, &mut out, &mut wire);
            d.on_market_text(0, p, now, now, &mut out);
            d.perform(now, &mut out, &mut wire);
            last = now;
        }
        let before = rig.rec.depths.len();
        let mut sweep_ts = 0u64;
        if let Some(after) = sweep_after {
            sweep_ts = last + after;
            d.on_market_text(0, b"PONG", sweep_ts, sweep_ts, &mut rig.out());
        }
        (rig.rec, before, sweep_ts)
    };
    let n = lines.len();
    let (every, _, _) = replay(true, 0, n, None);
    let (capped, _, _) = replay(true, 1_000, n, None);
    let (none, _, _) = replay(false, 0, n, None);
    assert!(none.depths.is_empty(), "rolling_depth = false records nothing");
    assert!(every.depths.len() > 100);
    assert!(capped.depths.len() * 3 < every.depths.len(), "{} capped vs {} every change", capped.depths.len(), every.depths.len());
    // Capture-only: the policy never touches a tick or an event
    // (`ChannelEvent` carries padding and no `PartialEq`: its fields).
    let key = |e: &ChannelEvent| (e.ts_ns, e.sym, e.venue, e.channel, e.flags, e.venue_seq, e.venue_time_ms, e.v0, e.v1);
    let same = |a: &Rec, b: &Rec| {
        a.ticks.len() == b.ticks.len()
            && a.ticks.iter().zip(b.ticks.iter()).all(|(x, y)| x == y)
            && a.events.iter().map(key).eq(b.events.iter().map(key))
    };
    assert!(same(&capped, &every) && same(&none, &every), "ticks and events identical under every policy");
    let up = pm_rolling_sym(0, pm_pair_of_window(w), PM_LEG_UP);
    let down = pm_rolling_sym(0, pm_pair_of_window(w), PM_LEG_DOWN);
    for leg in [up, down] {
        let ts: Vec<u64> = capped.depths.iter().filter(|x| x.sym == leg).map(|x| x.ts_ns).collect();
        assert!(!ts.is_empty(), "the leg's first book is captured at once");
        let mut k = 1usize;
        while k < ts.len() {
            assert!(ts[k] - ts[k - 1] >= 1_000 * MS, "records {} ns apart", ts[k] - ts[k - 1]);
            k += 1;
        }
    }
    // The sweep: the latest cut (before the window resolves and its slot stops
    // ticking) where the spacing holds a leg's newest book back and that
    // leg's last record is under the spacing at the cut's last frame. A PONG
    // under the spacing writes nothing for it; one past it writes the held
    // book.
    let last_of = |ds: &[core_types::DepthTopK], leg| ds.iter().rev().find(|x| x.sym == leg).copied();
    let held_at = |cut: usize| -> Vec<SymbolId> {
        let (ev, _, _) = replay(true, 0, cut, None);
        let (cp, _, _) = replay(true, 1_000, cut, None);
        let last_frame = lines[cut - 1].0 * MS;
        [up, down]
            .into_iter()
            .filter(|&leg| match (last_of(&ev.depths, leg), last_of(&cp.depths, leg)) {
                (Some(a), Some(b)) => (a.bids != b.bids || a.asks != b.asks) && last_frame + MS - b.ts_ns < 1_000 * MS,
                _ => false,
            })
            .collect()
    };
    let mut cut = n - 1;
    let mut held: Vec<SymbolId> = Vec::new();
    while cut > n / 2 {
        held = held_at(cut);
        if !held.is_empty() {
            break;
        }
        cut -= 1;
    }
    assert!(!held.is_empty(), "the fixture holds a change back somewhere");
    let (ev, _, _) = replay(true, 0, cut, None);
    let (early, b0, _) = replay(true, 1_000, cut, Some(MS));
    assert!(
        early.depths[b0..].iter().all(|x| !held.contains(&x.sym)),
        "a PONG under the spacing writes nothing for the held leg"
    );
    let (late, b1, sweep_ts) = replay(true, 1_000, cut, Some(1_100 * MS));
    assert!(
        late.depths[b1..].iter().any(|x| held.contains(&x.sym)),
        "the sweep past the spacing wrote the held book"
    );
    // Capture-only on the PONG path too: the sweep's re-asserted ticks are
    // the same with and without the spacing.
    let (late_every, _, _) = replay(true, 0, cut, Some(1_100 * MS));
    assert!(same(&late, &late_every), "the sweep changes no tick or event");
    for x in &late.depths[b1..] {
        assert_eq!(x.ts_ns, sweep_ts, "stamped at the sweep");
    }
    for leg in [up, down] {
        let (a, b) = (last_of(&ev.depths, leg).unwrap(), last_of(&late.depths, leg).unwrap());
        assert!(a.bids == b.bids && a.asks == b.asks, "the sweep caught the leg up");
    }
}

/// PMJ J7 (R-PMJ-S17..S19): two families on two market sockets. Each
/// shard's open subscribes only its family's tokens; a `PONG` on one shard
/// re-asserts only its legs; one shard's loss withdraws only its legs and
/// starts only its ladders over — the other family keeps ticking; a
/// window bound while its shard is down rides that shard's next open; a
/// settlement's unsubscribe goes out on the family's own shard.
#[test]
fn each_family_rides_its_own_market_socket_and_a_loss_withdraws_only_its_legs() {
    let (btc, eth) = (btc_spec(), spec_as(1, "eth", OPEN, UP_ETH, DOWN_ETH));
    let mut rig = Rig::new();
    let mut wire = Wire::default();
    let mut d = Pm15Driver::new(&[b"btc", b"eth"], 5_000).with_market_sockets(2);
    assert_eq!(d.market_shards(), 2);
    let now = (OPEN + 100) * 1_000_000_000;
    assert_eq!(d.bound_shards(), 0, "nothing bound: no shard dials");
    d.offer(&btc, now, now, &mut rig.out(), &mut wire);
    d.offer(&eth, now, now, &mut rig.out(), &mut wire);
    assert!(wire.frames.is_empty(), "no session yet: the opens subscribe");
    let (sb, se) = (
        pm_roll_family(0, pm_pair_of_window(btc.window)),
        pm_roll_family(1, pm_pair_of_window(eth.window)),
    );
    assert_eq!((d.shard_of(sb), d.shard_of(se)), (0, 1));
    assert_eq!(d.bound_shards(), 0b11);
    // Each shard's open names its own family's tokens, and only those.
    wire.open = true;
    assert!(d.on_market_open(0, &mut wire));
    assert!(d.on_market_open(1, &mut wire));
    let got = wire.take();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, 0);
    assert!(has(&got[0].1, UP) && has(&got[0].1, DOWN));
    assert!(!has(&got[0].1, UP_ETH) && !has(&got[0].1, DOWN_ETH));
    assert_eq!(got[1].0, 1);
    assert!(has(&got[1].1, UP_ETH) && has(&got[1].1, DOWN_ETH));
    assert!(!has(&got[1].1, UP) && !has(&got[1].1, DOWN));
    // A snapshot on every leg, each on its family's socket.
    let book = |id: &[u8]| {
        format!(
            "[{{\"market\":\"0x70\",\"asset_id\":\"{}\",\"timestamp\":\"1792216900000\",\"hash\":\"h\",\"bids\":[{{\"price\":\"0.5\",\"size\":\"10\"}}],\"asks\":[{{\"price\":\"0.52\",\"size\":\"7\"}}],\"event_type\":\"book\"}}]",
            utf8(id)
        )
    };
    for id in [UP, DOWN] {
        d.on_market_text(0, book(id).as_bytes(), now, now, &mut rig.out());
    }
    for id in [UP_ETH, DOWN_ETH] {
        d.on_market_text(1, book(id).as_bytes(), now, now, &mut rig.out());
    }
    assert_eq!(rig.rec.ticks.len(), 4);
    let pb = pm_pair_of_window(btc.window);
    let btc_syms = [pm_rolling_sym(0, pb, PM_LEG_UP), pm_rolling_sym(0, pb, PM_LEG_DOWN)];
    let pe = pm_pair_of_window(eth.window);
    let eth_syms = [pm_rolling_sym(1, pe, PM_LEG_UP), pm_rolling_sym(1, pe, PM_LEG_DOWN)];
    // A PONG on shard 1 speaks for ETH's legs only.
    d.on_market_text(1, b"PONG", now + 5, now + 5, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 6);
    assert!(rig.rec.ticks[4..].iter().all(|t| eth_syms.contains(&t.sym) && !t.is_stale()));
    assert_eq!(rig.counters.of(Pm15Counter::TouchesReasserted), 2);
    // Shard 0 drops: BTC's two touches are withdrawn, ETH's stand — and
    // ETH's sequencer and feed clocks are not reset with BTC's.
    let eth_clock = format!("{:?}", d.market_clock[1]);
    d.on_market_down(0, now + 6, &mut rig.out());
    assert_eq!(format!("{:?}", d.market_clock[1]), eth_clock, "shard 1's feed clock untouched");
    assert_eq!(rig.rec.ticks.len(), 8);
    assert!(rig.rec.ticks[6..]
        .iter()
        .all(|t| btc_syms.contains(&t.sym) && t.is_stale() && t.bid_px.raw() == 0 && t.ask_px.raw() == 0));
    assert_eq!(rig.counters.of(Pm15Counter::TouchesWithdrawn), 2);
    assert!(!d.ladders[2 * sb].is_ready() && !d.ladders[2 * sb + 1].is_ready());
    assert!(d.ladders[2 * se].is_ready() && d.ladders[2 * se + 1].is_ready());
    // Shard 0 has nothing to re-assert; shard 1 still does.
    d.on_market_text(0, b"PONG", now + 7, now + 7, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 8);
    d.on_market_text(1, b"PONG", now + 8, now + 8, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 10);
    assert!(rig.rec.ticks[8..].iter().all(|t| eth_syms.contains(&t.sym) && !t.is_stale()));
    // An ETH book older than the last one ETH ticked: its sequencer clock
    // still holds the newer key (not reset by shard 0's loss), so the tick
    // takes ordinal 0.
    let old = format!(
        "[{{\"market\":\"0x70\",\"asset_id\":\"{}\",\"timestamp\":\"1792216800000\",\"hash\":\"h\",\"bids\":[{{\"price\":\"0.49\",\"size\":\"10\"}}],\"asks\":[{{\"price\":\"0.52\",\"size\":\"7\"}}],\"event_type\":\"book\"}}]",
        utf8(UP_ETH)
    );
    d.on_market_text(1, old.as_bytes(), now + 8, now + 8, &mut rig.out());
    assert_eq!(rig.rec.ticks.len(), 11);
    assert_eq!(rig.rec.ticks[10].sym, eth_syms[0]);
    assert_eq!(rig.rec.ticks[10].venue_seq, 0, "shard 1's sequencer clock untouched");
    // BTC's next window binds while shard 0 is down: nothing goes out (and
    // nothing on ETH's socket); shard 0's next open names both BTC windows.
    wire.down = 0b01;
    let open2 = OPEN + 900;
    let up2: &[u8] = b"33333333333333333333333333333333333333333333333333333333333333333333333333333";
    let down2: &[u8] = b"44444444444444444444444444444444444444444444444444444444444444444444444444444";
    let btc2 = spec_as(0, "btc", open2, up2, down2);
    d.offer(&btc2, now + 9, now + 9, &mut rig.out(), &mut wire);
    assert!(wire.take().is_empty());
    let sb2 = pm_roll_family(0, pm_pair_of_window(btc2.window));
    assert_ne!(sb2, sb);
    assert_eq!(d.shard_of(sb2), 0);
    wire.down = 0;
    assert!(d.on_market_open(0, &mut wire));
    let got = wire.take();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 0);
    assert!(has(&got[0].1, UP) && has(&got[0].1, DOWN) && has(&got[0].1, up2) && has(&got[0].1, down2));
    assert!(!has(&got[0].1, UP_ETH));
    // ETH resolves: its unsubscribe goes out on shard 1.
    let resolved = format!(
        "{{\"market\":\"0x70\",\"winning_asset_id\":\"{}\",\"winning_outcome\":\"Up\",\"timestamp\":\"1792217815901\",\"event_type\":\"market_resolved\"}}",
        utf8(UP_ETH)
    );
    d.on_market_text(1, resolved.as_bytes(), now + 10, now + 10, &mut rig.out());
    d.perform(now + 10, &mut rig.out(), &mut wire);
    let got = wire.take();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 1);
    let unsub = format!(
        "{{\"assets_ids\":[\"{}\",\"{}\"],\"operation\":\"unsubscribe\"}}",
        utf8(UP_ETH),
        utf8(DOWN_ETH)
    );
    assert_eq!(got[0].1, unsub.into_bytes());
    assert_eq!(d.bound_shards(), 0b01, "ETH's shard carries no window now");
    // ETH's next window binds while both sockets are open: its subscribe
    // goes out on shard 1, and only there.
    let up3: &[u8] = b"55555555555555555555555555555555555555555555555555555555555555555555555555555";
    let down3: &[u8] = b"66666666666666666666666666666666666666666666666666666666666666666666666666666";
    let eth2 = spec_as(1, "eth", open2, up3, down3);
    d.offer(&eth2, now + 11, now + 11, &mut rig.out(), &mut wire);
    let got = wire.take();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 1, "a dynamic subscribe rides its family's socket");
    assert!(has(&got[0].1, up3) && has(&got[0].1, down3) && !has(&got[0].1, UP));
    assert!(got[0].1.ends_with(b"\"operation\":\"subscribe\",\"custom_feature_enabled\":true}"));
    assert_eq!(d.bound_shards(), 0b11);
}

/// PMJ J7: the market side's state over its shards.
#[test]
fn the_market_state_reads_backoff_while_a_bound_shard_waits_and_up_once_all_are_open() {
    assert_eq!(market_state(0, false, false), IngressState::Connecting, "before the first socket");
    assert_eq!(market_state(0, false, true), IngressState::Connecting);
    assert_eq!(market_state(3, false, true), IngressState::Connecting, "a shard still dials");
    assert_eq!(market_state(3, true, true), IngressState::Backoff, "a bound shard waits to redial");
    assert_eq!(market_state(0, true, false), IngressState::Backoff);
    assert_eq!(market_state(7, false, false), IngressState::Up);
}

/// PMJ J7: sharding moves only the socket a frame rides. The live window
/// replayed as two families (BTC, and the same frames on ETH's tokens),
/// with a `PONG` on every socket every 50 frames: one socket and two give
/// the same ticks, events and depth records, and the same dynamic frames —
/// each on its family's socket.
#[test]
fn sharding_changes_only_the_socket_a_frame_rides() {
    let (btc, eth) = (btc_spec(), spec_as(1, "eth", OPEN, UP_ETH, DOWN_ETH));
    let frames = fixture("market_btc_1792216800.frames");
    let eth_frames = utf8(&frames)
        .replace(utf8(UP), utf8(UP_ETH))
        .replace(utf8(DOWN), utf8(DOWN_ETH))
        .into_bytes();
    let (lb, le) = (lines_of(&frames), lines_of(&eth_frames));
    assert_eq!(lb.len(), le.len());
    let (sb, se) = (
        pm_roll_family(0, pm_pair_of_window(btc.window)),
        pm_roll_family(1, pm_pair_of_window(eth.window)),
    );
    let replay = |n: usize| {
        let mut rig = Rig::new();
        let mut wire = Wire::default();
        let mut d = Pm15Driver::new(&[b"btc", b"eth"], 5_000).with_market_sockets(n);
        let (kb, ke) = (d.shard_of(sb), d.shard_of(se));
        let t0 = lb[0].0 * MS;
        d.offer(&btc, t0, t0, &mut rig.out(), &mut wire);
        d.offer(&eth, t0, t0, &mut rig.out(), &mut wire);
        wire.open = true;
        let mut k = 0usize;
        while k < n {
            assert!(d.on_market_open(k, &mut wire));
            k += 1;
        }
        let opens = wire.take();
        for (i, ((ms, pb), (_, pe))) in lb.iter().zip(le.iter()).enumerate() {
            let now = ms * MS;
            let mut out = rig.out();
            d.on_clock(now, now, &mut out, &mut wire);
            d.on_market_text(kb, pb, now, now, &mut out);
            d.perform(now, &mut out, &mut wire);
            d.on_market_text(ke, pe, now, now, &mut out);
            d.perform(now, &mut out, &mut wire);
            if i % 50 == 49 {
                let mut k = 0usize;
                while k < n {
                    d.on_market_text(k, b"PONG", now, now, &mut out);
                    k += 1;
                }
            }
        }
        (rig.rec, opens, wire.take())
    };
    let (one, opens1, sent1) = replay(1);
    let (two, opens2, sent2) = replay(2);
    assert!(one.ticks.len() > 100, "{} ticks", one.ticks.len());
    assert!(one.ticks == two.ticks, "the same ticks");
    let key = |e: &ChannelEvent| (e.ts_ns, e.sym, e.venue, e.channel, e.flags, e.venue_seq, e.venue_time_ms, e.v0, e.v1);
    assert!(one.events.iter().map(key).eq(two.events.iter().map(key)), "the same events");
    let dk = |x: &core_types::DepthTopK| (x.ts_ns, x.sym, x.bids, x.asks);
    assert!(one.depths.iter().map(dk).eq(two.depths.iter().map(dk)), "the same depth records");
    // The opens: one frame naming all four tokens, or one per family.
    assert_eq!(opens1.len(), 1);
    assert!([UP, DOWN, UP_ETH, DOWN_ETH].iter().all(|id| has(&opens1[0].1, id)));
    assert_eq!(opens2.iter().map(|x| x.0).collect::<Vec<_>>(), vec![0, 1]);
    // The dynamic frames (the settlements' unsubscribes): the same bytes in
    // the same order, each on its family's socket.
    assert!(!sent1.is_empty());
    assert_eq!(
        sent1.iter().map(|x| &x.1).collect::<Vec<_>>(),
        sent2.iter().map(|x| &x.1).collect::<Vec<_>>()
    );
    assert!(sent1.iter().all(|x| x.0 == 0));
    for (k, f) in &sent2 {
        let eth_frame = has(f, UP_ETH);
        assert_eq!(*k, usize::from(eth_frame), "an unsubscribe rides its family's socket");
    }
}

/// PMJ J7: the shard table — family `f` on shard `f % n` for every period
/// and every admitted `n`; the default is one socket; the tokens of the
/// market shards and of RTDS never collide.
#[test]
fn the_shard_table_spreads_the_families_modulo_the_socket_count() {
    let assets: [&[u8]; 7] = [b"btc", b"eth", b"sol", b"xrp", b"hype", b"doge", b"bnb"];
    for period in [PmPeriod::M15, PmPeriod::M5] {
        let pairs = period.pairs();
        let mut n = 1usize;
        while n <= PM15_MARKET_SOCKETS_MAX {
            let d = Pm15Driver::new_for(period, &assets, 5_000).with_market_sockets(n);
            assert_eq!(d.market_shards(), n);
            for f in 0..assets.len() {
                for pair in 0..pairs {
                    assert_eq!(d.shard_of(f * pairs + pair), f % n, "{period:?} n={n} f={f}");
                }
            }
            n += 1;
        }
        let d = Pm15Driver::new_for(period, &assets, 5_000);
        assert_eq!(d.market_shards(), 1, "the default is one socket");
        assert!((0..PM_SLOTS).all(|s| d.shard_of(s) == 0));
    }
    assert_eq!(PM15_RTDS_TOKEN, mio::Token(PM15_MARKET_SOCKETS_MAX));
    assert!((0..PM15_MARKET_SOCKETS_MAX).all(|k| pm15_market_token(k) != PM15_RTDS_TOKEN));
}

#[test]
#[should_panic(expected = "rolling market sockets 0 out of 1..=")]
fn no_market_socket_at_all_is_refused() {
    let _ = Pm15Driver::new(&[b"btc"], 5_000).with_market_sockets(0);
}

#[test]
#[should_panic(expected = "out of 1..=")]
fn more_market_sockets_than_the_family_cap_is_refused() {
    let _ = Pm15Driver::new(&[b"btc"], 5_000).with_market_sockets(PM15_MARKET_SOCKETS_MAX + 1);
}
