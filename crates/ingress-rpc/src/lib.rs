//! # ingress-rpc
//!
//! Polygon JSON-RPC ingress (Alchemy primary, QuickNode failover).
//!
//! Phase 0 shipped: request ID allocator + method-tag enum.
//!
//! Phase 1a adds:
//! * [`classify_rpc`] — single-pass classifier for response vs.
//!   subscription vs. error vs. unknown frames.
//! * [`parse_hex_u64`] — parse a `0x`-prefixed lowercase-hex integer.
//! * [`parse_block_number_result`] — parse `eth_blockNumber` response.
//! * [`parse_new_head_notification`] — parse an `eth_subscription`
//!   `newHeads` push into a POD [`NewHead`].
//! * [`RpcError`] — zero-copy extraction of JSON-RPC error codes and
//!   messages (message stored as a range, not copied).
//! * Request serializers for the methods we actually use, writing into
//!   a caller-preallocated stack buffer (no allocation).
//!
//! Everything is a zero-alloc byte scanner over `&[u8]`. No
//! `serde_json`, no `hyper`, no `reqwest`. The transport layer (the
//! persistent H/2 session for RPC HTTPS + the WS session for
//! `eth_subscribe`) lands in Phase 1b.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use core_parse::{find_field, scan_i64, scan_u64, skip_byte, Pos};
use std::sync::atomic::{AtomicU64, Ordering};

pub mod run_loop;

pub use run_loop::{
    drive_one, note_transport_ready, run, Driver, Pending, RpcKind, RunResult, State, StopFlag,
    SubId, SubKind, DEFAULT_SIGNAL_RING_CAP, PENDING_CAP, RPC_POLL_NS, RX_BUF_SIZE, SUB_CAP,
    TX_BUF_SIZE,
};

// ---------------------------------------------------------------
// Request id allocator + method enum (Phase 0 API, unchanged)
// ---------------------------------------------------------------

/// Monotonic request ID allocator. One per endpoint instance.
pub struct RequestIds {
    next: AtomicU64,
}

