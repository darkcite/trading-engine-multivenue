// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-deribit run-loop
//!
//! Deribit JSON-RPC 2.0 over WS/TLS. Same opening-handshake shape as
//! every ingress; steady state is push-driven:
//!
//! ```text
//! Connecting ──► NeedsWsWrite ──► AwaitingWsUpgrade ──► Steady ──┐
//!      ▲                                                          │
//!      └──────────────── Closed / Err ────────────────────────────┘
//! ```
//!
//! On entry to `Steady` the driver queues **two** JSON-RPC calls:
//! `public/set_heartbeat {"interval":15}` (the venue then polices the
//! connection with `test_request`s) and **one** batched
//! `public/subscribe` covering every configured
//! `(channel × instrument)` pair — one subscribe call costs 3000 of a
//! 30 000-credit pool (§4.2), so batching is mandatory. Both are
//! correlated through a `core_net::subs::PendingTable` keyed by the
//! monotonic JSON-RPC id.
//!
//! The subscribe **result** echoes the successfully-subscribed
//! channel list; any expected channel missing ⇒ misconfiguration ⇒
//! session error (fail-fast). Venue `error` responses are equally
//! fatal.
//!
//! ## Heartbeat protocol (the 8c exit criterion)
//!
//! Deribit sends `{"method":"heartbeat","params":{"type":"test_request"}}`
//! and **closes the socket if it is not answered** with `public/test`.
//! The answer is queued in the same drive cycle. `KeepaliveAction::SendPing`
//! doubles as a proactive `public/test` probe when *nothing* has been
//! received for the ping interval; the idle budget (~2× the 15 s
//! heartbeat interval) then forces [`RunResult::IdleTimeout`].
//!
//! ## Integrity per §6.2
//!
//! `book.*` chains `change_id`/`prev_change_id` through
//! [`crate::DeribitBookChain`] (gap ⇒ unsubscribe+subscribe resync of
//! that channel + `gaps_total`); `trades.*` sequential `trade_seq`
//! through [`crate::DeribitTradeSeq`] (gap/regression ⇒ `gaps_total`,
//! no resubscribe — the venue does not replay trades). EVERY row of a
//! `trades` push is seq-checked (2026-08-15 — the earlier 16-row
//! sample produced one phantom gap per burst-coalesced frame): the
//! phase-1 walk checks row-to-row inside the frame, phase 2 checks
//! the frame edge against the monitor's persistent tail.
//!
//! ## §6.6 pairing (G1 remediation)
//!
//! Every `gaps_total` increment emits exactly one paired capture
//! event — [`ChannelId::TradeGap`] / [`ChannelId::BookGap`] carrying
//! expected vs observed seq — plus a rate-limited
//! (1 s) `WARN ingress-deribit: seq gap ...` stderr line. The
//! `audit-replay` pairing section cross-checks the events against the
//! re-derived capture stream, making "every increment paired with a
//! logged venue event" mechanically checkable offline.
//!
//! ## Oversize-frame guard
//!
//! Book snapshots are unbounded; [`RX_BUF_SIZE`] (4 MiB) covers the
//! deepest observed books with headroom. A single frame larger than
//! the rx buffer can never complete — that condition is detected
//! (buffer full + frame incomplete) and fails the session rather than
//! livelocking (fail-fast doctrine).
//!
//! Everything after the handshake is zero-alloc: parsers slice the rx
//! buffer in place; requests render into stack scratch; the only copy
//! is the 64-byte `Tick` moved into the ring.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_metrics::{IngressState, IngressStatus};
use core_net::{
    constant_time_eq, expected_accept, queue_masked_text_frame, read_server_handshake,
    sec_websocket_key_from_seed, write_client_handshake, ws_mask_from_counter, ws_read_frame,
    ws_unmask_in_place, ws_write_pong, HandshakeResult, IoBuf, Keepalive, KeepaliveAction,
    PendingTable, ReqKind, Status, SubErr, SubTable, Transport, WsOpcode, WsReadResult,
};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{Capture, ChannelEvent, ChannelId, OptSummary, Price, Qty, Tick, VenueId};

use crate::{
    classify, extract_instrument, parse_book_header, parse_option_ticker, parse_quote,
    parse_ticker, parse_trade,
    sub_id_of, write_book_op, write_set_heartbeat, write_subscribe_all, write_test, ChainOutcome,
    DeribitChannel, DeribitMsgKind, DeribitSymbolTable, DeribitTradeSeq, TradeSeqOutcome,
    DERIBIT_MAX_SYMBOLS, HEARTBEAT_INTERVAL_SECS,
};

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// Rx buffer: Deribit book **snapshots are unbounded** — a deep
/// BTC-PERPETUAL book renders to ~1–2 MiB of JSON. 4 MiB gives ≥2×
/// headroom; the oversize guard (module doc) fails fast rather than
/// livelocking if the venue ever exceeds it. Boot-time allocation.
pub const RX_BUF_SIZE: usize = 4 * 1024 * 1024;

/// Tx buffer: handshake + set_heartbeat + one batched subscribe
/// (≤ ~48 B/channel × [`MAX_CHANNELS`] = 192 ≈ 9.2 KiB with a full
/// M2.3 options block) + resync pairs + test replies. 24 KiB keeps
/// ≥2× margin (boot-time allocation).
pub const TX_BUF_SIZE: usize = 24 * 1024;

/// Tick-ring capacity. Must equal `engine::TICK_RING_SIZE` — the cli
/// const-asserts the equality when wiring lanes (8a §3.3 pattern).
pub const TICK_RING_CAP: usize = 16_384;

/// In-flight JSON-RPC request cap (power of two, `PendingTable`).
pub const PENDING_CAP: usize = 64;

/// Channels per STATIC instrument (quote, ticker, trades, book).
/// M2.1 option rows subscribe QUOTE only (1 channel).
pub const CHANNELS_PER_INSTR: usize = 4;

/// Bits reserved for the static block in the verification bitmask:
/// [`CHANNELS_PER_INSTR`] × [`crate::DERIBIT_STATIC_MAX`] = 64.
const STATIC_MASK_BITS: usize = CHANNELS_PER_INSTR * crate::DERIBIT_STATIC_MAX;

/// Upper bound on subscribed channels: the static block
/// ([`CHANNELS_PER_INSTR`] × [`crate::DERIBIT_STATIC_MAX`] = 64) +
/// TWO channels per option row (quote + ticker, M2.3;
/// [`crate::DERIBIT_OPT_MAX`] = 64) = 192. NOTE the
/// subscribe-verification MASK stays u128: option rows FOLD their
/// two channels into one per-row bit (64 static-channel bits + 64
/// option-row bits — see `row_bit`/`found_mask`); MAX_CHANNELS
/// bounds the SubTable/frame capacities, not the mask width.
pub const MAX_CHANNELS: usize =
    CHANNELS_PER_INSTR * crate::DERIBIT_STATIC_MAX + 2 * crate::DERIBIT_OPT_MAX;

/// Subscription-table capacity (≥ [`MAX_CHANNELS`]).
pub const SUB_CAP: usize = MAX_CHANNELS;

/// Stack scratch for one rendered subscribe batch (≤ ~48 B/channel ×
/// 192 channels ≈ 9.2 KiB; ~1.7× margin).
const SUBSCRIBE_SCRATCH: usize = 16 * 1024;

/// Stack scratch for one rendered channel name
/// (`"` + prefix ≤ 7 + instrument ≤ 32 + `.100ms` + `"` = 47 max).
const CHANNEL_NAME_SCRATCH: usize = 64;

/// Minimum interval between emitted gap log lines (operator-terminal
/// budget; increments beyond it are counted into `suppressed=` on the
/// next emitted line — the 1:1 evidence channel is the paired
/// `TradeGap`/`BookGap` capture events, not the log).
const GAP_LOG_INTERVAL_NS: u64 = 1_000_000_000;

// ---------------------------------------------------------------
// Request / subscription kinds for the core-net tables
// ---------------------------------------------------------------

/// In-flight JSON-RPC request shapes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeribitReqKind {
    /// `public/set_heartbeat`.
    SetHeartbeat = 0,
    /// The batched session `public/subscribe`.
    SubscribeAll = 1,
    /// `public/test` (test_request answer or proactive probe).
    Test = 2,
    /// Book-resync `public/unsubscribe`.
    BookUnsub = 3,
    /// Book-resync `public/subscribe`.
    BookSub = 4,
    /// Slot free.
    None = 255,
}

impl ReqKind for DeribitReqKind {
    const FREE: Self = DeribitReqKind::None;
}

/// Subscription-table tag (mirrors [`DeribitChannel`] + free sentinel).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeribitSubKind {
    /// `quote.*`.
    Quote = 0,
    /// `ticker.*`.
    Ticker = 1,
    /// `trades.*`.
    Trades = 2,
    /// `book.*`.
    Book = 3,
    /// Slot free.
    None = 255,
}

impl ReqKind for DeribitSubKind {
    const FREE: Self = DeribitSubKind::None;
}

impl DeribitSubKind {
    #[inline]
    const fn from_channel(c: DeribitChannel) -> Self {
        match c {
            DeribitChannel::Quote => DeribitSubKind::Quote,
            DeribitChannel::Ticker => DeribitSubKind::Ticker,
            DeribitChannel::Trades => DeribitSubKind::Trades,
            DeribitChannel::Book => DeribitSubKind::Book,
        }
    }
}

/// Channel order inside the verification bitmask: bit =
/// `sym_idx * CHANNELS_PER_INSTR + channel`.
const CHANNEL_ORDER: [DeribitChannel; CHANNELS_PER_INSTR] = [
    DeribitChannel::Quote,
    DeribitChannel::Ticker,
    DeribitChannel::Trades,
    DeribitChannel::Book,
];

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
    /// Upgraded — heartbeat armed, subscribed, pushes flowing.
    Steady,
    /// Peer closed.
    Closed,
}

/// How a run-loop invocation terminated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// External stop flag observed.
    Stopped,
    /// Peer closed the connection.
    Disconnected,
    /// No inbound bytes within the keepalive idle budget — caller
    /// reconnects.
    IdleTimeout,
    /// Fatal transport / protocol error (includes venue `error`
    /// responses and missing subscribe confirmations — fail-fast).
    Error,
}

// ---------------------------------------------------------------
// Driver
// ---------------------------------------------------------------

/// Mutable per-connection state owned by the run-loop. Preallocated
/// at construction; never reallocates in steady state.
///
/// **Single-writer invariant.** One thread drives one `Driver`; the
/// `PhantomData<UnsafeCell<()>>` marker makes it `!Sync` so sharing
/// is a compile error. `Send` stays allowed — the cli moves it onto
/// its dedicated ingress thread at boot.
pub struct Driver {
    state: State,
    rx: IoBuf,
    tx: IoBuf,
    sec_key: [u8; 24],
    expected_accept_val: [u8; 28],
    last_activity_ns: u64,
    mask_counter: u64,

