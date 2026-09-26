// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! One gateway WebSocket connection, driven without blocking (XH4b).
//!
//! The blocking [`crate::userws_conn::UserWs`] dials inside one bounded
//! call. The gateway cannot stop its loop for a dial — commands keep
//! arriving and the other connection keeps carrying posts — so this is
//! the same protocol as a state machine the gateway advances every turn:
//! TLS → upgrade → the two subscribes (`userFills`, `orderUpdates`, both
//! for the MASTER) → both acknowledged → [`Phase::Ready`]. The receive
//! side is the ingress I-3 discipline (`core_net::drain`): read the
//! transport into an [`IoBuf`] until it would block, parse every whole
//! frame in place, and drive again while a read stopped on a full buffer
//! (at most [`DRAIN_STEP_CAP`] times per turn). The socket's readiness
//! interest is re-registered whenever the transport's wish changes.
//!
//! Control frames — the upgrade, the subscribes, a ping, a pong echo —
//! are written into the connection's own small buffer; order posts are
//! framed in place in the gateway's TX buffer and handed to [`GwConn::send`].

use std::net::SocketAddr;
use std::sync::Arc;

use core_net::{
    fill_rx, read_server_handshake, sec_websocket_key_from_seed, write_client_handshake,
    ws_frame_in_place, ws_mask_from_counter, ws_read_frame, ws_write_pong, ws_write_text_frame,
    HandshakeResult, IoBuf, Keepalive, KeepaliveAction, KeepaliveCfg, RxFill, Status, TlsTransport,
    Transport, WsOpcode, WsReadResult, DRAIN_STEP_CAP, WS_CLIENT_HDR_MAX,
};
use mio::event::Event;
use mio::{Interest, Registry, Token};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;

use crate::request::subscribe_json;
use crate::userws_conn::{MAX_WS_BUF, PING_PAYLOAD, WS_PATH};
use crate::wspost::{classify, WsIn};

/// The control-frame buffer: the upgrade request (~200 B), a subscribe
/// (~130 B), a ping, a ≤ 125 B pong echo.
const CTL_CAP: usize = 512;

/// The two channels every gateway connection subscribes, for the master.
const CHANNELS: [&[u8]; 2] = [b"userFills", b"orderUpdates"];

/// Where a connection is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No socket.
    Down,
    /// TCP + TLS handshake in flight.
    Tls,
    /// The upgrade request is out; waiting for the 101.
    Upgrade,
    /// Upgraded; the subscribes are out, not both acknowledged yet.
    Subscribing,
    /// Both subscriptions acknowledged: posts may go out.
    Ready,
}

/// Why a connection failed. Every one ends the connection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConnErr {
    /// The dial (socket, TLS setup, registration) failed.
    Dial,
    /// A transport error.
    Io,
    /// The peer closed (EOF, a WebSocket Close, a TLS close).
    Closed,
    /// The upgrade was refused or unreadable.
    Upgrade,
    /// A frame the protocol forbids (masked, fragmented, oversized, a
    /// control frame over 125 B) or that cannot fit the receive buffer.
    BadFrame,
}

/// One connection slot.
pub struct GwConn {
    token: Token,
    t: Option<TlsTransport>,
    registered: Interest,
    phase: Phase,
    tls_ready: bool,
    acks: u8,
    rx: IoBuf,
    ctl: Box<[u8; CTL_CAP]>,
    mask_ctr: u64,
    posts: u32,
    last_rx_ns: u64,
    keepalive: Keepalive,
}

impl GwConn {
    /// Boot-time: slot `slot` (its mio token), a 1 MiB receive buffer.
    #[must_use]
    pub fn new(slot: u8, keepalive: KeepaliveCfg) -> Self {
        Self {
            token: Token(usize::from(slot)),
            t: None,
            registered: Interest::READABLE,
            phase: Phase::Down,
            tls_ready: false,
            acks: 0,
            rx: IoBuf::with_capacity(MAX_WS_BUF),
            ctl: Box::new([0u8; CTL_CAP]),
            mask_ctr: u64::from(slot) << 56,
            posts: 0,
            last_rx_ns: 0,
            keepalive: Keepalive::new(keepalive),
        }
    }

