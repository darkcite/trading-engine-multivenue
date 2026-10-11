// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (BX6): arbitrary bytes → `exec_binance::userstream::
//! scan_user_event` — every user-data frame, the source of every fill.
//!
//! It must never panic (BX-15: a frame it cannot read is a refusal the
//! gateway halts on, never a crash); an event it accepts names spans inside
//! the frame, and an order or trade event it accepts carries its symbol.
//! BX7: every product's and mode's events (dstream, `/pm/ws/` with `fs`,
//! `riskLevelChange`, `ACCOUNT_CONFIG_UPDATE` as bits); `ACCOUNT_UPDATE`'s
//! positions not cross are named by the walk (`NotCross`) — exactly as many
//! as the scan counted, in frame order, the first being the event's own
//! span, the count exact (never saturated); and an accepted whole raw frame,
//! wrapped in the combined form, scans alike (spans re-based) and walks
//! alike. BX11: the commission's asset (`N`) is a span inside the frame,
//! named — with a commission — only by an order update (a `TRADE_LITE`'s
//! reads empty), and re-based in the combined form like every span.
//! BX12: an order update's `N`, whatever its bytes (the raw span: the
//! scanner unescapes nothing), is judged by `journal::fee_asset` against
//! five rows — USDⓈ-M's BTCUSDT, BTCUSDC and a dated BTCUSDT, COIN-M's
//! BTCUSD_PERP and BNBUSD_PERP — as an independent oracle reads it.
//! BX8 (the parser review's P2-1): an accepted order or trade event whose
//! execution is a trade (`TRADE`, `CALCULATED`) is a trade of something at
//! a price — its last quantity and price above zero.
//! BX9: an option row's commission asset (its settle asset, USDT — the
//! symbol names no quote) judged the same way; and options' event name
//! for the risk level, `RISK_LEVEL_CHANGE`, scans as `riskLevelChange`
//! does (an accepted frame, renamed, reads the same level and figures).

#![no_main]

use std::sync::OnceLock;

use exec_binance::inst::{BindSpec, InstTable, OptBindSpec, WireTable, PRODUCT_COINM, PRODUCT_USDM};
use exec_binance::journal::{fee_asset, FEE_ASSET_BNB, FEE_ASSET_NONE, FEE_ASSET_OTHER, FEE_ASSET_SETTLE};
use exec_binance::userstream::{
    scan_user_event, NotCross, Span, UserEvent, CONFIG_LEVERAGE, CONFIG_MULTI_ASSETS_OFF, CONFIG_MULTI_ASSETS_ON,
    ISOLATED_MAX, RISK_MARGIN_CALL, RISK_NORMAL, UE_ACCOUNT, UE_CONFIG, UE_ORDER, UE_RISK, UE_TRADE_LITE, X_CALCULATED,
    X_TRADE,
};
use ingress_binance::discovery::BnDiscovery;
use libfuzzer_sys::fuzz_target;

/// The combined-stream wrapper's head: the event starts at its end.
const WRAP: &[u8] = br#"{"stream":"s","data":"#;

