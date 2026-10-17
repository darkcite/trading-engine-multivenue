// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

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
//!
//! ## `Tick.venue_seq` — the per-asset arrival ordinal (HK31, 2026-10-15)
//!
//! CLOB frames carry no sequence number, only the quoted-ms
//! `timestamp`, and the venue sends several frames per asset inside one
//! millisecond (8.0 % of the rows on the 2026-10-15 tap). The parsers
//! leave `venue_seq` 0; the run loop's per-asset sequencer
//! (`core_types::ArrivalSeq`, shared with Hyperliquid and Deribit since
//! HK32) stamps it: a frame whose `timestamp` is not older than its asset's
//! newest takes the next ordinal — 1, 2, 3, … in arrival order, which on
//! the one TCP stream is the venue's order — and an OLDER one takes 0,
//! which every `book_builder::TopOfBook` drops as `Stale` and the run loop
//! counts on `seq_regressions_total`. The ordinal is dense and monotone for
//! the life of the process (reconnects included — a reconnect zeroes only
//! the per-asset clock); a `u32` per asset lasts ~76 days even at the
//! measured one-asset peak of 655 ticks/s.
//!
//! Until HK31 it was the low 32 bits of the ms `timestamp`: every frame
//! after the first in a millisecond was `Stale` (172 touch changes in the
//! tap's hour lagged until the next millisecond; every post-trade `book`
//! re-snapshot was dropped), and the low 32 bits wrap every ~49.7 days
//! (next 2026-11-21 21:25:29Z), after which every tick would have been
//! `Stale` until a restart. The full stamp rides `Tick.venue_time_ms`
//! (VT2) under both laws.
//!
//! ## PM15 — the rolling 15-minute families (2026-10-17)
//!
//! Beside the static markets above, the crate carries Polymarket's
//! ROLLING 15-minute crypto up/down families on a thread of their own
//! (`core_types::pm_roll` is the data model):
//!
//! - [`pm15_discovery`] — a cold thread that looks each window's market
//!   up on Gamma by slug and checks it under the venue law ([`gamma15`]);
//! - [`rolling`] — the bindings and the roll schedule (pure state);
//! - [`book15`] — a full 0.001 ladder per token, so a touch carries its
//!   size;
//! - [`rtds`] — the Chainlink reports the markets settle on;
//! - [`pm15_loop`] — the ingress thread: the market and RTDS sockets on
//!   one poll, into the engine's PM15 lane.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod book15;
pub mod discovery;
pub mod gamma15;
pub mod pm15_discovery;
pub mod pm15_loop;
pub mod rolling;
pub mod rtds;
pub mod run_loop;

pub use run_loop::{
    drive_one, note_transport_ready, run, Driver, RunResult, State, StopFlag, SymbolMap,
    DEFAULT_TICK_RING_CAP, RX_BUF_SIZE, TX_BUF_SIZE,
};

use core_net::{WsPayload, WsWriteErr};
use core_parse::{find_field, scan_price_1e6, scan_u64, skip_byte, skip_ws};
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
/// That array is the subscribe-time snapshot; the post-trade `book`
/// re-snapshots sent while subscribed arrive as one plain,
/// `", "`-separated object per asset (live 2026-10-15) — same event,
/// same walk; [`parse_book_update`] says what they add (the touch sizes).
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

/// Extract the frame's `"timestamp"` (quoted ms) in full — the VT2
/// venue time that rides `Tick.venue_time_ms`, and the clock the run
/// loop's per-asset sequencer orders frames by (HK31, crate docs).
#[inline]
pub fn scan_venue_time_ms(buf: &[u8]) -> Option<u64> {
    let p = find_field(buf, b"\"timestamp\":")?;
    let p = skip_byte(buf, p, b'"');
    let (v, _end) = scan_u64(buf, p)?;
    Some(v)
}

