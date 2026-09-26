// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # https_conn — one keep-alive HTTPS/1.1 connection, non-blocking
//!
//! ONE host, several request shapes ("templates"), ONE request in
//! flight, driven by its OWNER's `mio` poll (BX5; Binance plan §13.3
//! item 11). An exec gateway thread owns every socket of its venue, and
//! a REST order (Binance options, Portfolio Margin, Stocks) must not park
//! that thread for a ~110 ms round trip, so the cycle is split:
//!
//! - [`HttpsConn::start`] stages the request and writes it — or opens
//!   the connection first — and returns at once;
//! - [`HttpsConn::on_event`] advances on each readiness event the
//!   owner's poll delivers for this connection's token;
//! - [`HttpsConn::on_tick`] enforces the request deadline.
//!
//! [`crate::HttpsPost`] is this connection's blocking one-template form
//! (its own poll; the caller's thread waits): one implementation of the
//! protocol, two ways to drive it. [`crate::HttpsReq`] (HC2) still runs
//! on the older blocking engine (`crate::https_keepalive`); moving it
//! here is a recorded follow-up of the 2026-09-26 merge.
//!
//! ## Templates: every request is rendered in place, in its wire buffer
//!
//! Each template owns one region of a [`ReqWire`], rendered at boot. The
//! caller renders a request's parameters into the template's window
//! ([`ReqWire::window_mut`]) and hands over their length; nothing else
//! is rendered per request but the length digits or the header tail.
//!
//! ```text
//! body form — parameters in the body (POST, PUT, DELETE):
//! [ pad | METHOD path HTTP/1.1 … Content-Length: | digits | CRLF CRLF | body … ]
//!         ^ head, rendered at boot                 ^ per request        ^ window
//!
//! query form — parameters in the target (GET; any method so configured):
//! [ METHOD path? | query … | tail slot ][ tail master ]
//!   ^ head         ^ window   ^ the master is copied here per request
//! ```
//!
//! The body form is [`crate::HttpsPost`]'s layout (its module doc has
//! the reasoning): the head sits flush against the digits and moves only
//! when the body's digit COUNT differs from the last request's. The
//! query form cannot avoid one copy: the header tail (` HTTP/1.1`, the
//! host, the extra headers, the blank line) must follow a query whose
//! length varies, so it is copied from its master on every request. The
//! GETs are the cold calls (recon, status); an order goes in a body.
//!
//! ## Keep-alive that does not lie about `left_host`
//!
//! As [`crate::HttpsPost`]'s module doc tells: the connection retires on
//! `Connection: close` or a peer close, and a one-byte non-blocking read
//! before each reuse retires one the peer closed while idle, so that
//! request dials fresh with `left_host == false`.
//!
//! ## What this layer does NOT decide
//!
//! Whether the venue accepted anything: it returns the status and the
//! body's span, and on failure whether any byte may have reached the
//! wire ([`PostErr::left_host`]).
//!
//! ## The owner's contract
//!
//! - The connection registers its own socket, with the registry the
//!   owner passes, under the token given at construction.
//! - [`HttpsConn::start`] is called between poll batches, never while
//!   one is being dispatched: a closed socket's last events may still be
//!   in the batch.
//! - [`HttpsConn::on_tick`] runs at least every few milliseconds while a
//!   request is in flight; the poll timeout is the owner's to bound.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};

use mio::event::Event;
use mio::{Registry, Token};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;

use crate::http1::{
    chunked_body, head_says_close, read_response, BodyFraming, ChunkedBody, HttpResult,
};
use crate::transport::{Status, TlsTransport, Transport};
use crate::ws_frame::PayloadSpan;

/// The longest path a template may carry.
pub const MAX_PATH: usize = 256;
/// The longest host (DNS's own bound).
pub const MAX_HOST: usize = 253;
/// The largest window a connection may be built for (7 length digits).
pub const MAX_BODY_CAP: usize = 9_999_999;
/// The most templates one connection carries.
pub const MAX_TEMPLATES: usize = 32;
/// The most extra header lines one template carries.
pub const MAX_HEADERS: usize = 4;
/// The longest extra header name.
pub const MAX_HEADER_NAME: usize = 64;
/// The longest extra header value (a Binance API key is 64 B).
pub const MAX_HEADER_VALUE: usize = 128;

const HTTP11: &[u8] = b" HTTP/1.1\r\nHost: ";
const CRLF: &[u8] = b"\r\n";
const HEAD_END: &[u8] = b"\r\n\r\n";
const CONTENT_TYPE: &[u8] = b"Content-Type: ";
const KEEP_ALIVE_LEN: &[u8] = b"\r\nConnection: keep-alive\r\nContent-Length: ";
const EMPTY_BODY: &[u8] = b"Content-Length: 0\r\n";
const KEEP_ALIVE_END: &[u8] = b"Connection: keep-alive\r\n\r\n";

// ---------------------------------------------------------------
// Errors
// ---------------------------------------------------------------

/// Why an HTTPS cycle failed.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PostErrKind {
    /// DNS resolution failed at construction.
    Dns,
    /// rustls rejected the host as a server name, or the URL, path,
    /// header or size was refused at construction.
    BadEndpoint,
    /// Connect, handshake, write or read failed, or the peer went away
    /// mid-response. The connection is closed; the next request redials.
    Disconnected,
    /// The request or the response did not fit its buffer.
    Overflow,
    /// The response was not well-framed HTTP/1.1 (malformed headers or
    /// chunk framing).
    BadHttp,
    /// The whole cycle exceeded its deadline.
    Timeout,
    /// A request was started while another was in flight on the same
    /// connection — an owner's bug; nothing was written.
    Busy,
    /// HC2 ([`crate::HttpsReq`]): the request head carried a field that
    /// would break its framing — a CR, LF or NUL (header injection), a
    /// target that is not `/…` origin-form, or a bad header name.
    /// Refused before any byte is written.
    BadRequest,
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
            Self::Busy => "https: a request is already in flight on this connection",
            Self::BadRequest => "https: the request head would break its framing",
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

#[inline]
const fn not_left(err: PostErrKind) -> PostErr {
    PostErr {
        err,
        left_host: false,
    }
}

// ---------------------------------------------------------------
// Request templates
// ---------------------------------------------------------------

/// The request method: HC2's [`crate::http1::Method`], one type for every
/// client in the crate (a `GET` template carries its parameters in the
/// query only).
pub use crate::http1::Method;

/// Where a template's parameters go.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Params<'a> {
    /// In the request target, after `?`.
    Query,
    /// In the body, sent with this `Content-Type` (never for `GET`).
    Body(&'a str),
}

