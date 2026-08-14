//! # Polymarket ingress run-loop
//!
//! Event-driven state machine that drives the Phase 1a codecs
//! ([`core_net::ws_frame`], [`crate::parse_book_update`]) against a
//! [`core_net::Transport`]. Monomorphised on the transport so the
//! compiler inlines through every call; no `dyn Trait` anywhere on the
//! hot path.
//!
//! ## State machine
//!
//! ```text
//! Connecting ──► TlsHandshake ──► WsHandshake ──► Steady ──┐
//!      ▲                                                    │
//!      └──────────────── Closed / Err ──────────────────────┘
//! ```
//!
//! 1. **Connecting / TlsHandshake** — the transport is pumped until
//!    [`core_net::Status::Ready`]. During this window rustls exchanges
//!    its ClientHello / ServerHello; the run-loop has nothing to do
//!    except forward mio events.
//! 2. **WsHandshake** — once TLS is ready we write the RFC 6455 opening
//!    request (once), then scan inbound bytes for `\r\n\r\n` plus an
//!    `Upgrade: websocket` header with a matching `Sec-WebSocket-Accept`.
//! 3. **Steady** — pull frames with [`core_net::ws_read_frame`], unmask
//!    if needed, dispatch by opcode. Text frames go through
//!    [`crate::parse_book_update`] and are pushed onto the tick ring.
//! 4. **Closed / Err** — the top-level driver closes the socket and
//!    enters an exponential-backoff sleep before a reconnect attempt.
//!
//! Everything after step 2 is zero-alloc. The tick ring is caller-owned
//! and preallocated at engine boot.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_metrics::{IngressState, IngressStatus};
use core_net::{
    constant_time_eq, expected_accept, read_server_handshake, sec_websocket_key_from_seed,
    write_client_handshake, ws_mask_from_counter, ws_read_frame, ws_unmask_in_place, ws_write_ping,
    ws_write_pong, HandshakeResult, Keepalive, KeepaliveAction, Status, Transport, WsOpcode,
    WsReadResult,
};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{NsTs, SymbolId, Tick};

use crate::parse_book_update;

// ---------------------------------------------------------------
// Configuration + sizing
// ---------------------------------------------------------------

/// Size of the rx byte buffer the run-loop owns. Polymarket CLOB frames
/// fit comfortably in 16 KiB; we size to 64 KiB to absorb bursts.
pub const RX_BUF_SIZE: usize = 64 * 1024;

/// Size of the tx byte buffer. Only used for the handshake + occasional
/// pong replies, so 4 KiB is generous.
pub const TX_BUF_SIZE: usize = 4 * 1024;

/// Compile-time guard that the tick-ring capacity is a power of two.
/// Ring construction itself checks this, but we restate it here because
/// this crate is where sizing is most visible to an operator.
pub const DEFAULT_TICK_RING_CAP: usize = 16_384;

// ---------------------------------------------------------------
// SymbolMap — asset_id string → SymbolId
// ---------------------------------------------------------------

/// Small linear table mapping Polymarket `asset_id` byte slices to the
/// compact [`SymbolId`] used inside the engine. Built at boot and never
/// mutated afterwards; lookups are branch-predictable on cache-warm
/// data.
///
/// A real deploy keeps O(10²) entries — linear scan over cache-resident
/// u32 IDs beats a HashMap for that size.
pub struct SymbolMap {
    entries: Box<[SymbolEntry]>,
    /// I-7: parallel array of 64-bit FNV-1a hashes of each
    /// `asset_id`. Hot path scans this contiguous `[u64]` rather
    /// than chasing pointers through `Box<[u8]>`. Collision check
    /// against the full asset_id only on hash match — at N≈100
    /// entries the false-positive rate is negligible.
    hashes: Box<[u64]>,
}

struct SymbolEntry {
    asset_id: Box<[u8]>,
    sym: SymbolId,
}

/// FNV-1a 64. Same hash core-types/ingress-rss use elsewhere; kept
/// local so we don't pull a dependency for two lines.
#[inline]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x00000100000001B3);
    }
    h
}

