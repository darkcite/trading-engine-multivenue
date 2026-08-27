// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # WebSocket opening handshake (RFC 6455 §4)
//!
//! Zero-alloc, pure `&[u8]` / `&mut [u8]` client-side handshake: we
//! write a `GET / HTTP/1.1` request with the mandatory headers into a
//! caller-owned buffer and parse the server's `101 Switching Protocols`
//! response out of a caller-owned buffer.
//!
//! Everything that could require a heap works against stack memory:
//! SHA-1 is inlined here (RFC-6455-only primitive), base64 comes from
//! `core-crypto` (shared with OKX login signing since Phase 8a).
//!
//! ## Scope
//!
//! This module owns **only** the opening handshake. Once the server
//! returns `101 Switching Protocols` the caller switches to
//! [`crate::ws_frame`] for frame-level IO.
//!
//! ## Security posture
//!
//! `Sec-WebSocket-Key` is a **per-connection ephemeral** nonce — its
//! sole purpose is to protect against caching proxies replaying a
//! non-WebSocket response at a WebSocket client. SHA-1 is fully
//! broken as a cryptographic primitive but is specified by RFC 6455
//! for this exact use; we implement it verbatim.
//!
//! We use a constant-time compare for the accept check so a malicious
//! server cannot time-oracle the key out of us, even though the key
//! has no secrecy value.

use core_crypto::base64_encode as b64_encode;

// ---------------------------------------------------------------
// Errors
// ---------------------------------------------------------------

/// Serializer error. Non-allocating: single-variant enum.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HandshakeErr {
    /// Destination slice cannot fit the request.
    BufferTooSmall,
}

/// Parser result for [`read_server_handshake`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HandshakeResult {
    /// Not enough bytes to decide yet. Caller keeps reading.
    Incomplete,
    /// Server returned `101 Switching Protocols` with an
    /// `Upgrade: websocket` header. `accept_start..accept_end`
    /// spans the raw `Sec-WebSocket-Accept` header value (28 bytes
    /// of base64). `header_end` is the byte offset one past the
    /// terminating `\r\n\r\n`.
    Upgraded {
        /// Inclusive byte offset into the input where the accept
        /// token starts.
        accept_start: usize,
        /// Exclusive byte offset one past the accept token.
        accept_end: usize,
        /// Exclusive byte offset one past the empty line terminator.
        header_end: usize,
    },
    /// Bytes violate HTTP/1.1 framing or the status line is not 101.
    /// Caller should drop the connection.
    Malformed,
}

// ---------------------------------------------------------------
// RFC 6455 GUID + request template
// ---------------------------------------------------------------

/// The GUID specified by RFC 6455 §1.3. Appended to the client key
/// before SHA-1 to form the server `Accept` value.
const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// ---------------------------------------------------------------
// SHA-1 (RFC 3174)
// ---------------------------------------------------------------

/// SHA-1 state. Private; we only expose the one-shot convenience fn.
#[derive(Copy, Clone)]
struct Sha1 {
    h: [u32; 5],
    /// 64-byte message block buffer.
    buf: [u8; 64],
    /// Bytes currently buffered (0..=63).
    buf_len: usize,
    /// Total message length in bits.
    total_bits: u64,
}

impl Sha1 {
    #[inline]
    const fn new() -> Self {
        Self {
            h: [
                0x6745_2301,
                0xEFCD_AB89,
                0x98BA_DCFE,
                0x1032_5476,
                0xC3D2_E1F0,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total_bits: 0,
        }
    }

    #[inline]
    fn update(&mut self, mut data: &[u8]) {
        self.total_bits = self.total_bits.wrapping_add((data.len() as u64) * 8);

        // First, fill the partial block if any bytes are buffered.
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                Self::compress(&mut self.h, &block);
                self.buf_len = 0;
            }
        }

