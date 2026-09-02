// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-bybit run-loop (WS9)
//!
//! Event-driven state machine for the Bybit v5 public WS. The venue
//! runs N single-class connections (spot + linear) on ONE thread
//! with ONE producer via [`run_multi`] — the Binance M1 multi-conn
//! shape, with the OKX-style batched subscribe + venue-literal ping.
//!
//! ```text
//! Connecting ──► NeedsWsWrite ──► AwaitingWsUpgrade ──► Steady ──┐
//!      ▲                                                          │
//!      └──────────────── Closed / Err ────────────────────────────┘
//! ```
//!
//! On entry to `Steady` the driver queues ONE batched
//! `{"op":"subscribe","args":[…]}` covering every configured
//! `(channel × symbol)` for the connection's class. The venue
//! answers with ONE all-or-nothing ack (crate docs) — WS2 semantics
//! at request granularity: a failed ack is FATAL until any ack has
//! ever succeeded on this driver (venue-blind boot refusal), then a
//! non-fatal drop (counter + `SubDrop` event + rate-limited WARN);
//! the WS2 establishment budget reaps sessions with nothing
//! confirmed either way.
//!
//! Everything after the handshake is zero-alloc: parsers slice the
//! rx buffer in place; the per-symbol BBO state is a fixed array;
//! the only copy is the 64-byte `Tick` moved into the ring.

use core::sync::atomic::AtomicBool;
use std::io;

use core_metrics::{IngressState, IngressStatus};
use core_net::{
    constant_time_eq, expected_accept, queue_masked_text_frame, read_server_handshake,
    sec_websocket_key_from_seed, write_client_handshake, ws_mask_from_counter, ws_read_frame,
    ws_unmask_in_place, ws_write_pong, HandshakeResult, IoBuf, Status, Transport, WsOpcode,
    WsReadResult,
};
use core_ring::Producer;
use core_time::{now_ns, FeedClock, NsTs};
use core_types::{
    Capture, ChannelEvent, ChannelId, Price, Qty, Tick, VenueId, EVENT_RING_SIZE, SYMBOL_ID_NONE,
    TICK_FLAG_STALE,
};

use crate::{
    classify, extract_topic_symbol, parse_orderbook1, parse_tickers, parse_trade_row,
    write_subscribe, BybitChannel, BybitMsgKind, BybitSymbolTable, BYBIT_MAX_SYMBOLS, PING_PAYLOAD,
};

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// Rx buffer: `orderbook.1` frames are ~250 B, `tickers` snapshots
/// ~700 B, `publicTrade` bursts a few KiB. 256 KiB absorbs a full
/// multi-symbol burst with a large margin (boot-time allocation).
pub const RX_BUF_SIZE: usize = 256 * 1024;

/// Tx buffer: handshake + one batched subscribe (~30 B/topic × 192
/// topics ≈ 5.8 KiB) + pings. 16 KiB keeps ≥2× margin.
pub const TX_BUF_SIZE: usize = 16 * 1024;

/// Tick-ring capacity. Must equal `engine::TICK_RING_SIZE` — the cli
/// const-asserts the equality when wiring lanes (8a §3.3 pattern).
pub const DEFAULT_TICK_RING_CAP: usize = 16_384;

/// Stack scratch for one rendered subscribe batch.
const SUBSCRIBE_SCRATCH: usize = 8 * 1024;

/// Max `publicTrade` rows seq-buffered per push (accounting only —
/// Bybit trades carry no venue seq; see the crate docs).
pub const MAX_TRADE_ROWS: usize = 16;

/// Minimum interval between emitted sub-drop WARN lines (the WS2
/// operator-terminal budget; evidence rides the SubDrop events).
const DROP_LOG_INTERVAL_NS: u64 = 1_000_000_000;

// ---------------------------------------------------------------
// State + outer result
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
    /// Peer closed.
    Closed,
}

/// How a run-loop invocation terminated (the multi-conn loop only
/// ever surfaces `Stopped`/`Error`; per-slot failures recycle the
/// slot — the Binance `run_multi` contract).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// External stop flag observed.
    Stopped,
    /// Poll-infrastructure failure.
    Error,
}

// ---------------------------------------------------------------
// Driver
// ---------------------------------------------------------------

/// Per-symbol BBO state for the `orderbook.1` snapshot/delta
/// protocol (qty 0 = side empty).
#[derive(Copy, Clone, Debug, Default)]
struct BboState {
    bid_px_1e6: i64,
    bid_qty_1e6: i64,
    ask_px_1e6: i64,
    ask_qty_1e6: i64,
}

/// Mutable per-connection state owned by the run-loop. Preallocated
/// at construction; never reallocates in steady state.
///
/// **Single-writer invariant.** `!Sync` via the marker field; the
/// cli moves every driver onto ONE venue thread at boot.
pub struct Driver {
    state: State,
    rx: IoBuf,
    tx: IoBuf,
    sec_key: [u8; 24],
    expected_accept_val: [u8; 28],
    /// Monotonic ns of the last inbound byte (the multi-conn
    /// keepalive reads it — the Binance MultiConn shape).
    pub last_activity_ns: NsTs,
    /// Monotonic counter feeding the outbound frame masks (public so
    /// the multi-conn keepalive can mask its ping — the BN shape).
    pub mask_counter: u64,

