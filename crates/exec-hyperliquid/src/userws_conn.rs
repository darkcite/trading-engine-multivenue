// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The user-event socket: the transport half of [`crate::userws`]
//! (plan §6.1).
//!
//! One TLS WebSocket to the venue, carrying two subscriptions:
//! `userFills` (the fills of record) and `orderUpdates` (the
//! acknowledgements). It is a SEPARATE socket from the market-data
//! ingress, deliberately — a market-data reconnect must not disturb
//! the order stream, and vice versa. Separate sockets, separate
//! reconnect state machines.
//!
//! ## `user` is the MASTER address, not the agent
//!
//! An API wallet signs but cannot be queried. Subscribing with the
//! agent address yields a stream that is valid, empty, and silent —
//! the worst possible failure, because everything looks healthy and no
//! fill ever arrives. Confirmed against the live venue: `userRole` on
//! an agent returns `{"role":"agent","data":{"user":"0x<master>"}}`,
//! and it is that master the subscription needs.
//!
//! ## What this does NOT do
//!
//! It does not reconnect, and it does not own the dedupe. Both belong
//! to the dispatcher worker: reconnect because the worker owns the
//! backoff shared with the exchange socket, and dedupe because the
//! [`crate::userws::TidRing`] must OUTLIVE this connection — a ring
//! that died with the socket would double-book the snapshot that
//! arrives on the very next one.
//!
//! A disconnect is reported as an error and the caller decides.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_net::{
    read_server_handshake, sec_websocket_key_from_seed, write_client_handshake,
    ws_mask_from_counter, ws_read_frame, ws_write_pong, HandshakeResult, Keepalive,
    KeepaliveAction, KeepaliveCfg, TlsTransport, Transport, WsOpcode, WsReadResult,
};
use mio::{Events, Poll, Token};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;

/// The venue's WS path.
pub const WS_PATH: &[u8] = b"/ws";

/// Receive buffer. A `userFills` snapshot can carry ~2,000 rows, and a
/// frame the buffer cannot hold is a frame we cannot parse — so this
/// is sized for the venue's worst case rather than its common one.
pub const MAX_WS_BUF: usize = 1 << 20;

// The buffer has to hold the venue's worst case, not its common one: a
// frame larger than this can never complete, and the snapshot is the
// frame that decides it. Compile-time, because shrinking this is a
// correctness change disguised as a memory saving.
const _: () = assert!(MAX_WS_BUF >= 1 << 20);

/// Outbound frame scratch: two subscribes and the occasional pong.
const TX_BUF: usize = 512;

const MIO_TOKEN: Token = Token(0);
/// How long ONE poll may block while the HANDSHAKE is in flight. The
/// steady-state pump never blocks at all (see [`UserWs::pump`]).
const POLL_SLICE: Duration = Duration::from_millis(50);

/// Handshake — TLS, upgrade AND both subscribes — must complete inside
/// this or the connection is abandoned. ONE deadline for the whole
/// establishment: the first cut minted a fresh 10 s inside each
/// subscribe on top of the handshake's own, so a reconnect could hold
/// the engine thread for 30 s (E7 review, 2026-09-19).
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);

/// Consecutive connect failures between DNS re-resolutions — the
/// address is cached at boot and the venue is CDN-fronted.
const RERESOLVE_AFTER: u32 = 3;

/// The venue cuts a `/ws` connection idle for 60 s
/// (`ingress_hyperliquid::PING_PAYLOAD`'s doc); the market-data
/// socket pings at 50 s for exactly that reason and this socket — the
/// one that carries the FILLS — sent nothing, ever, so a quiet account
/// (the normal state between quarter-hour instances) was cut and
/// redialled every minute, re-delivering the snapshot each time and
/// never growing `ws_gap_ns` enough to halt (E7 review, 2026-09-19).
const KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 50_000_000_000,
    idle_timeout_ns: 75_000_000_000,
};
/// `{"method":"ping"}` — the venue's application-level ping, answered
/// with `{"channel":"pong"}`. Same bytes as the ingress crate's.
const PING_PAYLOAD: &[u8] = b"{\"method\":\"ping\"}";

