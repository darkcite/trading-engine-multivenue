// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The keep-alive HTTPS connection engine behind [`crate::HttpsPost`]
//! (one endpoint, `POST`) and [`crate::HttpsReq`] (one host, any method
//! and target): the dial, the idle-close probe, one request/response
//! exchange, and the retire rules.
//!
//! HC2 moved this out of `HttpsPost` unchanged — the same mio + rustls
//! cycle, the same buffers allocated at construction, the same
//! `left_host` law — so the two clients share ONE implementation of the
//! part that decides whether a request may have reached the venue.
//! `HttpsPost`'s behaviour and allocation profile are what they were
//! (gate 72 pins it at exactly two rustls record buffers per post).
//!
//! ## Keep-alive that does not lie about `left_host`
//!
//! A kept-alive connection the server has since closed (its idle
//! timeout; `Connection: close`) would take the next request's bytes
//! into a dead socket and the read would then report "maybe sent" — a
//! false [`PostErr::left_host`]. So the connection is retired when the
//! answer says `Connection: close` or the peer closes with it, and a
//! one-byte non-blocking read before each reuse (one `read(2)` through
//! rustls) retires one the peer closed while idle: the request then
//! dials fresh with `left_host == false`. Every dial is counted
//! ([`KeepAlive::dials`]).

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::{Events, Poll, Token};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;

use crate::http1::{
    chunked_body, head_says_close, read_response, BodyFraming, ChunkedBody, HttpResult,
};
use crate::https_post::{PostErr, PostErrKind, MAX_HOST};
use crate::transport::{Status, TlsTransport, Transport};

const MIO_TOKEN: Token = Token(0);
const POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// Consecutive connect failures between DNS re-resolutions (a
/// CDN-fronted endpoint rotates addresses).
const RERESOLVE_AFTER: u32 = 3;

/// One keep-alive TLS connection to one `host:port`, and the response
/// buffer its answers land in.
pub(crate) struct KeepAlive {
    /// Visible ASCII, ≤ [`MAX_HOST`] — checked at construction.
    host: Box<str>,
    port: u16,
    addr: SocketAddr,
    connect_fail_streak: u32,
    /// Successful dials (TCP + TLS handshakes) over the connection's life.
    dials: u64,
    server_name: ServerName<'static>,
    tls_config: Arc<ClientConfig>,
    /// `None` until the first request; reopened after a disconnect.
    transport: Option<TlsTransport>,
    poll: Poll,
    events: Events,
    resp_buf: Box<[u8]>,
    resp_len: usize,
}

