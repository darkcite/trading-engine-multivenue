// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # MEXC futures — plain JSON over WS TEXT (MX4)
//!
//! The wire (plan §1.2, measured uncompressed):
//!
//! ```jsonc
//! {"symbol":"BTC_USDT","data":{"cts":1789897581009,
//!   "asks":[[80468.7,3446,2],…],"bids":[[80468.6,31288,7],…],
//!   "version":41925002140},"channel":"push.depth.full","ts":1789897581013}
//! {"symbol":"BTC_USDT","data":[{"p":80489,"v":11,"T":1,"O":3,"M":1,
//!   "t":1789897547210,"i":"16270106116","cts":"1789897547210"}],"channel":"push.deal","ts":…}
//! {"symbol":"XAU_USDT","data":{"fairPrice":4377.21,"indexPrice":4377.63,
//!   "fundingRate":0,"holdVol":92415393,"timestamp":1789897545754,…},"channel":"push.ticker","ts":…}
//! {"channel":"rs.sub.depth.full","data":"success","ts":…}      // the ack: NO symbol
//! {"channel":"pong","data":1789897545754,"ts":…}
//! ```
//!
//! Every number is a BARE JSON number that may be an integer (`80489`),
//! read with the exponent-tolerant `core_parse::scan_number_sci_*`
//! scanners (a quoted number is tolerated too). Depth levels are
//! `[price, volume (contracts), orderCount]`, best first. Everything is
//! a key-matched in-place scan; nothing allocates or copies.

use core_net::{WsPayload, WsWriteErr};
use core_parse::{
    find_field, scan_number_sci_1e6, scan_number_sci_1e9, skip_json_value, skip_ws,
};

use crate::{scan_u64_checked, MexcChannel, MexcDeal, DEAL_SIDE_BUY, DEAL_SIDE_SELL};

// ---------------------------------------------------------------
// Number readers (bare — the wire — or quoted, tolerated)
// ---------------------------------------------------------------

/// A JSON number at `pos` ×1e6 → `(value, pos after)`.
#[inline]
fn json_num_1e6(buf: &[u8], pos: usize) -> Option<(i64, usize)> {
    let p = skip_ws(buf, pos);
    if buf.get(p) == Some(&b'"') {
        let (v, e) = scan_number_sci_1e6(buf, p + 1)?;
        if buf.get(e) != Some(&b'"') {
            return None;
        }
        Some((v, e + 1))
    } else {
        scan_number_sci_1e6(buf, p)
    }
}

/// A JSON number at `pos` ×1e9 → `(value, pos after)`.
#[inline]
fn json_num_1e9(buf: &[u8], pos: usize) -> Option<(i64, usize)> {
    let p = skip_ws(buf, pos);
    if buf.get(p) == Some(&b'"') {
        let (v, e) = scan_number_sci_1e9(buf, p + 1)?;
        if buf.get(e) != Some(&b'"') {
            return None;
        }
        Some((v, e + 1))
    } else {
        scan_number_sci_1e9(buf, p)
    }
}

/// A JSON unsigned integer at `pos` (bare or quoted digits; checked,
/// never wraps) → `(value, pos after)`. A fraction or exponent after
/// the digits is NOT an integer and rejects.
#[inline]
fn json_u64(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let p = skip_ws(buf, pos);
    let quoted = buf.get(p) == Some(&b'"');
    let start = if quoted { p + 1 } else { p };
    let (v, e) = scan_u64_checked(buf, start)?;
    if quoted {
        if buf.get(e) != Some(&b'"') {
            return None;
        }
        return Some((v, e + 1));
    }
    if matches!(buf.get(e), Some(b'.' | b'e' | b'E')) {
        return None;
    }
    Some((v, e))
}

/// `key` (e.g. `b"\"ts\":"`) as an unsigned integer, 0 when absent or
/// unreadable.
#[inline]
fn field_u64(buf: &[u8], key: &[u8]) -> u64 {
    match find_field(buf, key) {
        Some(p) => match json_u64(buf, p) {
            Some((v, _)) => v,
            None => 0,
        },
        None => 0,
    }
}

// ---------------------------------------------------------------
// Classification
// ---------------------------------------------------------------

/// Coarse classification of one inbound futures frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MexcFutKind {
    /// `{"channel":"pong",…}` — the answer to `{"method":"ping"}`.
    Pong,
    /// `{"channel":"rs.sub.<ch>","data":…}` — a subscribe ack. It
    /// carries NO symbol (confirmation is first data, plan §4 D7).
    SubAck {
        /// `"data":"success"`.
        success: bool,
        /// The acked channel, `None` for one this ingress never asks.
        channel: Option<MexcChannel>,
    },
    /// `{"channel":"rs.error",…}` — a venue error notice: a refused
    /// contract ([`extract_refused_contract`]) or a session notice.
    RequestError,
    /// A push on one of the three data channels.
    Data(MexcChannel),
    /// Anything else — counted as a parse rejection by the caller.
    Unknown,
}

/// The `"channel"` string value (no escapes on this wire).
#[inline]
fn channel_value(payload: &[u8]) -> Option<&[u8]> {
    let pos = skip_ws(payload, find_field(payload, b"\"channel\":")?);
    if payload.get(pos) != Some(&b'"') {
        return None;
    }
    let start = pos + 1;
    let rel = memchr::memchr(b'"', payload.get(start..)?)?;
    payload.get(start..start + rel)
}

