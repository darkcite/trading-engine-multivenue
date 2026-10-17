// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM B'1 T5 (impl-Bp §7.1): the pine slot's Trade route — masks (I6)
//! and the `6006` refusal (I7); the `run` boot's route resolution and the
//! spawn masks it feeds (T5); the USDⓈ-M route sockets (T8).

use cli::trade_route::{
    boot_routes, resolve, route_bit, trade_capture_of, TradeCapture, TradeRoutes,
};
use cli::{bn_book_driver, bn_usdm_route_slot, bn_usdm_specs, funding_lane_mask, hl_lane_mask};
use core_config::universe::{AllocatedUniverse, Instrument};
use core_types::{
    event_lane_bit, ChannelId, SymbolId, VenueId, EVENT_LANE_ASSET_CTX, EVENT_LANE_FUNDING,
    EVENT_LANE_TRADE,
};
use engine::{tick_lane_of, NUM_EVENT_LANES};

/// A one-script manifest (contract §3.14 keys) charting `chart`, with
/// `extra` as its `[script.symbols]` targets.
fn manifest(chart: &str, extra: &[&str]) -> core_config::pine::PineManifest {
    let mut s = format!(
        "[[script]]\nsha256 = \"{sha}\"\nfile = \"s.pine\"\ndialect = 6\nenabled = 1\n\
         mode = \"bar_close\"\nsymbol = \"{chart}\"\ntimeframe_s = 3600\n\
         venue = \"hyperliquid\"\nmax_bars_back = 5000\ncoalesce_ms = 250\n\
         coalesce_bps = 0\ninsn_budget = 2000000\nqty_notional_1e6 = 12000000\n\
         limit_primitive = \"ioc\"\ncross_ticks = 2\ncap_instance_1e6 = 12000000\n\
         cap_day_1e6 = 48000000\nmax_open = 4\ncollection_cap = 4096\n\
         str_persist_bytes = 4096\nstr_cell_bytes = 128\nstr_scratch_bytes = 8192\n\
         intent_cap = 32\n",
        sha = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
    );
    if !extra.is_empty() {
        s.push_str("  [script.symbols]\n");
        for (i, k) in extra.iter().enumerate() {
            s.push_str(&format!("  \"T{i}\" = \"{k}\"\n"));
        }
    }
    core_config::pine::parse_manifest(&s).expect("fixture manifest parses")
}

fn inst(sym: SymbolId, name: &str, desc: &str) -> Instrument {
    Instrument {
        sym,
        name: name.to_string(),
        descriptor: desc.to_string(),
    }
}

/// HL `BTC` (5) and a HIP-4 leg `#1230` (6); USDⓈ-M `btcusdt` (9) and
/// `ethusdt` (10); spot `btcusdt` (12).
fn universe() -> AllocatedUniverse {
    AllocatedUniverse {
        hl: vec![
            inst(5, "BTC", "hyperliquid:BTC"),
            inst(6, "#1230", "hyperliquid:#1230"),
        ],
        bn_usdm: vec![
            inst(9, "btcusdt", "binance-usdm:btcusdt"),
            inst(10, "ethusdt", "binance-usdm:ethusdt"),
        ],
        bn_spot: vec![inst(12, "btcusdt", "binance:btcusdt")],
        ..AllocatedUniverse::default()
    }
}

#[test]
fn run_boot_without_pine_spawns_every_head_mask() {
    // T5 acceptance: a boot without the pine slot resolves the EMPTY
    // route and every spawn's mask is the head's (the literals below are
    // the pre-B'1 `paper.rs` masks).
    let r = boot_routes(None, &universe()).unwrap();
    assert_eq!(r.lanes, TradeRoutes::EMPTY.lanes);
    for v in [
        VenueId::Binance,
        VenueId::Okx,
        VenueId::Deribit,
        VenueId::Bybit,
    ] {
        assert_eq!(funding_lane_mask(r.of(v)), EVENT_LANE_FUNDING, "{v:?}");
    }
    let hl = r.of(VenueId::Hyperliquid);
    let fa = EVENT_LANE_FUNDING | EVENT_LANE_ASSET_CTX;
    let roll = event_lane_bit(ChannelId::InstrumentRoll);
    let mark = event_lane_bit(ChannelId::Mark);
    assert_eq!(hl_lane_mask(false, false, hl), fa);
    assert_eq!(hl_lane_mask(false, true, hl), fa | mark);
    assert_eq!(hl_lane_mask(true, false, hl), fa | roll | mark);
    assert_eq!(hl_lane_mask(true, true, hl), fa | roll | mark);
    // The multi-connection Binance lane: the caller's head mask (BX6:
    // the MARK only when an arm reads it) | the route bit.
    let bn = r.of(VenueId::Binance);
    for head in [EVENT_LANE_FUNDING, EVENT_LANE_FUNDING | mark] {
        assert_eq!(head | route_bit(bn), head);
    }
}

