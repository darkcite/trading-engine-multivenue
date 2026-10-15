// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Parser, table and render tests over the GOLDEN frames in
//! `tests/fixtures/` — captured live 2026-09-25 (quotes, index, clock,
//! acks, the slow-consumer close reason); the `Trade` and `MarketUpdate`
//! bodies are the docs / asyncapi-schema shapes (no such frame arrived
//! in any probe window) and are named so.
//!
//! COPY-DOCTRINE: test-only module (declared under `#[cfg(test)]` in
//! lib.rs); every copy here assembles or mutates a fixture frame.

use super::*;

const Q1: &[u8] = include_bytes!("../tests/fixtures/quote_one_provider.json");
const Q2X: &[u8] = include_bytes!("../tests/fixtures/quote_two_providers_crossed.json");
const Q0: &[u8] = include_bytes!("../tests/fixtures/quote_empty.json");
const QSNAP: &[u8] = include_bytes!("../tests/fixtures/quote_mac_snapshot.json");
const IDX: &[u8] = include_bytes!("../tests/fixtures/index_update.json");
const CLK: &[u8] = include_bytes!("../tests/fixtures/clock_synced.json");
const SUBD: &[u8] = include_bytes!("../tests/fixtures/subscribed.json");
const TRADE: &[u8] = include_bytes!("../tests/fixtures/trade_docs_example.json");
const MU_EXPIRED: &[u8] = include_bytes!("../tests/fixtures/market_update_expired_schema.json");
const CLOSE_ML: &[u8] = include_bytes!("../tests/fixtures/close_reason_message_limit.json");

