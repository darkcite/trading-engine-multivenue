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

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_net::{
    constant_time_eq, expected_accept, read_server_handshake, sec_websocket_key_from_seed,
    write_client_handshake, ws_mask_from_counter, ws_read_frame, ws_unmask_in_place, ws_write_pong,
    HandshakeResult, Status, Transport, WsOpcode, WsReadResult,
};
use core_ring::Producer;
use core_time::now_ns;
use core_types::{NsTs, Price, Qty, SymbolId, Tick};

use crate::parse_book_ticker;

// ---------------------------------------------------------------
// Configuration + sizing
// ---------------------------------------------------------------

/// Size of the rx byte buffer. Binance `@bookTicker` frames are ~140 B
/// each; 64 KiB accommodates huge bursts without ever reallocating.
pub const RX_BUF_SIZE: usize = 64 * 1024;

/// Size of the tx byte buffer. Only used for the opening handshake + pong
/// replies, so 4 KiB is generous.
pub const TX_BUF_SIZE: usize = 4 * 1024;

/// Default Binance tick-ring capacity. Must be a power of two (the
/// ring enforces this at construction); 8192 is plenty for a single
/// symbol at Binance cadence.
pub const DEFAULT_TICK_RING_CAP: usize = 8192;

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
}

/// Mutable per-connection state owned by the run-loop. Preallocated at
/// construction; never reallocates in steady state.
///
/// **Single-writer invariant.** `Driver: !Sync` via the `_not_sync`
/// marker field — `&Driver` cannot be shared across threads. The
/// cli spawns one per ingress thread.
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
    sym: SymbolId,
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
/// * `producer`: tick ring producer. A full ring drops ticks silently.
///
/// # Errors
///
/// Any transport error is surfaced. The caller's outer loop should close
/// and reconnect on `Err`.
pub fn drive_one<T: Transport>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
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
            advance_ws_upgrade(drv)?;
        }
        State::Steady => {
            drain_ws_frames(drv, producer)?;
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

fn drain_ws_frames(
    drv: &mut Driver,
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
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
                        handle_text_frame(drv, payload.start..payload.end, producer);
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

                drv.last_activity_ns = now_ns();
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
) {
    let payload = &drv.rx.filled()[payload_range];
    if let Some(f) = parse_book_ticker(payload, drv.sym) {
        let ts_ns = now_ns();
        // `update_id` fits comfortably in u32 over the lifetime of a
        // connection (Binance resets on (re)connect). Truncate for the
        // venue_seq slot; it's only used for monotonicity checks.
        let venue_seq = (f.update_id & 0xFFFF_FFFF) as u32;
        let tick = Tick::new(
            ts_ns,
            f.sym,
            venue_seq,
            Price::from_raw(f.bid_px_1e6),
            Qty::from_raw(f.bid_qty_1e6),
            Price::from_raw(f.ask_px_1e6),
            Qty::from_raw(f.ask_qty_1e6),
        );
        let _ = producer.try_push(tick);
    }
}

// ---------------------------------------------------------------
// Top-level driver
// ---------------------------------------------------------------

/// Stop flag that external threads can raise to signal a graceful
/// shutdown.
pub type StopFlag = AtomicBool;

/// Run the Binance ingress loop until `stop` is set or the transport
/// fails. Reconnect is the caller's responsibility.
pub fn run<T: Transport>(
    transport: &mut T,
    drv: &mut Driver,
    host: &[u8],
    path: &[u8],
    producer: &mut Producer<Tick, DEFAULT_TICK_RING_CAP>,
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

        // I-3: tight inner drain loop. See ingress-polymarket for
        // rationale.
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

    fn build_driver(seed: u64, sym: SymbolId) -> Driver {
        Driver::new(seed, sym)
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

        // Before Ready — no handshake written.
        drive_one(&mut t, &mut d, b"stream.binance.com", b"/ws/btcusdt@bookTicker", &mut prod).unwrap();
        assert_eq!(t.outgoing_len(), 0);

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"stream.binance.com", b"/ws/btcusdt@bookTicker", &mut prod).unwrap();

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

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod).unwrap();
        let mut scratch = [0u8; 4096];
        let _ = t.drain_outgoing(&mut scratch);

        let key = sec_key_pub(42);
        let accept = expected_accept_pub(&key);
        let resp = build_server_response(&accept);
        t.inject_incoming(&resp);

        drive_one(&mut t, &mut d, b"host", b"/", &mut prod).unwrap();
        assert_eq!(d.state(), State::Steady);
    }

    #[test]
    fn drive_one_rejects_wrong_accept_value() {
        let mut t = TestTransport::with_capacity(8192);
        let mut d = build_driver(1, 0);
        let ring = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new();
        let (mut prod, _cons) = ring.split();

        note_transport_ready(&mut d, Status::Ready);
        drive_one(&mut t, &mut d, b"host", b"/", &mut prod).unwrap();
        let mut scratch = [0u8; 4096];
        let _ = t.drain_outgoing(&mut scratch);

        let wrong: [u8; 28] = *b"XXXXXXXXXXXXXXXXXXXXXXXXXXXX";
        let resp = build_server_response(&wrong);
        t.inject_incoming(&resp);

        let err = drive_one(&mut t, &mut d, b"host", b"/", &mut prod).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
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

        drive_one(&mut t, &mut d, b"host", b"/", &mut prod).unwrap();

        let tick = cons.try_pop().expect("tick must be pushed");
        assert_eq!(tick.sym, 42);
        assert_eq!(tick.bid_px.raw(), 25_351_900);
        assert_eq!(tick.ask_px.raw(), 25_365_200);
        assert_eq!(tick.venue_seq, 400_900_217u32);
        assert!(cons.try_pop().is_none());
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

        drive_one(&mut t, &mut d, b"host", b"/", &mut prod).unwrap();
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

        drive_one(&mut t, &mut d, b"host", b"/", &mut prod).unwrap();
        assert!(cons.try_pop().is_none());
    }
}
