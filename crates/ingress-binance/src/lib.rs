// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-binance
//!
//! Binance WebSocket ingress (spot + USDS-M futures).
//!
//! Phase 0 shipped the `TradeFrame` type and a `parse_trade` byte
//! scanner over the `aggTrade` stream.
//!
//! Phase 1a adds [`BookTickerFrame`] + [`parse_book_ticker`] over the
//! `@bookTicker` stream — the cheap top-of-book feed we actually want
//! for latency-arb. Both parsers are zero-alloc byte scanners; no
//! `serde_json`.
//!
//! Phase 1c adds [`run_loop`] — an event-driven mio+rustls run-loop
//! that drives [`parse_book_ticker`] against a
//! [`core_net::Transport`]. Steady state is zero-alloc; no tokio, no
//! `dyn Trait`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod discovery;
pub mod eapi;
pub mod run_loop;

pub use run_loop::{
    drive_one, note_transport_ready, run, run_multi, Driver, MultiConn, RunResult, State, StopFlag,
    DEFAULT_TICK_RING_CAP, RX_BUF_SIZE, TX_BUF_SIZE,
};

use core_parse::{find_field, scan_price_1e6, scan_price_1e9, scan_u64, skip_byte};
use core_types::{NsTs, SymbolId};

/// A parsed Binance `aggTrade` — the VT2 spot staleness SENTINEL (its
/// `T` teaches the connection's clock offset) and, since VT2, a
/// captured `ChannelId::Trade` event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TradeFrame {
    /// Symbol id resolved from the "s" field at boot.
    pub sym: SymbolId,
    /// Price scaled by 1e6.
    pub price_1e6: i64,
    /// Qty scaled by 1e6.
    pub qty_1e6: i64,
    /// Trade time (`"T"` field, millis → nanos).
    pub ts_ns: NsTs,
    /// Trade time in raw venue milliseconds (`"T"`) — the sentinel
    /// stamp.
    pub ts_ms: u64,
    /// Aggregate trade id (`"a"`; 0 when absent) — the capture's
    /// `venue_seq`.
    pub agg_id: u64,
    /// `"m":true` ⇒ the buyer was the maker ⇒ the aggressor SOLD (the
    /// capture negates the qty, the OKX/Deribit/Bybit/HL convention).
    pub is_buyer_maker: bool,
}

/// Parse a Binance `aggTrade` frame into a `TradeFrame`. Returns
/// `None` on malformed input.
pub fn parse_trade(buf: &[u8], sym: SymbolId) -> Option<TradeFrame> {
    // Price: "p":"65432.10"; qty: "q":"0.05".
    let price_1e6 = quoted_field_1e6(buf, b"\"p\":")?;
    let qty_1e6 = quoted_field_1e6(buf, b"\"q\":")?;

    // Trade time: "T":1713000000000
    let ts_ms = bare_field_u64(buf, b"\"T\":")?;

    // Aggregate id: "a":26129 (optional — 0 when absent).
    let agg_id = bare_field_u64(buf, b"\"a\":").unwrap_or(0);
    let is_buyer_maker = memchr::memmem::find(buf, b"\"m\":true").is_some();

    Some(TradeFrame {
        sym,
        price_1e6,
        qty_1e6,
        ts_ns: ts_ms.saturating_mul(1_000_000),
        ts_ms,
        agg_id,
        is_buyer_maker,
    })
}

/// A bare (unquoted) integer field located by `key` (`"u":400900217`).
#[inline(always)]
fn bare_field_u64(buf: &[u8], key: &[u8]) -> Option<u64> {
    let pos = find_field(buf, key)?;
    let (v, _) = scan_u64(buf, pos)?;
    Some(v)
}

/// A quoted decimal field located by `key` (`"b":"65000.01"`), ×1e6.
#[inline(always)]
fn quoted_field_1e6(buf: &[u8], key: &[u8]) -> Option<i64> {
    let pos = find_field(buf, key)?;
    let (v, _) = scan_price_1e6(buf, skip_byte(buf, pos, b'"'))?;
    Some(v)
}

// ---------------------------------------------------------------
// bookTicker frame
// ---------------------------------------------------------------

