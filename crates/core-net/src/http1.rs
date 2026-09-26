// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # HTTP/1.1 minimal codec
//!
//! Zero-alloc, pure-byte-scanner HTTP/1.1 client codec used by the
//! boot-time REST discovery path (`boot_http`), the CLOB dispatcher and
//! the keep-alive clients ([`crate::HttpsPost`], [`crate::HttpsReq`]).
//! No cookies, no compression (the request hard-codes
//! `Accept-Encoding: identity`), no automatic redirect chasing.
//!
//! ## Requests (HC2)
//!
//! ONE writer, [`write_request_head`], renders every request head: any
//! [`Method`], an origin-form target (path + optional query), and the
//! caller's extra headers (Hypercall's signed `X-Hypercall-*` pair).
//! [`request_head_len`] sizes the head first, so a keep-alive client can
//! render it flush against a body already in place — one contiguous
//! slice, one write. [`write_get_request`] / [`write_post_request`] are
//! thin wrappers over it and emit the bytes they always did.
//!
//! ## Why hand-roll?
//!
//! The workspace bans `reqwest`, `hyper`'s client by default, and any
//! dependency that pulls tokio. The consumers' payload shapes are
//! small enough that
//! a ~150-line handwritten codec fits the doctrine: all work is over
//! `&[u8]` / `&mut [u8]`; the caller owns buffers; body extraction is
//! zero-copy (a `Range<usize>` into the caller's buffer).
//!
//! ## Supported framing
//!
//! * Explicit `Content-Length: N`.
//! * `Transfer-Encoding: chunked` — dechunked in-place into a caller-owned
//!   scratch slice via [`dechunk_in_place`].
//! * Connection-close framing (treated as `Content-Length: remaining`).
//!
//! ## Non-goals (deferred to Phase 1d)
//!
//! * HTTP/2 (clob-dispatcher uses `hyper` for that, not this module).
//! * Gzip / brotli bodies.
//! * Redirects (`3xx` is reported as [`HttpResult::Malformed`]).
//! * Keep-alive reuse — the codec is pure functions over buffers;
//!   connection lifetime belongs to the transport-owning callers.

// ---------------------------------------------------------------
// Errors + result types
// ---------------------------------------------------------------

/// Reason a request could not be rendered.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HttpErr {
    /// Caller's output buffer is too small to fit the request.
    BufferTooSmall,
    /// HC2: a head field would break the request's framing — a CR, LF or
    /// NUL in any field (header injection), a target that is not
    /// visible-ASCII origin-form (`/…`), an empty or non-visible host, or
    /// a header name that is empty or carries `:` / whitespace. Refused,
    /// never rewritten.
    BadHead,
}

/// Outcome of a single call to [`read_response`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HttpResult {
    /// Need more bytes — caller must read from the socket and retry.
    Incomplete,
    /// A complete response has been parsed.
    Complete {
        /// HTTP status code (e.g. 200, 404).
        status: u16,
        /// Exclusive end of the response header region (first byte of
        /// body, if any).
        header_end: usize,
        /// Inclusive start of the (possibly-empty) body region.
        body_start: usize,
        /// Exclusive end of the body region.
        body_end: usize,
        /// How the body was framed.
        framing: BodyFraming,
    },
    /// The buffer does not parse as HTTP/1.1.
    Malformed,
}

/// How the response body is framed on the wire.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BodyFraming {
    /// `Content-Length: N` header present; body occupies exactly `N`
    /// bytes following the blank line.
    ContentLength(u64),
    /// `Transfer-Encoding: chunked`. The caller should invoke
    /// [`dechunk_in_place`] on the body region to obtain raw bytes.
    Chunked,
    /// No framing headers present. Body extends to EOF of the connection
    /// — the caller is responsible for closing the socket to terminate.
    CloseDelimited,
}

// ---------------------------------------------------------------
// Request serialization
// ---------------------------------------------------------------

/// Request method (HC2) — the request line's first token.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// `GET` — no body; no `Content-Length` unless a body is given.
    Get = 0,
    /// `POST`.
    Post = 1,
    /// `PUT` (Hypercall: replace an order).
    Put = 2,
    /// `DELETE` — may carry a body (Hypercall cancels with one).
    Delete = 3,
}

impl Method {
    /// The request-line token.
    #[inline(always)]
    #[must_use]
    pub const fn token(self) -> &'static [u8] {
        match self {
            Self::Get => b"GET",
            Self::Post => b"POST",
            Self::Put => b"PUT",
            Self::Delete => b"DELETE",
        }
    }
}

/// One extra request header, `(name, value)`, sent verbatim after the
/// fixed ones.
pub type Header<'a> = (&'a [u8], &'a [u8]);

/// Everything in a request head but the body bytes (HC2).
///
/// Rendered as:
///
/// ```text
/// {METHOD} {target} HTTP/1.1
/// Host: {host}
/// User-Agent: {user_agent}
/// Accept: */*
/// Accept-Encoding: identity
/// Content-Type: {content_type}      (only when Some)
/// Content-Length: {body_len}        (every method but a bodiless GET)
/// {name}: {value}                   (each `extra` header, in order)
/// Connection: keep-alive | close
/// ```
#[derive(Copy, Clone, Debug)]
pub struct ReqHead<'a> {
    /// Request method.
    pub method: Method,
    /// `Host` header value.
    pub host: &'a [u8],
    /// Origin-form request target: `/path` plus an optional `?query`.
    pub target: &'a [u8],
    /// `User-Agent` header value.
    pub user_agent: &'a [u8],
    /// `Content-Type` header value, when the request carries one.
    pub content_type: Option<&'a [u8]>,
    /// Extra headers, after the fixed set.
    pub extra: &'a [Header<'a>],
    /// `Connection: keep-alive` (true) or `close`.
    pub keep_alive: bool,
}

