// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # MEXC spot — Protocol Buffers over WS BINARY (MX3)
//!
//! The wire, decoded from live frames (plan §1.1):
//!
//! ```text
//! PushDataV3ApiWrapper
//!   f1    channel     string   ignored (the body field names the channel)
//!   f3    symbol      string   "BTCUSDT"
//!   f5    createTime  varint   ms — absent in every observed frame
//!   f6    sendTime    varint   ms
//!   f314  publicAggreDeals       LEN  (tag bytes D2 13)
//!   f315  publicAggreBookTicker  LEN  (tag bytes DA 13)
//!
//! PublicAggreBookTickerV3Api        PublicAggreDealsV3Api
//!   f1 bidPrice     string            f1 deals (repeated item)
//!   f2 bidQuantity  string              f1 price      string
//!   f3 askPrice     string              f2 quantity   string
//!   f4 askQuantity  string              f3 tradeType  varint 1 buy / 2 sell
//!   f5 version      string (digits)     f4 time       varint ms
//!                                       f5 tradeId    string
//!                                     f2 eventType (skipped)
//! ```
//!
//! Every walk is an ORDER-AGNOSTIC forward walk over
//! [`core_parse::scan_pb_field`] (proto3 permits any order and omits
//! defaults): a field of interest is latched by number, every other
//! field number is skipped by construction, and the result is judged
//! only at end-of-message (plan §4 D5). A KNOWN field number carrying
//! the wrong wire type is malformed (a renumbered field must fail
//! loudly, not read as absent). Prices and quantities are ASCII decimal
//! strings scanned IN PLACE with [`core_parse::scan_price_1e6`], and the
//! scan must consume the whole string. Nothing allocates, nothing
//! copies: the parsers return spans into the caller's buffer.
//!
//! Text frames on this class are the subscribe ack
//! (`{"id":0,"code":0,"msg":"…"}`, [`parse_sub_ack`]) and the answer to
//! our `{"method":"PING"}` (`"msg":"PONG"`).

use core_parse::{
    find_field, scan_i64, scan_pb_field, scan_price_1e6, skip_string, skip_ws, PB_WT_LEN,
    PB_WT_VARINT,
};

use crate::{
    push_bytes, scan_u64_checked, MexcChannel, MexcClass, MexcDeal, MexcSymbolTable,
    DEAL_SIDE_BUY, DEAL_SIDE_SELL,
};

// Wrapper field numbers (`PushDataV3ApiWrapper`; f1 `channel` is
// deliberately not latched).
const F_SYMBOL: u32 = 3;
const F_CREATE_TIME: u32 = 5;
const F_SEND_TIME: u32 = 6;
const F_AGGRE_DEALS: u32 = 314;
const F_AGGRE_BOOK_TICKER: u32 = 315;

// Body field numbers.
const F_BID_PX: u32 = 1;
const F_BID_QTY: u32 = 2;
const F_ASK_PX: u32 = 3;
const F_ASK_QTY: u32 = 4;
const F_VERSION: u32 = 5;
const F_DEALS_ITEM: u32 = 1;
const F_DEAL_PX: u32 = 1;
const F_DEAL_QTY: u32 = 2;
const F_DEAL_TYPE: u32 = 3;
const F_DEAL_TIME: u32 = 4;
const F_DEAL_ID: u32 = 5;

// ---------------------------------------------------------------
// Classification
// ---------------------------------------------------------------

/// Coarse classification of one inbound spot frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MexcSpotKind {
    /// A non-empty BINARY frame — a `PushDataV3ApiWrapper` for
    /// [`parse_spot_wrapper`].
    Push,
    /// The answer to our `{"method":"PING"}` (`"msg":"PONG"`).
    Pong,
    /// The subscribe ack (`{"id":0,"code":…,"msg":"…"}`) for
    /// [`parse_sub_ack`].
    SubAck,
    /// Anything else — counted as a parse rejection by the caller.
    Unknown,
}

/// Classify one spot frame by its WS opcode (`binary`) and, for text,
/// by key. Zero-alloc.
#[inline]
pub fn classify_spot(payload: &[u8], binary: bool) -> MexcSpotKind {
    if binary {
        return if payload.is_empty() {
            MexcSpotKind::Unknown
        } else {
            MexcSpotKind::Push
        };
    }
    if find_field(payload, b"\"code\":").is_none() || find_field(payload, b"\"msg\":").is_none() {
        return MexcSpotKind::Unknown;
    }
    if memchr::memmem::find(payload, b"\"msg\":\"PONG\"").is_some() {
        return MexcSpotKind::Pong;
    }
    MexcSpotKind::SubAck
}

// ---------------------------------------------------------------
// Wrapper
// ---------------------------------------------------------------

/// One walked `PushDataV3ApiWrapper`: the channel its body names and
/// the spans of the symbol and the body inside the walked buffer.
/// 64-byte POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct MexcSpotFrame {
    /// Symbol span start (field 3).
    pub sym_start: usize,
    /// Symbol span end.
    pub sym_end: usize,
    /// Body span start (field 314 or 315 payload).
    pub body_start: usize,
    /// Body span end.
    pub body_end: usize,
    /// `createTime` ms (field 5; 0 = absent — every observed frame).
    pub create_time_ms: u64,
    /// `sendTime` ms (field 6; 0 = absent).
    pub send_time_ms: u64,
    /// [`MexcChannel::SpotBookTicker`] (f315) or
    /// [`MexcChannel::SpotDeals`] (f314).
    pub channel: MexcChannel,
    // Explicit tail padding.
    _pad: [u8; 15],
}

impl MexcSpotFrame {
    /// The symbol bytes — a subslice of `buf` (the walked buffer); empty
    /// when `buf` is some other buffer.
    #[inline]
    pub fn symbol<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        buf.get(self.sym_start..self.sym_end).unwrap_or(&[])
    }

    /// The body bytes — a subslice of `buf`; empty when `buf` is some
    /// other buffer.
    #[inline]
    pub fn body<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        buf.get(self.body_start..self.body_end).unwrap_or(&[])
    }

    /// The push's venue time: `createTime` when present, else
    /// `sendTime`, else 0 ("unknown, never stale").
    #[inline]
    pub const fn venue_time_ms(&self) -> u64 {
        if self.create_time_ms > 0 {
            self.create_time_ms
        } else {
            self.send_time_ms
        }
    }
}

// [`parse_spot_wrapper`] returns its frame's `Option` by value on every
// spot push: pinned within the 64 B bound (it fits only because
// `MexcChannel` gives the `Option` a niche).
const _: () = assert!(core::mem::size_of::<Option<MexcSpotFrame>>() <= 64);