/// Classify one futures text frame by its exact `"channel"` value.
/// Zero-alloc.
#[inline]
pub fn classify_futures(payload: &[u8]) -> MexcFutKind {
    let Some(ch) = channel_value(payload) else {
        return MexcFutKind::Unknown;
    };
    match ch {
        b"push.depth.full" => MexcFutKind::Data(MexcChannel::FutDepthFull),
        b"push.deal" => MexcFutKind::Data(MexcChannel::FutDeal),
        b"push.ticker" => MexcFutKind::Data(MexcChannel::FutTicker),
        b"pong" => MexcFutKind::Pong,
        b"rs.error" => MexcFutKind::RequestError,
        _ => match ch.strip_prefix(b"rs.sub.") {
            Some(acked) => MexcFutKind::SubAck {
                success: data_is_success(payload),
                channel: match acked {
                    b"depth.full" => Some(MexcChannel::FutDepthFull),
                    b"deal" => Some(MexcChannel::FutDeal),
                    b"ticker" => Some(MexcChannel::FutTicker),
                    _ => None,
                },
            },
            None => MexcFutKind::Unknown,
        },
    }
}

/// `"data":"success"` exactly.
#[inline]
fn data_is_success(payload: &[u8]) -> bool {
    match find_field(payload, b"\"data\":") {
        Some(p) => {
            let p = skip_ws(payload, p);
            payload.get(p..).is_some_and(|r| r.starts_with(b"\"success\""))
        }
        None => false,
    }
}

/// The push's `"symbol"` value — a subslice, no copy. `None` when
/// absent, empty or not a string.
#[inline]
pub fn extract_fut_symbol(payload: &[u8]) -> Option<&[u8]> {
    let pos = skip_ws(payload, find_field(payload, b"\"symbol\":")?);
    if payload.get(pos) != Some(&b'"') {
        return None;
    }
    let start = pos + 1;
    let rel = memchr::memchr(b'"', payload.get(start..)?)?;
    if rel == 0 {
        return None;
    }
    payload.get(start..start + rel)
}

/// The contract an `rs.error` refuses, from its `data` text —
/// measured live 2026-09-23: `{"channel":"rs.error","data":"Contract
/// [NOPE_USDT] not exists","ts":…}`. `None` for any other `rs.error`
/// (e.g. the heartbeat notice `"more than 60 seconds no response,
/// close the channel"`, which refuses nothing). A subslice; no copy.
#[inline]
pub fn extract_refused_contract(payload: &[u8]) -> Option<&[u8]> {
    const OPEN: &[u8] = b"\"data\":\"Contract [";
    let start = memchr::memmem::find(payload, OPEN)? + OPEN.len();
    let rel = memchr::memchr(b']', payload.get(start..)?)?;
    if rel == 0 {
        return None;
    }
    let sym = payload.get(start..start + rel)?;
    if memchr::memchr(b'"', sym).is_some() {
        return None; // the bracket closed outside the data string
    }
    Some(sym)
}

/// The envelope `"ts"` (send time, ms); 0 when absent. `"ts":` never
/// false-matches inside `"cts":` (`find_field` checks the byte before
/// the key's opening quote).
#[inline]
pub fn extract_fut_ts_ms(payload: &[u8]) -> u64 {
    field_u64(payload, b"\"ts\":")
}

// ---------------------------------------------------------------
// depth.full — the futures BBO
// ---------------------------------------------------------------

/// One parsed `push.depth.full` (top level of each side). A side is
/// PRESENCE-FLAGGED: an empty array is not an error, it is simply no
/// BBO (no tick). 64-byte POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct MexcDepthFrame {
    /// Book `version` (0 = absent).
    pub version: u64,
    /// Best bid ×1e6 (valid when `has_bid`).
    pub bid_px_1e6: i64,
    /// Best bid volume ×1e6, venue CONTRACTS.
    pub bid_qty_1e6: i64,
    /// Best ask ×1e6 (valid when `has_ask`).
    pub ask_px_1e6: i64,
    /// Best ask volume ×1e6, venue contracts.
    pub ask_qty_1e6: i64,
    /// `data.cts` (matching-engine time) when present, else the
    /// envelope `ts`, else 0 ("unknown, never stale").
    pub venue_time_ms: u64,
    /// 1 when `bids[0]` exists.
    pub has_bid: u8,
    /// 1 when `asks[0]` exists.
    pub has_ask: u8,
    // Explicit tail padding.
    _pad: [u8; 14],
}

impl MexcDepthFrame {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        version: 0,
        bid_px_1e6: 0,
        bid_qty_1e6: 0,
        ask_px_1e6: 0,
        ask_qty_1e6: 0,
        venue_time_ms: 0,
        has_bid: 0,
        has_ask: 0,
        _pad: [0; 14],
    };
}

/// The best level of one side: `"<key>":[[px,vol,count],…]` → `(1, px,
/// vol)`, `[]` → `(0, 0, 0)`.
#[inline]
fn side_top(payload: &[u8], key: &[u8]) -> Option<(u8, i64, i64)> {
    let pos = skip_ws(payload, find_field(payload, key)?);
    if payload.get(pos) != Some(&b'[') {
        return None;
    }
    let i = skip_ws(payload, pos + 1);
    match payload.get(i) {
        Some(b']') => return Some((0, 0, 0)),
        Some(b'[') => {}
        _ => return None,
    }
    let (px, e) = json_num_1e6(payload, i + 1)?;
    let c = skip_ws(payload, e);
    if payload.get(c) != Some(&b',') {
        return None;
    }
    let (qty, e2) = json_num_1e6(payload, c + 1)?;
    let t = skip_ws(payload, e2);
    if !matches!(payload.get(t), Some(b',' | b']')) {
        return None;
    }
    if px < 0 || qty < 0 {
        return None;
    }
    Some((1, px, qty))
}

