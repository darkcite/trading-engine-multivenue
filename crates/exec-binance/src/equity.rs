// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX10 — Binance Stocks (`/sapi/v1/equity/*`; O-BX1, O-BX16).** The
//! venue's order id and the scanners of its answers and its order stream.
//! Gateway thread (the scanners) and the cold boot (the symbol's rules).
//!
//! Binance Stocks is a BROKER path (Nest Trading → Alpaca), not a matching
//! engine: a place is acknowledged with `status: "S"` and a UUID
//! (`orderId`), the broker's own acknowledgement (`ACCEPTED`) follows, and
//! the order then rests — DAY or GTC, no IoC — until it fills, is
//! cancelled (by the venue's `orderId` only) or expires at its session's
//! close. Nothing here is a matching engine's word.
//!
//! * [`EqOrderId`] — the venue's UUID, held whole (36 bytes), never parsed
//!   into a number: [`eq_key`] is its FNV-1a key, the open-order table's
//!   `venue_oid` (never 0), and the full bytes decide every match.
//! * The answers, each read whole (the one-reading law: every key the
//!   reader reads named once, nothing after the value): the place's
//!   acknowledgement ([`scan_eq_ack`]), the cancel's ([`scan_eq_cancel`]),
//!   the cancel-all's ([`scan_eq_cancel_all`]), one order's detail with its
//!   executions ([`scan_eq_detail`]), the open orders ([`scan_eq_open`]),
//!   a page of executions ([`scan_eq_trades`]).
//! * The order stream's `orderReport` ([`scan_order_report`]): its numbers
//!   are JSON NUMBERS, read from their decimal text into ×1e6
//!   ([`json_num_1e6`], exponent form included) — never through `f64`;
//!   its words are lowercase; its base asset is `EQ_`-prefixed; it carries
//!   no client id and no execution id. The stream is the TRIGGER only:
//!   the order's detail books its fills (each execution once, by its
//!   `executionId`), its fee and its state.
//! * The symbol's rules (`market/exchangeInfo`; cold, boot): [`EqSymbolRow`],
//!   [`scan_eq_symbol`].
//!
//! Shapes: Binance's Stocks REST and WebSocket docs and its Go connector's
//! `clients/stocks` models (the field names and types, 2026-10-11), pinned
//! where the read-only probe could (K15, ruling S-1: plan doc BX10 §1a) —
//! the rules, the history pages' envelope, `order/detail`'s `486218` for an
//! order it does not know; the order rows, the detail's executions and the
//! stream's report are the docs' until the operator's battery places one.

use crate::json::{bool_of, dec_1e6, dec_1e9, is_null, str_of, u64_of, Elems, Pairs};
use crate::rest::{ends_whole, ArrErr};
use crate::userstream::{ScanErr, Span};
use core_types::Side;

/// The venue's order id's length: a UUID (`8-4-4-4-12`).
pub const EQ_ID_LEN: usize = 36;

/// **The venue's order id** — its UUID, whole. `n` is 0 (none yet: the
/// place's answer has not named it) or [`EQ_ID_LEN`].
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EqOrderId {
    b: [u8; EQ_ID_LEN],
    n: u8,
}

impl EqOrderId {
    /// No id yet.
    pub const NONE: Self = Self { b: [0; EQ_ID_LEN], n: 0 };

    /// `s` as an order id: `None` unless it is a UUID's shape — 36 bytes,
    /// hyphens at 8, 13, 18 and 23, hexadecimal digits elsewhere (it goes
    /// into a query: nothing else is rendered from it).
    #[must_use]
    pub fn of(s: &[u8]) -> Option<Self> {
        if !is_uuid(s) {
            return None;
        }
        let mut id = Self::NONE;
        // COPY: the venue's order id (36 B) into this value, which the
        // gateway then moves (≤ 64 B) into its per-order table — once per
        // order, when an answer first names it (never while one is held);
        // the answer is overwritten by the next request on its connection
        // — keeping a span into it was rejected.
        id.b.copy_from_slice(s);
        id.n = EQ_ID_LEN as u8;
        Some(id)
    }

    /// The id's bytes (empty: none).
    #[inline(always)]
    #[must_use]
    pub fn get(&self) -> &[u8] {
        &self.b[..self.n as usize]
    }

    /// An id is held.
    #[inline(always)]
    #[must_use]
    pub const fn is_some(&self) -> bool {
        self.n != 0
    }

    /// `s` names this id — held, and the same UUID whatever the case of
    /// its hexadecimal digits (an answer and the stream may spell it apart).
    #[inline]
    #[must_use]
    pub fn is(&self, s: &[u8]) -> bool {
        self.n != 0 && self.get().eq_ignore_ascii_case(s)
    }
}

/// A UUID's shape (module docs): 36 bytes, `-` at 8, 13, 18, 23, hex
/// digits elsewhere.
#[inline]
#[must_use]
pub fn is_uuid(s: &[u8]) -> bool {
    if s.len() != EQ_ID_LEN {
        return false;
    }
    let mut i = 0;
    while i < EQ_ID_LEN {
        let c = s[i];
        let ok = if i == 8 || i == 13 || i == 18 || i == 23 { c == b'-' } else { c.is_ascii_hexdigit() };
        if !ok {
            return false;
        }
        i += 1;
    }
    true
}

/// **An order id's key** — FNV-1a over its bytes, their letters folded to
/// lower case (one UUID however its hex is spelled), never 0: the
/// open-order table's `venue_oid` for an equity order (a key that matches
/// is then confirmed against the whole id, [`EqOrderId::is`]).
#[inline]
#[must_use]
pub fn eq_key(id: &[u8]) -> u64 {
    fnv(id, true)
}

/// **An execution id's key** — FNV-1a over its bytes as they are (an
/// execution id is the venue's own word, not a UUID: its case is its own),
/// never 0: the trade-id ring's key for an equity execution.
#[inline]
#[must_use]
pub fn exec_key(id: &[u8]) -> u64 {
    fnv(id, false)
}

/// FNV-1a, its letters folded to lower case (`fold`), never 0.
#[inline]
fn fnv(id: &[u8], fold: bool) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < id.len() {
        h ^= u64::from(if fold { id[i].to_ascii_lowercase() } else { id[i] });
        h = h.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    h | 1
}

/// The order's lifecycle (Binance Stocks' common definitions): `NEW` —
/// sent, not yet the broker's.
pub const EQ_S_NEW: u8 = 1;
/// `ACCEPTED`: the broker's acknowledgement.
pub const EQ_S_ACCEPTED: u8 = 2;
/// `PARTIALLY_FILLED`.
pub const EQ_S_PARTIAL: u8 = 3;
/// `FILLED` (terminal).
pub const EQ_S_FILLED: u8 = 4;
/// `CANCELED` (terminal).
pub const EQ_S_CANCELED: u8 = 5;
/// `EXPIRED` — a DAY order at its session's close (terminal).
pub const EQ_S_EXPIRED: u8 = 6;
/// `REJECTED` (terminal).
pub const EQ_S_REJECTED: u8 = 7;
/// A word this build does not know: no transition (the next read asks
/// again).
pub const EQ_S_OTHER: u8 = 8;

/// A status word, either case (REST says `FILLED`, the stream `filled`).
#[must_use]
pub fn eq_status_of(s: &[u8]) -> u8 {
    if eq_word(s, b"NEW") {
        EQ_S_NEW
    } else if eq_word(s, b"ACCEPTED") {
        EQ_S_ACCEPTED
    } else if eq_word(s, b"PARTIALLY_FILLED") {
        EQ_S_PARTIAL
    } else if eq_word(s, b"FILLED") {
        EQ_S_FILLED
    } else if eq_word(s, b"CANCELED") || eq_word(s, b"CANCELLED") {
        EQ_S_CANCELED
    } else if eq_word(s, b"EXPIRED") {
        EQ_S_EXPIRED
    } else if eq_word(s, b"REJECTED") {
        EQ_S_REJECTED
    } else {
        EQ_S_OTHER
    }
}

/// The status ends the order.
#[inline(always)]
#[must_use]
pub const fn eq_terminal(status: u8) -> bool {
    matches!(status, EQ_S_FILLED | EQ_S_CANCELED | EQ_S_EXPIRED | EQ_S_REJECTED)
}

/// `s` is `upper` in either case (ASCII).
#[inline]
fn eq_word(s: &[u8], upper: &[u8]) -> bool {
    s.len() == upper.len() && s.eq_ignore_ascii_case(upper)
}

/// `BUY` / `SELL`, either case, → the side's `u8`.
fn eq_side_of(s: &[u8]) -> Result<u8, ScanErr> {
    if eq_word(s, b"BUY") {
        Ok(Side::Bid as u8)
    } else if eq_word(s, b"SELL") {
        Ok(Side::Ask as u8)
    } else {
        Err(ScanErr::BadField)
    }
}

#[inline(always)]
fn malformed(_: ()) -> ScanErr {
    ScanErr::Malformed
}

#[inline(always)]
fn bad(_: ()) -> ScanErr {
    ScanErr::BadField
}

/// A key read twice — which copy would hold? Refused (the one-reading law).
#[inline(always)]
fn once(have: &mut u32, bit: u32) -> Result<(), ScanErr> {
    if *have & bit != 0 {
        return Err(ScanErr::Malformed);
    }
    *have |= bit;
    Ok(())
}

/// A decimal STRING ×1e6, or `null` (0, absent): the detail's nullable
/// figures (`avgFilledPrice` before a fill, `limitPrice` on a market
/// order).
#[inline]
fn dec_or_null(b: &[u8], v0: usize, v1: usize) -> Result<i64, ScanErr> {
    if is_null(b, v0, v1) {
        return Ok(0);
    }
    dec_1e6(b, v0, v1).map_err(bad)
}

/// A ticker's string — never empty (an answer naming no ticker names
/// none we could judge).
#[inline]
fn ticker_of(b: &[u8], v0: usize, v1: usize) -> Result<&[u8], ScanErr> {
    match str_of(b, v0, v1) {
        Ok(s) if !s.is_empty() => Ok(s),
        _ => Err(ScanErr::BadField),
    }
}

/// A value's text starts with `-` (after its quote, if quoted): a figure
/// that must not be negative is refused on its SIGN, never on its value —
/// `-0`, or a negative below the scale (`-0.0000009` at ×1e6), reads as 0
/// and would pass a check of the value (the parser review's F1).
#[inline]
fn signed(b: &[u8], v0: usize) -> bool {
    let q = usize::from(b.get(v0) == Some(&b'"'));
    b.get(v0 + q) == Some(&b'-')
}

