// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The private socket: the transport half of [`crate::events`].
//!
//! One TLS WebSocket to `wss://<host>/ws`, SEPARATE from the market-data
//! ingress (plan D1: the venue gives each socket one ordered write path;
//! a stalled public stream must not delay a fill). Establishment is one
//! bounded act ([`HANDSHAKE_DEADLINE`]): TLS, the upgrade, then
//! `{"type":"Authenticate","wallet":…}` — **unsigned; the wallet is the
//! OWNER** (fills, orders and the portfolio are the owner's, whoever
//! signs) — then, once the venue answers `Authenticated`, the two
//! subscriptions `fills` and `order_updates`.
//!
//! The venue pings every 20 s and closes a socket that does not pong
//! within 60 s; [`HcUserWs::pump`] answers every Ping from rx straight
//! into tx. A socket silent for [`KEEPALIVE`]'s idle timeout (three
//! missed venue pings) is dead.
//!
//! It does not reconnect and it does not dedupe: the arm owns both, so
//! the backoff and the [`crate::events::FillIds`] ring outlive a socket.
//! Pumped from the arm's idle path, never blocking once established.

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

use crate::config::write_hex20;
use crate::events::{self, Msg};

/// The venue's WS path.
pub const WS_PATH: &[u8] = b"/ws";

/// Receive buffer: a burst of fills and updates after a reconnect.
pub const MAX_WS_BUF: usize = 1 << 20;

const TX_BUF: usize = 512;
const MIO_TOKEN: Token = Token(0);
const POLL_SLICE: Duration = Duration::from_millis(50);

/// TLS + upgrade + Authenticate + subscribes, all inside this.
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);

/// The venue pings every 20 s; 70 s of silence is three missed pings.
/// A client ping goes out after 45 s of silence (never, while the venue
/// pings as documented).
pub const KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 45_000_000_000,
    idle_timeout_ns: 70_000_000_000,
};

/// Why the socket failed.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WsErr {
    /// DNS resolution failed.
    Dns,
    /// The host is not a valid TLS server name.
    BadServerName,
    /// Connect, read or write failed, or the peer went away.
    Disconnected,
    /// The server refused the upgrade.
    Upgrade,
    /// The venue answered `Authenticate` with an `Error`.
    AuthRefused,
    /// The venue answered a `Subscribe` with an `Error`.
    SubscribeRefused,
    /// A frame violated RFC 6455, was fragmented, or outgrew the buffer.
    BadFrame,
    /// Establishment did not finish inside [`HANDSHAKE_DEADLINE`].
    Timeout,
}

impl core::fmt::Display for WsErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::Dns => "dns resolution failed",
            Self::BadServerName => "host is not a valid TLS server name",
            Self::Disconnected => "the private socket went away",
            Self::Upgrade => "the server refused the websocket upgrade",
            Self::AuthRefused => "the venue refused the wallet on Authenticate",
            Self::SubscribeRefused => "the venue refused a subscription",
            Self::BadFrame => "the server sent a frame this client will not parse",
            Self::Timeout => "the socket did not establish in time",
        };
        write!(f, "hypercall userws: {s}")
    }
}

impl std::error::Error for WsErr {}

/// The private socket.
pub struct HcUserWs {
    host: String,
    port: u16,
    addr: SocketAddr,
    server_name: ServerName<'static>,
    tls: Arc<ClientConfig>,
    wallet: [u8; 20],
    transport: Option<TlsTransport>,
    poll: Poll,
    events: Events,
    rx: Box<[u8]>,
    rx_head: usize,
    rx_len: usize,
    tx: Box<[u8]>,
    mask_ctr: u64,
    epoch: Instant,
    keepalive: Keepalive,
    last_activity_ns: u64,
}

impl core::fmt::Debug for HcUserWs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HcUserWs")
            .field("host", &self.host)
            .field("connected", &self.transport.is_some())
            .finish_non_exhaustive()
    }
}

