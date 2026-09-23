// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-hyperevm run-loop
//!
//! HyperEVM JSON-RPC over WSS — `ingress-rpc`'s transport shape (the
//! same states, the same two-phase frame dispatch, the same capture and
//! observability laws) with a pool-state lifecycle on top:
//!
//! ```text
//! Steady ─► Subscribing ─► AwaitHead ─► Reading ─► Emitting ─► Flushing ─► Live
//!               (newHeads + logs)   (pin B)  (eth_call @B)  (snapshot)  (held > B)   │
//!                                     ▲                                               │
//!                                     └──── GAP · resync (ring drop, removed log) ────┘
//! ```
//!
//! * **Subscribing** — `newHeads` and `logs` (every pool, every event
//!   topic, one OR-array) are subscribed first, so nothing after the
//!   snapshot block can be missed.
//! * **AwaitHead** — the next head pins the snapshot block `B`.
//! * **Reading** — [`Snapshotter`] reads every pool at `B` (pipelined
//!   `eth_call`s, up to [`PENDING_CAP`] in flight), archive probe
//!   included (O-H4).
//! * **Emitting** — the snapshot goes onto the ring as ordinary signals
//!   (`SNAPSHOT` · `TICK`… · `STATE`), flow-controlled: never more than
//!   the ring has room for, so a snapshot is never torn by a drop.
//! * **Flushing** — every event that streamed in meanwhile was HELD
//!   (decoded, in arrival order, in a boot-allocated buffer); those at or
//!   before `B` are already in the snapshot and are dropped, the rest go
//!   out in order.
//! * **Live** — events go straight to the ring.
//!
//! A ring drop in Live loses a pool event the member cannot recover
//! from, so it is answered with a `GAP` and a fresh snapshot on the same
//! connection; so is a `removed: true` log. An event of a subscribed
//! pool that cannot be carried (an amount beyond `int128`) is answered
//! with a `GAP` on THAT pool's symbol — it is stale until the next
//! snapshot, the others are untouched.
//!
//! After the handshake nothing allocates: rx/tx, the pending and call
//! tables, the hold buffer and the snapshotter are sized in
//! [`Driver::new`].

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_amm::payload::{encode_gap, encode_head, Payload};
use core_metrics::{IngressState, IngressStatus};
use core_net::{
    constant_time_eq, expected_accept, queue_masked_binary_frame, read_server_handshake,
    sec_websocket_key_from_seed, write_client_handshake, ws_mask_from_counter, ws_read_frame,
    ws_unmask_in_place, ws_write_pong, HandshakeResult, IoBuf, Keepalive, KeepaliveAction,
    PendingTable, ReqKind, Status, SubErr, SubId, SubTable, Transport, WsOpcode, WsReadResult,
};
use core_parse::{find_field, skip_byte, skip_ws};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{Capture, LatencyClass, NsTs, Signal, SymbolId, SYMBOL_ID_NONE};
use ingress_rpc::{
    classify_rpc, parse_rpc_error, write_request_eth_block_number,
    write_request_subscribe_new_heads, RequestIds, RpcFrameKind,
};

use crate::logs::{parse_log, payloads, LogErr, LogMeta, PoolLog, SUBSCRIBED_TOPICS};
use crate::pools::PoolTable;
use crate::rpc::{
    parse_head, push_is_log, push_sub_id, response_id, subscribe_result, write_eth_call, Head,
};
use crate::snapshot::{Call, SnapErr, SnapState, Snapshotter};

#[cfg(test)]
#[path = "run_loop_tests.rs"]
mod tests;

// ---------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------

/// rx buffer: a snapshot's replies arrive pipelined, and a `logs` push
/// is ~1 KiB.
pub const RX_BUF_SIZE: usize = 256 * 1024;
/// tx buffer: up to [`PENDING_CAP`] queued `eth_call`s (~200 B each) or
/// the `logs` subscribe frame (~6 KiB for 128 pools).
pub const TX_BUF_SIZE: usize = 64 * 1024;
/// Signal-ring capacity the engine allocates for this source.
pub const DEFAULT_POOL_RING_CAP: usize = 4096;
/// In-flight JSON-RPC requests (power of two).
pub const PENDING_CAP: usize = 256;
/// Live subscriptions (`newHeads`, `logs`).
pub const SUB_CAP: usize = 4;
/// Liveness `eth_blockNumber` cadence.
pub const RPC_POLL_NS: u64 = 2_000_000_000;
/// Events held while a snapshot is read and emitted.
pub const HOLD_CAP: usize = 4096;
/// Default snapshot radius, ticks either side of the price (≈ ±40 %).
pub const DEFAULT_SNAPSHOT_RADIUS: i32 = 4_000;
/// `Signal::source` of this ingress — `core_types::SignalSource::HyperEvm`
/// (appended by HYPARB H0).
pub const SIGNAL_SOURCE_HYPEREVM: u8 = core_types::SignalSource::HyperEvm as u8;