    /// The connection's phase.
    #[inline]
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Posts written on this connection and not yet answered.
    #[inline]
    #[must_use]
    pub const fn posts(&self) -> u32 {
        self.posts
    }

    /// A post went out on this connection.
    #[inline]
    pub fn note_post(&mut self) {
        self.posts += 1;
    }

    /// `n` posts of this connection were answered or timed out.
    #[inline]
    pub fn note_answered(&mut self, n: u32) {
        debug_assert!(self.posts >= n, "answers for posts never sent");
        self.posts = self.posts.saturating_sub(n);
    }

    /// The next frame mask (RFC 6455 §5.3: a fresh key per frame).
    #[inline]
    pub fn next_mask(&mut self) -> [u8; 4] {
        self.mask_ctr = self.mask_ctr.wrapping_add(1);
        ws_mask_from_counter(self.mask_ctr)
    }

    /// Start a dial: TCP connect + TLS setup, registered for readiness.
    /// Cold: rustls allocates its connection here, and the server name
    /// is cloned for it.
    ///
    /// # Errors
    /// [`ConnErr::Dial`].
    pub fn dial(
        &mut self,
        addr: SocketAddr,
        name: &ServerName<'static>,
        tls: &Arc<ClientConfig>,
        registry: &Registry,
        now_ns: u64,
    ) -> Result<(), ConnErr> {
        let mut t = TlsTransport::connect(addr, name.clone(), Arc::clone(tls))
            .map_err(|_| ConnErr::Dial)?;
        t.register(registry, self.token)
            .map_err(|_| ConnErr::Dial)?;
        self.registered = t.interest();
        self.t = Some(t);
        self.phase = Phase::Tls;
        self.tls_ready = false;
        self.acks = 0;
        self.rx.clear();
        self.posts = 0;
        self.last_rx_ns = now_ns;
        self.keepalive.reset();
        Ok(())
    }

    /// Drop the socket (closing it deregisters it). Posts still counted
    /// here are the owner's to strand first.
    pub fn close(&mut self) {
        self.t = None;
        self.phase = Phase::Down;
        self.tls_ready = false;
        self.acks = 0;
        self.rx.clear();
        self.posts = 0;
    }

    /// A readiness event for this connection's token.
    ///
    /// # Errors
    /// [`ConnErr::Closed`] or [`ConnErr::Io`].
    pub fn on_event(&mut self, ev: &Event) -> Result<(), ConnErr> {
        let Some(t) = self.t.as_mut() else {
            return Ok(());
        };
        match t.pump(ev) {
            Ok(Status::Closed) => Err(ConnErr::Closed),
            Ok(Status::Ready) => {
                self.tls_ready = true;
                Ok(())
            }
            Ok(Status::Handshaking) => Ok(()),
            Err(_) => Err(ConnErr::Io),
        }
    }

    /// Advance the connection and read everything that arrived. Every
    /// complete text frame is handed to `on_text` — except the two
    /// subscription acknowledgements, which are this machine's own.
    /// Returns whether anything moved.
    ///
    /// # Errors
    /// Any [`ConnErr`]; the owner closes the connection.
    pub fn service<F: FnMut(&[u8])>(
        &mut self,
        now_ns: u64,
        host: &[u8],
        master_hex: &[u8],
        registry: &Registry,
        on_text: &mut F,
    ) -> Result<bool, ConnErr> {
        if self.t.is_none() {
            return Ok(false);
        }
        let mut moved = false;
        if self.phase == Phase::Tls && self.tls_ready {
            self.send_upgrade(host)?;
            self.phase = Phase::Upgrade;
            moved = true;
        }
        let mut steps = 0u32;
        loop {
            let before = self.rx.len();
            let fill = {
                let Some(t) = self.t.as_mut() else {
                    return Ok(moved);
                };
                fill_rx(t, &mut self.rx).map_err(|_| ConnErr::Io)?
            };
            if fill == RxFill::Eof {
                return Err(ConnErr::Closed);
            }
            if self.rx.len() != before {
                self.last_rx_ns = now_ns;
            }
            if self.phase == Phase::Upgrade {
                match read_server_handshake(self.rx.filled()) {
                    HandshakeResult::Upgraded { header_end, .. } => {
                        // Frames may follow the 101 in the same segment:
                        // the cursor simply starts after the header block.
                        self.rx.consume(header_end);
                        self.send_subscribes(master_hex)?;
                        self.phase = Phase::Subscribing;
                        moved = true;
                    }
                    HandshakeResult::Incomplete => {}
                    _ => return Err(ConnErr::Upgrade),
                }
            }
            if self.phase == Phase::Subscribing || self.phase == Phase::Ready {
                moved |= self.parse_frames(on_text)?;
            }
            steps += 1;
            if fill != RxFill::Full || steps == DRAIN_STEP_CAP {
                break;
            }
            moved = true;
        }
        self.reregister(registry)?;
        Ok(moved)
    }

