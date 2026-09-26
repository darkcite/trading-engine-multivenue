// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-hypercall run loop (HC3)
//!
//! Event-driven state machine for ONE Hypercall public socket, on the
//! ingress thread, with ONE tick producer, ONE venue-event producer and
//! ONE options-summary producer (single-writer law). The REST poller
//! thread ([`crate::rest`]) hands its `OptSummary` rows over an internal
//! SPSC ring that this thread drains, so every Hypercall capture file
//! and every engine lane still has exactly one writer.
//!
//! ```text
//! Connecting ──► NeedsWsWrite ──► AwaitingWsUpgrade ──► Steady ──┐
//!      ▲                                                          │
//!      └──────────────── Closed / Err ────────────────────────────┘
//! ```
//!
//! On entry to `Steady` the driver queues, in order: one `ClockSync`
//! (the venue has no REST time endpoint), then ONE `Subscribe` per
//! channel — `index_prices`, `trades`, `market_updates` unfiltered and
//! `indicative_market_data` naming the whole universe in ONE frame (the
//! D3 law, crate doc). A session is established once any channel is
//! acked (`Subscribed`) or its first quote arrives; the WS2 budget
//! reaps one that confirms nothing.
//!
//! **Slow-consumer law.** The venue closes a lagging socket with 1008
//! and a reason JSON naming the `cause`; there is no replay. The close
//! is counted by cause, the session is torn down, the reconnect
//! re-subscribes in one frame, and — because quotes that changed during
//! the gap are only republished when they next change — a SNAPSHOT is
//! requested from the REST poller (`snapshot_resubscribe`).
//!
//! **Tick law.** A tick is a BBO CHANGE (the house doctrine): a push
//! whose `(bid px, bid qty, ask px, ask qty)` and stale verdict equal
//! the last one EMITTED for that instrument takes no ring slot (the
//! venue re-pushes ~1/s/instrument while prices move ~0.7/s). Quotes
//! are emitted as the venue published them — one-sided (missing side
//! 0/0) and CROSSED included; a push with neither side emits nothing.
//! An instrument the venue announced `Expired`/`Deleted` stops emitting.
//!
//! **Drain law (I-3, [`core_net::drain`]).** A wake drives the socket
//! again while a step's read stopped on a full rx, published a tick or
//! moved the state, at most [`core_net::DRAIN_STEP_CAP`] steps; a capped
//! drain re-polls without sleeping, so a backlog never waits for a
//! readiness edge and the poller handoff and the heartbeat keep their
//! turn.
//!
//! Everything after the handshake is zero-alloc: parsers slice the rx
//! buffer in place; outbound frames are serialised from parts straight
//! into tx; the only copies are the 64-byte PODs moved into their slots.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_metrics::{IngressState, IngressStatus};
use core_net::{
    constant_time_eq, expected_accept, queue_masked_text_frame_parts, read_server_handshake,
    sec_websocket_key_from_seed, write_client_handshake, ws_mask_from_counter, ws_read_frame,
    ws_unmask_in_place, ws_write_pong, Drained, HandshakeResult, IoBuf, RxFill, Status, Transport,
    WsOpcode, WsReadResult,
};
use core_ring::{Consumer, Producer};
use core_time::{now_ns, FeedClock, NsTs};
use core_types::{
    event_lane_bit, Capture, ChannelEvent, ChannelId, OptSummary, Price, Qty, SymbolId, Tick,
    VenueId, EVENT_RING_SIZE, OPT_RING_SIZE, TICK_FLAG_STALE,
};

use crate::counters::{bump, set, HcCloseCause, HcCounters};
use crate::{
    classify, clock_sync_parts, fmt_u64, parse_clock_synced, parse_close_reason, parse_indicative,
    parse_index_update, parse_market_update, parse_trade, provider_quote_seq, span_bytes,
    subscribe_parts, walk_providers, HcIndexEntry, HcListingAction, HcMsg, HcProvider, HcQuote,
    HcSymbolTable, HcUnderlyings, CH_INDEX, CH_INDICATIVE, CH_MARKET_UPDATES, CH_TRADES,
    HC_MAX_INSTRUMENTS, HC_MAX_PROVIDERS, HC_MAX_UNDERLYINGS, SIDE_ASK, SIDE_BID,
    SUBSCRIBE_PARTS_MAX,
};

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// Rx buffer. A subscribe answers with a snapshot of every quoted
/// instrument at once (576 × ~500 B ≈ 290 KB), and the capped stream
/// runs ~400 msg/s ≈ 185 KiB/s (HC0, 2026-09-25): 1 MiB keeps a
/// multi-second stall inside the buffer while the parser catches up.
pub const RX_BUF_SIZE: usize = 1024 * 1024;

/// Tx buffer: handshake + the indicative subscribe at the table cap
/// (≤ 36 KiB) + three small subscribes + ClockSyncs + pongs.
pub const TX_BUF_SIZE: usize = 64 * 1024;

/// Tick-ring capacity. Must equal `engine::TICK_RING_SIZE` (the cli
/// const-asserts it).
pub const TICK_RING_CAP: usize = 16_384;

/// Poller → ingress handoff ring (`OptSummary` rows): one round of the
/// universe (≤ 1 024 rows) twice over.
pub const HANDOFF_RING_CAP: usize = 2048;