/// BX12: the rows a commission's asset is judged against — USDⓈ-M's
/// BTCUSDT, BTCUSDC and dated BTCUSDT (settling in USDT, USDC, USDT),
/// COIN-M's BTCUSD_PERP and BNBUSD_PERP (in BTC, in BNB: the settlement
/// asset before BNB).
const FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]},{"symbol":"BTCUSDC","pair":"BTCUSDC","contractType":"PERPETUAL","status":"TRADING","marginAsset":"USDC","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]},{"symbol":"BTCUSDT_261225","pair":"BTCUSDT","contractType":"CURRENT_QUARTER","status":"TRADING","marginAsset":"USDT","filters":[{"filterType":"PRICE_FILTER","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"}]}]}"#;
const DAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSD_PERP","pair":"BTCUSD","contractType":"PERPETUAL","contractStatus":"TRADING","marginAsset":"BTC","contractSize":100,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.1"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000000"}]},{"symbol":"BNBUSD_PERP","pair":"BNBUSD","contractType":"PERPETUAL","contractStatus":"TRADING","marginAsset":"BNB","contractSize":10,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.001"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000000"}]}]}"#;

/// BX9: an option row (BTC's call): its commission settles in USDT.
const EAPI: &[u8] = br#"{"optionContracts":[{"baseAsset":"BTC","quoteAsset":"USDT","underlying":"BTCUSDT","settleAsset":"USDT","nakedSell":true}],"optionAssets":[{"name":"USDT"}],"optionSymbols":[{"expiryDate":1798185600000,"filters":[{"filterType":"PRICE_FILTER","minPrice":"5.000","maxPrice":"23300.000","tickSize":"5.000"},{"filterType":"LOT_SIZE","minQty":"0.0001","maxQty":"200","stepSize":"0.0100"}],"symbol":"BTC-261225-85000-C","side":"CALL","strikePrice":"85000.000","underlying":"BTCUSDT","unit":1,"priceScale":3,"quantityScale":4,"quoteAsset":"USDT","status":"TRADING","contractType":"CRYPTO_OPTIONS","underlyingType":"CRYPTO"}]}"#;

/// Those rows, bound once.
fn rows() -> &'static WireTable {
    static ROWS: OnceLock<WireTable> = OnceLock::new();
    ROWS.get_or_init(|| {
        let (mut fapi, mut dapi) = (BnDiscovery::new(), BnDiscovery::new());
        fapi.ingest_body(FAPI).expect("the fapi fixture");
        dapi.ingest_body(DAPI).expect("the dapi fixture");
        let (mut t, mut w) = InstTable::new(7);
        let named: [(&[u8], u8); 5] = [
            (b"BTCUSDT", PRODUCT_USDM),
            (b"BTCUSDC", PRODUCT_USDM),
            (b"BTCUSDT_261225", PRODUCT_USDM),
            (b"BTCUSD_PERP", PRODUCT_COINM),
            (b"BNBUSD_PERP", PRODUCT_COINM),
        ];
        let mut i = 0;
        while i < named.len() {
            let (name, product) = named[i];
            let d = if product == PRODUCT_USDM { &fapi } else { &dapi };
            let row = d.find(name).expect("a fixture row");
            let sym = core_types::make_symbol_id(core_types::VenueId::Binance, 700 + i as u32);
            t.bind(&mut w, &BindSpec { sym, product, row, owned: true, maker_ok: true }).expect("bound");
            i += 1;
        }
        let mut e = ingress_binance::eapi::EapiDiscovery::new();
        e.ingest_exchange_info(EAPI).expect("the eapi fixture");
        let sym = core_types::make_symbol_id(core_types::VenueId::Binance, 1_025);
        t.bind_option(&mut w, &OptBindSpec { sym, row: &e.rows()[0], owned: true, writable: true }).expect("the option bound");
        w
    })
}

/// The walk's spans, at most 64.
fn walk(frame: &[u8], e: &UserEvent) -> ([Span; 64], usize) {
    let mut out = [Span::default(); 64];
    let mut n = 0;
    let mut w = NotCross::new(frame, e);
    while let Some(s) = w.next() {
        if n < out.len() {
            out[n] = s;
        }
        n += 1;
    }
    (out, n)
}

