// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Binance ingress run-loop
//!
//! Event-driven state machine that drives the Phase 1a
//! [`crate::parse_book_ticker`] codec against a [`core_net::Transport`].
//! Monomorphised on the transport so the compiler inlines through every
//! call; no `dyn Trait` anywhere on the hot path.
//!
//! Mirrors the shape of `ingress-polymarket::run_loop` deliberately — the
//! two loops are structurally identical so that lifting out common glue
//! in a later phase is a mechanical refactor. Differences:
//!
//! * **One WS connection per symbol.** Binance's `/ws/{symbol}@bookTicker`
//!   endpoint streams a single instrument; no combined-stream envelope to
//!   decode. `Driver` therefore carries a fixed [`SymbolId`] rather than
//!   a lookup table.
//! * **Text payload shape** is
//!   `{"u":…,"s":"BTCUSDT","b":"…","B":"…","a":"…","A":"…"}` — parsed by
//!   [`crate::parse_book_ticker`] into a [`crate::BookTickerFrame`] and
//!   projected onto [`core_types::Tick`].
//! * **Client frames are masked per RFC 6455 §5.3** — identical to
//!   Polymarket.
//!
//! All steady-state work after the WS upgrade is zero-alloc.
//!
//! Phase 8a wires in observability + liveness (D4/D5/D6/D7): the loop
//! publishes into a shared [`core_metrics::IngressStatus`] slot
//! (relaxed atomics only — still zero-alloc) and drives a
//! [`core_net::Keepalive`] that emits proactive WS protocol pings and
//! forces a reconnect when the connection goes silent.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_net::{
    constant_time_eq, expected_accept, read_server_handshake, sec_websocket_key_from_seed,
    write_client_handshake, ws_mask_from_counter, ws_read_frame, ws_unmask_in_place, ws_write_ping,
    ws_write_pong, HandshakeResult, Status, Transport, WsOpcode, WsReadResult,
};
use core_ring::Producer;
use core_time::{now_ns, FeedClock};
use core_types::{
    Capture, NsTs, OptSummary, Price, Qty, SymbolId, Tick, EVENT_RING_SIZE, OPT_RING_SIZE,
    TICK_FLAG_STALE,
};

use crate::parse_book_ticker;

// ---------------------------------------------------------------
// Configuration + sizing
// ---------------------------------------------------------------

/// Size of the rx byte buffer. Binance `@bookTicker` frames are ~140 B
/// each; 64 KiB accommodates huge bursts without ever reallocating.
pub const RX_BUF_SIZE: usize = 64 * 1024;

/// Rx sizing for the M2.4 eapi combined slot: 64 option tickers
/// (~1 KiB each) + index pushes can burst together; 512 KiB gives
/// ≥8× margin over a full-chain simultaneous burst (boot alloc).
pub const EAPI_RX_BUF_SIZE: usize = 512 * 1024;

/// Size of the tx byte buffer. Only used for the opening handshake + pong
/// replies, so 4 KiB is generous.
pub const TX_BUF_SIZE: usize = 4 * 1024;

/// Default Binance tick-ring capacity. Must be a power of two (the
/// ring enforces this at construction); 8192 is plenty for a single
/// symbol at Binance cadence.
pub const DEFAULT_TICK_RING_CAP: usize = 16_384;

// ---------------------------------------------------------------
// Buffers — cursor-draining byte windows, zero-alloc after construction
// ---------------------------------------------------------------

/// Fixed-size byte window with a **cursor pair** (head, tail).
/// O(1) `consume` — the residual compaction only runs in
/// [`free_mut`] when the tail hits the buffer end. See
/// ingress-polymarket for the rationale.
struct IoBuf {
    data: Box<[u8]>,
    head: usize,
    tail: usize,
}

impl IoBuf {
    fn with_capacity(cap: usize) -> Self {
        Self {
            data: vec![0u8; cap].into_boxed_slice(),
            head: 0,
            tail: 0,
        }
    }

    #[inline]
    fn filled(&self) -> &[u8] {
        &self.data[self.head..self.tail]
    }

    #[inline]
    fn len(&self) -> usize {
        self.tail - self.head
    }

    #[inline]
    fn filled_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.head..self.tail]
    }

    #[inline]
    fn free_mut(&mut self) -> &mut [u8] {
        if self.tail == self.data.len() && self.head > 0 {
            self.data.copy_within(self.head..self.tail, 0);
            self.tail -= self.head;
            self.head = 0;
        }
        &mut self.data[self.tail..]
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        debug_assert!(self.tail + n <= self.data.len());
        self.tail += n;
    }

    #[inline]
    fn consume(&mut self, n: usize) {
        debug_assert!(self.head + n <= self.tail);
        self.head += n;
        if self.head == self.tail {
            self.head = 0;
            self.tail = 0;
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
    }
}

// ---------------------------------------------------------------
// State
// ---------------------------------------------------------------

/// Run-loop state. Matches the shape used by `ingress-polymarket`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// TLS handshake in progress.
    Connecting,
    /// TLS ready; WebSocket opening request not yet sent.
    NeedsWsWrite,
    /// WebSocket opening request sent; awaiting `101` response.
    AwaitingWsUpgrade,
    /// Upgraded — frames can flow.
    Steady,
    /// Peer closed.
    Closed,
}

/// How a run-loop iteration terminated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// External stop signal was observed.
    Stopped,
    /// Peer closed the connection. Caller reconnects.
    Disconnected,
    /// Fatal transport error.
    Error,
    /// No inbound bytes within the keepalive idle budget (D5) — caller
    /// reconnects.
    IdleTimeout,
}

/// Mutable per-connection state owned by the run-loop. Preallocated at
/// construction; never reallocates in steady state.
///
/// **Single-writer invariant.** `Driver: !Sync` via the `_not_sync`
/// marker field — `&Driver` cannot be shared across threads. The
/// cli spawns one per ingress thread.
/// What a Binance connection slot carries (M2.4). The venue's lanes
/// share ONE thread + ONE tick producer (single-writer law); the lane
/// tag drives per-slot parse dispatch — monomorphic match, no `dyn`.
// Doctrine: the `Eapi` payload is deliberately inline — one slot per
// connection, preallocated at boot; `Box` is forbidden (CLAUDE.md
// zero-alloc rules), and the size delta buys pointer-chase-free access.
#[allow(clippy::large_enum_variant)]
pub enum StreamLane {
    /// `/ws/<symbol>@bookTicker` — the M1c spot/usdm lane (`sym`
    /// pinned on the driver).
    BookTicker,
    /// WS5: `/ws/<symbol>@markPrice` — USDS-M mark/index/funding
    /// stream (`sym` pinned on the driver, the bookTicker shape).
    /// Capture-only: `ChannelId::Mark` + (perps) `ChannelId::Funding`
    /// events; nothing reaches the engine ring until the WS10
    /// funding carrier lands.
    MarkPrice,
    /// M2.4 eapi combined options stream (`<sym>@ticker` × N +
    /// `<uly>@index`): BBO → `Tick`, mark/IV/greeks → `OptSummary`.
    Eapi(crate::eapi::EapiLane),
}

