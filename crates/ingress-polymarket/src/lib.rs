//! # ingress-polymarket
//!
//! Polymarket CLOB WebSocket + REST ingress.
//!
//! Phase 0 ships:
//!   * `FrameKind` — the subset of CLOB frames we consume.
//!   * `parse_book_update` — handwritten byte scanner over `&[u8]`
//!     producing a `Tick`. No `serde_json`.
//!   * Unit + property tests over the parser.
//!
//! Real network plumbing (mio event loop, TLS, handshake, ping/pong)
//! lands in Phase 0 finish + Phase 1.

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
    drive_one, note_transport_ready, run, Driver, RunResult, State, StopFlag, SymbolMap,
    DEFAULT_TICK_RING_CAP, RX_BUF_SIZE, TX_BUF_SIZE,
};

use core_parse::{find_field, scan_price_1e6, scan_u64, skip_byte};
use core_types::{NsTs, Price, Qty, SymbolId, Tick};

// ---------------------------------------------------------------
// Frame kinds
// ---------------------------------------------------------------

/// High-level CLOB frame classification. Decided by a leading-bytes
/// probe; never by JSON-shape inference.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// `book` update — top-of-book change.
    BookUpdate,
    /// `last_trade_price` — executed trade.
    LastTrade,
    /// `ping` / `pong` keep-alives.
    Keepalive,
    /// Any frame we don't currently parse. Dropped silently.
    Unknown,
}

/// Classify a frame by looking at the `"event_type"` field.
///
/// Zero-alloc: only `&[u8]` subslices are touched.
#[inline]
pub fn classify(buf: &[u8]) -> FrameKind {
    if buf.is_empty() {
        return FrameKind::Unknown;
    }
    // Heuristic — first match wins. Order matters: `book` is common.
    if memchr::memmem::find(buf, b"\"event_type\":\"book\"").is_some() {
        FrameKind::BookUpdate
    } else if memchr::memmem::find(buf, b"\"event_type\":\"last_trade_price\"").is_some() {
        FrameKind::LastTrade
    } else if memchr::memmem::find(buf, b"\"ping\"").is_some()
        || memchr::memmem::find(buf, b"\"pong\"").is_some()
    {
        FrameKind::Keepalive
    } else {
        FrameKind::Unknown
    }
}

// ---------------------------------------------------------------
// Book-update parser
// ---------------------------------------------------------------

/// Parse a minimal book update into a `Tick`.
///
/// Zero-alloc. Returns `None` if any required field is missing or
/// malformed; the caller logs and drops the frame.
///
/// # Expected shape (trimmed)
///
/// ```text
/// {"event_type":"book","asset_id":"0xAB..","timestamp":"1713000000000",
///  "hash":"...","bids":[["0.518","100"],...],"asks":[["0.520","50"],...]}
/// ```
///
/// We pluck top-of-book from `bids[0]` and `asks[0]`.
#[inline]
pub fn parse_book_update(buf: &[u8], sym: SymbolId, ts_ns: NsTs) -> Option<Tick> {
    // venue_seq comes from the frame's monotonic "timestamp" field.
    let venue_seq = {
        let p = find_field(buf, b"\"timestamp\":")?;
        let p = skip_byte(buf, p, b'"');
        let (v, _end) = scan_u64(buf, p)?;
        // Timestamps are millis; we keep the low 32 bits as a sequence
        // number (wraps ~49.7 days — good enough for a monotonic tag).
        (v & 0xFFFF_FFFF) as u32
    };

    let (bid_px, bid_qty) = parse_first_level(buf, b"\"bids\":[[")?;
    let (ask_px, ask_qty) = parse_first_level(buf, b"\"asks\":[[")?;

    Some(Tick::new(
        ts_ns,
        sym,
        venue_seq,
        Price::from_raw(bid_px),
        Qty::from_raw(bid_qty),
        Price::from_raw(ask_px),
        Qty::from_raw(ask_qty),
    ))
}