/// Handshake allowance in the tx budget.
const HANDSHAKE_TX_MAX: usize = 1024;

/// The indicative subscribe's worst case at the table cap: the
/// envelope and per row two quotes, a comma and the widest name, plus
/// the 64-bit WS header — it must fit tx beside the handshake.
const SUBSCRIBE_TX_MAX: usize = 128 + HC_MAX_INSTRUMENTS * (3 + crate::HC_SYMBOL_MAX) + 14;

/// Client heartbeat: a `ClockSync` every 20 s (the server pings every
/// 20 s and closes after 60 s without a pong — the pong is answered in
/// the drain; the ClockSync keeps the RTT gauge honest and proves the
/// write path), reconnect after 60 s without an inbound byte.
pub const KEEPALIVE: core_net::KeepaliveCfg = core_net::KeepaliveCfg {
    ping_interval_ns: 20_000_000_000,
    idle_timeout_ns: 60_000_000_000,
};

/// `Driver::acks` bit per acked channel.
const ACK_INDICATIVE: u8 = 1;
const ACK_INDEX: u8 = 2;
const ACK_TRADES: u8 = 4;
const ACK_MARKET_UPDATES: u8 = 8;

// ---------------------------------------------------------------
// State
// ---------------------------------------------------------------

/// Run-loop state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// TLS handshake in progress.
    Connecting,
    /// TLS ready; WebSocket opening request not yet sent.
    NeedsWsWrite,
    /// Opening request sent; awaiting `101 Switching Protocols`.
    AwaitingWsUpgrade,
    /// Upgraded — subscribed, pushes flowing.
    Steady,
    /// Peer closed (or sent CLOSE).
    Closed,
}

/// How the loop terminated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// External stop flag observed.
    Stopped,
    /// Poll-infrastructure failure.
    Error,
}

/// Per-instrument state. One cache line.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
struct RowState {
    /// The last EMITTED `[bid px, bid qty, ask px, ask qty]` ×1e6.
    /// Connection-scoped: a reconnect re-emits the first quote.
    last_bbo: [i64; 4],
    /// The stale verdict (0/1) of the push `last_bbo` latched.
    last_stale: u8,
    /// 1 once the venue announced the instrument `Expired`/`Deleted`:
    /// it never emits again this process.
    dead: u8,
    /// 1 once a two-sided quote was seen (the `quoted_instruments`
    /// gauge). Process-lifetime.
    quoted: u8,
    _pad: [u8; 29],
}

impl RowState {
    const EMPTY: Self = Self {
        last_bbo: [0; 4],
        last_stale: 0,
        dead: 0,
        quoted: 0,
        _pad: [0; 29],
    };
}

const _: () = assert!(core::mem::size_of::<RowState>() == 64);

/// Mutable per-connection state owned by the run loop. Preallocated at
/// construction; never reallocates in steady state.
///
/// **Single-writer invariant.** `!Sync` via the marker field; the cli
/// moves the driver onto the Hypercall ingress thread at boot.
pub struct Driver {
    state: State,
    rx: IoBuf,
    tx: IoBuf,
    sec_key: [u8; 24],
    expected_accept_val: [u8; 28],
    /// Monotonic ns of the last inbound frame (the keepalive reads it).
    pub last_activity_ns: NsTs,
    /// Monotonic counter feeding the outbound frame masks.
    pub mask_counter: u64,
    /// Inbound frames consumed, process-lifetime — the drain loop's
    /// progress predicate (see [`run`] step 3).
    frames: u64,
    /// The universe (boot-built).
    symbols: HcSymbolTable,
    /// Underlying → index sym (boot-built).
    underlyings: HcUnderlyings,
    /// Per-instrument state, indexed by symbol-table row.
    rows: Box<[RowState]>,
    /// Channels acked this session ([`ACK_INDICATIVE`] …).
    acks: u8,
    /// First quote seen this session (confirms like an ack).
    quoted_this_session: bool,
    /// The freshest quote `published_at` (venue ms) — the index-age
    /// gauge's venue-clock reference.
    last_published_ms: u64,
    /// The nonce and monotonic send instant of the outstanding
    /// ClockSync (`0` = none outstanding).
    clock_nonce: u64,
    clock_sent_ns: NsTs,
    /// Why the venue closed the last session (None = it did not send a
    /// CLOSE with a reason). Read by the loop to request a snapshot.
    last_close: Option<HcCloseCause>,
    /// WS2 establishment budget (ns from session start to the first
    /// confirmation).
    establish_budget_ns: u64,
    /// VT2 staleness judge over the quote `timestamp`. Reset on
    /// reconnect; threshold = the venue default or the operator's
    /// `--stale-after-ms hypercall:<ms>`.
    feed_clock: FeedClock,
    /// `!Sync` marker — see struct doc.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate buffers (boot-time) and seed the handshake nonce.
    #[must_use]
    pub fn new(nonce_seed: u64, symbols: HcSymbolTable, underlyings: HcUnderlyings) -> Self {
        let sec_key = sec_websocket_key_from_seed(nonce_seed);
        let accept = expected_accept(&sec_key);
        Self {
            state: State::Connecting,
            rx: IoBuf::with_capacity(RX_BUF_SIZE),
            tx: IoBuf::with_capacity(TX_BUF_SIZE),
            sec_key,
            expected_accept_val: accept,
            last_activity_ns: 0,
            mask_counter: 0,
            frames: 0,
            symbols,
            underlyings,
            rows: vec![RowState::EMPTY; HC_MAX_INSTRUMENTS].into_boxed_slice(),
            acks: 0,
            quoted_this_session: false,
            last_published_ms: 0,
            clock_nonce: 0,
            clock_sent_ns: 0,
            last_close: None,
            establish_budget_ns: core_net::ESTABLISH_BUDGET_NS,
            feed_clock: FeedClock::new(VenueId::Hypercall.default_stale_after_ms()),
            _not_sync: ::core::marker::PhantomData,
        }
    }