/// Scratch for one outgoing request body before it is masked into tx.
const SCRATCH: usize = 8 * 1024;
/// Stop issuing reads when tx has less free space than this.
const TX_HEADROOM: usize = 2 * 1024;
/// Requests kept free in the pending table for polls and subscribes.
const PENDING_HEADROOM: usize = 8;

const _: () = assert!(PENDING_CAP.is_power_of_two());

// ---------------------------------------------------------------
// Kinds and states
// ---------------------------------------------------------------

/// What a request was.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RpcKind {
    /// `eth_blockNumber` liveness poll.
    BlockNumber = 0,
    /// `eth_subscribe("newHeads")`.
    SubscribeNewHeads = 1,
    /// `eth_subscribe("logs", …)`.
    SubscribeLogs = 2,
    /// A snapshot `eth_call`.
    EthCall = 3,
    /// Free slot.
    None = 255,
}

impl ReqKind for RpcKind {
    const FREE: Self = RpcKind::None;
}

/// What a subscription streams.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SubKind {
    /// `newHeads`.
    NewHeads = 0,
    /// Pool logs.
    Logs = 1,
    /// Free slot.
    None = 255,
}

impl ReqKind for SubKind {
    const FREE: Self = SubKind::None;
}

/// Transport state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// TLS handshake in progress.
    Connecting,
    /// TLS ready; WebSocket opening request not yet sent.
    NeedsWsWrite,
    /// Opening request sent; awaiting `101`.
    AwaitingWsUpgrade,
    /// Upgraded.
    Steady,
    /// Peer closed.
    Closed,
}

/// Pool-state lifecycle within a Steady session (module docs).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Subscribe requests sent, ids not all back.
    Subscribing,
    /// Waiting for the head that pins the snapshot block.
    AwaitHead,
    /// Snapshot reads in flight.
    Reading,
    /// Snapshot signals going onto the ring.
    Emitting,
    /// Held events going onto the ring.
    Flushing,
    /// Streaming.
    Live,
}

/// How a run terminated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// External stop flag observed.
    Stopped,
    /// Peer closed.
    Disconnected,
    /// Keepalive idle budget exhausted.
    IdleTimeout,
    /// Fatal transport or protocol error.
    Error,
    /// The endpoint failed the archive probe (O-H4): its historical reads
    /// return latest state. Reconnecting to it cannot help; per O-H15 the
    /// caller disables the pool member, not the engine.
    ArchiveDishonest,
}

/// Driver-local counters (the per-ingress `IngressStatus` carries the
/// generic ones).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HyperEvmCounters {
    /// Snapshots completed and emitted.
    pub snapshots: u64,
    /// Resyncs forced by a ring drop or a removed log.
    pub resyncs: u64,
    /// Held events dropped because the snapshot already covers them.
    pub held_covered: u64,
    /// Hold-buffer overflows (each forces a resync).
    pub hold_overflows: u64,
    /// Logs of an address not in the pool table.
    pub foreign_logs: u64,
    /// Logs refused by the decoder, by reason.
    pub logs_malformed: u64,
    /// … topic0 not subscribed.
    pub logs_unknown_topic: u64,
    /// … wrong topic / data-word count.
    pub logs_shape: u64,
    /// … a value the payload cannot carry (pool marked stale).
    pub logs_out_of_range: u64,
    /// `removed: true` logs.
    pub logs_removed: u64,
    /// Pushes for an unknown subscription id.
    pub unknown_sub: u64,
}

/// One event held during a snapshot.
#[derive(Copy, Clone)]
struct Held {
    sym: SymbolId,
    block: u64,
    payload: Payload,
}

const HELD_NONE: Held = Held {
    sym: SYMBOL_ID_NONE,
    block: 0,
    payload: [0; core_amm::payload::PAYLOAD_LEN],
};

// ---------------------------------------------------------------
// Driver
// ---------------------------------------------------------------

