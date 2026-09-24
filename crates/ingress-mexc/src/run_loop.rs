// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-mexc run-loop (MX3 + MX4)
//!
//! Event-driven state machine for the MEXC public WS. N connections of
//! TWO classes (spot `/ws` protobuf, futures `/edge` JSON) run on ONE
//! thread with ONE tick producer and ONE venue-event producer via
//! [`run_multi`] — the Bybit spot/linear multi-conn shape beat for beat
//! (slot index = `mio::Token`, 50 ms poll, the six raw-index passes,
//! the WS2 establishment budget, `Backoff`, `Keepalive`); the
//! driver's [`MexcClass`] picks the parser, the subscribe renderer, the
//! ping payload and the confirmation law.
//!
//! ```text
//! Connecting ──► NeedsWsWrite ──► AwaitingWsUpgrade ──► Steady ──┐
//!      ▲                                                          │
//!      └──────────────── Closed / Err ────────────────────────────┘
//! ```
//!
//! On entry to `Steady` the driver queues its whole subscribe set at
//! once: spot ONE `SUBSCRIPTION` frame (every symbol × 2 channels),
//! futures ONE frame per (symbol, channel) (× 3). Confirmation (plan §4
//! D7): a spot pair is confirmed by the per-param ack (every requested
//! param not listed as failed) or by its first data; a futures pair by
//! its FIRST DATA only (the `rs.sub.*` acks carry no symbol).
//! [`Driver::sub_count`] is the confirmed-pair count the WS2
//! establishment predicate reads. A refusal while nothing was EVER
//! confirmed on the driver is fatal (venue-blind boot refusal); after
//! that it is a non-fatal drop (`sub_drops` + one `SubDrop` event per
//! refused param + a rate-limited WARN).
//!
//! Per-row driver state: the last-seen full-width book / trade seqs
//! (the Q-MX1 regression counter — process-lifetime, so a lagging node
//! behind a reconnect is still caught), the funding seed (Q-MX3 —
//! process-lifetime) and the per-channel confirmed bits
//! (connection-scoped, cleared on reconnect).
//!
//! Everything after the handshake is zero-alloc: parsers slice the rx
//! buffer in place; the only copies are the marked `// COPY:` sites
//! and the 64-byte PODs moved into their ring slots.

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
    Capture, ChannelEvent, ChannelId, Price, Qty, SymbolId, Tick, VenueId, EVENT_RING_SIZE,
    SYMBOL_ID_NONE, TICK_FLAG_STALE,
};

use crate::futures::FUT_SUB_PAYLOAD_MAX;
use crate::{
    classify_futures, classify_spot, extract_fut_symbol, extract_fut_ts_ms, extract_param_channel,
    extract_refused_contract,
    extract_param_symbol, funding_next_settle_ms, parse_book_ticker_body, parse_deal_item,
    parse_depth_full, parse_fut_deal_item, parse_spot_wrapper, parse_sub_ack, parse_ticker,
    write_fut_subscribe, write_spot_subscribe, MexcChannel, MexcClass, MexcDeal, MexcDealsWalk,
    MexcFutDealsWalk, MexcFutKind, MexcSpotAck, MexcSpotKind, MexcSymbolTable, MexcTickerFrame,
    MEXC_MAX_SYMBOLS_PER_CONN, MEXC_SYMBOL_MAX, MS_PER_HOUR,
};

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// Rx buffer: spot PB pushes are ~130–500 B, futures `depth.full`
/// ~400 B, `ticker` ~700 B. 256 KiB absorbs a full multi-symbol burst
/// with a large margin (boot-time allocation).
pub const RX_BUF_SIZE: usize = 256 * 1024;

/// Tx buffer: handshake + the whole subscribe set (spot one frame
/// ≤ ~2.3 KiB; futures ≤ 48 frames × ~90 B ≈ 4.2 KiB) + pings. 16 KiB
/// keeps ≥ 2× margin (const-asserted below).
pub const TX_BUF_SIZE: usize = 16 * 1024;

/// Tick-ring capacity. Must equal `engine::TICK_RING_SIZE` — named
/// `TICK_RING_CAP` so the cli's const-assert block covers it.
pub const TICK_RING_CAP: usize = 16_384;

/// `SubDrop.v0` for a refusal that carries no numeric venue code: a
/// param listed in the spot ack's `Not Subscribed successfully! […]`
/// echo, or a futures `rs.sub.*`/`rs.error` refusal (MEXC names no
/// code in either). A spot ack whose `code != 0` refuses the whole
/// request and its `SubDrop.v0` carries that code instead.
pub const SUB_DROP_REFUSED: i64 = 1;

/// `SubDrop.v1` when the refused channel cannot be named.
const SUB_DROP_CHANNEL_UNKNOWN: i64 = -1;

/// Longest spot subscribe payload (the render scratch size): the
/// envelope + per param (two quotes, a comma, the longest topic, the
/// longest symbol) — ~2.3 KiB at the 16-row table cap.
const SPOT_SUB_PAYLOAD_MAX: usize = br#"{"method":"SUBSCRIPTION","params":[]}"#.len()
    + MEXC_MAX_SYMBOLS_PER_CONN
        * 2
        * (3 + MexcChannel::SpotBookTicker.topic().len() + MEXC_SYMBOL_MAX);

/// Handshake allowance in the tx budget.
const HANDSHAKE_TX_MAX: usize = 1024;

/// Masked client-frame header: 2 + 2 (16-bit length) + 4 (mask).
const WS_CLIENT_HDR_MAX: usize = 8;

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

/// How a run-loop invocation terminated (the multi-conn loop only ever
/// surfaces `Stopped`/`Error`; per-slot failures recycle the slot).
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

/// Per-symbol-row driver state. One cache line.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
struct RowState {
    /// Last-seen full-width book seq (spot `version` / futures
    /// `version`); 0 = none yet. Process-lifetime.
    last_book_seq: u64,
    /// Last-seen full-width trade seq (spot `tradeId` leading digits /
    /// futures `i`); 0 = none yet. Process-lifetime.
    last_trade_seq: u64,
    /// Q-MX3: the latched next-settle instant (ms); 0 = unseeded.
    funding_next_ms: u64,
    /// The last EMITTED BBO `[bid px, bid qty, ask px, ask qty]` ×1e6
    /// — a push equal to it is not a tick (the BBO-change doctrine,
    /// see [`Dispatch::Book`] handling). Connection-scoped: a
    /// reconnect re-emits the first quote.
    last_bbo: [i64; 4],
    /// Q-MX3: the funding period (ms; ≤ 1 193 h fits); 0 = unknown.
    funding_cycle_ms: u32,
    /// Confirmed channels, bit = [`MexcChannel::slot`]. Connection-
    /// scoped.
    confirmed: u8,
    /// The VT2 stale verdict (0/1) of the push `last_bbo` latched — an
    /// unchanged quote whose verdict FLIPPED is still a tick, or a
    /// stale-flagged quote would mute the symbol in the vm (reads go
    /// ABSENT until a fresh tick) for as long as the touch stays put.
    /// Connection-scoped with `last_bbo`.
    last_stale: u8,
    _pad: [u8; 2],
}

impl RowState {
    const EMPTY: Self = Self {
        last_book_seq: 0,
        last_trade_seq: 0,
        funding_next_ms: 0,
        last_bbo: [0; 4],
        funding_cycle_ms: 0,
        confirmed: 0,
        last_stale: 0,
        _pad: [0; 2],
    };
}

const _ROW_SIZE: () = assert!(::core::mem::size_of::<RowState>() == 64);

/// All-channels mask of a class.
#[inline]
const fn class_mask(class: MexcClass) -> u8 {
    ((1u16 << class.channels_per_symbol()) - 1) as u8
}

/// Mutable per-connection state owned by the run-loop. Preallocated at
/// construction; never reallocates in steady state.
///
/// **Single-writer invariant.** `!Sync` via the marker field; the cli
/// moves every driver onto ONE venue thread at boot.
pub struct Driver {
    state: State,
    rx: IoBuf,
    tx: IoBuf,
    sec_key: [u8; 24],
    expected_accept_val: [u8; 28],
    /// Monotonic ns of the last inbound byte (the multi-conn keepalive
    /// reads it).
    pub last_activity_ns: NsTs,
    /// Monotonic counter feeding the outbound frame masks (public so the
    /// multi-conn keepalive can mask its ping).
    pub mask_counter: u64,