const H_HOST: &[u8] = b" HTTP/1.1\r\nHost: ";
const H_UA: &[u8] = b"\r\nUser-Agent: ";
const H_ACCEPT: &[u8] = b"\r\nAccept: */*\r\nAccept-Encoding: identity\r\n";
const H_CT: &[u8] = b"Content-Type: ";
const H_CL: &[u8] = b"Content-Length: ";
const H_SEP: &[u8] = b": ";
const CRLF: &[u8] = b"\r\n";
const H_KEEP: &[u8] = b"Connection: keep-alive\r\n\r\n";
const H_CLOSE: &[u8] = b"Connection: close\r\n\r\n";

/// The `Connection` line and the blank line that ends the head.
#[inline]
const fn connection_line(keep_alive: bool) -> &'static [u8] {
    if keep_alive {
        H_KEEP
    } else {
        H_CLOSE
    }
}

/// A field value that cannot break the head's framing: no CR, LF, NUL.
#[inline]
fn field_ok(v: &[u8]) -> bool {
    memchr::memchr3(b'\r', b'\n', 0, v).is_none()
}

/// A header name: a non-empty run of visible ASCII without `:`.
#[inline]
fn name_ok(n: &[u8]) -> bool {
    !n.is_empty() && n.iter().all(|&b| b.is_ascii_graphic() && b != b':')
}

impl ReqHead<'_> {
    /// `Content-Length` goes on every method that may carry a body (POST,
    /// PUT, DELETE) and on any request with a non-empty body; a bodiless
    /// GET sends none.
    #[inline]
    const fn sends_length(&self, body_len: usize) -> bool {
        !matches!(self.method, Method::Get) || body_len > 0
    }

    /// The framing law ([`HttpErr::BadHead`]).
    fn validate(&self) -> Result<(), HttpErr> {
        // Request target and host: visible ASCII only (RFC 9112 §3.2 /
        // §3.2.2) — a space would end the target, a CR/LF would end the
        // line.
        let visible = |v: &[u8]| v.iter().all(u8::is_ascii_graphic);
        let mut ok = self.target.first() == Some(&b'/')
            && visible(self.target)
            && !self.host.is_empty()
            && visible(self.host)
            && field_ok(self.user_agent)
            && self.content_type.is_none_or(field_ok);
        let mut i = 0usize;
        while ok && i < self.extra.len() {
            ok = name_ok(self.extra[i].0) && field_ok(self.extra[i].1);
            i += 1;
        }
        if ok {
            Ok(())
        } else {
            Err(HttpErr::BadHead)
        }
    }
}

/// Byte length of the head [`write_request_head`] renders for `head`
/// and a `body_len`-byte body — validated, so a head that sizes is a
/// head that renders.
pub fn request_head_len(head: &ReqHead<'_>, body_len: usize) -> Result<usize, HttpErr> {
    head.validate()?;
    let mut n = head.method.token().len() + 1 + head.target.len() + H_HOST.len() + head.host.len();
    n += H_UA.len() + head.user_agent.len() + H_ACCEPT.len();
    if let Some(ct) = head.content_type {
        n += H_CT.len() + ct.len() + CRLF.len();
    }
    if head.sends_length(body_len) {
        n += H_CL.len() + dec_digits(body_len as u64) + CRLF.len();
    }
    let mut i = 0usize;
    while i < head.extra.len() {
        n += head.extra[i].0.len() + H_SEP.len() + head.extra[i].1.len() + CRLF.len();
        i += 1;
    }
    n += connection_line(head.keep_alive).len();
    Ok(n)
}

/// Render the head of a request with a `body_len`-byte body into `dst`
/// (the body itself is the caller's: a keep-alive client renders it in
/// place, then this head flush against it). Returns bytes written. Zero-
/// alloc; [`HttpErr::BufferTooSmall`] if `dst` can't fit it,
/// [`HttpErr::BadHead`] on a field that would break the framing.
pub fn write_request_head(
    dst: &mut [u8],
    head: &ReqHead<'_>,
    body_len: usize,
) -> Result<usize, HttpErr> {
    head.validate()?;
    let mut cursor = 0usize;
    push(dst, &mut cursor, head.method.token())?;
    push(dst, &mut cursor, b" ")?;
    push(dst, &mut cursor, head.target)?;
    push(dst, &mut cursor, H_HOST)?;
    push(dst, &mut cursor, head.host)?;
    push(dst, &mut cursor, H_UA)?;
    push(dst, &mut cursor, head.user_agent)?;
    push(dst, &mut cursor, H_ACCEPT)?;
    if let Some(ct) = head.content_type {
        push(dst, &mut cursor, H_CT)?;
        push(dst, &mut cursor, ct)?;
        push(dst, &mut cursor, CRLF)?;
    }
    if head.sends_length(body_len) {
        push(dst, &mut cursor, H_CL)?;
        // u64 → ASCII into a stack scratch; 20 digits max.
        let mut len_buf = [0u8; 20];
        let digits = fmt_u64_ascii(body_len as u64, &mut len_buf);
        push(dst, &mut cursor, digits)?;
        push(dst, &mut cursor, CRLF)?;
    }
    let mut i = 0usize;
    while i < head.extra.len() {
        push(dst, &mut cursor, head.extra[i].0)?;
        push(dst, &mut cursor, H_SEP)?;
        push(dst, &mut cursor, head.extra[i].1)?;
        push(dst, &mut cursor, CRLF)?;
        i += 1;
    }
    push(dst, &mut cursor, connection_line(head.keep_alive))?;
    Ok(cursor)
}

/// Render a whole request — head, then `body` — into `dst`. Returns
/// bytes written. Zero-alloc.
pub fn write_request(dst: &mut [u8], head: &ReqHead<'_>, body: &[u8]) -> Result<usize, HttpErr> {
    let mut cursor = write_request_head(dst, head, body.len())?;
    push(dst, &mut cursor, body)?;
    Ok(cursor)
}

/// Decimal digits of `v` (≥ 1).
#[inline]
const fn dec_digits(mut v: u64) -> usize {
    let mut d = 1;
    while v >= 10 {
        v /= 10;
        d += 1;
    }
    d
}

