// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes and an arbitrary result span →
//! `exec_binance::recon::scan_um_account` — the `v2/account.status` scan
//! every reconciliation and every margin sample reads.
//!
//! It must never panic, for any span (in range or not); an account it
//! accepts wrote no more positions than it was given room for, and every
//! position's symbol lies inside the frame.

#![no_main]

use exec_binance::recon::{scan_um_account, PosRow};
use exec_binance::userstream::Span;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    // The first four bytes pick the span; the rest is the frame.
    let (head, frame) = data.split_at(4);
    let at = u16::from_le_bytes([head[0], head[1]]) as u32;
    let len = u16::from_le_bytes([head[2], head[3]]) as u32;
    let mut pos = [PosRow::default(); 8];
    if let Ok(snap) = scan_um_account(frame, Span { at, len }, &mut pos) {
        assert!(snap.n_pos <= pos.len(), "more positions than room");
        for p in &pos[..snap.n_pos] {
            assert!(p.symbol.at as usize + p.symbol.len as usize <= frame.len(), "a span past the frame");
        }
    }
});