/// One request shape, fixed at boot. No `Debug`: `headers` may carry
/// an API key.
#[derive(Copy, Clone)]
pub struct ReqSpec<'a> {
    /// The method.
    pub method: Method,
    /// The absolute path, without a query.
    pub path: &'a str,
    /// Where the parameters go.
    pub params: Params<'a>,
    /// Extra header lines (`X-MBX-APIKEY`), sent after `Host`.
    pub headers: &'a [(&'a str, &'a str)],
}

/// The longest template head: `DELETE`, a space, the longest path, the
/// host line, [`MAX_HEADERS`] of the longest headers, the longest content
/// type and the keep-alive / length lines (a query-form head is shorter).
const MAX_HEAD: usize = 6
    + 1
    + MAX_PATH
    + HTTP11.len()
    + MAX_HOST
    + CRLF.len()
    + MAX_HEADERS * (MAX_HEADER_NAME + 2 + MAX_HEADER_VALUE + CRLF.len())
    + CONTENT_TYPE.len()
    + MAX_HEADER_VALUE
    + KEEP_ALIVE_LEN.len();
/// The longest query-form header tail.
const MAX_TAIL: usize = HTTP11.len()
    + MAX_HOST
    + CRLF.len()
    + MAX_HEADERS * (MAX_HEADER_NAME + 2 + MAX_HEADER_VALUE + CRLF.len())
    + EMPTY_BODY.len()
    + KEEP_ALIVE_END.len();
// Every offset of the largest possible wire buffer fits a `u32`.
const _: () = assert!(
    MAX_TEMPLATES * (MAX_HEAD + 7 + HEAD_END.len() + MAX_BODY_CAP + 2 * MAX_TAIL)
        < u32::MAX as usize
);

/// Where one template lives in its [`ReqWire`]. Offsets are `u32`, so
/// the record read per request is 36 B, inside one cache line.
#[derive(Copy, Clone, Debug)]
struct Tmpl {
    /// First byte of the region.
    at: u32,
    /// Where the head starts now (the body form moves it).
    head_at: u32,
    head_len: u32,
    /// First window byte.
    win_at: u32,
    /// Body form: digits of the window capacity (the pad is sized for
    /// it). Query form: 0.
    max_digits: u32,
    /// Query form: the tail master's first byte and length. Body: 0.
    tail_at: u32,
    tail_len: u32,
    method_len: u32,
    path_len: u32,
}

const _: () = assert!(core::mem::size_of::<Tmpl>() == 36);

/// Every template of one connection, each in its own region of ONE
/// boot-allocated wire buffer (module doc). Pure: no socket — so the
/// staging is testable and alloc-gated on its own.
pub struct ReqWire {
    wire: Box<[u8]>,
    tmpl: Box<[Tmpl]>,
    win_cap: usize,
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

/// RFC 9110 `tchar`.
#[inline]
fn header_name_ok(s: &[u8]) -> bool {
    if s.is_empty() || s.len() > MAX_HEADER_NAME {
        return false;
    }
    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        if !(b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
            return false;
        }
        i += 1;
    }
    true
}

/// Visible ASCII and inner spaces; never CR or LF (header injection).
#[inline]
fn header_value_ok(s: &[u8]) -> bool {
    if s.is_empty() || s.len() > MAX_HEADER_VALUE || s[0] == b' ' || s[s.len() - 1] == b' ' {
        return false;
    }
    let mut i = 0;
    while i < s.len() {
        if !(s[i].is_ascii_graphic() || s[i] == b' ') {
            return false;
        }
        i += 1;
    }
    true
}

/// The spec's shape, checked; `(head_len, tail_len)` under `host`.
fn measure(host: &[u8], s: &ReqSpec<'_>) -> Result<(usize, usize), PostErrKind> {
    let path = s.path.as_bytes();
    let path_ok = path.first() == Some(&b'/') && path.len() <= MAX_PATH && visible(path);
    if !path_ok || s.headers.len() > MAX_HEADERS {
        return Err(PostErrKind::BadEndpoint);
    }
    let mut hdrs = 0usize;
    let mut i = 0;
    while i < s.headers.len() {
        let (n, v) = (s.headers[i].0.as_bytes(), s.headers[i].1.as_bytes());
        if !header_name_ok(n) || !header_value_ok(v) {
            return Err(PostErrKind::BadEndpoint);
        }
        hdrs += n.len() + 2 + v.len() + CRLF.len();
        i += 1;
    }
    let m = s.method.token().len();
    let host_line = HTTP11.len() + host.len() + CRLF.len();
    match s.params {
        Params::Body(ct) => {
            if s.method == Method::Get || ct.is_empty() || !header_value_ok(ct.as_bytes()) {
                return Err(PostErrKind::BadEndpoint);
            }
            let head = m
                + 1
                + path.len()
                + host_line
                + hdrs
                + CONTENT_TYPE.len()
                + ct.len()
                + KEEP_ALIVE_LEN.len();
            debug_assert!(head <= MAX_HEAD);
            Ok((head, 0))
        }
        Params::Query => {
            if path.contains(&b'?') || path.contains(&b'#') {
                return Err(PostErrKind::BadEndpoint);
            }
            let empty = if matches!(s.method, Method::Post | Method::Put) {
                EMPTY_BODY.len()
            } else {
                0
            };
            let tail = host_line + hdrs + empty + KEEP_ALIVE_END.len();
            debug_assert!(tail <= MAX_TAIL);
            Ok((m + 1 + path.len() + 1, tail))
        }
    }
}

/// A boot-time cursor over one region of the wire buffer.
struct Put<'a> {
    buf: &'a mut [u8],
    at: usize,
}

impl Put<'_> {
    #[inline]
    fn put(&mut self, src: &[u8]) {
        // COPY: a template's literals, method, path, host, header parts
        // and content type — ≤ MAX_HEAD (1 503 B) per head, ≤ MAX_TAIL
        // (1 101 B) per query-form tail master — rendered ONCE at boot:
        // a head into the wire region its requests are sent from, a tail
        // into its master behind the window (`stage` copies it from there)
        // — rejected: rendering either per request.
        self.buf[self.at..self.at + src.len()].copy_from_slice(src);
        self.at += src.len();
    }
}