/// Write a `GET {path} HTTP/1.1\r\n…` request into `dst`. Zero-alloc.
///
/// Emits a fixed header set:
///
/// ```text
/// GET {path} HTTP/1.1
/// Host: {host}
/// User-Agent: {user_agent}
/// Accept: */*
/// Accept-Encoding: identity
/// Connection: close
/// ```
///
/// Returns the number of bytes written. Fails with
/// [`HttpErr::BufferTooSmall`] if `dst` can't fit the request — never
/// allocates.
#[inline]
pub fn write_get_request(
    dst: &mut [u8],
    host: &[u8],
    path: &[u8],
    user_agent: &[u8],
) -> Result<usize, HttpErr> {
    let head = ReqHead {
        method: Method::Get,
        host,
        target: path,
        user_agent,
        content_type: None,
        extra: &[],
        keep_alive: false,
    };
    write_request(dst, &head, &[])
}

/// Write a `POST {path} HTTP/1.1\r\n…` request into `dst`, including the
/// body. Zero-alloc.
///
/// Emits a fixed header set:
///
/// ```text
/// POST {path} HTTP/1.1
/// Host: {host}
/// User-Agent: {user_agent}
/// Accept: */*
/// Accept-Encoding: identity
/// Content-Type: {content_type}
/// Content-Length: {body.len()}
/// Connection: close
/// ```
///
/// followed by the body bytes. Needed for venue REST endpoints that are
/// POST-only (Hyperliquid `/info` — plan §8.1). Returns the total number
/// of bytes written (headers + body). Fails with
/// [`HttpErr::BufferTooSmall`] if `dst` can't fit the request — never
/// allocates.
#[inline]
pub fn write_post_request(
    dst: &mut [u8],
    host: &[u8],
    path: &[u8],
    user_agent: &[u8],
    content_type: &[u8],
    body: &[u8],
) -> Result<usize, HttpErr> {
    let head = ReqHead {
        method: Method::Post,
        host,
        target: path,
        user_agent,
        content_type: Some(content_type),
        extra: &[],
        keep_alive: false,
    };
    write_request(dst, &head, body)
}

/// Render `v` as decimal ASCII into the tail of `scratch`, returning the
/// written subslice. Zero-alloc; `scratch` must be ≥ 20 bytes (max u64).
#[inline]
fn fmt_u64_ascii(mut v: u64, scratch: &mut [u8; 20]) -> &[u8] {
    let mut i = scratch.len();
    loop {
        i -= 1;
        scratch[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &scratch[i..]
}

#[inline]
fn push(dst: &mut [u8], cursor: &mut usize, src: &[u8]) -> Result<(), HttpErr> {
    let end = cursor
        .checked_add(src.len())
        .ok_or(HttpErr::BufferTooSmall)?;
    if end > dst.len() {
        return Err(HttpErr::BufferTooSmall);
    }
    dst[*cursor..end].copy_from_slice(src);
    *cursor = end;
    Ok(())
}

// ---------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------

/// Parse an HTTP/1.1 response from `buf`. Zero-alloc — returns offsets
/// into `buf` rather than owned slices.
///
/// Returns [`HttpResult::Incomplete`] as long as the header region
/// (ending in `\r\n\r\n`) has not yet been fully received. Once headers
/// are in, returns [`HttpResult::Complete`] with the body region sized
/// according to the declared framing. For [`BodyFraming::ContentLength`]
/// callers check that `body_end <= buf.len()` to know whether the body
/// is fully buffered; if it isn't they read more and call again.
pub fn read_response(buf: &[u8]) -> HttpResult {
    // Find end of headers.
    let header_end = match find_header_end(buf) {
        Some(n) => n,
        None => return HttpResult::Incomplete,
    };

    // Parse status line.
    let status_end = match memchr::memmem::find(&buf[..header_end], b"\r\n") {
        Some(n) => n,
        None => return HttpResult::Malformed,
    };
    let status_line = &buf[..status_end];
    let status = match parse_status_line(status_line) {
        Some(s) => s,
        None => return HttpResult::Malformed,
    };

    // Treat 3xx as malformed (redirect chasing is deferred).
    if (300..400).contains(&status) {
        return HttpResult::Malformed;
    }

    let headers = &buf[status_end + 2..header_end - 2];

    // Content-Length takes precedence; else Transfer-Encoding: chunked;
    // else close-delimited.
    let framing = if let Some(len) = find_content_length(headers) {
        BodyFraming::ContentLength(len)
    } else if find_chunked(headers) {
        BodyFraming::Chunked
    } else {
        BodyFraming::CloseDelimited
    };

    let body_start = header_end;
    let body_end = match framing {
        BodyFraming::ContentLength(len) => body_start.saturating_add(len as usize),
        BodyFraming::Chunked | BodyFraming::CloseDelimited => buf.len(),
    };

    HttpResult::Complete {
        status,
        header_end,
        body_start,
        body_end,
        framing,
    }
}

/// Whether a response head (`buf[..header_end]` of a
/// [`HttpResult::Complete`]) ends its connection: a `Connection: close`
/// token, or an HTTP/1.0 status line without `Connection: keep-alive`.
/// A keep-alive client retires the connection after such an answer
/// instead of writing its next request into a socket the server is
/// closing (HYPARB H9).
#[must_use]
pub fn head_says_close(head: &[u8]) -> bool {
    let Some(status_end) = memchr::memmem::find(head, b"\r\n") else {
        return true;
    };
    let headers = if head.len() >= status_end + 4 {
        &head[status_end + 2..head.len() - 2]
    } else {
        &head[..0]
    };
    let http10 = head.starts_with(b"HTTP/1.0 ");
    match find_header_value(headers, b"connection") {
        Some(v) => {
            if has_token(v, b"close") {
                true
            } else {
                http10 && !has_token(v, b"keep-alive")
            }
        }
        None => http10,
    }
}

/// A case-insensitive token in a comma-separated header value.
fn has_token(value: &[u8], token: &[u8]) -> bool {
    let bytes = trim_ascii(value);
    let mut start = 0usize;
    let mut i = 0usize;
    while i <= bytes.len() {
        if i == bytes.len() || bytes[i] == b',' {
            if eq_ignore_ascii_case(trim_ascii(&bytes[start..i]), token) {
                return true;
            }
            start = i + 1;
        }
        i += 1;
    }
    false
}

/// Find the end of the header region. Returns the offset *after* the
/// trailing `\r\n\r\n`.
#[inline]
fn find_header_end(buf: &[u8]) -> Option<usize> {
    memchr::memmem::find(buf, b"\r\n\r\n").map(|n| n + 4)
}

/// Parse an HTTP status line shaped `HTTP/1.1 200 OK`. Returns the code.
#[inline]
fn parse_status_line(line: &[u8]) -> Option<u16> {
    // Expect a prefix of either "HTTP/1.0 " or "HTTP/1.1 " — 9 bytes.
    if line.len() < 12 {
        return None;
    }
    if !(line.starts_with(b"HTTP/1.1 ") || line.starts_with(b"HTTP/1.0 ")) {
        return None;
    }
    let code_bytes = &line[9..12];
    let mut code: u16 = 0;
    let mut i = 0;
    while i < 3 {
        let b = code_bytes[i];
        if !b.is_ascii_digit() {
            return None;
        }
        code = code * 10 + (b - b'0') as u16;
        i += 1;
    }
    Some(code)
}

/// Search for a `Content-Length: N` header and return `N`. Case-
/// insensitive on the header name. Returns `None` if absent or
/// unparseable.
fn find_content_length(headers: &[u8]) -> Option<u64> {
    find_header_value(headers, b"content-length").and_then(parse_u64_trimmed)
}

/// Search for `Transfer-Encoding: chunked` (case-insensitive name,
/// case-insensitive `chunked`). Returns `true` on match.
fn find_chunked(headers: &[u8]) -> bool {
    match find_header_value(headers, b"transfer-encoding") {
        Some(v) => has_token(v, b"chunked"),
        None => false,
    }
}

/// Case-insensitive header lookup. `headers` is the slice between the
/// status line and the blank line (no leading/trailing `\r\n`).
fn find_header_value<'a>(headers: &'a [u8], name_lower: &[u8]) -> Option<&'a [u8]> {
    let mut cursor = 0usize;
    while cursor < headers.len() {
        let eol = memchr::memmem::find(&headers[cursor..], b"\r\n")
            .map(|n| cursor + n)
            .unwrap_or(headers.len());
        let line = &headers[cursor..eol];
        let colon = memchr::memchr(b':', line)?;
        let (name, value) = line.split_at(colon);
        if eq_ignore_ascii_case(name, name_lower) {
            // `value` starts with ':'
            return Some(trim_ascii(&value[1..]));
        }
        cursor = eol.saturating_add(2);
    }
    None
}

