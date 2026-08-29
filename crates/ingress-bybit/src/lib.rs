// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-bybit (WS9 — the sixth venue)
//!
//! Bybit v5 public WebSocket ingress. Two connection classes share
//! ONE thread and ONE tick producer (single-writer law): the SPOT
//! stream (`/v5/public/spot`) and the LINEAR-perp stream
//! (`/v5/public/linear`) — same frame grammar, different hosts/paths,
//! per-connection symbol tables (spot `BTCUSDT` and linear `BTCUSDT`
//! are DIFFERENT instruments sharing one venue name — the tables
//! disambiguate by connection).
//!
//! ## Channels (per configured symbol)
//!
//! * `orderbook.1.<SYM>` — the 1-level book IS the BBO source on both
//!   classes (spot `tickers` carries no bid/ask; linear `tickers`
//!   deltas would). Snapshot/delta protocol: the driver holds a tiny
//!   per-symbol BBO state; a delta replaces a side when present
//!   (empty side array = unchanged; a level with size 0 clears the
//!   side); a `Tick` is emitted whenever both sides are live and
//!   something changed. `u` rides as `venue_seq` (truncated).
//! * `publicTrade.<SYM>` — trade prints → `ChannelId::Trade` events
//!   (§6.5; v0 = px ×1e6, v1 = signed qty ×1e6). Bybit trade ids are
//!   UUIDs — no venue seq; `venue_seq` = 0 and NO §6.2 chain monitor
//!   exists on this venue (documented divergence; the offline audit
//!   sees cadence, not sequence).
//! * `tickers.<SYM>` — LINEAR connections only: mark/index/funding/
//!   next-funding/open-interest, all PRESENCE-FLAGGED (the venue
//!   pushes deltas carrying only changed fields) → capture events
//!   per the cross-venue conventions: `Mark` (v0 = mark ×1e6, v1 =
//!   index ×1e6 — the WS5 Binance shape), `Funding` (v0 = rate ×1e9,
//!   v1 = next-funding ms), `Ticker` (v0 = 0, v1 = OI ×1e6, venue
//!   base units — Bybit-specific mapping, see docs/wire-format.md).
//!
//! ## Subscribe / ack law
//!
//! ONE batched `{"op":"subscribe","args":[…]}` per connection. The
//! venue answers with ONE `{"success":true|false,…}` ack for the
//! WHOLE request — there is no per-topic echo, so the WS2 per-arg
//! drop machinery narrows to request granularity here: a failed ack
//! at BOOT (nothing ever confirmed on this driver) is fatal
//! (venue-blind refusal); on a reconnect it is a non-fatal drop of
//! the whole request (counted + SubDrop event) and the WS2
//! establishment budget reaps the empty session. Bybit spot/linear
//! symbols never EXPIRE (no options/dated classes in this lane), so
//! the OKX/Deribit settlement failure class cannot arise here.
//!
//! Keepalive: the venue wants a `{"op":"ping"}` text frame at least
//! every 20 s and answers with a pong-shaped frame (spot and linear
//! differ in shape; both contain `"pong"`).
//!
//! Everything after the handshake is zero-alloc: parsers slice the
//! rx buffer in place; subscribe payloads render into stack scratch;
//! the only copy is the 64-byte `Tick` moved into the ring.

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
    drive_one, note_transport_ready, run_multi, BybitConn, Driver, RunResult, State, StopFlag,
    DEFAULT_TICK_RING_CAP, RX_BUF_SIZE, TX_BUF_SIZE,
};

use core_parse::{find_field, scan_price_1e6, scan_price_1e9, scan_u64, skip_byte};
use core_types::{NsTs, SymbolId};

/// Max configured symbols per CONNECTION (spot and linear each get
/// their own table).
pub const BYBIT_MAX_SYMBOLS: usize = 64;

/// Longest venue symbol accepted (`BTCUSDT` class; generous).
pub const BYBIT_SYMBOL_MAX: usize = 24;

/// The venue keepalive probe (client → venue, ≤ 20 s cadence).
pub const PING_PAYLOAD: &[u8] = br#"{"op":"ping"}"#;

