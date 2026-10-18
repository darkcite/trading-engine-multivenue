// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-crypto
//!
//! Handwritten SHA-256, HMAC-SHA256 and base64 (RFC 4648: the standard
//! alphabet, the URL-safe one, and a decoder that takes both). Zero heap
//! allocation, zero external dependencies.
//!
//! ## Who uses this
//!
//! * OKX private/login signing: `Base64(HMAC-SHA256(ts + method + path,
//!   secret))` (Phase 8 §5.1).
//! * AI-Ingress frame authentication (Phase 8 §8.4 / 8f §4.1): 16-byte
//!   truncated `HMAC-SHA256` tag per 82-byte UDS frame
//!   ([`hmac_sha256_tag16`]), verified with the constant-time compare
//!   ([`ct_eq`]).
//! * `core-net` WS handshake key/accept encoding (base64 lives here;
//!   the WS-specific SHA-1 stays in `core-net::ws_handshake`).
//! * Binance REST signatures (BX4): an Ed25519 signature rides a query
//!   as percent-encoded base64 ([`base64_encode_pct`]). Ed25519 itself is
//!   NOT here — ruling O-BX5 puts it in `signer-ed25519`, on `ring` — so
//!   this crate stays dependency-free.
//! * Polymarket CLOB L2 auth (PM15-6a): the API secret is URL-safe base64
//!   ([`base64_decode`], once at boot) and `POLY_SIGNATURE` is the
//!   URL-safe, `=`-padded base64 of `HMAC-SHA256(ts + METHOD + path +
//!   body)` ([`base64url_encode`]).
//!
//! ## Hot-path posture
//!
//! None of this runs on the tick hot path — signing happens on
//! connection setup and on the order path (µs-budget, not ns-budget).
//! The implementation is still allocation-free and works entirely in
//! caller-provided or stack storage, so it *may* be called from any
//! path without touching the allocator.
//!
//! Buffer-size errors are programmer errors: sizes are statically
//! known at every call site. Per the fail-fast doctrine they are
//! `debug_assert!`ed, and out-of-range writes abort via the normal
//! bounds check in release (`panic = "abort"`).

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

// ---------------------------------------------------------------
// SHA-256 (FIPS 180-4)
// ---------------------------------------------------------------

/// SHA-256 block size in bytes. Also the HMAC pad width.
pub const SHA256_BLOCK: usize = 64;

/// SHA-256 digest size in bytes.
pub const SHA256_LEN: usize = 32;

/// The most bytes [`Sha256::finalize_in_place`] writes after the message:
/// the padding's `0x80`, its zeros and the 8-B bit length — 72 when the
/// message's last block holds 56..=63 bytes and the length spills into a
/// second block.
pub const SHA256_PAD_MAX: usize = SHA256_BLOCK + 8;

/// FIPS 180-4 §4.2.2 round constants.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// FIPS 180-4 §5.3.3 initial hash state.
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Streaming SHA-256 context. Stack-only: 64 B block buffer + state.
///
/// ```
/// let mut h = core_crypto::Sha256::new();
/// h.update(b"ab");
/// h.update(b"c");
/// assert_eq!(h.finalize()[..4], [0xba, 0x78, 0x16, 0xbf]);
/// ```
#[derive(Copy, Clone)]
pub struct Sha256 {
    state: [u32; 8],
    /// Unprocessed tail of the message (always < 64 bytes between calls).
    buf: [u8; SHA256_BLOCK],
    /// Bytes currently valid in `buf`.
    buf_len: usize,
    /// Total message length in bytes.
    total_len: u64,
}

impl Sha256 {
    /// Fresh context.
    #[inline]
    pub const fn new() -> Self {
        Self {
            state: H0,
            buf: [0; SHA256_BLOCK],
            buf_len: 0,
            total_len: 0,
        }
    }