    /// Boot-built `SYMBOL → SymbolId` map for THIS connection.
    symbols: BybitSymbolTable,
    /// True on linear connections: subscribe `tickers.<SYM>` too.
    want_tickers: bool,
    /// Per-symbol BBO state, indexed by symbol-table row.
    bbo: [BboState; BYBIT_MAX_SYMBOLS],
    /// Set once the subscribe batch has been queued this session.
    subscribed: bool,
    /// Set when THIS session's subscribe ack came back success.
    subs_ok: bool,
    /// WS2: PROCESS-LIFETIME flag — true once ANY subscribe ack has
    /// ever succeeded on this driver. While false, a failed ack is
    /// fatal (boot venue-blind refusal); after, a non-fatal drop.
    /// Deliberately NOT cleared by [`Self::reset_for_reconnect`].
    subs_ever_acked: bool,
    /// WS2: establishment budget (ns from session start to the first
    /// confirmed subscribe) enforced by [`run_multi`].
    establish_budget_ns: u64,
    /// WS2 drop-log rate limiter (process-lifetime, operator budget).
    drop_log_last_ns: u64,
    /// Drops swallowed by the rate limit since the last line.
    drop_log_suppressed: u32,
    /// VT2: THIS connection's venue-clock offset estimator + staleness
    /// judge for `orderbook.1` (`cts`, else `ts`). One per connection
    /// by doctrine (the multi-conn lane owns one driver per socket);
    /// reset on reconnect; threshold = venue default or the operator's
    /// `--stale-after-ms bybit:<ms>` via [`Self::set_stale_after_ms`].
    feed_clock: FeedClock,
    /// `!Sync` marker — see struct doc.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate buffers (boot-time) and seed the handshake nonce.
    /// `want_tickers` = linear connection (mark/funding/OI channel).
    pub fn new(nonce_seed: u64, symbols: BybitSymbolTable, want_tickers: bool) -> Self {
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
            symbols,
            want_tickers,
            bbo: [BboState::default(); BYBIT_MAX_SYMBOLS],
            subscribed: false,
            subs_ok: false,
            subs_ever_acked: false,
            establish_budget_ns: core_net::ESTABLISH_BUDGET_NS,
            drop_log_last_ns: 0,
            drop_log_suppressed: 0,
            feed_clock: FeedClock::new(VenueId::Bybit.default_stale_after_ms()),
            _not_sync: ::core::marker::PhantomData,
        }
    }

    /// WS2: override the establishment budget (tests use millisecond
    /// budgets; production keeps the default).
    #[inline]
    pub fn set_establish_budget_ns(&mut self, ns: u64) {
        self.establish_budget_ns = ns;
    }

    /// VT2: override the staleness threshold (operator
    /// `--stale-after-ms bybit:<ms>`). Boot-time only — re-arms the
    /// estimator unlearned, exactly like a fresh connection.
    #[inline]
    pub fn set_stale_after_ms(&mut self, ms: u32) {
        self.feed_clock = FeedClock::new(ms);
    }

    /// VT2: this connection's smoothed `orderbook.1` feed delay (ms).
    #[inline]
    pub fn feed_delay_ema_ms(&self) -> u32 {
        self.feed_clock.delay_ema_ms()
    }

    /// Current state (metrics + tests).
    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    /// Force a state in tests.
    #[cfg(test)]
    pub(crate) fn set_state(&mut self, s: State) {
        self.state = s;
    }

    /// Confirmed-subscription count for the WS2 establishment
    /// predicate: this venue acks the whole request, so a confirmed
    /// session counts its full topic set.
    #[inline]
    pub fn sub_count(&self) -> usize {
        if self.subs_ok {
            self.symbols.len() * if self.want_tickers { 3 } else { 2 }
        } else {
            0
        }
    }

    /// Reset per-connection state for a reconnect. BBO states are
    /// connection-scoped (a fresh snapshot re-seeds them);
    /// `subs_ever_acked`, the establishment budget and the drop-log
    /// limiter are process-lifetime — untouched here.
    pub fn reset_for_reconnect(&mut self, nonce_seed: u64) {
        self.state = State::Connecting;
        self.rx.clear();
        self.tx.clear();
        self.sec_key = sec_websocket_key_from_seed(nonce_seed);
        self.expected_accept_val = expected_accept(&self.sec_key);
        self.last_activity_ns = 0;
        self.mask_counter = 0;
        self.bbo = [BboState::default(); BYBIT_MAX_SYMBOLS];
        self.subscribed = false;
        self.subs_ok = false;
        // VT2: a new connection is a new offset; the threshold stays.
        self.feed_clock.reset();
    }
}

// ---------------------------------------------------------------
// drive_one — single-tick state machine advance
// ---------------------------------------------------------------

/// Pump the transport once and advance the state machine. Zero-alloc
/// once the handshake has completed.
#[allow(clippy::too_many_arguments)]
pub fn drive_one<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    flush_tx(transport, drv)?;
    fill_rx(transport, drv)?;

    match drv.state {
        State::Connecting => {}
        State::NeedsWsWrite => {
            write_handshake_to_tx(drv, host, path)?;
            drv.state = State::AwaitingWsUpgrade;
        }
        State::AwaitingWsUpgrade => {
            advance_ws_upgrade(drv, status)?;
            if drv.state == State::Steady {
                queue_subscribe_all(drv)?;
            }
        }
        State::Steady => {
            drain_ws_frames(drv, producer, event_tx, event_mask, status, capture)?;
        }
        State::Closed => {}
    }

    flush_tx(transport, drv)?;
    Ok(())
}

/// Bump `Connecting → NeedsWsWrite` once the transport is TLS-ready.
#[inline]
pub fn note_transport_ready(drv: &mut Driver, status: Status) {
    match status {
        Status::Ready if drv.state == State::Connecting => {
            drv.state = State::NeedsWsWrite;
        }
        Status::Closed => {
            drv.state = State::Closed;
        }
        _ => {}
    }
}

// ---------------------------------------------------------------
// tx / handshake helpers (template shape — see ingress-okx)
// ---------------------------------------------------------------

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

