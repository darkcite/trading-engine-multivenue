// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # WebSocket frame codec (RFC 6455)
//!
//! Pure `&[u8]` / `&mut [u8]` parsers and serializers. No IO, no
//! allocation, no external dependencies beyond `core` + `std`.
//!
//! Every ingress adapter talks to its upstream WebSocket over a
//! `FixedBuf`; the ingress event loop does `socket.read(buf.free_mut())`
//! and then loops calling [`ws_read_frame`] until it returns
//! [`WsReadResult::Incomplete`]. Client-to-server frames are written
//! with [`ws_write_text_frame`] into a separate preallocated tx buffer.
//!
//! ## Non-goals
//!
//! * No fragment reassembly logic lives here. The higher-level adapter
//!   owns the fragment-merging state machine (it knows where the
//!   message-payload buffer lives).
//! * No close-frame status-code decoding. The event loop just forwards
//!   a received Close to its handler.
//! * No permessage-deflate. We never negotiate it in the handshake.
//!
//! ## Zero-alloc guarantee
//!
//! All functions in this module operate on borrowed slices. The
//! `alloc_assertions` harness exercises a 10_000-iteration roundtrip
//! and asserts `0 B/op`.

// ---------------------------------------------------------------
// Span — Copy-able alternative to Range<usize>
// ---------------------------------------------------------------

/// Half-open byte range `[start, end)`. `Copy`-able alternative to
/// `core::ops::Range<usize>` (which isn't `Copy` on stable).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct PayloadSpan {
    /// Inclusive start index.
    pub start: usize,
    /// Exclusive end index.
    pub end: usize,
}

impl PayloadSpan {
    /// Constant constructor.
    #[inline(always)]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Number of bytes spanned.
    #[inline(always)]
    pub const fn len(self) -> usize {
        self.end - self.start
    }

    /// Is the span empty?
    #[inline(always)]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

// ---------------------------------------------------------------
// Opcode
// ---------------------------------------------------------------

/// RFC 6455 §5.2 opcodes. Values outside the enum are treated as
/// malformed input.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WsOpcode {
    /// Continuation of a fragmented message.
    Continuation = 0x0,
    /// UTF-8 text payload.
    Text = 0x1,
    /// Binary payload.
    Binary = 0x2,
    /// Close control frame.
    Close = 0x8,
    /// Ping control frame — peer expects a Pong.
    Ping = 0x9,
    /// Pong control frame — response to a Ping.
    Pong = 0xA,
}

impl WsOpcode {
    /// Is this a control frame (0x8..=0xF)?
    #[inline(always)]
    pub const fn is_control(self) -> bool {
        (self as u8) & 0x08 != 0
    }

    /// Decode a nibble. Returns `None` for reserved or unknown opcodes.
    #[inline]
    pub const fn from_nibble(n: u8) -> Option<Self> {
        match n {
            0x0 => Some(Self::Continuation),
            0x1 => Some(Self::Text),
            0x2 => Some(Self::Binary),
            0x8 => Some(Self::Close),
            0x9 => Some(Self::Ping),
            0xA => Some(Self::Pong),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------
// Frame header
// ---------------------------------------------------------------

/// Parsed RFC 6455 frame header. POD; carries no borrow.
///
/// `header_len` is the number of bytes at the start of the frame that
/// constitute the header (2..=14). The payload begins at
/// `start + header_len` and spans `payload_len` bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct WsFrameHeader {
    /// Is this the final fragment of the message?
    pub fin: bool,
    /// Opcode (decoded).
    pub opcode: WsOpcode,
    /// Was the payload masked? (Client frames: yes; server frames: no.)
    pub masked: bool,
    /// Payload length in bytes (0..=u64::MAX, but in practice bounded
    /// by the caller's rx buffer size).
    pub payload_len: u64,
    /// Number of bytes occupied by the header itself (2..=14).
    pub header_len: u8,
    /// Masking key (valid iff `masked`). Zeroed when unmasked.
    pub mask: [u8; 4],
}

/// Result of [`ws_read_frame`]. The `Frame` variant does **not** unmask
/// the payload — the caller decides whether to unmask in place and
/// whether to consume the bytes from its buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WsReadResult {
    /// Not enough bytes in the buffer yet — keep reading from the
    /// socket and call again.
    Incomplete,
    /// A full frame is available. `payload` is a byte-range into
    /// the input buffer that spans the (still-masked) payload.
    Frame {
        /// Parsed header.
        header: WsFrameHeader,
        /// Half-open byte range into the input buffer covering the
        /// payload (post-header).
        payload: PayloadSpan,
    },
    /// The buffer contents violate RFC 6455. Caller should close the
    /// connection.
    Malformed,
}

/// Serialization error. Non-allocating.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WsWriteErr {
    /// Destination slice cannot fit header + payload.
    BufferTooSmall,
    /// A header-first render ([`ws_write_text_frame_rendered`]) failed on
    /// its writing pass, or wrote a different length there from the one
    /// it counted.
    RenderDiverged,
}