    /// Absorb `data`. Zero-alloc; processes full 64 B blocks in place.
    pub fn update(&mut self, data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        let mut i = 0usize;

        // Top up a partially-filled block first.
        // COPY: the input's head (< 64 B) completes the block the last call
        // stashed — a streaming hash's own; see the tail below.
        if self.buf_len > 0 {
            while self.buf_len < SHA256_BLOCK && i < data.len() {
                self.buf[self.buf_len] = data[i];
                self.buf_len += 1;
                i += 1;
            }
            if self.buf_len == SHA256_BLOCK {
                compress(&mut self.state, &self.buf);
                self.buf_len = 0;
            }
        }

        // Whole blocks straight from the input — no copy into `buf`, none
        // onto the stack: a fixed-size view of the input itself (the loop
        // condition holds its length, so the view always forms).
        while i + SHA256_BLOCK <= data.len() {
            if let Ok(block) = <&[u8; SHA256_BLOCK]>::try_from(&data[i..i + SHA256_BLOCK]) {
                compress(&mut self.state, block);
            }
            i += SHA256_BLOCK;
        }

        // COPY: the input's tail (< 64 B) into the context's block — a
        // streaming hash keeps its partial block between calls — rejected
        // where the caller owns room after its message: it finishes in
        // place instead ([`Self::finalize_in_place`], the HMAC signer).
        while i < data.len() {
            self.buf[self.buf_len] = data[i];
            self.buf_len += 1;
            i += 1;
        }
    }

    /// Pad, process the final block(s), and return the 32-byte digest —
    /// in place: the context is borrowed, never moved or copied, and is
    /// spent (reuse requires `new()`).
    pub fn finalize(&mut self) -> [u8; SHA256_LEN] {
        let bit_len = self.total_len.wrapping_mul(8);

        // Append the 0x80 terminator.
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;

        // If no room for the 8-byte length, pad out this block and
        // compress, then start a fresh all-zero block.
        if self.buf_len > SHA256_BLOCK - 8 {
            while self.buf_len < SHA256_BLOCK {
                self.buf[self.buf_len] = 0;
                self.buf_len += 1;
            }
            compress(&mut self.state, &self.buf);
            self.buf = [0; SHA256_BLOCK];
            self.buf_len = 0;
        }

        // Zero-fill up to the length field, then write the big-endian
        // bit length in the last 8 bytes.
        while self.buf_len < SHA256_BLOCK - 8 {
            self.buf[self.buf_len] = 0;
            self.buf_len += 1;
        }
        // COPY: the 8-B length field into the last block — the padding's
        // own write.
        self.buf[SHA256_BLOCK - 8..].copy_from_slice(&bit_len.to_be_bytes());
        compress(&mut self.state, &self.buf);
        self.digest()
    }

    /// **Absorb `buf[..len]` and finish, padding INSIDE `buf`** (BX7) — the
    /// zero-copy finish for a caller that owns room after its message
    /// (`buf.len() ≥ len + SHA256_PAD_MAX`): whole blocks are hashed
    /// straight from `buf`, the padding is written in place after the
    /// message (`buf[len..len + pad]`, which the caller then overwrites)
    /// and the final block(s) compressed from there — no byte of the
    /// message is stashed in the context, unlike [`Self::update`]'s tail.
    /// The context must hold no partial block (whole-block `update`s only
    /// before it, e.g. a 64-B HMAC pad). `None` when it does or the room is
    /// short. Spends the context.
    pub fn finalize_in_place(&mut self, buf: &mut [u8], len: usize) -> Option<[u8; SHA256_LEN]> {
        if self.buf_len != 0 || len > buf.len() || buf.len() - len < SHA256_PAD_MAX {
            return None;
        }
        let bit_len = self.total_len.wrapping_add(len as u64).wrapping_mul(8);
        let whole = len - len % SHA256_BLOCK;
        // The final block(s): the tail, `0x80`, zeros, the length — one
        // block when the tail leaves room for the 9 bytes, else two.
        let end = whole + if len - whole < SHA256_BLOCK - 8 { SHA256_BLOCK } else { 2 * SHA256_BLOCK };
        buf[len] = 0x80;
        buf[len + 1..end - 8].fill(0);
        // COPY: the 8-B length field after the zeros — the padding's own
        // write, in the caller's room.
        buf[end - 8..end].copy_from_slice(&bit_len.to_be_bytes());
        let mut at = 0;
        while at < end {
            if let Ok(block) = <&[u8; SHA256_BLOCK]>::try_from(&buf[at..at + SHA256_BLOCK]) {
                compress(&mut self.state, block);
            }
            at += SHA256_BLOCK;
        }
        Some(self.digest())
    }