fuzz_target!(|data: &[u8]| {
    let mut e = UserEvent::default();
    if scan_user_event(data, &mut e).is_err() {
        return;
    }
    assert!((UE_ORDER..=UE_CONFIG).contains(&e.kind), "a kind no scan writes");
    assert!(matches!(e.fs, 0 | PRODUCT_USDM | PRODUCT_COINM | u8::MAX), "an fs no scan writes");
    for s in [e.symbol, e.cid, e.fee_asset] {
        assert!(s.at as usize + s.len as usize <= data.len(), "a span past the frame");
    }
    if e.kind != UE_ORDER {
        assert!(e.fee_asset.len == 0 && e.commission_1e9 == 0, "a commission on no order update");
    } else {
        // BX12: the update's commission asset (`N`) — the raw span, the
        // scanner unescapes nothing — judged against each row; the oracle:
        // the row's settlement asset whole, else BNB, else another; none
        // when absent.
        let n = e.fee_asset.get(data);
        let want = |settle: &[u8]| {
            if n.is_empty() {
                FEE_ASSET_NONE
            } else if n == settle {
                FEE_ASSET_SETTLE
            } else if n == b"BNB" {
                FEE_ASSET_BNB
            } else {
                FEE_ASSET_OTHER
            }
        };
        let w = rows();
        assert_eq!(fee_asset(w.row(0), n), want(b"USDT"), "a perpetual's asset");
        assert_eq!(fee_asset(w.row(1), n), want(b"USDC"), "a USDC row's asset");
        assert_eq!(fee_asset(w.row(2), n), want(b"USDT"), "a dated row's asset");
        assert_eq!(fee_asset(w.row(3), n), want(b"BTC"), "a COIN-M row's coin");
        assert_eq!(fee_asset(w.row(4), n), want(b"BNB"), "BNB as a COIN-M row's own coin");
        assert_eq!(fee_asset(w.row(5), n), want(b"USDT"), "an option's settle asset (BX9)");
    }
    if e.kind == UE_ORDER || e.kind == UE_TRADE_LITE {
        assert!(!e.symbol.get(data).is_empty(), "an order event with no symbol");
        if e.exec == X_TRADE || e.exec == X_CALCULATED {
            assert!(e.last_qty_1e6 > 0 && e.last_px_1e6 > 0, "a trade of nothing or at no price");
        }
    }
    if e.kind == UE_RISK {
        assert!((RISK_MARGIN_CALL..=RISK_NORMAL).contains(&e.risk), "a risk word no scan writes");
        // BX9: options' name for the event reads alike.
        const PM: &[u8] = b"\"riskLevelChange\"";
        const OPT: &[u8] = b"\"RISK_LEVEL_CHANGE\"";
        let at: Vec<usize> = data.windows(PM.len()).enumerate().filter(|(_, w)| *w == PM).map(|(i, _)| i).collect();
        if at.len() == 1 {
            let mut r = Vec::with_capacity(data.len() + 2);
            r.extend_from_slice(&data[..at[0]]);
            r.extend_from_slice(OPT);
            r.extend_from_slice(&data[at[0] + PM.len()..]);
            // Unconditional (the parser review, round 2): the renamed bytes
            // are no key the scanner reads, so a correct one reads the
            // renamed frame as the same risk event, whatever the name.
            let mut o = UserEvent::default();
            assert!(scan_user_event(&r, &mut o).is_ok() && o.kind == UE_RISK, "options' name read otherwise");
            assert_eq!((o.risk, o.qty_1e6, o.px_1e6), (e.risk, e.qty_1e6, e.px_1e6), "the two names read alike");
        }
    } else {
        assert_eq!(e.risk, 0);
    }
    if e.kind == UE_CONFIG {
        let all = CONFIG_LEVERAGE | CONFIG_MULTI_ASSETS_ON | CONFIG_MULTI_ASSETS_OFF;
        assert!(e.config != 0 && e.config & !all == 0, "config bits no scan writes");
        assert!(e.config & (CONFIG_MULTI_ASSETS_ON | CONFIG_MULTI_ASSETS_OFF) != (CONFIG_MULTI_ASSETS_ON | CONFIG_MULTI_ASSETS_OFF));
        if e.config & CONFIG_LEVERAGE != 0 {
            assert!(e.symbol.len > 0, "a leverage change with no symbol");
        }
    } else {
        assert_eq!(e.config, 0);
    }
    let (spans, n) = walk(data, &e);
    if e.kind == UE_ACCOUNT {
        assert!(e.isolated < ISOLATED_MAX, "a saturated count");
        assert_eq!(n, usize::from(e.isolated), "the walk names what the scan counted");
        let mut k = 0;
        while k < n.min(spans.len()) {
            let s = spans[k];
            assert!(s.at as usize + s.len as usize <= data.len(), "a span past the frame");
            assert!(k == 0 || s.at > spans[k - 1].at, "positions out of frame order");
            k += 1;
        }
        if n > 0 {
            assert_eq!(spans[0], e.symbol, "the event's symbol is the walk's first (the same span)");
        }
    } else {
        assert_eq!((e.isolated, e.positions, n), (0, 0, 0), "only an account update walks");
    }
    // Raw ⇔ combined: an accepted whole raw frame, wrapped, scans alike
    // (spans re-based) and walks alike.
    let whole = data.first() == Some(&b'{') && core_parse::skip_json_value(data, 0) == Some(data.len());
    if whole && !data.windows(6).any(|w| w == b"\"data\"") {
        let mut w = Vec::with_capacity(data.len() + WRAP.len() + 1);
        w.extend_from_slice(WRAP);
        w.extend_from_slice(data);
        w.push(b'}');
        let mut c = UserEvent::default();
        scan_user_event(&w, &mut c).expect("the combined form of an accepted frame");
        let off = WRAP.len() as u32;
        let mut want = e;
        want.symbol.at += off;
        want.cid.at += off;
        want.fee_asset.at += off;
        if want.positions != 0 {
            want.positions += off;
        }
        assert_eq!(c, want, "raw and combined disagree");
        let (cs, cn) = walk(&w, &c);
        assert_eq!(cn, n, "the walks disagree");
        let mut k = 0;
        while k < n.min(spans.len()) {
            assert_eq!(cs[k].at, spans[k].at + off, "the walks disagree");
            k += 1;
        }
    }
});
