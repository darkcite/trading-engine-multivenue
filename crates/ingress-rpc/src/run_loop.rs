//! # ingress-rpc run-loop
//!
//! Polygon JSON-RPC over WSS. Same opening-handshake shape as
//! `ingress-polymarket` and `ingress-binance`, but the steady-state
//! semantics are richer because the wire carries **both** request /
//! response pairs and subscription notifications.
//!
//! ## State machine
//!
//! ```text
//! Connecting ──► NeedsWsWrite ──► AwaitingWsUpgrade ──► Steady ──┐
//!      ▲                                                          │
//!      └──────────────── Closed / Err ────────────────────────────┘
//! ```
//!
//! ## Steady-state loop
//!
//! 1. On entry to [`State::Steady`] the driver writes exactly one
//!    `eth_subscribe("newHeads")` request and parks it in the pending
//!    map. The server's response carries the subscription id we then
//!    register in [`Driver::subs`] for later correlation.
//! 2. At a cadence of [`RPC_POLL_NS`] wall-clock nanoseconds the driver
//!    emits one `eth_blockNumber` request — a cheap liveness probe that
//!    also funnels a warm-path [`core_types::Signal`] into the ring.
//! 3. Every inbound text frame is classified with [`classify_rpc`]:
//!    * [`RpcFrameKind::Response`] — correlate by `id`, retire the
//!      pending slot, optionally harvest a subscription id.
//!    * [`RpcFrameKind::Subscription`] — match payload to a known
//!      `SubId`; if `newHeads`, emit a warm-class Signal.
//!    * [`RpcFrameKind::Error`] — surface the code to the operator
//!      and (if we can't correlate) drop the frame.
//!    * [`RpcFrameKind::Unknown`] — logged and dropped.
//!
//! Everything after the handshake is zero-alloc: rx/tx are owned by
//! `Driver`; the pending/sub tables are `[...; N]` arrays; the signal
//! ring is caller-owned and preallocated at engine boot.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_net::{
    constant_time_eq, expected_accept, read_server_handshake, sec_websocket_key_from_seed,
    write_client_handshake, ws_mask_from_counter, ws_read_frame, ws_unmask_in_place,
    ws_write_binary_frame, ws_write_pong, HandshakeResult, Status, Transport, WsOpcode,
    WsReadResult,
};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{LatencyClass, NsTs, Signal, SignalSource, SymbolId, SYMBOL_ID_NONE};

use crate::{
    classify_rpc, parse_block_number_result, parse_hex_u64, parse_new_head_notification,
    parse_rpc_error, write_request_eth_block_number, write_request_subscribe_new_heads, NewHead,
    RequestIds, RpcFrameKind,
};

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// Size of the rx byte buffer the run-loop owns. Polygon `newHeads`
/// frames are <2 KiB; we size to 64 KiB to absorb log pushes and bursts.
pub const RX_BUF_SIZE: usize = 64 * 1024;

/// Size of the tx byte buffer. Holds one handshake plus the largest
/// single JSON-RPC request we emit (<= 128 bytes).
pub const TX_BUF_SIZE: usize = 4 * 1024;

/// Signal-ring capacity default. Warm-class sources need far less
/// headroom than hot-class Tick rings.
pub const DEFAULT_SIGNAL_RING_CAP: usize = 1024;

/// Maximum number of in-flight JSON-RPC requests we track. Power of
/// two. The allocator never emits more than a couple per second so 64
/// is generous.
pub const PENDING_CAP: usize = 64;

/// Maximum number of active subscriptions. We currently register
/// exactly one (`newHeads`); 4 is the fixed-size ceiling.
pub const SUB_CAP: usize = 4;

/// Wall-clock cadence between `eth_blockNumber` liveness polls. Two
/// seconds keeps us under Polygon's 5 rps free-tier budget with
/// headroom for subscribe + initial handshake.
pub const RPC_POLL_NS: u64 = 2_000_000_000;

/// UTF-8 asset-id placeholder used when a newHeads notification doesn't
/// reference a specific symbol. Engine decides via `SymbolId == SYMBOL_ID_NONE`.
const RPC_CROSS_SYM: SymbolId = SYMBOL_ID_NONE;

// ---------------------------------------------------------------
// Pending-request map
// ---------------------------------------------------------------