    /// The state as the digest (big-endian words).
    #[inline]
    fn digest(&self) -> [u8; SHA256_LEN] {
        let mut out = [0u8; SHA256_LEN];
        let mut i = 0;
        while i < 8 {
            // COPY: each state word (4 B) into the digest — the result.
            out[i * 4..i * 4 + 4].copy_from_slice(&self.state[i].to_be_bytes());
            i += 1;
        }
        out
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// FIPS 180-4 §6.2.2 compression function over one 64-byte block.
fn compress(state: &mut [u32; 8], block: &[u8; SHA256_BLOCK]) {
    // Message schedule.
    let mut w = [0u32; 64];
    let mut t = 0usize;
    while t < 16 {
        w[t] = u32::from_be_bytes([
            block[t * 4],
            block[t * 4 + 1],
            block[t * 4 + 2],
            block[t * 4 + 3],
        ]);
        t += 1;
    }
    while t < 64 {
        let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
        let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
        w[t] = w[t - 16]
            .wrapping_add(s0)
            .wrapping_add(w[t - 7])
            .wrapping_add(s1);
        t += 1;
    }

    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];

    let mut t = 0usize;
    while t < 64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[t])
            .wrapping_add(w[t]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
        t += 1;
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// One-shot SHA-256.
#[inline]
pub fn sha256(data: &[u8]) -> [u8; SHA256_LEN] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

// ---------------------------------------------------------------
// HMAC-SHA256 (RFC 2104 / FIPS 198-1)
// ---------------------------------------------------------------

/// HMAC-SHA256 of `msg` under `key`. Keys longer than the 64-byte
/// block are pre-hashed per RFC 2104. Zero-alloc: two stack contexts
/// and two 64-byte pads, the key XORed straight into them (no copy of
/// it; the zero bytes past a short key leave the pads' constants).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; SHA256_LEN] {
    hmac_sha256_parts(key, &[msg])
}

/// HMAC-SHA256 of the CONCATENATION of `parts` under `key`, without
/// concatenating: the inner context absorbs each slice where it lies.
///
/// Polymarket's L2 MAC covers `ts ‖ METHOD ‖ path ‖ body` — four spans
/// of the request already rendered in its own buffer (PM15-6a, zero-copy:
/// no staging of the message). Zero-alloc.
pub fn hmac_sha256_parts(key: &[u8], parts: &[&[u8]]) -> [u8; SHA256_LEN] {
    // A key longer than the block is its digest (RFC 2104 §2).
    let long;
    let k: &[u8] = if key.len() > SHA256_BLOCK {
        long = sha256(key);
        &long
    } else {
        key
    };
    let mut ipad = [0x36u8; SHA256_BLOCK];
    let mut opad = [0x5cu8; SHA256_BLOCK];
    let mut i = 0;
    while i < k.len() {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
        i += 1;
    }

    let mut inner = Sha256::new();
    inner.update(&ipad);
    let mut j = 0usize;
    while j < parts.len() {
        inner.update(parts[j]);
        j += 1;
    }
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner_digest);
    outer.finalize()
}

/// Truncated-MAC width used by the AI-ingress frame protocol
/// (design 8f §4.1): `tag = HMAC-SHA256(key, cmd)[0..16]`.
pub const HMAC_TAG16_LEN: usize = 16;

/// HMAC-SHA256 truncated to its leftmost 16 bytes (RFC 2104 §5
/// truncation; 128-bit tag). The AI-ingress frame tag. Zero-alloc.
#[inline]
pub fn hmac_sha256_tag16(key: &[u8], msg: &[u8]) -> [u8; HMAC_TAG16_LEN] {
    let full = hmac_sha256(key, msg);
    let mut out = [0u8; HMAC_TAG16_LEN];
    // COPY: the tag's leftmost 16 B out of the 32-B digest — the result.
    out.copy_from_slice(&full[..HMAC_TAG16_LEN]);
    out
}

/// Constant-time byte-slice equality for MAC verification.
///
/// Accumulates XOR differences across the **entire** common width with
/// no data-dependent branch, so the comparison time is independent of
/// where the first mismatch sits — the property that defeats
/// byte-at-a-time tag-forgery timing probes. [`core::hint::black_box`]
/// pins the accumulator against compiler shortcuts.
///
/// Length mismatch returns `false` immediately: lengths are protocol
/// constants here (16-byte tags in 82-byte frames), never secrets.
#[inline]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    let mut i = 0;
    while i < a.len() {
        diff = core::hint::black_box(diff | (a[i] ^ b[i]));
        i += 1;
    }
    diff == 0
}