/// Mutable per-connection state owned by the run-loop. Preallocated at
/// construction; never reallocates in steady state.
///
/// **Single-writer invariant.** `Driver: !Sync` via the `_not_sync`
/// marker field — `&Driver` cannot be shared across threads. The
/// cli spawns one per ingress thread (or N per MultiConn thread —
/// still one thread, one producer).
pub struct Driver {
    state: State,
    rx: IoBuf,
    tx: IoBuf,
    sec_key: [u8; 24],
    expected_accept_val: [u8; 28],
    last_activity_ns: NsTs,
    /// Monotonic counter feeding [`ws_mask_from_counter`] for every
    /// outbound frame.
    mask_counter: u64,
    /// Symbol id pinned to this connection. Binance's
    /// `/ws/{symbol}@bookTicker` endpoint is single-symbol, so we resolve
    /// once at boot and avoid the per-tick lookup table Polymarket needs.
    /// (Unused sentinel 0 on the eapi lane — its syms come from the
    /// lane table.)
    sym: SymbolId,
    /// Per-slot parse dispatch (M2.4).
    lane: StreamLane,
    /// VT2: THIS connection's venue-clock offset estimator + staleness
    /// judge for `bookTicker` (USDS-M `T`/`E`; spot pushes carry no
    /// stamp and stay "unknown, never stale" until the aggTrade
    /// sentinel step). One per connection by doctrine — the multi-conn
    /// lane owns one driver per socket; reset on reconnect; threshold =
    /// venue default or `--stale-after-ms bn:<ms>` via
    /// [`Self::set_stale_after_ms`].
    feed_clock: FeedClock,
    /// `!Sync` marker.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate rx/tx buffers and seed the opening-handshake nonce.
    pub fn new(nonce_seed: u64, sym: SymbolId) -> Self {
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
            sym,
            lane: StreamLane::BookTicker,
            feed_clock: FeedClock::new(core_types::VenueId::Binance.default_stale_after_ms()),
            _not_sync: ::core::marker::PhantomData,
        }
    }

    /// VT2: override the staleness threshold (operator
    /// `--stale-after-ms bn:<ms>`). Boot-time only — re-arms the
    /// estimator unlearned, exactly like a fresh connection.
    #[inline]
    pub fn set_stale_after_ms(&mut self, ms: u32) {
        self.feed_clock = FeedClock::new(ms);
    }

    /// VT2: this connection's smoothed `bookTicker` feed delay (ms).
    #[inline]
    pub fn feed_delay_ema_ms(&self) -> u32 {
        self.feed_clock.delay_ema_ms()
    }

    /// WS5: a `@markPrice` single-stream slot (USDS-M mark/index/
    /// funding). Same sizing as bookTicker (~200 B frames at 1–3 s
    /// cadence).
    pub fn new_mark_price(nonce_seed: u64, sym: SymbolId) -> Self {
        let mut d = Self::new(nonce_seed, sym);
        d.lane = StreamLane::MarkPrice;
        d
    }

    /// M2.4: an eapi combined-stream slot (options tickers + index
    /// pushes). RX is sized up: one combined frame is small (~1 KiB)
    /// but 64 ticker streams burst together.
    pub fn new_eapi(nonce_seed: u64, lane: crate::eapi::EapiLane) -> Self {
        let sec_key = sec_websocket_key_from_seed(nonce_seed);
        let accept = expected_accept(&sec_key);
        Self {
            state: State::Connecting,
            rx: IoBuf::with_capacity(EAPI_RX_BUF_SIZE),
            tx: IoBuf::with_capacity(TX_BUF_SIZE),
            sec_key,
            expected_accept_val: accept,
            last_activity_ns: 0,
            mask_counter: 0,
            sym: 0,
            lane: StreamLane::Eapi(lane),
            // Options tickers are never judged (no bookTicker on this
            // slot); the estimator sits idle.
            feed_clock: FeedClock::new(core_types::VenueId::Binance.default_stale_after_ms()),
            _not_sync: ::core::marker::PhantomData,
        }
    }

    /// Current state.
    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    /// Force the state for tests.
    #[cfg(test)]
    pub(crate) fn set_state(&mut self, s: State) {
        self.state = s;
    }

    /// Reset buffers + state for a reconnect.
    pub fn reset_for_reconnect(&mut self, nonce_seed: u64) {
        self.state = State::Connecting;
        self.rx.clear();
        self.tx.clear();
        self.sec_key = sec_websocket_key_from_seed(nonce_seed);
        self.expected_accept_val = expected_accept(&self.sec_key);
        self.last_activity_ns = 0;
        self.mask_counter = 0;
        // VT2: a new connection is a new offset; the threshold stays.
        self.feed_clock.reset();
    }
}

// ---------------------------------------------------------------
// drive_one — single-tick state machine advance
// ---------------------------------------------------------------

/// Pump the transport once and consume any buffered frames.
///
/// Zero-alloc once the handshake has completed. Transport errors bubble
/// up so the outer loop can close and reconnect.
///
/// * `transport`: any [`Transport`] implementation.
/// * `drv`: per-connection driver state.
/// * `host`, `path`: sent verbatim into the `GET` line + `Host:` header.
/// * `producer`: tick ring producer. A full ring drops the tick and
///   counts it on `status` (D4).
/// * `status`: shared per-ingress observability slot (relaxed atomics
///   only; this thread is the sole writer).
///
/// # Errors
///
/// Any transport error is surfaced. The caller's outer loop should close
/// and reconnect on `Err`.
#[allow(clippy::too_many_arguments)]
pub fn drive_one<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    event_tx: &mut Producer<core_types::ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
    status: &core_metrics::IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    // 1. Flush any pending outbound bytes.
    flush_tx(transport, drv)?;

    // 2. Read whatever plaintext the transport has for us.
    fill_rx(transport, drv)?;

    // 3. Advance the state machine.
    match drv.state {
        State::Connecting => {}
        State::NeedsWsWrite => {
            write_handshake_to_tx(drv, host, path)?;
            drv.state = State::AwaitingWsUpgrade;
        }
        State::AwaitingWsUpgrade => {
            advance_ws_upgrade(drv, status)?;
        }
        State::Steady => {
            drain_ws_frames(drv, producer, event_tx, event_mask, opt_tx, status, capture)?;
        }
        State::Closed => {}
    }

    // 4. Push any bytes the state machine produced out onto the wire.
    flush_tx(transport, drv)?;
    Ok(())
}

/// Transition the driver from `Connecting` → `NeedsWsWrite` once the
/// transport reports [`Status::Ready`].
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
// Private helpers — each zero-alloc
// ---------------------------------------------------------------