impl RequestIds {
    /// Construct starting at `1`.
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    /// Allocate the next id. Relaxed — only one consumer per endpoint.
    #[inline]
    pub fn allocate(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for RequestIds {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical method tags used by the Polygon event feed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RpcMethod {
    /// `eth_blockNumber`.
    BlockNumber = 0,
    /// `eth_getLogs`.
    GetLogs = 1,
    /// `eth_subscribe` (newHeads / logs).
    Subscribe = 2,
}

// ---------------------------------------------------------------
// Frame classification
// ---------------------------------------------------------------

/// High-level RPC frame shape decided by the presence of marker
/// fields. Never parses nesting.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RpcFrameKind {
    /// Contains `"result":` — a success response.
    Response = 0,
    /// Contains `"method":"eth_subscription"` — a push.
    Subscription = 1,
    /// Contains `"error":{` — a JSON-RPC error.
    Error = 2,
    /// Anything we can't classify. Caller logs and drops.
    Unknown = 3,
}

/// Classify an RPC frame by probing marker fields.
#[inline]
pub fn classify_rpc(buf: &[u8]) -> RpcFrameKind {
    // Order matters: subscription is the push we care about most.
    if memchr::memmem::find(buf, b"\"method\":\"eth_subscription\"").is_some() {
        RpcFrameKind::Subscription
    } else if memchr::memmem::find(buf, b"\"error\":{").is_some() {
        RpcFrameKind::Error
    } else if memchr::memmem::find(buf, b"\"result\":").is_some() {
        RpcFrameKind::Response
    } else {
        RpcFrameKind::Unknown
    }
}

// ---------------------------------------------------------------
// Hex parsing — 0x-prefixed lowercase hex → u64
// ---------------------------------------------------------------

/// Parse a `0x`-prefixed ASCII hex integer starting at `pos`. Returns
/// `(value, new_pos)` on success.
///
/// Accepts both upper- and lowercase digits. Rejects empty hex
/// (e.g. `"0x"` alone). Zero-alloc, no panic.
#[inline]
pub fn parse_hex_u64(buf: &[u8], pos: Pos) -> Option<(u64, Pos)> {
    if pos + 2 > buf.len() || buf[pos] != b'0' || (buf[pos + 1] != b'x' && buf[pos + 1] != b'X') {
        return None;
    }
    let mut i = pos + 2;
    let mut v: u64 = 0;
    let mut any = false;
    while i < buf.len() {
        let b = buf[i];
        let nibble = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => 10 + (b - b'a'),
            b'A'..=b'F' => 10 + (b - b'A'),
            _ => break,
        };
        // Overflow check: u64 holds 16 hex digits max.
        if i - (pos + 2) >= 16 {
            return None;
        }
        v = (v << 4) | (nibble as u64);
        any = true;
        i += 1;
    }
    if any {
        Some((v, i))
    } else {
        None
    }
}

// ---------------------------------------------------------------
// Response parsers
// ---------------------------------------------------------------

/// Parse an `eth_blockNumber` response frame. Returns `(id, block)`.
///
/// Expected shape:
/// ```text
/// {"jsonrpc":"2.0","id":3,"result":"0x1a2b3c"}
/// ```
#[inline]
pub fn parse_block_number_result(buf: &[u8]) -> Option<(u64, u64)> {
    let pos = find_field(buf, b"\"id\":")?;
    let (id, _) = scan_u64(buf, pos)?;

    let pos = find_field(buf, b"\"result\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (block, _) = parse_hex_u64(buf, pos)?;

    Some((id, block))
}

/// A new-head push from `eth_subscribe("newHeads", ...)`. POD.
///
/// Layout: 48 bytes — fits under a 64-byte cache line with headroom.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct NewHead {
    /// Block number (parsed from `result.number`).
    pub number: u64,
    /// Block timestamp in seconds (parsed from `result.timestamp`).
    /// `0` if the field was missing (some endpoints omit it on first
    /// `newHead`).
    pub ts_sec: u64,
    /// Gas used (parsed from `result.gasUsed`). Useful for pool
    /// congestion heuristics; 0 if absent.
    pub gas_used: u64,
    /// Reserved for future fields (base-fee, miner, etc.).
    _pad: [u8; 40],
}

impl NewHead {
    /// Field-free constructor. `pub(crate)` so the run-loop and tests
    /// can synthesise heads without exposing the private `_pad`.
    #[inline(always)]
    pub(crate) const fn new(number: u64, ts_sec: u64, gas_used: u64) -> Self {
        Self {
            number,
            ts_sec,
            gas_used,
            _pad: [0; 40],
        }
    }