        // Process whole blocks straight from `data`.
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            Self::compress(&mut self.h, &block);
            data = &data[64..];
        }

        // Stash the tail.
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    #[inline]
    fn finalize(mut self) -> [u8; 20] {
        // Pad: 0x80, then zeros, then 64-bit big-endian length.
        let total_bits = self.total_bits;
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;
        if self.buf_len > 56 {
            // Not enough room for the length — flush and zero a new block.
            let rem = 64 - self.buf_len;
            let mut i = 0;
            while i < rem {
                self.buf[self.buf_len + i] = 0;
                i += 1;
            }
            let block = self.buf;
            Self::compress(&mut self.h, &block);
            self.buf = [0u8; 64];
            self.buf_len = 0;
        }
        // Zero-fill up to byte 56.
        let mut i = self.buf_len;
        while i < 56 {
            self.buf[i] = 0;
            i += 1;
        }
        // 64-bit big-endian length.
        self.buf[56..64].copy_from_slice(&total_bits.to_be_bytes());
        let block = self.buf;
        Self::compress(&mut self.h, &block);

        let mut out = [0u8; 20];
        let mut j = 0;
        while j < 5 {
            out[j * 4..j * 4 + 4].copy_from_slice(&self.h[j].to_be_bytes());
            j += 1;
        }
        out
    }

    #[inline]
    fn compress(h: &mut [u32; 5], block: &[u8; 64]) {
        let mut w = [0u32; 80];
        // Expand the 16-word block into 80 32-bit words.
        let mut i = 0;
        while i < 16 {
            let off = i * 4;
            w[i] = u32::from_be_bytes([
                block[off],
                block[off + 1],
                block[off + 2],
                block[off + 3],
            ]);
            i += 1;
        }
        while i < 80 {
            let v = w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16];
            w[i] = v.rotate_left(1);
            i += 1;
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];

        i = 0;
        while i < 80 {
            let (f, k) = if i < 20 {
                ((b & c) | ((!b) & d), 0x5A82_7999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9_EBA1u32)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1B_BCDCu32)
            } else {
                (b ^ c ^ d, 0xCA62_C1D6u32)
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
            i += 1;
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
}

/// One-shot SHA-1 of two concatenated slices. Stack-only.
#[inline]
fn sha1_concat(a: &[u8], b: &[u8]) -> [u8; 20] {
    let mut s = Sha1::new();
    s.update(a);
    s.update(b);
    s.finalize()
}

// Base64 (RFC 4648, standard alphabet) is provided by `core-crypto`
// — imported as `b64_encode` at the top of this module. Extracted in
// Phase 8a so OKX login signing and the WS handshake share one
// implementation.

// ---------------------------------------------------------------
// Key generation + accept value
// ---------------------------------------------------------------

/// Derive a 16-byte pseudo-random blob from a 64-bit seed and
/// base64-encode it to a 24-byte `Sec-WebSocket-Key`.
///
/// This is **not** a CSPRNG; the RFC only requires that the key be
/// "randomly selected" per connection, and the key has no secrecy
/// value — it exists only so a caching proxy can't replay a
/// previously-cached non-WebSocket response. Using splitmix64 gives
/// us a deterministic-per-seed output that's trivial to test.
#[inline]
pub fn sec_websocket_key_from_seed(seed: u64) -> [u8; 24] {
    // SplitMix64: two 64-bit outputs give us 16 bytes of entropy.
    let mut state = seed;
    let mut raw = [0u8; 16];

    let x0 = splitmix64(&mut state);
    let x1 = splitmix64(&mut state);
    raw[..8].copy_from_slice(&x0.to_le_bytes());
    raw[8..].copy_from_slice(&x1.to_le_bytes());

    let mut out = [0u8; 24];
    let n = b64_encode(&raw, &mut out);
    debug_assert_eq!(n, 24);
    out
}

#[inline]
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Compute the `Sec-WebSocket-Accept` value a server is required to
/// return for a given client key: `base64(sha1(key ++ GUID))`.
/// 20-byte SHA-1 output → 28-byte base64 (4 * ceil(20/3) = 28).
#[inline]
pub fn expected_accept(sec_key: &[u8; 24]) -> [u8; 28] {
    let digest = sha1_concat(sec_key, GUID);
    let mut out = [0u8; 28];
    let n = b64_encode(&digest, &mut out);
    debug_assert_eq!(n, 28);
    out
}