    /// Boot-built `instrument → SymbolId` map (venue-namespaced ids).
    symbols: DeribitSymbolTable,
    /// Whether `book.*.100ms` is subscribed (`--deribit-depth`).
    depth_enabled: bool,
    /// Acknowledged subscriptions (filled from the subscribe result).
    subs: SubTable<DeribitSubKind, SUB_CAP>,
    /// In-flight JSON-RPC requests, indexed by `id & (PENDING_CAP-1)`.
    pending: PendingTable<DeribitReqKind, PENDING_CAP>,
    /// Next JSON-RPC id (monotonic from 1; 0 is reserved).
    next_req_id: u64,
    /// Id of the batched session subscribe (0 = not yet sent) — lets
    /// phase-1 dispatch verify the result without a table peek.
    subscribe_req_id: u64,
    /// Book `change_id` chains, indexed by symbol-table row.
    book_chains: [crate::DeribitBookChain; DERIBIT_MAX_SYMBOLS],
    /// Trade `trade_seq` monitors, same indexing.
    trade_seqs: [DeribitTradeSeq; DERIBIT_MAX_SYMBOLS],
    /// Set once the session-start calls have been queued.
    session_started: bool,
    /// Wall clock of the last emitted gap log line (rate limit:
    /// [`GAP_LOG_INTERVAL_NS`]). Deliberately NOT reset per session —
    /// the limit is an operator-terminal budget, not session state.
    gap_log_last_ns: u64,
    /// Gap increments swallowed by the rate limit since the last
    /// emitted line (carried into that next line as `suppressed=`).
    gap_log_suppressed: u32,
    /// `!Sync` marker — see struct doc.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate buffers (boot-time) and seed the handshake nonce.
    /// `symbols` maps every configured instrument; `depth_enabled`
    /// adds the `book.*.100ms` channel per instrument.
    pub fn new(nonce_seed: u64, symbols: DeribitSymbolTable, depth_enabled: bool) -> Self {
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
            depth_enabled,
            subs: SubTable::new(),
            pending: PendingTable::new(),
            next_req_id: 1,
            subscribe_req_id: 0,
            book_chains: [crate::DeribitBookChain::new(); DERIBIT_MAX_SYMBOLS],
            trade_seqs: [DeribitTradeSeq::new(); DERIBIT_MAX_SYMBOLS],
            session_started: false,
            gap_log_last_ns: 0,
            gap_log_suppressed: 0,
            _not_sync: ::core::marker::PhantomData,
        }
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

    /// Acknowledged-subscription count.
    #[inline]
    pub fn sub_count(&self) -> usize {
        self.subs.count()
    }

    /// Live in-flight JSON-RPC request count (metrics/tests).
    #[inline]
    pub fn pending_count(&self) -> usize {
        self.pending.count()
    }

    /// Allocate the next JSON-RPC id (monotonic; never 0).
    #[inline]
    fn alloc_req_id(&mut self) -> u64 {
        let id = self.next_req_id;
        self.next_req_id = self.next_req_id.wrapping_add(1);
        // Wrapping past u64::MAX in one session is unreachable; the
        // reserved 0 would need 2^64 requests.
        debug_assert!(id != 0, "JSON-RPC id 0 is reserved");
        id
    }

    /// Reset per-connection state for a reconnect. Subscriptions,
    /// pending requests and integrity chains are connection-scoped:
    /// tables clear, chains re-arm for fresh snapshots, ids restart.
    pub fn reset_for_reconnect(&mut self, nonce_seed: u64) {
        self.state = State::Connecting;
        self.rx.clear();
        self.tx.clear();
        self.sec_key = sec_websocket_key_from_seed(nonce_seed);
        self.expected_accept_val = expected_accept(&self.sec_key);
        self.last_activity_ns = 0;
        self.mask_counter = 0;
        self.subs.clear();
        self.pending.clear();
        self.next_req_id = 1;
        self.subscribe_req_id = 0;
        let mut i = 0;
        while i < DERIBIT_MAX_SYMBOLS {
            self.book_chains[i].reset_await_snapshot();
            self.trade_seqs[i] = DeribitTradeSeq::new();
            i += 1;
        }
        self.session_started = false;
    }
}

// ---------------------------------------------------------------
// drive_one — single-tick state machine advance
// ---------------------------------------------------------------