/// A decimal STRING ×1e6, `null` reading 0 — never negative (a refusal,
/// never a clamp: a quantity, a price or a notional below 0 — `-0`
/// included — is no answer this build knows).
#[inline]
fn dec_nonneg_or_null(b: &[u8], v0: usize, v1: usize) -> Result<i64, ScanErr> {
    if signed(b, v0) {
        return Err(ScanErr::BadField);
    }
    dec_or_null(b, v0, v1)
}

/// A decimal ×1e9, never negative (a refusal on the sign, never a clamp).
#[inline]
fn dec9_nonneg(b: &[u8], v0: usize, v1: usize) -> Result<i64, ScanErr> {
    if signed(b, v0) {
        return Err(ScanErr::BadField);
    }
    dec_1e9(b, v0, v1).map_err(bad)
}

/// A JSON number ×1e6 that must not be negative: any `-` refuses it (a
/// tiny negative reads as 0 at ×1e6 — `-0.0000009` — and would pass a
/// sign check).
#[inline]
fn json_num_nonneg(b: &[u8], v0: usize, v1: usize) -> Result<i64, ScanErr> {
    if signed(b, v0) {
        return Err(ScanErr::BadField);
    }
    json_num_1e6(b, v0, v1).map_err(bad)
}

/// Two of the first `n` spans `at` names are the same id: a listing naming
/// one execution or order twice (which copy would hold?) — an order's UUID
/// whatever its case (`fold`), an execution's id as it is.
fn named_twice(body: &[u8], n: usize, fold: bool, at: impl Fn(usize) -> Span) -> bool {
    let mut i = 1;
    while i < n {
        let si = at(i).get(body);
        let mut j = 0;
        while j < i {
            let sj = at(j).get(body);
            if if fold { sj.eq_ignore_ascii_case(si) } else { sj == si } {
                return true;
            }
            j += 1;
        }
        i += 1;
    }
    false
}

/// **A JSON number ×1e6** (the stream's figures): `-?digits[.digits][e±d]`,
/// quoted or bare, read EXACTLY from its decimal text — never through
/// `f64` — truncated toward zero past the sixth decimal. Refused: a value
/// past `i64` at ×1e6, more than 30 significant digits, an exponent past
/// ±30, anything that is not the number whole.
#[allow(clippy::result_unit_err)]
pub fn json_num_1e6(b: &[u8], v0: usize, v1: usize) -> Result<i64, ()> {
    let s = if b.get(v0) == Some(&b'"') { str_of(b, v0, v1)? } else { b.get(v0..v1).ok_or(())? };
    let mut i = 0;
    let neg = s.first() == Some(&b'-');
    i += usize::from(neg);
    let (mut mant, mut digits, mut frac) = (0i128, 0u32, 0i32);
    let int0 = i;
    while i < s.len() && s[i].is_ascii_digit() {
        mant = mant * 10 + i128::from(s[i] - b'0');
        digits += u32::from(mant != 0);
        i += 1;
        // Refused as it passes 30 digits — never past `i128` first.
        if digits > 30 {
            return Err(());
        }
    }
    if i == int0 {
        return Err(());
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let f0 = i;
        while i < s.len() && s[i].is_ascii_digit() {
            mant = mant * 10 + i128::from(s[i] - b'0');
            digits += u32::from(mant != 0);
            frac += 1;
            i += 1;
            if digits > 30 {
                return Err(());
            }
        }
        if i == f0 {
            return Err(());
        }
    }
    let mut exp = 0i32;
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        i += 1;
        let eneg = match s.get(i) {
            Some(b'-') => {
                i += 1;
                true
            }
            Some(b'+') => {
                i += 1;
                false
            }
            _ => false,
        };
        let e0 = i;
        while i < s.len() && s[i].is_ascii_digit() {
            exp = exp * 10 + i32::from(s[i] - b'0');
            i += 1;
            if exp > 30 {
                return Err(());
            }
        }
        if i == e0 {
            return Err(());
        }
        if eneg {
            exp = -exp;
        }
    }
    if i != s.len() {
        return Err(());
    }
    // value = mant × 10^(exp − frac); ×1e6 → × 10^(exp − frac + 6).
    let p = exp - frac + 6;
    let v = if p >= 0 {
        let mut v = mant;
        let mut k = 0;
        while k < p {
            v = v.checked_mul(10).ok_or(())?;
            k += 1;
        }
        v
    } else {
        let mut v = mant;
        let mut k = 0;
        while k < -p && v != 0 {
            v /= 10;
            k += 1;
        }
        v
    };
    let v = i64::try_from(v).map_err(|_| ())?;
    Ok(if neg { -v } else { v })
}

// -------------------------------------------------------------------------
// The D5 keys (the order's statics)
// -------------------------------------------------------------------------

/// **The trading session an order is sent for** (D5: `equity_session`,
/// required, no default — no member exists to choose one, O-BX16).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EqSession {
    /// `RTH`: the regular session.
    Rth,
    /// `EXTENDED`: pre- and post-market.
    Extended,
    /// `24H`: the overnight session too (where `overnightSupported`).
    H24,
}

impl EqSession {
    /// From `exec.toml`'s word (or the battery's flag): `RTH`, `EXTENDED`
    /// or `24H`; `None` for anything else.
    #[must_use]
    pub fn from_word(w: &str) -> Option<Self> {
        match w {
            "RTH" => Some(Self::Rth),
            "EXTENDED" => Some(Self::Extended),
            "24H" => Some(Self::H24),
            _ => None,
        }
    }

    /// The venue's word (`tradingSession=`).
    #[must_use]
    pub const fn word(self) -> &'static [u8] {
        match self {
            Self::Rth => b"RTH",
            Self::Extended => b"EXTENDED",
            Self::H24 => b"24H",
        }
    }
}

/// The quote asset's longest name (`USDC`, `USDT`, `USD` …).
pub const EQ_QUOTE_MAX: usize = 8;

/// **The D5 keys** an equity order is sent with — required, no default
/// (plan §0 D5): the session, `tokenize` (true: the fill settles as the
/// tokenized bStock; false: the underlying equity, in the broker's
/// custody), and the quote asset. The gateway refuses an equity row
/// without them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EqKnobs {
    /// `tradingSession`.
    pub session: EqSession,
    /// `tokenize`.
    pub tokenize: bool,
    /// `quoteAsset` (upper-case letters, at most [`EQ_QUOTE_MAX`]).
    pub quote: String,
}

impl EqKnobs {
    /// The keys, the quote judged (`None`: not 1 to [`EQ_QUOTE_MAX`]
    /// upper-case letters — it is rendered into the order).
    #[must_use]
    pub fn new(session: EqSession, tokenize: bool, quote: &str) -> Option<Self> {
        let ok = !quote.is_empty() && quote.len() <= EQ_QUOTE_MAX && quote.bytes().all(|b| b.is_ascii_uppercase());
        ok.then(|| Self {
            session,
            tokenize,
            quote: String::from(quote),
        })
    }
}

// -------------------------------------------------------------------------
// The answers
// -------------------------------------------------------------------------

/// **The place's acknowledgement** (`POST …/order/place`): `{status:
/// "S"|"F", orderId, clientOrderId}` — `S` is the venue's receipt (the
/// order exists, `NEW`), not a lifecycle status; `F` a refusal.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EqAck {
    /// `status` is `S`.
    pub ok: bool,
    /// `orderId` (a UUID; required on `S`).
    pub id: Span,
    /// `clientOrderId` (our id echoed; required on `S`).
    pub cid: Span,
}

const A_STATUS: u32 = 1 << 0;
const A_ID: u32 = 1 << 1;
const A_CID: u32 = 1 << 2;

/// Read the place's acknowledgement into `out` (module docs). Fail closed:
/// one object read whole, each key once; `status` `S` or `F`; on `S` a
/// UUID-shaped `orderId` and the `clientOrderId`.
pub fn scan_eq_ack(body: &[u8], out: &mut EqAck) -> Result<(), ScanErr> {
    *out = EqAck::default();
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    let mut have = 0u32;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"status" => {
                once(&mut have, A_STATUS)?;
                out.ok = match str_of(body, a, z).map_err(bad)? {
                    b"S" => true,
                    b"F" => false,
                    _ => return Err(ScanErr::BadField),
                };
            }
            b"orderId" => {
                once(&mut have, A_ID)?;
                let s = str_of(body, a, z).map_err(bad)?;
                if !is_uuid(s) {
                    return Err(ScanErr::BadField);
                }
                out.id = Span::of(body, s);
            }
            b"clientOrderId" => {
                once(&mut have, A_CID)?;
                out.cid = Span::of(body, str_of(body, a, z).map_err(bad)?);
            }
            _ => {}
        }
    }
    if !ends_whole(body, w.end()) {
        return Err(ScanErr::Malformed);
    }
    if have & A_STATUS == 0 || (out.ok && have & (A_ID | A_CID) != A_ID | A_CID) {
        return Err(ScanErr::Missing);
    }
    Ok(())
}

/// **The cancel's acknowledgement** (`POST …/order/cancel`): `{orderId,
/// status: "S"|"F"}` → (the id it names, `S`). Read as [`scan_eq_ack`].
pub fn scan_eq_cancel(body: &[u8]) -> Result<(Span, bool), ScanErr> {
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    let (mut have, mut id, mut ok) = (0u32, Span::default(), false);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"status" => {
                once(&mut have, A_STATUS)?;
                ok = match str_of(body, a, z).map_err(bad)? {
                    b"S" => true,
                    b"F" => false,
                    _ => return Err(ScanErr::BadField),
                };
            }
            b"orderId" => {
                once(&mut have, A_ID)?;
                let s = str_of(body, a, z).map_err(bad)?;
                if !is_uuid(s) {
                    return Err(ScanErr::BadField);
                }
                id = Span::of(body, s);
            }
            _ => {}
        }
    }
    if !ends_whole(body, w.end()) {
        return Err(ScanErr::Malformed);
    }
    if have != A_STATUS | A_ID {
        return Err(ScanErr::Missing);
    }
    Ok((id, ok))
}

/// **The cancel-all's answer** (`POST …/order/cancel-all`): `{success}`.
pub fn scan_eq_cancel_all(body: &[u8]) -> Result<bool, ScanErr> {
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    let mut v = None;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        if k == b"success" {
            if v.is_some() {
                return Err(ScanErr::Malformed);
            }
            v = Some(bool_of(body, a, z).map_err(bad)?);
        }
    }
    if !ends_whole(body, w.end()) {
        return Err(ScanErr::Malformed);
    }
    v.ok_or(ScanErr::Missing)
}

