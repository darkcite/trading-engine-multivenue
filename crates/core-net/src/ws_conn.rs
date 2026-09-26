// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ws_conn — one client WebSocket over TLS, non-blocking
//!
//! The WebSocket counterpart of [`crate::HttpsConn`] for an exec
//! gateway (BX5: the transport under a WebSocket API session such as
//! Binance's `session.logon` / `order.place`): one socket on its
//! OWNER's `mio` poll.
//!
//! - [`WsConn::connect`] opens the TCP connection and returns at once;
//! - [`WsConn::on_event`] advances TLS, then the upgrade, then fills
//!   the receive window;
//! - [`WsConn::next_frame`] hands out each complete data frame IN
//!   PLACE (a span into the receive window) and answers the server's
//!   pings;
//! - [`WsConn::queue_text`] masks a frame straight into the send
//!   window, and [`WsConn::flush`] sends everything queued in ONE write
//!   — one TLS record per burst, not one per frame;
//! - [`WsConn::on_tick`] enforces the establishment deadline and the
//!   idle law (no inbound byte for `idle_ns`).
//!
//! The framing is [`WsFramer`]: no socket, so its hot half is testable
//! and alloc-gated on its own. Request ids for the session above it are
//! [`crate::ReqIds`].
//!
//! ## What it refuses
//!
//! A masked server frame (RFC 6455 §5.1), a FRAGMENTED message, a frame
//! larger than the receive window, and an upgrade whose
//! `Sec-WebSocket-Accept` is not the one our key demands. A venue that
//! fragments its JSON is one this client does not speak: refusing
//! loudly beats a reassembler no real traffic has tested (the rule
//! `exec_hyperliquid::userws_conn` set on its fill path).
//!
//! ## The owner's contract
//!
//! - After [`WsProgress::Opened`] or [`WsProgress::Readable`], drain
//!   [`WsConn::next_frame`] until [`WsNext::Idle`]. A data frame's span
//!   is valid until the next `next_frame` or `on_event`.
//! - The drain queues the pongs; they leave with the next
//!   [`WsConn::flush`]. Flush once per loop pass whenever
//!   [`WsConn::wants_flush`] — the gateway's "render the burst, flush
//!   once".
//! - [`WsConn::connect`] is called between poll batches, never while
//!   one is being dispatched (as [`crate::HttpsConn::start`]).

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use mio::event::Event;
use mio::{Registry, Token};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;

use crate::https_conn::{static_server_name, MAX_HOST, MAX_PATH};
use crate::iobuf::IoBuf;
use crate::transport::{Status, TlsTransport, Transport};
use crate::ws_frame::{
    ws_mask_from_counter, ws_read_frame, ws_write_pong, ws_write_text_frame_parts, PayloadSpan,
    WsOpcode, WsReadResult,
};
use crate::ws_handshake::{
    constant_time_eq, expected_accept, read_server_handshake, sec_websocket_key_from_seed,
    write_client_handshake, HandshakeResult,
};

/// The smallest window either direction may be built with: the
/// handshake request and the 101 must fit.
pub const MIN_WINDOW: usize = 1024;

/// Why a WebSocket session ended (or never began).
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WsErr {
    /// DNS resolution failed at construction.
    Dns,
    /// The host, path or a size was refused at construction.
    BadEndpoint,
    /// Connect, TLS, a read or a write failed, or the peer went away.
    Disconnected,
    /// The server refused the upgrade, or its accept was not ours.
    Upgrade,
    /// A frame violated RFC 6455 (a masked server frame included).
    BadFrame,
    /// A fragmented message — refused by policy (module doc).
    Fragmented,
    /// A frame larger than the receive window: it can never complete.
    TooLarge,
    /// The send window cannot take the frame.
    Overflow,
    /// The peer sent a Close frame.
    Closed,
    /// The establishment (TCP + TLS + upgrade) missed its deadline.
    Timeout,
    /// No inbound byte for the idle limit: the session is dead.
    Idle,
}

impl core::fmt::Display for WsErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Dns => "ws: dns resolution failed",
            Self::BadEndpoint => "ws: the host, path or a window size was refused",
            Self::Disconnected => "ws: connection lost",
            Self::Upgrade => "ws: the server refused the upgrade",
            Self::BadFrame => "ws: the server sent a frame that breaks RFC 6455",
            Self::Fragmented => "ws: the server fragmented a message (refused)",
            Self::TooLarge => "ws: a frame is larger than the receive window",
            Self::Overflow => "ws: the send window cannot take the frame",
            Self::Closed => "ws: the server closed the session",
            Self::Timeout => "ws: the session was not established in time",
            Self::Idle => "ws: no inbound byte for the idle limit",
        })
    }
}

