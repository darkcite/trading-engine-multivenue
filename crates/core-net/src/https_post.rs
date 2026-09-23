// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A keep-alive HTTPS `POST` client for ONE endpoint — host, port and
//! path fixed at construction (HYPARB H7c: the HyperEVM JSON-RPC write
//! arm, `https://rpc.hyperliquid-testnet.xyz/evm`).
//!
//! The shape is `exec_hyperliquid::http::HlHttp`'s, generalised only in
//! that the path comes from the boot URL instead of two compiled
//! constants: mio + rustls, HTTP/1.1 `Connection: keep-alive`, every
//! buffer allocated at construction, one synchronous request/response
//! cycle per call on the caller's own worker thread (never the engine
//! loop — a cycle may block up to [`REQ_DEADLINE`]). `Content-Length`
//! and `chunked` framing let the reader stop at the body's end, so the
//! connection survives for the next request and the TCP+TLS handshake
//! is paid once (a chunked body is decoded in place — `HlHttp` refuses
//! chunked; this client's endpoints include one that sends it).
//! **`HlHttp` itself is deliberately NOT migrated onto this type in the
//! HYPARB lane**: it is the armed-live E-lane arm, and changing it is
//! that lane's decision under its own review.
//!
//! ## One request, one write, one TLS record (HYPARB H9)
//!
//! rustls' buffered API seals every `write` call into its own record and
//! allocates that record's `Vec` (measured: 1 allocation per write, per
//! post, on top of 1 per received record). So the request is ONE
//! contiguous slice written ONCE — one record while it is ≤ 16 KiB
//! (the write arm's are ≤ 8 KiB; a larger one is split by rustls, still
//! from one write):
//!
//! ```text
//! [ pad | POST <path> HTTP/1.1 … Content-Length: | digits | \r\n\r\n | body … ]
//!         ^ prefix, rendered once at boot           ^ per post   ^ fixed   ^ body_at
//! ```
//!
//! The caller renders the body in place through [`HttpsPost::body_mut`]
//! (the doctrine's "encoders render into the FINAL wire buffer"); the
//! only per-post render is the length digits. The prefix sits flush
//! against the digits, so it moves (a ≤ 605 B `copy_within` inside one
//! cache-resident buffer) whenever the body's DIGIT COUNT differs from
//! the last request's — for the write arm, a 2-digit `eth_feeHistory`
//! between 3-digit sends and receipt polls: about twice per 2 s while a
//! swap is in flight. Two ways to never move it were weighed and
//! rejected: a fixed-width `Content-Length` — space-padded (OWS inside
//! the value) or zero-padded (valid `1*DIGIT` per RFC 9110) — is a
//! parser corner neither live endpoint (CloudFront, purroof) was shown
//! to accept, and a request that one of them refuses is a lost send;
//! padding short JSON bodies with whitespace puts bytes on the wire on
//! every small request. Both cost more than the move.
//!
//! ## Keep-alive that does not lie about `left_host`
//!
//! A kept-alive connection the server has since closed (its idle
//! timeout; `Connection: close`) would take the next request's bytes
//! into a dead socket and the read would then report "maybe sent" — a
//! false [`PostErr::left_host`] that costs the send arm a wallet for a
//! receipt timeout. So the connection is retired when the answer says
//! `Connection: close` or the peer closes with it, and a one-byte
//! non-blocking read before each reuse (one `read(2)` through rustls)
//! retires one the peer closed while idle: the request then dials fresh
//! with `left_host == false`. Every dial is counted ([`HttpsPost::dials`]):
//! an endpoint that closes after every answer shows as dials ≈ posts —
//! a full TLS handshake, and its allocations, per request.
//!
//! ## What this layer does NOT decide
//!
//! Whether the server accepted anything. JSON-RPC refusals arrive with
//! HTTP 200 and an `"error"` member; judging the body is the caller's
//! scanner's job. This layer returns the status and the body's byte
//! range, and — on failure — whether any byte of the request may have
//! reached the wire ([`PostErr::left_host`]), because a transaction
//! whose request left the host may have been accepted even though no
//! answer came back.

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
use crate::transport::{Status, TlsTransport, Transport};

