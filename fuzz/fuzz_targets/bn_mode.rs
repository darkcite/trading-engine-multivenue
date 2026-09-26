// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → the BX-19 boot assertions in
//! `exec_binance::mode` (one-way mode, single-asset margin, a key that
//! trades futures and cannot withdraw, the account mode).
//!
//! They must never panic; each judges fail closed — an answer it cannot
//! read is a refusal, never a pass by default.

#![no_main]

use exec_binance::mode::{judge_key_futures, judge_mode, judge_one_way, judge_single_asset, AccountMode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = judge_one_way(data);
    let _ = judge_single_asset(data);
    let _ = judge_key_futures(data);
    if data.len() >= 2 {
        let status = u16::from_le_bytes([data[0], data[1]]) % 600;
        for want in [AccountMode::Classic, AccountMode::Pm, AccountMode::PmPro] {
            let _ = judge_mode(status, &data[2..], want);
        }
    }
    // Fail closed: no field at all is never a pass.
    assert!(judge_one_way(b"{}").is_err() && judge_single_asset(b"{}").is_err() && judge_key_futures(b"{}").is_err());
});
