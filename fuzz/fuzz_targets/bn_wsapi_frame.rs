// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → `exec_binance::wsapi::scan_answer`
//! — every WS API answer the gateway reads on the order socket.
//!
//! It must never panic; an answer it accepts names spans inside the frame,
//! and any status but a 200 it accepts carries a code (BX7: an answer that
//! does not say why — a refusal, a 204, a 3xx — is not a shape the gateway
//! may act on; it would retire an order the venue may hold).

#![no_main]

use exec_binance::wsapi::{scan_answer, WsAnswer};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut a = WsAnswer::default();
    if scan_answer(data, &mut a).is_ok() {
        let r = a.result.at as usize + a.result.len as usize;
        let c = a.cid.at as usize + a.cid.len as usize;
        assert!(r <= data.len() && c <= data.len(), "a span past the frame");
        assert!(a.shutdown || a.status == 200 || a.code != 0, "a non-200 without a code");
    }
});
