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
    ws_write_pong, ws_write_text_frame_parts, HandshakeResult, Status, Transport, WsOpcode, WsReadResult,
};
use core_ring::Producer;
use core_time::{now_ns, FeedClock};
use core_types::{
    Capture, ChannelEvent, ChannelId, NsTs, OptSummary, Price, Qty, SymbolId, Tick,
    EVENT_RING_SIZE, OPT_RING_SIZE, TICK_FLAG_STALE, TICK_FLAG_VENUE_TIME_SENTINEL,
};

use crate::{parse_book_ticker, parse_trade, BookTickerFrame};

// ---------------------------------------------------------------
// Configuration + sizing
// ---------------------------------------------------------------

/// Size of the rx byte buffer. Binance `@bookTicker` frames are ~140 B
/// each; 64 KiB accommodates huge bursts without ever reallocating.
pub const RX_BUF_SIZE: usize = 64 * 1024;

/// Rx sizing for the options slot (BX0-F2): every push is ONE frame
/// holding a whole underlying's listed chain — measured 2026-09-23 at
/// 245.6 KB for BTC (752 options) and 194 KB for ETH (600), one each
/// per ~1 s, landing back to back. A frame only parses once it is
/// whole and contiguous in this buffer, and one larger than the
/// buffer would stall the slot until the idle timeout, so 2 MiB is
/// 8× the largest push with room for the chain to keep growing (boot
/// alloc, one slot).
pub const EAPI_RX_BUF_SIZE: usize = 2 * 1024 * 1024;

/// Longest reject record the options slot taps (BX0-F2): enough bytes
/// at the fault to identify it, not the ~246 KB push that held it — a
/// whole push would cost a quarter-megabyte copy on the thread that
/// also serves every bookTicker, and could spend the tap's budget in
/// one record.
pub const EAPI_REJECT_TAP_MAX: usize = 512;

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
            // Runs only when the tail reaches the end with bytes unread:
            // a slot drained to empty resets to 0 without copying, which
            // the options pushes' idle gaps usually allow. Alternatives
            // weighed: a split-slice ring (a second path in every
            // scanner), a double-mapped mirror ring (platform VM calls),
            // compacting at header time (a second header parse per
            // partial read on every slot, to shrink a rare copy).
            // COPY: the unread bytes, < the slot's buffer (64 KiB; 2 MiB
            // on the options slot — most of a ~246 KB push at worst) —
            // the scanners borrow ONE contiguous frame, and one
            // straddling the end cannot be borrowed from two places —
            // rejected: the three alternatives above.
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
    /// WS5: `/market/ws/<symbol>@markPrice` — USDS-M mark/index/
    /// funding stream on fstream's ROUTED `/market` path (BX0-F1: the
    /// legacy `/ws/` URL stopped carrying `/market` streams on
    /// 2026-04-23 and upgrades to a silent socket). `sym` pinned on the
    /// driver, the bookTicker shape.
    /// Capture-only: `ChannelId::Mark` + (perps) `ChannelId::Funding`
    /// events; nothing reaches the engine ring until the WS10
    /// funding carrier lands.
    MarkPrice,
    /// M2.4 / BX0-F2 options combined stream (`<uly>@optionMarkPrice`
    /// per underlying on fstream `/market`): each push is one array of
    /// the whole chain; the table's rows yield BBO → `Tick` and
    /// mark/IV/greeks/index → `OptSummary`.
    Eapi(crate::eapi::EapiSymbolTable),
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
    /// judge for `bookTicker` (USDS-M `T`/`E` directly; spot through the
    /// aggTrade sentinel below). One per connection by doctrine — the
    /// multi-conn lane owns one driver per socket; reset on reconnect;
    /// threshold = venue default or `--stale-after-ms bn:<ms>` via
    /// [`Self::set_stale_after_ms`].
    feed_clock: FeedClock,
    /// VT2 spot SENTINEL (docs/venue-time-capture-plan.md §4): spot
    /// `bookTicker` carries no venue stamp, so a spot slot also
    /// subscribes `<sym>@aggTrade` on the SAME socket after the upgrade;
    /// each aggTrade's `T` teaches [`Self::feed_clock`], and every
    /// bookTicker tick inherits the sentinel's latest stamp + verdict
    /// with `TICK_FLAG_VENUE_TIME_SENTINEL` set. `false` ⇒ not a
    /// sentinel slot (USDS-M / legacy). Nothing else is stored: the
    /// SUBSCRIBE is written at each (re)connect from the slot's own
    /// path ([`queue_sentinel_subscribe`]).
    sentinel: bool,
    /// Latest sentinel stamp (ms; 0 = none seen this connection).
    sentinel_time_ms: u64,
    /// Latest sentinel verdict (`TICK_FLAG_STALE` or 0).
    sentinel_stale_flag: u8,
    /// `!Sync` marker.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

/// The sentinel's stream suffix (`btcusdt` → `btcusdt@aggTrade`).
const SENTINEL_SUFFIX: &[u8] = b"@aggTrade";
/// The SUBSCRIBE request before the sentinel's stream name …
const SENTINEL_SUB_HEAD: &[u8] = b"{\"method\":\"SUBSCRIBE\",\"params\":[\"";
/// … and after it.
const SENTINEL_SUB_TAIL: &[u8] = b"\"],\"id\":1}";