fn fill_rx<T: Transport>(transport: &mut T, drv: &mut Driver) -> io::Result<()> {
    loop {
        if drv.rx.free_mut().is_empty() {
            break;
        }
        match transport.read(drv.rx.free_mut()) {
            Ok(0) => {
                drv.state = State::Closed;
                break;
            }
            Ok(n) => drv.rx.advance(n),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_handshake_to_tx(drv: &mut Driver, host: &[u8], path: &[u8]) -> io::Result<()> {
    let dst = drv.tx.free_mut();
    let n = write_client_handshake(dst, host, path, &drv.sec_key)
        .map_err(|_| io::Error::other("ws handshake buffer too small"))?;
    drv.tx.advance(n);
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
            // D7: publish Up exactly at the upgrade→Steady edge.
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

/// Queue the single batched subscribe op.
fn queue_subscribe_all(drv: &mut Driver) -> io::Result<()> {
    debug_assert!(
        !drv.subscribed,
        "subscribe batch must be queued exactly once"
    );
    if drv.symbols.is_empty() {
        return Err(io::Error::other("bybit: no symbols configured"));
    }
    let mut scratch = [0u8; SUBSCRIBE_SCRATCH];
    let len = write_subscribe(&mut scratch, &drv.symbols, drv.want_tickers)
        .ok_or_else(|| io::Error::other("bybit: subscribe scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..len])?;
    drv.subscribed = true;
    Ok(())
}

// ---------------------------------------------------------------
// Frame drain + dispatch
// ---------------------------------------------------------------

/// Per-push `publicTrade` scan result (phase-1 output; `Copy`).
#[derive(Copy, Clone)]
struct TradeScan {
    rows_parsed: u32,
    rows_rejected: u32,
}

/// Phase-1 dispatch outcome (two-phase borrow pattern — see
/// ingress-okx).
#[derive(Copy, Clone)]
enum Dispatch {
    /// Unparseable / unclassifiable — one rejection.
    Nothing,
    /// Pong — activity only.
    Quiet,
    /// The whole-request subscribe ack.
    SubAck { success: bool },
    /// `orderbook.1` push pre-parsed (BBO state applied in phase 2).
    Book {
        sym: u32,
        sym_idx: u8,
        frame: crate::BybitBookFrame,
    },
    /// `publicTrade` push scanned (+ events captured in phase 1).
    Trades { scan: TradeScan },
    /// LINEAR `tickers` push — events captured in phase 1.
    Tickers,
}

fn drain_ws_frames<C: Capture>(
    drv: &mut Driver,
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    loop {
        let read_result = ws_read_frame(drv.rx.filled());
        match read_result {
            WsReadResult::Incomplete => {
                // Oversize guard (fail-fast rather than livelock).
                if drv.rx.free_mut().is_empty() && !drv.rx.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ws frame exceeds rx buffer capacity",
                    ));
                }
                return Ok(());
            }
            WsReadResult::Malformed => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed ws frame",
                ));
            }
            WsReadResult::Frame { header, payload } => {
                let total = header.header_len as usize + header.payload_len as usize;
                debug_assert!(total <= drv.rx.filled().len());

                if header.masked {
                    let mask = header.mask;
                    let start = payload.start;
                    let end = payload.end;
                    ws_unmask_in_place(&mut drv.rx.filled_mut()[start..end], mask);
                }

                match header.opcode {
                    WsOpcode::Text | WsOpcode::Binary => {
                        handle_data_frame(
                            drv,
                            payload.start..payload.end,
                            producer,
                            event_tx,
                            event_mask,
                            status,
                            capture,
                        )?;
                    }
                    WsOpcode::Ping => {
                        let mask = ws_mask_from_counter(drv.mask_counter);
                        drv.mask_counter = drv.mask_counter.wrapping_add(1);
                        let start = payload.start;
                        let end = payload.end;
                        let plen = end - start;
                        let mut scratch = [0u8; 125];
                        debug_assert!(plen <= scratch.len());
                        scratch[..plen].copy_from_slice(&drv.rx.filled()[start..end]);
                        let dst = drv.tx.free_mut();
                        if let Ok(n) = ws_write_pong(dst, &scratch[..plen], mask) {
                            drv.tx.advance(n);
                        }
                    }
                    WsOpcode::Pong => {}
                    WsOpcode::Close => {
                        drv.state = State::Closed;
                    }
                    WsOpcode::Continuation => {
                        // Bybit does not fragment public pushes; drop
                        // rather than allocate a reassembly buffer.
                    }
                }

                // D5: every completed frame is inbound activity.
                let now = now_ns();
                drv.last_activity_ns = now;
                status.touch_activity(now);
                status.add_bytes(total as u64);
                drv.rx.consume(total);

                if drv.state == State::Closed {
                    return Ok(());
                }
            }
        }
    }
}

/// Walk the rows of one `publicTrade` push: rows sliced at successive
/// `"T":` markers; each row parses independently and captures a
/// `ChannelId::Trade` event (§6.5 — v0 = px ×1e6, v1 = signed qty
/// ×1e6, `venue_seq` = 0: Bybit trade ids are UUIDs, crate docs).
fn scan_trades<C: Capture>(payload: &[u8], sym: u32, capture: &mut C) -> TradeScan {
    const MARKER: &[u8] = b"\"T\":";
    let mut scan = TradeScan {
        rows_parsed: 0,
        rows_rejected: 0,
    };
    let mut at = 0usize;
    while let Some(off) = memchr::memmem::find(&payload[at..], MARKER) {
        let row_start = at + off;
        let next = memchr::memmem::find(&payload[row_start + MARKER.len()..], MARKER)
            .map(|o| row_start + MARKER.len() + o);
        let row_end = next.unwrap_or(payload.len());
        match parse_trade_row(&payload[row_start..row_end]) {
            Some(t) => {
                scan.rows_parsed += 1;
                let signed_qty = if t.side == 1 { -t.qty_1e6 } else { t.qty_1e6 };
                capture.event(&ChannelEvent::new(
                    now_ns(),
                    VenueId::Bybit,
                    ChannelId::Trade,
                    sym,
                    0,
                    t.ts_ns / 1_000_000,
                    t.px_1e6,
                    signed_qty,
                ));
            }
            None => {
                scan.rows_rejected += 1;
                capture.parse_reject(now_ns(), &payload[row_start..row_end]);
            }
        }
        at = row_end;
    }
    scan
}