impl SymbolMap {
    /// Build a map from `(asset_id, sym_id)` pairs. Allocation
    /// happens once here at boot; `lookup` is zero-alloc.
    pub fn from_pairs<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (Vec<u8>, SymbolId)>,
    {
        let mut out: Vec<SymbolEntry> = Vec::new();
        let mut hashes: Vec<u64> = Vec::new();
        for (asset_id, sym) in pairs {
            hashes.push(fnv1a_64(&asset_id));
            out.push(SymbolEntry {
                asset_id: asset_id.into_boxed_slice(),
                sym,
            });
        }
        Self {
            entries: out.into_boxed_slice(),
            hashes: hashes.into_boxed_slice(),
        }
    }

    /// O(N) lookup with hash pre-filter. Hash-mismatch entries are
    /// skipped on a single `u64` compare; on hash hit we still
    /// verify the full asset_id to handle the (negligible) FNV
    /// collision case.
    #[inline]
    pub fn lookup(&self, asset_id: &[u8]) -> Option<SymbolId> {
        let target = fnv1a_64(asset_id);
        let n = self.hashes.len();
        let mut i = 0usize;
        while i < n {
            if self.hashes[i] == target {
                let e = &self.entries[i];
                if e.asset_id.as_ref() == asset_id {
                    return Some(e.sym);
                }
            }
            i += 1;
        }
        None
    }

    /// Number of mapped symbols.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------
// Buffers — cursor-draining byte windows, zero-alloc after construction
// ---------------------------------------------------------------

/// Fixed-size byte window with a **cursor pair** (head, tail).
/// Allocation at construction; steady state is zero-alloc. Reads
/// happen at `head`, writes append at `tail`. `consume(n)` only
/// bumps `head` (O(1)) — the residual `copy_within` is deferred
/// to [`free_mut`] and only runs when there's no write space
/// left at the tail. Replaces the prior left-shift-on-every-
/// consume pattern that was O(N²) under burst load.
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

    /// Read view of the filled region.
    #[inline]
    fn filled(&self) -> &[u8] {
        &self.data[self.head..self.tail]
    }

    /// Length of the filled region. Equivalent to
    /// `self.filled().len()` but avoids the borrow.
    #[inline]
    fn len(&self) -> usize {
        self.tail - self.head
    }

    /// Mutable filled region — used by `ws_unmask_in_place`. Offsets
    /// the caller passes are relative to `filled()`'s start.
    #[inline]
    fn filled_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.head..self.tail]
    }

    /// Mutable write tail. Lazily compacts when the tail is at the
    /// buffer end and there's space to reclaim at the head — that's
    /// the only time the residual `copy_within` runs.
    #[inline]
    fn free_mut(&mut self) -> &mut [u8] {
        if self.tail == self.data.len() && self.head > 0 {
            // Shift only the residual `tail - head` bytes — not the
            // full buffer. After this the head is at 0 and the
            // tail follows.
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

    /// O(1) — just bumps the read cursor. No memcpy.
    #[inline]
    fn consume(&mut self, n: usize) {
        debug_assert!(self.head + n <= self.tail);
        self.head += n;
        // When the cursors collapse, reset both to 0 so the next
        // `free_mut` doesn't need to compact at all.
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

/// Run-loop state. Internal; surfaced to callers only through
/// [`RunResult`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// TLS handshake in progress. Nothing to decode yet.
    Connecting,
    /// TLS ready; WebSocket opening request not yet sent.
    NeedsWsWrite,
    /// WebSocket opening request sent; awaiting `101` response.
    AwaitingWsUpgrade,
    /// Upgraded — frames can flow.
    Steady,
    /// Peer closed cleanly or requested close.
    Closed,
}

/// How a run-loop iteration terminated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunResult {
    /// External stop signal was observed.
    Stopped,
    /// Peer closed the connection (clean or not). Caller reconnects.
    Disconnected,
    /// No inbound bytes within the keepalive idle budget (D5) — caller
    /// reconnects.
    IdleTimeout,
    /// Fatal transport error.
    Error,
}

/// Mutable per-connection state owned by the run-loop. Preallocated at
/// construction; never reallocates in steady state.
///
/// **Single-writer invariant.** `Driver: !Sync` — see [`Driver::new`]
/// for the marker field. The cli spawns one driver per ingress
/// thread; sharing the same `&Driver` between threads is rejected
/// at compile time.
pub struct Driver {
    state: State,
    rx: IoBuf,
    tx: IoBuf,
    sec_key: [u8; 24],
    expected_accept_val: [u8; 28],
    last_activity_ns: NsTs,
    /// Monotonic counter feeding [`ws_mask_from_counter`] for every
    /// outbound frame. Never wraps during a single session.
    mask_counter: u64,
    /// `!Sync` marker — keeps `Driver: Send` but blocks `&Driver`
    /// from crossing threads.
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
            _not_sync: ::core::marker::PhantomData,
        }
    }

    /// Current state (useful for tests + metrics).
    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    /// Force the state for tests. **Not** for production use.
    #[cfg(test)]
    pub(crate) fn set_state(&mut self, s: State) {
        self.state = s;
    }

    /// Reset buffers + state for a reconnect. Cheap — only cursor bumps.
    pub fn reset_for_reconnect(&mut self, nonce_seed: u64) {
        self.state = State::Connecting;
        self.rx.clear();
        self.tx.clear();
        self.sec_key = sec_websocket_key_from_seed(nonce_seed);
        self.expected_accept_val = expected_accept(&self.sec_key);
        self.last_activity_ns = 0;
        self.mask_counter = 0;
    }
}