// ---------------------------------------------------------------
// Frame reader
// ---------------------------------------------------------------

/// Parse the next frame header out of `buf`.
///
/// This function does not consume any bytes. On a successful
/// `Frame { payload, header }` return, the caller is expected to
/// advance its read buffer past `header.header_len + payload.len()`
/// bytes once it has processed (and optionally unmasked) the payload.
///
/// Zero-alloc. Bounded by the 14-byte maximum frame header; never
/// touches bytes outside `buf`.
#[inline]
pub fn ws_read_frame(buf: &[u8]) -> WsReadResult {
    // We need at least the 2-byte minimum header.
    if buf.len() < 2 {
        return WsReadResult::Incomplete;
    }

    let b0 = buf[0];
    let b1 = buf[1];

    let fin = (b0 & 0x80) != 0;
    // Reserved bits (RSV1/2/3) must be zero since we don't negotiate
    // extensions (no permessage-deflate).
    if (b0 & 0x70) != 0 {
        return WsReadResult::Malformed;
    }
    let opcode = match WsOpcode::from_nibble(b0 & 0x0F) {
        Some(op) => op,
        None => return WsReadResult::Malformed,
    };

    // Control frames must not be fragmented (RFC 6455 §5.4).
    if opcode.is_control() && !fin {
        return WsReadResult::Malformed;
    }

    let masked = (b1 & 0x80) != 0;
    let len_code = b1 & 0x7F;

    // Compute payload length + header length by walking the length
    // encoding.
    let (payload_len, mut cursor) = match len_code {
        0..=125 => (len_code as u64, 2usize),
        126 => {
            if buf.len() < 4 {
                return WsReadResult::Incomplete;
            }
            let v = u16::from_be_bytes([buf[2], buf[3]]) as u64;
            // Per RFC, extended length must be > 125 for the 16-bit
            // form to be canonical. We still accept it on read (some
            // intermediaries are non-canonical) but refuse to emit it
            // that way.
            (v, 4usize)
        }
        127 => {
            if buf.len() < 10 {
                return WsReadResult::Incomplete;
            }
            // Most significant bit of a 64-bit length must be zero.
            if buf[2] & 0x80 != 0 {
                return WsReadResult::Malformed;
            }
            let v = u64::from_be_bytes([
                buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
            ]);
            (v, 10usize)
        }
        _ => unreachable!(), // len_code is 7 bits, exhausted above
    };

    // Control-frame payload MUST be <= 125 bytes (RFC 6455 §5.5).
    if opcode.is_control() && payload_len > 125 {
        return WsReadResult::Malformed;
    }

    let mut mask = [0u8; 4];
    if masked {
        if buf.len() < cursor + 4 {
            return WsReadResult::Incomplete;
        }
        mask.copy_from_slice(&buf[cursor..cursor + 4]);
        cursor += 4;
    }

    // Enough bytes for the whole payload?
    let payload_end = match (cursor as u64).checked_add(payload_len) {
        Some(end) => end,
        None => return WsReadResult::Malformed,
    };
    if (buf.len() as u64) < payload_end {
        return WsReadResult::Incomplete;
    }
    // Safe downcast because we just compared against buf.len() (usize).
    let payload_end = payload_end as usize;

    WsReadResult::Frame {
        header: WsFrameHeader {
            fin,
            opcode,
            masked,
            payload_len,
            // cursor is 2, 4, 6, 8, 10, or 14 — always fits in u8.
            header_len: cursor as u8,
            mask,
        },
        payload: PayloadSpan::new(cursor, payload_end),
    }
}

// ---------------------------------------------------------------
// In-place unmask
// ---------------------------------------------------------------