/// Walk one `PushDataV3ApiWrapper` order-agnostically. Requires a
/// non-empty symbol and EXACTLY ONE known body (f314 or f315); every
/// other field number is skipped. `None` on malformed PB, a known field
/// with the wrong wire type, a missing symbol/body or two bodies.
#[inline]
pub fn parse_spot_wrapper(buf: &[u8]) -> Option<MexcSpotFrame> {
    let mut sym: Option<(usize, usize)> = None;
    let mut body: Option<(MexcChannel, usize, usize)> = None;
    let mut create_time_ms = 0u64;
    let mut send_time_ms = 0u64;
    let mut pos = 0usize;
    while pos < buf.len() {
        let f = scan_pb_field(buf, pos)?;
        match f.field_no {
            F_SYMBOL => {
                if f.wire_type != PB_WT_LEN {
                    return None;
                }
                sym = Some((f.start, f.end));
            }
            F_CREATE_TIME | F_SEND_TIME => {
                if f.wire_type != PB_WT_VARINT {
                    return None;
                }
                if f.field_no == F_CREATE_TIME {
                    create_time_ms = f.value;
                } else {
                    send_time_ms = f.value;
                }
            }
            F_AGGRE_DEALS | F_AGGRE_BOOK_TICKER => {
                if f.wire_type != PB_WT_LEN || body.is_some() {
                    return None;
                }
                let ch = if f.field_no == F_AGGRE_DEALS {
                    MexcChannel::SpotDeals
                } else {
                    MexcChannel::SpotBookTicker
                };
                body = Some((ch, f.start, f.end));
            }
            _ => {}
        }
        pos = f.end;
    }
    let (sym_start, sym_end) = sym?;
    if sym_end <= sym_start {
        return None;
    }
    let (channel, body_start, body_end) = body?;
    Some(MexcSpotFrame {
        sym_start,
        sym_end,
        body_start,
        body_end,
        create_time_ms,
        send_time_ms,
        channel,
        _pad: [0; 15],
    })
}

// ---------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------

/// A PB string payload scanned IN PLACE as a non-negative decimal ×1e6;
/// the scan must consume the WHOLE span (digits past the sixth
/// fraction place truncate — the engine's fixed point).
#[inline]
fn span_decimal_1e6(span: &[u8]) -> Option<i64> {
    let (v, end) = scan_price_1e6(span, 0)?;
    if end == span.len() && v >= 0 {
        Some(v)
    } else {
        None
    }
}

/// A PB string payload that must be ALL ASCII digits (checked).
#[inline]
fn span_u64(span: &[u8]) -> Option<u64> {
    let (v, end) = scan_u64_checked(span, 0)?;
    if end == span.len() {
        Some(v)
    } else {
        None
    }
}

/// Q-MX1: the spot trade `venue_seq` — the LEADING DIGITS of the
/// `tradeId` string (`"730292425431437318X0_730292425431437319X0"` →
/// 730292425431437318). 0 when the id has no leading digit, and 0 when
/// the run overflows `u64` (unrepresentable = absent, never wrapped).
#[inline]
pub fn trade_id_seq(id: &[u8]) -> u64 {
    match scan_u64_checked(id, 0) {
        Some((v, _)) => v,
        None => 0,
    }
}

/// One parsed `PublicAggreBookTickerV3Api` body — a COMPLETE BBO (no
/// delta state on this channel). 64-byte POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct MexcBookTicker {
    /// Best bid ×1e6.
    pub bid_px_1e6: i64,
    /// Size at best bid ×1e6 (base units).
    pub bid_qty_1e6: i64,
    /// Best ask ×1e6.
    pub ask_px_1e6: i64,
    /// Size at best ask ×1e6.
    pub ask_qty_1e6: i64,
    /// Book `version` (field 5, ASCII digits; 0 = absent).
    pub version: u64,
    // Explicit tail padding.
    _pad: [u8; 24],
}

impl MexcBookTicker {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        bid_px_1e6: 0,
        bid_qty_1e6: 0,
        ask_px_1e6: 0,
        ask_qty_1e6: 0,
        version: 0,
        _pad: [0; 24],
    };
}

/// Parse one bookTicker body (the f315 span). A SIDE is its price +
/// quantity pair: each side must be complete (both strings) or wholly
/// absent — proto3 omits an empty string, so an emptied book side (a
/// halted or one-sided book) arrives as two missing fields and reads as
/// 0/0 here: a quote the run loop does not emit, NOT a parse error (the
/// futures `depth.full` law for an empty side). At least one side must
/// be present — a body with neither is structural drift (plan R1) and
/// is rejected. `version` is optional but, when present, must be all
/// digits; every other field is skipped.
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_book_ticker_body(body: &[u8], out: &mut MexcBookTicker) -> bool {
    parse_book_ticker_body_fill(body, out).is_some()
}

/// [`parse_book_ticker_body`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_book_ticker_body_fill(body: &[u8], out: &mut MexcBookTicker) -> Option<()> {
    let mut vals = [0i64; 4];
    let mut have = 0u8;
    let mut version = 0u64;
    let mut pos = 0usize;
    while pos < body.len() {
        let f = scan_pb_field(body, pos)?;
        match f.field_no {
            F_BID_PX | F_BID_QTY | F_ASK_PX | F_ASK_QTY => {
                if f.wire_type != PB_WT_LEN {
                    return None;
                }
                let idx = (f.field_no - F_BID_PX) as usize;
                let span = body.get(f.start..f.end)?;
                // An explicitly empty string is the same absence proto3
                // would have omitted.
                if !span.is_empty() {
                    *vals.get_mut(idx)? = span_decimal_1e6(span)?;
                    have |= 1 << idx;
                }
            }
            F_VERSION => {
                if f.wire_type != PB_WT_LEN {
                    return None;
                }
                version = span_u64(body.get(f.start..f.end)?)?;
            }
            _ => {}
        }
        pos = f.end;
    }
    let bid = have & 0b0011;
    let ask = have & 0b1100;
    // Each side whole or absent (a lone price or size is malformed),
    // and at least one side present.
    if (bid != 0 && bid != 0b0011) || (ask != 0 && ask != 0b1100) || have == 0 {
        return None;
    }
    *out = MexcBookTicker {
        bid_px_1e6: vals[0],
        bid_qty_1e6: vals[1],
        ask_px_1e6: vals[2],
        ask_qty_1e6: vals[3],
        version,
        _pad: [0; 24],
    };
    Some(())
}

/// Forward walk over a `PublicAggreDealsV3Api` body yielding each
/// `deals` item span (field 1) in wire order; every other field
/// (`eventType`) is skipped. Stops for good at the end of the body or
/// at the first structurally malformed field ([`Self::is_malformed`]
/// tells the two apart). Zero-alloc, zero-copy.
pub struct MexcDealsWalk<'a> {
    body: &'a [u8],
    pos: usize,
    malformed: bool,
}