/// Parse one `book` event into a `Tick`.
///
/// Zero-alloc. Returns `false` if any required field is missing or
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
/// Re-sent while subscribed (live 2026-10-15), the same snapshot is one
/// plain object, `", "`-separated, `bids`/`asks` before `hash`/
/// `timestamp`, without `tick_size`/`last_trade_price`:
///
/// ```text
/// {"market":"0x..", "asset_id":"8837..", "bids":[{"price":"0.01", "size":"14741.19"}, ...],
///  "asks":[...], "hash":"..", "timestamp":"17..", "event_type":"book"}
/// ```
///
/// Both forms are the full snapshot and parse identically: the touch
/// and the `timestamp` (`hash`, `tick_size`, `last_trade_price` are
/// unused). The re-snapshot is post-trade: it carries the same
/// `timestamp`, `hash` and best bid/ask as the `price_change` sent for
/// that asset just before it (122 of 122 on the 2026-10-15 tap, 0–69 ms
/// behind it; 288 of 288 over the closed hour). Since HK31 the run
/// loop's arrival ordinal applies it — the ms-derived `venue_seq` made
/// `book_builder::TopOfBook::apply` drop it as `Stale` — and it brings
/// the book the touch SIZES a `price_change` row leaves 0: not a
/// fresher touch price.
///
/// `venue_seq` and `flags` are left 0 for the run loop's sequencer and
/// judge (HK31, VT2): it stamps `venue_seq` with the asset's arrival
/// ordinal.
///
/// Levels are objects sorted **worst→best**: top-of-book is the
/// **last** element of each side. The caller slices one event out of
/// the array wrapper (see the run loop's event walk).
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_book_update(buf: &[u8], sym: SymbolId, ts_ns: NsTs, out: &mut Tick) -> bool {
    parse_book_update_fill(buf, sym, ts_ns, out).is_some()
}

/// [`parse_book_update`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_book_update_fill(buf: &[u8], sym: SymbolId, ts_ns: NsTs, out: &mut Tick) -> Option<()> {
    let venue_time_ms = scan_venue_time_ms(buf)?;
    let (bid_px, bid_qty) = scan_best_level(buf, b"\"bids\":[")?;
    let (ask_px, ask_qty) = scan_best_level(buf, b"\"asks\":[")?;
    // A book with both sides empty carries no information.
    if bid_px == 0 && ask_px == 0 {
        return None;
    }
    // VT2: the stamp rides the slot; the run loop judges staleness
    // (the parser has no per-connection estimator) and sets `flags`,
    // and its per-asset sequencer sets `venue_seq` (HK31).
    *out = Tick::new_stamped(
        ts_ns,
        core_types::VenueId::Polymarket,
        sym,
        0,
        Price::from_raw(bid_px),
        Qty::from_raw(bid_qty),
        Price::from_raw(ask_px),
        Qty::from_raw(ask_qty),
        venue_time_ms,
        0,
    );
    Some(())
}