/// XOR-unmask `buf` in place using `mask`. No allocation.
///
/// Walks the buffer in 8-byte strides when the slice is long enough,
/// with a scalar tail. The stride body compiles to a pair of 64-bit
/// XORs on aarch64/x86_64.
///
/// # Safety invariants
/// This function is entirely safe. Any byte slice is fair game.
#[inline]
pub fn ws_unmask_in_place(buf: &mut [u8], mask: [u8; 4]) {
    // Expand 4-byte mask to 8 bytes for wider XOR chunks.
    let mask8 = [
        mask[0], mask[1], mask[2], mask[3], mask[0], mask[1], mask[2], mask[3],
    ];
    let m64 = u64::from_ne_bytes(mask8);

    let mut i = 0usize;
    let len = buf.len();

    // Fast path: 8-byte strided XOR. Tolerates unaligned loads on the
    // architectures we target (M4 / x86_64). No allocation, no branching
    // except the loop exit.
    while i + 8 <= len {
        // SAFETY: we just checked i+8 <= len, so the read and write are
        // both in-bounds. Unaligned load/store is permitted on aarch64
        // and x86_64 — the platforms we build for — and does not UB in
        // Rust when using `read_unaligned`/`write_unaligned`.
        unsafe {
            let p = buf.as_mut_ptr().add(i) as *mut u64;
            let v = p.read_unaligned();
            p.write_unaligned(v ^ m64);
        }
        i += 8;
    }

    // Scalar tail.
    while i < len {
        // Mask index is (i - start_of_tail) mod 4, which is the same as
        // `i mod 4` because the fast-path stride preserves alignment
        // w.r.t. the 4-byte mask.
        buf[i] ^= mask[i & 3];
        i += 1;
    }
}

// ---------------------------------------------------------------
// Frame writer (client → server; always masked)
// ---------------------------------------------------------------

/// Serialize a single-fragment client text frame into `dst`.
///
/// Always sets `FIN=1` and masks the payload. Returns the number of
/// bytes written, or `WsWriteErr::BufferTooSmall` if `dst` can't hold
/// the header+payload.
///
/// Zero-alloc. The caller supplies a 4-byte `mask`; callers typically
/// draw it from a 64-bit xoshiro seeded at boot.
#[inline]
pub fn ws_write_text_frame(
    dst: &mut [u8],
    payload: &[u8],
    mask: [u8; 4],
) -> Result<usize, WsWriteErr> {
    ws_write_frame(dst, WsOpcode::Text, payload, mask)
}

/// Serialize a single-fragment client text frame whose payload is the
/// concatenation of `parts`, each written straight into `dst` — a
/// caller never assembles the payload anywhere first (BX0: the Binance
/// spot sentinel's SUBSCRIBE). Same contract as
/// [`ws_write_text_frame`]: `FIN=1`, masked, zero-alloc.
#[inline]
pub fn ws_write_text_frame_parts(
    dst: &mut [u8],
    parts: &[&[u8]],
    mask: [u8; 4],
) -> Result<usize, WsWriteErr> {
    ws_write_frame_parts(dst, WsOpcode::Text, parts, mask)
}

/// Binary counterpart of [`ws_write_text_frame_parts`]: the payload is
/// the concatenation of `parts`, each written straight into `dst` (the
/// RPC venues' fixed-shape JSON-RPC requests). `FIN=1`, masked,
/// zero-alloc.
#[inline]
pub fn ws_write_binary_frame_parts(
    dst: &mut [u8],
    parts: &[&[u8]],
    mask: [u8; 4],
) -> Result<usize, WsWriteErr> {
    ws_write_frame_parts(dst, WsOpcode::Binary, parts, mask)
}

/// Serialize a Pong control frame (in response to a Ping). Control
/// frames must not be fragmented; payload must be ≤ 125 bytes.
#[inline]
pub fn ws_write_pong(dst: &mut [u8], payload: &[u8], mask: [u8; 4]) -> Result<usize, WsWriteErr> {
    debug_assert!(payload.len() <= 125, "pong payload must be <= 125 bytes");
    ws_write_frame(dst, WsOpcode::Pong, payload, mask)
}

/// Serialize a Ping control frame with an optional ≤125-byte payload.
#[inline]
pub fn ws_write_ping(dst: &mut [u8], payload: &[u8], mask: [u8; 4]) -> Result<usize, WsWriteErr> {
    debug_assert!(payload.len() <= 125, "ping payload must be <= 125 bytes");
    ws_write_frame(dst, WsOpcode::Ping, payload, mask)
}

#[inline]
fn ws_write_frame(
    dst: &mut [u8],
    opcode: WsOpcode,
    payload: &[u8],
    mask: [u8; 4],
) -> Result<usize, WsWriteErr> {
    ws_write_frame_parts(dst, opcode, &[payload], mask)
}