// ---------------------------------------------------------------
// Channels + classification
// ---------------------------------------------------------------

/// Public channels this ingress speaks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BybitChannel {
    /// `orderbook.1.<SYM>` — the BBO source.
    OrderbookL1 = 0,
    /// `publicTrade.<SYM>`.
    PublicTrade = 1,
    /// `tickers.<SYM>` (linear connections only).
    Tickers = 2,
}

impl BybitChannel {
    /// Topic prefix up to and including the dot before the symbol.
    #[inline]
    pub const fn topic_prefix(self) -> &'static [u8] {
        match self {
            BybitChannel::OrderbookL1 => b"orderbook.1.",
            BybitChannel::PublicTrade => b"publicTrade.",
            BybitChannel::Tickers => b"tickers.",
        }
    }
}

/// Coarse classification of one inbound text frame. Cheap byte scans
/// only — full parsing happens per-channel afterwards.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BybitMsgKind {
    /// Pong-shaped answer to our `{"op":"ping"}` (spot and linear
    /// shapes both contain `"pong"`).
    Pong,
    /// The single ack for a batched subscribe request.
    SubAck {
        /// `"success":true|false` — the WHOLE request's verdict.
        success: bool,
    },
    /// A topic push.
    Data(BybitChannel),
    /// Anything else — counted as a parse rejection by the caller.
    Unknown,
}

/// Classify one inbound payload. Zero-alloc; key-matched.
#[inline]
pub fn classify(payload: &[u8]) -> BybitMsgKind {
    if memchr::memmem::find(payload, b"\"topic\":\"").is_some() {
        if memchr::memmem::find(payload, b"\"topic\":\"orderbook.1.").is_some() {
            return BybitMsgKind::Data(BybitChannel::OrderbookL1);
        }
        if memchr::memmem::find(payload, b"\"topic\":\"publicTrade.").is_some() {
            return BybitMsgKind::Data(BybitChannel::PublicTrade);
        }
        if memchr::memmem::find(payload, b"\"topic\":\"tickers.").is_some() {
            return BybitMsgKind::Data(BybitChannel::Tickers);
        }
        return BybitMsgKind::Unknown;
    }
    // Pong shapes: linear `{"op":"pong",...}`, spot
    // `{"ret_msg":"pong","op":"ping",...}`.
    if memchr::memmem::find(payload, b"\"op\":\"pong\"").is_some()
        || memchr::memmem::find(payload, b"\"ret_msg\":\"pong\"").is_some()
    {
        return BybitMsgKind::Pong;
    }
    if memchr::memmem::find(payload, b"\"op\":\"subscribe\"").is_some() {
        if memchr::memmem::find(payload, b"\"success\":true").is_some() {
            return BybitMsgKind::SubAck { success: true };
        }
        if memchr::memmem::find(payload, b"\"success\":false").is_some() {
            return BybitMsgKind::SubAck { success: false };
        }
    }
    BybitMsgKind::Unknown
}

/// Extract the symbol bytes from a push's
/// `"topic":"<prefix><SYM>"`. Returns a subslice; no copy.
#[inline]
pub fn extract_topic_symbol(payload: &[u8], channel: BybitChannel) -> Option<&[u8]> {
    let key_pos = find_field(payload, b"\"topic\":")?;
    let val = skip_byte(payload, key_pos, b'"');
    let prefix = channel.topic_prefix();
    let rest = payload.get(val..)?;
    if !rest.starts_with(prefix) {
        return None;
    }
    let sym_start = val + prefix.len();
    let tail = payload.get(sym_start..)?;
    let rel_end = memchr::memchr(b'"', tail)?;
    payload.get(sym_start..sym_start + rel_end)
}

// ---------------------------------------------------------------
// Frames
// ---------------------------------------------------------------