    /// WS2: override the establishment budget (tests).
    #[inline]
    pub fn set_establish_budget_ns(&mut self, ns: u64) {
        self.establish_budget_ns = ns;
    }

    /// VT2: override the staleness threshold (boot-time; re-arms the
    /// estimator unlearned, like a fresh connection).
    #[inline]
    pub fn set_stale_after_ms(&mut self, ms: u32) {
        self.feed_clock = FeedClock::new(ms);
    }

    /// VT2: the smoothed quote feed delay (ms).
    #[inline]
    #[must_use]
    pub fn feed_delay_ema_ms(&self) -> u32 {
        self.feed_clock.delay_ema_ms()
    }

    /// Current state (metrics + tests).
    #[inline]
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Confirmations this session: acked channels, plus one once a
    /// quote arrived — the WS2 establishment predicate.
    #[inline]
    #[must_use]
    pub fn sub_count(&self) -> usize {
        self.acks.count_ones() as usize + usize::from(self.quoted_this_session)
    }

    /// Inbound frames consumed since construction (every opcode).
    #[inline]
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The universe size.
    #[inline]
    #[must_use]
    pub fn instruments(&self) -> usize {
        self.symbols.len()
    }

    /// Why the venue closed the last session (`None` = no reason sent).
    #[inline]
    #[must_use]
    pub fn last_close(&self) -> Option<HcCloseCause> {
        self.last_close
    }

    /// Reset per-connection state for a reconnect. The dead flags and
    /// the quoted set are process-lifetime; the BBO latches, acks and
    /// the feed clock are connection-scoped.
    pub fn reset_for_reconnect(&mut self, nonce_seed: u64) {
        self.state = State::Connecting;
        self.rx.clear();
        self.tx.clear();
        self.sec_key = sec_websocket_key_from_seed(nonce_seed);
        self.expected_accept_val = expected_accept(&self.sec_key);
        self.last_activity_ns = 0;
        self.mask_counter = 0;
        let mut i = 0;
        while i < self.rows.len() {
            self.rows[i].last_bbo = [0; 4];
            self.rows[i].last_stale = 0;
            i += 1;
        }
        self.acks = 0;
        self.quoted_this_session = false;
        self.clock_nonce = 0;
        self.clock_sent_ns = 0;
        self.last_close = None;
        self.feed_clock.reset();
    }
}

// ---------------------------------------------------------------
// drive_one
// ---------------------------------------------------------------

/// The producers one Hypercall ingress owns.
pub struct Lanes<'a> {
    /// Tick lane (option BBO ticks).
    pub ticks: &'a mut Producer<Tick, TICK_RING_CAP>,
    /// Venue-event lane.
    pub events: &'a mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    /// Which channels ride the event lane (`event_lane_bit` mask); the
    /// rest are capture-only.
    pub event_mask: u16,
    /// Options-summary lane (opt lane 3).
    pub opts: &'a mut Producer<OptSummary, OPT_RING_SIZE>,
}

/// Pump the transport once and advance the state machine. Zero-alloc
/// once the handshake has completed.
///
/// Returns `Ok(true)` when this step's read stopped on a full rx
/// ([`RxFill::Full`]): input may still wait below it, so the caller
/// drives again ([`core_net::drain`]).
pub fn drive_one<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    counters: &HcCounters,
    capture: &mut C,
) -> io::Result<bool> {
    flush_tx(transport, drv)?;
    let fill = core_net::fill_rx(transport, &mut drv.rx)?;
    if fill == RxFill::Eof {
        drv.state = State::Closed;
    }
    match drv.state {
        State::Connecting | State::Closed => {}
        State::NeedsWsWrite => {
            let n = write_client_handshake(drv.tx.free_mut(), host, crate::HC_WS_PATH, &drv.sec_key)
                .map_err(|_| io::Error::other("ws handshake buffer too small"))?;
            drv.tx.advance(n);
            drv.state = State::AwaitingWsUpgrade;
        }
        State::AwaitingWsUpgrade => {
            advance_ws_upgrade(drv, status)?;
            if drv.state == State::Steady {
                queue_clock_sync(drv, now_ns())?;
                queue_subscribe_all(drv, counters)?;
            }
        }
        State::Steady => drain_ws_frames(drv, lanes, status, counters, capture)?,
    }
    flush_tx(transport, drv)?;
    Ok(fill == RxFill::Full)
}

