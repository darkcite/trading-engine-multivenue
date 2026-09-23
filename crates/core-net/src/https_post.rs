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
//! chunked; this client's endpoints include one that sends it). **`HlHttp` itself is deliberately NOT migrated onto
//! this type in the HYPARB lane**: it is the armed-live E-lane arm, and
//! changing it is that lane's decision under its own review.
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
    dechunk_in_place, fmt_u64_ascii, read_response, BodyFraming, DechunkResult, HttpResult,
};
use crate::transport::{Status, TlsTransport, Transport};

/// Request-header buffer: the literals + host + path + length digits.
const REQ_HEADER_BUF: usize = 1024;
/// The longest path a boot URL may carry.
pub const MAX_PATH: usize = 256;
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
    host: String,
    port: u16,
    path: String,
    addr: SocketAddr,
    connect_fail_streak: u32,
    server_name: ServerName<'static>,
    tls_config: Arc<ClientConfig>,
    /// `None` until the first request; reopened after a disconnect.
    transport: Option<TlsTransport>,
    poll: Poll,
    events: Events,
    req_header: Box<[u8]>,
    resp_buf: Box<[u8]>,
    resp_len: usize,
}

impl HttpsPost {
    /// Resolve and prepare. **Boot-only** — the allocations live here.
    /// No socket is opened until the first request.
    pub fn new(
        host: &str,
        port: u16,
        path: &str,
        tls_config: Arc<ClientConfig>,
        resp_cap: usize,
    ) -> Result<Self, PostErrKind> {
        if !path.starts_with('/') || path.len() > MAX_PATH {
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
            // COPY: ≤ 64 B host + ≤ 256 B path, ONCE at boot — the Host
            // header, the request line and the re-resolve need them for
            // the client's life; borrowing would pin the boot config.
            host: host.to_owned(),
            port,
            path: path.to_owned(),
            addr,
            connect_fail_streak: 0,
            server_name,
            tls_config,
            transport: None,
            poll: Poll::new().map_err(|_| PostErrKind::Disconnected)?,
            events: Events::with_capacity(8),
            req_header: vec![0u8; REQ_HEADER_BUF].into_boxed_slice(),
            resp_buf: vec![0u8; resp_cap].into_boxed_slice(),
            resp_len: 0,
        })
    }

    /// The host this client talks to (boot tells).
    #[inline]
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The fixed request path.
    #[inline]
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Drop the connection. The next request redials.
    pub fn close(&mut self) {
        self.transport = None;
        self.resp_len = 0;
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

    /// One request/response cycle: `(http_status, body_range)` into
    /// [`Self::resp`]. **The status is not a verdict.**
    pub fn post(&mut self, body: &[u8]) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        let deadline = Instant::now() + REQ_DEADLINE;
        self.ensure_connected(deadline).map_err(|err| PostErr {
            err,
            left_host: false,
        })?;
        let mut left_host = false;
        match self.cycle(body, deadline, &mut left_host) {
            Ok(v) => Ok(v),
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
        body: &[u8],
        deadline: Instant,
        left_host: &mut bool,
    ) -> Result<(u16, core::ops::Range<usize>), PostErrKind> {
        let header_len = self.write_header(body.len())?;
        {
            let t = self.transport.as_mut().ok_or(PostErrKind::Disconnected)?;
            let segments: [&[u8]; 2] = [&self.req_header[..header_len], body];
            *left_host = true;
            write_segments(t, &segments, deadline)?;
            // `write` only queues into rustls; flush now (no writable
            // edge is armed after the handshake — without this the first
            // poll sleeps a full POLL_TIMEOUT: HlHttp's E7 finding).
            t.flush().map_err(|_| PostErrKind::Disconnected)?;
            t.reregister(self.poll.registry(), MIO_TOKEN)
                .map_err(|_| PostErrKind::Disconnected)?;
        }
        self.read_response(deadline)
    }

    fn write_header(&mut self, body_len: usize) -> Result<usize, PostErrKind> {
        let mut len_buf = [0u8; 20];
        let len_str = fmt_u64_ascii(body_len as u64, &mut len_buf);
        let parts: [&[u8]; 11] = [
            b"POST ",
            self.path.as_bytes(),
            b" HTTP/1.1\r\n",
            b"Host: ",
            self.host.as_bytes(),
            b"\r\n",
            b"Content-Type: application/json\r\n",
            b"Content-Length: ",
            len_str,
            b"\r\n",
            b"Connection: keep-alive\r\n\r\n",
        ];
        let buf = &mut *self.req_header;
        let mut pos = 0usize;
        let mut i = 0usize;
        while i < parts.len() {
            let p = parts[i];
            let end = pos + p.len();
            if end > buf.len() {
                return Err(PostErrKind::Overflow);
            }
            // COPY: ≤ 512 B of header literals + host + path + length
            // digits into the boot-owned header buffer — the RENDER of
            // the request head, sent as its own segment ahead of the
            // body (no body byte is staged).
            buf[pos..end].copy_from_slice(p);
            pos = end;
            i += 1;
        }
        Ok(pos)
    }

    fn ensure_connected(&mut self, deadline: Instant) -> Result<(), PostErrKind> {
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
        Ok(())
    }

    fn read_response(
        &mut self,
        deadline: Instant,
    ) -> Result<(u16, core::ops::Range<usize>), PostErrKind> {
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
                    body_start,
                    body_end,
                    framing,
                    ..
                } => match framing {
                    // One framing this client can bound. A truncated
                    // body must never be handed on as the whole answer.
                    BodyFraming::ContentLength(_) => {
                        if self.resp_len >= body_end {
                            return Ok((status, body_start..body_end));
                        }
                        if peer_closed {
                            return Err(PostErrKind::Disconnected);
                        }
                    }
                    BodyFraming::CloseDelimited => {
                        if peer_closed {
                            return Ok((status, body_start..self.resp_len));
                        }
                    }
                    // Decoded in place once the terminating chunk has
                    // arrived (`dechunk_in_place` leaves an incomplete
                    // body untouched, so every read can retry it). The
                    // archive endpoint (purroof) answers chunked.
                    BodyFraming::Chunked => {
                        let end = self.resp_len;
                        match dechunk_in_place(&mut self.resp_buf[body_start..end]) {
                            DechunkResult::Complete { length } => {
                                return Ok((status, body_start..body_start + length))
                            }
                            DechunkResult::Incomplete => {
                                if peer_closed {
                                    return Err(PostErrKind::Disconnected);
                                }
                            }
                            DechunkResult::Malformed => return Err(PostErrKind::BadHttp),
                        }
                    }
                },
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

/// Write every segment in order, resuming across partial writes.
/// Bounded by `deadline`; a TLS transport never blocks on `write`, so
/// `WouldBlock` is a buffer the request did not fit.
fn write_segments<T: Transport>(
    t: &mut T,
    segments: &[&[u8]],
    deadline: Instant,
) -> Result<(), PostErrKind> {
    let mut seg = 0usize;
    let mut off = 0usize;
    while seg < segments.len() {
        let s = segments[seg];
        if off >= s.len() {
            seg += 1;
            off = 0;
            continue;
        }
        if Instant::now() >= deadline {
            return Err(PostErrKind::Timeout);
        }
        match t.write(&s[off..]) {
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

    #[test]
    fn the_header_carries_the_boot_path() {
        let cfg = TlsTransport::default_client_config();
        let mut h = HttpsPost::new("127.0.0.1", 1, "/evm", cfg, 64).expect("loopback");
        let n = h.write_header(42).expect("fits");
        let s = String::from_utf8_lossy(&h.req_header[..n]).to_string();
        assert!(s.starts_with("POST /evm HTTP/1.1\r\n"), "{s}");
        assert!(s.contains("Host: 127.0.0.1\r\n"), "{s}");
        assert!(s.contains("Content-Length: 42\r\n"), "{s}");
        assert!(s.ends_with("Connection: keep-alive\r\n\r\n"), "{s}");
        assert!(!h.is_connected(), "no socket is opened at construction");
        h.req_header = vec![0u8; 8].into_boxed_slice();
        assert_eq!(h.write_header(1), Err(PostErrKind::Overflow));
    }

    #[test]
    fn a_bad_endpoint_is_refused_at_construction() {
        let cfg = TlsTransport::default_client_config();
        let e = HttpsPost::new("no-such-host.invalid.example", 443, "/", cfg.clone(), 64);
        assert!(matches!(e, Err(PostErrKind::Dns)));
        let e = HttpsPost::new("127.0.0.1", 1, "evm", cfg, 64);
        assert!(
            matches!(e, Err(PostErrKind::BadEndpoint)),
            "a path must be absolute"
        );
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