#[test]
fn every_golden_frame_classifies() {
    assert_eq!(classify(Q1), HcMsg::Indicative);
    assert_eq!(classify(Q2X), HcMsg::Indicative);
    assert_eq!(classify(Q0), HcMsg::Indicative);
    assert_eq!(classify(IDX), HcMsg::IndexPrice);
    assert_eq!(classify(CLK), HcMsg::ClockSynced);
    assert_eq!(classify(SUBD), HcMsg::Subscribed);
    assert_eq!(classify(TRADE), HcMsg::Trade);
    assert_eq!(classify(MU_EXPIRED), HcMsg::MarketUpdate);
    assert_eq!(classify(br#"{"type":"Error","message":"bad symbol"}"#), HcMsg::Error);
    assert_eq!(classify(br#"{"type":"OrderbookUpdate","symbol":"x"}"#), HcMsg::Other);
    // `type` not first: the object walk finds it.
    assert_eq!(classify(br#"{"channel":"trades", "type" : "Subscribed"}"#), HcMsg::Subscribed);
    assert_eq!(classify(br#"{"channel":"trades"}"#), HcMsg::Malformed);
    assert_eq!(classify(b"{\"type\":\"Trun"), HcMsg::Malformed);
    assert_eq!(classify(b""), HcMsg::Malformed);
}

#[test]
fn a_one_provider_quote_parses_to_the_exact_fixed_point() {
    let mut q = HcQuote::ZERO;
    let meta = parse_indicative(Q1, &mut q).expect("golden");
    assert_eq!(span_bytes(Q1, q.instrument), b"ETH-20260927-2675-P");
    assert_eq!((q.bid_px_1e6, q.ask_px_1e6), (7_679_000, 15_240_500));
    assert_eq!((q.bid_qty_1e6, q.ask_qty_1e6), (63_816_073, 63_816_073));
    assert_eq!((q.published_at_ms, q.timestamp_ms), (1_790_373_478_234, 1_790_373_476_585));
    assert_eq!(meta, HcQuoteMeta { sides: SIDE_BID | SIDE_ASK, num_providers: 1 });
    let mut ps = [HcProvider::default(); HC_MAX_PROVIDERS];
    assert_eq!(walk_providers(Q1, q.providers, &mut ps), Some((1, 1)));
    assert_eq!(ps[0].wallet_lo32, 0xf9a5_e86e);
    assert_eq!((ps[0].bid_px_1e6, ps[0].ask_px_1e6), (7_679_000, 15_240_500));
    assert_eq!(ps[0].updated_at_ms, 1_790_373_476_585);
}

#[test]
fn a_crossed_two_provider_quote_is_parsed_as_published() {
    let mut q = HcQuote::ZERO;
    let meta = parse_indicative(Q2X, &mut q).expect("golden");
    assert_eq!(span_bytes(Q2X, q.instrument), b"MU-20260928-1090-C");
    // best_bid (provider A's bid) ABOVE best_ask (provider B's ask):
    // the venue's cross-provider BBO, crossed. "12.067400000000001"
    // truncates at 1e-6.
    assert_eq!((q.bid_px_1e6, q.ask_px_1e6), (18_644_500, 12_067_400));
    assert!(q.bid_px_1e6 > q.ask_px_1e6);
    assert_eq!((q.bid_qty_1e6, q.ask_qty_1e6), (100_379_100, 100_000_000));
    assert_eq!(meta.num_providers, 2);
    let mut ps = [HcProvider::default(); HC_MAX_PROVIDERS];
    assert_eq!(walk_providers(Q2X, q.providers, &mut ps), Some((2, 2)));
    assert_eq!(ps[0].wallet_lo32, 0xf9a5_e86e);
    assert_eq!((ps[0].bid_px_1e6, ps[0].ask_px_1e6), (18_644_500, 21_097_500));
    assert_eq!(ps[1].wallet_lo32, 0x6cb3_94cd);
    assert_eq!((ps[1].bid_px_1e6, ps[1].ask_px_1e6), (10_402_900, 12_067_400));
    assert_eq!((ps[1].bid_qty_1e6, ps[1].ask_qty_1e6), (100_000_000, 100_000_000));
    assert_eq!(ps[1].updated_at_ms, 1_790_373_486_436);
}

#[test]
fn an_empty_quote_has_no_side_and_no_providers() {
    let mut q = HcQuote::ZERO;
    let meta = parse_indicative(Q0, &mut q).expect("golden");
    assert_eq!(meta, HcQuoteMeta { sides: 0, num_providers: 0 });
    assert_eq!((q.bid_px_1e6, q.bid_qty_1e6, q.ask_px_1e6, q.ask_qty_1e6), (0, 0, 0, 0));
    let mut ps = [HcProvider::default(); HC_MAX_PROVIDERS];
    assert_eq!(walk_providers(Q0, q.providers, &mut ps), Some((0, 0)));
    // The Mac snapshot frame: published 2 s after its quote was made.
    let meta = parse_indicative(QSNAP, &mut q).expect("golden");
    assert_eq!(meta.sides, SIDE_BID | SIDE_ASK);
    assert_eq!(q.published_at_ms - q.timestamp_ms, 1_962);
}

#[test]
fn quote_shapes_the_venue_may_send_are_legal_and_broken_ones_are_not() {
    let mut q = HcQuote::ZERO;
    // One-sided (best_bid null): the missing side is 0 / 0, even when a
    // stray size is present.
    let one = br#"{"type":"IndicativeMarketData","instrument":"X-20261002-1-C","best_bid":null,"best_ask":"1.5","indicative_bid_size":"3","indicative_ask_size":"2","num_providers":1,"rfq_provider_quotes":null,"published_at":2,"timestamp":1}"#;
    let meta = parse_indicative(one, &mut q).expect("one-sided");
    assert_eq!(meta.sides, SIDE_ASK);
    assert_eq!((q.bid_px_1e6, q.bid_qty_1e6, q.ask_px_1e6, q.ask_qty_1e6), (0, 0, 1_500_000, 2_000_000));
    assert_eq!(q.providers, (0, 0), "a null provider list is no list");
    // Member order is irrelevant; whitespace, unknown members, IV doubles.
    let shuffled = br#"{ "timestamp" : 7 , "bid_iv": 0.61, "num_providers":1, "published_at":9,
        "best_ask":"2", "instrument":"Y-20261002-2.5-P", "best_bid":"1.25", "type":"IndicativeMarketData",
        "ask_iv":null }"#;
    let meta = parse_indicative(shuffled, &mut q).expect("shuffled");
    assert_eq!(span_bytes(shuffled, q.instrument), b"Y-20261002-2.5-P");
    assert_eq!((q.bid_px_1e6, q.ask_px_1e6, q.timestamp_ms), (1_250_000, 2_000_000, 7));
    assert_eq!(meta.sides, SIDE_BID | SIDE_ASK);
    // Missing required members, broken values.
    for bad in [
        &br#"{"type":"IndicativeMarketData","instrument":"X","num_providers":1,"published_at":2}"#[..],
        br#"{"type":"IndicativeMarketData","num_providers":1,"published_at":2,"timestamp":1}"#,
        br#"{"type":"IndicativeMarketData","instrument":"","num_providers":1,"published_at":2,"timestamp":1}"#,
        br#"{"type":"IndicativeMarketData","instrument":"X","best_bid":"1.5x","num_providers":1,"published_at":2,"timestamp":1}"#,
        br#"{"type":"IndicativeMarketData","instrument":"X","best_bid":nul,"num_providers":1,"published_at":2,"timestamp":1}"#,
        br#"{"type":"IndicativeMarketData","instrument":"X","num_providers":1,"published_at":2,"timestamp":1"#,
        b"[]",
    ] {
        assert!(parse_indicative(bad, &mut q).is_none(), "{}", String::from_utf8_lossy(bad));
    }
}

#[test]
fn a_provider_walk_bounds_its_reads_and_refuses_a_malformed_entry() {
    // Nine providers: eight read, nine present.
    let mut s = String::from("{\"rfq_provider_quotes\":[");
    for i in 0..9 {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"wallet\":\"0x00000000000000000000000000000000000000{i:02x}\",\"bid_price\":\"1\",\"ask_price\":\"2\",\"max_bid_size\":\"3\",\"max_ask_size\":\"4\",\"updated_at\":{i}}}"
        ));
    }
    s.push_str("]}");
    let b = s.as_bytes();
    let start = memchr::memchr(b'[', b).unwrap();
    let end = b.len() - 1;
    let mut ps = [HcProvider::default(); HC_MAX_PROVIDERS];
    assert_eq!(walk_providers(b, (start as u32, end as u32), &mut ps), Some((8, 9)));
    assert_eq!(ps[7].wallet_lo32, 7);
    let bad = br#"[{"wallet":"nothex","updated_at":1}]"#;
    assert_eq!(walk_providers(bad, (0, bad.len() as u32), &mut ps), None);
    let no_stamp = br#"[{"wallet":"0x0000000000000000000000000000000000000001"}]"#;
    assert_eq!(walk_providers(no_stamp, (0, no_stamp.len() as u32), &mut ps), None);
    assert_eq!(provider_quote_seq(1, true, 2, 0x6cb3_94cd), 0x6cb3_94cd_0002_0101);
}

#[test]
fn the_index_frame_yields_every_underlying() {
    let mut xs = [HcIndexEntry::default(); HC_MAX_UNDERLYINGS];
    let (read, present, ts) = parse_index_update(IDX, &mut xs).expect("golden");
    assert_eq!((read, present), (12, 12));
    assert_eq!(ts, 1_790_373_479_066);
    assert_eq!(span_bytes(IDX, xs[0].underlying), b"AAPL");
    assert_eq!(xs[0].price_1e6, 341_150_000);
    assert_eq!(span_bytes(IDX, xs[3].underlying), b"BTC");
    assert_eq!(xs[3].price_1e6, 83_839_000_000, "integer strings too");
    assert_eq!(span_bytes(IDX, xs[10].underlying), b"SP500");
    assert_eq!(xs[10].ts_ms, 1_790_373_479_066);
    assert!(parse_index_update(br#"{"timestamp":1}"#, &mut xs).is_none(), "no prices");
    assert!(
        parse_index_update(br#"{"prices":[{"underlying":"A","timestamp":1}]}"#, &mut xs).is_none(),
        "an entry without a price"
    );
}

#[test]
fn trade_listing_clock_error_and_close_parse() {
    let t = parse_trade(TRADE).expect("docs example");
    assert_eq!(span_bytes(TRADE, t.symbol), b"BTC-20261002-100000-C");
    assert_eq!((t.px_1e6, t.signed_qty_1e6, t.ts_ms), (52_300, 5_000_000, 1_737_331_200_000));
    let sold = br#"{"type":"Trade","symbol":"S","price":"1","size":"2.5","side":"sell","timestamp":3}"#;
    assert_eq!(parse_trade(sold).unwrap().signed_qty_1e6, -2_500_000, "negated when the aggressor sold");
    let no_side = br#"{"type":"Trade","symbol":"S","price":"1","size":"2","side":"hold","timestamp":3}"#;
    assert!(parse_trade(no_side).is_none(), "an unknown direction is never signed");

    let (action, sym, ts) = parse_market_update(MU_EXPIRED).expect("schema shape");
    assert_eq!(action, HcListingAction::Expired);
    assert_eq!(span_bytes(MU_EXPIRED, sym), b"MU-20260928-1090-C");
    assert_eq!(ts, 1_790_539_200_123);

    assert_eq!(parse_clock_synced(CLK), Some((123, 1_790_373_478_201)));
    assert_eq!(parse_clock_synced(br#"{"type":"ClockSynced","nonce":"probe-1","server_at":5}"#), None);

    let e = br#"{"type":"Error","message":"unknown channel"}"#;
    assert_eq!(span_bytes(e, parse_error(e).unwrap()), b"unknown channel");

    assert_eq!(parse_close_reason(CLOSE_ML), HcCloseCause::MessageLimit);
    assert_eq!(
        parse_close_reason(br#"{"error":"slow_consumer","cause":"byte_limit"}"#),
        HcCloseCause::ByteLimit
    );
    assert_eq!(
        parse_close_reason(br#"{"error":"slow_consumer","cause":"new_cause"}"#),
        HcCloseCause::SlowOther
    );
    assert_eq!(parse_close_reason(b"pong timeout"), HcCloseCause::Other);
    assert_eq!(parse_close_reason(b""), HcCloseCause::Other);
}

#[test]
fn the_symbol_table_hashes_probes_and_refuses() {
    let mut t = HcSymbolTable::new();
    assert_eq!(t.insert(b"BTC-20261002-100000-C", 11), Ok(0));
    assert_eq!(t.insert(b"BTC-20261002-100000-P", 12), Ok(1));
    assert_eq!(t.lookup(b"BTC-20261002-100000-P"), Some((1, 12)));
    assert_eq!(t.lookup(b"BTC-20261002-100000"), None);
    assert_eq!(t.insert(b"BTC-20261002-100000-C", 13), Err(SymbolTableErr::Duplicate));
    assert_eq!(t.insert(b"", 1), Err(SymbolTableErr::Empty));
    assert_eq!(t.insert(&[b'a'; HC_SYMBOL_MAX + 1], 1), Err(SymbolTableErr::TooLong));
    // Fill to capacity: every name still resolves (probe chains), the
    // next insert is refused.
    let mut i = 2u32;
    while (i as usize) < HC_MAX_INSTRUMENTS {
        let name = format!("SNDK-20261002-{i}-C");
        assert_eq!(t.insert(name.as_bytes(), 100 + i), Ok(i as usize));
        i += 1;
    }
    assert_eq!(t.insert(b"ONE-MORE-1-C", 1), Err(SymbolTableErr::Full));
    let mut i = 2u32;
    while (i as usize) < HC_MAX_INSTRUMENTS {
        let name = format!("SNDK-20261002-{i}-C");
        assert_eq!(t.lookup(name.as_bytes()), Some((i as usize, 100 + i)), "{name}");
        i += 1;
    }
    assert_eq!(t.get(0), Some((&b"BTC-20261002-100000-C"[..], 11)));
    assert_eq!(t.get(HC_MAX_INSTRUMENTS), None);

    let mut u = HcUnderlyings::new();
    assert_eq!(u.insert(b"SP500", 7), Ok(0));
    assert_eq!(u.lookup(b"SP500"), Some((0, 7)));
    assert_eq!(u.lookup(b"SP50"), None);
    assert_eq!(u.insert(b"SP500", 8), Err(SymbolTableErr::Duplicate));
    assert_eq!(u.insert(b"THIRTEENCHARS", 8), Err(SymbolTableErr::TooLong));
}

fn concat(parts: &[&[u8]]) -> String {
    let mut s = Vec::new();
    for p in parts {
        s.extend_from_slice(p);
    }
    String::from_utf8(s).unwrap()
}

#[test]
fn the_subscribe_law_is_one_frame_per_channel() {
    let mut t = HcSymbolTable::new();
    t.insert(b"BTC-20261002-100000-C", 1).unwrap();
    t.insert(b"BOT-20260925-2.5-P", 2).unwrap();
    let mut parts: [&[u8]; SUBSCRIBE_PARTS_MAX] = [&[]; SUBSCRIBE_PARTS_MAX];
    let n = subscribe_parts(CH_INDICATIVE, Some(&t), &mut parts).unwrap();
    let json = concat(&parts[..n]);
    assert_eq!(
        json,
        r#"{"type":"Subscribe","channel":"indicative_market_data","symbols":["BTC-20261002-100000-C","BOT-20260925-2.5-P"]}"#
    );
    let n = subscribe_parts(CH_INDEX, None, &mut parts).unwrap();
    assert_eq!(concat(&parts[..n]), r#"{"type":"Subscribe","channel":"index_prices"}"#);
    // An empty filter would subscribe to nothing, silently: refused.
    let empty = HcSymbolTable::new();
    assert_eq!(subscribe_parts(CH_INDICATIVE, Some(&empty), &mut parts), None);
    // A full table fits the parts cap.
    let mut full = HcSymbolTable::new();
    let mut i = 0;
    while i < HC_MAX_INSTRUMENTS {
        full.insert(format!("X-{i}").as_bytes(), i as u32).unwrap();
        i += 1;
    }
    assert!(subscribe_parts(CH_INDICATIVE, Some(&full), &mut parts).is_some());
    let mut short: [&[u8]; 3] = [&[]; 3];
    assert_eq!(subscribe_parts(CH_INDICATIVE, Some(&t), &mut short), None);

    let mut d = [0u8; 20];
    assert_eq!(
        concat(&clock_sync_parts(fmt_u64(1_790_373_478_201, &mut d))),
        r#"{"type":"ClockSync","nonce":"1790373478201"}"#
    );
    assert_eq!(fmt_u64(0, &mut d), b"0");
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Every parser is total over arbitrary bytes: no panic, no
        /// out-of-bounds, whatever the venue (or an attacker) sends.
        #[test]
        fn parsers_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut q = HcQuote::ZERO;
            let _ = classify(&bytes);
            if parse_indicative(&bytes, &mut q).is_some() {
                let mut ps = [HcProvider::default(); HC_MAX_PROVIDERS];
                let _ = walk_providers(&bytes, q.providers, &mut ps);
            }
            let mut xs = [HcIndexEntry::default(); HC_MAX_UNDERLYINGS];
            let _ = parse_index_update(&bytes, &mut xs);
            let _ = parse_trade(&bytes);
            let _ = parse_market_update(&bytes);
            let _ = parse_clock_synced(&bytes);
            let _ = parse_error(&bytes);
            let _ = parse_close_reason(&bytes);
            let _ = crate::rest::parse_summary_rows(&bytes, |_| {});
        }

        /// A mutated golden frame never panics either (the mutations a
        /// byte-level fuzzer finds first: truncation and flips).
        #[test]
        fn mutated_golden_quotes_never_panic(cut in 0usize..700, flip in 0usize..700, to in any::<u8>()) {
            let mut b = Q2X.to_vec();
            if flip < b.len() {
                b[flip] = to;
            }
            b.truncate(cut.min(b.len()));
            let mut q = HcQuote::ZERO;
            if parse_indicative(&b, &mut q).is_some() {
                let mut ps = [HcProvider::default(); HC_MAX_PROVIDERS];
                let _ = walk_providers(&b, q.providers, &mut ps);
            }
        }

        /// Generated well-formed quotes parse to exactly their values.
        #[test]
        fn generated_quotes_roundtrip(
            bid in 0u32..10_000_000, ask in 0u32..10_000_000,
            bq in 0u32..1_000_000, aq in 0u32..1_000_000,
            ts in 1u64..u64::from(u32::MAX), lag in 0u64..10_000,
            np in 0u8..4,
        ) {
            let f = format!(
                "{{\"type\":\"IndicativeMarketData\",\"instrument\":\"BTC-20261002-100000-C\",\"best_bid\":\"{}.{:06}\",\"best_ask\":\"{}.{:06}\",\"indicative_bid_size\":\"{}.{:06}\",\"indicative_ask_size\":\"{}.{:06}\",\"num_providers\":{np},\"rfq_provider_quotes\":[],\"published_at\":{},\"timestamp\":{ts}}}",
                bid / 1_000_000, bid % 1_000_000, ask / 1_000_000, ask % 1_000_000,
                bq / 1_000_000, bq % 1_000_000, aq / 1_000_000, aq % 1_000_000, ts + lag,
            );
            let mut q = HcQuote::ZERO;
            let meta = parse_indicative(f.as_bytes(), &mut q).expect("well-formed");
            prop_assert_eq!(q.bid_px_1e6, i64::from(bid));
            prop_assert_eq!(q.ask_px_1e6, i64::from(ask));
            prop_assert_eq!(q.bid_qty_1e6, i64::from(bq));
            prop_assert_eq!(q.ask_qty_1e6, i64::from(aq));
            prop_assert_eq!(q.timestamp_ms, ts);
            prop_assert_eq!(q.published_at_ms, ts + lag);
            prop_assert_eq!(meta.num_providers, np);
            prop_assert_eq!(meta.sides, SIDE_BID | SIDE_ASK);
        }
    }
}