/// Per-connection state, preallocated. `!Sync`: one ingress thread
/// drives it (see `ingress-rpc`).
pub struct Driver {
    state: State,
    phase: Phase,
    rx: IoBuf,
    tx: IoBuf,
    sec_key: [u8; 24],
    expected_accept_val: [u8; 28],
    last_activity_ns: NsTs,
    mask_counter: u64,
    ids: RequestIds,
    pending: PendingTable<RpcKind, PENDING_CAP>,
    calls: Box<[Call; PENDING_CAP]>,
    subs: SubTable<SubKind, SUB_CAP>,
    next_poll_at_ns: u64,
    pools: Box<PoolTable>,
    snap: Box<Snapshotter>,
    hold: Box<[Held; HOLD_CAP]>,
    hold_len: usize,
    hold_pos: usize,
    scratch: Box<[u8; SCRATCH]>,
    /// Last block of an event delivered in Live (the `GAP` point).
    last_live_block: u64,
    /// A resync is owed (ring drop / removed log) once the ring has room.
    resync: bool,
    archive_dishonest: bool,
    counters: HyperEvmCounters,
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

impl Driver {
    /// Allocate everything for `pools`, snapshots `radius` ticks wide.
    pub fn new(nonce_seed: u64, pools: PoolTable, radius: i32) -> Self {
        let sec_key = sec_websocket_key_from_seed(nonce_seed);
        let accept = expected_accept(&sec_key);
        let snap = Box::new(Snapshotter::new(&pools, radius));
        Self {
            state: State::Connecting,
            phase: Phase::Subscribing,
            rx: IoBuf::with_capacity(RX_BUF_SIZE),
            tx: IoBuf::with_capacity(TX_BUF_SIZE),
            sec_key,
            expected_accept_val: accept,
            last_activity_ns: 0,
            mask_counter: 0,
            ids: RequestIds::new(),
            pending: PendingTable::new(),
            calls: Box::new([Call::NONE; PENDING_CAP]),
            subs: SubTable::new(),
            next_poll_at_ns: 0,
            pools: Box::new(pools),
            snap,
            hold: Box::new([HELD_NONE; HOLD_CAP]),
            hold_len: 0,
            hold_pos: 0,
            scratch: Box::new([0u8; SCRATCH]),
            last_live_block: 0,
            resync: false,
            archive_dishonest: false,
            counters: HyperEvmCounters::default(),
            _not_sync: ::core::marker::PhantomData,
        }
    }

    /// Transport state.
    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    /// Pool-state phase.
    #[inline]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Driver counters.
    #[inline]
    pub fn counters(&self) -> HyperEvmCounters {
        self.counters
    }

    /// Counters of the current / last snapshot.
    #[inline]
    pub fn snapshot_counters(&self) -> crate::snapshot::SnapCounters {
        self.snap.counters()
    }

    /// Live pending requests.
    #[inline]
    pub fn pending_count(&self) -> usize {
        self.pending.count()
    }

    /// Live subscriptions.
    #[inline]
    pub fn sub_count(&self) -> usize {
        self.subs.count()
    }

    /// Reset for a reconnect. The pool table, the snapshotter and the
    /// counters survive; the `GAP` point survives so the next session
    /// announces the break.
    pub fn reset_for_reconnect(&mut self, nonce_seed: u64) {
        self.state = State::Connecting;
        self.phase = Phase::Subscribing;
        self.rx.clear();
        self.tx.clear();
        self.sec_key = sec_websocket_key_from_seed(nonce_seed);
        self.expected_accept_val = expected_accept(&self.sec_key);
        self.last_activity_ns = 0;
        self.mask_counter = 0;
        self.ids = RequestIds::new();
        self.pending.clear();
        self.subs.clear();
        self.next_poll_at_ns = 0;
        self.hold_len = 0;
        self.hold_pos = 0;
        self.resync = false;
    }

    #[cfg(test)]
    pub(crate) fn set_state(&mut self, s: State) {
        self.state = s;
    }

    #[cfg(test)]
    pub(crate) fn suppress_polling_for_test(&mut self) {
        self.next_poll_at_ns = u64::MAX;
    }
}

// ---------------------------------------------------------------
// drive_one
// ---------------------------------------------------------------

/// Pump the transport once and advance both state machines. Zero-alloc
/// after the handshake.
///
/// # Errors
/// A transport error, a protocol error, or a snapshot the archive probe
/// refused (then [`run`] reports [`RunResult::ArchiveDishonest`]).
pub fn drive_one<T: Transport, C: Capture, const CAP: usize>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Signal, CAP>,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    flush_tx(transport, drv)?;
    fill_rx(transport, drv)?;

