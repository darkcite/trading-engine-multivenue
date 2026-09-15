// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `POST /exchange` over TLS — the network arm (E3).
//!
//! Deliberately the same shape as `clob_dispatcher::live::LiveDispatcher`:
//! mio + rustls, HTTP/1.1 with `Connection: keep-alive`, everything
//! preallocated at construction, one synchronous request/response cycle
//! per call on the dispatcher's own worker thread. **HTTP/1.1, not /2 —
//! the no-tokio rule stands**, and reusing the arm the engine already
//! trusts is worth more than a protocol upgrade nothing needs.
//!
//! Content-Length framing means the reader exits on the declared length
//! rather than waiting for a peer FIN, so the connection survives for
//! the next order: the 50–150 ms TCP+TLS handshake is paid once, not
//! per order.
//!
//! ## What this layer does NOT decide
//!
//! Whether the venue accepted anything. It returns the HTTP status and
//! the body's byte range and stops there. **A Hyperliquid refusal
//! arrives with HTTP 200** (see [`crate::response`]), so a layer that
//! judged success from the status code would be wrong most of the time
//! it mattered. Judging is [`crate::response::scan`]'s job.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_net::{read_response, HttpResult, TlsTransport, Transport};
use mio::{Events, Poll, Token};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;

/// Request-body buffer. An action of [`crate::action::MAX_ACTION`]
/// plus the signature envelope fits far inside this.
pub const MAX_REQ_BODY: usize = 16 * 1024;
/// Response buffer. Exchange responses are small; a batch of statuses
/// is still well under this.
pub const MAX_RESP_BUF: usize = 16 * 1024;
/// Request-header buffer, written separately from the body so the two
/// go out as one logical frame without a copy.
const REQ_HEADER_BUF: usize = 1024;

const MIO_TOKEN: Token = Token(0);
const POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// One request's whole budget: connect, write, read. Generous enough
/// for a WAN round trip, short enough that a wedged connection cannot
/// hold the worker thread.
pub const REQ_DEADLINE: Duration = Duration::from_secs(5);

/// The `/exchange` path. Not configurable: a different path is a
/// different API, not a deployment choice.
pub const EXCHANGE_PATH: &[u8] = b"/exchange";

/// The `/info` path — READ-ONLY, unsigned, and the only other path
/// this client may use.
///
/// Still not configurable. Two named constants is not the same thing
/// as a settable path: the caller picks between two APIs this crate
/// knows, and nothing outside can introduce a third.
///
/// **Shares the connection and the response buffer with
/// [`EXCHANGE_PATH`].** [`HlHttp::resp`] holds only the last answer, so
/// an `/info` read must be consumed before the next order goes out.
/// Both callers live on one thread and one is on the idle path, so
/// they are naturally serialised — but that is a property of the
/// caller, not of this type.
pub const INFO_PATH: &[u8] = b"/info";

/// Why an HTTP cycle failed.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum HttpErr {
    /// DNS resolution failed at construction.
    Dns,
    /// rustls rejected the host as a server name.
    BadServerName,
    /// Connect, handshake, write or read failed, or the peer went away
    /// mid-response. The connection is closed; the next call redials.
    Disconnected,
    /// The request did not fit its preallocated buffer.
    Overflow,
    /// The response was not parseable as HTTP/1.1.
    BadHttp,
    /// The whole cycle exceeded [`REQ_DEADLINE`].
    Timeout,
}

impl core::fmt::Display for HttpErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            HttpErr::Dns => "dns resolution failed",
            HttpErr::BadServerName => "host is not a valid TLS server name",
            HttpErr::Disconnected => "connection lost",
            HttpErr::Overflow => "request did not fit its buffer",
            HttpErr::BadHttp => "response was not HTTP/1.1",
            HttpErr::Timeout => "request deadline exceeded",
        };
        write!(f, "hl http: {s}")
    }
}

impl std::error::Error for HttpErr {}

/// A keep-alive HTTPS connection to one Hyperliquid API host.
pub struct HlHttp {
    host: String,
    addr: SocketAddr,
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

impl HlHttp {
    /// Resolve and prepare. **Boot-only** — this is where the
    /// allocations live. No socket is opened until the first request.
    pub fn new(host: &str, port: u16, tls_config: Arc<ClientConfig>) -> Result<Self, HttpErr> {
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|_| HttpErr::Dns)?
            .next()
            .ok_or(HttpErr::Dns)?;
        let server_name =
            TlsTransport::server_name_from_host(host).map_err(|_| HttpErr::BadServerName)?;
        Ok(Self {
            host: host.to_owned(),
            addr,
            server_name,
            tls_config,
            transport: None,
            poll: Poll::new().map_err(|_| HttpErr::Disconnected)?,
            events: Events::with_capacity(8),
            req_header: vec![0u8; REQ_HEADER_BUF].into_boxed_slice(),
            resp_buf: vec![0u8; MAX_RESP_BUF].into_boxed_slice(),
            resp_len: 0,
        })
    }

    /// The host this arm talks to. Used by the boot tell, and by the
    /// smoke path's testnet assertion.
    #[inline]
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
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