// ---------------------------------------------------------------
// drive_one — single-tick state machine advance
// ---------------------------------------------------------------

/// Pump the transport once and consume any buffered frames.
///
/// Zero-alloc once the handshake has completed. Returns `Ok(true)` if
/// the caller should reregister with mio (because [`Transport::interest`]
/// may have shifted); `Ok(false)` if readiness is unchanged.
///
/// * `transport`: any [`Transport`] implementation.
/// * `drv`: per-connection driver state.
/// * `host`, `path`: sent verbatim into the `GET` line + `Host:` header.
/// * `producer`: tick ring producer. A full ring drops the tick and
///   counts it on `status` (D4); no allocation here.
/// * `symbol_map`: asset_id → SymbolId resolver.
/// * `status`: per-ingress observability slot (D4/D5/D7). This thread
///   is its only writer; counter bumps are `Relaxed` atomics.
///
/// # Errors
///
/// Any transport error is surfaced. The caller's outer loop should
/// close and reconnect on `Err`.
pub fn drive_one<T: Transport>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    symbol_map: &SymbolMap,
    status: &IngressStatus,
) -> io::Result<()> {
    // 1. Flush any pending outbound bytes.
    flush_tx(transport, drv)?;

    // 2. Read whatever plaintext the transport has for us.
    fill_rx(transport, drv)?;

    // 3. Advance the state machine.
    match drv.state {
        State::Connecting => {
            // Nothing to do until pump() reports Ready; caller re-enters.
        }
        State::NeedsWsWrite => {
            write_handshake_to_tx(drv, host, path)?;
            drv.state = State::AwaitingWsUpgrade;
        }
        State::AwaitingWsUpgrade => {
            advance_ws_upgrade(drv, status)?;
        }
        State::Steady => {
            drain_ws_frames(drv, producer, symbol_map, status)?;
        }
        State::Closed => {}
    }

    // 4. Push any bytes the state machine produced (handshake, pong,
    //    close-ack, ...) out onto the wire.
    flush_tx(transport, drv)?;
    Ok(())
}

/// Transition the driver from `Connecting` → `NeedsWsWrite` once the
/// transport reports [`Status::Ready`]. Call from the caller's mio
/// poll loop *after* `Transport::pump` returns `Ready` for the first
/// time.
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
            // rx is full — nothing to do until the consumer drains it.
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
            // D7: the one WS-upgrade → Steady transition per session.
            status.set_state(IngressState::Up);
            // D5: the 101 response bytes count as inbound activity.
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

