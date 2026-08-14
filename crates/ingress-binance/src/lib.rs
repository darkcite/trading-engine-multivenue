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

pub mod run_loop;

pub use run_loop::{
    drive_one, note_transport_ready, run, Driver, RunResult, State, StopFlag,
    DEFAULT_TICK_RING_CAP, RX_BUF_SIZE, TX_BUF_SIZE,
};

use core_parse::{find_field, scan_price_1e6, scan_u64, skip_byte};
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
    /// Reserved for layout stability (keeps struct at 64 bytes).
    _pad: [u8; 20],
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
    ) -> Self {
        Self {
            update_id,
            bid_px_1e6,
            bid_qty_1e6,
            ask_px_1e6,
            ask_qty_1e6,
            sym,
            _pad: [0; 20],
        }
    }
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
    ))
}

// ---------------------------------------------------------------
// Static layout
// ---------------------------------------------------------------

const _BOOK_TICKER_SIZE_CHECK: [(); 64] = [(); ::core::mem::size_of::<BookTickerFrame>()];

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
        ) {
            let mut buf = String::with_capacity(160);
            use std::fmt::Write;
            write!(
                &mut buf,
                r#"{{"u":{u},"s":"X","b":"0.{bp:06}","B":"0.{bq:06}","a":"0.{ap:06}","A":"0.{aq:06}"}}"#,
            ).unwrap();
            let f = parse_book_ticker(buf.as_bytes(), 7).unwrap();
            prop_assert_eq!(f.sym, 7);
            prop_assert_eq!(f.update_id, u);
            prop_assert_eq!(f.bid_px_1e6, bp as i64);
            prop_assert_eq!(f.bid_qty_1e6, bq as i64);
            prop_assert_eq!(f.ask_px_1e6, ap as i64);
            prop_assert_eq!(f.ask_qty_1e6, aq as i64);
        }

        #[test]
        fn bookticker_never_panics_on_arbitrary_bytes(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = parse_book_ticker(&buf, 0);
        }
    }
}