impl<'a> MexcDealsWalk<'a> {
    /// Start a walk over one deals body (the f314 span).
    #[inline]
    pub const fn new(body: &'a [u8]) -> Self {
        Self {
            body,
            pos: 0,
            malformed: false,
        }
    }

    /// The next item's payload span, `None` at the end or on malformed
    /// structure.
    #[inline]
    pub fn next_item(&mut self) -> Option<&'a [u8]> {
        while self.pos < self.body.len() && !self.malformed {
            let Some(f) = scan_pb_field(self.body, self.pos) else {
                self.malformed = true;
                return None;
            };
            self.pos = f.end;
            if f.field_no == F_DEALS_ITEM {
                if f.wire_type != PB_WT_LEN {
                    self.malformed = true;
                    return None;
                }
                return self.body.get(f.start..f.end);
            }
        }
        None
    }

    /// True once the walk stopped on malformed structure (the items
    /// already yielded stand; the rest of the body is unreadable).
    #[inline]
    pub const fn is_malformed(&self) -> bool {
        self.malformed
    }
}

/// Parse one deals ITEM. Price, quantity and `tradeType` (1 buy / 2
/// sell — anything else rejects the item) are required; `time` and
/// `tradeId` are optional (0 = absent).
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_deal_item(item: &[u8], out: &mut MexcDeal) -> bool {
    parse_deal_item_fill(item, out).is_some()
}

/// [`parse_deal_item`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_deal_item_fill(item: &[u8], out: &mut MexcDeal) -> Option<()> {
    let mut px: Option<i64> = None;
    let mut qty: Option<i64> = None;
    let mut side: Option<u8> = None;
    let mut time_ms = 0u64;
    let mut seq = 0u64;
    let mut pos = 0usize;
    while pos < item.len() {
        let f = scan_pb_field(item, pos)?;
        match f.field_no {
            F_DEAL_PX | F_DEAL_QTY => {
                if f.wire_type != PB_WT_LEN {
                    return None;
                }
                let v = span_decimal_1e6(item.get(f.start..f.end)?)?;
                if f.field_no == F_DEAL_PX {
                    px = Some(v);
                } else {
                    qty = Some(v);
                }
            }
            F_DEAL_TYPE => {
                if f.wire_type != PB_WT_VARINT {
                    return None;
                }
                side = Some(match f.value {
                    1 => DEAL_SIDE_BUY,
                    2 => DEAL_SIDE_SELL,
                    _ => return None,
                });
            }
            F_DEAL_TIME => {
                if f.wire_type != PB_WT_VARINT {
                    return None;
                }
                time_ms = f.value;
            }
            F_DEAL_ID => {
                if f.wire_type != PB_WT_LEN {
                    return None;
                }
                seq = trade_id_seq(item.get(f.start..f.end)?);
            }
            _ => {}
        }
        pos = f.end;
    }
    *out = MexcDeal::new(px?, qty?, time_ms, seq, side?);
    Some(())
}

const _POD_SIZES: () = {
    assert!(::core::mem::size_of::<MexcSpotFrame>() == 64);
    assert!(::core::mem::size_of::<MexcBookTicker>() == 64);
    assert!(::core::mem::size_of::<MexcSpotAck>() == 64);
};

// ---------------------------------------------------------------
// Subscribe ack (the per-param echo — plan §4 D7)
// ---------------------------------------------------------------

/// The failed-param marker inside the ack `msg`.
const FAILED_MARK: &[u8] = b"Not Subscribed successfully!";

/// One parsed spot subscribe ack. The per-param outcome rides the
/// `msg` text: params listed after `Not Subscribed successfully! [` are
/// REFUSED; every requested param not listed there is confirmed (a
/// success-only ack may echo the plain comma-separated params or
/// `Subscribed successful! […]` — neither needs reading). 64-byte POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct MexcSpotAck {
    /// The ack's `code`: 0 = processed per param; anything else refuses
    /// the WHOLE request.
    pub code: i64,
    /// Span of the failed-param list (between `[` and `]`) in the ack
    /// payload; empty when nothing failed.
    pub failed_start: usize,
    /// End of that span.
    pub failed_end: usize,
    // Explicit tail padding.
    _pad: [u8; 40],
}

impl MexcSpotAck {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        code: 0,
        failed_start: 0,
        failed_end: 0,
        _pad: [0; 40],
    };
}

impl MexcSpotAck {
    /// True when the ack lists at least one refused param.
    #[inline]
    pub fn has_failures(&self, payload: &[u8]) -> bool {
        let mut w = self.failed_params(payload);
        w.next_param().is_some()
    }

    /// Walk the refused params (subslices of `payload`, the parsed ack).
    #[inline]
    pub fn failed_params<'a>(&self, payload: &'a [u8]) -> MexcAckParams<'a> {
        MexcAckParams {
            list: payload.get(self.failed_start..self.failed_end).unwrap_or(&[]),
            pos: 0,
        }
    }
}

/// Parse one spot ack. `false` when `code` or a string `msg` is missing,
/// or when the failed list is opened but never closed (a truncated
/// ack must not read as "nothing failed").
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_sub_ack(payload: &[u8], out: &mut MexcSpotAck) -> bool {
    parse_sub_ack_fill(payload, out).is_some()
}

/// [`parse_sub_ack`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_sub_ack_fill(payload: &[u8], out: &mut MexcSpotAck) -> Option<()> {
    let cpos = skip_ws(payload, find_field(payload, b"\"code\":")?);
    let (code, _) = scan_i64(payload, cpos)?;
    let mpos = skip_ws(payload, find_field(payload, b"\"msg\":")?);
    if payload.get(mpos) != Some(&b'"') {
        return None;
    }
    let msg_start = mpos + 1;
    let msg_end = skip_string(payload, msg_start)? - 1;
    let msg = payload.get(msg_start..msg_end)?;
    let (failed_start, failed_end) = match memchr::memmem::find(msg, FAILED_MARK) {
        None => (0, 0),
        Some(off) => {
            let open = skip_ws(msg, off + FAILED_MARK.len());
            if msg.get(open) != Some(&b'[') {
                return None;
            }
            let list_start = open + 1;
            let close = memchr::memchr(b']', msg.get(list_start..)?)?;
            (msg_start + list_start, msg_start + list_start + close)
        }
    };
    *out = MexcSpotAck {
        code,
        failed_start,
        failed_end,
        _pad: [0; 40],
    };
    Some(())
}