/// **One execution** — an `order/detail` trade (`executionId`,
/// `executionAt`, `price`, `qty`) or a `trade/history` row (also `orderId`,
/// `symbol`, `side`). 64 B at most: a row is copied by value.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EqExec {
    /// `price` ×1e6 (positive).
    pub px_1e6: i64,
    /// `qty` ×1e6 (positive; the venue's 9 decimals truncated to 6).
    pub qty_1e6: i64,
    /// `executionAt`, ms.
    pub at_ms: u64,
    /// `executionId`.
    pub exec: Span,
    /// `orderId` (a history row's; empty in a detail).
    pub id: Span,
    /// `symbol` (a history row's).
    pub symbol: Span,
    /// `Side` as its `u8` (a history row's).
    pub side: u8,
    _r: [u8; 7],
}

const _: () = assert!(core::mem::size_of::<EqExec>() <= 64);

const X_ID: u32 = 1 << 0;
const X_AT: u32 = 1 << 1;
const X_PX: u32 = 1 << 2;
const X_QTY: u32 = 1 << 3;
const X_OID: u32 = 1 << 4;
const X_SYM: u32 = 1 << 5;
const X_SIDE: u32 = 1 << 6;
const X_DETAIL: u32 = X_ID | X_AT | X_PX | X_QTY;
const X_HISTORY: u32 = X_DETAIL | X_OID | X_SYM | X_SIDE;

/// One execution object at `at` into `out`; `need` the keys it must name.
fn scan_exec(body: &[u8], at: usize, need: u32, out: &mut EqExec) -> Result<(), ScanErr> {
    *out = EqExec::default();
    let mut w = Pairs::new(body, at).map_err(malformed)?;
    let mut have = 0u32;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"executionId" => {
                once(&mut have, X_ID)?;
                let s = str_of(body, a, z).map_err(bad)?;
                if s.is_empty() {
                    return Err(ScanErr::BadField);
                }
                out.exec = Span::of(body, s);
            }
            b"executionAt" => {
                once(&mut have, X_AT)?;
                out.at_ms = u64_of(body, a, z).map_err(bad)?;
            }
            // A trade at no price, or of nothing, is no trade.
            b"price" => {
                once(&mut have, X_PX)?;
                out.px_1e6 = dec_1e6(body, a, z).map_err(bad)?;
                if out.px_1e6 <= 0 {
                    return Err(ScanErr::BadField);
                }
            }
            b"qty" => {
                once(&mut have, X_QTY)?;
                out.qty_1e6 = dec_1e6(body, a, z).map_err(bad)?;
                if out.qty_1e6 <= 0 {
                    return Err(ScanErr::BadField);
                }
            }
            b"orderId" if need & X_OID != 0 => {
                once(&mut have, X_OID)?;
                let s = str_of(body, a, z).map_err(bad)?;
                if !is_uuid(s) {
                    return Err(ScanErr::BadField);
                }
                out.id = Span::of(body, s);
            }
            b"symbol" if need & X_SYM != 0 => {
                once(&mut have, X_SYM)?;
                out.symbol = Span::of(body, ticker_of(body, a, z)?);
            }
            b"side" if need & X_SIDE != 0 => {
                once(&mut have, X_SIDE)?;
                out.side = eq_side_of(str_of(body, a, z).map_err(bad)?)?;
            }
            _ => {}
        }
    }
    if have & need != need {
        return Err(ScanErr::Missing);
    }
    Ok(())
}

/// **One order's detail** (`GET …/order/detail`): its state, cumulative
/// figures and fee; its executions go to the caller's array.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EqDetail {
    /// `orderId`.
    pub id: Span,
    /// `clientOrderId` (empty when absent).
    pub cid: Span,
    /// `symbol`.
    pub symbol: Span,
    /// `Side` as its `u8`.
    pub side: u8,
    /// `EQ_S_*`.
    pub status: u8,
    /// `filledQty` ×1e6 — cumulative.
    pub filled_1e6: i64,
    /// `filledTotal` ×1e6 — the cumulative filled notional (0: null).
    pub filled_total_1e6: i64,
    /// `avgFilledPrice` ×1e6 (0: null — no fill yet).
    pub avg_1e6: i64,
    /// `fee` ×1e9, USD — cumulative.
    pub fee_1e9: i64,
    /// `updatedAt`, ms (0: absent).
    pub updated_ms: u64,
}

const D_ID: u32 = 1 << 0;
const D_CID: u32 = 1 << 1;
const D_SYM: u32 = 1 << 2;
const D_SIDE: u32 = 1 << 3;
const D_STATUS: u32 = 1 << 4;
const D_FILLED: u32 = 1 << 5;
const D_TOTAL: u32 = 1 << 6;
const D_AVG: u32 = 1 << 7;
const D_FEE: u32 = 1 << 8;
const D_UPD: u32 = 1 << 9;
const D_TRADES: u32 = 1 << 10;
const D_NEED: u32 = D_ID | D_SYM | D_SIDE | D_STATUS | D_FILLED | D_FEE;

/// Read one order's detail into `out` and its executions into `trades`;
/// the executions' count. Fail closed: one object read whole, each key
/// once; the id a UUID; every execution whole (an id, a time, a positive
/// price and quantity), no execution named twice, and their quantities
/// summing to no more than `filledQty` (the venue's 9 decimals each
/// truncated to 6, so never more); no figure negative; more executions
/// than `trades` holds is refused, never cut short. `filledQty` must be
/// named; the venue's nullable figures (`filledQty`, `filledTotal`,
/// `avgFilledPrice`, `fee`) read 0 as `null`. A `FILLED` or
/// `PARTIALLY_FILLED` order with nothing filled is refused (the risk
/// review's B1: an answer short of its fill must never end an order
/// FILLED with nothing booked); `trades` `null` is no execution (the
/// cumulative quantity books at the implied price), absent only while
/// nothing is filled.
pub fn scan_eq_detail(body: &[u8], out: &mut EqDetail, trades: &mut [EqExec]) -> Result<usize, ArrErr> {
    *out = EqDetail::default();
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    let (mut have, mut n, mut sum) = (0u32, 0usize, 0i64);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"orderId" => {
                once(&mut have, D_ID)?;
                let s = str_of(body, a, z).map_err(bad)?;
                if !is_uuid(s) {
                    return Err(ArrErr::Scan(ScanErr::BadField));
                }
                out.id = Span::of(body, s);
            }
            b"clientOrderId" => {
                once(&mut have, D_CID)?;
                if !is_null(body, a, z) {
                    out.cid = Span::of(body, str_of(body, a, z).map_err(bad)?);
                }
            }
            b"symbol" => {
                once(&mut have, D_SYM)?;
                out.symbol = Span::of(body, ticker_of(body, a, z)?);
            }
            b"side" => {
                once(&mut have, D_SIDE)?;
                out.side = eq_side_of(str_of(body, a, z).map_err(bad)?)?;
            }
            b"status" => {
                once(&mut have, D_STATUS)?;
                out.status = eq_status_of(str_of(body, a, z).map_err(bad)?);
            }
            b"filledQty" => {
                once(&mut have, D_FILLED)?;
                out.filled_1e6 = dec_nonneg_or_null(body, a, z)?;
            }
            b"filledTotal" => {
                once(&mut have, D_TOTAL)?;
                out.filled_total_1e6 = dec_nonneg_or_null(body, a, z)?;
            }
            b"avgFilledPrice" => {
                once(&mut have, D_AVG)?;
                out.avg_1e6 = dec_nonneg_or_null(body, a, z)?;
            }
            b"fee" => {
                once(&mut have, D_FEE)?;
                out.fee_1e9 = if is_null(body, a, z) { 0 } else { dec9_nonneg(body, a, z)? };
            }
            b"updatedAt" => {
                once(&mut have, D_UPD)?;
                out.updated_ms = u64_of(body, a, z).map_err(bad)?;
            }
            b"trades" => {
                once(&mut have, D_TRADES)?;
                if is_null(body, a, z) {
                    continue;
                }
                let mut e = Elems::new(body, a).map_err(malformed)?;
                while let Some((t0, _)) = e.next().map_err(malformed)? {
                    if n == trades.len() {
                        return Err(ArrErr::Truncated);
                    }
                    scan_exec(body, t0, X_DETAIL, &mut trades[n])?;
                    sum = sum.saturating_add(trades[n].qty_1e6);
                    n += 1;
                }
            }
            _ => {}
        }
    }
    if !ends_whole(body, w.end()) {
        return Err(ArrErr::Scan(ScanErr::Malformed));
    }
    if have & D_NEED != D_NEED || (out.filled_1e6 > 0 && have & D_TRADES == 0) {
        return Err(ArrErr::Scan(ScanErr::Missing));
    }
    let filled_word = matches!(out.status, EQ_S_FILLED | EQ_S_PARTIAL);
    if sum > out.filled_1e6 || (filled_word && out.filled_1e6 == 0) || named_twice(body, n, false, |i| trades[i].exec) {
        return Err(ArrErr::Scan(ScanErr::BadField));
    }
    Ok(n)
}

/// **One open order** (`GET …/order/open-orders`): the listing names no
/// client id — an order is ours by its UUID alone.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EqOpenRow {
    /// `filledQty` ×1e6.
    pub filled_1e6: i64,
    /// `orderId`.
    pub id: Span,
    /// `symbol`.
    pub symbol: Span,
    /// `Side` as its `u8`.
    pub side: u8,
    /// `EQ_S_*`.
    pub status: u8,
    _r: [u8; 6],
}

const O_ID: u32 = 1 << 0;
const O_SYM: u32 = 1 << 1;
const O_SIDE: u32 = 1 << 2;
const O_STATUS: u32 = 1 << 3;
const O_FILLED: u32 = 1 << 4;
const O_NEED: u32 = O_ID | O_SYM | O_SIDE | O_STATUS;