fn drain_ws_frames(
    drv: &mut Driver,
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    symbol_map: &SymbolMap,
    status: &IngressStatus,
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
                // Debug-assert we've got the whole frame in the buffer.
                debug_assert!(total <= drv.rx.filled().len());

                // Unmask in place if the frame is masked (server frames
                // shouldn't be, but be defensive).
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
                            symbol_map,
                            status,
                        );
                    }
                    WsOpcode::Binary => {
                        // Polymarket CLOB is text-only; drop silently.
                    }
                    WsOpcode::Ping => {
                        // Echo payload as Pong. Clients must mask every
                        // frame per RFC 6455 §5.3.
                        let mask = ws_mask_from_counter(drv.mask_counter);
                        drv.mask_counter = drv.mask_counter.wrapping_add(1);
                        let payload_start = payload.start;
                        let payload_end = payload.end;
                        let payload_len = payload_end - payload_start;
                        // Split-borrow: copy payload into a small stack
                        // scratch so we can hand &mut drv.tx.free_mut()
                        // into the writer without aliasing rx.
                        let mut scratch = [0u8; 125];
                        debug_assert!(payload_len <= scratch.len());
                        scratch[..payload_len]
                            .copy_from_slice(&drv.rx.filled()[payload_start..payload_end]);
                        let dst = drv.tx.free_mut();
                        if let Ok(n) = ws_write_pong(dst, &scratch[..payload_len], mask) {
                            drv.tx.advance(n);
                        }
                    }
                    WsOpcode::Pong => {
                        // Keep-alive acknowledgement; nothing to do.
                    }
                    WsOpcode::Close => {
                        drv.state = State::Closed;
                    }
                    WsOpcode::Continuation => {
                        // Fragmented frames are unused by Polymarket CLOB;
                        // drop silently rather than allocate a reassembly
                        // buffer.
                    }
                }

                // D5: every complete inbound frame is activity.
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

fn handle_text_frame(
    drv: &Driver,
    payload_range: core::ops::Range<usize>,
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    symbol_map: &SymbolMap,
    status: &IngressStatus,
) {
    let payload = &drv.rx.filled()[payload_range];
    let Some(asset_id) = extract_asset_id(payload) else {
        // Not a book/price frame (subscription ack, unrelated event) —
        // neither a message nor a parse error.
        return;
    };
    let Some(sym) = symbol_map.lookup(asset_id) else {
        return;
    };
    let ts_ns = now_ns();
    match parse_book_update(payload, sym, ts_ns) {
        Some(tick) => {
            status.add_msgs(1);
            // Ring-full is not an error from the run-loop's
            // perspective; it is back-pressure — but it is no longer
            // silent (D4): every failed push is loss-accounted.
            if producer.try_push(tick).is_err() {
                status.inc_ring_drops();
            }
        }
        None => status.inc_parse_errors(),
    }
}

/// Pull the `asset_id` string value out of a Polymarket CLOB frame.
/// Zero-alloc — returns a subslice of `payload`.
#[inline]
fn extract_asset_id(payload: &[u8]) -> Option<&[u8]> {
    const MARKER: &[u8] = b"\"asset_id\":\"";
    let start = memchr::memmem::find(payload, MARKER)? + MARKER.len();
    let end = memchr::memchr(b'"', &payload[start..])? + start;
    Some(&payload[start..end])
}

// ---------------------------------------------------------------
// Top-level driver — production-only wrapper around drive_one
// ---------------------------------------------------------------

/// Stop flag that external threads can raise to signal a graceful
/// shutdown. The run-loop checks it between mio poll cycles.
pub type StopFlag = AtomicBool;