/// The longest path a boot URL may carry.
pub const MAX_PATH: usize = 256;
/// The longest host (DNS's own bound).
pub const MAX_HOST: usize = 253;
/// The largest body a client may be built for (7 length digits).
pub const MAX_BODY_CAP: usize = 9_999_999;
const PREFIX_A: &[u8] = b"POST ";
const PREFIX_B: &[u8] = b" HTTP/1.1\r\nHost: ";
const PREFIX_C: &[u8] =
    b"\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: ";
const HEAD_END: &[u8] = b"\r\n\r\n";
const MIO_TOKEN: Token = Token(0);
const POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// Consecutive connect failures between DNS re-resolutions (a
/// CDN-fronted endpoint rotates addresses).
const RERESOLVE_AFTER: u32 = 3;
/// One request's whole budget: connect, write, read.
pub const REQ_DEADLINE: Duration = Duration::from_secs(5);

/// Why an HTTPS cycle failed.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PostErrKind {
    /// DNS resolution failed at construction.
    Dns,
    /// rustls rejected the host as a server name, or the URL / path was
    /// refused at construction.
    BadEndpoint,
    /// Connect, handshake, write or read failed, or the peer went away
    /// mid-response. The connection is closed; the next call redials.
    Disconnected,
    /// The request or the response did not fit its buffer.
    Overflow,
    /// The response was not well-framed HTTP/1.1 (malformed headers
    /// or chunk framing).
    BadHttp,
    /// The whole cycle exceeded [`REQ_DEADLINE`].
    Timeout,
}

impl core::fmt::Display for PostErrKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Dns => "https: dns resolution failed",
            Self::BadEndpoint => "https: the endpoint URL or host was refused",
            Self::Disconnected => "https: connection lost",
            Self::Overflow => "https: request or response did not fit its buffer",
            Self::BadHttp => "https: response was not bounded HTTP/1.1",
            Self::Timeout => "https: request deadline exceeded",
        })
    }
}

impl std::error::Error for PostErrKind {}

/// A failed request and **whether any byte of it may have reached the
/// socket** — set BEFORE the write is attempted, so a torn write counts
/// (a caller that must not double-spend reads `left_host == true` as
/// "the server may have it").
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PostErr {
    /// What went wrong.
    pub err: PostErrKind,
    /// Any byte of this request may have left the host.
    pub left_host: bool,
}

impl core::fmt::Display for PostErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.left_host {
            write!(f, "{} (the request had already left the host)", self.err)
        } else {
            write!(f, "{}", self.err)
        }
    }
}

impl std::error::Error for PostErr {}

/// `https://host[:port][/path]` → `(host, port, path)`; the path
/// defaults to `/`. Boot-only. `None` on any other scheme, an empty
/// host, a bad port, a query/fragment, or a path over [`MAX_PATH`].
#[must_use]
pub fn parse_https_url(url: &str) -> Option<(&str, u16, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if path.len() > MAX_PATH || path.contains(['?', '#', ' ']) {
        return None;
    }
    let (host, port) = match authority.rfind(':') {
        Some(i) => (&authority[..i], authority[i + 1..].parse::<u16>().ok()?),
        None => (authority, 443),
    };
    if host.is_empty() || port == 0 || host.contains(['@', '[', ']']) {
        return None;
    }
    Some((host, port, path))
}

/// A keep-alive HTTPS connection to one endpoint.
pub struct HttpsPost {
    port: u16,
    addr: SocketAddr,
    connect_fail_streak: u32,
    /// Successful dials (TCP + TLS handshakes) over the client's life.
    dials: u64,
    server_name: ServerName<'static>,
    tls_config: Arc<ClientConfig>,
    /// `None` until the first request; reopened after a disconnect.
    transport: Option<TlsTransport>,
    poll: Poll,
    events: Events,
    /// `[pad | prefix | digits | CRLFCRLF | body]` — module doc.
    req: Box<[u8]>,
    /// Where the prefix currently starts (it moves only when the
    /// body's digit count changes).
    head_at: usize,
    /// Prefix length; the path is `prefix[5..5 + path_len]`, the host
    /// `prefix[host_off..host_off + host_len]`.
    prefix_len: usize,
    path_len: usize,
    host_off: usize,
    host_len: usize,
    /// Digits of the largest body (`body_cap`); the pad is sized for it.
    max_digits: usize,
    /// First body byte in `req`.
    body_at: usize,
    resp_buf: Box<[u8]>,
    resp_len: usize,
}

