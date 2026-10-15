// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX5): arbitrary server bytes, in arbitrary pieces →
//! `core_net::WsFramer::next_frame` — the exec gateway's WebSocket
//! framing.
//!
//! Whatever arrives, the framer never panics, never hands out a payload
//! outside its window, never spins, and queues nothing but whole masked
//! pongs of at most 125 B (a pong is the only frame it writes unasked).
//!
//! Byte 0 picks the piece size (1–64); the rest is the stream.

#![no_main]

use core_net::{WsFramer, WsNext};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&b0, stream)) = data.split_first() else {
        return;
    };
    let piece = usize::from(b0 % 64) + 1;
    let mut f = WsFramer::new(1024, 64 * 1024, u64::from(b0));
    let mut at = 0usize;
    'feed: while at < stream.len() {
        let free = f.rx_free_mut();
        let n = piece.min(stream.len() - at).min(free.len());
        assert!(n > 0, "a full window must have been refused, not left idle");
        free[..n].copy_from_slice(&stream[at..at + n]);
        f.rx_advance(n);
        at += n;
        let mut spins = 0u32;
        loop {
            spins += 1;
            assert!(spins < 100_000, "next_frame spun");
            match f.next_frame() {
                WsNext::Text(s) | WsNext::Binary(s) => {
                    assert!(s.start <= s.end);
                    assert_eq!(f.payload(s).len(), s.end - s.start);
                }
                WsNext::Idle => break,
                WsNext::Failed(_) => break 'feed,
            }
        }
    }
    let tx = f.tx_pending();
    let mut i = 0usize;
    while i < tx.len() {
        assert_eq!(tx[i], 0x8A, "FIN + Pong, nothing else");
        assert!(tx[i + 1] & 0x80 != 0, "masked");
        let len = usize::from(tx[i + 1] & 0x7F);
        assert!(len <= 125);
        i += 2 + 4 + len;
    }
    assert_eq!(i, tx.len(), "whole frames only");
});
