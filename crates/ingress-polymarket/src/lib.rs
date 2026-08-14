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

pub mod discovery;
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
///
/// **Live wire note (2026-08-14, 8d live test):** market-channel
/// `book` events arrive **array-wrapped** (`[{...}]`, one element
/// per asset in the market) while `price_change` events arrive as
/// plain objects. Classification is `memmem`-based, so the wrapper
/// is irrelevant here; the run loop walks multi-event frames.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// `book` — full snapshot per asset (levels sorted worst→best).
    BookUpdate,
    /// `price_change` — level deltas; each row carries the current
    /// `best_bid`/`best_ask`, so top-of-book needs no ladder.
    PriceChange,
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
    // Heuristic — first match wins. Order matters: `book` and
    // `price_change` dominate the market channel.
    if memchr::memmem::find(buf, b"\"event_type\":\"book\"").is_some() {
        FrameKind::BookUpdate
    } else if memchr::memmem::find(buf, b"\"event_type\":\"price_change\"").is_some() {
        FrameKind::PriceChange
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

/// Extract the frame's monotonic `"timestamp"` (quoted ms): low 32
/// bits become `Tick.venue_seq` (wraps ~49.7 days — the documented
/// venue_seq policy on this venue since Phase 1).
#[inline]
pub fn scan_venue_seq(buf: &[u8]) -> Option<u32> {
    let p = find_field(buf, b"\"timestamp\":")?;
    let p = skip_byte(buf, p, b'"');
    let (v, _end) = scan_u64(buf, p)?;
    Some((v & 0xFFFF_FFFF) as u32)
}

/// Parse one `book` event into a `Tick`.
///
/// Zero-alloc. Returns `None` if any required field is missing or
/// malformed; the caller counts and drops the frame.
///
/// # Wire shape (live-verified 2026-08-14 — the Phase-1 shape was
/// wrong on every count and had never met the venue because D1
/// masked it)
///
/// ```text
/// [{"market":"0x..","asset_id":"1054..","timestamp":"17..","hash":"..",
///   "bids":[{"price":"0.001","size":"5684901.78"},...],
///   "asks":[{"price":"0.999","size":"11009166.77"},...],
///   "event_type":"book"}]
/// ```
///
/// Levels are objects sorted **worst→best**: top-of-book is the
/// **last** element of each side. The caller slices one event out of
/// the array wrapper (see the run loop's event walk).
#[inline]
pub fn parse_book_update(buf: &[u8], sym: SymbolId, ts_ns: NsTs) -> Option<Tick> {
    let venue_seq = scan_venue_seq(buf)?;
    let (bid_px, bid_qty) = scan_best_level(buf, b"\"bids\":[")?;
    let (ask_px, ask_qty) = scan_best_level(buf, b"\"asks\":[")?;
    // A book with both sides empty carries no information.
    if bid_px == 0 && ask_px == 0 {
        return None;
    }
    Some(Tick::new(
        ts_ns,
        core_types::VenueId::Polymarket,
        sym,
        venue_seq,
        Price::from_raw(bid_px),
        Qty::from_raw(bid_qty),
        Price::from_raw(ask_px),
        Qty::from_raw(ask_qty),
    ))
}

/// Walk one side's level array (`marker` = `"bids":[` / `"asks":[`)
/// and return the **last** (= best) `{"price":"..","size":".."}`
/// pair as `(px_1e6, sz_1e6)`. An empty side yields `(0, 0)`.
/// Level count on this venue is bounded by the price grid (≤ 999
/// levels at the 0.001 tick), so the strict walk stays cheap.
#[inline]
fn scan_best_level(buf: &[u8], marker: &[u8]) -> Option<(i64, i64)> {
    let mut at = memchr::memmem::find(buf, marker)? + marker.len();
    let mut best: (i64, i64) = (0, 0);
    loop {
        match *buf.get(at)? {
            b']' => return Some(best),
            b'{' | b',' => {
                if *buf.get(at)? == b',' {
                    at += 1;
                    continue;
                }
                // {"price":"X","size":"Y"}
                if buf.get(at..at + 10)? != b"{\"price\":\"" {
                    return None;
                }
                let (px, px_end) = scan_price_1e6(buf, at + 10)?;
                if buf.get(px_end..px_end + 10)? != b"\",\"size\":\"" {
                    return None;
                }
                let (sz, sz_end) = scan_price_1e6(buf, px_end + 10)?;
                let rel = memchr::memchr(b'}', buf.get(sz_end..)?)?;
                best = (px, sz);
                at = sz_end + rel + 1;
            }
            _ => return None,
        }
    }
}

/// Parse one `price_change` **row** into a `Tick`, using the row's
/// own `best_bid`/`best_ask` fields (live wire carries the current
/// touch on every row — no ladder needed).
///
/// Best-side **sizes** are only known when the changed level *is*
/// the touch (`price == best_bid` on a BUY row / `== best_ask` on a
/// SELL row) — the row's `size` is then the new touch size;
/// otherwise the size is 0 = unknown. Documented tick semantics for
/// this venue until 8e ladder work refines it.
///
/// `venue_seq` comes from the enclosing event's `timestamp` (rows
/// carry none) — the caller extracts it once per frame via
/// [`scan_venue_seq`].
#[inline]
pub fn parse_price_change_row(
    row: &[u8],
    sym: SymbolId,
    ts_ns: NsTs,
    venue_seq: u32,
) -> Option<Tick> {
    let p = find_field(row, b"\"best_bid\":")?;
    let p = skip_byte(row, p, b'"');
    let (bid_px, _) = scan_price_1e6(row, p)?;
    let p = find_field(row, b"\"best_ask\":")?;
    let p = skip_byte(row, p, b'"');
    let (ask_px, _) = scan_price_1e6(row, p)?;
    let p = find_field(row, b"\"price\":")?;
    let p = skip_byte(row, p, b'"');
    let (level_px, _) = scan_price_1e6(row, p)?;
    let p = find_field(row, b"\"size\":")?;
    let p = skip_byte(row, p, b'"');
    let (level_sz, _) = scan_price_1e6(row, p)?;
    let is_buy = if memchr::memmem::find(row, b"\"side\":\"BUY\"").is_some() {
        true
    } else if memchr::memmem::find(row, b"\"side\":\"SELL\"").is_some() {
        false
    } else {
        return None;
    };
    let bid_qty = if is_buy && level_px == bid_px { level_sz } else { 0 };
    let ask_qty = if !is_buy && level_px == ask_px { level_sz } else { 0 };
    Some(Tick::new(
        ts_ns,
        core_types::VenueId::Polymarket,
        sym,
        venue_seq,
        Price::from_raw(bid_px),
        Qty::from_raw(bid_qty),
        Price::from_raw(ask_px),
        Qty::from_raw(ask_qty),
    ))
}

// ---------------------------------------------------------------
// Market-channel subscribe writer (8d live fix)
// ---------------------------------------------------------------

/// Longest CLOB asset id we accept: a uint256 renders to ≤ 78
/// decimal digits.
pub const PM_ASSET_ID_MAX: usize = 80;

/// Serialize the market-channel subscribe frame the CLOB WS host
/// (`ws-subscriptions-clob.polymarket.com/ws/market`) requires
/// before it sends anything:
/// `{"assets_ids":["<id>"],"type":"market"}`.
///
/// Discovered live 2026-08-14: without this frame the server stays
/// silent and the endpoint path `/ws/` (pre-8d value) 404s — the
/// Phase-1 run loop had never been proven against the venue because
/// D1 masked it. Single asset for now; the multi-asset form arrives
/// with 8e Gamma discovery. Returns the byte length, `None` if the
/// id is empty/oversized or `dst` is too small.
#[inline]
pub fn write_market_subscribe(dst: &mut [u8], asset_id: &[u8]) -> Option<usize> {
    if asset_id.is_empty() || asset_id.len() > PM_ASSET_ID_MAX {
        return None;
    }
    const HEAD: &[u8] = b"{\"assets_ids\":[\"";
    const TAIL: &[u8] = b"\"],\"type\":\"market\"}";
    let total = HEAD.len() + asset_id.len() + TAIL.len();
    if dst.len() < total {
        return None;
    }
    dst[..HEAD.len()].copy_from_slice(HEAD);
    dst[HEAD.len()..HEAD.len() + asset_id.len()].copy_from_slice(asset_id);
    dst[HEAD.len() + asset_id.len()..total].copy_from_slice(TAIL);
    Some(total)
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

    /// Live wire shape (2026-08-14): array-wrapped event, object
    /// levels, both sides sorted **worst→best**.
    const SAMPLE_BOOK: &[u8] = br#"[{"market":"0x60c2","asset_id":"1054263738","timestamp":"1713000000000","hash":"deadbeef","bids":[{"price":"0.517","size":"200.0"},{"price":"0.518","size":"100.0"}],"asks":[{"price":"0.521","size":"150.0"},{"price":"0.520","size":"50.0"}],"event_type":"book"}]"#;
    const SAMPLE_PC: &[u8] = br#"{"market":"0x60c2","price_changes":[{"asset_id":"1054263738","price":"0.518","size":"642.77","side":"BUY","hash":"d0c1","best_bid":"0.518","best_ask":"0.520"}],"timestamp":"1713000000123","event_type":"price_change"}"#;

    #[test]
    fn classify_recognises_book_frame() {
        assert_eq!(classify(SAMPLE_BOOK), FrameKind::BookUpdate);
    }

    #[test]
    fn write_market_subscribe_exact_bytes() {
        let mut dst = [0u8; 160];
        let n = write_market_subscribe(&mut dst, b"1234567890").unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"assets_ids":["1234567890"],"type":"market"}"# as &[u8]
        );
    }

    #[test]
    fn write_market_subscribe_rejects_bad_input() {
        let mut dst = [0u8; 160];
        assert!(write_market_subscribe(&mut dst, b"").is_none());
        assert!(write_market_subscribe(&mut dst, &[b'1'; PM_ASSET_ID_MAX + 1]).is_none());
        let mut tiny = [0u8; 16];
        assert!(write_market_subscribe(&mut tiny, b"1234567890").is_none());
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
    fn parse_book_update_takes_last_level_as_top_of_book() {
        let t = parse_book_update(SAMPLE_BOOK, 7, 42).unwrap();
        assert_eq!(t.sym, 7);
        assert_eq!(t.ts_ns, 42);
        // Sides are worst→best: top is the LAST level of each array.
        assert_eq!(t.bid_px.raw(), 518_000);
        assert_eq!(t.bid_qty.raw(), 100_000_000);
        assert_eq!(t.ask_px.raw(), 520_000);
        assert_eq!(t.ask_qty.raw(), 50_000_000);
        // venue_seq = low 32 bits of "1713000000000".
        assert_eq!(t.venue_seq, (1_713_000_000_000u64 & 0xFFFF_FFFF) as u32);
    }

    #[test]
    fn parse_book_update_empty_side_and_rejects() {
        let empty_bids = br#"{"timestamp":"1","bids":[],"asks":[{"price":"0.5","size":"1.0"}]}"#;
        let t = parse_book_update(empty_bids, 1, 0).unwrap();
        assert_eq!(t.bid_px.raw(), 0);
        assert_eq!(t.ask_px.raw(), 500_000);
        // Missing bids array entirely is malformed.
        let b = br#"{"timestamp":"1","asks":[{"price":"0.5","size":"1.0"}]}"#;
        assert!(parse_book_update(b, 1, 0).is_none());
        assert!(parse_book_update(b"not json at all", 1, 0).is_none());
        // Both sides empty carries no information.
        let both = br#"{"timestamp":"1","bids":[],"asks":[]}"#;
        assert!(parse_book_update(both, 1, 0).is_none());
    }

    #[test]
    fn classify_recognises_price_change() {
        assert_eq!(classify(SAMPLE_PC), FrameKind::PriceChange);
    }

    #[test]
    fn parse_price_change_row_uses_row_touch() {
        let seq = scan_venue_seq(SAMPLE_PC).unwrap();
        assert_eq!(seq, (1_713_000_000_123u64 & 0xFFFF_FFFF) as u32);
        let t = parse_price_change_row(SAMPLE_PC, 3, 99, seq).unwrap();
        assert_eq!(t.sym, 3);
        assert_eq!(t.ts_ns, 99);
        assert_eq!(t.venue_seq, seq);
        assert_eq!(t.bid_px.raw(), 518_000);
        assert_eq!(t.ask_px.raw(), 520_000);
        // BUY row at price == best_bid ⇒ the row size IS the new
        // touch size; the far side is unknown (0).
        assert_eq!(t.bid_qty.raw(), 642_770_000);
        assert_eq!(t.ask_qty.raw(), 0);
    }

    #[test]
    fn parse_price_change_row_rejects_missing_fields() {
        let no_side = br#"{"asset_id":"1","price":"0.5","size":"1","best_bid":"0.5","best_ask":"0.6"}"#;
        assert!(parse_price_change_row(no_side, 1, 0, 0).is_none());
        let no_touch = br#"{"asset_id":"1","price":"0.5","size":"1","side":"BUY"}"#;
        assert!(parse_price_change_row(no_touch, 1, 0, 0).is_none());
        // SELL row away from the ask: prices known, sizes unknown.
        let away = br#"{"asset_id":"1","price":"0.4","size":"7","side":"SELL","best_bid":"0.5","best_ask":"0.6"}"#;
        let t = parse_price_change_row(away, 1, 0, 0).unwrap();
        assert_eq!(t.bid_qty.raw(), 0);
        assert_eq!(t.ask_qty.raw(), 0);
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
            // Real wire: object levels, worst→best — render a worse
            // level first so "last level wins" is exercised.
            prop_assume!(bp > 0 || ap > 0);
            let mut buf = String::with_capacity(320);
            use std::fmt::Write;
            write!(
                &mut buf,
                r#"{{"event_type":"book","timestamp":"{ts}","bids":[{{"price":"0.000001","size":"1.0"}},{{"price":"0.{bp:06}","size":"0.{bq:06}"}}],"asks":[{{"price":"0.999999","size":"1.0"}},{{"price":"0.{ap:06}","size":"0.{aq:06}"}}]}}"#,
            ).unwrap();
            let t = parse_book_update(buf.as_bytes(), 0, 0).unwrap();
            prop_assert_eq!(t.bid_px.raw(), bp as i64);
            prop_assert_eq!(t.bid_qty.raw(), bq as i64);
            prop_assert_eq!(t.ask_px.raw(), ap as i64);
            prop_assert_eq!(t.ask_qty.raw(), aq as i64);
        }
    }
}