#[inline]
fn eq_ignore_ascii_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if !a[i].eq_ignore_ascii_case(&b[i]) {
            return false;
        }
        i += 1;
    }
    true
}

#[inline]
fn trim_ascii(buf: &[u8]) -> &[u8] {
    let mut start = 0usize;
    while start < buf.len() && (buf[start] == b' ' || buf[start] == b'\t') {
        start += 1;
    }
    let mut end = buf.len();
    while end > start && (buf[end - 1] == b' ' || buf[end - 1] == b'\t') {
        end -= 1;
    }
    &buf[start..end]
}

#[inline]
fn parse_u64_trimmed(buf: &[u8]) -> Option<u64> {
    let t = trim_ascii(buf);
    if t.is_empty() {
        return None;
    }
    let mut out: u64 = 0;
    let mut i = 0;
    while i < t.len() {
        let b = t[i];
        if !b.is_ascii_digit() {
            return None;
        }
        out = out.checked_mul(10)?.checked_add((b - b'0') as u64)?;
        i += 1;
    }
    Some(out)
}

// ---------------------------------------------------------------
// Chunked transfer-encoding dechunker
// ---------------------------------------------------------------

/// Outcome of [`dechunk_in_place`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DechunkResult {
    /// All chunks parsed; `length` is the new logical length of the
    /// buffer (dechunked bytes occupy `[0..length]`).
    Complete {
        /// Bytes of decoded payload in the output buffer.
        length: usize,
    },
    /// Not enough bytes yet — caller reads more and retries with the
    /// extended buffer.
    Incomplete,
    /// Malformed chunk framing.
    Malformed,
}

/// In-place decode of `Transfer-Encoding: chunked`. Writes the decoded
/// body over the top of `buf` (so `buf[..length]` is the payload). Zero
/// alloc.
///
/// **`buf` is untouched unless the result is `Complete`** (HYPARB H8):
/// the framing is walked once read-only and decoded only when it is
/// whole, so a caller that is still filling its buffer can call this on
/// every read and retry on `Incomplete` — the first cut shifted chunks
/// left before discovering the tail was missing, which is harmless to a
/// read-to-EOF caller and corrupting to an incremental one.
pub fn dechunk_in_place(buf: &mut [u8]) -> DechunkResult {
    match walk_chunks(buf, false, 0).res {
        DechunkResult::Complete { .. } => walk_chunks(buf, true, 0).res,
        other => other,
    }
}

/// Where [`chunked_body`] left a chunked body's payload.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChunkedBody {
    /// The decoded payload is `buf[start..start + len]`.
    Span {
        /// Offset of the payload's first byte within `buf`.
        start: usize,
        /// Payload length.
        len: usize,
    },
    /// Not enough bytes yet; `buf` is untouched — read more and retry.
    Incomplete,
    /// Malformed chunk framing.
    Malformed,
}

