// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → `exec_binance::userstream::
//! scan_user_event` — every user-data frame, the source of every fill.
//!
//! It must never panic (BX-15: a frame it cannot read is a refusal the
//! gateway halts on, never a crash); an event it accepts names spans inside
//! the frame, and an order or trade event it accepts carries its symbol and
//! client id.

#![no_main]

use exec_binance::userstream::{scan_user_event, UserEvent, UE_ORDER, UE_TRADE_LITE};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut e = UserEvent::default();
    if scan_user_event(data, &mut e).is_ok() {
        for s in [e.symbol, e.cid] {
            assert!(s.at as usize + s.len as usize <= data.len(), "a span past the frame");
        }
        if e.kind == UE_ORDER || e.kind == UE_TRADE_LITE {
            // An order event names its symbol: an empty one is refused.
            assert!(e.symbol.len > 0, "an order event with no symbol");
            assert!(!e.symbol.get(data).is_empty());
        }
    }
});
