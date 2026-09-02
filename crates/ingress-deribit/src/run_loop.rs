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
//! channel list.
//!
//! ## Subscribe-verification policy (WS2 — outage 2026-08-27 §5.2)
//!
//! Until this process has ever seen ONE fully-verified subscribe
//! result, any expected channel missing from the echo ⇒
//! misconfiguration ⇒ session error, and venue `error` responses are
//! equally fatal (fail-fast at BOOT: refuse to run venue-blind).
//! Once a full verification has ever succeeded, a missing channel on
//! a RECONNECT is the expired-instrument class — the venue dropped it
//! from the universe mid-run — and becomes a NON-FATAL PER-CHANNEL
//! DROP: only the echoed subset registers, everything echoed keeps
//! flowing on the same connection. Venue `error` responses likewise
//! become non-fatal (the pending request completes; a whole-subscribe
//! refusal leaves zero registered channels and the establishment
//! budget below tears the session down). Each drop increments
//! `sub_drops_total`, emits one paired `ChannelId::SubDrop` capture
//! event (§6.6 pairing) and a rate-limited stderr WARN. Reconnects
//! re-send the full configured set (drops are session-scoped); the
//! 0830/1605 slot restarts remain the chain-refresh mechanism.
//!
//! ## Establishment budget (WS2 — outage §5.3)
//!
//! The keepalive idle timeout is gated on `Steady`; a session wedged
//! in `Connecting`/`AwaitingWsUpgrade` (blackholed SYN under the
//! non-blocking connect), or `Steady` with zero registered channels,
//! could previously live forever. [`run`] now returns
//! [`RunResult::EstablishTimeout`] when no subscription has been
//! confirmed within the driver's establishment budget
//! (`core_net::ESTABLISH_BUDGET_NS` default).
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
use core_time::{now_ns, FeedClock};
use core_types::{
    Capture, ChannelEvent, ChannelId, DepthTopK, OptSummary, Price, Qty, Tick, VenueId,
    DEPTH_RING_SIZE, EVENT_RING_SIZE, OPT_RING_SIZE, SYMBOL_ID_NONE, TICK_FLAG_STALE,
};