/// One parsed `orderbook.1` push (BBO). Sides are PRESENCE-FLAGGED:
/// a delta may update one side only (the other array is empty), and
/// a present level with qty 0 CLEARS its side. 64-byte POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct BybitBookFrame {
    /// Venue update id (`"u"`).
    pub update_id: u64,
    /// Best bid price ×1e6 (valid when `has_bid` = 1).
    pub bid_px_1e6: i64,
    /// Best bid size ×1e6 (0 clears the side).
    pub bid_qty_1e6: i64,
    /// Best ask price ×1e6 (valid when `has_ask` = 1).
    pub ask_px_1e6: i64,
    /// Best ask size ×1e6 (0 clears the side).
    pub ask_qty_1e6: i64,
    /// 1 when the push carried a bid level.
    pub has_bid: u8,
    /// 1 when the push carried an ask level.
    pub has_ask: u8,
    /// 1 on `"type":"snapshot"` (resets both sides first).
    pub is_snapshot: u8,
    // Explicit tail padding.
    _pad: [u8; 21],
}

/// Parse one `orderbook.1` push. `None` on malformed input.
#[inline]
pub fn parse_orderbook1(payload: &[u8]) -> Option<BybitBookFrame> {
    let pos = find_field(payload, b"\"u\":")?;
    let (update_id, _) = scan_u64(payload, pos)?;
    let is_snapshot = u8::from(memchr::memmem::find(payload, b"\"type\":\"snapshot\"").is_some());

    // A side: `"b":[["<px>","<qty>"]...]` — depth-1 subscribes only
    // ever carry 0 or 1 level; level[0] is the touch.
    #[inline]
    fn side(payload: &[u8], key: &[u8]) -> Option<(u8, i64, i64)> {
        let pos = find_field(payload, key)?;
        let rest = payload.get(pos..)?;
        if !rest.starts_with(b"[") {
            return None;
        }
        if rest.starts_with(b"[]") {
            return Some((0, 0, 0)); // side absent from this delta
        }
        if !rest.starts_with(b"[[") {
            return None;
        }
        let px_at = pos + 3; // past `[["`
        let (px, after_px) = scan_price_1e6(payload, px_at)?;
        // `","` between px and qty.
        let qty_at = after_px + 3;
        let (qty, _) = scan_price_1e6(payload, qty_at)?;
        Some((1, px, qty))
    }

    let (has_bid, bid_px_1e6, bid_qty_1e6) = side(payload, b"\"b\":")?;
    let (has_ask, ask_px_1e6, ask_qty_1e6) = side(payload, b"\"a\":")?;
    Some(BybitBookFrame {
        update_id,
        bid_px_1e6,
        bid_qty_1e6,
        ask_px_1e6,
        ask_qty_1e6,
        has_bid,
        has_ask,
        is_snapshot,
        _pad: [0; 21],
    })
}

/// One parsed `publicTrade` ROW slice.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct BybitTradeFrame {
    /// Venue trade time (`"T"`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Price ×1e6.
    pub px_1e6: i64,
    /// Quantity ×1e6 (unsigned; side in `side`).
    pub qty_1e6: i64,
    /// 0 = Buy (taker bought), 1 = Sell.
    pub side: u8,
    // Explicit tail padding.
    _pad: [u8; 39],
}

/// Parse one `publicTrade` row slice (the run loop cuts rows at
/// successive `"T":` markers; every key here is matched inside the
/// row slice only). `None` on malformed input.
#[inline]
pub fn parse_trade_row(row: &[u8]) -> Option<BybitTradeFrame> {
    let pos = find_field(row, b"\"T\":")?;
    let (ts_ms, _) = scan_u64(row, pos)?;
    let pos = find_field(row, b"\"p\":")?;
    let pos = skip_byte(row, pos, b'"');
    let (px_1e6, _) = scan_price_1e6(row, pos)?;
    let pos = find_field(row, b"\"v\":")?;
    let pos = skip_byte(row, pos, b'"');
    let (qty_1e6, _) = scan_price_1e6(row, pos)?;
    let side = if memchr::memmem::find(row, b"\"S\":\"Sell\"").is_some() {
        1u8
    } else if memchr::memmem::find(row, b"\"S\":\"Buy\"").is_some() {
        0u8
    } else {
        return None;
    };
    Some(BybitTradeFrame {
        ts_ns: ts_ms.saturating_mul(1_000_000),
        px_1e6,
        qty_1e6,
        side,
        _pad: [0; 39],
    })
}