    /// Spot or futures — parser, subscribe renderer, ping, confirmation.
    class: MexcClass,
    /// Boot-built `SYMBOL → SymbolId` map for THIS connection.
    symbols: MexcSymbolTable,
    /// Per-row state, indexed by symbol-table row.
    rows: [RowState; MEXC_MAX_SYMBOLS_PER_CONN],
    /// Set once the subscribe set has been queued this session.
    subscribed: bool,
    /// WS2: PROCESS-LIFETIME flag — true once ANY pair has ever been
    /// confirmed on this driver. While false, a refusal is fatal (boot
    /// venue-blind refusal); after, a non-fatal drop. Deliberately NOT
    /// cleared by [`Self::reset_for_reconnect`].
    ever_confirmed: bool,
    /// WS2: establishment budget (ns from session start to the first
    /// confirmed pair) enforced by [`run_multi`].
    establish_budget_ns: u64,
    /// WS2 drop-log rate limiter (process-lifetime, operator budget).
    drop_log_last_ns: u64,
    /// Drops swallowed by the rate limit since the last line.
    drop_log_suppressed: u32,
    /// VT2: THIS connection's venue-clock offset estimator + staleness
    /// judge for the class's BBO (spot `createTime` else `sendTime`;
    /// futures `cts` else `ts`). Reset on reconnect; threshold = the
    /// venue default or `--stale-after-ms mexc:<ms>`.
    feed_clock: FeedClock,
    /// `!Sync` marker — see struct doc.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate buffers (boot-time) and seed the handshake nonce. The
    /// caller chunks its universe by [`MexcClass::symbols_per_conn`]; a
    /// table over that cap is refused at the first subscribe.
    pub fn new(nonce_seed: u64, class: MexcClass, symbols: MexcSymbolTable) -> Self {
        debug_assert!(
            symbols.len() <= class.symbols_per_conn(),
            "mexc: connection over its class symbol cap"
        );
        let sec_key = sec_websocket_key_from_seed(nonce_seed);
        let accept = expected_accept(&sec_key);
        // COPY: the 520 B symbol table moves into the driver, and the 1 792 B
        // driver returns by value — once per connection, at boot — the
        // driver holds its table INLINE so every hot lookup walks contiguous
        // rows with no pointer to chase — rejected: a `Box` (a heap hop on
        // the hot lookup) and a two-phase in-place init behind `&mut` (for a
        // boot-only cost).
        Self {
            state: State::Connecting,
            rx: IoBuf::with_capacity(RX_BUF_SIZE),
            tx: IoBuf::with_capacity(TX_BUF_SIZE),
            sec_key,
            expected_accept_val: accept,
            last_activity_ns: 0,
            mask_counter: 0,
            class,
            symbols,
            rows: [RowState::EMPTY; MEXC_MAX_SYMBOLS_PER_CONN],
            subscribed: false,
            ever_confirmed: false,
            establish_budget_ns: core_net::ESTABLISH_BUDGET_NS,
            drop_log_last_ns: 0,
            drop_log_suppressed: 0,
            feed_clock: FeedClock::new(VenueId::Mexc.default_stale_after_ms()),
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
    /// `--stale-after-ms mexc:<ms>`). Boot-time only — re-arms the
    /// estimator unlearned, exactly like a fresh connection.
    #[inline]
    pub fn set_stale_after_ms(&mut self, ms: u32) {
        self.feed_clock = FeedClock::new(ms);
    }

    /// Q-MX3: seed one perp row's funding clock from REST
    /// `funding_rate/{SYM}` (`nextSettleTime` ms, `collectCycle` hours).
    /// Boot-time. Returns false when `sym` is not on this driver or the
    /// driver is not a futures connection (nothing is seeded then).
    pub fn set_funding_seed(&mut self, sym: SymbolId, next_settle_ms: u64, collect_cycle_h: u32) -> bool {
        if self.class != MexcClass::Futures {
            return false;
        }
        let mut i = 0;
        while let Some((_, s)) = self.symbols.get(i) {
            if s == sym {
                return match self.rows.get_mut(i) {
                    Some(r) => {
                        r.funding_next_ms = next_settle_ms;
                        r.funding_cycle_ms =
                            ((collect_cycle_h as u64) * MS_PER_HOUR).min(u32::MAX as u64) as u32;
                        true
                    }
                    None => false,
                };
            }
            i += 1;
        }
        false
    }

    /// VT2: this connection's smoothed BBO feed delay (ms).
    #[inline]
    pub fn feed_delay_ema_ms(&self) -> u32 {
        self.feed_clock.delay_ema_ms()
    }

    /// Current state (metrics + tests).
    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    /// The connection class.
    #[inline]
    pub fn class(&self) -> MexcClass {
        self.class
    }

    /// Confirmed (symbol, channel) pairs this session — the WS2
    /// establishment predicate.
    #[inline]
    pub fn sub_count(&self) -> usize {
        let mask = class_mask(self.class);
        let rows = match self.rows.get(..self.symbols.len()) {
            Some(r) => r,
            None => &self.rows[..],
        };
        let mut n = 0usize;
        let mut i = 0;
        while i < rows.len() {
            n += (rows[i].confirmed & mask).count_ones() as usize;
            i += 1;
        }
        n
    }

    /// Reset per-connection state for a reconnect. Confirmations are
    /// connection-scoped; `ever_confirmed`, the seqs, the funding seeds,
    /// the establishment budget and the drop-log limiter are
    /// process-lifetime — untouched here.
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
            self.rows[i].confirmed = 0;
            self.rows[i].last_bbo = [0; 4];
            self.rows[i].last_stale = 0;
            i += 1;
        }
        self.subscribed = false;
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
    producer: &mut Producer<Tick, TICK_RING_CAP>,
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
// tx / handshake helpers (the Bybit/OKX template shape)
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

/// Queue the whole subscribe set of the connection's class: spot ONE
/// `SUBSCRIPTION` frame, futures ONE frame per (symbol, channel).
fn queue_subscribe_all(drv: &mut Driver) -> io::Result<()> {
    debug_assert!(!drv.subscribed, "subscribe set must be queued exactly once");
    if drv.symbols.is_empty() {
        return Err(io::Error::other("mexc: no symbols configured"));
    }
    if drv.symbols.len() > drv.class.symbols_per_conn() {
        return Err(io::Error::other("mexc: connection over its class symbol cap"));
    }
    // The whole set is queued at the upgrade edge: both classes' worst
    // case (+ the handshake) fits in half of tx.
    const {
        assert!(HANDSHAKE_TX_MAX + SPOT_SUB_PAYLOAD_MAX + WS_CLIENT_HDR_MAX <= TX_BUF_SIZE / 2);
        assert!(
            HANDSHAKE_TX_MAX
                + MEXC_MAX_SYMBOLS_PER_CONN * 3 * (FUT_SUB_PAYLOAD_MAX + WS_CLIENT_HDR_MAX)
                <= TX_BUF_SIZE / 2
        );
    };
    match drv.class {
        MexcClass::Spot => {
            let mut scratch = [0u8; SPOT_SUB_PAYLOAD_MAX];
            let len = write_spot_subscribe(&mut scratch, &drv.symbols)
                .ok_or_else(|| io::Error::other("mexc: spot subscribe scratch too small"))?;
            queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..len])?;
        }
        MexcClass::Futures => {
            let per_symbol = MexcClass::Futures.channels_per_symbol() as u8;
            let mut i = 0;
            while let Some((symbol, _sym)) = drv.symbols.get(i) {
                let mut slot = 0u8;
                while slot < per_symbol {
                    let ch = MexcChannel::from_slot(MexcClass::Futures, slot)
                        .ok_or_else(|| io::Error::other("mexc: futures slot out of range"))?;
                    let mut scratch = [0u8; FUT_SUB_PAYLOAD_MAX];
                    let len = write_fut_subscribe(&mut scratch, ch, symbol)
                        .ok_or_else(|| io::Error::other("mexc: futures subscribe scratch too small"))?;
                    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..len])?;
                    slot += 1;
                }
                i += 1;
            }
        }
    }
    drv.subscribed = true;
    Ok(())
}

// ---------------------------------------------------------------
// Frame drain + dispatch
// ---------------------------------------------------------------

/// A BBO candidate (spot bookTicker or futures depth.full), `Copy`.
#[derive(Copy, Clone)]
#[repr(C)]
struct BookScan {
    bid_px_1e6: i64,
    bid_qty_1e6: i64,
    ask_px_1e6: i64,
    ask_qty_1e6: i64,
    seq: u64,
    venue_time_ms: u64,
    two_sided: bool,
}

impl BookScan {
    /// No candidate yet — the phase-1 scratch's starting value.
    const EMPTY: Self = Self {
        bid_px_1e6: 0,
        bid_qty_1e6: 0,
        ask_px_1e6: 0,
        ask_qty_1e6: 0,
        seq: 0,
        venue_time_ms: 0,
        two_sided: false,
    };
}

/// Per-push trade walk result (events already captured in phase 1).
#[derive(Copy, Clone)]
#[repr(C)]
struct TradeScan {
    parsed: u32,
    rejected: u32,
    malformed: bool,
    /// Largest trade seq in the push (the regression judge — immune to
    /// the venue's intra-push print order).
    max_seq: u64,
}

impl TradeScan {
    const EMPTY: Self = Self {
        parsed: 0,
        rejected: 0,
        malformed: false,
        max_seq: 0,
    };
}

/// A walked spot ack: its code and the per-row refused-channel bits.
#[derive(Copy, Clone)]
#[repr(C)]
struct AckScan {
    code: i64,
    failures: u32,
    failed: [u8; MEXC_MAX_SYMBOLS_PER_CONN],
    /// The refusals are non-fatal: the driver ever confirmed a pair OR
    /// this very ack confirms one — a delisted symbol must not blind
    /// the socket's other symbols (plan R10). Only an ack that refuses
    /// EVERY pair of a never-confirmed driver is the boot fail-fast.
    non_fatal: bool,
}

/// Phase-1 dispatch outcome (two-phase borrow pattern — see
/// ingress-okx / ingress-bybit).
#[derive(Copy, Clone)]
enum Dispatch {
    /// Unparseable / unclassifiable / unknown symbol — one rejection.
    Nothing,
    /// Pong — activity only.
    Quiet,
    /// The spot per-param ack.
    SpotAck(AckScan),
    /// A futures `rs.sub.*` success (names no symbol — counts nothing).
    FutAckOk,
    /// A futures refusal: `rs.sub.*` non-success (names no symbol —
    /// fatal on a never-confirmed driver) or an `rs.error` naming a
    /// contract (never fatal: the socket's other symbols live on, and
    /// the establishment budget reaps a session confirming nothing).
    FutRefusal {
        channel: i64,
        sym: SymbolId,
        fatal_if_blind: bool,
    },
    /// An `rs.error` that refuses nothing (e.g. the 60 s heartbeat
    /// notice) — a venue session error, not a subscribe drop.
    FutVenueNotice,
    /// A BBO push for row `row`, walked into the phase-1 book scratch.
    Book {
        row: usize,
        sym: SymbolId,
        channel: MexcChannel,
    },
    /// A trade push for row `row` (events captured in phase 1).
    Trades {
        row: usize,
        channel: MexcChannel,
        scan: TradeScan,
    },
    /// A futures ticker push for row `row`, parsed into the phase-1
    /// ticker scratch (events emitted in phase 2 — the funding clock is
    /// mutable row state).
    Ticker { row: usize, sym: SymbolId },
}

// The dispatch value crosses phase 1 → phase 2 by value: pinned
// within the 64 B by-value bound (frames ride in phase-1 scratch).
const _: () = assert!(core::mem::size_of::<Dispatch>() <= 64);