/// Kind of request we fired — determines how the response is handled.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RpcKind {
    /// `eth_blockNumber` — payload is a single hex-encoded `u64`; we
    /// emit one warm Signal per response.
    BlockNumber = 0,
    /// `eth_subscribe("newHeads")` — payload is a subscription id we
    /// register in [`Driver::subs`].
    SubscribeNewHeads = 1,
    /// Slot is free.
    None = 255,
}

/// One pending-request slot. 24 bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Pending {
    /// Allocated JSON-RPC id. `0` means "slot free".
    pub id: u64,
    /// When the request left the wire (for metrics / staleness checks).
    pub created_at_ns: u64,
    /// Request shape.
    pub kind: RpcKind,
    _pad: [u8; 7],
}

impl Pending {
    /// Empty slot sentinel — all zeros, kind = None.
    #[inline(always)]
    pub const fn empty() -> Self {
        Self {
            id: 0,
            created_at_ns: 0,
            kind: RpcKind::None,
            _pad: [0; 7],
        }
    }

    /// Whether the slot is currently in use.
    #[inline(always)]
    pub const fn is_used(&self) -> bool {
        !matches!(self.kind, RpcKind::None)
    }
}

// ---------------------------------------------------------------
// Subscription table
// ---------------------------------------------------------------

/// What a subscription id streams to us.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SubKind {
    /// `newHeads`.
    NewHeads = 0,
    /// Slot free.
    None = 255,
}

/// Zero-copy subscription id. Polygon subscription ids are `0x`-prefixed
/// 16-hex-digit lowercase strings (`0x%016x`). We store them as raw u64
/// for O(1) compare.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct SubId(pub u64);

impl SubId {
    /// Sentinel "unused".
    pub const NONE: SubId = SubId(0);
}

// ---------------------------------------------------------------
// IoBuf — same cursor-draining pattern as the other ingress crates
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
    /// Upgraded — JSON-RPC frames flow in both directions.
    Steady,
    /// Peer closed.
    Closed,
}

/// How a run-loop iteration terminated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// External stop flag observed.
    Stopped,
    /// Peer closed the connection.
    Disconnected,
    /// Fatal transport error.
    Error,
}

// ---------------------------------------------------------------
// Driver
// ---------------------------------------------------------------

/// Mutable per-connection state owned by the run-loop. Preallocated at
/// construction; never reallocates in steady state.
///
/// **Single-writer invariant.** The pending-request table is
/// indexed by `id & (PENDING_CAP - 1)` and assumes no two threads
/// ever call `drive_one` for the same `Driver` concurrently. The
/// `PhantomData<UnsafeCell<()>>` field below makes `Driver: !Sync`
/// so the compiler refuses any attempt to share it across threads.
/// `Driver: Send` is still allowed — the cli spawns one onto a
/// dedicated thread at boot.
pub struct Driver {
    state: State,
    rx: IoBuf,
    tx: IoBuf,
    sec_key: [u8; 24],
    expected_accept_val: [u8; 28],
    last_activity_ns: NsTs,
    mask_counter: u64,