    match drv.state {
        State::Connecting | State::Closed => {}
        State::NeedsWsWrite => {
            let n = write_client_handshake(drv.tx.free_mut(), host, path, &drv.sec_key)
                .map_err(|_| io::Error::other("ws handshake buffer too small"))?;
            drv.tx.advance(n);
            drv.state = State::AwaitingWsUpgrade;
        }
        State::AwaitingWsUpgrade => {
            advance_ws_upgrade(drv, status)?;
            if drv.state == State::Steady {
                on_session_start(drv, producer, status, capture)?;
                drv.next_poll_at_ns = now_ns().saturating_add(RPC_POLL_NS);
            }
        }
        State::Steady => {
            maybe_queue_block_number_poll(drv)?;
            drain_ws_frames(drv, producer, status, capture)?;
            if drv.archive_dishonest {
                return Err(io::Error::other("archive probe failed (O-H4)"));
            }
            advance_phase(drv, producer, status, capture)?;
        }
    }

    flush_tx(transport, drv)?;
    Ok(())
}

/// Bump `Connecting → NeedsWsWrite` once TLS is ready.
#[inline]
pub fn note_transport_ready(drv: &mut Driver, status: Status) {
    match status {
        Status::Ready if drv.state == State::Connecting => drv.state = State::NeedsWsWrite,
        Status::Closed => drv.state = State::Closed,
        _ => {}
    }
}

/// Steady entry: announce a break if a previous session delivered
/// anything, then subscribe.
fn on_session_start<C: Capture, const CAP: usize>(
    drv: &mut Driver,
    producer: &mut Producer<Signal, CAP>,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    if drv.last_live_block != 0 {
        if let Some(p) = encode_gap(drv.last_live_block) {
            emit(producer, status, capture, SYMBOL_ID_NONE, p);
        }
    }
    drv.phase = Phase::Subscribing;
    let id = drv.ids.allocate();
    record_pending(drv, id, RpcKind::SubscribeNewHeads)?;
    let n = write_request_subscribe_new_heads(&mut drv.scratch[..], id)
        .map_err(|_| io::Error::other("subscribe request buffer too small"))?;
    queue_frame(drv, n)?;
    let id = drv.ids.allocate();
    record_pending(drv, id, RpcKind::SubscribeLogs)?;
    let n = crate::rpc::write_request_subscribe_logs(
        &mut drv.scratch[..],
        id,
        drv.pools.addresses_hex(),
        &SUBSCRIBED_TOPICS,
    )
    .map_err(|_| io::Error::other("logs subscribe request buffer too small"))?;
    queue_frame(drv, n)
}

/// Mask `drv.scratch[..n]` into tx as one binary frame.
#[inline]
fn queue_frame(drv: &mut Driver, n: usize) -> io::Result<()> {
    // COPY: one request body (≤ 8 KiB, typically ~200 B) scratch → tx — the WebSocket client mask is a transform pass into tx regardless; rendering in place would need a core-net "reserve header, mask in place" API for no saved pass.
    queue_masked_binary_frame(&mut drv.tx, &mut drv.mask_counter, &drv.scratch[..n])
}

// ---------------------------------------------------------------
// Phase machine
// ---------------------------------------------------------------