/// I-3 ([`core_net::drain`]): drive the connection until a step makes no
/// progress — its read did not stop on a full rx, it published no tick,
/// its state held — or [`core_net::DRAIN_STEP_CAP`] steps have run, so a
/// flood cannot hold the poller handoff, the heartbeat or the stop flag.
fn drive_until_idle<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    counters: &HcCounters,
    capture: &mut C,
) -> Drained {
    core_net::drain_until_idle!(
        step: drive_one(transport, drv, host, lanes, status, counters, capture),
        published: lanes.ticks.published(),
        key: drv.state(),
        closed: drv.state() == State::Closed,
    )
}

/// Bump `Connecting → NeedsWsWrite` once the transport is TLS-ready.
#[inline]
pub fn note_transport_ready(drv: &mut Driver, status: Status) {
    match status {
        Status::Ready if drv.state == State::Connecting => drv.state = State::NeedsWsWrite,
        Status::Closed => drv.state = State::Closed,
        _ => {}
    }
}

fn flush_tx<T: Transport>(transport: &mut T, drv: &mut Driver) -> io::Result<()> {
    if drv.tx.is_empty() {
        return Ok(());
    }
    let mut written = 0;
    while written < drv.tx.len() {
        match transport.write(&drv.tx.filled()[written..]) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    if written == drv.tx.len() {
        drv.tx.clear();
    } else if written > 0 {
        drv.tx.consume(written);
    }
    Ok(())
}

fn advance_ws_upgrade(drv: &mut Driver, status: &IngressStatus) -> io::Result<()> {
    match read_server_handshake(drv.rx.filled()) {
        HandshakeResult::Incomplete => Ok(()),
        HandshakeResult::Upgraded {
            accept_start,
            accept_end,
            header_end,
        } => {
            let got = &drv.rx.filled()[accept_start..accept_end];
            if got.len() != drv.expected_accept_val.len()
                || !constant_time_eq(got, &drv.expected_accept_val)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Sec-WebSocket-Accept mismatch",
                ));
            }
            drv.rx.consume(header_end);
            drv.state = State::Steady;
            status.set_state(IngressState::Up);
            let now = now_ns();
            drv.last_activity_ns = now;
            status.touch_activity(now);
            status.add_bytes(header_end as u64);
            Ok(())
        }
        HandshakeResult::Malformed => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed server handshake",
        )),
    }
}

/// Queue a `ClockSync` whose nonce is the monotonic send instant (the
/// answer's arrival minus it is the RTT; the wall clock is never read).
fn queue_clock_sync(drv: &mut Driver, now: NsTs) -> io::Result<()> {
    let mut digits = [0u8; 20];
    let d = fmt_u64(now, &mut digits);
    queue_masked_text_frame_parts(&mut drv.tx, &mut drv.mask_counter, &clock_sync_parts(d))?;
    drv.clock_nonce = now;
    drv.clock_sent_ns = now;
    Ok(())
}

/// Queue the session's subscribe set: ONE frame per channel, the
/// indicative one naming the whole universe (the D3 law).
fn queue_subscribe_all(drv: &mut Driver, counters: &HcCounters) -> io::Result<()> {
    if drv.symbols.is_empty() {
        return Err(io::Error::other("hypercall: no instruments configured"));
    }
    // The whole set is queued at the upgrade edge: the handshake, the
    // widest indicative frame and the three small ones fit tx at once.
    const { assert!(HANDSHAKE_TX_MAX + SUBSCRIBE_TX_MAX + 1024 <= TX_BUF_SIZE) };
    let mut parts: [&[u8]; SUBSCRIBE_PARTS_MAX] = [&[]; SUBSCRIBE_PARTS_MAX];
    for ch in [CH_INDEX, CH_TRADES, CH_MARKET_UPDATES] {
        let n = subscribe_parts(ch, None, &mut parts)
            .ok_or_else(|| io::Error::other("hypercall: subscribe parts"))?;
        queue_masked_text_frame_parts(&mut drv.tx, &mut drv.mask_counter, &parts[..n])?;
    }
    let n = subscribe_parts(CH_INDICATIVE, Some(&drv.symbols), &mut parts)
        .ok_or_else(|| io::Error::other("hypercall: universe over the subscribe parts cap"))?;
    queue_masked_text_frame_parts(&mut drv.tx, &mut drv.mask_counter, &parts[..n])?;
    bump(&counters.ws.subscribes);
    Ok(())
}

// ---------------------------------------------------------------
// Frame drain + dispatch
// ---------------------------------------------------------------