fn drain_ws_frames<C: Capture>(
    drv: &mut Driver,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
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
                            matches!(header.opcode, WsOpcode::Binary),
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
                        // The echo goes straight from rx into tx (disjoint
                        // field borrows) — no scratch; `ws_read_frame`
                        // already refused a control payload over 125 B.
                        if let Some(src) = drv.rx.filled().get(payload.start..payload.end) {
                            if let Ok(n) = ws_write_pong(drv.tx.free_mut(), src, mask) {
                                drv.tx.advance(n);
                            }
                        }
                    }
                    WsOpcode::Pong => {}
                    WsOpcode::Close => {
                        drv.state = State::Closed;
                    }
                    WsOpcode::Continuation => {
                        // MEXC does not fragment pushes; drop rather
                        // than allocate a reassembly buffer.
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

/// Capture one walked print (§6.5 `Trade`: v0 = px ×1e6, v1 = signed
/// qty ×1e6, `venue_seq` = the full-width Q-MX1 seq, venue ms = the
/// item's time or the push's own stamp when the item carries none).
#[inline]
fn record_deal<C: Capture>(
    deal: Option<&MexcDeal>,
    item: &[u8],
    sym: SymbolId,
    fallback_ms: u64,
    scan: &mut TradeScan,
    capture: &mut C,
) {
    match deal {
        Some(d) => {
            scan.parsed += 1;
            if d.trade_seq > scan.max_seq {
                scan.max_seq = d.trade_seq;
            }
            capture.event(&ChannelEvent::new(
                now_ns(),
                VenueId::Mexc,
                ChannelId::Trade,
                sym,
                d.trade_seq,
                if d.time_ms > 0 { d.time_ms } else { fallback_ms },
                d.px_1e6,
                d.signed_qty_1e6(),
            ));
        }
        None => {
            scan.rejected += 1;
            capture.parse_reject(now_ns(), item);
        }
    }
}

/// Walk a spot ack's refused params: resolve each to (row, channel)
/// bits, decide fatality — non-fatal when a pair was ever confirmed on
/// the driver OR this ack confirms one (every requested pair not
/// listed as refused) — and, when non-fatal, emit one `SubDrop` per
/// refused param, 1:1 with `sub_drops` (the fatal path emits nothing,
/// the Bybit law). Two walks of the (short) refused list: the verdict
/// needs every refusal before the first event.
fn scan_spot_ack<C: Capture>(
    payload: &[u8],
    ack: &MexcSpotAck,
    symbols: &MexcSymbolTable,
    ever_confirmed: bool,
    status: &IngressStatus,
    capture: &mut C,
) -> AckScan {
    let mut scan = AckScan {
        code: ack.code,
        failures: 0,
        failed: [0; MEXC_MAX_SYMBOLS_PER_CONN],
        non_fatal: ever_confirmed,
    };
    if ack.code != 0 {
        return scan;
    }
    let mut w = ack.failed_params(payload);
    while let Some(param) = w.next_param() {
        scan.failures += 1;
        let resolved = extract_param_symbol(param).and_then(|s| symbols.lookup(s));
        if let (Some((row, _)), Some(ch)) = (resolved, extract_param_channel(param)) {
            if let Some(bits) = scan.failed.get_mut(row) {
                *bits |= 1 << ch.slot();
            }
        }
    }
    let mask = class_mask(MexcClass::Spot);
    let mut i = 0;
    while i < symbols.len() && i < MEXC_MAX_SYMBOLS_PER_CONN {
        if mask & !scan.failed[i] != 0 {
            scan.non_fatal = true;
        }
        i += 1;
    }
    if scan.failures == 0 || !scan.non_fatal {
        return scan;
    }
    let mut w = ack.failed_params(payload);
    while let Some(param) = w.next_param() {
        let resolved = extract_param_symbol(param).and_then(|s| symbols.lookup(s));
        let channel = extract_param_channel(param);
        status.inc_sub_drops();
        capture.event(&ChannelEvent::new(
            now_ns(),
            VenueId::Mexc,
            ChannelId::SubDrop,
            match resolved {
                Some((_, sym)) => sym,
                None => SYMBOL_ID_NONE,
            },
            0,
            0,
            SUB_DROP_REFUSED,
            match channel {
                Some(ch) => ch.discriminant(),
                None => SUB_DROP_CHANNEL_UNKNOWN,
            },
        ));
    }
    scan
}

/// Phase 1 for a SPOT frame. A BBO candidate lands in `book`
/// ([`Dispatch::Book`] says so).
fn spot_dispatch<C: Capture>(
    payload: &[u8],
    binary: bool,
    symbols: &MexcSymbolTable,
    ever_confirmed: bool,
    status: &IngressStatus,
    capture: &mut C,
    book: &mut BookScan,
) -> Dispatch {
    match classify_spot(payload, binary) {
        MexcSpotKind::Pong => Dispatch::Quiet,
        MexcSpotKind::SubAck => {
            let mut ack = crate::spot::MexcSpotAck::ZERO;
            if parse_sub_ack(payload, &mut ack) {
                Dispatch::SpotAck(scan_spot_ack(
                    payload,
                    &ack,
                    symbols,
                    ever_confirmed,
                    status,
                    capture,
                ))
            } else {
                Dispatch::Nothing
            }
        },
        MexcSpotKind::Push => {
            let Some(frame) = parse_spot_wrapper(payload) else {
                return Dispatch::Nothing;
            };
            let Some((row, sym)) = symbols.lookup(frame.symbol(payload)) else {
                return Dispatch::Nothing;
            };
            let body = frame.body(payload);
            match frame.channel {
                MexcChannel::SpotBookTicker => {
                    let mut b = crate::spot::MexcBookTicker::ZERO;
                    if parse_book_ticker_body(body, &mut b) {
                        *book = BookScan {
                            bid_px_1e6: b.bid_px_1e6,
                            bid_qty_1e6: b.bid_qty_1e6,
                            ask_px_1e6: b.ask_px_1e6,
                            ask_qty_1e6: b.ask_qty_1e6,
                            seq: b.version,
                            venue_time_ms: frame.venue_time_ms(),
                            // An emptied side parses as 0/0 (see
                            // `parse_book_ticker_body`): no quote.
                            two_sided: b.bid_px_1e6 > 0 && b.ask_px_1e6 > 0,
                        };
                        Dispatch::Book {
                            row,
                            sym,
                            channel: MexcChannel::SpotBookTicker,
                        }
                    } else {
                        Dispatch::Nothing
                    }
                },
                MexcChannel::SpotDeals => {
                    let fallback_ms = frame.venue_time_ms();
                    let mut walk = MexcDealsWalk::new(body);
                    let mut scan = TradeScan::EMPTY;
                    let mut deal = crate::MexcDeal::ZERO;
                    while let Some(item) = walk.next_item() {
                        let parsed = parse_deal_item(item, &mut deal);
                        record_deal(parsed.then_some(&deal), item, sym, fallback_ms, &mut scan, capture);
                    }
                    scan.malformed = walk.is_malformed();
                    Dispatch::Trades {
                        row,
                        channel: MexcChannel::SpotDeals,
                        scan,
                    }
                }
                _ => Dispatch::Nothing,
            }
        }
        MexcSpotKind::Unknown => Dispatch::Nothing,
    }
}

/// Phase 1 for a FUTURES frame. A BBO candidate lands in `book`, a
/// ticker in `ticker` ([`Dispatch::Book`] / [`Dispatch::Ticker`] say
/// which).
fn fut_dispatch<C: Capture>(
    payload: &[u8],
    binary: bool,
    symbols: &MexcSymbolTable,
    capture: &mut C,
    book: &mut BookScan,
    ticker: &mut MexcTickerFrame,
) -> Dispatch {
    if binary {
        // Compression is never requested; a binary push is foreign.
        return Dispatch::Nothing;
    }
    match classify_futures(payload) {
        MexcFutKind::Pong => Dispatch::Quiet,
        MexcFutKind::SubAck { success: true, .. } => Dispatch::FutAckOk,
        MexcFutKind::SubAck {
            success: false,
            channel,
        } => Dispatch::FutRefusal {
            channel: match channel {
                Some(ch) => ch.discriminant(),
                None => SUB_DROP_CHANNEL_UNKNOWN,
            },
            sym: SYMBOL_ID_NONE,
            fatal_if_blind: true,
        },
        MexcFutKind::RequestError => match extract_refused_contract(payload) {
            Some(contract) => Dispatch::FutRefusal {
                channel: SUB_DROP_CHANNEL_UNKNOWN,
                sym: match symbols.lookup(contract) {
                    Some((_, sym)) => sym,
                    None => SYMBOL_ID_NONE,
                },
                fatal_if_blind: false,
            },
            None => Dispatch::FutVenueNotice,
        },
        MexcFutKind::Data(channel) => {
            let Some((row, sym)) = extract_fut_symbol(payload).and_then(|s| symbols.lookup(s))
            else {
                return Dispatch::Nothing;
            };
            match channel {
                MexcChannel::FutDepthFull => {
                    let mut d = crate::futures::MexcDepthFrame::ZERO;
                    if parse_depth_full(payload, &mut d) {
                        *book = BookScan {
                            bid_px_1e6: d.bid_px_1e6,
                            bid_qty_1e6: d.bid_qty_1e6,
                            ask_px_1e6: d.ask_px_1e6,
                            ask_qty_1e6: d.ask_qty_1e6,
                            seq: d.version,
                            venue_time_ms: d.venue_time_ms,
                            two_sided: d.has_bid == 1 && d.has_ask == 1,
                        };
                        Dispatch::Book { row, sym, channel }
                    } else {
                        Dispatch::Nothing
                    }
                },
                MexcChannel::FutDeal => {
                    let fallback_ms = extract_fut_ts_ms(payload);
                    let mut walk = MexcFutDealsWalk::new(payload);
                    let mut scan = TradeScan::EMPTY;
                    let mut deal = crate::MexcDeal::ZERO;
                    while let Some(item) = walk.next_item() {
                        let parsed = parse_fut_deal_item(item, &mut deal);
                        record_deal(parsed.then_some(&deal), item, sym, fallback_ms, &mut scan, capture);
                    }
                    scan.malformed = walk.is_malformed();
                    Dispatch::Trades { row, channel, scan }
                }
                MexcChannel::FutTicker => {
                    if parse_ticker(payload, ticker) {
                        Dispatch::Ticker { row, sym }
                    } else {
                        Dispatch::Nothing
                    }
                },
                _ => Dispatch::Nothing,
            }
        }
        MexcFutKind::Unknown => Dispatch::Nothing,
    }
}

/// Mark a (row, channel) confirmed by data.
#[inline]
fn note_confirmed(drv: &mut Driver, row: usize, channel: MexcChannel) {
    if let Some(r) = drv.rows.get_mut(row) {
        r.confirmed |= 1 << channel.slot();
        drv.ever_confirmed = true;
    }
}

/// Q-MX1 regression judge: `v` strictly below the last-seen full-width
/// value counts once; 0 = absent never counts and never overwrites.
#[inline]
fn note_seq(last: &mut u64, v: u64, status: &IngressStatus) {
    if v == 0 {
        return;
    }
    if *last > 0 && v < *last {
        status.inc_seq_regressions();
    }
    *last = v;
}

/// The boot fail-fast refusal (nothing ever confirmed on this driver:
/// the configured set is venue-blind).
fn refuse_fatal(status: &IngressStatus, venue_code: i64) -> io::Result<()> {
    if venue_code != 0 {
        status.note_venue_err_code(venue_code as u32);
    }
    status.note_session_err(
        core_metrics::ERR_SITE_VENUE_ERROR,
        core_metrics::io_kind_code(io::ErrorKind::InvalidData),
    );
    debug_assert!(false, "mexc subscribe refused at boot");
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "mexc subscribe refused",
    ))
}

