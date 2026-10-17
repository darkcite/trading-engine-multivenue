// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The user channel (`wss://ws-subscriptions-clob.polymarket.com/ws/user`,
//! spec §F): its `trade` and `order` events, scanned in place.
//!
//! * A frame is one JSON object or an array of them; unknown
//!   `event_type`s are skipped (counted by the caller); the text `PONG`
//!   is the keepalive's answer and never reaches this scanner.
//! * Fields are read at the object's **top level** only — a `trade`
//!   carries `maker_orders[]` whose entries repeat `price`, `side`,
//!   `asset_id` and `owner`, so a key search over the whole object could
//!   read a maker's price as ours.
//! * A `trade` is the fill record (LAW E-5): one event per status change
//!   of the same trade `id`; the first `MATCHED` (or
//!   `MATCHED_NOT_BROADCASTED`) is the fill, `CONFIRMED` closes it,
//!   `FAILED` reverses it. Its top-level `size`, `price` and `side` are the
//!   taker's (`trader_side` says whether that is us).
//! * A frame that does not scan is an error the gateway counts and halts
//!   on (fail-closed: a fill must never be lost silently).

use core_parse::{scan_price_1e6, skip_json_value, skip_string, skip_ws};

/// One event, as a 128 B record.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UserEvt {
    /// `trade`: `taker_order_id`; `order`: `id` — the order hash.
    pub order_id: [u8; 32],
    /// `trade`: the trade `id`'s text (a UUID); zero for an `order`.
    pub trade_id: [u8; TRADE_ID_MAX],
    /// `trade`: `size` (shares ×1e6); `order`: `size_matched` (cumulative).
    pub size_1e6: i64,
    /// `price` ×1e6.
    pub price_1e6: i64,
    /// `timestamp` (ms).
    pub ts_ms: u64,
    /// FNV-1a 64 of `asset_id`'s digits (the gateway's token table keys
    /// the same hash).
    pub asset_hash: u64,
    /// `fee_rate_bps` (0 when absent).
    pub fee_bps: u32,
    /// [`KIND_TRADE`] or [`KIND_ORDER`].
    pub kind: u8,
    /// `trade`: `TS_*`; `order`: `OT_*`.
    pub status: u8,
    /// 0 BUY, 1 SELL.
    pub side: u8,
    /// `trade`: [`ROLE_TAKER`] / [`ROLE_MAKER`] / 0 when absent.
    pub role: u8,
    /// Bytes of `trade_id` used.
    pub trade_id_len: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 11],
}

const _: () = assert!(core::mem::size_of::<UserEvt>() == 128);

/// The longest trade id kept (a UUID is 36 B).
pub const TRADE_ID_MAX: usize = 40;

/// `event_type: "trade"`.
pub const KIND_TRADE: u8 = 1;
/// `event_type: "order"`.
pub const KIND_ORDER: u8 = 2;

/// Trade status `MATCHED` — the fill.
pub const TS_MATCHED: u8 = 1;
/// Trade status `MATCHED_NOT_BROADCASTED` — also the fill.
pub const TS_MATCHED_NOT_BROADCASTED: u8 = 2;
/// Trade status `MINED`.
pub const TS_MINED: u8 = 3;
/// Trade status `CONFIRMED` — final.
pub const TS_CONFIRMED: u8 = 4;
/// Trade status `RETRYING`.
pub const TS_RETRYING: u8 = 5;
/// Trade status `FAILED` — final, the fill reversed.
pub const TS_FAILED: u8 = 6;

/// Order event `PLACEMENT`.
pub const OT_PLACEMENT: u8 = 1;
/// Order event `UPDATE` (some or all matched).
pub const OT_UPDATE: u8 = 2;
/// Order event `CANCELLATION` (the remainder canceled).
pub const OT_CANCELLATION: u8 = 3;

/// `trader_side: "TAKER"`.
pub const ROLE_TAKER: u8 = 1;
/// `trader_side: "MAKER"`.
pub const ROLE_MAKER: u8 = 2;

impl UserEvt {
    /// An empty record — the scanner's output buffers start as these.
    pub const BLANK: Self = Self {
        order_id: [0; 32],
        trade_id: [0; TRADE_ID_MAX],
        size_1e6: 0,
        price_1e6: 0,
        ts_ms: 0,
        asset_hash: 0,
        fee_bps: 0,
        kind: 0,
        status: 0,
        side: 0,
        role: 0,
        trade_id_len: 0,
        _pad: [0; 11],
    };
}