/// Decimal digits of `v` (≥ 1).
#[inline]
const fn dec_digits(mut v: usize) -> usize {
    let mut d = 1;
    while v >= 10 {
        v /= 10;
        d += 1;
    }
    d
}

impl HttpsPost {
    /// Resolve and prepare: the request prefix is rendered here, once,
    /// and the body window ([`Self::body_mut`]) is `body_cap` bytes.
    /// **Boot-only** — the allocations live here. No socket is opened
    /// until the first request.
    pub fn new(
        host: &str,
        port: u16,
        path: &str,
        tls_config: Arc<ClientConfig>,
        body_cap: usize,
        resp_cap: usize,
    ) -> Result<Self, PostErrKind> {
        // The request target is sent verbatim: visible ASCII only.
        if !path.starts_with('/')
            || path.len() > MAX_PATH
            || !path.bytes().all(|b| b.is_ascii_graphic())
            || host.is_empty()
            || host.len() > MAX_HOST
            || !host.bytes().all(|b| b.is_ascii_graphic())
            || body_cap == 0
            || body_cap > MAX_BODY_CAP
        {
            return Err(PostErrKind::BadEndpoint);
        }
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|_| PostErrKind::Dns)?
            .next()
            .ok_or(PostErrKind::Dns)?;
        let server_name =
            TlsTransport::server_name_from_host(host).map_err(|_| PostErrKind::BadEndpoint)?;
        let prefix_len = PREFIX_A.len() + path.len() + PREFIX_B.len() + host.len() + PREFIX_C.len();
        let max_digits = dec_digits(body_cap);
        let body_at = prefix_len + max_digits + HEAD_END.len();
        let mut req = vec![0u8; body_at + body_cap].into_boxed_slice();
        let parts: [&[u8]; 5] = [
            PREFIX_A,
            path.as_bytes(),
            PREFIX_B,
            host.as_bytes(),
            PREFIX_C,
        ];
        let mut at = 0usize;
        let mut i = 0usize;
        while i < parts.len() {
            let end = at + parts[i].len();
            // COPY: the request prefix (≤ 5 + MAX_PATH + 17 + MAX_HOST +
            // 74 = 605 B) rendered ONCE at boot into the wire buffer it is
            // sent from — no request ever re-renders it — rejected:
            // borrowing the boot URL would pin the config for life.
            req[at..end].copy_from_slice(parts[i]);
            at = end;
            i += 1;
        }
        // COPY: 4 B `\r\n\r\n`, once at boot, at its fixed place before
        // the body — rejected: rendering it per post.
        req[body_at - HEAD_END.len()..body_at].copy_from_slice(HEAD_END);
        let host_off = PREFIX_A.len() + path.len() + PREFIX_B.len();
        Ok(Self {
            port,
            addr,
            connect_fail_streak: 0,
            dials: 0,
            server_name,
            tls_config,
            transport: None,
            poll: Poll::new().map_err(|_| PostErrKind::Disconnected)?,
            events: Events::with_capacity(8),
            req,
            // Rendered for the widest length; `stage` moves it right.
            head_at: 0,
            prefix_len,
            path_len: path.len(),
            host_off,
            host_len: host.len(),
            max_digits,
            body_at,
            resp_buf: vec![0u8; resp_cap].into_boxed_slice(),
            resp_len: 0,
        })
    }

    /// The prefix bytes `[off, off + len)` as text (ASCII by
    /// construction — [`Self::new`] refuses anything else).
    fn prefix_str(&self, off: usize, len: usize) -> &str {
        let at = self.head_at + off;
        core::str::from_utf8(&self.req[at..at + len]).unwrap_or("")
    }

    /// The host this client talks to (boot tells).
    #[inline]
    #[must_use]
    pub fn host(&self) -> &str {
        self.prefix_str(self.host_off, self.host_len)
    }

    /// The fixed request path.
    #[inline]
    #[must_use]
    pub fn path(&self) -> &str {
        self.prefix_str(PREFIX_A.len(), self.path_len)
    }

    /// The body window's size (`body_cap` at construction).
    #[inline]
    #[must_use]
    pub fn body_cap(&self) -> usize {
        self.req.len() - self.body_at
    }

    /// The body window: render the request body here, then
    /// [`Self::post`] its length. Its contents survive a post.
    #[inline]
    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.req[self.body_at..]
    }

    /// Drop the connection. The next request redials.
    pub fn close(&mut self) {
        self.transport = None;
        self.resp_len = 0;
    }

    /// Successful dials over the client's life: 1 on a healthy
    /// keep-alive endpoint; ≈ posts on one that closes after every
    /// answer (a handshake per request).
    #[inline]
    #[must_use]
    pub const fn dials(&self) -> u64 {
        self.dials
    }

    /// Whether a connection is currently open.
    #[inline]
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.transport.is_some()
    }

    /// The response bytes of the last successful [`Self::post`].
    #[inline]
    #[must_use]
    pub fn resp(&self) -> &[u8] {
        &self.resp_buf[..self.resp_len]
    }

    /// Place the head flush against a `body_len`-byte body and render its
    /// length: the range of `req` to write. Moves the prefix only when
    /// the digit count differs from the last request's.
    fn stage(&mut self, body_len: usize) -> Result<core::ops::Range<usize>, PostErrKind> {
        if body_len > self.req.len() - self.body_at {
            return Err(PostErrKind::Overflow);
        }
        let d = dec_digits(body_len);
        let want = self.max_digits - d;
        if want != self.head_at {
            // COPY: the ≤ 605 B prefix, shifted within the wire buffer
            // when the body's digit COUNT differs from the last request's
            // (never between two bodies of one order of magnitude) —
            // rejected: a fixed-width Content-Length (space- or zero-
            // padded: unproven against the live endpoints' parsers) and
            // whitespace-padded bodies (wire bytes on every request).
            self.req
                .copy_within(self.head_at..self.head_at + self.prefix_len, want);
            self.head_at = want;
        }
        let digits_end = self.body_at - HEAD_END.len();
        let mut v = body_len;
        let mut k = digits_end;
        while k > digits_end - d {
            k -= 1;
            self.req[k] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        Ok(self.head_at..self.body_at + body_len)
    }

    /// One request/response cycle for the first `body_len` bytes of
    /// [`Self::body_mut`]: `(http_status, body_range)` into
    /// [`Self::resp`]. **The status is not a verdict.**
    pub fn post(&mut self, body_len: usize) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        let deadline = Instant::now() + REQ_DEADLINE;
        let not_left = |err| PostErr {
            err,
            left_host: false,
        };
        let wire = self.stage(body_len).map_err(not_left)?;
        self.ensure_connected(deadline).map_err(not_left)?;
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
                // Any failure closes the connection: a half-read answer
                // left in the buffer would be read as the NEXT request's.
                self.close();
                Err(PostErr { err, left_host })
            }
        }
    }

    fn cycle(
        &mut self,
        wire: core::ops::Range<usize>,
        deadline: Instant,
        left_host: &mut bool,
    ) -> Result<(u16, core::ops::Range<usize>, bool), PostErrKind> {
        {
            let t = self.transport.as_mut().ok_or(PostErrKind::Disconnected)?;
            *left_host = true;
            write_all(t, &self.req[wire], deadline)?;
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
                    let port = self.port;
                    if let Some(a) = (self.host(), port)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_to_host_port_and_path() {
        assert_eq!(
            parse_https_url("https://rpc.hyperliquid-testnet.xyz/evm"),
            Some(("rpc.hyperliquid-testnet.xyz", 443, "/evm"))
        );
        assert_eq!(
            parse_https_url("https://localhost:8443"),
            Some(("localhost", 8443, "/"))
        );
        assert_eq!(parse_https_url("http://x/evm"), None, "plain http refused");
        assert_eq!(parse_https_url("https:///evm"), None, "empty host");
        assert_eq!(parse_https_url("https://x:0/"), None);
        assert_eq!(parse_https_url("https://x:99999/"), None);
        assert_eq!(parse_https_url("https://x/evm?key=1"), None, "no query");
        assert_eq!(parse_https_url("https://u@x/"), None, "no userinfo");
    }

    fn wire(h: &mut HttpsPost, body: &[u8]) -> String {
        h.body_mut()[..body.len()].copy_from_slice(body);
        let r = h.stage(body.len()).expect("fits");
        String::from_utf8_lossy(&h.req[r]).to_string()
    }

    #[test]
    fn the_request_is_one_contiguous_slice_with_the_boot_path() {
        let cfg = TlsTransport::default_client_config();
        let mut h = HttpsPost::new("127.0.0.1", 1, "/evm", cfg, 4096, 64).expect("loopback");
        let s = wire(&mut h, &[b'x'; 42]);
        assert!(s.starts_with("POST /evm HTTP/1.1\r\n"), "{s}");
        assert!(s.contains("Host: 127.0.0.1\r\n"), "{s}");
        assert!(s.contains("Connection: keep-alive\r\n"), "{s}");
        assert!(
            s.ends_with(&format!("Content-Length: 42\r\n\r\n{}", "x".repeat(42))),
            "{s}"
        );
        assert_eq!((h.host(), h.path()), ("127.0.0.1", "/evm"));
        assert!(!h.is_connected(), "no socket is opened at construction");
    }

    #[test]
    fn the_head_follows_the_digit_count_and_the_body_never_moves() {
        let cfg = TlsTransport::default_client_config();
        let mut h = HttpsPost::new("127.0.0.1", 1, "/p", cfg, 1000, 64).expect("loopback");
        let body_at = h.body_at;
        let mut n = 0usize;
        while n < 4 {
            let len = [7usize, 10, 999, 1000][n];
            let body = vec![b'a' + n as u8; len];
            let s = wire(&mut h, &body);
            assert!(s.starts_with("POST /p HTTP/1.1\r\n"), "{len}: {s}");
            assert!(
                s.ends_with(&format!(
                    "Content-Length: {len}\r\n\r\n{}",
                    String::from_utf8_lossy(&body)
                )),
                "{len}"
            );
            assert_eq!(h.body_at, body_at, "the body window is fixed");
            assert_eq!((h.host(), h.path()), ("127.0.0.1", "/p"), "{len}");
            n += 1;
        }
        // Back to one digit: the prefix moves right again.
        let s = wire(&mut h, b"z");
        assert!(
            s.starts_with("POST /p ") && s.ends_with("Content-Length: 1\r\n\r\nz"),
            "{s}"
        );
        assert_eq!(
            h.stage(1001),
            Err(PostErrKind::Overflow),
            "over the body cap"
        );
        assert_eq!(
            h.post(1001).err(),
            Some(PostErr {
                err: PostErrKind::Overflow,
                left_host: false
            }),
            "refused before any byte is written"
        );
    }

    #[test]
    fn a_bad_endpoint_is_refused_at_construction() {
        let cfg = TlsTransport::default_client_config();
        let e = HttpsPost::new(
            "no-such-host.invalid.example",
            443,
            "/",
            cfg.clone(),
            64,
            64,
        );
        assert!(matches!(e, Err(PostErrKind::Dns)));
        let bad = |path: &str, body_cap: usize| {
            matches!(
                HttpsPost::new("127.0.0.1", 1, path, cfg.clone(), body_cap, 64),
                Err(PostErrKind::BadEndpoint)
            )
        };
        assert!(bad("evm", 64), "a path must be absolute");
        assert!(bad("/é", 64), "a path must be visible ASCII");
        assert!(bad("/", 0), "a body window is required");
        assert!(bad("/", MAX_BODY_CAP + 1), "seven length digits at most");
    }

    #[test]
    fn errors_render_for_an_operator() {
        let e = PostErr {
            err: PostErrKind::Timeout,
            left_host: true,
        };
        assert!(e.to_string().contains("deadline"), "{e}");
        assert!(e.to_string().contains("left the host"), "{e}");
    }
}