/// Why the user-event socket failed.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WsErr {
    /// DNS resolution failed.
    Dns,
    /// The host is not a valid TLS server name.
    BadServerName,
    /// Connect, handshake, read or write failed, or the peer went away.
    Disconnected,
    /// The server did not complete the WebSocket upgrade.
    Upgrade,
    /// A frame violated RFC 6455, or was larger than [`MAX_WS_BUF`].
    BadFrame,
    /// The handshake did not finish inside [`HANDSHAKE_DEADLINE`].
    Timeout,
}

impl core::fmt::Display for WsErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            WsErr::Dns => "dns resolution failed",
            WsErr::BadServerName => "host is not a valid TLS server name",
            WsErr::Disconnected => "the user-event socket went away",
            WsErr::Upgrade => "the server refused the websocket upgrade",
            WsErr::BadFrame => "the server sent a frame this client will not parse",
            WsErr::Timeout => "the websocket handshake did not complete in time",
        };
        write!(f, "hl userws: {s}")
    }
}

impl std::error::Error for WsErr {}

/// A connected user-event socket.
pub struct UserWs {
    host: String,
    addr: SocketAddr,
    server_name: ServerName<'static>,
    tls: Arc<ClientConfig>,
    master_hex: String,

    transport: Option<TlsTransport>,
    poll: Poll,
    events: Events,

    rx: Box<[u8]>,
    /// Bytes `rx[..rx_head]` are consumed frames not yet reclaimed;
    /// `rx[rx_head..rx_len]` is unread. A CURSOR, not a compaction:
    /// the first cut `copy_within`'d the whole unread tail after
    /// EVERY frame — O(k²) bytes over a k-frame burst, on the engine
    /// thread (zero-copy audit, 2026-09-19). Now a drained buffer
    /// resets to 0 for free and a compaction happens only when the
    /// buffer is FULL with a partial frame at the end — the
    /// `core_net::IoBuf` shape.
    rx_head: usize,
    rx_len: usize,
    tx: Box<[u8]>,
    mask_ctr: u64,
    /// Consecutive `connect` failures; see [`RERESOLVE_AFTER`].
    connect_fail_streak: u32,
    port: u16,
    /// Monotonic origin for the keepalive's nanosecond clock.
    epoch: Instant,
    keepalive: Keepalive,
    /// Last inbound byte, ns since `epoch`; the session start until
    /// the first byte arrives.
    last_activity_ns: u64,
}

/// Hand-written so the 1 MiB receive buffer never reaches a log line,
/// and so the shape of what is printed is a deliberate choice rather
/// than whatever the fields happen to be.
///
/// There is no secret here to redact — this socket carries no key,
/// only the public master address — but a derived `Debug` would dump
/// a megabyte of buffer into the first `?ws` somebody writes.
impl core::fmt::Debug for UserWs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserWs")
            .field("host", &self.host)
            .field("master", &self.master_hex)
            .field("connected", &self.transport.is_some())
            .field("rx_unread", &(self.rx_len - self.rx_head))
            .finish()
    }
}