/// One parsed LINEAR `tickers` push — every field PRESENCE-FLAGGED
/// (the venue's delta pushes carry only what changed).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct BybitTickerFrame {
    /// Venue event time (`"ts"` envelope field, ms) converted to ns.
    pub ts_ns: NsTs,
    /// `markPrice` ×1e6 (valid when `has_mark`).
    pub mark_px_1e6: i64,
    /// `indexPrice` ×1e6 (valid when `has_index`).
    pub index_px_1e6: i64,
    /// `fundingRate` ×1e9, signed (valid when `has_funding`).
    pub funding_rate_1e9: i64,
    /// `nextFundingTime` ms (valid when `has_funding`; quoted ms).
    pub next_funding_ms: u64,
    /// `openInterest` ×1e6, base/contract units (valid when
    /// `has_oi`).
    pub open_interest_1e6: i64,
    /// Presence flags for the delta-tolerant fields.
    pub has_mark: u8,
    /// See `has_mark`.
    pub has_index: u8,
    /// See `has_mark`.
    pub has_funding: u8,
    /// See `has_mark`.
    pub has_oi: u8,
    // Explicit tail padding.
    _pad: [u8; 12],
}

/// Parse one LINEAR `tickers` push. `None` only when the envelope is
/// unusable (no `ts`); an all-absent delta parses with every flag 0.
#[inline]
pub fn parse_tickers(payload: &[u8]) -> Option<BybitTickerFrame> {
    let pos = find_field(payload, b"\"ts\":")?;
    let (ts_ms, _) = scan_u64(payload, pos)?;
    let mut f = BybitTickerFrame {
        ts_ns: ts_ms.saturating_mul(1_000_000),
        mark_px_1e6: 0,
        index_px_1e6: 0,
        funding_rate_1e9: 0,
        next_funding_ms: 0,
        open_interest_1e6: 0,
        has_mark: 0,
        has_index: 0,
        has_funding: 0,
        has_oi: 0,
        _pad: [0; 12],
    };
    if let Some(pos) = find_field(payload, b"\"markPrice\":") {
        let pos = skip_byte(payload, pos, b'"');
        if let Some((v, _)) = scan_price_1e6(payload, pos) {
            f.mark_px_1e6 = v;
            f.has_mark = 1;
        }
    }
    if let Some(pos) = find_field(payload, b"\"indexPrice\":") {
        let pos = skip_byte(payload, pos, b'"');
        if let Some((v, _)) = scan_price_1e6(payload, pos) {
            f.index_px_1e6 = v;
            f.has_index = 1;
        }
    }
    if let Some(pos) = find_field(payload, b"\"fundingRate\":") {
        let pos = skip_byte(payload, pos, b'"');
        if let Some((v, _)) = scan_price_1e9(payload, pos) {
            f.funding_rate_1e9 = v;
            f.has_funding = 1;
            if let Some(tpos) = find_field(payload, b"\"nextFundingTime\":") {
                let tpos = skip_byte(payload, tpos, b'"');
                if let Some((t, _)) = scan_u64(payload, tpos) {
                    f.next_funding_ms = t;
                }
            }
        }
    }
    if let Some(pos) = find_field(payload, b"\"openInterest\":") {
        let pos = skip_byte(payload, pos, b'"');
        if let Some((v, _)) = scan_price_1e6(payload, pos) {
            f.open_interest_1e6 = v;
            f.has_oi = 1;
        }
    }
    Some(f)
}

const _SIZE_CHECKS: () = {
    assert!(::core::mem::size_of::<BybitBookFrame>() == 64);
    assert!(::core::mem::size_of::<BybitTradeFrame>() == 64);
    assert!(::core::mem::size_of::<BybitTickerFrame>() == 64);
};

// ---------------------------------------------------------------
// Symbol table (per connection)
// ---------------------------------------------------------------