fn advance_phase<C: Capture, const CAP: usize>(
    drv: &mut Driver,
    producer: &mut Producer<Signal, CAP>,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    match drv.phase {
        Phase::Subscribing | Phase::AwaitHead => Ok(()),
        Phase::Reading => {
            match drv.snap.state() {
                SnapState::Ready => drv.phase = Phase::Emitting,
                SnapState::Failed(SnapErr::ArchiveDishonest) => {
                    drv.archive_dishonest = true;
                    return Err(io::Error::other("archive probe failed (O-H4)"));
                }
                SnapState::Failed(SnapErr::NoPools) => {
                    return Err(io::Error::other("snapshot: every pool failed"))
                }
                SnapState::Reading | SnapState::Idle => return issue_reads(drv),
            }
            advance_phase(drv, producer, status, capture)
        }
        Phase::Emitting => {
            while producer.len() < CAP {
                match drv.snap.next_signal() {
                    Some((sym, p)) => {
                        emit(producer, status, capture, sym, p);
                    }
                    None => {
                        drv.counters.snapshots += 1;
                        drv.phase = Phase::Flushing;
                        return advance_phase(drv, producer, status, capture);
                    }
                }
            }
            Ok(())
        }
        Phase::Flushing => {
            let b = drv.snap.block();
            while drv.hold_pos < drv.hold_len {
                let h = drv.hold[drv.hold_pos];
                if h.block <= b {
                    drv.counters.held_covered += 1;
                    drv.hold_pos += 1;
                    continue;
                }
                if producer.len() >= CAP {
                    return Ok(());
                }
                emit(producer, status, capture, h.sym, h.payload);
                if h.block != GAP_BLOCK {
                    drv.last_live_block = drv.last_live_block.max(h.block);
                }
                drv.hold_pos += 1;
            }
            drv.hold_len = 0;
            drv.hold_pos = 0;
            // Everything through B is delivered (by the snapshot).
            drv.last_live_block = drv.last_live_block.max(b);
            drv.phase = Phase::Live;
            Ok(())
        }
        Phase::Live => {
            if drv.resync && producer.len() < CAP {
                if let Some(p) = encode_gap(drv.last_live_block) {
                    emit(producer, status, capture, SYMBOL_ID_NONE, p);
                }
                drv.resync = false;
                drv.counters.resyncs += 1;
                drv.phase = Phase::AwaitHead;
            }
            Ok(())
        }
    }
}

/// Queue as many snapshot reads as the pending table and tx allow.
fn issue_reads(drv: &mut Driver) -> io::Result<()> {
    while drv.pending.count() + PENDING_HEADROOM < PENDING_CAP
        && drv.tx.free_mut().len() >= TX_HEADROOM
    {
        let Some(call) = drv.snap.next_call() else {
            return Ok(());
        };
        let id = drv.ids.allocate();
        record_pending(drv, id, RpcKind::EthCall)?;
        drv.calls[(id as usize) & (PENDING_CAP - 1)] = call;
        let p = call.pool as usize;
        let mut data = [0u8; 74];
        let dl = call.calldata(drv.snap.family(p), &mut data);
        let to = drv.pools.addresses_hex()[p];
        let n = write_eth_call(&mut drv.scratch[..], id, &to, &data[..dl], call.block)
            .map_err(|_| io::Error::other("eth_call request buffer too small"))?;
        queue_frame(drv, n)?;
    }
    Ok(())
}

/// `block` of a per-pool `GAP`: never covered by a snapshot, never a
/// delivered block.
const GAP_BLOCK: u64 = u64::MAX;

/// One decoded event: to the ring when Live, to the hold buffer
/// otherwise.
fn route_event<C: Capture, const CAP: usize>(
    drv: &mut Driver,
    producer: &mut Producer<Signal, CAP>,
    status: &IngressStatus,
    capture: &mut C,
    sym: SymbolId,
    block: u64,
    payload: Payload,
) {
    if drv.phase == Phase::Live && !drv.resync {
        if emit(producer, status, capture, sym, payload) {
            if block != GAP_BLOCK {
                drv.last_live_block = drv.last_live_block.max(block);
            }
        } else {
            // The member lost an event it cannot recover from: resync.
            drv.resync = true;
        }
        return;
    }
    if drv.phase == Phase::Live {
        // A resync is owed: everything from here is re-read by it.
        drv.hold_len = 0;
        return;
    }
    if drv.hold_len == HOLD_CAP {
        drv.counters.hold_overflows += 1;
        drv.hold_len = 0;
        drv.hold_pos = 0;
        drv.phase = Phase::AwaitHead;
        return;
    }
    drv.hold[drv.hold_len] = Held {
        sym,
        block,
        payload,
    };
    drv.hold_len += 1;
}

/// Capture, then push. `false` = ring full (counted).
#[inline]
fn emit<C: Capture, const CAP: usize>(
    producer: &mut Producer<Signal, CAP>,
    status: &IngressStatus,
    capture: &mut C,
    sym: SymbolId,
    payload: Payload,
) -> bool {
    let sig = Signal::new(
        now_ns(),
        sym,
        LatencyClass::Warm,
        SIGNAL_SOURCE_HYPEREVM,
        payload,
    );
    // §6.5: capture BEFORE the push — a dropped signal still reaches the log.
    capture.signal(&sig);
    if producer.try_push(sig).is_err() {
        status.inc_ring_drops();
        return false;
    }
    true
}

