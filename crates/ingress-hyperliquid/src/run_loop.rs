//! # ingress-hyperliquid run-loop
//!
//! Hyperliquid public WS over TLS. Same opening-handshake shape as
//! every ingress; steady state is push-driven:
//!
//! ```text
//! Connecting ──► NeedsWsWrite ──► AwaitingWsUpgrade ──► Steady ──┐
//!      ▲                                                          │
//!      └──────────────── Closed / Err / Stale ────────────────────┘
//! ```
//!
//! On entry to `Steady` the driver queues **one subscribe frame per
//! configured subscription** — Hyperliquid has no batch form; N
//! frames sit far inside the 2000 client msgs/min budget. Every
//! subscription must be acknowledged by a `subscriptionResponse`
//! echo within [`HL_SUB_ACK_BUDGET_NS`]; the expected/found bitmask
//! is checked by [`session_health`] and a missed deadline fails the
//! session (**without** a debug assert — ack latency is a venue
//! timing condition, not a code invariant; contrast venue `error`
//! frames, which are misconfiguration and crash debug builds).
//!
//! Keepalive is the venue-specific `{"method":"ping"}` text frame
//! queued when `core_net::Keepalive` says so (50 s interval vs the
//! venue's 60 s idle cutoff); the `{"channel":"pong"}` answer counts
//! as activity like any frame.
//!
//! Integrity per §6.2: **staleness only** — stateless snapshots have
//! no chain. [`crate::HlStaleness`] is armed once all acks verify;
//! a coin whose `l2Book` venue time stops advancing within the
//! budget trips `gaps_total` and ends the session with
//! [`RunResult::Stale`] (reconnect; the next snapshot recovers all
//! state by construction).
//!
//! ## Oversize-frame guard
//!
//! `l2Book` snapshots are ≤ 20 levels/side (~2 KiB) — [`RX_BUF_SIZE`]
//! (256 KiB, OKX-sized) is two orders of magnitude of headroom. The
//! guard still applies: a frame that cannot fit the rx buffer never
//! completes — detected (buffer full + frame incomplete) and failed
//! rather than livelocked (fail-fast doctrine).
//!
//! Everything after the handshake is zero-alloc: parsers slice the
//! rx buffer in place; subscribe/ping payloads render into stack
//! scratch; the only copy is the 64-byte `Tick` moved into the ring.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_metrics::{IngressState, IngressStatus};
use core_net::{
    constant_time_eq, expected_accept, queue_masked_text_frame, read_server_handshake,
    sec_websocket_key_from_seed, write_client_handshake, ws_mask_from_counter, ws_read_frame,
    ws_unmask_in_place, ws_write_pong, HandshakeResult, IoBuf, Keepalive, KeepaliveAction,
    ReqKind, Status, SubErr, SubId, SubTable, Transport, WsOpcode, WsReadResult,
};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{Price, Qty, Tick, VenueId};

use crate::{
    bit_of, classify, coin_wants_asset_ctx, expected_mask, extract_coin, parse_active_asset_ctx,
    parse_all_mids, parse_bbo, parse_l2book_header, parse_outcome_meta, parse_sub_response,
    parse_trade, sub_id_of, write_subscribe, HlChannel, HlCoinTable, HlMsgKind, HlStaleness,
    MaskBits, ALL_MIDS_BIT, CHANNELS_PER_COIN, HL_MAX_COINS, OUTCOME_META_BIT, PING_PAYLOAD,
};

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// Rx buffer: an `l2Book` snapshot is ≤ 20 levels/side (~2 KiB);
/// 256 KiB (OKX-sized) absorbs bursts of snapshots across all
/// configured coins plus `allMids` sweeps. Boot-time allocation.
pub const RX_BUF_SIZE: usize = 256 * 1024;

/// Tx buffer: handshake + up to [`MAX_SUBS`] individually-framed
/// subscribes (~100 B masked each ≈ 7 KiB queued in one drive
/// cycle) + pings. 16 KiB gives 2× margin.
pub const TX_BUF_SIZE: usize = 16 * 1024;

/// Tick-ring capacity. Must equal `engine::TICK_RING_SIZE` — the cli
/// const-asserts the equality when wiring lanes (8a §3.3 pattern).
pub const TICK_RING_CAP: usize = 16_384;

/// Upper bound on subscriptions: 4 per-coin channels ×
/// [`HL_MAX_COINS`] + `allMids` + `outcomeMetaUpdates`.
pub const MAX_SUBS: usize = CHANNELS_PER_COIN * HL_MAX_COINS + 2;

/// Stack scratch for one rendered subscribe frame (longest:
/// `l2Book` + a [`crate::HL_COIN_MAX`]-byte coin ≈ 90 B).
const SUBSCRIBE_SCRATCH: usize = 160;