impl ReqWire {
    /// Render every template for `host`. **Boot-only** — the buffer is
    /// allocated here, once: each template's region, a window of
    /// `win_cap` bytes in each.
    pub fn new(host: &str, specs: &[ReqSpec<'_>], win_cap: usize) -> Result<Self, PostErrKind> {
        let h = host.as_bytes();
        if specs.is_empty()
            || specs.len() > MAX_TEMPLATES
            || win_cap == 0
            || win_cap > MAX_BODY_CAP
            || h.is_empty()
            || h.len() > MAX_HOST
            || !visible(h)
        {
            return Err(PostErrKind::BadEndpoint);
        }
        let max_digits = dec_digits(win_cap);
        let mut total = 0usize;
        let mut i = 0;
        while i < specs.len() {
            let (head, tail) = measure(h, &specs[i])?;
            total += match specs[i].params {
                Params::Body(_) => head + max_digits + HEAD_END.len() + win_cap,
                Params::Query => head + win_cap + 2 * tail,
            };
            i += 1;
        }
        let mut wire = vec![0u8; total].into_boxed_slice();
        let zero = Tmpl {
            at: 0,
            head_at: 0,
            head_len: 0,
            win_at: 0,
            max_digits: 0,
            tail_at: 0,
            tail_len: 0,
            method_len: 0,
            path_len: 0,
        };
        let mut tmpl = vec![zero; specs.len()].into_boxed_slice();
        let mut at = 0usize;
        i = 0;
        while i < specs.len() {
            let s = &specs[i];
            let (head_len, tail_len) = measure(h, s)?;
            let mut w = Put {
                buf: &mut wire[..],
                at,
            };
            w.put(s.method.token());
            w.put(b" ");
            w.put(s.path.as_bytes());
            let (win_at, tail_at, digits, next) = match s.params {
                Params::Body(ct) => {
                    w.put(HTTP11);
                    w.put(h);
                    w.put(CRLF);
                    put_headers(&mut w, s.headers);
                    w.put(CONTENT_TYPE);
                    w.put(ct.as_bytes());
                    w.put(KEEP_ALIVE_LEN);
                    debug_assert_eq!(w.at - at, head_len);
                    let win_at = at + head_len + max_digits + HEAD_END.len();
                    // Rendered for the widest length; `stage` moves the
                    // head right for a shorter one.
                    w.at = win_at - HEAD_END.len();
                    w.put(HEAD_END);
                    (win_at, 0, max_digits, win_at + win_cap)
                }
                Params::Query => {
                    w.put(b"?");
                    debug_assert_eq!(w.at - at, head_len);
                    let win_at = at + head_len;
                    let tail_at = win_at + win_cap + tail_len;
                    w.at = tail_at;
                    w.put(HTTP11);
                    w.put(h);
                    w.put(CRLF);
                    put_headers(&mut w, s.headers);
                    if matches!(s.method, Method::Post | Method::Put) {
                        w.put(EMPTY_BODY);
                    }
                    w.put(KEEP_ALIVE_END);
                    debug_assert_eq!(w.at - tail_at, tail_len);
                    (win_at, tail_at, 0, tail_at + tail_len)
                }
            };
            tmpl[i] = Tmpl {
                at: at as u32,
                head_at: at as u32,
                head_len: head_len as u32,
                win_at: win_at as u32,
                max_digits: digits as u32,
                tail_at: tail_at as u32,
                tail_len: if digits == 0 { tail_len as u32 } else { 0 },
                method_len: s.method.token().len() as u32,
                path_len: s.path.len() as u32,
            };
            at = next;
            i += 1;
        }
        debug_assert_eq!(at, total);
        Ok(Self {
            wire,
            tmpl,
            win_cap,
        })
    }

    /// Templates carried.
    #[inline]
    #[must_use]
    pub fn templates(&self) -> usize {
        self.tmpl.len()
    }

    /// Every window's size (`win_cap` at construction).
    #[inline]
    #[must_use]
    pub fn window_cap(&self) -> usize {
        self.win_cap
    }

    /// Template `t`'s window: render the body or the query here, then
    /// stage its length. Its contents survive a request.
    #[inline]
    pub fn window_mut(&mut self, t: usize) -> &mut [u8] {
        debug_assert!(t < self.tmpl.len(), "template {t} does not exist");
        let at = self.tmpl[t].win_at as usize;
        &mut self.wire[at..at + self.win_cap]
    }

    /// Template `t`'s path.
    #[must_use]
    pub fn path(&self, t: usize) -> &str {
        debug_assert!(t < self.tmpl.len(), "template {t} does not exist");
        let m = &self.tmpl[t];
        let at = (m.head_at + m.method_len + 1) as usize;
        core::str::from_utf8(&self.wire[at..at + m.path_len as usize]).unwrap_or("")
    }

    /// Make template `t`'s request whole for a `len`-byte window and
    /// return its span: the one contiguous slice to write. Body form:
    /// the length digits (and the head, when the digit count changed).
    /// Query form: the header tail after the query (`?` dropped for an
    /// empty one). `Overflow` past the window; `BadEndpoint` for a
    /// template that does not exist.
    pub fn stage(&mut self, t: usize, len: usize) -> Result<PayloadSpan, PostErrKind> {
        let Some(&m) = self.tmpl.get(t) else {
            return Err(PostErrKind::BadEndpoint);
        };
        if len > self.win_cap {
            return Err(PostErrKind::Overflow);
        }
        let (at, head_at, win_at) = (m.at as usize, m.head_at as usize, m.win_at as usize);
        if m.max_digits != 0 {
            let d = dec_digits(len);
            let want = at + m.max_digits as usize - d;
            if want != head_at {
                // COPY: the head (≤ MAX_HEAD = 1 503 B; ≤ 605 B for
                // HttpsPost's one template), shifted within its region
                // when the body's digit COUNT differs from the last
                // request's (never between two bodies of one order of
                // magnitude) — rejected: a fixed-width Content-Length
                // (space- or zero-padded: unproven against the live
                // endpoints' parsers) and whitespace-padded bodies.
                self.wire.copy_within(head_at..head_at + m.head_len as usize, want);
                self.tmpl[t].head_at = want as u32;
            }
            let digits_end = win_at - HEAD_END.len();
            let mut v = len;
            let mut k = digits_end;
            while k > digits_end - d {
                k -= 1;
                self.wire[k] = b'0' + (v % 10) as u8;
                v /= 10;
            }
            return Ok(PayloadSpan::new(want, win_at + len));
        }
        let dst = if len == 0 {
            win_at - 1
        } else {
            // An empty query overwrote the `?` last time.
            self.wire[win_at - 1] = b'?';
            win_at + len
        };
        let (tail_at, tail_len) = (m.tail_at as usize, m.tail_len as usize);
        // COLD ONLY — GET recon/status, listenKey: an order's place,
        // cancel or amend MUST use `Params::Body`, which has no such copy.
        // COPY: the header tail (≤ MAX_TAIL = 1 101 B; ~141 B for a Binance
        // GET) after a query whose length varies — rejected: a gather
        // write (rustls 0.23's `write_vectored` collects a `Vec` per call;
        // two writes seal two records), the query rendered right-aligned
        // (its length is unknown before it is rendered and signed), and
        // moving the query instead (as many bytes or more).
        self.wire.copy_within(tail_at..tail_at + tail_len, dst);
        Ok(PayloadSpan::new(head_at, dst + tail_len))
    }

    /// The bytes of a span [`Self::stage`] returned.
    #[inline]
    #[must_use]
    pub fn bytes(&self, span: PayloadSpan) -> &[u8] {
        &self.wire[span.start..span.end]
    }
}

fn put_headers(w: &mut Put<'_>, headers: &[(&str, &str)]) {
    let mut i = 0;
    while i < headers.len() {
        w.put(headers[i].0.as_bytes());
        w.put(b": ");
        w.put(headers[i].1.as_bytes());
        w.put(CRLF);
        i += 1;
    }
}

// ---------------------------------------------------------------
// The answer
// ---------------------------------------------------------------

/// A whole answer: its status, its body's span, and whether the
/// connection must retire with it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The HTTP status.
    pub status: u16,
    /// The body, within the buffer judged.
    pub body: PayloadSpan,
    /// The peer closed, or said it will: the connection is not reusable.
    pub retire: bool,
    /// How the body was delimited: a close-delimited body is whole only
    /// if the close was clean (RFC 9112 §9.8).
    pub framing: BodyFraming,
}

