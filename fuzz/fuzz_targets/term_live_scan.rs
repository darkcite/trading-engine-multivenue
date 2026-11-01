// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! TERM live feeds: the HL and Binance USDⓈ-M scanners never panic on any
//! bytes, and every level or print they accept is positive (T-L8).
#![no_main]
use libfuzzer_sys::fuzz_target;
use terminal::live::scan;

fuzz_target!(|data: &[u8]| {
    if let Some(scan::Hl::L2 { levels, .. }) = scan::hl(data) {
        let _ = scan::hl_levels(levels, |_, p, q| assert!(p > 0 && q >= 0));
    }
    if let Some(scan::Hl::Trades { data: d }) = scan::hl(data) {
        for t in scan::items(d) {
            if let Some((_, _, p, q, _)) = scan::hl_trade(t) {
                assert!(p > 0 && q > 0);
            }
        }
    }
    match scan::bn(data) {
        Some(scan::Bn::Depth { bids, asks, .. }) => {
            let _ = scan::bn_pairs(bids, |p, q| assert!(p > 0 && q >= 0));
            let _ = scan::bn_pairs(asks, |p, q| assert!(p > 0 && q >= 0));
        }
        Some(scan::Bn::Agg { px, qty, .. }) => assert!(px > 0 && qty > 0),
        _ => {}
    }
});