/// The open orders into `out`; the count. Fail closed, as the other
/// listings: the array whole, each key an order reads once, no order named
/// twice, no fill negative, an order past `out` refusing the answer.
pub fn scan_eq_open(body: &[u8], out: &mut [EqOpenRow]) -> Result<usize, ArrErr> {
    let mut e = Elems::new(body, 0).map_err(malformed)?;
    let mut n = 0;
    while let Some((a, _)) = e.next().map_err(malformed)? {
        if n == out.len() {
            return Err(ArrErr::Truncated);
        }
        let r = &mut out[n];
        *r = EqOpenRow::default();
        let mut w = Pairs::new(body, a).map_err(malformed)?;
        let mut have = 0u32;
        while let Some((k, va, vz)) = w.next().map_err(malformed)? {
            match k {
                b"orderId" => {
                    once(&mut have, O_ID)?;
                    let s = str_of(body, va, vz).map_err(bad)?;
                    if !is_uuid(s) {
                        return Err(ArrErr::Scan(ScanErr::BadField));
                    }
                    r.id = Span::of(body, s);
                }
                b"symbol" => {
                    once(&mut have, O_SYM)?;
                    r.symbol = Span::of(body, ticker_of(body, va, vz)?);
                }
                b"side" => {
                    once(&mut have, O_SIDE)?;
                    r.side = eq_side_of(str_of(body, va, vz).map_err(bad)?)?;
                }
                b"status" => {
                    once(&mut have, O_STATUS)?;
                    r.status = eq_status_of(str_of(body, va, vz).map_err(bad)?);
                }
                b"filledQty" => {
                    once(&mut have, O_FILLED)?;
                    r.filled_1e6 = dec_nonneg_or_null(body, va, vz)?;
                }
                _ => {}
            }
        }
        if have & O_NEED != O_NEED {
            return Err(ArrErr::Scan(ScanErr::Missing));
        }
        n += 1;
    }
    if !ends_whole(body, e.end()) {
        return Err(ArrErr::Scan(ScanErr::Malformed));
    }
    if named_twice(body, n, true, |i| out[i].id) {
        return Err(ArrErr::Scan(ScanErr::BadField));
    }
    Ok(n)
}

/// A history page's envelope: `{page, size, total, rows}`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EqPage {
    /// `total`: the rows the whole window holds.
    pub total: u64,
    /// `page`: which page this is (1-based) — the caller judges it the one
    /// asked for.
    pub page: u64,
    /// `size`: the page's size — the caller judges it the one asked for.
    pub size: u64,
    /// The rows on this page (in the caller's array).
    pub n: usize,
}

const P_TOTAL: u32 = 1 << 0;
const P_ROWS: u32 = 1 << 1;
const P_PAGE: u32 = 1 << 2;
const P_SIZE: u32 = 1 << 3;
const P_NEED: u32 = P_TOTAL | P_ROWS | P_PAGE | P_SIZE;

/// **A page of executions** (`GET …/trade/history`): the envelope and the
/// rows into `out`. Fail closed: one object read whole, its four keys each
/// once, every row whole (an id, an order id, the symbol, the side, a
/// time, a positive price and quantity), no execution named twice on the
/// page, no more rows than its `size` or its window's `total`; more rows
/// than `out` holds is refused.
pub fn scan_eq_trades(body: &[u8], out: &mut [EqExec]) -> Result<EqPage, ArrErr> {
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    let (mut have, mut page) = (0u32, EqPage::default());
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"total" => {
                once(&mut have, P_TOTAL)?;
                page.total = u64_of(body, a, z).map_err(bad)?;
            }
            b"page" => {
                once(&mut have, P_PAGE)?;
                page.page = u64_of(body, a, z).map_err(bad)?;
            }
            b"size" => {
                once(&mut have, P_SIZE)?;
                page.size = u64_of(body, a, z).map_err(bad)?;
            }
            b"rows" => {
                once(&mut have, P_ROWS)?;
                let mut e = Elems::new(body, a).map_err(malformed)?;
                while let Some((r0, _)) = e.next().map_err(malformed)? {
                    if page.n == out.len() {
                        return Err(ArrErr::Truncated);
                    }
                    scan_exec(body, r0, X_HISTORY, &mut out[page.n])?;
                    page.n += 1;
                }
            }
            _ => {}
        }
    }
    if !ends_whole(body, w.end()) {
        return Err(ArrErr::Scan(ScanErr::Malformed));
    }
    if have & P_NEED != P_NEED {
        return Err(ArrErr::Scan(ScanErr::Missing));
    }
    // A page naming more rows than its size or its window holds is no
    // page of it.
    let n = page.n as u64;
    if n > page.total || n > page.size || named_twice(body, page.n, false, |i| out[i].exec) {
        return Err(ArrErr::Scan(ScanErr::BadField));
    }
    Ok(page)
}

// -------------------------------------------------------------------------
// The order stream
// -------------------------------------------------------------------------

/// [`EqReport::kind`]: an `orderReport`.
pub const EQ_EV_ORDER: u8 = 1;
/// Any other event (counted, never booked).
pub const EQ_EV_OTHER: u8 = 2;

/// **One `orderReport`** (module docs) — the trigger for the order's
/// detail.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EqReport {
    /// `EQ_EV_*`.
    pub kind: u8,
    /// `x` is `ORDER_TERMINAL`.
    pub terminal: bool,
    /// `EQ_S_*` (`s`).
    pub status: u8,
    /// `Side` as its `u8` (`S`; 0 when absent).
    pub side: u8,
    /// `fq` ×1e6: the cumulative filled quantity.
    pub filled_1e6: i64,
    /// `FN` ×1e6: the cumulative filled notional (0: absent).
    pub filled_notional_1e6: i64,
    /// `U` (else `E`), ms.
    pub update_ms: u64,
    /// `i`: the order id.
    pub id: Span,
    /// `b` without its `EQ_` prefix: the ticker.
    pub ticker: Span,
}

const R_E: u32 = 1 << 0;
const R_X: u32 = 1 << 1;
const R_I: u32 = 1 << 2;
const R_B: u32 = 1 << 3;
const R_S: u32 = 1 << 4;
const R_FQ: u32 = 1 << 5;
const R_SIDE: u32 = 1 << 6;
const R_FN: u32 = 1 << 7;
const R_U: u32 = 1 << 8;
const R_EVT: u32 = 1 << 9;
const R_P: u32 = 1 << 10;
const R_Q: u32 = 1 << 11;
const R_N: u32 = 1 << 12;
const R_TC: u32 = 1 << 13;
const R_Z: u32 = 1 << 14;
const R_NEED: u32 = R_E | R_X | R_I | R_B | R_S | R_FQ;
/// One value span per key bit: the highest bit indexes the last slot.
const R_KEYS: usize = 15;
// Every key bit within the slots: a key added past them fails the build.
const _: () = {
    let all = R_E | R_X | R_I | R_B | R_S | R_SIDE | R_FQ | R_FN | R_U | R_EVT | R_P | R_Q | R_N | R_TC | R_Z;
    assert!((32 - all.leading_zeros()) as usize <= R_KEYS && R_KEYS <= 32);
};

/// The `EQ_` prefix of the stream's base asset.
const EQ_PREFIX: &[u8] = b"EQ_";

/// **Read one order-stream frame** into `out`. An event other than
/// `orderReport` is [`EQ_EV_OTHER`]: its `e` read, every other key only
/// for the one-reading law (named once), no value judged. An `orderReport`
/// needs `x` (`ORDER_UPDATE` or `ORDER_TERMINAL`), `i` (a UUID), `b`
/// (`EQ_` + a ticker), `s` and `fq` (a JSON number, never negative); its
/// other numbers, when named, must read as numbers or `null` (`FN`, `U`,
/// `E` never negative; `E` or `U` `null` reads 0 — `U` then falls back to
/// `E`). Fail closed: the frame one object read whole, each key once.
pub fn scan_order_report(frame: &[u8], out: &mut EqReport) -> Result<(), ScanErr> {
    *out = EqReport::default();
    // Each key's value span (`at[bit]`), read whole once `e` is known.
    let mut at = [(0usize, 0usize); R_KEYS];
    let mut w = Pairs::new(frame, 0).map_err(malformed)?;
    let mut have = 0u32;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let bit = match k {
            b"e" => R_E,
            b"E" => R_EVT,
            b"x" => R_X,
            b"i" => R_I,
            b"b" => R_B,
            b"s" => R_S,
            b"S" => R_SIDE,
            b"fq" => R_FQ,
            b"FN" => R_FN,
            b"U" => R_U,
            b"p" => R_P,
            b"N" => R_N,
            b"Q" => R_Q,
            b"tc" => R_TC,
            b"Z" => R_Z,
            _ => continue,
        };
        once(&mut have, bit)?;
        at[bit.trailing_zeros() as usize] = (a, z);
    }
    if !ends_whole(frame, w.end()) {
        return Err(ScanErr::Malformed);
    }
    if have & R_E == 0 {
        return Err(ScanErr::NoEvent);
    }
    let v = |bit: u32| at[bit.trailing_zeros() as usize];
    let (a, z) = v(R_E);
    if str_of(frame, a, z).map_err(bad)? != b"orderReport" {
        out.kind = EQ_EV_OTHER;
        return Ok(());
    }
    if have & R_NEED != R_NEED {
        return Err(ScanErr::Missing);
    }
    let (a, z) = v(R_X);
    let x = str_of(frame, a, z).map_err(bad)?;
    out.terminal = if x == b"ORDER_TERMINAL" {
        true
    } else if x == b"ORDER_UPDATE" {
        false
    } else {
        return Err(ScanErr::BadField);
    };
    let (a, z) = v(R_I);
    let s = str_of(frame, a, z).map_err(bad)?;
    if !is_uuid(s) {
        return Err(ScanErr::BadField);
    }
    out.id = Span::of(frame, s);
    let (a, z) = v(R_B);
    let s = str_of(frame, a, z).map_err(bad)?;
    if s.len() <= EQ_PREFIX.len() || !s.starts_with(EQ_PREFIX) {
        return Err(ScanErr::BadField);
    }
    out.ticker = Span::of(frame, &s[EQ_PREFIX.len()..]);
    let (a, z) = v(R_S);
    out.status = eq_status_of(str_of(frame, a, z).map_err(bad)?);
    let (a, z) = v(R_FQ);
    out.filled_1e6 = json_num_nonneg(frame, a, z)?;
    if have & R_SIDE != 0 {
        let (a, z) = v(R_SIDE);
        out.side = eq_side_of(str_of(frame, a, z).map_err(bad)?)?;
    }
    if have & R_FN != 0 {
        let (a, z) = v(R_FN);
        out.filled_notional_1e6 = if is_null(frame, a, z) { 0 } else { json_num_nonneg(frame, a, z)? };
    }
    let mut event_ms = 0u64;
    if have & R_EVT != 0 {
        let (a, z) = v(R_EVT);
        event_ms = if is_null(frame, a, z) { 0 } else { u64_of(frame, a, z).map_err(bad)? };
    }
    if have & R_U != 0 {
        let (a, z) = v(R_U);
        out.update_ms = if is_null(frame, a, z) { 0 } else { u64_of(frame, a, z).map_err(bad)? };
    }
    // Read for their shape alone (the detail books the figures).
    let mut i = 0;
    let shapes = [R_P, R_N, R_Q, R_TC, R_Z];
    while i < shapes.len() {
        if have & shapes[i] != 0 {
            let (a, z) = v(shapes[i]);
            if !is_null(frame, a, z) {
                json_num_1e6(frame, a, z).map_err(bad)?;
            }
        }
        i += 1;
    }
    out.kind = EQ_EV_ORDER;
    if out.update_ms == 0 {
        out.update_ms = event_ms;
    }
    Ok(())
}