/// Judge the bytes read so far (`buf`, from the status line on):
/// `Some` once the answer is whole — a chunked body is decoded in place
/// first — `None` while more may come. A truncated body is never handed
/// on as the whole answer. Pure: the connection's reader, and a fuzz
/// target's.
pub fn parse_answer(buf: &mut [u8], peer_closed: bool) -> Result<Option<Answer>, PostErrKind> {
    let len = buf.len();
    let (status, header_end, body_start, body_end, framing) = match read_response(buf) {
        HttpResult::Complete {
            status,
            header_end,
            body_start,
            body_end,
            framing,
        } => (status, header_end, body_start, body_end, framing),
        HttpResult::Incomplete if peer_closed => return Err(PostErrKind::Disconnected),
        HttpResult::Incomplete => return Ok(None),
        HttpResult::Malformed => return Err(PostErrKind::BadHttp),
    };
    let retire = peer_closed || head_says_close(&buf[..header_end]);
    match framing {
        BodyFraming::ContentLength(_) => {
            if len >= body_end {
                return Ok(Some(Answer {
                    status,
                    body: PayloadSpan::new(body_start, body_end),
                    retire,
                    framing,
                }));
            }
        }
        BodyFraming::CloseDelimited => {
            if peer_closed {
                return Ok(Some(Answer {
                    status,
                    body: PayloadSpan::new(body_start, len),
                    retire: true,
                    framing,
                }));
            }
        }
        // Located once the terminating chunk has arrived (`chunked_body`
        // leaves an incomplete body untouched, so every read retries it);
        // a one-chunk body is not moved.
        BodyFraming::Chunked => match chunked_body(&mut buf[body_start..]) {
            ChunkedBody::Span { start, len: n } => {
                let a = body_start + start;
                return Ok(Some(Answer {
                    status,
                    body: PayloadSpan::new(a, a + n),
                    retire,
                    framing,
                }));
            }
            ChunkedBody::Incomplete => {}
            ChunkedBody::Malformed => return Err(PostErrKind::BadHttp),
        },
    }
    if peer_closed {
        return Err(PostErrKind::Disconnected);
    }
    Ok(None)
}

// ---------------------------------------------------------------
// The connection
// ---------------------------------------------------------------

/// A connection's sizes and budget, fixed at construction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ConnCfg {
    /// Every template's window, bytes (a body or a query).
    pub win_cap: usize,
    /// The response buffer, bytes: the largest whole answer.
    pub resp_cap: usize,
    /// One request's whole budget — dial, write, read — in ns.
    pub req_timeout_ns: u64,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// No request in flight (connected or not).
    Idle,
    /// A request is staged; TCP + TLS are being established for it.
    Dialing,
    /// The request was written; its answer is being read.
    Awaiting,
}

/// What a step of the connection produced.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Nothing to report: the request is still in flight, or none is.
    Waiting,
    /// The answer is whole. **The status is not a verdict.**
    Done {
        /// The HTTP status.
        status: u16,
        /// The body, in [`HttpsConn::resp`].
        body: PayloadSpan,
    },
    /// The request failed; the connection is closed.
    Failed(PostErr),
}

/// A keep-alive HTTPS connection to one host, driven by its owner's
/// poll (module doc).
pub struct HttpsConn {
    phase: Phase,
    left_host: bool,
    peer_closed: bool,
    /// A read saw the peer close CLEANLY (`close_notify`, confirmed):
    /// the only close that delimits a body with no length of its own.
    /// After a FIN without `close_notify` (rustls' `UnexpectedEof`) a
    /// Content-Length or chunked answer read whole still counts, a
    /// close-delimited one cannot be told from a truncation.
    clean_eof: bool,
    token: Token,
    deadline_ns: u64,
    /// The staged request (`wire`), written once the dial completes.
    send: PayloadSpan,
    resp_len: usize,
    /// `None` until the first request; reopened after a disconnect.
    transport: Option<TlsTransport>,
    wire: ReqWire,
    resp_buf: Box<[u8]>,
    addr: SocketAddr,
    port: u16,
    /// Interned for the process's life (`static_server_name`): the
    /// server name borrows it, so a dial clones a pointer, not a `String`.
    host: &'static str,
    server_name: ServerName<'static>,
    tls_config: Arc<ClientConfig>,
    req_timeout_ns: u64,
    /// Successful dials (TCP + TLS handshakes) over the connection's life.
    dials: u64,
    /// Consecutive failed dials.
    fail_streak: u32,
}

impl HttpsConn {
    /// Resolve and render: every template's head is rendered here, once.
    /// **Boot-only** — the allocations live here. No socket is opened
    /// until the first request; `token` is the one this connection's
    /// socket is registered under in the owner's poll.
    pub fn new(
        host: &str,
        port: u16,
        tls_config: Arc<ClientConfig>,
        specs: &[ReqSpec<'_>],
        cfg: ConnCfg,
        token: Token,
    ) -> Result<Self, PostErrKind> {
        if cfg.resp_cap == 0 || cfg.req_timeout_ns == 0 {
            return Err(PostErrKind::BadEndpoint);
        }
        let wire = ReqWire::new(host, specs, cfg.win_cap)?;
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|_| PostErrKind::Dns)?
            .next()
            .ok_or(PostErrKind::Dns)?;
        let (host, server_name) = static_server_name(host).ok_or(PostErrKind::BadEndpoint)?;
        Ok(Self {
            phase: Phase::Idle,
            left_host: false,
            peer_closed: false,
            clean_eof: false,
            token,
            deadline_ns: 0,
            send: PayloadSpan::new(0, 0),
            resp_len: 0,
            transport: None,
            wire,
            resp_buf: vec![0u8; cfg.resp_cap].into_boxed_slice(),
            addr,
            port,
            host,
            server_name,
            tls_config,
            req_timeout_ns: cfg.req_timeout_ns,
            dials: 0,
            fail_streak: 0,
        })
    }

    /// The host this connection talks to.
    #[inline]
    #[must_use]
    pub fn host(&self) -> &str {
        self.host
    }