/// Why a [`BybitSymbolTable::insert`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbolTableErr {
    /// Table full ([`BYBIT_MAX_SYMBOLS`]).
    Full,
    /// Symbol longer than [`BYBIT_SYMBOL_MAX`].
    TooLong,
    /// Symbol empty.
    Empty,
}

/// Fixed-capacity `SYMBOL → SymbolId` map for ONE connection. Spot
/// and linear connections each own one (same venue symbol text can
/// map to different ids per class). Linear scan — N ≤ 64.
pub struct BybitSymbolTable {
    rows: [(u8, [u8; BYBIT_SYMBOL_MAX], SymbolId); BYBIT_MAX_SYMBOLS],
    len: usize,
}

impl BybitSymbolTable {
    /// Empty table.
    pub const fn new() -> Self {
        Self {
            rows: [(0, [0; BYBIT_SYMBOL_MAX], 0); BYBIT_MAX_SYMBOLS],
            len: 0,
        }
    }

    /// Register `symbol → sym` (boot-time).
    pub fn insert(&mut self, symbol: &[u8], sym: SymbolId) -> Result<(), SymbolTableErr> {
        if symbol.is_empty() {
            return Err(SymbolTableErr::Empty);
        }
        if symbol.len() > BYBIT_SYMBOL_MAX {
            return Err(SymbolTableErr::TooLong);
        }
        if self.len >= BYBIT_MAX_SYMBOLS {
            return Err(SymbolTableErr::Full);
        }
        let row = &mut self.rows[self.len];
        row.0 = symbol.len() as u8;
        row.1[..symbol.len()].copy_from_slice(symbol);
        row.2 = sym;
        self.len += 1;
        Ok(())
    }

    /// Resolve a symbol. Hot path: length gate then bytewise compare.
    #[inline]
    pub fn lookup(&self, symbol: &[u8]) -> Option<SymbolId> {
        let n = symbol.len();
        let mut i = 0;
        while i < self.len {
            let row = &self.rows[i];
            if row.0 as usize == n && &row.1[..n] == symbol {
                return Some(row.2);
            }
            i += 1;
        }
        None
    }

    /// Row accessor: `(symbol, sym)`.
    #[inline]
    pub fn get(&self, idx: usize) -> Option<(&[u8], SymbolId)> {
        if idx >= self.len {
            return None;
        }
        let row = &self.rows[idx];
        Some((&row.1[..row.0 as usize], row.2))
    }