/// Walk one side's level array (`marker` = `"bids":[` / `"asks":[`)
/// and return the **last** (= best) `{"price":"..","size":".."}`
/// pair as `(px_1e6, sz_1e6)`. An empty side yields `(0, 0)`.
/// Level count on this venue is bounded by the price grid (≤ 999
/// levels at the 0.001 tick), so the strict walk stays cheap.
///
/// JSON whitespace is skipped after the `[`, after every `,` (between
/// levels and between `price` and `size`) and after each level's `}`:
/// the single-object re-snapshot is `", "`-separated (every one was
/// rejected before 2026-10-15). Keys stay colon-compact (`"price":"`),
/// as everywhere on this wire. On compact input each skip is one
/// failed compare.
#[inline]
fn scan_best_level(buf: &[u8], marker: &[u8]) -> Option<(i64, i64)> {
    let mut at = skip_ws(buf, memchr::memmem::find(buf, marker)? + marker.len());
    if *buf.get(at)? == b']' {
        return Some((0, 0));
    }
    loop {
        // {"price":"X","size":"Y"} / {"price":"X", "size":"Y"}
        if buf.get(at..at + 10)? != b"{\"price\":\"" {
            return None;
        }
        let (px, px_end) = scan_price_1e6(buf, at + 10)?;
        if buf.get(px_end..px_end + 2)? != b"\"," {
            return None;
        }
        let sz_at = skip_ws(buf, px_end + 2);
        if buf.get(sz_at..sz_at + 8)? != b"\"size\":\"" {
            return None;
        }
        let (sz, sz_end) = scan_price_1e6(buf, sz_at + 8)?;
        let rel = memchr::memchr(b'}', buf.get(sz_end..)?)?;
        at = skip_ws(buf, sz_end + rel + 1);
        match *buf.get(at)? {
            b',' => at = skip_ws(buf, at + 1),
            // The last level is the best one.
            b']' => return Some((px, sz)),
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
/// `venue_time_ms` is the enclosing event's `timestamp` (rows carry
/// none) — the caller extracts it once per frame via
/// [`scan_venue_time_ms`]. `venue_seq` and `flags` are left 0 for the
/// run loop's sequencer and judge.
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_price_change_row(
    row: &[u8],
    sym: SymbolId,
    ts_ns: NsTs,
    venue_time_ms: u64,
    out: &mut Tick,
) -> bool {
    parse_price_change_row_fill(row, sym, ts_ns, venue_time_ms, out).is_some()
}

/// [`parse_price_change_row`]'s body: `?` short-circuits, and `out` is
/// written once, at the end, only after every field has parsed.
#[inline(always)]
fn parse_price_change_row_fill(
    row: &[u8],
    sym: SymbolId,
    ts_ns: NsTs,
    venue_time_ms: u64,
    out: &mut Tick,
) -> Option<()> {
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
    let bid_qty = if is_buy && level_px == bid_px {
        level_sz
    } else {
        0
    };
    let ask_qty = if !is_buy && level_px == ask_px {
        level_sz
    } else {
        0
    };
    *out = Tick::new_stamped(
        ts_ns,
        core_types::VenueId::Polymarket,
        sym,
        // HK31: the run loop's sequencer sets `venue_seq`.
        0,
        Price::from_raw(bid_px),
        Qty::from_raw(bid_qty),
        Price::from_raw(ask_px),
        Qty::from_raw(ask_qty),
        venue_time_ms,
        0,
    );
    Some(())
}

// ---------------------------------------------------------------
// Market-channel subscribe writer (8d live fix)
// ---------------------------------------------------------------

/// Longest CLOB asset id we accept: a uint256 renders to ≤ 78
/// decimal digits.
pub const PM_ASSET_ID_MAX: usize = 80;

/// Maximum asset ids in one market-channel subscribe (M1 multi-market
/// boot cap: 64 markets × YES/NO tokens; `core-config::universe`
/// enforces the market-entry cap upstream).
pub const PM_SUBSCRIBE_IDS_MAX: usize = 128;

/// Whether `ids` can go in one market subscribe: 1..=
/// [`PM_SUBSCRIBE_IDS_MAX`] ids, each 1..=[`PM_ASSET_ID_MAX`] bytes.
#[inline]
pub(crate) fn market_subscribe_ids_valid(ids: &[&[u8]]) -> bool {
    if ids.is_empty() || ids.len() > PM_SUBSCRIBE_IDS_MAX {
        return false;
    }
    let mut i = 0;
    while i < ids.len() {
        if ids[i].is_empty() || ids[i].len() > PM_ASSET_ID_MAX {
            return false;
        }
        i += 1;
    }
    true
}

/// Render the market-channel subscribe through `p`, straight into its
/// frame, header first (`core_net::queue_masked_text_frame_rendered`):
/// `{"assets_ids":["<id0>","<id1>",…],"type":"market"}` — one frame
/// lists every id (M1 multi-market). `ids` must pass
/// [`market_subscribe_ids_valid`].
///
/// The CLOB WS host (`ws-subscriptions-clob.polymarket.com/ws/market`)
/// sends nothing before it: discovered live 2026-08-14 — without this
/// frame the server stays silent and the endpoint path `/ws/` (pre-8d
/// value) 404s; the Phase-1 run loop had never been proven against the
/// venue because D1 masked it.
///
/// # Errors
/// [`WsWriteErr::BufferTooSmall`] when `p` runs out of room.
#[inline]
pub(crate) fn render_market_subscribe(
    p: &mut WsPayload<'_>,
    ids: &[&[u8]],
) -> Result<(), WsWriteErr> {
    debug_assert!(
        market_subscribe_ids_valid(ids),
        "market subscribe ids must be checked first"
    );
    p.put(b"{\"assets_ids\":[")?;
    let mut i = 0;
    while i < ids.len() {
        if i > 0 {
            p.put(b",")?;
        }
        p.put(b"\"")?;
        p.put(ids[i])?;
        p.put(b"\"")?;
        i += 1;
    }
    p.put(b"],\"type\":\"market\"}")
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

/// Test views in the old by-value shape, shared by the unit and the
/// property tests: each wraps one in-place parser and hands back its
/// frame's `Option`, so assertions read naturally. Cold — production
/// callers parse in place.
#[cfg(test)]
mod views {
    // COPY: `Option<Tick>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_book_update_view(buf: &[u8], sym: core_types::SymbolId, ts_ns: core_types::NsTs) -> Option<core_types::Tick> {
        let mut f = core_types::Tick::ZERO;
        crate::parse_book_update(buf, sym, ts_ns, &mut f).then_some(f)
    }
    // COPY: `Option<Tick>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_price_change_row_view(
        row: &[u8],
        sym: core_types::SymbolId,
        ts_ns: core_types::NsTs,
        venue_time_ms: u64,
    ) -> Option<core_types::Tick> {
        let mut f = core_types::Tick::ZERO;
        crate::parse_price_change_row(row, sym, ts_ns, venue_time_ms, &mut f).then_some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::views::*;

    /// Live wire shape (2026-08-14): array-wrapped event, object
    /// levels, both sides sorted **worst→best**.
    const SAMPLE_BOOK: &[u8] = br#"[{"market":"0x60c2","asset_id":"1054263738","timestamp":"1713000000000","hash":"deadbeef","bids":[{"price":"0.517","size":"200.0"},{"price":"0.518","size":"100.0"}],"asks":[{"price":"0.521","size":"150.0"},{"price":"0.520","size":"50.0"}],"event_type":"book"}]"#;
    /// Live wire 2026-10-15 (raw tap `pm-raw.tap`, a parse reject before
    /// the fix): the single-object `book` re-snapshot — `", "`
    /// separators, `bids`/`asks` before `hash`/`timestamp`, no
    /// `tick_size`. Level arrays trimmed to first + last two; spacing
    /// and key order exact.
    const LIVE_BOOK_SPACED: &[u8] = br#"{"market":"0x99b5ab8514be117c2a544cfc01186da0157453f1db21ae635c11d6a6afef22ee", "asset_id":"88378104575386233188962226569320382272592669666454108287035040791872565531906", "bids":[{"price":"0.01", "size":"14741.19"}, {"price":"0.16", "size":"190"}, {"price":"0.17", "size":"10"}], "asks":[{"price":"0.99", "size":"107748.47"}, {"price":"0.2", "size":"5465"}, {"price":"0.19", "size":"1861.19"}], "hash":"b383d532df1450e8b0a702c5c42fef0aff0b461b", "timestamp":"1792068514889", "event_type":"book"}"#;
    /// The same snapshot in the subscribe-time array form (compact,
    /// key order and extra fields of the live array frame).
    const LIVE_BOOK_COMPACT: &[u8] = br#"[{"market":"0x99b5ab8514be117c2a544cfc01186da0157453f1db21ae635c11d6a6afef22ee","asset_id":"88378104575386233188962226569320382272592669666454108287035040791872565531906","timestamp":"1792068514889","hash":"b383d532df1450e8b0a702c5c42fef0aff0b461b","bids":[{"price":"0.01","size":"14741.19"},{"price":"0.16","size":"190"},{"price":"0.17","size":"10"}],"asks":[{"price":"0.99","size":"107748.47"},{"price":"0.2","size":"5465"},{"price":"0.19","size":"1861.19"}],"tick_size":"0.01","event_type":"book","last_trade_price":"0.180"}]"#;
    const SAMPLE_PC: &[u8] = br#"{"market":"0x60c2","price_changes":[{"asset_id":"1054263738","price":"0.518","size":"642.77","side":"BUY","hash":"d0c1","best_bid":"0.518","best_ask":"0.520"}],"timestamp":"1713000000123","event_type":"price_change"}"#;

    #[test]
    fn classify_recognises_book_frame() {
        assert_eq!(classify(SAMPLE_BOOK), FrameKind::BookUpdate);
    }

    /// The market subscribe rendered into a plain buffer — exactly what
    /// the frame's payload span receives; `None` for an id list
    /// [`market_subscribe_ids_valid`] refuses, or when it does not fit.
    fn write_market_subscribe_multi(buf: &mut [u8], ids: &[&[u8]]) -> Option<usize> {
        if !market_subscribe_ids_valid(ids) {
            return None;
        }
        let mut p = WsPayload::writing(buf);
        render_market_subscribe(&mut p, ids).ok()?;
        Some(p.len())
    }

    #[test]
    fn write_market_subscribe_multi_exact_bytes() {
        let mut dst = [0u8; 256];
        let n = write_market_subscribe_multi(&mut dst, &[b"1234567890"]).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"assets_ids":["1234567890"],"type":"market"}"# as &[u8]
        );
        let n = write_market_subscribe_multi(&mut dst, &[b"1234567890", b"9876543210"]).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"assets_ids":["1234567890","9876543210"],"type":"market"}"# as &[u8]
        );
    }

    #[test]
    fn write_market_subscribe_multi_rejects_bad_input() {
        let mut dst = [0u8; 4096];
        // Empty list.
        assert!(write_market_subscribe_multi(&mut dst, &[]).is_none());
        // Any empty / oversized id.
        assert!(write_market_subscribe_multi(&mut dst, &[b"1234567890", b""]).is_none());
        let big = [b'1'; PM_ASSET_ID_MAX + 1];
        assert!(write_market_subscribe_multi(&mut dst, &[&big]).is_none());
        // Over the id-count cap.
        let id: &[u8] = b"1234567890";
        let too_many = [id; PM_SUBSCRIBE_IDS_MAX + 1];
        let mut huge = [0u8; 32 * 1024];
        assert!(write_market_subscribe_multi(&mut huge, &too_many).is_none());
        // dst too small.
        let mut tiny = [0u8; 32];
        assert!(write_market_subscribe_multi(&mut tiny, &[id, id]).is_none());
    }

    #[test]
    fn write_market_subscribe_multi_cap_boundary_fits() {
        // Exactly PM_SUBSCRIBE_IDS_MAX maximal ids must serialize.
        let big = [b'7'; PM_ASSET_ID_MAX];
        let ids = [&big[..]; PM_SUBSCRIBE_IDS_MAX];
        let mut dst = [0u8; 16 * 1024];
        let n = write_market_subscribe_multi(&mut dst, &ids).expect("cap boundary serializes");
        assert!(n <= dst.len());
        assert!(dst[..n].ends_with(br#"],"type":"market"}"#));
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
        let t = parse_book_update_view(SAMPLE_BOOK, 7, 42).unwrap();
        assert_eq!(t.sym, 7);
        assert_eq!(t.ts_ns, 42);
        // Sides are worst→best: top is the LAST level of each array.
        assert_eq!(t.bid_px.raw(), 518_000);
        assert_eq!(t.bid_qty.raw(), 100_000_000);
        assert_eq!(t.ask_px.raw(), 520_000);
        assert_eq!(t.ask_qty.raw(), 50_000_000);
        // HK31: the run loop's sequencer sets venue_seq, not the parser.
        assert_eq!(t.venue_seq, 0);
        // VT2: the full stamp rides the slot; the parser never judges.
        assert_eq!(t.venue_time_ms, 1_713_000_000_000);
        assert_eq!(t.flags, 0);
    }

    #[test]
    fn parse_book_update_empty_side_and_rejects() {
        let empty_bids = br#"{"timestamp":"1","bids":[],"asks":[{"price":"0.5","size":"1.0"}]}"#;
        let t = parse_book_update_view(empty_bids, 1, 0).unwrap();
        assert_eq!(t.bid_px.raw(), 0);
        assert_eq!(t.ask_px.raw(), 500_000);
        // Missing bids array entirely is malformed.
        let b = br#"{"timestamp":"1","asks":[{"price":"0.5","size":"1.0"}]}"#;
        assert!(parse_book_update_view(b, 1, 0).is_none());
        assert!(parse_book_update_view(b"not json at all", 1, 0).is_none());
        // Both sides empty carries no information.
        let both = br#"{"timestamp":"1","bids":[],"asks":[]}"#;
        assert!(parse_book_update_view(both, 1, 0).is_none());
    }

    /// Every live single-object re-snapshot was a parse reject before
    /// 2026-10-15 (the strict walk wanted `","size":"` and `},{`).
    #[test]
    fn parse_book_update_accepts_live_spaced_single_object_snapshot() {
        assert_eq!(classify(LIVE_BOOK_SPACED), FrameKind::BookUpdate);
        let t = parse_book_update_view(LIVE_BOOK_SPACED, 7, 42).unwrap();
        assert_eq!(t.sym, 7);
        assert_eq!(t.ts_ns, 42);
        // Worst→best: the LAST level of each side is the touch.
        assert_eq!(t.bid_px.raw(), 170_000);
        assert_eq!(t.bid_qty.raw(), 10_000_000);
        assert_eq!(t.ask_px.raw(), 190_000);
        assert_eq!(t.ask_qty.raw(), 1_861_190_000);
        assert_eq!(t.venue_time_ms, 1_792_068_514_889);
        assert_eq!(t.venue_seq, 0);
        assert_eq!(t.flags, 0);
    }

    /// Both snapshot forms land identically — same touch, same stamp.
    #[test]
    fn spaced_and_compact_snapshots_parse_to_the_same_tick() {
        let spaced = parse_book_update_view(LIVE_BOOK_SPACED, 3, 9).unwrap();
        let compact = parse_book_update_view(LIVE_BOOK_COMPACT, 3, 9).unwrap();
        assert_eq!(spaced, compact);
    }

    #[test]
    fn scan_best_level_skips_json_whitespace() {
        // JSON whitespace after `[`, `,` and `}` — newlines and tabs too.
        let ws = b"{\"timestamp\":\"1\",\"bids\":[ \n{\"price\":\"0.1\",\t\"size\":\"1\"} ,\r\n {\"price\":\"0.2\", \"size\":\"2\"} ],\"asks\":[ ]}";
        let t = parse_book_update_view(ws, 1, 0).unwrap();
        assert_eq!(t.bid_px.raw(), 200_000);
        assert_eq!(t.bid_qty.raw(), 2_000_000);
        assert_eq!(t.ask_px.raw(), 0);
    }

    #[test]
    fn scan_best_level_rejects_malformed_separators() {
        // Leading, trailing or doubled commas and a missing comma now
        // reject — the pre-2026-10-15 walker accepted all four compact
        // forms — with or without whitespace between the levels.
        for bids in [
            r#"[,{"price":"0.1","size":"1"}]"#,
            r#"[{"price":"0.1","size":"1"},]"#,
            r#"[{"price":"0.1","size":"1"},,{"price":"0.2","size":"2"}]"#,
            r#"[{"price":"0.1","size":"1"}{"price":"0.2","size":"2"}]"#,
            r#"[{"price":"0.1","size":"1"}, ,{"price":"0.2","size":"2"}]"#,
            r#"[{"price":"0.1","size":"1"} {"price":"0.2","size":"2"}]"#,
        ] {
            let frame = format!(
                r#"{{"timestamp":"1","asks":[{{"price":"0.5","size":"1"}}],"bids":{bids}}}"#
            );
            assert!(parse_book_update_view(frame.as_bytes(), 1, 0).is_none(), "{bids}");
        }
    }

    #[test]
    fn classify_recognises_price_change() {
        assert_eq!(classify(SAMPLE_PC), FrameKind::PriceChange);
    }

    #[test]
    fn parse_price_change_row_uses_row_touch() {
        let venue_time_ms = scan_venue_time_ms(SAMPLE_PC).unwrap();
        assert_eq!(venue_time_ms, 1_713_000_000_123);
        let t = parse_price_change_row_view(SAMPLE_PC, 3, 99, venue_time_ms).unwrap();
        assert_eq!(t.sym, 3);
        assert_eq!(t.ts_ns, 99);
        assert_eq!(t.venue_seq, 0, "the run loop's sequencer sets it");
        assert_eq!(t.venue_time_ms, venue_time_ms);
        assert_eq!(t.flags, 0);
        assert_eq!(t.bid_px.raw(), 518_000);
        assert_eq!(t.ask_px.raw(), 520_000);
        // BUY row at price == best_bid ⇒ the row size IS the new
        // touch size; the far side is unknown (0).
        assert_eq!(t.bid_qty.raw(), 642_770_000);
        assert_eq!(t.ask_qty.raw(), 0);
    }

    #[test]
    fn parse_price_change_row_rejects_missing_fields() {
        let no_side =
            br#"{"asset_id":"1","price":"0.5","size":"1","best_bid":"0.5","best_ask":"0.6"}"#;
        assert!(parse_price_change_row_view(no_side, 1, 0, 0).is_none());
        let no_touch = br#"{"asset_id":"1","price":"0.5","size":"1","side":"BUY"}"#;
        assert!(parse_price_change_row_view(no_touch, 1, 0, 0).is_none());
        // SELL row away from the ask: prices known, sizes unknown.
        let away = br#"{"asset_id":"1","price":"0.4","size":"7","side":"SELL","best_bid":"0.5","best_ask":"0.6"}"#;
        let t = parse_price_change_row_view(away, 1, 0, 0).unwrap();
        assert_eq!(t.bid_qty.raw(), 0);
        assert_eq!(t.ask_qty.raw(), 0);
    }

    #[test]
    fn scan_venue_time_ms_rejects_missing_or_unquoted_garbage() {
        assert_eq!(scan_venue_time_ms(b"{\"bids\":[]}"), None);
        assert_eq!(scan_venue_time_ms(br#"{"timestamp":"soon"}"#), None);
        assert_eq!(scan_venue_time_ms(br#"{"timestamp":"1713000000123"}"#), Some(1_713_000_000_123));
    }
}

// ---------------------------------------------------------------
// Property tests — any well-formed book frame roundtrips through the
// parser without allocation.
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::views::*;
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
            let t = parse_book_update_view(buf.as_bytes(), 0, 0).unwrap();
            prop_assert_eq!(t.bid_px.raw(), bp as i64);
            prop_assert_eq!(t.bid_qty.raw(), bq as i64);
            prop_assert_eq!(t.ask_px.raw(), ap as i64);
            prop_assert_eq!(t.ask_qty.raw(), aq as i64);
            // VT2: the stamp rides in full; HK31: venue_seq is the run
            // loop's, 0 out of the parser
            prop_assert_eq!(t.venue_time_ms, ts);
            prop_assert_eq!(t.venue_seq, 0);
            prop_assert_eq!(t.flags, 0);
        }
    }
}