/// VT2: the stream symbol a spot sentinel slot subscribes for — its
/// path's last segment up to the `@` (`/ws/btcusdt@bookTicker` →
/// `btcusdt`), borrowed from the path. `None` when the path names no
/// stream (no `@`, or nothing before it).
#[inline]
fn sentinel_symbol(path: &[u8]) -> Option<&[u8]> {
    let seg = match memchr::memrchr(b'/', path) {
        Some(i) => &path[i + 1..],
        None => path,
    };
    match memchr::memchr(b'@', seg) {
        Some(at) if at > 0 => Some(&seg[..at]),
        _ => None,
    }
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
            sentinel: false,
            sentinel_time_ms: 0,
            sentinel_stale_flag: 0,
            _not_sync: ::core::marker::PhantomData,
        }
    }

    /// VT2: a SPOT bookTicker slot with the aggTrade sentinel — the
    /// slot subscribes `<symbol>@aggTrade` on the same socket right
    /// after the upgrade, the symbol read from its own endpoint path
    /// (`/ws/<symbol>@bookTicker`) at each (re)connect.
    pub fn new_spot_sentinel(nonce_seed: u64, sym: SymbolId) -> Self {
        let mut d = Self::new(nonce_seed, sym);
        d.sentinel = true;
        d
    }

    /// VT2: true when this slot carries the aggTrade sentinel.
    #[inline]
    pub fn has_sentinel(&self) -> bool {
        self.sentinel
    }

    /// VT2: the sentinel's latest stamp (ms; 0 = none this connection).
    #[inline]
    pub fn sentinel_time_ms(&self) -> u64 {
        self.sentinel_time_ms
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

    /// M2.4 / BX0-F2: an options combined-stream slot carrying the
    /// selected chain's table. RX is sized for the push — a whole
    /// underlying's chain per frame ([`EAPI_RX_BUF_SIZE`]).
    pub fn new_eapi(nonce_seed: u64, table: crate::eapi::EapiSymbolTable) -> Self {
        let sec_key = sec_websocket_key_from_seed(nonce_seed);
        let accept = expected_accept(&sec_key);
        // COPY: the 2 568 B boot table (64 rows × 40 B + its length)
        // moves into the slot's lane at boot — after moving through the
        // spec, and before moving on with the Driver (`MultiConn::new`)
        // — the lane holds it INLINE so every per-element lookup walks
        // contiguous rows with no pointer to chase — rejected: a `Box`
        // (a heap hop on the options path, and the crate's no-`Box`
        // rule).
        Self {
            state: State::Connecting,
            rx: IoBuf::with_capacity(EAPI_RX_BUF_SIZE),
            tx: IoBuf::with_capacity(TX_BUF_SIZE),
            sec_key,
            expected_accept_val: accept,
            last_activity_ns: 0,
            mask_counter: 0,
            sym: 0,
            lane: StreamLane::Eapi(table),
            // Options tickers are never judged (no bookTicker on this
            // slot); the estimator sits idle.
            feed_clock: FeedClock::new(core_types::VenueId::Binance.default_stale_after_ms()),
            sentinel: false,
            sentinel_time_ms: 0,
            sentinel_stale_flag: 0,
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
        // The sentinel's last stamp is connection-scoped too.
        self.feed_clock.reset();
        self.sentinel_time_ms = 0;
        self.sentinel_stale_flag = 0;
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
            advance_ws_upgrade(drv, path, status)?;
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

/// VT2: queue the sentinel's live SUBSCRIBE (`{"method":"SUBSCRIBE",
/// "params":["<sym>@aggTrade"],"id":1}`) onto the connection's tx.
/// Zero-copy: the stream symbol is read from the slot's own path
/// ([`sentinel_symbol`]) and the request's four parts go straight into
/// the masked frame ([`ws_write_text_frame_parts`]) — nothing is
/// assembled first and nothing is stored. The venue's ack
/// (`{"result":null,"id":1}`) is classified as a control frame.
fn queue_sentinel_subscribe(drv: &mut Driver, path: &[u8]) -> io::Result<()> {
    let Some(symbol) = sentinel_symbol(path) else {
        // The boot makes a sentinel slot only for a `/ws/<sym>@bookTicker`
        // path: anything else is a boot bug — run the slot sentinel-less
        // rather than send a broken SUBSCRIBE.
        debug_assert!(false, "sentinel slot path names no stream symbol");
        drv.sentinel = false;
        return Ok(());
    };
    // Same masked-frame shape as this crate's ping/pong writes (the
    // crate keeps its own private IoBuf).
    let mask = ws_mask_from_counter(drv.mask_counter);
    drv.mask_counter = drv.mask_counter.wrapping_add(1);
    let n = ws_write_text_frame_parts(
        drv.tx.free_mut(),
        &[SENTINEL_SUB_HEAD, symbol, SENTINEL_SUFFIX, SENTINEL_SUB_TAIL],
        mask,
    )
    .map_err(|_| io::Error::other("sentinel subscribe: tx buffer too small"))?;
    drv.tx.advance(n);
    Ok(())
}

/// VT2 sentinel-slot frame kinds (spot bookTicker connections only;
/// every other slot keeps its single-stream shape).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SentinelFrame {
    /// `{"e":"aggTrade",…}` — the clock sentinel + a Trade capture row.
    AggTrade,
    /// `{"result":null,"id":1}` / `{"error":…}` — the SUBSCRIBE reply.
    Control,
    /// Anything else on the socket is the bookTicker stream.
    BookTicker,
}

#[inline]
fn classify_sentinel_frame(payload: &[u8]) -> SentinelFrame {
    if memchr::memmem::find(payload, b"\"e\":\"aggTrade\"").is_some() {
        SentinelFrame::AggTrade
    } else if payload.starts_with(b"{\"result\":") || payload.starts_with(b"{\"error\":") {
        SentinelFrame::Control
    } else {
        SentinelFrame::BookTicker
    }
}

fn advance_ws_upgrade(
    drv: &mut Driver,
    path: &[u8],
    status: &core_metrics::IngressStatus,
) -> io::Result<()> {
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
            // VT2: the spot sentinel subscribes on the same socket the
            // moment the upgrade lands (the next flush sends it).
            if drv.has_sentinel() {
                queue_sentinel_subscribe(drv, path)?;
            }
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
                        // The echo goes straight from rx into tx (disjoint
                        // field borrows) — no scratch; `ws_read_frame`
                        // already refused a control payload over 125 B.
                        if let Ok(n) = ws_write_pong(
                            drv.tx.free_mut(),
                            &drv.rx.filled()[payload.start..payload.end],
                            mask,
                        ) {
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

/// M2.4 / BX0-F2: handle one options frame —
/// `{"stream":"<uly>@optionMarkPrice","data":[{…},…]}`, the whole
/// listed chain of one underlying (752 BTC elements, ~246 KB, about
/// once a second). One pass over the array IN PLACE: each element
/// whose `"s"` is in the boot table yields an `OptSummary` (always)
/// and a `Tick` (when a side is quoted); every other element costs one
/// symbol compare. The only bytes that leave rx are the PODs.
///
/// A frame that is not this stream, or not an array, is ONE rejection;
/// a selected element that fails its fields is one rejection of that
/// element; a walk that turns malformed mid-array keeps the rows it
/// already emitted — each was parsed from its own complete element —
/// and counts one rejection. A reject taps the element, or at most
/// [`EAPI_REJECT_TAP_MAX`] bytes from where the walk stopped — never
/// the quarter-megabyte push (`--raw-tap bn` in All mode still holds
/// every whole frame).
fn handle_eapi_frame<C: Capture>(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
    status: &core_metrics::IngressStatus,
    capture: &mut C,
) {
    // Disjoint field borrows: the frame is a view into `rx`, the table
    // is read-only; nothing on the driver is written.
    let Driver { rx, lane, .. } = drv;
    let payload = &rx.filled()[payload_range];
    capture.raw_frame(now_ns(), payload);
    let StreamLane::Eapi(table) = lane else {
        debug_assert!(false, "options handler on a non-options slot");
        return;
    };
    let array = match crate::eapi::split_combined(payload) {
        Some((stream, data)) if stream.ends_with(crate::eapi::EAPI_MARK_STREAM.as_bytes()) => {
            crate::eapi::EapiArrayCursor::new(data)
        }
        _ => None,
    };
    let Some(mut cur) = array else {
        status.inc_parse_errors();
        capture.parse_reject(now_ns(), &payload[..payload.len().min(EAPI_REJECT_TAP_MAX)]);
        return;
    };
    let ts_ns = now_ns();
    let mut rows = 0u64;
    // One parse target for the whole walk (88 B — filled in place,
    // never returned by value).
    let mut f = crate::eapi::EapiMarkFrame::ZERO;
    loop {
        let elem = match cur.next_elem() {
            crate::eapi::ArrayStep::Elem(e) => e,
            crate::eapi::ArrayStep::End => break,
            crate::eapi::ArrayStep::Malformed => {
                status.inc_parse_errors();
                let at = cur.rest();
                capture.parse_reject(now_ns(), &at[..at.len().min(EAPI_REJECT_TAP_MAX)]);
                break;
            }
        };
        let sym = match crate::eapi::eapi_elem_symbol(elem) {
            Some(s) => match table.lookup(s) {
                Some(sym) => sym,
                // Not in the selected chain — the common case.
                None => continue,
            },
            None => {
                status.inc_parse_errors();
                capture.parse_reject(now_ns(), elem);
                continue;
            }
        };
        if !crate::eapi::parse_eapi_mark(elem, &mut f) {
            status.inc_parse_errors();
            capture.parse_reject(now_ns(), elem);
            continue;
        }
        let summary = OptSummary::new(
            ts_ns,
            core_types::VenueId::Binance,
            sym,
            // No open interest on this stream — MARK_PX only
            // (docs/wire-format.md flags law).
            core_types::OPT_SUMMARY_FLAG_MARK_PX,
            f.mark_px_1e9,
            f.mark_iv_1e9,
            f.index_px_1e9,
            0,
            f.delta_1e9,
            f.gamma_1e9,
            f.vega_1e6,
            f.theta_1e6,
        );
        // §6.5: capture first. VM2 V2: the summary also rides the opt
        // lane (the kind-6 channel's engine entry).
        capture.opt_summary(&summary);
        if !opt_tx.try_push_ref(&summary) {
            status.inc_opt_ring_drops();
        }
        if f.bid_px_1e6 != 0 || f.ask_px_1e6 != 0 {
            let tick = Tick::new(
                ts_ns,
                core_types::VenueId::Binance,
                sym,
                // The mark array carries no venue sequence.
                0,
                Price::from_raw(f.bid_px_1e6),
                Qty::from_raw(f.bid_qty_1e6),
                Price::from_raw(f.ask_px_1e6),
                Qty::from_raw(f.ask_qty_1e6),
            );
            capture.tick(&tick);
            if !producer.try_push_ref(&tick) {
                status.inc_ring_drops();
            }
        }
        rows += 1;
    }
    status.add_msgs(1);
    status.add_ticks(rows);
}

/// WS5: handle one `@markPrice` frame — capture-only events, the
/// OKX Mark/Funding conventions (`Mark` v0 = mark ×1e6; on this
/// venue v1 = index ×1e6, where OKX leaves 0; `Funding` v0 = rate
/// ×1e9, v1 = next-funding ms — gated on wire truth, the WS3
/// `has_funding` split: a dated contract pushes `"r":""` (WS5-era) or
/// `"r":"0.00000000","T":0` (live since at least 2026-09-23) and
/// writes no Funding row).
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
    // Parsed in place: the 64 B frame inside an `Option` would be 128 B
    // by value.
    let mut f = crate::BnMarkPriceFrame::ZERO;
    if !crate::parse_mark_price(payload, drv.sym, &mut f) {
        status.inc_parse_errors();
        capture.parse_reject(now_ns(), payload);
        return;
    }
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
        // WS10-A: onto the venue-event lane (capture stays first —
        // §6.5 capture-before-push law).
        if event_mask & core_types::event_lane_bit(core_types::ChannelId::Funding) != 0
            && !event_tx.try_push_ref(&ev)
        {
            status.inc_event_ring_drops();
        }
    }
    status.add_msgs(1);
    status.add_ticks(1);
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
    // Disjoint field borrows: the payload is a view into `drv.rx`; the
    // VT2 judge and the sentinel state are the only mutations.
    let Driver {
        rx,
        sym: slot_sym,
        feed_clock,
        sentinel,
        sentinel_time_ms,
        sentinel_stale_flag,
        ..
    } = drv;
    let payload = &rx.filled()[payload_range];
    // §6.5 capture: raw tap fires before parsing.
    capture.raw_frame(now_ns(), payload);
    // VT2 sentinel slot: the socket also carries aggTrade prints and
    // the SUBSCRIBE reply; one substring probe per frame sorts them.
    if *sentinel {
        match classify_sentinel_frame(payload) {
            SentinelFrame::AggTrade => {
                match parse_trade(payload, *slot_sym) {
                    Some(t) => {
                        let now = now_ns();
                        // The print's `T` teaches the connection clock;
                        // the verdict is what the next bookTicker ticks
                        // inherit until the next print.
                        let judged = feed_clock.judge(t.ts_ms, now);
                        *sentinel_time_ms = t.ts_ms;
                        *sentinel_stale_flag = (judged.stale as u8) * TICK_FLAG_STALE;
                        status.set_feed_delay_ema_ms(feed_clock.delay_ema_ms());
                        // §6.5 capture as a Trade row (v0 px ×1e6, v1
                        // qty ×1e6 negated when the aggressor sold —
                        // the cross-venue convention); capture only,
                        // no engine lane (the trade lane is deferred).
                        let signed_qty = if t.is_buyer_maker { -t.qty_1e6 } else { t.qty_1e6 };
                        capture.event(&ChannelEvent::new(
                            now,
                            core_types::VenueId::Binance,
                            ChannelId::Trade,
                            t.sym,
                            t.agg_id,
                            t.ts_ms,
                            t.price_1e6,
                            signed_qty,
                        ));
                        status.add_msgs(1);
                        status.add_ticks(1);
                    }
                    None => {
                        status.inc_parse_errors();
                        capture.parse_reject(now_ns(), payload);
                    }
                }
                return;
            }
            SentinelFrame::Control => {
                // The SUBSCRIBE ack (or a venue error naming it) — a
                // message, not data, not a rejection.
                status.add_msgs(1);
                return;
            }
            SentinelFrame::BookTicker => {}
        }
    }
    // Parsed in place: the 64 B frame inside an `Option` would be 128 B
    // by value.
    let mut f = BookTickerFrame::ZERO;
    if parse_book_ticker(payload, *slot_sym, &mut f) {
        let ts_ns = now_ns();
        // VT2: one parse-complete stamp serves the judgement and the
        // tick. A direct stamp (USDS-M `T`/`E`) is judged here; a spot
        // push carries none and inherits the sentinel's latest stamp +
        // verdict (bit1 marks the inference; no sentinel print yet ⇒
        // 0 / never stale, the v2 law).
        let (venue_time_ms, flags) = if f.venue_time_ms != 0 {
            let judged = feed_clock.judge(f.venue_time_ms, ts_ns);
            (f.venue_time_ms, (judged.stale as u8) * TICK_FLAG_STALE)
        } else if *sentinel_time_ms != 0 {
            (
                *sentinel_time_ms,
                *sentinel_stale_flag | TICK_FLAG_VENUE_TIME_SENTINEL,
            )
        } else {
            (0, 0)
        };
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
            venue_time_ms,
            flags,
        );
        if tick.is_stale() {
            status.inc_stale_ticks();
        }
        status.set_feed_delay_ema_ms(feed_clock.delay_ema_ms());
        // §6.5 capture BEFORE the push — a ring-dropped tick must still
        // reach the replay log (the audit pairs capture counts with
        // ring_drops_total).
        capture.tick(&tick);
        // D4: a full ring is data loss — count it, never block on it.
        if !producer.try_push_ref(&tick) {
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
            let n_before = producer.published();
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
            if producer.published() == n_before && drv.state() == state_before {
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
pub struct MultiConn<'a, T: Transport> {
    /// Live transport; `None` while the slot awaits a reconnect.
    transport: Option<T>,
    /// Per-connection WS state machine (sym pinned inside).
    drv: Driver,
    /// Host bytes for the `Host:` header (spot vs USDS-M hosts
    /// differ — each slot carries its own), borrowed from the boot's
    /// endpoint list, which outlives the loop.
    host: &'a [u8],
    /// Request path (`/ws/<symbol>@bookTicker`), borrowed likewise.
    path: &'a [u8],
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

impl<'a, T: Transport> MultiConn<'a, T> {
    /// New slot, initially disconnected (the loop's reconnect pass
    /// dials it; `next_attempt_ns` 0 = due immediately). `host` and
    /// `path` are borrowed, not copied: the boot keeps its endpoint list
    /// alive for as long as the loop runs.
    pub fn new(
        drv: Driver,
        host: &'a [u8],
        path: &'a [u8],
        keepalive: core_net::Keepalive,
        backoff: core_net::Backoff,
    ) -> Self {
        // COPY: the Driver (≈ 2.9 KB: every slot's `StreamLane` is sized
        // for the options table it carries inline) moves into its slot
        // here and, with the slot, into the boot's Vec — 2–4 moves per
        // connection, once, at boot, well under a millisecond for the
        // whole lane — rejected: a two-phase in-place init behind `&mut`
        // (every constructor split, for a boot-only cost) and boxing the
        // table (a heap hop on the options path).
        Self {
            transport: None,
            drv,
            host,
            path,
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
    conns: &mut [MultiConn<'_, T>],
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
                        // COPY: the new transport (rustls' ClientConnection
                        // held inline, several hundred bytes) moves from
                        // `connect` into its slot — once per reconnect,
                        // beside a TCP + TLS handshake that costs orders of
                        // magnitude more — rejected: a placement API on
                        // core-net's connect, for one move per reconnect.
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
                let n_before = producer.published();
                let state_before = c.drv.state();
                if drive_one(
                    t, &mut c.drv, c.host, c.path, producer, event_tx, event_mask, opt_tx,
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
                if producer.published() == n_before && c.drv.state() == state_before {
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
        // A plain slot subscribes nothing after the upgrade.
        assert_eq!(t.outgoing_len(), 0, "no sentinel ⇒ no SUBSCRIBE frame");
    }

    /// VT2: a spot sentinel slot queues `<sym>@aggTrade` on the same
    /// socket the moment the upgrade lands.
    #[test]
    fn sentinel_slot_subscribes_agg_trade_after_upgrade() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new_spot_sentinel(42, 7);
        assert!(d.has_sentinel());
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"host", b"/ws/btcusdt@bookTicker", &mut prod, &status, &mut NullCapture).unwrap();
        let mut scratch = [0u8; 4096];
        let _ = t.drain_outgoing(&mut scratch);
        let key = sec_key_pub(42);
        let resp = build_server_response(&expected_accept_pub(&key));
        t.inject_incoming(&resp);
        drive_one(&mut t, &mut d, b"host", b"/ws/btcusdt@bookTicker", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.state(), State::Steady);
        // The next drive flushes the queued SUBSCRIBE (masked text frame).
        drive_one(&mut t, &mut d, b"host", b"/ws/btcusdt@bookTicker", &mut prod, &status, &mut NullCapture).unwrap();
        let n = t.drain_outgoing(&mut scratch);
        assert!(n > 0, "SUBSCRIBE must be sent");
        assert_eq!(scratch[0], 0x81, "FIN + text");
        assert_ne!(scratch[1] & 0x80, 0, "client frames are masked");
        let len = (scratch[1] & 0x7F) as usize;
        let mask = [scratch[2], scratch[3], scratch[4], scratch[5]];
        let mut body = scratch[6..6 + len].to_vec();
        for (i, b) in body.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
        assert_eq!(
            body,
            br#"{"method":"SUBSCRIBE","params":["btcusdt@aggTrade"],"id":1}"#
        );
    }

    /// VT2 / BX0: the sentinel's stream symbol is read from the slot's
    /// own path — its last segment up to the `@` — and borrowed.
    #[test]
    fn sentinel_symbol_is_read_from_the_path() {
        assert_eq!(sentinel_symbol(b"/ws/btcusdt@bookTicker"), Some(&b"btcusdt"[..]));
        assert_eq!(sentinel_symbol(b"/public/ws/ethusdt@bookTicker"), Some(&b"ethusdt"[..]));
        let path: &[u8] = b"/ws/solusdt@bookTicker";
        let s = sentinel_symbol(path).unwrap();
        assert!(path.as_ptr_range().contains(&s.as_ptr()), "borrowed, not copied");
        assert_eq!(sentinel_symbol(b"/ws/btcusdt"), None, "no stream suffix");
        assert_eq!(sentinel_symbol(b"/ws/@bookTicker"), None, "empty symbol");
    }

    /// A sentinel slot whose path names no stream runs sentinel-less (a
    /// boot bug, debug-asserted) instead of sending a broken SUBSCRIBE.
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "names no stream symbol"))]
    fn a_sentinel_path_without_a_symbol_subscribes_nothing() {
        let mut d = Driver::new_spot_sentinel(1, 7);
        assert!(queue_sentinel_subscribe(&mut d, b"/ws/btcusdt").is_ok());
        assert!(!d.has_sentinel());
        assert_eq!(d.tx.filled().len(), 0);
    }

    /// VT2 helper: one spot bookTicker push (no stamp) on a sentinel
    /// slot; returns its tick.
    fn push_spot_book_ticker(
        t: &mut TestTransport,
        d: &mut Driver,
        prod: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
        cons: &mut core_ring::Consumer<Tick, DEFAULT_TICK_RING_CAP>,
        status: &core_metrics::IngressStatus,
        u: u64,
    ) -> Tick {
        let s = format!(r#"{{"u":{u},"s":"BTCUSDT","b":"25.35","B":"31.21","a":"25.36","A":"40.66"}}"#);
        t.inject_incoming(&ws_text_frame(s.as_bytes()));
        drive_one(t, d, b"host", b"/", prod, status, &mut NullCapture).unwrap();
        *cons.try_pop_ref().expect("bookTicker must produce a tick")
    }

    /// VT2 helper: one aggTrade print stamped `T = t_ms` on the same
    /// slot (captured, never a tick).
    fn push_agg_trade<C: Capture>(
        t: &mut TestTransport,
        d: &mut Driver,
        prod: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
        status: &core_metrics::IngressStatus,
        capture: &mut C,
        t_ms: u64,
        agg_id: u64,
        buyer_is_maker: bool,
    ) {
        let s = format!(
            r#"{{"e":"aggTrade","E":{},"s":"BTCUSDT","a":{agg_id},"p":"25.35","q":"0.5","f":1,"l":2,"T":{t_ms},"m":{buyer_is_maker},"M":true}}"#,
            t_ms + 1
        );
        t.inject_incoming(&ws_text_frame(s.as_bytes()));
        drive_one(t, d, b"host", b"/", prod, status, capture).unwrap();
    }

    #[test]
    fn spot_book_ticker_inherits_the_sentinel_stamp_and_verdict() {
        // VT2: before any print the spot tick is "unknown, never stale";
        // after a fresh print it inherits that stamp with bit1; after a
        // 5 s-older print it inherits STALE | SENTINEL; a fresh print
        // clears it again. aggTrades never produce ticks.
        let mut t = TestTransport::with_capacity(16 * 1024);
        let mut d = Driver::new_spot_sentinel(7, 42);
        d.set_state(State::Steady);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let t0: u64 = 1_755_216_000_000;

        let unknown = push_spot_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, 1);
        assert_eq!(unknown.venue_time_ms, 0);
        assert_eq!(unknown.flags, 0);

        push_agg_trade(&mut t, &mut d, &mut prod, &status, &mut NullCapture, t0, 100, false);
        assert!(cons.try_pop_ref().is_none(), "a print is never a tick");
        assert_eq!(d.sentinel_time_ms(), t0);
        let fresh = push_spot_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, 2);
        assert_eq!(fresh.venue_time_ms, t0, "inherited from the sentinel");
        assert_eq!(fresh.flags, TICK_FLAG_VENUE_TIME_SENTINEL);
        assert!(!fresh.is_stale());
        assert_eq!(status.stale_ticks_total(), 0);

        push_agg_trade(&mut t, &mut d, &mut prod, &status, &mut NullCapture, t0 - 5_000, 101, true);
        let stale = push_spot_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, 3);
        assert_eq!(stale.venue_time_ms, t0 - 5_000);
        assert_eq!(stale.flags, TICK_FLAG_STALE | TICK_FLAG_VENUE_TIME_SENTINEL);
        assert!(stale.is_stale());
        assert_eq!(status.stale_ticks_total(), 1);
        // every bookTicker between prints inherits the same verdict
        let stale2 = push_spot_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, 4);
        assert!(stale2.is_stale());
        assert_eq!(status.stale_ticks_total(), 2);

        push_agg_trade(&mut t, &mut d, &mut prod, &status, &mut NullCapture, t0 + 10, 102, false);
        let again = push_spot_book_ticker(&mut t, &mut d, &mut prod, &mut cons, &status, 5);
        assert_eq!(again.flags, TICK_FLAG_VENUE_TIME_SENTINEL);
        assert!(!again.is_stale());
        assert_eq!(status.stale_ticks_total(), 2);
        // 5 bookTicker ticks + 3 prints = 8 data rows; no rejects.
        assert_eq!(status.ticks_total(), 8);
        assert_eq!(status.parse_errors_total(), 0);
    }

    #[test]
    fn sentinel_prints_are_captured_as_signed_trade_events_and_acks_are_quiet() {
        struct EventCap {
            events: Vec<core_types::ChannelEvent>,
            rejects: u32,
        }
        impl Capture for EventCap {
            fn tick(&mut self, _t: &Tick) {}
            fn event(&mut self, e: &core_types::ChannelEvent) {
                self.events.push(*e);
            }
            fn raw_frame(&mut self, _ts: NsTs, _p: &[u8]) {}
            fn parse_reject(&mut self, _ts: NsTs, _p: &[u8]) {
                self.rejects += 1;
            }
        }
        let mut t = TestTransport::with_capacity(16 * 1024);
        let mut d = Driver::new_spot_sentinel(7, 42);
        d.set_state(State::Steady);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut cap = EventCap {
            events: Vec::new(),
            rejects: 0,
        };
        // The SUBSCRIBE ack: a message, not data, not a reject.
        t.inject_incoming(&ws_text_frame(br#"{"result":null,"id":1}"#));
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.ticks_total(), 0);
        assert_eq!(cap.rejects, 0);
        assert!(cons.try_pop_ref().is_none());

        push_agg_trade(&mut t, &mut d, &mut prod, &status, &mut cap, 1_755_216_000_000, 26_129, true);
        push_agg_trade(&mut t, &mut d, &mut prod, &status, &mut cap, 1_755_216_000_050, 26_130, false);
        assert_eq!(cap.events.len(), 2);
        let sell = &cap.events[0];
        assert_eq!(sell.venue, core_types::VenueId::Binance as u8);
        assert_eq!(sell.channel, ChannelId::Trade as u8);
        assert_eq!(sell.sym, 42);
        assert_eq!(sell.venue_seq, 26_129);
        assert_eq!(sell.venue_time_ms, 1_755_216_000_000);
        assert_eq!(sell.v0, 25_350_000, "px ×1e6");
        assert_eq!(sell.v1, -500_000, "m:true = the aggressor sold ⇒ negated qty");
        assert_eq!(cap.events[1].v1, 500_000, "m:false = the aggressor bought");
        assert!(
            cons.try_pop_ref().is_none(),
            "prints never reach the tick ring"
        );
        assert_eq!(cap.rejects, 0);
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
        // All three length forms: 7-bit, 16-bit (combined payloads
        // exceed 125 bytes) and 64-bit (a whole-chain options push is
        // ~250 KB — BX0-F2).
        let mut f = Vec::with_capacity(10 + payload.len());
        f.push(0x81);
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

        let first = cons.try_pop_ref().unwrap().sym;
        let mut syms = [first, cons.try_pop_ref().unwrap().sym];
        syms.sort_unstable();
        assert_eq!(syms, [7, 42]);
        assert!(cons.try_pop_ref().is_none());
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

    /// The K6 BTC push (2026-09-23), trimmed to three of its 752
    /// elements: the push's first row and the ATM pair, verbatim.
    const LIVE_BTC_MARKS: &[u8] = br#"{"stream":"btcusdt@optionMarkPrice","data":[{"s":"BTC-261225-92000-C","mp":"4696.169","E":1790161477975,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"4670.000","ao":"4760.000","bq":"3.52","aq":"3.52","b":"0.38545907","a":"0.39075673","hl":"8450.000","ll":"940.000","vo":"0.387","rf":"0.0529","d":"0.42618971","t":"-35.49083602","g":"0.00002332","v":"169.85468453"},{"s":"BTC-260925-86000-P","mp":"905.351","E":1790161477974,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"905.000","ao":"920.000","bq":"4.43","aq":"12.00","b":"0.34885705","a":"0.35497367","hl":"1625.000","ll":"185.000","vo":"0.349","rf":"0.0558","d":"-0.51276574","t":"-230.5222729","g":"0.00018424","v":"24.52223767"},{"s":"BTC-260925-86000-C","mp":"809.784","E":1790161477974,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"800.000","ao":"810.000","bq":"5.08","aq":"1.10","b":"0.34500957","a":"0.34908772","hl":"1455.000","ll":"165.000","vo":"0.349","rf":"0.0558","d":"0.48723426","t":"-227.33432684","g":"0.00018682","v":"24.52223767"}]}"#;

    /// Records the options lane's capture calls.
    #[derive(Default)]
    struct OptCap {
        summaries: Vec<OptSummary>,
        ticks: u32,
        rejects: Vec<usize>,
    }
    impl Capture for OptCap {
        fn opt_summary(&mut self, o: &OptSummary) {
            self.summaries.push(*o);
        }
        fn tick(&mut self, _t: &Tick) {
            self.ticks += 1;
        }
        fn parse_reject(&mut self, _ts: NsTs, p: &[u8]) {
            self.rejects.push(p.len());
        }
    }

    /// BX0-F2: the options slot walks the live mark array — the two
    /// selected rows become a `Tick` + an `OptSummary` each (index from
    /// the element itself), the unselected row costs nothing; a push
    /// of other underlyings' rows is a clean frame with no output; an
    /// empty book yields the summary alone; a foreign stream and a
    /// selected row missing its mark are one rejection each.
    #[test]
    fn options_slot_walks_the_mark_array() {
        let mut table = crate::eapi::EapiSymbolTable::new();
        let call: SymbolId = (1 << 24) | 1025;
        let put: SymbolId = (1 << 24) | 1026;
        table.insert(b"BTC-260925-86000-C", call).unwrap();
        table.insert(b"BTC-260925-86000-P", put).unwrap();
        let mut d = Driver::new_eapi(7, table);
        d.set_state(State::Steady);

        let mut t = TestTransport::with_capacity(64 * 1024);
        // 1. the live push: two selected rows, one skipped.
        t.inject_incoming(&ws_text_frame(LIVE_BTC_MARKS));
        // 2. a push with no selected row: clean, silent.
        t.inject_incoming(&ws_text_frame(
            br#"{"stream":"ethusdt@optionMarkPrice","data":[{"s":"ETH-260925-2750-C","mp":"30.442","i":"2735.23837209","bo":"29.6000","ao":"30.2000","bq":"619.93","aq":"233.24","vo":"0.4661","d":"0.4507823","t":"-9.51030442","g":"0.00440247","v":"0.77546924"}]}"#,
        ));
        // 3. the call again, book empty (`"0.000"` both sides).
        t.inject_incoming(&ws_text_frame(
            br#"{"stream":"btcusdt@optionMarkPrice","data":[{"s":"BTC-260925-86000-C","mp":"810.0","i":"85900.5","bo":"0.000","ao":"0.000","bq":"0.00","aq":"0.00","vo":"0.35","d":"0.49","t":"-228.0","g":"0.0002","v":"24.6"}]}"#,
        ));
        // 4. a foreign stream: one rejection, the whole frame.
        let foreign: &[u8] =
            br#"{"stream":"btcusdt@index","data":{"e":"index","s":"BTCUSDT","p":"77000.5"}}"#;
        t.inject_incoming(&ws_text_frame(foreign));
        // 5. the put without its mark: one rejection, that element.
        t.inject_incoming(&ws_text_frame(
            br#"{"stream":"btcusdt@optionMarkPrice","data":[{"s":"BTC-260925-86000-P","i":"85900.5","bo":"905.000","ao":"920.000","bq":"1.00","aq":"1.00","vo":"0.35","d":"-0.51","t":"-230.0","g":"0.0002","v":"24.6"}]}"#,
        ));

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let (mut otx, mut orx) = opt_ring_pair();
        let (mut etx, _erx) = event_ring_pair();
        let status = core_metrics::IngressStatus::new();
        let mut cap = OptCap::default();
        super::drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut otx,
            &status,
            &mut cap,
        )
        .unwrap();

        // Ticks: the put and the call from the live push, in wire
        // order; nothing for the empty book.
        let tp = *cons.try_pop_ref().expect("put tick");
        assert_eq!(tp.sym, put);
        assert_eq!((tp.bid_px.raw(), tp.ask_px.raw()), (905_000_000, 920_000_000));
        let tc = *cons.try_pop_ref().expect("call tick");
        assert_eq!(tc.sym, call);
        assert_eq!((tc.bid_px.raw(), tc.ask_px.raw()), (800_000_000, 810_000_000));
        assert_eq!((tc.bid_qty.raw(), tc.ask_qty.raw()), (5_080_000, 1_100_000));
        assert!(
            cons.try_pop_ref().is_none(),
            "the empty book yields no tick"
        );
        assert_eq!(cap.ticks, 2);

        // Summaries: put, call, call (empty book) — captured AND laned.
        assert_eq!(cap.summaries.len(), 3);
        let s = &cap.summaries[1];
        assert_eq!(s.sym, call);
        assert_eq!(s.venue, core_types::VenueId::Binance as u8);
        assert_eq!(s.flags, core_types::OPT_SUMMARY_FLAG_MARK_PX);
        assert_eq!(s.mark_px_1e9, 809_784_000_000);
        assert_eq!(s.mark_iv_1e9, 349_000_000, "vo, not the bid/ask IVs");
        assert_eq!(s.underlying_px_1e9, 85_879_828_260_870, "the element's own index");
        assert_eq!(s.open_interest_1e6, 0);
        assert_eq!(s.theta_1e6, -227_334_326);
        assert_eq!(cap.summaries[0].sym, put);
        assert_eq!(cap.summaries[2].underlying_px_1e9, 85_900_500_000_000);
        let mut laned = 0;
        while orx.try_pop_ref().is_some() {
            laned += 1;
        }
        assert_eq!(laned, 3, "every summary rides the opt lane too");

        // Rejections: the foreign frame whole, then the one element.
        assert_eq!(cap.rejects.len(), 2);
        assert_eq!(cap.rejects[0], foreign.len(), "the foreign frame, tapped whole");
        assert!(cap.rejects[1] < 200, "the bad element alone, not its frame");
        assert_eq!(status.parse_errors_total(), 2);
        assert_eq!(status.msgs_total(), 4, "four clean frames (the foreign one is not)");
        assert_eq!(status.ticks_total(), 3, "one per emitted row");
    }

    /// BX0-F2 at the measured size: a 752-element push (~250 KB, the
    /// venue's 64-bit-length frame) parses whole through the real
    /// rx buffer and yields exactly its selected rows.
    #[test]
    fn options_slot_takes_a_full_size_push() {
        let mut table = crate::eapi::EapiSymbolTable::new();
        table.insert(b"BTC-260925-10100-C", 11).unwrap();
        table.insert(b"BTC-260925-10701-P", 12).unwrap();
        let mut d = Driver::new_eapi(9, table);
        d.set_state(State::Steady);
        let mut body = String::from(r#"{"stream":"btcusdt@optionMarkPrice","data":["#);
        for k in 0..752u32 {
            if k > 0 {
                body.push(',');
            }
            let side = if k % 2 == 0 { 'C' } else { 'P' };
            body.push_str(&format!(
                r#"{{"s":"BTC-260925-{}-{side}","mp":"809.784","E":1790161477974,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"800.000","ao":"810.000","bq":"5.08","aq":"1.10","b":"0.34500957","a":"0.34908772","hl":"1455.000","ll":"165.000","vo":"0.349","rf":"0.0558","d":"0.48723426","t":"-227.33432684","g":"0.00018682","v":"24.52223767"}}"#,
                10_000 + k
            ));
        }
        body.push_str("]}");
        assert!(body.len() > u16::MAX as usize, "must take the 64-bit length form");
        assert!(body.len() < EAPI_RX_BUF_SIZE / 4);

        let mut t = TestTransport::with_capacity(1024 * 1024);
        t.inject_incoming(&ws_text_frame(body.as_bytes()));
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let (mut otx, _orx) = opt_ring_pair();
        let (mut etx, _erx) = event_ring_pair();
        let status = core_metrics::IngressStatus::new();
        let mut cap = OptCap::default();
        super::drive_one(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut otx,
            &status,
            &mut cap,
        )
        .unwrap();
        assert_eq!(cons.try_pop_ref().map(|t| t.sym), Some(11));
        assert_eq!(cons.try_pop_ref().map(|t| t.sym), Some(12));
        assert!(cons.try_pop_ref().is_none());
        assert_eq!(cap.summaries.len(), 2);
        assert!(cap.rejects.is_empty());
        assert_eq!((status.msgs_total(), status.ticks_total()), (1, 2));
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

        let tick = *cons.try_pop_ref().expect("tick must be pushed");
        assert_eq!(tick.sym, 42);
        assert_eq!(tick.bid_px.raw(), 25_351_900);
        assert_eq!(tick.ask_px.raw(), 25_365_200);
        assert_eq!(tick.venue_seq, 400_900_217u32);
        // VT2: a SPOT push carries no stamp — unknown, never stale.
        assert_eq!(tick.venue_time_ms, 0);
        assert!(!tick.is_stale());
        assert_eq!(status.stale_ticks_total(), 0);
        assert!(cons.try_pop_ref().is_none());
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
        *cons.try_pop_ref().expect("bookTicker must produce a tick")
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
        assert!(cons.try_pop_ref().is_none());
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
        while prod.try_push_ref(&filler) {}
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
        assert!(cons.try_pop_ref().is_none(), "capture-only: nothing rings");
        assert_eq!(status.ticks_total(), 1, "market-data row counted");
        assert_eq!(status.parse_errors_total(), 0);
    }

    /// WS10-A: the perp markPrice Funding event reaches the venue-
    /// event lane; the Mark event stays capture-only (mask gates per
    /// channel); a DATED frame (either wire shape) puts NOTHING on the
    /// lane.
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

        let ev = *erx.try_pop_ref().expect("funding event on the lane");
        assert_eq!(ev.channel, core_types::ChannelId::Funding as u8);
        assert_eq!(ev.v0, 381_670, "rate ×1e9");
        assert_eq!(ev.v1, 1_562_306_400_000, "next funding ms");
        assert!(
            erx.try_pop_ref().is_none(),
            "Mark event is NOT on the lane (mask gates per channel)"
        );
        assert_eq!(status.event_ring_drops_total(), 0);

        // Dated frames, both wire generations: has_funding = 0 ⇒
        // nothing new on the lane (the live 2026-09-23 shape carries a
        // ZERO rate and `T` = 0 rather than an empty rate — BX0 K6).
        let dated = br#"{"e":"markPriceUpdate","E":1000,"s":"BTCUSDT_260327","p":"65000.1","i":"64999.9","P":"65000.0","r":"","T":0}"#;
        t.inject_incoming(&ws_text_frame(dated));
        let dated_live = br#"{"e":"markPriceUpdate","E":1790161545000,"s":"BTCUSDT_260925","p":"85901.84762319","ap":"85901.84762319","P":"85864.31580990","i":"85884.08695652","r":"0.00000000","T":0,"st":1}"#;
        t.inject_incoming(&ws_text_frame(dated_live));
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
        assert!(erx.try_pop_ref().is_none(), "dated future never funds");
    }

    #[test]
    fn mark_price_slot_dated_future_emits_mark_only() {
        // WS5/WS3 convention: a delivery contract ⇒ no Funding event,
        // Mark still captured — in the WS5-era empty-rate shape AND the
        // live zero-rate/`T`=0 shape (BX0 K6, 2026-09-23).
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new_mark_price(7, 77);
        d.set_state(State::Steady);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let status = core_metrics::IngressStatus::new();
        let mut cap = CountingCapture::default();

        let dated = br#"{"e":"markPriceUpdate","E":1000,"s":"BTCUSDT_260327","p":"65000.1","i":"64999.9","P":"65000.0","r":"","T":0}"#;
        t.inject_incoming(&ws_text_frame(dated));
        let dated_live = br#"{"e":"markPriceUpdate","E":1790161545000,"s":"BTCUSDT_260925","p":"85901.84762319","ap":"85901.84762319","P":"85864.31580990","i":"85884.08695652","r":"0.00000000","T":0,"st":1}"#;
        t.inject_incoming(&ws_text_frame(dated_live));
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.events.len(), 2, "one Mark per frame, no Funding");
        assert_eq!(cap.events[0].channel, core_types::ChannelId::Mark as u8);
        assert_eq!(cap.events[1].channel, core_types::ChannelId::Mark as u8);
        assert_eq!(cap.events[1].v0, 85_901_847_623, "live dated mark ×1e6");
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