// ---------------------------------------------------------------
// Client request writer
// ---------------------------------------------------------------

/// Write a minimal WebSocket client handshake into `dst` and return
/// the number of bytes written. Zero-alloc.
///
/// The resulting request has:
///
/// ```text
/// GET {path} HTTP/1.1
/// Host: {host}
/// Upgrade: websocket
/// Connection: Upgrade
/// Sec-WebSocket-Key: {sec_key}
/// Sec-WebSocket-Version: 13
///
/// ```
///
/// Extensions are not negotiated (no permessage-deflate).
pub fn write_client_handshake(
    dst: &mut [u8],
    host: &[u8],
    path: &[u8],
    sec_key: &[u8; 24],
) -> Result<usize, HandshakeErr> {
    write_client_handshake_with_headers(dst, host, path, sec_key, &[])
}

/// [`write_client_handshake`] plus caller-supplied extra headers,
/// written verbatim as `name: value\r\n` lines before the final
/// CRLF. Needed by header-auth venues (Phase 8a §3.4). The caller
/// owns header-name/value validity (no CR/LF injection — inputs are
/// compile-time literals or `.env`-sourced credentials, asserted in
/// debug).
pub fn write_client_handshake_with_headers(
    dst: &mut [u8],
    host: &[u8],
    path: &[u8],
    sec_key: &[u8; 24],
    extra: &[(&[u8], &[u8])],
) -> Result<usize, HandshakeErr> {
    // Minimum width: 4 + path + 9 + 2 + 6 + host + 2 + 19 + 2 + 19 + 2
    //              + 18 + 24 + 2 + 22 + 2 + per-header (k + 2 + v + 2)
    //              + 2 (final CRLF)
    // Compute cheaply via running cursor.
    let mut o = 0usize;

    write_slice(dst, &mut o, b"GET ")?;
    write_slice(dst, &mut o, path)?;
    write_slice(dst, &mut o, b" HTTP/1.1\r\n")?;
    write_slice(dst, &mut o, b"Host: ")?;
    write_slice(dst, &mut o, host)?;
    write_slice(dst, &mut o, b"\r\n")?;
    write_slice(dst, &mut o, b"Upgrade: websocket\r\n")?;
    write_slice(dst, &mut o, b"Connection: Upgrade\r\n")?;
    write_slice(dst, &mut o, b"Sec-WebSocket-Key: ")?;
    write_slice(dst, &mut o, sec_key)?;
    write_slice(dst, &mut o, b"\r\n")?;
    write_slice(dst, &mut o, b"Sec-WebSocket-Version: 13\r\n")?;
    let mut i = 0;
    while i < extra.len() {
        let (name, value) = extra[i];
        debug_assert!(
            !contains_crlf(name) && !contains_crlf(value),
            "header injection: extra headers must not contain CR/LF"
        );
        write_slice(dst, &mut o, name)?;
        write_slice(dst, &mut o, b": ")?;
        write_slice(dst, &mut o, value)?;
        write_slice(dst, &mut o, b"\r\n")?;
        i += 1;
    }
    write_slice(dst, &mut o, b"\r\n")?;
    Ok(o)
}

/// CR/LF scan for the debug-build injection assert above.
#[inline]
fn contains_crlf(s: &[u8]) -> bool {
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\r' || s[i] == b'\n' {
            return true;
        }
        i += 1;
    }
    false
}

#[inline]
fn write_slice(dst: &mut [u8], o: &mut usize, s: &[u8]) -> Result<(), HandshakeErr> {
    let end = *o + s.len();
    if end > dst.len() {
        return Err(HandshakeErr::BufferTooSmall);
    }
    dst[*o..end].copy_from_slice(s);
    *o = end;
    Ok(())
}

// ---------------------------------------------------------------
// Server response parser
// ---------------------------------------------------------------