    /// Template `t`'s path.
    #[inline]
    #[must_use]
    pub fn path(&self, t: usize) -> &str {
        self.wire.path(t)
    }

    /// Templates carried.
    #[inline]
    #[must_use]
    pub fn templates(&self) -> usize {
        self.wire.templates()
    }

    /// Every window's size.
    #[inline]
    #[must_use]
    pub fn window_cap(&self) -> usize {
        self.wire.window_cap()
    }

    /// Template `t`'s window ([`ReqWire::window_mut`]). **Empty while a
    /// request is being dialed** — its bytes are not in rustls yet, and
    /// a render into its window would put a different request on the
    /// wire than the one the owner booked; an empty window makes that
    /// render fail instead, so "not sent" stays true.
    #[inline]
    pub fn window_mut(&mut self, t: usize) -> &mut [u8] {
        if self.phase == Phase::Dialing {
            debug_assert!(false, "a window was written while its request waits for the dial");
            return &mut [];
        }
        self.wire.window_mut(t)
    }

    /// The response bytes of the last [`Progress::Done`]; valid until the
    /// next [`Self::start`].
    #[inline]
    #[must_use]
    pub fn resp(&self) -> &[u8] {
        &self.resp_buf[..self.resp_len]
    }

    /// No request is in flight: [`Self::start`] may be called.
    #[inline]
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// Whether a connection is currently open.
    #[inline]
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.transport.is_some()
    }

    /// Successful dials over the connection's life: 1 on a healthy
    /// keep-alive endpoint; ≈ requests on one that closes after every
    /// answer (a handshake — and its allocations — per request).
    #[inline]
    #[must_use]
    pub const fn dials(&self) -> u64 {
        self.dials
    }

    /// Consecutive failed dials (0 after a successful one): the owner's
    /// cue for backoff and [`Self::reresolve`].
    #[inline]
    #[must_use]
    pub const fn fail_streak(&self) -> u32 {
        self.fail_streak
    }

    /// Drop the connection and any request in flight. The next request
    /// redials.
    pub fn close(&mut self) {
        self.transport = None;
        self.phase = Phase::Idle;
        self.resp_len = 0;
    }

    /// Resolve the host again (a CDN-fronted endpoint rotates
    /// addresses). **Blocking DNS** — so crate-private: only
    /// [`crate::HttpsPost`], on its own blocking thread, calls it. An
    /// owner whose thread serves other sockets resolves on a cold thread
    /// and hands the address in through [`Self::set_addr`]. Whether the
    /// address was replaced.
    pub(crate) fn reresolve(&mut self) -> bool {
        let fresh = (self.host, self.port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next());
        match fresh {
            Some(a) => {
                self.addr = a;
                true
            }
            None => false,
        }
    }

    /// The address the next dial goes to (one resolved elsewhere).
    #[inline]
    pub fn set_addr(&mut self, addr: SocketAddr) {
        self.addr = addr;
    }

    /// Stage template `t` with a `len`-byte window and send it — dialing
    /// first when no connection is open (or the kept-alive one died
    /// idle). Returns at once; the answer comes through
    /// [`Self::on_event`]. An `Err` means nothing more will happen for
    /// this request.
    pub fn start(
        &mut self,
        t: usize,
        len: usize,
        registry: &Registry,
        now_ns: u64,
    ) -> Result<(), PostErr> {
        if self.phase != Phase::Idle {
            debug_assert!(false, "HttpsConn::start with a request in flight");
            return Err(not_left(PostErrKind::Busy));
        }
        self.send = self.wire.stage(t, len).map_err(not_left)?;
        self.resp_len = 0;
        self.left_host = false;
        self.peer_closed = false;
        self.clean_eof = false;
        self.deadline_ns = now_ns.saturating_add(self.req_timeout_ns);
        if self.transport.is_some() && !self.still_open() {
            self.transport = None;
        }
        if self.transport.is_none() {
            return match self.dial(registry) {
                Ok(()) => {
                    self.phase = Phase::Dialing;
                    Ok(())
                }
                Err(err) => {
                    self.fail_streak = self.fail_streak.saturating_add(1);
                    Err(self.abort(err))
                }
            };
        }
        self.phase = Phase::Awaiting;
        match self.send_request(registry) {
            Ok(()) => Ok(()),
            Err(err) => Err(self.abort(err)),
        }
    }

    /// One readiness event for this connection's token.
    pub fn on_event(&mut self, ev: &Event, registry: &Registry) -> Progress {
        match self.phase {
            Phase::Idle => {
                self.idle_event(ev, registry);
                Progress::Waiting
            }
            Phase::Dialing => self.dial_event(ev, registry),
            Phase::Awaiting => self.await_event(ev, registry),
        }
    }

    /// The deadline: a request still in flight at its deadline fails
    /// with `Timeout` (and `left_host` as it stands).
    pub fn on_tick(&mut self, now_ns: u64) -> Progress {
        if self.phase == Phase::Idle || now_ns < self.deadline_ns {
            return Progress::Waiting;
        }
        if self.phase == Phase::Dialing {
            self.fail_streak = self.fail_streak.saturating_add(1);
        }
        Progress::Failed(self.abort(PostErrKind::Timeout))
    }

    /// Fail the request in flight: close, and say whether it left.
    pub(crate) fn abort(&mut self, err: PostErrKind) -> PostErr {
        let e = PostErr {
            err,
            left_host: self.left_host,
        };
        self.close();
        e
    }

    /// Before reusing a kept-alive connection: one non-blocking read. No
    /// request is outstanding, so anything but `WouldBlock` — EOF,
    /// `close_notify`, an unsolicited byte, an error — means the peer is
    /// gone or out of step, and the request must dial fresh.
    fn still_open(&mut self) -> bool {
        let Some(t) = self.transport.as_mut() else {
            return false;
        };
        let mut probe = [0u8; 1];
        matches!(t.read(&mut probe), Err(ref e) if e.kind() == io::ErrorKind::WouldBlock)
    }

    fn dial(&mut self, registry: &Registry) -> Result<(), PostErrKind> {
        let mut t =
            TlsTransport::connect(self.addr, self.server_name.clone(), self.tls_config.clone())
                .map_err(|_| PostErrKind::Disconnected)?;
        t.register(registry, self.token)
            .map_err(|_| PostErrKind::Disconnected)?;
        self.transport = Some(t);
        Ok(())
    }

    /// Write the staged request as ONE slice (one TLS record while it is
    /// ≤ 16 KiB) and push it to the socket now.
    fn send_request(&mut self, registry: &Registry) -> Result<(), PostErrKind> {
        let Some(t) = self.transport.as_mut() else {
            return Err(PostErrKind::Disconnected);
        };
        self.left_host = true;
        write_all(t, self.wire.bytes(self.send))?;
        // `write` only queues into rustls; flush now (no writable edge is
        // armed after the handshake — without this the answer waits for
        // a poll timeout: HlHttp's E7 finding).
        t.flush().map_err(|_| PostErrKind::Disconnected)?;
        t.reregister(registry, self.token)
            .map_err(|_| PostErrKind::Disconnected)
    }

    /// An event with no request in flight: the peer may be closing the
    /// kept-alive connection (idle timeout, `close_notify`).
    fn idle_event(&mut self, ev: &Event, registry: &Registry) {
        let Some(t) = self.transport.as_mut() else {
            return;
        };
        let keep = match t.pump(ev) {
            Ok(Status::Closed) | Err(_) => false,
            Ok(_) => t.reregister(registry, self.token).is_ok(),
        };
        if !keep {
            self.transport = None;
        }
    }

    fn dial_event(&mut self, ev: &Event, registry: &Registry) -> Progress {
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
                self.fail_streak = 0;
                self.phase = Phase::Awaiting;
                match self.send_request(registry) {
                    Ok(()) => Progress::Waiting,
                    Err(err) => Progress::Failed(self.abort(err)),
                }
            }
            Status::Handshaking => {
                let ok = match self.transport.as_mut() {
                    Some(t) => t.reregister(registry, self.token).is_ok(),
                    None => false,
                };
                if ok {
                    Progress::Waiting
                } else {
                    self.fail_streak = self.fail_streak.saturating_add(1);
                    Progress::Failed(self.abort(PostErrKind::Disconnected))
                }
            }
            Status::Closed => {
                self.fail_streak = self.fail_streak.saturating_add(1);
                Progress::Failed(self.abort(PostErrKind::Disconnected))
            }
        }
    }

    fn await_event(&mut self, ev: &Event, registry: &Registry) -> Progress {
        if let Err(err) = self.fill(ev) {
            return Progress::Failed(self.abort(err));
        }
        match parse_answer(&mut self.resp_buf[..self.resp_len], self.peer_closed) {
            Ok(Some(a)) if a.framing == BodyFraming::CloseDelimited && !self.clean_eof => {
                // Delimited by the close alone, and no read saw that close
                // clean: a truncation — by the peer, or by our own full
                // buffer — would look the same (RFC 9112 §9.8).
                let err = if self.resp_len >= self.resp_buf.len() {
                    PostErrKind::Overflow
                } else {
                    PostErrKind::Disconnected
                };
                Progress::Failed(self.abort(err))
            }
            Ok(Some(a)) => {
                self.phase = Phase::Idle;
                if a.retire {
                    // The peer closed, or said it will: the answer is
                    // whole, the connection is not reusable. Keep the
                    // response; drop only the transport.
                    self.transport = None;
                }
                Progress::Done {
                    status: a.status,
                    body: a.body,
                }
            }
            Ok(None) => {
                if self.resp_len >= self.resp_buf.len() {
                    return Progress::Failed(self.abort(PostErrKind::Overflow));
                }
                let ok = match self.transport.as_mut() {
                    Some(t) => t.reregister(registry, self.token).is_ok(),
                    None => false,
                };
                if ok {
                    Progress::Waiting
                } else {
                    Progress::Failed(self.abort(PostErrKind::Disconnected))
                }
            }
            Err(err) => Progress::Failed(self.abort(err)),
        }
    }

    /// Pump the event and drain every plaintext byte it made available
    /// (mio is edge-triggered: read to `WouldBlock`).
    fn fill(&mut self, ev: &Event) -> Result<(), PostErrKind> {
        let Some(t) = self.transport.as_mut() else {
            return Err(PostErrKind::Disconnected);
        };
        match t.pump(ev) {
            Ok(Status::Closed) => self.peer_closed = true,
            Ok(_) => {}
            Err(_) => return Err(PostErrKind::Disconnected),
        }
        while self.resp_len < self.resp_buf.len() {
            match t.read(&mut self.resp_buf[self.resp_len..]) {
                Ok(0) => {
                    self.peer_closed = true;
                    // `Ok(0)` may be the transport's pull-through meeting
                    // TCP EOF, which does not say whether `close_notify`
                    // came first. Once rustls has seen the EOF its reader
                    // does: `Ok(0)` only after a clean close,
                    // `UnexpectedEof` otherwise.
                    let mut probe = [0u8; 1];
                    self.clean_eof = matches!(t.read(&mut probe), Ok(0));
                    break;
                }
                Ok(n) => self.resp_len += n,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                // A FIN without `close_notify`: every byte before it is
                // authentic, so an answer its own framing proves whole
                // still counts (RFC 9112 §9.8) — it was a lost answer and
                // a false in-doubt before BX5.
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.peer_closed = true;
                    break;
                }
                Err(_) => return Err(PostErrKind::Disconnected),
            }
        }
        Ok(())
    }

    /// The staging under test: template `t`'s wire bytes for `len`.
    #[cfg(test)]
    pub(crate) fn staged(&mut self, t: usize, len: usize) -> Result<&[u8], PostErrKind> {
        let s = self.wire.stage(t, len)?;
        Ok(self.wire.bytes(s))
    }
}