#[allow(clippy::too_many_arguments)]
fn handle_data_frame<C: Capture>(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    binary: bool,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    let reject_range = payload_range.clone();
    // Phase 1's BBO candidate and futures ticker, built in place: riding
    // inside the dispatch value they would widen it past the 64 B bound
    // and cross each phase-1 call → phase 2 by value. `Dispatch::Book` /
    // `Dispatch::Ticker` say which one was written.
    let mut book = BookScan::EMPTY;
    let mut ticker = MexcTickerFrame::ZERO;
    // Phase 1: immutable borrows — classify, resolve, pre-parse into a
    // Copy dispatch; capture hooks needing the payload fire here.
    let dispatch: Dispatch = {
        let payload = drv.rx.filled().get(payload_range).unwrap_or(&[]);
        capture.raw_frame(now_ns(), payload);
        match drv.class {
            MexcClass::Spot => spot_dispatch(
                payload,
                binary,
                &drv.symbols,
                drv.ever_confirmed,
                status,
                capture,
                &mut book,
            ),
            MexcClass::Futures => {
                fut_dispatch(payload, binary, &drv.symbols, capture, &mut book, &mut ticker)
            }
        }
    };

    // Phase 2: mutable applies.
    match dispatch {
        Dispatch::Nothing => {
            status.inc_parse_errors();
            if let Some(p) = drv.rx.filled().get(reject_range) {
                capture.parse_reject(now_ns(), p);
            }
        }
        Dispatch::Quiet => {}
        Dispatch::SpotAck(scan) => {
            status.add_msgs(1);
            if scan.code != 0 {
                // The WHOLE request refused.
                if !drv.ever_confirmed {
                    return refuse_fatal(status, scan.code);
                }
                status.inc_sub_drops();
                capture.event(&ChannelEvent::new(
                    now_ns(),
                    VenueId::Mexc,
                    ChannelId::SubDrop,
                    SYMBOL_ID_NONE,
                    0,
                    0,
                    scan.code,
                    SUB_DROP_CHANNEL_UNKNOWN,
                ));
                log_sub_drop_rate_limited(drv);
                return Ok(());
            }
            if scan.failures > 0 {
                if !scan.non_fatal {
                    // Every pair refused on a never-confirmed driver.
                    return refuse_fatal(status, 0);
                }
                // Non-fatal: the SubDrop events rode phase 1.
                log_sub_drop_rate_limited(drv);
            }
            // Every requested pair NOT listed as refused is confirmed.
            let mask = class_mask(drv.class);
            let n = drv.symbols.len();
            let mut i = 0;
            while i < n && i < MEXC_MAX_SYMBOLS_PER_CONN {
                let add = mask & !scan.failed[i];
                drv.rows[i].confirmed |= add;
                if add != 0 {
                    drv.ever_confirmed = true;
                }
                i += 1;
            }
        }
        Dispatch::FutAckOk => {
            // The ack names no symbol: nothing to confirm (first data
            // confirms — plan §4 D7).
            status.add_msgs(1);
        }
        Dispatch::FutRefusal {
            channel,
            sym,
            fatal_if_blind,
        } => {
            status.add_msgs(1);
            if fatal_if_blind && !drv.ever_confirmed {
                return refuse_fatal(status, 0);
            }
            status.inc_sub_drops();
            capture.event(&ChannelEvent::new(
                now_ns(),
                VenueId::Mexc,
                ChannelId::SubDrop,
                sym,
                0,
                0,
                SUB_DROP_REFUSED,
                channel,
            ));
            log_sub_drop_rate_limited(drv);
        }
        Dispatch::FutVenueNotice => {
            // A venue notice refusing nothing (the socket usually closes
            // right after): name it on the T1(a) exit line; not a drop.
            status.add_msgs(1);
            status.note_session_err(
                core_metrics::ERR_SITE_VENUE_ERROR,
                core_metrics::io_kind_code(io::ErrorKind::ConnectionAborted),
            );
        }
        Dispatch::Book { row, sym, channel } => {
            let scan = &book;
            status.add_msgs(1);
            note_confirmed(drv, row, channel);
            // VT2: judge EVERY stamped push — a republished quote still
            // carries a fresh venue stamp; one parse-complete stamp
            // serves the judgement and the tick.
            let now = now_ns();
            let judged = drv.feed_clock.judge(scan.venue_time_ms, now);
            status.set_feed_delay_ema_ms(drv.feed_clock.delay_ema_ms());
            let bbo = [
                scan.bid_px_1e6,
                scan.bid_qty_1e6,
                scan.ask_px_1e6,
                scan.ask_qty_1e6,
            ];
            let stale = judged.stale as u8;
            // A tick is a BBO CHANGE (the house doctrine: Binance
            // bookTicker, Bybit orderbook.1 and OKX bbo-tbt push on
            // change). MEXC republishes an UNCHANGED quote — spot
            // `aggre.bookTicker@10ms` every 10 ms (measured live
            // 2026-09-23: 96.5–99.9 % of spot pushes byte-identical to
            // the previous one), futures `depth.full` whenever a deeper
            // level moves (26–79 %). Such a push is a msg, not a tick:
            // no ring slot, no capture row; its version still feeds the
            // regression check and its stamp the feed clock above.
            // An unchanged quote whose STALE verdict flipped IS a tick:
            // the vm mirrors the latest tick's flag (VT3 — reads go
            // ABSENT while it is set), so suppressing the fresh
            // republication of a stale-flagged quote would mute the
            // symbol until the touch next moves (hours on a weekend
            // xStock), and suppressing a newly-stale one would hide the
            // lag.
            let changed = match drv.rows.get_mut(row) {
                Some(r) => {
                    note_seq(&mut r.last_book_seq, scan.seq, status);
                    let changed = r.last_bbo != bbo || r.last_stale != stale;
                    r.last_bbo = bbo;
                    r.last_stale = stale;
                    changed
                }
                None => true,
            };
            // Emit only a live two-sided BBO (an empty side, a zero size
            // or a price that truncated to 0 at 1e-6 is not a quote).
            if changed
                && scan.two_sided
                && scan.bid_px_1e6 > 0
                && scan.bid_qty_1e6 > 0
                && scan.ask_px_1e6 > 0
                && scan.ask_qty_1e6 > 0
            {
                status.add_ticks(1);
                let tick = Tick::new_stamped(
                    now,
                    VenueId::Mexc,
                    sym,
                    (scan.seq & 0xFFFF_FFFF) as u32,
                    Price::from_raw(scan.bid_px_1e6),
                    Qty::from_raw(scan.bid_qty_1e6),
                    Price::from_raw(scan.ask_px_1e6),
                    Qty::from_raw(scan.ask_qty_1e6),
                    scan.venue_time_ms,
                    stale * TICK_FLAG_STALE,
                );
                if judged.stale {
                    status.inc_stale_ticks();
                }
                // §6.5: capture BEFORE the push (ring-dropped ticks
                // still reach the replay log).
                capture.tick(&tick);
                if !producer.try_push_ref(&tick) {
                    status.inc_ring_drops();
                    // The engine never saw this quote: forget it, so the
                    // venue's next republication of the same touch is
                    // emitted instead of suppressed (pre-dedupe, the
                    // next push healed a drop within 10 ms).
                    if let Some(r) = drv.rows.get_mut(row) {
                        r.last_bbo = [0; 4];
                    }
                }
            }
        }
        Dispatch::Trades { row, channel, scan } => {
            status.add_msgs(scan.parsed as u64);
            status.add_ticks(scan.parsed as u64);
            let mut r = 0;
            while r < scan.rejected {
                status.inc_parse_errors();
                r += 1;
            }
            if scan.malformed {
                status.inc_parse_errors();
                if let Some(p) = drv.rx.filled().get(reject_range) {
                    capture.parse_reject(now_ns(), p);
                }
            }
            note_confirmed(drv, row, channel);
            if let Some(rs) = drv.rows.get_mut(row) {
                note_seq(&mut rs.last_trade_seq, scan.max_seq, status);
            }
        }
        Dispatch::Ticker { row, sym } => {
            let frame = &ticker;
            status.add_msgs(1);
            status.add_ticks(1);
            note_confirmed(drv, row, MexcChannel::FutTicker);
            let ts_ms = frame.venue_time_ms;
            // §6.5 capture, presence-gated per field group.
            if frame.has_fair == 1 || frame.has_index == 1 {
                capture.event(&ChannelEvent::new(
                    now_ns(),
                    VenueId::Mexc,
                    ChannelId::Mark,
                    sym,
                    0,
                    ts_ms,
                    frame.fair_px_1e6,
                    frame.index_px_1e6,
                ));
            }
            if frame.has_funding == 1 {
                // Q-MX3: advance the latched next-settle instant when
                // this event's venue time reaches it.
                let next = match drv.rows.get_mut(row) {
                    Some(r) => {
                        let next =
                            funding_next_settle_ms(r.funding_next_ms, r.funding_cycle_ms as u64, ts_ms);
                        if next > 0 {
                            r.funding_next_ms = next;
                        }
                        next
                    }
                    None => 0,
                };
                let ev = ChannelEvent::new(
                    now_ns(),
                    VenueId::Mexc,
                    ChannelId::Funding,
                    sym,
                    0,
                    ts_ms,
                    frame.funding_rate_1e9,
                    next.min(i64::MAX as u64) as i64,
                );
                capture.event(&ev);
                // WS10-A: onto the venue-event lane (capture stays first
                // — §6.5 capture-before-push law).
                if event_mask & core_types::event_lane_bit(ChannelId::Funding) != 0
                    && !event_tx.try_push_ref(&ev)
                {
                    status.inc_event_ring_drops();
                }
            }
            if frame.has_hold_vol == 1 {
                capture.event(&ChannelEvent::new(
                    now_ns(),
                    VenueId::Mexc,
                    ChannelId::Ticker,
                    sym,
                    0,
                    ts_ms,
                    0,
                    frame.hold_vol_1e6,
                ));
            }
        }
    }
    Ok(())
}

/// Rate-limited WS2 sub-drop WARN line (zero-alloc stderr; the SubDrop
/// capture event is the 1:1 evidence channel).
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
        match buf.get_mut(*n..*n + src.len()) {
            Some(dst) => {
                // COPY: one part of the WARN line into the 128 B stack line,
                // rate-limited to one line per DROP_LOG_INTERVAL_NS (1 s) — the line
                // must reach stderr in ONE write (no interleaving with tracing) —
                // rejected: one write per part (interleaves), `write!` into the same
                // buffer (the same bytes, through fmt), one `writev` of the parts (a
                // short writev splits the line; `write_all_vectored` is unstable).
                dst.copy_from_slice(src);
                *n += src.len();
            }
            None => *ok = false,
        }
    };
    put(&mut buf, &mut n, &mut ok, b"WARN ingress-mexc: sub-drop class=");
    put(&mut buf, &mut n, &mut ok, drv.class.label().as_bytes());
    put(&mut buf, &mut n, &mut ok, b" suppressed=");
    let mut d = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, fmt_u64(suppressed as u64, &mut d));
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
// Multi-connection loop (the Bybit/Binance M1 shape)
// ---------------------------------------------------------------

/// Stop flag raised by external threads for graceful shutdown.
pub type StopFlag = AtomicBool;

/// One connection slot for [`run_multi`] (spot or futures — the
/// driver's class decides).
pub struct MexcConn<'a, T: Transport> {
    /// Live transport, `None` while disconnected.
    pub transport: Option<T>,
    /// Per-connection driver.
    pub drv: Driver,
    /// Host bytes for the `Host:` header, borrowed from the boot's
    /// endpoint list, which outlives the loop.
    host: &'a [u8],
    /// Request path, borrowed likewise.
    path: &'a [u8],
    keepalive: core_net::Keepalive,
    backoff: core_net::Backoff,
    next_attempt_ns: NsTs,
    session_start_ns: NsTs,
    last_interest: Option<mio::Interest>,
}

