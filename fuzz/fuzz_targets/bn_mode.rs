// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → the BX-19 boot assertions in
//! `exec_binance::mode` (one-way mode, single-asset margin, a key that
//! trades futures and cannot withdraw, the account mode) — and BX7's: the
//! key in Portfolio Margin, a row's margin type and leverage
//! (`judge_symbol_config`) — and Portfolio Margin's unified account
//! (`exec_binance::recon::scan_pm_account`); BX8: the key judged for each
//! product set armed (futures, spot, both); BX9: and the options key's —
//! each answer judged whole, and arming options only adding a law.
//!
//! They must never panic; each judges fail closed — an answer it cannot
//! read is a refusal, never a pass by default; an account it reads has a
//! known status; and only a refusal (not a 200) reads a classic account,
//! only a 200 a Portfolio Margin one (BX7: K17's code read whole).

#![no_main]

use exec_binance::mode::{
    judge_key, judge_key_futures, judge_mode, judge_one_way, judge_single_asset, judge_symbol_config, AccountMode,
};
use exec_binance::recon::{scan_pm_account, PM_LOCKED};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = judge_one_way(data);
    let _ = judge_single_asset(data);
    let _ = judge_key_futures(data);
    // BX8: the key per product set armed, in each mode it is judged in.
    // BX9 (the parser review, round 2): the answer judged is the object
    // WHOLE — a byte after it refuses what passed — and arming options only
    // adds a law (a key that passes with options passes without them).
    let mut more = data.to_vec();
    more.push(b'x');
    for (futures, spot, options) in [(true, false, false), (false, true, false), (true, true, false), (false, false, true), (true, true, true)] {
        for mode in [AccountMode::Classic, AccountMode::Pm] {
            if judge_key(data, mode, futures, spot, options).is_ok() {
                assert!(judge_key(&more, mode, futures, spot, options).is_err(), "a byte after the key's answer");
                if options {
                    assert!(judge_key(data, mode, futures, spot, false).is_ok(), "options' law dropped another");
                }
            }
        }
    }
    if judge_one_way(data).is_ok() {
        assert!(judge_one_way(&more).is_err(), "a byte after the mode's answer");
    }
    let _ = judge_symbol_config(data, b"BTCUSDT", 20);
    if let Ok(s) = scan_pm_account(data) {
        assert!(s.status <= PM_LOCKED, "an account status no read writes");
    }
    if data.len() >= 2 {
        let status = u16::from_le_bytes([data[0], data[1]]) % 600;
        for want in [AccountMode::Classic, AccountMode::Pm, AccountMode::PmPro] {
            if judge_mode(status, &data[2..], want).is_ok() {
                assert_eq!(want == AccountMode::Classic, status != 200, "a mode read off the wrong status");
            }
        }
    }
    // Fail closed: no field at all is never a pass.
    assert!(judge_one_way(b"{}").is_err() && judge_single_asset(b"{}").is_err() && judge_key_futures(b"{}").is_err());
    assert!(judge_symbol_config(b"[]", b"BTCUSDT", 20).is_err() && scan_pm_account(b"{}").is_err());
    assert!(judge_key(b"{}", AccountMode::Classic, false, true, false).is_err(), "a spot key with no fields passed");
    assert!(judge_key(b"{}", AccountMode::Classic, false, false, true).is_err(), "an options key with no fields passed");
});