fn drain_ws_frames<C: Capture>(
    drv: &mut Driver,
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    counters: &HcCounters,
    capture: &mut C,
) -> io::Result<()> {
    loop {
        match ws_read_frame(drv.rx.filled()) {
            WsReadResult::Incomplete => {
                if drv.rx.free_mut().is_empty() && !drv.rx.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ws frame exceeds rx buffer capacity",
                    ));
                }
                return Ok(());
            }
            WsReadResult::Malformed => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed ws frame"));
            }
            WsReadResult::Frame { header, payload } => {
                let total = header.header_len as usize + header.payload_len as usize;
                if header.masked {
                    ws_unmask_in_place(&mut drv.rx.filled_mut()[payload.start..payload.end], header.mask);
                }
                match header.opcode {
                    WsOpcode::Text => {
                        handle_text(drv, payload.start..payload.end, lanes, status, counters, capture);
                    }
                    WsOpcode::Ping => {
                        let mask = ws_mask_from_counter(drv.mask_counter);
                        drv.mask_counter = drv.mask_counter.wrapping_add(1);
                        // The echo goes straight from rx into tx (disjoint
                        // field borrows) — `ws_read_frame` already refused
                        // a control payload over 125 B.
                        if let Some(src) = drv.rx.filled().get(payload.start..payload.end) {
                            if let Ok(n) = ws_write_pong(drv.tx.free_mut(), src, mask) {
                                drv.tx.advance(n);
                            }
                        }
                    }
                    WsOpcode::Close => {
                        // The slow-consumer law: code (2 B) + reason JSON.
                        let reason = drv
                            .rx
                            .filled()
                            .get(payload.start..payload.end)
                            .and_then(|p| p.get(2..))
                            .unwrap_or(&[]);
                        let cause = parse_close_reason(reason);
                        bump(&counters.ws.closes[cause as usize]);
                        drv.last_close = Some(cause);
                        drv.state = State::Closed;
                    }
                    // Hypercall pushes JSON text, never binary, and does
                    // not fragment: count and drop rather than allocate
                    // a reassembly buffer.
                    WsOpcode::Binary | WsOpcode::Continuation => status.inc_parse_errors(),
                    WsOpcode::Pong => {}
                }
                let now = now_ns();
                drv.last_activity_ns = now;
                status.touch_activity(now);
                status.add_bytes(total as u64);
                drv.rx.consume(total);
                drv.frames += 1;
                if drv.state == State::Closed {
                    return Ok(());
                }
            }
        }
    }
}

/// Emit one event: capture first (the §6.5 capture-before-push law),
/// then the event lane when its channel is in the mask.
#[inline]
fn emit_event<C: Capture>(
    ev: &ChannelEvent,
    ch: ChannelId,
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    capture: &mut C,
) {
    capture.event(ev);
    if lanes.event_mask & event_lane_bit(ch) != 0 && !lanes.events.try_push_ref(ev) {
        status.inc_event_ring_drops();
    }
}

fn handle_text<C: Capture>(
    drv: &mut Driver,
    range: core::ops::Range<usize>,
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    counters: &HcCounters,
    capture: &mut C,
) {
    let payload = drv.rx.filled().get(range).unwrap_or(&[]);
    capture.raw_frame(now_ns(), payload);
    status.add_msgs(1);
    let ok = match classify(payload) {
        HcMsg::Indicative => on_quote(
            payload,
            &drv.symbols,
            &mut drv.rows,
            &mut drv.feed_clock,
            &mut drv.quoted_this_session,
            &mut drv.last_published_ms,
            lanes,
            status,
            counters,
            capture,
        ),
        HcMsg::IndexPrice => on_index(
            payload,
            &drv.underlyings,
            drv.last_published_ms,
            lanes,
            status,
            counters,
            capture,
        ),
        HcMsg::Trade => on_trade(payload, &drv.symbols, lanes, status, counters, capture),
        HcMsg::MarketUpdate => on_listing(payload, &drv.symbols, &mut drv.rows, counters),
        HcMsg::Subscribed => {
            let ack = if memchr::memmem::find(payload, CH_INDICATIVE).is_some() {
                ACK_INDICATIVE
            } else if memchr::memmem::find(payload, CH_INDEX).is_some() {
                ACK_INDEX
            } else if memchr::memmem::find(payload, CH_MARKET_UPDATES).is_some() {
                ACK_MARKET_UPDATES
            } else if memchr::memmem::find(payload, CH_TRADES).is_some() {
                ACK_TRADES
            } else {
                0
            };
            drv.acks |= ack;
            ack != 0
        }
        HcMsg::ClockSynced => match parse_clock_synced(payload) {
            Some((nonce, _server_at)) => {
                if drv.clock_nonce != 0 && nonce == drv.clock_nonce {
                    let rtt_ms = now_ns().saturating_sub(drv.clock_sent_ns) / 1_000_000;
                    set(&counters.ws.clock_rtt_ms, rtt_ms);
                    drv.clock_nonce = 0;
                }
                bump(&counters.ws.clock_syncs);
                true
            }
            None => false,
        },
        HcMsg::Error => {
            bump(&counters.ws.venue_errors);
            true
        }
        HcMsg::Other => true,
        HcMsg::Malformed => false,
    };
    if !ok {
        status.inc_parse_errors();
        capture.parse_reject(now_ns(), payload);
    }
}

