// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → `exec_binance::wsapi::scan_answer`
//! — every WS API answer the gateway reads on the order socket.
//!
//! It must never panic; an answer it accepts names spans inside the frame,
//! and a refusal status (≥ 400) it accepts always carries a code (a
//! refusal that does not say why is not a shape the gateway may act on).

#![no_main]

use exec_binance::wsapi::{scan_answer, WsAnswer};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut a = WsAnswer::default();
    if scan_answer(data, &mut a).is_ok() {
        let r = a.result.at as usize + a.result.len as usize;
        let c = a.cid.at as usize + a.cid.len as usize;
        assert!(r <= data.len() && c <= data.len(), "a span past the frame");
        assert!(a.shutdown || a.status < 400 || a.code != 0, "a refusal without a code");
    }
});