    /// JSON-RPC id allocator. Monotonic; never wraps within a session.
    ids: RequestIds,
    /// Pending-request table, indexed by `id & (PENDING_CAP - 1)`.
    pending: [Pending; PENDING_CAP],
    /// Subscription table. `NONE`-padded.
    subs: [(SubId, SubKind); SUB_CAP],
    /// Wall-clock nanosecond deadline for the next liveness poll.
    next_poll_at_ns: u64,
    /// Have we issued the initial `eth_subscribe` for this session?
    subscribed: bool,
    /// `!Sync` marker — see struct-level doc.
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate rx/tx buffers and seed the opening-handshake nonce.
    pub fn new(nonce_seed: u64) -> Self {
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
            ids: RequestIds::new(),
            pending: [Pending::empty(); PENDING_CAP],
            subs: [(SubId::NONE, SubKind::None); SUB_CAP],
            next_poll_at_ns: 0,
            subscribed: false,
            _not_sync: ::core::marker::PhantomData,
        }
    }

    /// Current state (useful for metrics + tests).
    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    /// Force the state for tests. **Not** for production use.
    #[cfg(test)]
    pub(crate) fn set_state(&mut self, s: State) {
        self.state = s;
    }

    /// Suspend liveness polling in tests by pushing the next-poll deadline
    /// to `u64::MAX`. Production code lets `drive_one` seed this.
    #[cfg(test)]
    pub(crate) fn suppress_polling_for_test(&mut self) {
        self.next_poll_at_ns = u64::MAX;
    }

    /// Number of live subscription rows.
    #[inline]
    pub fn sub_count(&self) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < SUB_CAP {
            if !matches!(self.subs[i].1, SubKind::None) {
                n += 1;
            }
            i += 1;
        }
        n
    }

    /// Number of live pending-request rows.
    #[inline]
    pub fn pending_count(&self) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < PENDING_CAP {
            if self.pending[i].is_used() {
                n += 1;
            }
            i += 1;
        }
        n
    }

    /// Reset buffers + state for a reconnect. Cheap — only cursor bumps
    /// and array fills.
    pub fn reset_for_reconnect(&mut self, nonce_seed: u64) {
        self.state = State::Connecting;
        self.rx.clear();
        self.tx.clear();
        self.sec_key = sec_websocket_key_from_seed(nonce_seed);
        self.expected_accept_val = expected_accept(&self.sec_key);
        self.last_activity_ns = 0;
        self.mask_counter = 0;
        self.ids = RequestIds::new();
        let mut i = 0;
        while i < PENDING_CAP {
            self.pending[i] = Pending::empty();
            i += 1;
        }
        let mut j = 0;
        while j < SUB_CAP {
            self.subs[j] = (SubId::NONE, SubKind::None);
            j += 1;
        }
        self.next_poll_at_ns = 0;
        self.subscribed = false;
    }
}

// ---------------------------------------------------------------
// drive_one — single-tick state machine advance
// ---------------------------------------------------------------

/// Pump the transport once and advance the state machine. Zero-alloc
/// once the handshake has completed.
///
/// * `transport`: any [`Transport`].
/// * `drv`: per-connection driver state.
/// * `host`, `path`: sent verbatim into the `GET` + `Host:` line.
/// * `producer`: warm-class Signal ring producer. A full ring drops the
///   signal silently (back-pressure metric is emitted at the engine
///   layer).
///
/// # Errors
///
/// Any transport error is surfaced so the outer scheduler can close +
/// reconnect.
pub fn drive_one<T: Transport>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Signal, DEFAULT_SIGNAL_RING_CAP>,
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
            advance_ws_upgrade(drv)?;
            if drv.state == State::Steady {
                // First thing after upgrade: subscribe to newHeads.
                queue_subscribe_new_heads(drv)?;
                // Align the first liveness poll to now + RPC_POLL_NS.
                drv.next_poll_at_ns = now_ns().saturating_add(RPC_POLL_NS);
            }
        }
        State::Steady => {
            maybe_queue_block_number_poll(drv)?;
            drain_ws_frames(drv, producer)?;
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
// tx helpers
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

fn advance_ws_upgrade(drv: &mut Driver) -> io::Result<()> {
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
            drv.last_activity_ns = now_ns();
            Ok(())
        }
        HandshakeResult::Malformed => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed server handshake",
        )),
    }
}

// ---------------------------------------------------------------
// request queue helpers — write JSON-RPC payloads as masked binary
// WebSocket frames. The server happily accepts binary-framed JSON.
// ---------------------------------------------------------------

fn queue_subscribe_new_heads(drv: &mut Driver) -> io::Result<()> {
    debug_assert!(!drv.subscribed, "subscribe must only be fired once");
    let id = drv.ids.allocate();
    record_pending(drv, id, RpcKind::SubscribeNewHeads)?;
    let mut scratch = [0u8; 96];
    let n = write_request_subscribe_new_heads(&mut scratch, id)
        .map_err(|_| io::Error::other("subscribe request buffer too small"))?;
    queue_ws_binary_frame(drv, &scratch[..n])?;
    drv.subscribed = true;
    Ok(())
}

fn maybe_queue_block_number_poll(drv: &mut Driver) -> io::Result<()> {
    let now = now_ns();
    if now < drv.next_poll_at_ns {
        return Ok(());
    }
    drv.next_poll_at_ns = now.saturating_add(RPC_POLL_NS);
    let id = drv.ids.allocate();
    record_pending(drv, id, RpcKind::BlockNumber)?;
    let mut scratch = [0u8; 96];
    let n = write_request_eth_block_number(&mut scratch, id)
        .map_err(|_| io::Error::other("blockNumber request buffer too small"))?;
    queue_ws_binary_frame(drv, &scratch[..n])
}