/// Pull the first `["<price>","<qty>"]` level from either the `bids`
/// or `asks` array, given a `marker` that lands on the first byte
/// after the opening `[` of the first level.
#[inline]
fn parse_first_level(buf: &[u8], marker: &[u8]) -> Option<(i64, i64)> {
    let start = memchr::memmem::find(buf, marker)? + marker.len();
    // We're sitting right after `[[`. Expect `"<price>","<qty>"]]`.
    let p = skip_byte(buf, start, b'"');
    let (price_raw, after_price) = scan_price_1e6(buf, p)?;
    // Expect `","`
    let p = skip_byte(buf, after_price, b'"');
    let p = skip_byte(buf, p, b',');
    let p = skip_byte(buf, p, b'"');
    let (qty_raw, _after) = scan_price_1e6(buf, p)?;
    Some((price_raw, qty_raw))
}

// ---------------------------------------------------------------
// Errors
// ---------------------------------------------------------------

/// A parse failure. Carries a static message so it never allocates.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ParseError(pub &'static str);

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_BOOK: &[u8] = br#"{
        "event_type":"book",
        "asset_id":"0xabc",
        "timestamp":"1713000000000",
        "hash":"deadbeef",
        "bids":[["0.518","100.0"],["0.517","200.0"]],
        "asks":[["0.520","50.0"],["0.521","150.0"]]
    }"#;

    #[test]
    fn classify_recognises_book_frame() {
        assert_eq!(classify(SAMPLE_BOOK), FrameKind::BookUpdate);
    }

    #[test]
    fn classify_recognises_keepalive() {
        assert_eq!(classify(b"{\"ping\":1}"), FrameKind::Keepalive);
    }

    #[test]
    fn classify_unknown_for_empty() {
        assert_eq!(classify(b""), FrameKind::Unknown);
    }

    #[test]
    fn parse_book_update_extracts_top_of_book() {
        let t = parse_book_update(SAMPLE_BOOK, 7, 42).unwrap();
        assert_eq!(t.sym, 7);
        assert_eq!(t.ts_ns, 42);
        assert_eq!(t.bid_px.raw(), 518_000);
        assert_eq!(t.bid_qty.raw(), 100_000_000);
        assert_eq!(t.ask_px.raw(), 520_000);
        assert_eq!(t.ask_qty.raw(), 50_000_000);
        // venue_seq = low 32 bits of "1713000000000".
        assert_eq!(t.venue_seq, (1_713_000_000_000u64 & 0xFFFF_FFFF) as u32);
    }

    #[test]
    fn parse_book_update_returns_none_on_missing_bids() {
        let b = br#"{"event_type":"book","timestamp":"1","asks":[["0.5","1"]]}"#;
        assert!(parse_book_update(b, 1, 0).is_none());
    }

    #[test]
    fn parse_book_update_returns_none_on_garbage() {
        assert!(parse_book_update(b"not json at all", 1, 0).is_none());
    }
}

// ---------------------------------------------------------------
// Property tests — any well-formed book frame roundtrips through the
// parser without allocation.
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn arbitrary_prices_roundtrip_through_parser(
            bp in 0u32..999_999u32,
            bq in 0u32..999_999u32,
            ap in 0u32..999_999u32,
            aq in 0u32..999_999u32,
            ts in 0u64..1_000_000_000u64,
        ) {
            let mut buf = String::with_capacity(256);
            use std::fmt::Write;
            write!(
                &mut buf,
                r#"{{"event_type":"book","timestamp":"{ts}","bids":[["0.{bp:06}","0.{bq:06}"]],"asks":[["0.{ap:06}","0.{aq:06}"]]}}"#,
            ).unwrap();
            let t = parse_book_update(buf.as_bytes(), 0, 0).unwrap();
            prop_assert_eq!(t.bid_px.raw(), bp as i64);
            prop_assert_eq!(t.bid_qty.raw(), bq as i64);
            prop_assert_eq!(t.ask_px.raw(), ap as i64);
            prop_assert_eq!(t.ask_qty.raw(), aq as i64);
        }
    }
}