    /// Public test-only constructor. Not compiled in release builds.
    #[cfg(test)]
    #[inline(always)]
    pub const fn new_for_test(number: u64, ts_sec: u64, gas_used: u64) -> Self {
        Self::new(number, ts_sec, gas_used)
    }
}

/// Parse an `eth_subscription` `newHeads` notification.
///
/// Expected shape (trimmed):
/// ```text
/// {"jsonrpc":"2.0","method":"eth_subscription","params":{
///    "subscription":"0x...",
///    "result":{"number":"0x1a","timestamp":"0x65a...","gasUsed":"0x7a12","hash":"0x...", ...}
/// }}
/// ```
#[inline]
pub fn parse_new_head_notification(buf: &[u8]) -> Option<NewHead> {
    let pos = find_field(buf, b"\"number\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (number, _) = parse_hex_u64(buf, pos)?;

    // ts + gasUsed are optional on some providers. We fall back to 0.
    let ts_sec = find_field(buf, b"\"timestamp\":")
        .map(|p| skip_byte(buf, p, b'"'))
        .and_then(|p| parse_hex_u64(buf, p))
        .map(|(v, _)| v)
        .unwrap_or(0);

    let gas_used = find_field(buf, b"\"gasUsed\":")
        .map(|p| skip_byte(buf, p, b'"'))
        .and_then(|p| parse_hex_u64(buf, p))
        .map(|(v, _)| v)
        .unwrap_or(0);

    Some(NewHead::new(number, ts_sec, gas_used))
}

// ---------------------------------------------------------------
// Error parsing (range-only — message stays in the caller's buffer)
// ---------------------------------------------------------------

/// Zero-copy JSON-RPC error. `message_start..message_end` is a byte
/// range into the frame buffer spanning the unescaped `"message"`
/// value — we do **not** copy it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct RpcError {
    /// JSON-RPC error code (negative for server-side errors).
    pub code: i32,
    /// Start of the message string (exclusive of the opening `"`).
    pub message_start: u32,
    /// End of the message string (exclusive of the closing `"`).
    pub message_end: u32,
}

/// Parse `{"jsonrpc":"2.0","id":_,"error":{"code":_,"message":"..."}}`.
/// Returns `None` on malformed input.
#[inline]
pub fn parse_rpc_error(buf: &[u8]) -> Option<RpcError> {
    let pos = find_field(buf, b"\"code\":")?;
    let (code_i, _) = scan_i64(buf, pos)?;
    if code_i < i32::MIN as i64 || code_i > i32::MAX as i64 {
        return None;
    }

    let pos = find_field(buf, b"\"message\":")?;
    let pos = skip_byte(buf, pos, b'"');
    // Walk forward until a non-escaped closing quote.
    let mut i = pos;
    while i < buf.len() {
        if buf[i] == b'\\' {
            // Skip escaped char.
            i += 2;
            continue;
        }
        if buf[i] == b'"' {
            return Some(RpcError {
                code: code_i as i32,
                message_start: pos as u32,
                message_end: i as u32,
            });
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------
// Request serializers (zero-alloc; write into caller's buffer)
// ---------------------------------------------------------------

/// Serialization error. Mirrors the shape used in core-net.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RpcWriteErr {
    /// Destination slice too small.
    BufferTooSmall,
}

/// Write an `eth_blockNumber` request into `dst`. Returns bytes
/// written. Zero-alloc.
///
/// Shape:
/// ```text
/// {"jsonrpc":"2.0","id":<id>,"method":"eth_blockNumber","params":[]}
/// ```
#[inline]
pub fn write_request_eth_block_number(dst: &mut [u8], id: u64) -> Result<usize, RpcWriteErr> {
    const PREFIX: &[u8] = br#"{"jsonrpc":"2.0","id":"#;
    const SUFFIX: &[u8] = br#","method":"eth_blockNumber","params":[]}"#;

    // Worst-case id length: 20 digits (u64::MAX).
    let mut tmp = [0u8; 20];
    let id_len = format_u64(&mut tmp, id);

    let total = PREFIX.len() + id_len + SUFFIX.len();
    if dst.len() < total {
        return Err(RpcWriteErr::BufferTooSmall);
    }

    let mut o = 0usize;
    dst[o..o + PREFIX.len()].copy_from_slice(PREFIX);
    o += PREFIX.len();
    dst[o..o + id_len].copy_from_slice(&tmp[..id_len]);
    o += id_len;
    dst[o..o + SUFFIX.len()].copy_from_slice(SUFFIX);
    o += SUFFIX.len();
    debug_assert_eq!(o, total);
    Ok(total)
}

/// Write an `eth_subscribe(newHeads)` request into `dst`.
///
/// Shape:
/// ```text
/// {"jsonrpc":"2.0","id":<id>,"method":"eth_subscribe","params":["newHeads"]}
/// ```
#[inline]
pub fn write_request_subscribe_new_heads(dst: &mut [u8], id: u64) -> Result<usize, RpcWriteErr> {
    const PREFIX: &[u8] = br#"{"jsonrpc":"2.0","id":"#;
    const SUFFIX: &[u8] = br#","method":"eth_subscribe","params":["newHeads"]}"#;

    let mut tmp = [0u8; 20];
    let id_len = format_u64(&mut tmp, id);

    let total = PREFIX.len() + id_len + SUFFIX.len();
    if dst.len() < total {
        return Err(RpcWriteErr::BufferTooSmall);
    }

    let mut o = 0usize;
    dst[o..o + PREFIX.len()].copy_from_slice(PREFIX);
    o += PREFIX.len();
    dst[o..o + id_len].copy_from_slice(&tmp[..id_len]);
    o += id_len;
    dst[o..o + SUFFIX.len()].copy_from_slice(SUFFIX);
    debug_assert_eq!(o + SUFFIX.len(), total);
    Ok(total)
}

/// Format a u64 into a byte buffer; returns number of bytes written.
/// Zero-alloc. Max output length is 20 bytes.
#[inline]
fn format_u64(buf: &mut [u8], mut v: u64) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    buf[..n].copy_from_slice(&tmp[i..]);
    n
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_monotonic() {
        let r = RequestIds::new();
        assert_eq!(r.allocate(), 1);
        assert_eq!(r.allocate(), 2);
        assert_eq!(r.allocate(), 3);
    }

    // ---- classify_rpc ----

    #[test]
    fn classify_response() {
        let b = br#"{"jsonrpc":"2.0","id":1,"result":"0x10"}"#;
        assert_eq!(classify_rpc(b), RpcFrameKind::Response);
    }

    #[test]
    fn classify_subscription() {
        let b = br#"{"jsonrpc":"2.0","method":"eth_subscription","params":{}}"#;
        assert_eq!(classify_rpc(b), RpcFrameKind::Subscription);
    }

    #[test]
    fn classify_error() {
        let b = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"oops"}}"#;
        assert_eq!(classify_rpc(b), RpcFrameKind::Error);
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(classify_rpc(b"{}"), RpcFrameKind::Unknown);
    }