/// The host as a `&'static str` — interned: each distinct host is
/// leaked ONCE for the process's life (≤ [`MAX_HOST`] B each; a process
/// talks to a handful), however many connection objects name it — and a
/// TLS server name BORROWING it, so cloning that name per dial copies a
/// pointer where an owned one copies a `String` (both BX5 audits).
/// Construction-time only (a lock and a scan). `None` when rustls
/// refuses the name.
pub(crate) fn static_server_name(host: &str) -> Option<(&'static str, ServerName<'static>)> {
    static HOSTS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut hosts = match HOSTS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut i = 0;
    let mut found = None;
    while i < hosts.len() {
        if hosts[i] == host {
            found = Some(hosts[i]);
            break;
        }
        i += 1;
    }
    let host: &'static str = match found {
        Some(h) => h,
        None => {
            // COPY: a host not seen before (≤ MAX_HOST = 253 B), ONCE per
            // process, into the leaked `&'static str` every server name
            // for it borrows — rejected: an owned `ServerName` (a `String`
            // per dial) and reading the host out of a template (not
            // `'static`).
            let h: &'static str = Box::leak(host.to_owned().into_boxed_str());
            hosts.push(h);
            h
        }
    };
    ServerName::try_from(host).ok().map(|name| (host, name))
}

