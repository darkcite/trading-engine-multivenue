// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the Binance exchangeInfo discovery parser
//! (`ingress_binance::discovery::BnDiscovery::ingest_body`) — spot
//! single-symbol, USDⓈ-M and (BX2) COIN-M page bodies share one
//! walker. House rule §21.3/§21.4: REST discovery parses
//! venue-controlled bytes at boot; it must never panic, loop, or
//! misindex on arbitrary input.
//!
//! BX2: on a successful parse every row upholds the row laws — a
//! symbol and a status, `trading` iff TRADING, inverse iff a contract
//! size, no negative rule, and a group count covering its exact groups.

#![no_main]

use ingress_binance::discovery::{BnDiscovery, BnStatus};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = BnDiscovery::new();
    if let Ok(n) = d.ingest_body(data) {
        assert_eq!(n as usize, d.rows().len());
        let mut trading = 0u32;
        for r in d.rows() {
            assert!(!r.symbol().is_empty());
            assert_ne!(r.status, BnStatus::Absent);
            assert_eq!(r.trading, r.status == BnStatus::Trading);
            assert_eq!(r.is_inverse(), r.contract_size > 0);
            let f = r.filters;
            for v in [
                f.tick_size_1e9,
                f.lot_step_1e9,
                f.min_qty_1e9,
                f.max_qty_1e9,
                f.min_notional_1e9,
                f.bid_up_1e9,
                f.bid_down_1e9,
                f.ask_up_1e9,
                f.ask_down_1e9,
            ] {
                assert!(v >= 0, "a rule parsed negative");
            }
            let exact: u32 = r.perm.groups.iter().map(|w| w.count_ones()).sum();
            assert!(exact <= u32::from(r.perm.group_count));
            trading += u32::from(r.trading);
        }
        assert_eq!(trading, d.universe_trading());
    }
});
