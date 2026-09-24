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
//! ## A kept-alive connection the venue has closed is never reused
//!
//! (Operator ask, 2026-09-24 — the finding the HYPARB H9 review made on
//! `core_net::HttpsPost`, which has the same shape.) The venue closes an
//! idle keep-alive connection on its own schedule, and it can announce
//! `Connection: close` on an answer. Reusing such a connection wrote
//! the next ORDER into a socket the peer had already closed: the write
//! succeeds locally, the read then finds EOF, and the order comes back
//! `Disconnected` with `left_host == true` — lost to reconciliation,
//! counted `sent_unanswered`, charged to the address budget, although
//! no byte of it ever reached the venue. Now:
//!
//! * an answer that says `Connection: close` (or arrives with the
//!   peer's FIN) retires the connection WITH that answer — the answer
//!   is kept, only the transport is dropped;
//! * before a connection is reused, one non-blocking one-byte read
//!   (a single `read(2)` through rustls) asks whether the peer closed it
//!   while idle — EOF, `close_notify`, an unsolicited byte or an error
//!   retires it — and the request dials fresh with `left_host == false`.
//!
//! [`HlHttp::dials`] counts handshakes, so an endpoint that closes after
//! every answer shows as dials ≈ posts.
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

use core_net::{head_says_close, read_response, HttpResult, TlsTransport, Transport};
use mio::{Events, Poll, Token};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;

/// Request-body buffer. An action of [`crate::action::MAX_ACTION`]
/// plus the signature envelope fits far inside this.
pub const MAX_REQ_BODY: usize = 16 * 1024;
/// Response buffer. Exchange responses are small; a batch of statuses
/// is still well under this.
///
/// **S7-L1: sized for the largest `/info` answer the arm reads** — a
/// `userFillsByTime` page (the day-spend read, `crate::dayspend`) of up
/// to 2 000 fills, measured on mainnet at 751 477 B for a full page
/// (~376 B a row, 2026-09-24, `Content-Length`-framed). A body that
/// does not fit is refused (`HttpErr::Overflow`), and a day-spend read
/// that can never succeed would never let the live slot seed. One
/// boot allocation; nothing on the order path touches the unused tail.
pub const MAX_RESP_BUF: usize = 1024 * 1024;
/// Request-header buffer, written separately from the body so the two
/// go out as one logical frame without a copy.
const REQ_HEADER_BUF: usize = 1024;

const MIO_TOKEN: Token = Token(0);
const POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// Consecutive connect failures between DNS re-resolutions.
const RERESOLVE_AFTER: u32 = 3;
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
/// knows, and nothing outside can introduce a third — `post_to` is
/// `pub(crate)` so that sentence is enforced by visibility, not by a
/// `debug_assert!` the release build drops.
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

/// A failed request, **and whether any of it reached the wire.**
///
/// ## Why this is a struct and not another `HttpErr` variant
///
/// The caller that matters — `HlExchange::send_action` — has to
/// decide whether the venue's address-rate governor should count the
/// action. Counting only successful posts makes the governor drift
/// OPTIMISTIC, which `AddressBudget::on_action_sent` names as the
/// wrong direction: under-counting means exceeding the venue's real
/// limit and then reading the rate-limit answer as a transport
/// problem.
///
/// That decision cannot be made from the [`HttpErr`] variant. A
/// `Disconnected` is both "the connect failed" (nothing left) and
/// "the peer went away mid-response" (everything left); a `Timeout`
/// covers the whole cycle. A classifier over the variants would be a
/// name describing a stronger property than its condition tests.
///
/// So the fact is recorded where it is known — inside the cycle, at
/// the moment the write is attempted — and returned in a struct the
/// caller must destructure. No call site can ignore it by accident.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PostErr {
    /// What went wrong.
    pub err: HttpErr,
    /// **Any byte of this request may have reached the socket.**
    ///
    /// Deliberately set BEFORE the write is attempted rather than
    /// after it succeeds, so a torn write counts too. We cannot tell
    /// a write that died on its first byte from one that died on its
    /// last, and of the two ways to be wrong, sending fewer actions
    /// than the venue allows is the recoverable one.
    pub left_host: bool,
}