impl UserWs {
    /// Build a client. Resolves DNS; opens nothing.
    ///
    /// `master` is the MASTER account address — see the module docs.
    pub fn new(
        host: &str,
        port: u16,
        tls: Arc<ClientConfig>,
        master: &[u8; 20],
    ) -> Result<Self, WsErr> {
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|_| WsErr::Dns)?
            .next()
            .ok_or(WsErr::Dns)?;
        let server_name =
            TlsTransport::server_name_from_host(host).map_err(|_| WsErr::BadServerName)?;
        Ok(Self {
            // COPY: ≤ 64 B host name for re-resolve on reconnect, ONCE
            // at boot (as `HlHttp::new`).
            host: host.to_owned(),
            addr,
            server_name,
            tls,
            master_hex: crate::config::hex20(master),
            transport: None,
            poll: Poll::new().map_err(|_| WsErr::Disconnected)?,
            events: Events::with_capacity(8),
            rx: vec![0u8; MAX_WS_BUF].into_boxed_slice(),
            rx_head: 0,
            rx_len: 0,
            tx: vec![0u8; TX_BUF].into_boxed_slice(),
            mask_ctr: 0,
            connect_fail_streak: 0,
            port,
            epoch: Instant::now(),
            keepalive: Keepalive::new(KEEPALIVE),
            last_activity_ns: 0,
        })
    }

    #[inline]
    fn now_ns(&self) -> u64 {
        // Never 0: the keepalive reads 0 as "nothing yet".
        (self.epoch.elapsed().as_nanos() as u64).max(1)
    }

    /// Is the socket up?
    #[inline]
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.transport.is_some()
    }

    /// The master address this socket subscribes for, as `0x…` hex.
    #[inline]
    #[must_use]
    pub fn master_hex(&self) -> &str {
        &self.master_hex
    }

    /// Replace the keepalive law ([`KEEPALIVE`] by default). Boot-only.
    ///
    /// Exists so the loopback test can drive a 50 s / 75 s law in
    /// milliseconds against a scripted server; the engine never calls
    /// it. Takes effect from the next `connect` (the timer is reset
    /// there).
    pub fn set_keepalive(&mut self, cfg: KeepaliveCfg) {
        self.keepalive = Keepalive::new(cfg);
    }

    /// Connect, upgrade, and send both subscriptions.
    ///
    /// Idempotent: returns immediately if already connected.
    pub fn connect(&mut self) -> Result<(), WsErr> {
        if self.transport.is_some() {
            return Ok(());
        }
        match self.dial() {
            Ok(()) => {
                self.connect_fail_streak = 0;
                Ok(())
            }
            Err(e) => {
                self.transport = None;
                self.rx_head = 0;
                self.rx_len = 0;
                self.connect_fail_streak = self.connect_fail_streak.saturating_add(1);
                if self.connect_fail_streak % RERESOLVE_AFTER == 0 {
                    // Blocking DNS on the FAILURE path only; best
                    // effort — an unresolvable host keeps the old
                    // address.
                    if let Some(a) = (self.host.as_str(), self.port)
                        .to_socket_addrs()
                        .ok()
                        .and_then(|mut it| it.next())
                    {
                        self.addr = a;
                    }
                }
                Err(e)
            }
        }
    }

    fn dial(&mut self) -> Result<(), WsErr> {
        let deadline = Instant::now() + HANDSHAKE_DEADLINE;
        let mut t = TlsTransport::connect(self.addr, self.server_name.clone(), self.tls.clone())
            .map_err(|_| WsErr::Disconnected)?;
        t.register(self.poll.registry(), MIO_TOKEN)
            .map_err(|_| WsErr::Disconnected)?;

        // --- TLS up ---------------------------------------------------
        loop {
            if Instant::now() >= deadline {
                return Err(WsErr::Timeout);
            }
            self.poll
                .poll(&mut self.events, Some(POLL_SLICE))
                .map_err(|_| WsErr::Disconnected)?;
            let mut status = core_net::Status::Handshaking;
            for ev in self.events.iter() {
                if ev.token() != MIO_TOKEN {
                    continue;
                }
                status = t.pump(ev).map_err(|_| WsErr::Disconnected)?;
            }
            match status {
                core_net::Status::Ready => break,
                core_net::Status::Closed => return Err(WsErr::Disconnected),
                _ => t
                    .reregister(self.poll.registry(), MIO_TOKEN)
                    .map_err(|_| WsErr::Disconnected)?,
            }
        }

        // --- WS upgrade -----------------------------------------------
        let seed = seed_from_clock();
        let sec_key = sec_websocket_key_from_seed(seed);
        let n = write_client_handshake(&mut self.tx, self.host.as_bytes(), WS_PATH, &sec_key)
            .map_err(|_| WsErr::Upgrade)?;
        write_all_t(&mut t, &self.tx[..n], deadline)?;

        self.rx_head = 0;
        self.rx_len = 0;
        loop {
            if Instant::now() >= deadline {
                return Err(WsErr::Timeout);
            }
            match read_server_handshake(&self.rx[..self.rx_len]) {
                HandshakeResult::Upgraded { header_end, .. } => {
                    // Anything after the header block is already frame
                    // bytes — the venue can pack the first message into
                    // the same TCP segment as the 101. Dropping it
                    // would lose the snapshot. Zero-copy: the cursor
                    // simply starts after the header block.
                    self.rx_head = header_end;
                    break;
                }
                HandshakeResult::Incomplete => {}
                _ => return Err(WsErr::Upgrade),
            }
            self.fill_rx(&mut t, deadline, POLL_SLICE)?;
        }

        self.transport = Some(t);

        // --- subscribe ------------------------------------------------
        // `user` is the MASTER. See the module docs. Same deadline as
        // the handshake: establishment is ONE bounded act.
        self.send_subscribe(b"userFills", deadline)?;
        self.send_subscribe(b"orderUpdates", deadline)?;
        self.keepalive.reset();
        self.last_activity_ns = self.now_ns();
        Ok(())
    }

    /// Drop the socket. The next [`UserWs::connect`] redials.
    pub fn disconnect(&mut self) {
        self.transport = None;
        self.rx_head = 0;
        self.rx_len = 0;
    }

    /// Read whatever has arrived and hand each complete TEXT message
    /// to `on_payload`.
    ///
    /// Returns how many messages were delivered. Control frames are
    /// handled here: a Ping is answered, a Close is a disconnect.
    ///
    /// **Never blocks.** This runs on the ENGINE THREAD from
    /// `on_idle`, at least every 2 ms; it is a readiness CHECK, and
    /// the engine loop's own pacing is the wait. The first cut polled
    /// with a 50 ms slice here, which on a quiet fill socket — the
    /// normal state — would have parked the single-writer thread for
    /// 50 ms out of every 52 (E7 review, 2026-09-19). `budget` bounds
    /// the DRAIN of a burst that has already arrived, not a wait.
    ///
    /// Also the keepalive's clock: a ping goes out after 50 s of
    /// silence and 75 s without an inbound byte is a dead session.
    ///
    /// Zero-alloc: payloads are borrowed slices of the receive buffer.
    pub fn pump<F>(&mut self, budget: Duration, mut on_payload: F) -> Result<usize, WsErr>
    where
        F: FnMut(&[u8]),
    {
        let deadline = Instant::now() + budget;
        let mut t = self.transport.take().ok_or(WsErr::Disconnected)?;
        let r = self
            .maintain(&mut t, deadline)
            .and_then(|()| self.pump_inner(&mut t, deadline, &mut on_payload));
        match r {
            Ok(n) => {
                self.transport = Some(t);
                Ok(n)
            }
            Err(e) => {
                // Any failure closes the socket, so the next message
                // can never be read out of this one's leftovers.
                self.rx_head = 0;
                self.rx_len = 0;
                Err(e)
            }
        }
    }

    fn pump_inner<F>(
        &mut self,
        t: &mut TlsTransport,
        deadline: Instant,
        on_payload: &mut F,
    ) -> Result<usize, WsErr>
    where
        F: FnMut(&[u8]),
    {
        let mut delivered = 0usize;
        // One non-blocking read, then drain every frame it completed.
        self.fill_rx(t, deadline, Duration::ZERO)?;
        loop {
            let head = self.rx_head;
            let (header, span) = match ws_read_frame(&self.rx[head..self.rx_len]) {
                WsReadResult::Incomplete => return Ok(self.settle(delivered)),
                WsReadResult::Malformed => return Err(WsErr::BadFrame),
                WsReadResult::Frame { header, payload } => (header, payload),
            };
            let total = header.header_len as usize + (header.payload_len as usize);
            if head + total > self.rx_len {
                return Ok(self.settle(delivered));
            }
            if header.masked {
                // Server frames are never masked (RFC 6455 §5.1).
                return Err(WsErr::BadFrame);
            }
            // The span is relative to the unread slice.
            let (p0, p1) = (head + span.start, head + span.end);
            match header.opcode {
                WsOpcode::Text | WsOpcode::Binary => {
                    if header.fin {
                        on_payload(&self.rx[p0..p1]);
                        delivered += 1;
                    } else {
                        // The venue does not fragment its JSON, and a
                        // reassembler we could not test against real
                        // traffic would be a liability on the fill
                        // path. Refuse loudly instead of guessing.
                        return Err(WsErr::BadFrame);
                    }
                }
                WsOpcode::Ping => {
                    let mask = self.next_mask();
                    if p1 - p0 > 125 {
                        return Err(WsErr::BadFrame);
                    }
                    // The echo goes straight from rx into tx — two
                    // disjoint fields, so no scratch.
                    let n = ws_write_pong(&mut self.tx, &self.rx[p0..p1], mask)
                        .map_err(|_| WsErr::BadFrame)?;
                    write_all_t(t, &self.tx[..n], deadline)?;
                }
                WsOpcode::Close => return Err(WsErr::Disconnected),
                WsOpcode::Pong | WsOpcode::Continuation => {}
            }
            // Consumed: advance the cursor. No bytes move.
            self.rx_head = head + total;
        }
    }

    /// End of a drain: a fully consumed buffer is reclaimed for free
    /// (both indices to 0). A partial frame at the tail stays where it
    /// is behind the cursor; `fill_rx` compacts it only if the buffer
    /// fills up around it.
    #[inline(always)]
    fn settle(&mut self, delivered: usize) -> usize {
        if self.rx_head == self.rx_len {
            self.rx_head = 0;
            self.rx_len = 0;
        }
        delivered
    }

    /// The keepalive step: ping when quiet, give up when dead.
    fn maintain(&mut self, t: &mut TlsTransport, deadline: Instant) -> Result<(), WsErr> {
        let now = self.now_ns();
        match self.keepalive.poll(now, self.last_activity_ns) {
            KeepaliveAction::None => Ok(()),
            KeepaliveAction::SendPing => {
                let mask = self.next_mask();
                let n = core_net::ws_write_text_frame(&mut self.tx, PING_PAYLOAD, mask)
                    .map_err(|_| WsErr::BadFrame)?;
                write_all_t(t, &self.tx[..n], deadline)?;
                self.keepalive.mark_ping_sent(now);
                Ok(())
            }
            KeepaliveAction::Reconnect => Err(WsErr::Disconnected),
        }
    }

    fn send_subscribe(&mut self, channel: &[u8], deadline: Instant) -> Result<(), WsErr> {
        let mut body = [0u8; 256];
        let mut n = 0usize;
        let mut put = |b: &[u8], n: &mut usize| -> Result<(), WsErr> {
            if *n + b.len() > body.len() {
                return Err(WsErr::BadFrame);
            }
            // COPY: subscribe-frame literals + the 42 B master hex,
            // ≤ 256 B, twice per connect — building the JSON body from
            // fixed parts IS its construction, and the masked frame
            // writer needs it contiguous; cold, once per socket.
            body[*n..*n + b.len()].copy_from_slice(b);
            *n += b.len();
            Ok(())
        };
        put(br#"{"method":"subscribe","subscription":{"type":""#, &mut n)?;
        put(channel, &mut n)?;
        put(br#"","user":""#, &mut n)?;
        put(self.master_hex.as_bytes(), &mut n)?;
        put(br#""}}"#, &mut n)?;

        let mask = self.next_mask();
        let frame_len = core_net::ws_write_text_frame(&mut self.tx, &body[..n], mask)
            .map_err(|_| WsErr::BadFrame)?;
        let t = self.transport.as_mut().ok_or(WsErr::Disconnected)?;
        write_all_t(t, &self.tx[..frame_len], deadline)?;
        Ok(())
    }

    fn next_mask(&mut self) -> [u8; 4] {
        self.mask_ctr = self.mask_ctr.wrapping_add(1);
        ws_mask_from_counter(self.mask_ctr)
    }

    /// One poll-and-read into the receive buffer. `wait` is how long
    /// the poll may block: [`POLL_SLICE`] during the handshake,
    /// `Duration::ZERO` on the steady-state pump.
    fn fill_rx(
        &mut self,
        t: &mut TlsTransport,
        deadline: Instant,
        wait: Duration,
    ) -> Result<(), WsErr> {
        if self.rx_len >= self.rx.len() {
            if self.rx_head == 0 {
                // A frame bigger than the buffer can never complete.
                return Err(WsErr::BadFrame);
            }
            // COPY: the ONE compaction — a partial frame at the tail
            // of a FULL buffer, ≤ MAX_WS_BUF, only when the consumed
            // prefix is the room it needs (`core_net::IoBuf::free_mut`'s
            // law) — rejected: a ring buffer, which splits the frame
            // across the wrap and pays the copy at the parse instead.
            self.rx.copy_within(self.rx_head..self.rx_len, 0);
            self.rx_len -= self.rx_head;
            self.rx_head = 0;
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        self.poll
            .poll(&mut self.events, Some(wait))
            .map_err(|_| WsErr::Disconnected)?;
        let mut readable = false;
        for ev in self.events.iter() {
            if ev.token() == MIO_TOKEN {
                let st = t.pump(ev).map_err(|_| WsErr::Disconnected)?;
                if st == core_net::Status::Closed {
                    return Err(WsErr::Disconnected);
                }
                readable = true;
            }
        }
        // Re-register on EVERY pass, readable or not: the transport's
        // desired interest changes as its buffers fill and drain, and
        // a stale registration is a socket that stops waking us.
        t.reregister(self.poll.registry(), MIO_TOKEN)
            .map_err(|_| WsErr::Disconnected)?;
        if !readable {
            return Ok(());
        }
        // DRAIN TO WouldBlock. mio is EDGE-triggered: one read per
        // event consumes the readiness edge and leaves the rest of the
        // bytes sitting in the socket with no further event to come.
        // The first version of this read once per event and saw
        // nothing at all — not even the subscription ack — because the
        // handshake's own traffic had already spent the edge.
        loop {
            if self.rx_len >= self.rx.len() {
                // Full again inside one drain: either a frame that
                // cannot fit (head 0) or bytes to reclaim first — let
                // the next call decide; the parse loop runs in between.
                return if self.rx_head == 0 {
                    Err(WsErr::BadFrame)
                } else {
                    Ok(())
                };
            }
            match t.read(&mut self.rx[self.rx_len..]) {
                Ok(0) => return Err(WsErr::Disconnected),
                Ok(n) => {
                    self.rx_len += n;
                    self.last_activity_ns = self.now_ns();
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(_) => return Err(WsErr::Disconnected),
            }
        }
    }
}

/// Write every byte, retrying short writes, then push them to the
/// socket. The transport trait's `write` only QUEUES into rustls;
/// `flush` is what makes the frame leave now rather than on the next
/// readiness event.
///
/// No sleeping: a TLS transport never returns `WouldBlock` from
/// `write` (it buffers), and a transport that did has nowhere to park
/// the bytes — the first cut slept 1 ms per retry on the engine
/// thread.
fn write_all_t<T: Transport>(t: &mut T, buf: &[u8], deadline: Instant) -> Result<(), WsErr> {
    let mut off = 0usize;
    while off < buf.len() {
        if Instant::now() >= deadline {
            return Err(WsErr::Timeout);
        }
        match t.write(&buf[off..]) {
            Ok(0) => return Err(WsErr::Disconnected),
            Ok(n) => off += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(WsErr::Disconnected)
            }
            Err(_) => return Err(WsErr::Disconnected),
        }
    }
    t.flush().map_err(|_| WsErr::Disconnected)
}

fn seed_from_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: [u8; 20] = [0x44; 20];

    #[test]
    fn a_bad_host_is_refused_at_construction() {
        let tls = core_net::TlsTransport::default_client_config();
        let e = UserWs::new("no such host anywhere.invalid", 443, tls, &MASTER).unwrap_err();
        assert!(matches!(e, WsErr::Dns | WsErr::BadServerName), "{e:?}");
    }

    #[test]
    fn the_subscription_names_the_master_not_the_agent() {
        let tls = core_net::TlsTransport::default_client_config();
        let ws = UserWs::new("localhost", 1, tls, &MASTER).expect("construct");
        assert_eq!(ws.master_hex(), crate::config::hex20(&MASTER));
        assert!(!ws.is_connected());
        // The master is a 20-byte address rendered 0x + 40 hex. An
        // agent address here yields a stream that is valid, empty and
        // silent — so the value is pinned rather than trusted.
        assert_eq!(ws.master_hex().len(), 42);
        assert!(ws.master_hex().starts_with("0x"));
    }

    #[test]
    fn the_errors_say_what_happened() {
        for e in [
            WsErr::Dns,
            WsErr::BadServerName,
            WsErr::Disconnected,
            WsErr::Upgrade,
            WsErr::BadFrame,
            WsErr::Timeout,
        ] {
            let m = e.to_string();
            assert!(m.starts_with("hl userws:"), "{m}");
            assert!(m.len() > 15, "{m}");
        }
    }

}