/// Pump the transport once and advance the state machine. Zero-alloc
/// once the handshake has completed.
///
/// * `producer`: this venue's Tick lane. A full ring drops the tick
///   and bumps `IngressStatus::ring_drops`.
/// * `status`: per-ingress observability slot; this thread is its
///   single writer.
pub fn drive_one<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, TICK_RING_CAP>,
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
                queue_session_start(drv)?;
            }
        }
        State::Steady => {
            drain_ws_frames(drv, producer, status, capture)?;
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
            // D5: the 101 response is inbound activity.
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

// ---------------------------------------------------------------
// Session start: set_heartbeat + one batched subscribe
// ---------------------------------------------------------------

/// Record a freshly-queued JSON-RPC request. Slot collision means
/// more than [`PENDING_CAP`] unanswered requests — a protocol bug or
/// a dead venue; fail-fast either way.
fn record_pending(drv: &mut Driver, id: u64, kind: DeribitReqKind) -> io::Result<()> {
    match drv.pending.record(id, kind, now_ns()) {
        Ok(()) => Ok(()),
        Err(_e) => {
            debug_assert!(false, "deribit pending table rejected id {id}: {_e:?}");
            Err(io::Error::other("deribit pending-request slot collision"))
        }
    }
}

/// Queue `public/set_heartbeat` **then** the single batched
/// `public/subscribe`. Called exactly once per connection, on the
/// upgrade→Steady edge.
fn queue_session_start(drv: &mut Driver) -> io::Result<()> {
    debug_assert!(
        !drv.session_started,
        "session start must be queued exactly once"
    );
    if drv.symbols.is_empty() {
        return Err(io::Error::other("deribit: no instruments configured"));
    }
    // 1. Heartbeat first — the venue polices the connection from the
    //    moment this is acked (test_request every 15 s).
    let hb_id = drv.alloc_req_id();
    let mut scratch = [0u8; 128];
    let n = write_set_heartbeat(&mut scratch, hb_id, HEARTBEAT_INTERVAL_SECS)
        .ok_or_else(|| io::Error::other("deribit: set_heartbeat scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..n])?;
    record_pending(drv, hb_id, DeribitReqKind::SetHeartbeat)?;

    // 2. One batched subscribe for every (channel × instrument).
    let sub_id = drv.alloc_req_id();
    let mut scratch = [0u8; SUBSCRIBE_SCRATCH];
    let n = write_subscribe_all(&mut scratch, sub_id, &drv.symbols, drv.depth_enabled)
        .ok_or_else(|| io::Error::other("deribit: subscribe scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..n])?;
    record_pending(drv, sub_id, DeribitReqKind::SubscribeAll)?;
    drv.subscribe_req_id = sub_id;
    drv.session_started = true;
    Ok(())
}

/// Queue one `public/test` (test_request answer / proactive probe).
fn queue_test(drv: &mut Driver) -> io::Result<()> {
    let id = drv.alloc_req_id();
    let mut scratch = [0u8; 96];
    let n = write_test(&mut scratch, id)
        .ok_or_else(|| io::Error::other("deribit: test scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..n])?;
    record_pending(drv, id, DeribitReqKind::Test)
}

/// Queue an unsubscribe+subscribe pair for one `book.{instr}.100ms` —
/// the §6.2 resync action after a chain break (fresh snapshot).
fn queue_book_resync(drv: &mut Driver, sym_idx: usize) -> io::Result<()> {
    // Ids allocated before the symbol-row borrow (alloc needs &mut).
    let unsub_id = drv.alloc_req_id();
    let sub_id = drv.alloc_req_id();
    let Some((instr_ref, _sym)) = drv.symbols.get(sym_idx) else {
        debug_assert!(false, "resync for unknown symbol row {sym_idx}");
        return Ok(());
    };
    // Copy the instrument out of the table row (≤32 B stack) so the
    // immutable borrow ends before the tx queueing below.
    let mut instr_buf = [0u8; crate::DERIBIT_INSTR_MAX];
    let instr_len = instr_ref.len();
    instr_buf[..instr_len].copy_from_slice(instr_ref);
    let instr = &instr_buf[..instr_len];

    let mut scratch = [0u8; 256];
    let n = write_book_op(&mut scratch, unsub_id, b"public/unsubscribe", instr)
        .ok_or_else(|| io::Error::other("deribit: resync scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..n])?;
    record_pending(drv, unsub_id, DeribitReqKind::BookUnsub)?;
    let n = write_book_op(&mut scratch, sub_id, b"public/subscribe", instr)
        .ok_or_else(|| io::Error::other("deribit: resync scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..n])?;
    record_pending(drv, sub_id, DeribitReqKind::BookSub)?;
    Ok(())
}

// ---------------------------------------------------------------
// Subscribe-result verification
// ---------------------------------------------------------------

/// Bit for `(static sym_idx, channel)` in the verification masks.
/// Static rows occupy bits `0..64`; option rows (M2.1, quote-only)
/// occupy bits `64..128` via [`option_bit`].
#[inline]
const fn channel_bit(sym_idx: usize, ch: usize) -> u128 {
    1u128 << (sym_idx * CHANNELS_PER_INSTR + ch)
}

/// Bit for option row `sym_idx` (table index; the row's single quote
/// channel).
#[inline]
const fn option_bit(sym_idx: usize, static_len: usize) -> u128 {
    1u128 << (STATIC_MASK_BITS + (sym_idx - static_len))
}

/// Channels a table row subscribes: options quote + ticker (M2.3 —
/// the mark/IV stream), static rows the full set (minus book without
/// depth).
#[inline]
fn row_channels(symbols: &DeribitSymbolTable, idx: usize, depth_enabled: bool) -> usize {
    if symbols.is_option_row(idx) {
        2
    } else if depth_enabled {
        CHANNELS_PER_INSTR
    } else {
        CHANNELS_PER_INSTR - 1
    }
}

/// Verification-mask bit for `(row, channel)` under the M2 partition
/// law. Option rows FOLD their channels (quote + ticker) into ONE
/// per-row bit — [`found_mask`] sets it only when EVERY option
/// channel was acknowledged, so the u128 stays exactly
/// 64 static-channel bits + 64 option-row bits.
#[inline]
fn row_bit(symbols: &DeribitSymbolTable, idx: usize, ch: usize) -> u128 {
    if symbols.is_option_row(idx) {
        debug_assert!(ch < 2, "option rows have quote + ticker only");
        option_bit(idx, symbols.static_len())
    } else {
        channel_bit(idx, ch)
    }
}

/// Expected-channel mask for the configured table (+depth flag).
fn expected_mask(symbols: &DeribitSymbolTable, depth_enabled: bool) -> u128 {
    let mut m = 0u128;
    let mut i = 0;
    while i < symbols.len() {
        let n_ch = row_channels(symbols, i, depth_enabled);
        let mut c = 0;
        while c < n_ch {
            m |= row_bit(symbols, i, c);
            c += 1;
        }
        i += 1;
    }
    m
}

/// Scan a subscribe **result** payload for every expected channel
/// name (rendered with surrounding quotes so `quote.BTC-PERP` can
/// never alias `quote.BTC-PERPETUAL`). Returns the found-bit mask.
fn found_mask(payload: &[u8], symbols: &DeribitSymbolTable, depth_enabled: bool) -> u128 {
    let mut m = 0u128;
    let mut i = 0;
    while let Some((instr, _sym)) = symbols.get(i) {
        let n_ch = row_channels(symbols, i, depth_enabled);
        // Option rows fold: the per-row bit requires EVERY channel.
        let mut found_in_row = 0usize;
        let mut c = 0;
        while c < n_ch {
            let ch = CHANNEL_ORDER[c];
            let mut name = [0u8; CHANNEL_NAME_SCRATCH];
            let mut n = 0usize;
            name[n] = b'"';
            n += 1;
            let p = ch.wire_prefix();
            name[n..n + p.len()].copy_from_slice(p);
            n += p.len();
            name[n..n + instr.len()].copy_from_slice(instr);
            n += instr.len();
            let s = ch.wire_suffix();
            name[n..n + s.len()].copy_from_slice(s);
            n += s.len();
            name[n] = b'"';
            n += 1;
            if memchr::memmem::find(payload, &name[..n]).is_some() {
                if symbols.is_option_row(i) {
                    found_in_row += 1;
                } else {
                    m |= row_bit(symbols, i, c);
                }
            }
            c += 1;
        }
        if symbols.is_option_row(i) && found_in_row == n_ch {
            m |= option_bit(i, symbols.static_len());
        }
        i += 1;
    }
    m
}

// ---------------------------------------------------------------
// Frame drain + dispatch
// ---------------------------------------------------------------

/// Per-push `trades` scan result (phase-1 output; `Copy`).
///
/// 2026-08-15 (G1 remediation): replaced the fixed `seqs: [i64; 16]`
/// sample — EVERY row is now seq-checked. Within-frame discontinuities
/// are classified during the phase-1 walk (their paired `TradeGap`
/// events are emitted right there, where expected/observed are at
/// hand); the frame edge is checked in phase 2 via
/// [`DeribitTradeSeq::apply_frame`] against the persistent monitor.
#[derive(Copy, Clone)]
struct TradeScan {
    rows_parsed: u32,
    rows_rejected: u32,
    /// Rows that carried a parsed `trade_seq` (== `rows_parsed`; kept
    /// separate so the phase-2 arm reads as intent, not inference).
    n_seq: u32,
    /// First parsed row's seq (valid when `n_seq > 0`).
    first_seq: i64,
    /// Last parsed row's seq — the frame's true tail, adopted by the
    /// monitor regardless of interior breaks.
    last_seq: i64,
    /// First parsed row's venue timestamp (ms) — `venue_time_ms` of a
    /// frame-edge `TradeGap` event.
    first_ts_ms: u64,
    /// Within-frame breaks (jump or regression between adjacent rows).
    /// Their `TradeGap` events were already emitted by the walk; phase
    /// 2 adds exactly this many `gaps_total` increments.
    intra_breaks: u32,
    /// expected/observed of the LAST within-frame break (log-line
    /// substance; every break's full detail is in its capture event).
    intra_last_expected: i64,
    intra_last_observed: i64,
}

/// Phase-1 dispatch outcome — everything pre-parsed while the rx
/// borrow is live, applied after it ends (template pattern).
#[derive(Copy, Clone)]
enum Dispatch {
    /// Unparseable / unclassifiable — one rejection.
    Nothing,
    /// Venue liveness heartbeat — activity only.
    Quiet,
    /// `test_request` — must answer `public/test` (phase 2 queues it).
    TestRequest,
    /// Response to one of our requests (not the session subscribe).
    RpcOk { id: u64 },
    /// The session subscribe result, pre-verified against the
    /// configured channel set.
    SubscribeResult { id: u64, found: u128, expected: u128 },
    /// Venue `error` response — fatal (fail-fast).
    VenueError { code: i32 },
    /// `quote` push became a Tick.
    Quote { tick: Tick },
    /// `ticker` push validated (slow lane; captured as a §6.5 event).
    Ticker,
    /// M2.3: an OPTION row's ticker parsed → `OptSummary` captured in
    /// phase 1 (capture-only; nothing reaches the engine ring).
    OptSummary,
    /// `trades` push scanned (`sym` rides along for phase-2 gap
    /// events — resolving it again would re-borrow `drv.symbols`).
    Trades { sym: u32, sym_idx: u8, scan: TradeScan },
    /// `book` push header (`ts_ms` rides along for the phase-2
    /// `BookGap` event's `venue_time_ms`).
    Book { sym: u32, sym_idx: u8, ts_ms: u64, action: u8, prev: i64, seq: i64 },
}

fn drain_ws_frames<C: Capture>(
    drv: &mut Driver,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    loop {
        let read_result = ws_read_frame(drv.rx.filled());
        match read_result {
            WsReadResult::Incomplete => {
                // Oversize guard: a frame that cannot fit the rx
                // buffer never completes — fail the session instead
                // of livelocking (module doc).
                if drv.rx.free_mut().is_empty() && drv.rx.len() > 0 {
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
                        handle_data_frame(drv, payload.start..payload.end, producer, status, capture)?;
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
                        // Deribit does not fragment public pushes;
                        // drop rather than allocate a reassembly
                        // buffer.
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

/// Walk the `trades` rows of one push. Rows are sliced at successive
/// `"trade_seq":` markers; each slice parses independently (every
/// row field follows its own `trade_seq` within the row). Each parsed
/// row is captured as a `ChannelId::Trade` event (§6.5): `venue_seq` =
/// `trade_seq`, `venue_time_ms` from the venue ts, `v0` = px ×1e6,
/// `v1` = amount ×1e6 negated for sell-side prints (Deribit amounts
/// are USD notionals for perps/futures).
fn scan_trades<C: Capture>(payload: &[u8], sym: u32, capture: &mut C) -> TradeScan {
    let mut scan = TradeScan {
        rows_parsed: 0,
        rows_rejected: 0,
        n_seq: 0,
        first_seq: 0,
        last_seq: 0,
        first_ts_ms: 0,
        intra_breaks: 0,
        intra_last_expected: 0,
        intra_last_observed: 0,
    };
    // Rows are the OBJECTS of the `"data":[...]` array, sliced by JSON
    // object extent. The 8c implementation sliced at `"trade_seq":`
    // markers instead and broke live on 2026-08-15: Deribit's starbase
    // rollout moved `trade_seq` mid-row (timestamp/price/direction now
    // precede it), so a marker-to-marker slice straddled two rows —
    // single-trade frames lost their head fields entirely (~1.3 %
    // reject rate) and every reject holed the trade-seq monitor
    // (the phantom "gaps"). Object-extent slicing + key-matched
    // per-row parsing is order-tolerant against both wire layouts.
    let mut i = match core_parse::find_field(payload, b"\"data\":") {
        Some(p) => p,
        None => {
            scan.rows_rejected += 1;
            capture.parse_reject(now_ns(), payload);
            return scan;
        }
    };
    i = core_parse::skip_ws(payload, i);
    if i >= payload.len() || payload[i] != b'[' {
        scan.rows_rejected += 1;
        capture.parse_reject(now_ns(), payload);
        return scan;
    }
    i += 1;
    loop {
        i = core_parse::skip_ws(payload, i);
        if i >= payload.len() || payload[i] == b']' {
            break;
        }
        if payload[i] == b',' {
            i += 1;
            continue;
        }
        let row_start = i;
        let row_end = match core_parse::skip_json_value(payload, i) {
            Some(e) if payload[row_start] == b'{' => e,
            _ => {
                // Structurally broken remainder — reject it once.
                scan.rows_rejected += 1;
                capture.parse_reject(now_ns(), &payload[row_start..]);
                break;
            }
        };
        i = row_end;
        match parse_trade(&payload[row_start..row_end], sym) {
            Some(t) => {
                scan.rows_parsed += 1;
                if scan.n_seq == 0 {
                    scan.first_seq = t.trade_seq;
                    scan.first_ts_ms = t.ts_ns / 1_000_000;
                } else if t.trade_seq != scan.last_seq.wrapping_add(1) {
                    // Within-frame discontinuity. §6.6 pairing: the
                    // increment this row will cause in phase 2 gets
                    // its ChannelEvent HERE, where expected/observed
                    // are both at hand (v0 = expected, v1 = observed).
                    scan.intra_breaks += 1;
                    scan.intra_last_expected = scan.last_seq.wrapping_add(1);
                    scan.intra_last_observed = t.trade_seq;
                    capture.event(&ChannelEvent::new(
                        now_ns(),
                        VenueId::Deribit,
                        ChannelId::TradeGap,
                        sym,
                        t.trade_seq as u64,
                        t.ts_ns / 1_000_000,
                        scan.last_seq.wrapping_add(1),
                        t.trade_seq,
                    ));
                }
                scan.last_seq = t.trade_seq;
                scan.n_seq += 1;
                // §6.5 capture: v0 = px ×1e6, v1 = amount ×1e6 (USD
                // notional), negated when `direction` is sell.
                let signed_qty = if t.side == 1 { -t.qty_1e6 } else { t.qty_1e6 };
                capture.event(&ChannelEvent::new(
                    now_ns(),
                    VenueId::Deribit,
                    ChannelId::Trade,
                    sym,
                    t.trade_seq as u64,
                    t.ts_ns / 1_000_000,
                    t.px_1e6,
                    signed_qty,
                ));
            }
            None => {
                scan.rows_rejected += 1;
                // Tap the exact rejected row slice — the §6.5 raw-tap
                // differential audit consumes these.
                capture.parse_reject(now_ns(), &payload[row_start..row_end]);
            }
        }
    }
    scan
}

fn handle_data_frame<C: Capture>(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    // Retained for the phase-2 reject re-borrow (Range is not Copy).
    let reject_range = payload_range.clone();
    // Phase 1: immutable borrow of rx (+ symbols) — classify, resolve
    // the instrument, pre-parse into a Copy dispatch value. Capture
    // hooks that need parsed values fire here (events); the raw tap
    // sees every data payload pre-classify (heartbeats and acks
    // included — this venue is the §6.5 raw-tap target).
    let dispatch: Dispatch = {
        let payload = &drv.rx.filled()[payload_range];
        capture.raw_frame(now_ns(), payload);
        match classify(payload) {
            DeribitMsgKind::Heartbeat => Dispatch::Quiet,
            DeribitMsgKind::TestRequest => Dispatch::TestRequest,
            DeribitMsgKind::RpcError { id: _, code } => Dispatch::VenueError { code },
            DeribitMsgKind::RpcResult(id) => {
                if id == drv.subscribe_req_id {
                    Dispatch::SubscribeResult {
                        id,
                        found: found_mask(payload, &drv.symbols, drv.depth_enabled),
                        expected: expected_mask(&drv.symbols, drv.depth_enabled),
                    }
                } else {
                    Dispatch::RpcOk { id }
                }
            }
            DeribitMsgKind::Notification(channel) => {
                match extract_instrument(payload, channel).and_then(|instr| {
                    drv.symbols
                        .lookup(instr)
                        .map(|sym| (sym, drv.symbols.index_of(sym)))
                }) {
                    Some((sym, Some(sym_idx))) => match channel {
                        // §6.5: no ChannelEvent for quotes — BBO flows
                        // as `Tick` into the per-venue tick log (see
                        // the `ChannelId` doc in core-types).
                        DeribitChannel::Quote => match parse_quote(payload, sym) {
                            Some(f) => Dispatch::Quote {
                                tick: Tick::new(
                                    now_ns(),
                                    VenueId::Deribit,
                                    sym,
                                    // No seq on quotes: venue ms
                                    // timestamp, truncated (crate doc).
                                    f.ts_ms as u32,
                                    Price::from_raw(f.bid_px_1e6),
                                    Qty::from_raw(f.bid_qty_1e6),
                                    Price::from_raw(f.ask_px_1e6),
                                    Qty::from_raw(f.ask_qty_1e6),
                                ),
                            },
                            None => Dispatch::Nothing,
                        },
                        // M2.3: OPTION rows' ticker carries the
                        // mark/IV/greeks/OI surface → OptSummary
                        // capture (never the engine ring). The
                        // futures parser would reject it (no
                        // current_funding on option tickers).
                        DeribitChannel::Ticker if drv.symbols.is_option_row(sym_idx) => {
                            match parse_option_ticker(payload) {
                                Some(f) => {
                                    capture.opt_summary(&OptSummary::new(
                                        now_ns(),
                                        VenueId::Deribit,
                                        sym,
                                        core_types::OPT_SUMMARY_FLAG_MARK_PX
                                            | core_types::OPT_SUMMARY_FLAG_OI,
                                        f.mark_px_1e9,
                                        f.mark_iv_1e9,
                                        f.underlying_px_1e9,
                                        f.open_interest_1e6,
                                        f.delta_1e9,
                                        f.gamma_1e9,
                                        f.vega_1e6,
                                        f.theta_1e6,
                                    ));
                                    Dispatch::OptSummary
                                }
                                None => Dispatch::Nothing,
                            }
                        }
                        DeribitChannel::Ticker => match parse_ticker(payload, sym) {
                            Some(tk) => {
                                // §6.5 capture: v0 = mark px ×1e6
                                // (`mark_px_1e6`), v1 = open interest
                                // ×1e6 as stored in the parsed POD
                                // (`open_interest_1e6` — USD notional
                                // for perps/futures). Tickers carry
                                // no venue seq.
                                capture.event(&ChannelEvent::new(
                                    now_ns(),
                                    VenueId::Deribit,
                                    ChannelId::Ticker,
                                    sym,
                                    0,
                                    tk.ts_ns / 1_000_000,
                                    tk.mark_px_1e6,
                                    tk.open_interest_1e6,
                                ));
                                Dispatch::Ticker
                            }
                            None => Dispatch::Nothing,
                        },
                        DeribitChannel::Trades => Dispatch::Trades {
                            sym,
                            sym_idx: sym_idx as u8,
                            scan: scan_trades(payload, sym, capture),
                        },
                        DeribitChannel::Book => match parse_book_header(payload, sym) {
                            Some(b) => {
                                // §6.5 capture: venue_seq = change_id,
                                // v0 = prev_change_id (−1 on snapshots
                                // — the crate's convention for the
                                // wire-absent field), v1 = levels
                                // counted (bids + asks, including the
                                // beyond-DEPTH_CAP excess counts) —
                                // the offline audit re-derives chain
                                // breaks from these.
                                capture.event(&ChannelEvent::new(
                                    now_ns(),
                                    VenueId::Deribit,
                                    ChannelId::Book,
                                    sym,
                                    b.change_id as u64,
                                    b.ts_ns / 1_000_000,
                                    b.prev_change_id,
                                    (b.n_bids as i64)
                                        + (b.n_asks as i64)
                                        + (b.excess_bids as i64)
                                        + (b.excess_asks as i64),
                                ));
                                Dispatch::Book {
                                    sym,
                                    sym_idx: sym_idx as u8,
                                    ts_ms: b.ts_ns / 1_000_000,
                                    action: b.action,
                                    prev: b.prev_change_id,
                                    seq: b.change_id,
                                }
                            }
                            None => Dispatch::Nothing,
                        },
                    },
                    // Data for an instrument we never configured —
                    // a mapping bug or venue noise; count it.
                    _ => Dispatch::Nothing,
                }
            }
            DeribitMsgKind::Unknown => Dispatch::Nothing,
        }
    };

    // Phase 2: mutable applies. The rx borrow above has ended.
    match dispatch {
        Dispatch::Nothing => {
            status.inc_parse_errors();
            // Tap the rejected payload (re-borrow is safe: dispatch is
            // Copy and the rx buffer is untouched since phase 1).
            capture.parse_reject(now_ns(), &drv.rx.filled()[reject_range]);
        }
        Dispatch::Quiet => status.add_msgs(1),
        Dispatch::TestRequest => {
            // The venue closes the socket on an unanswered
            // test_request — the reply is queued in this same drive
            // cycle (flushed by drive_one's trailing flush).
            status.add_msgs(1);
            queue_test(drv)?;
        }
        Dispatch::RpcOk { id } => {
            match drv.pending.complete(id) {
                Some(_req) => status.add_msgs(1),
                // Late/duplicate/foreign response — count, don't
                // crash: venues do redeliver. Tapped like every other
                // parse_errors site (§6.5); same safe re-borrow as
                // Dispatch::Nothing.
                None => {
                    status.inc_parse_errors();
                    capture.parse_reject(now_ns(), &drv.rx.filled()[reject_range]);
                }
            }
        }
        Dispatch::SubscribeResult { id, found, expected } => {
            let _ = drv.pending.complete(id);
            status.add_msgs(1);
            if found & expected != expected {
                // A configured channel was refused — misconfiguration.
                // Fail-fast: crash loudly in debug, session error in
                // release (reconnect w/ backoff, operator sees it).
                // T1(a) (outage 2026-08-27 §5.2): this is the exact
                // post-settlement kill site — expired option channels
                // vanish from the echo. `venue_code` carries the
                // COUNT of missing channels (the u128 masks don't fit
                // a u32; the count is the operator-facing signal).
                status.note_venue_err_code((expected & !found).count_ones());
                status.note_session_err(
                    core_metrics::ERR_SITE_SUBSCRIBE_MISSING,
                    core_metrics::io_kind_code(io::ErrorKind::InvalidData),
                );
                debug_assert!(
                    false,
                    "deribit subscribe result missing channels: expected {expected:#x} found {found:#x}"
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "deribit subscribe result missing configured channels",
                ));
            }
            register_confirmed_subs(drv);
        }
        Dispatch::VenueError { code } => {
            // Fail-fast doctrine: a venue error response means our
            // request (or framing) is wrong. Crash loudly in debug,
            // surface a session error in release.
            // T1(a) (outage 2026-08-27 §5.2): record the venue's
            // numeric code (i32 bit-cast; negative JSON-RPC codes
            // round-trip through the u32 slot) — first-wins so the
            // outer drive-site conversion keeps this inner site.
            status.note_venue_err_code(code as u32);
            status.note_session_err(
                core_metrics::ERR_SITE_VENUE_ERROR,
                core_metrics::io_kind_code(io::ErrorKind::InvalidData),
            );
            debug_assert!(false, "deribit venue error response, code={code}");
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "deribit venue error response",
            ));
        }
        Dispatch::Quote { tick } => {
            status.add_msgs(1);
            status.add_ticks(1);
            // §6.5 capture BEFORE the push — a ring-dropped tick must
            // still reach the replay log (the audit pairs capture
            // counts with ring_drops_total).
            capture.tick(&tick);
            // D4: a full ring is data loss — count it, never block.
            if producer.try_push(tick).is_err() {
                status.inc_ring_drops();
            }
        }
        Dispatch::Ticker => {
            status.add_msgs(1);
            status.add_ticks(1);
        }
        Dispatch::OptSummary => {
            status.add_msgs(1);
            status.add_ticks(1);
        }
        Dispatch::Trades { sym, sym_idx, scan } => {
            status.add_msgs(scan.rows_parsed as u64);
            status.add_ticks(scan.rows_parsed as u64);
            let mut r = 0;
            while r < scan.rows_rejected {
                status.inc_parse_errors();
                r += 1;
            }
            // Within-frame breaks: their paired TradeGap events were
            // emitted by the phase-1 walk; count them here where the
            // counter lives (1:1 with the events, §6.6 letter).
            let mut k = 0;
            while k < scan.intra_breaks {
                status.inc_gaps();
                k += 1;
            }
            if scan.intra_breaks > 0 {
                log_gap_rate_limited(
                    drv,
                    b"trades",
                    sym,
                    scan.intra_last_expected,
                    scan.intra_last_observed,
                    scan.intra_breaks,
                );
            }
            // Frame edge vs the persistent monitor. Checks the first
            // row against the previous frame's TRUE tail — every row
            // of every frame is seq-checked since 2026-08-15 (the
            // 16-row sample cap produced one phantom gap per
            // burst-coalesced frame; 67 across the first 6 h soak).
            if scan.n_seq > 0 {
                let expected = drv.trade_seqs[sym_idx as usize].next_expected();
                match drv.trade_seqs[sym_idx as usize].apply_frame(scan.first_seq, scan.last_seq) {
                    TradeSeqOutcome::Ok => {}
                    // Missed or replayed trades — counted; no
                    // resubscribe (Deribit does not replay trades).
                    TradeSeqOutcome::Gap | TradeSeqOutcome::Regression => {
                        status.inc_gaps();
                        capture.event(&ChannelEvent::new(
                            now_ns(),
                            VenueId::Deribit,
                            ChannelId::TradeGap,
                            sym,
                            scan.first_seq as u64,
                            scan.first_ts_ms,
                            expected,
                            scan.first_seq,
                        ));
                        log_gap_rate_limited(drv, b"trades", sym, expected, scan.first_seq, 1);
                    }
                }
            }
        }
        Dispatch::Book { sym, sym_idx, ts_ms, action, prev, seq } => {
            status.add_msgs(1);
            status.add_ticks(1);
            let expected_prev = drv.book_chains[sym_idx as usize].last_change_id();
            match drv.book_chains[sym_idx as usize].apply(action, prev, seq) {
                ChainOutcome::Init | ChainOutcome::Chained => {}
                ChainOutcome::Gap => {
                    status.inc_gaps();
                    // §6.6 pairing: v0 = expected prev (i64::MIN =
                    // the monitor was still awaiting a snapshot),
                    // v1 = observed prev.
                    capture.event(&ChannelEvent::new(
                        now_ns(),
                        VenueId::Deribit,
                        ChannelId::BookGap,
                        sym,
                        seq as u64,
                        ts_ms,
                        expected_prev,
                        prev,
                    ));
                    log_gap_rate_limited(drv, b"book", sym, expected_prev, prev, 1);
                    status.inc_resubscribes();
                    queue_book_resync(drv, sym_idx as usize)?;
                }
            }
        }
    }
    Ok(())
}

/// Rate-limited (1 line / [`GAP_LOG_INTERVAL_NS`]) gap log line —
/// operator visibility for every `gaps_total` increment class; the
/// 1:1 evidence record is the paired capture event, not this line.
///
/// Zero-alloc: rendered into stack scratch with the crate's fixed
/// formatters and written to stderr in one `write_all` (single
/// syscall; interleaves with the cli's tracing lines at line
/// granularity). Increments inside the rate-limit window are counted
/// and surface as `suppressed=` on the next emitted line.
/// Timestamp is raw unix nanos (`ts_ns=`) — the offline pairing
/// section and the capture events carry the same clock.
fn log_gap_rate_limited(
    drv: &mut Driver,
    channel: &[u8],
    sym: u32,
    expected: i64,
    observed: i64,
    increments: u32,
) {
    let now = now_ns();
    if now.wrapping_sub(drv.gap_log_last_ns) < GAP_LOG_INTERVAL_NS {
        drv.gap_log_suppressed = drv.gap_log_suppressed.saturating_add(increments);
        return;
    }
    let suppressed = drv.gap_log_suppressed;
    drv.gap_log_last_ns = now;
    drv.gap_log_suppressed = 0;

    // `WARN ingress-deribit: seq gap channel=<c> sym=<hex> expected=<i64>
    //  observed=<i64> increments=<n> suppressed=<n> ts_ns=<n>\n`
    let mut buf = [0u8; 224];
    let mut digits = [0u8; 20];
    let mut n = 0usize;
    let mut ok = true;
    let put = |buf: &mut [u8; 224], n: &mut usize, ok: &mut bool, src: &[u8]| {
        match crate::push_bytes(&mut buf[..], *n, src) {
            Some(e) => *n = e,
            None => *ok = false,
        }
    };
    put(&mut buf, &mut n, &mut ok, b"WARN ingress-deribit: seq gap channel=");
    put(&mut buf, &mut n, &mut ok, channel);
    put(&mut buf, &mut n, &mut ok, b" sym=");
    put(&mut buf, &mut n, &mut ok, fmt_hex_u32(sym, &mut digits));
    put(&mut buf, &mut n, &mut ok, b" expected=");
    let mut d2 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, fmt_i64(expected, &mut d2));
    put(&mut buf, &mut n, &mut ok, b" observed=");
    let mut d3 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, fmt_i64(observed, &mut d3));
    put(&mut buf, &mut n, &mut ok, b" increments=");
    let mut d4 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, crate::fmt_u64(increments as u64, &mut d4));
    put(&mut buf, &mut n, &mut ok, b" suppressed=");
    let mut d5 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, crate::fmt_u64(suppressed as u64, &mut d5));
    put(&mut buf, &mut n, &mut ok, b" ts_ns=");
    let mut d6 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, crate::fmt_u64(now, &mut d6));
    put(&mut buf, &mut n, &mut ok, b"\n");
    debug_assert!(ok, "gap log scratch sized for the worst case");
    if ok {
        // Best-effort: a torn/failed stderr write must never affect
        // the session (the counter + capture event already landed).
        let mut err = std::io::stderr().lock();
        let _ = std::io::Write::write_all(&mut err, &buf[..n]);
    }
}

/// Render `v` as `0x`-prefixed lowercase hex (SymbolId display form —
/// matches the audit tool's `sym=0x...` rendering).
#[inline]
fn fmt_hex_u32(v: u32, scratch: &mut [u8; 20]) -> &[u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    scratch[0] = b'0';
    scratch[1] = b'x';
    let mut i = 2;
    let mut shift = 32;
    while shift > 0 {
        shift -= 4;
        scratch[i] = HEX[((v >> shift) & 0xF) as usize];
        i += 1;
    }
    &scratch[..10]
}

/// Render `v` as decimal ASCII, `-`-prefixed when negative
/// (`i64::MIN` renders via u64 magnitude — no overflow branch).
#[inline]
fn fmt_i64(v: i64, scratch: &mut [u8; 20]) -> &[u8] {
    if v >= 0 {
        return crate::fmt_u64(v as u64, scratch);
    }
    // Magnitude fits u64 for every negative i64 incl. MIN.
    let mag = (v as i128).unsigned_abs() as u64;
    let digits = crate::fmt_u64(mag, scratch);
    let len = digits.len();
    let start = scratch.len() - len;
    debug_assert!(start >= 1, "20-digit scratch always leaves sign room");
    scratch[start - 1] = b'-';
    &scratch[start - 1..]
}

/// After a verified subscribe result: mark every configured channel
/// acknowledged in the `SubTable`.
fn register_confirmed_subs(drv: &mut Driver) {
    let mut i = 0;
    while i < drv.symbols.len() {
        let n_ch = row_channels(&drv.symbols, i, drv.depth_enabled);
        // Instrument bytes copied to end the immutable table borrow
        // before the mutable subs borrow (two-phase pattern).
        let mut instr_buf = [0u8; crate::DERIBIT_INSTR_MAX];
        let instr_len = {
            let Some((instr, _sym)) = drv.symbols.get(i) else {
                debug_assert!(false, "row {i} < len() must exist");
                break;
            };
            instr_buf[..instr.len()].copy_from_slice(instr);
            instr.len()
        };
        let instr = &instr_buf[..instr_len];
        let mut c = 0;
        while c < n_ch {
            let ch = CHANNEL_ORDER[c];
            match drv.subs.insert(sub_id_of(ch, instr), DeribitSubKind::from_channel(ch)) {
                Ok(()) => {}
                Err(SubErr::ReservedId) => {}
                Err(SubErr::Full) => {
                    debug_assert!(false, "deribit sub table full at SUB_CAP={SUB_CAP}");
                }
            }
            c += 1;
        }
        i += 1;
    }
}

// ---------------------------------------------------------------
// Top-level driver — mio-driven loop
// ---------------------------------------------------------------

/// Stop flag raised by external threads for graceful shutdown.
pub type StopFlag = AtomicBool;

/// Run the Deribit ingress loop until `stop` is set or the session
/// ends. Reconnect is the caller's responsibility.
///
/// `keepalive` drives the venue-specific probe: on `SendPing` a
/// `public/test` request is queued and flushed (Deribit has no
/// WS-level ping; the venue's own `test_request` heartbeats are
/// answered inline in the drain). On `Reconnect` the session is dead
/// by policy → [`RunResult::IdleTimeout`] — the cli configures the
/// idle budget at ~2× the 15 s heartbeat interval.
#[allow(clippy::too_many_arguments)]
pub fn run<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    token: mio::Token,
    stop: &StopFlag,
    status: &IngressStatus,
    keepalive: &mut Keepalive,
    capture: &mut C,
) -> RunResult {
    let session_start_ns = now_ns();
    keepalive.reset();

    // T1(a) (outage 2026-08-27 §5.5): every fatal site below records
    // its site + io-kind into the status slot before returning
    // `RunResult::Error`; the cli names the triple on its
    // `run-loop returned` line. First-error-wins, cleared by the
    // caller — see `IngressStatus::take_last_err`.
    if let Err(e) = transport.register(poll.registry(), token) {
        status.note_session_err(
            core_metrics::ERR_SITE_REGISTER,
            core_metrics::io_kind_code(e.kind()),
        );
        return RunResult::Error;
    }
    let mut last_interest = transport.interest();

    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = poll.poll(events, Some(std::time::Duration::from_millis(50))) {
            status.note_session_err(
                core_metrics::ERR_SITE_POLL,
                core_metrics::io_kind_code(e.kind()),
            );
            return RunResult::Error;
        }

        for ev in events.iter() {
            if ev.token() != token {
                continue;
            }
            let transport_status = match transport.pump(ev) {
                Ok(s) => s,
                Err(e) => {
                    status.note_session_err(
                        core_metrics::ERR_SITE_PUMP,
                        core_metrics::io_kind_code(e.kind()),
                    );
                    return RunResult::Error;
                }
            };
            note_transport_ready(drv, transport_status);
        }

        // Tight inner drain (see ingress-polymarket for rationale).
        loop {
            let n_before = producer.len();
            let state_before = drv.state();
            if let Err(e) = drive_one(transport, drv, host, path, producer, status, capture) {
                status.note_session_err(
                    core_metrics::ERR_SITE_DRIVE,
                    core_metrics::io_kind_code(e.kind()),
                );
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

        // Keepalive: proactive `public/test` when nothing has been
        // received for the ping interval; venue heartbeats and every
        // push refresh the activity clock.
        if drv.state() == State::Steady {
            let now = now_ns();
            let last = if drv.last_activity_ns == 0 {
                session_start_ns
            } else {
                drv.last_activity_ns
            };
            match keepalive.poll(now, last) {
                KeepaliveAction::None => {}
                KeepaliveAction::SendPing => {
                    if queue_test(drv).is_err() || flush_tx(transport, drv).is_err() {
                        // T1(a): site only — the two sources have
                        // distinct error types; the site is the
                        // diagnostic payload here.
                        status.note_session_err(core_metrics::ERR_SITE_KEEPALIVE, 0);
                        return RunResult::Error;
                    }
                    keepalive.mark_ping_sent(now);
                }
                KeepaliveAction::Reconnect => return RunResult::IdleTimeout,
            }
        }

        let cur = transport.interest();
        if cur != last_interest {
            if let Err(e) = transport.reregister(poll.registry(), token) {
                status.note_session_err(
                    core_metrics::ERR_SITE_REREGISTER,
                    core_metrics::io_kind_code(e.kind()),
                );
                return RunResult::Error;
            }
            last_interest = cur;
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
        KeepaliveCfg, TestTransport,
    };
    use core_ring::Ring;
    use core_types::NullCapture;

    /// Venue-namespaced test symbol (venue byte 3 = Deribit, ord 1).
    const SYM_BTC: u32 = (3 << 24) | 1;

    fn test_symbols() -> DeribitSymbolTable {
        let mut t = DeribitSymbolTable::new();
        t.insert(b"BTC-PERPETUAL", SYM_BTC).unwrap();
        t.insert(b"ETH-PERPETUAL", (3 << 24) | 2).unwrap();
        t
    }

    fn steady_driver(depth: bool) -> Driver {
        let mut d = Driver::new(7, test_symbols(), depth);
        d.set_state(State::Steady);
        d.session_started = true;
        // Session subscribe pretend-sent with id 2 (hb = 1).
        d.next_req_id = 3;
        d.subscribe_req_id = 2;
        d
    }

    fn generous_keepalive() -> Keepalive {
        Keepalive::new(KeepaliveCfg {
            ping_interval_ns: u64::MAX / 4,
            idle_timeout_ns: u64::MAX / 2,
        })
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

    /// Unmasked (server→client) WS text frame around `body`.
    fn wrap_text_frame(body: &[u8], out: &mut [u8]) -> usize {
        out[0] = 0x81;
        if body.len() <= 125 {
            out[1] = body.len() as u8;
            out[2..2 + body.len()].copy_from_slice(body);
            2 + body.len()
        } else {
            assert!(body.len() <= u16::MAX as usize);
            out[1] = 126;
            let len_be = (body.len() as u16).to_be_bytes();
            out[2] = len_be[0];
            out[3] = len_be[1];
            out[4..4 + body.len()].copy_from_slice(body);
            4 + body.len()
        }
    }

    /// Unmask every masked client frame in `buf`, concatenating the
    /// payloads (test-side inspection of our own tx stream).
    fn unmask_client_frames(mut buf: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while buf.len() >= 2 {
            let masked = buf[1] & 0x80 != 0;
            let mut len = (buf[1] & 0x7F) as usize;
            let mut at = 2;
            if len == 126 {
                len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                at = 4;
            }
            assert!(masked, "client frames must be masked");
            let mask = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
            at += 4;
            for i in 0..len {
                out.push(buf[at + i] ^ mask[i & 3]);
            }
            buf = &buf[at + len..];
        }
        out
    }

    fn ring_pair() -> (
        Producer<Tick, TICK_RING_CAP>,
        core_ring::Consumer<Tick, TICK_RING_CAP>,
    ) {
        let ring = Ring::<Tick, TICK_RING_CAP>::new();
        ring.split()
    }

    fn inject_text(t: &mut TestTransport, body: &[u8]) {
        let mut frame = vec![0u8; body.len() + 8];
        let n = wrap_text_frame(body, &mut frame);
        t.inject_incoming(&frame[..n]);
    }

    #[test]
    fn driver_starts_in_connecting() {
        let d = Driver::new(1, test_symbols(), false);
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.sub_count(), 0);
        assert_eq!(d.pending_count(), 0);
        assert!(!d.session_started);
    }

    #[test]
    fn note_transport_ready_advances_and_closes() {
        let mut d = Driver::new(1, test_symbols(), false);
        note_transport_ready(&mut d, Status::Ready);
        assert_eq!(d.state(), State::NeedsWsWrite);
        note_transport_ready(&mut d, Status::Closed);
        assert_eq!(d.state(), State::Closed);
    }

    #[test]
    fn handshake_completes_queues_heartbeat_then_batched_subscribe() {
        let mut t = TestTransport::with_capacity(65536);
        let mut d = Driver::new(42, test_symbols(), true);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"www.deribit.com", b"/ws/api/v2", &mut prod, &status, &mut NullCapture).unwrap();
        let mut scratch = vec![0u8; 65536];
        let _ = t.drain_outgoing(&mut scratch);

        let key = sec_key_pub(42);
        let accept = expected_accept_pub(&key);
        let resp = build_server_response(&accept);
        t.inject_incoming(&resp);

        drive_one(&mut t, &mut d, b"www.deribit.com", b"/ws/api/v2", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.state(), State::Steady);
        assert!(d.session_started);
        assert_eq!(status.state(), IngressState::Up);
        // Two in-flight requests: set_heartbeat (id 1) + subscribe (id 2).
        assert_eq!(d.pending_count(), 2);
        assert_eq!(d.subscribe_req_id, 2);

        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        let hb_at = memchr::memmem::find(&body, b"\"method\":\"public/set_heartbeat\"")
            .expect("set_heartbeat queued");
        let sub_at = memchr::memmem::find(&body, b"\"method\":\"public/subscribe\"")
            .expect("subscribe queued");
        assert!(hb_at < sub_at, "heartbeat must be armed before subscribing");
        assert!(memchr::memmem::find(&body, b"\"interval\":15").is_some());
        // One batched subscribe containing every configured pair.
        assert_eq!(
            memchr::memmem::find_iter(&body, b"\"method\":\"public/subscribe\"").count(),
            1
        );
        assert!(memchr::memmem::find(&body, b"\"quote.BTC-PERPETUAL\"").is_some());
        assert!(memchr::memmem::find(&body, b"\"ticker.ETH-PERPETUAL.100ms\"").is_some());
        assert!(memchr::memmem::find(&body, b"\"trades.BTC-PERPETUAL.100ms\"").is_some());
        // Depth enabled: book present.
        assert!(memchr::memmem::find(&body, b"\"book.BTC-PERPETUAL.100ms\"").is_some());
    }

    #[test]
    fn subscribe_result_confirms_all_channels() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        record_pending(&mut d, 2, DeribitReqKind::SubscribeAll).unwrap();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","quote.ETH-PERPETUAL","ticker.ETH-PERPETUAL.100ms","trades.ETH-PERPETUAL.100ms"],"testnet":false}"#;
        inject_text(&mut t, result);

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.pending_count(), 0, "subscribe retired");
        // 2 instruments × 3 channels acknowledged.
        assert_eq!(d.sub_count(), 6);
        assert_eq!(
            d.subs.kind_of(sub_id_of(DeribitChannel::Quote, b"BTC-PERPETUAL")),
            Some(DeribitSubKind::Quote)
        );
        assert_eq!(status.parse_errors_total(), 0);
    }

    #[test]
    fn subscribe_result_missing_channel_fails_the_session() {
        // debug builds crash on the debug_assert (fail-fast); the
        // release-path behaviour is a session error → reconnect.
        if cfg!(debug_assertions) {
            return;
        }
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        record_pending(&mut d, 2, DeribitReqKind::SubscribeAll).unwrap();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        // trades.ETH missing from the echo.
        let result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","quote.ETH-PERPETUAL","ticker.ETH-PERPETUAL.100ms"]}"#;
        inject_text(&mut t, result);

        let e = drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_request_is_answered_with_public_test() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        inject_text(
            &mut t,
            br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"test_request"}}"#,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();

        // The reply is already flushed (same drive cycle).
        let mut scratch = [0u8; 8192];
        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        assert!(
            memchr::memmem::find(&body, b"\"method\":\"public/test\"").is_some(),
            "test_request must be answered with public/test"
        );
        assert_eq!(d.pending_count(), 1, "the public/test call is in flight");
        assert_eq!(status.msgs_total(), 1);

        // The venue's result then retires the pending slot.
        inject_text(&mut t, br#"{"jsonrpc":"2.0","id":3,"result":{"version":"1.2.26"}}"#);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(d.pending_count(), 0);
        assert_eq!(status.parse_errors_total(), 0);
    }

    #[test]
    fn plain_heartbeat_is_quiet_activity() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        inject_text(
            &mut t,
            br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"heartbeat"}}"#,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(d.pending_count(), 0, "no reply for a plain heartbeat");
        assert!(status.last_activity_ns() > 0, "heartbeat refreshes the idle clock");
        let mut scratch = [0u8; 4096];
        assert_eq!(t.drain_outgoing(&mut scratch), 0, "nothing queued");
    }

    #[test]
    fn quote_advances_ticks_total_heartbeat_does_not() {
        // T1(b) (outage 2026-08-27 §5.3): the backoff predicate reads
        // `ticks_total` — a heartbeat advances msgs only; a quote
        // push advances ticks. Failure mode covered: a session that
        // only ever receives control frames keeps ticks_total at 0.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let hb = br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"heartbeat"}}"#;
        inject_text(&mut t, hb);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.msgs_total(), 1, "heartbeat counts as a message");
        assert_eq!(status.ticks_total(), 0, "heartbeat must not count as a tick");

        let quote = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-PERPETUAL","data":{"timestamp":1550658624149,"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}"#;
        inject_text(&mut t, quote);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.ticks_total(), 1, "quote push is a tick");
        let _ = cons.try_pop().expect("tick must be pushed");
    }

    #[test]
    fn quote_push_emits_tick_with_venue_byte_and_ms_seq() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let quote = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-PERPETUAL","data":{"timestamp":1550658624149,"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}"#;
        inject_text(&mut t, quote);

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.ring_drops_total(), 0);

        let tick = cons.try_pop().expect("tick must be pushed");
        assert_eq!(tick.sym, SYM_BTC);
        assert_eq!(tick.venue, VenueId::Deribit as u8);
        // venue_seq = venue ms timestamp truncated to u32 (crate doc).
        assert_eq!(tick.venue_seq, 1_550_658_624_149u64 as u32);
        assert_eq!(tick.bid_px.raw(), 3_914_970_000);
        assert_eq!(tick.ask_px.raw(), 3_996_610_000);
        assert_eq!(tick.bid_qty.raw(), 40_000_000);
        assert_eq!(tick.ask_qty.raw(), 50_000_000);
        assert!(tick.ts_ns > 0);
    }

    #[test]
    fn unknown_instrument_counts_parse_error() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let quote = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.SOL-PERPETUAL","data":{"timestamp":1000,"best_bid_price":1.0,"best_bid_amount":1.0,"best_ask_price":2.0,"best_ask_amount":1.0}}}"#;
        inject_text(&mut t, quote);

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.parse_errors_total(), 1);
        assert_eq!(status.msgs_total(), 0);
        assert!(cons.try_pop().is_none());
    }

    #[test]
    fn venue_error_response_fails_the_session() {
        // debug builds crash on the debug_assert (fail-fast).
        if cfg!(debug_assertions) {
            return;
        }
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        inject_text(
            &mut t,
            br#"{"jsonrpc":"2.0","id":9,"error":{"code":10028,"message":"too_many_requests"}}"#,
        );
        let e = drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn ticker_push_counts_as_slow_message() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let ticker = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-PERPETUAL.100ms","data":{"timestamp":1550652954406,"open_interest":18918470,"min_price":3943.21,"max_price":3982.84,"mark_price":3940.06,"index_price":3931.73,"current_funding":0.00042}}}"#;
        inject_text(&mut t, ticker);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.parse_errors_total(), 0);
        assert!(cons.try_pop().is_none(), "tickers do not enter the Tick lane");
    }

    #[test]
    fn book_gap_queues_unsubscribe_subscribe_resync() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(true);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = GapEventRecorder::default();

        // Snapshot roots the chain…
        let snap = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":1000,"instrument_name":"BTC-PERPETUAL","change_id":10,"bids":[["new",1.0,1.0]],"asks":[],"type":"snapshot"}}}"#;
        inject_text(&mut t, snap);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.gaps_total(), 0);
        let mut scratch = vec![0u8; 16384];
        let _ = t.drain_outgoing(&mut scratch);
        let pending_before = d.pending_count();

        // …then a chain break (prev 99 ≠ last 10).
        let broken = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":2000,"instrument_name":"BTC-PERPETUAL","change_id":100,"prev_change_id":99,"bids":[],"asks":[["delete",1.0,0.0]],"type":"change"}}}"#;
        inject_text(&mut t, broken);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(status.gaps_total(), 1);
        assert_eq!(status.resubscribes_total(), 1);
        // §6.6 pairing: the increment must have exactly one BookGap
        // event carrying expected (chain last = 10) vs observed (99).
        assert_eq!(cap.gaps.len(), 1, "1:1 event pairing");
        let g = cap.gaps[0];
        assert_eq!(g.0, ChannelId::BookGap as u8);
        assert_eq!(g.2, 100, "venue_seq = the message change_id");
        assert_eq!(g.3, 2000, "venue_time_ms from the broken frame");
        assert_eq!(g.4, 10, "v0 = expected prev_change_id");
        assert_eq!(g.5, 99, "v1 = observed prev_change_id");
        assert_eq!(d.pending_count(), pending_before + 2, "unsub + sub in flight");
        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        let unsub_at = memchr::memmem::find(&body, b"\"method\":\"public/unsubscribe\"")
            .expect("unsubscribe queued");
        let sub_at = memchr::memmem::find(&body, b"\"method\":\"public/subscribe\"")
            .expect("subscribe queued");
        assert!(unsub_at < sub_at, "unsubscribe must precede subscribe");
        assert!(memchr::memmem::find(&body, b"\"book.BTC-PERPETUAL.100ms\"").is_some());

        // The fresh snapshot then re-roots without further gaps.
        let fresh = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":3000,"instrument_name":"BTC-PERPETUAL","change_id":200,"bids":[],"asks":[],"type":"snapshot"}}}"#;
        inject_text(&mut t, fresh);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.gaps_total(), 1, "snapshot is not a gap");
        assert_eq!(cap.gaps.len(), 1, "no event without an increment");
    }

    /// Records every `TradeGap`/`BookGap` capture event:
    /// `(channel, sym, venue_seq, venue_time_ms, v0 expected, v1
    /// observed)` — pins the §6.6 1:1 increment↔event pairing.
    #[derive(Default)]
    struct GapEventRecorder {
        gaps: Vec<(u8, u32, u64, u64, i64, i64)>,
        trade_rows: u32,
    }

    impl core_types::Capture for GapEventRecorder {
        fn event(&mut self, e: &ChannelEvent) {
            if e.channel == ChannelId::TradeGap as u8 || e.channel == ChannelId::BookGap as u8 {
                self.gaps.push((e.channel, e.sym, e.venue_seq, e.venue_time_ms, e.v0, e.v1));
            } else if e.channel == ChannelId::Trade as u8 {
                self.trade_rows += 1;
            }
        }
    }

    /// THE G1-remediation regression (first 6 h soak, 2026-08-15):
    /// burst-coalesced `trades` frames bigger than the old 16-row
    /// seq-check cap produced one phantom gap each — 67 across the
    /// soak (41 BTC + 26 ETH), every one refuted by capture. Fixture
    /// sequences are the REAL ones from
    /// `run-1786742370972151000/deribit-events.pmlr`: the largest BTC
    /// burst frame (59 rows, seqs 296247098..=296247156, venue ts
    /// 1786752386548) followed by the real next frame head
    /// (seq 296247157). Raw frame bytes were not tapped (tap ran
    /// rejects-only), so rows are re-rendered in the live starbase
    /// field order around those sequences.
    #[test]
    fn burst_frame_all_rows_seq_checked_no_phantom_gap() {
        const FIRST: i64 = 296_247_098;
        const LAST: i64 = 296_247_156; // 59 rows
        const NEXT: i64 = 296_247_157;

        // Shadow of the PRE-FIX semantics (16-row sample + per-row
        // apply): documents that this fixture is discriminating — it
        // fails the old algorithm (the "red" of red→green) without
        // resurrecting it in the shipped code.
        {
            let mut m = DeribitTradeSeq::new();
            let mut s = FIRST;
            while s < FIRST + 16 {
                assert_eq!(m.apply(s), TradeSeqOutcome::Ok);
                s += 1;
            }
            assert_eq!(
                m.apply(NEXT),
                TradeSeqOutcome::Gap,
                "old cap-16 semantics: phantom gap on the next frame"
            );
        }

        let mut t = TestTransport::with_capacity(65536);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = GapEventRecorder::default();

        // 59-row burst frame in live starbase field order.
        let mut burst = String::from(
            r#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":["#,
        );
        let mut seq = FIRST;
        while seq <= LAST {
            if seq != FIRST {
                burst.push(',');
            }
            burst.push_str(&format!(
                r#"{{"timestamp":1786752386548,"price":62959.5,"direction":"sell","instrument_name":"BTC-PERPETUAL","index_price":62955.01,"trade_id":"{}","trade_seq":{seq},"amount":2500.0,"mark_price":62959.0,"tick_direction":1,"starbase_match_id":214172482240081920,"contracts":250.0,"starbase_timestamp":1786752386548878197}}"#,
                seq - FIRST + 1
            ));
            seq += 1;
        }
        burst.push_str("]}}");
        inject_text(&mut t, burst.as_bytes());
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.msgs_total(), 59, "every row counted");
        assert_eq!(status.gaps_total(), 0, "contiguous burst: no gaps");

        // The real next frame chains off the burst's TRUE tail.
        let next = format!(
            r#"{{"jsonrpc":"2.0","method":"subscription","params":{{"channel":"trades.BTC-PERPETUAL.100ms","data":[{{"timestamp":1786752410291,"price":62941.5,"direction":"buy","instrument_name":"BTC-PERPETUAL","trade_id":"60","trade_seq":{NEXT},"amount":100.0,"mark_price":62941.0,"tick_direction":0,"starbase_match_id":214172482240081999,"contracts":10.0,"starbase_timestamp":1786752410291707552}}]}}}}"#
        );
        inject_text(&mut t, next.as_bytes());
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(status.msgs_total(), 60);
        assert_eq!(status.gaps_total(), 0, "NO phantom gap on the frame edge");
        assert_eq!(cap.gaps.len(), 0, "no increment ⇒ no gap event");
        assert_eq!(cap.trade_rows, 60, "capture still records every row");
    }

    /// A REAL inter-frame hole must still be caught — and must carry
    /// exactly one paired `TradeGap` event with expected vs observed.
    #[test]
    fn inter_frame_real_gap_pairs_one_trade_gap_event() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = GapEventRecorder::default();

        let f1 = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":50,"trade_id":"1","timestamp":1000,"price":1.0,"direction":"buy","amount":1.0},{"trade_seq":51,"trade_id":"2","timestamp":1001,"price":1.0,"direction":"sell","amount":1.0}]}}"#;
        inject_text(&mut t, f1);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.gaps_total(), 0);

        // Frame edge jumps 51 → 53: one missed trade.
        let f2 = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":53,"trade_id":"3","timestamp":2000,"price":1.0,"direction":"buy","amount":1.0}]}}"#;
        inject_text(&mut t, f2);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(status.gaps_total(), 1, "real hole caught at the frame edge");
        assert_eq!(cap.gaps.len(), 1, "1:1 event pairing");
        let g = cap.gaps[0];
        assert_eq!(g.0, ChannelId::TradeGap as u8);
        assert_eq!(g.2, 53, "venue_seq = observed");
        assert_eq!(g.3, 2000, "venue_time_ms from the gapped frame head");
        assert_eq!(g.4, 52, "v0 = expected");
        assert_eq!(g.5, 53, "v1 = observed");
    }

    /// A discontinuity INSIDE one frame: counted once, paired once
    /// (event emitted by the phase-1 walk), and the monitor adopts the
    /// frame tail so the next contiguous frame chains cleanly.
    #[test]
    fn intra_frame_break_pairs_event_and_monitor_adopts_tail() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = GapEventRecorder::default();

        // 50 then 52 in ONE frame — break between adjacent rows.
        let f1 = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":50,"trade_id":"1","timestamp":1000,"price":1.0,"direction":"buy","amount":1.0},{"trade_seq":52,"trade_id":"2","timestamp":1001,"price":1.0,"direction":"sell","amount":1.0}]}}"#;
        inject_text(&mut t, f1);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(status.gaps_total(), 1, "intra-frame break counted once");
        assert_eq!(cap.gaps.len(), 1, "1:1 event pairing");
        let g = cap.gaps[0];
        assert_eq!(g.0, ChannelId::TradeGap as u8);
        assert_eq!(g.4, 51, "v0 = expected (prev row + 1)");
        assert_eq!(g.5, 52, "v1 = observed");

        // Tail adopted: 53 chains without a second count.
        let f2 = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":53,"trade_id":"3","timestamp":2000,"price":1.0,"direction":"buy","amount":1.0}]}}"#;
        inject_text(&mut t, f2);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.gaps_total(), 1, "52 → 53 chains off the adopted tail");
        assert_eq!(cap.gaps.len(), 1);
    }

    #[test]
    fn trade_seq_gap_and_regression_count_gaps() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let t1 = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":50,"trade_id":"1","timestamp":1000,"price":1.0,"direction":"buy","amount":1.0}]}}"#;
        // seq 52 after 50: one-trade gap.
        let t2 = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":52,"trade_id":"2","timestamp":1001,"price":1.0,"direction":"sell","amount":1.0}]}}"#;
        // seq 40: regression.
        let t3 = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":40,"trade_id":"3","timestamp":1002,"price":1.0,"direction":"buy","amount":1.0}]}}"#;
        inject_text(&mut t, t1);
        inject_text(&mut t, t2);
        inject_text(&mut t, t3);

        let mut cap = GapEventRecorder::default();
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.msgs_total(), 3);
        assert_eq!(status.gaps_total(), 2, "one jump + one regression");
        assert_eq!(status.resubscribes_total(), 0, "trades are never resubscribed");
        assert_eq!(status.parse_errors_total(), 0);
        // §6.6 pairing: both increments carry TradeGap events.
        assert_eq!(cap.gaps.len(), 2, "1:1 event pairing");
        assert_eq!((cap.gaps[0].4, cap.gaps[0].5), (51, 52), "jump: expected 51, observed 52");
        assert_eq!((cap.gaps[1].4, cap.gaps[1].5), (53, 40), "regression: expected 53, observed 40");
    }

    #[test]
    fn trades_multi_row_push_counts_each_row_sequentially() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let multi = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":50,"trade_id":"1","timestamp":1000,"price":1.0,"direction":"buy","amount":1.0},{"trade_seq":51,"trade_id":"2","timestamp":1001,"price":1.1,"direction":"sell","amount":2.0}]}}"#;
        inject_text(&mut t, multi);

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.msgs_total(), 2, "both rows counted");
        assert_eq!(status.gaps_total(), 0, "50 → 51 chains");
    }

    /// Records every hook invocation — pins the §6.5 capture-site
    /// semantics without touching the filesystem.
    #[derive(Default)]
    struct CountingCapture {
        ticks: u32,
        events: u32,
        raw_frames: u32,
        rejects: u32,
        flushes: u32,
        last_event_channel: u8,
    }

    /// Records every Trade event's (venue_seq, v0 px, v1 signed qty) —
    /// pins per-row field pairing for the starbase-order regression.
    #[derive(Default)]
    struct TradeRecorder {
        rows: Vec<(u64, i64, i64)>,
        rejects: u32,
    }

    impl core_types::Capture for TradeRecorder {
        fn event(&mut self, e: &ChannelEvent) {
            if e.channel == ChannelId::Trade as u8 {
                self.rows.push((e.venue_seq, e.v0, e.v1));
            }
        }
        fn parse_reject(&mut self, _ts_ns: u64, _payload: &[u8]) {
            self.rejects += 1;
        }
    }

    /// Live-wire regression (raw-tapped 2026-08-15): Deribit's
    /// starbase engine reorders trade rows — `timestamp`/`price`/
    /// `direction` precede `trade_seq`; `amount` (scientific notation
    /// for round sizes) and `starbase_*` fields follow it. The 8c
    /// marker slicing straddled rows here; object-extent slicing must
    /// parse every row with correctly-paired fields.
    #[test]
    fn starbase_order_trades_parse_and_pair_correctly() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = TradeRecorder::default();

        // Two-row frame in the NEW field order, distinct px/amount per
        // row (anti-cross-pairing), row 2 amount in sci notation.
        let multi = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"timestamp":1786740206308,"price":62863.5,"direction":"buy","instrument_name":"BTC-PERPETUAL","index_price":62860.11,"trade_id":"101","trade_seq":296244199,"amount":250.0,"mark_price":62863.68,"tick_direction":0,"starbase_match_id":214121762240081920,"contracts":25.0,"starbase_timestamp":1786740206308878197},{"timestamp":1786740206309,"price":62864.0,"direction":"sell","instrument_name":"BTC-PERPETUAL","index_price":62860.12,"trade_id":"102","trade_seq":296244200,"amount":1.0e3,"mark_price":62863.70,"tick_direction":1,"starbase_match_id":214121762240081921,"contracts":100.0,"starbase_timestamp":1786740206309878197}]}}"#;
        // Single-row frame in the new order — the case the marker
        // slicing could NEVER parse (head fields before trade_seq).
        let single = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"timestamp":1786740299406,"price":62863.0,"direction":"buy","instrument_name":"BTC-PERPETUAL","trade_id":"103","trade_seq":296244201,"amount":50040.0,"mark_price":62863.13,"tick_direction":0,"starbase_match_id":214122152721395712,"contracts":5004.0,"starbase_timestamp":1786740299406707552}]}}"#;
        let mut frame = [0u8; 2048];
        for payload in [&multi[..], &single[..]] {
            let n = wrap_text_frame(payload, &mut frame);
            t.inject_incoming(&frame[..n]);
        }

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(cap.rejects, 0, "no rejects on the starbase wire");
        assert_eq!(status.parse_errors_total(), 0);
        assert_eq!(status.msgs_total(), 3, "all three rows parsed");
        assert_eq!(status.gaps_total(), 0, "sequential seqs, no holes");
        assert_eq!(cap.rows.len(), 3);
        // Field pairing: each row's px/amount stay together; sell row
        // negated; sci amount decoded (1.0e3 → 1000 USD ×1e6).
        assert_eq!(cap.rows[0], (296244199, 62_863_500_000, 250_000_000));
        assert_eq!(cap.rows[1], (296244200, 62_864_000_000, -1_000_000_000));
        assert_eq!(cap.rows[2], (296244201, 62_863_000_000, 50_040_000_000));
    }

    impl core_types::Capture for CountingCapture {
        fn tick(&mut self, _t: &Tick) {
            self.ticks += 1;
        }
        fn event(&mut self, e: &ChannelEvent) {
            self.events += 1;
            self.last_event_channel = e.channel;
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
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = CountingCapture::default();

        // A ticker alone first — pins the Ticker event channel.
        let ticker = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-PERPETUAL.100ms","data":{"timestamp":1550652954406,"open_interest":18918470,"min_price":3943.21,"max_price":3982.84,"mark_price":3940.06,"index_price":3931.73,"current_funding":0.00042}}}"#;
        inject_text(&mut t, ticker);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.events, 1, "ticker captured as event");
        assert_eq!(cap.last_event_channel, core_types::ChannelId::Ticker as u8);
        assert_eq!(cap.ticks, 0, "tickers do not enter the Tick lane");

        // One quote (tick + raw), one trades push with one good + one
        // broken row (event + row reject + raw), one garbage frame
        // (frame reject + raw).
        let quote = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-PERPETUAL","data":{"timestamp":1550658624149,"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}"#;
        let trades = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":50,"trade_id":"1","timestamp":1000,"price":1.0,"direction":"sell","amount":2.0},{"trade_seq":51,"trade_id":"2","broken":true}]}}"#;
        let garbage = br#"{"what":"ever"}"#;
        inject_text(&mut t, quote);
        inject_text(&mut t, trades);
        inject_text(&mut t, garbage);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(cap.raw_frames, 4, "every data payload is tapped");
        assert_eq!(cap.ticks, 1, "quote captured as tick");
        assert_eq!(cap.events, 2, "one good trade row captured");
        assert_eq!(cap.last_event_channel, core_types::ChannelId::Trade as u8);
        assert_eq!(
            cap.rejects, 2,
            "broken trade row + garbage frame both tapped as rejects"
        );
        assert_eq!(status.parse_errors_total(), 2);
        // Tick still captured when the ring is full: fill it, resend.
        while prod.try_push(Tick::new(
            1,
            VenueId::Deribit,
            SYM_BTC,
            1,
            Price::from_raw(1),
            Qty::from_raw(1),
            Price::from_raw(2),
            Qty::from_raw(1),
        ))
        .is_ok()
        {}
        inject_text(&mut t, quote);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(cap.ticks, 2, "ring-dropped tick still captured");
        assert_eq!(status.ring_drops_total(), 1);
    }

    #[test]
    fn late_rpc_result_counts_parse_error() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        // id 55 was never sent by us.
        inject_text(&mut t, br#"{"jsonrpc":"2.0","id":55,"result":"ok"}"#);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap();
        assert_eq!(status.parse_errors_total(), 1);
        assert_eq!(status.msgs_total(), 0);
    }

    #[test]
    fn oversized_frame_fails_the_session() {
        let mut t = TestTransport::with_capacity(RX_BUF_SIZE + 64 * 1024);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        // 64-bit-length frame header declaring a payload larger than
        // the whole rx buffer, followed by filler that fills rx.
        let declared = (RX_BUF_SIZE + 1024) as u64;
        let mut junk = vec![0u8; RX_BUF_SIZE + 16];
        junk[0] = 0x81;
        junk[1] = 127;
        junk[2..10].copy_from_slice(&declared.to_be_bytes());
        t.inject_incoming(&junk);

        let e = drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut NullCapture).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reset_for_reconnect_clears_connection_state() {
        let mut d = steady_driver(true);
        record_pending(&mut d, 9, DeribitReqKind::Test).unwrap();
        d.subs
            .insert(sub_id_of(DeribitChannel::Quote, b"BTC-PERPETUAL"), DeribitSubKind::Quote)
            .unwrap();
        assert_eq!(
            d.book_chains[0].apply(crate::BOOK_ACTION_SNAPSHOT, -1, 5),
            ChainOutcome::Init
        );
        d.reset_for_reconnect(9);
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.sub_count(), 0);
        assert_eq!(d.pending_count(), 0);
        assert_eq!(d.next_req_id, 1);
        assert_eq!(d.subscribe_req_id, 0);
        assert!(!d.session_started);
        // Chains re-armed: a change without snapshot is a gap again.
        assert_eq!(
            d.book_chains[0].apply(crate::BOOK_ACTION_CHANGE, 5, 6),
            ChainOutcome::Gap
        );
    }

    #[test]
    fn run_returns_idle_timeout_on_dead_session() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let mut ka = Keepalive::new(KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: 2,
        });

        let res = run(
            &mut t, &mut d, b"h", b"/", &mut prod, &mut poll, &mut events,
            mio::Token(1), &stop, &status, &mut ka, &mut NullCapture,
        );
        assert_eq!(res, RunResult::IdleTimeout);
    }

    #[test]
    fn run_emits_proactive_public_test_before_idle_timeout() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        // Probe due immediately; death after ~150 ms of silence.
        let mut ka = Keepalive::new(KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: 150_000_000,
        });

        let res = run(
            &mut t, &mut d, b"h", b"/", &mut prod, &mut poll, &mut events,
            mio::Token(1), &stop, &status, &mut ka, &mut NullCapture,
        );
        assert_eq!(res, RunResult::IdleTimeout);
        let mut scratch = [0u8; 4096];
        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        assert!(
            memchr::memmem::find(&body, b"\"method\":\"public/test\"").is_some(),
            "proactive public/test must have been sent"
        );
    }

    #[test]
    fn run_disconnects_on_server_close() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver(false);
        // Server-side Close frame (unmasked, empty payload).
        t.inject_incoming(&[0x88, 0x00]);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let mut ka = generous_keepalive();

        let res = run(
            &mut t, &mut d, b"h", b"/", &mut prod, &mut poll, &mut events,
            mio::Token(1), &stop, &status, &mut ka, &mut NullCapture,
        );
        assert_eq!(res, RunResult::Disconnected);
        assert_eq!(status.bytes_total(), 2);
    }

    #[test]
    fn masks_cover_exactly_the_configured_channel_set() {
        let syms = test_symbols();
        // Without depth: 2 instruments × 3 channels = bits 0..2, 4..6.
        let e = expected_mask(&syms, false);
        assert_eq!(e, 0b0111 | (0b0111 << 4));
        // With depth: all four channels per instrument.
        let e = expected_mask(&syms, true);
        assert_eq!(e, 0b1111 | (0b1111 << 4));
        // found_mask matches only quoted full names.
        let payload = br#"["quote.BTC-PERPETUAL","trades.ETH-PERPETUAL.100ms"]"#;
        let f = found_mask(payload, &syms, false);
        assert_eq!(f, channel_bit(0, 0) | channel_bit(1, 2));
    }

    #[test]
    fn masks_fold_option_rows_into_one_bit_requiring_all_channels() {
        // M2 partition law: static rows bits 0..64; option rows ONE
        // bit each at 64 + opt_idx, folded over quote + ticker (M2.3)
        // — found only when BOTH channels are acknowledged; depth
        // never touches them.
        let mut syms = test_symbols(); // 2 static rows
        syms.insert_option(b"BTC-27MAR26-100000-C", 513).unwrap();
        syms.insert_option(b"BTC-27MAR26-100000-P", 514).unwrap();
        let e = expected_mask(&syms, false);
        let static_part: u128 = 0b0111 | (0b0111 << 4);
        let opt_part: u128 = (1u128 << STATIC_MASK_BITS) | (1u128 << (STATIC_MASK_BITS + 1));
        assert_eq!(e, static_part | opt_part);
        // Depth adds book bits for STATIC rows only.
        let e_depth = expected_mask(&syms, true);
        assert_eq!(e_depth, (0b1111u128 | (0b1111 << 4)) | opt_part);
        // Quote alone does NOT confirm an option row…
        let quote_only = br#"["quote.BTC-27MAR26-100000-P"]"#;
        assert_eq!(found_mask(quote_only, &syms, false), 0);
        // …ticker alone doesn't either…
        let ticker_only = br#"["ticker.BTC-27MAR26-100000-P.100ms"]"#;
        assert_eq!(found_mask(ticker_only, &syms, false), 0);
        // …both together set exactly the row's folded bit (the other
        // option's stray ticker contributes nothing).
        let both = br#"["quote.BTC-27MAR26-100000-P","ticker.BTC-27MAR26-100000-P.100ms","ticker.BTC-27MAR26-100000-C.100ms"]"#;
        let f = found_mask(both, &syms, false);
        assert_eq!(f, 1u128 << (STATIC_MASK_BITS + 1));
    }
}