impl PostErr {
    /// A failure that happened before anything could be written —
    /// DNS, the TLS handshake, or a request that did not fit its
    /// buffer.
    #[inline]
    #[must_use]
    pub const fn before_send(err: HttpErr) -> Self {
        Self {
            err,
            left_host: false,
        }
    }
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

/// A keep-alive HTTPS connection to one Hyperliquid API host.
pub struct HlHttp {
    host: String,
    port: u16,
    addr: SocketAddr,
    /// Consecutive `ensure_connected` failures. Every
    /// [`RERESOLVE_AFTER`] of them re-resolves `host` — the address
    /// was cached at boot, and a CDN-fronted venue rotates IPs; a
    /// client that redialled one dead address for the life of the boot
    /// would report every order as `Disconnected` until a restart
    /// (E7 review, 2026-09-19).
    connect_fail_streak: u32,
    /// Successful dials (TCP + TLS handshakes) over the client's life.
    dials: u64,
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
            // COPY: ≤ 64 B host name for re-resolve + the Host header,
            // ONCE at boot; borrowed from the config it would pin the
            // config's lifetime to the client's.
            host: host.to_owned(),
            port,
            addr,
            connect_fail_streak: 0,
            dials: 0,
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

    /// Successful dials over the client's life: 1 on a healthy
    /// keep-alive venue; ≈ posts on one that closes after every answer.
    #[inline]
    #[must_use]
    pub const fn dials(&self) -> u64 {
        self.dials
    }

    /// One request/response cycle.
    ///
    /// Returns `(http_status, body_range)` into [`Self::resp`]. **The
    /// status is not a verdict** — see the module note.
    pub fn post(&mut self, body: &[u8]) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.post_to(EXCHANGE_PATH, body)
    }

    /// One request/response cycle against `path`, which must be
    /// [`EXCHANGE_PATH`] or [`INFO_PATH`].
    ///
    /// `pub(crate)`: the two-API claim in [`INFO_PATH`]'s doc is only
    /// true if nothing outside this crate can name a third path, and a
    /// `debug_assert!` is compiled out of the binary that ships.
    ///
    /// # Errors
    /// As [`Self::post`].
    pub(crate) fn post_to(
        &mut self,
        path: &'static [u8],
        body: &[u8],
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        debug_assert!(
            path == EXCHANGE_PATH || path == INFO_PATH,
            "this client knows two APIs and no others"
        );
        let deadline = Instant::now() + REQ_DEADLINE;
        self.ensure_connected(deadline)
            .map_err(PostErr::before_send)?;
        let mut left_host = false;
        match self.cycle(path, body, deadline, &mut left_host) {
            Ok((status, range, retire)) => {
                if retire {
                    // The venue closed, or said it will: the answer is
                    // whole, the connection is not reusable. Keep the
                    // answer; drop only the transport (module doc).
                    self.transport = None;
                }
                Ok((status, range))
            }
            Err(err) => {
                // Any failure closes the connection. A half-read
                // response left in the buffer would be read as the
                // NEXT order's answer, which is how a fill gets
                // attributed to the wrong order.
                self.close();
                Err(PostErr { err, left_host })
            }
        }
    }

    /// The response bytes from the last successful [`Self::post`].
    #[inline]
    #[must_use]
    pub fn resp(&self) -> &[u8] {
        &self.resp_buf[..self.resp_len]
    }