use crate::{
    classify, extract_instrument, parse_book_header, parse_option_ticker, parse_quote,
    parse_ticker, parse_trade, parse_vol_index, row_wants_channel, sub_id_of, write_book_op,
    write_set_heartbeat, write_subscribe_all, write_test, ChainOutcome, DeribitChannel,
    DeribitMsgKind, DeribitSymbolTable, DeribitTradeSeq, DvolName, TradeSeqOutcome,
    DERIBIT_DVOL_MAX, DERIBIT_MAX_SYMBOLS, HEARTBEAT_INTERVAL_SECS,
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
    /// WS2 (outage §5.3): no subscription was confirmed within the
    /// establishment budget — the session never got past the
    /// handshake, or the venue refused/omitted every channel. Caller
    /// reconnects; the backoff keeps ESCALATING (no market data
    /// moved, so the T1(b) reset predicate stays false —
    /// deliberately unlike `IdleTimeout`'s venue-quiet reset).
    EstablishTimeout,
    /// Fatal transport / protocol error (includes venue `error`
    /// responses and missing subscribe confirmations at BOOT — see
    /// the module's subscribe-verification policy).
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
    /// WS10-B: per-instrument depth ladders (symbol-table order;
    /// empty when depth is off). Boot-allocated; steady state only
    /// indexes.
    ladders: Vec<book_builder::ladder::DepthLadder>,
    /// WS10-B: last EMITTED top-K per instrument — the change gate.
    last_depth: Vec<DepthTopK>,
    /// WS6: configured DVOL index names (`n_dvol` valid). Boot-set,
    /// connection-independent; ordinal = position here (the capture
    /// event's `v1` identity).
    dvol: [DvolName; DERIBIT_DVOL_MAX],
    /// Valid prefix length of `dvol`.
    n_dvol: usize,
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
    /// WS2: PROCESS-LIFETIME flag — true once ANY subscribe result
    /// has ever passed FULL verification. While false, missing
    /// channels and venue errors stay fatal (boot fail-fast: refuse
    /// venue-blind configs); once true, they become non-fatal drops.
    /// Deliberately NOT cleared by [`Self::reset_for_reconnect`].
    subs_ever_confirmed: bool,
    /// WS2: establishment budget (ns from session start to the first
    /// confirmed subscription) enforced by [`run`]. Defaults to
    /// [`core_net::ESTABLISH_BUDGET_NS`]; overridable for tests /
    /// operator tuning via [`Self::set_establish_budget_ns`].
    establish_budget_ns: u64,
    /// Wall clock of the last emitted gap log line (rate limit:
    /// [`GAP_LOG_INTERVAL_NS`]). Deliberately NOT reset per session —
    /// the limit is an operator-terminal budget, not session state.
    gap_log_last_ns: u64,
    /// Gap increments swallowed by the rate limit since the last
    /// emitted line (carried into that next line as `suppressed=`).
    gap_log_suppressed: u32,
    /// WS2 drop-log rate limiter — same operator-terminal-budget
    /// semantics as the gap logger's (not session state).
    drop_log_last_ns: u64,
    /// Drop increments swallowed by the rate limit since the last
    /// emitted drop line.
    drop_log_suppressed: u32,
    /// VT2: this connection's venue-clock offset estimator + staleness
    /// judge for `quote.*` (`timestamp`, ms). Reset on reconnect;
    /// threshold = venue default or `--stale-after-ms deribit:<ms>` via
    /// [`Self::set_stale_after_ms`].
    feed_clock: FeedClock,
    /// `!Sync` marker — see struct doc.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate buffers (boot-time) and seed the handshake nonce.
    /// `symbols` maps every configured instrument; `depth_enabled`
    /// adds the `book.*.100ms` channel per instrument.
    pub fn new(nonce_seed: u64, symbols: DeribitSymbolTable, depth_enabled: bool) -> Self {
        Self::new_with_dvol(nonce_seed, symbols, depth_enabled, &[])
    }

    /// WS6: [`Self::new`] plus configured DVOL index names
    /// (`btc_usd` forms — one `deribit_volatility_index.{index}`
    /// subscription each). Over-long/overflowing entries are dropped
    /// with a debug assert (the config layer caps both — the OKX
    /// families pattern).
    pub fn new_with_dvol(
        nonce_seed: u64,
        symbols: DeribitSymbolTable,
        depth_enabled: bool,
        dvol_names: &[&[u8]],
    ) -> Self {
        let sec_key = sec_websocket_key_from_seed(nonce_seed);
        let accept = expected_accept(&sec_key);
        let mut dvol: [DvolName; DERIBIT_DVOL_MAX] = [(0, [0; 16]); DERIBIT_DVOL_MAX];
        let mut n_dvol = 0usize;
        let mut i = 0;
        while i < dvol_names.len() {
            let name = dvol_names[i];
            if name.is_empty() || name.len() > 16 || n_dvol >= DERIBIT_DVOL_MAX {
                debug_assert!(
                    false,
                    "dvol entry dropped (len/cap) — config layer caps this"
                );
                i += 1;
                continue;
            }
            dvol[n_dvol].0 = name.len() as u8;
            dvol[n_dvol].1[..name.len()].copy_from_slice(name);
            n_dvol += 1;
            i += 1;
        }
        // WS10-B: per-instrument ladders + last-emitted snapshots,
        // boot-allocated once (empty when depth is off).
        let n_syms = symbols.len();
        let (ladders, last_depth) = if depth_enabled {
            (
                vec![book_builder::ladder::DepthLadder::new(); n_syms],
                vec![DepthTopK::EMPTY; n_syms],
            )
        } else {
            (Vec::new(), Vec::new())
        };
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
            ladders,
            last_depth,
            dvol,
            n_dvol,
            subs: SubTable::new(),
            pending: PendingTable::new(),
            next_req_id: 1,
            subscribe_req_id: 0,
            book_chains: [crate::DeribitBookChain::new(); DERIBIT_MAX_SYMBOLS],
            trade_seqs: [DeribitTradeSeq::new(); DERIBIT_MAX_SYMBOLS],
            session_started: false,
            subs_ever_confirmed: false,
            establish_budget_ns: core_net::ESTABLISH_BUDGET_NS,
            gap_log_last_ns: 0,
            gap_log_suppressed: 0,
            drop_log_last_ns: 0,
            drop_log_suppressed: 0,
            feed_clock: FeedClock::new(VenueId::Deribit.default_stale_after_ms()),
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
    /// `--stale-after-ms deribit:<ms>`). Boot-time only — re-arms the
    /// estimator unlearned, exactly like a fresh connection.
    #[inline]
    pub fn set_stale_after_ms(&mut self, ms: u32) {
        self.feed_clock = FeedClock::new(ms);
    }

    /// VT2: the connection's smoothed `quote` feed delay in ms.
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
    /// `subs_ever_confirmed` (WS2 boot/reconnect discriminator), the
    /// establishment budget and both log rate limiters are
    /// process-lifetime — untouched here.
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
        // VT2: a new connection is a new offset; the threshold stays.
        self.feed_clock.reset();
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
#[allow(clippy::too_many_arguments)]
pub fn drive_one<T: Transport, C: Capture>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    depth_tx: &mut Producer<DepthTopK, DEPTH_RING_SIZE>,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
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
            drain_ws_frames(
                drv, producer, event_tx, event_mask, depth_tx, opt_tx, status, capture,
            )?;
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

    // 2. One batched subscribe for every (channel × instrument)
    //    (+ WS6 DVOL indices).
    let sub_id = drv.alloc_req_id();
    let mut scratch = [0u8; SUBSCRIBE_SCRATCH];
    let n = write_subscribe_all(
        &mut scratch,
        sub_id,
        &drv.symbols,
        drv.depth_enabled,
        &drv.dvol[..drv.n_dvol],
    )
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

/// WS6: true when row `idx` is a TAIL row (option or combo) — tail
/// rows FOLD their channels into one per-row mask bit.
#[inline]
fn is_tail_row(symbols: &DeribitSymbolTable, idx: usize) -> bool {
    symbols.is_option_row(idx) || symbols.is_combo_row(idx)
}

/// Verification-mask bit for `(row, channel)` under the M2/WS6
/// partition law. TAIL rows (options: quote + ticker; combos: quote
/// only) FOLD their channels into ONE per-row bit — [`found_mask`]
/// sets it only when EVERY wanted channel was acknowledged, so the
/// u128 stays exactly 64 static-channel bits + 64 tail-row bits.
/// Static rows use per-channel bits at the fixed channel index
/// (spot rows simply never occupy the ticker bit —
/// [`row_wants_channel`]).
#[inline]
fn row_bit(symbols: &DeribitSymbolTable, idx: usize, ch: usize) -> u128 {
    if is_tail_row(symbols, idx) {
        option_bit(idx, symbols.static_len())
    } else {
        channel_bit(idx, ch)
    }
}

/// Expected-channel mask for the configured table (+depth flag),
/// driven by the [`row_wants_channel`] policy. WS6: DVOL channels
/// are deliberately OUTSIDE this mask (no bits left in the u128; an
/// absent DVOL echo surfaces as a missing capture series, not a
/// session verdict).
fn expected_mask(symbols: &DeribitSymbolTable, depth_enabled: bool) -> u128 {
    let mut m = 0u128;
    let mut i = 0;
    while i < symbols.len() {
        let mut c = 0;
        while c < CHANNELS_PER_INSTR {
            if row_wants_channel(symbols, i, c, depth_enabled) {
                m |= row_bit(symbols, i, c);
            }
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
        // Tail rows fold: the per-row bit requires EVERY wanted
        // channel (options 2, combos 1).
        let mut wanted = 0usize;
        let mut found_in_row = 0usize;
        let mut c = 0;
        while c < CHANNELS_PER_INSTR {
            if !row_wants_channel(symbols, i, c, depth_enabled) {
                c += 1;
                continue;
            }
            wanted += 1;
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
                if is_tail_row(symbols, i) {
                    found_in_row += 1;
                } else {
                    m |= row_bit(symbols, i, c);
                }
            }
            c += 1;
        }
        if is_tail_row(symbols, i) && wanted > 0 && found_in_row == wanted {
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
    SubscribeResult {
        id: u64,
        found: u128,
        expected: u128,
    },
    /// Venue `error` response. Fatal at boot; a non-fatal drop on
    /// reconnect (WS2 — module doc). Carries the JSON-RPC id so the
    /// drop path can complete the pending request.
    VenueError { id: u64, code: i32 },
    /// WS6: one DVOL push, pre-parsed + ordinal-resolved in phase 1.
    VolIndex {
        ts_ms: u64,
        vol_1e9: i64,
        ordinal: i64,
    },
    /// `quote` push became a Tick.
    Quote { tick: Tick },
    /// `ticker` push validated (slow lane; captured as a §6.5 event).
    Ticker,
    /// M2.3: an OPTION row's ticker parsed → `OptSummary` captured in
    /// phase 1 (capture-only; nothing reaches the engine ring).
    OptSummary,
    /// `trades` push scanned (`sym` rides along for phase-2 gap
    /// events — resolving it again would re-borrow `drv.symbols`).
    Trades {
        sym: u32,
        sym_idx: u8,
        scan: TradeScan,
    },
    /// `book` push — chain applied AND (WS10-B) ladder walked in
    /// phase 1 (payload in scope there; the `BookGap` pairing event
    /// moved with the apply). Phase 2 counts, rate-limit-logs and
    /// queues the resync when `gapped`.
    Book {
        sym: u32,
        sym_idx: u8,
        gapped: bool,
        expected_prev: i64,
        prev: i64,
    },
}

fn drain_ws_frames<C: Capture>(
    drv: &mut Driver,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    depth_tx: &mut Producer<DepthTopK, DEPTH_RING_SIZE>,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
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
                            depth_tx,
                            opt_tx,
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

#[allow(clippy::too_many_arguments)]
fn handle_data_frame<C: Capture>(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    depth_tx: &mut Producer<DepthTopK, DEPTH_RING_SIZE>,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
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
            DeribitMsgKind::RpcError { id, code } => Dispatch::VenueError { id, code },
            // WS6: DVOL — venue-global; identity = ordinal into the
            // boot-configured index list (never the symbol table).
            DeribitMsgKind::VolIndexPush => match parse_vol_index(payload) {
                Some(f) => {
                    let name = &f.index_name[..f.index_name_len as usize];
                    let mut ordinal: i64 = -1;
                    let mut d = 0;
                    while d < drv.n_dvol {
                        let (len, ref bytes) = drv.dvol[d];
                        if &bytes[..len as usize] == name {
                            ordinal = d as i64;
                            break;
                        }
                        d += 1;
                    }
                    if ordinal < 0 {
                        // A push for an index we never configured —
                        // venue noise; count it.
                        Dispatch::Nothing
                    } else {
                        Dispatch::VolIndex {
                            ts_ms: f.ts_ns / 1_000_000,
                            vol_1e9: f.vol_1e9,
                            ordinal,
                        }
                    }
                }
                None => Dispatch::Nothing,
            },
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
                            Some(f) => {
                                // VT2: one parse-complete stamp serves
                                // the tick AND the staleness judgement;
                                // `timestamp` is the venue quote time.
                                let now = now_ns();
                                let judged = drv.feed_clock.judge(f.ts_ms, now);
                                Dispatch::Quote {
                                    tick: Tick::new_stamped(
                                        now,
                                        VenueId::Deribit,
                                        sym,
                                        // No seq on quotes: venue ms
                                        // timestamp, truncated (crate doc).
                                        f.ts_ms as u32,
                                        Price::from_raw(f.bid_px_1e6),
                                        Qty::from_raw(f.bid_qty_1e6),
                                        Price::from_raw(f.ask_px_1e6),
                                        Qty::from_raw(f.ask_qty_1e6),
                                        f.ts_ms,
                                        (judged.stale as u8) * TICK_FLAG_STALE,
                                    ),
                                }
                            }
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
                                    let o = OptSummary::new(
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
                                    );
                                    capture.opt_summary(&o);
                                    // VM2 V2: onto the opt lane
                                    // (capture stays first — §6.5
                                    // capture-before-push law).
                                    if opt_tx.try_push(o).is_err() {
                                        status.inc_opt_ring_drops();
                                    }
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
                                // WS3 (gaps §1): the parser filled
                                // `current_funding_1e9` since 8c and
                                // this site DROPPED it. Emit it as the
                                // venue's Funding event — v0 = rate
                                // ×1e9 (OKX-compatible scaling).
                                // VM2 V2: v1 = `funding_8h` ×1e9 (was
                                // a constant 0 — additive: the vm's
                                // hourly deribit sample prefers this,
                                // the SAME series the worker's REST
                                // lane stores, ÷8 law downstream; 0
                                // when the frame lacked the field).
                                // Gated on venue truth: dated-future
                                // tickers carry no funding field
                                // (`has_funding` = the wire-level
                                // settlement_period split).
                                if tk.has_funding == 1 {
                                    let ev = ChannelEvent::new(
                                        now_ns(),
                                        VenueId::Deribit,
                                        ChannelId::Funding,
                                        sym,
                                        0,
                                        tk.ts_ns / 1_000_000,
                                        tk.current_funding_1e9,
                                        tk.funding_8h_1e9,
                                    );
                                    capture.event(&ev);
                                    // WS10-A: onto the venue-event
                                    // lane (capture stays first —
                                    // §6.5 capture-before-push law).
                                    if event_mask & core_types::event_lane_bit(ChannelId::Funding)
                                        != 0
                                        && event_tx.try_push(ev).is_err()
                                    {
                                        status.inc_event_ring_drops();
                                    }
                                }
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
                                // Chain apply moved into phase 1
                                // (WS10-B needs the payload for the
                                // level walk; `book_chains` and `rx`
                                // are disjoint driver fields). The
                                // §6.6 BookGap pairing event moves
                                // with it; log + resync stay phase 2.
                                let expected_prev = drv.book_chains[sym_idx].last_change_id();
                                let outcome = drv.book_chains[sym_idx].apply(
                                    b.action,
                                    b.prev_change_id,
                                    b.change_id,
                                );
                                let gapped = matches!(outcome, ChainOutcome::Gap);
                                if gapped {
                                    // §6.6 pairing: v0 = expected prev
                                    // (i64::MIN = awaiting snapshot),
                                    // v1 = observed prev.
                                    capture.event(&ChannelEvent::new(
                                        now_ns(),
                                        VenueId::Deribit,
                                        ChannelId::BookGap,
                                        sym,
                                        b.change_id as u64,
                                        b.ts_ns / 1_000_000,
                                        expected_prev,
                                        b.prev_change_id,
                                    ));
                                }
                                if !drv.ladders.is_empty() {
                                    deribit_depth_step(
                                        &mut drv.ladders[sym_idx],
                                        &mut drv.last_depth[sym_idx],
                                        payload,
                                        sym,
                                        gapped,
                                        b.action == crate::BOOK_ACTION_SNAPSHOT,
                                        depth_tx,
                                        status,
                                        capture,
                                    );
                                }
                                Dispatch::Book {
                                    sym,
                                    sym_idx: sym_idx as u8,
                                    gapped,
                                    expected_prev,
                                    prev: b.prev_change_id,
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
        Dispatch::SubscribeResult {
            id,
            found,
            expected,
        } => {
            let _ = drv.pending.complete(id);
            status.add_msgs(1);
            let missing = expected & !found;
            if missing != 0 {
                // T1(a) (outage 2026-08-27 §5.2): this is the exact
                // post-settlement kill site — expired option channels
                // vanish from the echo. `venue_code` carries the
                // COUNT of missing channels (the u128 masks don't fit
                // a u32; the count is the operator-facing signal).
                // Recorded on BOTH paths: evidence on the drop path,
                // verdict on the boot path.
                status.note_venue_err_code(missing.count_ones());
                if !drv.subs_ever_confirmed {
                    // BOOT fail-fast (unchanged doctrine): the first-
                    // ever subscribe of a config came back incomplete
                    // — misconfiguration; refuse to run venue-blind.
                    // Crash loudly in debug, session error in release
                    // (reconnect w/ backoff, operator sees it).
                    status.note_session_err(
                        core_metrics::ERR_SITE_SUBSCRIBE_MISSING,
                        core_metrics::io_kind_code(io::ErrorKind::InvalidData),
                    );
                    debug_assert!(
                        false,
                        "deribit subscribe result missing channels at boot: expected {expected:#x} found {found:#x}"
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "deribit subscribe result missing configured channels",
                    ));
                }
                // WS2 NON-FATAL DROP (outage §5.2): the venue dropped
                // these channels from its universe mid-run (expired
                // instruments) — register what WAS echoed, name every
                // missing channel, keep the session flowing. A fully-
                // empty echo registers nothing and the establishment
                // budget tears the session down.
                emit_sub_drops(drv, missing, status, capture);
            } else {
                // WS2: a FULL verification arms the process-lifetime
                // boot/reconnect discriminator.
                drv.subs_ever_confirmed = true;
            }
            register_confirmed_subs(drv, found);
        }
        Dispatch::VenueError { id, code } => {
            // T1(a) (outage 2026-08-27 §5.2): record the venue's
            // numeric code (i32 bit-cast; negative JSON-RPC codes
            // round-trip through the u32 slot) — first-wins so the
            // outer drive-site conversion keeps this inner site.
            // Recorded on BOTH paths below: evidence on the drop
            // path, verdict on the boot path.
            status.note_venue_err_code(code as u32);
            if !drv.subs_ever_confirmed {
                // BOOT fail-fast (unchanged doctrine): a venue error
                // before any confirmed subscribe means our request /
                // framing / config is wrong. Crash loudly in debug,
                // surface a session error in release.
                status.note_session_err(
                    core_metrics::ERR_SITE_VENUE_ERROR,
                    core_metrics::io_kind_code(io::ErrorKind::InvalidData),
                );
                debug_assert!(false, "deribit venue error response at boot, code={code}");
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "deribit venue error response",
                ));
            }
            // WS2 NON-FATAL DROP: complete the failed request (frees
            // the pending slot) and count/name the refusal. A refused
            // session subscribe leaves sub_count() == 0 → the
            // establishment budget ends the session; a refused book
            // resync/test/heartbeat degrades only that lane.
            let _ = drv.pending.complete(id);
            status.inc_sub_drops();
            capture.event(&ChannelEvent::new(
                now_ns(),
                VenueId::Deribit,
                ChannelId::SubDrop,
                SYMBOL_ID_NONE, // RPC errors carry id+code, no instrument
                0,
                0,
                code as i64,
                -1,
            ));
            log_sub_drop_rate_limited(drv, SYMBOL_ID_NONE, code as i64, -1);
        }
        Dispatch::VolIndex {
            ts_ms,
            vol_1e9,
            ordinal,
        } => {
            // WS6 §6.5 capture: venue-global series — sym =
            // SYMBOL_ID_NONE, v0 = volatility points ×1e9, v1 = the
            // configured-index ordinal (the offline identity; the
            // boot log + universe file resolve it).
            capture.event(&ChannelEvent::new(
                now_ns(),
                VenueId::Deribit,
                ChannelId::VolIndex,
                SYMBOL_ID_NONE,
                0,
                ts_ms,
                vol_1e9,
                ordinal,
            ));
            status.add_msgs(1);
            status.add_ticks(1);
        }
        Dispatch::Quote { tick } => {
            status.add_msgs(1);
            status.add_ticks(1);
            // VT2: stale quotes are captured and pushed like any other
            // (the flag travels with the slot); counted here, gauge
            // published per tick.
            if tick.is_stale() {
                status.inc_stale_ticks();
            }
            status.set_feed_delay_ema_ms(drv.feed_clock.delay_ema_ms());
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
        Dispatch::Book {
            sym,
            sym_idx,
            gapped,
            expected_prev,
            prev,
        } => {
            status.add_msgs(1);
            status.add_ticks(1);
            // Chain applied + BookGap event emitted in phase 1; the
            // &mut-Driver log + tx-queueing halves live here.
            if gapped {
                status.inc_gaps();
                log_gap_rate_limited(drv, b"book", sym, expected_prev, prev, 1);
                status.inc_resubscribes();
                queue_book_resync(drv, sym_idx as usize)?;
            }
        }
    }
    Ok(())
}

/// WS10-B: one book frame's depth step — ladder maintenance +
/// change-gated emission (capture first, then the depth-lane push;
/// full-ring pushes count `depth_ring_drops`). On a chain gap the
/// ladder clears and a `DEPTH_FLAG_STALE` snapshot ALWAYS emits so a
/// strategy never trades a known-broken book.
#[allow(clippy::too_many_arguments)]
fn deribit_depth_step<C: Capture>(
    ladder: &mut book_builder::ladder::DepthLadder,
    last: &mut DepthTopK,
    payload: &[u8],
    sym: u32,
    gapped: bool,
    is_snapshot: bool,
    depth_tx: &mut Producer<DepthTopK, DEPTH_RING_SIZE>,
    status: &IngressStatus,
    capture: &mut C,
) {
    if gapped {
        ladder.clear();
        let stale = ladder.snapshot(
            now_ns(),
            VenueId::Deribit,
            sym,
            core_types::DEPTH_FLAG_STALE,
        );
        capture.depth(&stale);
        if depth_tx.try_push(stale).is_err() {
            status.inc_depth_ring_drops();
        }
        *last = stale;
        return;
    }
    if is_snapshot {
        ladder.clear();
    }
    match crate::walk_book_levels(payload, ladder) {
        Some(_) => {
            let snap = ladder.snapshot(now_ns(), VenueId::Deribit, sym, 0);
            if !book_builder::ladder::levels_equal(&snap, last) {
                capture.depth(&snap);
                if depth_tx.try_push(snap).is_err() {
                    status.inc_depth_ring_drops();
                }
                *last = snap;
            }
        }
        None => {
            // Malformed level rows behind a valid header: count it;
            // the chain monitor remains the resync law.
            status.inc_parse_errors();
            capture.parse_reject(now_ns(), payload);
        }
    }
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
    let put =
        |buf: &mut [u8; 224], n: &mut usize, ok: &mut bool, src: &[u8]| match crate::push_bytes(
            &mut buf[..],
            *n,
            src,
        ) {
            Some(e) => *n = e,
            None => *ok = false,
        };
    put(
        &mut buf,
        &mut n,
        &mut ok,
        b"WARN ingress-deribit: seq gap channel=",
    );
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
    put(
        &mut buf,
        &mut n,
        &mut ok,
        crate::fmt_u64(increments as u64, &mut d4),
    );
    put(&mut buf, &mut n, &mut ok, b" suppressed=");
    let mut d5 = [0u8; 20];
    put(
        &mut buf,
        &mut n,
        &mut ok,
        crate::fmt_u64(suppressed as u64, &mut d5),
    );
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

/// After a subscribe result: mark every channel the venue actually
/// ECHOED (`found` bit set) acknowledged in the `SubTable`. WS2: on
/// a full verification `found == expected` and every configured
/// channel registers (the pre-WS2 behaviour); on a partial reconnect
/// echo only the confirmed subset registers — option rows fold
/// quote+ticker into one bit, so a clear bit registers neither.
fn register_confirmed_subs(drv: &mut Driver, found: u128) {
    let mut i = 0;
    while i < drv.symbols.len() {
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
        while c < CHANNELS_PER_INSTR {
            if !row_wants_channel(&drv.symbols, i, c, drv.depth_enabled) {
                c += 1;
                continue;
            }
            if found & row_bit(&drv.symbols, i, c) == 0 {
                // WS2: not echoed by the venue — dropped, not
                // registered (emit_sub_drops named it).
                c += 1;
                continue;
            }
            let ch = CHANNEL_ORDER[c];
            match drv
                .subs
                .insert(sub_id_of(ch, instr), DeribitSubKind::from_channel(ch))
            {
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

/// WS2: name every missing subscribe-echo bit — one `sub_drops_total`
/// increment + one paired [`ChannelId::SubDrop`] capture event per
/// missing (row, channel), plus the rate-limited stderr WARN. Static
/// rows carry the channel index in `v1`; option rows fold quote +
/// ticker into one per-row bit and carry `v1 = -1`. Cold path — runs
/// once per subscribe result.
fn emit_sub_drops<C: Capture>(
    drv: &mut Driver,
    missing: u128,
    status: &IngressStatus,
    capture: &mut C,
) {
    let mut i = 0;
    while i < drv.symbols.len() {
        // Symbol id copied out (Copy) — the table borrow must end
        // before the mutable logger borrow below.
        let sym = match drv.symbols.get(i) {
            Some((_instr, sym)) => sym,
            None => {
                debug_assert!(false, "row {i} < len() must exist");
                break;
            }
        };
        if is_tail_row(&drv.symbols, i) {
            if missing & option_bit(i, drv.symbols.static_len()) != 0 {
                status.inc_sub_drops();
                capture.event(&ChannelEvent::new(
                    now_ns(),
                    VenueId::Deribit,
                    ChannelId::SubDrop,
                    sym,
                    0,
                    0,
                    0, // missing-from-echo: no venue code
                    -1,
                ));
                log_sub_drop_rate_limited(drv, sym, 0, -1);
            }
        } else {
            let mut c = 0;
            while c < CHANNELS_PER_INSTR {
                if row_wants_channel(&drv.symbols, i, c, drv.depth_enabled)
                    && missing & channel_bit(i, c) != 0
                {
                    status.inc_sub_drops();
                    capture.event(&ChannelEvent::new(
                        now_ns(),
                        VenueId::Deribit,
                        ChannelId::SubDrop,
                        sym,
                        0,
                        0,
                        0,
                        c as i64,
                    ));
                    log_sub_drop_rate_limited(drv, sym, 0, c as i64);
                }
                c += 1;
            }
        }
        i += 1;
    }
}

/// Rate-limited (1 line / [`GAP_LOG_INTERVAL_NS`]) WS2 sub-drop WARN
/// line — same zero-alloc stderr contract as
/// [`log_gap_rate_limited`]; the 1:1 evidence record is the paired
/// `SubDrop` capture event, not this line. `code` 0 = missing-from-
/// echo; `ch` −1 = unknown / folded option row.
fn log_sub_drop_rate_limited(drv: &mut Driver, sym: u32, code: i64, ch: i64) {
    let now = now_ns();
    if now.wrapping_sub(drv.drop_log_last_ns) < GAP_LOG_INTERVAL_NS {
        drv.drop_log_suppressed = drv.drop_log_suppressed.saturating_add(1);
        return;
    }
    let suppressed = drv.drop_log_suppressed;
    drv.drop_log_last_ns = now;
    drv.drop_log_suppressed = 0;

    // `WARN ingress-deribit: sub-drop sym=<hex> code=<i64> ch=<i64>
    //  suppressed=<n> ts_ns=<n>\n`
    let mut buf = [0u8; 160];
    let mut digits = [0u8; 20];
    let mut n = 0usize;
    let mut ok = true;
    let put =
        |buf: &mut [u8; 160], n: &mut usize, ok: &mut bool, src: &[u8]| match crate::push_bytes(
            &mut buf[..],
            *n,
            src,
        ) {
            Some(e) => *n = e,
            None => *ok = false,
        };
    put(
        &mut buf,
        &mut n,
        &mut ok,
        b"WARN ingress-deribit: sub-drop sym=",
    );
    put(&mut buf, &mut n, &mut ok, fmt_hex_u32(sym, &mut digits));
    put(&mut buf, &mut n, &mut ok, b" code=");
    let mut d2 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, fmt_i64(code, &mut d2));
    put(&mut buf, &mut n, &mut ok, b" ch=");
    let mut d3 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, fmt_i64(ch, &mut d3));
    put(&mut buf, &mut n, &mut ok, b" suppressed=");
    let mut d4 = [0u8; 20];
    put(
        &mut buf,
        &mut n,
        &mut ok,
        crate::fmt_u64(suppressed as u64, &mut d4),
    );
    put(&mut buf, &mut n, &mut ok, b" ts_ns=");
    let mut d5 = [0u8; 20];
    put(&mut buf, &mut n, &mut ok, crate::fmt_u64(now, &mut d5));
    put(&mut buf, &mut n, &mut ok, b"\n");
    debug_assert!(ok, "drop log scratch sized for the worst case");
    if ok {
        // Best-effort: a torn/failed stderr write must never affect
        // the session (the counter + capture event already landed).
        let mut err = std::io::stderr().lock();
        let _ = std::io::Write::write_all(&mut err, &buf[..n]);
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
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    depth_tx: &mut Producer<DepthTopK, DEPTH_RING_SIZE>,
    opt_tx: &mut Producer<OptSummary, OPT_RING_SIZE>,
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
            if let Err(e) = drive_one(
                transport, drv, host, path, producer, event_tx, event_mask, depth_tx, opt_tx,
                status, capture,
            ) {
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
        let flush_now = now_ns();
        capture.maybe_flush(flush_now);

        // WS2 (outage §5.3): establishment budget — deliberately NOT
        // gated on `Steady`. A session wedged pre-upgrade (blackholed
        // SYN under the non-blocking connect) or `Steady` with zero
        // confirmed channels (subscribe refused/empty on a reconnect)
        // is dead by policy once the budget expires. One compare per
        // poll iteration, clock read shared with the flush above.
        if core_net::establishment_expired(
            flush_now,
            session_start_ns,
            drv.sub_count(),
            drv.establish_budget_ns,
        ) {
            status.note_session_err(
                core_metrics::ERR_SITE_ESTABLISH,
                core_metrics::io_kind_code(io::ErrorKind::TimedOut),
            );
            return RunResult::EstablishTimeout;
        }

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

    fn event_ring_pair() -> (
        Producer<ChannelEvent, EVENT_RING_SIZE>,
        core_ring::Consumer<ChannelEvent, EVENT_RING_SIZE>,
    ) {
        Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split()
    }

    fn depth_ring_pair() -> (
        Producer<DepthTopK, DEPTH_RING_SIZE>,
        core_ring::Consumer<DepthTopK, DEPTH_RING_SIZE>,
    ) {
        Ring::<DepthTopK, DEPTH_RING_SIZE>::new().split()
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
        producer: &mut Producer<Tick, TICK_RING_CAP>,
        status: &IngressStatus,
        capture: &mut C,
    ) -> io::Result<()> {
        let (mut etx, _erx) = event_ring_pair();
        let (mut dtx, _drx) = depth_ring_pair();
        let (mut otx, _orx) = opt_ring_pair();
        super::drive_one(
            transport,
            drv,
            host,
            path,
            producer,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut dtx,
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
        producer: &mut Producer<Tick, TICK_RING_CAP>,
        poll: &mut mio::Poll,
        events: &mut mio::Events,
        token: mio::Token,
        stop: &StopFlag,
        status: &IngressStatus,
        keepalive: &mut Keepalive,
        capture: &mut C,
    ) -> RunResult {
        let (mut etx, _erx) = event_ring_pair();
        let (mut dtx, _drx) = depth_ring_pair();
        let (mut otx, _orx) = opt_ring_pair();
        super::run(
            transport,
            drv,
            host,
            path,
            producer,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut dtx,
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
        drive_one(
            &mut t,
            &mut d,
            b"www.deribit.com",
            b"/ws/api/v2",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
        let mut scratch = vec![0u8; 65536];
        let _ = t.drain_outgoing(&mut scratch);

        let key = sec_key_pub(42);
        let accept = expected_accept_pub(&key);
        let resp = build_server_response(&accept);
        t.inject_incoming(&resp);

        drive_one(
            &mut t,
            &mut d,
            b"www.deribit.com",
            b"/ws/api/v2",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap();
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
        assert_eq!(d.pending_count(), 0, "subscribe retired");
        // 2 instruments × 3 channels acknowledged.
        assert_eq!(d.sub_count(), 6);
        assert_eq!(
            d.subs
                .kind_of(sub_id_of(DeribitChannel::Quote, b"BTC-PERPETUAL")),
            Some(DeribitSubKind::Quote)
        );
        assert_eq!(status.parse_errors_total(), 0);
    }

    #[test]
    fn subscribe_result_missing_channel_fails_the_session_at_boot() {
        // WS2: BOOT fail-fast preserved — before any full
        // verification has ever succeeded, a missing channel is
        // fatal (refuse venue-blind).
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

        let e = drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(status.sub_drops_total(), 0, "boot misses are not drops");
    }

    /// Records every ChannelEvent — WS2 drop-event pinning.
    #[derive(Default)]
    struct EventRecCap {
        events: Vec<ChannelEvent>,
    }
    impl Capture for EventRecCap {
        fn event(&mut self, e: &ChannelEvent) {
            self.events.push(*e);
        }
    }

    #[test]
    fn missing_channels_on_reconnect_drop_nonfatally_and_statics_flow() {
        // The WS2 exit test as named by the plan: instruments that
        // expired mid-run vanish from the reconnect echo; the echoed
        // channels register and keep flowing on the same session.
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        d.subs_ever_confirmed = true; // a boot session verified fully
        record_pending(&mut d, 2, DeribitReqKind::SubscribeAll).unwrap();
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let mut cap = EventRecCap::default();

        // The whole ETH row (3 channels) vanished from the echo —
        // the expired-instrument shape.
        let result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms"]}"#;
        inject_text(&mut t, result);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap)
            .expect("missing channels on reconnect must be non-fatal");

        assert_eq!(d.sub_count(), 3, "only the echoed subset registers");
        assert_eq!(
            d.subs
                .kind_of(sub_id_of(DeribitChannel::Quote, b"BTC-PERPETUAL")),
            Some(DeribitSubKind::Quote)
        );
        assert_eq!(
            d.subs
                .kind_of(sub_id_of(DeribitChannel::Quote, b"ETH-PERPETUAL")),
            None,
            "missing channel never registers"
        );
        assert_eq!(status.sub_drops_total(), 3, "one drop per missing channel");
        // Every drop event names the ETH row; static rows carry the
        // channel index in v1.
        let drops: Vec<_> = cap
            .events
            .iter()
            .filter(|e| e.channel == ChannelId::SubDrop as u8)
            .collect();
        assert_eq!(drops.len(), 3);
        let mut i = 0;
        while i < drops.len() {
            assert_eq!(drops[i].sym, (3 << 24) | 2);
            assert_eq!(drops[i].v0, 0, "missing-from-echo carries no venue code");
            assert!(drops[i].v1 >= 0 && drops[i].v1 < CHANNELS_PER_INSTR as i64);
            i += 1;
        }
        // T1 evidence: the missing COUNT recorded without a fatal
        // verdict.
        let snap = status.take_last_err();
        assert_eq!(snap.venue_code, 3);
        assert_eq!(snap.site, 0, "no session-error site on the drop path");
        // The surviving instrument still moves data.
        let quote = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-PERPETUAL","data":{"timestamp":1550658624149,"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40,"best_ask_price":3996.61,"best_ask_amount":50}}}"#;
        inject_text(&mut t, quote);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        let tick = cons.try_pop().expect("echoed channel keeps flowing");
        assert_eq!(tick.sym, SYM_BTC);
    }

    #[test]
    fn missing_option_row_drops_as_one_folded_bit() {
        // Option rows fold quote+ticker into ONE mask bit — a
        // vanished option is one drop event with v1 = -1.
        let mut symbols = test_symbols();
        symbols
            .insert_option(b"BTC-29AUG26-45000-C", (3 << 24) | 513)
            .unwrap();
        let mut t = TestTransport::with_capacity(16384);
        let mut d = Driver::new(7, symbols, false);
        d.set_state(State::Steady);
        d.session_started = true;
        d.next_req_id = 3;
        d.subscribe_req_id = 2;
        d.subs_ever_confirmed = true;
        record_pending(&mut d, 2, DeribitReqKind::SubscribeAll).unwrap();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = EventRecCap::default();

        // Statics fully echoed; the option's quote+ticker both
        // missing (expired strike).
        let result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","quote.ETH-PERPETUAL","ticker.ETH-PERPETUAL.100ms","trades.ETH-PERPETUAL.100ms"]}"#;
        inject_text(&mut t, result);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(
            d.sub_count(),
            6,
            "statics register; the option row does not"
        );
        assert_eq!(status.sub_drops_total(), 1, "folded row = one drop");
        let drop = cap
            .events
            .iter()
            .find(|e| e.channel == ChannelId::SubDrop as u8)
            .expect("SubDrop event captured");
        assert_eq!(drop.sym, (3 << 24) | 513);
        assert_eq!(drop.v1, -1, "folded option row");
    }

    #[test]
    fn full_verification_arms_the_reconnect_discriminator() {
        // subs_ever_confirmed flips exactly on a FULL verification
        // and survives reset_for_reconnect (process-lifetime).
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        assert!(!d.subs_ever_confirmed);
        record_pending(&mut d, 2, DeribitReqKind::SubscribeAll).unwrap();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","quote.ETH-PERPETUAL","ticker.ETH-PERPETUAL.100ms","trades.ETH-PERPETUAL.100ms"],"testnet":false}"#;
        inject_text(&mut t, result);
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
        assert!(d.subs_ever_confirmed);
        d.reset_for_reconnect(9);
        assert!(d.subs_ever_confirmed, "discriminator survives reset");
        assert_eq!(d.sub_count(), 0, "subscriptions stay connection-scoped");
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
        inject_text(
            &mut t,
            br#"{"jsonrpc":"2.0","id":3,"result":{"version":"1.2.26"}}"#,
        );
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
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(d.pending_count(), 0, "no reply for a plain heartbeat");
        assert!(
            status.last_activity_ns() > 0,
            "heartbeat refreshes the idle clock"
        );
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
        assert_eq!(status.msgs_total(), 1, "heartbeat counts as a message");
        assert_eq!(
            status.ticks_total(),
            0,
            "heartbeat must not count as a tick"
        );

        let quote = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-PERPETUAL","data":{"timestamp":1550658624149,"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}"#;
        inject_text(&mut t, quote);
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
        // VT2: the venue stamp rides the slot in full (venue_seq keeps
        // its truncated-u32 law).
        assert_eq!(tick.venue_time_ms, 1_550_658_624_149);
    }

    /// VT2 helper: one two-sided `quote.BTC-PERPETUAL` push stamped
    /// `ts_ms` through the steady driver; returns the tick it produced.
    fn push_quote_with_ts(
        t: &mut TestTransport,
        d: &mut Driver,
        prod: &mut Producer<Tick, TICK_RING_CAP>,
        cons: &mut core_ring::Consumer<Tick, TICK_RING_CAP>,
        status: &IngressStatus,
        ts_ms: u64,
    ) -> Tick {
        let s = format!(
            r#"{{"jsonrpc":"2.0","method":"subscription","params":{{"channel":"quote.BTC-PERPETUAL","data":{{"timestamp":{ts_ms},"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}}}}"#
        );
        inject_text(t, s.as_bytes());
        drive_one(t, d, b"h", b"/", prod, status, &mut NullCapture).unwrap();
        cons.try_pop().expect("quote must produce a tick")
    }

    #[test]
    fn quote_ticks_carry_the_stale_judgement() {
        // VT2: first stamped quote = the offset (fresh); a quote whose
        // timestamp is 5 s older is stale at deribit 600 ms (flag +
        // counter); a later stamp is fresh again.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let t0: u64 = 1_755_216_000_000;

        let fresh = push_quote_with_ts(&mut t, &mut d, &mut prod, &mut cons, &status, t0);
        assert_eq!(fresh.venue_time_ms, t0);
        assert!(!fresh.is_stale());
        assert_eq!(status.stale_ticks_total(), 0);

        let stale = push_quote_with_ts(&mut t, &mut d, &mut prod, &mut cons, &status, t0 - 5_000);
        assert!(stale.is_stale());
        assert_eq!(stale.flags, TICK_FLAG_STALE);
        assert_eq!(status.stale_ticks_total(), 1);
        assert!(status.feed_delay_ema_ms() > 0);

        let again = push_quote_with_ts(&mut t, &mut d, &mut prod, &mut cons, &status, t0 + 10);
        assert!(!again.is_stale());
        assert_eq!(status.stale_ticks_total(), 1);
        assert_eq!(status.ticks_total(), 3);
    }

    #[test]
    fn stale_threshold_override_and_reconnect_reset_apply() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        d.set_stale_after_ms(10_000);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();
        let t0: u64 = 1_755_216_000_000;
        let _ = push_quote_with_ts(&mut t, &mut d, &mut prod, &mut cons, &status, t0);
        let five_s = push_quote_with_ts(&mut t, &mut d, &mut prod, &mut cons, &status, t0 - 5_000);
        assert!(!five_s.is_stale(), "5 s is under a 10 s threshold");
        assert_eq!(status.stale_ticks_total(), 0);

        d.reset_for_reconnect(9);
        d.set_state(State::Steady);
        d.session_started = true;
        d.next_req_id = 3;
        d.subscribe_req_id = 2;
        let after = push_quote_with_ts(&mut t, &mut d, &mut prod, &mut cons, &status, t0 - 60_000);
        assert!(!after.is_stale(), "a reconnect starts a fresh offset");
        assert_eq!(after.venue_time_ms, t0 - 60_000);
    }

    #[test]
    fn unknown_instrument_counts_parse_error() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let quote = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.SOL-PERPETUAL","data":{"timestamp":1000,"best_bid_price":1.0,"best_bid_amount":1.0,"best_ask_price":2.0,"best_ask_amount":1.0}}}"#;
        inject_text(&mut t, quote);

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
        assert_eq!(status.parse_errors_total(), 1);
        assert_eq!(status.msgs_total(), 0);
        assert!(cons.try_pop().is_none());
    }

    #[test]
    fn venue_error_response_fails_the_session_at_boot() {
        // WS2: BOOT fail-fast preserved — venue errors before the
        // first-ever full verification stay fatal.
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
        let e = drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(status.sub_drops_total(), 0, "boot errors are not drops");
    }

    #[test]
    fn venue_error_on_reconnect_is_nonfatal_and_retires_pending() {
        // WS2: after the first-ever full verification, an RPC error
        // completes its pending request and counts a drop — the
        // session lives (a refused book resync degrades one lane; a
        // refused session subscribe leaves sub_count()==0 for the
        // establishment budget to reap).
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        d.subs_ever_confirmed = true;
        record_pending(&mut d, 9, DeribitReqKind::BookSub).unwrap();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = EventRecCap::default();

        inject_text(
            &mut t,
            br#"{"jsonrpc":"2.0","id":9,"error":{"code":10028,"message":"too_many_requests"}}"#,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap)
            .expect("post-confirmation venue error must be non-fatal");
        assert_eq!(
            d.pending_count(),
            0,
            "failed request retired from the table"
        );
        assert_eq!(status.sub_drops_total(), 1);
        let drop = cap
            .events
            .iter()
            .find(|e| e.channel == ChannelId::SubDrop as u8)
            .expect("SubDrop event captured");
        assert_eq!(drop.sym, SYMBOL_ID_NONE, "RPC errors name no instrument");
        assert_eq!(drop.v0, 10028);
        let snap = status.take_last_err();
        assert_eq!(snap.venue_code as i32, 10028);
        assert_eq!(snap.site, 0, "no session-error site on the drop path");
    }

    #[test]
    fn establish_timeout_fires_when_nothing_confirms() {
        // WS2 (outage §5.3): a session that never confirms a
        // subscription dies at the establishment budget — NOT gated
        // on Steady (here the driver never even upgrades).
        let mut t = TestTransport::with_capacity(4096);
        let mut d = Driver::new(1, test_symbols(), false);
        d.set_establish_budget_ns(50_000_000); // 50 ms test budget
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let mut ka = generous_keepalive();

        let res = run(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
        );
        assert_eq!(res, RunResult::EstablishTimeout);
        let snap = status.take_last_err();
        assert_eq!(snap.site, core_metrics::ERR_SITE_ESTABLISH);
    }

    #[test]
    fn establish_timeout_disarmed_by_confirmed_subscribe() {
        // Once the subscribe result registers channels, the tiny 1 ns
        // budget here would otherwise trip on the first iteration —
        // instead the session lives on to the ordinary idle timeout.
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        d.set_establish_budget_ns(1);
        record_pending(&mut d, 2, DeribitReqKind::SubscribeAll).unwrap();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let mut ka = Keepalive::new(KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: 150_000_000,
        });

        let result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","quote.ETH-PERPETUAL","ticker.ETH-PERPETUAL.100ms","trades.ETH-PERPETUAL.100ms"],"testnet":false}"#;
        inject_text(&mut t, result);

        let res = run(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
        );
        assert_eq!(
            res,
            RunResult::IdleTimeout,
            "confirmed subscribe disarms the establishment budget"
        );
        assert_eq!(d.sub_count(), 6);
    }

    #[test]
    fn ticker_push_counts_as_slow_message() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let ticker = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-PERPETUAL.100ms","data":{"timestamp":1550652954406,"open_interest":18918470,"min_price":3943.21,"max_price":3982.84,"mark_price":3940.06,"index_price":3931.73,"current_funding":0.00042}}}"#;
        inject_text(&mut t, ticker);
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
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.parse_errors_total(), 0);
        assert!(
            cons.try_pop().is_none(),
            "tickers do not enter the Tick lane"
        );
    }

    #[test]
    fn perp_ticker_emits_funding_event_dated_does_not() {
        // WS3 (gaps §1): `current_funding_1e9` was parsed since 8c
        // and dropped at the capture site. A perp ticker now emits a
        // paired Funding event (v0 = rate ×1e9, v1 = 0); a DATED
        // future's ticker (no current_funding on the wire) emits the
        // Ticker event only — and PARSES, where pre-WS3 it was
        // rejected wholesale.
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = EventRecCap::default();

        let perp = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-PERPETUAL.100ms","data":{"timestamp":1550652954406,"open_interest":18918470,"min_price":3943.21,"max_price":3982.84,"mark_price":3940.06,"index_price":3931.73,"current_funding":-0.000375}}}"#;
        // ETH-PERPETUAL row reused as the dated stand-in: the gate is
        // the WIRE (field presence), not the instrument name.
        let dated = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.ETH-PERPETUAL.100ms","data":{"timestamp":1550652954500,"open_interest":1234,"min_price":64000.0,"max_price":66000.0,"mark_price":65100.5,"index_price":65099.0}}}"#;
        inject_text(&mut t, perp);
        inject_text(&mut t, dated);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();

        assert_eq!(status.parse_errors_total(), 0, "dated ticker must parse");
        assert_eq!(status.msgs_total(), 2);
        let funding: Vec<_> = cap
            .events
            .iter()
            .filter(|e| e.channel == ChannelId::Funding as u8)
            .collect();
        assert_eq!(funding.len(), 1, "funding only from the perp ticker");
        assert_eq!(funding[0].sym, SYM_BTC);
        assert_eq!(funding[0].v0, -375_000, "rate ×1e9");
        assert_eq!(funding[0].v1, 0, "no next-funding time on this venue");
        let tickers = cap
            .events
            .iter()
            .filter(|e| e.channel == ChannelId::Ticker as u8)
            .count();
        assert_eq!(tickers, 2, "both tickers still captured as Ticker events");
    }

    #[test]
    fn dvol_push_captures_vol_index_event() {
        // WS6: DVOL — venue-global capture series, ordinal identity.
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new_with_dvol(7, test_symbols(), false, &[b"btc_usd", b"eth_usd"]);
        d.set_state(State::Steady);
        d.session_started = true;
        d.next_req_id = 3;
        d.subscribe_req_id = 2;
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = EventRecCap::default();

        let push = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"deribit_volatility_index.eth_usd","data":{"timestamp":1619777946007,"volatility":72.5,"index_name":"eth_usd"}}}"#;
        inject_text(&mut t, push);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        let ev = cap
            .events
            .iter()
            .find(|e| e.channel == ChannelId::VolIndex as u8)
            .expect("VolIndex event captured");
        assert_eq!(ev.sym, SYMBOL_ID_NONE, "venue-global series");
        assert_eq!(ev.v0, 72_500_000_000, "points ×1e9");
        assert_eq!(ev.v1, 1, "ordinal of eth_usd in the configured list");
        assert_eq!(ev.venue_time_ms, 1_619_777_946_007);
        assert_eq!(status.parse_errors_total(), 0);
        assert_eq!(status.ticks_total(), 1, "market-data row counted");

        // A push for an index we never configured is counted noise.
        let foreign = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"deribit_volatility_index.sol_usd","data":{"timestamp":1,"volatility":50.0,"index_name":"sol_usd"}}}"#;
        inject_text(&mut t, foreign);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(status.parse_errors_total(), 1);
    }

    #[test]
    fn spot_row_verification_and_registration_skip_ticker() {
        // WS6: a spot static row (no `-` in the name) wants
        // quote + trades only — the subscribe echo without a spot
        // ticker passes FULL verification and arms the WS2
        // discriminator.
        let mut symbols = DeribitSymbolTable::new();
        symbols.insert(b"BTC-PERPETUAL", SYM_BTC).unwrap();
        symbols.insert(b"BTC_USDC", (3 << 24) | 2).unwrap();
        let mut t = TestTransport::with_capacity(16384);
        let mut d = Driver::new(7, symbols, false);
        d.set_state(State::Steady);
        d.session_started = true;
        d.next_req_id = 3;
        d.subscribe_req_id = 2;
        record_pending(&mut d, 2, DeribitReqKind::SubscribeAll).unwrap();
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","quote.BTC_USDC","trades.BTC_USDC.100ms"],"testnet":false}"#;
        inject_text(&mut t, result);
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .expect("spot echo without ticker is a FULL verification");
        assert!(
            d.subs_ever_confirmed,
            "full verification armed the discriminator"
        );
        assert_eq!(d.sub_count(), 5, "3 perp + 2 spot channels registered");
        assert_eq!(status.sub_drops_total(), 0);
        // Spot quote flows as a Tick like any static row.
        let quote = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC_USDC","data":{"timestamp":1550658624149,"instrument_name":"BTC_USDC","best_bid_price":64000.5,"best_bid_amount":1.0,"best_ask_price":64001.0,"best_ask_amount":2.0}}}"#;
        inject_text(&mut t, quote);
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
        let tick = cons.try_pop().expect("spot BBO rings");
        assert_eq!(tick.sym, (3 << 24) | 2);
    }

    #[test]
    fn combo_row_quote_only_verification_and_flow() {
        // WS6: combo rows are quote-only tail rows — one folded mask
        // bit, echo of the quote alone fully verifies, and the combo
        // BBO captures as a Tick.
        let mut symbols = test_symbols();
        symbols
            .insert_combo(b"BTC-FS-27MAR26_PERP", (3 << 24) | 1025)
            .unwrap();
        let mut t = TestTransport::with_capacity(16384);
        let mut d = Driver::new(7, symbols, false);
        d.set_state(State::Steady);
        d.session_started = true;
        d.next_req_id = 3;
        d.subscribe_req_id = 2;
        record_pending(&mut d, 2, DeribitReqKind::SubscribeAll).unwrap();
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        let result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","quote.ETH-PERPETUAL","ticker.ETH-PERPETUAL.100ms","trades.ETH-PERPETUAL.100ms","quote.BTC-FS-27MAR26_PERP"],"testnet":false}"#;
        inject_text(&mut t, result);
        drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut NullCapture,
        )
        .expect("combo quote echo fully verifies");
        assert!(d.subs_ever_confirmed);
        assert_eq!(d.sub_count(), 7, "6 static channels + 1 combo quote");
        let quote = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-FS-27MAR26_PERP","data":{"timestamp":1550658624149,"instrument_name":"BTC-FS-27MAR26_PERP","best_bid_price":-25.5,"best_bid_amount":10.0,"best_ask_price":-24.0,"best_ask_amount":5.0}}}"#;
        inject_text(&mut t, quote);
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
        let tick = cons.try_pop().expect("combo BBO rings");
        assert_eq!(tick.sym, (3 << 24) | 1025);
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
        assert_eq!(
            d.pending_count(),
            pending_before + 2,
            "unsub + sub in flight"
        );
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
                self.gaps
                    .push((e.channel, e.sym, e.venue_seq, e.venue_time_ms, e.v0, e.v1));
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
        assert_eq!(
            status.gaps_total(),
            1,
            "52 → 53 chains off the adopted tail"
        );
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
        assert_eq!(
            status.resubscribes_total(),
            0,
            "trades are never resubscribed"
        );
        assert_eq!(status.parse_errors_total(), 0);
        // §6.6 pairing: both increments carry TradeGap events.
        assert_eq!(cap.gaps.len(), 2, "1:1 event pairing");
        assert_eq!(
            (cap.gaps[0].4, cap.gaps[0].5),
            (51, 52),
            "jump: expected 51, observed 52"
        );
        assert_eq!(
            (cap.gaps[1].4, cap.gaps[1].5),
            (53, 40),
            "regression: expected 53, observed 40"
        );
    }

    #[test]
    fn trades_multi_row_push_counts_each_row_sequentially() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        let multi = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":50,"trade_id":"1","timestamp":1000,"price":1.0,"direction":"buy","amount":1.0},{"trade_seq":51,"trade_id":"2","timestamp":1001,"price":1.1,"direction":"sell","amount":2.0}]}}"#;
        inject_text(&mut t, multi);

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

    /// WS10-A: the perp ticker's Funding event reaches the venue-event
    /// lane — and ONLY the Funding one (the mask has no Ticker bit, so
    /// the Ticker capture event stays capture-only). Mask-0 and
    /// ring-full behavior are pinned crate-uniformly in ingress-okx.
    #[test]
    fn funding_event_reaches_the_event_lane_ticker_stays_capture_only() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let (mut etx, mut erx) = event_ring_pair();
        let mut cap = CountingCapture::default();

        let ticker = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-PERPETUAL.100ms","data":{"timestamp":1550652954406,"open_interest":18918470,"min_price":3943.21,"max_price":3982.84,"mark_price":3940.06,"index_price":3931.73,"current_funding":0.00042}}}"#;
        inject_text(&mut t, ticker);
        super::drive_one(
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut depth_ring_pair().0,
            &mut opt_ring_pair().0,
            &status,
            &mut cap,
        )
        .unwrap();

        assert_eq!(cap.events, 2, "capture: Ticker + Funding");
        let ev = erx.try_pop().expect("funding event on the lane");
        assert_eq!(ev.channel, core_types::ChannelId::Funding as u8);
        assert_eq!(ev.v0, 420_000, "0.00042 ×1e9");
        assert_eq!(ev.v1, 0, "no funding_8h in this frame ⇒ v1 = 0 (VM2 V2)");
        assert!(
            erx.try_pop().is_none(),
            "Ticker event is NOT on the lane (mask gates per channel)"
        );
        assert_eq!(status.event_ring_drops_total(), 0);
    }

    #[test]
    fn capture_hooks_fire_at_documented_sites() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver(false);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let mut cap = CountingCapture::default();

        // A perp ticker alone first — pins the Ticker event channel
        // PLUS the WS3 funding emit (v0 = rate ×1e9, gated on the
        // wire carrying `current_funding`).
        let ticker = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-PERPETUAL.100ms","data":{"timestamp":1550652954406,"open_interest":18918470,"min_price":3943.21,"max_price":3982.84,"mark_price":3940.06,"index_price":3931.73,"current_funding":0.00042}}}"#;
        inject_text(&mut t, ticker);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status, &mut cap).unwrap();
        assert_eq!(
            cap.events, 2,
            "perp ticker captures Ticker + Funding events"
        );
        assert_eq!(cap.last_event_channel, core_types::ChannelId::Funding as u8);
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
        assert_eq!(cap.events, 3, "one good trade row captured");
        assert_eq!(cap.last_event_channel, core_types::ChannelId::Trade as u8);
        assert_eq!(
            cap.rejects, 2,
            "broken trade row + garbage frame both tapped as rejects"
        );
        assert_eq!(status.parse_errors_total(), 2);
        // Tick still captured when the ring is full: fill it, resend.
        while prod
            .try_push(Tick::new(
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

        let e = drive_one(
            &mut t,
            &mut d,
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
    fn reset_for_reconnect_clears_connection_state() {
        let mut d = steady_driver(true);
        record_pending(&mut d, 9, DeribitReqKind::Test).unwrap();
        d.subs
            .insert(
                sub_id_of(DeribitChannel::Quote, b"BTC-PERPETUAL"),
                DeribitSubKind::Quote,
            )
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
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
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
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
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
            &mut t,
            &mut d,
            b"h",
            b"/",
            &mut prod,
            &mut poll,
            &mut events,
            mio::Token(1),
            &stop,
            &status,
            &mut ka,
            &mut NullCapture,
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