// ---------------------------------------------------------------
// Base64 (RFC 4648 §4, standard alphabet, '=' padding)
// ---------------------------------------------------------------

const B64_ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// RFC 4648 §5, the URL- and filename-safe alphabet (`-` and `_` for `+`
/// and `/`).
const B64_URL_ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encoded length for `n` input bytes (including `=` padding).
#[inline(always)]
pub const fn base64_encoded_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// Encode `input` into `dst`, returning the number of bytes written
/// (always [`base64_encoded_len`]`(input.len())`).
///
/// `dst` too small is a programmer error — every call site passes a
/// statically-sized buffer. Debug builds assert; release builds abort
/// via the bounds check (fail-fast doctrine). Zero-alloc.
pub fn base64_encode(input: &[u8], dst: &mut [u8]) -> usize {
    debug_assert!(dst.len() >= base64_encoded_len(input.len()));
    b64_render::<false, false>(input, dst)
}

/// Encode `input` with the URL-safe alphabet (RFC 4648 §5) **and** `=`
/// padding into `dst`, returning [`base64_encoded_len`]`(input.len())`.
///
/// Polymarket's L2 `POLY_SIGNATURE` is this encoding of an HMAC-SHA256
/// tag (44 characters). `dst` too small is a programmer error, as for
/// [`base64_encode`]. Zero-alloc.
pub fn base64url_encode(input: &[u8], dst: &mut [u8]) -> usize {
    debug_assert!(dst.len() >= base64_encoded_len(input.len()));
    b64_render::<false, true>(input, dst)
}

/// Upper bound of [`base64_decode`]'s output for `n` encoded bytes.
#[inline(always)]
pub const fn base64_decoded_max_len(n: usize) -> usize {
    n.div_ceil(4) * 3
}

/// A base64 text [`base64_decode`] refuses.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum B64Err {
    /// A byte outside both alphabets (and not trailing `=`).
    Char,
    /// A length no padding completes (`len % 4 == 1`), or `=` before the end.
    Shape,
    /// `dst` shorter than [`base64_decoded_max_len`].
    Dst,
}

/// Decode `input` into `dst`; returns the bytes written. **Both
/// alphabets** (standard `+/` and URL-safe `-_`) and **padding present or
/// absent** are accepted — the forms Polymarket's SDKs accept for an API
/// secret. Boot-time (an API secret is decoded once); zero-alloc.
///
/// # Errors
/// [`B64Err`] — nothing in `dst` is meaningful then.
pub fn base64_decode(input: &[u8], dst: &mut [u8]) -> Result<usize, B64Err> {
    let mut n = input.len();
    while n > 0 && input[n - 1] == b'=' {
        n -= 1;
    }
    if input.len() - n > 2 || n % 4 == 1 {
        return Err(B64Err::Shape);
    }
    if dst.len() < base64_decoded_max_len(n) {
        return Err(B64Err::Dst);
    }
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut o = 0usize;
    let mut i = 0usize;
    while i < n {
        let c = input[i];
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => return Err(B64Err::Shape),
            _ => return Err(B64Err::Char),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            dst[o] = (acc >> bits) as u8;
            o += 1;
        }
        i += 1;
    }
    Ok(o)
}

/// Upper bound of [`base64_encode_pct`]'s output for `n` input bytes:
/// every base64 character escaped to three bytes.
#[inline(always)]
pub const fn base64_pct_encoded_max_len(n: usize) -> usize {
    base64_encoded_len(n) * 3
}

/// Base64 (standard alphabet, `=` padding) percent-encoded for a URL
/// query or an `application/x-www-form-urlencoded` body (RFC 3986 §2.1):
/// `+` → `%2B`, `/` → `%2F`, `=` → `%3D`, every other alphabet byte as
/// is. Returns the bytes written.
///
/// Binance's REST APIs take an Ed25519 signature this way. It is the
/// same one-pass render as [`base64_encode`], escaping each character as
/// it is written — no base64 string exists anywhere but in `dst`.
///
/// `dst` must hold [`base64_pct_encoded_max_len`]`(input.len())`: a
/// programmer error otherwise, as for [`base64_encode`] (debug assert;
/// release aborts on the bounds check). Zero-alloc.
pub fn base64_encode_pct(input: &[u8], dst: &mut [u8]) -> usize {
    debug_assert!(dst.len() >= base64_pct_encoded_max_len(input.len()));
    b64_render::<true, false>(input, dst)
}

