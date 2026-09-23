// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the whole MEXC WS-frame surface
//! (MX3 + MX4), both connection classes:
//!
//! * spot — `classify_spot` (binary and text), the order-agnostic
//!   `PushDataV3ApiWrapper` walk, both body parsers (bookTicker, and
//!   the deals walk + items) on the walked body AND on the raw input,
//!   `trade_id_seq`, the subscribe-ack parser + its failed-param walker
//!   + the param helpers;
//! * futures — `classify_futures`, `extract_fut_symbol`,
//!   `extract_fut_ts_ms`, `parse_depth_full`, the deal walk + items,
//!   `parse_ticker`;
//! * the Q-MX3 funding clock on input-derived numbers.
//!
//! Every scanner must tolerate any input — returning `None`/`Unknown`
//! on malformed frames and never panicking, looping or reading past
//! the slice.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "common/poison.rs"]
mod poison;

fuzz_target!(|data: &[u8]| {
    // ---- spot (protobuf + the text ack) ----
    let _ = ingress_mexc::classify_spot(data, true);
    let _ = ingress_mexc::classify_spot(data, false);
    if let Some(f) = ingress_mexc::parse_spot_wrapper(data) {
        assert!(!f.symbol(data).is_empty());
        let _ = f.venue_time_ms();
        let body = f.body(data);
        let mut f: ingress_mexc::spot::MexcBookTicker = poison::poisoned();
        if !ingress_mexc::parse_book_ticker_body(body, &mut f) {
            assert_eq!(f, poison::poisoned::<ingress_mexc::spot::MexcBookTicker>(), "a failed parse wrote the frame");
        }
        let mut w = ingress_mexc::MexcDealsWalk::new(body);
        while let Some(item) = w.next_item() {
            let mut f: ingress_mexc::MexcDeal = poison::poisoned();
            if !ingress_mexc::parse_deal_item(item, &mut f) {
                assert_eq!(f, poison::poisoned::<ingress_mexc::MexcDeal>(), "a failed parse wrote the frame");
            }
        }
    }
    let mut f: ingress_mexc::spot::MexcBookTicker = poison::poisoned();
    if !ingress_mexc::parse_book_ticker_body(data, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_mexc::spot::MexcBookTicker>(), "a failed parse wrote the frame");
    }
    let mut w = ingress_mexc::MexcDealsWalk::new(data);
    while let Some(item) = w.next_item() {
        let mut d: ingress_mexc::MexcDeal = poison::poisoned();
        let ok = ingress_mexc::parse_deal_item(item, &mut d);
        if !ok {
            assert_eq!(d, poison::poisoned::<ingress_mexc::MexcDeal>(), "a failed parse wrote the frame");
        }
        if ok {
            let _ = d.signed_qty_1e6();
        }
    }
    let _ = w.is_malformed();
    let mut f: ingress_mexc::MexcDeal = poison::poisoned();
    if !ingress_mexc::parse_deal_item(data, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_mexc::MexcDeal>(), "a failed parse wrote the frame");
    }
    let _ = ingress_mexc::trade_id_seq(data);
    let mut ack: ingress_mexc::spot::MexcSpotAck = poison::poisoned();
    let ok = ingress_mexc::parse_sub_ack(data, &mut ack);
    if !ok {
        assert_eq!(ack, poison::poisoned::<ingress_mexc::spot::MexcSpotAck>(), "a failed parse wrote the frame");
    }
    if ok {
        let _ = ack.has_failures(data);
        let mut p = ack.failed_params(data);
        while let Some(param) = p.next_param() {
            assert!(!param.is_empty());
            let _ = ingress_mexc::extract_param_symbol(param);
            let _ = ingress_mexc::extract_param_channel(param);
        }
    }
    let _ = ingress_mexc::extract_param_symbol(data);
    let _ = ingress_mexc::extract_param_channel(data);

    // ---- futures (JSON) ----
    let _ = ingress_mexc::classify_futures(data);
    let _ = ingress_mexc::extract_fut_symbol(data);
    let _ = ingress_mexc::extract_fut_ts_ms(data);
    let _ = ingress_mexc::extract_refused_contract(data);
    let mut f: ingress_mexc::futures::MexcDepthFrame = poison::poisoned();
    if !ingress_mexc::parse_depth_full(data, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_mexc::futures::MexcDepthFrame>(), "a failed parse wrote the frame");
    }
    let mut fw = ingress_mexc::MexcFutDealsWalk::new(data);
    while let Some(item) = fw.next_item() {
        let mut f: ingress_mexc::MexcDeal = poison::poisoned();
        if !ingress_mexc::parse_fut_deal_item(item, &mut f) {
            assert_eq!(f, poison::poisoned::<ingress_mexc::MexcDeal>(), "a failed parse wrote the frame");
        }
    }
    let _ = fw.is_malformed();
    let mut f: ingress_mexc::MexcDeal = poison::poisoned();
    if !ingress_mexc::parse_fut_deal_item(data, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_mexc::MexcDeal>(), "a failed parse wrote the frame");
    }
    let mut f: ingress_mexc::futures::MexcTickerFrame = poison::poisoned();
    if !ingress_mexc::parse_ticker(data, &mut f) {
        assert_eq!(f, poison::poisoned::<ingress_mexc::futures::MexcTickerFrame>(), "a failed parse wrote the frame");
    }

    // ---- the funding clock (arithmetic, never a loop) ----
    if let Some(n) = data.get(..24) {
        let word = |k: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&n[k * 8..k * 8 + 8]);
            u64::from_le_bytes(b)
        };
        let (next, cycle, venue) = (word(0), word(1), word(2));
        let out = ingress_mexc::funding_next_settle_ms(next, cycle, venue);
        if next > 0 && cycle > 0 && venue >= next && out != u64::MAX {
            assert!(out > venue);
        }
    }
});