/// One `IndicativeMarketData`: the tick (BBO change), the provider
/// detail of a multi-provider quote, the quote-shape counters.
#[allow(clippy::too_many_arguments)]
#[inline]
fn on_quote<C: Capture>(
    payload: &[u8],
    symbols: &HcSymbolTable,
    rows: &mut [RowState],
    feed_clock: &mut FeedClock,
    quoted_this_session: &mut bool,
    last_published_ms: &mut u64,
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    counters: &HcCounters,
    capture: &mut C,
) -> bool {
    let mut q = HcQuote::ZERO;
    let Some(meta) = parse_indicative(payload, &mut q) else {
        return false;
    };
    let Some((row, sym)) = symbols.lookup(span_bytes(payload, q.instrument)) else {
        // Not ours: the filter should make this impossible — count it
        // as a parse error so a venue-side filter change is visible.
        return false;
    };
    *quoted_this_session = true;
    *last_published_ms = (*last_published_ms).max(q.published_at_ms);
    set(
        &counters.ws.quote_publish_lag_ms,
        q.published_at_ms.saturating_sub(q.timestamp_ms),
    );
    let now = now_ns();
    let judged = feed_clock.judge(q.timestamp_ms, now);
    status.set_feed_delay_ema_ms(feed_clock.delay_ema_ms());
    let Some(r) = rows.get_mut(row) else {
        return false;
    };
    if r.dead != 0 {
        return true;
    }
    match meta.sides {
        0 => {
            bump(&counters.ws.empty_quotes);
            return true;
        }
        SIDE_BID | SIDE_ASK => bump(&counters.ws.one_sided_quotes),
        _ => {
            if q.bid_px_1e6 > q.ask_px_1e6 {
                bump(&counters.ws.crossed_quotes);
            }
            if r.quoted == 0 {
                r.quoted = 1;
                counters.ws.quoted_instruments.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if meta.num_providers >= 2 {
        emit_providers(payload, &q, meta.num_providers, sym, lanes, status, counters, capture);
    }
    let stale = judged.stale as u8;
    let bbo = [q.bid_px_1e6, q.bid_qty_1e6, q.ask_px_1e6, q.ask_qty_1e6];
    if r.last_bbo == bbo && r.last_stale == stale {
        return true;
    }
    r.last_bbo = bbo;
    r.last_stale = stale;
    status.add_ticks(1);
    if judged.stale {
        status.inc_stale_ticks();
    }
    let tick = Tick::new_stamped(
        now,
        VenueId::Hypercall,
        sym,
        0,
        Price::from_raw(q.bid_px_1e6),
        Qty::from_raw(q.bid_qty_1e6),
        Price::from_raw(q.ask_px_1e6),
        Qty::from_raw(q.ask_qty_1e6),
        q.timestamp_ms,
        stale * TICK_FLAG_STALE,
    );
    capture.tick(&tick);
    if !lanes.ticks.try_push_ref(&tick) {
        status.inc_ring_drops();
        // The engine never saw this quote: forget it, so the venue's
        // next republication of the same touch is emitted.
        r.last_bbo = [0; 4];
    }
    true
}

/// The per-provider sides of a quote with ≥ 2 providers, as
/// `ProviderQuote` events (two per provider: bid, ask).
#[allow(clippy::too_many_arguments)]
#[inline]
fn emit_providers<C: Capture>(
    payload: &[u8],
    q: &HcQuote,
    num_providers: u8,
    sym: SymbolId,
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    counters: &HcCounters,
    capture: &mut C,
) {
    let mut ps = [HcProvider::default(); HC_MAX_PROVIDERS];
    let Some((read, present)) = walk_providers(payload, q.providers, &mut ps) else {
        status.inc_parse_errors();
        return;
    };
    let seen = u64::from(present);
    if seen > counters.ws.providers_max.load(Ordering::Relaxed) {
        set(&counters.ws.providers_max, seen);
    }
    let now = now_ns();
    let mut i = 0usize;
    while i < read as usize {
        let p = &ps[i];
        let bid = provider_side(now, sym, i as u8, false, num_providers, p, p.bid_px_1e6, p.bid_qty_1e6);
        let ask = provider_side(now, sym, i as u8, true, num_providers, p, p.ask_px_1e6, p.ask_qty_1e6);
        emit_event(&bid, ChannelId::ProviderQuote, lanes, status, capture);
        emit_event(&ask, ChannelId::ProviderQuote, lanes, status, capture);
        counters.ws.provider_quotes.fetch_add(2, Ordering::Relaxed);
        i += 1;
    }
}

/// One side of one provider's quote as a `ProviderQuote` event.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
const fn provider_side(
    now: NsTs,
    sym: SymbolId,
    index: u8,
    ask: bool,
    num_providers: u8,
    p: &HcProvider,
    px_1e6: i64,
    qty_1e6: i64,
) -> ChannelEvent {
    ChannelEvent::new(
        now,
        VenueId::Hypercall,
        ChannelId::ProviderQuote,
        sym,
        provider_quote_seq(index, ask, num_providers, p.wallet_lo32),
        p.updated_at_ms,
        px_1e6,
        qty_1e6,
    )
}

/// One `IndexPriceUpdate`: a `Mark` per configured underlying on its
/// `hypercall-idx:<U>` sym (`v0` = index ×1e6, `v1` = the entry's
/// source time).
#[inline]
fn on_index<C: Capture>(
    payload: &[u8],
    underlyings: &HcUnderlyings,
    last_published_ms: u64,
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    counters: &HcCounters,
    capture: &mut C,
) -> bool {
    let mut xs = [HcIndexEntry::default(); HC_MAX_UNDERLYINGS];
    let Some((read, _present, _frame_ts)) = parse_index_update(payload, &mut xs) else {
        return false;
    };
    let now = now_ns();
    let mut newest = 0u64;
    let mut i = 0usize;
    while i < read as usize {
        let x = &xs[i];
        if let Some((_, idx_sym)) = underlyings.lookup(span_bytes(payload, x.underlying)) {
            let ev = ChannelEvent::new(
                now,
                VenueId::Hypercall,
                ChannelId::Mark,
                idx_sym,
                0,
                x.ts_ms,
                x.price_1e6,
                x.ts_ms.min(i64::MAX as u64) as i64,
            );
            emit_event(&ev, ChannelId::Mark, lanes, status, capture);
            status.add_ticks(1);
        }
        newest = newest.max(x.ts_ms);
        i += 1;
    }
    // The index stamps its SOURCE observation; its lag behind the quote
    // stream's freshest publish stamp is the index-age gauge — venue
    // clock on both sides.
    if last_published_ms > 0 && newest > 0 {
        set(&counters.ws.index_age_ms, last_published_ms.saturating_sub(newest));
    }
    true
}

/// One `Trade` on an instrument of ours → `Trade` (`v0` = px ×1e6,
/// `v1` = size ×1e6, negated when the aggressor sold).
#[inline]
fn on_trade<C: Capture>(
    payload: &[u8],
    symbols: &HcSymbolTable,
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    counters: &HcCounters,
    capture: &mut C,
) -> bool {
    let Some(t) = parse_trade(payload) else {
        return false;
    };
    match symbols.lookup(span_bytes(payload, t.symbol)) {
        Some((_, sym)) => {
            let ev = ChannelEvent::new(
                now_ns(),
                VenueId::Hypercall,
                ChannelId::Trade,
                sym,
                0,
                t.ts_ms,
                t.px_1e6,
                t.signed_qty_1e6,
            );
            emit_event(&ev, ChannelId::Trade, lanes, status, capture);
        }
        None => bump(&counters.ws.foreign_trades),
    }
    true
}

/// One `MarketUpdate`: counted by action; an instrument of ours that
/// expired or was removed stops emitting for the rest of the process.
#[inline]
fn on_listing(payload: &[u8], symbols: &HcSymbolTable, rows: &mut [RowState], counters: &HcCounters) -> bool {
    let Some((action, symbol, _ts)) = parse_market_update(payload) else {
        return false;
    };
    let slot = match action {
        HcListingAction::Created => 0,
        HcListingAction::Expired => 1,
        HcListingAction::Deleted => 2,
        HcListingAction::ComboCreated | HcListingAction::Other => 3,
    };
    bump(&counters.ws.listings[slot]);
    if matches!(action, HcListingAction::Expired | HcListingAction::Deleted) {
        if let Some((row, _)) = symbols.lookup(span_bytes(payload, symbol)) {
            if let Some(r) = rows.get_mut(row) {
                r.dead = 1;
            }
        }
    }
    true
}

/// Move every `OptSummary` the REST poller handed over onto the opt
/// lane, capture first. Bounded by the handoff ring's contents.
pub fn drain_handoff<C: Capture>(
    handoff: &mut Consumer<OptSummary, HANDOFF_RING_CAP>,
    lanes: &mut Lanes<'_>,
    status: &IngressStatus,
    capture: &mut C,
) -> usize {
    let mut n = 0usize;
    // Each row is read IN PLACE in its handoff slot (the `Popped`
    // guard): captured from there, then copied once into its opt-lane
    // slot — the ring-slot publish, the one designed copy.
    while let Some(o) = handoff.try_pop_ref() {
        capture.opt_summary(&o);
        if !lanes.opts.try_push_ref(&o) {
            status.inc_opt_ring_drops();
        }
        n += 1;
    }
    n
}

// ---------------------------------------------------------------
// The loop
// ---------------------------------------------------------------

/// Stop flag raised by external threads for graceful shutdown.
pub type StopFlag = AtomicBool;

/// The one Hypercall public connection slot.
pub struct HcConn<'a, T: Transport> {
    /// Live transport, `None` while disconnected.
    pub transport: Option<T>,
    /// The driver.
    pub drv: Driver,
    /// `Host:` bytes (borrowed from the boot endpoint, which outlives
    /// the loop).
    host: &'a [u8],
    keepalive: core_net::Keepalive,
    backoff: core_net::Backoff,
    next_attempt_ns: NsTs,
    session_start_ns: NsTs,
    last_interest: Option<mio::Interest>,
}

impl<'a, T: Transport> HcConn<'a, T> {
    /// New slot, initially disconnected (due immediately).
    #[must_use]
    pub fn new(drv: Driver, host: &'a [u8], backoff: core_net::Backoff) -> Self {
        Self {
            transport: None,
            drv,
            host,
            keepalive: core_net::Keepalive::new(KEEPALIVE),
            backoff,
            next_attempt_ns: 0,
            session_start_ns: 0,
            last_interest: None,
        }
    }

    /// Tear down and schedule the next dial; a session that confirmed
    /// anything resets the backoff. A venue close with a slow-consumer
    /// cause asks the poller for a snapshot (the gap is not replayed).
    fn kill(&mut self, now: NsTs, status: &IngressStatus, counters: &HcCounters) {
        if self.transport.take().is_some() {
            status.inc_reconnects();
            if self.drv.sub_count() > 0 {
                self.backoff.reset();
            }
            if matches!(self.drv.last_close, Some(c) if c != HcCloseCause::Other) {
                counters.snapshot_req.fetch_add(1, Ordering::Release);
            }
        }
        self.next_attempt_ns = now + self.backoff.next_delay_ns();
    }
}

/// Drive the Hypercall connection until `stop` is set: dial with
/// backoff, pump, drain the poller handoff, keepalive, WS2 budget.
/// Only poll-infrastructure failure ends the loop.
#[allow(clippy::too_many_arguments)]
pub fn run<T: Transport, C: Capture>(
    conn: &mut HcConn<'_, T>,
    lanes: &mut Lanes<'_>,
    handoff: &mut Consumer<OptSummary, HANDOFF_RING_CAP>,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    stop: &StopFlag,
    status: &IngressStatus,
    counters: &HcCounters,
    capture: &mut C,
    mut connect: impl FnMut() -> Option<T>,
) -> RunResult {
    const TOKEN: mio::Token = mio::Token(0);
    // Set when the drain hit its step cap: poll without sleeping.
    let mut repoll_now = false;
    while !stop.load(Ordering::Relaxed) {
        // 1. Dial when due.
        let now = now_ns();
        if conn.transport.is_none() && now >= conn.next_attempt_ns {
            match connect() {
                Some(mut t) => {
                    if t.register(poll.registry(), TOKEN).is_err() {
                        conn.kill(now, status, counters);
                    } else {
                        conn.last_interest = Some(t.interest());
                        conn.drv.reset_for_reconnect(now);
                        conn.keepalive.reset();
                        conn.session_start_ns = now;
                        // COPY: the new transport (a `TlsTransport`, rustls'
                        // ClientConnection held inline, ~1 KB) moves from
                        // `connect` into its slot — once per reconnect, beside
                        // a TCP + TLS handshake that costs orders of magnitude
                        // more — rejected: a placement API on core-net's
                        // connect, for one move per reconnect.
                        conn.transport = Some(t);
                    }
                }
                None => conn.kill(now, status, counters),
            }
        }

        if poll
            .poll(events, Some(core_net::poll_timeout(repoll_now)))
            .is_err()
        {
            return RunResult::Error;
        }
        repoll_now = false;

        // 2. Readiness → pump.
        for ev in events.iter() {
            if ev.token() != TOKEN {
                continue;
            }
            let Some(t) = conn.transport.as_mut() else {
                continue;
            };
            match t.pump(ev) {
                Ok(s) => note_transport_ready(&mut conn.drv, s),
                Err(_e) => conn.kill(now_ns(), status, counters),
            }
        }

        // 3. Drain the connection (I-3, core_net::drain): again while a
        //    step's read stopped on a full rx — most Hypercall pushes are
        //    republications that publish nothing, and an rx-full run of
        //    them must not wait for a readiness edge that never comes —
        //    or it published or moved the state, at most DRAIN_STEP_CAP
        //    steps; a capped drain resumes after a poll that does not
        //    sleep, so the handoff and the heartbeat keep their turn.
        if let Some(t) = conn.transport.as_mut() {
            match drive_until_idle(t, &mut conn.drv, conn.host, lanes, status, counters, capture) {
                Drained::Idle => {}
                Drained::Capped => repoll_now = true,
                Drained::Closed | Drained::Failed(_) => conn.kill(now_ns(), status, counters),
            }
        }

        // 4. The poller's OptSummary rows → capture + opt lane.
        drain_handoff(handoff, lanes, status, capture);

        // 5. Capture flush cadence + the WS2 establishment budget.
        let flush_now = now_ns();
        capture.maybe_flush(flush_now);
        if conn.transport.is_some()
            && core_net::establishment_expired(
                flush_now,
                conn.session_start_ns,
                conn.drv.sub_count(),
                conn.drv.establish_budget_ns,
            )
        {
            status.note_session_err(
                core_metrics::ERR_SITE_ESTABLISH,
                core_metrics::io_kind_code(io::ErrorKind::TimedOut),
            );
            conn.kill(flush_now, status, counters);
        }

        // 6. Client heartbeat: a ClockSync every interval from the last
        //    one SENT, busy or not; reconnect after 60 s of silence.
        if conn.drv.state() == State::Steady {
            if let Some(t) = conn.transport.as_mut() {
                let now = now_ns();
                let act = if conn.drv.last_activity_ns == 0 {
                    conn.session_start_ns
                } else {
                    conn.drv.last_activity_ns
                };
                match conn.keepalive.poll_client_heartbeat(now, act, conn.session_start_ns) {
                    core_net::KeepaliveAction::SendPing => {
                        let ok = queue_clock_sync(&mut conn.drv, now).is_ok();
                        conn.keepalive.mark_ping_sent(now);
                        if !ok || flush_tx(t, &mut conn.drv).is_err() {
                            conn.kill(now, status, counters);
                        }
                    }
                    core_net::KeepaliveAction::Reconnect => conn.kill(now, status, counters),
                    core_net::KeepaliveAction::None => {}
                }
            }
        }

        // 7. Interest re-registration.
        if let Some(t) = conn.transport.as_mut() {
            let cur = t.interest();
            if conn.last_interest != Some(cur) {
                if t.reregister(poll.registry(), TOKEN).is_err() {
                    conn.kill(now_ns(), status, counters);
                } else {
                    conn.last_interest = Some(cur);
                }
            }
        }
    }
    RunResult::Stopped
}

#[cfg(test)]
mod tests;