/// Ack deadline: every subscribe must be echoed within this budget
/// of entering `Steady` (acks normally arrive within one RTT; 5 s
/// tolerates a congested venue without masking a dead sub).
pub const HL_SUB_ACK_BUDGET_NS: u64 = 5_000_000_000;

// ---------------------------------------------------------------
// Subscription kinds for the core-net SubTable
// ---------------------------------------------------------------

/// Subscription-table tag (mirrors [`HlChannel`] + free sentinel).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HlSubKind {
    /// `bbo`.
    Bbo = 0,
    /// `l2Book`.
    L2Book = 1,
    /// `trades`.
    Trades = 2,
    /// `activeAssetCtx`.
    AssetCtx = 3,
    /// `allMids`.
    AllMids = 4,
    /// `outcomeMetaUpdates`.
    OutcomeMeta = 5,
    /// Slot free.
    None = 255,
}

impl ReqKind for HlSubKind {
    const FREE: Self = HlSubKind::None;
}

impl HlSubKind {
    #[inline]
    const fn from_channel(c: HlChannel) -> Self {
        match c {
            HlChannel::Bbo => HlSubKind::Bbo,
            HlChannel::L2Book => HlSubKind::L2Book,
            HlChannel::Trades => HlSubKind::Trades,
            HlChannel::ActiveAssetCtx => HlSubKind::AssetCtx,
            HlChannel::AllMids => HlSubKind::AllMids,
            HlChannel::OutcomeMetaUpdates => HlSubKind::OutcomeMeta,
        }
    }
}