/// Write all of `src`. A TLS transport never blocks on `write` (it
/// buffers), so `WouldBlock` is a request its buffer cannot take.
fn write_all<T: Transport>(t: &mut T, src: &[u8]) -> Result<(), PostErrKind> {
    let mut off = 0usize;
    while off < src.len() {
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

    const KEY: &str = "vmPUZE6mv9SD5VNHk4HlWFsOr6aKE2zvsw0MuIgwCIPy6utIco14y7Ju91duEh8A";

    fn specs() -> [ReqSpec<'static>; 5] {
        const H: &[(&str, &str)] = &[("X-MBX-APIKEY", KEY)];
        [
            ReqSpec {
                method: Method::Post,
                path: "/fapi/v1/order",
                params: Params::Body("application/x-www-form-urlencoded"),
                headers: H,
            },
            ReqSpec {
                method: Method::Delete,
                path: "/fapi/v1/order",
                params: Params::Body("application/x-www-form-urlencoded"),
                headers: H,
            },
            ReqSpec {
                method: Method::Get,
                path: "/fapi/v1/openOrders",
                params: Params::Query,
                headers: H,
            },
            ReqSpec {
                method: Method::Put,
                path: "/fapi/v1/listenKey",
                params: Params::Query,
                headers: H,
            },
            ReqSpec {
                method: Method::Get,
                path: "/fapi/v1/time",
                params: Params::Query,
                headers: &[],
            },
        ]
    }

    fn stage_str(w: &mut ReqWire, t: usize, content: &[u8]) -> String {
        w.window_mut(t)[..content.len()].copy_from_slice(content);
        let s = w.stage(t, content.len()).expect("fits");
        String::from_utf8(w.bytes(s).to_vec()).expect("ascii")
    }

    #[test]
    fn a_body_template_renders_the_exact_request() {
        let mut w = ReqWire::new("fapi.binance.com", &specs(), 2048).expect("specs");
        let body = b"symbol=BTCUSDT&side=BUY&type=LIMIT&signature=x%2By";
        assert_eq!(
            stage_str(&mut w, 0, body),
            format!(
                "POST /fapi/v1/order HTTP/1.1\r\nHost: fapi.binance.com\r\nX-MBX-APIKEY: {KEY}\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\nConnection: keep-alive\r\n\
                 Content-Length: {}\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(body)
            )
        );
        assert!(stage_str(&mut w, 1, b"orderId=1").starts_with("DELETE /fapi/v1/order HTTP/1.1\r\n"));
    }

    #[test]
    fn a_query_template_carries_its_tail_after_any_query() {
        let mut w = ReqWire::new("fapi.binance.com", &specs(), 512).expect("specs");
        let tail = format!(
            " HTTP/1.1\r\nHost: fapi.binance.com\r\nX-MBX-APIKEY: {KEY}\r\nConnection: keep-alive\r\n\r\n"
        );
        let mut i = 0;
        while i < 3 {
            let q = ["symbol=BTCUSDT&timestamp=1&signature=ab", "a=1", ""][i];
            let want = if q.is_empty() {
                format!("GET /fapi/v1/openOrders{tail}")
            } else {
                format!("GET /fapi/v1/openOrders?{q}{tail}")
            };
            assert_eq!(stage_str(&mut w, 2, q.as_bytes()), want, "query {q:?}");
            i += 1;
        }
        // After an empty query, the `?` is back for the next one.
        assert_eq!(
            stage_str(&mut w, 2, b"x=2"),
            format!("GET /fapi/v1/openOrders?x=2{tail}")
        );
        // A POST/PUT without a body still says so; a GET never does.
        let put = stage_str(&mut w, 3, b"");
        assert!(put.starts_with("PUT /fapi/v1/listenKey HTTP/1.1\r\n"), "{put}");
        assert!(put.ends_with("Content-Length: 0\r\nConnection: keep-alive\r\n\r\n"), "{put}");
        assert_eq!(
            stage_str(&mut w, 4, b""),
            "GET /fapi/v1/time HTTP/1.1\r\nHost: fapi.binance.com\r\nConnection: keep-alive\r\n\r\n"
        );
        assert_eq!(w.path(2), "/fapi/v1/openOrders");
        assert_eq!(w.path(0), "/fapi/v1/order");
    }

    #[test]
    fn templates_are_independent_regions_and_windows_never_move() {
        let mut w = ReqWire::new("h", &specs(), 1000).expect("specs");
        let mut at = [0usize; 5];
        let mut t = 0;
        while t < 5 {
            at[t] = w.window_mut(t).as_ptr() as usize;
            t += 1;
        }
        // Bodies of 1–4 digits, in any order, on two body templates, with
        // queries in between: every request renders exactly, no window
        // moves, and no template disturbs another.
        let lens = [7usize, 10, 999, 1000, 1, 55];
        let mut k = 0;
        while k < lens.len() {
            let body = vec![b'a' + k as u8; lens[k]];
            let s = stage_str(&mut w, k % 2, &body);
            assert!(
                s.ends_with(&format!("Content-Length: {}\r\n\r\n{}", lens[k], String::from_utf8_lossy(&body))),
                "{k}"
            );
            let q = stage_str(&mut w, 2, b"q=1");
            assert!(q.starts_with("GET /fapi/v1/openOrders?q=1 HTTP/1.1\r\n"), "{q}");
            assert_eq!(w.path(k % 2), "/fapi/v1/order");
            k += 1;
        }
        t = 0;
        while t < 5 {
            assert_eq!(w.window_mut(t).as_ptr() as usize, at[t], "window {t} moved");
            t += 1;
        }
        assert_eq!(w.stage(0, 1001), Err(PostErrKind::Overflow));
        assert_eq!(w.stage(2, 1001), Err(PostErrKind::Overflow));
        assert_eq!(w.stage(5, 1), Err(PostErrKind::BadEndpoint), "no such template: refused, no panic");
        // The largest query fills the window exactly; the tail follows.
        let full = vec![b'z'; 1000];
        assert!(stage_str(&mut w, 2, &full).ends_with("Connection: keep-alive\r\n\r\n"));
    }

    #[test]
    fn bad_specs_are_refused_at_construction() {
        let ok = specs();
        let bad = |s: ReqSpec<'_>| {
            let mut v = ok;
            v[0] = s;
            matches!(ReqWire::new("h", &v, 64), Err(PostErrKind::BadEndpoint))
        };
        let base = ok[0];
        assert!(bad(ReqSpec { path: "fapi", ..base }), "relative path");
        assert!(bad(ReqSpec { path: "/é", ..base }), "non-ASCII path");
        assert!(
            bad(ReqSpec {
                method: Method::Get,
                ..base
            }),
            "a GET has no body"
        );
        assert!(
            bad(ReqSpec {
                params: Params::Query,
                path: "/a?b=1",
                ..base
            }),
            "a query-form path carries no query"
        );
        assert!(
            bad(ReqSpec {
                headers: &[("X-A", "v\r\nX-B: injected")],
                ..base
            }),
            "header injection"
        );
        assert!(bad(ReqSpec { headers: &[("Bad Name", "v")], ..base }));
        assert!(bad(ReqSpec { headers: &[("X-A", "")], ..base }));
        assert!(
            bad(ReqSpec {
                params: Params::Body(""),
                ..base
            }),
            "a body needs a content type"
        );
        let many: [(&str, &str); 5] = [("A", "1"), ("B", "2"), ("C", "3"), ("D", "4"), ("E", "5")];
        assert!(bad(ReqSpec { headers: &many, ..base }), "too many headers");
        assert!(matches!(ReqWire::new("h", &[], 64), Err(PostErrKind::BadEndpoint)));
        assert!(matches!(ReqWire::new("h", &ok, 0), Err(PostErrKind::BadEndpoint)));
        assert!(matches!(ReqWire::new("h", &ok, MAX_BODY_CAP + 1), Err(PostErrKind::BadEndpoint)));
        assert!(matches!(ReqWire::new("", &ok, 64), Err(PostErrKind::BadEndpoint)));
        assert!(matches!(ReqWire::new("a b", &ok, 64), Err(PostErrKind::BadEndpoint)));
    }

    fn parse(raw: &[u8], closed: bool) -> Result<Option<(u16, Vec<u8>, bool)>, PostErrKind> {
        let mut b = raw.to_vec();
        Ok(parse_answer(&mut b, closed)?
            .map(|a| (a.status, b[a.body.start..a.body.end].to_vec(), a.retire)))
    }

    #[test]
    fn an_answer_is_whole_only_when_its_framing_says_so() {
        let cl = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse(cl, false), Ok(Some((200, b"hello".to_vec(), false))));
        assert_eq!(parse(&cl[..cl.len() - 1], false), Ok(None), "one byte short");
        assert_eq!(
            parse(&cl[..cl.len() - 1], true),
            Err(PostErrKind::Disconnected),
            "a truncated body is never whole"
        );
        let close = b"HTTP/1.1 400 Bad\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse(close, false), Ok(Some((400, b"{}".to_vec(), true))));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse(chunked, false), Ok(Some((200, b"abcde".to_vec(), false))));
        assert_eq!(parse(&chunked[..chunked.len() - 2], false), Ok(None));
        let eof = b"HTTP/1.1 200 OK\r\n\r\nuntil-close";
        assert_eq!(parse(eof, false), Ok(None));
        assert_eq!(parse(eof, true), Ok(Some((200, b"until-close".to_vec(), true))));
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nContent", false), Ok(None));
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nContent", true), Err(PostErrKind::Disconnected));
        assert_eq!(parse(b"SSH-2.0\r\n\r\n", false), Err(PostErrKind::BadHttp));
    }

    #[test]
    fn a_connection_refuses_bad_sizes_and_hosts_at_construction() {
        let cfg = TlsTransport::default_client_config();
        let c = ConnCfg {
            win_cap: 64,
            resp_cap: 64,
            req_timeout_ns: 1,
        };
        let mk = |host: &str, c: ConnCfg| HttpsConn::new(host, 1, cfg.clone(), &specs(), c, Token(3));
        assert!(matches!(mk("no-such-host.invalid.example", c), Err(PostErrKind::Dns)));
        assert!(matches!(mk("127.0.0.1", ConnCfg { resp_cap: 0, ..c }), Err(PostErrKind::BadEndpoint)));
        assert!(matches!(mk("127.0.0.1", ConnCfg { req_timeout_ns: 0, ..c }), Err(PostErrKind::BadEndpoint)));
        let h = mk("127.0.0.1", c).expect("loopback");
        assert!(h.is_idle() && !h.is_connected(), "no socket is opened at construction");
        assert_eq!((h.host(), h.templates(), h.window_cap()), ("127.0.0.1", 5, 64));
    }

    #[test]
    fn errors_render_for_an_operator() {
        let e = PostErr {
            err: PostErrKind::Timeout,
            left_host: true,
        };
        assert!(e.to_string().contains("deadline"), "{e}");
        assert!(e.to_string().contains("left the host"), "{e}");
        assert!(PostErrKind::Busy.to_string().contains("in flight"));
    }

    #[test]
    fn a_host_is_interned_once_however_many_connections_name_it() {
        let (a, _) = static_server_name("intern-test.example").expect("name");
        let (b, _) = static_server_name("intern-test.example").expect("name");
        assert!(core::ptr::eq(a, b), "the second connection reuses the first's host");
        let (c, _) = static_server_name("intern-test-2.example").expect("name");
        assert!(!core::ptr::eq(a, c));
        assert!(static_server_name("not a host").is_none());
    }
}

