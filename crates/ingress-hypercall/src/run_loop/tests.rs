// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Run-loop tests, `TestTransport`-driven, over the golden frames.
//!
//! COPY-DOCTRINE: test-only module (declared under `#[cfg(test)]` in
//! run_loop.rs); every copy here assembles a fixture frame.

use super::*;
use core_net::TestTransport;
use core_ring::Ring;
use core_types::{NullCapture, EVENT_LANE_FUNDING};

const Q1: &[u8] = include_bytes!("../../tests/fixtures/quote_one_provider.json");
const Q2X: &[u8] = include_bytes!("../../tests/fixtures/quote_two_providers_crossed.json");
const Q0: &[u8] = include_bytes!("../../tests/fixtures/quote_empty.json");
const IDX: &[u8] = include_bytes!("../../tests/fixtures/index_update.json");
const TRADE: &[u8] = include_bytes!("../../tests/fixtures/trade_docs_example.json");
const MU_EXPIRED: &[u8] = include_bytes!("../../tests/fixtures/market_update_expired_schema.json");
const CLOSE_ML: &[u8] = include_bytes!("../../tests/fixtures/close_reason_message_limit.json");

const ETH_P: u32 = (9 << 24) | 512;
const MU_C: u32 = (9 << 24) | 513;
const MSFT_P: u32 = (9 << 24) | 514;
const BTC_C: u32 = (9 << 24) | 515;
const IDX_AAPL: u32 = (9 << 24) | 1;
const IDX_BTC: u32 = (9 << 24) | 2;

fn tables() -> (HcSymbolTable, HcUnderlyings) {
    let mut t = HcSymbolTable::new();
    t.insert(b"ETH-20260927-2675-P", ETH_P).unwrap();
    t.insert(b"MU-20260928-1090-C", MU_C).unwrap();
    t.insert(b"MSFT-20260927-500-P", MSFT_P).unwrap();
    t.insert(b"BTC-20261002-100000-C", BTC_C).unwrap();
    let mut u = HcUnderlyings::new();
    u.insert(b"AAPL", IDX_AAPL).unwrap();
    u.insert(b"BTC", IDX_BTC).unwrap();
    (t, u)
}

fn steady() -> Driver {
    let (t, u) = tables();
    let mut d = Driver::new(9, t, u);
    d.state = State::Steady;
    d
}

/// Every lane, owned, with its consumers for inspection.
struct Rig {
    ticks: (Producer<Tick, TICK_RING_CAP>, core_ring::Consumer<Tick, TICK_RING_CAP>),
    events: (
        Producer<ChannelEvent, EVENT_RING_SIZE>,
        core_ring::Consumer<ChannelEvent, EVENT_RING_SIZE>,
    ),
    opts: (
        Producer<OptSummary, OPT_RING_SIZE>,
        core_ring::Consumer<OptSummary, OPT_RING_SIZE>,
    ),
    mask: u16,
    status: IngressStatus,
    counters: HcCounters,
}

impl Rig {
    fn new(mask: u16) -> Self {
        Self {
            ticks: Ring::<Tick, TICK_RING_CAP>::new().split(),
            events: Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split(),
            opts: Ring::<OptSummary, OPT_RING_SIZE>::new().split(),
            mask,
            status: IngressStatus::new(),
            counters: HcCounters::new(),
        }
    }

    fn drive<C: Capture>(&mut self, t: &mut TestTransport, d: &mut Driver, cap: &mut C) -> io::Result<()> {
        let mut lanes = Lanes {
            ticks: &mut self.ticks.0,
            events: &mut self.events.0,
            event_mask: self.mask,
            opts: &mut self.opts.0,
        };
        drive_one(t, d, b"api.hypercall.xyz", &mut lanes, &self.status, &self.counters, cap)
    }

    fn pop_ticks(&mut self) -> Vec<Tick> {
        let mut v = Vec::new();
        while let Some(t) = self.ticks.1.try_pop_ref() {
            v.push(*t);
        }
        v
    }
}