impl HcUserWs {
    /// Build a client for `wallet` (the OWNER). Resolves DNS; opens
    /// nothing. Boot-only: allocates its buffers.
    ///
    /// # Errors
    ///
    /// [`WsErr::Dns`], [`WsErr::BadServerName`], [`WsErr::Disconnected`].
    pub fn new(
        host: &str,
        port: u16,
        tls: Arc<ClientConfig>,
        wallet: &[u8; 20],
    ) -> Result<Self, WsErr> {
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|_| WsErr::Dns)?
            .next()
            .ok_or(WsErr::Dns)?;
        let server_name =
            TlsTransport::server_name_from_host(host).map_err(|_| WsErr::BadServerName)?;
        Ok(Self {
            // COPY: ≤ 64 B host name for the upgrade and re-resolution,
            // ONCE at boot.
            host: host.to_owned(),
            port,
            addr,
            server_name,
            tls,
            wallet: *wallet,
            transport: None,
            poll: Poll::new().map_err(|_| WsErr::Disconnected)?,
            events: Events::with_capacity(8),
            rx: vec![0u8; MAX_WS_BUF].into_boxed_slice(),
            rx_head: 0,
            rx_len: 0,
            tx: vec![0u8; TX_BUF].into_boxed_slice(),
            mask_ctr: 0,
            epoch: Instant::now(),
            keepalive: Keepalive::new(KEEPALIVE),
            last_activity_ns: 0,
        })
    }

    #[inline]
    fn now_ns(&self) -> u64 {
        (self.epoch.elapsed().as_nanos() as u64).max(1)
    }

    /// Is the socket up?
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.transport.is_some()
    }

    /// Replace the keepalive law (the loopback test). Boot-only.
    pub fn set_keepalive(&mut self, cfg: KeepaliveCfg) {
        self.keepalive = Keepalive::new(cfg);
    }

    /// Connect, upgrade, authenticate, subscribe. Idempotent.
    ///
    /// # Errors
    ///
    /// Every [`WsErr`]; the socket is dropped on any.
    pub fn connect(&mut self) -> Result<(), WsErr> {
        if self.transport.is_some() {
            return Ok(());
        }
        let r = self.dial();
        if r.is_err() {
            self.transport = None;
            self.rx_head = 0;
            self.rx_len = 0;
            if let Some(a) = (self.host.as_str(), self.port)
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.next())
            {
                // Best effort on the failure path only: the venue is
                // CDN-fronted and the boot-time address can go stale.
                self.addr = a;
            }
        }
        r
    }

    fn dial(&mut self) -> Result<(), WsErr> {
        let deadline = Instant::now() + HANDSHAKE_DEADLINE;
        let mut t = TlsTransport::connect(self.addr, self.server_name.clone(), self.tls.clone())
            .map_err(|_| WsErr::Disconnected)?;
        t.register(self.poll.registry(), MIO_TOKEN)
            .map_err(|_| WsErr::Disconnected)?;
        loop {
            if Instant::now() >= deadline {
                return Err(WsErr::Timeout);
            }
            self.poll
                .poll(&mut self.events, Some(POLL_SLICE))
                .map_err(|_| WsErr::Disconnected)?;
            let mut status = core_net::Status::Handshaking;
            for ev in self.events.iter() {
                if ev.token() == MIO_TOKEN {
                    status = t.pump(ev).map_err(|_| WsErr::Disconnected)?;
                }
            }
            match status {
                core_net::Status::Ready => break,
                core_net::Status::Closed => return Err(WsErr::Disconnected),
                _ => t
                    .reregister(self.poll.registry(), MIO_TOKEN)
                    .map_err(|_| WsErr::Disconnected)?,
            }
        }
        let sec_key = sec_websocket_key_from_seed(seed_from_clock());
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
                    self.rx_head = header_end;
                    break;
                }
                HandshakeResult::Incomplete => {}
                _ => return Err(WsErr::Upgrade),
            }
            self.fill_rx(&mut t, deadline, POLL_SLICE)?;
        }

        // Authenticate (unsigned; the owner wallet), then wait for the
        // venue's answer before subscribing — its documented order.
        let mut wh = [0u8; 42];
        write_hex20(&self.wallet, &mut wh);
        let mask = self.next_mask();
        // The frame is masked into tx straight from its parts.
        let n = core_net::ws_write_text_frame_parts(
            &mut self.tx,
            &[&br#"{"type":"Authenticate","wallet":""#[..], &wh[..], &br#""}"#[..]],
            mask,
        )
        .map_err(|_| WsErr::BadFrame)?;
        write_all_t(&mut t, &self.tx[..n], deadline)?;
        let mut authed = false;
        while !authed {
            if Instant::now() >= deadline {
                return Err(WsErr::Timeout);
            }
            self.fill_rx(&mut t, deadline, POLL_SLICE)?;
            let mut refused = false;
            self.drain(&mut t, deadline, &mut |p: &[u8]| match events::parse(p) {
                Msg::Authenticated => authed = true,
                Msg::Error(_) => refused = true,
                _ => {}
            })?;
            if refused {
                return Err(WsErr::AuthRefused);
            }
        }
        self.send_text(&mut t, br#"{"type":"Subscribe","channel":"fills"}"#, deadline)?;
        self.send_text(&mut t, br#"{"type":"Subscribe","channel":"order_updates"}"#, deadline)?;
        // Both confirmations before the socket counts as up: a fill
        // stream the venue did not confirm is a silent one (E-5).
        let mut confirmed = 0u32;
        while confirmed < 2 {
            if Instant::now() >= deadline {
                return Err(WsErr::Timeout);
            }
            self.fill_rx(&mut t, deadline, POLL_SLICE)?;
            let mut refused = false;
            self.drain(&mut t, deadline, &mut |p: &[u8]| match events::parse(p) {
                Msg::Subscribed => confirmed += 1,
                Msg::Error(_) => refused = true,
                _ => {}
            })?;
            if refused {
                return Err(WsErr::SubscribeRefused);
            }
        }
        self.keepalive.reset();
        self.last_activity_ns = self.now_ns();
        self.transport = Some(t);
        Ok(())
    }

    /// Drop the socket; the next [`Self::connect`] redials.
    pub fn disconnect(&mut self) {
        self.transport = None;
        self.rx_head = 0;
        self.rx_len = 0;
    }

    /// Read what has arrived and hand each complete text message to
    /// `on_payload`; the count delivered. **Never blocks** (one
    /// non-blocking read, then the frames it completed); pings are
    /// answered here; a Close is a disconnect.
    ///
    /// # Errors
    ///
    /// Any failure closes the socket.
    pub fn pump<F>(&mut self, budget: Duration, mut on_payload: F) -> Result<usize, WsErr>
    where
        F: FnMut(&[u8]),
    {
        let deadline = Instant::now() + budget;
        let mut t = self.transport.take().ok_or(WsErr::Disconnected)?;
        let r = self.maintain(&mut t, deadline).and_then(|()| {
            self.fill_rx(&mut t, deadline, Duration::ZERO)?;
            self.drain(&mut t, deadline, &mut on_payload)
        });
        match r {
            Ok(n) => {
                self.transport = Some(t);
                Ok(n)
            }
            Err(e) => {
                self.rx_head = 0;
                self.rx_len = 0;
                Err(e)
            }
        }
    }

    /// Every complete frame in rx: text to `on_payload`, pings echoed.
    fn drain<F: FnMut(&[u8])>(
        &mut self,
        t: &mut TlsTransport,
        deadline: Instant,
        on_payload: &mut F,
    ) -> Result<usize, WsErr> {
        let mut delivered = 0usize;
        loop {
            let head = self.rx_head;
            let (header, span) = match ws_read_frame(&self.rx[head..self.rx_len]) {
                WsReadResult::Incomplete => break,
                WsReadResult::Malformed => return Err(WsErr::BadFrame),
                WsReadResult::Frame { header, payload } => (header, payload),
            };
            let total = header.header_len as usize + header.payload_len as usize;
            if head + total > self.rx_len {
                break;
            }
            if header.masked {
                return Err(WsErr::BadFrame);
            }
            let (p0, p1) = (head + span.start, head + span.end);
            match header.opcode {
                WsOpcode::Text | WsOpcode::Binary => {
                    if !header.fin {
                        // The venue does not fragment its JSON; a
                        // reassembler nothing tested is a liability on
                        // the fill path.
                        return Err(WsErr::BadFrame);
                    }
                    on_payload(&self.rx[p0..p1]);
                    delivered += 1;
                }
                WsOpcode::Ping => {
                    if p1 - p0 > 125 {
                        return Err(WsErr::BadFrame);
                    }
                    let mask = self.next_mask();
                    // The echo goes rx → tx directly (disjoint fields).
                    let n = ws_write_pong(&mut self.tx, &self.rx[p0..p1], mask)
                        .map_err(|_| WsErr::BadFrame)?;
                    write_all_t(t, &self.tx[..n], deadline)?;
                }
                WsOpcode::Close => return Err(WsErr::Disconnected),
                WsOpcode::Pong | WsOpcode::Continuation => {}
            }
            self.rx_head = head + total;
        }
        if self.rx_head == self.rx_len {
            self.rx_head = 0;
            self.rx_len = 0;
        }
        Ok(delivered)
    }

    fn maintain(&mut self, t: &mut TlsTransport, deadline: Instant) -> Result<(), WsErr> {
        let now = self.now_ns();
        match self.keepalive.poll(now, self.last_activity_ns) {
            KeepaliveAction::None => Ok(()),
            KeepaliveAction::SendPing => {
                let mask = self.next_mask();
                let n = core_net::ws_write_ping(&mut self.tx, b"", mask)
                    .map_err(|_| WsErr::BadFrame)?;
                write_all_t(t, &self.tx[..n], deadline)?;
                self.keepalive.mark_ping_sent(now);
                Ok(())
            }
            KeepaliveAction::Reconnect => Err(WsErr::Disconnected),
        }
    }

    fn send_text(&mut self, t: &mut TlsTransport, payload: &[u8], deadline: Instant) -> Result<(), WsErr> {
        let mask = self.next_mask();
        let n = core_net::ws_write_text_frame(&mut self.tx, payload, mask)
            .map_err(|_| WsErr::BadFrame)?;
        write_all_t(t, &self.tx[..n], deadline)
    }

    fn next_mask(&mut self) -> [u8; 4] {
        self.mask_ctr = self.mask_ctr.wrapping_add(1);
        ws_mask_from_counter(self.mask_ctr)
    }

    /// One poll-and-read into rx (`wait` 0 on the steady-state pump).
    fn fill_rx(&mut self, t: &mut TlsTransport, deadline: Instant, wait: Duration) -> Result<(), WsErr> {
        if self.rx_len >= self.rx.len() {
            if self.rx_head == 0 {
                return Err(WsErr::BadFrame);
            }
            // COPY: the ONE compaction — a partial frame at the tail of
            // a FULL buffer, ≤ MAX_WS_BUF, only when the consumed prefix
            // is the room it needs (`core_net::IoBuf`'s law) — rejected:
            // a ring buffer, which splits the frame across the wrap and
            // pays the copy at the parse instead.
            self.rx.copy_within(self.rx_head..self.rx_len, 0);
            self.rx_len -= self.rx_head;
            self.rx_head = 0;
        }
        if Instant::now() >= deadline && !wait.is_zero() {
            return Ok(());
        }
        self.poll
            .poll(&mut self.events, Some(wait))
            .map_err(|_| WsErr::Disconnected)?;
        let mut readable = false;
        for ev in self.events.iter() {
            if ev.token() == MIO_TOKEN {
                if t.pump(ev).map_err(|_| WsErr::Disconnected)? == core_net::Status::Closed {
                    return Err(WsErr::Disconnected);
                }
                readable = true;
            }
        }
        t.reregister(self.poll.registry(), MIO_TOKEN)
            .map_err(|_| WsErr::Disconnected)?;
        if !readable {
            return Ok(());
        }
        // Drain to WouldBlock: mio is edge-triggered.
        loop {
            if self.rx_len >= self.rx.len() {
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

/// Write every byte, then flush (rustls queues on `write`).
fn write_all_t<T: Transport>(t: &mut T, buf: &[u8], deadline: Instant) -> Result<(), WsErr> {
    let mut off = 0usize;
    while off < buf.len() {
        if Instant::now() >= deadline {
            return Err(WsErr::Timeout);
        }
        match t.write(&buf[off..]) {
            Ok(0) => return Err(WsErr::Disconnected),
            Ok(n) => off += n,
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

    #[test]
    fn a_bad_host_is_refused_at_construction() {
        let tls = core_net::TlsTransport::default_client_config();
        let e = HcUserWs::new("no such host anywhere.invalid", 443, tls, &[0x44; 20]).unwrap_err();
        assert!(matches!(e, WsErr::Dns | WsErr::BadServerName), "{e:?}");
    }

    #[test]
    fn the_errors_say_what_happened() {
        for e in [
            WsErr::Dns,
            WsErr::BadServerName,
            WsErr::Disconnected,
            WsErr::Upgrade,
            WsErr::AuthRefused,
            WsErr::SubscribeRefused,
            WsErr::BadFrame,
            WsErr::Timeout,
        ] {
            let m = e.to_string();
            assert!(m.starts_with("hypercall userws:") && m.len() > 25, "{m}");
        }
    }
}