// ---------------------------------------------------------------
// tx / rx
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

fn maybe_queue_block_number_poll(drv: &mut Driver) -> io::Result<()> {
    let now = now_ns();
    if now < drv.next_poll_at_ns {
        return Ok(());
    }
    drv.next_poll_at_ns = now.saturating_add(RPC_POLL_NS);
    let id = drv.ids.allocate();
    record_pending(drv, id, RpcKind::BlockNumber)?;
    let n = write_request_eth_block_number(&mut drv.scratch[..], id)
        .map_err(|_| io::Error::other("blockNumber request buffer too small"))?;
    queue_frame(drv, n)
}

fn record_pending(drv: &mut Driver, id: u64, kind: RpcKind) -> io::Result<()> {
    match drv.pending.record(id, kind, now_ns()) {
        Ok(()) => Ok(()),
        Err(_e) => {
            debug_assert!(false, "pending-request table rejected id {id}: {_e:?}");
            Err(io::Error::other("pending-request slot collision"))
        }
    }
}

// ---------------------------------------------------------------
// Frame drain + dispatch
// ---------------------------------------------------------------

fn drain_ws_frames<C: Capture, const CAP: usize>(
    drv: &mut Driver,
    producer: &mut Producer<Signal, CAP>,
    status: &IngressStatus,
    capture: &mut C,
) -> io::Result<()> {
    loop {
        match ws_read_frame(drv.rx.filled()) {
            WsReadResult::Incomplete => return Ok(()),
            WsReadResult::Malformed => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed ws frame",
                ))
            }
            WsReadResult::Frame { header, payload } => {
                let total = header.header_len as usize + header.payload_len as usize;
                if header.masked {
                    ws_unmask_in_place(
                        &mut drv.rx.filled_mut()[payload.start..payload.end],
                        header.mask,
                    );
                }
                match header.opcode {
                    WsOpcode::Text | WsOpcode::Binary => {
                        handle_json_frame(
                            drv,
                            payload.start..payload.end,
                            producer,
                            status,
                            capture,
                        );
                    }
                    WsOpcode::Ping => {
                        let mask = ws_mask_from_counter(drv.mask_counter);
                        drv.mask_counter = drv.mask_counter.wrapping_add(1);
                        let plen = payload.end - payload.start;
                        let mut scratch = [0u8; 125];
                        debug_assert!(plen <= scratch.len());
                        // COPY: ≤ 125 B ping payload → pong (RFC 6455 control frame) — rx and tx are one borrow of `drv`; same as ingress-rpc.
                        scratch[..plen]
                            .copy_from_slice(&drv.rx.filled()[payload.start..payload.end]);
                        if let Ok(n) = ws_write_pong(drv.tx.free_mut(), &scratch[..plen], mask) {
                            drv.tx.advance(n);
                        }
                    }
                    WsOpcode::Pong | WsOpcode::Continuation => {}
                    WsOpcode::Close => drv.state = State::Closed,
                }
                let now = now_ns();
                drv.last_activity_ns = now;
                status.touch_activity(now);
                status.add_bytes(total as u64);
                drv.rx.consume(total);
                if drv.state == State::Closed || drv.archive_dishonest {
                    return Ok(());
                }
            }
        }
    }
}

/// Phase-1 result (computed under the immutable rx borrow).
#[derive(Copy, Clone)]
enum Dispatch {
    Nothing,
    Head(Head),
    Log(LogMeta, PoolLog),
    LogRefused([u8; 20], LogErr),
    UnknownSub,
    Response {
        id: u64,
        sub: Option<SubId>,
        result: Option<(usize, usize)>,
    },
    Error {
        id: Option<u64>,
    },
}

/// The byte span of a response's `"result":"0x…"` string content.
#[inline]
fn result_span(buf: &[u8]) -> Option<(usize, usize)> {
    let p = find_field(buf, b"\"result\":")?;
    let p = skip_ws(buf, p);
    if p >= buf.len() || buf[p] != b'"' {
        return None;
    }
    let start = skip_byte(buf, p, b'"');
    let len = memchr::memchr(b'"', &buf[start..])?;
    Some((start, start + len))
}