/// Zero-alloc walker over an ack's comma-separated failed-param list
/// (each param trimmed of spaces; empty entries skipped).
pub struct MexcAckParams<'a> {
    list: &'a [u8],
    pos: usize,
}

impl<'a> MexcAckParams<'a> {
    /// The next refused param, `None` at the end of the list.
    #[inline]
    pub fn next_param(&mut self) -> Option<&'a [u8]> {
        while self.pos < self.list.len() {
            let rest = self.list.get(self.pos..)?;
            let cut = memchr::memchr(b',', rest).unwrap_or(rest.len());
            self.pos += cut + 1;
            let p = trim_spaces(rest.get(..cut)?);
            if !p.is_empty() {
                return Some(p);
            }
        }
        None
    }
}

/// Strip leading/trailing ASCII spaces.
#[inline]
fn trim_spaces(mut s: &[u8]) -> &[u8] {
    while let [b' ', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' '] = s {
        s = rest;
    }
    s
}

/// The symbol a spot param names: the text after its LAST `@`
/// (`spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT` → `BTCUSDT`).
#[inline]
pub fn extract_param_symbol(param: &[u8]) -> Option<&[u8]> {
    let at = memchr::memrchr(b'@', param)?;
    let s = param.get(at + 1..)?;
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The channel a spot param names, from its channel text (any
/// interval); `None` for a channel this ingress never subscribes (e.g.
/// the `Blocked!` `increase.depth`).
#[inline]
pub fn extract_param_channel(param: &[u8]) -> Option<MexcChannel> {
    if param.starts_with(b"spot@public.aggre.bookTicker.") {
        Some(MexcChannel::SpotBookTicker)
    } else if param.starts_with(b"spot@public.aggre.deals.") {
        Some(MexcChannel::SpotDeals)
    } else {
        None
    }
}

// ---------------------------------------------------------------
// Subscribe writer
// ---------------------------------------------------------------

/// Render the single spot subscribe op for one connection:
/// `{"method":"SUBSCRIPTION","params":["<bookTicker><SYM>","<deals><SYM>",…]}`.
/// Returns the byte length, `None` if `dst` is too small.
#[inline]
pub fn write_spot_subscribe(dst: &mut [u8], symbols: &MexcSymbolTable) -> Option<usize> {
    let mut n = push_bytes(dst, 0, b"{\"method\":\"SUBSCRIPTION\",\"params\":[")?;
    let per_symbol = MexcClass::Spot.channels_per_symbol() as u8;
    let mut first = true;
    let mut i = 0;
    while let Some((symbol, _sym)) = symbols.get(i) {
        let mut slot = 0u8;
        while slot < per_symbol {
            let ch = MexcChannel::from_slot(MexcClass::Spot, slot)?;
            if !first {
                n = push_bytes(dst, n, b",")?;
            }
            first = false;
            n = push_bytes(dst, n, b"\"")?;
            n = push_bytes(dst, n, ch.topic())?;
            n = push_bytes(dst, n, symbol)?;
            n = push_bytes(dst, n, b"\"")?;
            slot += 1;
        }
        i += 1;
    }
    push_bytes(dst, n, b"]}")
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

/// Test-only PB ENCODER (allocating) reproducing the plan §1.1 frames.
#[cfg(test)]
pub(crate) mod enc {
    pub(crate) fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    pub(crate) fn tag(out: &mut Vec<u8>, field_no: u32, wire_type: u8) {
        varint(out, ((field_no as u64) << 3) | wire_type as u64);
    }

    pub(crate) fn len_field(out: &mut Vec<u8>, field_no: u32, payload: &[u8]) {
        tag(out, field_no, core_parse::PB_WT_LEN);
        varint(out, payload.len() as u64);
        out.extend_from_slice(payload);
    }

    pub(crate) fn varint_field(out: &mut Vec<u8>, field_no: u32, v: u64) {
        tag(out, field_no, core_parse::PB_WT_VARINT);
        varint(out, v);
    }

    /// The live channel string of plan §1.1 (`0A 34` = 52 bytes — the
    /// `@100ms` spelling; the parser ignores f1 either way).
    pub(crate) const CHANNEL_BOOK: &[u8] = b"spot@public.aggre.bookTicker.v3.api.pb@100ms@BTCUSDT";
    /// sendTime of the live frame: `30 A7 D3 D2 F1 8B 34`.
    pub(crate) const SEND_TIME: u64 = 1_789_897_517_479;

    /// The §1.1 bookTicker body, field for field.
    pub(crate) fn book_body() -> Vec<u8> {
        let mut b = Vec::new();
        len_field(&mut b, 1, b"80535.88");
        len_field(&mut b, 2, b"0.380497");
        len_field(&mut b, 3, b"80535.89");
        len_field(&mut b, 4, b"0.33336356");
        len_field(&mut b, 5, b"81721676217");
        varint_field(&mut b, 6, 1_789_897_517_454); // lastOrderCreateTime
        b
    }

    /// The §1.1 wrapper around `body` at `body_field` in the LIVE order
    /// (channel, symbol, sendTime, body).
    pub(crate) fn wrapper(channel: &[u8], symbol: &[u8], body_field: u32, body: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        len_field(&mut f, 1, channel);
        len_field(&mut f, 3, symbol);
        varint_field(&mut f, 6, SEND_TIME);
        len_field(&mut f, body_field, body);
        f
    }

    /// One deals item (§1.1 shape).
    pub(crate) fn deal_item(px: &[u8], qty: &[u8], trade_type: u64, time_ms: u64, id: &[u8]) -> Vec<u8> {
        let mut it = Vec::new();
        len_field(&mut it, 1, px);
        len_field(&mut it, 2, qty);
        varint_field(&mut it, 3, trade_type);
        varint_field(&mut it, 4, time_ms);
        len_field(&mut it, 5, id);
        it
    }

    /// Deals time of the live item: `20 B6 D9 D2 F1 8B 34`.
    pub(crate) const DEAL_TIME: u64 = 1_789_897_518_262;

    /// The §1.1 deals body: two items (the live 74- and 73-byte shapes)
    /// + the `eventType` string.
    pub(crate) fn deals_body() -> Vec<u8> {
        let mut b = Vec::new();
        len_field(
            &mut b,
            1,
            &deal_item(b"80535.88", b"0.01362099", 2, DEAL_TIME, b"730292425431437318X0_730292425431437319X0"),
        );
        len_field(
            &mut b,
            1,
            &deal_item(b"80535.89", b"0.0012345", 1, DEAL_TIME + 1, b"730292425431437320X0_730292425431437320X0"),
        );
        len_field(&mut b, 2, b"spot@public.aggre.deals.v3.api.pb@10ms");
        b
    }
}

/// Test views in the old by-value shape, shared by the unit and the
/// property tests: each wraps one in-place parser and hands back its
/// frame's `Option`, so assertions read naturally. Cold — production
/// callers parse in place.
#[cfg(test)]
mod views {
    // COPY: `Option<MexcBookTicker>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_book_ticker_body_view(body: &[u8]) -> Option<crate::spot::MexcBookTicker> {
        let mut f = crate::spot::MexcBookTicker::ZERO;
        crate::spot::parse_book_ticker_body(body, &mut f).then_some(f)
    }
    // COPY: `Option<MexcDeal>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_deal_item_view(item: &[u8]) -> Option<crate::MexcDeal> {
        let mut f = crate::MexcDeal::ZERO;
        crate::spot::parse_deal_item(item, &mut f).then_some(f)
    }
    // COPY: `Option<MexcSpotAck>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_sub_ack_view(payload: &[u8]) -> Option<crate::spot::MexcSpotAck> {
        let mut f = crate::spot::MexcSpotAck::ZERO;
        crate::spot::parse_sub_ack(payload, &mut f).then_some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::enc;
    use super::*;
    use super::views::*;

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        memchr::memmem::find(hay, needle).is_some()
    }

    #[test]
    fn the_encoder_reproduces_the_live_bytes() {
        // Plan §1.1 byte strings, verbatim.
        let mut v = Vec::new();
        enc::varint(&mut v, enc::SEND_TIME);
        assert_eq!(v, [0xA7, 0xD3, 0xD2, 0xF1, 0x8B, 0x34]);
        let body = enc::book_body();
        assert_eq!(body.len(), 0x3E, "body length byte 3E");
        let frame = enc::wrapper(enc::CHANNEL_BOOK, b"BTCUSDT", 315, &body);
        assert_eq!(&frame[..2], &[0x0A, 0x34]);
        assert!(contains(&frame, &[0x1A, 0x07]));
        assert!(contains(&frame, &[0x30, 0xA7, 0xD3, 0xD2, 0xF1, 0x8B, 0x34]));
        assert!(contains(&frame, &[0xDA, 0x13, 0x3E]), "f315 tag DA 13 + len 3E");
        let item = enc::deal_item(b"80535.88", b"0.01362099", 2, enc::DEAL_TIME, b"730292425431437318X0_730292425431437319X0");
        assert_eq!(item.len(), 0x4A, "deals[0] len 4A");
        assert!(contains(&item, &[0x20, 0xB6, 0xD9, 0xD2, 0xF1, 0x8B, 0x34]));
        assert!(contains(&item, &[0x2A, 0x29]));
        let deals = enc::wrapper(b"spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT", b"BTCUSDT", 314, &enc::deals_body());
        assert!(contains(&deals, &[0xD2, 0x13]), "f314 tag D2 13");
        assert!(contains(&deals, &[0x0A, 0x49]), "deals[1] len 49");
    }

    #[test]
    fn classify_spot_by_opcode_then_key() {
        assert_eq!(classify_spot(&[0x0A, 0x00], true), MexcSpotKind::Push);
        assert_eq!(classify_spot(&[], true), MexcSpotKind::Unknown);
        assert_eq!(classify_spot(br#"{"id":0,"code":0,"msg":"PONG"}"#, false), MexcSpotKind::Pong);
        assert_eq!(
            classify_spot(br#"{"id":0,"code":0,"msg":"spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT"}"#, false),
            MexcSpotKind::SubAck
        );
        assert_eq!(classify_spot(br#"{"nonsense":true}"#, false), MexcSpotKind::Unknown);
        // A PB push delivered as TEXT is not a push.
        let frame = enc::wrapper(enc::CHANNEL_BOOK, b"BTCUSDT", 315, &enc::book_body());
        assert_eq!(classify_spot(&frame, false), MexcSpotKind::Unknown);
    }

    #[test]
    fn golden_book_ticker_frame_decodes_exactly() {
        let frame = enc::wrapper(enc::CHANNEL_BOOK, b"BTCUSDT", 315, &enc::book_body());
        let w = parse_spot_wrapper(&frame).expect("golden wrapper");
        assert_eq!(w.channel, MexcChannel::SpotBookTicker);
        assert_eq!(w.symbol(&frame), b"BTCUSDT");
        assert_eq!(w.create_time_ms, 0, "createTime absent on the live wire");
        assert_eq!(w.send_time_ms, enc::SEND_TIME);
        assert_eq!(w.venue_time_ms(), enc::SEND_TIME);
        let b = parse_book_ticker_body_view(w.body(&frame)).expect("golden body");
        assert_eq!(b.bid_px_1e6, 80_535_880_000);
        assert_eq!(b.bid_qty_1e6, 380_497);
        assert_eq!(b.ask_px_1e6, 80_535_890_000);
        assert_eq!(b.ask_qty_1e6, 333_363, "0.33336356 truncates at 1e-6");
        assert_eq!(b.version, 81_721_676_217);
    }

    #[test]
    fn wrapper_is_order_agnostic_and_skips_unknown_fields() {
        let body = enc::book_body();
        let golden = enc::wrapper(enc::CHANNEL_BOOK, b"BTCUSDT", 315, &body);
        let g = parse_spot_wrapper(&golden).unwrap();
        // Permuted: body FIRST, symbol LAST, createTime present, plus
        // unknown fields of every wire type (varint, LEN, fixed64,
        // fixed32) and an unknown field number past the oneof range.
        let mut p = Vec::new();
        enc::len_field(&mut p, 315, &body);
        enc::varint_field(&mut p, 7, 99);
        enc::tag(&mut p, 9, core_parse::PB_WT_I64);
        p.extend_from_slice(&7u64.to_le_bytes());
        enc::tag(&mut p, 10, core_parse::PB_WT_I32);
        p.extend_from_slice(&7u32.to_le_bytes());
        enc::len_field(&mut p, 999, b"future field");
        enc::varint_field(&mut p, 6, enc::SEND_TIME);
        enc::len_field(&mut p, 1, enc::CHANNEL_BOOK);
        enc::len_field(&mut p, 3, b"BTCUSDT");
        let q = parse_spot_wrapper(&p).expect("permuted wrapper");
        assert_eq!(q.channel, g.channel);
        assert_eq!(q.symbol(&p), g.symbol(&golden));
        assert_eq!(q.venue_time_ms(), g.venue_time_ms());
        assert_eq!(
            parse_book_ticker_body_view(q.body(&p)),
            parse_book_ticker_body_view(g.body(&golden)),
            "identical decode"
        );
        // createTime wins over sendTime when present.
        enc::varint_field(&mut p, 5, enc::SEND_TIME - 4);
        assert_eq!(parse_spot_wrapper(&p).unwrap().venue_time_ms(), enc::SEND_TIME - 4);
        // Body fields permuted + an unknown body field.
        let mut b2 = Vec::new();
        enc::len_field(&mut b2, 5, b"81721676217");
        enc::len_field(&mut b2, 4, b"0.33336356");
        enc::varint_field(&mut b2, 42, 1);
        enc::len_field(&mut b2, 3, b"80535.89");
        enc::len_field(&mut b2, 2, b"0.380497");
        enc::len_field(&mut b2, 1, b"80535.88");
        assert_eq!(parse_book_ticker_body_view(&b2), parse_book_ticker_body_view(&body));
    }

    #[test]
    fn wrapper_failure_modes() {
        let body = enc::book_body();
        // No symbol.
        let mut f = Vec::new();
        enc::len_field(&mut f, 315, &body);
        assert!(parse_spot_wrapper(&f).is_none());
        // Empty symbol.
        let mut f = Vec::new();
        enc::len_field(&mut f, 3, b"");
        enc::len_field(&mut f, 315, &body);
        assert!(parse_spot_wrapper(&f).is_none());
        // No body.
        let mut f = Vec::new();
        enc::len_field(&mut f, 3, b"BTCUSDT");
        assert!(parse_spot_wrapper(&f).is_none());
        // Two bodies.
        let mut f = Vec::new();
        enc::len_field(&mut f, 3, b"BTCUSDT");
        enc::len_field(&mut f, 315, &body);
        enc::len_field(&mut f, 314, &enc::deals_body());
        assert!(parse_spot_wrapper(&f).is_none());
        // Known field, wrong wire type.
        let mut f = Vec::new();
        enc::varint_field(&mut f, 3, 1);
        enc::len_field(&mut f, 315, &body);
        assert!(parse_spot_wrapper(&f).is_none());
        let mut f = Vec::new();
        enc::len_field(&mut f, 3, b"BTCUSDT");
        enc::len_field(&mut f, 6, b"x");
        enc::len_field(&mut f, 315, &body);
        assert!(parse_spot_wrapper(&f).is_none());
        // Truncated.
        let g = enc::wrapper(enc::CHANNEL_BOOK, b"BTCUSDT", 315, &body);
        assert!(parse_spot_wrapper(&g[..g.len() - 1]).is_none());
        assert!(parse_spot_wrapper(&[]).is_none());
        // Spans are empty against a foreign buffer.
        let w = parse_spot_wrapper(&g).unwrap();
        assert_eq!(w.symbol(&[]), b"");
        assert_eq!(w.body(&[]), b"");
    }

    /// proto3 omits an empty string: an emptied side arrives as two
    /// missing fields (or two empty strings) and is a 0/0 side, not a
    /// parse error; a body with NO side is drift and rejects.
    #[test]
    fn book_body_tolerates_an_empty_side_but_not_an_empty_book() {
        let mut ask_only = Vec::new();
        enc::len_field(&mut ask_only, 3, b"80535.89");
        enc::len_field(&mut ask_only, 4, b"0.5");
        enc::len_field(&mut ask_only, 5, b"7");
        let t = parse_book_ticker_body_view(&ask_only).expect("a one-sided book parses");
        assert_eq!((t.bid_px_1e6, t.bid_qty_1e6), (0, 0));
        assert_eq!((t.ask_px_1e6, t.ask_qty_1e6, t.version), (80_535_890_000, 500_000, 7));
        let mut bid_empty_strings = Vec::new();
        enc::len_field(&mut bid_empty_strings, 1, b"");
        enc::len_field(&mut bid_empty_strings, 2, b"");
        enc::len_field(&mut bid_empty_strings, 3, b"2");
        enc::len_field(&mut bid_empty_strings, 4, b"1");
        let t = parse_book_ticker_body_view(&bid_empty_strings).expect("explicit empties = absent");
        assert_eq!((t.bid_px_1e6, t.ask_px_1e6), (0, 2_000_000));
        let mut none = Vec::new();
        enc::len_field(&mut none, 5, b"7");
        assert!(parse_book_ticker_body_view(&none).is_none(), "no side at all is drift");
        assert!(parse_book_ticker_body_view(&[]).is_none());
    }

    #[test]
    fn book_body_failure_modes() {
        // Missing a required field.
        let mut b = Vec::new();
        enc::len_field(&mut b, 1, b"1.0");
        enc::len_field(&mut b, 2, b"1.0");
        enc::len_field(&mut b, 3, b"1.0");
        assert!(parse_book_ticker_body_view(&b).is_none());
        // Partial scans reject (exponent, junk, negative, bare dot).
        for bad in [&b"1e5"[..], b"1.0x", b"-1.0", b".5", b"5.", b""] {
            let mut b = Vec::new();
            enc::len_field(&mut b, 1, bad);
            enc::len_field(&mut b, 2, b"1");
            enc::len_field(&mut b, 3, b"1");
            enc::len_field(&mut b, 4, b"1");
            assert!(parse_book_ticker_body_view(&b).is_none(), "{:?}", core::str::from_utf8(bad));
        }
        // version present but not digits.
        let mut b = Vec::new();
        enc::len_field(&mut b, 1, b"1");
        enc::len_field(&mut b, 2, b"1");
        enc::len_field(&mut b, 3, b"1");
        enc::len_field(&mut b, 4, b"1");
        let mut ok = b.clone();
        enc::len_field(&mut b, 5, b"12a");
        assert!(parse_book_ticker_body_view(&b).is_none());
        // version absent is fine (0).
        assert_eq!(parse_book_ticker_body_view(&ok).unwrap().version, 0);
        // A price as a varint is malformed.
        enc::varint_field(&mut ok, 1, 5);
        assert!(parse_book_ticker_body_view(&ok).is_none());
    }

    #[test]
    fn golden_deals_frame_decodes_exactly() {
        let frame = enc::wrapper(b"spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT", b"BTCUSDT", 314, &enc::deals_body());
        let w = parse_spot_wrapper(&frame).unwrap();
        assert_eq!(w.channel, MexcChannel::SpotDeals);
        let mut walk = MexcDealsWalk::new(w.body(&frame));
        let d0 = parse_deal_item_view(walk.next_item().unwrap()).unwrap();
        assert_eq!(d0.px_1e6, 80_535_880_000);
        assert_eq!(d0.qty_1e6, 13_620, "0.01362099 truncates at 1e-6");
        assert_eq!(d0.side, DEAL_SIDE_SELL);
        assert_eq!(d0.signed_qty_1e6(), -13_620);
        assert_eq!(d0.time_ms, enc::DEAL_TIME);
        assert_eq!(d0.trade_seq, 730_292_425_431_437_318, "Q-MX1 leading digits");
        let d1 = parse_deal_item_view(walk.next_item().unwrap()).unwrap();
        assert_eq!(d1.side, DEAL_SIDE_BUY);
        assert_eq!(d1.qty_1e6, 1_234);
        assert_eq!(d1.trade_seq, 730_292_425_431_437_320);
        assert!(walk.next_item().is_none(), "eventType skipped, walk ends");
        assert!(!walk.is_malformed());
    }

    #[test]
    fn deals_walk_and_item_failure_modes() {
        // Structurally malformed body stops the walk and says so.
        let mut body = Vec::new();
        enc::len_field(&mut body, 1, &enc::deal_item(b"1", b"1", 1, 1, b"1"));
        body.extend_from_slice(&[0x0A, 0x7F]); // item promising 127 bytes
        let mut w = MexcDealsWalk::new(&body);
        assert!(w.next_item().is_some());
        assert!(w.next_item().is_none());
        assert!(w.is_malformed());
        // deals item as a varint is malformed.
        let mut body = Vec::new();
        enc::varint_field(&mut body, 1, 3);
        let mut w = MexcDealsWalk::new(&body);
        assert!(w.next_item().is_none());
        assert!(w.is_malformed());
        // Empty body: clean end.
        let mut w = MexcDealsWalk::new(&[]);
        assert!(w.next_item().is_none());
        assert!(!w.is_malformed());
        // Items: bad tradeType, missing tradeType, missing qty.
        assert!(parse_deal_item_view(&enc::deal_item(b"1", b"1", 3, 1, b"1")).is_none());
        let mut it = Vec::new();
        enc::len_field(&mut it, 1, b"1");
        enc::len_field(&mut it, 2, b"1");
        assert!(parse_deal_item_view(&it).is_none(), "tradeType required");
        let mut it = Vec::new();
        enc::len_field(&mut it, 1, b"1");
        enc::varint_field(&mut it, 3, 1);
        assert!(parse_deal_item_view(&it).is_none(), "quantity required");
        // Optional fields absent → 0.
        let mut it = Vec::new();
        enc::len_field(&mut it, 1, b"2.5");
        enc::len_field(&mut it, 2, b"3");
        enc::varint_field(&mut it, 3, 1);
        let d = parse_deal_item_view(&it).unwrap();
        assert_eq!((d.time_ms, d.trade_seq), (0, 0));
    }

    #[test]
    fn trade_id_seq_takes_the_leading_digits() {
        assert_eq!(trade_id_seq(b"730292425431437318X0_730292425431437319X0"), 730_292_425_431_437_318);
        assert_eq!(trade_id_seq(b"42"), 42);
        assert_eq!(trade_id_seq(b"X0_1"), 0, "no leading digit");
        assert_eq!(trade_id_seq(b""), 0);
        assert_eq!(trade_id_seq(b"99999999999999999999999X"), 0, "overflow = absent");
    }

    const ACK_OK_ECHO: &[u8] = br#"{"id":0,"code":0,"msg":"spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT,spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT"}"#;
    const ACK_MIXED: &[u8] = "{\"id\":0,\"code\":0,\"msg\":\"Subscribed successful! [spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT,spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT]. Not Subscribed successfully! [spot@public.increase.depth.v3.api.pb@BTCUSDT, spot@public.aggre.deals.v3.api.pb@10ms@ETHUSDT].  Reason： Blocked! \"}".as_bytes();

    #[test]
    fn sub_ack_success_only_lists_nothing() {
        let a = parse_sub_ack_view(ACK_OK_ECHO).unwrap();
        assert_eq!(a.code, 0);
        assert!(!a.has_failures(ACK_OK_ECHO));
        let b = br#"{"id":0,"code":0,"msg":"Subscribed successful! [spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT]"}"#;
        assert!(!parse_sub_ack_view(b).unwrap().has_failures(b));
    }

    #[test]
    fn sub_ack_walks_the_failed_params() {
        let a = parse_sub_ack_view(ACK_MIXED).unwrap();
        assert_eq!(a.code, 0);
        assert!(a.has_failures(ACK_MIXED));
        let mut w = a.failed_params(ACK_MIXED);
        let p0 = w.next_param().unwrap();
        assert_eq!(p0, b"spot@public.increase.depth.v3.api.pb@BTCUSDT");
        assert_eq!(extract_param_symbol(p0), Some(&b"BTCUSDT"[..]));
        assert_eq!(extract_param_channel(p0), None, "the Blocked! channel is not ours");
        let p1 = w.next_param().unwrap();
        assert_eq!(p1, b"spot@public.aggre.deals.v3.api.pb@10ms@ETHUSDT", "spaces trimmed");
        assert_eq!(extract_param_symbol(p1), Some(&b"ETHUSDT"[..]));
        assert_eq!(extract_param_channel(p1), Some(MexcChannel::SpotDeals));
        assert!(w.next_param().is_none());
        assert_eq!(
            extract_param_channel(b"spot@public.aggre.bookTicker.v3.api.pb@100ms@X"),
            Some(MexcChannel::SpotBookTicker)
        );
    }

    /// The refusal EXACTLY as measured live 2026-09-23: code stays 0, no
    /// `Subscribed successful!` prefix, two spaces and a FULL-WIDTH colon
    /// before the reason (an unknown symbol reads `Blocked!` too).
    #[test]
    fn sub_ack_parses_the_live_refusal_text() {
        let live = "{\"id\":0,\"code\":0,\"msg\":\"Not Subscribed successfully! [spot@public.aggre.bookTicker.v3.api.pb@10ms@NOPEUSDT,spot@public.aggre.deals.v3.api.pb@10ms@NOPEUSDT].  Reason\u{ff1a} Blocked! \"}".as_bytes();
        assert_eq!(classify_spot(live, false), MexcSpotKind::SubAck);
        let a = parse_sub_ack_view(live).unwrap();
        assert_eq!(a.code, 0);
        let mut w = a.failed_params(live);
        let p0 = w.next_param().unwrap();
        assert_eq!(extract_param_channel(p0), Some(MexcChannel::SpotBookTicker));
        assert_eq!(extract_param_symbol(p0), Some(&b"NOPEUSDT"[..]));
        let p1 = w.next_param().unwrap();
        assert_eq!(extract_param_channel(p1), Some(MexcChannel::SpotDeals));
        assert_eq!(extract_param_symbol(p1), Some(&b"NOPEUSDT"[..]));
        assert!(w.next_param().is_none());
    }

    #[test]
    fn sub_ack_failure_modes() {
        // Whole-request refusal keeps its code.
        let e = br#"{"id":0,"code":30001,"msg":"invalid params"}"#;
        assert_eq!(parse_sub_ack_view(e).unwrap().code, 30001);
        assert!(parse_sub_ack_view(br#"{"id":0,"msg":"x"}"#).is_none(), "code required");
        assert!(parse_sub_ack_view(br#"{"id":0,"code":0}"#).is_none(), "msg required");
        assert!(parse_sub_ack_view(br#"{"id":0,"code":0,"msg":7}"#).is_none(), "msg is a string");
        assert!(parse_sub_ack_view(br#"{"id":0,"code":0,"msg":"x"#).is_none(), "unterminated");
        // The failed list opened but never closed / never opened.
        assert!(parse_sub_ack_view(br#"{"code":0,"msg":"Not Subscribed successfully! [a,b"}"#).is_none());
        assert!(parse_sub_ack_view(br#"{"code":0,"msg":"Not Subscribed successfully! a"}"#).is_none());
        // Empty entries skipped.
        let s = br#"{"code":0,"msg":"Not Subscribed successfully! [ , ,x@Y,]"}"#;
        let a = parse_sub_ack_view(s).unwrap();
        let mut w = a.failed_params(s);
        assert_eq!(w.next_param(), Some(&b"x@Y"[..]));
        assert_eq!(w.next_param(), None);
        assert_eq!(extract_param_symbol(b"no-at-sign"), None);
        assert_eq!(extract_param_symbol(b"trailing@"), None);
    }

    #[test]
    fn write_spot_subscribe_renders_one_frame() {
        let mut t = MexcSymbolTable::new();
        t.insert(b"BTCUSDT", 1).unwrap();
        t.insert(b"AAPLXUSDT", 2).unwrap();
        let mut buf = [0u8; 1024];
        let n = write_spot_subscribe(&mut buf, &t).unwrap();
        assert_eq!(
            &buf[..n],
            br#"{"method":"SUBSCRIPTION","params":["spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT","spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT","spot@public.aggre.bookTicker.v3.api.pb@10ms@AAPLXUSDT","spot@public.aggre.deals.v3.api.pb@10ms@AAPLXUSDT"]}"# as &[u8]
        );
        let mut tiny = [0u8; 16];
        assert!(write_spot_subscribe(&mut tiny, &t).is_none());
    }
}

#[cfg(test)]
mod proptests {
    use super::enc;
    use super::*;
    use proptest::prelude::*;
    use super::views::*;

    fn dec(v: u64) -> String {
        format!("{}.{:06}", v / 1_000_000, v % 1_000_000)
    }

    proptest! {
        #[test]
        fn wrapper_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..=400)) {
            if let Some(w) = parse_spot_wrapper(&buf) {
                prop_assert!(w.sym_start < w.sym_end && w.sym_end <= buf.len());
                prop_assert!(w.body_start <= w.body_end && w.body_end <= buf.len());
            }
        }

        #[test]
        fn book_body_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..=300)) {
            let _ = parse_book_ticker_body_view(&buf);
        }

        #[test]
        fn deals_walk_and_items_never_panic(buf in proptest::collection::vec(any::<u8>(), 0..=400)) {
            let mut w = MexcDealsWalk::new(&buf);
            let mut n = 0;
            while let Some(item) = w.next_item() {
                let _ = parse_deal_item_view(item);
                n += 1;
                prop_assert!(n <= buf.len(), "the walk always progresses");
            }
        }

        #[test]
        fn ack_and_classify_never_panic(buf in proptest::collection::vec(any::<u8>(), 0..=300), binary in any::<bool>()) {
            let _ = classify_spot(&buf, binary);
            if let Some(a) = parse_sub_ack_view(&buf) {
                let mut w = a.failed_params(&buf);
                while let Some(p) = w.next_param() {
                    prop_assert!(!p.is_empty());
                    let _ = extract_param_symbol(p);
                    let _ = extract_param_channel(p);
                }
            }
            let _ = trade_id_seq(&buf);
        }

        /// The PB bookTicker round-trips through the reference encoder
        /// in ANY field order, with unknown fields interleaved.
        #[test]
        fn book_ticker_roundtrips(
            bp in 0u64..1_000_000_000_000u64,
            bq in 0u64..1_000_000_000_000u64,
            ap in 0u64..1_000_000_000_000u64,
            aq in 0u64..1_000_000_000_000u64,
            version in any::<u64>(),
            send in 1u64..4_000_000_000_000u64,
            rot in 0usize..6,
            body_rot in 0usize..6,
        ) {
            let mut body_fields: Vec<Vec<u8>> = Vec::new();
            for (no, v) in [(1u32, bp), (2, bq), (3, ap), (4, aq)] {
                let mut f = Vec::new();
                enc::len_field(&mut f, no, dec(v).as_bytes());
                body_fields.push(f);
            }
            let mut vf = Vec::new();
            enc::len_field(&mut vf, 5, version.to_string().as_bytes());
            body_fields.push(vf);
            let mut unk = Vec::new();
            enc::varint_field(&mut unk, 6, send);
            body_fields.push(unk);
            body_fields.rotate_left(body_rot);
            let body: Vec<u8> = body_fields.concat();

            let mut parts: Vec<Vec<u8>> = Vec::new();
            let mut p = Vec::new(); enc::len_field(&mut p, 1, enc::CHANNEL_BOOK); parts.push(p);
            let mut p = Vec::new(); enc::len_field(&mut p, 3, b"BTCUSDT"); parts.push(p);
            let mut p = Vec::new(); enc::varint_field(&mut p, 6, send); parts.push(p);
            let mut p = Vec::new(); enc::len_field(&mut p, 315, &body); parts.push(p);
            let mut p = Vec::new(); enc::varint_field(&mut p, 77, 1); parts.push(p);
            let mut p = Vec::new(); enc::len_field(&mut p, 2, b"?"); parts.push(p);
            parts.rotate_left(rot);
            let frame: Vec<u8> = parts.concat();

            let w = parse_spot_wrapper(&frame).unwrap();
            prop_assert_eq!(w.channel, MexcChannel::SpotBookTicker);
            prop_assert_eq!(w.symbol(&frame), b"BTCUSDT");
            prop_assert_eq!(w.venue_time_ms(), send);
            let b = parse_book_ticker_body_view(w.body(&frame)).unwrap();
            prop_assert_eq!(b.bid_px_1e6, bp as i64);
            prop_assert_eq!(b.bid_qty_1e6, bq as i64);
            prop_assert_eq!(b.ask_px_1e6, ap as i64);
            prop_assert_eq!(b.ask_qty_1e6, aq as i64);
            prop_assert_eq!(b.version, version);
        }
    }
}