/// The one frame serialiser: the payload is the concatenation of
/// `parts`, each written straight into place.
#[inline]
fn ws_write_frame_parts(
    dst: &mut [u8],
    opcode: WsOpcode,
    parts: &[&[u8]],
    mask: [u8; 4],
) -> Result<usize, WsWriteErr> {
    let mut plen = 0usize;
    let mut k = 0usize;
    while k < parts.len() {
        plen += parts[k].len();
        k += 1;
    }
    let total = client_header_len(plen) + plen;
    if dst.len() < total {
        return Err(WsWriteErr::BufferTooSmall);
    }
    let cursor = write_client_header(dst, opcode, plen, mask);
    debug_assert_eq!(cursor, client_header_len(plen), "header vs its length");

    // Copy the payload parts, then XOR-mask them in place in one pass.
    // Copy-then-mask (rather than streaming through a scratch) because
    // `dst` is the caller's preallocated tx buffer and the final write
    // must go out masked.
    let mut at = cursor;
    let mut k = 0usize;
    while k < parts.len() {
        let p = parts[k];
        // COPY: each payload part into the final wire buffer, the frame's
        // payload in all — the serialiser's own write: WebSocket masks
        // the payload on the wire and the caller's bytes are not ours to
        // mutate — rejected: masking the caller's buffer in place (not
        // ours) or staging the parts in a scratch first (a second copy).
        dst[at..at + p.len()].copy_from_slice(p);
        at += p.len();
        k += 1;
    }
    ws_unmask_in_place(&mut dst[cursor..cursor + plen], mask);

    Ok(total)
}

/// Header length of a client frame carrying `plen` payload bytes: the
/// shortest length form (RFC 6455 §5.2 requires it) plus the 4-byte mask
/// key.
#[inline]
const fn client_header_len(plen: usize) -> usize {
    if plen <= 125 {
        2 + 4
    } else if plen <= u16::MAX as usize {
        2 + 2 + 4
    } else {
        2 + 8 + 4
    }
}

/// Write a client frame's header — FIN, `opcode`, MASK, the shortest
/// length form for `plen`, the mask key — into `dst`, which holds at least
/// [`client_header_len`]`(plen)` bytes. Returns the header's length.
#[inline]
fn write_client_header(dst: &mut [u8], opcode: WsOpcode, plen: usize, mask: [u8; 4]) -> usize {
    // Byte 0: FIN=1 | RSV=0 | opcode.
    dst[0] = 0x80 | (opcode as u8);

    // Byte 1: MASK=1 | len code (+ extended length following).
    let mut cursor: usize;
    if plen <= 125 {
        dst[1] = 0x80 | (plen as u8);
        cursor = 2;
    } else if plen <= u16::MAX as usize {
        dst[1] = 0x80 | 126;
        let be = (plen as u16).to_be_bytes();
        dst[2] = be[0];
        dst[3] = be[1];
        cursor = 4;
    } else {
        dst[1] = 0x80 | 127;
        let be = (plen as u64).to_be_bytes();
        dst[2..10].copy_from_slice(&be);
        cursor = 10;
    }

    // 4-byte mask follows the length encoding.
    dst[cursor..cursor + 4].copy_from_slice(&mask);
    cursor += 4;
    cursor
}

// ---------------------------------------------------------------
// Header-first rendered frames
// ---------------------------------------------------------------

/// A client frame's payload, rendered straight into place
/// ([`ws_write_text_frame_rendered`]). A render appends through
/// [`put`](Self::put) and runs twice, identically: a counting pass, which
/// writes nothing (the header needs the payload's length first), then the
/// writing pass into the frame behind the header.
#[derive(Debug)]
pub struct WsPayload<'a> {
    dst: &'a mut [u8],
    len: usize,
    counting: bool,
}

impl<'a> WsPayload<'a> {
    /// A counting pass: [`put`](Self::put) only adds up lengths.
    #[inline]
    pub fn counting() -> Self {
        Self {
            dst: &mut [],
            len: 0,
            counting: true,
        }
    }

    /// A writing pass into `dst` — the frame's payload span, or a test's
    /// buffer.
    #[inline]
    pub fn writing(dst: &'a mut [u8]) -> Self {
        Self {
            dst,
            len: 0,
            counting: false,
        }
    }

    /// Append `bytes` to the payload.
    ///
    /// # Errors
    /// [`WsWriteErr::BufferTooSmall`] when a writing pass runs past its
    /// destination.
    #[inline]
    pub fn put(&mut self, bytes: &[u8]) -> Result<(), WsWriteErr> {
        let end = self.len + bytes.len();
        if !self.counting {
            let slot = self
                .dst
                .get_mut(self.len..end)
                .ok_or(WsWriteErr::BufferTooSmall)?;
            // COPY: the render itself — each literal fragment, symbol and
            // id put once, straight into the frame's payload (≤ tx's free
            // room, 16–24 KiB; at session start; Deribit's request id is
            // put from its 20 B digit render) — the only write of those
            // bytes into tx; the mask is then an in-place XOR — rejected:
            // the scratch render and masked copy into tx it replaces.
            slot.copy_from_slice(bytes);
        }
        self.len = end;
        Ok(())
    }

