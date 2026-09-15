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
    ws_mask_from_counter, ws_read_frame, ws_unmask_in_place, ws_write_pong, HandshakeResult,
    TlsTransport, Transport, WsOpcode, WsReadResult,
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
const POLL_SLICE: Duration = Duration::from_millis(50);

/// Handshake must complete inside this or the connection is abandoned.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);

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
    rx_len: usize,
    tx: Box<[u8]>,
    mask_ctr: u64,
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
            .field("rx_buffered", &self.rx_len)
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
            host: host.to_owned(),
            addr,
            server_name,
            tls,
            master_hex: crate::config::hex20(master),
            transport: None,
            poll: Poll::new().map_err(|_| WsErr::Disconnected)?,
            events: Events::with_capacity(8),
            rx: vec![0u8; MAX_WS_BUF].into_boxed_slice(),
            rx_len: 0,
            tx: vec![0u8; TX_BUF].into_boxed_slice(),
            mask_ctr: 0,
        })
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

    /// Connect, upgrade, and send both subscriptions.
    ///
    /// Idempotent: returns immediately if already connected.
    pub fn connect(&mut self) -> Result<(), WsErr> {
        if self.transport.is_some() {
            return Ok(());
        }
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
                    // would lose the snapshot.
                    self.rx.copy_within(header_end..self.rx_len, 0);
                    self.rx_len -= header_end;
                    break;
                }
                HandshakeResult::Incomplete => {}
                _ => return Err(WsErr::Upgrade),
            }
            self.fill_rx(&mut t, deadline)?;
        }

        self.transport = Some(t);

        // --- subscribe ------------------------------------------------
        // `user` is the MASTER. See the module docs.
        self.send_subscribe(b"userFills")?;
        self.send_subscribe(b"orderUpdates")?;
        Ok(())
    }

    /// Drop the socket. The next [`UserWs::connect`] redials.
    pub fn disconnect(&mut self) {
        self.transport = None;
        self.rx_len = 0;
    }

    /// Read whatever has arrived and hand each complete TEXT message
    /// to `on_payload`.
    ///
    /// Returns how many messages were delivered. Control frames are
    /// handled here: a Ping is answered, a Close is a disconnect.
    ///
    /// Zero-alloc: payloads are borrowed slices of the receive buffer.
    pub fn pump<F>(&mut self, budget: Duration, mut on_payload: F) -> Result<usize, WsErr>
    where
        F: FnMut(&[u8]),
    {
        let deadline = Instant::now() + budget;
        let mut t = self.transport.take().ok_or(WsErr::Disconnected)?;
        let r = self.pump_inner(&mut t, deadline, &mut on_payload);
        match r {
            Ok(n) => {
                self.transport = Some(t);
                Ok(n)
            }
            Err(e) => {
                // Any failure closes the socket, so the next message
                // can never be read out of this one's leftovers.
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
        // One read, then drain every frame it completed.
        self.fill_rx(t, deadline)?;
        loop {
            let (header, span) = match ws_read_frame(&self.rx[..self.rx_len]) {
                WsReadResult::Incomplete => return Ok(delivered),
                WsReadResult::Malformed => return Err(WsErr::BadFrame),
                WsReadResult::Frame { header, payload } => (header, payload),
            };
            let total = header.header_len as usize + (header.payload_len as usize);
            if total > self.rx_len {
                return Ok(delivered);
            }
            if header.masked {
                // Server frames are never masked (RFC 6455 §5.1).
                return Err(WsErr::BadFrame);
            }
            match header.opcode {
                WsOpcode::Text | WsOpcode::Binary => {
                    if header.fin {
                        on_payload(&self.rx[span.start..span.end]);
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
                    let payload_len = span.end - span.start;
                    if payload_len > 125 {
                        return Err(WsErr::BadFrame);
                    }
                    // Copy out before writing: the pong borrows `tx`
                    // mutably while the payload borrows `rx`.
                    let mut echo = [0u8; 125];
                    echo[..payload_len].copy_from_slice(&self.rx[span.start..span.end]);
                    let n = ws_write_pong(&mut self.tx, &echo[..payload_len], mask)
                        .map_err(|_| WsErr::BadFrame)?;
                    write_all_t(t, &self.tx[..n], deadline)?;
                }
                WsOpcode::Close => return Err(WsErr::Disconnected),
                WsOpcode::Pong | WsOpcode::Continuation => {}
            }
            self.rx.copy_within(total..self.rx_len, 0);
            self.rx_len -= total;
        }
    }

    fn send_subscribe(&mut self, channel: &[u8]) -> Result<(), WsErr> {
        let mut body = [0u8; 256];
        let mut n = 0usize;
        let mut put = |b: &[u8], n: &mut usize| -> Result<(), WsErr> {
            if *n + b.len() > body.len() {
                return Err(WsErr::BadFrame);
            }
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
        let deadline = Instant::now() + HANDSHAKE_DEADLINE;
        let t = self.transport.as_mut().ok_or(WsErr::Disconnected)?;
        write_all_t(t, &self.tx[..frame_len], deadline)?;
        Ok(())
    }

    fn next_mask(&mut self) -> [u8; 4] {
        self.mask_ctr = self.mask_ctr.wrapping_add(1);
        ws_mask_from_counter(self.mask_ctr)
    }

    /// One poll-and-read into the receive buffer.
    fn fill_rx(&mut self, t: &mut TlsTransport, deadline: Instant) -> Result<(), WsErr> {
        if self.rx_len >= self.rx.len() {
            // A frame bigger than the buffer can never complete.
            return Err(WsErr::BadFrame);
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        self.poll
            .poll(&mut self.events, Some(POLL_SLICE))
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
                return Err(WsErr::BadFrame);
            }
            match t.read(&mut self.rx[self.rx_len..]) {
                Ok(0) => return Err(WsErr::Disconnected),
                Ok(n) => self.rx_len += n,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(_) => return Err(WsErr::Disconnected),
            }
        }
    }

    /// The `ws_unmask_in_place` re-export exists so a caller that
    /// receives a masked frame from a TEST double can unmask it with
    /// the same routine this client uses.
    #[doc(hidden)]
    pub fn unmask(buf: &mut [u8], mask: [u8; 4]) {
        ws_unmask_in_place(buf, mask);
    }
}

/// Write every byte, retrying short writes. The transport trait's
/// `write` accepts what it can and leaves the rest to the caller.
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
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return Err(WsErr::Disconnected),
        }
    }
    Ok(())
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