impl KeepAlive {
    /// Resolve and prepare; **boot-only** (the allocations live here).
    /// No socket is opened until the first exchange.
    pub(crate) fn new(
        host: &str,
        port: u16,
        tls_config: Arc<ClientConfig>,
        resp_cap: usize,
    ) -> Result<Self, PostErrKind> {
        if host.is_empty() || host.len() > MAX_HOST || !host.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(PostErrKind::BadEndpoint);
        }
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|_| PostErrKind::Dns)?
            .next()
            .ok_or(PostErrKind::Dns)?;
        let server_name =
            TlsTransport::server_name_from_host(host).map_err(|_| PostErrKind::BadEndpoint)?;
        Ok(Self {
            host: host.into(),
            port,
            addr,
            connect_fail_streak: 0,
            dials: 0,
            server_name,
            tls_config,
            transport: None,
            poll: Poll::new().map_err(|_| PostErrKind::Disconnected)?,
            events: Events::with_capacity(8),
            resp_buf: vec![0u8; resp_cap].into_boxed_slice(),
            resp_len: 0,
        })
    }

    /// The host this connection talks to.
    #[inline]
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// Drop the connection. The next exchange redials.
    pub(crate) fn close(&mut self) {
        self.transport = None;
        self.resp_len = 0;
    }

    /// Successful dials over the connection's life.
    #[inline]
    pub(crate) const fn dials(&self) -> u64 {
        self.dials
    }

    /// Whether a connection is currently open.
    #[inline]
    pub(crate) fn is_connected(&self) -> bool {
        self.transport.is_some()
    }

    /// The response bytes of the last successful exchange.
    #[inline]
    pub(crate) fn resp(&self) -> &[u8] {
        &self.resp_buf[..self.resp_len]
    }

    /// One request/response cycle: write `wire` (the whole request, ONE
    /// contiguous slice — one TLS record while it fits one) and read one
    /// whole answer: `(http_status, body_range)` into [`Self::resp`].
    /// A failure before the write is `left_host == false`; from the
    /// first write attempt on it is `true`. Any failure closes the
    /// connection — a half-read answer left in the buffer would be read
    /// as the NEXT request's.
    pub(crate) fn exchange(
        &mut self,
        wire: &[u8],
        deadline: Instant,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.ensure_connected(deadline).map_err(|err| PostErr {
            err,
            left_host: false,
        })?;
        let mut left_host = false;
        match self.cycle(wire, deadline, &mut left_host) {
            Ok((status, range, retire)) => {
                if retire {
                    // The peer closed, or said it will: the answer is
                    // whole, the connection is not reusable. Keep the
                    // response; drop only the transport.
                    self.transport = None;
                }
                Ok((status, range))
            }
            Err(err) => {
                self.close();
                Err(PostErr { err, left_host })
            }
        }
    }

    fn cycle(
        &mut self,
        wire: &[u8],
        deadline: Instant,
        left_host: &mut bool,
    ) -> Result<(u16, core::ops::Range<usize>, bool), PostErrKind> {
        {
            let t = self.transport.as_mut().ok_or(PostErrKind::Disconnected)?;
            *left_host = true;
            write_all(t, wire, deadline)?;
            // `write` only queues into rustls; flush now (no writable
            // edge is armed after the handshake — without this the first
            // poll sleeps a full POLL_TIMEOUT: HlHttp's E7 finding).
            t.flush().map_err(|_| PostErrKind::Disconnected)?;
            t.reregister(self.poll.registry(), MIO_TOKEN)
                .map_err(|_| PostErrKind::Disconnected)?;
        }
        self.read_response(deadline)
    }

    fn ensure_connected(&mut self, deadline: Instant) -> Result<(), PostErrKind> {
        if self.transport.is_some() && !self.still_open() {
            self.transport = None;
        }
        if self.transport.is_some() {
            return Ok(());
        }
        match self.dial(deadline) {
            Ok(()) => {
                self.connect_fail_streak = 0;
                Ok(())
            }
            Err(e) => {
                self.connect_fail_streak = self.connect_fail_streak.saturating_add(1);
                if self.connect_fail_streak % RERESOLVE_AFTER == 0 {
                    // Blocking DNS on the failure path only; best effort.
                    if let Some(a) = (&*self.host, self.port)
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

    /// Before reusing a kept-alive connection: one non-blocking read.
    /// No request is outstanding, so anything but `WouldBlock` — EOF,
    /// `close_notify`, an unsolicited byte, an error — means the peer
    /// is gone or out of step, and the request must dial fresh.
    fn still_open(&mut self) -> bool {
        let Some(t) = self.transport.as_mut() else {
            return false;
        };
        let mut probe = [0u8; 1];
        matches!(t.read(&mut probe), Err(ref e) if e.kind() == io::ErrorKind::WouldBlock)
    }

    fn dial(&mut self, deadline: Instant) -> Result<(), PostErrKind> {
        let mut t =
            TlsTransport::connect(self.addr, self.server_name.clone(), self.tls_config.clone())
                .map_err(|_| PostErrKind::Disconnected)?;
        t.register(self.poll.registry(), MIO_TOKEN)
            .map_err(|_| PostErrKind::Disconnected)?;
        loop {
            if Instant::now() >= deadline {
                return Err(PostErrKind::Timeout);
            }
            self.poll
                .poll(&mut self.events, Some(POLL_TIMEOUT))
                .map_err(|_| PostErrKind::Disconnected)?;
            let mut status = Status::Handshaking;
            for ev in self.events.iter() {
                if ev.token() == MIO_TOKEN {
                    status = t.pump(ev).map_err(|_| PostErrKind::Disconnected)?;
                }
            }
            match status {
                Status::Ready => break,
                Status::Closed => return Err(PostErrKind::Disconnected),
                _ => t
                    .reregister(self.poll.registry(), MIO_TOKEN)
                    .map_err(|_| PostErrKind::Disconnected)?,
            }
        }
        self.transport = Some(t);
        self.dials += 1;
        Ok(())
    }

    /// Read one whole answer: `(status, body_range, retire)` — `retire`
    /// when the peer closed with it or announced `Connection: close`.
    fn read_response(
        &mut self,
        deadline: Instant,
    ) -> Result<(u16, core::ops::Range<usize>, bool), PostErrKind> {
        self.resp_len = 0;
        let mut peer_closed = false;
        loop {
            if Instant::now() >= deadline {
                return Err(PostErrKind::Timeout);
            }
            self.poll
                .poll(&mut self.events, Some(POLL_TIMEOUT))
                .map_err(|_| PostErrKind::Disconnected)?;
            {
                let t = self.transport.as_mut().ok_or(PostErrKind::Disconnected)?;
                for ev in self.events.iter() {
                    if ev.token() == MIO_TOKEN
                        && t.pump(ev).map_err(|_| PostErrKind::Disconnected)? == Status::Closed
                    {
                        peer_closed = true;
                    }
                }
                // Drain every plaintext byte available on this readiness.
                while self.resp_len < self.resp_buf.len() {
                    let cap = self.resp_buf.len();
                    match t.read(&mut self.resp_buf[self.resp_len..cap]) {
                        Ok(0) => {
                            peer_closed = true;
                            break;
                        }
                        Ok(n) => self.resp_len += n,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => return Err(PostErrKind::Disconnected),
                    }
                }
            }
            match read_response(&self.resp_buf[..self.resp_len]) {
                HttpResult::Complete {
                    status,
                    header_end,
                    body_start,
                    body_end,
                    framing,
                } => {
                    let retire = peer_closed || head_says_close(&self.resp_buf[..header_end]);
                    match framing {
                        // One framing this client can bound. A truncated
                        // body must never be handed on as the whole answer.
                        BodyFraming::ContentLength(_) => {
                            if self.resp_len >= body_end {
                                return Ok((status, body_start..body_end, retire));
                            }
                            if peer_closed {
                                return Err(PostErrKind::Disconnected);
                            }
                        }
                        BodyFraming::CloseDelimited => {
                            if peer_closed {
                                return Ok((status, body_start..self.resp_len, true));
                            }
                        }
                        // Located once the terminating chunk has arrived
                        // (`chunked_body` leaves an incomplete body
                        // untouched, so every read can retry it); a
                        // one-chunk body is not moved. The archive
                        // endpoint (purroof) answers chunked.
                        BodyFraming::Chunked => {
                            let end = self.resp_len;
                            match chunked_body(&mut self.resp_buf[body_start..end]) {
                                ChunkedBody::Span { start, len } => {
                                    let a = body_start + start;
                                    return Ok((status, a..a + len, retire));
                                }
                                ChunkedBody::Incomplete => {
                                    if peer_closed {
                                        return Err(PostErrKind::Disconnected);
                                    }
                                }
                                ChunkedBody::Malformed => return Err(PostErrKind::BadHttp),
                            }
                        }
                    }
                }
                HttpResult::Incomplete => {
                    if peer_closed {
                        return Err(PostErrKind::Disconnected);
                    }
                }
                HttpResult::Malformed => return Err(PostErrKind::BadHttp),
            }
            if self.resp_len >= self.resp_buf.len() {
                return Err(PostErrKind::Overflow);
            }
            let t = self.transport.as_mut().ok_or(PostErrKind::Disconnected)?;
            t.reregister(self.poll.registry(), MIO_TOKEN)
                .map_err(|_| PostErrKind::Disconnected)?;
        }
    }
}

/// Write all of `src`, resuming across partial writes. Bounded by
/// `deadline`; a TLS transport never blocks on `write`, so `WouldBlock`
/// is a buffer the request did not fit.
fn write_all<T: Transport>(t: &mut T, src: &[u8], deadline: Instant) -> Result<(), PostErrKind> {
    let mut off = 0usize;
    while off < src.len() {
        if Instant::now() >= deadline {
            return Err(PostErrKind::Timeout);
        }
        match t.write(&src[off..]) {
            Ok(0) => return Err(PostErrKind::Overflow),
            Ok(n) => off += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(PostErrKind::Overflow)
            }
            Err(_) => return Err(PostErrKind::Disconnected),
        }
    }
    Ok(())
}