impl<'a, T: Transport> MexcConn<'a, T> {
    /// New slot, initially disconnected (`next_attempt_ns` 0 = due
    /// immediately). `host`/`path` are usually the driver class's
    /// [`MexcClass::default_ws_host`] / [`MexcClass::ws_path`] (or the
    /// operator's host override). They are borrowed, not copied: the
    /// conns are built on the ingress thread itself, from an endpoint
    /// list that lives for as long as the loop runs.
    pub fn new(
        drv: Driver,
        host: &'a [u8],
        path: &'a [u8],
        keepalive: core_net::Keepalive,
        backoff: core_net::Backoff,
    ) -> Self {
        // COPY: the Driver (1 792 B, its symbol table inline) moves into its
        // slot here and, with the slot (3 008 B), into the boot's Vec — two
        // moves per connection, once, at boot — rejected: a two-phase
        // in-place init behind `&mut` (every constructor split, for a
        // boot-only cost).
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

    /// Tear down + schedule the next dial. A session that CONFIRMED
    /// subscriptions resets the backoff (the caller passes
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

/// Drive N connections (both classes) on one thread with one tick
/// producer and one event producer until `stop` is set (per-slot
/// failures recycle the slot; only poll-infrastructure failure ends
/// the loop). WS2: each slot enforces the establishment budget — a
/// session with nothing confirmed past the driver's budget is torn
/// down with escalating backoff. The keepalive ping is the slot's class
/// payload.
// Doctrine: raw indices over `conns`, not iterator adapters — hot poll
// loop (CLAUDE.md hot-path rules; `i` is also the mio Token identity).
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
pub fn run_multi<T: Transport, C: Capture>(
    conns: &mut [MexcConn<'_, T>],
    producer: &mut Producer<Tick, TICK_RING_CAP>,
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
                        // COPY: the new transport (1 080 B `TlsTransport`, rustls'
                        // ClientConnection held inline) moves from `connect` into
                        // its slot — once per reconnect, beside a TCP + TLS
                        // handshake that costs orders of magnitude more —
                        // rejected: a placement API on core-net's connect, for
                        // one move per reconnect.
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
                    t, &mut c.drv, c.host, c.path, producer, event_tx, event_mask, status, capture,
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

        // 4. §6.5 capture flush cadence + WS2 establishment budget (one
        //    clock read for both).
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

        // 5. Keepalive per steady slot: the class's ping every interval
        //    AFTER THE LAST PING, busy or not — MEXC closes a socket that
        //    has not heard a client ping for 60 s however much it pushes
        //    (futures `rs.error`, measured live 2026-09-23 — see
        //    `Keepalive::poll_client_heartbeat`).
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
            match c.keepalive.poll_client_heartbeat(now, act, c.session_start_ns) {
                core_net::KeepaliveAction::SendPing => {
                    let ping_ok = queue_masked_text_frame(
                        &mut c.drv.tx,
                        &mut c.drv.mask_counter,
                        c.drv.class.ping_payload(),
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
    use crate::spot::enc;
    use core_net::TestTransport;
    use core_ring::Ring;
    use core_types::NullCapture;

    /// Venue-namespaced test syms (venue byte 7 = Mexc; perps from the
    /// 512 ordinal base).
    const SYM_BTC: u32 = (7 << 24) | 1;
    const SYM_ETH: u32 = (7 << 24) | 2;
    const SYM_FBTC: u32 = (7 << 24) | 513;
    const SYM_FXAU: u32 = (7 << 24) | 514;
    const H8: u64 = 8 * MS_PER_HOUR;

    fn spot_symbols() -> MexcSymbolTable {
        let mut t = MexcSymbolTable::new();
        t.insert(b"BTCUSDT", SYM_BTC).unwrap();
        t.insert(b"ETHUSDT", SYM_ETH).unwrap();
        t
    }

    fn fut_symbols() -> MexcSymbolTable {
        let mut t = MexcSymbolTable::new();
        t.insert(b"BTC_USDT", SYM_FBTC).unwrap();
        t.insert(b"XAU_USDT", SYM_FXAU).unwrap();
        t
    }

    fn steady(class: MexcClass) -> Driver {
        let syms = match class {
            MexcClass::Spot => spot_symbols(),
            MexcClass::Futures => fut_symbols(),
        };
        let mut d = Driver::new(7, class, syms);
        d.state = State::Steady;
        d.subscribed = true;
        d
    }

    fn ring_pair() -> (
        Producer<Tick, TICK_RING_CAP>,
        core_ring::Consumer<Tick, TICK_RING_CAP>,
    ) {
        Ring::<Tick, TICK_RING_CAP>::new().split()
    }

    fn event_ring_pair() -> (
        Producer<ChannelEvent, EVENT_RING_SIZE>,
        core_ring::Consumer<ChannelEvent, EVENT_RING_SIZE>,
    ) {
        Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split()
    }

    /// Drive with a throwaway event lane (mask = FUNDING; consumer
    /// dropped). The event-lane tests call `super::drive_one` directly.
    fn drive<C: Capture>(
        t: &mut TestTransport,
        d: &mut Driver,
        prod: &mut Producer<Tick, TICK_RING_CAP>,
        status: &IngressStatus,
        cap: &mut C,
    ) -> io::Result<()> {
        let (mut etx, _erx) = event_ring_pair();
        super::drive_one(
            t,
            d,
            b"h",
            b"/",
            prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            status,
            cap,
        )
    }

    /// Unmasked server→client frame (0x81 text / 0x82 binary).
    fn ws_frame(first: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(4 + payload.len());
        f.push(first);
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

    fn text(payload: &[u8]) -> Vec<u8> {
        ws_frame(0x81, payload)
    }

    fn binary(payload: &[u8]) -> Vec<u8> {
        ws_frame(0x82, payload)
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
            }
            let mask = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
            at += 4;
            let body: Vec<u8> = (0..len).map(|i| buf[at + i] ^ mask[i & 3]).collect();
            out.push((op, body));
            buf = &buf[at + len..];
        }
        out
    }

    /// Records every event / tick / reject.
    #[derive(Default)]
    struct Rec {
        events: Vec<ChannelEvent>,
        ticks: Vec<Tick>,
        rejects: u32,
    }
    impl Capture for Rec {
        fn tick(&mut self, t: &Tick) {
            self.ticks.push(*t);
        }
        fn event(&mut self, e: &ChannelEvent) {
            self.events.push(*e);
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

    /// Run a fresh driver through the real handshake; returns the
    /// subscribe frames it queued at the upgrade edge.
    fn handshake(d: &mut Driver, seed: u64, host: &[u8], path: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut t = TestTransport::with_capacity(65536);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let (mut etx, _erx) = event_ring_pair();
        note_transport_ready(d, Status::Ready);
        super::drive_one(&mut t, d, host, path, &mut prod, &mut etx, 0, &status, &mut NullCapture).unwrap();
        let mut scratch = vec![0u8; 65536];
        let n = t.drain_outgoing(&mut scratch);
        let req = &scratch[..n];
        let get = format!("GET {} HTTP/1.1", core::str::from_utf8(path).unwrap());
        assert!(memchr::memmem::find(req, get.as_bytes()).is_some(), "request line");
        assert!(memchr::memmem::find(req, host).is_some(), "Host header");

        let key = core_net::sec_websocket_key_from_seed(seed);
        let accept = core_net::expected_accept(&key);
        let mut resp: Vec<u8> = Vec::new();
        resp.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
        resp.extend_from_slice(b"Upgrade: websocket\r\nConnection: Upgrade\r\n");
        resp.extend_from_slice(b"Sec-WebSocket-Accept: ");
        resp.extend_from_slice(&accept);
        resp.extend_from_slice(b"\r\n\r\n");
        t.inject_incoming(&resp);
        super::drive_one(&mut t, d, host, path, &mut prod, &mut etx, 0, &status, &mut NullCapture).unwrap();
        assert_eq!(d.state(), State::Steady);
        assert!(d.subscribed);
        assert_eq!(status.state(), IngressState::Up);
        let n = t.drain_outgoing(&mut scratch);
        client_frames(&scratch[..n])
    }

    fn book_push(symbol: &[u8], bid: &[u8], ask: &[u8], version: &[u8], send_ms: u64) -> Vec<u8> {
        let mut body = Vec::new();
        enc::len_field(&mut body, 1, bid);
        enc::len_field(&mut body, 2, b"1.5");
        enc::len_field(&mut body, 3, ask);
        enc::len_field(&mut body, 4, b"2.0");
        if !version.is_empty() {
            enc::len_field(&mut body, 5, version);
        }
        let mut f = Vec::new();
        enc::len_field(&mut f, 1, b"spot@public.aggre.bookTicker.v3.api.pb@10ms@X");
        enc::len_field(&mut f, 3, symbol);
        enc::varint_field(&mut f, 6, send_ms);
        enc::len_field(&mut f, 315, &body);
        f
    }

    fn deals_push(symbol: &[u8], ids: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut k = 0;
        while k < ids.len() {
            enc::len_field(&mut body, 1, &enc::deal_item(b"100.5", b"0.25", 1 + (k as u64 % 2), 1_000 + k as u64, ids[k]));
            k += 1;
        }
        let mut f = Vec::new();
        enc::len_field(&mut f, 3, symbol);
        enc::varint_field(&mut f, 6, 999);
        enc::len_field(&mut f, 314, &body);
        f
    }

    const ACK_ALL_OK: &[u8] = br#"{"id":0,"code":0,"msg":"spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT,spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT,spot@public.aggre.bookTicker.v3.api.pb@10ms@ETHUSDT,spot@public.aggre.deals.v3.api.pb@10ms@ETHUSDT"}"#;
    const ACK_ETH_DEALS_BLOCKED: &[u8] = "{\"id\":0,\"code\":0,\"msg\":\"Subscribed successful! [spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT]. Not Subscribed successfully! [spot@public.aggre.deals.v3.api.pb@10ms@ETHUSDT,spot@public.aggre.bookTicker.v3.api.pb@10ms@DOGEUSDT].  Reason： Blocked! \"}".as_bytes();

    // ---- handshake + subscribe -------------------------------------

    #[test]
    fn spot_handshake_emits_one_subscription_frame() {
        let mut d = Driver::new(42, MexcClass::Spot, spot_symbols());
        let frames = handshake(&mut d, 42, b"wbs-api.mexc.com", b"/ws");
        assert_eq!(frames.len(), 1, "ONE SUBSCRIPTION frame");
        assert_eq!(frames[0].0, 0x1, "text opcode");
        assert_eq!(
            frames[0].1,
            br#"{"method":"SUBSCRIPTION","params":["spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT","spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT","spot@public.aggre.bookTicker.v3.api.pb@10ms@ETHUSDT","spot@public.aggre.deals.v3.api.pb@10ms@ETHUSDT"]}"#.to_vec()
        );
        assert_eq!(d.sub_count(), 0, "nothing confirmed before the ack");
    }

    #[test]
    fn futures_handshake_emits_one_frame_per_symbol_channel() {
        let mut d = Driver::new(43, MexcClass::Futures, fut_symbols());
        let frames = handshake(&mut d, 43, b"contract.mexc.com", b"/edge");
        let got: Vec<Vec<u8>> = frames.iter().map(|f| f.1.clone()).collect();
        let want: Vec<Vec<u8>> = [
            &br#"{"method":"sub.depth.full","param":{"symbol":"BTC_USDT","limit":5}}"#[..],
            br#"{"method":"sub.deal","param":{"symbol":"BTC_USDT"}}"#,
            br#"{"method":"sub.ticker","param":{"symbol":"BTC_USDT"}}"#,
            br#"{"method":"sub.depth.full","param":{"symbol":"XAU_USDT","limit":5}}"#,
            br#"{"method":"sub.deal","param":{"symbol":"XAU_USDT"}}"#,
            br#"{"method":"sub.ticker","param":{"symbol":"XAU_USDT"}}"#,
        ]
        .iter()
        .map(|s| s.to_vec())
        .collect();
        assert_eq!(got, want);
        assert!(frames.iter().all(|f| f.0 == 0x1));
    }

    #[test]
    fn full_class_caps_fit_the_tx_budget_and_over_cap_refuses() {
        // 13 perps × 3 frames and 15 spot × 2 params queue in one go.
        let mut t = MexcSymbolTable::new();
        let mut k = 0u32;
        while k < 13 {
            let name = format!("SYMBOLNAME{k:02}_USDT");
            t.insert(name.as_bytes(), (7 << 24) | (513 + k)).unwrap();
            k += 1;
        }
        let mut d = Driver::new(5, MexcClass::Futures, t);
        assert_eq!(handshake(&mut d, 5, b"h", b"/edge").len(), 39);
        let mut t = MexcSymbolTable::new();
        let mut k = 0u32;
        while k < 15 {
            let name = format!("SYMBOLNAME{k:02}USDT");
            t.insert(name.as_bytes(), (7 << 24) | (1 + k)).unwrap();
            k += 1;
        }
        let mut d = Driver::new(6, MexcClass::Spot, t);
        let frames = handshake(&mut d, 6, b"h", b"/ws");
        assert_eq!(frames.len(), 1);
        assert_eq!(memchr::memmem::find_iter(&frames[0].1, b"\"spot@").count(), 30, "30 params");

        // Over the class cap: the subscribe refuses (release semantics;
        // debug builds assert at construction).
        if cfg!(debug_assertions) {
            return;
        }
        let mut t = MexcSymbolTable::new();
        let mut k = 0u32;
        while k < 16 {
            t.insert(format!("S{k:02}USDT").as_bytes(), (7 << 24) | (1 + k)).unwrap();
            k += 1;
        }
        let mut d = Driver::new(8, MexcClass::Spot, t);
        d.state = State::Steady;
        assert!(queue_subscribe_all(&mut d).is_err());
    }

    // ---- spot data ---------------------------------------------------

    #[test]
    fn spot_golden_book_push_emits_the_exact_tick() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let mut cap = Rec::default();
        let frame = enc::wrapper(enc::CHANNEL_BOOK, b"BTCUSDT", 315, &enc::book_body());
        t.inject_incoming(&binary(&frame));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).unwrap();

        let tick = *cons.try_pop_ref().expect("tick");
        assert_eq!(tick.venue, VenueId::Mexc as u8);
        assert_eq!(tick.sym, SYM_BTC);
        assert_eq!(tick.bid_px.raw(), 80_535_880_000);
        assert_eq!(tick.bid_qty.raw(), 380_497);
        assert_eq!(tick.ask_px.raw(), 80_535_890_000);
        assert_eq!(tick.ask_qty.raw(), 333_363);
        assert_eq!(tick.venue_seq, (81_721_676_217u64 & 0xFFFF_FFFF) as u32, "truncated version");
        assert_eq!(tick.venue_time_ms, enc::SEND_TIME, "sendTime (createTime absent)");
        assert!(!tick.is_stale());
        assert_eq!(cap.ticks.len(), 1, "captured before the push");
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.ticks_total(), 1);
        assert_eq!(status.parse_errors_total(), 0);
        assert_eq!(d.sub_count(), 1, "data confirms the pair");
        assert_eq!(d.rows[0].last_book_seq, 81_721_676_217, "full width kept");
    }

    #[test]
    fn spot_deals_push_captures_trade_events() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let mut cap = Rec::default();
        let frame = enc::wrapper(b"spot@public.aggre.deals.v3.api.pb@10ms@ETHUSDT", b"ETHUSDT", 314, &enc::deals_body());
        t.inject_incoming(&binary(&frame));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).unwrap();

        assert!(cons.try_pop_ref().is_none(), "prints are events, not ticks");
        let tr = cap.of(ChannelId::Trade);
        assert_eq!(tr.len(), 2);
        assert_eq!(tr[0].sym, SYM_ETH);
        assert_eq!(tr[0].venue, VenueId::Mexc as u8);
        assert_eq!(tr[0].v0, 80_535_880_000);
        assert_eq!(tr[0].v1, -13_620, "sell aggressor = negated qty");
        assert_eq!(tr[0].venue_seq, 730_292_425_431_437_318, "full-width u64 seq");
        assert_eq!(tr[0].venue_time_ms, enc::DEAL_TIME);
        assert_eq!(tr[1].v1, 1_234, "buy = positive");
        assert_eq!(status.msgs_total(), 2);
        assert_eq!(status.ticks_total(), 2);
        assert_eq!(d.rows[1].last_trade_seq, 730_292_425_431_437_320, "max of the push");
        assert_eq!(d.sub_count(), 1);
    }

    // ---- spot ack law ------------------------------------------------

    #[test]
    fn spot_ack_success_confirms_every_requested_pair() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        t.inject_incoming(&text(ACK_ALL_OK));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.sub_count(), 4, "2 symbols × 2 channels");
        assert!(d.ever_confirmed);
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.sub_drops_total(), 0);
        // Pong: quiet (activity only).
        t.inject_incoming(&text(br#"{"id":0,"code":0,"msg":"PONG"}"#));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.parse_errors_total(), 0);
    }

    /// Plan R10 (MEXC delists aggressively): a refused param at BOOT
    /// must not blind the socket's other symbols — the pairs the same
    /// ack confirms make the refusal a per-param drop.
    #[test]
    fn spot_ack_with_a_blocked_param_at_boot_drops_it_and_confirms_the_rest() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&text(ACK_ETH_DEALS_BLOCKED));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).expect("a partial refusal is not fatal");
        assert!(d.ever_confirmed);
        assert_eq!(d.sub_count(), 3, "4 requested − ETH deals");
        assert_eq!(cap.of(ChannelId::SubDrop).len(), 2);
        assert_eq!(status.sub_drops_total(), 2);
    }

    #[test]
    fn spot_ack_refusing_every_pair_is_fatal_at_boot() {
        // Release semantics only (debug builds assert, the Bybit law).
        if cfg!(debug_assertions) {
            return;
        }
        // The live refusal shape (2026-09-23): `code` stays 0.
        let all_blocked = "{\"id\":0,\"code\":0,\"msg\":\"Not Subscribed successfully! [spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT,spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT,spot@public.aggre.bookTicker.v3.api.pb@10ms@ETHUSDT,spot@public.aggre.deals.v3.api.pb@10ms@ETHUSDT].  Reason： Blocked! \"}".as_bytes();
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&text(all_blocked));
        let e = drive(&mut t, &mut d, &mut prod, &status, &mut cap).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(status.take_last_err().site, core_metrics::ERR_SITE_VENUE_ERROR);
        assert!(cap.of(ChannelId::SubDrop).is_empty(), "the fatal path emits no drop");
        assert_eq!(status.sub_drops_total(), 0);
        assert!(!d.ever_confirmed);
        // A whole-request refusal at boot is fatal too, and keeps the code.
        let mut t2 = TestTransport::with_capacity(8192);
        let mut d2 = steady(MexcClass::Spot);
        let s2 = IngressStatus::new();
        t2.inject_incoming(&text(br#"{"id":0,"code":30001,"msg":"bad"}"#));
        assert!(drive(&mut t2, &mut d2, &mut prod, &s2, &mut NullCapture).is_err());
        assert_eq!(s2.take_last_err().venue_code, 30001);
    }

    #[test]
    fn spot_ack_blocked_param_after_first_success_is_a_per_param_drop() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Spot);
        d.ever_confirmed = true; // a pair was ever confirmed (reconnect)
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&text(ACK_ETH_DEALS_BLOCKED));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).expect("non-fatal after first success");

        let drops = cap.of(ChannelId::SubDrop);
        assert_eq!(drops.len(), 2, "one SubDrop per refused param");
        assert_eq!(status.sub_drops_total(), 2, "1:1 with the counter");
        assert_eq!(drops[0].sym, SYM_ETH);
        assert_eq!(drops[0].v0, SUB_DROP_REFUSED);
        assert_eq!(drops[0].v1, MexcChannel::SpotDeals.discriminant());
        assert_eq!(drops[1].sym, SYMBOL_ID_NONE, "DOGEUSDT is not on this connection");
        assert_eq!(drops[1].v1, MexcChannel::SpotBookTicker.discriminant());
        assert_eq!(d.sub_count(), 3, "4 requested − ETH deals");
        assert_eq!(d.rows[1].confirmed, 0b01, "ETH book confirmed, deals refused");
    }

    #[test]
    fn spot_whole_request_refusal_after_first_success_is_one_drop_with_the_code() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Spot);
        d.ever_confirmed = true;
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&text(br#"{"id":0,"code":30001,"msg":"rate limited"}"#));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).unwrap();
        let drops = cap.of(ChannelId::SubDrop);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].v0, 30001, "the ack's code");
        assert_eq!(drops[0].v1, -1);
        assert_eq!(drops[0].sym, SYMBOL_ID_NONE);
        assert_eq!(status.sub_drops_total(), 1);
        assert_eq!(d.sub_count(), 0, "nothing confirmed — the budget will reap");
    }

    // ---- futures -----------------------------------------------------

    const F_DEPTH: &[u8] = br#"{"symbol":"BTC_USDT","data":{"cts":1789897581009,"asks":[[80468.7,3446,2],[80469.1,1239,1]],"bids":[[80468.6,31288,7]],"version":41925002140},"channel":"push.depth.full","ts":1789897581013}"#;
    const F_DEAL: &[u8] = br#"{"symbol":"BTC_USDT","data":[{"p":80489,"v":11,"T":1,"O":3,"M":1,"t":1789897547210,"i":"16270106116","cts":"1789897547210"},{"p":80488.5,"v":2,"T":2,"O":3,"M":2,"t":1789897547211,"i":"16270106117"}],"channel":"push.deal","ts":1789897547215}"#;

    fn fut_ticker(rate: &str, ts_ms: u64) -> Vec<u8> {
        format!(r#"{{"symbol":"XAU_USDT","data":{{"symbol":"XAU_USDT","lastPrice":4377.18,"indexPrice":4377.63,"fairPrice":4377.21,"fundingRate":{rate},"holdVol":92415393,"timestamp":{ts_ms}}},"channel":"push.ticker","ts":{ts_ms}}}"#).into_bytes()
    }

    #[test]
    fn futures_golden_depth_emits_the_exact_tick() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        t.inject_incoming(&text(F_DEPTH));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        let tick = *cons.try_pop_ref().expect("tick");
        assert_eq!(tick.sym, SYM_FBTC);
        assert_eq!(tick.venue, VenueId::Mexc as u8);
        assert_eq!(tick.bid_px.raw(), 80_468_600_000);
        assert_eq!(tick.bid_qty.raw(), 31_288_000_000, "contracts ×1e6");
        assert_eq!(tick.ask_px.raw(), 80_468_700_000);
        assert_eq!(tick.ask_qty.raw(), 3_446_000_000);
        assert_eq!(tick.venue_seq, (41_925_002_140u64 & 0xFFFF_FFFF) as u32);
        assert_eq!(tick.venue_time_ms, 1_789_897_581_009, "cts");
        assert_eq!(d.rows[0].last_book_seq, 41_925_002_140);
    }

    #[test]
    fn futures_one_sided_depth_emits_no_tick_and_no_error() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        t.inject_incoming(&text(br#"{"symbol":"BTC_USDT","data":{"cts":5,"asks":[],"bids":[[1.5,2,1]],"version":3},"channel":"push.depth.full","ts":6}"#));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert!(cons.try_pop_ref().is_none());
        assert_eq!(status.parse_errors_total(), 0);
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(d.sub_count(), 1, "still data: the pair is confirmed");
    }

    #[test]
    fn futures_deal_captures_trade_events() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&text(F_DEAL));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).unwrap();
        let tr = cap.of(ChannelId::Trade);
        assert_eq!(tr.len(), 2);
        assert_eq!(tr[0].sym, SYM_FBTC);
        assert_eq!(tr[0].v0, 80_489_000_000);
        assert_eq!(tr[0].v1, 11_000_000);
        assert_eq!(tr[0].venue_seq, 16_270_106_116);
        assert_eq!(tr[0].venue_time_ms, 1_789_897_547_210);
        assert_eq!(tr[1].v1, -2_000_000, "T=2 sells");
        assert_eq!(d.rows[0].last_trade_seq, 16_270_106_117);
    }

    #[test]
    fn futures_ticker_emits_mark_funding_and_oi_and_funding_reaches_the_lane() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Futures);
        let t0 = 1_789_920_000_000u64;
        assert!(d.set_funding_seed(SYM_FXAU, t0, 8));
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let (mut etx, mut erx) = event_ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&text(&fut_ticker("0.0001", 1_789_897_545_754)));
        super::drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &mut etx, core_types::EVENT_LANE_FUNDING, &status, &mut cap).unwrap();
        let mark = cap.of(ChannelId::Mark);
        assert_eq!(mark.len(), 1);
        assert_eq!(mark[0].sym, SYM_FXAU);
        assert_eq!(mark[0].v0, 4_377_210_000, "fairPrice");
        assert_eq!(mark[0].v1, 4_377_630_000, "indexPrice");
        assert_eq!(mark[0].venue_time_ms, 1_789_897_545_754);
        let fu = cap.of(ChannelId::Funding);
        assert_eq!(fu.len(), 1);
        assert_eq!(fu[0].v0, 100_000, "0.0001 ×1e9");
        assert_eq!(fu[0].v1, t0 as i64, "next settle from the seed");
        let oi = cap.of(ChannelId::Ticker);
        assert_eq!(oi.len(), 1);
        assert_eq!((oi[0].v0, oi[0].v1), (0, 92_415_393_000_000));
        let lane = *erx.try_pop_ref().expect("funding on the lane");
        assert_eq!(lane.channel, ChannelId::Funding as u8);
        assert_eq!(lane.v1, t0 as i64);
        assert!(erx.try_pop_ref().is_none(), "Mark/OI stay capture-only");
        assert_eq!(status.event_ring_drops_total(), 0);
        // Mask without the funding bit: capture only.
        t.inject_incoming(&text(&fut_ticker("0.0001", 1_789_897_545_760)));
        super::drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &mut etx, 0, &status, &mut cap).unwrap();
        assert!(erx.try_pop_ref().is_none());
        assert_eq!(cap.of(ChannelId::Funding).len(), 2);
    }

    #[test]
    fn funding_v1_advances_exactly_across_a_settlement() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Futures);
        let t0 = 1_789_920_000_000u64;
        assert!(d.set_funding_seed(SYM_FXAU, t0, 8));
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        // Before, AT, and after the boundary; then a long gap.
        for (ts, want) in [(t0 - 1, t0), (t0, t0 + H8), (t0 + 1, t0 + H8), (t0 + 3 * H8 + 5, t0 + 4 * H8)] {
            t.inject_incoming(&text(&fut_ticker("0.0002", ts)));
            drive(&mut t, &mut d, &mut prod, &status, &mut cap).unwrap();
            let fu = cap.of(ChannelId::Funding);
            assert_eq!(fu.last().unwrap().v1, want as i64, "venue ts {ts}");
        }
        // An event older than the latched instant never moves it back.
        t.inject_incoming(&text(&fut_ticker("0.0002", t0)));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.of(ChannelId::Funding).last().unwrap().v1, (t0 + 4 * H8) as i64);
    }

    #[test]
    fn funding_unseeded_or_cycle_zero_emits_v1_zero_and_seeding_is_futures_only() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&text(&fut_ticker("-0.0001", 5)));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.of(ChannelId::Funding)[0].v1, 0, "unseeded");
        assert_eq!(cap.of(ChannelId::Funding)[0].v0, -100_000);
        assert!(d.set_funding_seed(SYM_FXAU, 1_000, 0));
        t.inject_incoming(&text(&fut_ticker("0", 5_000)));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.of(ChannelId::Funding)[1].v1, 0, "cycle 0");
        // Failure modes of the setter.
        assert!(!d.set_funding_seed(SYM_BTC, 1, 8), "not on this driver");
        let mut s = steady(MexcClass::Spot);
        assert!(!s.set_funding_seed(SYM_BTC, 1, 8), "spot has no funding");
    }

    #[test]
    fn futures_confirmation_is_first_data_per_pair() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        // Acks name no symbol: they confirm nothing.
        t.inject_incoming(&text(br#"{"channel":"rs.sub.depth.full","data":"success","ts":1}"#));
        t.inject_incoming(&text(br#"{"channel":"rs.sub.deal","data":"success","ts":1}"#));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.sub_count(), 0);
        assert!(!d.ever_confirmed);
        assert_eq!(status.msgs_total(), 2);
        t.inject_incoming(&text(F_DEPTH));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.sub_count(), 1);
        t.inject_incoming(&text(F_DEPTH));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.sub_count(), 1, "a pair counts once");
        t.inject_incoming(&text(F_DEAL));
        t.inject_incoming(&text(&fut_ticker("0", 7)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.sub_count(), 3);
        assert_eq!(d.rows[0].confirmed, 0b011);
        assert_eq!(d.rows[1].confirmed, 0b100);
        // Pong is quiet.
        t.inject_incoming(&text(br#"{"channel":"pong","data":1,"ts":1}"#));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.parse_errors_total(), 0);
    }

    /// A non-success `rs.sub.*` names no symbol: on a never-confirmed
    /// driver it is the venue-blind boot fail-fast.
    #[test]
    fn futures_rs_sub_refusal_is_fatal_at_boot() {
        if cfg!(debug_assertions) {
            return;
        }
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        t.inject_incoming(&text(br#"{"channel":"rs.sub.deal","data":"fail","ts":1}"#));
        let e = drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(status.take_last_err().site, core_metrics::ERR_SITE_VENUE_ERROR);
    }

    #[test]
    fn futures_refusal_after_first_data_is_a_drop() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&text(F_DEPTH));
        t.inject_incoming(&text(br#"{"channel":"rs.sub.deal","data":"contract not exists","ts":1}"#));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).expect("non-fatal after first data");
        let drops = cap.of(ChannelId::SubDrop);
        assert_eq!(drops.len(), 1);
        assert_eq!(status.sub_drops_total(), 1);
        assert_eq!((drops[0].sym, drops[0].v0), (SYMBOL_ID_NONE, SUB_DROP_REFUSED));
        assert_eq!(drops[0].v1, MexcChannel::FutDeal.discriminant());
    }

    /// The live `rs.error` forms (2026-09-23). A refused CONTRACT is a
    /// per-symbol drop and never fatal — at boot too (plan R10: one
    /// delisted row must not blind the socket). Any other `rs.error`
    /// (the 60 s heartbeat notice) refuses nothing: a session error on
    /// the exit line, not a drop.
    #[test]
    fn futures_rs_error_forms_are_a_symbol_drop_or_a_notice() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&text(br#"{"channel":"rs.error","data":"Contract [XAU_USDT] not exists","ts":1790148334500}"#));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).expect("a refused contract is never fatal");
        let drops = cap.of(ChannelId::SubDrop);
        assert_eq!(drops.len(), 1);
        assert_eq!((drops[0].sym, drops[0].v0, drops[0].v1), (SYM_FXAU, SUB_DROP_REFUSED, -1));
        assert!(!d.ever_confirmed, "a refusal confirms nothing");
        t.inject_incoming(&text(br#"{"channel":"rs.error","data":"more than 60 seconds no response, close the channel","ts":1790148403253}"#));
        drive(&mut t, &mut d, &mut prod, &status, &mut cap).expect("a notice is not a refusal");
        assert_eq!(status.sub_drops_total(), 1, "the notice is not a drop");
        assert_eq!(status.take_last_err().site, core_metrics::ERR_SITE_VENUE_ERROR);
        assert_eq!(status.parse_errors_total(), 0);
    }

    // ---- seq regressions (Q-MX1) --------------------------------------

    #[test]
    fn seq_regressions_are_counted_per_symbol_and_stream() {
        let mut t = TestTransport::with_capacity(65536);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let push = |t: &mut TestTransport, f: Vec<u8>| {
            t.inject_incoming(&binary(&f));
        };
        push(&mut t, book_push(b"BTCUSDT", b"1", b"2", b"100", 1));
        push(&mut t, book_push(b"BTCUSDT", b"1", b"2", b"100", 2)); // equal: fine
        push(&mut t, book_push(b"BTCUSDT", b"1", b"2", b"150", 3)); // skip ahead: fine
        push(&mut t, book_push(b"ETHUSDT", b"1", b"2", b"5", 4)); // other symbol: own state
        push(&mut t, book_push(b"BTCUSDT", b"1", b"2", b"", 5)); // absent: never counts
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.seq_regressions_total(), 0);
        assert_eq!(status.gaps_total(), 0, "no chain law on this venue");
        push(&mut t, book_push(b"BTCUSDT", b"1", b"2", b"149", 6)); // regression
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.seq_regressions_total(), 1);
        push(&mut t, book_push(b"BTCUSDT", b"1", b"2", b"149", 7)); // last-seen, not max
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.seq_regressions_total(), 1);
        // Trade stream: judged by the push's largest id, independent of
        // the book stream and of intra-push order.
        push(&mut t, deals_push(b"BTCUSDT", &[b"20X0_1", b"10X0_2"]));
        push(&mut t, deals_push(b"BTCUSDT", &[b"21X0_1"]));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.seq_regressions_total(), 1);
        push(&mut t, deals_push(b"BTCUSDT", &[b"15X0_1", b"X"]));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.seq_regressions_total(), 2);
        assert_eq!(status.parse_errors_total(), 0);
    }

    #[test]
    fn futures_seq_regressions_on_book_and_deal() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        t.inject_incoming(&text(F_DEPTH));
        t.inject_incoming(&text(br#"{"symbol":"BTC_USDT","data":{"asks":[[2,1,1]],"bids":[[1,1,1]],"version":41925002139},"channel":"push.depth.full","ts":9}"#));
        t.inject_incoming(&text(F_DEAL));
        t.inject_incoming(&text(br#"{"symbol":"BTC_USDT","data":[{"p":1,"v":1,"T":1,"t":5,"i":"16270106000"}],"channel":"push.deal","ts":9}"#));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.seq_regressions_total(), 2);
    }

    // ---- staleness -----------------------------------------------------

    #[test]
    fn spot_ticks_carry_send_time_and_the_stale_judgement() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let t0: u64 = 1_789_897_517_479;
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"1", t0)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        let fresh = *cons.try_pop_ref().unwrap();
        assert!(!fresh.is_stale());
        assert_eq!(fresh.venue_time_ms, t0);
        // 5 s older than the learned offset: stale at the mexc 400 ms default.
        // (a changed ask — an identical quote is not a tick)
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"3", b"2", t0 - 5_000)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        let stale = *cons.try_pop_ref().unwrap();
        assert!(stale.is_stale());
        assert_eq!(stale.flags, TICK_FLAG_STALE);
        assert_eq!(status.stale_ticks_total(), 1);
        assert!(d.feed_delay_ema_ms() > 0);
        assert!(status.feed_delay_ema_ms() > 0);
    }

    /// An emptied spot side (proto3 omits the strings) is a quote the
    /// driver does not emit — and not a parse error.
    #[test]
    fn a_one_sided_spot_book_is_no_tick_and_no_error() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let mut body = Vec::new();
        enc::len_field(&mut body, 3, b"2");
        enc::len_field(&mut body, 4, b"1");
        enc::len_field(&mut body, 5, b"9");
        let mut f = Vec::new();
        enc::len_field(&mut f, 3, b"BTCUSDT");
        enc::varint_field(&mut f, 6, 1_789_897_517_479);
        enc::len_field(&mut f, 315, &body);
        t.inject_incoming(&binary(&f));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert!(cons.try_pop_ref().is_none(), "one side is not a BBO");
        assert_eq!(status.parse_errors_total(), 0);
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(d.rows[0].last_book_seq, 9, "its version still counts");
    }

    /// The BBO-change doctrine (measured live 2026-09-23: spot
    /// republishes an unchanged quote every 10 ms): an identical push
    /// is a msg, not a tick — any changed field emits, and a reconnect
    /// re-emits the first quote. The seq check still sees every push.
    #[test]
    fn an_unchanged_bbo_is_not_a_tick_until_it_changes_or_reconnects() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let t0: u64 = 1_789_897_517_479;
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"10", t0)));
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"11", t0 + 10)));
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"12", t0 + 20)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert!(cons.try_pop_ref().is_some(), "the first quote is a tick");
        assert!(cons.try_pop_ref().is_none(), "two republications are not");
        assert_eq!(status.msgs_total(), 3);
        assert_eq!(status.ticks_total(), 1);
        assert_eq!(d.rows[0].last_book_seq, 12, "the seq check saw every push");
        // A changed BBO emits.
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"3", b"13", t0 + 30)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(cons.try_pop_ref().unwrap().ask_px.raw(), 3_000_000);
        // A reconnect re-emits the same quote once.
        d.reset_for_reconnect(5);
        d.state = State::Steady;
        d.subscribed = true;
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"3", b"14", t0 + 40)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert!(cons.try_pop_ref().is_some(), "first quote of a new session");
        assert_eq!(status.ticks_total(), 3);
    }

    /// Review finding: the BBO dedupe must not LATCH a stale verdict —
    /// an unchanged quote whose stale judgement flipped is a tick. The
    /// vm mirrors the latest tick's flag (VT3: reads ABSENT while set),
    /// so a suppressed fresh republication of a stale-flagged quote
    /// muted the symbol until the touch next moved.
    #[test]
    fn an_unchanged_bbo_whose_stale_verdict_flips_is_a_tick() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let t0: u64 = 1_789_897_517_479;
        // Fresh, then the SAME touch 5 s late (stale at the 400 ms default).
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"1", t0)));
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"2", t0 - 5_000)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert!(!cons.try_pop_ref().expect("first quote").is_stale());
        let late = *cons
            .try_pop_ref()
            .expect("an unchanged quote that went stale is a tick");
        assert!(late.is_stale());
        // The SAME touch fresh again: the tick that un-mutes the vm; the
        // next fresh republication is a duplicate again.
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"3", t0 + 20)));
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"4", t0 + 30)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        let healed = *cons
            .try_pop_ref()
            .expect("the fresh republication of a stale quote is a tick");
        assert!(!healed.is_stale());
        assert_eq!(healed.bid_px.raw(), 1_000_000);
        assert!(
            cons.try_pop_ref().is_none(),
            "fresh and unchanged: still no tick"
        );
        assert_eq!(status.msgs_total(), 4);
        assert_eq!(status.ticks_total(), 3);
        assert_eq!(status.stale_ticks_total(), 1);
    }

    /// Review finding: a quote the ring DROPPED never reached the
    /// engine — the venue's next republication of the same touch must
    /// be emitted, not suppressed as a duplicate of it.
    #[test]
    fn a_ring_dropped_quote_is_re_emitted_on_its_next_republication() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let filler = Tick::new_stamped(
            1,
            VenueId::Mexc,
            SYM_ETH,
            0,
            Price::from_raw(1),
            Qty::from_raw(1),
            Price::from_raw(2),
            Qty::from_raw(1),
            0,
            0,
        );
        let mut filled = 0usize;
        while prod.try_push_ref(&filler) {
            filled += 1;
        }
        let t0: u64 = 1_789_897_517_479;
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"1", t0)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.ring_drops_total(), 1);
        // Room again; the venue republishes the SAME touch.
        let mut k = 0usize;
        while k < filled {
            assert!(cons.try_pop_ref().is_some());
            k += 1;
        }
        t.inject_incoming(&binary(&book_push(b"BTCUSDT", b"1", b"2", b"2", t0 + 10)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        let tick = *cons
            .try_pop_ref()
            .expect("the dropped quote reaches the engine on its republication");
        assert_eq!(tick.sym, SYM_BTC);
        assert!(cons.try_pop_ref().is_none());
        assert_eq!(status.ring_drops_total(), 1);
    }

    #[test]
    fn futures_stale_threshold_override_and_reconnect_reset() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady(MexcClass::Futures);
        d.set_stale_after_ms(10_000);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        // `v` is also the ask size, so every push is a BBO change.
        let depth = |cts: u64, v: u64| {
            format!(r#"{{"symbol":"BTC_USDT","data":{{"cts":{cts},"asks":[[2,{v},1]],"bids":[[1,1,1]],"version":{v}}},"channel":"push.depth.full","ts":{cts}}}"#).into_bytes()
        };
        let t0 = 1_789_897_581_009u64;
        t.inject_incoming(&text(&depth(t0, 1)));
        t.inject_incoming(&text(&depth(t0 - 5_000, 2)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert!(!cons.try_pop_ref().unwrap().is_stale());
        assert!(
            !cons.try_pop_ref().unwrap().is_stale(),
            "5 s under a 10 s threshold"
        );
        // Reconnect: a fresh offset; confirmations clear; seqs and the
        // funding seed survive.
        assert!(d.set_funding_seed(SYM_FBTC, 77, 8));
        d.ever_confirmed = true;
        d.reset_for_reconnect(9);
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.sub_count(), 0);
        assert!(d.ever_confirmed, "process-lifetime");
        assert_eq!(d.rows[0].last_book_seq, 2);
        assert_eq!(d.rows[0].funding_next_ms, 77);
        d.state = State::Steady;
        d.subscribed = true;
        t.inject_incoming(&text(&depth(t0 - 60_000, 3)));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert!(
            !cons.try_pop_ref().unwrap().is_stale(),
            "a reconnect starts a fresh offset"
        );
    }

    // ---- rejections ----------------------------------------------------

    #[test]
    fn unknown_symbols_channels_and_opcodes_count_rejects() {
        let mut t = TestTransport::with_capacity(16384);
        let mut s = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = Rec::default();
        t.inject_incoming(&binary(&book_push(b"DOGEUSDT", b"1", b"2", b"1", 1))); // unknown sym
        t.inject_incoming(&text(&book_push(b"BTCUSDT", b"1", b"2", b"1", 1))); // PB as text
        t.inject_incoming(&binary(b"\x0b\x00")); // group wire type
        t.inject_incoming(&text(br#"{"nonsense":true}"#));
        drive(&mut t, &mut s, &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.parse_errors_total(), 4);
        assert_eq!(cap.rejects, 4);
        assert_eq!(status.msgs_total(), 0);

        let mut f = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        t.inject_incoming(&text(br#"{"symbol":"DOGE_USDT","data":{"asks":[],"bids":[]},"channel":"push.depth.full","ts":1}"#));
        t.inject_incoming(&text(br#"{"symbol":"BTC_USDT","data":{},"channel":"push.kline","ts":1}"#));
        t.inject_incoming(&binary(F_DEPTH)); // futures never speaks binary
        t.inject_incoming(&text(br#"{"symbol":"BTC_USDT","data":[{"p":1,"v":1,"T":9}],"channel":"push.deal","ts":1}"#)); // bad row
        t.inject_incoming(&text(br#"{"symbol":"BTC_USDT","data":[{"p":1,"v":1,"T":1},7],"channel":"push.deal","ts":1}"#)); // malformed
        drive(&mut t, &mut f, &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.parse_errors_total(), 5);
        assert_eq!(status.msgs_total(), 1, "the one good print");
    }

    /// The WS Ping echo goes rx → tx with no scratch (the BX0 shape): an
    /// empty, a 4 B and a 125 B (control-frame cap) ping each come back
    /// as exactly one masked pong carrying the same bytes.
    #[test]
    fn a_venue_ws_ping_is_answered_with_a_pong() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady(MexcClass::Spot);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        for payload in TestTransport::PING_ECHO_CASES {
            t.inject_server_ping(payload);
            drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
            t.expect_pong_echo(payload);
        }
        // A close frame closes.
        t.inject_incoming(&ws_frame(0x88, b""));
        drive(&mut t, &mut d, &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.state(), State::Closed);
    }

    #[test]
    fn transport_status_malformed_and_oversized_frames_fail_fast() {
        let mut d = Driver::new(1, MexcClass::Futures, fut_symbols());
        assert_eq!(d.class(), MexcClass::Futures);
        note_transport_ready(&mut d, Status::Handshaking);
        assert_eq!(d.state(), State::Connecting);
        note_transport_ready(&mut d, Status::Ready);
        assert_eq!(d.state(), State::NeedsWsWrite);
        note_transport_ready(&mut d, Status::Ready);
        assert_eq!(d.state(), State::NeedsWsWrite, "Ready only bumps Connecting");
        note_transport_ready(&mut d, Status::Closed);
        assert_eq!(d.state(), State::Closed);

        // A compressed (RSV1) frame is malformed: the session fails fast.
        let mut t = TestTransport::with_capacity(8192);
        let mut s = steady(MexcClass::Futures);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        t.inject_incoming(&[0xC1, 0x02, b'{', b'}']);
        let e = drive(&mut t, &mut s, &mut prod, &status, &mut NullCapture).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        // A frame larger than rx fails fast rather than livelocking.
        let mut t = TestTransport::with_capacity(RX_BUF_SIZE + 64);
        let mut s = steady(MexcClass::Spot);
        let mut big = vec![0x82, 127];
        big.extend_from_slice(&((RX_BUF_SIZE as u64) * 2).to_be_bytes());
        big.resize(RX_BUF_SIZE + 32, 0);
        t.inject_incoming(&big);
        let e = drive(&mut t, &mut s, &mut prod, &status, &mut NullCapture).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    // ---- run_multi -------------------------------------------------------

    #[test]
    fn run_multi_establishment_budget_reaps_unconfirmed_slots() {
        // WS2: a slot whose subscriptions never confirm is torn down at
        // the budget — the fake connect serves one transport, then
        // refuses, and the loop keeps running until stop.
        let mut d = Driver::new(1, MexcClass::Futures, fut_symbols());
        d.set_establish_budget_ns(50_000_000); // 50 ms
        let mut conns = vec![MexcConn::new(
            d,
            crate::FUT_WS_HOST,
            b"/edge",
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
        assert!(conns[0].transport.is_none(), "unconfirmed slot reaped by the budget");
        assert_eq!(status.take_last_err().site, core_metrics::ERR_SITE_ESTABLISH);
        assert!(status.reconnects_total() >= 1);
    }
}