/// Run the Polymarket ingress loop until `stop` is set, the transport
/// fails, or the keepalive idle budget is exhausted. Reconnect is the
/// caller's responsibility — this function returns
/// [`RunResult::Disconnected`] / [`RunResult::IdleTimeout`] so the
/// outer scheduler can sleep + retry.
///
/// Bounded stack use: all buffers live inside `drv`, which was
/// preallocated. `events` is an mio `Events` buffer the caller owns.
///
/// The first argument is any [`Transport`] implementation; in production
/// this is `TlsTransport`, in integration tests it is `TestTransport`.
///
/// `status` is this session's observability slot (D4/D5/D7); this
/// thread is its only writer. `keepalive` is polled once per
/// steady-state iteration (D5/D6): it schedules masked protocol-level
/// pings and declares the connection dead when no inbound bytes arrive
/// within the idle budget.
pub fn run<T: Transport>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
    symbol_map: &SymbolMap,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    token: mio::Token,
    stop: &StopFlag,
    status: &IngressStatus,
    keepalive: &mut Keepalive,
) -> RunResult {
    let session_start_ns = now_ns();
    keepalive.reset();
    if let Err(_e) = transport.register(poll.registry(), token) {
        return RunResult::Error;
    }
    // Track last-registered interest so we only call
    // `reregister` (one `epoll_ctl` syscall on Linux,
    // ~300-1000 ns) when the bitmask actually changes. In steady
    // state the transport keeps the same readable+writable set,
    // so this is near-zero overhead.
    let mut last_interest = transport.interest();

    while !stop.load(Ordering::Relaxed) {
        if let Err(_e) = poll.poll(events, Some(std::time::Duration::from_millis(50))) {
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

        // I-3: tight inner drain loop. After mio wakes us, keep
        // calling drive_one as long as we either produced a tick
        // or transitioned state. The loop terminates the moment a
        // call makes no observable progress — at which point the
        // outer mio poll handles the next readiness event. This
        // removes the 50 ms mio-poll wakeup cap on first-after-
        // idle bursts where one poll surfaces multiple frames'
        // worth of buffered bytes.
        loop {
            let n_before = producer.len();
            let state_before = drv.state();
            if let Err(_e) = drive_one(transport, drv, host, path, producer, symbol_map, status) {
                return RunResult::Error;
            }
            if drv.state() == State::Closed {
                return RunResult::Disconnected;
            }
            // Progress iff we produced a tick OR moved the
            // driver's state machine forward.
            if producer.len() == n_before && drv.state() == state_before {
                break;
            }
        }

        // D5/D6: keepalive poll, once per steady-state iteration. A
        // connection that never delivered a byte anchors on the
        // session start so it still times out (`last_activity_ns` is
        // only 0 before the upgrade completes).
        if drv.state() == State::Steady {
            let now = now_ns();
            let act = if drv.last_activity_ns == 0 {
                session_start_ns
            } else {
                drv.last_activity_ns
            };
            match keepalive.poll(now, act) {
                KeepaliveAction::SendPing => {
                    // Masked protocol-level ping, empty payload —
                    // mirrors the pong tx path (RFC 6455 §5.3: clients
                    // mask every frame).
                    let mask = ws_mask_from_counter(drv.mask_counter);
                    drv.mask_counter = drv.mask_counter.wrapping_add(1);
                    let dst = drv.tx.free_mut();
                    if let Ok(n) = ws_write_ping(dst, &[], mask) {
                        drv.tx.advance(n);
                    }
                    keepalive.mark_ping_sent(now);
                    // Push the ping onto the wire through the existing
                    // flush path instead of waiting for the next
                    // readiness event.
                    if flush_tx(transport, drv).is_err() {
                        return RunResult::Error;
                    }
                }
                KeepaliveAction::Reconnect => return RunResult::IdleTimeout,
                KeepaliveAction::None => {}
            }
        }

        let cur = transport.interest();
        if cur != last_interest {
            if let Err(_e) = transport.reregister(poll.registry(), token) {
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
        ws_write_text_frame, KeepaliveCfg, TestTransport,
    };
    use core_ring::Ring;

    fn build_driver_with_seed(seed: u64) -> Driver {
        Driver::new(seed)
    }

    /// Build a server-style HTTP/1.1 `101 Switching Protocols` response
    /// that a real Polymarket WSS endpoint would emit.
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
        let d = build_driver_with_seed(1);
        assert_eq!(d.state(), State::Connecting);
    }

    #[test]
    fn note_transport_ready_advances_to_needs_ws_write() {
        let mut d = build_driver_with_seed(1);
        note_transport_ready(&mut d, Status::Ready);
        assert_eq!(d.state(), State::NeedsWsWrite);
    }

    #[test]
    fn note_transport_ready_closed_transitions_to_closed() {
        let mut d = build_driver_with_seed(1);
        note_transport_ready(&mut d, Status::Closed);
        assert_eq!(d.state(), State::Closed);
    }

    #[test]
    fn drive_one_writes_handshake_once_ready() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = build_driver_with_seed(1);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let map = SymbolMap::from_pairs(std::iter::empty());
        let status = IngressStatus::new();

        // Before Ready — no handshake written.
        drive_one(&mut t, &mut d, b"example.com", b"/ws", &mut prod, &map, &status).unwrap();
        assert_eq!(t.outgoing_len(), 0);

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"example.com", b"/ws", &mut prod, &map, &status).unwrap();

        // We now expect a GET-request in the outbound buffer.
        let mut buf = [0u8; 4096];
        let n = t.drain_outgoing(&mut buf);
        assert!(n > 0);
        let prefix = b"GET /ws HTTP/1.1\r\n";
        assert_eq!(&buf[..prefix.len()], prefix);
        assert_eq!(&buf[n - 4..n], b"\r\n\r\n");
        assert_eq!(d.state(), State::AwaitingWsUpgrade);
    }

    #[test]
    fn drive_one_completes_upgrade_on_valid_response() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = build_driver_with_seed(42);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let map = SymbolMap::from_pairs(std::iter::empty());
        let status = IngressStatus::new();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &map, &status).unwrap();
        // drain outbound request so we don't confuse the test.
        let mut scratch = [0u8; 4096];
        let _ = t.drain_outgoing(&mut scratch);

        let key = sec_key_pub(42);
        let accept = expected_accept_pub(&key);
        let resp = build_server_response(&accept);
        t.inject_incoming(&resp);

        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &map, &status).unwrap();
        assert_eq!(d.state(), State::Steady);
        // D7: exactly this transition publishes Up, with the 101
        // response bytes accounted as activity (D5).
        assert_eq!(status.state(), IngressState::Up);
        assert!(status.last_activity_ns() > 0);
        assert!(status.bytes_total() > 0);
    }

    #[test]
    fn drive_one_rejects_wrong_accept_value() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = build_driver_with_seed(1);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let map = SymbolMap::from_pairs(std::iter::empty());
        let status = IngressStatus::new();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &map, &status).unwrap();
        let mut scratch = [0u8; 4096];
        let _ = t.drain_outgoing(&mut scratch);

        // Tamper the accept token — 28 ASCII Xs is the wrong value for
        // any seed.
        let wrong_accept: [u8; 28] = *b"XXXXXXXXXXXXXXXXXXXXXXXXXXXX";
        let resp = build_server_response(&wrong_accept);
        t.inject_incoming(&resp);

        let err = drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &map, &status).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn steady_state_parses_book_frame_into_tick_ring() {
        let mut t = TestTransport::with_capacity(16 * 1024);
        let mut d = build_driver_with_seed(7);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, mut cons) = ring.split();
        let map = SymbolMap::from_pairs(std::iter::once((b"0xABC".to_vec(), 42u32)));
        let status = IngressStatus::new();

        let payload = br#"{"event_type":"book","asset_id":"0xABC","timestamp":"1713000000000","bids":[["0.518","100.0"]],"asks":[["0.520","50.0"]]}"#;
        let mut frame_buf = [0u8; 512];
        // Server frames are unmasked; write a mask-free text frame by
        // hand since ws_write_text_frame masks for the client direction.
        // We emit an unmasked text frame directly.
        assert!(payload.len() <= 125, "test payload must fit short header");
        frame_buf[0] = 0x81; // FIN + Text
        frame_buf[1] = payload.len() as u8; // mask=0
        frame_buf[2..2 + payload.len()].copy_from_slice(payload);
        let frame_len = 2 + payload.len();
        t.inject_incoming(&frame_buf[..frame_len]);

        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &map, &status).unwrap();

        let tick = cons.try_pop().expect("tick must be pushed");
        assert_eq!(tick.sym, 42);
        assert_eq!(tick.bid_px.raw(), 518_000);
        assert_eq!(tick.ask_px.raw(), 520_000);
        // Prove we only queued one tick.
        assert!(cons.try_pop().is_none());
        // §6.4 accounting: one parsed+dispatched message, frame bytes
        // counted, nothing rejected, nothing dropped.
        assert_eq!(status.msgs_total(), 1);
        assert!(status.bytes_total() > 0);
        assert_eq!(status.parse_errors_total(), 0);
        assert_eq!(status.ring_drops_total(), 0);
        let _ = ws_write_text_frame; // quiet unused-import lint on tests that don't call it
    }

    #[test]
    fn steady_state_replies_pong_to_ping() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = build_driver_with_seed(7);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let map = SymbolMap::from_pairs(std::iter::empty());
        let status = IngressStatus::new();

        // Unmasked Ping with a 4-byte payload.
        let mut frame = [0u8; 16];
        frame[0] = 0x89; // FIN + Ping
        frame[1] = 4;
        frame[2..6].copy_from_slice(b"PING");
        t.inject_incoming(&frame[..6]);

        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &map, &status).unwrap();
        assert!(t.outgoing_len() > 0, "driver should have emitted a pong");

        let mut out = [0u8; 64];
        let n = t.drain_outgoing(&mut out);
        // Pong is 0x8A + (0x80|4) + 4-byte mask + 4-byte XORed payload.
        assert_eq!(out[0], 0x8A);
        assert_eq!(out[1], 0x80 | 4);
        let mask = [out[2], out[3], out[4], out[5]];
        let mut unmasked = [0u8; 4];
        for i in 0..4 {
            unmasked[i] = out[6 + i] ^ mask[i & 3];
        }
        assert_eq!(&unmasked, b"PING");
        assert_eq!(n, 10);
    }

    /// Inject an unmasked (server-side) short-form text frame.
    fn inject_unmasked_text(t: &mut TestTransport, payload: &[u8]) {
        assert!(payload.len() <= 125, "short-form only for tests");
        let mut frame = [0u8; 128];
        frame[0] = 0x81; // FIN | Text
        frame[1] = payload.len() as u8; // mask=0
        frame[2..2 + payload.len()].copy_from_slice(payload);
        t.inject_incoming(&frame[..2 + payload.len()]);
    }

    #[test]
    fn steady_state_counts_parse_errors_and_ring_drops() {
        let mut t = TestTransport::with_capacity(16 * 1024);
        let mut d = build_driver_with_seed(7);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let map = SymbolMap::from_pairs(std::iter::once((b"0xABC".to_vec(), 42u32)));
        let status = IngressStatus::new();

        // Known asset_id but no timestamp / levels → parser rejection.
        let bad = br#"{"event_type":"book","asset_id":"0xABC","bids":"nope"}"#;
        inject_unmasked_text(&mut t, bad);
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &map, &status).unwrap();
        assert_eq!(status.parse_errors_total(), 1);
        assert_eq!(status.msgs_total(), 0);
        assert_eq!(status.ring_drops_total(), 0);

        // Fill the ring, then deliver a valid frame: the push must
        // fail and be loss-accounted (D4) while the message still
        // counts as parsed.
        let filler = Tick::new(
            0,
            core_types::VenueId::Polymarket,
            42u32,
            0,
            core_types::Price::from_raw(1),
            core_types::Qty::from_raw(1),
            core_types::Price::from_raw(2),
            core_types::Qty::from_raw(1),
        );
        while prod.try_push(filler).is_ok() {}
        let good = br#"{"event_type":"book","asset_id":"0xABC","timestamp":"1713000000000","bids":[["0.518","100.0"]],"asks":[["0.520","50.0"]]}"#;
        inject_unmasked_text(&mut t, good);
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod, &map, &status).unwrap();
        assert_eq!(status.msgs_total(), 1);
        assert_eq!(status.ring_drops_total(), 1);
        assert_eq!(status.parse_errors_total(), 1);
    }

    #[test]
    fn run_returns_idle_timeout_on_silent_steady_transport() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = build_driver_with_seed(7);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let map = SymbolMap::from_pairs(std::iter::empty());
        let status = IngressStatus::new();
        // Tiny idle budget: the silent transport must trip it on the
        // first steady-state iteration, anchored on the session start
        // because no byte ever arrives (D5).
        let mut keepalive = Keepalive::new(KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: 2,
        });

        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let stop = StopFlag::new(false);

        let res = run(
            &mut t,
            &mut d,
            b"host",
            b"/",
            &mut prod,
            &map,
            &mut poll,
            &mut events,
            mio::Token(0),
            &stop,
            &status,
            &mut keepalive,
        );
        assert_eq!(res, RunResult::IdleTimeout);
    }

    #[test]
    fn run_sends_masked_ws_ping_when_interval_elapses() {
        let mut t = TestTransport::with_capacity(4096);
        let mut d = build_driver_with_seed(7);
        d.set_state(State::Steady);

        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();
        let map = SymbolMap::from_pairs(std::iter::empty());
        let status = IngressStatus::new();
        // Tiny ping interval + huge idle budget: the loop must queue
        // pings (D6) but never trip the idle reconnect.
        let mut keepalive = Keepalive::new(KeepaliveCfg {
            ping_interval_ns: 1,
            idle_timeout_ns: u64::MAX / 2,
        });

        let mut poll = mio::Poll::new().unwrap();
        let mut events = mio::Events::with_capacity(4);
        let stop = StopFlag::new(false);

        // `run` borrows the transport for its whole lifetime, so raise
        // the stop flag from a helper thread after a few 50 ms poll
        // cycles — the first cycle already queues + flushes a ping.
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(300));
                stop.store(true, Ordering::Relaxed);
            });
            let res = run(
                &mut t,
                &mut d,
                b"host",
                b"/",
                &mut prod,
                &map,
                &mut poll,
                &mut events,
                mio::Token(0),
                &stop,
                &status,
                &mut keepalive,
            );
            assert_eq!(res, RunResult::Stopped);
        });

        let mut out = [0u8; 256];
        let n = t.drain_outgoing(&mut out);
        // Empty-payload masked ping = 2 header bytes + 4 mask bytes.
        assert!(n >= 6, "a ping frame must have reached the transport");
        assert_eq!(out[0] & 0x0F, 0x9, "opcode must be Ping");
        assert_ne!(out[1] & 0x80, 0, "client frames must be masked");
        assert_eq!(out[1] & 0x7F, 0, "keepalive ping payload is empty");
    }

    #[test]
    fn symbol_map_lookup_roundtrips() {
        let map = SymbolMap::from_pairs(vec![
            (b"foo".to_vec(), 1),
            (b"bar".to_vec(), 2),
            (b"baz".to_vec(), 3),
        ]);
        assert_eq!(map.lookup(b"foo"), Some(1));
        assert_eq!(map.lookup(b"bar"), Some(2));
        assert_eq!(map.lookup(b"baz"), Some(3));
        assert_eq!(map.lookup(b"qux"), None);
        assert_eq!(map.len(), 3);
        assert!(!map.is_empty());
    }

    #[test]
    fn extract_asset_id_finds_value() {
        let p = br#"{"event_type":"book","asset_id":"0xDEADBEEF","x":1}"#;
        assert_eq!(extract_asset_id(p), Some(&b"0xDEADBEEF"[..]));
    }

    #[test]
    fn extract_asset_id_none_when_missing() {
        let p = br#"{"event_type":"book"}"#;
        assert_eq!(extract_asset_id(p), None);
    }
}
