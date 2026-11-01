// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The server side of RFC 6455 for one loopback client. Reads reuse
//! `core-net`'s frame reader; writes are unmasked (a server must not mask,
//! and `core-net`'s writers are the client's, which always do).
//! Client frames must be masked, final, and binary or control: a text frame,
//! a fragment or anything over one term-proto frame closes the connection.

use core_net::ws_frame::{ws_read_frame, ws_unmask_in_place, WsOpcode, WsReadResult};

pub const MAX_IN: usize = term_proto::MAX_FRAME;

/// The 101 answer for a validated upgrade.
pub fn accept_response(key: &[u8], out: &mut Vec<u8>) -> bool {
    let Ok(k) = <&[u8; 24]>::try_from(key) else { return false };
    let accept = core_net::ws_handshake::expected_accept(k);
    out.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ");
    out.extend_from_slice(&accept);
    out.extend_from_slice(b"\r\n\r\n");
    true
}

/// Append one unmasked server frame.
pub fn write_frame(op: u8, payload: &[u8], out: &mut Vec<u8>) {
    out.push(0x80 | op);
    let n = payload.len();
    if n < 126 {
        out.push(n as u8);
    } else if n <= 0xffff {
        out.push(126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
}

pub fn write_binary(payload: &[u8], out: &mut Vec<u8>) {
    write_frame(0x2, payload, out)
}

#[derive(Debug, PartialEq, Eq)]
pub enum In {
    /// Need more bytes.
    More,
    /// A binary message at `buf[start..end]` (unmasked in place); consume
    /// `used` bytes after handling it.
    Binary { start: usize, end: usize, used: usize },
    Ping { start: usize, end: usize, used: usize },
    /// Pong or anything else harmless: consume and ignore.
    Skip { used: usize },
    Close,
    /// Protocol violation: close.
    Bad,
}

/// Read one client frame from the front of `buf`, unmasking in place.
pub fn read(buf: &mut [u8]) -> In {
    match ws_read_frame(buf) {
        WsReadResult::Incomplete => In::More,
        WsReadResult::Malformed => In::Bad,
        WsReadResult::Frame { header, payload } => {
            if !header.masked || !header.fin || payload.len() > MAX_IN {
                return In::Bad;
            }
            let used = payload.end;
            ws_unmask_in_place(&mut buf[payload.start..payload.end], header.mask);
            match header.opcode {
                WsOpcode::Binary => In::Binary { start: payload.start, end: payload.end, used },
                WsOpcode::Ping => In::Ping { start: payload.start, end: payload.end, used },
                WsOpcode::Pong => In::Skip { used },
                WsOpcode::Close => In::Close,
                _ => In::Bad, // text and continuation frames are refused
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_frame(op: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
        let mut f = vec![0x80 | op];
        if payload.len() < 126 {
            f.push(0x80 | payload.len() as u8);
        } else {
            f.push(0x80 | 126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        f
    }

    #[test]
    fn reads_masked_binary() {
        let mut f = client_frame(2, b"hello term", [1, 2, 3, 4]);
        let n = f.len();
        match read(&mut f) {
            In::Binary { start, end, used } => {
                assert_eq!(&f[start..end], b"hello term");
                assert_eq!(used, n);
            }
            x => panic!("{x:?}"),
        }
    }

    #[test]
    fn refuses_unmasked_text_and_fragments() {
        let mut text = client_frame(1, b"x", [9, 9, 9, 9]);
        assert_eq!(read(&mut text), In::Bad);
        let mut unmasked = vec![0x82, 1, b'x'];
        assert_eq!(read(&mut unmasked), In::Bad);
        let mut frag = client_frame(2, b"x", [1, 1, 1, 1]);
        frag[0] &= 0x7f;
        assert_eq!(read(&mut frag), In::Bad);
    }

    #[test]
    fn rfc_accept_example() {
        let mut out = Vec::new();
        assert!(accept_response(b"dGhlIHNhbXBsZSBub25jZQ==", &mut out));
        assert!(out.windows(28).any(|w| w == b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    }
}