impl std::error::Error for WsErr {}

/// What [`WsFramer::next_frame`] found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WsNext {
    /// No complete data frame is buffered.
    Idle,
    /// A whole text message: its payload is `payload(span)` until the
    /// next call.
    Text(PayloadSpan),
    /// A whole binary message, likewise.
    Binary(PayloadSpan),
    /// The session is over.
    Failed(WsErr),
}

#[inline(always)]
fn next_mask(ctr: &mut u64) -> [u8; 4] {
    *ctr = ctr.wrapping_add(1);
    ws_mask_from_counter(*ctr)
}

// ---------------------------------------------------------------
// WsFramer — the socket-free half
// ---------------------------------------------------------------

/// Client-side framing over a receive window and a send window.
pub struct WsFramer {
    rx: IoBuf,
    tx: IoBuf,
    rx_cap: usize,
    mask_ctr: u64,
    /// Bytes of the data frame last handed out, released by the next
    /// `next_frame` or receive-window fill.
    consumed: usize,
}

impl WsFramer {
    /// Allocate both windows. **Boot-only.** `mask_seed` starts the
    /// mask counter (RFC 6455 §10.3: masks must not be predictable).
    pub fn new(rx_cap: usize, tx_cap: usize, mask_seed: u64) -> Self {
        Self {
            rx: IoBuf::with_capacity(rx_cap),
            tx: IoBuf::with_capacity(tx_cap),
            rx_cap,
            mask_ctr: mask_seed,
            consumed: 0,
        }
    }

    #[inline(always)]
    fn release(&mut self) {
        if self.consumed != 0 {
            self.rx.consume(self.consumed);
            self.consumed = 0;
        }
    }

    /// Where received bytes go next (the last data frame is released
    /// first; the window compacts only when its tail is pinned —
    /// [`IoBuf::free_mut`]). Empty when the window is full.
    #[inline]
    pub fn rx_free_mut(&mut self) -> &mut [u8] {
        self.release();
        self.rx.free_mut()
    }

    /// Commit `n` bytes written into [`Self::rx_free_mut`].
    #[inline]
    pub fn rx_advance(&mut self, n: usize) {
        self.rx.advance(n);
    }

    /// The next complete data frame, answering pings and skipping pongs
    /// on the way (module doc: what is refused).
    pub fn next_frame(&mut self) -> WsNext {
        self.release();
        loop {
            let (fin, opcode, masked, span) = match ws_read_frame(self.rx.filled()) {
                WsReadResult::Incomplete => {
                    return if self.rx.len() >= self.rx_cap {
                        WsNext::Failed(WsErr::TooLarge)
                    } else {
                        WsNext::Idle
                    };
                }
                WsReadResult::Malformed => return WsNext::Failed(WsErr::BadFrame),
                WsReadResult::Frame { header, payload } => {
                    (header.fin, header.opcode, header.masked, payload)
                }
            };
            if masked {
                return WsNext::Failed(WsErr::BadFrame);
            }
            // The span is relative to the unread window, and the payload
            // ends the frame: `span.end` is the frame's whole length.
            match opcode {
                WsOpcode::Text | WsOpcode::Binary if fin => {
                    self.consumed = span.end;
                    return if opcode == WsOpcode::Text {
                        WsNext::Text(span)
                    } else {
                        WsNext::Binary(span)
                    };
                }
                WsOpcode::Text | WsOpcode::Binary | WsOpcode::Continuation => {
                    return WsNext::Failed(WsErr::Fragmented)
                }
                WsOpcode::Ping => {
                    let mask = next_mask(&mut self.mask_ctr);
                    // The echo goes straight from the receive window into
                    // the send window — two disjoint buffers, no scratch
                    // (`ws_write_pong` owns the one copy, marked there).
                    match ws_write_pong(
                        self.tx.free_mut(),
                        &self.rx.filled()[span.start..span.end],
                        mask,
                    ) {
                        Ok(n) => self.tx.advance(n),
                        Err(_) => return WsNext::Failed(WsErr::Overflow),
                    }
                    self.rx.consume(span.end);
                }
                WsOpcode::Pong => self.rx.consume(span.end),
                WsOpcode::Close => {
                    self.rx.consume(span.end);
                    return WsNext::Failed(WsErr::Closed);
                }
            }
        }
    }