/// Parse one `push.depth.full`. Both side arrays must be present
/// (either may be empty); `version` is optional.
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_depth_full(payload: &[u8], out: &mut MexcDepthFrame) -> bool {
    parse_depth_full_fill(payload, out).is_some()
}

/// [`parse_depth_full`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_depth_full_fill(payload: &[u8], out: &mut MexcDepthFrame) -> Option<()> {
    let (has_ask, ask_px_1e6, ask_qty_1e6) = side_top(payload, b"\"asks\":")?;
    let (has_bid, bid_px_1e6, bid_qty_1e6) = side_top(payload, b"\"bids\":")?;
    let cts = field_u64(payload, b"\"cts\":");
    let venue_time_ms = if cts > 0 { cts } else { extract_fut_ts_ms(payload) };
    *out = MexcDepthFrame {
        version: field_u64(payload, b"\"version\":"),
        bid_px_1e6,
        bid_qty_1e6,
        ask_px_1e6,
        ask_qty_1e6,
        venue_time_ms,
        has_bid,
        has_ask,
        _pad: [0; 14],
    };
    Some(())
}

// ---------------------------------------------------------------
// deal — futures prints
// ---------------------------------------------------------------

/// Walk over a `push.deal` payload's `"data"`: an ARRAY of item objects
/// (the measured shape) or a SINGLE object (tolerated). Yields each item
/// object's span; stops for good at the end or on malformed structure
/// ([`Self::is_malformed`]). Zero-alloc, zero-copy.
pub struct MexcFutDealsWalk<'a> {
    buf: &'a [u8],
    pos: usize,
    single: bool,
    done: bool,
    malformed: bool,
}

impl<'a> MexcFutDealsWalk<'a> {
    /// Start a walk over one `push.deal` payload.
    #[inline]
    pub fn new(payload: &'a [u8]) -> Self {
        let mut w = Self {
            buf: payload,
            pos: 0,
            single: false,
            done: false,
            malformed: false,
        };
        match find_field(payload, b"\"data\":") {
            Some(p) => {
                let p = skip_ws(payload, p);
                match payload.get(p) {
                    Some(b'[') => w.pos = p + 1,
                    Some(b'{') => {
                        w.pos = p;
                        w.single = true;
                    }
                    _ => w.malformed = true,
                }
            }
            None => w.malformed = true,
        }
        w
    }

    /// The next item object's span, `None` at the end or on malformed
    /// structure.
    #[inline]
    pub fn next_item(&mut self) -> Option<&'a [u8]> {
        if self.done || self.malformed {
            return None;
        }
        if self.single {
            self.done = true;
            return match skip_json_value(self.buf, self.pos) {
                Some(e) => self.buf.get(self.pos..e),
                None => {
                    self.malformed = true;
                    None
                }
            };
        }
        loop {
            let i = skip_ws(self.buf, self.pos);
            match self.buf.get(i) {
                Some(b']') => {
                    self.done = true;
                    return None;
                }
                Some(b',') => self.pos = i + 1,
                Some(b'{') => match skip_json_value(self.buf, i) {
                    Some(e) => {
                        self.pos = e;
                        return self.buf.get(i..e);
                    }
                    None => {
                        self.malformed = true;
                        return None;
                    }
                },
                _ => {
                    self.malformed = true;
                    return None;
                }
            }
        }
    }

    /// True once the walk stopped on malformed structure.
    #[inline]
    pub const fn is_malformed(&self) -> bool {
        self.malformed
    }
}

/// Parse one futures deal ITEM object. `p`, `v` and `T` (1 buy / 2
/// sell — anything else rejects) are required; `t` (ms) and `i` (the
/// numeric trade id, quoted on the wire) are optional — 0 when absent
/// or unreadable.
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_fut_deal_item(item: &[u8], out: &mut MexcDeal) -> bool {
    parse_fut_deal_item_fill(item, out).is_some()
}

/// [`parse_fut_deal_item`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_fut_deal_item_fill(item: &[u8], out: &mut MexcDeal) -> Option<()> {
    let (px, _) = json_num_1e6(item, find_field(item, b"\"p\":")?)?;
    let (qty, _) = json_num_1e6(item, find_field(item, b"\"v\":")?)?;
    if px < 0 || qty < 0 {
        return None;
    }
    let (t, _) = json_u64(item, find_field(item, b"\"T\":")?)?;
    let side = match t {
        1 => DEAL_SIDE_BUY,
        2 => DEAL_SIDE_SELL,
        _ => return None,
    };
    *out = MexcDeal::new(
        px,
        qty,
        field_u64(item, b"\"t\":"),
        field_u64(item, b"\"i\":"),
        side,
    );
    Some(())
}

// ---------------------------------------------------------------
// ticker — mark / funding / open interest (capture + funding lane)
// ---------------------------------------------------------------

