// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes and an arbitrary result span →
//! `exec_binance::recon::scan_um_account` — the `v2/account.status` scan
//! every reconciliation and every margin sample reads — and (BX7) COIN-M's
//! (`scan_cm_account`) and Portfolio Margin's positions (`scan_positions`).
//!
//! None may panic, for any span (in range or not); an account it accepts
//! wrote no more positions than it was given room for, every position's
//! symbol lies inside the frame, and its margin type is a known word — and
//! a COIN-M account's every position carries its margin type and a
//! leverage of at least 1 (BX7: BX-19 judges them on every read).

#![no_main]

use exec_binance::recon::{scan_cm_account, scan_positions, scan_um_account, PosRow, MARGIN_ISOLATED, MARGIN_UNKNOWN};
use exec_binance::userstream::Span;
use libfuzzer_sys::fuzz_target;

fn check(frame: &[u8], pos: &[PosRow], n: usize) {
    assert!(n <= pos.len(), "more positions than room");
    for p in &pos[..n] {
        assert!(p.symbol.at as usize + p.symbol.len as usize <= frame.len(), "a span past the frame");
        assert!(p.margin <= MARGIN_ISOLATED, "a margin type no read writes");
    }
}

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
        check(frame, &pos, snap.n_pos);
    }
    let mut pos = [PosRow::default(); 8];
    if let Ok(snap) = scan_cm_account(frame, Span { at, len }, &mut pos) {
        check(frame, &pos, snap.n_pos);
        assert!(snap.ratio_1e6 >= 0, "a negative ratio");
        for p in &pos[..snap.n_pos] {
            assert!(p.margin != MARGIN_UNKNOWN && p.leverage >= 1, "a COIN-M position short of its BX-19 fields");
        }
    }
    let mut pos = [PosRow::default(); 8];
    if let Ok(n) = scan_positions(frame, Span { at, len }, &mut pos) {
        check(frame, &pos, n);
    }
});