    /// The payload of a frame [`Self::next_frame`] just returned.
    #[inline]
    #[must_use]
    pub fn payload(&self, span: PayloadSpan) -> &[u8] {
        &self.rx.filled()[span.start..span.end]
    }

    /// Mask one text frame — the concatenation of `parts` — straight
    /// into the send window.
    pub fn queue_text(&mut self, parts: &[&[u8]]) -> Result<(), WsErr> {
        let mask = next_mask(&mut self.mask_ctr);
        match ws_write_text_frame_parts(self.tx.free_mut(), parts, mask) {
            Ok(n) => {
                self.tx.advance(n);
                Ok(())
            }
            Err(_) => Err(WsErr::Overflow),
        }
    }

    /// Bytes queued for the wire.
    #[inline]
    #[must_use]
    pub fn tx_pending(&self) -> &[u8] {
        self.tx.filled()
    }

    /// Release `n` queued bytes once they are written.
    #[inline]
    pub fn tx_consume(&mut self, n: usize) {
        self.tx.consume(n);
    }

    /// Forget both windows (a new session).
    pub fn clear(&mut self) {
        self.rx.clear();
        self.tx.clear();
        self.consumed = 0;
    }
}

// ---------------------------------------------------------------
// WsConn — the socket
// ---------------------------------------------------------------

/// A session's windows and clocks, fixed at construction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WsCfg {
    /// The receive window, bytes: the largest frame the session reads.
    pub rx_cap: usize,
    /// The send window, bytes: the largest burst queued between flushes.
    pub tx_cap: usize,
    /// TCP + TLS + upgrade must complete inside this, ns.
    pub establish_ns: u64,
    /// No inbound byte for this long is a dead session, ns. Pick it
    /// above the server's ping interval (Binance's spot WS API pings
    /// every 20 s; its futures WS API every 3 min).
    pub idle_ns: u64,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WsPhase {
    Down,
    Dialing,
    Upgrading,
    Open,
}

/// What a step of the session produced.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WsProgress {
    /// Nothing to report.
    Waiting,
    /// The upgrade completed: the session is open (frames may already
    /// be buffered — drain them).
    Opened,
    /// Bytes arrived: drain [`WsConn::next_frame`].
    Readable,
    /// The session ended; the socket is closed.
    Failed(WsErr),
}

/// One client WebSocket over TLS, driven by its owner's poll (module
/// doc).
pub struct WsConn {
    phase: WsPhase,
    /// The peer closed the stream; frames already read still drain.
    eof: bool,
    /// The last fill stopped on a full receive window with bytes left
    /// behind: the drain reads on once it has made room (mio is
    /// edge-triggered; no second event comes for them).
    starved: bool,
    token: Token,
    deadline_ns: u64,
    last_rx_ns: u64,
    idle_ns: u64,
    establish_ns: u64,
    transport: Option<TlsTransport>,
    framer: WsFramer,
    sec_key: [u8; 24],
    seed: u64,
    addr: SocketAddr,
    /// Interned for the process's life (`https_conn::static_server_name`).
    host: &'static str,
    path: Box<str>,
    server_name: ServerName<'static>,
    tls_config: Arc<ClientConfig>,
    /// Successful TLS handshakes over the connection's life.
    dials: u64,
    /// Consecutive failed establishments (0 once one opens).
    fail_streak: u32,
}