#[test]
fn run_boot_routes_the_manifest_symbols_onto_their_lanes() {
    // T5 (I6 at the boot): R1 over the manifest → the route; the trade
    // bit joins exactly the lanes hosting a route symbol.
    let m = manifest("hyperliquid:btc", &["binance_usdm:btcusdt"]);
    let r = boot_routes(Some(&m), &universe()).unwrap();
    assert_eq!(r.of(VenueId::Hyperliquid), &[5]);
    assert_eq!(r.of(VenueId::Binance), &[9]);
    assert_eq!(
        hl_lane_mask(false, false, r.of(VenueId::Hyperliquid)),
        EVENT_LANE_FUNDING | EVENT_LANE_ASSET_CTX | EVENT_LANE_TRADE
    );
    assert_eq!(
        funding_lane_mask(r.of(VenueId::Binance)),
        EVENT_LANE_FUNDING | EVENT_LANE_TRADE
    );
    for v in [VenueId::Okx, VenueId::Deribit, VenueId::Bybit] {
        assert_eq!(funding_lane_mask(r.of(v)), EVENT_LANE_FUNDING, "{v:?}");
    }
}

#[test]
fn run_boot_refuses_a_route_symbol_without_trade_capture_with_6006() {
    // T5 (I7 at the boot): a HIP-4 leg on the route refuses, naming it.
    let m = manifest("hyperliquid:btc", &["hyperliquid:#1230"]);
    let e = boot_routes(Some(&m), &universe()).unwrap_err();
    assert_eq!(e.code.0, 6006);
    assert_eq!(e.descriptor, "hyperliquid:#1230");
    assert_eq!(e.why, TradeCapture::NoSubscription);
    // A key the universe lacks: R1's bare 6006, named by the boot
    // (pin:Bp-r1-unresolved-6006).
    let m = manifest("hyperliquid:btc", &["binance_usdm:solusdt"]);
    let e = boot_routes(Some(&m), &universe()).unwrap_err();
    assert_eq!(e.code.0, 6006);
    assert_eq!(e.descriptor, "binance_usdm:solusdt");
}

#[test]
fn usdm_route_symbol_socket_carries_its_trades_and_no_other_socket_changes() {
    // T8 (Q1 = (a)): only the route symbol's USDⓈ-M bookTicker slot
    // becomes a trade slot; its path and every other slot (the mark
    // slots, the non-route symbol, spot) are built exactly as before.
    let r = resolve(&[(9, "binance-usdm:btcusdt")]).unwrap();
    let route = r.of(VenueId::Binance);
    let [mut b9, m9] = bn_usdm_specs("fstream.binance.com", "btcusdt", 9);
    let [mut b10, m10] = bn_usdm_specs("fstream.binance.com", "ethusdt", 10);
    let [head9, _] = bn_usdm_specs("fstream.binance.com", "btcusdt", 9);
    bn_usdm_route_slot(&mut b9, route);
    bn_usdm_route_slot(&mut b10, route);
    assert!(b9.usdm_trades && !b10.usdm_trades);
    assert!(!m9.usdm_trades && !m10.usdm_trades);
    assert_eq!(
        (b9.host.as_str(), b9.path.as_str()),
        (head9.host.as_str(), head9.path.as_str())
    );
    let d9 = bn_book_driver(&b9, 1);
    assert!(d9.has_usdm_trades() && !d9.has_sentinel());
    let d10 = bn_book_driver(&b10, 1);
    assert!(!d10.has_usdm_trades() && !d10.has_sentinel());
    // The empty route (no pine) marks nothing.
    let [mut e9, _] = bn_usdm_specs("fstream.binance.com", "btcusdt", 9);
    bn_usdm_route_slot(&mut e9, TradeRoutes::EMPTY.of(VenueId::Binance));
    assert!(!e9.usdm_trades && !bn_book_driver(&e9, 1).has_usdm_trades());
    // A spot slot stays the VT2 sentinel, never a USDⓈ-M trade slot.
    let spot = cli::BinanceConnSpec {
        host: "stream.binance.com".to_string(),
        path: "/ws/btcusdt@bookTicker".to_string(),
        sym: 12,
        eapi: None,
        mark_price: false,
        spot_sentinel: true,
        usdm_trades: false,
    };
    let ds = bn_book_driver(&spot, 1);
    assert!(ds.has_sentinel() && !ds.has_usdm_trades());
}