/// Parse the server handshake response out of `buf`.
///
/// Returns `Incomplete` if the headers haven't terminated yet
/// (`\r\n\r\n`), `Upgraded` on a valid `HTTP/1.1 101 Switching
/// Protocols` response with an `Upgrade: websocket` header and a
/// `Sec-WebSocket-Accept:` header, and `Malformed` otherwise.
///
/// The parser is deliberately strict on the status line and
/// case-insensitive on header names (HTTP/1.1 §3.2). Header values
/// are trimmed of leading/trailing spaces and compared
/// case-insensitively when that's what the protocol says (`Upgrade`,
/// `Connection` tokens) and case-sensitively for `Sec-WebSocket-Accept`
/// (base64 alphabet is case-sensitive).
pub fn read_server_handshake(buf: &[u8]) -> HandshakeResult {
    // Need the full header block (terminated by \r\n\r\n) before we
    // can commit to a result.
    let header_end = match find_header_terminator(buf) {
        Some(end) => end,
        None => return HandshakeResult::Incomplete,
    };

    // Status line: `HTTP/1.1 101 ...CRLF`
    let first_crlf = match find_crlf(&buf[..header_end]) {
        Some(i) => i,
        None => return HandshakeResult::Malformed,
    };
    let status = &buf[..first_crlf];
    if !status.starts_with(b"HTTP/1.1 101") && !status.starts_with(b"HTTP/1.0 101") {
        return HandshakeResult::Malformed;
    }

    // Header iteration.
    let mut seen_upgrade = false;
    let mut seen_connection = false;
    let mut accept_start: Option<usize> = None;
    let mut accept_end: Option<usize> = None;

    let mut pos = first_crlf + 2;
    while pos + 2 <= header_end {
        let line_end = match find_crlf(&buf[pos..header_end]) {
            Some(rel) => pos + rel,
            None => return HandshakeResult::Malformed,
        };
        // Blank line => headers done (shouldn't happen; header_end is here).
        if line_end == pos {
            break;
        }
        let line = &buf[pos..line_end];
        let colon = match memchr_u8(line, b':') {
            Some(i) => i,
            None => return HandshakeResult::Malformed,
        };
        let name = &line[..colon];
        // Trim leading spaces on the value.
        let mut vstart = colon + 1;
        while vstart < line.len() && (line[vstart] == b' ' || line[vstart] == b'\t') {
            vstart += 1;
        }
        let mut vend = line.len();
        while vend > vstart && (line[vend - 1] == b' ' || line[vend - 1] == b'\t') {
            vend -= 1;
        }
        let value = &line[vstart..vend];

        if eq_ascii_ci(name, b"upgrade") {
            if eq_ascii_ci(value, b"websocket") {
                seen_upgrade = true;
            }
        } else if eq_ascii_ci(name, b"connection") {
            if token_list_contains_ci(value, b"upgrade") {
                seen_connection = true;
            }
        } else if eq_ascii_ci(name, b"sec-websocket-accept") {
            if value.len() != 28 {
                return HandshakeResult::Malformed;
            }
            accept_start = Some(pos + vstart);
            accept_end = Some(pos + vend);
        }

        pos = line_end + 2;
    }

    match (seen_upgrade, seen_connection, accept_start, accept_end) {
        (true, true, Some(s), Some(e)) => HandshakeResult::Upgraded {
            accept_start: s,
            accept_end: e,
            header_end,
        },
        _ => HandshakeResult::Malformed,
    }
}

// ---------------------------------------------------------------
// Constant-time compare
// ---------------------------------------------------------------

/// Constant-time equality check over two byte slices of the same
/// length. Returns `false` if the lengths differ; timing does not
/// depend on which bytes mismatch.
#[inline]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    let mut i = 0usize;
    while i < a.len() {
        acc |= a[i] ^ b[i];
        i += 1;
    }
    acc == 0
}

// ---------------------------------------------------------------
// Tiny byte helpers (no `memchr` dep on purpose — these are small)
// ---------------------------------------------------------------

