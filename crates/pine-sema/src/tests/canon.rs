// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! impl-02 §4.11.5: `sub_key` differs by every field it hashes.

use crate::canon::sub_key;

#[test]
fn security_keys_differ_by_every_field() {
    let k = sub_key("binance_usdm:ethusdt", 3600, false, false, 7);
    assert_ne!(k, sub_key("binance_usdm:btcusdt", 3600, false, false, 7));
    assert_ne!(k, sub_key("binance_usdm:ethusdt", 900, false, false, 7));
    assert_ne!(k, sub_key("binance_usdm:ethusdt", 3600, true, false, 7));
    assert_ne!(k, sub_key("binance_usdm:ethusdt", 3600, false, true, 7));
    assert_ne!(k, sub_key("binance_usdm:ethusdt", 3600, false, false, 8));
    assert_eq!(k, sub_key("binance_usdm:ethusdt", 3600, false, false, 7));
}