    /// Every whole frame in the receive buffer, in place.
    fn parse_frames<F: FnMut(&[u8])>(&mut self, on_text: &mut F) -> Result<bool, ConnErr> {
        let mut moved = false;
        loop {
            let (header, span) = match ws_read_frame(self.rx.filled()) {
                WsReadResult::Incomplete => return Ok(moved),
                WsReadResult::Malformed => return Err(ConnErr::BadFrame),
                WsReadResult::Frame { header, payload } => (header, payload),
            };
            let total = header.header_len as usize + header.payload_len as usize;
            if total > self.rx.len() {
                if total > MAX_WS_BUF {
                    // A frame larger than the buffer can never complete.
                    return Err(ConnErr::BadFrame);
                }
                return Ok(moved);
            }
            if header.masked {
                // Server frames are never masked (RFC 6455 §5.1).
                return Err(ConnErr::BadFrame);
            }
            match header.opcode {
                WsOpcode::Text | WsOpcode::Binary => {
                    if !header.fin {
                        // The venue does not fragment its JSON; a
                        // reassembler nobody can test against it would
                        // be a liability on the fill path.
                        return Err(ConnErr::BadFrame);
                    }
                    let frame = &self.rx.filled()[span.start..span.end];
                    if self.phase == Phase::Subscribing && classify(frame) == Ok(WsIn::SubAck) {
                        self.acks += 1;
                        if self.acks as usize == CHANNELS.len() {
                            self.phase = Phase::Ready;
                        }
                    } else {
                        on_text(frame);
                    }
                }
                WsOpcode::Ping => {
                    if span.end - span.start > 125 {
                        return Err(ConnErr::BadFrame);
                    }
                    self.mask_ctr = self.mask_ctr.wrapping_add(1);
                    let mask = ws_mask_from_counter(self.mask_ctr);
                    // The echo goes straight from rx into ctl — two
                    // disjoint buffers, no scratch.
                    let n = ws_write_pong(
                        &mut self.ctl[..],
                        &self.rx.filled()[span.start..span.end],
                        mask,
                    )
                    .map_err(|_| ConnErr::BadFrame)?;
                    let t = self.t.as_mut().ok_or(ConnErr::Closed)?;
                    write_all(t, &self.ctl[..n])?;
                }
                WsOpcode::Close => return Err(ConnErr::Closed),
                WsOpcode::Pong | WsOpcode::Continuation => {}
            }
            self.rx.consume(total);
            moved = true;
        }
    }

    /// Write one framed post. Only a [`Phase::Ready`] connection takes one.
    ///
    /// # Errors
    /// [`ConnErr::Io`] (the socket is dead; the frame may or may not have
    /// left) or [`ConnErr::Closed`].
    pub fn send(&mut self, frame: &[u8]) -> Result<(), ConnErr> {
        debug_assert_eq!(self.phase, Phase::Ready, "a post on a connection not ready");
        let t = self.t.as_mut().ok_or(ConnErr::Closed)?;
        write_all(t, frame)
    }

    /// The keepalive step for a ready connection: ping when quiet.
    /// `Ok(true)` = the connection is dead (silent past the idle timeout).
    ///
    /// # Errors
    /// The ping could not be written.
    pub fn maintain(&mut self, now_ns: u64) -> Result<bool, ConnErr> {
        if self.phase != Phase::Ready {
            return Ok(false);
        }
        match self.keepalive.poll(now_ns, self.last_rx_ns) {
            KeepaliveAction::None => Ok(false),
            KeepaliveAction::Reconnect => Ok(true),
            KeepaliveAction::SendPing => {
                let mask = self.next_mask();
                let n = ws_write_text_frame(&mut self.ctl[..], PING_PAYLOAD, mask)
                    .map_err(|_| ConnErr::BadFrame)?;
                let t = self.t.as_mut().ok_or(ConnErr::Closed)?;
                write_all(t, &self.ctl[..n])?;
                self.keepalive.mark_ping_sent(now_ns);
                Ok(false)
            }
        }
    }