/// [`dechunk_in_place`] without the moves that are not needed (HYPARB
/// H9, the zero-copy review): the payload is assembled where its FIRST
/// data chunk already lies — a one-chunk body (what a JSON-RPC endpoint
/// sends for a small answer) moves no byte at all, and a multi-chunk
/// body moves only chunks 2.. left over the framing, onto the end of
/// chunk 1. `buf` is untouched unless the result is a `Span`, so an
/// incremental reader retries on `Incomplete`.
pub fn chunked_body(buf: &mut [u8]) -> ChunkedBody {
    let w = walk_chunks(buf, false, 0);
    match w.res {
        DechunkResult::Complete { length } if w.chunks <= 1 => ChunkedBody::Span {
            start: w.first,
            len: length,
        },
        DechunkResult::Complete { .. } => match walk_chunks(buf, true, w.first).res {
            DechunkResult::Complete { length } => ChunkedBody::Span {
                start: w.first,
                len: length,
            },
            DechunkResult::Incomplete => ChunkedBody::Incomplete,
            DechunkResult::Malformed => ChunkedBody::Malformed,
        },
        DechunkResult::Incomplete => ChunkedBody::Incomplete,
        DechunkResult::Malformed => ChunkedBody::Malformed,
    }
}

/// One pass of [`walk_chunks`]: the verdict, the first data chunk's
/// offset and the number of non-empty data chunks.
struct Walk {
    res: DechunkResult,
    first: usize,
    chunks: u32,
}

#[inline(always)]
const fn done(res: DechunkResult, first: usize, chunks: u32) -> Walk {
    Walk { res, first, chunks }
}

/// The chunk walker behind [`dechunk_in_place`] and [`chunked_body`]:
/// validates the framing and, with `copy`, shifts each chunk body left
/// over the framing, assembling the payload from `write0` (at most the
/// first chunk's own offset). `length` is the payload's length.
fn walk_chunks(buf: &mut [u8], copy: bool, write0: usize) -> Walk {
    let mut read: usize = 0;
    let mut write: usize = write0;
    let mut first: usize = 0;
    let mut chunks: u32 = 0;
    loop {
        // Find \r\n after the chunk-size hex digits.
        let remain = &buf[read..];
        let crlf = match memchr::memmem::find(remain, b"\r\n") {
            Some(n) => n,
            None => return done(DechunkResult::Incomplete, first, chunks),
        };
        // Parse size as hex (allow chunk extensions after ';').
        let size_bytes = match memchr::memchr(b';', &remain[..crlf]) {
            Some(n) => &remain[..n],
            None => &remain[..crlf],
        };
        let size = match parse_hex_u64(size_bytes) {
            Some(n) => n as usize,
            None => return done(DechunkResult::Malformed, first, chunks),
        };
        let chunk_data = read + crlf + 2;
        if size == 0 {
            // Terminator: expect one more \r\n.
            let need = chunk_data + 2;
            if buf.len() < need {
                return done(DechunkResult::Incomplete, first, chunks);
            }
            if &buf[chunk_data..chunk_data + 2] != b"\r\n" {
                return done(DechunkResult::Malformed, first, chunks);
            }
            return done(
                DechunkResult::Complete {
                    length: write - write0,
                },
                first,
                chunks,
            );
        }
        // A size no buffer can hold is MALFORMED, never wrapped (fuzz,
        // HYPARB H9: `fffffffffffffffe` wrapped `chunk_end` below
        // `chunk_data`, the framing check then read the size line's own
        // CRLF and passed, and the copy pass panicked on an inverted
        // range — a process abort in release, from any chunked answer).
        let Some(chunk_end) = chunk_data.checked_add(size) else {
            return done(DechunkResult::Malformed, first, chunks);
        };
        // +2 for the trailing CRLF after the chunk body.
        let Some(chunk_tail) = chunk_end.checked_add(2) else {
            return done(DechunkResult::Malformed, first, chunks);
        };
        if buf.len() < chunk_tail {
            return done(DechunkResult::Incomplete, first, chunks);
        }
        if &buf[chunk_end..chunk_end + 2] != b"\r\n" {
            return done(DechunkResult::Malformed, first, chunks);
        }
        if chunks == 0 {
            first = chunk_data;
        }
        chunks += 1;
        // COPY: each chunk body after the payload's start (≤ the caller's
        // response buffer), shifted left over its framing ONCE — the
        // payload must be contiguous for the byte scanners — rejected: a
        // segmented scan over chunk spans (every scanner would need a
        // split-token path). `chunked_body` never moves chunk 1.
        if copy && chunk_data != write {
            buf.copy_within(chunk_data..chunk_end, write);
        }
        write += size;
        read = chunk_end + 2;
    }
}