    /// One request/response cycle.
    ///
    /// Returns `(http_status, body_range)` into [`Self::resp`]. **The
    /// status is not a verdict** — see the module note.
    pub fn post(&mut self, body: &[u8]) -> Result<(u16, core::ops::Range<usize>), HttpErr> {
        self.post_to(EXCHANGE_PATH, body)
    }

    /// One request/response cycle against `path`, which must be
    /// [`EXCHANGE_PATH`] or [`INFO_PATH`].
    ///
    /// # Errors
    /// As [`Self::post`].
    pub fn post_to(
        &mut self,
        path: &'static [u8],
        body: &[u8],
    ) -> Result<(u16, core::ops::Range<usize>), HttpErr> {
        debug_assert!(
            path == EXCHANGE_PATH || path == INFO_PATH,
            "this client knows two APIs and no others"
        );
        let deadline = Instant::now() + REQ_DEADLINE;
        self.ensure_connected(deadline)?;
        match self.cycle(path, body, deadline) {
            Ok(v) => Ok(v),
            Err(e) => {
                // Any failure closes the connection. A half-read
                // response left in the buffer would be read as the
                // NEXT order's answer, which is how a fill gets
                // attributed to the wrong order.
                self.close();
                Err(e)
            }
        }
    }

    /// The response bytes from the last successful [`Self::post`].
    #[inline]
    #[must_use]
    pub fn resp(&self) -> &[u8] {
        &self.resp_buf[..self.resp_len]
    }

    fn cycle(
        &mut self,
        path: &'static [u8],
        body: &[u8],
        deadline: Instant,
    ) -> Result<(u16, core::ops::Range<usize>), HttpErr> {
        let header_len = self.write_header(path, body.len())?;
        {
            let t = self.transport.as_mut().ok_or(HttpErr::Disconnected)?;
            // Header and body as one logical frame: a partial write
            // under TLS backpressure resumes at the same offset.
            let segments: [&[u8]; 2] = [&self.req_header[..header_len], body];
            write_segments(t, &segments, deadline)?;
        }
        self.read_response(deadline)
    }

    fn write_header(&mut self, path: &'static [u8], body_len: usize) -> Result<usize, HttpErr> {
        let mut len_buf = [0u8; 20];
        let len_str = format_u64_into(&mut len_buf, body_len as u64);
        let host = self.host.as_bytes();
        let parts: [&[u8]; 11] = [
            b"POST ",
            path,
            b" HTTP/1.1\r\n",
            b"Host: ",
            host,
            b"\r\n",
            b"Content-Type: application/json\r\n",
            b"Content-Length: ",
            len_str,
            b"\r\n",
            b"Connection: keep-alive\r\n\r\n",
        ];
        let buf = &mut *self.req_header;
        let mut pos = 0usize;
        for p in parts {
            let end = pos.checked_add(p.len()).ok_or(HttpErr::Overflow)?;
            if end > buf.len() {
                return Err(HttpErr::Overflow);
            }
            buf[pos..end].copy_from_slice(p);
            pos = end;
        }
        Ok(pos)
    }

    fn ensure_connected(&mut self, deadline: Instant) -> Result<(), HttpErr> {
        if self.transport.is_some() {
            return Ok(());
        }
        let mut t =
            TlsTransport::connect(self.addr, self.server_name.clone(), self.tls_config.clone())
                .map_err(|_| HttpErr::Disconnected)?;
        t.register(self.poll.registry(), MIO_TOKEN)
            .map_err(|_| HttpErr::Disconnected)?;
        loop {
            if Instant::now() >= deadline {
                return Err(HttpErr::Timeout);
            }
            self.poll
                .poll(&mut self.events, Some(POLL_TIMEOUT))
                .map_err(|_| HttpErr::Disconnected)?;
            let mut status = core_net::Status::Handshaking;
            for ev in self.events.iter() {
                if ev.token() != MIO_TOKEN {
                    continue;
                }
                status = t.pump(ev).map_err(|_| HttpErr::Disconnected)?;
            }
            match status {
                core_net::Status::Ready => break,
                core_net::Status::Closed => return Err(HttpErr::Disconnected),
                _ => t
                    .reregister(self.poll.registry(), MIO_TOKEN)
                    .map_err(|_| HttpErr::Disconnected)?,
            }
        }
        self.transport = Some(t);
        Ok(())
    }