/// Subscription-table capacity (= [`MAX_SUBS`]).
pub const SUB_CAP: usize = MAX_SUBS;

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
    /// A coin's `l2Book` venue time stopped advancing within the
    /// staleness budget (§6.2) — caller reconnects; the next
    /// snapshot recovers all state by construction. `gaps_total`
    /// was incremented (module doc).
    Stale,
    /// Fatal transport / protocol error (includes venue `error`
    /// frames and a missed ack deadline — fail-fast).
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

    /// Boot-built `coin → SymbolId` map (venue-namespaced ids).
    coins: HlCoinTable,
    /// Acknowledged subscriptions.
    subs: SubTable<HlSubKind, SUB_CAP>,
    /// Ack bits this session must collect ([`expected_mask`]).
    expected: MaskBits,
    /// Ack bits collected so far.
    found: MaskBits,
    /// Per-coin `l2Book` staleness monitor (armed on verification).
    staleness: HlStaleness,
    /// Ack deadline budget (ns from `Steady` entry).
    sub_ack_budget_ns: u64,
    /// `now_ns` at the upgrade→Steady edge (ack-deadline anchor).
    steady_since_ns: u64,
    /// Set once the post-upgrade subscribe frames have been queued.
    subscribed: bool,
    /// Set once `found == expected` (staleness armed at that edge).
    verified: bool,
    /// `!Sync` marker — see struct doc.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate buffers (boot-time) and seed the handshake nonce.
    /// `coins` maps every configured coin (HIP-4 `#<enc>` included);
    /// `staleness_budget_ns` is the §6.2 per-coin budget (default
    /// [`crate::HL_STALENESS_BUDGET_NS`]); `sub_ack_budget_ns` the
    /// ack deadline (default [`HL_SUB_ACK_BUDGET_NS`]).
    pub fn new(
        nonce_seed: u64,
        coins: HlCoinTable,
        staleness_budget_ns: u64,
        sub_ack_budget_ns: u64,
    ) -> Self {
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
            coins,
            subs: SubTable::new(),
            expected: 0,
            found: 0,
            staleness: HlStaleness::new(staleness_budget_ns),
            sub_ack_budget_ns,
            steady_since_ns: 0,
            subscribed: false,
            verified: false,
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

    /// Whether every expected subscription has been acknowledged.
    #[inline]
    pub fn is_verified(&self) -> bool {
        self.verified
    }

    /// Reset per-connection state for a reconnect. Subscriptions,
    /// ack masks and the staleness monitor are connection-scoped.
    pub fn reset_for_reconnect(&mut self, nonce_seed: u64) {
        self.state = State::Connecting;
        self.rx.clear();
        self.tx.clear();
        self.sec_key = sec_websocket_key_from_seed(nonce_seed);
        self.expected_accept_val = expected_accept(&self.sec_key);
        self.last_activity_ns = 0;
        self.mask_counter = 0;
        self.subs.clear();
        self.expected = 0;
        self.found = 0;
        self.staleness.disarm();
        self.steady_since_ns = 0;
        self.subscribed = false;
        self.verified = false;
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
pub fn drive_one<T: Transport>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
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
            drain_ws_frames(drv, producer, status)?;
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
            drv.steady_since_ns = now;
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
// Subscribe queueing (one frame per subscription — venue protocol)
// ---------------------------------------------------------------

#[inline]
fn queue_one_subscribe(
    drv: &mut Driver,
    channel: HlChannel,
    coin: Option<&[u8]>,
) -> io::Result<()> {
    let mut scratch = [0u8; SUBSCRIBE_SCRATCH];
    let n = write_subscribe(&mut scratch, channel, coin)
        .ok_or_else(|| io::Error::other("hl: subscribe scratch too small"))?;
    queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, &scratch[..n])
}

/// Queue one subscribe frame per configured subscription and set the
/// expected-ack mask. Per-coin: `bbo` + `l2Book` + `trades`
/// (+ `activeAssetCtx` for perp coins); global: `allMids` +
/// `outcomeMetaUpdates`.
fn queue_subscribe_all(drv: &mut Driver) -> io::Result<()> {
    debug_assert!(!drv.subscribed, "subscribe frames must be queued exactly once");
    if drv.coins.is_empty() {
        return Err(io::Error::other("hl: no coins configured"));
    }
    let mut i = 0;
    while i < drv.coins.len() {
        // Row `i` exists — bounded by the loop condition.
        let (coin_bytes, _sym) = match drv.coins.get(i) {
            Some(row) => row,
            None => break,
        };
        // Borrow discipline: copy the coin into stack scratch so the
        // `&drv.coins` borrow ends before `&mut drv.tx` is taken.
        let mut coin_buf = [0u8; crate::HL_COIN_MAX];
        let coin_len = coin_bytes.len();
        coin_buf[..coin_len].copy_from_slice(coin_bytes);
        let coin = &coin_buf[..coin_len];
        let wants_ctx = coin_wants_asset_ctx(coin);

        queue_one_subscribe(drv, HlChannel::Bbo, Some(coin))?;
        queue_one_subscribe(drv, HlChannel::L2Book, Some(coin))?;
        queue_one_subscribe(drv, HlChannel::Trades, Some(coin))?;
        if wants_ctx {
            queue_one_subscribe(drv, HlChannel::ActiveAssetCtx, Some(coin))?;
        }
        i += 1;
    }
    queue_one_subscribe(drv, HlChannel::AllMids, None)?;
    queue_one_subscribe(drv, HlChannel::OutcomeMetaUpdates, None)?;
    drv.expected = expected_mask(&drv.coins);
    drv.found = 0;
    drv.subscribed = true;
    Ok(())
}

// ---------------------------------------------------------------
// Session health — ack deadline + staleness (called from `run`)
// ---------------------------------------------------------------

/// Post-drain health check for a `Steady` session.
///
/// * Ack verification: once `found == expected`, marks the session
///   verified and arms the staleness monitor. A session still
///   unverified past the ack budget returns
///   `Some(`[`RunResult::Error`]`)` (fail-fast; no debug assert —
///   module doc).
/// * Staleness: a stale coin increments `gaps_total` and returns
///   `Some(`[`RunResult::Stale`]`)`.
///
/// Returns `None` while the session is healthy.
pub fn session_health(drv: &mut Driver, status: &IngressStatus, now_ns: u64) -> Option<RunResult> {
    if drv.state != State::Steady || !drv.subscribed {
        return None;
    }
    if !drv.verified {
        if drv.found == drv.expected {
            drv.verified = true;
            drv.staleness.arm(now_ns, drv.coins.len());
        } else if now_ns.saturating_sub(drv.steady_since_ns) > drv.sub_ack_budget_ns {
            return Some(RunResult::Error);
        }
        return None;
    }
    if drv.staleness.first_stale(now_ns).is_some() {
        // §6.2: staleness counts into gaps_total (no dedicated
        // counter — crate doc), and the session reconnects.
        status.inc_gaps();
        return Some(RunResult::Stale);
    }
    None
}

// ---------------------------------------------------------------
// Frame drain + dispatch
// ---------------------------------------------------------------

/// Per-push `trades` scan result (phase-1 output; `Copy`).
#[derive(Copy, Clone)]
struct TradeScan {
    rows_parsed: u32,
    rows_rejected: u32,
}

/// Phase-1 dispatch outcome — everything pre-parsed while the rx
/// borrow is live, applied after it ends (template pattern).
#[derive(Copy, Clone)]
enum Dispatch {
    /// Unparseable / unclassifiable — one rejection.
    Nothing,
    /// Keepalive pong — activity only.
    Quiet,
    /// Subscribe ack for one subscription.
    SubAck {
        id: SubId,
        kind: HlSubKind,
        bit: MaskBits,
    },
    /// Venue `error` frame — fatal (fail-fast).
    VenueError,
    /// `bbo` push became a Tick.
    Bbo { tick: Tick },
    /// `l2Book` snapshot header (staleness food).
    L2Book { coin_idx: u8, venue_ts_ns: u64 },
    /// `trades` push scanned.
    Trades { scan: TradeScan },
    /// `activeAssetCtx` / `allMids` / `outcomeMetaUpdates` push
    /// validated (slow-lane capture).
    Slow,
}

fn drain_ws_frames(
    drv: &mut Driver,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
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
                        handle_data_frame(drv, payload.start..payload.end, producer, status)?;
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
                        // Hyperliquid does not fragment public pushes;
                        // drop rather than allocate a reassembly buffer.
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
/// `"coin":"` markers; each slice parses independently (`side`/`px`/
/// `sz`/`time`/`tid` all follow `coin` within a row).
fn scan_trades(payload: &[u8], sym: u32) -> TradeScan {
    const MARKER: &[u8] = b"\"coin\":\"";
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
        match parse_trade(&payload[row_start..row_end], sym) {
            Some(_t) => scan.rows_parsed += 1,
            None => scan.rows_rejected += 1,
        }
        at = row_end;
    }
    scan
}

/// Ack-mask bit for one parsed `subscriptionResponse`, or `None`
/// when the echoed coin is not in the table (venue noise /
/// misconfiguration — counted as a rejection).
#[inline]
fn ack_bit(coins: &HlCoinTable, channel: HlChannel, coin: Option<&[u8]>) -> Option<MaskBits> {
    match channel {
        HlChannel::AllMids => Some(ALL_MIDS_BIT),
        HlChannel::OutcomeMetaUpdates => Some(OUTCOME_META_BIT),
        _ => {
            let idx = coins.index_of_coin(coin?)?;
            Some(bit_of(idx, channel))
        }
    }
}

fn handle_data_frame(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, TICK_RING_CAP>,
    status: &IngressStatus,
) -> io::Result<()> {
    // Phase 1: immutable borrow of rx (+ coins) — classify, resolve
    // the coin, pre-parse into a Copy dispatch value.
    let dispatch: Dispatch = {
        let payload = &drv.rx.filled()[payload_range];
        match classify(payload) {
            HlMsgKind::Pong => Dispatch::Quiet,
            HlMsgKind::Error => Dispatch::VenueError,
            HlMsgKind::SubResponse => match parse_sub_response(payload) {
                Some((channel, coin)) => match ack_bit(&drv.coins, channel, coin) {
                    Some(bit) => Dispatch::SubAck {
                        id: sub_id_of(channel, coin.unwrap_or(b"")),
                        kind: HlSubKind::from_channel(channel),
                        bit,
                    },
                    None => Dispatch::Nothing,
                },
                None => Dispatch::Nothing,
            },
            HlMsgKind::Data(channel) => match channel {
                // Global channels resolve no coin.
                HlChannel::AllMids => match parse_all_mids(payload) {
                    Some(_n) => Dispatch::Slow,
                    None => Dispatch::Nothing,
                },
                HlChannel::OutcomeMetaUpdates => match parse_outcome_meta(payload) {
                    Some(_f) => Dispatch::Slow,
                    None => Dispatch::Nothing,
                },
                _ => {
                    match extract_coin(payload).and_then(|coin| {
                        drv.coins
                            .lookup(coin)
                            .map(|sym| (sym, drv.coins.index_of(sym)))
                    }) {
                        Some((sym, Some(coin_idx))) => match channel {
                            HlChannel::Bbo => match parse_bbo(payload, sym) {
                                Some(f) => Dispatch::Bbo {
                                    // venue_seq = time (ms) as u32 —
                                    // crate-header policy.
                                    tick: Tick::new(
                                        now_ns(),
                                        VenueId::Hyperliquid,
                                        sym,
                                        (f.ts_ns / 1_000_000) as u32,
                                        Price::from_raw(f.bid_px_1e6),
                                        Qty::from_raw(f.bid_qty_1e6),
                                        Price::from_raw(f.ask_px_1e6),
                                        Qty::from_raw(f.ask_qty_1e6),
                                    ),
                                },
                                None => Dispatch::Nothing,
                            },
                            HlChannel::L2Book => match parse_l2book_header(payload, sym) {
                                Some(f) => Dispatch::L2Book {
                                    coin_idx: coin_idx as u8,
                                    venue_ts_ns: f.ts_ns,
                                },
                                None => Dispatch::Nothing,
                            },
                            HlChannel::Trades => Dispatch::Trades {
                                scan: scan_trades(payload, sym),
                            },
                            HlChannel::ActiveAssetCtx => {
                                match parse_active_asset_ctx(payload, sym) {
                                    Some(_f) => Dispatch::Slow,
                                    None => Dispatch::Nothing,
                                }
                            }
                            // Handled above.
                            HlChannel::AllMids | HlChannel::OutcomeMetaUpdates => {
                                Dispatch::Nothing
                            }
                        },
                        // Data for a coin we never configured — a
                        // mapping bug or venue noise; count it.
                        _ => Dispatch::Nothing,
                    }
                }
            },
            HlMsgKind::Unknown => Dispatch::Nothing,
        }
    };

    // Phase 2: mutable applies. The rx borrow above has ended.
    match dispatch {
        Dispatch::Nothing => status.inc_parse_errors(),
        Dispatch::Quiet => {}
        Dispatch::SubAck { id, kind, bit } => {
            status.add_msgs(1);
            drv.found |= bit;
            match drv.subs.insert(id, kind) {
                Ok(()) => {}
                Err(SubErr::ReservedId) => {}
                Err(SubErr::Full) => {
                    debug_assert!(false, "hl sub table full at SUB_CAP={SUB_CAP}");
                }
            }
        }
        Dispatch::VenueError => {
            // Fail-fast doctrine: a venue error frame means our
            // subscribe (or framing) is wrong. Crash loudly in debug,
            // surface a session error in release — the reconnect
            // path applies backoff and the operator sees it.
            debug_assert!(false, "hl venue error frame");
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "hl venue error frame",
            ));
        }
        Dispatch::Bbo { tick } => {
            status.add_msgs(1);
            // D4: a full ring is data loss — count it, never block.
            if producer.try_push(tick).is_err() {
                status.inc_ring_drops();
            }
        }
        Dispatch::L2Book {
            coin_idx,
            venue_ts_ns,
        } => {
            status.add_msgs(1);
            drv.staleness
                .on_l2book(coin_idx as usize, venue_ts_ns, now_ns());
        }
        Dispatch::Trades { scan } => {
            status.add_msgs(scan.rows_parsed as u64);
            let mut r = 0;
            while r < scan.rows_rejected {
                status.inc_parse_errors();
                r += 1;
            }
        }
        Dispatch::Slow => status.add_msgs(1),
    }
    Ok(())
}