#[inline]
fn parse_hex_u64(buf: &[u8]) -> Option<u64> {
    if buf.is_empty() {
        return None;
    }
    let mut out: u64 = 0;
    let mut i = 0;
    while i < buf.len() {
        let c = buf[i];
        let d = if c.is_ascii_digit() {
            (c - b'0') as u64
        } else if (b'a'..=b'f').contains(&c) {
            10 + (c - b'a') as u64
        } else if (b'A'..=b'F').contains(&c) {
            10 + (c - b'A') as u64
        } else {
            return None;
        };
        out = out.checked_shl(4)?.checked_add(d)?;
        i += 1;
    }
    Some(out)
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_get_request_emits_expected_bytes() {
        let mut buf = [0u8; 512];
        let n = write_get_request(&mut buf, b"example.com", b"/feed", b"pm/0.1").unwrap();
        let got = &buf[..n];
        let expected = b"GET /feed HTTP/1.1\r\nHost: example.com\r\nUser-Agent: pm/0.1\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n";
        assert_eq!(got, expected);
    }

    #[test]
    fn write_get_request_rejects_tiny_buffer() {
        let mut buf = [0u8; 8];
        assert!(matches!(
            write_get_request(&mut buf, b"h", b"/", b"ua"),
            Err(HttpErr::BufferTooSmall)
        ));
    }

    #[test]
    fn write_post_request_emits_expected_bytes() {
        let mut buf = [0u8; 512];
        let n = write_post_request(
            &mut buf,
            b"api.hyperliquid.xyz",
            b"/info",
            b"pm/0.1",
            b"application/json",
            b"{\"type\":\"meta\"}",
        )
        .unwrap();
        let got = &buf[..n];
        let expected: &[u8] = b"POST /info HTTP/1.1\r\nHost: api.hyperliquid.xyz\r\nUser-Agent: pm/0.1\r\nAccept: */*\r\nAccept-Encoding: identity\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"type\":\"meta\"}";
        assert_eq!(got, expected);
    }

    #[test]
    fn write_post_request_empty_body_has_zero_content_length() {
        let mut buf = [0u8; 256];
        let n = write_post_request(&mut buf, b"h", b"/", b"ua", b"text/plain", b"").unwrap();
        let got = &buf[..n];
        assert!(
            memchr::memmem::find(got, b"Content-Length: 0\r\n").is_some(),
            "missing zero content-length: {}",
            String::from_utf8_lossy(got)
        );
        assert!(got.ends_with(b"\r\n\r\n"));
    }

    #[test]
    fn write_post_request_rejects_tiny_buffer() {
        let mut buf = [0u8; 16];
        assert!(matches!(
            write_post_request(&mut buf, b"h", b"/", b"ua", b"a/b", b"xyz"),
            Err(HttpErr::BufferTooSmall)
        ));
    }

    #[test]
    fn fmt_u64_ascii_renders_digits() {
        let mut s = [0u8; 20];
        assert_eq!(fmt_u64_ascii(0, &mut s), b"0");
        let mut s = [0u8; 20];
        assert_eq!(fmt_u64_ascii(15, &mut s), b"15");
        let mut s = [0u8; 20];
        assert_eq!(fmt_u64_ascii(u64::MAX, &mut s), b"18446744073709551615");
    }

    #[test]
    fn read_response_incomplete_before_headers_terminate() {
        let r = read_response(b"HTTP/1.1 200 OK\r\nHost: x\r\n");
        assert_eq!(r, HttpResult::Incomplete);
    }

    #[test]
    fn read_response_content_length_complete() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        match read_response(raw) {
            HttpResult::Complete {
                status,
                header_end,
                body_start,
                body_end,
                framing,
            } => {
                assert_eq!(status, 200);
                assert_eq!(header_end, raw.len() - 5);
                assert_eq!(body_start, header_end);
                assert_eq!(body_end, raw.len());
                assert_eq!(framing, BodyFraming::ContentLength(5));
                assert_eq!(&raw[body_start..body_end], b"hello");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn read_response_chunked_flag_detected() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        match read_response(raw) {
            HttpResult::Complete {
                status, framing, ..
            } => {
                assert_eq!(status, 200);
                assert_eq!(framing, BodyFraming::Chunked);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn read_response_close_delimited_default() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nsome-payload";
        match read_response(raw) {
            HttpResult::Complete { framing, .. } => {
                assert_eq!(framing, BodyFraming::CloseDelimited);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn read_response_rejects_malformed_status_line() {
        let r = read_response(b"HTTP/2.0 200 OK\r\n\r\n");
        assert_eq!(r, HttpResult::Malformed);
    }

    #[test]
    fn read_response_rejects_3xx_redirect() {
        let r = read_response(b"HTTP/1.1 301 Moved\r\nLocation: /x\r\n\r\n");
        assert_eq!(r, HttpResult::Malformed);
    }

    #[test]
    fn dechunk_in_place_leaves_an_incomplete_buffer_untouched() {
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n";
        let mut buf = raw.to_vec();
        assert_eq!(dechunk_in_place(&mut buf), DechunkResult::Incomplete);
        assert_eq!(&buf[..], &raw[..], "no chunk was shifted");
        buf.extend_from_slice(b"\r\n");
        assert_eq!(
            dechunk_in_place(&mut buf),
            DechunkResult::Complete { length: 11 }
        );
        assert_eq!(&buf[..11], b"hello world");
    }

    #[test]
    fn dechunk_in_place_single_chunk() {
        let mut buf: [u8; 32] = [0u8; 32];
        let raw = b"5\r\nhello\r\n0\r\n\r\n";
        buf[..raw.len()].copy_from_slice(raw);
        match dechunk_in_place(&mut buf[..raw.len()]) {
            DechunkResult::Complete { length } => {
                assert_eq!(length, 5);
                assert_eq!(&buf[..length], b"hello");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn dechunk_in_place_multiple_chunks() {
        let mut buf: [u8; 64] = [0u8; 64];
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        buf[..raw.len()].copy_from_slice(raw);
        match dechunk_in_place(&mut buf[..raw.len()]) {
            DechunkResult::Complete { length } => {
                assert_eq!(length, 11);
                assert_eq!(&buf[..length], b"hello world");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn dechunk_in_place_incomplete_returns_incomplete() {
        let mut buf: [u8; 16] = [0u8; 16];
        let raw = b"5\r\nhe";
        buf[..raw.len()].copy_from_slice(raw);
        assert_eq!(
            dechunk_in_place(&mut buf[..raw.len()]),
            DechunkResult::Incomplete
        );
    }

    #[test]
    fn dechunk_in_place_bad_hex_is_malformed() {
        let mut buf: [u8; 32] = [0u8; 32];
        let raw = b"zz\r\nhello\r\n0\r\n\r\n";
        buf[..raw.len()].copy_from_slice(raw);
        assert_eq!(
            dechunk_in_place(&mut buf[..raw.len()]),
            DechunkResult::Malformed
        );
    }

    #[test]
    fn chunked_body_returns_a_single_chunk_where_it_lies() {
        let raw = b"5\r\nhello\r\n0\r\n\r\n";
        let mut buf = raw.to_vec();
        assert_eq!(
            chunked_body(&mut buf),
            ChunkedBody::Span { start: 3, len: 5 }
        );
        assert_eq!(&buf[..], &raw[..], "no byte moved");
        assert_eq!(&buf[3..8], b"hello");
    }

    #[test]
    fn chunked_body_moves_only_the_chunks_after_the_first() {
        let mut buf = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".to_vec();
        assert_eq!(
            chunked_body(&mut buf),
            ChunkedBody::Span { start: 3, len: 11 }
        );
        assert_eq!(
            &buf[3..14],
            b"hello world",
            "chunk 1 stayed; chunk 2 joined it"
        );
    }

    /// Fuzz, HYPARB H9: a chunk size near `usize::MAX` used to wrap the
    /// chunk's end below its start and abort the process in the copy
    /// pass. Now it is malformed, whatever the bytes after it.
    #[test]
    fn a_chunk_size_that_wraps_is_malformed_not_an_abort() {
        let mut i = 0;
        while i < 3 {
            let size = ["fffffffffffffffe", "ffffffffffffffff", "fffffffffffffff0"][i];
            let raw = format!("{size}\r\nabc\r\n0\r\n\r\n");
            let mut a = raw.as_bytes().to_vec();
            assert_eq!(dechunk_in_place(&mut a), DechunkResult::Malformed, "{size}");
            let mut b = raw.as_bytes().to_vec();
            assert_eq!(chunked_body(&mut b), ChunkedBody::Malformed, "{size}");
            assert_eq!(&b[..], raw.as_bytes(), "untouched");
            // …and as a SECOND chunk, after a good one.
            let raw2 = format!("3\r\nabc\r\n{size}\r\nxyz\r\n0\r\n\r\n");
            let mut c = raw2.as_bytes().to_vec();
            assert_eq!(chunked_body(&mut c), ChunkedBody::Malformed, "{size}");
            i += 1;
        }
        // A size that fits usize but not the buffer is only incomplete.
        let mut d = b"ffffff\r\nabc".to_vec();
        assert_eq!(dechunk_in_place(&mut d), DechunkResult::Incomplete);
    }

    #[test]
    fn chunked_body_failure_modes_leave_the_buffer_untouched() {
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n";
        let mut buf = raw.to_vec();
        assert_eq!(chunked_body(&mut buf), ChunkedBody::Incomplete);
        assert_eq!(&buf[..], &raw[..]);
        let mut bad = b"zz\r\nhello\r\n0\r\n\r\n".to_vec();
        assert_eq!(chunked_body(&mut bad), ChunkedBody::Malformed);
        let mut empty = b"0\r\n\r\n".to_vec();
        assert_eq!(
            chunked_body(&mut empty),
            ChunkedBody::Span { start: 0, len: 0 }
        );
    }

    #[test]
    fn head_says_close_reads_the_connection_token() {
        let head = |s: &str| s.as_bytes().to_vec();
        assert!(!head_says_close(&head(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n"
        )));
        assert!(!head_says_close(&head(
            "HTTP/1.1 200 OK\r\nConnection: keep-alive\r\n\r\n"
        )));
        assert!(head_says_close(&head(
            "HTTP/1.1 200 OK\r\nconnection: Close\r\n\r\n"
        )));
        assert!(head_says_close(&head(
            "HTTP/1.1 200 OK\r\nConnection: upgrade, close\r\n\r\n"
        )));
        assert!(
            head_says_close(&head("HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n")),
            "1.0 closes by default"
        );
        assert!(!head_says_close(&head(
            "HTTP/1.0 200 OK\r\nConnection: keep-alive\r\n\r\n"
        )));
        assert!(
            head_says_close(b"garbage"),
            "an unreadable head is not reused"
        );
    }

    #[test]
    fn find_chunked_detects_token_in_list() {
        // Edge case: `Transfer-Encoding: gzip, chunked` — chunked must
        // be recognised as a token.
        let headers = b"Transfer-Encoding: gzip, chunked";
        assert!(find_chunked(headers));
    }

    #[test]
    fn find_content_length_is_case_insensitive() {
        let headers = b"content-LENGTH: 42";
        assert_eq!(find_content_length(headers), Some(42));
    }
}

#[cfg(test)]
mod proptests {
    //! Property tests for the HTTP/1.1 codec.
    //!
    //! The fuzz harness (`fuzz/fuzz_targets/http1_response.rs`)
    //! catches panics + UB; these proptests cover the
    //! *structural* invariants the harness can't easily assert:
    //!
    //!   * `read_response` never indexes past the input length.
    //!   * On `Complete{...}`, the reported offsets are mutually
    //!     consistent (`header_end ≤ body_start ≤ body_end`).
    //!   * `dechunk_in_place` returns ≤ input length on success.
    //!   * `write_get_request` returns exactly the bytes the next
    //!     `read_response` would consume on a server side mirror.
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// `read_response` is panic-free on arbitrary input and
        /// only reports offsets that fit in the buffer.
        #[test]
        fn read_response_offsets_in_bounds(input in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let res = read_response(&input);
            if let HttpResult::Complete { header_end, body_start, body_end, .. } = res {
                prop_assert!(header_end <= input.len());
                prop_assert!(body_start <= input.len());
                prop_assert!(body_end <= input.len() || body_end >= body_start);
                prop_assert!(header_end <= body_start);
                prop_assert!(body_start <= body_end);
            }
        }

        /// `dechunk_in_place` either errors or returns a length
        /// not exceeding the input.
        #[test]
        fn dechunk_in_place_never_grows(input in proptest::collection::vec(any::<u8>(), 0..1024)) {
            let mut buf = input.clone();
            let len = buf.len();
            match dechunk_in_place(&mut buf) {
                DechunkResult::Complete { length } => prop_assert!(length <= len),
                DechunkResult::Incomplete | DechunkResult::Malformed => {}
            }
        }

        /// `chunked_body` names exactly the payload `dechunk_in_place`
        /// produces, for any split of any payload into chunks.
        #[test]
        fn chunked_body_agrees_with_dechunk_in_place(
            payload in proptest::collection::vec(any::<u8>(), 0..512),
            cuts in proptest::collection::vec(1usize..64, 0..8),
        ) {
            let mut wire = Vec::new();
            let mut at = 0usize;
            let mut k = 0usize;
            while at < payload.len() {
                let n = if k < cuts.len() { cuts[k].min(payload.len() - at) } else { payload.len() - at };
                wire.extend_from_slice(format!("{n:x}\r\n").as_bytes());
                wire.extend_from_slice(&payload[at..at + n]);
                wire.extend_from_slice(b"\r\n");
                at += n;
                k += 1;
            }
            wire.extend_from_slice(b"0\r\n\r\n");
            let mut a = wire.clone();
            let mut b = wire;
            let len = match dechunk_in_place(&mut a) {
                DechunkResult::Complete { length } => length,
                other => return Err(TestCaseError::fail(format!("{other:?}"))),
            };
            match chunked_body(&mut b) {
                ChunkedBody::Span { start, len: l } => {
                    prop_assert_eq!(l, len);
                    prop_assert_eq!(&b[start..start + l], &a[..len]);
                    prop_assert_eq!(&a[..len], &payload[..]);
                }
                other => return Err(TestCaseError::fail(format!("{other:?}"))),
            }
        }

        /// `write_get_request` is panic-free across host/path
        /// shapes and either succeeds writing ≤ buf.len() bytes
        /// or returns `BufferTooSmall`.
        #[test]
        fn write_get_request_bounded(
            host in "[a-z0-9.-]{1,64}",
            path in "/[a-zA-Z0-9/_-]{0,128}",
            ua in "[a-zA-Z0-9./_+-]{1,32}",
            buf_size in 64usize..1024,
        ) {
            let mut buf = vec![0u8; buf_size];
            let res = write_get_request(&mut buf, host.as_bytes(), path.as_bytes(), ua.as_bytes());
            match res {
                Ok(n) => prop_assert!(n <= buf.len()),
                Err(HttpErr::BufferTooSmall) => {}
                Err(HttpErr::BadHead) => prop_assert!(false, "well-formed fields are never BadHead"),
            }
        }

        /// `write_post_request` is panic-free, bounded, and on success
        /// the declared `Content-Length` matches the appended body,
        /// with the body occupying the exact tail of the request.
        #[test]
        fn write_post_request_bounded_and_consistent(
            host in "[a-z0-9.-]{1,64}",
            path in "/[a-zA-Z0-9/_-]{0,64}",
            ua in "[a-zA-Z0-9./_+-]{1,16}",
            body in proptest::collection::vec(any::<u8>(), 0..256),
            buf_size in 64usize..2048,
        ) {
            let mut buf = vec![0u8; buf_size];
            let res = write_post_request(
                &mut buf,
                host.as_bytes(),
                path.as_bytes(),
                ua.as_bytes(),
                b"application/json",
                &body,
            );
            match res {
                Ok(n) => {
                    prop_assert!(n <= buf.len());
                    prop_assert!(buf[..n].ends_with(&body));
                    // Headers region terminates with the blank line right
                    // before the body.
                    let header_len = n - body.len();
                    prop_assert!(buf[..header_len].ends_with(b"\r\n\r\n"));
                }
                Err(HttpErr::BufferTooSmall) => {}
                Err(HttpErr::BadHead) => prop_assert!(false, "well-formed fields are never BadHead"),
            }
        }

        /// HC2: for ANY field bytes the generic head writer never
        /// panics; the head it sizes is the head it renders; a head that
        /// renders ends in exactly one blank line and carries exactly
        /// `2 + extra` header-terminating CRLFs past the request line —
        /// no field can smuggle a line in; and a CR / LF / NUL anywhere
        /// is `BadHead`.
        #[test]
        fn write_request_head_is_sized_bounded_and_injection_proof(
            method in 0u8..4,
            target in proptest::collection::vec(any::<u8>(), 0..96),
            host in proptest::collection::vec(any::<u8>(), 0..48),
            names in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..16), 0..3),
            value in proptest::collection::vec(any::<u8>(), 0..48),
            body_len in 0usize..100_000,
            keep_alive in any::<bool>(),
            buf_size in 0usize..1024,
        ) {
            let method = [Method::Get, Method::Post, Method::Put, Method::Delete][method as usize];
            let extra: Vec<Header<'_>> = names.iter().map(|n| (&n[..], &value[..])).collect();
            let head = ReqHead {
                method,
                host: &host,
                target: &target,
                user_agent: b"t/1",
                content_type: Some(b"application/json"),
                extra: &extra,
                keep_alive,
            };
            let mut buf = vec![0u8; buf_size];
            let sized = request_head_len(&head, body_len);
            let res = write_request_head(&mut buf, &head, body_len);
            let dirty = |v: &[u8]| v.iter().any(|&b| b == b'\r' || b == b'\n' || b == 0);
            let injected = dirty(&target) || dirty(&host) || dirty(&value)
                || names.iter().any(|n| dirty(n));
            match (sized, res) {
                (Ok(len), Ok(n)) => {
                    prop_assert!(!injected);
                    prop_assert_eq!(len, n);
                    let h = &buf[..n];
                    prop_assert!(h.ends_with(b"\r\n\r\n"));
                    let crlfs = memchr::memmem::find_iter(h, b"\r\n").count();
                    // request line + Host + UA + Accept + A-E + C-T
                    // [+ C-L] + extras + Connection + the blank line.
                    let has_cl = !matches!(method, Method::Get) || body_len > 0;
                    prop_assert_eq!(crlfs, 7 + usize::from(has_cl) + extra.len() + 1);
                }
                (Ok(len), Err(HttpErr::BufferTooSmall)) => prop_assert!(len > buf.len()),
                (Err(HttpErr::BadHead), Err(HttpErr::BadHead)) => {}
                (a, b) => prop_assert!(false, "sized {a:?} vs rendered {b:?}"),
            }
            if injected {
                prop_assert_eq!(request_head_len(&head, body_len), Err(HttpErr::BadHead));
            }
        }
    }
}