fn handle_data_frame<C: Capture>(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    let reject_range = payload_range.clone();
    // Phase 1: immutable borrows — classify, resolve, pre-parse into
    // a Copy dispatch; capture hooks needing parsed values fire here.
    let dispatch: Dispatch = {
        let payload = &drv.rx.filled()[payload_range];
        capture.raw_frame(now_ns(), payload);
        match classify(payload) {
            BybitMsgKind::Pong => Dispatch::Quiet,
            BybitMsgKind::SubAck { success } => Dispatch::SubAck { success },
            BybitMsgKind::Data(BybitChannel::OrderbookL1) => {
                match extract_topic_symbol(payload, BybitChannel::OrderbookL1).and_then(|s| {
                    drv.symbols
                        .lookup(s)
                        .map(|sym| (sym, drv.symbols.index_of(sym)))
                }) {
                    Some((sym, Some(sym_idx))) => match parse_orderbook1(payload) {
                        Some(frame) => Dispatch::Book {
                            sym,
                            sym_idx: sym_idx as u8,
                            frame,
                        },
                        None => Dispatch::Nothing,
                    },
                    _ => Dispatch::Nothing,
                }
            }
            BybitMsgKind::Data(BybitChannel::PublicTrade) => {
                match extract_topic_symbol(payload, BybitChannel::PublicTrade)
                    .and_then(|s| drv.symbols.lookup(s))
                {
                    Some(sym) => Dispatch::Trades {
                        scan: scan_trades(payload, sym, capture),
                    },
                    None => Dispatch::Nothing,
                }
            }
            BybitMsgKind::Data(BybitChannel::Tickers) => {
                match extract_topic_symbol(payload, BybitChannel::Tickers)
                    .and_then(|s| drv.symbols.lookup(s))
                {
                    Some(sym) => match parse_tickers(payload) {
                        Some(f) => {
                            // §6.5 capture (crate-doc conventions):
                            // presence-gated per delta field group.
                            let ts_ms = f.ts_ns / 1_000_000;
                            if f.has_mark == 1 || f.has_index == 1 {
                                capture.event(&ChannelEvent::new(
                                    now_ns(),
                                    VenueId::Bybit,
                                    ChannelId::Mark,
                                    sym,
                                    0,
                                    ts_ms,
                                    f.mark_px_1e6,
                                    f.index_px_1e6,
                                ));
                            }
                            if f.has_funding == 1 {
                                let ev = ChannelEvent::new(
                                    now_ns(),
                                    VenueId::Bybit,
                                    ChannelId::Funding,
                                    sym,
                                    0,
                                    ts_ms,
                                    f.funding_rate_1e9,
                                    f.next_funding_ms as i64,
                                );
                                capture.event(&ev);
                                // WS10-A: onto the venue-event lane
                                // (capture stays first — §6.5
                                // capture-before-push law).
                                if event_mask & core_types::event_lane_bit(ChannelId::Funding) != 0
                                    && event_tx.try_push(ev).is_err()
                                {
                                    status.inc_event_ring_drops();
                                }
                            }
                            if f.has_oi == 1 {
                                capture.event(&ChannelEvent::new(
                                    now_ns(),
                                    VenueId::Bybit,
                                    ChannelId::Ticker,
                                    sym,
                                    0,
                                    ts_ms,
                                    0,
                                    f.open_interest_1e6,
                                ));
                            }
                            Dispatch::Tickers
                        }
                        None => Dispatch::Nothing,
                    },
                    None => Dispatch::Nothing,
                }
            }
            BybitMsgKind::Unknown => Dispatch::Nothing,
        }
    };

    // Phase 2: mutable applies.
    match dispatch {
        Dispatch::Nothing => {
            status.inc_parse_errors();
            capture.parse_reject(now_ns(), &drv.rx.filled()[reject_range]);
        }
        Dispatch::Quiet => {}
        Dispatch::SubAck { success } => {
            status.add_msgs(1);
            if success {
                drv.subs_ok = true;
                drv.subs_ever_acked = true;
            } else if !drv.subs_ever_acked {
                // BOOT fail-fast: the first-ever subscribe of this
                // connection's config was refused — venue-blind.
                status.note_session_err(
                    core_metrics::ERR_SITE_VENUE_ERROR,
                    core_metrics::io_kind_code(io::ErrorKind::InvalidData),
                );
                debug_assert!(false, "bybit subscribe refused at boot");
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bybit subscribe refused",
                ));
            } else {
                // WS2 NON-FATAL DROP (request granularity — this
                // venue acks all-or-nothing): count it, pair it, name
                // it; the establishment budget reaps the empty
                // session.
                status.inc_sub_drops();
                capture.event(&ChannelEvent::new(
                    now_ns(),
                    VenueId::Bybit,
                    ChannelId::SubDrop,
                    SYMBOL_ID_NONE,
                    0,
                    0,
                    0,
                    -1,
                ));
                log_sub_drop_rate_limited(drv);
            }
        }
        Dispatch::Book {
            sym,
            sym_idx,
            frame,
        } => {
            status.add_msgs(1);
            status.add_ticks(1);
            // VT2: judge EVERY stamped push (the offset learns from
            // one-sided deltas too); one parse-complete stamp serves
            // the judgement and the tick.
            let now = now_ns();
            let judged = drv.feed_clock.judge(frame.venue_time_ms, now);
            status.set_feed_delay_ema_ms(drv.feed_clock.delay_ema_ms());
            let st = &mut drv.bbo[sym_idx as usize];
            if frame.is_snapshot == 1 {
                *st = BboState::default();
            }
            if frame.has_bid == 1 {
                st.bid_px_1e6 = frame.bid_px_1e6;
                st.bid_qty_1e6 = frame.bid_qty_1e6;
            }
            if frame.has_ask == 1 {
                st.ask_px_1e6 = frame.ask_px_1e6;
                st.ask_qty_1e6 = frame.ask_qty_1e6;
            }
            // Emit only when both sides are live (a one-sided book is
            // not a BBO; the next delta completes it).
            if st.bid_qty_1e6 > 0 && st.ask_qty_1e6 > 0 {
                let tick = Tick::new_stamped(
                    now,
                    VenueId::Bybit,
                    sym,
                    (frame.update_id & 0xFFFF_FFFF) as u32,
                    Price::from_raw(st.bid_px_1e6),
                    Qty::from_raw(st.bid_qty_1e6),
                    Price::from_raw(st.ask_px_1e6),
                    Qty::from_raw(st.ask_qty_1e6),
                    frame.venue_time_ms,
                    (judged.stale as u8) * TICK_FLAG_STALE,
                );
                if judged.stale {
                    status.inc_stale_ticks();
                }
                // §6.5: capture BEFORE the push (ring-dropped ticks
                // still reach the replay log).
                capture.tick(&tick);
                if producer.try_push(tick).is_err() {
                    status.inc_ring_drops();
                }
            }
        }
        Dispatch::Trades { scan } => {
            status.add_msgs(scan.rows_parsed as u64);
            status.add_ticks(scan.rows_parsed as u64);
            let mut r = 0;
            while r < scan.rows_rejected {
                status.inc_parse_errors();
                r += 1;
            }
        }
        Dispatch::Tickers => {
            status.add_msgs(1);
            status.add_ticks(1);
        }
    }
    Ok(())
}

/// Rate-limited WS2 sub-drop WARN line (zero-alloc stderr; the
/// SubDrop capture event is the 1:1 evidence channel).
fn log_sub_drop_rate_limited(drv: &mut Driver) {
    let now = now_ns();
    if now.wrapping_sub(drv.drop_log_last_ns) < DROP_LOG_INTERVAL_NS {
        drv.drop_log_suppressed = drv.drop_log_suppressed.saturating_add(1);
        return;
    }
    let suppressed = drv.drop_log_suppressed;
    drv.drop_log_last_ns = now;
    drv.drop_log_suppressed = 0;

    let mut buf = [0u8; 128];
    let mut n = 0usize;
    let mut ok = true;
    let put = |buf: &mut [u8; 128], n: &mut usize, ok: &mut bool, src: &[u8]| {
        if *n + src.len() <= buf.len() {
            buf[*n..*n + src.len()].copy_from_slice(src);
            *n += src.len();
        } else {
            *ok = false;
        }
    };
    put(
        &mut buf,
        &mut n,
        &mut ok,
        b"WARN ingress-bybit: sub-drop (whole request) suppressed=",
    );
    let mut d = [0u8; 20];
    put(
        &mut buf,
        &mut n,
        &mut ok,
        fmt_u64(suppressed as u64, &mut d),
    );
    put(&mut buf, &mut n, &mut ok, b" ts_ns=");
    let mut d2 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, fmt_u64(now, &mut d2));
    put(&mut buf, &mut n, &mut ok, b"\n");
    debug_assert!(ok, "drop log scratch sized for the worst case");
    if ok {
        let mut err = std::io::stderr().lock();
        let _ = std::io::Write::write_all(&mut err, &buf[..n]);
    }
}