    // ---- parse_hex_u64 ----

    #[test]
    fn parse_hex_basic() {
        let (v, end) = parse_hex_u64(b"0x1a2b3c", 0).unwrap();
        assert_eq!(v, 0x1A2B3C);
        assert_eq!(end, 8);
    }

    #[test]
    fn parse_hex_uppercase_ok() {
        let (v, _) = parse_hex_u64(b"0xFF", 0).unwrap();
        assert_eq!(v, 0xFF);
    }

    #[test]
    fn parse_hex_rejects_missing_prefix() {
        assert!(parse_hex_u64(b"1a", 0).is_none());
    }

    #[test]
    fn parse_hex_rejects_empty_hex() {
        // "0x" alone — no digits after the prefix.
        assert!(parse_hex_u64(b"0x", 0).is_none());
    }

    #[test]
    fn parse_hex_rejects_overflow() {
        // 17 hex digits overflows u64.
        let b = b"0x10000000000000000";
        assert!(parse_hex_u64(b, 0).is_none());
    }

    // ---- parse_block_number_result ----

    #[test]
    fn parse_block_number_happy() {
        let b = br#"{"jsonrpc":"2.0","id":42,"result":"0x1a2b3c"}"#;
        let (id, block) = parse_block_number_result(b).unwrap();
        assert_eq!(id, 42);
        assert_eq!(block, 0x1A2B3C);
    }

    #[test]
    fn parse_block_number_missing_id() {
        let b = br#"{"jsonrpc":"2.0","result":"0x10"}"#;
        assert!(parse_block_number_result(b).is_none());
    }

    // ---- parse_new_head_notification ----

