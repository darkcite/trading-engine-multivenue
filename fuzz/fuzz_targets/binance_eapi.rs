// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target: arbitrary bytes → the M2.4 Binance eapi byte
//! scanners — exchangeInfo option rows, the index-price REST body,
//! the combined-stream splitter and the mark-element scanners on a
//! bare input (§21.4: every new byte scanner ships with a fuzz
//! target). The array walk has its own target,
//! `binance_eapi_mark_array` (BX0-F2).
//!
//! None may panic or read out of bounds on any input. On a
//! successful exchangeInfo parse every row upholds the BX2 row laws
//! (a symbol, an underlying and a lifecycle; `trading` iff TRADING;
//! no negative rule) and the capped
//! selection runs too: it must uphold its ≤ E×K×2 law and select no
//! series that is not trading.

#![no_main]

use ingress_binance::discovery::BnStatus;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = ingress_binance::eapi::EapiDiscovery::new();
    if d.ingest_exchange_info(data).is_ok() {
        for r in d.rows() {
            assert!(!r.symbol().is_empty() && !r.underlying().is_empty());
            assert!(r.status != BnStatus::Absent, "a row without a lifecycle parsed");
            assert_eq!(r.trading, r.status == BnStatus::Trading);
            let f = r.filters;
            assert!(f.tick_size_1e9 >= 0 && f.lot_step_1e9 >= 0);
            assert!(f.min_qty_1e9 >= 0 && f.max_qty_1e9 >= 0);
            assert!(f.min_notional_1e9 >= 0);
            assert!(f.bid_up_1e9 >= 0 && f.bid_down_1e9 >= 0);
            assert!(f.ask_up_1e9 >= 0 && f.ask_down_1e9 >= 0);
        }
        let sel = ingress_binance::eapi::select_capped_chain(
            d.rows(),
            b"BTCUSDT",
            1_000_000_000,
            2,
            8,
            0,
        );
        assert!(sel.len() as u32 <= 2 * 8 * 2);
        assert!(sel.iter().all(|r| r.trading), "a closed series selected");
    }
    let _ = ingress_binance::eapi::parse_index_price(data);
    let _ = ingress_binance::eapi::split_combined(data);
    let _ = ingress_binance::eapi::eapi_elem_symbol(data);
    let mut f = ingress_binance::eapi::EapiMarkFrame::ZERO;
    if ingress_binance::eapi::parse_eapi_mark(data, &mut f) {
        assert!(f.index_px_1e9 > 0, "a parsed element always carries a positive index");
    }
});
