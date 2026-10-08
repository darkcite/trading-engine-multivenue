// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → the BX-19 boot assertions in
//! `exec_binance::mode` (one-way mode, single-asset margin, a key that
//! trades futures and cannot withdraw, the account mode) — and BX7's: the
//! key in Portfolio Margin, a row's margin type and leverage
//! (`judge_symbol_config`) — and Portfolio Margin's unified account
//! (`exec_binance::recon::scan_pm_account`).
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
    let _ = judge_key(data, AccountMode::Pm);
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
});