    #[test]
    fn parse_new_head_happy() {
        let b = br#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xab",
            "result":{"number":"0x1a","timestamp":"0x65a1","gasUsed":"0x7a12","hash":"0xdeadbeef"}}}"#;
        let h = parse_new_head_notification(b).unwrap();
        assert_eq!(h.number, 0x1A);
        assert_eq!(h.ts_sec, 0x65A1);
        assert_eq!(h.gas_used, 0x7A12);
    }

    #[test]
    fn parse_new_head_tolerates_missing_optional_fields() {
        // No timestamp or gasUsed.
        let b = br#"{"params":{"result":{"number":"0x2a","hash":"0xabc"}}}"#;
        let h = parse_new_head_notification(b).unwrap();
        assert_eq!(h.number, 0x2A);
        assert_eq!(h.ts_sec, 0);
        assert_eq!(h.gas_used, 0);
    }

    #[test]
    fn parse_new_head_requires_number() {
        let b = br#"{"params":{"result":{"hash":"0xabc"}}}"#;
        assert!(parse_new_head_notification(b).is_none());
    }

    #[test]
    fn new_head_is_cache_aligned() {
        assert_eq!(::core::mem::align_of::<NewHead>(), 64);
        assert_eq!(::core::mem::size_of::<NewHead>(), 64);
    }

    // ---- parse_rpc_error ----

    #[test]
    fn parse_rpc_error_basic() {
        let b = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"invalid params"}}"#;
        let e = parse_rpc_error(b).unwrap();
        assert_eq!(e.code, -32000);
        let msg = &b[e.message_start as usize..e.message_end as usize];
        assert_eq!(msg, b"invalid params");
    }

    #[test]
    fn parse_rpc_error_handles_escaped_quotes() {
        let b = br#"{"error":{"code":42,"message":"bad \"thing\" happened"}}"#;
        let e = parse_rpc_error(b).unwrap();
        assert_eq!(e.code, 42);
        let msg = &b[e.message_start as usize..e.message_end as usize];
        assert_eq!(msg, br#"bad \"thing\" happened"#);
    }

    #[test]
    fn parse_rpc_error_missing_message() {
        let b = br#"{"error":{"code":1}}"#;
        assert!(parse_rpc_error(b).is_none());
    }

    // ---- Request writers ----

    #[test]
    fn write_eth_block_number_roundtrip() {
        let mut dst = [0u8; 128];
        let n = write_request_eth_block_number(&mut dst, 7).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"jsonrpc":"2.0","id":7,"method":"eth_blockNumber","params":[]}"#
        );
    }

    #[test]
    fn write_eth_block_number_buffer_too_small() {
        let mut dst = [0u8; 10];
        assert_eq!(
            write_request_eth_block_number(&mut dst, 1),
            Err(RpcWriteErr::BufferTooSmall)
        );
    }

    #[test]
    fn write_subscribe_new_heads_roundtrip() {
        let mut dst = [0u8; 128];
        let n = write_request_subscribe_new_heads(&mut dst, 99).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"jsonrpc":"2.0","id":99,"method":"eth_subscribe","params":["newHeads"]}"#
        );
    }

    #[test]
    fn format_u64_zero() {
        let mut buf = [0u8; 20];
        let n = format_u64(&mut buf, 0);
        assert_eq!(&buf[..n], b"0");
    }

    #[test]
    fn format_u64_max() {
        let mut buf = [0u8; 20];
        let n = format_u64(&mut buf, u64::MAX);
        assert_eq!(&buf[..n], b"18446744073709551615");
    }
}

// ---------------------------------------------------------------
// Property tests — random-but-well-shaped frames roundtrip cleanly
// and malformed bytes never panic the parsers.
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn block_number_response_roundtrips(id in 0u64..1_000_000u64, block in 0u64..1_000_000_000u64) {
            let mut buf = String::with_capacity(128);
            use std::fmt::Write;
            write!(&mut buf, r#"{{"jsonrpc":"2.0","id":{id},"result":"0x{block:x}"}}"#).unwrap();
            let (gid, gblock) = parse_block_number_result(buf.as_bytes()).unwrap();
            prop_assert_eq!(gid, id);
            prop_assert_eq!(gblock, block);
        }

        #[test]
        fn new_head_notification_roundtrips(
            number in 0u64..1_000_000_000u64,
            ts in 0u64..2_000_000_000u64,
        ) {
            let mut buf = String::with_capacity(256);
            use std::fmt::Write;
            write!(
                &mut buf,
                r#"{{"params":{{"result":{{"number":"0x{number:x}","timestamp":"0x{ts:x}"}}}}}}"#,
            ).unwrap();
            let h = parse_new_head_notification(buf.as_bytes()).unwrap();
            prop_assert_eq!(h.number, number);
            prop_assert_eq!(h.ts_sec, ts);
        }

        #[test]
        fn arbitrary_bytes_dont_panic_classifier(buf in proptest::collection::vec(any::<u8>(), 0..=200)) {
            let _ = classify_rpc(&buf);
            let _ = parse_hex_u64(&buf, 0);
            let _ = parse_block_number_result(&buf);
            let _ = parse_new_head_notification(&buf);
            let _ = parse_rpc_error(&buf);
        }

        #[test]
        fn eth_block_number_request_contains_id(id in 0u64..u64::MAX) {
            let mut dst = [0u8; 128];
            let n = write_request_eth_block_number(&mut dst, id).unwrap();
            let frame = &dst[..n];
            // Frame contains `"id":<id>,"`
            let mut expected = String::new();
            use std::fmt::Write;
            write!(&mut expected, r#""id":{id},"#).unwrap();
            prop_assert!(memchr::memmem::find(frame, expected.as_bytes()).is_some());
        }
    }
}