// -------------------------------------------------------------------------
// The symbol's rules (cold: the boot)
// -------------------------------------------------------------------------

/// [`EqSymbolRow::tradability`]: the venue takes BUYs.
pub const EQ_SIDE_BUY: u8 = 1 << 0;
/// The venue takes SELLs.
pub const EQ_SIDE_SELL: u8 = 1 << 1;

/// The wire symbol's capacity.
pub const EQ_SYMBOL_MAX: usize = 16;

/// **One ticker's rules** (`GET …/market/exchangeInfo`, the probe's EQ1
/// shape) — what the arm's row binds from. Prices carry no tick: the venue
/// takes at most two decimals. ×1e9 throughout (the step is a billionth).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EqSymbolRow {
    symbol: [u8; EQ_SYMBOL_MAX],
    len: u8,
    /// `tradability`: `EQ_SIDE_*` (`BUY_SELL` both, `NONE` neither).
    pub tradability: u8,
    /// `fractionable`.
    pub fractionable: bool,
    /// `stepSize` ×1e9.
    pub step_1e9: i64,
    /// `minQty` ×1e9 (0: not named — the probe found none).
    pub min_qty_1e9: i64,
    /// `maxQty` ×1e9 (0: none).
    pub max_qty_1e9: i64,
    /// `minNotional` ×1e9.
    pub min_notional_1e9: i64,
    /// `maxNotional` ×1e9 (0: none).
    pub max_notional_1e9: i64,
    /// `multiplierUp` / `multiplierDown` ×1e9 (0: none).
    pub up_1e9: i64,
    /// `multiplierDown` ×1e9.
    pub down_1e9: i64,
    /// `maxNumOrders` (0: none).
    pub max_orders: u32,
}

impl EqSymbolRow {
    /// No rules read.
    const EMPTY: Self = Self {
        symbol: [0; EQ_SYMBOL_MAX],
        len: 0,
        tradability: 0,
        fractionable: false,
        step_1e9: 0,
        min_qty_1e9: 0,
        max_qty_1e9: 0,
        min_notional_1e9: 0,
        max_notional_1e9: 0,
        up_1e9: 0,
        down_1e9: 0,
        max_orders: 0,
    };

    /// The ticker.
    #[must_use]
    pub fn symbol(&self) -> &[u8] {
        &self.symbol[..self.len as usize]
    }
}

const Y_SYM: u32 = 1 << 0;
const Y_TRAD: u32 = 1 << 1;
const Y_FRAC: u32 = 1 << 2;
const Y_STEP: u32 = 1 << 3;
const Y_MINN: u32 = 1 << 4;
const Y_MAXQ: u32 = 1 << 5;
const Y_MAXN: u32 = 1 << 6;
const Y_UP: u32 = 1 << 7;
const Y_DOWN: u32 = 1 << 8;
const Y_ORD: u32 = 1 << 9;
const Y_MINQ: u32 = 1 << 10;
const Y_NEED: u32 = Y_SYM | Y_TRAD | Y_FRAC | Y_STEP | Y_MINN;

/// **`symbol`'s rules** from an `exchangeInfo` answer (cold: the boot's
/// discovery). Fail closed: the answer one object read whole, `symbols`
/// once; every row read whole (each key it reads once, `tradability` a
/// word this build knows, the step positive, no figure negative); the row
/// `symbol`'s named once in the list — only its ticker is copied.
pub fn scan_eq_symbol(body: &[u8], symbol: &[u8]) -> Result<EqSymbolRow, ScanErr> {
    if symbol.is_empty() || symbol.len() > EQ_SYMBOL_MAX {
        return Err(ScanErr::BadField);
    }
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    let (mut listed, mut found) = (false, false);
    let mut out = EqSymbolRow::EMPTY;
    while let Some((k, a, _)) = w.next().map_err(malformed)? {
        if k != b"symbols" {
            continue;
        }
        if listed {
            return Err(ScanErr::Malformed);
        }
        listed = true;
        let mut e = Elems::new(body, a).map_err(malformed)?;
        while let Some((r0, _)) = e.next().map_err(malformed)? {
            if scan_symbol_row(body, r0, symbol, &mut out)? {
                if found {
                    return Err(ScanErr::Malformed);
                }
                found = true;
            }
        }
    }
    if !ends_whole(body, w.end()) {
        return Err(ScanErr::Malformed);
    }
    if !listed || !found {
        return Err(ScanErr::Missing);
    }
    Ok(out)
}

