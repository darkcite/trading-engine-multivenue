// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the Bybit WS-frame surface (WS9):
//! `classify`, `extract_topic_symbol`, `parse_orderbook1`,
//! `parse_trade_row`, `parse_tickers`.
//!
//! Every scanner is expected to tolerate any input — returning
//! `None`/`Unknown` on malformed frames and never panicking or
//! reading past the end of the slice.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/poison.rs"]
mod poison;

fuzz_target!(|data: &[u8]| {
    let _ = ingress_bybit::classify(data);
    let _ = ingress_bybit::extract_topic_symbol(data, ingress_bybit::BybitChannel::OrderbookL1);
    let _ = ingress_bybit::extract_topic_symbol(data, ingress_bybit::BybitChannel::PublicTrade);
    let _ = ingress_bybit::extract_topic_symbol(data, ingress_bybit::BybitChannel::Tickers);
    let mut f: ingress_bybit::BybitBookFrame = poison::poisoned();
    if !ingress_bybit::parse_orderbook1(data, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_bybit::BybitBookFrame>(), "a failed parse wrote the frame");
    }
    let mut f: ingress_bybit::BybitTradeFrame = poison::poisoned();
    if !ingress_bybit::parse_trade_row(data, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_bybit::BybitTradeFrame>(), "a failed parse wrote the frame");
    }
    let mut f: ingress_bybit::BybitTickerFrame = poison::poisoned();
    if !ingress_bybit::parse_tickers(data, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_bybit::BybitTickerFrame>(), "a failed parse wrote the frame");
    }
});