impl WsConn {
    /// Resolve and allocate. **Boot-only.** Nothing is opened until
    /// [`Self::connect`]; `token` is the one this session's socket is
    /// registered under in the owner's poll; `seed` seeds the masks and
    /// the handshake keys.
    pub fn new(
        host: &str,
        port: u16,
        path: &str,
        tls_config: Arc<ClientConfig>,
        cfg: WsCfg,
        token: Token,
        seed: u64,
    ) -> Result<Self, WsErr> {
        let (h, p) = (host.as_bytes(), path.as_bytes());
        let host_ok = !h.is_empty() && h.len() <= MAX_HOST && visible(h);
        let path_ok = p.first() == Some(&b'/') && p.len() <= MAX_PATH && visible(p);
        if !host_ok
            || !path_ok
            || cfg.rx_cap < MIN_WINDOW
            || cfg.tx_cap < MIN_WINDOW
            || cfg.establish_ns == 0
            || cfg.idle_ns == 0
        {
            return Err(WsErr::BadEndpoint);
        }
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|_| WsErr::Dns)?
            .next()
            .ok_or(WsErr::Dns)?;
        let (host, server_name) = static_server_name(host).ok_or(WsErr::BadEndpoint)?;
        Ok(Self {
            phase: WsPhase::Down,
            eof: false,
            starved: false,
            token,
            deadline_ns: 0,
            last_rx_ns: 0,
            idle_ns: cfg.idle_ns,
            establish_ns: cfg.establish_ns,
            transport: None,
            framer: WsFramer::new(cfg.rx_cap, cfg.tx_cap, seed),
            sec_key: [0; 24],
            seed,
            addr,
            host,
            // The path (≤ MAX_PATH B), once at boot, rendered into every
            // handshake.
            path: path.into(),
            server_name,
            tls_config,
            dials: 0,
            fail_streak: 0,
        })
    }

    /// The session is open: frames may be queued and read.
    #[inline]
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.phase == WsPhase::Open
    }

    /// No socket at all: [`Self::connect`] may be called.
    #[inline]
    #[must_use]
    pub fn is_down(&self) -> bool {
        self.phase == WsPhase::Down
    }

    /// Successful TLS handshakes over the session's life.
    #[inline]
    #[must_use]
    pub const fn dials(&self) -> u64 {
        self.dials
    }

    /// Consecutive failed establishments: the owner's backoff cue.
    #[inline]
    #[must_use]
    pub const fn fail_streak(&self) -> u32 {
        self.fail_streak
    }

    /// The address the next [`Self::connect`] dials. There is no
    /// re-resolution in here: DNS blocks, and this session's owner serves
    /// other sockets — it resolves on a cold thread and hands the address
    /// in.
    #[inline]
    pub fn set_addr(&mut self, addr: SocketAddr) {
        self.addr = addr;
    }

    /// Drop the socket; the next [`Self::connect`] starts a new session.
    pub fn close(&mut self) {
        self.transport = None;
        self.phase = WsPhase::Down;
    }

    /// Open a new session: TCP now, then TLS and the upgrade through
    /// [`Self::on_event`]. A no-op unless the session is down.
    pub fn connect(&mut self, registry: &Registry, now_ns: u64) -> Result<(), WsErr> {
        if self.phase != WsPhase::Down {
            return Ok(());
        }
        self.framer.clear();
        self.eof = false;
        self.starved = false;
        self.sec_key = sec_websocket_key_from_seed(
            self.seed ^ now_ns ^ self.dials.rotate_left(32) ^ u64::from(self.fail_streak),
        );
        let t = TlsTransport::connect(self.addr, self.server_name.clone(), self.tls_config.clone());
        let mut t = match t {
            Ok(t) => t,
            Err(_) => {
                self.fail_streak = self.fail_streak.saturating_add(1);
                return Err(WsErr::Disconnected);
            }
        };
        if t.register(registry, self.token).is_err() {
            self.fail_streak = self.fail_streak.saturating_add(1);
            return Err(WsErr::Disconnected);
        }
        self.transport = Some(t);
        self.phase = WsPhase::Dialing;
        self.deadline_ns = now_ns.saturating_add(self.establish_ns);
        Ok(())
    }

    /// One readiness event for this session's token.
    pub fn on_event(&mut self, ev: &Event, registry: &Registry, now_ns: u64) -> WsProgress {
        match self.phase {
            WsPhase::Down => WsProgress::Waiting,
            WsPhase::Dialing => self.dial_event(ev, registry),
            WsPhase::Upgrading => self.upgrade_event(ev, registry, now_ns),
            WsPhase::Open => self.open_event(ev, registry, now_ns),
        }
    }

    /// The clocks: the establishment deadline, then the idle law.
    pub fn on_tick(&mut self, now_ns: u64) -> WsProgress {
        match self.phase {
            WsPhase::Down => WsProgress::Waiting,
            WsPhase::Dialing | WsPhase::Upgrading => {
                if now_ns >= self.deadline_ns {
                    self.fail_establish(WsErr::Timeout)
                } else {
                    WsProgress::Waiting
                }
            }
            WsPhase::Open => {
                if now_ns.saturating_sub(self.last_rx_ns) >= self.idle_ns {
                    self.fail(WsErr::Idle)
                } else {
                    WsProgress::Waiting
                }
            }
        }
    }

    /// The next complete data frame ([`WsFramer::next_frame`]); after a
    /// full window, reads on as room is made; `Failed` closes the
    /// socket.
    pub fn next_frame(&mut self) -> WsNext {
        if self.phase != WsPhase::Open {
            return WsNext::Idle;
        }
        loop {
            match self.framer.next_frame() {
                WsNext::Idle => {
                    if self.starved {
                        match self.read_more() {
                            Ok(0) => {}
                            Ok(_) => continue,
                            Err(e) => {
                                self.close();
                                return WsNext::Failed(e);
                            }
                        }
                    }
                    if self.eof {
                        self.close();
                        return WsNext::Failed(WsErr::Disconnected);
                    }
                    return WsNext::Idle;
                }
                WsNext::Failed(e) => {
                    self.close();
                    return WsNext::Failed(e);
                }
                data => return data,
            }
        }
    }

    /// The payload of a frame [`Self::next_frame`] just returned.
    #[inline]
    #[must_use]
    pub fn payload(&self, span: PayloadSpan) -> &[u8] {
        self.framer.payload(span)
    }

    /// Mask one text frame into the send window; it leaves with the
    /// next [`Self::flush`].
    pub fn queue_text(&mut self, parts: &[&[u8]]) -> Result<(), WsErr> {
        if self.phase != WsPhase::Open {
            return Err(WsErr::Disconnected);
        }
        self.framer.queue_text(parts)
    }

    /// Frames (or pongs) are queued and a socket is there to take them.
    #[inline]
    #[must_use]
    pub fn wants_flush(&self) -> bool {
        self.transport.is_some() && !self.framer.tx.is_empty()
    }

    /// Send everything queued in ONE write and push it to the socket.
    /// A failure closes the session.
    pub fn flush(&mut self, registry: &Registry) -> Result<(), WsErr> {
        if self.framer.tx.is_empty() {
            return Ok(());
        }
        match self.write_queued(registry) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.close();
                Err(e)
            }
        }
    }

    fn write_queued(&mut self, registry: &Registry) -> Result<(), WsErr> {
        let Some(t) = self.transport.as_mut() else {
            return Err(WsErr::Disconnected);
        };
        let n = self.framer.tx.len();
        let mut off = 0usize;
        while off < n {
            match t.write(&self.framer.tx.filled()[off..]) {
                Ok(0) => return Err(WsErr::Overflow),
                Ok(k) => off += k,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(WsErr::Overflow)
                }
                Err(_) => return Err(WsErr::Disconnected),
            }
        }
        self.framer.tx.consume(n);
        t.flush().map_err(|_| WsErr::Disconnected)?;
        t.reregister(registry, self.token)
            .map_err(|_| WsErr::Disconnected)
    }

    fn fail(&mut self, e: WsErr) -> WsProgress {
        self.close();
        WsProgress::Failed(e)
    }

    fn fail_establish(&mut self, e: WsErr) -> WsProgress {
        self.fail_streak = self.fail_streak.saturating_add(1);
        self.fail(e)
    }

    fn reregister(&mut self, registry: &Registry) -> bool {
        match self.transport.as_mut() {
            Some(t) => t.reregister(registry, self.token).is_ok(),
            None => false,
        }
    }

    fn dial_event(&mut self, ev: &Event, registry: &Registry) -> WsProgress {
        let st = match self.transport.as_mut() {
            Some(t) => match t.pump(ev) {
                Ok(s) => s,
                Err(_) => Status::Closed,
            },
            None => Status::Closed,
        };
        match st {
            Status::Ready => {
                self.dials += 1;
                // Rendered straight into the send window.
                let n = match write_client_handshake(
                    self.framer.tx.free_mut(),
                    self.host.as_bytes(),
                    self.path.as_bytes(),
                    &self.sec_key,
                ) {
                    Ok(n) => n,
                    Err(_) => return self.fail_establish(WsErr::Overflow),
                };
                self.framer.tx.advance(n);
                self.phase = WsPhase::Upgrading;
                match self.write_queued(registry) {
                    Ok(()) => WsProgress::Waiting,
                    Err(e) => self.fail_establish(e),
                }
            }
            Status::Handshaking => {
                if self.reregister(registry) {
                    WsProgress::Waiting
                } else {
                    self.fail_establish(WsErr::Disconnected)
                }
            }
            Status::Closed => self.fail_establish(WsErr::Disconnected),
        }
    }

    fn upgrade_event(&mut self, ev: &Event, registry: &Registry, now_ns: u64) -> WsProgress {
        if let Err(e) = self.fill(ev) {
            return self.fail_establish(e);
        }
        match read_server_handshake(self.framer.rx.filled()) {
            HandshakeResult::Upgraded {
                accept_start,
                accept_end,
                header_end,
            } => {
                let want = expected_accept(&self.sec_key);
                if !constant_time_eq(&self.framer.rx.filled()[accept_start..accept_end], &want) {
                    return self.fail_establish(WsErr::Upgrade);
                }
                // Anything after the head is already frames (a venue may
                // pack its first message with the 101): the cursor simply
                // starts after the head.
                self.framer.rx.consume(header_end);
                self.phase = WsPhase::Open;
                self.fail_streak = 0;
                self.last_rx_ns = now_ns;
                if self.reregister(registry) {
                    WsProgress::Opened
                } else {
                    self.fail(WsErr::Disconnected)
                }
            }
            HandshakeResult::Incomplete => {
                if self.eof || self.framer.rx.len() >= self.framer.rx_cap {
                    return self.fail_establish(WsErr::Upgrade);
                }
                if self.reregister(registry) {
                    WsProgress::Waiting
                } else {
                    self.fail_establish(WsErr::Disconnected)
                }
            }
            HandshakeResult::Malformed => self.fail_establish(WsErr::Upgrade),
        }
    }

    fn open_event(&mut self, ev: &Event, registry: &Registry, now_ns: u64) -> WsProgress {
        match self.fill(ev) {
            Ok(n) => {
                if n > 0 {
                    self.last_rx_ns = now_ns;
                }
                if !self.eof && !self.reregister(registry) {
                    return self.fail(WsErr::Disconnected);
                }
                if n > 0 || self.eof {
                    WsProgress::Readable
                } else {
                    WsProgress::Waiting
                }
            }
            Err(e) => self.fail(e),
        }
    }

    /// Pump the event, then read to `WouldBlock` (mio is edge-triggered).
    fn fill(&mut self, ev: &Event) -> Result<usize, WsErr> {
        let Some(t) = self.transport.as_mut() else {
            return Err(WsErr::Disconnected);
        };
        match t.pump(ev) {
            Ok(Status::Closed) => self.eof = true,
            Ok(_) => {}
            Err(_) => return Err(WsErr::Disconnected),
        }
        self.read_more()
    }

    /// Read what the socket holds into the receive window, until
    /// `WouldBlock`, EOF, or a full window (then `starved`).
    fn read_more(&mut self) -> Result<usize, WsErr> {
        let Some(t) = self.transport.as_mut() else {
            return Err(WsErr::Disconnected);
        };
        let mut got = 0usize;
        loop {
            let free = self.framer.rx_free_mut();
            if free.is_empty() {
                self.starved = true;
                return Ok(got);
            }
            match t.read(free) {
                Ok(0) => {
                    self.eof = true;
                    self.starved = false;
                    return Ok(got);
                }
                Ok(n) => {
                    self.framer.rx.advance(n);
                    got += n;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.starved = false;
                    return Ok(got);
                }
                // A FIN without `close_notify`: the frames already read
                // are authentic and whole-or-not by their own headers, so
                // they drain before the session ends.
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.eof = true;
                    self.starved = false;
                    return Ok(got);
                }
                Err(_) => return Err(WsErr::Disconnected),
            }
        }
    }
}