fn record_pending(drv: &mut Driver, id: u64, kind: RpcKind) -> io::Result<()> {
    let slot = (id as usize) & (PENDING_CAP - 1);
    let entry = &mut drv.pending[slot];
    // Fail-fast: our allocator should never collide within PENDING_CAP.
    debug_assert!(
        !entry.is_used(),
        "pending slot {slot} still holds id {} — collision with id {id}",
        entry.id
    );
    if entry.is_used() {
        return Err(io::Error::other("pending-request slot collision"));
    }
    *entry = Pending {
        id,
        created_at_ns: now_ns(),
        kind,
        _pad: [0; 7],
    };
    Ok(())
}

#[inline]
fn take_pending(drv: &mut Driver, id: u64) -> Option<RpcKind> {
    let slot = (id as usize) & (PENDING_CAP - 1);
    let entry = &mut drv.pending[slot];
    if entry.is_used() && entry.id == id {
        let kind = entry.kind;
        *entry = Pending::empty();
        Some(kind)
    } else {
        None
    }
}

fn queue_ws_binary_frame(drv: &mut Driver, payload: &[u8]) -> io::Result<()> {
    let mask = ws_mask_from_counter(drv.mask_counter);
    drv.mask_counter = drv.mask_counter.wrapping_add(1);
    let dst = drv.tx.free_mut();
    let n = ws_write_binary_frame(dst, payload, mask)
        .map_err(|_| io::Error::other("ws binary frame buffer too small"))?;
    drv.tx.advance(n);
    Ok(())
}

// ---------------------------------------------------------------
// Frame drain + dispatch
// ---------------------------------------------------------------

fn drain_ws_frames(
    drv: &mut Driver,
    producer: &mut Producer<Signal, DEFAULT_SIGNAL_RING_CAP>,
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
                    WsOpcode::Text | WsOpcode::Binary => {
                        handle_json_frame(drv, payload.start..payload.end, producer);
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
                        // Fragmented RPC frames are not spec-legal in
                        // practice; drop rather than allocate a
                        // reassembly buffer.
                    }
                }

                drv.last_activity_ns = now_ns();
                drv.rx.consume(total);

                if drv.state == State::Closed {
                    return Ok(());
                }
            }
        }
    }
}

/// Dispatch outcome computed while the immutable borrow of `drv.rx` is
/// still live. Carries pre-parsed numeric results so the mutating half
/// can run without re-borrowing the payload.
#[derive(Copy, Clone)]
enum Dispatch {
    Nothing,
    EmitNewHead(NewHead),
    Response {
        id: u64,
        block: Option<u64>,
        sub_id: Option<SubId>,
    },
    Error {
        id: Option<u64>,
    },
}

fn handle_json_frame(
    drv: &mut Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Signal, DEFAULT_SIGNAL_RING_CAP>,
) {
    // Phase 1: immutable-borrow phase — classify and pre-parse
    // everything we might need. Zero-alloc scanners over `&[u8]`.
    let dispatch: Dispatch = {
        let payload = &drv.rx.filled()[payload_range];
        match classify_rpc(payload) {
            RpcFrameKind::Subscription => parse_new_head_notification(payload)
                .map(Dispatch::EmitNewHead)
                .unwrap_or(Dispatch::Nothing),
            RpcFrameKind::Response => match extract_id_decimal(payload) {
                Some((id, _)) => Dispatch::Response {
                    id,
                    block: parse_block_number_result(payload).map(|(_id, b)| b),
                    sub_id: extract_subscribe_result(payload),
                },
                None => Dispatch::Nothing,
            },
            RpcFrameKind::Error => {
                // Fail-fast in debug builds: every error indicates a
                // protocol bug we want to catch. In release, drop and
                // let the stop-loss layer decide.
                if let Some(e) = parse_rpc_error(payload) {
                    debug_assert!(
                        e.code >= -33000 && e.code <= 0,
                        "unexpected RPC error code {}",
                        e.code
                    );
                }
                Dispatch::Error {
                    id: extract_id_decimal(payload).map(|(id, _)| id),
                }
            }
            RpcFrameKind::Unknown => Dispatch::Nothing,
        }
    };

    // Phase 2: mutable-borrow phase — apply the dispatch result. The
    // immutable borrow above is already released.
    match dispatch {
        Dispatch::Nothing => {}
        Dispatch::EmitNewHead(head) => emit_new_head_signal(producer, head),
        Dispatch::Response { id, block, sub_id } => match take_pending(drv, id) {
            Some(RpcKind::BlockNumber) => {
                if let Some(b) = block {
                    emit_block_number_signal(producer, b);
                }
            }
            Some(RpcKind::SubscribeNewHeads) => {
                if let Some(sid) = sub_id {
                    register_subscription(drv, sid, SubKind::NewHeads);
                }
            }
            Some(RpcKind::None) | None => {
                // Unexpected / already retired; drop.
            }
        },
        Dispatch::Error { id } => {
            if let Some(id) = id {
                let _ = take_pending(drv, id);
            }
        }
    }
}