/// Unmasked server→client frame.
fn frame(first: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(10 + payload.len());
    f.push(first);
    if payload.len() <= 125 {
        f.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        f.push(126);
        f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        f.push(127);
        f.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    f.extend_from_slice(payload);
    f
}

fn text(payload: &[u8]) -> Vec<u8> {
    frame(0x81, payload)
}

/// Decode every masked client frame → (opcode, payload).
fn client_frames(mut buf: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while buf.len() >= 2 {
        let op = buf[0] & 0x0F;
        assert!(buf[1] & 0x80 != 0, "client frames are masked");
        let mut len = (buf[1] & 0x7F) as usize;
        let mut at = 2;
        if len == 126 {
            len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            at = 4;
        } else if len == 127 {
            len = u64::from_be_bytes(buf[2..10].try_into().unwrap()) as usize;
            at = 10;
        }
        let mask = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        at += 4;
        let body: Vec<u8> = (0..len).map(|i| buf[at + i] ^ mask[i & 3]).collect();
        out.push((op, body));
        buf = &buf[at + len..];
    }
    out
}

/// Records every emission.
#[derive(Default)]
struct Rec {
    events: Vec<ChannelEvent>,
    ticks: Vec<Tick>,
    opts: Vec<OptSummary>,
    rejects: u32,
}
impl Capture for Rec {
    fn tick(&mut self, t: &Tick) {
        self.ticks.push(*t);
    }
    fn event(&mut self, e: &ChannelEvent) {
        self.events.push(*e);
    }
    fn opt_summary(&mut self, o: &OptSummary) {
        self.opts.push(*o);
    }
    fn parse_reject(&mut self, _ts: u64, _p: &[u8]) {
        self.rejects += 1;
    }
}
impl Rec {
    fn of(&self, ch: ChannelId) -> Vec<ChannelEvent> {
        self.events.iter().filter(|e| e.channel == ch as u8).copied().collect()
    }
}

#[test]
fn the_handshake_queues_a_clock_sync_then_one_subscribe_per_channel() {
    let (t, u) = tables();
    let mut d = Driver::new(42, t, u);
    let mut rig = Rig::new(0);
    let mut tr = TestTransport::with_capacity(1 << 20);
    note_transport_ready(&mut d, Status::Ready);
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    let mut out = vec![0u8; 1 << 20];
    let n = tr.drain_outgoing(&mut out);
    assert!(memchr::memmem::find(&out[..n], b"GET /ws HTTP/1.1").is_some());
    assert!(memchr::memmem::find(&out[..n], b"api.hypercall.xyz").is_some());

    let key = core_net::sec_websocket_key_from_seed(42);
    let accept = core_net::expected_accept(&key);
    let mut resp = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ".to_vec();
    resp.extend_from_slice(&accept);
    resp.extend_from_slice(b"\r\n\r\n");
    tr.inject_incoming(&resp);
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert_eq!(d.state(), State::Steady);
    assert_eq!(rig.status.state(), IngressState::Up);
    let n = tr.drain_outgoing(&mut out);
    let frames = client_frames(&out[..n]);
    assert_eq!(frames.len(), 5, "ClockSync + one Subscribe per channel");
    assert!(frames.iter().all(|f| f.0 == 0x1), "text frames");
    assert!(frames[0].1.starts_with(br#"{"type":"ClockSync","nonce":""#));
    assert_eq!(frames[1].1, br#"{"type":"Subscribe","channel":"index_prices"}"#);
    assert_eq!(frames[2].1, br#"{"type":"Subscribe","channel":"trades"}"#);
    assert_eq!(frames[3].1, br#"{"type":"Subscribe","channel":"market_updates"}"#);
    assert_eq!(
        frames[4].1,
        br#"{"type":"Subscribe","channel":"indicative_market_data","symbols":["ETH-20260927-2675-P","MU-20260928-1090-C","MSFT-20260927-500-P","BTC-20261002-100000-C"]}"#,
        "the WHOLE universe in ONE frame (the D3 law)"
    );
    assert_eq!(crate::counters::get(&rig.counters.ws.subscribes), 1);
    // Nothing confirmed until an ack or a quote.
    assert_eq!(d.sub_count(), 0);
    tr.inject_incoming(&text(br#"{"type":"Subscribed","channel":"indicative_market_data"}"#));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert_eq!(d.sub_count(), 1);
}

#[test]
fn a_quote_is_a_tick_once_per_bbo_change() {
    let mut d = steady();
    let mut rig = Rig::new(0);
    let mut tr = TestTransport::with_capacity(1 << 16);
    let mut rec = Rec::default();
    tr.inject_incoming(&text(Q1));
    tr.inject_incoming(&text(Q1));
    rig.drive(&mut tr, &mut d, &mut rec).unwrap();
    let ticks = rig.pop_ticks();
    assert_eq!(ticks.len(), 1, "a republished identical quote is not a tick");
    let t = ticks[0];
    assert_eq!(t.sym, ETH_P);
    assert_eq!(t.venue, VenueId::Hypercall as u8);
    assert_eq!((t.bid_px.raw(), t.bid_qty.raw()), (7_679_000, 63_816_073));
    assert_eq!((t.ask_px.raw(), t.ask_qty.raw()), (15_240_500, 63_816_073));
    assert_eq!(t.venue_time_ms, 1_790_373_476_585, "the quote's own timestamp");
    assert_eq!(rec.ticks.len(), 1, "captured once too");
    assert_eq!(rig.status.msgs_total(), 2);
    assert_eq!(d.sub_count(), 1, "a quote confirms the session");
    assert_eq!(crate::counters::get(&rig.counters.ws.quoted_instruments), 1);
    assert_eq!(crate::counters::get(&rig.counters.ws.quote_publish_lag_ms), 1_649);
}

#[test]
fn a_crossed_quote_is_emitted_counted_and_its_providers_captured() {
    let mut d = steady();
    let mut rig = Rig::new(event_lane_bit(ChannelId::ProviderQuote));
    let mut tr = TestTransport::with_capacity(1 << 16);
    let mut rec = Rec::default();
    tr.inject_incoming(&text(Q2X));
    rig.drive(&mut tr, &mut d, &mut rec).unwrap();
    let ticks = rig.pop_ticks();
    assert_eq!(ticks.len(), 1);
    assert_eq!((ticks[0].bid_px.raw(), ticks[0].ask_px.raw()), (18_644_500, 12_067_400));
    assert_eq!(crate::counters::get(&rig.counters.ws.crossed_quotes), 1);
    let pq = rec.of(ChannelId::ProviderQuote);
    assert_eq!(pq.len(), 4, "two providers x (bid, ask)");
    assert!(pq.iter().all(|e| e.sym == MU_C));
    // Provider B's ask: the side that crossed the book.
    let b_ask = pq[3];
    assert_eq!(b_ask.venue_seq, crate::provider_quote_seq(1, true, 2, 0x6cb3_94cd));
    assert_eq!((b_ask.v0, b_ask.v1), (12_067_400, 100_000_000));
    assert_eq!(b_ask.venue_time_ms, 1_790_373_486_436);
    // The ProviderQuote bit is in the mask: the lane saw them too.
    let mut n = 0;
    while rig.events.1.try_pop_ref().is_some() {
        n += 1;
    }
    assert_eq!(n, 4);
    assert_eq!(crate::counters::get(&rig.counters.ws.provider_quotes), 4);
    assert_eq!(crate::counters::get(&rig.counters.ws.providers_max), 2);
}

#[test]
fn one_sided_and_empty_quotes() {
    let mut d = steady();
    let mut rig = Rig::new(0);
    let mut tr = TestTransport::with_capacity(1 << 16);
    tr.inject_incoming(&text(Q0));
    let one = br#"{"type":"IndicativeMarketData","instrument":"BTC-20261002-100000-C","best_bid":null,"best_ask":"1.5","indicative_ask_size":"2","num_providers":1,"rfq_provider_quotes":[],"published_at":20,"timestamp":10}"#;
    tr.inject_incoming(&text(one));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    let ticks = rig.pop_ticks();
    assert_eq!(ticks.len(), 1, "the empty quote emits nothing, the one-sided one a tick");
    assert_eq!(ticks[0].sym, BTC_C);
    assert_eq!((ticks[0].bid_px.raw(), ticks[0].bid_qty.raw()), (0, 0));
    assert_eq!((ticks[0].ask_px.raw(), ticks[0].ask_qty.raw()), (1_500_000, 2_000_000));
    assert_eq!(crate::counters::get(&rig.counters.ws.empty_quotes), 1);
    assert_eq!(crate::counters::get(&rig.counters.ws.one_sided_quotes), 1);
    assert_eq!(crate::counters::get(&rig.counters.ws.quoted_instruments), 0, "never two-sided");
}

#[test]
fn the_index_frame_marks_every_configured_underlying() {
    let mut d = steady();
    let mut rig = Rig::new(EVENT_LANE_FUNDING);
    let mut tr = TestTransport::with_capacity(1 << 16);
    let mut rec = Rec::default();
    // A quote first: the index-age gauge's venue-clock reference.
    tr.inject_incoming(&text(Q1));
    tr.inject_incoming(&text(IDX));
    rig.drive(&mut tr, &mut d, &mut rec).unwrap();
    let marks = rec.of(ChannelId::Mark);
    assert_eq!(marks.len(), 2, "AAPL and BTC configured; the ten others are not");
    assert_eq!((marks[0].sym, marks[0].v0), (IDX_AAPL, 341_150_000));
    assert_eq!((marks[1].sym, marks[1].v0), (IDX_BTC, 83_839_000_000));
    assert_eq!(marks[0].venue_time_ms, 1_790_373_479_066);
    assert!(rig.events.1.try_pop_ref().is_none(), "Mark is not in the lane mask");
    // published 1_790_373_478_234, index source 1_790_373_479_066: the
    // index is NEWER than the quote here — the gauge saturates at 0.
    assert_eq!(crate::counters::get(&rig.counters.ws.index_age_ms), 0);
}

#[test]
fn trades_on_the_universe_are_events_and_the_rest_are_counted() {
    let mut d = steady();
    let mut rig = Rig::new(0);
    let mut tr = TestTransport::with_capacity(1 << 16);
    let mut rec = Rec::default();
    tr.inject_incoming(&text(TRADE));
    tr.inject_incoming(&text(
        br#"{"type":"Trade","symbol":"SNDK-20260927-1948-P","price":"1","size":"1","side":"sell","timestamp":1}"#,
    ));
    rig.drive(&mut tr, &mut d, &mut rec).unwrap();
    let trades = rec.of(ChannelId::Trade);
    assert_eq!(trades.len(), 1);
    assert_eq!((trades[0].sym, trades[0].v0, trades[0].v1), (BTC_C, 52_300, 5_000_000));
    assert_eq!(crate::counters::get(&rig.counters.ws.foreign_trades), 1);
}

#[test]
fn an_expired_instrument_stops_emitting() {
    let mut d = steady();
    let mut rig = Rig::new(0);
    let mut tr = TestTransport::with_capacity(1 << 16);
    tr.inject_incoming(&text(MU_EXPIRED));
    tr.inject_incoming(&text(Q2X));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert!(rig.pop_ticks().is_empty(), "MU-20260928-1090-C expired");
    assert_eq!(crate::counters::get(&rig.counters.ws.listings[1]), 1);
    // A reconnect does not revive it (process-lifetime).
    d.reset_for_reconnect(1);
    d.state = State::Steady;
    tr.inject_incoming(&text(Q2X));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert!(rig.pop_ticks().is_empty());
}

#[test]
fn a_slow_consumer_close_is_counted_by_cause_and_closes_the_session() {
    let mut d = steady();
    let mut rig = Rig::new(0);
    let mut tr = TestTransport::with_capacity(1 << 16);
    let mut body = 1008u16.to_be_bytes().to_vec();
    body.extend_from_slice(CLOSE_ML);
    tr.inject_incoming(&frame(0x88, &body));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert_eq!(d.state(), State::Closed);
    assert_eq!(d.last_close(), Some(HcCloseCause::MessageLimit));
    assert_eq!(
        crate::counters::get(&rig.counters.ws.closes[HcCloseCause::MessageLimit as usize]),
        1
    );
}

#[test]
fn a_kill_after_a_slow_consumer_close_requests_a_snapshot() {
    let mut d = steady();
    d.last_close = Some(HcCloseCause::MessageLimit);
    let mut conn = HcConn::<TestTransport>::new(d, b"h", core_net::Backoff::default_for_ingress(3));
    conn.transport = Some(TestTransport::with_capacity(64));
    let status = IngressStatus::new();
    let counters = HcCounters::new();
    conn.kill(1, &status, &counters);
    assert_eq!(counters.snapshot_req.load(Ordering::Acquire), 1);
    // A plain close (no slow-consumer reason) does not.
    conn.drv.last_close = Some(HcCloseCause::Other);
    conn.transport = Some(TestTransport::with_capacity(64));
    conn.kill(2, &status, &counters);
    assert_eq!(counters.snapshot_req.load(Ordering::Acquire), 1);
}

#[test]
fn a_server_ping_is_answered_and_the_clock_sync_measures_its_rtt() {
    for p in TestTransport::PING_ECHO_CASES {
        let mut d = steady();
        let mut rig = Rig::new(0);
        let mut tr = TestTransport::with_capacity(1 << 12);
        tr.inject_server_ping(p);
        rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
        tr.expect_pong_echo(p);
    }
    let mut d = steady();
    let mut rig = Rig::new(0);
    let mut tr = TestTransport::with_capacity(1 << 12);
    queue_clock_sync(&mut d, 5_000_000).unwrap();
    let answer = format!("{{\"type\":\"ClockSynced\",\"nonce\":\"{}\",\"server_at\":1}}", 5_000_000);
    tr.inject_incoming(&text(answer.as_bytes()));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert_eq!(crate::counters::get(&rig.counters.ws.clock_syncs), 1);
    assert_eq!(d.clock_nonce, 0, "answered");
    // A stranger's nonce is counted but measures nothing.
    tr.inject_incoming(&text(br#"{"type":"ClockSynced","nonce":"999","server_at":1}"#));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert_eq!(crate::counters::get(&rig.counters.ws.clock_syncs), 2);
}

#[test]
fn malformed_and_foreign_messages_are_rejects_and_nothing_else() {
    let mut d = steady();
    let mut rig = Rig::new(0);
    let mut tr = TestTransport::with_capacity(1 << 16);
    let mut rec = Rec::default();
    tr.inject_incoming(&text(b"{not json"));
    tr.inject_incoming(&text(
        br#"{"type":"IndicativeMarketData","instrument":"NOT-OURS-1-C","num_providers":0,"published_at":1,"timestamp":1}"#,
    ));
    tr.inject_incoming(&text(br#"{"type":"Error","message":"x"}"#));
    tr.inject_incoming(&frame(0x82, b"\x00\x01"));
    rig.drive(&mut tr, &mut d, &mut rec).unwrap();
    assert!(rig.pop_ticks().is_empty());
    assert_eq!(rec.rejects, 2, "the broken JSON and the foreign instrument");
    assert_eq!(rig.status.parse_errors_total(), 3, "+ the binary frame");
    assert_eq!(crate::counters::get(&rig.counters.ws.venue_errors), 1);
}

#[test]
fn the_handoff_drains_poller_rows_onto_the_opt_lane_capture_first() {
    let (mut hp, mut hc) = Ring::<OptSummary, HANDOFF_RING_CAP>::new().split();
    let mut rig = Rig::new(0);
    let mut rec = Rec::default();
    let row = OptSummary::new(1, VenueId::Hypercall, ETH_P, 1, 5, 6, 7, 8, 9, 10, 11, 12);
    assert!(hp.try_push_ref(&row));
    assert!(hp.try_push_ref(&row));
    let mut lanes = Lanes {
        ticks: &mut rig.ticks.0,
        events: &mut rig.events.0,
        event_mask: 0,
        opts: &mut rig.opts.0,
    };
    assert_eq!(drain_handoff(&mut hc, &mut lanes, &rig.status, &mut rec), 2);
    assert_eq!(rec.opts.len(), 2);
    let mut n = 0;
    while let Some(o) = rig.opts.1.try_pop_ref() {
        assert_eq!(o.sym, ETH_P);
        n += 1;
    }
    assert_eq!(n, 2);
}

#[test]
fn a_ring_dropped_tick_is_re_emitted_on_its_republication() {
    let mut d = steady();
    // Pre-fill the tick lane so the quote's push drops.
    let mut rig = Rig::new(0);
    let mut k = 0;
    while k < TICK_RING_CAP {
        assert!(rig.ticks.0.try_push_ref(&Tick::ZERO));
        k += 1;
    }
    let mut tr = TestTransport::with_capacity(1 << 16);
    tr.inject_incoming(&text(Q1));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert_eq!(rig.status.ring_drops_total(), 1);
    // Drain the backlog; the same quote republished is emitted again.
    let _ = rig.pop_ticks();
    tr.inject_incoming(&text(Q1));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert_eq!(rig.pop_ticks().len(), 1);
}

#[test]
fn every_consumed_frame_is_progress_even_one_that_emits_nothing() {
    // The drain loop judges progress by frames consumed (`run` step 3):
    // a republication, a server ping and an unknown message emit no tick
    // and must still count, or an rx-full step of them would wait for
    // the next readiness edge.
    let mut d = steady();
    let mut rig = Rig::new(0);
    let mut tr = TestTransport::with_capacity(1 << 16);
    tr.inject_incoming(&text(Q1));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert_eq!(d.frames(), 1);
    let ticks_before = rig.ticks.0.published();
    tr.inject_incoming(&text(Q1));
    tr.inject_server_ping(b"x");
    tr.inject_incoming(&text(br#"{"type":"Heartbeat"}"#));
    rig.drive(&mut tr, &mut d, &mut NullCapture).unwrap();
    assert_eq!(rig.ticks.0.published(), ticks_before, "nothing emitted");
    assert_eq!(d.frames(), 4, "yet three more frames consumed");
    assert_eq!(rig.status.parse_errors_total(), 0);
}