/// The one base64 walk behind the encoders: 3-byte groups, then the
/// padded tail. `PCT` escapes each character as it is written; `URL`
/// picks the URL-safe alphabet; `<false, false>` monomorphizes to the
/// plain encoder.
#[inline(always)]
fn b64_render<const PCT: bool, const URL: bool>(input: &[u8], dst: &mut [u8]) -> usize {
    let alpha: &[u8; 64] = if URL { B64_URL_ALPHA } else { B64_ALPHA };
    let mut i = 0usize;
    let mut o = 0usize;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        o += b64_put::<PCT>(alpha[((n >> 18) & 0x3F) as usize], &mut dst[o..]);
        o += b64_put::<PCT>(alpha[((n >> 12) & 0x3F) as usize], &mut dst[o..]);
        o += b64_put::<PCT>(alpha[((n >> 6) & 0x3F) as usize], &mut dst[o..]);
        o += b64_put::<PCT>(alpha[(n & 0x3F) as usize], &mut dst[o..]);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = (input[i] as u32) << 16;
        o += b64_put::<PCT>(alpha[((n >> 18) & 0x3F) as usize], &mut dst[o..]);
        o += b64_put::<PCT>(alpha[((n >> 12) & 0x3F) as usize], &mut dst[o..]);
        o += b64_put::<PCT>(b'=', &mut dst[o..]);
        o += b64_put::<PCT>(b'=', &mut dst[o..]);
    } else if rem == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        o += b64_put::<PCT>(alpha[((n >> 18) & 0x3F) as usize], &mut dst[o..]);
        o += b64_put::<PCT>(alpha[((n >> 12) & 0x3F) as usize], &mut dst[o..]);
        o += b64_put::<PCT>(alpha[((n >> 6) & 0x3F) as usize], &mut dst[o..]);
        o += b64_put::<PCT>(b'=', &mut dst[o..]);
    }
    o
}

/// One base64 character into `dst`; with `PCT`, escaped when RFC 3986
/// reserves it. Returns the bytes written (1, or 3 for an escape).
#[inline(always)]
fn b64_put<const PCT: bool>(c: u8, dst: &mut [u8]) -> usize {
    if PCT {
        let esc: &[u8; 2] = match c {
            b'+' => b"2B",
            b'/' => b"2F",
            b'=' => b"3D",
            _ => {
                dst[0] = c;
                return 1;
            }
        };
        dst[0] = b'%';
        dst[1] = esc[0];
        dst[2] = esc[1];
        return 3;
    }
    dst[0] = c;
    1
}