    /// When the last inbound byte arrived (monotonic ns; the dial time
    /// until the first).
    #[inline]
    #[must_use]
    pub const fn last_rx_ns(&self) -> u64 {
        self.last_rx_ns
    }

    fn send_upgrade(&mut self, host: &[u8]) -> Result<(), ConnErr> {
        self.mask_ctr = self.mask_ctr.wrapping_add(1);
        let key = sec_websocket_key_from_seed(self.mask_ctr ^ self.last_rx_ns);
        let n = write_client_handshake(&mut self.ctl[..], host, WS_PATH, &key)
            .map_err(|_| ConnErr::Upgrade)?;
        let t = self.t.as_mut().ok_or(ConnErr::Closed)?;
        write_all(t, &self.ctl[..n])
    }

    fn send_subscribes(&mut self, master_hex: &[u8]) -> Result<(), ConnErr> {
        let mut i = 0usize;
        while i < CHANNELS.len() {
            // The body is rendered behind the frame-header room and
            // framed in place: no staging copy.
            let n = subscribe_json(&mut self.ctl[WS_CLIENT_HDR_MAX..], CHANNELS[i], master_hex)
                .map_err(|_| ConnErr::BadFrame)?;
            let mask = self.next_mask();
            let start = ws_frame_in_place(
                &mut self.ctl[..],
                WS_CLIENT_HDR_MAX,
                WsOpcode::Text,
                n,
                mask,
            )
            .map_err(|_| ConnErr::BadFrame)?;
            let t = self.t.as_mut().ok_or(ConnErr::Closed)?;
            write_all(t, &self.ctl[start..WS_CLIENT_HDR_MAX + n])?;
            i += 1;
        }
        Ok(())
    }

    /// Re-register only when the transport's wish changed: a stale
    /// registration is a socket that stops waking the loop.
    fn reregister(&mut self, registry: &Registry) -> Result<(), ConnErr> {
        let Some(t) = self.t.as_mut() else {
            return Ok(());
        };
        let want = t.interest();
        if want != self.registered {
            t.reregister(registry, self.token)
                .map_err(|_| ConnErr::Io)?;
            self.registered = want;
        }
        Ok(())
    }
}

/// Queue every byte into the TLS layer, then push it to the socket. A
/// TLS transport buffers rather than returning `WouldBlock`; what the
/// socket cannot take now leaves on the next readiness event.
fn write_all(t: &mut TlsTransport, buf: &[u8]) -> Result<(), ConnErr> {
    let mut off = 0usize;
    while off < buf.len() {
        match t.write(&buf[off..]) {
            Ok(0) | Err(_) => return Err(ConnErr::Io),
            Ok(n) => off += n,
        }
    }
    t.flush().map_err(|_| ConnErr::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KA: KeepaliveCfg = KeepaliveCfg {
        ping_interval_ns: 30_000_000_000,
        idle_timeout_ns: 60_000_000_000,
    };

    #[test]
    fn a_new_connection_is_down_with_no_posts() {
        let mut c = GwConn::new(1, KA);
        assert_eq!(c.phase(), Phase::Down);
        assert_eq!(c.posts(), 0);
        c.note_post();
        c.note_post();
        c.note_answered(1);
        assert_eq!(c.posts(), 1);
        c.close();
        assert_eq!(c.posts(), 0, "a closed connection carries nothing");
        assert_eq!(
            c.maintain(u64::MAX),
            Ok(false),
            "a down connection is not kept alive"
        );
    }

    #[test]
    fn masks_differ_per_frame_and_per_slot() {
        let mut a = GwConn::new(0, KA);
        let mut b = GwConn::new(1, KA);
        let (a1, a2, b1) = (a.next_mask(), a.next_mask(), b.next_mask());
        assert_ne!(a1, a2);
        assert_ne!(a1, b1);
    }
}