fn handle_json_frame<C: Capture, const CAP: usize>(
    drv: &mut Driver,
    range: core::ops::Range<usize>,
    producer: &mut Producer<Signal, CAP>,
    status: &IngressStatus,
    capture: &mut C,
) {
    let base = range.start;
    let reject_range = range.clone();
    let dispatch = {
        let payload = &drv.rx.filled()[range];
        capture.raw_frame(now_ns(), payload);
        match classify_rpc(payload) {
            RpcFrameKind::Subscription => {
                match push_sub_id(payload).and_then(|s| drv.subs.kind_of(s)) {
                    Some(SubKind::NewHeads) => parse_head(payload)
                        .map(Dispatch::Head)
                        .unwrap_or(Dispatch::Nothing),
                    Some(SubKind::Logs) if push_is_log(payload) => match parse_log(payload) {
                        Ok((meta, log)) => Dispatch::Log(meta, log),
                        Err(e) => {
                            let addr = match find_field(payload, b"\"address\":") {
                                Some(p) => crate::hex::hex_fixed::<20>(
                                    payload,
                                    skip_byte(payload, skip_ws(payload, p), b'"'),
                                )
                                .map(|x| x.0)
                                .unwrap_or([0; 20]),
                                None => [0; 20],
                            };
                            Dispatch::LogRefused(addr, e)
                        }
                    },
                    Some(_) => Dispatch::Nothing,
                    None => Dispatch::UnknownSub,
                }
            }
            RpcFrameKind::Response => match response_id(payload) {
                Some(id) => Dispatch::Response {
                    id,
                    sub: subscribe_result(payload),
                    result: result_span(payload).map(|(s, e)| (base + s, base + e)),
                },
                None => Dispatch::Nothing,
            },
            RpcFrameKind::Error => {
                if let Some(e) = parse_rpc_error(payload) {
                    debug_assert!(
                        e.code >= -33000 && e.code <= 3,
                        "unexpected RPC error code {}",
                        e.code
                    );
                }
                Dispatch::Error {
                    id: response_id(payload),
                }
            }
            RpcFrameKind::Unknown => Dispatch::Nothing,
        }
    };

    match dispatch {
        Dispatch::Nothing => {
            status.inc_parse_errors();
            capture.parse_reject(now_ns(), &drv.rx.filled()[reject_range]);
        }
        Dispatch::UnknownSub => {
            status.add_msgs(1);
            drv.counters.unknown_sub += 1;
        }
        Dispatch::Head(h) => {
            status.add_msgs(1);
            status.add_ticks(1);
            on_head(drv, producer, status, capture, h);
        }
        Dispatch::Log(meta, log) => {
            status.add_msgs(1);
            status.add_ticks(1);
            on_log(drv, producer, status, capture, meta, log);
        }
        Dispatch::LogRefused(addr, e) => {
            status.inc_parse_errors();
            capture.parse_reject(now_ns(), &drv.rx.filled()[reject_range]);
            match e {
                LogErr::Malformed => drv.counters.logs_malformed += 1,
                LogErr::UnknownTopic => drv.counters.logs_unknown_topic += 1,
                LogErr::Shape => drv.counters.logs_shape += 1,
                LogErr::OutOfRange => drv.counters.logs_out_of_range += 1,
            }
            // A subscribed pool's event we cannot carry: that pool is
            // stale until the next snapshot.
            if let Some(sym) = drv.pools.lookup(&addr).map(|p| p.sym) {
                if let Some(p) = encode_gap(drv.last_live_block) {
                    route_event(drv, producer, status, capture, sym, GAP_BLOCK, p);
                }
            }
        }
        Dispatch::Response { id, sub, result } => {
            status.add_msgs(1);
            match drv.pending.complete(id).map(|r| r.kind) {
                Some(RpcKind::SubscribeNewHeads) => register(drv, sub, SubKind::NewHeads),
                Some(RpcKind::SubscribeLogs) => register(drv, sub, SubKind::Logs),
                Some(RpcKind::EthCall) => {
                    let call = drv.calls[(id as usize) & (PENDING_CAP - 1)];
                    match result {
                        Some((s, e)) => drv.snap.on_result(call, Some(&drv.rx.filled()[s..e])),
                        None => drv.snap.on_result(call, None),
                    }
                }
                Some(RpcKind::BlockNumber) | Some(RpcKind::None) | None => {}
            }
        }
        Dispatch::Error { id } => {
            status.add_msgs(1);
            if let Some(id) = id {
                if let Some(RpcKind::EthCall) = drv.pending.complete(id).map(|r| r.kind) {
                    let call = drv.calls[(id as usize) & (PENDING_CAP - 1)];
                    drv.snap.on_result(call, None);
                }
            }
        }
    }
}