    /// `left_host` is set the instant a write is ATTEMPTED — see
    /// [`PostErr::left_host`] for why that is the conservative moment
    /// rather than after the write returns.
    fn cycle(
        &mut self,
        path: &'static [u8],
        body: &[u8],
        deadline: Instant,
        left_host: &mut bool,
    ) -> Result<(u16, core::ops::Range<usize>, bool), HttpErr> {
        let header_len = self.write_header(path, body.len())?;
        {
            let t = self.transport.as_mut().ok_or(HttpErr::Disconnected)?;
            // Header and body as one logical frame: a partial write
            // under TLS backpressure resumes at the same offset.
            let segments: [&[u8]; 2] = [&self.req_header[..header_len], body];
            *left_host = true;
            write_segments(t, &segments, deadline)?;
            // The request is in rustls' buffer, not on the wire.
            // `write` only queues; the bytes leave on `write_tls`, which
            // `pump` runs on a WRITABLE event — and no writable edge is
            // armed after the handshake. Without this flush the first
            // `poll` in `read_response` slept a full `POLL_TIMEOUT`
            // before the `reregister` at its tail armed the edge that
            // finally sent the request: 50 ms added to EVERY order,
            // cancel and requote, invisible to a loopback that asserts
            // outcomes (E7 review, 2026-09-19). Flush now; reregister
            // so a partial flush completes on the next event.
            t.flush().map_err(|_| HttpErr::Disconnected)?;
            t.reregister(self.poll.registry(), MIO_TOKEN)
                .map_err(|_| HttpErr::Disconnected)?;
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
            // COPY: ≤ 256 B of header literals + host + length digits
            // into the boot-owned header buffer — the RENDER of the
            // request head, sent as its own TLS segment ahead of the
            // body (no body byte is staged; see `write_segments`).
            buf[pos..end].copy_from_slice(p);
            pos = end;
        }
        Ok(pos)
    }

    fn ensure_connected(&mut self, deadline: Instant) -> Result<(), HttpErr> {
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
                    // Blocking DNS, on the failure path ONLY — the
                    // caller is already inside an outage. Best effort:
                    // an unresolvable host keeps the old address.
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

    /// Before reusing a kept-alive connection: one non-blocking read.
    /// No request is outstanding, so anything but `WouldBlock` — EOF,
    /// `close_notify`, an unsolicited byte, an error — means the venue
    /// is gone or out of step, and the request must dial fresh.
    fn still_open(&mut self) -> bool {
        let Some(t) = self.transport.as_mut() else {
            return false;
        };
        let mut probe = [0u8; 1];
        matches!(t.read(&mut probe), Err(ref e) if e.kind() == io::ErrorKind::WouldBlock)
    }

    fn dial(&mut self, deadline: Instant) -> Result<(), HttpErr> {
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
        self.dials += 1;
        Ok(())
    }

    /// Read one whole answer: `(status, body_range, retire)` — `retire`
    /// when the venue closed with it or announced `Connection: close`.
    fn read_response(
        &mut self,
        deadline: Instant,
    ) -> Result<(u16, core::ops::Range<usize>, bool), HttpErr> {
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
                    header_end,
                    body_start,
                    body_end,
                    framing,
                } => {
                    let retire = peer_closed || head_says_close(&self.resp_buf[..header_end]);
                    // This client knows ONE framing. The venue answers
                    // `Content-Length`; a chunked body would need
                    // de-chunking before the scanner could read it and
                    // a close-delimited one is complete only at the
                    // FIN. The first cut returned either "as soon as
                    // any bytes were buffered" — so a truncated ok
                    // envelope (cut before `"statuses"`) was handed to
                    // the scanner as the venue's whole answer, and the
                    // scanner's no-statuses branch read it as an
                    // acceptance. A body the layer cannot bound is a
                    // shape it may not guess at: refuse it.
                    match framing {
                        core_net::BodyFraming::ContentLength(_) => {
                            if self.resp_len >= body_end {
                                return Ok((status, body_start..body_end, retire));
                            }
                            // Declared more body than has arrived. If
                            // the peer is gone it never will — a
                            // TRUNCATED body must not be handed on as
                            // if it were the venue's answer.
                            if peer_closed {
                                return Err(HttpErr::Disconnected);
                            }
                        }
                        core_net::BodyFraming::CloseDelimited => {
                            if peer_closed {
                                return Ok((status, body_start..self.resp_len, true));
                            }
                        }
                        core_net::BodyFraming::Chunked => return Err(HttpErr::BadHttp),
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
            // rustls' `Ok(0)` is its send-buffer limit (64 KiB
            // default), not a disconnect: the request did not fit.
            // Unreachable at 17 KiB, named correctly for when it is.
            Ok(0) => return Err(HttpErr::Overflow),
            Ok(n) => off += n,
            // A TLS transport buffers plaintext and never blocks on
            // `write`; a transport that does has nowhere to park the
            // bytes, and sleeping on the order path is not a
            // backpressure strategy (the first cut slept 1 ms here).
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(HttpErr::Overflow)
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