/// Why a frame did not scan.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScanErr {
    /// Not a JSON object or array of objects.
    Json,
    /// A `trade` or `order` event without a required field, or with one
    /// that does not read.
    Field,
    /// More events than `out` holds.
    Full,
}

/// What one frame held.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Scanned {
    /// Events written to `out`.
    pub events: usize,
    /// Objects of another `event_type`, skipped.
    pub skipped: usize,
}

/// Scan one user-channel frame into `out`.
///
/// # Errors
/// [`ScanErr`] — the gateway counts it and halts the arm's growth.
pub fn scan_user_frame(buf: &[u8], out: &mut [UserEvt]) -> Result<Scanned, ScanErr> {
    let mut r = Scanned::default();
    let p = skip_ws(buf, 0);
    if p >= buf.len() {
        return Err(ScanErr::Json);
    }
    if buf[p] == b'{' {
        let end = skip_json_value(buf, p).ok_or(ScanErr::Json)?;
        one_object(&buf[p..end], out, &mut r)?;
        return Ok(r);
    }
    if buf[p] != b'[' {
        return Err(ScanErr::Json);
    }
    let mut i = p + 1;
    loop {
        i = skip_ws(buf, i);
        if i >= buf.len() {
            return Err(ScanErr::Json);
        }
        match buf[i] {
            b']' => return Ok(r),
            b',' => i += 1,
            b'{' => {
                let end = skip_json_value(buf, i).ok_or(ScanErr::Json)?;
                one_object(&buf[i..end], out, &mut r)?;
                i = end;
            }
            _ => return Err(ScanErr::Json),
        }
    }
}

/// The top-level value spans of the fields one event needs.
#[derive(Default)]
struct Spans {
    event_type: Option<(usize, usize)>,
    id: Option<(usize, usize)>,
    taker_order_id: Option<(usize, usize)>,
    asset_id: Option<(usize, usize)>,
    side: Option<(usize, usize)>,
    size: Option<(usize, usize)>,
    size_matched: Option<(usize, usize)>,
    price: Option<(usize, usize)>,
    status: Option<(usize, usize)>,
    kind: Option<(usize, usize)>,
    trader_side: Option<(usize, usize)>,
    timestamp: Option<(usize, usize)>,
    fee_rate_bps: Option<(usize, usize)>,
}

fn one_object(obj: &[u8], out: &mut [UserEvt], r: &mut Scanned) -> Result<(), ScanErr> {
    let sp = members(obj)?;
    let et = text(obj, sp.event_type);
    let kind = match et {
        Some(b"trade") => KIND_TRADE,
        Some(b"order") => KIND_ORDER,
        _ => {
            r.skipped += 1;
            return Ok(());
        }
    };
    if r.events >= out.len() {
        return Err(ScanErr::Full);
    }
    let mut e = UserEvt::BLANK;
    e.kind = kind;
    e.side = match text(obj, sp.side) {
        Some(b"BUY") => 0,
        Some(b"SELL") => 1,
        _ => return Err(ScanErr::Field),
    };
    e.price_1e6 = decimal(obj, sp.price).ok_or(ScanErr::Field)?;
    e.ts_ms = match text(obj, sp.timestamp) {
        Some(t) => match core_parse::scan_u64(t, 0) {
            Some((v, n)) if n == t.len() => v,
            _ => return Err(ScanErr::Field),
        },
        None => return Err(ScanErr::Field),
    };
    e.asset_hash = fnv1a(text(obj, sp.asset_id).ok_or(ScanErr::Field)?);
    e.fee_bps = match text(obj, sp.fee_rate_bps) {
        Some(t) => match core_parse::scan_u64(t, 0) {
            Some((v, _)) => v.min(u64::from(u32::MAX)) as u32,
            None => 0,
        },
        None => 0,
    };
    if kind == KIND_TRADE {
        e.order_id = crate::num::hex32(text(obj, sp.taker_order_id).ok_or(ScanErr::Field)?)
            .ok_or(ScanErr::Field)?;
        let id = text(obj, sp.id).ok_or(ScanErr::Field)?;
        if id.is_empty() || id.len() > TRADE_ID_MAX {
            return Err(ScanErr::Field);
        }
        // COPY: the trade id's ≤ 40 B text into the record — the key the
        // gateway dedups a trade's status changes by; per trade event.
        e.trade_id[..id.len()].copy_from_slice(id);
        e.trade_id_len = id.len() as u8;
        e.size_1e6 = decimal(obj, sp.size).ok_or(ScanErr::Field)?;
        e.status =
            trade_status(text(obj, sp.status).ok_or(ScanErr::Field)?).ok_or(ScanErr::Field)?;
        e.role = match text(obj, sp.trader_side) {
            Some(b"TAKER") => ROLE_TAKER,
            Some(b"MAKER") => ROLE_MAKER,
            _ => 0,
        };
    } else {
        e.order_id =
            crate::num::hex32(text(obj, sp.id).ok_or(ScanErr::Field)?).ok_or(ScanErr::Field)?;
        e.size_1e6 = decimal(obj, sp.size_matched).ok_or(ScanErr::Field)?;
        e.status = match text(obj, sp.kind) {
            Some(b"PLACEMENT") => OT_PLACEMENT,
            Some(b"UPDATE") => OT_UPDATE,
            Some(b"CANCELLATION") => OT_CANCELLATION,
            _ => return Err(ScanErr::Field),
        };
    }
    out[r.events] = e;
    r.events += 1;
    Ok(())
}