/// One parsed `push.ticker` — every field PRESENCE-FLAGGED. 64-byte
/// POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct MexcTickerFrame {
    /// `data.timestamp` ms when present, else the envelope `ts`, else 0.
    pub venue_time_ms: u64,
    /// `fairPrice` ×1e6 (the mark; valid when `has_fair`).
    pub fair_px_1e6: i64,
    /// `indexPrice` ×1e6 (valid when `has_index`).
    pub index_px_1e6: i64,
    /// `fundingRate` ×1e9, signed (valid when `has_funding`).
    pub funding_rate_1e9: i64,
    /// `holdVol` ×1e6 — open interest in venue CONTRACTS (valid when
    /// `has_hold_vol`).
    pub hold_vol_1e6: i64,
    /// Presence flag for `fair_px_1e6`.
    pub has_fair: u8,
    /// Presence flag for `index_px_1e6`.
    pub has_index: u8,
    /// Presence flag for `funding_rate_1e9`.
    pub has_funding: u8,
    /// Presence flag for `hold_vol_1e6`.
    pub has_hold_vol: u8,
    // Explicit tail padding.
    _pad: [u8; 20],
}

impl MexcTickerFrame {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        venue_time_ms: 0,
        fair_px_1e6: 0,
        index_px_1e6: 0,
        funding_rate_1e9: 0,
        hold_vol_1e6: 0,
        has_fair: 0,
        has_index: 0,
        has_funding: 0,
        has_hold_vol: 0,
        _pad: [0; 20],
    };
}

/// Parse one `push.ticker`. `false` only when `"data"` is not an object;
/// keys are matched INSIDE the data object only.
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_ticker(payload: &[u8], out: &mut MexcTickerFrame) -> bool {
    parse_ticker_fill(payload, out).is_some()
}

/// [`parse_ticker`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_ticker_fill(payload: &[u8], out: &mut MexcTickerFrame) -> Option<()> {
    let dpos = skip_ws(payload, find_field(payload, b"\"data\":")?);
    if payload.get(dpos) != Some(&b'{') {
        return None;
    }
    let dend = skip_json_value(payload, dpos)?;
    let data = payload.get(dpos..dend)?;
    let mut f = MexcTickerFrame {
        venue_time_ms: 0,
        fair_px_1e6: 0,
        index_px_1e6: 0,
        funding_rate_1e9: 0,
        hold_vol_1e6: 0,
        has_fair: 0,
        has_index: 0,
        has_funding: 0,
        has_hold_vol: 0,
        _pad: [0; 20],
    };
    if let Some((v, _)) = find_field(data, b"\"fairPrice\":").and_then(|p| json_num_1e6(data, p)) {
        f.fair_px_1e6 = v;
        f.has_fair = 1;
    }
    if let Some((v, _)) = find_field(data, b"\"indexPrice\":").and_then(|p| json_num_1e6(data, p)) {
        f.index_px_1e6 = v;
        f.has_index = 1;
    }
    if let Some((v, _)) = find_field(data, b"\"fundingRate\":").and_then(|p| json_num_1e9(data, p)) {
        f.funding_rate_1e9 = v;
        f.has_funding = 1;
    }
    if let Some((v, _)) = find_field(data, b"\"holdVol\":").and_then(|p| json_num_1e6(data, p)) {
        f.hold_vol_1e6 = v;
        f.has_hold_vol = 1;
    }
    let ts = field_u64(data, b"\"timestamp\":");
    f.venue_time_ms = if ts > 0 { ts } else { extract_fut_ts_ms(payload) };
    *out = f;
    Some(())
}

const _POD_SIZES: () = {
    assert!(::core::mem::size_of::<MexcDepthFrame>() == 64);
    assert!(::core::mem::size_of::<MexcTickerFrame>() == 64);
};

// ---------------------------------------------------------------
// Subscribe writer — ONE frame per (symbol, channel)
// ---------------------------------------------------------------

/// Render one futures subscribe frame through `p`, straight into its
/// frame, header first (`core_net::queue_masked_text_frame_rendered`):
/// `{"method":"sub.depth.full","param":{"symbol":"BTC_USDT","limit":5}}`,
/// `{"method":"sub.deal","param":{"symbol":"BTC_USDT"}}` or
/// `{"method":"sub.ticker","param":{"symbol":"BTC_USDT"}}`. `channel`
/// must be a futures channel.
///
/// # Errors
/// [`WsWriteErr::BufferTooSmall`] when `p` runs out of room.
#[inline]
pub fn render_fut_subscribe(
    p: &mut WsPayload<'_>,
    channel: MexcChannel,
    symbol: &[u8],
) -> Result<(), WsWriteErr> {
    debug_assert!(
        channel.class() == crate::MexcClass::Futures,
        "a futures subscribe needs a futures channel"
    );
    p.put(b"{\"method\":\"")?;
    p.put(channel.topic())?;
    p.put(b"\",\"param\":{\"symbol\":\"")?;
    p.put(symbol)?;
    p.put(b"\"")?;
    if channel == MexcChannel::FutDepthFull {
        p.put(b",\"limit\":5")?;
    }
    p.put(b"}}")
}

/// Longest rendered futures subscribe payload (depth.full at the
/// longest accepted symbol) — sizes the tx budget.
pub const FUT_SUB_PAYLOAD_MAX: usize = br#"{"method":"sub.depth.full","param":{"symbol":"","limit":5}}"#.len()
    + crate::MEXC_SYMBOL_MAX;

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