/// A parsed Binance `@bookTicker` frame — the cheap top-of-book feed.
/// 64-byte POD; fits one cache line. 8-byte fields come first so the
/// `u32` symbol id sits at the tail and the struct doesn't need
/// internal padding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct BookTickerFrame {
    /// Monotonic update id from Binance (`"u"` field).
    pub update_id: u64,
    /// Best bid price scaled by 1e6.
    pub bid_px_1e6: i64,
    /// Size at best bid scaled by 1e6.
    pub bid_qty_1e6: i64,
    /// Best ask price scaled by 1e6.
    pub ask_px_1e6: i64,
    /// Size at best ask scaled by 1e6.
    pub ask_qty_1e6: i64,
    /// Resolved symbol id (`"s"` field mapped at boot).
    pub sym: SymbolId,
    /// Explicit padding (keeps `venue_time_ms` 8-aligned).
    _pad0: [u8; 4],
    /// VT2: venue time of the push in ms — USDS-M `bookTicker` carries
    /// `"T"` (transaction time, preferred) and `"E"` (event time,
    /// fallback); SPOT `bookTicker` carries neither ⇒ 0 ("unknown,
    /// never stale") until the aggTrade sentinel (VT2, last step)
    /// supplies the connection's stamp.
    pub venue_time_ms: u64,
    /// Reserved for layout stability (keeps struct at 64 bytes).
    _pad: [u8; 8],
}

impl BookTickerFrame {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self::new(0, 0, 0, 0, 0, 0, 0);

    /// Named-field-free constructor.
    #[inline(always)]
    const fn new(
        sym: SymbolId,
        update_id: u64,
        bid_px_1e6: i64,
        bid_qty_1e6: i64,
        ask_px_1e6: i64,
        ask_qty_1e6: i64,
        venue_time_ms: u64,
    ) -> Self {
        Self {
            update_id,
            bid_px_1e6,
            bid_qty_1e6,
            ask_px_1e6,
            ask_qty_1e6,
            sym,
            _pad0: [0; 4],
            venue_time_ms,
            _pad: [0; 8],
        }
    }
}

/// VT2: the `bookTicker` venue stamp — `"T"` (transaction time)
/// preferred, `"E"` (event time) as the fallback, 0 when absent (spot).
/// Both are bare integers on the wire; the single-letter keys cannot
/// false-match any other bookTicker field.
#[inline]
fn book_ticker_venue_time_ms(buf: &[u8]) -> u64 {
    if let Some(ms) = bare_field_u64(buf, b"\"T\":") {
        return ms;
    }
    bare_field_u64(buf, b"\"E\":").unwrap_or(0)
}

/// Parse a Binance `@bookTicker` frame INTO `out`. Zero-alloc and
/// zero-copy: the frame is written in place — returned inside an
/// `Option` it would be 128 B by value (a 64 B `align(64)` frame plus
/// its tag), twice the by-value bound. `out` is written only once every
/// field has parsed, so on `false` (malformed input; the caller counts
/// and drops) it still holds what it held before.
///
/// # Expected shape
///
/// ```text
/// {"u":12345,"s":"BTCUSDT","b":"65000.00","B":"1.2","a":"65001.00","A":"0.8"}
/// ```
///
/// Field order as documented on
/// <https://binance-docs.github.io/apidocs/spot/en/#individual-symbol-book-ticker-streams>.
/// We match by key so the scanner is robust to field reordering (some
/// upstream variants reorder `s` and `u`).
#[inline]
pub fn parse_book_ticker(buf: &[u8], sym: SymbolId, out: &mut BookTickerFrame) -> bool {
    // update id: "u":<integer>
    let Some(update_id) = bare_field_u64(buf, b"\"u\":") else {
        return false;
    };
    // Best bid / ask, price then size: "b" "B" "a" "A", quoted decimals.
    let Some(bid_px_1e6) = quoted_field_1e6(buf, b"\"b\":") else {
        return false;
    };
    let Some(bid_qty_1e6) = quoted_field_1e6(buf, b"\"B\":") else {
        return false;
    };
    let Some(ask_px_1e6) = quoted_field_1e6(buf, b"\"a\":") else {
        return false;
    };
    let Some(ask_qty_1e6) = quoted_field_1e6(buf, b"\"A\":") else {
        return false;
    };
    *out = BookTickerFrame::new(
        sym,
        update_id,
        bid_px_1e6,
        bid_qty_1e6,
        ask_px_1e6,
        ask_qty_1e6,
        book_ticker_venue_time_ms(buf),
    );
    true
}