/// Walk `obj`'s top-level members, recording the spans of the keys
/// [`Spans`] names; every value is skipped whole, so a nested key never
/// matches.
fn members(obj: &[u8]) -> Result<Spans, ScanErr> {
    let mut sp = Spans::default();
    let mut i = 1usize; // past '{'
    loop {
        i = skip_ws(obj, i);
        if i >= obj.len() {
            return Err(ScanErr::Json);
        }
        match obj[i] {
            b'}' => return Ok(sp),
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {}
            _ => return Err(ScanErr::Json),
        }
        let kend = skip_string(obj, i + 1).ok_or(ScanErr::Json)?;
        let key = &obj[i + 1..kend - 1];
        let mut v = skip_ws(obj, kend);
        if v >= obj.len() || obj[v] != b':' {
            return Err(ScanErr::Json);
        }
        v = skip_ws(obj, v + 1);
        let vend = skip_json_value(obj, v).ok_or(ScanErr::Json)?;
        let span = Some((v, vend));
        match key {
            b"event_type" => sp.event_type = span,
            b"id" => sp.id = span,
            b"taker_order_id" => sp.taker_order_id = span,
            b"asset_id" => sp.asset_id = span,
            b"side" => sp.side = span,
            b"size" => sp.size = span,
            b"size_matched" => sp.size_matched = span,
            b"price" => sp.price = span,
            b"status" => sp.status = span,
            b"type" => sp.kind = span,
            b"trader_side" => sp.trader_side = span,
            b"timestamp" => sp.timestamp = span,
            b"fee_rate_bps" => sp.fee_rate_bps = span,
            _ => {}
        }
        i = vend;
    }
}

/// A string value's bytes (without its quotes); `None` for a non-string.
fn text(obj: &[u8], span: Option<(usize, usize)>) -> Option<&[u8]> {
    let (s, e) = span?;
    if e < s + 2 || obj[s] != b'"' {
        return None;
    }
    Some(&obj[s + 1..e - 1])
}

/// A quoted decimal ×1e6.
fn decimal(obj: &[u8], span: Option<(usize, usize)>) -> Option<i64> {
    let t = text(obj, span)?;
    match scan_price_1e6(t, 0) {
        Some((v, n)) if n == t.len() => Some(v),
        _ => None,
    }
}

fn trade_status(t: &[u8]) -> Option<u8> {
    let t = t.strip_prefix(b"TRADE_STATUS_").unwrap_or(t);
    match t {
        b"MATCHED" => Some(TS_MATCHED),
        b"MATCHED_NOT_BROADCASTED" => Some(TS_MATCHED_NOT_BROADCASTED),
        b"MINED" => Some(TS_MINED),
        b"CONFIRMED" => Some(TS_CONFIRMED),
        b"RETRYING" => Some(TS_RETRYING),
        b"FAILED" => Some(TS_FAILED),
        _ => None,
    }
}

/// FNV-1a 64 — the token table's key for an `asset_id`'s digits.
#[inline]
#[must_use]
pub fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0usize;
    while i < b.len() {
        h ^= u64::from(b[i]);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    h
}

#[cfg(test)]
mod tests;