#[cfg(test)]
mod proptests {
    //! The answer judge is a parser over bytes a server chose: it must
    //! never panic, never name a body outside the buffer, and agree with
    //! the framing it was given.
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn parse_answer_never_panics_and_stays_in_bounds(
            input in proptest::collection::vec(any::<u8>(), 0..2048),
            closed in any::<bool>(),
        ) {
            let mut b = input.clone();
            if let Ok(Some(a)) = parse_answer(&mut b, closed) {
                prop_assert!(a.body.start <= a.body.end && a.body.end <= b.len());
            }
        }

        #[test]
        fn a_content_length_answer_is_found_whole_in_any_split(
            body in proptest::collection::vec(any::<u8>(), 0..300),
            cut in 0usize..400,
        ) {
            let mut wire = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            wire.extend_from_slice(&body);
            let cut = cut.min(wire.len());
            let mut part = wire[..cut].to_vec();
            let got = parse_answer(&mut part, false);
            if cut < wire.len() {
                prop_assert_eq!(got, Ok(None));
            }
            let mut whole = wire.clone();
            match parse_answer(&mut whole, false) {
                Ok(Some(a)) => prop_assert_eq!(&whole[a.body.start..a.body.end], &body[..]),
                other => return Err(TestCaseError::fail(format!("{other:?}"))),
            }
        }

        #[test]
        fn a_query_stage_is_always_one_well_formed_request(
            q in "[A-Za-z0-9=&%._-]{0,200}",
        ) {
            let specs = [ReqSpec {
                method: Method::Get,
                path: "/api/v3/openOrders",
                params: Params::Query,
                headers: &[("X-MBX-APIKEY", "k")],
            }];
            let mut w = ReqWire::new("api.binance.com", &specs, 256).expect("spec");
            w.window_mut(0)[..q.len()].copy_from_slice(q.as_bytes());
            let s = w.stage(0, q.len()).expect("fits");
            let req = w.bytes(s);
            let head_end = req.windows(4).position(|x| x == b"\r\n\r\n").expect("blank line");
            prop_assert_eq!(head_end + 4, req.len(), "nothing after the blank line");
            let line_end = req.windows(2).position(|x| x == b"\r\n").expect("request line");
            let line = &req[..line_end];
            let want = if q.is_empty() {
                "GET /api/v3/openOrders HTTP/1.1".to_string()
            } else {
                format!("GET /api/v3/openOrders?{q} HTTP/1.1")
            };
            prop_assert_eq!(line, want.as_bytes());
        }
    }
}