    fn read_response(
        &mut self,
        deadline: Instant,
    ) -> Result<(u16, core::ops::Range<usize>), HttpErr> {
        self.resp_len = 0;
        let mut peer_closed = false;
        loop {
            if Instant::now() >= deadline {
                return Err(HttpErr::Timeout);
            }
            self.poll
                .poll(&mut self.events, Some(POLL_TIMEOUT))
                .map_err(|_| HttpErr::Disconnected)?;
            {
                let t = self.transport.as_mut().ok_or(HttpErr::Disconnected)?;
                for ev in self.events.iter() {
                    if ev.token() != MIO_TOKEN {
                        continue;
                    }
                    if t.pump(ev).map_err(|_| HttpErr::Disconnected)? == core_net::Status::Closed {
                        peer_closed = true;
                    }
                }
                // Drain all available plaintext on one readiness.
                loop {
                    if self.resp_len >= self.resp_buf.len() {
                        break;
                    }
                    let cap = self.resp_buf.len();
                    match t.read(&mut self.resp_buf[self.resp_len..cap]) {
                        Ok(0) => {
                            peer_closed = true;
                            break;
                        }
                        Ok(n) => self.resp_len += n,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => return Err(HttpErr::Disconnected),
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
                } => {
                    let need = match framing {
                        core_net::BodyFraming::ContentLength(_) => body_end,
                        core_net::BodyFraming::CloseDelimited | core_net::BodyFraming::Chunked => {
                            self.resp_len
                        }
                    };
                    if self.resp_len >= need {
                        return Ok((status, body_start..body_end.min(self.resp_len)));
                    }
                    // Declared more body than has arrived. If the peer
                    // is gone it never will — a TRUNCATED body must
                    // not be handed on as if it were the venue's answer.
                    if peer_closed {
                        return Err(HttpErr::Disconnected);
                    }
                }
                HttpResult::Incomplete => {
                    if peer_closed {
                        return Err(HttpErr::Disconnected);
                    }
                }
                HttpResult::Malformed => return Err(HttpErr::BadHttp),
            }

            if self.resp_len >= self.resp_buf.len() {
                // Buffer full and still not a complete response.
                return Err(HttpErr::Overflow);
            }
            let t = self.transport.as_mut().ok_or(HttpErr::Disconnected)?;
            t.reregister(self.poll.registry(), MIO_TOKEN)
                .map_err(|_| HttpErr::Disconnected)?;
        }
    }
}

/// Write every segment in order, resuming across partial writes and
/// `WouldBlock`. Bounded by `deadline`.
fn write_segments<T: Transport>(
    t: &mut T,
    segments: &[&[u8]],
    deadline: Instant,
) -> Result<(), HttpErr> {
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
            return Err(HttpErr::Timeout);
        }
        match t.write(&s[off..]) {
            Ok(0) => return Err(HttpErr::Disconnected),
            Ok(n) => off += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return Err(HttpErr::Disconnected),
        }
    }
    Ok(())
}

/// `u64` → decimal bytes, into a caller-owned scratch. No `format!`.
fn format_u64_into(buf: &mut [u8; 20], mut v: u64) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = 20usize;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    &buf[i..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u64_renders_without_allocating() {
        let mut b = [0u8; 20];
        assert_eq!(format_u64_into(&mut b, 0), b"0");
        assert_eq!(format_u64_into(&mut b, 1), b"1");
        assert_eq!(format_u64_into(&mut b, 1024), b"1024");
        assert_eq!(format_u64_into(&mut b, u64::MAX), b"18446744073709551615");
    }

    #[test]
    fn a_bad_host_is_refused_at_construction_not_at_submit() {
        let cfg = TlsTransport::default_client_config();
        // Unresolvable — must fail HERE, at boot, not on the first
        // order.
        // `HlHttp` is not `Debug` (it owns a TLS config and a poll),
        // so unwrap the Result by hand.
        let e = match HlHttp::new("no-such-host.invalid.example", 443, cfg.clone()) {
            Err(e) => e,
            Ok(_) => panic!("an unresolvable host must fail at construction"),
        };
        assert_eq!(e, HttpErr::Dns);
    }

    #[test]
    fn the_header_is_well_formed_and_bounded() {
        let cfg = TlsTransport::default_client_config();
        let mut h = HlHttp::new("127.0.0.1", 1, cfg).expect("loopback resolves");
        let n = h.write_header(EXCHANGE_PATH, 42).expect("fits");
        let s = String::from_utf8_lossy(&h.req_header[..n]).to_string();
        assert!(s.starts_with("POST /exchange HTTP/1.1\r\n"), "{s}");
        assert!(s.contains("Host: 127.0.0.1\r\n"), "{s}");
        assert!(s.contains("Content-Type: application/json\r\n"), "{s}");
        assert!(s.contains("Content-Length: 42\r\n"), "{s}");
        assert!(s.contains("Connection: keep-alive\r\n\r\n"), "{s}");
        assert!(!h.is_connected(), "no socket is opened at construction");
    }

    #[test]
    fn a_header_too_big_for_its_buffer_is_refused() {
        let cfg = TlsTransport::default_client_config();
        let mut h = HlHttp::new("127.0.0.1", 1, cfg).expect("loopback");
        // Shrink the buffer to force the overflow path.
        h.req_header = vec![0u8; 8].into_boxed_slice();
        assert_eq!(h.write_header(EXCHANGE_PATH, 1), Err(HttpErr::Overflow));
    }

    #[test]
    fn errors_render_for_an_operator() {
        for (e, needle) in [
            (HttpErr::Dns, "dns"),
            (HttpErr::BadServerName, "server name"),
            (HttpErr::Disconnected, "connection lost"),
            (HttpErr::Overflow, "buffer"),
            (HttpErr::BadHttp, "HTTP/1.1"),
            (HttpErr::Timeout, "deadline"),
        ] {
            assert!(e.to_string().contains(needle), "{e}");
        }
    }
}