/// Render `v` as decimal ASCII into the tail of `scratch`.
#[inline]
fn fmt_u64(mut v: u64, scratch: &mut [u8; 20]) -> &[u8] {
    let mut i = scratch.len();
    loop {
        i -= 1;
        scratch[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &scratch[i..]
}

// ---------------------------------------------------------------
// Multi-connection loop (the Binance M1 shape)
// ---------------------------------------------------------------

/// Stop flag raised by external threads for graceful shutdown.
pub type StopFlag = AtomicBool;

/// One connection slot for [`run_multi`] (spot or linear).
pub struct BybitConn<T: Transport> {
    /// Live transport, `None` while disconnected.
    pub transport: Option<T>,
    /// Per-connection driver.
    pub drv: Driver,
    host: Vec<u8>,
    path: Vec<u8>,
    keepalive: core_net::Keepalive,
    backoff: core_net::Backoff,
    next_attempt_ns: NsTs,
    session_start_ns: NsTs,
    last_interest: Option<mio::Interest>,
}

impl<T: Transport> BybitConn<T> {
    /// New slot, initially disconnected (`next_attempt_ns` 0 = due
    /// immediately). Boot-time allocation for host/path is sanctioned
    /// (never touched on the hot path).
    pub fn new(
        drv: Driver,
        host: &[u8],
        path: &[u8],
        keepalive: core_net::Keepalive,
        backoff: core_net::Backoff,
    ) -> Self {
        Self {
            transport: None,
            drv,
            host: host.to_vec(),
            path: path.to_vec(),
            keepalive,
            backoff,
            next_attempt_ns: 0,
            session_start_ns: 0,
            last_interest: None,
        }
    }

    /// Tear down + schedule the next dial. A session that CONFIRMED
    /// subscriptions AND moved ticks resets the backoff (T1(b): the
    /// data-arm predicate is enforced by the caller passing
    /// `ticks_moved`).
    fn kill(&mut self, now: NsTs, status: &IngressStatus, ticks_moved: bool) {
        if self.transport.take().is_some() {
            status.inc_reconnects();
            if ticks_moved {
                self.backoff.reset();
            }
        }
        self.next_attempt_ns = now + self.backoff.next_delay_ns();
    }
}

/// Drive N single-class connections on one thread with one producer
/// until `stop` is set (the Binance `run_multi` contract: per-slot
/// failures recycle the slot; only poll-infrastructure failure ends
/// the loop). WS2: each slot enforces the establishment budget —
/// a session with nothing confirmed past the driver's budget is
/// torn down with escalating backoff.
// Doctrine: raw indices over `conns`, not iterator adapters — hot poll
// loop (CLAUDE.md hot-path rules; `i` is also the mio Token identity).
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
pub fn run_multi<T: Transport, C: Capture>(
    conns: &mut [BybitConn<T>],
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    stop: &StopFlag,
    status: &IngressStatus,
    capture: &mut C,
    mut connect: impl FnMut(usize) -> Option<T>,
) -> RunResult {
    while !stop.load(core::sync::atomic::Ordering::Relaxed) {
        // 1. Reconnect pass — one dial per iteration, oldest-due first.
        let now = now_ns();
        let mut due: Option<usize> = None;
        for i in 0..conns.len() {
            if conns[i].transport.is_none() && now >= conns[i].next_attempt_ns {
                let better = match due {
                    None => true,
                    Some(j) => conns[i].next_attempt_ns < conns[j].next_attempt_ns,
                };
                if better {
                    due = Some(i);
                }
            }
        }
        if let Some(i) = due {
            match connect(i) {
                Some(mut t) => {
                    if t.register(poll.registry(), mio::Token(i)).is_err() {
                        conns[i].kill(now, status, false);
                    } else {
                        conns[i].last_interest = Some(t.interest());
                        conns[i].drv.reset_for_reconnect(now);
                        conns[i].keepalive.reset();
                        conns[i].session_start_ns = now;
                        conns[i].transport = Some(t);
                    }
                }
                None => conns[i].kill(now, status, false),
            }
        }

        if poll
            .poll(events, Some(std::time::Duration::from_millis(50)))
            .is_err()
        {
            return RunResult::Error;
        }

        // 2. Readiness → per-slot pump.
        for ev in events.iter() {
            let i = ev.token().0;
            if i >= conns.len() {
                continue;
            }
            let c = &mut conns[i];
            let Some(t) = c.transport.as_mut() else {
                continue;
            };
            match t.pump(ev) {
                Ok(s) => note_transport_ready(&mut c.drv, s),
                Err(_e) => c.kill(now_ns(), status, false),
            }
        }

        // 3. Drain every live slot (bounded no-progress loop).
        for i in 0..conns.len() {
            let c = &mut conns[i];
            let Some(t) = c.transport.as_mut() else {
                continue;
            };
            loop {
                let n_before = producer.len();
                let state_before = c.drv.state();
                if drive_one(
                    t, &mut c.drv, &c.host, &c.path, producer, event_tx, event_mask, status,
                    capture,
                )
                .is_err()
                {
                    c.kill(now_ns(), status, false);
                    break;
                }
                if c.drv.state() == State::Closed {
                    let moved = c.drv.sub_count() > 0;
                    c.kill(now_ns(), status, moved);
                    break;
                }
                if producer.len() == n_before && c.drv.state() == state_before {
                    break;
                }
            }
        }

        // 4. §6.5 capture flush cadence + WS2 establishment budget
        //    (one clock read for both).
        let flush_now = now_ns();
        capture.maybe_flush(flush_now);
        for i in 0..conns.len() {
            let c = &mut conns[i];
            if c.transport.is_none() {
                continue;
            }
            if core_net::establishment_expired(
                flush_now,
                c.session_start_ns,
                c.drv.sub_count(),
                c.drv.establish_budget_ns,
            ) {
                status.note_session_err(
                    core_metrics::ERR_SITE_ESTABLISH,
                    core_metrics::io_kind_code(io::ErrorKind::TimedOut),
                );
                c.kill(flush_now, status, false);
            }
        }

        // 5. Keepalive per steady slot: the venue-literal
        //    `{"op":"ping"}` every interval.
        for i in 0..conns.len() {
            let c = &mut conns[i];
            if c.drv.state() != State::Steady {
                continue;
            }
            let Some(t) = c.transport.as_mut() else {
                continue;
            };
            let now = now_ns();
            let act = if c.drv.last_activity_ns == 0 {
                c.session_start_ns
            } else {
                c.drv.last_activity_ns
            };
            match c.keepalive.poll(now, act) {
                core_net::KeepaliveAction::SendPing => {
                    let ping_ok = queue_masked_text_frame(
                        &mut c.drv.tx,
                        &mut c.drv.mask_counter,
                        PING_PAYLOAD,
                    )
                    .is_ok();
                    c.keepalive.mark_ping_sent(now);
                    if !ping_ok || flush_tx(t, &mut c.drv).is_err() {
                        c.kill(now, status, false);
                    }
                }
                core_net::KeepaliveAction::Reconnect => {
                    let moved = c.drv.sub_count() > 0;
                    c.kill(now, status, moved);
                }
                core_net::KeepaliveAction::None => {}
            }
        }

        // 6. Interest re-registration per live slot.
        for i in 0..conns.len() {
            let c = &mut conns[i];
            let Some(t) = c.transport.as_mut() else {
                continue;
            };
            let cur = t.interest();
            if c.last_interest != Some(cur) {
                if t.reregister(poll.registry(), mio::Token(i)).is_err() {
                    c.kill(now_ns(), status, false);
                } else {
                    c.last_interest = Some(cur);
                }
            }
        }
    }
    RunResult::Stopped
}

// ---------------------------------------------------------------
// Tests — TestTransport-driven
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_net::TestTransport;
    use core_ring::Ring;
    use core_types::NullCapture;

    /// Venue-namespaced test syms (venue byte 6 = Bybit).
    const SYM_BTC: u32 = (6 << 24) | 1;
    const SYM_ETH: u32 = (6 << 24) | 2;

    fn test_symbols() -> BybitSymbolTable {
        let mut t = BybitSymbolTable::new();
        t.insert(b"BTCUSDT", SYM_BTC).unwrap();
        t.insert(b"ETHUSDT", SYM_ETH).unwrap();
        t
    }

    fn steady_driver(want_tickers: bool) -> Driver {
        let mut d = Driver::new(7, test_symbols(), want_tickers);
        d.set_state(State::Steady);
        d.subscribed = true;
        d
    }

    fn ring_pair() -> (
        Producer<Tick, DEFAULT_TICK_RING_CAP>,
        core_ring::Consumer<Tick, DEFAULT_TICK_RING_CAP>,
    ) {
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        ring.split()
    }

    fn event_ring_pair() -> (
        Producer<ChannelEvent, EVENT_RING_SIZE>,
        core_ring::Consumer<ChannelEvent, EVENT_RING_SIZE>,
    ) {
        Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split()
    }

    /// WS10-A shim: legacy tests drive with a fresh throwaway event
    /// lane (mask = FUNDING; consumer dropped — pushes vanish). A
    /// local item shadows the glob-imported `super::drive_one`, so
    /// every pre-WS10 call site stays byte-identical. The dedicated
    /// event-lane test below calls `super::drive_one` directly.
    #[allow(clippy::too_many_arguments)]
    fn drive_one<T: Transport, C: Capture>(
        transport: &mut T,
        drv: &mut Driver,
        host: &[u8],
        path: &[u8],
        producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
        status: &IngressStatus,
        capture: &mut C,
    ) -> io::Result<()> {
        let (mut etx, _erx) = event_ring_pair();
        super::drive_one(
            transport,
            drv,
            host,
            path,
            producer,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            status,
            capture,
        )
    }

    fn ws_text_frame(payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(4 + payload.len());
        f.push(0x81);
        if payload.len() <= 125 {
            f.push(payload.len() as u8);
        } else {
            assert!(payload.len() <= u16::MAX as usize);
            f.push(126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        f.extend_from_slice(payload);
        f
    }

    /// Records every ChannelEvent (capture-site pinning).
    #[derive(Default)]
    struct EventRecCap {
        events: Vec<ChannelEvent>,
        ticks: u32,
        rejects: u32,
    }
    impl Capture for EventRecCap {
        fn tick(&mut self, _t: &Tick) {
            self.ticks += 1;
        }
        fn event(&mut self, e: &ChannelEvent) {
            self.events.push(*e);
        }
        fn parse_reject(&mut self, _ts: u64, _p: &[u8]) {
            self.rejects += 1;
        }
    }

    #[test]
    fn handshake_completes_and_batched_subscribe_is_emitted() {
        let mut t = TestTransport::with_capacity(65536);
        let mut d = Driver::new(42, test_symbols(), true);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(
            &mut t,
            &mut d,
            b"stream.bybit.com",
            b"/v5/public/linear",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        let mut scratch = vec![0u8; 65536];
        let _ = t.drain_outgoing(&mut scratch);

        let key = core_net::sec_websocket_key_from_seed(42);
        let accept = core_net::expected_accept(&key);
        let mut resp: Vec<u8> = Vec::new();
        resp.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
        resp.extend_from_slice(b"Upgrade: websocket\r\nConnection: Upgrade\r\n");
        resp.extend_from_slice(b"Sec-WebSocket-Accept: ");
        resp.extend_from_slice(&accept);
        resp.extend_from_slice(b"\r\n\r\n");
        t.inject_incoming(&resp);

        drive_one(
            &mut t,
            &mut d,
            b"stream.bybit.com",
            b"/v5/public/linear",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(d.state(), State::Steady);
        assert!(d.subscribed);
        assert_eq!(status.state(), IngressState::Up);
        let n = t.drain_outgoing(&mut scratch);
        // Client frames are masked; just check the op landed by
        // unmasking manually.
        let mut body = Vec::new();
        let mut buf = &scratch[..n];
        while buf.len() >= 2 {
            let masked = buf[1] & 0x80 != 0;
            let mut len = (buf[1] & 0x7F) as usize;
            let mut at = 2;
            if len == 126 {
                len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                at = 4;
            }
            assert!(masked);
            let mask = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
            at += 4;
            for i in 0..len {
                body.push(buf[at + i] ^ mask[i & 3]);
            }
            buf = &buf[at + len..];
        }
        assert!(memchr::memmem::find(&body, b"\"op\":\"subscribe\"").is_some());
        assert!(memchr::memmem::find(&body, b"\"orderbook.1.BTCUSDT\"").is_some());
        assert!(memchr::memmem::find(&body, b"\"tickers.ETHUSDT\"").is_some());
    }

    #[test]
    fn book_snapshot_then_delta_emits_ticks_with_merged_state() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let mut cap = EventRecCap::default();

        let snap = br#"{"topic":"orderbook.1.BTCUSDT","type":"snapshot","ts":1,"data":{"s":"BTCUSDT","b":[["50005.12","1.5"]],"a":[["50006.34","2.0"]],"u":100,"seq":1}}"#;
        let delta = br#"{"topic":"orderbook.1.BTCUSDT","type":"delta","ts":2,"data":{"s":"BTCUSDT","b":[["50005.50","1.0"]],"a":[],"u":101,"seq":2}}"#;
        t.inject_incoming(&ws_text_frame(snap));
        t.inject_incoming(&ws_text_frame(delta));
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        let t1 = cons.try_pop().expect("snapshot tick");
        assert_eq!(t1.sym, SYM_BTC);
        assert_eq!(t1.venue, VenueId::Bybit as u8);
        assert_eq!(t1.bid_px.raw(), 50_005_120_000);
        assert_eq!(t1.ask_px.raw(), 50_006_340_000);
        assert_eq!(t1.venue_seq, 100);
        let t2 = cons
            .try_pop()
            .expect("delta tick — ask side carried forward");
        assert_eq!(t2.bid_px.raw(), 50_005_500_000);
        assert_eq!(
            t2.ask_px.raw(),
            50_006_340_000,
            "unchanged side survives the delta"
        );
        assert_eq!(t2.venue_seq, 101);
        assert_eq!(cap.ticks, 2);
        assert_eq!(status.ticks_total(), 2);
        assert_eq!(status.parse_errors_total(), 0);
    }

    /// VT2 helper: one two-sided `orderbook.1` snapshot stamped `cts`
    /// through the steady driver; returns the tick it produced.
    fn push_snapshot_with_cts(
        t: &mut TestTransport,
        d: &mut Driver,
        prod: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
        cons: &mut core_ring::Consumer<Tick, DEFAULT_TICK_RING_CAP>,
        status: &IngressStatus,
        cts_ms: u64,
        u: u64,
    ) -> Tick {
        let s = format!(
            r#"{{"topic":"orderbook.1.BTCUSDT","type":"snapshot","ts":{},"data":{{"s":"BTCUSDT","b":[["50005.12","1.5"]],"a":[["50006.34","2.0"]],"u":{u},"seq":{u}}},"cts":{cts_ms}}}"#,
            cts_ms + 2
        );
        t.inject_incoming(&ws_text_frame(s.as_bytes()));
        drive_one(t, d, b"h", b"/", prod, status, &mut NullCapture).unwrap();
        cons.try_pop().expect("two-sided snapshot must produce a tick")
    }

    #[test]
    fn book_ticks_carry_cts_and_the_stale_judgement() {
        // VT2: first stamped push = the offset (fresh); a push whose
        // matching-engine time is 5 s older is stale at bybit 500 ms
        // (flag + counter); a later stamp is fresh again.
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let t0: u64 = 1_755_216_000_000;

        let fresh = push_snapshot_with_cts(&mut t, &mut d, &mut prod, &mut cons, &status, t0, 1);
        assert_eq!(fresh.venue_time_ms, t0, "cts wins over the ts envelope (+2)");
        assert!(!fresh.is_stale());
        assert_eq!(status.stale_ticks_total(), 0);

        let stale = push_snapshot_with_cts(&mut t, &mut d, &mut prod, &mut cons, &status, t0 - 5_000, 2);
        assert!(stale.is_stale());
        assert_eq!(stale.flags, TICK_FLAG_STALE);
        assert_eq!(stale.venue_time_ms, t0 - 5_000);
        assert_eq!(status.stale_ticks_total(), 1);
        assert!(status.feed_delay_ema_ms() > 0);

        let again = push_snapshot_with_cts(&mut t, &mut d, &mut prod, &mut cons, &status, t0 + 10, 3);
        assert!(!again.is_stale());
        assert_eq!(status.stale_ticks_total(), 1);
        assert_eq!(status.ticks_total(), 3);
    }

    #[test]
    fn stale_threshold_override_and_reconnect_reset_apply() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        d.set_stale_after_ms(10_000);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let t0: u64 = 1_755_216_000_000;
        let _ = push_snapshot_with_cts(&mut t, &mut d, &mut prod, &mut cons, &status, t0, 1);
        let five_s = push_snapshot_with_cts(&mut t, &mut d, &mut prod, &mut cons, &status, t0 - 5_000, 2);
        assert!(!five_s.is_stale(), "5 s is under a 10 s threshold");
        assert_eq!(status.stale_ticks_total(), 0);

        d.reset_for_reconnect(9);
        d.set_state(State::Steady);
        d.subscribed = true;
        let after = push_snapshot_with_cts(&mut t, &mut d, &mut prod, &mut cons, &status, t0 - 60_000, 3);
        assert!(!after.is_stale(), "a reconnect starts a fresh offset");
        assert_eq!(after.venue_time_ms, t0 - 60_000);
    }

    #[test]
    fn one_sided_book_never_emits() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        // Snapshot with only a bid — no tick until the ask arrives.
        let snap = br#"{"topic":"orderbook.1.BTCUSDT","type":"snapshot","ts":1,"data":{"s":"BTCUSDT","b":[["50005.12","1.5"]],"a":[],"u":100,"seq":1}}"#;
        t.inject_incoming(&ws_text_frame(snap));
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert!(cons.try_pop().is_none(), "one-sided book is not a BBO");
        let ask = br#"{"topic":"orderbook.1.BTCUSDT","type":"delta","ts":2,"data":{"s":"BTCUSDT","b":[],"a":[["50006.00","1.0"]],"u":101,"seq":2}}"#;
        t.inject_incoming(&ws_text_frame(ask));
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert!(cons.try_pop().is_some(), "both sides live now");
    }

    #[test]
    fn trades_capture_events_per_row() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = EventRecCap::default();

        let trades = br#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":3,"data":[{"T":1672304486865,"s":"BTCUSDT","S":"Buy","v":"0.001","p":"16578.50","i":"a"},{"T":1672304486866,"s":"BTCUSDT","S":"Sell","v":"0.5","p":"16578.00","i":"b"}]}"#;
        t.inject_incoming(&ws_text_frame(trades));
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        let trades_ev: Vec<_> = cap
            .events
            .iter()
            .filter(|e| e.channel == ChannelId::Trade as u8)
            .collect();
        assert_eq!(trades_ev.len(), 2);
        assert_eq!(trades_ev[0].sym, SYM_BTC);
        assert_eq!(trades_ev[0].v0, 16_578_500_000);
        assert_eq!(trades_ev[0].v1, 1_000, "buy = positive qty");
        assert_eq!(trades_ev[1].v1, -500_000, "sell = negated qty");
        assert_eq!(status.msgs_total(), 2);
        assert_eq!(status.ticks_total(), 2);
    }

    #[test]
    fn linear_tickers_emit_mark_funding_and_oi_events() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(true);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = EventRecCap::default();

        let snap = br#"{"topic":"tickers.BTCUSDT","type":"snapshot","cs":1,"ts":1673272861686,"data":{"symbol":"BTCUSDT","markPrice":"17217.33","indexPrice":"17227.36","openInterest":"68744.761","fundingRate":"-0.000212","nextFundingTime":"1673280000000"}}"#;
        t.inject_incoming(&ws_text_frame(snap));
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        let mark = cap
            .events
            .iter()
            .find(|e| e.channel == ChannelId::Mark as u8)
            .unwrap();
        assert_eq!(mark.v0, 17_217_330_000);
        assert_eq!(mark.v1, 17_227_360_000, "index rides v1 (the WS5 shape)");
        let fu = cap
            .events
            .iter()
            .find(|e| e.channel == ChannelId::Funding as u8)
            .unwrap();
        assert_eq!(fu.v0, -212_000);
        assert_eq!(fu.v1, 1_673_280_000_000);
        let oi = cap
            .events
            .iter()
            .find(|e| e.channel == ChannelId::Ticker as u8)
            .unwrap();
        assert_eq!(oi.v0, 0);
        assert_eq!(oi.v1, 68_744_761_000);

        // A mark-only delta emits ONE event.
        cap.events.clear();
        let delta = br#"{"topic":"tickers.BTCUSDT","type":"delta","cs":2,"ts":1673272862690,"data":{"symbol":"BTCUSDT","markPrice":"17218.01"}}"#;
        t.inject_incoming(&ws_text_frame(delta));
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.events.len(), 1, "delta with mark only");
        assert_eq!(cap.events[0].channel, ChannelId::Mark as u8);
    }

    /// WS10-A: the linear tickers' Funding event reaches the venue-
    /// event lane — Mark and OI stay capture-only (mask gates per
    /// channel). Mask-0 and ring-full behavior are pinned crate-
    /// uniformly in ingress-okx.
    #[test]
    fn funding_event_reaches_the_event_lane_mark_oi_stay_capture_only() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(true);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let (mut etx, mut erx) = event_ring_pair();
        let mut cap = EventRecCap::default();

        let snap = br#"{"topic":"tickers.BTCUSDT","type":"snapshot","cs":1,"ts":1673272861686,"data":{"symbol":"BTCUSDT","markPrice":"17217.33","indexPrice":"17227.36","openInterest":"68744.761","fundingRate":"-0.000212","nextFundingTime":"1673280000000"}}"#;
        t.inject_incoming(&ws_text_frame(snap));
        super::drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &status,
            &mut cap,
        )
        .unwrap();

        assert_eq!(cap.events.len(), 3, "capture: Mark + Funding + OI");
        let ev = erx.try_pop().expect("funding event on the lane");
        assert_eq!(ev.channel, ChannelId::Funding as u8);
        assert_eq!(ev.v0, -212_000, "rate ×1e9");
        assert_eq!(ev.v1, 1_673_280_000_000, "next funding ms");
        assert!(
            erx.try_pop().is_none(),
            "Mark/OI are NOT on the lane (mask gates per channel)"
        );
        assert_eq!(status.event_ring_drops_total(), 0);
    }

    #[test]
    fn sub_ack_success_confirms_and_failure_is_fatal_at_boot() {
        // Success arms the WS2 discriminator + the sub count.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(true);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let ok = br#"{"success":true,"ret_msg":"subscribe","conn_id":"x","op":"subscribe"}"#;
        t.inject_incoming(&ws_text_frame(ok));
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert!(d.subs_ever_acked);
        assert_eq!(d.sub_count(), 6, "2 symbols × 3 topics on a linear conn");

        // Boot failure path (fresh driver, release semantics).
        if cfg!(debug_assertions) {
            return;
        }
        let mut t2 = TestTransport::with_capacity(8192);
        let mut d2 = steady_driver(false);
        let fail = br#"{"success":false,"ret_msg":"error:handler not found","conn_id":"x","op":"subscribe"}"#;
        t2.inject_incoming(&ws_text_frame(fail));
        let e = drive_one(
            &mut t2,
            &mut d2,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn sub_ack_failure_after_first_success_is_nonfatal_drop() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        d.subs_ever_acked = true; // a session ever succeeded
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = EventRecCap::default();

        let fail =
            br#"{"success":false,"ret_msg":"error:rate limit","conn_id":"x","op":"subscribe"}"#;
        t.inject_incoming(&ws_text_frame(fail));
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap)
            .expect("post-first-success refusal must be non-fatal");
        assert_eq!(status.sub_drops_total(), 1);
        assert_eq!(d.sub_count(), 0, "nothing confirmed — the budget will reap");
        let drop = cap
            .events
            .iter()
            .find(|e| e.channel == ChannelId::SubDrop as u8)
            .expect("SubDrop event captured");
        assert_eq!(drop.sym, SYMBOL_ID_NONE);
    }

    #[test]
    fn foreign_topic_and_unknown_symbol_count_rejects() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let foreign = br#"{"topic":"kline.1.BTCUSDT","data":[]}"#;
        let unknown = br#"{"topic":"orderbook.1.DOGEUSDT","type":"snapshot","ts":1,"data":{"s":"DOGEUSDT","b":[["1","1"]],"a":[["2","1"]],"u":1}}"#;
        t.inject_incoming(&ws_text_frame(foreign));
        t.inject_incoming(&ws_text_frame(unknown));
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(status.parse_errors_total(), 2);
        assert_eq!(status.msgs_total(), 0);
    }

    #[test]
    fn run_multi_establishment_budget_reaps_unconfirmed_slots() {
        // WS2: a slot whose subscribe never confirms is torn down at
        // the budget — with a fake connect that always fails after,
        // the loop keeps running (per-slot recycling) until stop.
        let mut d = Driver::new(1, test_symbols(), false);
        d.set_establish_budget_ns(50_000_000); // 50 ms
        let mut conns = vec![BybitConn::new(
            d,
            b"h",
            b"/",
            core_net::Keepalive::new(core_net::KeepaliveCfg {
                ping_interval_ns: u64::MAX / 4,
                idle_timeout_ns: u64::MAX / 2,
            }),
            core_net::Backoff::new(1_000_000, 10_000_000, 1),
        )];
        let (mut prod, _cons) = ring_pair();
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let stop = StopFlag::new(false);
        let status = IngressStatus::new();

        // Serve exactly one TestTransport (never upgrades), then stop
        // dialing; a scoped watchdog raises stop after ~250 ms.
        let mut served = false;
        let res = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(250));
                stop.store(true, core::sync::atomic::Ordering::Relaxed);
            });
            let (mut etx, _erx) = event_ring_pair();
            run_multi(
                &mut conns,
                &mut prod,
                &mut etx,
                core_types::EVENT_LANE_FUNDING,
                &mut poll,
                &mut events,
                &stop,
                &status,
                &mut NullCapture,
                |_i| {
                    if served {
                        None
                    } else {
                        served = true;
                        Some(TestTransport::with_capacity(4096))
                    }
                },
            )
        });
        assert_eq!(res, RunResult::Stopped);
        assert!(
            conns[0].transport.is_none(),
            "unconfirmed slot was reaped by the establishment budget"
        );
        let snap = status.take_last_err();
        assert_eq!(snap.site, core_metrics::ERR_SITE_ESTABLISH);
        assert!(status.reconnects_total() >= 1);
    }
}