fn register(drv: &mut Driver, sub: Option<SubId>, kind: SubKind) {
    let Some(id) = sub else { return };
    if drv.subs.kind_of(id).is_some() {
        // Two live ids folded to one key would alias; refuse loudly.
        debug_assert!(false, "subscription id fold collision");
        return;
    }
    match drv.subs.insert(id, kind) {
        Ok(()) | Err(SubErr::ReservedId) => {}
        Err(SubErr::Full) => debug_assert!(false, "subscription table full at SUB_CAP={SUB_CAP}"),
    }
    if drv.phase == Phase::Subscribing && drv.subs.count() == 2 {
        drv.phase = Phase::AwaitHead;
    }
}

fn on_head<C: Capture, const CAP: usize>(
    drv: &mut Driver,
    producer: &mut Producer<Signal, CAP>,
    status: &IngressStatus,
    capture: &mut C,
    h: Head,
) {
    if drv.phase == Phase::AwaitHead && h.number > crate::snapshot::PROBE_DEPTH {
        drv.snap.begin(h.number);
        drv.phase = Phase::Reading;
        return; // this head is the snapshot block — covered by it
    }
    if let Some(p) = encode_head(h.number, h.timestamp, h.base_fee) {
        route_event(drv, producer, status, capture, SYMBOL_ID_NONE, h.number, p);
    }
}

fn on_log<C: Capture, const CAP: usize>(
    drv: &mut Driver,
    producer: &mut Producer<Signal, CAP>,
    status: &IngressStatus,
    capture: &mut C,
    meta: LogMeta,
    log: PoolLog,
) {
    if meta.removed {
        drv.counters.logs_removed += 1;
        if drv.phase == Phase::Live {
            drv.resync = true;
        } else if drv.phase != Phase::Subscribing {
            // Mid-snapshot: the snapshot may already include the retracted
            // log; abandon it and restart from a fresh head.
            drv.hold_len = 0;
            drv.hold_pos = 0;
            drv.phase = Phase::AwaitHead;
        }
        return;
    }
    let Some(sym) = drv.pools.lookup(&meta.address).map(|p| p.sym) else {
        drv.counters.foreign_logs += 1;
        return;
    };
    match payloads(&meta, &log) {
        Some((ps, n)) => {
            let mut i = 0;
            while i < n {
                route_event(drv, producer, status, capture, sym, meta.block, ps[i]);
                i += 1;
            }
        }
        None => {
            drv.counters.logs_out_of_range += 1;
            if let Some(p) = encode_gap(drv.last_live_block) {
                route_event(drv, producer, status, capture, sym, GAP_BLOCK, p);
            }
        }
    }
}

// ---------------------------------------------------------------
// Top-level loop
// ---------------------------------------------------------------

/// Stop flag.
pub type StopFlag = AtomicBool;

/// Run until `stop`, a disconnect, an idle timeout or an error. Reconnect
/// is the caller's (see [`RunResult`]).
#[allow(clippy::too_many_arguments)]
pub fn run<T: Transport, C: Capture, const CAP: usize>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Signal, CAP>,
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
            match transport.pump(ev) {
                Ok(s) => note_transport_ready(drv, s),
                Err(_) => return RunResult::Error,
            }
        }
        loop {
            let n_before = producer.len();
            let state_before = drv.state();
            let phase_before = drv.phase();
            if drive_one(transport, drv, host, path, producer, status, capture).is_err() {
                return if drv.archive_dishonest {
                    RunResult::ArchiveDishonest
                } else {
                    RunResult::Error
                };
            }
            if drv.state() == State::Closed {
                return RunResult::Disconnected;
            }
            if producer.len() == n_before
                && drv.state() == state_before
                && drv.phase() == phase_before
            {
                break;
            }
        }
        capture.maybe_flush(now_ns());
        // Keepalive: the eth_blockNumber poll is this venue's probe; the
        // keepalive supplies only the idle deadline (as ingress-rpc).
        if drv.state() == State::Steady {
            let now = now_ns();
            let last = if drv.last_activity_ns == 0 {
                session_start_ns
            } else {
                drv.last_activity_ns
            };
            match keepalive.poll(now, last) {
                KeepaliveAction::None => {}
                KeepaliveAction::SendPing => keepalive.mark_ping_sent(now),
                KeepaliveAction::Reconnect => return RunResult::IdleTimeout,
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