#[inline]
fn visible(s: &[u8]) -> bool {
    let mut i = 0;
    while i < s.len() {
        if !s[i].is_ascii_graphic() {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server frame, unmasked: FIN bit and opcode as given.
    fn frame(fin: bool, op: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(if fin { 0x80 } else { 0 }) | op];
        if payload.len() < 126 {
            v.push(payload.len() as u8);
        } else {
            v.push(126);
            v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        v.extend_from_slice(payload);
        v
    }

    fn feed(f: &mut WsFramer, bytes: &[u8]) {
        let free = f.rx_free_mut();
        free[..bytes.len()].copy_from_slice(bytes);
        f.rx_advance(bytes.len());
    }

    /// Unmask one client frame from the send window: (opcode, payload).
    fn unmask(wire: &[u8]) -> (u8, Vec<u8>, usize) {
        let op = wire[0] & 0x0F;
        assert!(wire[0] & 0x80 != 0, "FIN");
        assert!(wire[1] & 0x80 != 0, "a client frame is masked");
        let (len, mut at) = match wire[1] & 0x7F {
            126 => (u16::from_be_bytes([wire[2], wire[3]]) as usize, 4),
            127 => panic!("no 64-bit frames here"),
            n => (n as usize, 2),
        };
        let mask = [wire[at], wire[at + 1], wire[at + 2], wire[at + 3]];
        at += 4;
        let p = (0..len).map(|i| wire[at + i] ^ mask[i & 3]).collect();
        (op, p, at + len)
    }

    #[test]
    fn data_frames_come_out_in_place_and_pings_are_echoed() {
        let mut f = WsFramer::new(4096, 4096, 7);
        let mut wire = frame(true, 0x1, br#"{"id":"1","status":200}"#);
        wire.extend(frame(true, 0x9, b"ping-payload"));
        wire.extend(frame(true, 0xA, b"pong"));
        wire.extend(frame(true, 0x2, &[1, 2, 3]));
        feed(&mut f, &wire);
        let WsNext::Text(s) = f.next_frame() else { panic!("text") };
        assert_eq!(f.payload(s), br#"{"id":"1","status":200}"#);
        let WsNext::Binary(s) = f.next_frame() else { panic!("binary") };
        assert_eq!(f.payload(s), &[1, 2, 3]);
        assert_eq!(f.next_frame(), WsNext::Idle);
        let (op, p, n) = unmask(f.tx_pending());
        assert_eq!((op, p.as_slice()), (0xA, &b"ping-payload"[..]), "the pong echoes the ping");
        assert_eq!(n, f.tx_pending().len(), "exactly one pong, nothing else");
    }

    #[test]
    fn a_frame_split_across_reads_completes_when_its_last_byte_lands() {
        let mut f = WsFramer::new(4096, 4096, 1);
        let wire = frame(true, 0x1, &[b'x'; 300]);
        let mut at = 0;
        while at < wire.len() - 1 {
            feed(&mut f, &wire[at..at + 1]);
            assert_eq!(f.next_frame(), WsNext::Idle, "byte {at}");
            at += 1;
        }
        feed(&mut f, &wire[at..]);
        let WsNext::Text(s) = f.next_frame() else { panic!("text") };
        assert_eq!(f.payload(s).len(), 300);
    }

    #[test]
    fn what_this_client_does_not_speak_is_refused() {
        let cases: [(Vec<u8>, WsErr); 5] = [
            (frame(false, 0x1, b"part"), WsErr::Fragmented),
            (frame(true, 0x0, b"cont"), WsErr::Fragmented),
            (vec![0x81, 0x81, 1, 2, 3, 4, b'x'], WsErr::BadFrame),
            (vec![0xC1, 0x00], WsErr::BadFrame),
            (frame(true, 0x8, &[0x03, 0xE8]), WsErr::Closed),
        ];
        let mut i = 0;
        while i < cases.len() {
            let mut f = WsFramer::new(4096, 4096, 1);
            feed(&mut f, &cases[i].0);
            assert_eq!(f.next_frame(), WsNext::Failed(cases[i].1), "case {i}");
            i += 1;
        }
        // A frame the window can never hold.
        let mut f = WsFramer::new(1024, 1024, 1);
        let big = frame(true, 0x1, &[b'y'; 2000]);
        feed(&mut f, &big[..1024]);
        assert_eq!(f.next_frame(), WsNext::Failed(WsErr::TooLarge));
    }

    #[test]
    fn a_handed_out_frame_is_released_by_the_next_call_or_fill() {
        let mut f = WsFramer::new(1024, 1024, 1);
        // Fill the window with frames; each one handed out is released
        // before the window is written again, so an endless stream flows
        // through a fixed window.
        let one = frame(true, 0x1, &[b'z'; 100]);
        let mut seen = 0;
        while seen < 50 {
            if f.rx_free_mut().len() >= one.len() {
                feed(&mut f, &one);
            }
            match f.next_frame() {
                WsNext::Text(s) => {
                    assert_eq!(f.payload(s), &[b'z'; 100][..]);
                    seen += 1;
                }
                WsNext::Idle => {}
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn queued_text_is_one_masked_frame_per_call() {
        let mut f = WsFramer::new(1024, 1024, 9);
        f.queue_text(&[b"{\"id\":\"", b"7", b"\",\"method\":\"order.place\"}"]).expect("fits");
        f.queue_text(&[b"{}"]).expect("fits");
        let wire = f.tx_pending().to_vec();
        let (op, p, n) = unmask(&wire);
        assert_eq!((op, p.as_slice()), (0x1, &br#"{"id":"7","method":"order.place"}"#[..]));
        let (op2, p2, n2) = unmask(&wire[n..]);
        assert_eq!((op2, p2.as_slice()), (0x1, &b"{}"[..]));
        assert_eq!(n + n2, wire.len());
        f.tx_consume(wire.len());
        assert!(f.tx_pending().is_empty());
        assert_eq!(f.queue_text(&[&[0u8; 2000]]), Err(WsErr::Overflow));
    }

    #[test]
    fn a_session_refuses_bad_endpoints_at_construction() {
        let tls = TlsTransport::default_client_config();
        let cfg = WsCfg {
            rx_cap: 4096,
            tx_cap: 4096,
            establish_ns: 1,
            idle_ns: 1,
        };
        let mk = |host: &str, path: &str, c: WsCfg| {
            WsConn::new(host, 1, path, tls.clone(), c, Token(1), 5).err()
        };
        assert_eq!(mk("no-such-host.invalid.example", "/ws", cfg), Some(WsErr::Dns));
        assert_eq!(mk("127.0.0.1", "ws", cfg), Some(WsErr::BadEndpoint));
        assert_eq!(mk("127.0.0.1", "/w s", cfg), Some(WsErr::BadEndpoint));
        assert_eq!(mk("127.0.0.1", "/ws", WsCfg { rx_cap: 100, ..cfg }), Some(WsErr::BadEndpoint));
        assert_eq!(mk("127.0.0.1", "/ws", WsCfg { idle_ns: 0, ..cfg }), Some(WsErr::BadEndpoint));
        let c = WsConn::new("127.0.0.1", 1, "/ws", tls.clone(), cfg, Token(1), 5).expect("ok");
        assert!(c.is_down() && !c.is_open() && !c.wants_flush());
    }

    #[test]
    fn errors_render_for_an_operator() {
        let all = [
            WsErr::Dns,
            WsErr::BadEndpoint,
            WsErr::Disconnected,
            WsErr::Upgrade,
            WsErr::BadFrame,
            WsErr::Fragmented,
            WsErr::TooLarge,
            WsErr::Overflow,
            WsErr::Closed,
            WsErr::Timeout,
            WsErr::Idle,
        ];
        let mut i = 0;
        while i < all.len() {
            let m = all[i].to_string();
            assert!(m.starts_with("ws: ") && m.len() > 12, "{m}");
            i += 1;
        }
    }
}

#[cfg(test)]
mod proptests {
    //! The framer walks bytes a server chose: whatever arrives, in
    //! whatever pieces, it never panics, never hands out a span outside
    //! the window, and never writes anything but whole masked pongs.
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn next_frame_never_panics_on_arbitrary_input(
            chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..300), 1..8),
        ) {
            let mut f = WsFramer::new(1024, 4096, 3);
            let mut k = 0;
            while k < chunks.len() {
                let c = &chunks[k];
                let free = f.rx_free_mut();
                let n = c.len().min(free.len());
                free[..n].copy_from_slice(&c[..n]);
                f.rx_advance(n);
                let mut guard = 0;
                loop {
                    guard += 1;
                    prop_assert!(guard < 10_000);
                    match f.next_frame() {
                        WsNext::Text(s) | WsNext::Binary(s) => {
                            prop_assert!(s.start <= s.end);
                            let _ = f.payload(s);
                        }
                        WsNext::Idle => break,
                        WsNext::Failed(_) => return Ok(()),
                    }
                }
                k += 1;
            }
            // Whatever was queued is whole pongs.
            let tx = f.tx_pending().to_vec();
            let mut at = 0;
            while at < tx.len() {
                prop_assert_eq!(tx[at], 0x8A);
                let len = (tx[at + 1] & 0x7F) as usize;
                prop_assert!(tx[at + 1] & 0x80 != 0 && len <= 125);
                at += 2 + 4 + len;
            }
            prop_assert_eq!(at, tx.len());
        }

        #[test]
        fn text_frames_round_trip_in_any_split(
            payloads in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..200), 1..6),
            cut in 1usize..64,
        ) {
            let mut wire = Vec::new();
            let mut i = 0;
            while i < payloads.len() {
                let p = &payloads[i];
                wire.push(0x81);
                if p.len() < 126 {
                    wire.push(p.len() as u8);
                } else {
                    wire.push(126);
                    wire.extend_from_slice(&(p.len() as u16).to_be_bytes());
                }
                wire.extend_from_slice(p);
                i += 1;
            }
            let mut f = WsFramer::new(4096, 4096, 3);
            let mut got: Vec<Vec<u8>> = Vec::new();
            let mut at = 0;
            while at < wire.len() {
                let n = cut.min(wire.len() - at);
                let free = f.rx_free_mut();
                free[..n].copy_from_slice(&wire[at..at + n]);
                f.rx_advance(n);
                at += n;
                loop {
                    match f.next_frame() {
                        WsNext::Text(s) => got.push(f.payload(s).to_vec()),
                        WsNext::Idle => break,
                        other => return Err(TestCaseError::fail(format!("{other:?}"))),
                    }
                }
            }
            prop_assert_eq!(got, payloads);
        }
    }
}
