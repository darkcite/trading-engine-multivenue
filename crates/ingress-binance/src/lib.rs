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

/// A parsed Binance trade — mapped to a `Signal` downstream.
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
}

/// Parse a Binance `aggTrade` frame into a `TradeFrame`. Returns
/// `None` on malformed input.
pub fn parse_trade(buf: &[u8], sym: SymbolId) -> Option<TradeFrame> {
    // Price: "p":"65432.10"
    let pos = find_field(buf, b"\"p\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (price_1e6, _) = scan_price_1e6(buf, pos)?;

    // Qty: "q":"0.05"
    let pos = find_field(buf, b"\"q\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (qty_1e6, _) = scan_price_1e6(buf, pos)?;

    // Trade time: "T":1713000000000
    let pos = find_field(buf, b"\"T\":")?;
    let (ts_ms, _) = scan_u64(buf, pos)?;

    Some(TradeFrame {
        sym,
        price_1e6,
        qty_1e6,
        ts_ns: ts_ms.saturating_mul(1_000_000),
    })
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
    if let Some(pos) = find_field(buf, b"\"T\":") {
        if let Some((ms, _)) = scan_u64(buf, pos) {
            return ms;
        }
    }
    if let Some(pos) = find_field(buf, b"\"E\":") {
        if let Some((ms, _)) = scan_u64(buf, pos) {
            return ms;
        }
    }
    0
}

/// Parse a Binance `@bookTicker` frame. Zero-alloc. Returns `None` on
/// malformed input (caller logs and drops).
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
pub fn parse_book_ticker(buf: &[u8], sym: SymbolId) -> Option<BookTickerFrame> {
    // update id: "u":<integer>
    let pos = find_field(buf, b"\"u\":")?;
    let (update_id, _) = scan_u64(buf, pos)?;

    // best bid price: "b":"<decimal>"
    let pos = find_field(buf, b"\"b\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (bid_px_1e6, _) = scan_price_1e6(buf, pos)?;

    // bid qty: "B":"<decimal>"
    let pos = find_field(buf, b"\"B\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (bid_qty_1e6, _) = scan_price_1e6(buf, pos)?;

    // ask px: "a":"<decimal>"
    let pos = find_field(buf, b"\"a\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (ask_px_1e6, _) = scan_price_1e6(buf, pos)?;

    // ask qty: "A":"<decimal>"
    let pos = find_field(buf, b"\"A\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (ask_qty_1e6, _) = scan_price_1e6(buf, pos)?;

    Some(BookTickerFrame::new(
        sym,
        update_id,
        bid_px_1e6,
        bid_qty_1e6,
        ask_px_1e6,
        ask_qty_1e6,
        book_ticker_venue_time_ms(buf),
    ))
}

// ---------------------------------------------------------------
// markPrice frame (WS5 — gaps §2.1)
// ---------------------------------------------------------------

/// A parsed USDS-M `<sym>@markPrice` frame (WS5): mark, index,
/// funding rate and next-funding time in one push. Dated futures'
/// frames carry an EMPTY `"r"` (no funding on delivery contracts) —
/// `has_funding` records wire truth, the WS3 Deribit convention.
/// 64-byte POD; one cache line.
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
    /// 1 when the wire carried a parseable funding rate — perps do;
    /// dated futures send `"r":""`.
    pub has_funding: u8,
    /// Reserved for layout stability (keeps struct at 64 bytes).
    _pad: [u8; 19],
}

/// Parse a USDS-M `@markPrice` frame (WS5). Zero-alloc byte scan;
/// `None` on malformed input (caller counts + taps).
///
/// # Expected shape
///
/// ```text
/// {"e":"markPriceUpdate","E":1562305380000,"s":"BTCUSDT",
///  "p":"11794.15000000","i":"11784.62659091","P":"11784.25",
///  "r":"0.00038167","T":1562306400000}
/// ```
///
/// Key-matched (field order never assumed); the `"e"` tag is
/// REQUIRED — a foreign frame on a markPrice connection is a reject,
/// not a guess. `"r"`/`"T"` are optional-by-value: an empty or
/// unparseable rate ⇒ `has_funding` = 0 (dated futures), a missing
/// `"T"` ⇒ 0.
#[inline]
pub fn parse_mark_price(buf: &[u8], sym: SymbolId) -> Option<BnMarkPriceFrame> {
    memchr::memmem::find(buf, b"\"e\":\"markPriceUpdate\"")?;
    let pos = find_field(buf, b"\"E\":")?;
    let (ts_ms, _) = scan_u64(buf, pos)?;
    let pos = find_field(buf, b"\"p\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (mark_px_1e6, _) = scan_price_1e6(buf, pos)?;
    let pos = find_field(buf, b"\"i\":")?;
    let pos = skip_byte(buf, pos, b'"');
    let (index_px_1e6, _) = scan_price_1e6(buf, pos)?;
    let (funding_rate_1e9, has_funding) = match find_field(buf, b"\"r\":") {
        Some(pos) => {
            let pos = skip_byte(buf, pos, b'"');
            match scan_price_1e9(buf, pos) {
                Some((v, _)) => (v, 1u8),
                None => (0, 0u8), // `"r":""` — the dated-future shape
            }
        }
        None => (0, 0u8),
    };
    let next_funding_ms = match find_field(buf, b"\"T\":") {
        Some(pos) => scan_u64(buf, pos).map(|(v, _)| v).unwrap_or(0),
        None => 0,
    };
    Some(BnMarkPriceFrame {
        ts_ns: ts_ms.saturating_mul(1_000_000),
        mark_px_1e6,
        index_px_1e6,
        funding_rate_1e9,
        next_funding_ms,
        sym,
        has_funding,
        _pad: [0; 19],
    })
}

// ---------------------------------------------------------------
// Static layout
// ---------------------------------------------------------------

const _BOOK_TICKER_SIZE_CHECK: [(); 64] = [(); ::core::mem::size_of::<BookTickerFrame>()];
const _MARK_PRICE_SIZE_CHECK: [(); 64] = [(); ::core::mem::size_of::<BnMarkPriceFrame>()];

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn parse_trade_returns_none_on_missing_fields() {
        assert!(parse_trade(b"{}", 0).is_none());
    }

    const SAMPLE_MARK: &[u8] = br#"{"e":"markPriceUpdate","E":1562305380000,"s":"BTCUSDT","p":"11794.15000000","i":"11784.62659091","P":"11784.25641265","r":"0.00038167","T":1562306400000}"#;

    #[test]
    fn parse_mark_price_extracts_all_fields() {
        // WS5: mark + index + funding + next-funding in one frame.
        let f = parse_mark_price(SAMPLE_MARK, 9).unwrap();
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
        assert_eq!(parse_mark_price(b, 0).unwrap().funding_rate_1e9, -381_670);
    }

    #[test]
    fn parse_mark_price_dated_future_empty_rate() {
        // WS5: delivery contracts push `"r":""` — no funding, still a
        // valid frame (the WS3 has_funding convention).
        let b = br#"{"e":"markPriceUpdate","E":1000,"s":"BTCUSDT_260327","p":"65000.1","i":"64999.9","P":"65000.0","r":"","T":0}"#;
        let f = parse_mark_price(b, 7).unwrap();
        assert_eq!(f.has_funding, 0);
        assert_eq!(f.funding_rate_1e9, 0);
        assert_eq!(f.mark_px_1e6, 65_000_100_000);
        assert_eq!(f.next_funding_ms, 0);
    }

    #[test]
    fn parse_mark_price_rejects_foreign_and_malformed() {
        // A bookTicker frame on a markPrice slot is a reject (the
        // required "e" tag), as is a tagless blob.
        assert!(parse_mark_price(SAMPLE_BT, 0).is_none());
        assert!(parse_mark_price(b"{}", 0).is_none());
        // Tag present but the price fields missing.
        assert!(parse_mark_price(br#"{"e":"markPriceUpdate","E":1}"#, 0).is_none());
    }

    #[test]
    fn parse_book_ticker_extracts_top_of_book() {
        let f = parse_book_ticker(SAMPLE_BT, 42).unwrap();
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
        assert_eq!(parse_book_ticker(SAMPLE_BT, 42).unwrap().venue_time_ms, 0);
        let usdm = br#"{"e":"bookTicker","u":400900217,"E":1568014460893,"T":1568014460891,"s":"BNBUSDT","b":"25.35190000","B":"31.21000000","a":"25.36520000","A":"40.66000000"}"#;
        assert_eq!(parse_book_ticker(usdm, 42).unwrap().venue_time_ms, 1_568_014_460_891);
        let e_only = br#"{"e":"bookTicker","u":1,"E":1568014460893,"s":"X","b":"1.0","B":"1.0","a":"1.0","A":"1.0"}"#;
        assert_eq!(parse_book_ticker(e_only, 0).unwrap().venue_time_ms, 1_568_014_460_893);
        let bad = br#"{"u":1,"T":"soon","s":"X","b":"1.0","B":"1.0","a":"1.0","A":"1.0"}"#;
        assert_eq!(parse_book_ticker(bad, 0).unwrap().venue_time_ms, 0);
    }

    #[test]
    fn parse_book_ticker_returns_none_on_missing_fields() {
        // Missing "a" price.
        let b = br#"{"u":1,"s":"X","b":"1.0","B":"1.0","A":"1.0"}"#;
        assert!(parse_book_ticker(b, 0).is_none());
    }

    #[test]
    fn parse_book_ticker_returns_none_on_garbage() {
        assert!(parse_book_ticker(b"not json", 0).is_none());
    }

    #[test]
    fn book_ticker_frame_is_64_bytes() {
        assert_eq!(::core::mem::size_of::<BookTickerFrame>(), 64);
        assert_eq!(::core::mem::align_of::<BookTickerFrame>(), 64);
    }
}

// ---------------------------------------------------------------
// Property tests — any well-formed bookTicker roundtrips.
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
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
            let f = parse_book_ticker(buf.as_bytes(), 7).unwrap();
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
            let _ = parse_book_ticker(&buf, 0);
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
            let f = parse_mark_price(buf.as_bytes(), 7).unwrap();
            prop_assert_eq!(f.sym, 7);
            prop_assert_eq!(f.ts_ns, ts * 1_000_000);
            prop_assert_eq!(f.mark_px_1e6, mp as i64);
            prop_assert_eq!(f.index_px_1e6, ip as i64);
            prop_assert_eq!(f.funding_rate_1e9, r_num);
            prop_assert_eq!(f.has_funding, 1);
            prop_assert_eq!(f.next_funding_ms, t_next);
        }

        #[test]
        fn mark_price_never_panics_on_arbitrary_bytes(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = parse_mark_price(&buf, 0);
        }
    }
}