// ---------------------------------------------------------------
// Tests — NIST FIPS 180-4 / RFC 4231 / RFC 4648 vectors
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    // ---- SHA-256 ----

    #[test]
    fn sha256_empty() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_abc() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_two_block_message() {
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_million_a_streaming() {
        // FIPS 180-4 long vector; exercises the streaming path with
        // chunk sizes that straddle block boundaries.
        let mut h = Sha256::new();
        let chunk = [b'a'; 977]; // prime-ish, misaligned with 64
        let mut fed = 0usize;
        while fed < 1_000_000 {
            let take = core::cmp::min(977, 1_000_000 - fed);
            h.update(&chunk[..take]);
            fed += take;
        }
        assert_eq!(
            hex(&h.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sha256_length_field_boundary() {
        // 55 bytes fits terminator+length in one block; 56 forces a
        // second block. Both must be correct.
        let v55 = [0x41u8; 55];
        let v56 = [0x41u8; 56];
        assert_eq!(
            hex(&sha256(&v55)),
            hex(&{
                let mut h = Sha256::new();
                h.update(&v55[..20]);
                h.update(&v55[20..]);
                h.finalize()
            })
        );
        assert_ne!(hex(&sha256(&v55)), hex(&sha256(&v56)));
    }

    /// BX7: finishing in place equals the streaming finish at every
    /// message length around the block and padding bounds, after a
    /// whole-block prefix (an HMAC pad) or none; the padding stays in the
    /// caller's room and the message is untouched; a context holding a
    /// partial block, or a room short of `SHA256_PAD_MAX`, is refused.
    #[test]
    fn finalize_in_place_matches_the_streaming_finish() {
        let msg: [u8; 200] = core::array::from_fn(|i| (i * 7 + 3) as u8);
        let mut len = 0;
        while len <= msg.len() {
            for prefix in [false, true] {
                let (mut s, mut p) = (Sha256::new(), Sha256::new());
                if prefix {
                    s.update(&[0x36; SHA256_BLOCK]);
                    p.update(&[0x36; SHA256_BLOCK]);
                }
                s.update(&msg[..len]);
                let want = s.finalize();
                let mut buf = [0xAAu8; 200 + SHA256_PAD_MAX + 8];
                buf[..len].copy_from_slice(&msg[..len]);
                assert_eq!(p.finalize_in_place(&mut buf[..len + SHA256_PAD_MAX], len), Some(want), "len {len}, prefix {prefix}");
                assert_eq!(&buf[..len], &msg[..len], "the message is untouched");
                assert!(buf[len + SHA256_PAD_MAX..].iter().all(|b| *b == 0xAA), "nothing past the room");
            }
            len += 1;
        }
        let mut buf = [0u8; 128];
        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(h.finalize_in_place(&mut buf, 3), None, "a partial block in the context");
        let mut h = Sha256::new();
        assert_eq!(h.finalize_in_place(&mut buf[..80], 9), None, "a room short of SHA256_PAD_MAX");
        let mut h = Sha256::new();
        assert_eq!(h.finalize_in_place(&mut buf[..81], 9), Some(sha256(&[0u8; 9])));
    }

    // ---- HMAC-SHA256 (RFC 4231) ----

    #[test]
    fn hmac_rfc4231_case_1() {
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_rfc4231_case_2() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_rfc4231_case_3() {
        let key = [0xaau8; 20];
        let data = [0xddu8; 50];
        assert_eq!(
            hex(&hmac_sha256(&key, &data)),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    #[test]
    fn hmac_rfc4231_case_6_long_key() {
        // Key longer than the block size — exercises the pre-hash.
        let key = [0xaau8; 131];
        assert_eq!(
            hex(&hmac_sha256(
                &key,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn hmac_rfc4231_case_7_long_key_long_data() {
        let key = [0xaau8; 131];
        assert_eq!(
            hex(&hmac_sha256(
                &key,
                b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm."
            )),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    // ---- truncated tag (RFC 4231 case 5 is THE 128-bit KAT) ----

    #[test]
    fn hmac_tag16_rfc4231_case_5_truncated_vector() {
        // RFC 4231 test case 5: the only official HMAC-SHA-256 vector
        // published in truncated-to-128-bits form.
        let key = [0x0cu8; 20];
        let tag = hmac_sha256_tag16(&key, b"Test With Truncation");
        assert_eq!(hex(&tag), "a3b6167473100ee06e0c796c2955552b");
    }

    #[test]
    fn hmac_tag16_is_prefix_of_full_mac() {
        // RFC 2104 §5: truncation keeps the leftmost bytes. Pin against
        // the case-2 full vector so prefix + KAT agree.
        let tag = hmac_sha256_tag16(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(hex(&tag), "5bdcc146bf60754e6a042426089575c7");
        let full = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(tag[..], full[..HMAC_TAG16_LEN]);
    }

    #[test]
    fn hmac_tag16_differs_across_keys_and_messages() {
        // Failure-mode: a wrong key or a flipped message byte must not
        // verify. (Exercises exactly what the ingress accept path does.)
        let key = [0xabu8; 32];
        let msg = [0x11u8; 64];
        let tag = hmac_sha256_tag16(&key, &msg);

        let mut wrong_key = key;
        wrong_key[0] ^= 1;
        assert_ne!(tag, hmac_sha256_tag16(&wrong_key, &msg));

        let mut wrong_msg = msg;
        wrong_msg[63] ^= 1;
        assert_ne!(tag, hmac_sha256_tag16(&key, &wrong_msg));
    }

    // ---- constant-time compare ----

    #[test]
    fn ct_eq_accepts_equal_slices() {
        let a = [0x5au8; HMAC_TAG16_LEN];
        let b = [0x5au8; HMAC_TAG16_LEN];
        assert!(ct_eq(&a, &b));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn ct_eq_rejects_any_difference() {
        let a = [0x5au8; HMAC_TAG16_LEN];
        // First, middle, and last byte — the accumulate-OR must catch
        // a mismatch at every position.
        let mut b = a;
        b[0] ^= 0x01;
        assert!(!ct_eq(&a, &b));
        let mut c = a;
        c[7] ^= 0x80;
        assert!(!ct_eq(&a, &c));
        let mut d = a;
        d[HMAC_TAG16_LEN - 1] ^= 0xFF;
        assert!(!ct_eq(&a, &d));
    }

    #[test]
    fn ct_eq_rejects_length_mismatch() {
        let a = [0u8; 16];
        let b = [0u8; 15];
        assert!(!ct_eq(&a, &b));
    }

    #[test]
    fn ct_eq_verifies_a_real_frame_tag() {
        // End-to-end shape of the 8f accept path: tag over 64 cmd
        // bytes, verify good, reject forged.
        let key = [0x42u8; 32];
        let cmd = [0x07u8; 64];
        let tag = hmac_sha256_tag16(&key, &cmd);
        assert!(ct_eq(&tag, &hmac_sha256_tag16(&key, &cmd)));
        let mut forged = tag;
        forged[3] ^= 0x10;
        assert!(!ct_eq(&tag, &forged));
    }

    // ---- base64 (RFC 4648 §10) ----

    #[test]
    fn base64_rfc4648_vectors() {
        let cases: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (input, want) in cases {
            let mut dst = [0u8; 16];
            let n = base64_encode(input, &mut dst);
            assert_eq!(n, base64_encoded_len(input.len()));
            assert_eq!(&dst[..n], want.as_bytes(), "input {input:?}");
        }
    }

    #[test]
    fn base64_encodes_hmac_output_at_44_chars() {
        // The OKX login shape: Base64(HMAC-SHA256(...)) — 32 bytes in,
        // 44 chars out, no truncation.
        let tag = hmac_sha256(b"secret", b"1700000000GET/users/self/verify");
        let mut dst = [0u8; 44];
        let n = base64_encode(&tag, &mut dst);
        assert_eq!(n, 44);
        assert_eq!(base64_encoded_len(32), 44);
        // Every byte must be valid base64 or padding.
        for b in dst {
            assert!(b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=');
        }
    }

    // ---- base64, percent-encoded (RFC 4648 §10 + RFC 3986 §2.1) ----

    #[test]
    fn base64_pct_rfc4648_vectors() {
        // The RFC 4648 §10 table, its '=' padding escaped.
        let cases: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg%3D%3D"),
            (b"fo", "Zm8%3D"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg%3D%3D"),
            (b"fooba", "Zm9vYmE%3D"),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (input, want) in cases {
            let mut dst = [0u8; 32];
            let n = base64_encode_pct(input, &mut dst);
            assert_eq!(&dst[..n], want.as_bytes(), "input {input:?}");
        }
    }

    #[test]
    fn base64_pct_escapes_plus_and_slash() {
        // 0xFB 0xFF encodes to "+/8=" — all three reserved bytes at once.
        let mut plain = [0u8; 4];
        assert_eq!(base64_encode(&[0xFB, 0xFF], &mut plain), 4);
        assert_eq!(&plain, b"+/8=");
        let mut dst = [0u8; 12];
        let n = base64_encode_pct(&[0xFB, 0xFF], &mut dst);
        assert_eq!(&dst[..n], b"%2B%2F8%3D");
    }

    #[test]
    fn base64_pct_unescapes_to_the_plain_form_and_fits_its_bound() {
        // Every 2-byte input: the escaped form decodes back to exactly
        // base64_encode's output, never exceeds the bound, and carries
        // no byte outside [A-Za-z0-9%].
        let mut plain = [0u8; 4];
        let mut dst = [0u8; 12];
        let mut back = [0u8; 12];
        let mut v = 0u32;
        while v < 0x1_0000 {
            let input = [(v >> 8) as u8, v as u8];
            let p = base64_encode(&input, &mut plain);
            let n = base64_encode_pct(&input, &mut dst);
            assert!(n <= base64_pct_encoded_max_len(2));
            let mut i = 0;
            let mut m = 0;
            while i < n {
                let b = dst[i];
                assert!(b.is_ascii_alphanumeric() || b == b'%', "byte {b:#x}");
                if b == b'%' {
                    back[m] = match (dst[i + 1], dst[i + 2]) {
                        (b'2', b'B') => b'+',
                        (b'2', b'F') => b'/',
                        (b'3', b'D') => b'=',
                        other => panic!("unexpected escape {other:?}"),
                    };
                    i += 3;
                } else {
                    back[m] = b;
                    i += 1;
                }
                m += 1;
            }
            assert_eq!(&back[..m], &plain[..p], "input {input:?}");
            v += 1;
        }
    }

    // ---- PM15-6a: URL-safe encode, the any-alphabet decode ----

    #[test]
    fn base64url_rfc4648_vectors_and_the_two_alphabet_bytes() {
        let cases: [(&[u8], &[u8]); 4] = [
            (b"", b""),
            (b"f", b"Zg=="),
            (b"foob", b"Zm9vYg=="),
            (b"foobar", b"Zm9vYmFy"),
        ];
        let mut dst = [0u8; 16];
        for (input, want) in cases {
            let n = base64url_encode(input, &mut dst);
            assert_eq!(&dst[..n], want);
        }
        // 0xfb 0xff: `+/` in the standard alphabet, `-_` URL-safe.
        let n = base64_encode(&[0xfb, 0xff], &mut dst);
        assert_eq!(&dst[..n], b"+/8=");
        let n = base64url_encode(&[0xfb, 0xff], &mut dst);
        assert_eq!(&dst[..n], b"-_8=");
    }

    #[test]
    fn base64_decode_takes_both_alphabets_with_or_without_padding() {
        let mut out = [0u8; 8];
        for text in [&b"-_8="[..], b"+/8=", b"-_8", b"+/8"] {
            let n = base64_decode(text, &mut out).unwrap();
            assert_eq!(&out[..n], &[0xfb, 0xff], "{text:?}");
        }
        let n = base64_decode(b"Zm9vYmFy", &mut out).unwrap();
        assert_eq!(&out[..n], b"foobar");
        let n = base64_decode(b"", &mut out).unwrap();
        assert_eq!(n, 0);
        assert_eq!(base64_decode(b"Zm9v!mFy", &mut out), Err(B64Err::Char));
        assert_eq!(base64_decode(b"Zm9vY", &mut out), Err(B64Err::Shape));
        assert_eq!(base64_decode(b"Zg===", &mut out), Err(B64Err::Shape));
        assert_eq!(base64_decode(b"Z=g=", &mut out), Err(B64Err::Shape));
        assert_eq!(base64_decode(b"Zm9vYmFyZm9v", &mut out[..8]), Err(B64Err::Dst));
    }

    /// Polymarket's SDK HMAC secret vector: 43 `A`s and `=` are 32 zero bytes.
    #[test]
    fn base64_decode_reads_the_sdk_secret_and_round_trips_every_pair() {
        let mut out = [0xAAu8; 33];
        let n = base64_decode(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", &mut out).unwrap();
        assert_eq!(n, 32);
        assert!(out[..32].iter().all(|b| *b == 0));
        let mut enc = [0u8; 4];
        let mut dec = [0u8; 3];
        let mut v = 0u32;
        while v < 0x1_0000 {
            let input = [(v >> 8) as u8, v as u8];
            let e = base64url_encode(&input, &mut enc);
            let d = base64_decode(&enc[..e], &mut dec).unwrap();
            assert_eq!(&dec[..d], &input);
            let e = base64_encode(&input, &mut enc);
            let d = base64_decode(&enc[..e], &mut dec).unwrap();
            assert_eq!(&dec[..d], &input);
            v += 1;
        }
    }

    /// The parts form equals the one-shot form at every split of one
    /// RFC 4231 case-2 message.
    #[test]
    fn hmac_parts_equals_the_one_shot_at_every_split() {
        let key = b"Jefe";
        let msg = b"what do ya want for nothing?";
        let whole = hmac_sha256(key, msg);
        let mut i = 0usize;
        while i <= msg.len() {
            let mut j = i;
            while j <= msg.len() {
                assert_eq!(hmac_sha256_parts(key, &[&msg[..i], &msg[i..j], &msg[j..]]), whole);
                j += 1;
            }
            i += 1;
        }
        assert_eq!(hmac_sha256_parts(key, &[]), hmac_sha256(key, b""));
    }
}