// ---------------------------------------------------------------
// markPrice frame (WS5 — gaps §2.1)
// ---------------------------------------------------------------

/// A parsed USDS-M `<sym>@markPrice` frame (WS5): mark, index,
/// funding rate and next-funding time in one push. Delivery contracts
/// carry no funding — `has_funding` records wire truth, the WS3
/// Deribit convention (see [`parse_mark_price`] for the two dated
/// shapes). 64-byte POD; one cache line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct BnMarkPriceFrame {
    /// Venue event time (`"E"`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// `"p"` mark price ×1e6.
    pub mark_px_1e6: i64,
    /// `"i"` index price ×1e6.
    pub index_px_1e6: i64,
    /// `"r"` funding rate ×1e9 (signed; 0 when `has_funding` = 0).
    pub funding_rate_1e9: i64,
    /// `"T"` next funding time, ms since epoch (0 when absent/none).
    pub next_funding_ms: u64,
    /// Resolved symbol id (connection-pinned, like bookTicker).
    pub sym: SymbolId,
    /// 1 when the wire carried a parseable funding rate AND a next
    /// funding time — perps do; dated futures send neither.
    pub has_funding: u8,
    /// Reserved for layout stability (keeps struct at 64 bytes).
    _pad: [u8; 19],
}

impl BnMarkPriceFrame {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        ts_ns: 0,
        mark_px_1e6: 0,
        index_px_1e6: 0,
        funding_rate_1e9: 0,
        next_funding_ms: 0,
        sym: 0,
        has_funding: 0,
        _pad: [0; 19],
    };
}

/// Parse a USDS-M `@markPrice` frame (WS5) INTO `out`. Zero-alloc byte
/// scan, written in place like [`parse_book_ticker`] (the same 64 B
/// `align(64)` frame, the same 128 B `Option` it no longer returns);
/// `false` on malformed input (caller counts + taps), `out` untouched.
///
/// # Expected shape (live, fstream `/market/ws/`, 2026-09-23)
///
/// ```text
/// {"e":"markPriceUpdate","E":1790161527002,"s":"BTCUSDT",
///  "p":"85840.40234633","ap":"85840.40234633","P":"85863.42568007",
///  "i":"85882.44043478","r":"0.00005016","T":1790179200000,"st":1}
/// ```
///
/// Key-matched (field order never assumed; the `ap`/`st` keys the
/// venue added since WS5 are skipped like any other); the `"e"` tag
/// is REQUIRED — a foreign frame on a markPrice connection is a
/// reject, not a guess. `"r"`/`"T"` are optional-by-value.
///
/// **A dated future has no funding, in either of its two shapes.**
/// The WS5-era wire sent an EMPTY rate (`"r":""`); the live wire of
/// 2026-09-23 sends a ZERO rate with no next-funding time
/// (`"r":"0.00000000","T":0`, BX0 K6). A parseable rate alone would
/// read the second shape as a perpetual paying 0 % every 1970-01-01
/// — a `Funding` event per mark push for every dated contract — so
/// `has_funding` needs BOTH a parseable rate and `"T"` > 0; anything
/// else reports rate 0 and next-funding 0.
#[inline]
pub fn parse_mark_price(buf: &[u8], sym: SymbolId, out: &mut BnMarkPriceFrame) -> bool {
    if memchr::memmem::find(buf, b"\"e\":\"markPriceUpdate\"").is_none() {
        return false;
    }
    let Some(ts_ms) = bare_field_u64(buf, b"\"E\":") else {
        return false;
    };
    let Some(mark_px_1e6) = quoted_field_1e6(buf, b"\"p\":") else {
        return false;
    };
    let Some(index_px_1e6) = quoted_field_1e6(buf, b"\"i\":") else {
        return false;
    };
    let rate_1e9 = match find_field(buf, b"\"r\":") {
        Some(pos) => scan_price_1e9(buf, skip_byte(buf, pos, b'"')).map(|(v, _)| v),
        None => None,
    };
    let next_ms = bare_field_u64(buf, b"\"T\":").unwrap_or(0);
    // Funding needs a rate AND a next settlement — see the doc above.
    let (funding_rate_1e9, next_funding_ms, has_funding) = match rate_1e9 {
        Some(r) if next_ms != 0 => (r, next_ms, 1u8),
        _ => (0, 0, 0u8),
    };
    *out = BnMarkPriceFrame {
        ts_ns: ts_ms.saturating_mul(1_000_000),
        mark_px_1e6,
        index_px_1e6,
        funding_rate_1e9,
        next_funding_ms,
        sym,
        has_funding,
        _pad: [0; 19],
    };
    true
}