#[test]
fn empty_route_reproduces_the_head_masks() {
    // Every spawn ORs `mask_bit(lane)` into the mask it builds today:
    // with `EMPTY` that is 0 on every lane, so every mask is unchanged.
    let heads = [
        0u16,
        EVENT_LANE_FUNDING,
        EVENT_LANE_FUNDING | EVENT_LANE_ASSET_CTX,
    ];
    let mut l = 0;
    while l < NUM_EVENT_LANES {
        assert_eq!(TradeRoutes::EMPTY.mask_bit(l), 0);
        for h in heads {
            assert_eq!(h | TradeRoutes::EMPTY.mask_bit(l), h);
        }
        l += 1;
    }
    assert_eq!(resolve(&[]).unwrap().lanes, TradeRoutes::EMPTY.lanes);
}

#[test]
fn trade_bit_is_set_only_on_lanes_hosting_a_route_symbol() {
    let r = resolve(&[
        (5, "hyperliquid:BTC"),
        (9, "okx:BTC-USDT-SWAP"),
        (11, "okx:ETH-USDT"),
    ])
    .unwrap();
    let hl = tick_lane_of(VenueId::Hyperliquid).unwrap();
    let okx = tick_lane_of(VenueId::Okx).unwrap();
    let mut l = 0;
    while l < NUM_EVENT_LANES {
        let want = if l == hl || l == okx {
            EVENT_LANE_TRADE
        } else {
            0
        };
        assert_eq!(r.mask_bit(l), want, "lane {l}");
        l += 1;
    }
    assert_eq!(r.lanes[hl], &[5]);
    assert_eq!(r.lanes[okx], &[9, 11]);
}

#[test]
fn route_symbol_without_trade_capture_refuses_with_6006() {
    // pin:Bp-usdm-q1a: with Q1 = (a) `binance-usdm` captures (T8); the
    // spec's first example therefore now routes, and the refusal cases
    // are Polymarket, a HIP-4 leg and a lane the B'1 route does not push.
    assert!(resolve(&[(3, "binance-usdm:btcusdt")]).is_ok());
    for (desc, why) in [
        ("1234567890", TradeCapture::NoEvent),
        ("hyperliquid:#1230", TradeCapture::NoSubscription),
        ("mexc:BTCUSDT", TradeCapture::NoSubscription),
        (
            "binance-opt:BTC-261225-92000-C",
            TradeCapture::NoSubscription,
        ),
    ] {
        let e = resolve(&[(5, "hyperliquid:BTC"), (7, desc)]).unwrap_err();
        assert_eq!(e.code.0, 6006, "{desc}");
        assert_eq!(e.descriptor, desc);
        assert_eq!(e.why, why, "{desc}");
        assert!(e.to_string().contains("`symbol`"));
    }
}

#[test]
fn trade_capture_table_matches_the_descriptor_namespaces() {
    // Every namespace of `core_config::instrument_class` has a row.
    let rows: [(&str, Option<TradeCapture>); 16] = [
        ("binance:btcusdt", Some(TradeCapture::Yes)),
        ("binance-usdm:btcusdt", Some(TradeCapture::Yes)),
        ("binance-usdm:btcusdt_261225", Some(TradeCapture::Yes)),
        (
            "binance-opt:BTC-261225-92000-C",
            Some(TradeCapture::NoSubscription),
        ),
        (
            "binance-coinm:btcusd_perp",
            Some(TradeCapture::NoSubscription),
        ),
        ("okx:BTC-USDT", Some(TradeCapture::Yes)),
        ("deribit:BTC-PERPETUAL", Some(TradeCapture::Yes)),
        ("hyperliquid:BTC", Some(TradeCapture::Yes)),
        ("hyperliquid:#1230", Some(TradeCapture::NoSubscription)),
        ("bybit:BTCUSDT", Some(TradeCapture::Yes)),
        ("bybit-linear:BTCUSDT", Some(TradeCapture::Yes)),
        ("mexc:BTCUSDT", Some(TradeCapture::NoSubscription)),
        ("mexc-perp:BTC_USDT", Some(TradeCapture::NoSubscription)),
        (
            "hypercall:BTC-20261002-100000-C",
            Some(TradeCapture::NoSubscription),
        ),
        ("hypercall-idx:BTC", Some(TradeCapture::NoSubscription)),
        ("hyperevm:0xabc", None),
    ];
    for (desc, want) in rows {
        assert_eq!(trade_capture_of(desc).map(|(_, c)| c), want, "{desc}");
    }
    assert_eq!(
        trade_capture_of("1234").map(|(_, c)| c),
        Some(TradeCapture::NoEvent)
    );
    assert_eq!(trade_capture_of(""), None);
    assert_eq!(trade_capture_of("okx:"), None);
}
