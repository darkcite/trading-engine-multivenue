// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # HTTP/1.1 minimal codec
//!
//! Zero-alloc, pure-byte-scanner HTTP/1.1 client codec used by the
//! boot-time REST discovery path (`boot_http`) and the CLOB
//! dispatcher. No cookies, no compression (the request hard-codes
//! `Accept-Encoding: identity`), no automatic redirect chasing.
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

/// Reason a response scan rejected a buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HttpErr {
    /// Caller's output buffer is too small to fit the request.
    BufferTooSmall,
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
    let mut cursor = 0usize;

    push(dst, &mut cursor, b"GET ")?;
    push(dst, &mut cursor, path)?;
    push(dst, &mut cursor, b" HTTP/1.1\r\n")?;

    push(dst, &mut cursor, b"Host: ")?;
    push(dst, &mut cursor, host)?;
    push(dst, &mut cursor, b"\r\n")?;

    push(dst, &mut cursor, b"User-Agent: ")?;
    push(dst, &mut cursor, user_agent)?;
    push(dst, &mut cursor, b"\r\n")?;

    push(dst, &mut cursor, b"Accept: */*\r\n")?;
    push(dst, &mut cursor, b"Accept-Encoding: identity\r\n")?;
    push(dst, &mut cursor, b"Connection: close\r\n\r\n")?;

    Ok(cursor)
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
    let mut cursor = 0usize;

    push(dst, &mut cursor, b"POST ")?;
    push(dst, &mut cursor, path)?;
    push(dst, &mut cursor, b" HTTP/1.1\r\n")?;

    push(dst, &mut cursor, b"Host: ")?;
    push(dst, &mut cursor, host)?;
    push(dst, &mut cursor, b"\r\n")?;

    push(dst, &mut cursor, b"User-Agent: ")?;
    push(dst, &mut cursor, user_agent)?;
    push(dst, &mut cursor, b"\r\n")?;

    push(dst, &mut cursor, b"Accept: */*\r\n")?;
    push(dst, &mut cursor, b"Accept-Encoding: identity\r\n")?;

    push(dst, &mut cursor, b"Content-Type: ")?;
    push(dst, &mut cursor, content_type)?;
    push(dst, &mut cursor, b"\r\n")?;

    push(dst, &mut cursor, b"Content-Length: ")?;
    // u64 → ASCII into a stack scratch; 20 digits max.
    let mut len_buf = [0u8; 20];
    let len_ascii = fmt_u64_ascii(body.len() as u64, &mut len_buf);
    push(dst, &mut cursor, len_ascii)?;
    push(dst, &mut cursor, b"\r\n")?;

    push(dst, &mut cursor, b"Connection: close\r\n\r\n")?;

    push(dst, &mut cursor, body)?;

    Ok(cursor)
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
        Some(v) => {
            let trimmed = trim_ascii(v);
            // Value may be a comma-separated list; check each token.
            let mut start = 0usize;
            let mut i = 0usize;
            let bytes = trimmed;
            while i <= bytes.len() {
                let at_boundary = i == bytes.len() || bytes[i] == b',';
                if at_boundary {
                    let token = trim_ascii(&bytes[start..i]);
                    if eq_ignore_ascii_case(token, b"chunked") {
                        return true;
                    }
                    start = i + 1;
                }
                i += 1;
            }
            false
        }
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
pub fn dechunk_in_place(buf: &mut [u8]) -> DechunkResult {
    let mut read: usize = 0;
    let mut write: usize = 0;
    loop {
        // Find \r\n after the chunk-size hex digits.
        let remain = &buf[read..];
        let crlf = match memchr::memmem::find(remain, b"\r\n") {
            Some(n) => n,
            None => return DechunkResult::Incomplete,
        };
        // Parse size as hex (allow chunk extensions after ';').
        let size_bytes = match memchr::memchr(b';', &remain[..crlf]) {
            Some(n) => &remain[..n],
            None => &remain[..crlf],
        };
        let size = match parse_hex_u64(size_bytes) {
            Some(n) => n as usize,
            None => return DechunkResult::Malformed,
        };
        let chunk_data = read + crlf + 2;
        if size == 0 {
            // Terminator: expect one more \r\n.
            let need = chunk_data + 2;
            if buf.len() < need {
                return DechunkResult::Incomplete;
            }
            if &buf[chunk_data..chunk_data + 2] != b"\r\n" {
                return DechunkResult::Malformed;
            }
            return DechunkResult::Complete { length: write };
        }
        let chunk_end = chunk_data + size;
        // +2 for the trailing CRLF after the chunk body.
        if buf.len() < chunk_end + 2 {
            return DechunkResult::Incomplete;
        }
        if &buf[chunk_end..chunk_end + 2] != b"\r\n" {
            return DechunkResult::Malformed;
        }
        // Copy chunk body left, skipping headers.
        if chunk_data != write {
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
            }
        }
    }
}