// ---------------------------------------------------------------
// Static layout
// ---------------------------------------------------------------

const _BOOK_TICKER_SIZE_CHECK: [(); 64] = [(); ::core::mem::size_of::<BookTickerFrame>()];
const _MARK_PRICE_SIZE_CHECK: [(); 64] = [(); ::core::mem::size_of::<BnMarkPriceFrame>()];
// `parse_trade` keeps its `Option` return: the flag's niche carries the
// tag, so the whole return stays inside the 64 B by-value bound that
// sent the two frames above to in-place parsing.
const _: () = assert!(::core::mem::size_of::<Option<TradeFrame>>() <= 64);

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // COPY: the 128 B `Option` of a 64 B frame returns by value — a
    // test-only view, so the assertions read naturally (tests are cold;
    // the production callers use the in-place API) — rejected: an
    // `&mut` frame threaded through every assertion.
    /// Test view of the in-place [`parse_book_ticker`]: `Some(frame)`
    /// on success.
    pub(crate) fn book_of(buf: &[u8], sym: SymbolId) -> Option<BookTickerFrame> {
        let mut f = BookTickerFrame::ZERO;
        parse_book_ticker(buf, sym, &mut f).then_some(f)
    }

    // COPY: the same test-only view for the markPrice frame.
    /// Test view of the in-place [`parse_mark_price`].
    pub(crate) fn mark_of(buf: &[u8], sym: SymbolId) -> Option<BnMarkPriceFrame> {
        let mut f = BnMarkPriceFrame::ZERO;
        parse_mark_price(buf, sym, &mut f).then_some(f)
    }

    const SAMPLE: &[u8] = br#"{"e":"aggTrade","E":1713000000000,"s":"BTCUSDT","p":"65432.1","q":"0.050000","T":1713000000000}"#;

    const SAMPLE_BT: &[u8] =
        br#"{"u":400900217,"s":"BNBUSDT","b":"25.35190000","B":"31.21000000","a":"25.36520000","A":"40.66000000"}"#;

    #[test]
    fn parse_trade_extracts_fields() {
        let t = parse_trade(SAMPLE, 1).unwrap();
        assert_eq!(t.sym, 1);
        assert_eq!(t.price_1e6, 65_432_100_000);
        assert_eq!(t.qty_1e6, 50_000);
        assert_eq!(t.ts_ns, 1_713_000_000_000 * 1_000_000);
        // VT2: raw ms stamp, absent agg id ⇒ 0, absent "m" ⇒ taker bought.
        assert_eq!(t.ts_ms, 1_713_000_000_000);
        assert_eq!(t.agg_id, 0);
        assert!(!t.is_buyer_maker);
    }

    #[test]
    fn parse_trade_extracts_agg_id_and_maker_flag() {
        // The live spot aggTrade shape (docs: a, p, q, f, l, T, m, M).
        let live = br#"{"e":"aggTrade","E":1672515782136,"s":"BTCUSDT","a":26129,"p":"0.001","q":"100","f":100,"l":105,"T":1672515782136,"m":true,"M":true}"#;
        let t = parse_trade(live, 7).unwrap();
        assert_eq!(t.agg_id, 26_129);
        assert!(t.is_buyer_maker, "m:true = the aggressor sold");
        assert_eq!(t.ts_ms, 1_672_515_782_136);
        let taker_buy = br#"{"e":"aggTrade","a":1,"p":"1","q":"1","T":5,"m":false}"#;
        assert!(!parse_trade(taker_buy, 7).unwrap().is_buyer_maker);
    }

    #[test]
    fn parse_trade_returns_none_on_missing_fields() {
        assert!(parse_trade(b"{}", 0).is_none());
    }

    const SAMPLE_MARK: &[u8] = br#"{"e":"markPriceUpdate","E":1562305380000,"s":"BTCUSDT","p":"11794.15000000","i":"11784.62659091","P":"11784.25641265","r":"0.00038167","T":1562306400000}"#;

    #[test]
    fn parse_mark_price_extracts_all_fields() {
        // WS5: mark + index + funding + next-funding in one frame.
        let f = mark_of(SAMPLE_MARK, 9).unwrap();
        assert_eq!(f.sym, 9);
        assert_eq!(f.ts_ns, 1_562_305_380_000 * 1_000_000);
        assert_eq!(f.mark_px_1e6, 11_794_150_000);
        assert_eq!(f.index_px_1e6, 11_784_626_590);
        assert_eq!(f.funding_rate_1e9, 381_670);
        assert_eq!(f.has_funding, 1);
        assert_eq!(f.next_funding_ms, 1_562_306_400_000);
    }

    #[test]
    fn parse_mark_price_negative_funding() {
        let b = br#"{"e":"markPriceUpdate","E":1000,"s":"X","p":"1.0","i":"1.0","r":"-0.00038167","T":2000}"#;
        assert_eq!(mark_of(b, 0).unwrap().funding_rate_1e9, -381_670);
    }

    #[test]
    fn parse_mark_price_dated_future_empty_rate() {
        // WS5: delivery contracts push `"r":""` — no funding, still a
        // valid frame (the WS3 has_funding convention).
        let b = br#"{"e":"markPriceUpdate","E":1000,"s":"BTCUSDT_260327","p":"65000.1","i":"64999.9","P":"65000.0","r":"","T":0}"#;
        let f = mark_of(b, 7).unwrap();
        assert_eq!(f.has_funding, 0);
        assert_eq!(f.funding_rate_1e9, 0);
        assert_eq!(f.mark_px_1e6, 65_000_100_000);
        assert_eq!(f.next_funding_ms, 0);
    }

    /// BX0-F1: the two frames fstream's `/market/ws/` path delivered
    /// on 2026-09-23 (K6, verbatim). The perp carries the `ap`/`st`
    /// keys the venue added since WS5 — skipped, not misread (`"ap":`
    /// must not satisfy the `"p":` anchor). The dated contract carries
    /// a ZERO rate with no next-funding time — no funding, exactly
    /// like the older empty-rate shape above.
    #[test]
    fn parse_mark_price_live_shapes_2026_09_23() {
        let perp = br#"{"e":"markPriceUpdate","E":1790161527002,"s":"BTCUSDT","p":"85840.40234633","ap":"85840.40234633","P":"85863.42568007","i":"85882.44043478","r":"0.00005016","T":1790179200000,"st":1}"#;
        let f = mark_of(perp, 11).unwrap();
        assert_eq!(f.ts_ns, 1_790_161_527_002 * 1_000_000);
        assert_eq!(f.mark_px_1e6, 85_840_402_346);
        assert_eq!(f.index_px_1e6, 85_882_440_434);
        assert_eq!(f.funding_rate_1e9, 50_160);
        assert_eq!(f.next_funding_ms, 1_790_179_200_000);
        assert_eq!(f.has_funding, 1);

        let dated = br#"{"e":"markPriceUpdate","E":1790161545000,"s":"BTCUSDT_260925","p":"85901.84762319","ap":"85901.84762319","P":"85864.31580990","i":"85884.08695652","r":"0.00000000","T":0,"st":1}"#;
        let f = mark_of(dated, 12).unwrap();
        assert_eq!(f.mark_px_1e6, 85_901_847_623);
        assert_eq!(f.index_px_1e6, 85_884_086_956);
        assert_eq!(f.has_funding, 0, "a dated contract pays no funding");
        assert_eq!((f.funding_rate_1e9, f.next_funding_ms), (0, 0));

        // A perp whose rate is genuinely zero still funds: the next
        // settlement is what separates it from a delivery contract.
        let flat = br#"{"e":"markPriceUpdate","E":1,"s":"X","p":"1.0","i":"1.0","r":"0.00000000","T":1790179200000}"#;
        let f = mark_of(flat, 0).unwrap();
        assert_eq!((f.has_funding, f.funding_rate_1e9), (1, 0));
    }

    #[test]
    fn parse_mark_price_rejects_foreign_and_malformed() {
        // A bookTicker frame on a markPrice slot is a reject (the
        // required "e" tag), as is a tagless blob.
        assert!(mark_of(SAMPLE_BT, 0).is_none());
        assert!(mark_of(b"{}", 0).is_none());
        // Tag present but the price fields missing.
        assert!(mark_of(br#"{"e":"markPriceUpdate","E":1}"#, 0).is_none());
    }

    #[test]
    fn parse_book_ticker_extracts_top_of_book() {
        let f = book_of(SAMPLE_BT, 42).unwrap();
        assert_eq!(f.sym, 42);
        assert_eq!(f.update_id, 400_900_217);
        assert_eq!(f.bid_px_1e6, 25_351_900);
        assert_eq!(f.bid_qty_1e6, 31_210_000);
        assert_eq!(f.ask_px_1e6, 25_365_200);
        assert_eq!(f.ask_qty_1e6, 40_660_000);
    }

    #[test]
    fn parse_book_ticker_venue_time_prefers_t_then_e_then_zero() {
        // VT2: spot bookTicker carries no stamp ⇒ 0 ("unknown, never
        // stale"); USDS-M carries E (event) and T (transaction) ⇒ T
        // wins; E alone is the fallback; a garbage stamp is 0, never a
        // parse failure.
        assert_eq!(book_of(SAMPLE_BT, 42).unwrap().venue_time_ms, 0);
        let usdm = br#"{"e":"bookTicker","u":400900217,"E":1568014460893,"T":1568014460891,"s":"BNBUSDT","b":"25.35190000","B":"31.21000000","a":"25.36520000","A":"40.66000000"}"#;
        assert_eq!(book_of(usdm, 42).unwrap().venue_time_ms, 1_568_014_460_891);
        let e_only = br#"{"e":"bookTicker","u":1,"E":1568014460893,"s":"X","b":"1.0","B":"1.0","a":"1.0","A":"1.0"}"#;
        assert_eq!(book_of(e_only, 0).unwrap().venue_time_ms, 1_568_014_460_893);
        let bad = br#"{"u":1,"T":"soon","s":"X","b":"1.0","B":"1.0","a":"1.0","A":"1.0"}"#;
        assert_eq!(book_of(bad, 0).unwrap().venue_time_ms, 0);
    }

    #[test]
    fn parse_book_ticker_returns_none_on_missing_fields() {
        // Missing "a" price.
        let b = br#"{"u":1,"s":"X","b":"1.0","B":"1.0","A":"1.0"}"#;
        assert!(book_of(b, 0).is_none());
    }

    #[test]
    fn parse_book_ticker_returns_none_on_garbage() {
        assert!(book_of(b"not json", 0).is_none());
    }

    #[test]
    fn book_ticker_frame_is_64_bytes() {
        assert_eq!(::core::mem::size_of::<BookTickerFrame>(), 64);
        assert_eq!(::core::mem::align_of::<BookTickerFrame>(), 64);
    }

    /// The in-place contract: a frame that fails to parse leaves `out`
    /// exactly as it was — never half-written over the caller's last
    /// good frame.
    #[test]
    fn a_failed_parse_leaves_the_frame_untouched() {
        let mut f = book_of(SAMPLE_BT, 42).unwrap();
        let before = f;
        // Every field but the ask price parses first.
        let no_ask = br#"{"u":1,"s":"X","b":"1.0","B":"1.0","A":"1.0"}"#;
        assert!(!parse_book_ticker(no_ask, 9, &mut f));
        assert_eq!(f, before);

        let mut m = mark_of(SAMPLE_MARK, 9).unwrap();
        let before = m;
        // Tag, stamp and mark parse; the index is missing.
        let no_index = br#"{"e":"markPriceUpdate","E":1,"p":"1.0","r":"0.0001","T":5}"#;
        assert!(!parse_mark_price(no_index, 3, &mut m));
        assert_eq!(m, before);
    }
}

// ---------------------------------------------------------------
// Property tests — any well-formed bookTicker roundtrips.
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use crate::tests::{book_of, mark_of};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn bookticker_roundtrips(
            u in 0u64..10_000_000_000u64,
            bp in 0u32..999_999u32,
            bq in 0u32..999_999u32,
            ap in 0u32..999_999u32,
            aq in 0u32..999_999u32,
            e in 1u64..4_000_000_000_000u64,
            t in 1u64..4_000_000_000_000u64,
            shape in 0u8..3u8, // 0 = spot (no stamp), 1 = E only, 2 = E + T
        ) {
            let mut buf = String::with_capacity(200);
            use std::fmt::Write;
            buf.push('{');
            if shape >= 1 {
                write!(&mut buf, r#""e":"bookTicker","E":{e},"#).unwrap();
            }
            if shape == 2 {
                write!(&mut buf, r#""T":{t},"#).unwrap();
            }
            write!(
                &mut buf,
                r#""u":{u},"s":"X","b":"0.{bp:06}","B":"0.{bq:06}","a":"0.{ap:06}","A":"0.{aq:06}"}}"#,
            ).unwrap();
            let f = book_of(buf.as_bytes(), 7).unwrap();
            prop_assert_eq!(f.sym, 7);
            prop_assert_eq!(f.update_id, u);
            prop_assert_eq!(f.bid_px_1e6, bp as i64);
            prop_assert_eq!(f.bid_qty_1e6, bq as i64);
            prop_assert_eq!(f.ask_px_1e6, ap as i64);
            prop_assert_eq!(f.ask_qty_1e6, aq as i64);
            // VT2: T > E > 0
            prop_assert_eq!(f.venue_time_ms, match shape { 0 => 0, 1 => e, _ => t });
        }

        #[test]
        fn bookticker_never_panics_on_arbitrary_bytes(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = book_of(&buf, 0);
        }

        // WS5: markPrice roundtrip — the funding sign and the dated
        // empty-rate form both hold under generated values.
        #[test]
        fn mark_price_roundtrips(
            ts in 1u64..4_000_000_000_000u64,
            mp in 0u32..999_999u32,
            ip in 0u32..999_999u32,
            r_num in -999_999i64..1_000_000i64,
            t_next in 0u64..4_000_000_000_000u64,
        ) {
            let mut buf = String::with_capacity(220);
            use std::fmt::Write;
            let sign = if r_num < 0 { "-" } else { "" };
            write!(
                &mut buf,
                r#"{{"e":"markPriceUpdate","E":{ts},"s":"X","p":"0.{mp:06}","i":"0.{ip:06}","r":"{sign}0.{:09}","T":{t_next}}}"#,
                r_num.unsigned_abs(),
            ).unwrap();
            let f = mark_of(buf.as_bytes(), 7).unwrap();
            prop_assert_eq!(f.sym, 7);
            prop_assert_eq!(f.ts_ns, ts * 1_000_000);
            prop_assert_eq!(f.mark_px_1e6, mp as i64);
            prop_assert_eq!(f.index_px_1e6, ip as i64);
            // BX0-F1: funding needs a next settlement; `T` = 0 is the
            // live dated-contract shape and reports none at all.
            let funds = t_next != 0;
            prop_assert_eq!(f.has_funding, u8::from(funds));
            prop_assert_eq!(f.funding_rate_1e9, if funds { r_num } else { 0 });
            prop_assert_eq!(f.next_funding_ms, t_next);
        }

        #[test]
        fn mark_price_never_panics_on_arbitrary_bytes(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = mark_of(&buf, 0);
        }
    }
}