// ---------------------------------------------------------------
// Subscription table helpers
// ---------------------------------------------------------------

fn register_subscription(drv: &mut Driver, id: SubId, kind: SubKind) {
    let mut i = 0;
    while i < SUB_CAP {
        if matches!(drv.subs[i].1, SubKind::None) {
            drv.subs[i] = (id, kind);
            return;
        }
        i += 1;
    }
    // Table full: debug-assert and drop. Production code will see this
    // as "no sub tracking" — not fatal, but we want the test harness to
    // notice if we ever register more than SUB_CAP subscriptions.
    debug_assert!(false, "subscription table full at SUB_CAP={SUB_CAP}");
}

// ---------------------------------------------------------------
// Signal emission — zero-alloc pack helpers
// ---------------------------------------------------------------

/// Pack a [`NewHead`] into a 40-byte [`Signal::payload`] slot.
/// Layout:
/// - bytes 0..8   = number (LE u64)
/// - bytes 8..16  = timestamp_sec (LE u64)
/// - bytes 16..24 = gas_used (LE u64)
/// - bytes 24..40 = reserved, zero
#[inline]
fn pack_new_head_into_payload(head: NewHead) -> [u8; 40] {
    let mut out = [0u8; 40];
    out[0..8].copy_from_slice(&head.number.to_le_bytes());
    out[8..16].copy_from_slice(&head.ts_sec.to_le_bytes());
    out[16..24].copy_from_slice(&head.gas_used.to_le_bytes());
    out
}

/// Pack a raw block number into the 40-byte Signal payload.
/// Layout:
/// - bytes 0..8 = block number (LE u64)
/// - bytes 8..40 = zero
#[inline]
fn pack_block_number_into_payload(block: u64) -> [u8; 40] {
    let mut out = [0u8; 40];
    out[0..8].copy_from_slice(&block.to_le_bytes());
    out
}

fn emit_new_head_signal(
    producer: &mut Producer<Signal, DEFAULT_SIGNAL_RING_CAP>,
    head: NewHead,
) {
    let sig = Signal::new(
        now_ns(),
        RPC_CROSS_SYM,
        LatencyClass::Warm,
        SignalSource::Rpc as u8,
        pack_new_head_into_payload(head),
    );
    let _ = producer.try_push(sig);
}

fn emit_block_number_signal(
    producer: &mut Producer<Signal, DEFAULT_SIGNAL_RING_CAP>,
    block: u64,
) {
    let sig = Signal::new(
        now_ns(),
        RPC_CROSS_SYM,
        LatencyClass::Warm,
        SignalSource::Rpc as u8,
        pack_block_number_into_payload(block),
    );
    let _ = producer.try_push(sig);
}

// ---------------------------------------------------------------
// JSON-RPC scrap parsers — byte scanners that don't pay serde_json cost
// ---------------------------------------------------------------

/// Parse `"id":<decimal>` out of an RPC frame. Returns `(id, end_pos)`.
#[inline]
fn extract_id_decimal(buf: &[u8]) -> Option<(u64, usize)> {
    const MARKER: &[u8] = b"\"id\":";
    let start = memchr::memmem::find(buf, MARKER)? + MARKER.len();
    // Skip whitespace.
    let mut i = start;
    while i < buf.len() && (buf[i] == b' ' || buf[i] == b'\t') {
        i += 1;
    }
    let mut v: u64 = 0;
    let mut any = false;
    while i < buf.len() {
        let b = buf[i];
        if !b.is_ascii_digit() {
            break;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as u64)?;
        any = true;
        i += 1;
    }
    if any {
        Some((v, i))
    } else {
        None
    }
}