    /// Bytes appended (or counted) so far.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been appended yet.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Serialize a single-fragment client text frame whose payload `render`
/// writes straight into `dst`, header first: a counting pass sizes the
/// payload, the header goes down, the writing pass renders behind it, and
/// the payload is masked in place — the batch subscribes, whose length is
/// unknown until they are rendered, need no scratch and no second copy.
/// `FIN=1`, masked, zero-alloc. Cold paths only: the render runs twice.
///
/// `render` must produce the same bytes on both passes — a pure function
/// of what it captured (`Fn`: it cannot mutate a capture).
///
/// # Errors
/// [`WsWriteErr::BufferTooSmall`] when `dst` cannot hold the frame, or the
/// render's own error from its counting pass — both before a byte is
/// written; [`WsWriteErr::RenderDiverged`] when the writing pass fails or
/// writes a different length from the one it counted.
#[inline]
pub fn ws_write_text_frame_rendered<F>(
    dst: &mut [u8],
    mask: [u8; 4],
    render: F,
) -> Result<usize, WsWriteErr>
where
    F: Fn(&mut WsPayload<'_>) -> Result<(), WsWriteErr>,
{
    let mut count = WsPayload::counting();
    render(&mut count)?;
    let plen = count.len;
    let hdr = client_header_len(plen);
    let total = hdr + plen;
    if dst.len() < total {
        return Err(WsWriteErr::BufferTooSmall);
    }
    let at = write_client_header(dst, WsOpcode::Text, plen, mask);
    debug_assert_eq!(at, hdr, "header vs its length");
    // The writing pass gets exactly the counted span: running past it, or
    // stopping short of it, is a render that diverged from its count.
    let payload = &mut dst[hdr..total];
    let mut written = WsPayload::writing(payload);
    if render(&mut written).is_err() || written.len != plen {
        debug_assert!(
            false,
            "a render's writing pass diverged from its count of {plen} B"
        );
        return Err(WsWriteErr::RenderDiverged);
    }
    ws_unmask_in_place(&mut dst[hdr..total], mask);
    Ok(total)
}

// ---------------------------------------------------------------
// Mask-key draw helper
// ---------------------------------------------------------------

/// Draw a 4-byte mask key from a 64-bit counter. Clients that want a
/// real CSPRNG-derived mask can provide their own; this helper is just
/// a fast, zero-alloc default that callers back by an `AtomicU64`
/// incremented at boot.
#[inline]
pub fn ws_mask_from_counter(counter: u64) -> [u8; 4] {
    // splitmix-style scramble so adjacent counters produce very
    // different masks (important because a static mask is a denial-of-
    // service footgun per RFC 6455 §10.3).
    let mut z = counter.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let b = z.to_ne_bytes();
    [b[0], b[1], b[2], b[3]]
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Header-first rendered frames ----

    /// A render over `n` repeats of a 7-byte fragment, then a tail.
    fn render_n(p: &mut WsPayload<'_>, n: usize) -> Result<(), WsWriteErr> {
        let mut i = 0;
        while i < n {
            p.put(b"{\"a\":1}")?;
            i += 1;
        }
        p.put(b"!")
    }

    #[test]
    fn a_rendered_frame_is_byte_identical_to_the_parts_writer_in_every_length_form() {
        // 7-bit (≤ 125), 16-bit and 64-bit length forms.
        for n in [0usize, 17, 18, 1_000, 9_363, 9_400] {
            let parts: Vec<&[u8]> = (0..n)
                .map(|_| &b"{\"a\":1}"[..])
                .chain(core::iter::once(&b"!"[..]))
                .collect();
            let mask = [0x11, 0x22, 0x33, 0x44];
            let mut want = vec![0u8; 14 + 7 * n + 1];
            let w = ws_write_text_frame_parts(&mut want, &parts, mask).unwrap();
            let mut got = vec![0u8; 14 + 7 * n + 1];
            let g = ws_write_text_frame_rendered(&mut got, mask, |p| render_n(p, n)).unwrap();
            assert_eq!(&got[..g], &want[..w], "n = {n}");
        }
    }

    #[test]
    fn a_rendered_frame_that_does_not_fit_writes_nothing_and_fails() {
        let mut dst = [0u8; 20];
        assert_eq!(
            ws_write_text_frame_rendered(&mut dst, [1, 2, 3, 4], |p| render_n(p, 3)),
            Err(WsWriteErr::BufferTooSmall)
        );
        assert_eq!(dst, [0u8; 20], "the counting pass sized it first");
    }

    #[test]
    fn a_rendered_frame_reads_back_at_every_length_boundary() {
        // Each length form at its edges, read back by the frame reader and
        // unmasked — a check independent of the header writer the parts
        // frame shares.
        let mask = [0xA5, 0x5A, 0x0F, 0xF0];
        for (len, header_len) in [(0usize, 6u8), (125, 6), (126, 8), (65_535, 8), (65_536, 14)] {
            let body: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut dst = vec![0u8; 14 + len];
            let n = ws_write_text_frame_rendered(&mut dst, mask, |p| p.put(&body)).unwrap();
            let WsReadResult::Frame { header, payload } = ws_read_frame(&dst[..n]) else {
                panic!("len = {len}: not a frame");
            };
            assert!(header.fin && header.masked && header.opcode == WsOpcode::Text);
            assert_eq!(
                (header.header_len, header.payload_len),
                (header_len, len as u64)
            );
            assert_eq!(payload.end, n, "len = {len}");
            ws_unmask_in_place(&mut dst[payload.start..payload.end], header.mask);
            assert_eq!(&dst[payload.start..payload.end], &body[..], "len = {len}");
        }
    }

    #[test]
    fn a_render_that_fails_its_counting_pass_writes_nothing() {
        let mut dst = [0u8; 32];
        let r = ws_write_text_frame_rendered(&mut dst, [1, 2, 3, 4], |_| {
            Err(WsWriteErr::BufferTooSmall)
        });
        assert_eq!(r, Err(WsWriteErr::BufferTooSmall), "the render's own error");
        assert_eq!(dst, [0u8; 32]);
    }

    /// A render asked for `first` repeats on its counting pass and
    /// `second` on its writing pass.
    fn diverging(first: usize, second: usize) -> Result<usize, WsWriteErr> {
        let calls = core::cell::Cell::new(0);
        let mut dst = [0u8; 64];
        ws_write_text_frame_rendered(&mut dst, [1, 2, 3, 4], |p| {
            calls.set(calls.get() + 1);
            render_n(p, if calls.get() == 1 { first } else { second })
        })
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "diverged from its count"))]
    fn a_render_that_writes_more_than_it_counted_is_refused() {
        assert_eq!(diverging(1, 2), Err(WsWriteErr::RenderDiverged));
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "diverged from its count"))]
    fn a_render_that_writes_less_than_it_counted_is_refused() {
        assert_eq!(diverging(2, 1), Err(WsWriteErr::RenderDiverged));
    }

    #[test]
    fn a_payload_counts_without_writing_and_writes_within_bounds() {
        let mut c = WsPayload::counting();
        assert!(c.is_empty());
        c.put(b"abc").unwrap();
        c.put(b"de").unwrap();
        assert_eq!(c.len(), 5);
        let mut buf = [0u8; 4];
        let mut w = WsPayload::writing(&mut buf);
        w.put(b"abc").unwrap();
        assert_eq!(w.put(b"de"), Err(WsWriteErr::BufferTooSmall));
        assert_eq!(w.len(), 3, "a refused put appends nothing");
        assert_eq!(&buf[..3], b"abc");
    }

    // ---- Reader ----

    #[test]
    fn read_minimum_header_happy_path() {
        // Unmasked text frame, empty payload.
        let buf: &[u8] = &[0x81, 0x00];
        match ws_read_frame(buf) {
            WsReadResult::Frame { header, payload } => {
                assert!(header.fin);
                assert_eq!(header.opcode, WsOpcode::Text);
                assert!(!header.masked);
                assert_eq!(header.payload_len, 0);
                assert_eq!(header.header_len, 2);
                assert_eq!(payload, PayloadSpan::new(2, 2));
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn read_too_short_returns_incomplete() {
        assert_eq!(ws_read_frame(&[0x81]), WsReadResult::Incomplete);
        assert_eq!(ws_read_frame(&[]), WsReadResult::Incomplete);
    }

    #[test]
    fn read_detects_rsv_bits_as_malformed() {
        // RSV1 set — we don't negotiate extensions.
        let buf: &[u8] = &[0xC1, 0x00];
        assert_eq!(ws_read_frame(buf), WsReadResult::Malformed);
    }

    #[test]
    fn read_detects_unknown_opcode_as_malformed() {
        // Opcode 0x3 is reserved.
        let buf: &[u8] = &[0x83, 0x00];
        assert_eq!(ws_read_frame(buf), WsReadResult::Malformed);
    }

    #[test]
    fn read_detects_fragmented_control_frame_as_malformed() {
        // FIN=0 on a Ping.
        let buf: &[u8] = &[0x09, 0x00];
        assert_eq!(ws_read_frame(buf), WsReadResult::Malformed);
    }

    #[test]
    fn read_16_bit_length_parses() {
        // Unmasked binary frame, payload len = 200 → 126 path.
        let mut buf = Vec::with_capacity(4 + 200);
        buf.extend_from_slice(&[0x82, 126]);
        buf.extend_from_slice(&200u16.to_be_bytes());
        buf.extend(core::iter::repeat_n(0xAAu8, 200));
        match ws_read_frame(&buf) {
            WsReadResult::Frame { header, payload } => {
                assert_eq!(header.opcode, WsOpcode::Binary);
                assert_eq!(header.payload_len, 200);
                assert_eq!(header.header_len, 4);
                assert_eq!(payload.len(), 200);
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn read_64_bit_length_incomplete_partial_header() {
        // len=127 but only 2 of 8 extended-length bytes present.
        let buf: &[u8] = &[0x82, 127, 0x00, 0x00];
        assert_eq!(ws_read_frame(buf), WsReadResult::Incomplete);
    }

    #[test]
    fn read_64_bit_msb_set_is_malformed() {
        let mut v = vec![0x82, 127];
        v.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(ws_read_frame(&v), WsReadResult::Malformed);
    }

    #[test]
    fn read_masked_client_frame_keeps_payload_masked() {
        // "hi" masked with [0x37, 0xFA, 0x21, 0x3D].
        let mask = [0x37u8, 0xFA, 0x21, 0x3D];
        let plain = b"hi";
        let masked_payload = [plain[0] ^ mask[0], plain[1] ^ mask[1]];
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x81, 0x82]);
        buf.extend_from_slice(&mask);
        buf.extend_from_slice(&masked_payload);
        match ws_read_frame(&buf) {
            WsReadResult::Frame { header, payload } => {
                assert!(header.masked);
                assert_eq!(header.mask, mask);
                assert_eq!(header.header_len, 6);
                assert_eq!(&buf[payload.start..payload.end], &masked_payload);
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn read_control_frame_len_over_125_is_malformed() {
        let buf: &[u8] = &[0x89, 126, 0x00, 0xF0];
        assert_eq!(ws_read_frame(buf), WsReadResult::Malformed);
    }

    // ---- Unmask ----

    #[test]
    fn unmask_empty_slice_is_noop() {
        let mut buf: [u8; 0] = [];
        ws_unmask_in_place(&mut buf, [0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn unmask_small_payload_uses_scalar_tail() {
        let mut buf = [0x00u8, 0x01, 0x02]; // len=3, scalar-tail only
        ws_unmask_in_place(&mut buf, [0xFF, 0x01, 0x02, 0x03]);
        assert_eq!(buf, [0xFF, 0x00, 0x00]);
    }

    #[test]
    fn unmask_is_self_inverse() {
        let mut buf = [5u8; 257]; // > 8, forces stride + tail
        let mask = [0x01, 0x02, 0x03, 0x04];
        let orig = buf;
        ws_unmask_in_place(&mut buf, mask);
        ws_unmask_in_place(&mut buf, mask);
        assert_eq!(buf, orig);
    }

    // ---- Writer ----

    #[test]
    fn write_text_frame_round_trip() {
        let mut dst = [0u8; 256];
        let mask = [1u8, 2, 3, 4];
        let written = ws_write_text_frame(&mut dst, b"hello", mask).unwrap();
        assert_eq!(written, 2 + 4 + 5);

        // Re-parse the written frame.
        match ws_read_frame(&dst[..written]) {
            WsReadResult::Frame { header, payload } => {
                assert!(header.fin);
                assert_eq!(header.opcode, WsOpcode::Text);
                assert!(header.masked);
                assert_eq!(header.mask, mask);
                let mut plain = dst[payload.start..payload.end].to_vec();
                ws_unmask_in_place(&mut plain, header.mask);
                assert_eq!(&plain, b"hello");
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    /// BX0: a frame written from parts is byte-identical to the frame
    /// written from their concatenation, in every length class.
    #[test]
    fn parts_frame_matches_the_concatenated_frame() {
        let mask = [0x11u8, 0x22, 0x33, 0x44];
        for total in [0usize, 3, 125, 126, 1_000, 70_000] {
            let whole: Vec<u8> = (0..total).map(|i| (i * 7 + 3) as u8).collect();
            let (a, rest) = whole.split_at(total / 3);
            let (b, c) = rest.split_at(rest.len() / 2);
            let mut one = vec![0u8; total + 14];
            let mut three = vec![0u8; total + 14];
            let n1 = ws_write_text_frame(&mut one, &whole, mask).unwrap();
            let n3 = ws_write_text_frame_parts(&mut three, &[a, b, c], mask).unwrap();
            assert_eq!(n1, n3, "length class {total}");
            assert_eq!(one[..n1], three[..n3], "length class {total}");
        }
        // Too small a destination is refused, never truncated.
        let mut tiny = [0u8; 9];
        assert_eq!(
            ws_write_text_frame_parts(&mut tiny, &[b"abc", b"defg"], mask),
            Err(WsWriteErr::BufferTooSmall)
        );
    }

    #[test]
    fn write_errors_when_buffer_too_small() {
        let mut dst = [0u8; 3]; // header alone won't fit
        let mask = [0u8; 4];
        assert_eq!(
            ws_write_text_frame(&mut dst, b"payload", mask),
            Err(WsWriteErr::BufferTooSmall)
        );
    }

    #[test]
    fn write_16_bit_length_boundary() {
        // 126 triggers the 16-bit form.
        let payload = vec![0xAAu8; 126];
        let mut dst = vec![0u8; 4 + 4 + payload.len()];
        let n = ws_write_binary_frame_parts(&mut dst, &[&payload], [9, 9, 9, 9]).unwrap();
        assert_eq!(n, 2 + 2 + 4 + 126);
        // Re-parse.
        let r = ws_read_frame(&dst[..n]);
        match r {
            WsReadResult::Frame { header, .. } => {
                assert_eq!(header.payload_len, 126);
                assert_eq!(header.header_len, 8);
            }
            _ => panic!("round-trip failed"),
        }
    }

    #[test]
    fn ping_pong_helpers_produce_correct_opcodes() {
        let mut a = [0u8; 64];
        let n = ws_write_ping(&mut a, b"p", [1, 2, 3, 4]).unwrap();
        assert_eq!(a[0] & 0x0F, WsOpcode::Ping as u8);
        assert!(a[0] & 0x80 != 0); // FIN set
        assert_eq!(n, 2 + 4 + 1);

        let mut b = [0u8; 64];
        let m = ws_write_pong(&mut b, b"q", [1, 2, 3, 4]).unwrap();
        assert_eq!(b[0] & 0x0F, WsOpcode::Pong as u8);
        assert_eq!(m, 2 + 4 + 1);
    }

    #[test]
    fn mask_counter_produces_distinct_masks_for_adjacent_ids() {
        let m0 = ws_mask_from_counter(0);
        let m1 = ws_mask_from_counter(1);
        let m2 = ws_mask_from_counter(2);
        assert_ne!(m0, m1);
        assert_ne!(m1, m2);
        assert_ne!(m0, m2);
    }
}

// ---------------------------------------------------------------
// Property tests — arbitrary masked payloads roundtrip cleanly.
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn masked_write_then_read_roundtrips(
            payload in proptest::collection::vec(any::<u8>(), 0..=300),
            mask in proptest::array::uniform4(any::<u8>()),
        ) {
            let hdr_overhead = if payload.len() <= 125 { 6 } else { 8 };
            let mut dst = vec![0u8; hdr_overhead + payload.len()];
            let n = ws_write_text_frame(&mut dst, &payload, mask).unwrap();
            prop_assert_eq!(n, hdr_overhead + payload.len());

            match ws_read_frame(&dst[..n]) {
                WsReadResult::Frame { header, payload: span } => {
                    prop_assert_eq!(header.mask, mask);
                    prop_assert_eq!(header.payload_len, payload.len() as u64);
                    let mut got = dst[span.start..span.end].to_vec();
                    ws_unmask_in_place(&mut got, header.mask);
                    prop_assert_eq!(got, payload);
                }
                other => prop_assert!(false, "roundtrip failed: {:?}", other),
            }
        }

        #[test]
        fn arbitrary_bytes_dont_panic_reader(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            // Simply must not panic.
            let _ = ws_read_frame(&buf);
        }

        #[test]
        fn unmask_inverse_over_arbitrary_inputs(
            bytes in proptest::collection::vec(any::<u8>(), 0..=300),
            mask in proptest::array::uniform4(any::<u8>()),
        ) {
            let mut buf = bytes.clone();
            ws_unmask_in_place(&mut buf, mask);
            ws_unmask_in_place(&mut buf, mask);
            prop_assert_eq!(buf, bytes);
        }
    }
}