#[inline]
fn memchr_u8(buf: &[u8], needle: u8) -> Option<usize> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[inline]
fn find_crlf(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[inline]
fn find_header_terminator(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' && buf[i + 2] == b'\r' && buf[i + 3] == b'\n' {
            return Some(i + 4);
        }
        i += 1;
    }
    None
}

#[inline]
fn eq_ascii_ci(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if ascii_lower(a[i]) != ascii_lower(b[i]) {
            return false;
        }
        i += 1;
    }
    true
}

#[inline]
const fn ascii_lower(b: u8) -> u8 {
    if b >= b'A' && b <= b'Z' {
        b + 32
    } else {
        b
    }
}

/// `true` if `token` appears (case-insensitively) in the comma-
/// separated list `list`, with surrounding whitespace trimmed.
#[inline]
fn token_list_contains_ci(list: &[u8], token: &[u8]) -> bool {
    let mut i = 0usize;
    while i < list.len() {
        // Skip leading spaces/tabs.
        while i < list.len() && (list[i] == b' ' || list[i] == b'\t') {
            i += 1;
        }
        let start = i;
        while i < list.len() && list[i] != b',' {
            i += 1;
        }
        let mut end = i;
        while end > start && (list[end - 1] == b' ' || list[end - 1] == b'\t') {
            end -= 1;
        }
        if eq_ascii_ci(&list[start..end], token) {
            return true;
        }
        if i < list.len() {
            // skip the comma
            i += 1;
        }
    }
    false
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical RFC 3174 test vector: `sha1("abc")`.
    #[test]
    fn sha1_known_answer_abc() {
        let mut s = Sha1::new();
        s.update(b"abc");
        let got = s.finalize();
        let want: [u8; 20] = [
            0xA9, 0x99, 0x3E, 0x36, 0x47, 0x06, 0x81, 0x6A, 0xBA, 0x3E, 0x25, 0x71, 0x78, 0x50,
            0xC2, 0x6C, 0x9C, 0xD0, 0xD8, 0x9D,
        ];
        assert_eq!(got, want);
    }

    /// 448-bit test vector (exercises the 56-byte overflow pad path).
    #[test]
    fn sha1_known_answer_long() {
        let mut s = Sha1::new();
        s.update(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        let got = s.finalize();
        let want: [u8; 20] = [
            0x84, 0x98, 0x3E, 0x44, 0x1C, 0x3B, 0xD2, 0x6E, 0xBA, 0xAE, 0x4A, 0xA1, 0xF9, 0x51,
            0x29, 0xE5, 0xE5, 0x46, 0x70, 0xF1,
        ];
        assert_eq!(got, want);
    }

    /// Canonical RFC 6455 §1.3 accept-value test vector.
    ///
    /// Client key `"dGhlIHNhbXBsZSBub25jZQ=="` → server must return
    /// `"s3pPLMBiTxaQ9kYGzzhZRbK+xOo="`.
    #[test]
    fn expected_accept_rfc6455_example() {
        let key: [u8; 24] = *b"dGhlIHNhbXBsZSBub25jZQ==";
        let got = expected_accept(&key);
        assert_eq!(&got, b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn sec_websocket_key_is_24_bytes_base64() {
        let k = sec_websocket_key_from_seed(0xdead_beef);
        assert_eq!(k.len(), 24);
        // Must be all valid base64 characters ending in `==` (16-byte
        // payload → 2 padding chars).
        assert_eq!(&k[22..], b"==");
        for &b in &k[..22] {
            assert!(
                b.is_ascii_alphanumeric() || b == b'+' || b == b'/',
                "non-base64 byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn sec_websocket_key_is_deterministic_per_seed() {
        assert_eq!(
            sec_websocket_key_from_seed(1),
            sec_websocket_key_from_seed(1)
        );
        assert_ne!(
            sec_websocket_key_from_seed(1),
            sec_websocket_key_from_seed(2)
        );
    }

    #[test]
    fn b64_encode_known_vectors() {
        // RFC 4648 §10 test vectors.
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"f", b"Zg=="),
            (b"fo", b"Zm8="),
            (b"foo", b"Zm9v"),
            (b"foob", b"Zm9vYg=="),
            (b"fooba", b"Zm9vYmE="),
            (b"foobar", b"Zm9vYmFy"),
        ];
        for (input, want) in cases {
            let mut buf = [0u8; 32];
            let n = b64_encode(input, &mut buf);
            assert_eq!(&buf[..n], *want, "input={input:?}");
        }
    }

    #[test]
    fn with_headers_appends_extra_before_final_crlf() {
        let mut buf = [0u8; 512];
        let key = sec_websocket_key_from_seed(42);
        let n = write_client_handshake_with_headers(
            &mut buf,
            b"ws.okx.com",
            b"/ws/v5/public",
            &key,
            &[(b"OK-ACCESS-KEY", b"abc123"), (b"X-Custom", b"1")],
        )
        .unwrap();
        let body = &buf[..n];
        assert!(body.ends_with(b"\r\n\r\n"));
        let text = core::str::from_utf8(body).unwrap();
        assert!(text.contains("OK-ACCESS-KEY: abc123\r\n"));
        assert!(text.contains("X-Custom: 1\r\n"));
        // Extra headers must precede the terminating blank line.
        let blank = text.find("\r\n\r\n").unwrap();
        assert!(text.find("OK-ACCESS-KEY").unwrap() < blank);
        // Empty extra slice must produce byte-identical output to the
        // plain writer.
        let mut plain = [0u8; 512];
        let m = write_client_handshake(&mut plain, b"ws.okx.com", b"/ws/v5/public", &key).unwrap();
        let mut with_none = [0u8; 512];
        let k =
            write_client_handshake_with_headers(&mut with_none, b"ws.okx.com", b"/ws/v5/public", &key, &[])
                .unwrap();
        assert_eq!(&plain[..m], &with_none[..k]);
    }

    #[test]
    fn with_headers_propagates_buffer_too_small() {
        let mut tiny = [0u8; 32];
        let key = sec_websocket_key_from_seed(1);
        assert_eq!(
            write_client_handshake_with_headers(&mut tiny, b"h", b"/", &key, &[(b"A", b"B")]),
            Err(HandshakeErr::BufferTooSmall)
        );
    }

    #[test]
    fn write_client_handshake_ends_with_double_crlf() {
        let mut buf = [0u8; 512];
        let key = sec_websocket_key_from_seed(42);
        let n = write_client_handshake(&mut buf, b"example.com", b"/ws", &key).unwrap();
        let body = &buf[..n];
        assert!(body.ends_with(b"\r\n\r\n"));
        assert!(body.starts_with(b"GET /ws HTTP/1.1\r\n"));
        assert!(body.windows(8).any(|w| w == b"Host: ex"));
        assert!(
            body.windows(b"Sec-WebSocket-Key: ".len())
                .any(|w| w == b"Sec-WebSocket-Key: ")
        );
    }

    #[test]
    fn write_client_handshake_rejects_tiny_buffer() {
        let mut buf = [0u8; 16];
        let key = sec_websocket_key_from_seed(7);
        let err = write_client_handshake(&mut buf, b"example.com", b"/", &key);
        assert_eq!(err, Err(HandshakeErr::BufferTooSmall));
    }

    #[test]
    fn read_server_handshake_happy_path() {
        let key: [u8; 24] = *b"dGhlIHNhbXBsZSBub25jZQ==";
        let accept = expected_accept(&key);
        let mut resp = Vec::<u8>::new();
        resp.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
        resp.extend_from_slice(b"Upgrade: websocket\r\n");
        resp.extend_from_slice(b"Connection: Upgrade\r\n");
        resp.extend_from_slice(b"Sec-WebSocket-Accept: ");
        resp.extend_from_slice(&accept);
        resp.extend_from_slice(b"\r\n\r\n");

        match read_server_handshake(&resp) {
            HandshakeResult::Upgraded {
                accept_start,
                accept_end,
                header_end,
            } => {
                assert_eq!(&resp[accept_start..accept_end], &accept);
                assert_eq!(header_end, resp.len());
                assert!(constant_time_eq(&resp[accept_start..accept_end], &accept));
            }
            other => panic!("expected Upgraded, got {other:?}"),
        }
    }

    #[test]
    fn read_server_handshake_is_case_insensitive_on_header_names() {
        let key: [u8; 24] = *b"dGhlIHNhbXBsZSBub25jZQ==";
        let accept = expected_accept(&key);
        let mut resp = Vec::<u8>::new();
        resp.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
        resp.extend_from_slice(b"UPGRADE: WebSocket\r\n");
        resp.extend_from_slice(b"connection: keep-alive, Upgrade\r\n");
        resp.extend_from_slice(b"sec-websocket-accept: ");
        resp.extend_from_slice(&accept);
        resp.extend_from_slice(b"\r\n\r\n");

        assert!(matches!(
            read_server_handshake(&resp),
            HandshakeResult::Upgraded { .. }
        ));
    }

    #[test]
    fn read_server_handshake_incomplete() {
        let partial = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n";
        assert_eq!(
            read_server_handshake(partial),
            HandshakeResult::Incomplete
        );
    }

    #[test]
    fn read_server_handshake_rejects_wrong_status() {
        let resp = b"HTTP/1.1 200 OK\r\n\r\n";
        assert_eq!(read_server_handshake(resp), HandshakeResult::Malformed);
    }

    #[test]
    fn read_server_handshake_rejects_missing_upgrade() {
        let resp = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
        assert_eq!(read_server_handshake(resp), HandshakeResult::Malformed);
    }

    #[test]
    fn read_server_handshake_rejects_bad_accept_length() {
        let resp = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: tooshort\r\n\r\n";
        assert_eq!(read_server_handshake(resp), HandshakeResult::Malformed);
    }

    #[test]
    fn constant_time_eq_matches_eq_for_small_inputs() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn token_list_contains_is_case_insensitive() {
        assert!(token_list_contains_ci(b"keep-alive, UpGrAdE", b"upgrade"));
        assert!(token_list_contains_ci(b"Upgrade", b"upgrade"));
        assert!(!token_list_contains_ci(b"keep-alive", b"upgrade"));
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The reader must never panic on arbitrary bytes.
        #[test]
        fn read_server_handshake_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = read_server_handshake(&bytes);
        }

        /// Any seed → a 24-byte ASCII base64 key ending `==`.
        #[test]
        fn sec_websocket_key_is_base64_for_any_seed(seed in any::<u64>()) {
            let k = sec_websocket_key_from_seed(seed);
            prop_assert_eq!(k.len(), 24);
            prop_assert_eq!(&k[22..], b"==");
            for (i, &b) in k[..22].iter().enumerate() {
                prop_assert!(
                    b.is_ascii_alphanumeric() || b == b'+' || b == b'/',
                    "byte {} (0x{:02x}) is not base64", i, b
                );
            }
        }

        /// `expected_accept(k)` is deterministic.
        #[test]
        fn expected_accept_is_deterministic(seed in any::<u64>()) {
            let k = sec_websocket_key_from_seed(seed);
            prop_assert_eq!(expected_accept(&k), expected_accept(&k));
        }

        /// Writer + reader round-trip: a client request we generate,
        /// paired with a synthetic server response that uses the
        /// correct accept value, must parse cleanly.
        #[test]
        fn client_writer_server_reader_roundtrip(seed in any::<u64>()) {
            let key = sec_websocket_key_from_seed(seed);
            let mut req = [0u8; 512];
            let _ = write_client_handshake(&mut req, b"example.com", b"/", &key).unwrap();
            let accept = expected_accept(&key);
            let mut resp = Vec::<u8>::with_capacity(256);
            resp.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ");
            resp.extend_from_slice(&accept);
            resp.extend_from_slice(b"\r\n\r\n");
            match read_server_handshake(&resp) {
                HandshakeResult::Upgraded { accept_start, accept_end, .. } => {
                    prop_assert!(constant_time_eq(&resp[accept_start..accept_end], &accept));
                }
                _ => prop_assert!(false, "expected Upgraded"),
            }
        }
    }
}