/// Extract the subscription id from an `eth_subscribe` response:
/// `{"jsonrpc":"2.0","id":1,"result":"0xab…"}`.
/// Returns the 64-bit value encoded after `0x`.
#[inline]
fn extract_subscribe_result(buf: &[u8]) -> Option<SubId> {
    const MARKER: &[u8] = b"\"result\":\"";
    let start = memchr::memmem::find(buf, MARKER)? + MARKER.len();
    let (v, _) = parse_hex_u64(buf, start)?;
    Some(SubId(v))
}

// ---------------------------------------------------------------
// Top-level driver — mio-driven loop
// ---------------------------------------------------------------

/// Stop flag that external threads can raise to signal a graceful
/// shutdown.
pub type StopFlag = AtomicBool;

/// Run the RPC ingress loop until `stop` is set or the transport fails.
/// Reconnect is the caller's responsibility — this function returns
/// [`RunResult::Disconnected`] so the outer scheduler can sleep + retry.
pub fn run<T: Transport>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Signal, DEFAULT_SIGNAL_RING_CAP>,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    token: mio::Token,
    stop: &StopFlag,
) -> RunResult {
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
            let status = match transport.pump(ev) {
                Ok(s) => s,
                Err(_e) => return RunResult::Error,
            };
            note_transport_ready(drv, status);
        }

        // I-3: tight inner drain loop. See ingress-polymarket
        // for rationale.
        loop {
            let n_before = producer.len();
            let state_before = drv.state();
            if drive_one(transport, drv, host, path, producer).is_err() {
                return RunResult::Error;
            }
            if drv.state() == State::Closed {
                return RunResult::Disconnected;
            }
            if producer.len() == n_before && drv.state() == state_before {
                break;
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
        TestTransport,
    };
    use core_ring::Ring;

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

    /// Construct an unmasked (server-side) WebSocket text frame holding
    /// `body`. Supports both short (<=125) and medium (<=65535) forms.
    fn wrap_text_frame(body: &[u8], out: &mut [u8]) -> usize {
        out[0] = 0x81; // FIN + Text
        if body.len() <= 125 {
            out[1] = body.len() as u8;
            out[2..2 + body.len()].copy_from_slice(body);
            2 + body.len()
        } else {
            assert!(body.len() <= u16::MAX as usize, "frame too long for test");
            out[1] = 126;
            let len_be = (body.len() as u16).to_be_bytes();
            out[2] = len_be[0];
            out[3] = len_be[1];
            out[4..4 + body.len()].copy_from_slice(body);
            4 + body.len()
        }
    }

    #[test]
    fn driver_starts_in_connecting() {
        let d = Driver::new(1);
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.sub_count(), 0);
        assert_eq!(d.pending_count(), 0);
        assert!(!d.subscribed);
    }

    #[test]
    fn note_transport_ready_advances_to_needs_ws_write() {
        let mut d = Driver::new(1);
        note_transport_ready(&mut d, Status::Ready);
        assert_eq!(d.state(), State::NeedsWsWrite);
    }

    #[test]
    fn note_transport_ready_closed_transitions_to_closed() {
        let mut d = Driver::new(1);
        note_transport_ready(&mut d, Status::Closed);
        assert_eq!(d.state(), State::Closed);
    }

    #[test]
    fn handshake_completes_and_subscribe_is_emitted() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new(42);
        let ring = Ring::<Signal, DEFAULT_SIGNAL_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"rpc.example", b"/v2", &mut prod).unwrap();
        // Drain the GET handshake so it doesn't confuse later assertions.
        let mut scratch = [0u8; 4096];
        let _ = t.drain_outgoing(&mut scratch);

        let key = sec_key_pub(42);
        let accept = expected_accept_pub(&key);
        let resp = build_server_response(&accept);
        t.inject_incoming(&resp);

        drive_one(&mut t, &mut d, b"rpc.example", b"/v2", &mut prod).unwrap();
        assert_eq!(d.state(), State::Steady);
        assert!(d.subscribed);
        // One pending slot for the subscribe; the liveness poll may also
        // have fired if wall-clock already reached the next deadline.
        assert!(d.pending_count() >= 1);
        // Subscribe request should be sitting in the outbound buffer.
        assert!(t.outgoing_len() > 0);
        let n = t.drain_outgoing(&mut scratch);
        // It's a masked binary frame: opcode 0x82, mask bit set.
        assert_eq!(scratch[0], 0x82);
        assert!(scratch[1] & 0x80 != 0);
        // Unmask and confirm the body contains the subscribe method tag.
        let payload_len = (scratch[1] & 0x7F) as usize;
        assert!(payload_len <= 125, "test assumes short frame");
        let mask = [scratch[2], scratch[3], scratch[4], scratch[5]];
        let mut unmasked = [0u8; 256];
        for i in 0..payload_len {
            unmasked[i] = scratch[6 + i] ^ mask[i & 3];
        }
        assert!(memchr::memmem::find(&unmasked[..payload_len], b"eth_subscribe").is_some());
        // Drain any additional frame (e.g. the liveness poll) that may
        // have been queued after the subscribe.
        let _ = n;
    }

    #[test]
    fn subscribe_response_registers_sub_id() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new(7);
        d.set_state(State::Steady);
        d.suppress_polling_for_test();
        // Simulate that we issued a subscribe.
        d.subscribed = true;
        let id = d.ids.allocate();
        record_pending(&mut d, id, RpcKind::SubscribeNewHeads).unwrap();

        let ring = Ring::<Signal, DEFAULT_SIGNAL_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();

        let body = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":"0x00000000deadbeef"}}"#,
        );
        let mut frame_buf = [0u8; 256];
        let n = wrap_text_frame(body.as_bytes(), &mut frame_buf);
        t.inject_incoming(&frame_buf[..n]);

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod).unwrap();
        assert_eq!(d.sub_count(), 1);
        assert_eq!(d.pending_count(), 0);
        assert_eq!(d.subs[0].0, SubId(0xDEADBEEF));
        assert!(matches!(d.subs[0].1, SubKind::NewHeads));
    }

    #[test]
    fn new_head_notification_emits_warm_signal() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new(7);
        d.set_state(State::Steady);
        d.suppress_polling_for_test();
        d.subscribed = true;

        let ring = Ring::<Signal, DEFAULT_SIGNAL_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();

        // Inject a newHeads notification.
        let body = br#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xab","result":{"number":"0x2a","timestamp":"0x65","gasUsed":"0x1"}}}"#;
        let mut frame_buf = [0u8; 512];
        let n = wrap_text_frame(body, &mut frame_buf);
        t.inject_incoming(&frame_buf[..n]);

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod).unwrap();

        let sig = cons.try_pop().expect("signal must be pushed");
        assert_eq!(sig.sym, SYMBOL_ID_NONE);
        assert!(matches!(sig.class, LatencyClass::Warm));
        assert_eq!(sig.source, SignalSource::Rpc as u8);
        let number = u64::from_le_bytes(sig.payload[0..8].try_into().unwrap());
        let ts = u64::from_le_bytes(sig.payload[8..16].try_into().unwrap());
        let gas = u64::from_le_bytes(sig.payload[16..24].try_into().unwrap());
        assert_eq!(number, 0x2A);
        assert_eq!(ts, 0x65);
        assert_eq!(gas, 0x1);
    }

    #[test]
    fn block_number_response_emits_warm_signal() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new(7);
        d.set_state(State::Steady);
        d.suppress_polling_for_test();
        d.subscribed = true;
        let id = d.ids.allocate();
        record_pending(&mut d, id, RpcKind::BlockNumber).unwrap();

        let ring = Ring::<Signal, DEFAULT_SIGNAL_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();

        let body = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"0x1a2b3c"}}"#);
        let mut frame_buf = [0u8; 256];
        let n = wrap_text_frame(body.as_bytes(), &mut frame_buf);
        t.inject_incoming(&frame_buf[..n]);

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod).unwrap();

        let sig = cons.try_pop().expect("signal must be pushed");
        assert!(matches!(sig.class, LatencyClass::Warm));
        assert_eq!(sig.source, SignalSource::Rpc as u8);
        let block = u64::from_le_bytes(sig.payload[0..8].try_into().unwrap());
        assert_eq!(block, 0x1A2B3C);
    }

    #[test]
    fn rpc_error_retires_pending_slot() {
        let mut d = Driver::new(7);
        d.set_state(State::Steady);
        d.suppress_polling_for_test();
        d.subscribed = true;
        let id = d.ids.allocate();
        record_pending(&mut d, id, RpcKind::BlockNumber).unwrap();
        assert_eq!(d.pending_count(), 1);

        let ring = Ring::<Signal, DEFAULT_SIGNAL_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();

        // Fake an RPC error frame reusing the same id.
        let mut t = TestTransport::with_capacity(4096);
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32000,"message":"transient"}}}}"#,
        );
        let mut frame_buf = [0u8; 256];
        let n = wrap_text_frame(body.as_bytes(), &mut frame_buf);
        t.inject_incoming(&frame_buf[..n]);

        drive_one(&mut t, &mut d, b"h", b"/", &mut prod).unwrap();
        assert_eq!(d.pending_count(), 0);
    }

    #[test]
    fn extract_id_decimal_parses_various_positions() {
        let b = br#"{"jsonrpc":"2.0","id":123,"result":"0x1"}"#;
        let (id, _) = extract_id_decimal(b).unwrap();
        assert_eq!(id, 123);
    }

    #[test]
    fn extract_id_decimal_returns_none_without_marker() {
        let b = br#"{"jsonrpc":"2.0"}"#;
        assert!(extract_id_decimal(b).is_none());
    }

    #[test]
    fn extract_subscribe_result_decodes_hex() {
        let b = br#"{"jsonrpc":"2.0","id":1,"result":"0xabcd"}"#;
        let s = extract_subscribe_result(b).unwrap();
        assert_eq!(s.0, 0xABCD);
    }

    #[test]
    fn pack_new_head_into_payload_layout() {
        let h = NewHead::new_for_test(0x1122_3344_5566_7788, 0xAABB_CCDD, 0x99);
        let p = pack_new_head_into_payload(h);
        assert_eq!(
            u64::from_le_bytes(p[0..8].try_into().unwrap()),
            0x1122_3344_5566_7788
        );
        assert_eq!(u64::from_le_bytes(p[8..16].try_into().unwrap()), 0xAABB_CCDD);
        assert_eq!(u64::from_le_bytes(p[16..24].try_into().unwrap()), 0x99);
        // Reserved region must be zero.
        assert_eq!(&p[24..40], &[0u8; 16]);
    }

    #[test]
    fn pending_slot_collision_debug_asserts() {
        // In release builds the function returns an error; in debug
        // builds the debug_assert fires first. Here we exercise the
        // release-path error return.
        let mut d = Driver::new(1);
        record_pending(&mut d, 1, RpcKind::BlockNumber).unwrap();
        // Force a collision at the same slot.
        let colliding_id = 1u64 + (PENDING_CAP as u64);
        // In debug builds this path panics via debug_assert; skip the
        // behavioural assertion when debug_assertions is on.
        if !cfg!(debug_assertions) {
            let err = record_pending(&mut d, colliding_id, RpcKind::SubscribeNewHeads).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Other);
        }
    }

    #[test]
    fn pending_empty_and_kind_markers_roundtrip() {
        let p = Pending::empty();
        assert!(!p.is_used());
        let used = Pending {
            id: 5,
            created_at_ns: 0,
            kind: RpcKind::BlockNumber,
            _pad: [0; 7],
        };
        assert!(used.is_used());
    }

    #[test]
    fn reset_for_reconnect_clears_tables() {
        let mut d = Driver::new(1);
        d.set_state(State::Steady);
        d.subscribed = true;
        record_pending(&mut d, 1, RpcKind::BlockNumber).unwrap();
        register_subscription(&mut d, SubId(0xDEAD), SubKind::NewHeads);
        assert_eq!(d.pending_count(), 1);
        assert_eq!(d.sub_count(), 1);

        d.reset_for_reconnect(2);
        assert_eq!(d.state(), State::Connecting);
        assert_eq!(d.pending_count(), 0);
        assert_eq!(d.sub_count(), 0);
        assert!(!d.subscribed);
    }

    #[test]
    fn rejects_wrong_accept_value() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = Driver::new(1);
        let ring = Ring::<Signal, DEFAULT_SIGNAL_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"h", b"/", &mut prod).unwrap();
        let mut scratch = [0u8; 4096];
        let _ = t.drain_outgoing(&mut scratch);

        let wrong_accept: [u8; 28] = *b"XXXXXXXXXXXXXXXXXXXXXXXXXXXX";
        let resp = build_server_response(&wrong_accept);
        t.inject_incoming(&resp);

        let err = drive_one(&mut t, &mut d, b"h", b"/", &mut prod).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