fn flush_tx<T: Transport>(transport: &mut T, drv: &mut Driver) -> io::Result<()> {
    if drv.tx.len() == 0 {
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
        let result = transport.read(drv.rx.free_mut());
        match result {
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

fn advance_ws_upgrade(drv: &mut Driver, status: &core_metrics::IngressStatus) -> io::Result<()> {
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
            // D7: the one Connecting → Steady transition this session —
            // publish `Up` exactly once, at the transition.
            status.set_state(core_metrics::IngressState::Up);
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

fn drain_ws_frames<C: Capture>(
    drv: &mut Driver,
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    event_tx: &mut Producer<core_types::ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
    status: &core_metrics::IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    loop {
        let read_result = ws_read_frame(drv.rx.filled());
        match read_result {
            WsReadResult::Incomplete => return Ok(()),
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
                    WsOpcode::Text => {
                        handle_text_frame(
                            drv,
                            payload.start..payload.end,
                            producer,
                            event_tx,
                            event_mask,
                            opt_tx,
                            status,
                            capture,
                        );
                    }
                    WsOpcode::Binary => {
                        // Binance @bookTicker is text-only; drop.
                    }
                    WsOpcode::Ping => {
                        let mask = ws_mask_from_counter(drv.mask_counter);
                        drv.mask_counter = drv.mask_counter.wrapping_add(1);
                        let payload_start = payload.start;
                        let payload_end = payload.end;
                        let payload_len = payload_end - payload_start;
                        let mut scratch = [0u8; 125];
                        debug_assert!(payload_len <= scratch.len());
                        scratch[..payload_len]
                            .copy_from_slice(&drv.rx.filled()[payload_start..payload_end]);
                        let dst = drv.tx.free_mut();
                        if let Ok(n) = ws_write_pong(dst, &scratch[..payload_len], mask) {
                            drv.tx.advance(n);
                        }
                    }
                    WsOpcode::Pong => {}
                    WsOpcode::Close => {
                        drv.state = State::Closed;
                    }
                    WsOpcode::Continuation => {
                        // Fragmented frames unused; drop rather than allocate
                        // a reassembly buffer.
                    }
                }

                // D5: record inbound liveness + byte accounting on the
                // shared status slot alongside the driver-local stamp.
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

/// Phase-1 outcome of one eapi combined frame (M2.4; `Copy` — the
/// two-phase pattern lets the index write mutate the lane after the
/// rx borrow ends).
#[derive(Copy, Clone)]
enum EapiAction {
    /// Unknown stream / malformed data — one rejection.
    Reject,
    /// Option ticker: the summary is always captured; the tick only
    /// when a side exists (quiet far options carry empty quotes).
    Ticker {
        tick: Option<Tick>,
        summary: OptSummary,
    },
    /// Index push for the per-underlying cache.
    Index { uly_idx: u8, px_1e9: i64 },
}

/// M2.4: handle one eapi combined-stream frame. Phase 1 borrows rx +
/// lane immutably (index READS during summary assembly are fine);
/// phase 2 applies the one mutable effect (the index-cache write).
fn handle_eapi_frame<C: Capture>(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
    status: &core_metrics::IngressStatus,
    capture: &mut C,
) {
    let action: EapiAction = {
        let payload = &drv.rx.filled()[payload_range.clone()];
        capture.raw_frame(now_ns(), payload);
        let StreamLane::Eapi(lane) = &drv.lane else {
            debug_assert!(false, "eapi handler on a bookTicker slot");
            return;
        };
        match crate::eapi::split_combined(payload) {
            Some((stream, data)) if stream.ends_with(b"@ticker") => {
                let sym_part = &stream[..stream.len() - b"@ticker".len()];
                match lane.table.lookup(sym_part) {
                    Some((sym, uly_idx)) => match crate::eapi::parse_eapi_ticker(data) {
                        Some(f) => {
                            let ts_ns = now_ns();
                            let tick = if f.bid_px_1e6 != 0 || f.ask_px_1e6 != 0 {
                                Some(Tick::new(
                                    ts_ns,
                                    core_types::VenueId::Binance,
                                    sym,
                                    // eapi tickers carry no venue seq.
                                    0,
                                    Price::from_raw(f.bid_px_1e6),
                                    Qty::from_raw(f.bid_qty_1e6),
                                    Price::from_raw(f.ask_px_1e6),
                                    Qty::from_raw(f.ask_qty_1e6),
                                ))
                            } else {
                                None
                            };
                            let summary = OptSummary::new(
                                ts_ns,
                                core_types::VenueId::Binance,
                                sym,
                                // eapi has no OI stream — MARK_PX only
                                // (docs/wire-format.md flags law).
                                core_types::OPT_SUMMARY_FLAG_MARK_PX,
                                f.mark_px_1e9,
                                f.mark_iv_1e9,
                                lane.index_px(uly_idx),
                                0,
                                f.delta_1e9,
                                f.gamma_1e9,
                                f.vega_1e6,
                                f.theta_1e6,
                            );
                            EapiAction::Ticker { tick, summary }
                        }
                        None => EapiAction::Reject,
                    },
                    None => EapiAction::Reject,
                }
            }
            Some((stream, data)) if stream.ends_with(b"@index") => {
                let uly_part = &stream[..stream.len() - b"@index".len()];
                match lane.uly_lookup(uly_part) {
                    Some(uly_idx) => match crate::eapi::parse_eapi_index(data) {
                        Some(px_1e9) => EapiAction::Index { uly_idx, px_1e9 },
                        None => EapiAction::Reject,
                    },
                    None => EapiAction::Reject,
                }
            }
            _ => EapiAction::Reject,
        }
    };
    match action {
        EapiAction::Reject => {
            status.inc_parse_errors();
            capture.parse_reject(now_ns(), &drv.rx.filled()[payload_range]);
        }
        EapiAction::Ticker { tick, summary } => {
            // §6.5: capture first. VM2 V2: the summary now ALSO rides
            // the opt lane (the kind-6 channel's engine entry).
            capture.opt_summary(&summary);
            if opt_tx.try_push(summary).is_err() {
                status.inc_opt_ring_drops();
            }
            if let Some(t) = tick {
                capture.tick(&t);
                if producer.try_push(t).is_err() {
                    status.inc_ring_drops();
                }
            }
            status.add_msgs(1);
            status.add_ticks(1);
        }
        EapiAction::Index { uly_idx, px_1e9 } => {
            if let StreamLane::Eapi(lane) = &mut drv.lane {
                lane.set_index_px(uly_idx, px_1e9);
            }
            status.add_msgs(1);
            status.add_ticks(1);
        }
    }
}

/// WS5: handle one `@markPrice` frame — capture-only events, the
/// OKX Mark/Funding conventions (`Mark` v0 = mark ×1e6; on this
/// venue v1 = index ×1e6, where OKX leaves 0; `Funding` v0 = rate
/// ×1e9, v1 = next-funding ms — gated on wire truth, the WS3
/// `has_funding` split: dated futures push `"r":""`).
fn handle_mark_price_frame<C: Capture>(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    event_tx: &mut Producer<core_types::ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    status: &core_metrics::IngressStatus,
    capture: &mut C,
) {
    let payload = &drv.rx.filled()[payload_range];
    capture.raw_frame(now_ns(), payload);
    match crate::parse_mark_price(payload, drv.sym) {
        Some(f) => {
            capture.event(&core_types::ChannelEvent::new(
                now_ns(),
                core_types::VenueId::Binance,
                core_types::ChannelId::Mark,
                f.sym,
                0,
                f.ts_ns / 1_000_000,
                f.mark_px_1e6,
                f.index_px_1e6,
            ));
            if f.has_funding == 1 {
                let ev = core_types::ChannelEvent::new(
                    now_ns(),
                    core_types::VenueId::Binance,
                    core_types::ChannelId::Funding,
                    f.sym,
                    0,
                    f.ts_ns / 1_000_000,
                    f.funding_rate_1e9,
                    f.next_funding_ms as i64,
                );
                capture.event(&ev);
                // WS10-A: onto the venue-event lane (capture stays
                // first — §6.5 capture-before-push law).
                if event_mask & core_types::event_lane_bit(core_types::ChannelId::Funding) != 0
                    && event_tx.try_push(ev).is_err()
                {
                    status.inc_event_ring_drops();
                }
            }
            status.add_msgs(1);
            status.add_ticks(1);
        }
        None => {
            status.inc_parse_errors();
            capture.parse_reject(now_ns(), payload);
        }
    }
}

fn handle_text_frame<C: Capture>(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    event_tx: &mut Producer<core_types::ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
    status: &core_metrics::IngressStatus,
    capture: &mut C,
) {
    // M2.4/WS5: per-slot lane dispatch (monomorphic).
    if matches!(drv.lane, StreamLane::Eapi(_)) {
        return handle_eapi_frame(drv, payload_range, producer, opt_tx, status, capture);
    }
    if matches!(drv.lane, StreamLane::MarkPrice) {
        return handle_mark_price_frame(drv, payload_range, event_tx, event_mask, status, capture);
    }
    let payload = &drv.rx.filled()[payload_range];
    // §6.5 capture: raw tap fires before parsing.
    capture.raw_frame(now_ns(), payload);
    if let Some(f) = parse_book_ticker(payload, drv.sym) {
        let ts_ns = now_ns();
        // VT2: one parse-complete stamp serves the judgement and the
        // tick. Spot pushes carry venue_time 0 ⇒ never stale (the
        // sentinel step supplies their stamp later).
        let judged = drv.feed_clock.judge(f.venue_time_ms, ts_ns);
        // `update_id` fits comfortably in u32 over the lifetime of a
        // connection (Binance resets on (re)connect). Truncate for the
        // venue_seq slot; it's only used for monotonicity checks.
        let venue_seq = (f.update_id & 0xFFFF_FFFF) as u32;
        let tick = Tick::new_stamped(
            ts_ns,
            core_types::VenueId::Binance,
            f.sym,
            venue_seq,
            Price::from_raw(f.bid_px_1e6),
            Qty::from_raw(f.bid_qty_1e6),
            Price::from_raw(f.ask_px_1e6),
            Qty::from_raw(f.ask_qty_1e6),
            f.venue_time_ms,
            (judged.stale as u8) * TICK_FLAG_STALE,
        );
        if judged.stale {
            status.inc_stale_ticks();
        }
        status.set_feed_delay_ema_ms(drv.feed_clock.delay_ema_ms());
        // §6.5 capture BEFORE the push — a ring-dropped tick must still
        // reach the replay log (the audit pairs capture counts with
        // ring_drops_total).
        capture.tick(&tick);
        // D4: a full ring is data loss — count it, never block on it.
        if producer.try_push(tick).is_err() {
            status.inc_ring_drops();
        }
        status.add_msgs(1);
        status.add_ticks(1);
    } else {
        status.inc_parse_errors();
        capture.parse_reject(now_ns(), payload);
    }
}

// ---------------------------------------------------------------
// Top-level driver
// ---------------------------------------------------------------

/// Stop flag that external threads can raise to signal a graceful
/// shutdown.
pub type StopFlag = AtomicBool;

/// Run the Binance ingress loop until `stop` is set, the transport
/// fails, or the keepalive declares the connection dead
/// ([`RunResult::IdleTimeout`]). Reconnect is the caller's
/// responsibility.
///
/// * `status`: shared per-ingress observability slot (D4/D5/D7); this
///   thread is the sole writer.
/// * `keepalive`: proactive-ping + idle-timeout scheduler (D5/D6);
///   reset at entry, polled once per loop iteration in `Steady`.
#[allow(clippy::too_many_arguments)]
pub fn run<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    event_tx: &mut Producer<core_types::ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    token: mio::Token,
    stop: &StopFlag,
    status: &core_metrics::IngressStatus,
    keepalive: &mut core_net::Keepalive,
    capture: &mut C,
) -> RunResult {
    let session_start_ns = now_ns();
    keepalive.reset();
    if transport.register(poll.registry(), token).is_err() {
        return RunResult::Error;
    }
    // See ingress-polymarket for rationale — skip `epoll_ctl`
    // when the readable+writable bitmask is unchanged.
    let mut last_interest = transport.interest();

    while !stop.load(Ordering::Relaxed) {
        if poll
            .poll(events, Some(std::time::Duration::from_millis(50)))
            .is_err()
        {
            return RunResult::Error;
        }

        for ev in events.iter() {
            if ev.token() != token {
                continue;
            }
            // Named to avoid shadowing the `status` metrics slot.
            let transport_status = match transport.pump(ev) {
                Ok(s) => s,
                Err(_e) => return RunResult::Error,
            };
            note_transport_ready(drv, transport_status);
        }

        // I-3: tight inner drain loop. See ingress-polymarket for
        // rationale.
        loop {
            let n_before = producer.len();
            let state_before = drv.state();
            if drive_one(
                transport, drv, host, path, producer, event_tx, event_mask, opt_tx, status,
                capture,
            )
            .is_err()
            {
                return RunResult::Error;
            }
            if drv.state() == State::Closed {
                return RunResult::Disconnected;
            }
            if producer.len() == n_before && drv.state() == state_before {
                break;
            }
        }

        // §6.5: staged capture reaches disk within the flush interval
        // even on quiet feeds (one clock read + one branch per poll
        // iteration; ~50 ms cadence).
        capture.maybe_flush(now_ns());

        // D5/D6: keepalive check — once per steady-state iteration,
        // after IO has been processed and before blocking on poll
        // again. Two relaxed loads + integer compares when idle.
        if drv.state() == State::Steady {
            let now = now_ns();
            let act = if drv.last_activity_ns == 0 {
                session_start_ns
            } else {
                drv.last_activity_ns
            };
            match keepalive.poll(now, act) {
                core_net::KeepaliveAction::SendPing => {
                    // Masked WS protocol ping, empty payload — mirrors
                    // the pong path in `drain_ws_frames`.
                    let mask = ws_mask_from_counter(drv.mask_counter);
                    drv.mask_counter = drv.mask_counter.wrapping_add(1);
                    let dst = drv.tx.free_mut();
                    if let Ok(n) = ws_write_ping(dst, &[], mask) {
                        drv.tx.advance(n);
                    }
                    keepalive.mark_ping_sent(now);
                    // Flush in this iteration via the existing write
                    // path rather than waiting for the next drive_one.
                    if flush_tx(transport, drv).is_err() {
                        return RunResult::Error;
                    }
                }
                core_net::KeepaliveAction::Reconnect => return RunResult::IdleTimeout,
                core_net::KeepaliveAction::None => {}
            }
        }

        let cur = transport.interest();
        if cur != last_interest {
            if transport.reregister(poll.registry(), token).is_err() {
                return RunResult::Error;
            }
            last_interest = cur;
        }
    }

    RunResult::Stopped
}

// ---------------------------------------------------------------
// run_multi — N single-stream connections, ONE thread, ONE producer
// (M1: Binance multi-symbol spot + USDS-M futures)
// ---------------------------------------------------------------

/// One connection slot for [`run_multi`]: endpoint bytes + per-
/// connection driver/keepalive/backoff, owned by the single venue
/// thread. **Single-writer law:** N sockets, one thread, one tick
/// producer — the slots never leave the thread.
///
/// Boot-time construction (allocations fine); steady state is the
/// same zero-alloc [`drive_one`] the single-connection path runs.
pub struct MultiConn<T: Transport> {
    /// Live transport; `None` while the slot awaits a reconnect.
    transport: Option<T>,
    /// Per-connection WS state machine (sym pinned inside).
    drv: Driver,
    /// Host bytes for the `Host:` header (spot vs USDS-M hosts
    /// differ — each slot carries its own).
    host: Vec<u8>,
    /// Request path (`/ws/<symbol>@bookTicker`).
    path: Vec<u8>,
    keepalive: core_net::Keepalive,
    backoff: core_net::Backoff,
    /// Monotonic ns before which no reconnect is attempted.
    next_attempt_ns: NsTs,
    /// Session start for the keepalive activity fallback.
    session_start_ns: NsTs,
    /// Interest bitmask at the last (re)registration — skip the
    /// syscall when unchanged (same rationale as `run()`).
    last_interest: Option<mio::Interest>,
}

impl<T: Transport> MultiConn<T> {
    /// New slot, initially disconnected (the loop's reconnect pass
    /// dials it; `next_attempt_ns` 0 = due immediately).
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

    /// Tear down the slot's transport (socket closes on drop; kqueue/
    /// epoll deregister closed fds) and schedule the next attempt.
    /// A session that saw inbound activity resets the backoff first —
    /// the D8 flap-vs-healthy distinction the single-connection
    /// wrapper makes in the cli.
    fn kill(&mut self, now: NsTs, status: &core_metrics::IngressStatus) {
        if self.transport.take().is_some() {
            status.inc_reconnects();
            if self.drv.last_activity_ns > self.session_start_ns {
                self.backoff.reset();
            }
        }
        self.next_attempt_ns = now + self.backoff.next_delay_ns();
    }
}

/// Drive N single-stream connections on one thread with one producer
/// until `stop` is set. Per-slot failures (transport error, WS close,
/// idle timeout) never end the loop — the slot is torn down and
/// re-dialed via `connect` with jittered backoff, **at most one
/// blocking dial per poll iteration** so a flapping endpoint cannot
/// starve the live slots. Returns [`RunResult::Stopped`] on the stop
/// flag; [`RunResult::Error`] only on poll-infrastructure failure.
///
/// `connect(i)` dials slot `i` (blocking, bounded by the caller's
/// connect timeout) and returns `None` on failure.
// Doctrine: raw indices over `conns`, not iterator adapters — hot poll
// loop (CLAUDE.md hot-path rules; `i` is also the mio Token identity).
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
pub fn run_multi<T: Transport, C: Capture>(
    conns: &mut [MultiConn<T>],
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    event_tx: &mut Producer<core_types::ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    stop: &StopFlag,
    status: &core_metrics::IngressStatus,
    capture: &mut C,
    mut connect: impl FnMut(usize) -> Option<T>,
) -> RunResult {
    while !stop.load(Ordering::Relaxed) {
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
                        conns[i].kill(now, status);
                    } else {
                        conns[i].last_interest = Some(t.interest());
                        conns[i].drv.reset_for_reconnect(now);
                        conns[i].keepalive.reset();
                        conns[i].session_start_ns = now;
                        conns[i].transport = Some(t);
                    }
                }
                None => conns[i].kill(now, status),
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
                Err(_e) => c.kill(now_ns(), status),
            }
        }

        // 3. Drain every live slot (I-3 bounded no-progress loop).
        for i in 0..conns.len() {
            let c = &mut conns[i];
            let Some(t) = c.transport.as_mut() else {
                continue;
            };
            loop {
                let n_before = producer.len();
                let state_before = c.drv.state();
                if drive_one(
                    t, &mut c.drv, &c.host, &c.path, producer, event_tx, event_mask, opt_tx,
                    status, capture,
                )
                .is_err()
                {
                    c.kill(now_ns(), status);
                    break;
                }
                if c.drv.state() == State::Closed {
                    c.kill(now_ns(), status);
                    break;
                }
                if producer.len() == n_before && c.drv.state() == state_before {
                    break;
                }
            }
        }

        // 4. §6.5 capture flush cadence (one clock read per iteration).
        capture.maybe_flush(now_ns());

        // 5. Keepalive per steady slot (D5/D6).
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
                    let mask = ws_mask_from_counter(c.drv.mask_counter);
                    c.drv.mask_counter = c.drv.mask_counter.wrapping_add(1);
                    let dst = c.drv.tx.free_mut();
                    if let Ok(n) = ws_write_ping(dst, &[], mask) {
                        c.drv.tx.advance(n);
                    }
                    c.keepalive.mark_ping_sent(now);
                    if flush_tx(t, &mut c.drv).is_err() {
                        c.kill(now, status);
                    }
                }
                core_net::KeepaliveAction::Reconnect => c.kill(now, status),
                core_net::KeepaliveAction::None => {}
            }
        }

        // 6. Interest re-registration per live slot — only when the
        // bitmask actually changed (see run()'s rationale).
        for i in 0..conns.len() {
            let c = &mut conns[i];
            let Some(t) = c.transport.as_mut() else {
                continue;
            };
            let cur = t.interest();
            if c.last_interest != Some(cur) {
                if t.reregister(poll.registry(), mio::Token(i)).is_err() {
                    c.kill(now_ns(), status);
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
    use core_net::{
        expected_accept as expected_accept_pub, sec_websocket_key_from_seed as sec_key_pub,
        TestTransport,
    };
    use core_ring::Ring;
    use core_types::NullCapture;

    fn build_driver(seed: u64, sym: SymbolId) -> Driver {
        Driver::new(seed, sym)
    }

    fn event_ring_pair() -> (
        core_ring::Producer<core_types::ChannelEvent, EVENT_RING_SIZE>,
        core_ring::Consumer<core_types::ChannelEvent, EVENT_RING_SIZE>,
    ) {
        Ring::<core_types::ChannelEvent, EVENT_RING_SIZE>::new().split()
    }

    fn opt_ring_pair() -> (
        Producer<OptSummary, OPT_RING_SIZE>,
        core_ring::Consumer<OptSummary, OPT_RING_SIZE>,
    ) {
        Ring::<OptSummary, OPT_RING_SIZE>::new().split()
    }

    /// WS10-A shim: legacy tests drive with a fresh throwaway event
    /// lane (mask = FUNDING; consumer dropped — pushes vanish). A
    /// local item shadows the glob-imported `super::drive_one`, so
    /// every pre-WS10 call site stays byte-identical. The dedicated
    /// event-lane tests below call `super::drive_one` directly.
    #[allow(clippy::too_many_arguments)]
    fn drive_one<T: Transport, C: Capture>(
        transport: &mut T,
        drv: &mut Driver,
        host: &[u8],
        path: &[u8],
        producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
        status: &core_metrics::IngressStatus,
        capture: &mut C,
    ) -> io::Result<()> {
        let (mut etx, _erx) = event_ring_pair();
        let (mut otx, _orx) = opt_ring_pair();
        super::drive_one(
            transport,
            drv,
            host,
            path,
            producer,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut otx,
            status,
            capture,
        )
    }

    /// WS10-A shim for `run` — same rationale as the `drive_one` shim.
    #[allow(clippy::too_many_arguments)]
    fn run<T: Transport, C: Capture>(
        transport: &mut T,
        drv: &mut Driver,
        host: &[u8],
        path: &[u8],
        producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
        poll: &mut mio::Poll,
        events: &mut mio::Events,
        token: mio::Token,
        stop: &StopFlag,
        status: &core_metrics::IngressStatus,
        keepalive: &mut core_net::Keepalive,
        capture: &mut C,
    ) -> RunResult {
        let (mut etx, _erx) = event_ring_pair();
        let (mut otx, _orx) = opt_ring_pair();
        super::run(
            transport,
            drv,
            host,
            path,
            producer,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut otx,
            poll,
            events,
            token,
            stop,
            status,
            keepalive,
            capture,
        )
    }

    fn build_server_response(accept: &[u8; 28]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(256);
        out.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
        out.extend_from_slice(b"Upgrade: websocket\r\n");
        out.extend_from_slice(b"Connection: Upgrade\r\n");
        out.extend_from_slice(b"Sec-WebSocket-Accept: ");
        out.extend_from_slice(accept);
        out.extend_from_slice(b"\r\n\r\n");
        out
    }

    #[test]
    fn driver_starts_in_connecting() {
        let d = build_driver(1, 42);
        assert_eq!(d.state(), State::Connecting);
    }

    #[test]
    fn note_transport_ready_advances_to_needs_ws_write() {
        let mut d = build_driver(1, 42);
        note_transport_ready(&mut d, Status::Ready);
        assert_eq!(d.state(), State::NeedsWsWrite);
    }

    #[test]
    fn note_transport_ready_closed_transitions_to_closed() {
        let mut d = build_driver(1, 42);
        note_transport_ready(&mut d, Status::Closed);
        assert_eq!(d.state(), State::Closed);
    }

    #[test]
    fn drive_one_writes_handshake_once_ready() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = build_driver(1, 42);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();

        // Before Ready — no handshake written.
        drive_one(
            &mut t,
            &mut d,
            b"stream.binance.com",
            b"/ws/btcusdt@bookTicker",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(t.outgoing_len(), 0);

        note_transport_ready(&mut d, Status::Ready);
        drive_one(
            &mut t,
            &mut d,
            b"stream.binance.com",
            b"/ws/btcusdt@bookTicker",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();

        let mut buf = [0u8; 4096];
        let n = t.drain_outgoing(&mut buf);
        assert!(n > 0);
        let prefix = b"GET /ws/btcusdt@bookTicker HTTP/1.1\r\n";
        assert_eq!(&buf[..prefix.len()], prefix);
        assert_eq!(&buf[n - 4..n], b"\r\n\r\n");
        assert_eq!(d.state(), State::AwaitingWsUpgrade);
    }

    #[test]
    fn drive_one_completes_upgrade_on_valid_response() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = build_driver(42, 1);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        let mut scratch = [0u8; 4096];
        let _ = t.drain_outgoing(&mut scratch);
        assert_eq!(status.state(), core_metrics::IngressState::Down);

        let key = sec_key_pub(42);
        let accept = expected_accept_pub(&key);
        let resp = build_server_response(&accept);
        t.inject_incoming(&resp);

        drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert_eq!(d.state(), State::Steady);
        // D7: the upgrade transition publishes Up + activity + bytes.
        assert_eq!(status.state(), core_metrics::IngressState::Up);
        assert!(status.last_activity_ns() > 0);
        assert_eq!(status.bytes_total(), resp.len() as u64);
    }

    #[test]
    fn drive_one_rejects_wrong_accept_value() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = build_driver(1, 0);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        let mut scratch = [0u8; 4096];
        let _ = t.drain_outgoing(&mut scratch);

        let wrong: [u8; 28] = *b"XXXXXXXXXXXXXXXXXXXXXXXXXXXX";
        let resp = build_server_response(&wrong);
        t.inject_incoming(&resp);

        let err = drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // Failed upgrade must never publish Up.
        assert_eq!(status.state(), core_metrics::IngressState::Down);
    }

    fn huge_keepalive() -> core_net::Keepalive {
        core_net::Keepalive::new(core_net::KeepaliveCfg {
            ping_interval_ns: u64::MAX / 4,
            idle_timeout_ns: u64::MAX / 2,
        })
    }

    fn test_backoff(seed: u64) -> core_net::Backoff {
        core_net::Backoff::new(1_000_000, 1_000_000_000, seed)
    }

    fn ws_text_frame(payload: &[u8]) -> Vec<u8> {
        // 7-bit and 16-bit length forms (M2.4 eapi combined payloads
        // exceed 125 bytes).
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

    /// M1 core guarantee: N connections, ONE thread, ONE producer —
    /// ticks from both slots land in the same ring with their pinned
    /// syms; per-slot state machines stay independent.
    #[test]
    fn run_multi_drains_two_steady_slots_into_one_producer() {
        let mut t_a = TestTransport::with_capacity(16 * 1024);
        let mut t_b = TestTransport::with_capacity(16 * 1024);
        let payload_a = br#"{"u":1,"s":"BTCUSDT","b":"25.10","B":"1.0","a":"25.20","A":"1.0"}"#;
        let payload_b = br#"{"u":2,"s":"ETHUSDT","b":"3.10","B":"1.0","a":"3.20","A":"1.0"}"#;
        t_a.inject_incoming(&ws_text_frame(payload_a));
        t_b.inject_incoming(&ws_text_frame(payload_b));

        let mut d_a = build_driver(1, 42);
        d_a.set_state(State::Steady);
        let mut d_b = build_driver(2, 7);
        d_b.set_state(State::Steady);

        let mut c_a = MultiConn::new(
            d_a,
            b"spot.example",
            b"/ws/a",
            huge_keepalive(),
            test_backoff(1),
        );
        c_a.transport = Some(t_a);
        let mut c_b = MultiConn::new(
            d_b,
            b"fut.example",
            b"/ws/b",
            huge_keepalive(),
            test_backoff(2),
        );
        c_b.transport = Some(t_b);
        let mut conns = [c_a, c_b];

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(8);
        let stop = StopFlag::new(false);

        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(250));
                stop.store(true, Ordering::Relaxed);
            });
            let (mut etx, _erx) = event_ring_pair();
            let res = run_multi(
                &mut conns,
                &mut prod,
                &mut etx,
                core_types::EVENT_LANE_FUNDING,
                &mut opt_ring_pair().0,
                &mut poll,
                &mut events,
                &stop,
                &status,
                &mut NullCapture,
                |_i| None,
            );
            assert_eq!(res, RunResult::Stopped);
        });

        let mut syms = [cons.try_pop().unwrap().sym, cons.try_pop().unwrap().sym];
        syms.sort_unstable();
        assert_eq!(syms, [7, 42]);
        assert!(cons.try_pop().is_none());
        assert_eq!(status.msgs_total(), 2);
        // No kills: both transports still installed, zero reconnects.
        assert!(conns[0].transport.is_some());
        assert!(conns[1].transport.is_some());
        assert_eq!(status.reconnects_total(), 0);
    }

    /// Reconnect pacing: at most ONE dial per poll iteration; failed
    /// dials schedule jittered retries; the loop exits only on stop.
    #[test]
    fn run_multi_paces_one_reconnect_attempt_per_iteration() {
        let d_a = build_driver(1, 42);
        let d_b = build_driver(2, 7);
        let conns_init = [
            MultiConn::new(d_a, b"h", b"/a", huge_keepalive(), test_backoff(3)),
            MultiConn::new(d_b, b"h", b"/b", huge_keepalive(), test_backoff(4)),
        ];
        let mut conns = conns_init;

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(8);
        let stop = StopFlag::new(false);

        let calls = std::cell::Cell::new(0u32);
        let (mut etx, _erx) = event_ring_pair();
        let res = run_multi(
            &mut conns,
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut opt_ring_pair().0,
            &mut poll,
            &mut events,
            &stop,
            &status,
            &mut NullCapture,
            |_i| {
                let n = calls.get() + 1;
                calls.set(n);
                if n >= 3 {
                    stop.store(true, Ordering::Relaxed);
                }
                None::<TestTransport>
            },
        );
        assert_eq!(res, RunResult::Stopped);
        assert_eq!(calls.get(), 3, "exactly one dial per iteration");
        assert!(conns[0].next_attempt_ns > 0, "slot 0 got scheduled");
        assert!(conns[1].next_attempt_ns > 0, "slot 1 got scheduled");
    }

    /// M2.4 eapi lane: index push fills the cache; a ticker push
    /// yields a Tick (ring) + an OptSummary (capture) with the cached
    /// underlying px; empty quotes yield the summary alone; unknown
    /// streams reject.
    #[test]
    fn eapi_slot_routes_ticker_index_and_rejects() {
        struct RecCap {
            summaries: Vec<OptSummary>,
            rejects: u32,
        }
        impl Capture for RecCap {
            fn opt_summary(&mut self, o: &OptSummary) {
                self.summaries.push(*o);
            }
            fn parse_reject(&mut self, _ts: NsTs, _p: &[u8]) {
                self.rejects += 1;
            }
        }

        let mut table = crate::eapi::EapiSymbolTable::new();
        let sym: SymbolId = (1 << 24) | 1025; // venue 1, ordinal 1025 (base-1024 block)
        table.insert(b"BTC-260327-100000-C", sym, 0).unwrap();
        let lane = crate::eapi::EapiLane::new(table, &[b"BTCUSDT"]);
        let mut d = Driver::new_eapi(7, lane);
        d.set_state(State::Steady);

        let mut t = TestTransport::with_capacity(64 * 1024);
        // 1. index push (fills the cache) …
        t.inject_incoming(&ws_text_frame(
            br#"{"stream":"btcusdt@index","data":{"e":"index","E":1,"s":"BTCUSDT","p":"77000.5"}}"#,
        ));
        // 2. … a full ticker (tick + summary with underlying px) …
        t.inject_incoming(&ws_text_frame(
            br#"{"stream":"btc-260327-100000-c@ticker","data":{"e":"24hrTicker","s":"BTC-260327-100000-C","bo":"2040.5","ao":"2060.1","bq":"1.25","aq":"0.75","b":"0.62","a":"0.68","d":"0.512","t":"-85.3","g":"0.0000123","v":"152.3","vo":"0.6543","mp":"2051.2"}}"#,
        ));
        // 3. … a quiet-quotes ticker (summary only) …
        t.inject_incoming(&ws_text_frame(
            br#"{"stream":"btc-260327-100000-c@ticker","data":{"s":"BTC-260327-100000-C","bo":"","ao":"","bq":"","aq":"","d":"0.5","t":"-80.0","g":"0.00001","v":"150.0","vo":"0.65","mp":"2050.0"}}"#,
        ));
        // 4. … an unsubscribed stream (reject).
        t.inject_incoming(&ws_text_frame(
            br#"{"stream":"eth-1-c@ticker","data":{"mp":"1","vo":"1","d":"0","g":"0","v":"0","t":"0"}}"#,
        ));

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut cap = RecCap {
            summaries: Vec::new(),
            rejects: 0,
        };
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &status, &mut cap).unwrap();

        // Exactly ONE tick (the full ticker), sym + BBO from bo/ao.
        let tick = cons.try_pop().expect("tick from the full ticker");
        assert_eq!(tick.sym, sym);
        assert_eq!(tick.bid_px.raw(), 2_040_500_000);
        assert_eq!(tick.ask_px.raw(), 2_060_100_000);
        assert!(cons.try_pop().is_none());

        // TWO summaries; the first carries the cached underlying px,
        // MARK_PX-only flags, vo (not b/a) as the IV.
        assert_eq!(cap.summaries.len(), 2);
        let s0 = &cap.summaries[0];
        assert_eq!(s0.sym, sym);
        assert_eq!(s0.venue, core_types::VenueId::Binance as u8);
        assert_eq!(s0.flags, core_types::OPT_SUMMARY_FLAG_MARK_PX);
        assert_eq!(s0.underlying_px_1e9, 77_000_500_000_000);
        assert_eq!(s0.mark_px_1e9, 2_051_200_000_000);
        assert_eq!(s0.mark_iv_1e9, 654_300_000);
        assert_eq!(s0.open_interest_1e6, 0);
        assert_eq!(s0.theta_1e6, -85_300_000);
        assert_eq!(cap.summaries[1].mark_px_1e9, 2_050_000_000_000);

        // One reject (the unsubscribed stream).
        assert_eq!(cap.rejects, 1);
    }

    #[test]
    fn steady_state_parses_book_ticker_into_tick_ring() {
        let mut t = TestTransport::with_capacity(16 * 1024);
        let mut d = build_driver(7, 42);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();

        let payload = br#"{"u":400900217,"s":"BTCUSDT","b":"25.35190000","B":"31.21","a":"25.36520000","A":"40.66"}"#;
        let mut frame_buf = [0u8; 256];
        assert!(payload.len() <= 125);
        frame_buf[0] = 0x81; // FIN + Text
        frame_buf[1] = payload.len() as u8; // mask=0
        frame_buf[2..2 + payload.len()].copy_from_slice(payload);
        let frame_len = 2 + payload.len();
        t.inject_incoming(&frame_buf[..frame_len]);

        let status = core_metrics::IngressStatus::new();
        drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();

        let tick = cons.try_pop().expect("tick must be pushed");
        assert_eq!(tick.sym, 42);
        assert_eq!(tick.bid_px.raw(), 25_351_900);
        assert_eq!(tick.ask_px.raw(), 25_365_200);
        assert_eq!(tick.venue_seq, 400_900_217u32);
        // VT2: a SPOT push carries no stamp — unknown, never stale.
        assert_eq!(tick.venue_time_ms, 0);
        assert!(!tick.is_stale());
        assert_eq!(status.stale_ticks_total(), 0);
        assert!(cons.try_pop().is_none());
        // D5 accounting: one parsed message, whole frame counted.
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.bytes_total(), frame_len as u64);
        assert_eq!(status.parse_errors_total(), 0);
        assert_eq!(status.ring_drops_total(), 0);
        assert!(status.last_activity_ns() > 0);
    }

    /// VT2 helper: one USDS-M `bookTicker` push stamped `T = t_ms`
    /// (`E = t_ms + 2`) through a steady driver; returns its tick.
    fn push_usdm_book_ticker(
        t: &mut TestTransport,
        d: &mut Driver,
        prod: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
        cons: &mut core_ring::Consumer<Tick, DEFAULT_TICK_RING_CAP>,
        status: &core_metrics::IngressStatus,
        t_ms: u64,
        u: u64,
    ) -> Tick {
        let s = format!(
            r#"{{"e":"bookTicker","u":{u},"E":{},"T":{t_ms},"s":"BTCUSDT","b":"25.35","B":"31.21","a":"25.36","A":"40.66"}}"#,
            t_ms + 2
        );
        t.inject_incoming(&ws_text_frame(s.as_bytes()));
        drive_one(t, d, b"host", b"/", prod, status, &mut NullCapture).unwrap();
        cons.try_pop().expect("bookTicker must produce a tick")
    }

    #[test]
    fn usdm_book_ticker_carries_t_and_the_stale_judgement() {
        // VT2: first stamped push = the offset (fresh); a push whose
        // transaction time is 5 s older is stale at bn 1000 ms (flag +
        // counter); a later stamp is fresh again.
        let mut t = TestTransport::with_capacity(16 * 1024);
        let mut d = build_driver(7, 42);
        d.set_state(State::Steady);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let t0: u64 = 1_755_216_000_000;

        let fresh = push_usdm_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, t0, 1);
        assert_eq!(fresh.venue_time_ms, t0, "T wins over E (+2)");
        assert!(!fresh.is_stale());
        assert_eq!(status.stale_ticks_total(), 0);

        let stale = push_usdm_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, t0 - 5_000, 2);
        assert!(stale.is_stale());
        assert_eq!(stale.flags, TICK_FLAG_STALE);
        assert_eq!(status.stale_ticks_total(), 1);
        assert!(status.feed_delay_ema_ms() > 0);

        let again = push_usdm_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, t0 + 10, 3);
        assert!(!again.is_stale());
        assert_eq!(status.stale_ticks_total(), 1);
        assert_eq!(status.ticks_total(), 3);
    }

    #[test]
    fn stale_threshold_override_and_reconnect_reset_apply() {
        let mut t = TestTransport::with_capacity(16 * 1024);
        let mut d = build_driver(7, 42);
        d.set_stale_after_ms(10_000);
        d.set_state(State::Steady);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let t0: u64 = 1_755_216_000_000;
        let _ = push_usdm_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, t0, 1);
        let five_s = push_usdm_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, t0 - 5_000, 2);
        assert!(!five_s.is_stale(), "5 s is under a 10 s threshold");
        assert_eq!(status.stale_ticks_total(), 0);

        d.reset_for_reconnect(9);
        d.set_state(State::Steady);
        let after = push_usdm_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, t0 - 60_000, 3);
        assert!(!after.is_stale(), "a reconnect starts a fresh offset");
        assert_eq!(after.venue_time_ms, t0 - 60_000);
    }

    #[test]
    fn steady_state_replies_pong_to_ping() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = build_driver(7, 1);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();

        let mut frame = [0u8; 16];
        frame[0] = 0x89; // FIN + Ping
        frame[1] = 4;
        frame[2..6].copy_from_slice(b"PING");
        t.inject_incoming(&frame[..6]);

        let status = core_metrics::IngressStatus::new();
        drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert!(t.outgoing_len() > 0);

        let mut out = [0u8; 64];
        let n = t.drain_outgoing(&mut out);
        assert_eq!(out[0], 0x8A);
        assert_eq!(out[1], 0x80 | 4);
        let mask = [out[2], out[3], out[4], out[5]];
        let mut unmasked = [0u8; 4];
        let mut i = 0;
        while i < 4 {
            unmasked[i] = out[6 + i] ^ mask[i & 3];
            i += 1;
        }
        assert_eq!(&unmasked, b"PING");
        assert_eq!(n, 10);
    }

    #[test]
    fn garbled_text_frame_is_dropped_silently() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = build_driver(1, 5);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();

        let payload = b"not-json-not-anything";
        let mut frame = [0u8; 64];
        frame[0] = 0x81;
        frame[1] = payload.len() as u8;
        frame[2..2 + payload.len()].copy_from_slice(payload);
        t.inject_incoming(&frame[..2 + payload.len()]);

        let status = core_metrics::IngressStatus::new();
        drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        assert!(cons.try_pop().is_none());
        // Silent drop on the ring, but the rejection is counted.
        assert_eq!(status.parse_errors_total(), 1);
        assert_eq!(status.msgs_total(), 0);
    }

    #[test]
    fn run_returns_idle_timeout_when_transport_stays_silent() {
        // D5: a steady-state connection that never delivers a byte must
        // be torn down once the keepalive idle budget is exhausted.
        let mut t = TestTransport::with_capacity(4096);
        let mut d = build_driver(3, 9);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();

        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let stop = StopFlag::new(false);
        let status = core_metrics::IngressStatus::new();
        // Tiny idle budget: the first keepalive check after the first
        // poll wakeup is already past it.
        let mut ka = core_net::Keepalive::new(core_net::KeepaliveCfg {
            ping_interval_ns: 0,
            idle_timeout_ns: 1,
        });

        let res = run(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(0),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
        );
        assert_eq!(res, RunResult::IdleTimeout);
    }

    #[test]
    fn run_emits_masked_protocol_ping_when_interval_elapses() {
        // D6: with a tiny ping interval and a huge idle budget the loop
        // must proactively queue + flush a masked, empty WS ping.
        let mut t = TestTransport::with_capacity(4096);
        let mut d = build_driver(4, 9);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();

        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let stop = std::sync::Arc::new(StopFlag::new(false));
        let status = core_metrics::IngressStatus::new();
        let mut ka = core_net::Keepalive::new(core_net::KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: u64::MAX / 2,
        });

        // `run` blocks on this thread (50 ms poll timeout per
        // iteration); a helper thread raises `stop` after a couple of
        // iterations. The ping is queued *and* flushed inside the
        // first iteration, so any exit after one full pass suffices.
        let stopper = stop.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            stopper.store(true, Ordering::Relaxed);
        });

        let res = run(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(0),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
        );
        h.join().unwrap();
        assert_eq!(res, RunResult::Stopped);

        let mut out = [0u8; 512];
        let n = t.drain_outgoing(&mut out);
        // Empty-payload client ping = 2 header bytes + 4 mask bytes.
        assert!(n >= 6, "at least one ping frame must reach the wire");
        assert_eq!(out[0] & 0x0F, 0x9, "opcode must be Ping");
        assert_ne!(out[0] & 0x80, 0, "FIN must be set");
        assert_ne!(
            out[1] & 0x80,
            0,
            "client frames must be masked (RFC 6455 §5.3)"
        );
        assert_eq!(out[1] & 0x7F, 0, "keepalive ping carries an empty payload");
    }

    /// Records every hook invocation — pins the §6.5 capture-site
    /// semantics without touching the filesystem. The bookTicker lane
    /// never emits `ChannelEvent`s (BBO flows as `Tick`); the WS5
    /// markPrice lane does — `event()` records them.
    #[derive(Default)]
    struct CountingCapture {
        ticks: u32,
        raw_frames: u32,
        rejects: u32,
        flushes: u32,
        events: Vec<core_types::ChannelEvent>,
    }

    impl core_types::Capture for CountingCapture {
        fn tick(&mut self, _t: &Tick) {
            self.ticks += 1;
        }
        fn event(&mut self, e: &core_types::ChannelEvent) {
            self.events.push(*e);
        }
        fn raw_frame(&mut self, _ts_ns: u64, _payload: &[u8]) {
            self.raw_frames += 1;
        }
        fn parse_reject(&mut self, _ts_ns: u64, _payload: &[u8]) {
            self.rejects += 1;
        }
        fn maybe_flush(&mut self, _now_ns: u64) {
            self.flushes += 1;
        }
    }

    #[test]
    fn capture_hooks_fire_at_documented_sites() {
        let mut t = TestTransport::with_capacity(16 * 1024);
        let mut d = build_driver(7, 42);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut cap = CountingCapture::default();

        let good = br#"{"u":400900217,"s":"BTCUSDT","b":"25.35190000","B":"31.21","a":"25.36520000","A":"40.66"}"#;
        let bad = b"not-json-not-anything";

        let mut frame = [0u8; 256];
        frame[0] = 0x81;
        frame[1] = good.len() as u8;
        frame[2..2 + good.len()].copy_from_slice(good);
        t.inject_incoming(&frame[..2 + good.len()]);

        let mut frame2 = [0u8; 64];
        frame2[0] = 0x81;
        frame2[1] = bad.len() as u8;
        frame2[2..2 + bad.len()].copy_from_slice(bad);
        t.inject_incoming(&frame2[..2 + bad.len()]);

        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(cap.raw_frames, 2, "every payload tapped pre-parse");
        assert_eq!(cap.ticks, 1, "good bookTicker captured as tick");
        assert_eq!(cap.rejects, 1, "garbled payload tapped as reject");
        assert_eq!(status.parse_errors_total(), 1);

        // Tick still captured when the ring is full: fill it, resend.
        let filler = Tick::new(
            0,
            core_types::VenueId::Binance,
            42u32,
            0,
            core_types::Price::from_raw(1),
            core_types::Qty::from_raw(1),
            core_types::Price::from_raw(2),
            core_types::Qty::from_raw(1),
        );
        while prod.try_push(filler).is_ok() {}
        let mut frame3 = [0u8; 256];
        frame3[0] = 0x81;
        frame3[1] = good.len() as u8;
        frame3[2..2 + good.len()].copy_from_slice(good);
        t.inject_incoming(&frame3[..2 + good.len()]);
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.ticks, 2, "ring-dropped tick still captured");
        assert_eq!(status.ring_drops_total(), 1);
    }

    #[test]
    fn mark_price_slot_emits_mark_and_funding_events() {
        // WS5: a perp markPrice frame → Mark event (v0 = mark ×1e6,
        // v1 = index ×1e6) + Funding event (v0 = rate ×1e9, v1 =
        // next-funding ms). Capture-only — the ring stays empty.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new_mark_price(7, 42);
        d.set_state(State::Steady);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut cap = CountingCapture::default();

        let mark = br#"{"e":"markPriceUpdate","E":1562305380000,"s":"BTCUSDT","p":"11794.15","i":"11784.62","P":"11784.25","r":"0.00038167","T":1562306400000}"#;
        t.inject_incoming(&ws_text_frame(mark));
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(cap.events.len(), 2, "Mark + Funding");
        let m = &cap.events[0];
        assert_eq!(m.channel, core_types::ChannelId::Mark as u8);
        assert_eq!(m.sym, 42);
        assert_eq!(m.v0, 11_794_150_000);
        assert_eq!(m.v1, 11_784_620_000, "BN Mark carries index in v1");
        assert_eq!(m.venue_time_ms, 1_562_305_380_000);
        let fu = &cap.events[1];
        assert_eq!(fu.channel, core_types::ChannelId::Funding as u8);
        assert_eq!(fu.v0, 381_670);
        assert_eq!(fu.v1, 1_562_306_400_000);
        assert!(cons.try_pop().is_none(), "capture-only: nothing rings");
        assert_eq!(status.ticks_total(), 1, "market-data row counted");
        assert_eq!(status.parse_errors_total(), 0);
    }

    /// WS10-A: the perp markPrice Funding event reaches the venue-
    /// event lane; the Mark event stays capture-only (mask gates per
    /// channel); a DATED frame (`"r":""`) puts NOTHING on the lane.
    #[test]
    fn funding_event_reaches_the_event_lane_dated_stays_off() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new_mark_price(7, 42);
        d.set_state(State::Steady);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut cap = CountingCapture::default();
        let (mut etx, mut erx) = event_ring_pair();

        let mark = br#"{"e":"markPriceUpdate","E":1562305380000,"s":"BTCUSDT","p":"11794.15","i":"11784.62","P":"11784.25","r":"0.00038167","T":1562306400000}"#;
        t.inject_incoming(&ws_text_frame(mark));
        super::drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut opt_ring_pair().0,
            &status,
            &mut cap,
        )
        .unwrap();

        let ev = erx.try_pop().expect("funding event on the lane");
        assert_eq!(ev.channel, core_types::ChannelId::Funding as u8);
        assert_eq!(ev.v0, 381_670, "rate ×1e9");
        assert_eq!(ev.v1, 1_562_306_400_000, "next funding ms");
        assert!(
            erx.try_pop().is_none(),
            "Mark event is NOT on the lane (mask gates per channel)"
        );
        assert_eq!(status.event_ring_drops_total(), 0);

        // Dated frame: has_funding = 0 ⇒ nothing new on the lane.
        let dated = br#"{"e":"markPriceUpdate","E":1000,"s":"BTCUSDT_260327","p":"65000.1","i":"64999.9","P":"65000.0","r":"","T":0}"#;
        t.inject_incoming(&ws_text_frame(dated));
        super::drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut opt_ring_pair().0,
            &status,
            &mut cap,
        )
        .unwrap();
        assert!(erx.try_pop().is_none(), "dated future never funds");
    }

    #[test]
    fn mark_price_slot_dated_future_emits_mark_only() {
        // WS5/WS3 convention: `"r":""` (delivery contract) ⇒ no
        // Funding event, Mark still captured.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new_mark_price(7, 77);
        d.set_state(State::Steady);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut cap = CountingCapture::default();

        let dated = br#"{"e":"markPriceUpdate","E":1000,"s":"BTCUSDT_260327","p":"65000.1","i":"64999.9","P":"65000.0","r":"","T":0}"#;
        t.inject_incoming(&ws_text_frame(dated));
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.events.len(), 1, "Mark only");
        assert_eq!(cap.events[0].channel, core_types::ChannelId::Mark as u8);
        assert_eq!(status.parse_errors_total(), 0);
    }

    #[test]
    fn mark_price_slot_rejects_foreign_frames() {
        // A bookTicker payload on a markPrice slot is a tapped reject
        // (the required "e" tag) — never a silent mis-parse.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new_mark_price(7, 42);
        d.set_state(State::Steady);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut cap = CountingCapture::default();

        let bt = br#"{"u":1,"s":"BTCUSDT","b":"25.10","B":"1.0","a":"25.20","A":"1.0"}"#;
        t.inject_incoming(&ws_text_frame(bt));
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.rejects, 1);
        assert_eq!(status.parse_errors_total(), 1);
        assert!(cap.events.is_empty());
    }
}