/// Test views in the old by-value shape, shared by the unit and the
/// property tests: each wraps one in-place parser and hands back its
/// frame's `Option`, so assertions read naturally. Cold — production
/// callers parse in place.
#[cfg(test)]
mod views {
    // COPY: `Option<MexcDepthFrame>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_depth_full_view(payload: &[u8]) -> Option<crate::futures::MexcDepthFrame> {
        let mut f = crate::futures::MexcDepthFrame::ZERO;
        crate::futures::parse_depth_full(payload, &mut f).then_some(f)
    }
    // COPY: `Option<MexcDeal>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_fut_deal_item_view(item: &[u8]) -> Option<crate::MexcDeal> {
        let mut f = crate::MexcDeal::ZERO;
        crate::futures::parse_fut_deal_item(item, &mut f).then_some(f)
    }
    // COPY: `Option<MexcTickerFrame>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_ticker_view(payload: &[u8]) -> Option<crate::futures::MexcTickerFrame> {
        let mut f = crate::futures::MexcTickerFrame::ZERO;
        crate::futures::parse_ticker(payload, &mut f).then_some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::views::*;

    /// One futures subscribe rendered into a plain buffer — exactly what
    /// the frame's payload span receives; `None` when it does not fit.
    fn write_fut_subscribe(buf: &mut [u8], channel: MexcChannel, symbol: &[u8]) -> Option<usize> {
        let mut p = WsPayload::writing(buf);
        render_fut_subscribe(&mut p, channel, symbol).ok()?;
        Some(p.len())
    }

    // Plan §1.2, verbatim shapes.
    const DEPTH: &[u8] = br#"{"symbol":"BTC_USDT","data":{"cts":1789897581009,"asks":[[80468.7,3446,2],[80469.1,1239,1]],"bids":[[80468.6,31288,7],[80468.5,10,1]],"version":41925002140},"channel":"push.depth.full","ts":1789897581013}"#;
    const DEAL: &[u8] = br#"{"symbol":"BTC_USDT","data":[{"p":80489,"v":11,"T":1,"O":3,"M":1,"t":1789897547210,"i":"16270106116","cts":"1789897547210"},{"p":80488.5,"v":2,"T":2,"O":3,"M":2,"t":1789897547211,"i":"16270106117","cts":"1789897547211"}],"channel":"push.deal","ts":1789897547215}"#;
    const TICKER: &[u8] = br#"{"symbol":"XAU_USDT","data":{"symbol":"XAU_USDT","lastPrice":4377.18,"bid1":4377.16,"ask1":4377.2,"indexPrice":4377.63,"fairPrice":4377.21,"fundingRate":0,"holdVol":92415393,"volume24":24315543,"amount24":106460536.13,"riseFallRates":{"zone":"UTC+8","r":0.001},"timestamp":1789897545754},"channel":"push.ticker","ts":1789897545760}"#;

    #[test]
    fn extract_refused_contract_reads_the_live_rs_error() {
        assert_eq!(
            extract_refused_contract(br#"{"channel":"rs.error","data":"Contract [NOPE_USDT] not exists","ts":1790148334500}"#),
            Some(&b"NOPE_USDT"[..])
        );
        assert_eq!(
            extract_refused_contract(br#"{"channel":"rs.error","data":"more than 60 seconds no response, close the channel","ts":1}"#),
            None
        );
        assert_eq!(extract_refused_contract(br#"{"data":"Contract []"}"#), None, "empty");
        assert_eq!(extract_refused_contract(br#"{"data":"Contract [X"}"#), None, "unclosed");
        assert_eq!(extract_refused_contract(br#"{"data":"Contract [X","y":"]"}"#), None, "bracket outside the string");
    }

    #[test]
    fn classify_futures_covers_the_grammar() {
        assert_eq!(classify_futures(DEPTH), MexcFutKind::Data(MexcChannel::FutDepthFull));
        assert_eq!(classify_futures(DEAL), MexcFutKind::Data(MexcChannel::FutDeal));
        assert_eq!(classify_futures(TICKER), MexcFutKind::Data(MexcChannel::FutTicker));
        assert_eq!(
            classify_futures(br#"{"channel":"pong","data":1789897545754,"ts":1789897545754}"#),
            MexcFutKind::Pong
        );
        assert_eq!(
            classify_futures(br#"{"channel":"rs.sub.depth.full","data":"success","ts":1}"#),
            MexcFutKind::SubAck { success: true, channel: Some(MexcChannel::FutDepthFull) }
        );
        assert_eq!(
            classify_futures(br#"{"channel":"rs.sub.deal","data":"contract not exists","ts":1}"#),
            MexcFutKind::SubAck { success: false, channel: Some(MexcChannel::FutDeal) }
        );
        assert_eq!(
            classify_futures(br#"{"channel":"rs.sub.kline","data":"success","ts":1}"#),
            MexcFutKind::SubAck { success: true, channel: None }
        );
        assert_eq!(
            classify_futures(br#"{"channel":"rs.error","data":"unknown method","ts":1}"#),
            MexcFutKind::RequestError
        );
        assert_eq!(classify_futures(br#"{"channel":"push.kline","data":{}}"#), MexcFutKind::Unknown);
        assert_eq!(classify_futures(br#"{"channel":"push.deals","data":{}}"#), MexcFutKind::Unknown, "exact match");
        assert_eq!(classify_futures(br#"{"nonsense":true}"#), MexcFutKind::Unknown);
        assert_eq!(classify_futures(br#"{"channel":7}"#), MexcFutKind::Unknown);
    }

    #[test]
    fn extract_symbol_and_ts() {
        assert_eq!(extract_fut_symbol(DEPTH), Some(&b"BTC_USDT"[..]));
        assert_eq!(extract_fut_symbol(TICKER), Some(&b"XAU_USDT"[..]));
        assert_eq!(extract_fut_symbol(br#"{"symbol":"","data":{}}"#), None);
        assert_eq!(extract_fut_symbol(br#"{"symbol":5}"#), None);
        assert_eq!(extract_fut_symbol(br#"{"data":{}}"#), None);
        assert_eq!(extract_fut_ts_ms(DEPTH), 1_789_897_581_013, "not the cts");
        assert_eq!(extract_fut_ts_ms(br#"{"cts":5}"#), 0);
    }

    #[test]
    fn golden_depth_full_decodes_exactly() {
        let d = parse_depth_full_view(DEPTH).unwrap();
        assert_eq!((d.has_bid, d.has_ask), (1, 1));
        assert_eq!(d.bid_px_1e6, 80_468_600_000);
        assert_eq!(d.bid_qty_1e6, 31_288_000_000);
        assert_eq!(d.ask_px_1e6, 80_468_700_000);
        assert_eq!(d.ask_qty_1e6, 3_446_000_000);
        assert_eq!(d.version, 41_925_002_140);
        assert_eq!(d.venue_time_ms, 1_789_897_581_009, "cts wins over ts");
    }

    #[test]
    fn depth_full_edges_and_failure_modes() {
        // One side empty: not an error, no BBO.
        let one = br#"{"symbol":"X","data":{"asks":[],"bids":[[1.5,2,1]],"version":3},"channel":"push.depth.full","ts":9}"#;
        let d = parse_depth_full_view(one).unwrap();
        assert_eq!((d.has_bid, d.has_ask), (1, 0));
        assert_eq!(d.venue_time_ms, 9, "no cts: envelope ts");
        // Exponent + quoted numbers tolerated; version absent = 0.
        let sci = br#"{"data":{"asks":[[1e2, "3" ,1]],"bids":[[9.5e1,2]]},"channel":"push.depth.full"}"#;
        let d = parse_depth_full_view(sci).unwrap();
        assert_eq!(d.ask_px_1e6, 100_000_000);
        assert_eq!(d.ask_qty_1e6, 3_000_000);
        assert_eq!(d.bid_px_1e6, 95_000_000);
        assert_eq!((d.version, d.venue_time_ms), (0, 0));
        // Failures.
        assert!(parse_depth_full_view(br#"{"data":{"bids":[]}}"#).is_none(), "asks required");
        assert!(parse_depth_full_view(br#"{"data":{"asks":[],"bids":{}}}"#).is_none());
        assert!(parse_depth_full_view(br#"{"data":{"asks":[[1]],"bids":[]}}"#).is_none(), "vol required");
        assert!(parse_depth_full_view(br#"{"data":{"asks":[[1,2x]],"bids":[]}}"#).is_none());
        assert!(parse_depth_full_view(br#"{"data":{"asks":[[-1,2]],"bids":[]}}"#).is_none());
        assert!(parse_depth_full_view(br#"{"data":{"asks":[["1,2]],"bids":[]}}"#).is_none());
        assert!(parse_depth_full_view(b"").is_none());
    }

    #[test]
    fn golden_deals_decode_exactly_array_and_single() {
        let mut w = MexcFutDealsWalk::new(DEAL);
        let d0 = parse_fut_deal_item_view(w.next_item().unwrap()).unwrap();
        assert_eq!(d0.px_1e6, 80_489_000_000, "integer price");
        assert_eq!(d0.qty_1e6, 11_000_000);
        assert_eq!(d0.side, DEAL_SIDE_BUY);
        assert_eq!(d0.time_ms, 1_789_897_547_210);
        assert_eq!(d0.trade_seq, 16_270_106_116);
        let d1 = parse_fut_deal_item_view(w.next_item().unwrap()).unwrap();
        assert_eq!(d1.px_1e6, 80_488_500_000);
        assert_eq!(d1.signed_qty_1e6(), -2_000_000);
        assert_eq!(d1.trade_seq, 16_270_106_117);
        assert!(w.next_item().is_none());
        assert!(!w.is_malformed());
        // The single-object shape.
        let one = br#"{"symbol":"BTC_USDT","data":{"p":1.5,"v":2,"T":2,"t":7,"i":"9"},"channel":"push.deal","ts":8}"#;
        let mut w = MexcFutDealsWalk::new(one);
        let d = parse_fut_deal_item_view(w.next_item().unwrap()).unwrap();
        assert_eq!((d.px_1e6, d.qty_1e6, d.side, d.time_ms, d.trade_seq), (1_500_000, 2_000_000, DEAL_SIDE_SELL, 7, 9));
        assert!(w.next_item().is_none());
    }

    #[test]
    fn deals_walk_and_item_failure_modes() {
        let mut w = MexcFutDealsWalk::new(br#"{"channel":"push.deal"}"#);
        assert!(w.next_item().is_none());
        assert!(w.is_malformed(), "no data");
        let mut w = MexcFutDealsWalk::new(br#"{"data":7}"#);
        assert!(w.next_item().is_none());
        assert!(w.is_malformed());
        let mut w = MexcFutDealsWalk::new(br#"{"data":[{"p":1,"v":1,"T":1},{"p":2"#);
        assert!(w.next_item().is_some());
        assert!(w.next_item().is_none());
        assert!(w.is_malformed(), "truncated second item");
        let mut w = MexcFutDealsWalk::new(br#"{"data":[7]}"#);
        assert!(w.next_item().is_none());
        assert!(w.is_malformed());
        let mut w = MexcFutDealsWalk::new(br#"{"data":[]}"#);
        assert!(w.next_item().is_none());
        assert!(!w.is_malformed(), "empty is clean");
        assert!(parse_fut_deal_item_view(br#"{"p":1,"v":1,"T":3}"#).is_none(), "T must be 1|2");
        assert!(parse_fut_deal_item_view(br#"{"p":1,"T":1}"#).is_none(), "v required");
        assert!(parse_fut_deal_item_view(br#"{"v":1,"T":1}"#).is_none(), "p required");
        assert!(parse_fut_deal_item_view(br#"{"p":-1,"v":1,"T":1}"#).is_none());
        assert!(parse_fut_deal_item_view(br#"{"p":1,"v":1,"T":1.5}"#).is_none());
        // Optional t / i absent or garbage → 0.
        let d = parse_fut_deal_item_view(br#"{"p":1,"v":1,"T":1,"i":"abc"}"#).unwrap();
        assert_eq!((d.time_ms, d.trade_seq), (0, 0));
    }

    #[test]
    fn golden_ticker_decodes_exactly() {
        let f = parse_ticker_view(TICKER).unwrap();
        assert_eq!((f.has_fair, f.has_index, f.has_funding, f.has_hold_vol), (1, 1, 1, 1));
        assert_eq!(f.fair_px_1e6, 4_377_210_000);
        assert_eq!(f.index_px_1e6, 4_377_630_000);
        assert_eq!(f.funding_rate_1e9, 0, "integer zero rate");
        assert_eq!(f.hold_vol_1e6, 92_415_393_000_000);
        assert_eq!(f.venue_time_ms, 1_789_897_545_754, "data.timestamp wins");
    }

    #[test]
    fn ticker_presence_flags_and_failure_modes() {
        let partial = br#"{"symbol":"BTC_USDT","data":{"fundingRate":-0.000125},"channel":"push.ticker","ts":77}"#;
        let f = parse_ticker_view(partial).unwrap();
        assert_eq!((f.has_fair, f.has_index, f.has_funding, f.has_hold_vol), (0, 0, 1, 0));
        assert_eq!(f.funding_rate_1e9, -125_000);
        assert_eq!(f.venue_time_ms, 77, "no timestamp: envelope ts");
        // Keys OUTSIDE data are not read.
        let outside = br#"{"fairPrice":5,"data":{},"channel":"push.ticker"}"#;
        assert_eq!(parse_ticker_view(outside).unwrap().has_fair, 0);
        assert!(parse_ticker_view(br#"{"data":[1]}"#).is_none());
        assert!(parse_ticker_view(br#"{"data":{"fairPrice":1"#).is_none(), "unterminated data");
        assert!(parse_ticker_view(br#"{"channel":"push.ticker"}"#).is_none());
    }

    /// The live 2026-09-23 ticker shape: `riseFallRates` carries `null`s
    /// and nested keys; the data object is still walked whole and the
    /// wanted keys read from it (bare numbers).
    #[test]
    fn ticker_with_live_nulls_and_nested_rates_decodes() {
        let live = br#"{"symbol":"EUR_USDT","data":{"contractId":1,"symbol":"EUR_USDT","lastPrice":1.1743,"bid1":1.1743,"ask1":1.1744,"volume24":1,"amount24":2.5,"holdVol":1234,"lower24Price":1.17,"high24Price":1.18,"riseFallRate":0.0001,"riseFallValue":0.0001,"indexPrice":1.17435,"fairPrice":1.1743,"fundingRate":-0.00002,"maxBidPrice":1.2,"minAskPrice":1.1,"timestamp":1789897545754,"riseFallRates":{"zone":"UTC+8","r":0.0001,"v":0.0001,"r7":null,"r30":null,"r90":null,"r180":null,"r365":null},"riseFallRatesOfTimezone":[0.0001,null,0.0002]},"channel":"push.ticker","ts":1789897545760}"#;
        assert_eq!(classify_futures(live), MexcFutKind::Data(MexcChannel::FutTicker));
        assert_eq!(extract_fut_symbol(live), Some(&b"EUR_USDT"[..]));
        let f = parse_ticker_view(live).unwrap();
        assert_eq!((f.has_fair, f.has_index, f.has_funding, f.has_hold_vol), (1, 1, 1, 1));
        assert_eq!(f.fair_px_1e6, 1_174_300);
        assert_eq!(f.index_px_1e6, 1_174_350);
        assert_eq!(f.funding_rate_1e9, -20_000);
        assert_eq!(f.hold_vol_1e6, 1_234_000_000);
        assert_eq!(f.venue_time_ms, 1_789_897_545_754);
    }

    #[test]
    fn write_fut_subscribe_renders_each_channel() {
        let mut buf = [0u8; 256];
        let n = write_fut_subscribe(&mut buf, MexcChannel::FutDepthFull, b"BTC_USDT").unwrap();
        assert_eq!(&buf[..n], br#"{"method":"sub.depth.full","param":{"symbol":"BTC_USDT","limit":5}}"# as &[u8]);
        let n = write_fut_subscribe(&mut buf, MexcChannel::FutDeal, b"BTC_USDT").unwrap();
        assert_eq!(&buf[..n], br#"{"method":"sub.deal","param":{"symbol":"BTC_USDT"}}"# as &[u8]);
        let n = write_fut_subscribe(&mut buf, MexcChannel::FutTicker, b"XAU_USDT").unwrap();
        assert_eq!(&buf[..n], br#"{"method":"sub.ticker","param":{"symbol":"XAU_USDT"}}"# as &[u8]);
        let mut tiny = [0u8; 12];
        assert!(write_fut_subscribe(&mut tiny, MexcChannel::FutDeal, b"BTC_USDT").is_none());
        let long = [b'A'; crate::MEXC_SYMBOL_MAX];
        let mut exact = [0u8; FUT_SUB_PAYLOAD_MAX];
        assert_eq!(write_fut_subscribe(&mut exact, MexcChannel::FutDepthFull, &long), Some(FUT_SUB_PAYLOAD_MAX));
    }

    /// A spot channel is a caller bug: the run loop draws futures
    /// channels only (`MexcChannel::from_slot(MexcClass::Futures, …)`).
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "needs a futures channel"))]
    fn render_fut_subscribe_refuses_a_spot_channel_in_debug() {
        let mut buf = [0u8; 256];
        let _ = write_fut_subscribe(&mut buf, MexcChannel::SpotDeals, b"BTC_USDT");
    }

    #[test]
    fn json_number_readers() {
        assert_eq!(json_u64(b" 42,", 0), Some((42, 3)));
        assert_eq!(json_u64(b"\"42\"", 0), Some((42, 4)));
        assert_eq!(json_u64(b"\"42", 0), None);
        assert_eq!(json_u64(b"4.2", 0), None);
        assert_eq!(json_u64(b"4e2", 0), None);
        assert_eq!(json_u64(b"-4", 0), None);
        assert_eq!(json_num_1e9(b"\"0.5\"", 0), Some((500_000_000, 5)));
        assert_eq!(json_num_1e9(b"\"0.5", 0), None);
        assert_eq!(json_num_1e6(b"80489,", 0), Some((80_489_000_000, 5)));
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use super::views::*;

    proptest! {
        #[test]
        fn classify_and_extract_never_panic(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = classify_futures(&buf);
            let _ = extract_fut_symbol(&buf);
            let _ = extract_fut_ts_ms(&buf);
        }

        #[test]
        fn depth_full_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = parse_depth_full_view(&buf);
        }

        #[test]
        fn deals_never_panic(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let mut w = MexcFutDealsWalk::new(&buf);
            let mut n = 0;
            while let Some(item) = w.next_item() {
                let _ = parse_fut_deal_item_view(item);
                n += 1;
                prop_assert!(n <= buf.len());
            }
            let _ = parse_fut_deal_item_view(&buf);
        }

        #[test]
        fn ticker_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = parse_ticker_view(&buf);
        }

        /// Structured fuzz: the JSON grammar with random bytes spliced
        /// into the numeric slots never panics.
        #[test]
        fn depth_full_spliced_never_panics(a in "[0-9eE.+\\-\"]{0,12}", b in "[0-9eE.+\\-\"]{0,12}") {
            let s = format!(r#"{{"data":{{"asks":[[{a},{b},1]],"bids":[[{b},{a}]],"version":{a}}},"ts":{b}}}"#);
            let _ = parse_depth_full_view(s.as_bytes());
        }

        /// depth.full round-trips: integer and fractional prices,
        /// with and without `cts`.
        #[test]
        fn depth_full_roundtrips(
            bp in 1u64..10_000_000_000_000u64,
            bq in 0u64..1_000_000_000u64,
            ap in 1u64..10_000_000_000_000u64,
            aq in 0u64..1_000_000_000u64,
            version in any::<u64>(),
            ts in 1u64..4_000_000_000_000u64,
            cts in 1u64..4_000_000_000_000u64,
            with_cts in any::<bool>(),
            integer_px in any::<bool>(),
        ) {
            let px = |v: u64| if integer_px { format!("{}", v / 1_000_000) } else { format!("{}.{:06}", v / 1_000_000, v % 1_000_000) };
            let want = |v: u64| if integer_px { (v / 1_000_000 * 1_000_000) as i64 } else { v as i64 };
            let cts_part = if with_cts { format!(r#""cts":{cts},"#) } else { String::new() };
            let s = format!(
                r#"{{"symbol":"BTC_USDT","data":{{{cts_part}"asks":[[{},{},2],[1,1,1]],"bids":[[{},{},7]],"version":{version}}},"channel":"push.depth.full","ts":{ts}}}"#,
                px(ap), bq, px(bp), aq,
            );
            let d = parse_depth_full_view(s.as_bytes()).unwrap();
            prop_assert_eq!(d.ask_px_1e6, want(ap));
            prop_assert_eq!(d.ask_qty_1e6, (bq * 1_000_000) as i64);
            prop_assert_eq!(d.bid_px_1e6, want(bp));
            prop_assert_eq!(d.bid_qty_1e6, (aq * 1_000_000) as i64);
            prop_assert_eq!(d.version, version);
            prop_assert_eq!(d.venue_time_ms, if with_cts { cts } else { ts });
            prop_assert_eq!((d.has_bid, d.has_ask), (1, 1));
            prop_assert_eq!(classify_futures(s.as_bytes()), MexcFutKind::Data(MexcChannel::FutDepthFull));
            prop_assert_eq!(extract_fut_symbol(s.as_bytes()), Some(&b"BTC_USDT"[..]));
        }
    }
}
