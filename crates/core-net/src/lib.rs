//! # core-net
//!
//! Networking primitives shared by ingress adapters and the CLOB
//! dispatcher.
//!
//! Phase 0 shipped the [`FixedBuf`] type.
//!
//! Phase 1a adds the RFC 6455 [`ws_frame`] codec — a zero-alloc,
//! pure-`&[u8]`/`&mut [u8]` parser and serializer that every ingress
//! adapter uses. The mio+rustls event loop that drives it over a real
//! TLS socket lands in Phase 1b.
//!
//! Phase 1b adds the [`ws_handshake`] module — a handwritten RFC 6455
//! §4 opening handshake with inlined SHA-1 and base64. Still zero-alloc,
//! still pure `&[u8]` / `&mut [u8]`.
//!
//! Phase 1c adds the [`http1`] module — a minimal HTTP/1.1 client codec
//! (GET request writer + response parser with `Content-Length` /
//! chunked / close-delimited framing). Used by the RSS poller; not the
//! CLOB dispatcher (that path keeps `hyper` + HTTP/2).

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod backoff;
pub mod error;
pub mod http1;
pub mod iobuf;
pub mod keepalive;
pub mod subs;
pub mod transport;
pub mod ws_frame;
pub mod ws_handshake;

pub use backoff::{Backoff, BACKOFF_BASE_NS, BACKOFF_CAP_NS};
pub use error::{NetworkErr, NetworkErrKind, NetworkSource};
pub use iobuf::IoBuf;
pub use keepalive::{Keepalive, KeepaliveAction, KeepaliveCfg};
pub use subs::{
    queue_masked_binary_frame, queue_masked_text_frame, PendingErr, PendingReq, PendingTable,
    ReqKind, SubErr, SubId, SubTable,
};

pub use http1::{
    dechunk_in_place, read_response, write_get_request, BodyFraming, DechunkResult, HttpErr,
    HttpResult,
};
pub use transport::{
    PlainTcpTransport, Status, TestBuffer, TestTransport, TlsTransport, Transport,
};
pub use ws_frame::{
    ws_mask_from_counter, ws_read_frame, ws_unmask_in_place, ws_write_binary_frame, ws_write_ping,
    ws_write_pong, ws_write_text_frame, PayloadSpan, WsFrameHeader, WsOpcode, WsReadResult,
    WsWriteErr,
};
pub use ws_handshake::{
    constant_time_eq, expected_accept, read_server_handshake, sec_websocket_key_from_seed,
    write_client_handshake, write_client_handshake_with_headers, HandshakeErr, HandshakeResult,
};

/// A fixed-capacity byte buffer with a cursor. The ingress adapters
/// accumulate inbound TCP bytes here and hand `&[u8]` slices of
/// complete frames to their parsers — zero-copy, zero-alloc.
pub struct FixedBuf {
    data: Box<[u8]>,
    len: usize,
}

impl FixedBuf {
    /// Allocate a buffer of exactly `cap` bytes. Single allocation, at
    /// boot. Never grows.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            data: vec![0u8; cap].into_boxed_slice(),
            len: 0,
        }
    }

    /// Bytes currently filled.
    #[inline]
    pub fn filled(&self) -> &[u8] {
        &self.data[..self.len]
    }

    /// Free tail — write directly into this slice, then call `advance`.
    #[inline]
    pub fn free_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.len..]
    }

    /// Mark `n` more bytes as filled after a successful write.
    #[inline]
    pub fn advance(&mut self, n: usize) {
        debug_assert!(self.len + n <= self.data.len());
        self.len += n;
    }

    /// Drop the first `n` bytes (after a complete frame was consumed).
    /// Amortised O(1): uses `copy_within` on the remaining tail.
    pub fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.len);
        let new_len = self.len - n;
        if new_len > 0 {
            self.data.copy_within(n..self.len, 0);
        }
        self.len = new_len;
    }

    /// Total capacity.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_then_consume_cycles_correctly() {
        let mut b = FixedBuf::with_capacity(8);
        b.free_mut()[..3].copy_from_slice(b"abc");
        b.advance(3);
        assert_eq!(b.filled(), b"abc");
        b.consume(2);
        assert_eq!(b.filled(), b"c");
    }

    #[test]
    fn consume_all_empties_buffer() {
        let mut b = FixedBuf::with_capacity(4);
        b.free_mut()[..2].copy_from_slice(b"xy");
        b.advance(2);
        b.consume(2);
        assert_eq!(b.filled(), b"");
    }
}