// ---------------------------------------------------------------
// Top-level driver — mio-driven loop
// ---------------------------------------------------------------

/// Stop flag raised by external threads for graceful shutdown.
pub type StopFlag = AtomicBool;

/// Run the Hyperliquid ingress loop until `stop` is set or the
/// session ends. Reconnect is the caller's responsibility.
///
/// `keepalive` drives the venue-specific probe: on `SendPing` the
/// `{"method":"ping"}` text frame is queued and flushed (venue cuts
/// idle connections at 60 s; cli configures a 50 s interval); on
/// `Reconnect` the session is dead by policy →
/// [`RunResult::IdleTimeout`]. [`session_health`] then enforces the
/// ack deadline and the §6.2 staleness budget.
#[allow(clippy::too_many_arguments)]
pub fn run<T: Transport>(
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
) -> RunResult {
    let session_start_ns = now_ns();
    keepalive.reset();

    if transport.register(poll.registry(), token).is_err() {
        return RunResult::Error;
    }
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
            let transport_status = match transport.pump(ev) {
                Ok(s) => s,
                Err(_e) => return RunResult::Error,
            };
            note_transport_ready(drv, transport_status);
        }

        // Tight inner drain (see ingress-polymarket for rationale).
        loop {
            let n_before = producer.len();
            let state_before = drv.state();
            if drive_one(transport, drv, host, path, producer, status).is_err() {
                return RunResult::Error;
            }
            if drv.state() == State::Closed {
                return RunResult::Disconnected;
            }
            if producer.len() == n_before && drv.state() == state_before {
                break;
            }
        }

        if drv.state() == State::Steady {
            let now = now_ns();
            // Keepalive: JSON ping; the venue's pong (or any push)
            // refreshes the activity clock.
            let last = if drv.last_activity_ns == 0 {
                session_start_ns
            } else {
                drv.last_activity_ns
            };
            match keepalive.poll(now, last) {
                KeepaliveAction::None => {}
                KeepaliveAction::SendPing => {
                    if queue_masked_text_frame(&mut drv.tx, &mut drv.mask_counter, PING_PAYLOAD)
                        .is_err()
                        || flush_tx(transport, drv).is_err()
                    {
                        return RunResult::Error;
                    }
                    keepalive.mark_ping_sent(now);
                }
                KeepaliveAction::Reconnect => return RunResult::IdleTimeout,
            }
            // Ack deadline + per-coin staleness.
            if let Some(r) = session_health(drv, status, now) {
                return r;
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

    /// Venue-namespaced test symbols (venue byte 4 = Hyperliquid).
    const SYM_BTC: u32 = (4 << 24) | 1;
    const SYM_HIP4: u32 = (4 << 24) | 2;

    fn test_coins() -> HlCoinTable {
        let mut t = HlCoinTable::new();
        t.insert(b"BTC", SYM_BTC).unwrap();
        t.insert(b"#330", SYM_HIP4).unwrap();
        t
    }

    fn new_driver() -> Driver {
        Driver::new(
            7,
            test_coins(),
            crate::HL_STALENESS_BUDGET_NS,
            HL_SUB_ACK_BUDGET_NS,
        )
    }

    fn steady_driver() -> Driver {
        let mut d = new_driver();
        d.set_state(State::Steady);
        d.subscribed = true;
        d.expected = expected_mask(&d.coins);
        d.steady_since_ns = now_ns();
        d
    }

    /// Steady, all acks found, staleness armed — the post-verification
    /// shape a healthy session has.
    fn verified_driver() -> Driver {
        let mut d = steady_driver();
        d.found = d.expected;
        let status = IngressStatus::new();
        assert_eq!(session_health(&mut d, &status, now_ns()), None);
        assert!(d.is_verified());
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
        let d = new_driver();
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.sub_count(), 0);
        assert!(!d.subscribed);
        assert!(!d.is_verified());
    }

    #[test]
    fn note_transport_ready_advances_and_closes() {
        let mut d = new_driver();
        note_transport_ready(&mut d, Status::Ready);
        assert_eq!(d.state(), State::NeedsWsWrite);
        note_transport_ready(&mut d, Status::Closed);
        assert_eq!(d.state(), State::Closed);
    }

    #[test]
    fn handshake_completes_and_per_sub_frames_are_emitted() {
        let mut t = TestTransport::with_capacity(65536);
        let mut d = new_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"api.hyperliquid.xyz", b"/ws", &mut prod, &status).unwrap();
        let mut scratch = vec![0u8; 65536];
        let _ = t.drain_outgoing(&mut scratch);

        let key = sec_key_pub(7);
        let accept = expected_accept_pub(&key);
        let resp = build_server_response(&accept);
        t.inject_incoming(&resp);

        drive_one(&mut t, &mut d, b"api.hyperliquid.xyz", b"/ws", &mut prod, &status).unwrap();
        assert_eq!(d.state(), State::Steady);
        assert!(d.subscribed);
        assert_eq!(status.state(), IngressState::Up);
        assert_eq!(d.expected, expected_mask(&d.coins));

        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        // One frame per subscription (no batch form on this venue):
        // BTC (perp): bbo+l2Book+trades+ctx; #330 (outcome): 3 —
        // ctx gated off; + allMids + outcomeMetaUpdates = 9.
        assert_eq!(
            memchr::memmem::find_iter(&body, b"{\"method\":\"subscribe\"").count(),
            9
        );
        assert!(memchr::memmem::find(&body, br#"{"type":"bbo","coin":"BTC"}"#).is_some());
        assert!(memchr::memmem::find(&body, br##"{"type":"l2Book","coin":"#330"}"##).is_some());
        assert!(memchr::memmem::find(&body, br#"{"type":"activeAssetCtx","coin":"BTC"}"#).is_some());
        assert!(
            memchr::memmem::find(&body, br##"{"type":"activeAssetCtx","coin":"#330"}"##).is_none(),
            "outcome coins must not subscribe activeAssetCtx"
        );
        assert!(memchr::memmem::find(&body, br#"{"type":"allMids"}"#).is_some());
        assert!(memchr::memmem::find(&body, br#"{"type":"outcomeMetaUpdates"}"#).is_some());
    }

    #[test]
    fn sub_response_registers_and_sets_found_bit() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        inject_text(
            &mut t,
            br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}}"#,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(d.sub_count(), 1);
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(d.found, bit_of(0, HlChannel::Bbo));
        assert_eq!(
            d.subs.kind_of(sub_id_of(HlChannel::Bbo, b"BTC")),
            Some(HlSubKind::Bbo)
        );
    }

    #[test]
    fn all_acks_verify_and_arm_staleness() {
        let mut t = TestTransport::with_capacity(16384);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        // All 9 expected acks: 4×BTC, 3×#330, 2 global.
        for body in [
            &br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}}"#[..],
            &br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}}"#[..],
            &br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"trades","coin":"BTC"}}}"#[..],
            &br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"BTC"}}}"#[..],
            &br##"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"#330"}}}"##[..],
            &br##"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"l2Book","coin":"#330"}}}"##[..],
            &br##"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"trades","coin":"#330"}}}"##[..],
            &br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"allMids"}}}"#[..],
            &br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"outcomeMetaUpdates"}}}"#[..],
        ] {
            inject_text(&mut t, body);
        }
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(d.sub_count(), 9);
        assert_eq!(d.found, d.expected);

        assert_eq!(session_health(&mut d, &status, now_ns()), None);
        assert!(d.is_verified());
        assert!(d.staleness.is_armed());
    }

    #[test]
    fn ack_deadline_miss_returns_error() {
        let status = IngressStatus::new();
        let mut d = steady_driver();
        // One ack short of expected, past the deadline.
        let t0 = d.steady_since_ns;
        assert_eq!(session_health(&mut d, &status, t0 + 1), None, "inside budget");
        assert_eq!(
            session_health(&mut d, &status, t0 + HL_SUB_ACK_BUDGET_NS + 1),
            Some(RunResult::Error),
            "unverified past the ack budget fails the session"
        );
        assert!(!d.is_verified());
    }

    #[test]
    fn staleness_trips_gap_and_stale_result() {
        let status = IngressStatus::new();
        let mut d = verified_driver();
        let now = now_ns();
        // Both coins fresh…
        d.staleness.on_l2book(0, 1_000, now);
        d.staleness.on_l2book(1, 1_000, now);
        assert_eq!(session_health(&mut d, &status, now), None);
        assert_eq!(status.gaps_total(), 0);
        // …then the budget passes with no advancing snapshot.
        let later = now + crate::HL_STALENESS_BUDGET_NS + 1;
        assert_eq!(session_health(&mut d, &status, later), Some(RunResult::Stale));
        assert_eq!(status.gaps_total(), 1, "staleness counts into gaps_total");
    }

    #[test]
    fn bbo_push_emits_tick_with_venue_byte_and_time_ms_seq() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        inject_text(
            &mut t,
            br#"{"channel":"bbo","data":{"coin":"BTC","time":1708622398623,"bbo":[{"px":"64437.0","sz":"1.4491","n":2},{"px":"64438.0","sz":"0.541","n":3}]}}"#,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.ring_drops_total(), 0);

        let tick = cons.try_pop().expect("tick must be pushed");
        assert_eq!(tick.sym, SYM_BTC);
        assert_eq!(tick.venue, VenueId::Hyperliquid as u8);
        // venue_seq = time (ms) truncated to u32 (crate-header policy).
        assert_eq!(tick.venue_seq, 1_708_622_398_623u64 as u32);
        assert_eq!(tick.bid_px.raw(), 64_437_000_000);
        assert_eq!(tick.ask_px.raw(), 64_438_000_000);
        assert_eq!(tick.bid_qty.raw(), 1_449_100);
        assert_eq!(tick.ask_qty.raw(), 541_000);
        assert!(tick.ts_ns > 0);
    }

    #[test]
    fn hip4_coin_bbo_roundtrips_to_tick() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        inject_text(
            &mut t,
            br##"{"channel":"bbo","data":{"coin":"#330","time":1723600000001,"bbo":[{"px":"0.4","sz":"100.0","n":1},{"px":"0.6","sz":"50.0","n":1}]}}"##,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        let tick = cons.try_pop().expect("HIP-4 tick must flow the same path");
        assert_eq!(tick.sym, SYM_HIP4);
        assert_eq!(tick.venue, VenueId::Hyperliquid as u8);
        assert_eq!(tick.bid_px.raw(), 400_000);
        assert_eq!(tick.ask_px.raw(), 600_000);
    }

    #[test]
    fn l2book_push_feeds_staleness_monitor() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = verified_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        // Deterministic baselines: both coins armed exactly one
        // budget in the past, so an un-refreshed coin is stale at
        // `t0 + 2` while a drive-refreshed one is fresh.
        let t0 = now_ns();
        d.staleness = HlStaleness::new(crate::HL_STALENESS_BUDGET_NS);
        d.staleness
            .arm(t0.saturating_sub(crate::HL_STALENESS_BUDGET_NS), 2);

        inject_text(
            &mut t,
            br#"{"channel":"l2Book","data":{"coin":"BTC","time":1677700000000,"levels":[[{"px":"1.0","sz":"1.0","n":1}],[{"px":"2.0","sz":"1.0","n":1}]]}}"#,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.msgs_total(), 1);
        // Coin 0 was refreshed by the drive at ~t0; coin 1 (#330)
        // still sits on the aged baseline — it is the stale one.
        assert_eq!(d.staleness.first_stale(t0 + 2), Some(1));
    }

    #[test]
    fn trades_multi_row_push_counts_each_row() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        inject_text(
            &mut t,
            br#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":"1.0","sz":"1.0","hash":"0x1","time":1000,"tid":1},{"coin":"BTC","side":"A","px":"1.1","sz":"2.0","hash":"0x2","time":1001,"tid":2}]}"#,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.msgs_total(), 2, "both rows counted");
        assert_eq!(status.parse_errors_total(), 0);
    }

    #[test]
    fn slow_channels_validate_and_count() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        inject_text(
            &mut t,
            br#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"funding":"0.0000125","markPx":"1.0","oraclePx":"1.0","openInterest":"2.0"}}}"#,
        );
        inject_text(&mut t, br#"{"channel":"allMids","data":{"mids":{"BTC":"1.0"}}}"#);
        inject_text(
            &mut t,
            br##"{"channel":"outcomeMetaUpdates","data":[{"kind":"outcomeCreated","coin":"#330","time":1}]}"##,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.msgs_total(), 3);
        assert_eq!(status.parse_errors_total(), 0);
    }

    #[test]
    fn unknown_coin_counts_parse_error() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, mut cons) = ring_pair();

        inject_text(
            &mut t,
            br#"{"channel":"bbo","data":{"coin":"DOGE","time":1000,"bbo":[{"px":"1.0","sz":"1.0","n":1},{"px":"1.1","sz":"1.0","n":1}]}}"#,
        );
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.parse_errors_total(), 1);
        assert_eq!(status.msgs_total(), 0);
        assert!(cons.try_pop().is_none());
    }

    #[test]
    fn venue_error_frame_fails_the_session() {
        // debug builds crash on the debug_assert (fail-fast); the
        // release-path behaviour is a session error → reconnect.
        if cfg!(debug_assertions) {
            return;
        }
        let mut t = TestTransport::with_capacity(8192);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        inject_text(
            &mut t,
            br#"{"channel":"error","data":"Invalid subscription {\"type\":\"nope\"}"}"#,
        );
        let e = drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn pong_is_quiet_activity() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        inject_text(&mut t, br#"{"channel":"pong"}"#);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap();
        assert_eq!(status.msgs_total(), 0);
        assert_eq!(status.parse_errors_total(), 0);
        assert!(status.last_activity_ns() > 0, "pong refreshes the idle clock");
    }

    #[test]
    fn oversize_frame_fails_session_instead_of_livelock() {
        let mut t = TestTransport::with_capacity(RX_BUF_SIZE + 1024);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();

        // 64-bit-length header claiming a payload larger than the rx
        // buffer, then enough bytes to fill rx completely.
        let huge = (RX_BUF_SIZE as u64 + 4096).to_be_bytes();
        let mut frame = vec![0u8; RX_BUF_SIZE + 512];
        frame[0] = 0x81;
        frame[1] = 127;
        frame[2..10].copy_from_slice(&huge);
        t.inject_incoming(&frame);

        let e = drive_one(&mut t, &mut d, b"h", b"/", &mut prod, &status).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reset_for_reconnect_clears_connection_state() {
        let mut d = verified_driver();
        d.subs
            .insert(sub_id_of(HlChannel::Bbo, b"BTC"), HlSubKind::Bbo)
            .unwrap();
        d.reset_for_reconnect(9);
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.sub_count(), 0);
        assert!(!d.subscribed);
        assert!(!d.is_verified());
        assert_eq!(d.found, 0);
        assert_eq!(d.expected, 0);
        assert!(!d.staleness.is_armed());
    }

    #[test]
    fn run_returns_idle_timeout_on_dead_session() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver();
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
            mio::Token(1), &stop, &status, &mut ka,
        );
        assert_eq!(res, RunResult::IdleTimeout);
    }

    #[test]
    fn run_emits_json_ping_before_idle_timeout() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver();
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        // Ping due immediately; death after ~150 ms of silence.
        let mut ka = Keepalive::new(KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: 150_000_000,
        });

        let res = run(
            &mut t, &mut d, b"h", b"/", &mut prod, &mut poll, &mut events,
            mio::Token(1), &stop, &status, &mut ka,
        );
        assert_eq!(res, RunResult::IdleTimeout);
        let mut scratch = [0u8; 4096];
        let n = t.drain_outgoing(&mut scratch);
        let body = unmask_client_frames(&scratch[..n]);
        assert!(
            memchr::memmem::find(&body, b"{\"method\":\"ping\"}").is_some(),
            "JSON ping frame must have been sent"
        );
    }

    #[test]
    fn run_returns_stale_when_no_snapshots_advance() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = verified_driver();
        // Tiny staleness budget so the armed monitor trips fast.
        d.staleness = HlStaleness::new(1);
        d.staleness.arm(now_ns(), 2);
        let status = IngressStatus::new();
        let (mut prod, _cons) = ring_pair();
        let stop = StopFlag::new(false);
        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let mut ka = generous_keepalive();

        let res = run(
            &mut t, &mut d, b"h", b"/", &mut prod, &mut poll, &mut events,
            mio::Token(1), &stop, &status, &mut ka,
        );
        assert_eq!(res, RunResult::Stale);
        assert_eq!(status.gaps_total(), 1);
    }

    #[test]
    fn run_disconnects_on_server_close() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = steady_driver();
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
            mio::Token(1), &stop, &status, &mut ka,
        );
        assert_eq!(res, RunResult::Disconnected);
        assert_eq!(status.bytes_total(), 2);
    }
}
