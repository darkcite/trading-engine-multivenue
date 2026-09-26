// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → every `exec_binance::rest` answer
//! scanner — the venue error, the listenKey, the server time, the dead-man's
//! answer, the order arrays (recon) and the trade arrays (the day's spend).
//!
//! None may panic; an array scan either fits the buffer whole or refuses
//! (never truncates), and every span it names lies inside the body.

#![no_main]

use exec_binance::rest::{
    scan_countdown, scan_error, scan_listen_key, scan_orders, scan_server_time, scan_trades,
    ArrErr, OrderRow, TradeRow,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = scan_error(data);
    let _ = scan_server_time(data);
    if let Ok(s) = scan_listen_key(data) {
        assert!(s.at as usize + s.len as usize <= data.len());
    }
    if let Ok(s) = scan_countdown(data) {
        assert!(s.at as usize + s.len as usize <= data.len());
    }
    let mut o = [OrderRow::default(); 16];
    match scan_orders(data, &mut o) {
        Ok(n) => {
            assert!(n <= o.len());
            for r in &o[..n] {
                assert!(r.symbol.at as usize + r.symbol.len as usize <= data.len());
                assert!(r.cid.at as usize + r.cid.len as usize <= data.len());
            }
        }
        Err(ArrErr::Truncated) | Err(ArrErr::Scan(_)) => {}
    }
    let mut t = [TradeRow::default(); 16];
    if let Ok(n) = scan_trades(data, &mut t) {
        assert!(n <= t.len());
    }
});