/// One row at `at`, read whole: `true` — it is `want`'s, and `out` holds
/// it (a row of another ticker is judged and nothing of it kept).
fn scan_symbol_row(body: &[u8], at: usize, want: &[u8], out: &mut EqSymbolRow) -> Result<bool, ScanErr> {
    let mut r = EqSymbolRow::EMPTY;
    let mut w = Pairs::new(body, at).map_err(malformed)?;
    let (mut have, mut sym) = (0u32, Span::default());
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"symbol" => {
                once(&mut have, Y_SYM)?;
                let s = str_of(body, a, z).map_err(bad)?;
                if s.is_empty() || s.len() > EQ_SYMBOL_MAX || !s.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'.') {
                    return Err(ScanErr::BadField);
                }
                sym = Span::of(body, s);
            }
            b"tradability" => {
                once(&mut have, Y_TRAD)?;
                r.tradability = match str_of(body, a, z).map_err(bad)? {
                    b"BUY_SELL" => EQ_SIDE_BUY | EQ_SIDE_SELL,
                    b"BUY" => EQ_SIDE_BUY,
                    b"SELL" => EQ_SIDE_SELL,
                    b"NONE" => 0,
                    _ => return Err(ScanErr::BadField),
                };
            }
            b"fractionable" => {
                once(&mut have, Y_FRAC)?;
                r.fractionable = bool_of(body, a, z).map_err(bad)?;
            }
            b"stepSize" => {
                once(&mut have, Y_STEP)?;
                r.step_1e9 = dec9_nonneg(body, a, z)?;
                if r.step_1e9 == 0 {
                    return Err(ScanErr::BadField);
                }
            }
            b"minQty" => {
                once(&mut have, Y_MINQ)?;
                r.min_qty_1e9 = dec9_nonneg(body, a, z)?;
            }
            b"maxQty" => {
                once(&mut have, Y_MAXQ)?;
                r.max_qty_1e9 = dec9_nonneg(body, a, z)?;
            }
            b"minNotional" => {
                once(&mut have, Y_MINN)?;
                r.min_notional_1e9 = dec9_nonneg(body, a, z)?;
            }
            b"maxNotional" => {
                once(&mut have, Y_MAXN)?;
                r.max_notional_1e9 = dec9_nonneg(body, a, z)?;
            }
            b"multiplierUp" => {
                once(&mut have, Y_UP)?;
                r.up_1e9 = dec9_nonneg(body, a, z)?;
            }
            b"multiplierDown" => {
                once(&mut have, Y_DOWN)?;
                r.down_1e9 = dec9_nonneg(body, a, z)?;
            }
            b"maxNumOrders" => {
                once(&mut have, Y_ORD)?;
                r.max_orders = u32::try_from(u64_of(body, a, z).map_err(bad)?).map_err(|_| ScanErr::BadField)?;
            }
            _ => {}
        }
    }
    if have & Y_NEED != Y_NEED {
        return Err(ScanErr::Missing);
    }
    let s = sym.get(body);
    if s != want {
        return Ok(false);
    }
    // COPY: the asked ticker (≤ 16 B) into its row, and the row (80 B)
    // into the caller's (returned by value once more) — cold, once a call
    // at boot; the answer is dropped after the discovery — rejected: a span
    // into it (outlived), re-scanning into the caller's row (a second walk
    // of the answer a call, to save 80 B once at boot).
    r.symbol[..s.len()].copy_from_slice(s);
    r.len = s.len() as u8;
    *out = r;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "8b1f3a52-7c0e-4d55-9a8e-1f2b3c4d5e6f";
    const ID2: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

    /// The id's shape and key. Break-and-watch: accepting a hyphen
    /// anywhere fails the third assertion.
    #[test]
    fn an_order_id_is_a_uuid_held_whole() {
        assert!(is_uuid(ID.as_bytes()));
        assert!(!is_uuid(&ID.as_bytes()[..35]), "short");
        assert!(!is_uuid(b"8b1f3a52-7c0e-4d55-9a8e-1f2b3c4d5e-f"), "a hyphen out of place");
        assert!(!is_uuid(b"8b1f3a52-7c0e-4d55-9a8e-1f2b3c4d5e6g"), "not hex");
        let id = EqOrderId::of(ID.as_bytes()).expect("a UUID");
        assert_eq!(id.get(), ID.as_bytes());
        assert!(id.is_some() && !EqOrderId::NONE.is_some());
        assert_eq!(EqOrderId::of(b"1234"), None);
        assert_ne!(eq_key(ID.as_bytes()), eq_key(ID2.as_bytes()));
        assert_eq!(eq_key(b"") & 1, 1, "never 0");
    }

    /// Numbers from their text, exactly. Break-and-watch: reading through
    /// `f64` makes `0.07` 69 999 (or 70 000 by luck) and loses the
    /// 17-digit case.
    #[test]
    fn a_json_number_reads_exactly() {
        let n = |s: &str| json_num_1e6(s.as_bytes(), 0, s.len());
        assert_eq!(n("336.09"), Ok(336_090_000));
        assert_eq!(n("0.07"), Ok(70_000));
        assert_eq!(n("0.0000019"), Ok(1), "past six decimals: truncated");
        assert_eq!(n("-1.5"), Ok(-1_500_000));
        assert_eq!(n("1e-05"), Ok(10), "the exponent form");
        assert_eq!(n("1.5E2"), Ok(150_000_000));
        assert_eq!(n("2E+3"), Ok(2_000_000_000));
        assert_eq!(n("\"12.5\""), Ok(12_500_000), "quoted");
        assert_eq!(n("12345678901.123456"), Ok(12_345_678_901_123_456));
        assert_eq!(n("9200000000000"), Ok(9_200_000_000_000_000_000));
        assert_eq!(n("9300000000000"), Err(()), "past i64 at ×1e6");
        for bad in ["", "-", ".5", "1.", "1e", "1e+", "0x10", "1.2.3", "1 ", "null", "1e31", "NaN"] {
            assert_eq!(n(bad), Err(()), "{bad}");
        }
        // Past 30 significant digits, refused before `i128` could wrap (a
        // 40-digit integer once overflowed it before the count was read).
        let long = format!("1{}", "0".repeat(39));
        assert_eq!(n(&long), Err(()));
        assert_eq!(n(&format!("{}1", "0".repeat(60))), Ok(1_000_000), "leading zeros are no digits");
    }

    #[test]
    fn the_place_and_cancel_acknowledgements_read_whole() {
        let ok = format!(r#"{{"status":"S","orderId":"{ID}","clientOrderId":"mv6512ab0f20000000000000000c900000"}}"#);
        let mut a = EqAck::default();
        assert_eq!(scan_eq_ack(ok.as_bytes(), &mut a), Ok(()));
        assert!(a.ok);
        assert_eq!(a.id.get(ok.as_bytes()), ID.as_bytes());
        assert_eq!(a.cid.get(ok.as_bytes()), b"mv6512ab0f20000000000000000c900000");
        assert_eq!(scan_eq_ack(br#"{"status":"F"}"#, &mut a), Ok(()));
        assert!(!a.ok, "a refusal names no order");
        assert_eq!(scan_eq_ack(br#"{"status":"S","clientOrderId":"x"}"#, &mut a), Err(ScanErr::Missing), "S names its order");
        assert_eq!(scan_eq_ack(br#"{"status":"X","orderId":"y"}"#, &mut a), Err(ScanErr::BadField));
        assert_eq!(scan_eq_ack(br#"{"status":"S","orderId":"1234","clientOrderId":"x"}"#, &mut a), Err(ScanErr::BadField), "not a UUID");
        assert_eq!(scan_eq_ack(br#"{"status":"S","status":"F"}"#, &mut a), Err(ScanErr::Malformed), "named twice");
        assert_eq!(scan_eq_ack(format!("{ok} x").as_bytes(), &mut a), Err(ScanErr::Malformed), "bytes after");
        let c = format!(r#"{{"orderId":"{ID}","status":"S"}}"#);
        let (id, s) = scan_eq_cancel(c.as_bytes()).expect("cancel");
        assert!(s && id.get(c.as_bytes()) == ID.as_bytes());
        assert_eq!(scan_eq_cancel(br#"{"status":"S"}"#), Err(ScanErr::Missing));
        assert_eq!(scan_eq_cancel_all(br#"{"success":true}"#), Ok(true));
        assert_eq!(scan_eq_cancel_all(br#"{"success":true,"success":false}"#), Err(ScanErr::Malformed));
    }

    fn detail(status: &str, filled: &str, trades: &str) -> String {
        format!(
            r#"{{"orderId":"{ID}","clientOrderId":"mvc","symbol":"AAPL","quote":"USDC","side":"BUY","orderType":"LIMIT","limitPrice":"336.10","avgFilledPrice":null,"qty":"0.03","notional":null,"filledQty":"{filled}","filledTotal":null,"fee":"0.35","session":"RTH","status":"{status}","createdAt":1791700000000,"updatedAt":1791700001000,"trades":[{trades}]}}"#
        )
    }

    /// A detail and its executions. Break-and-watch: dropping the sum
    /// check lets the over-filled detail through.
    #[test]
    fn a_detail_reads_its_executions_and_its_cumulative_figures() {
        let t = r#"{"executionId":"e2","executionAt":1791700001000,"price":"336.08","qty":"0.010000000"},{"executionId":"e1","executionAt":1791700000500,"price":"336.09","qty":"0.02"}"#;
        let b = detail("FILLED", "0.030000000", t);
        let (mut d, mut x) = (EqDetail::default(), [EqExec::default(); 4]);
        assert_eq!(scan_eq_detail(b.as_bytes(), &mut d, &mut x), Ok(2));
        assert_eq!((d.status, d.side, d.filled_1e6, d.fee_1e9), (EQ_S_FILLED, Side::Bid as u8, 30_000, 350_000_000));
        assert_eq!((d.avg_1e6, d.filled_total_1e6), (0, 0), "nulls read as none");
        assert_eq!(d.cid.get(b.as_bytes()), b"mvc");
        assert_eq!((x[0].exec.get(b.as_bytes()), x[0].px_1e6, x[0].qty_1e6), (&b"e2"[..], 336_080_000, 10_000));
        assert_eq!(x[1].at_ms, 1_791_700_000_500);
        let over = detail("FILLED", "0.01", t);
        assert_eq!(scan_eq_detail(over.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "executions past the cumulative");
        assert_eq!(scan_eq_detail(b.as_bytes(), &mut d, &mut x[..1]), Err(ArrErr::Truncated));
        let none = detail("ACCEPTED", "0", "");
        assert_eq!(scan_eq_detail(none.as_bytes(), &mut d, &mut x), Ok(0));
        assert_eq!(d.status, EQ_S_ACCEPTED);
        // An order not yet filled: `trades` absent or null, `filledQty`
        // null — no execution, nothing filled.
        let no_trades = none.replace(r#","trades":[]"#, "");
        assert_eq!(scan_eq_detail(no_trades.as_bytes(), &mut d, &mut x), Ok(0));
        let null_trades = none.replace(r#""trades":[]"#, r#""trades":null"#).replace(r#""filledQty":"0""#, r#""filledQty":null"#);
        assert_eq!(scan_eq_detail(null_trades.as_bytes(), &mut d, &mut x), Ok(0));
        assert_eq!(d.filled_1e6, 0);
        let no_fee = none.replace(r#""fee":"0.35","#, "");
        assert_eq!(scan_eq_detail(no_fee.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::Missing)));
        // The risk review's B1: `filledQty` must be named, and a filled
        // word with nothing filled is no answer — never an end with nothing
        // booked. Break-and-watch: each case below passed before.
        let no_filled = none.replace(r#""filledQty":"0","#, "");
        assert_ne!(no_filled, none);
        assert_eq!(scan_eq_detail(no_filled.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::Missing)), "filledQty absent");
        let filled_nothing = detail("FILLED", "0", "");
        assert_eq!(scan_eq_detail(filled_nothing.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "FILLED, nothing filled");
        let filled_null = filled_nothing.replace(r#""filledQty":"0""#, r#""filledQty":null"#).replace(r#""trades":[]"#, r#""trades":null"#);
        assert_eq!(scan_eq_detail(filled_null.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "FILLED, filledQty null");
        let partial_nothing = detail("PARTIALLY_FILLED", "0", "");
        assert_eq!(scan_eq_detail(partial_nothing.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "PARTIAL, nothing filled");
        let filled_no_trades = detail("FILLED", "0.01", "").replace(r#","trades":[]"#, "");
        assert_eq!(scan_eq_detail(filled_no_trades.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::Missing)), "a fill with no trades key");
        let filled_null_trades = detail("FILLED", "0.01", "").replace(r#""trades":[]"#, r#""trades":null"#);
        assert_eq!(scan_eq_detail(filled_null_trades.as_bytes(), &mut d, &mut x), Ok(0), "trades null: the implied price books it");
        // The parser review's F1: a negative is refused on its sign —
        // `-0` and a negative below the scale included.
        for neg in [r#""filledQty":"-0""#, r#""filledQty":"-0.0000009""#] {
            let b2 = none.replace(r#""filledQty":"0""#, neg);
            assert_eq!(scan_eq_detail(b2.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "{neg}");
        }
        let mut o = [EqOpenRow::default(); 2];
        let listed = format!(r#"[{{"orderId":"{ID}","symbol":"AAPL","side":"BUY","status":"NEW","filledQty":"-0"}}]"#);
        assert_eq!(scan_eq_open(listed.as_bytes(), &mut o), Err(ArrErr::Scan(ScanErr::BadField)), "a listed fill of -0");
        let mut r = EqReport::default();
        let fn0 = report("0", "new", "ORDER_UPDATE").replace(r#""FN":10.0861"#, r#""FN":-0"#);
        assert_eq!(scan_order_report(fn0.as_bytes(), &mut r), Err(ScanErr::BadField), "a report's notional of -0");
        let neg_fee = none.replace(r#""fee":"0.35""#, r#""fee":"-0.0000000001""#);
        assert_eq!(scan_eq_detail(neg_fee.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "a fee below the scale, negative");
        let zero_px = detail("FILLED", "0.01", r#"{"executionId":"e","executionAt":1,"price":"0","qty":"0.01"}"#);
        assert_eq!(scan_eq_detail(zero_px.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)));
        let twice = b.replacen(r#""fee":"0.35","#, r#""fee":"0.35","fee":"0","#, 1);
        assert_eq!(scan_eq_detail(twice.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::Malformed)));
    }

    #[test]
    fn the_open_orders_and_a_history_page_read_whole() {
        let o = format!(
            r#"[{{"orderId":"{ID}","symbol":"AAPL","quote":"USDC","side":"BUY","orderType":"LIMIT","limitPrice":"1.00","avgFilledPrice":null,"qty":"5","notional":null,"filledQty":"0","filledTotal":null,"fee":"0","session":"RTH","status":"ACCEPTED","createdAt":1,"updatedAt":2}}]"#
        );
        let mut rows = [EqOpenRow::default(); 2];
        assert_eq!(scan_eq_open(o.as_bytes(), &mut rows), Ok(1));
        assert_eq!((rows[0].id.get(o.as_bytes()), rows[0].status), (ID.as_bytes(), EQ_S_ACCEPTED));
        assert_eq!(scan_eq_open(b"[]", &mut rows), Ok(0));
        assert_eq!(scan_eq_open(b"[] 1", &mut rows), Err(ArrErr::Scan(ScanErr::Malformed)));
        let p = format!(
            r#"{{"page":1,"size":100,"total":1,"rows":[{{"executionId":"x1","orderId":"{ID}","symbol":"AAPL","quote":"USDC","side":"SELL","orderType":"LIMIT","price":"336.1","qty":"0.5","total":"168.05","executionAt":1791700000000,"updatedAt":1791700000000}}]}}"#
        );
        let mut x = [EqExec::default(); 2];
        let page = scan_eq_trades(p.as_bytes(), &mut x).expect("page");
        assert_eq!((page.total, page.n), (1, 1));
        assert_eq!((x[0].side, x[0].qty_1e6, x[0].symbol.get(p.as_bytes())), (Side::Ask as u8, 500_000, &b"AAPL"[..]));
        assert_eq!(scan_eq_trades(br#"{"page":1,"size":100,"total":0,"rows":[]}"#, &mut x), Ok(EqPage { total: 0, page: 1, size: 100, n: 0 }));
        assert_eq!(scan_eq_trades(br#"{"size":100,"total":0,"rows":[]}"#, &mut x), Err(ArrErr::Scan(ScanErr::Missing)), "the page named");
        let small = p.replace(r#""size":100"#, r#""size":0"#);
        assert_eq!(scan_eq_trades(small.as_bytes(), &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "more rows than its size");
        assert_eq!(scan_eq_trades(br#"{"page":1,"size":100,"rows":[]}"#, &mut x), Err(ArrErr::Scan(ScanErr::Missing)));
        let more = p.replace(r#""total":1"#, r#""total":0"#);
        assert_eq!(scan_eq_trades(more.as_bytes(), &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "more rows than the window");
    }

    fn report(fq: &str, s: &str, x: &str) -> String {
        format!(
            r#"{{"e":"orderReport","E":1791700001000,"x":"{x}","i":"{ID}","ai":"a1","b":"EQ_AAPL","q":"USD","S":"buy","o":"limit","p":336.1,"Q":0.03,"N":null,"fq":{fq},"FN":10.0861,"tc":10.4361,"Z":100,"n":"Regular","s":"{s}","T":1791700000000,"U":1791700001000}}"#
        )
    }

    /// The stream's report. Break-and-watch: reading `fq` through `f64`
    /// (0.03 → 29 999) fails the second assertion.
    #[test]
    fn an_order_report_is_the_trigger() {
        let f = report("0.03", "filled", "ORDER_TERMINAL");
        let mut r = EqReport::default();
        assert_eq!(scan_order_report(f.as_bytes(), &mut r), Ok(()));
        assert_eq!((r.kind, r.terminal, r.status, r.filled_1e6), (EQ_EV_ORDER, true, EQ_S_FILLED, 30_000));
        assert_eq!((r.side, r.update_ms, r.filled_notional_1e6), (Side::Bid as u8, 1_791_700_001_000, 10_086_100));
        assert_eq!((r.id.get(f.as_bytes()), r.ticker.get(f.as_bytes())), (ID.as_bytes(), &b"AAPL"[..]));
        let u = report("0", "accepted", "ORDER_UPDATE");
        assert_eq!(scan_order_report(u.as_bytes(), &mut r), Ok(()));
        assert_eq!((r.terminal, r.status), (false, EQ_S_ACCEPTED));
        assert_eq!(scan_order_report(br#"{"e":"heartbeat","E":1}"#, &mut r), Ok(()));
        assert_eq!(r.kind, EQ_EV_OTHER);
        assert_eq!(scan_order_report(br#"{"E":1}"#, &mut r), Err(ScanErr::NoEvent));
        assert_eq!(scan_order_report(report("0", "new", "TRADE").as_bytes(), &mut r), Err(ScanErr::BadField));
        assert_eq!(scan_order_report(report("\"x\"", "new", "ORDER_UPDATE").as_bytes(), &mut r), Err(ScanErr::BadField));
        let bare = report("0", "new", "ORDER_UPDATE").replace("EQ_AAPL", "AAPL");
        assert_eq!(scan_order_report(bare.as_bytes(), &mut r), Err(ScanErr::BadField), "the base names its EQ_ prefix");
        let short = report("0", "new", "ORDER_UPDATE").replace(r#""fq":0,"#, "");
        assert_eq!(scan_order_report(short.as_bytes(), &mut r), Err(ScanErr::Missing));
        assert_eq!(eq_status_of(b"PARTIALLY_FILLED"), EQ_S_PARTIAL);
        assert_eq!(eq_status_of(b"cancelled"), EQ_S_CANCELED);
        assert_eq!(eq_status_of(b"pending"), EQ_S_OTHER);
        assert!(eq_terminal(EQ_S_EXPIRED) && !eq_terminal(EQ_S_ACCEPTED));
    }

    /// The probe's EQ1 row, by value (2026-10-11). Break-and-watch:
    /// accepting a second `symbols` fails the last assertion.
    #[test]
    fn a_tickers_rules_read_from_exchange_info() {
        let b = br#"{"timezone":"UTC","symbols":[{"symbol":"AAPL","tradability":"BUY_SELL","tradabilityUpdateTime":1778233101000,"overnightSupported":true,"fractionable":true,"fractionableEh":true,"extendedSession":true,"maxNumOrders":200,"stepSize":"0.000000001","multiplierUp":"1.1000","multiplierDown":"0.9000","maxQty":"1000000.000000000","minNotional":"5.00000000","maxNotional":"1000000.00000000","listingTime":1778000000000},{"symbol":"SPY","tradability":"SELL","fractionable":false,"stepSize":"1","minNotional":"5"}]}"#;
        let r = scan_eq_symbol(b, b"AAPL").expect("AAPL");
        assert_eq!((r.symbol(), r.tradability, r.fractionable), (&b"AAPL"[..], EQ_SIDE_BUY | EQ_SIDE_SELL, true));
        assert_eq!((r.step_1e9, r.min_notional_1e9, r.max_qty_1e9, r.max_orders), (1, 5_000_000_000, 1_000_000_000_000_000, 200));
        assert_eq!((r.up_1e9, r.down_1e9, r.min_qty_1e9), (1_100_000_000, 900_000_000, 0));
        let s = scan_eq_symbol(b, b"SPY").expect("SPY");
        assert_eq!((s.tradability, s.fractionable, s.step_1e9), (EQ_SIDE_SELL, false, 1_000_000_000));
        assert_eq!(scan_eq_symbol(b, b"MSFT"), Err(ScanErr::Missing));
        assert_eq!(scan_eq_symbol(br#"{"symbols":[{"symbol":"A","tradability":"MAYBE","fractionable":true,"stepSize":"1","minNotional":"5"}]}"#, b"A"), Err(ScanErr::BadField));
        assert_eq!(scan_eq_symbol(br#"{"symbols":[],"symbols":[]}"#, b"A"), Err(ScanErr::Malformed));
    }

    /// **Every refusal its own case** (the parser review: each line fails
    /// if its rule is weakened — the mutation named beside it survived the
    /// suite before).
    #[test]
    fn every_refusal_is_its_own_case() {
        let up = ID.to_ascii_uppercase();
        let (mut d, mut x, mut r, mut o) = (EqDetail::default(), [EqExec::default(); 4], EqReport::default(), [EqOpenRow::default(); 4]);
        let ex = |id: &str, qty: &str| format!(r#"{{"executionId":"{id}","executionAt":1,"price":"336.1","qty":"{qty}"}}"#);
        // The executions: a quantity of 0, an empty id, a key named twice,
        // one execution named twice ("qty <= 0" → "< 0"; the non-empty id;
        // `once` on an execution key).
        let det = |t: &str| detail("FILLED", "0.5", t);
        for bad in [ex("e1", "0"), ex("", "0.01"), ex("e1", "0.01").replace(r#""qty":"#, r#""qty":"0.01","qty":"#)] {
            assert!(scan_eq_detail(det(&bad).as_bytes(), &mut d, &mut x).is_err(), "{bad}");
        }
        let both = format!("{},{}", ex("e1", "0.01"), ex("e1", "0.01"));
        assert_eq!(scan_eq_detail(det(&both).as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "one execution twice");
        let cased = format!("{},{}", ex("e1", "0.01"), ex("E1", "0.01"));
        assert_eq!(scan_eq_detail(det(&cased).as_bytes(), &mut d, &mut x), Ok(2), "an execution id's case is its own");
        assert_ne!(exec_key(b"e1"), exec_key(b"E1"));
        // The detail's figures: none negative (`filledQty`, the fee — a
        // billionth below 0 — the total, the average).
        for (k, v) in [("filledQty", "-0.5"), ("fee", "-0.000000001"), ("filledTotal", "-7"), ("avgFilledPrice", "-5")] {
            let b = det("").replace(&format!(r#""{k}":"#), &format!(r#""{k}":"{v}","x{k}":"#));
            assert_eq!(scan_eq_detail(b.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "{k} {v}");
        }
        // The report: its id a UUID, its fill never negative (a tiny one
        // reads 0 at ×1e6), its notional never negative, its base a ticker.
        for f in [
            report("0", "new", "ORDER_UPDATE").replace(ID, "not-a-uuid"),
            report("-0.0000009", "new", "ORDER_UPDATE"),
            report("-1", "new", "ORDER_UPDATE"),
            report("0", "new", "ORDER_UPDATE").replace(r#""FN":10.0861"#, r#""FN":-5"#),
            report("0", "new", "ORDER_UPDATE").replace("EQ_AAPL", "EQ_"),
        ] {
            assert_eq!(scan_order_report(f.as_bytes(), &mut r), Err(ScanErr::BadField), "{f}");
        }
        // Another event: its values not judged; a null figure: no fill yet.
        assert_eq!(scan_order_report(br#"{"e":"balanceUpdate","E":1,"b":"USDC","S":"x"}"#, &mut r), Ok(()));
        assert_eq!(r.kind, EQ_EV_OTHER);
        assert_eq!(scan_order_report(br#"{"e":"balanceUpdate","b":"USDC","b":"X"}"#, &mut r), Err(ScanErr::Malformed), "named twice, whatever the event");
        let nulls = report("0", "accepted", "ORDER_UPDATE").replace(r#""FN":10.0861"#, r#""FN":null"#).replace(r#""Q":0.03"#, r#""Q":null"#);
        assert_eq!(scan_order_report(nulls.as_bytes(), &mut r), Ok(()));
        assert_eq!(r.filled_notional_1e6, 0);
        // The listing: its id a UUID, no fill negative, no order twice.
        let row = |id: &str, filled: &str| format!(r#"{{"orderId":"{id}","symbol":"AAPL","side":"BUY","status":"NEW","filledQty":"{filled}"}}"#);
        for b in [format!("[{}]", row("1234", "0")), format!("[{}]", row(ID, "-1.5")), format!("[{},{}]", row(ID, "0"), row(&up, "0"))] {
            assert_eq!(scan_eq_open(b.as_bytes(), &mut o), Err(ArrErr::Scan(ScanErr::BadField)), "{b}");
        }
        // A page: its rows' order ids UUIDs, no execution twice.
        let hrow = |oid: &str, xid: &str| format!(r#"{{"executionId":"{xid}","orderId":"{oid}","symbol":"AAPL","side":"BUY","price":"1","qty":"1","executionAt":1}}"#);
        for rows in [hrow("1234", "x1"), format!("{},{}", hrow(ID, "x1"), hrow(ID, "x1"))] {
            let b = format!(r#"{{"page":1,"size":100,"total":2,"rows":[{rows}]}}"#);
            assert_eq!(scan_eq_trades(b.as_bytes(), &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "{b}");
        }
        // The cancel's answer: its id once.
        let c = format!(r#"{{"orderId":"{ID}","status":"S","orderId":"{ID}"}}"#);
        assert_eq!(scan_eq_cancel(c.as_bytes()), Err(ScanErr::Malformed));
        // The number's bounds: an exponent past ±30, a 31st significant
        // digit — each refused even where the value would fit.
        let n = |t: &str| json_num_1e6(t.as_bytes(), 0, t.len());
        assert_eq!((n("1e-30"), n("1e-31")), (Ok(0), Err(())));
        let d30 = "0.123456789012345678901234567890";
        assert_eq!((n(d30), n(&format!("{d30}1"))), (Ok(123_456), Err(())));
        // The rules: the answer whole, the step positive, no figure
        // negative (a negative ceiling is no "none").
        let rules = br#"{"symbols":[{"symbol":"A","tradability":"BUY_SELL","fractionable":true,"stepSize":"1","minNotional":"5"}]}"#;
        assert!(scan_eq_symbol(rules, b"A").is_ok());
        let mut trailed = rules.to_vec();
        trailed.push(b'x');
        assert_eq!(scan_eq_symbol(&trailed, b"A"), Err(ScanErr::Malformed));
        let rules = String::from_utf8_lossy(rules).into_owned();
        for (k, v) in [("stepSize", "0"), ("stepSize", "-1"), ("minNotional", "-5")] {
            let b = rules.replace(&format!(r#""{k}":"#), &format!(r#""x{k}":"#)).replace("}]}", &format!(r#","{k}":"{v}"}}]}}"#));
            assert_eq!(scan_eq_symbol(b.as_bytes(), b"A"), Err(ScanErr::BadField), "{k} {v}");
        }
        for k in ["maxQty", "maxNotional", "minQty", "multiplierUp"] {
            let b = rules.replace("}]}", &format!(r#","{k}":"-1"}}]}}"#));
            assert_eq!(scan_eq_symbol(b.as_bytes(), b"A"), Err(ScanErr::BadField), "{k}");
        }
        let two = rules.replace("}]}", r#"},{"symbol":"A","tradability":"SELL","fractionable":false,"stepSize":"1","minNotional":"5"}]}"#);
        assert_eq!(scan_eq_symbol(two.as_bytes(), b"A"), Err(ScanErr::Malformed), "a ticker listed twice");
        // One UUID, either case: one key, one id.
        assert_eq!(eq_key(up.as_bytes()), eq_key(ID.as_bytes()));
        let id = EqOrderId::of(ID.as_bytes()).expect("a UUID");
        assert!(id.is(up.as_bytes()) && !EqOrderId::NONE.is(b""));
    }

    /// The parser review's round-2 F2 and F3: each refusal a mutation of
    /// its own check survived until now — one case each (break-and-watch:
    /// the ack without `status` read as `F`, and the history row without
    /// `symbol` read whole, before these cases; each fails with its check
    /// dropped).
    #[test]
    fn the_round_two_refusals_are_each_their_own_case() {
        let (mut a, mut d, mut x, mut r, mut o) = (EqAck::default(), EqDetail::default(), [EqExec::default(); 4], EqReport::default(), [EqOpenRow::default(); 4]);
        // The place's receipt: `status` required (else a lost status reads
        // as the venue's refusal — an order resting, retired REJECTED);
        // `S` names both ids.
        let no_status = format!(r#"{{"orderId":"{ID}","clientOrderId":"x"}}"#);
        assert_eq!(scan_eq_ack(no_status.as_bytes(), &mut a), Err(ScanErr::Missing), "no status");
        let s_no_cid = format!(r#"{{"status":"S","orderId":"{ID}"}}"#);
        assert_eq!(scan_eq_ack(s_no_cid.as_bytes(), &mut a), Err(ScanErr::Missing), "S without our id");
        // The cancel's answer: its word, its id a UUID, both named.
        assert_eq!(scan_eq_cancel(format!(r#"{{"orderId":"{ID}","status":"X"}}"#).as_bytes()), Err(ScanErr::BadField), "status X");
        assert_eq!(scan_eq_cancel(br#"{"orderId":"1234","status":"S"}"#), Err(ScanErr::BadField), "not a UUID");
        assert_eq!(scan_eq_cancel(format!(r#"{{"orderId":"{ID}"}}"#).as_bytes()), Err(ScanErr::Missing), "no status");
        // The cancel-all's answer: `success`, a bool.
        assert_eq!(scan_eq_cancel_all(b"{}"), Err(ScanErr::Missing));
        assert_eq!(scan_eq_cancel_all(br#"{"success":"yes"}"#), Err(ScanErr::BadField));
        // A detail's execution: its time.
        let no_at = detail("FILLED", "0.01", r#"{"executionId":"e1","price":"336.1","qty":"0.01"}"#);
        assert_eq!(scan_eq_detail(no_at.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::Missing)), "an execution without its time");
        // A detail's own keys.
        let full = detail("ACCEPTED", "0", "");
        for k in ["orderId", "symbol", "side", "status"] {
            let b = full.replacen(&format!(r#""{k}":"#), &format!(r#""x{k}":"#), 1);
            assert_eq!(scan_eq_detail(b.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::Missing)), "{k}");
        }
        // A history row: its symbol (else its execution leaves the sum
        // without a word), its side named and known.
        let hrow = |extra: &str| format!(r#"{{"page":1,"size":100,"total":1,"rows":[{{"executionId":"x1","orderId":"{ID}"{extra},"price":"1","qty":"1","executionAt":1}}]}}"#);
        let whole = hrow(r#","symbol":"AAPL","side":"BUY""#);
        assert!(scan_eq_trades(whole.as_bytes(), &mut x).is_ok());
        assert_eq!(scan_eq_trades(hrow(r#","side":"BUY""#).as_bytes(), &mut x), Err(ArrErr::Scan(ScanErr::Missing)), "no symbol");
        assert_eq!(scan_eq_trades(hrow(r#","symbol":"AAPL""#).as_bytes(), &mut x), Err(ArrErr::Scan(ScanErr::Missing)), "no side");
        assert_eq!(scan_eq_trades(hrow(r#","symbol":"AAPL","side":"HOLD""#).as_bytes(), &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "an unknown side");
        // A page: its size and rows named.
        assert_eq!(scan_eq_trades(br#"{"page":1,"total":0,"rows":[]}"#, &mut x), Err(ArrErr::Scan(ScanErr::Missing)), "no size");
        assert_eq!(scan_eq_trades(br#"{"page":1,"size":100,"total":0}"#, &mut x), Err(ArrErr::Scan(ScanErr::Missing)), "no rows");
        // A ticker never empty: a detail's, a listed order's, a history
        // row's.
        let empty = full.replace(r#""symbol":"AAPL""#, r#""symbol":"""#);
        assert_eq!(scan_eq_detail(empty.as_bytes(), &mut d, &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "a detail's empty ticker");
        assert_eq!(scan_eq_trades(hrow(r#","symbol":"","side":"BUY""#).as_bytes(), &mut x), Err(ArrErr::Scan(ScanErr::BadField)), "a row's empty ticker");
        let open_empty = format!(r#"[{{"orderId":"{ID}","symbol":"","side":"BUY","status":"NEW","filledQty":"0"}}]"#);
        assert_eq!(scan_eq_open(open_empty.as_bytes(), &mut o), Err(ArrErr::Scan(ScanErr::BadField)), "a listed order's empty ticker");
        // The listing's rows: symbol, side and status named.
        for k in ["symbol", "side", "status"] {
            let row = format!(r#"[{{"orderId":"{ID}","symbol":"AAPL","side":"BUY","status":"NEW","filledQty":"0"}}]"#).replacen(&format!(r#""{k}":"#), &format!(r#""x{k}":"#), 1);
            assert_eq!(scan_eq_open(row.as_bytes(), &mut o), Err(ArrErr::Scan(ScanErr::Missing)), "{k}");
        }
        // The report: the figures' shapes, its side's word, its times as
        // numbers — and (F3) a time `null` reads 0, `U` then `E`.
        for (from, to) in [
            (r#""p":336.1"#, r#""p":"abc""#),
            (r#""tc":10.4361"#, r#""tc":true"#),
            (r#""S":"buy""#, r#""S":"hold""#),
            (r#""E":1791700001000"#, r#""E":"x""#),
            (r#""U":1791700001000"#, r#""U":-1"#),
        ] {
            let f = report("0", "new", "ORDER_UPDATE").replace(from, to);
            assert_ne!(f, report("0", "new", "ORDER_UPDATE"), "{from}");
            assert_eq!(scan_order_report(f.as_bytes(), &mut r), Err(ScanErr::BadField), "{to}");
        }
        let u_null = report("0", "new", "ORDER_UPDATE").replace(r#""U":1791700001000"#, r#""U":null"#);
        assert_eq!(scan_order_report(u_null.as_bytes(), &mut r), Ok(()));
        assert_eq!(r.update_ms, 1_791_700_001_000, "U null: the event's time");
        let both_null = u_null.replace(r#""E":1791700001000"#, r#""E":null"#);
        assert_eq!(scan_order_report(both_null.as_bytes(), &mut r), Ok(()));
        assert_eq!(r.update_ms, 0);
        // The rules: no figure negative — `-0` and a negative below the
        // scale included (F1) — `maxNumOrders` within u32, `fractionable` a
        // bool; another row whole (its charset, its keys) even when not
        // the one asked.
        let rules = r#"{"symbols":[{"symbol":"A","tradability":"BUY_SELL","fractionable":true,"stepSize":"1","minNotional":"5"}]}"#;
        for extra in [
            r#""multiplierDown":"-1""#,
            r#""maxQty":"-0""#,
            r#""multiplierUp":"-0.0000000001""#,
            r#""minNotional":"-0""#,
            r#""maxNumOrders":4294967296"#,
        ] {
            let b = rules.replace(r#""minNotional":"5"}"#, &format!(r#""minNotional":"5",{extra}}}"#)).replacen(r#""minNotional":"5","minNotional""#, r#""xminNotional":"5","minNotional""#, 1);
            assert_eq!(scan_eq_symbol(b.as_bytes(), b"A"), Err(ScanErr::BadField), "{extra}");
        }
        assert_eq!(scan_eq_symbol(rules.replace("true", r#""yes""#).as_bytes(), b"A"), Err(ScanErr::BadField), "fractionable a string");
        let other_bad = rules.replace("}]}", r#"},{"symbol":"B$","tradability":"SELL","fractionable":false,"stepSize":"1","minNotional":"5"}]}"#);
        assert_eq!(scan_eq_symbol(other_bad.as_bytes(), b"A"), Err(ScanErr::BadField), "another row's charset");
        let other_short = rules.replace("}]}", r#"},{"symbol":"B","tradability":"SELL","stepSize":"1","minNotional":"5"}]}"#);
        assert_eq!(scan_eq_symbol(other_short.as_bytes(), b"A"), Err(ScanErr::Missing), "another row short of a key");
    }
}