    /// Index of a resolved sym (BBO-state slot index).
    #[inline]
    pub fn index_of(&self, sym: SymbolId) -> Option<usize> {
        let mut i = 0;
        while i < self.len {
            if self.rows[i].2 == sym {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Configured symbol count.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when no symbol is configured.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for BybitSymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Subscribe writer
// ---------------------------------------------------------------

#[inline]
fn push_bytes(dst: &mut [u8], at: usize, src: &[u8]) -> Option<usize> {
    let end = at + src.len();
    if end > dst.len() {
        return None;
    }
    dst[at..end].copy_from_slice(src);
    Some(end)
}

/// Serialize the single batched subscribe op for one connection:
/// `orderbook.1.<SYM>` + `publicTrade.<SYM>` per symbol, plus
/// `tickers.<SYM>` when `want_tickers` (linear connections). Returns
/// the byte length, `None` if `dst` is too small.
#[inline]
pub fn write_subscribe(
    dst: &mut [u8],
    symbols: &BybitSymbolTable,
    want_tickers: bool,
) -> Option<usize> {
    let mut n = push_bytes(dst, 0, b"{\"op\":\"subscribe\",\"args\":[")?;
    let mut first = true;
    let mut i = 0;
    while let Some((symbol, _sym)) = symbols.get(i) {
        let channels: [BybitChannel; 3] = [
            BybitChannel::OrderbookL1,
            BybitChannel::PublicTrade,
            BybitChannel::Tickers,
        ];
        let n_ch = if want_tickers { 3 } else { 2 };
        let mut c = 0;
        while c < n_ch {
            if !first {
                n = push_bytes(dst, n, b",")?;
            }
            first = false;
            n = push_bytes(dst, n, b"\"")?;
            n = push_bytes(dst, n, channels[c].topic_prefix())?;
            n = push_bytes(dst, n, symbol)?;
            n = push_bytes(dst, n, b"\"")?;
            c += 1;
        }
        i += 1;
    }
    n = push_bytes(dst, n, b"]}")?;
    Some(n)
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK_SNAP: &[u8] = br#"{"topic":"orderbook.1.BTCUSDT","type":"snapshot","ts":1687940967466,"data":{"s":"BTCUSDT","b":[["50005.12","403.24"]],"a":[["50006.34","0.2297"]],"u":18521288,"seq":7961638724},"cts":1687940967464}"#;
    const BOOK_DELTA_BID_ONLY: &[u8] = br#"{"topic":"orderbook.1.BTCUSDT","type":"delta","ts":1687940967470,"data":{"s":"BTCUSDT","b":[["50006.00","1.5"]],"a":[],"u":18521289,"seq":7961638725}}"#;
    const TRADES: &[u8] = br#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1672304486868,"data":[{"T":1672304486865,"s":"BTCUSDT","S":"Buy","v":"0.001","p":"16578.50","L":"PlusTick","i":"20f43950-d8dd-5b31-9112-a178eb6023af","BT":false},{"T":1672304486866,"s":"BTCUSDT","S":"Sell","v":"0.5","p":"16578.00","L":"MinusTick","i":"3d8f2a1c-aaaa-bbbb-cccc-a178eb6023af","BT":false}]}"#;
    const TICKERS_SNAP: &[u8] = br#"{"topic":"tickers.BTCUSDT","type":"snapshot","cs":24987956059,"ts":1673272861686,"data":{"symbol":"BTCUSDT","tickDirection":"PlusTick","price24hPcnt":"0.017103","lastPrice":"17216.00","markPrice":"17217.33","indexPrice":"17227.36","openInterest":"68744.761","openInterestValue":"1183601235.91","turnover24h":"1570383121.943499","volume24h":"91705.276","fundingRate":"-0.000212","nextFundingTime":"1673280000000","bid1Price":"17215.50","bid1Size":"2.752","ask1Price":"17216.00","ask1Size":"28.981"}}"#;
    const TICKERS_DELTA: &[u8] = br#"{"topic":"tickers.BTCUSDT","type":"delta","cs":24987956060,"ts":1673272862690,"data":{"symbol":"BTCUSDT","markPrice":"17218.01"}}"#;
    const SUB_OK: &[u8] = br#"{"success":true,"ret_msg":"subscribe","conn_id":"2324d924-aa4d-45b0-a858-7b8be29ab52b","req_id":"10001","op":"subscribe"}"#;
    const SUB_FAIL: &[u8] = br#"{"success":false,"ret_msg":"error:handler not found,topic:orderbook.1.NOPEUSDT","conn_id":"x","op":"subscribe"}"#;
    const PONG_LINEAR: &[u8] = br#"{"op":"pong","args":["1675418560633"],"conn_id":"x"}"#;
    const PONG_SPOT: &[u8] = br#"{"success":true,"ret_msg":"pong","conn_id":"x","op":"ping"}"#;

    #[test]
    fn classify_covers_the_grammar() {
        assert_eq!(
            classify(BOOK_SNAP),
            BybitMsgKind::Data(BybitChannel::OrderbookL1)
        );
        assert_eq!(
            classify(TRADES),
            BybitMsgKind::Data(BybitChannel::PublicTrade)
        );
        assert_eq!(
            classify(TICKERS_SNAP),
            BybitMsgKind::Data(BybitChannel::Tickers)
        );
        assert_eq!(classify(SUB_OK), BybitMsgKind::SubAck { success: true });
        assert_eq!(classify(SUB_FAIL), BybitMsgKind::SubAck { success: false });
        assert_eq!(classify(PONG_LINEAR), BybitMsgKind::Pong);
        assert_eq!(classify(PONG_SPOT), BybitMsgKind::Pong);
        assert_eq!(classify(b"{\"nonsense\":true}"), BybitMsgKind::Unknown);
        assert_eq!(
            classify(br#"{"topic":"kline.1.BTCUSDT","data":[]}"#),
            BybitMsgKind::Unknown,
            "unknown topics reject"
        );
    }

    #[test]
    fn extract_topic_symbol_per_channel() {
        assert_eq!(
            extract_topic_symbol(BOOK_SNAP, BybitChannel::OrderbookL1),
            Some(&b"BTCUSDT"[..])
        );
        assert_eq!(
            extract_topic_symbol(TRADES, BybitChannel::PublicTrade),
            Some(&b"BTCUSDT"[..])
        );
        assert_eq!(
            extract_topic_symbol(TICKERS_SNAP, BybitChannel::Tickers),
            Some(&b"BTCUSDT"[..])
        );
        assert_eq!(extract_topic_symbol(BOOK_SNAP, BybitChannel::Tickers), None);
    }

    #[test]
    fn parse_orderbook1_snapshot_and_one_sided_delta() {
        let f = parse_orderbook1(BOOK_SNAP).unwrap();
        assert_eq!(f.is_snapshot, 1);
        assert_eq!((f.has_bid, f.has_ask), (1, 1));
        assert_eq!(f.bid_px_1e6, 50_005_120_000);
        assert_eq!(f.bid_qty_1e6, 403_240_000);
        assert_eq!(f.ask_px_1e6, 50_006_340_000);
        assert_eq!(f.ask_qty_1e6, 229_700);
        assert_eq!(f.update_id, 18_521_288);

        let d = parse_orderbook1(BOOK_DELTA_BID_ONLY).unwrap();
        assert_eq!(d.is_snapshot, 0);
        assert_eq!(
            (d.has_bid, d.has_ask),
            (1, 0),
            "empty ask array = unchanged"
        );
        assert_eq!(d.bid_px_1e6, 50_006_000_000);

        assert!(parse_orderbook1(b"{}").is_none());
    }

    #[test]
    fn parse_orderbook1_zero_size_clears_a_side() {
        let z = br#"{"topic":"orderbook.1.X","type":"delta","data":{"s":"X","b":[["50005.12","0"]],"a":[],"u":2}}"#;
        let f = parse_orderbook1(z).unwrap();
        assert_eq!(f.has_bid, 1);
        assert_eq!(f.bid_qty_1e6, 0, "explicit zero size = side cleared");
    }

    #[test]
    fn parse_trade_rows_and_sides() {
        // Row slicing the run-loop way: at successive `"T":` markers.
        let first_t = memchr::memmem::find(TRADES, b"\"T\":").unwrap();
        let second_t =
            first_t + 4 + memchr::memmem::find(&TRADES[first_t + 4..], b"\"T\":").unwrap();
        let row1 = &TRADES[first_t..second_t];
        let row2 = &TRADES[second_t..];
        let t1 = parse_trade_row(row1).unwrap();
        assert_eq!(t1.side, 0);
        assert_eq!(t1.px_1e6, 16_578_500_000);
        assert_eq!(t1.qty_1e6, 1_000);
        assert_eq!(t1.ts_ns, 1_672_304_486_865 * 1_000_000);
        let t2 = parse_trade_row(row2).unwrap();
        assert_eq!(t2.side, 1);
        assert_eq!(t2.qty_1e6, 500_000);
        assert!(parse_trade_row(b"{\"T\":1}").is_none(), "price required");
    }

    #[test]
    fn parse_tickers_snapshot_delta_and_flags() {
        let s = parse_tickers(TICKERS_SNAP).unwrap();
        assert_eq!(
            (s.has_mark, s.has_index, s.has_funding, s.has_oi),
            (1, 1, 1, 1)
        );
        assert_eq!(s.mark_px_1e6, 17_217_330_000);
        assert_eq!(s.index_px_1e6, 17_227_360_000);
        assert_eq!(s.funding_rate_1e9, -212_000);
        assert_eq!(s.next_funding_ms, 1_673_280_000_000);
        assert_eq!(s.open_interest_1e6, 68_744_761_000);
        assert_eq!(s.ts_ns, 1_673_272_861_686 * 1_000_000);

        let d = parse_tickers(TICKERS_DELTA).unwrap();
        assert_eq!(
            (d.has_mark, d.has_index, d.has_funding, d.has_oi),
            (1, 0, 0, 0)
        );
        assert_eq!(d.mark_px_1e6, 17_218_010_000);
        assert!(parse_tickers(b"{}").is_none(), "envelope ts required");
    }

    #[test]
    fn symbol_table_laws() {
        let mut t = BybitSymbolTable::new();
        assert!(t.is_empty());
        t.insert(b"BTCUSDT", 1).unwrap();
        t.insert(b"ETHUSDT", 2).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t.lookup(b"BTCUSDT"), Some(1));
        assert_eq!(t.lookup(b"NOPE"), None);
        assert_eq!(t.index_of(2), Some(1));
        assert_eq!(t.get(0).unwrap().0, b"BTCUSDT");
        assert_eq!(t.insert(b"", 3), Err(SymbolTableErr::Empty));
        assert_eq!(
            t.insert(b"AAAAAAAAAAAAAAAAAAAAAAAAA", 3),
            Err(SymbolTableErr::TooLong)
        );
        let mut full = BybitSymbolTable::new();
        let mut k = 0u32;
        while k < BYBIT_MAX_SYMBOLS as u32 {
            let name = [b'A' + ((k / 26) % 26) as u8, b'A' + (k % 26) as u8];
            full.insert(&name, k + 1).unwrap();
            k += 1;
        }
        assert_eq!(full.insert(b"OVER", 999), Err(SymbolTableErr::Full));
    }

    #[test]
    fn write_subscribe_renders_both_classes() {
        let mut t = BybitSymbolTable::new();
        t.insert(b"BTCUSDT", 1).unwrap();
        t.insert(b"ETHUSDT", 2).unwrap();
        let mut buf = [0u8; 1024];
        // Spot connection: no tickers.
        let n = write_subscribe(&mut buf, &t, false).unwrap();
        assert_eq!(
            &buf[..n],
            br#"{"op":"subscribe","args":["orderbook.1.BTCUSDT","publicTrade.BTCUSDT","orderbook.1.ETHUSDT","publicTrade.ETHUSDT"]}"#
                as &[u8]
        );
        // Linear connection: + tickers per symbol.
        let n = write_subscribe(&mut buf, &t, true).unwrap();
        let s = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(s.contains("\"tickers.BTCUSDT\""));
        assert!(s.contains("\"tickers.ETHUSDT\""));
        // Tiny buffer fails clean.
        let mut tiny = [0u8; 8];
        assert!(write_subscribe(&mut tiny, &t, false).is_none());
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn orderbook1_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = parse_orderbook1(&buf);
        }

        #[test]
        fn trade_row_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = parse_trade_row(&buf);
        }

        #[test]
        fn tickers_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = parse_tickers(&buf);
        }

        #[test]
        fn orderbook1_roundtrips(
            u in 0u64..10_000_000_000u64,
            bp in 1u32..999_999u32,
            bq in 1u32..999_999u32,
            ap in 1u32..999_999u32,
            aq in 1u32..999_999u32,
        ) {
            let mut buf = String::with_capacity(256);
            use std::fmt::Write;
            write!(
                &mut buf,
                r#"{{"topic":"orderbook.1.X","type":"snapshot","data":{{"s":"X","b":[["0.{bp:06}","0.{bq:06}"]],"a":[["0.{ap:06}","0.{aq:06}"]],"u":{u}}}}}"#,
            ).unwrap();
            let f = parse_orderbook1(buf.as_bytes()).unwrap();
            prop_assert_eq!(f.update_id, u);
            prop_assert_eq!(f.bid_px_1e6, bp as i64);
            prop_assert_eq!(f.bid_qty_1e6, bq as i64);
            prop_assert_eq!(f.ask_px_1e6, ap as i64);
            prop_assert_eq!(f.ask_qty_1e6, aq as i64);
            prop_assert_eq!((f.has_bid, f.has_ask, f.is_snapshot), (1, 1, 1));
        }
    }
}
