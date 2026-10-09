// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The user-data stream (plan §3.3; BX-2, BX-15).** Gateway thread.
//!
//! The FILL is the user stream's (BX-2): an order answer is only the ACK.
//! USDⓈ-M reports one trade twice — `TRADE_LITE` (lower latency, no
//! commission) and inside `ORDER_TRADE_UPDATE` — and the gateway books the
//! first and drops the second on (row, trade id) ([`crate::oot::TidRing`]).
//! The commission rides only the update (`n`, its asset `N`): the gateway
//! journals it once per trade, whichever report booked the fill (BX11,
//! `crate::journal::J_FEE`). A `TRADE_LITE` naming one is read past: its
//! commission reads 0 and its asset empty.
//!
//! [`scan_user_event`] reads one frame in place into a [`UserEvent`] (spans
//! into the frame, integers ×1e6; no allocation). Fail closed (BX-15): a
//! frame whose event it knows but cannot read is an `Err`, which the
//! gateway turns into a halt observation — never a skip. An event it does
//! not act on (`STRATEGY_UPDATE`, `GRID_UPDATE`, …) scans as [`UE_OTHER`]
//! and is counted. A key the scan reads, named twice in one object, is
//! refused (BX7): no other reader of the frame can then pick another copy
//! than the scan did. Both the raw form (`/ws`, `/private/ws`) and the
//! combined form (`{"stream":…,"data":{…}}`) are read.
//!
//! [`UserStream`] holds the listenKey and the socket's lifecycle state;
//! the gateway drives it (the REST calls that create and keep the key are
//! the gateway's, the one REST caller).

use core_types::Side;

use crate::inst::{PRODUCT_COINM, PRODUCT_USDM};
use crate::json::{dec_1e6, dec_1e9, str_of, u64_of, Elems, Pairs};

/// `ORDER_TRADE_UPDATE`.
pub const UE_ORDER: u8 = 1;
/// `TRADE_LITE`.
pub const UE_TRADE_LITE: u8 = 2;
/// `ACCOUNT_UPDATE` (a margin re-read follows). BX7: its positions' margin
/// types are read — [`UserEvent::isolated`] counts the ones not cross,
/// [`NotCross`] names them.
pub const UE_ACCOUNT: u8 = 3;
/// `MARGIN_CALL` (BX-20: `MarginRisk` on every Binance slot).
pub const UE_MARGIN_CALL: u8 = 4;
/// `listenKeyExpired` (re-create the key, reconnect).
pub const UE_KEY_EXPIRED: u8 = 5;
/// An event the arm does not act on.
pub const UE_OTHER: u8 = 6;
/// Portfolio Margin's `riskLevelChange` (BX7, C2): [`UserEvent::risk`]
/// says which; `m` and `eq` ride in `qty_1e6` and `px_1e6`.
pub const UE_RISK: u8 = 7;
/// Portfolio Margin's `liabilityChange` (a borrow; journaled only).
pub const UE_LIABILITY: u8 = 8;
/// `ACCOUNT_CONFIG_UPDATE` (BX7, carried): [`UserEvent::config`] says what
/// changed, as bits — a symbol's leverage (`ac`, its symbol in `symbol`),
/// the multi-assets mode (`ai`), or both in one frame.
pub const UE_CONFIG: u8 = 9;

/// [`UserEvent::config`]'s bit for `ac`: a symbol's leverage changed.
pub const CONFIG_LEVERAGE: u8 = 1 << 0;
/// Its bit for `ai` with `j: true`: the multi-assets mode went ON (BX-19's
/// single-asset assertion no longer holds).
pub const CONFIG_MULTI_ASSETS_ON: u8 = 1 << 1;
/// Its bit for `ai` with `j: false`: the multi-assets mode is off (as
/// asserted).
pub const CONFIG_MULTI_ASSETS_OFF: u8 = 1 << 2;

/// A frame naming this many positions not cross-margined is refused
/// (`BadField`): [`UserEvent::isolated`] is exact, never saturated (BX7).
pub const ISOLATED_MAX: u8 = u8::MAX;

/// `riskLevelChange`'s `s`: `MARGIN_CALL`.
pub const RISK_MARGIN_CALL: u8 = 1;
/// `riskLevelChange`'s `s`: `REDUCE_ONLY` (the venue restricts the
/// account: a `VenueLock`).
pub const RISK_REDUCE_ONLY: u8 = 2;
/// `riskLevelChange`'s `s`: `FORCE_LIQUIDATION` (a `VenueLock`).
pub const RISK_FORCE_LIQUIDATION: u8 = 3;
/// `riskLevelChange`'s `s`: a word this build does not know (read as a
/// lock: fail closed).
pub const RISK_UNKNOWN: u8 = 4;
/// `riskLevelChange`'s `s`: `NORMAL` (PM Pro's stream names the way back;
/// BX7, C3).
pub const RISK_NORMAL: u8 = 5;

/// Execution type `x`: `NEW`.
pub const X_NEW: u8 = 1;
/// `CANCELED`.
pub const X_CANCELED: u8 = 2;
/// `CALCULATED` (a liquidation fill).
pub const X_CALCULATED: u8 = 3;
/// `EXPIRED`.
pub const X_EXPIRED: u8 = 4;
/// `TRADE`.
pub const X_TRADE: u8 = 5;
/// `AMENDMENT` (a modify applied).
pub const X_AMENDMENT: u8 = 6;
/// Any other execution type.
pub const X_OTHER: u8 = 7;

/// Order status `X`: `NEW`.
pub const S_NEW: u8 = 1;
/// `PARTIALLY_FILLED`.
pub const S_PARTIALLY_FILLED: u8 = 2;
/// `FILLED`.
pub const S_FILLED: u8 = 3;
/// `CANCELED`.
pub const S_CANCELED: u8 = 4;
/// `EXPIRED`.
pub const S_EXPIRED: u8 = 5;
/// `EXPIRED_IN_MATCH` (self-trade prevention).
pub const S_EXPIRED_IN_MATCH: u8 = 6;
/// Any other status.
pub const S_OTHER: u8 = 7;

/// Why a frame did not scan (BX-15).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ScanErr {
    /// Not a JSON object, or a malformed one.
    Malformed = 1,
    /// No `e` (event type).
    NoEvent = 2,
    /// A field the event needs is missing.
    Missing = 3,
    /// A field is present but unreadable.
    BadField = 4,
}

/// A byte span into the frame.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    /// Start.
    pub at: u32,
    /// Length.
    pub len: u32,
}

impl Span {
    /// The span of `inner`, a sub-slice of `outer`.
    #[inline(always)]
    pub(crate) fn of(outer: &[u8], inner: &[u8]) -> Self {
        // `inner` is a sub-slice of `outer`: its offset is the address gap.
        let at = inner.as_ptr() as usize - outer.as_ptr() as usize;
        Self {
            at: at as u32,
            len: inner.len() as u32,
        }
    }

    /// The bytes it names in `frame`.
    #[inline(always)]
    #[must_use]
    pub fn get<'a>(&self, frame: &'a [u8]) -> &'a [u8] {
        let a = self.at as usize;
        frame.get(a..a + self.len as usize).unwrap_or(&[])
    }
}

/// One scanned event. 128 B.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct UserEvent {
    /// `i`: the venue order id.
    pub venue_oid: u64,
    /// `t`: the trade id (0 when no trade).
    pub trade_id: u64,
    /// `l`: the last fill's quantity ×1e6.
    pub last_qty_1e6: i64,
    /// `L`: the last fill's price ×1e6.
    pub last_px_1e6: i64,
    /// `z`: the order's cumulative filled quantity ×1e6.
    pub cum_qty_1e6: i64,
    /// `q`: the order's quantity ×1e6 (`riskLevelChange`: `m`, the total
    /// maintenance margin, USD ×1e6).
    pub qty_1e6: i64,
    /// `p`: the order's price ×1e6 (`riskLevelChange`: `eq`, the account
    /// equity, USD ×1e6).
    pub px_1e6: i64,
    /// `n`: the commission ×1e9, in its asset [`Self::fee_asset`] (BX11:
    /// ×1e9 — COIN-M charges it in the coin, 8 decimals); 0 when absent
    /// (`TRADE_LITE`, an update without a trade).
    pub commission_1e9: i64,
    /// `ap`: the order's average fill price ×1e6 (0 when none, and on
    /// `TRADE_LITE`).
    pub avg_px_1e6: i64,
    /// `E`: the event time, ms.
    pub event_ms: u64,
    /// `T`: the trade / transaction time, ms.
    pub trade_ms: u64,
    /// `s`: the symbol.
    pub symbol: Span,
    /// `c`: the client order id.
    pub cid: Span,
    /// `N`: the commission's asset (BX11); empty when absent.
    pub fee_asset: Span,
    /// `UE_*`.
    pub kind: u8,
    /// `Side` as its `u8`.
    pub side: u8,
    /// `X_*`.
    pub exec: u8,
    /// `S_*`.
    pub status: u8,
    /// `m`: the fill was a maker.
    pub maker: u8,
    /// `fs`: the business unit a Portfolio Margin event names —
    /// `PRODUCT_USDM` (`"UM"`), `PRODUCT_COINM` (`"CM"`); 0 when absent (a
    /// classic stream's event is its own product's).
    pub fs: u8,
    /// `riskLevelChange`'s `s` (`RISK_*`); 0 otherwise.
    pub risk: u8,
    /// `ACCOUNT_CONFIG_UPDATE`'s changes (`CONFIG_*` bits); 0 otherwise.
    pub config: u8,
    /// `ACCOUNT_UPDATE` (BX7, BX-19): how many positions (`a.P[]`) are not
    /// cross-margined — `mt` `"isolated"`, a word this build does not know,
    /// or none at all on a classic stream (fail closed). Portfolio Margin's
    /// events name `fs`, and its positions carry no `mt`: cross by
    /// construction. Exact: a frame with [`ISOLATED_MAX`] is refused. The
    /// first one's symbol is in [`Self::symbol`]; [`NotCross`] names them all.
    pub isolated: u8,
    /// `ACCOUNT_UPDATE`: where its `a.P` array starts in the frame (0: it
    /// names none) — the array the scan counted, which [`NotCross`] walks.
    pub positions: u32,
}

const _: () = assert!(core::mem::size_of::<UserEvent>() == 128);

fn exec_of(s: &[u8]) -> u8 {
    match s {
        b"NEW" => X_NEW,
        b"CANCELED" => X_CANCELED,
        b"CALCULATED" => X_CALCULATED,
        b"EXPIRED" => X_EXPIRED,
        b"TRADE" => X_TRADE,
        b"AMENDMENT" => X_AMENDMENT,
        _ => X_OTHER,
    }
}

/// An order status word (`X`, and the `status` of an order answer).
#[must_use]
pub fn status_of(s: &[u8]) -> u8 {
    match s {
        b"NEW" => S_NEW,
        b"PARTIALLY_FILLED" => S_PARTIALLY_FILLED,
        b"FILLED" => S_FILLED,
        b"CANCELED" => S_CANCELED,
        b"EXPIRED" => S_EXPIRED,
        b"EXPIRED_IN_MATCH" => S_EXPIRED_IN_MATCH,
        _ => S_OTHER,
    }
}

/// `BUY` / `SELL` → the side's `u8`.
pub fn side_of(s: &[u8]) -> Result<u8, ScanErr> {
    match s {
        b"BUY" => Ok(Side::Bid as u8),
        b"SELL" => Ok(Side::Ask as u8),
        _ => Err(ScanErr::BadField),
    }
}

// Field-presence bits for the fail-closed check.
const F_S: u32 = 1 << 0;
const F_C: u32 = 1 << 1;
const F_SIDE: u32 = 1 << 2;
const F_X: u32 = 1 << 3;
const F_ST: u32 = 1 << 4;
const F_I: u32 = 1 << 5;
const F_L: u32 = 1 << 6;
const F_LP: u32 = 1 << 7;
const F_Z: u32 = 1 << 8;
const F_T: u32 = 1 << 9;
const F_Q: u32 = 1 << 10;
const F_P: u32 = 1 << 11;
const F_N: u32 = 1 << 12;
const F_AP: u32 = 1 << 13;
const F_M: u32 = 1 << 14;
const F_TT: u32 = 1 << 15;
const F_FA: u32 = 1 << 16;
const NEED_ORDER: u32 = F_S | F_C | F_SIDE | F_X | F_ST | F_I | F_L | F_LP | F_Z | F_T | F_Q | F_P;
const NEED_LITE: u32 = F_S | F_C | F_SIDE | F_I | F_L | F_LP | F_T;

#[inline(always)]
fn malformed(_: ()) -> ScanErr {
    ScanErr::Malformed
}

#[inline(always)]
fn bad(_: ()) -> ScanErr {
    ScanErr::BadField
}

/// Read the order fields of `obj` (the `o` object of `ORDER_TRADE_UPDATE`,
/// or the `TRADE_LITE` event itself) into `out`: the bits of the keys read,
/// each at most once — a second copy is refused (BX7, the one-reading law:
/// two copies of `c` or `s` would book a fill to whichever a reader picked).
fn order_fields(f: &[u8], obj: usize, out: &mut UserEvent) -> Result<u32, ScanErr> {
    let mut w = Pairs::new(f, obj).map_err(malformed)?;
    let mut seen = 0u32;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let bit = match k {
            b"s" => {
                // An order event names its symbol: an empty one is refused
                // (fail closed), never looked up.
                let sym = str_of(f, a, z).map_err(bad)?;
                if sym.is_empty() {
                    return Err(ScanErr::BadField);
                }
                out.symbol = Span::of(f, sym);
                F_S
            }
            b"c" => {
                out.cid = Span::of(f, str_of(f, a, z).map_err(bad)?);
                F_C
            }
            b"S" => {
                out.side = side_of(str_of(f, a, z).map_err(bad)?)?;
                F_SIDE
            }
            b"x" => {
                out.exec = exec_of(str_of(f, a, z).map_err(bad)?);
                F_X
            }
            b"X" => {
                out.status = status_of(str_of(f, a, z).map_err(bad)?);
                F_ST
            }
            b"i" => {
                out.venue_oid = u64_of(f, a, z).map_err(bad)?;
                F_I
            }
            b"l" => {
                out.last_qty_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                F_L
            }
            b"L" => {
                out.last_px_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                F_LP
            }
            b"z" => {
                out.cum_qty_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                F_Z
            }
            b"t" => {
                out.trade_id = u64_of(f, a, z).map_err(bad)?;
                F_T
            }
            b"q" => {
                out.qty_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                F_Q
            }
            b"p" => {
                out.px_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                F_P
            }
            b"n" => {
                out.commission_1e9 = dec_1e9(f, a, z).map_err(bad)?;
                F_N
            }
            b"N" => {
                out.fee_asset = Span::of(f, str_of(f, a, z).map_err(bad)?);
                F_FA
            }
            b"ap" => {
                out.avg_px_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                F_AP
            }
            b"m" => {
                out.maker = crate::json::bool_of(f, a, z).map_err(bad)? as u8;
                F_M
            }
            b"T" => {
                out.trade_ms = u64_of(f, a, z).map_err(bad)?;
                F_TT
            }
            _ => 0,
        };
        if seen & bit != 0 {
            return Err(ScanErr::Malformed);
        }
        seen |= bit;
    }
    Ok(seen)
}

/// **Scan one user-data frame** (module docs).
pub fn scan_user_event(frame: &[u8], out: &mut UserEvent) -> Result<(), ScanErr> {
    *out = UserEvent::default();
    let mut w = Pairs::new(frame, 0).map_err(malformed)?;
    let mut ev: &[u8] = &[];
    let mut inner = usize::MAX;
    let mut data = usize::MAX;
    // `riskLevelChange`'s top-level fields (PM, BX7), read once the event
    // is known: its status, maintenance and equity.
    let (mut top_s, mut top_m, mut top_eq) = (usize::MAX, usize::MAX, usize::MAX);
    let (mut top_s_end, mut top_m_end, mut top_eq_end) = (0usize, 0usize, 0usize);
    // `ACCOUNT_CONFIG_UPDATE`'s objects: a symbol's leverage, the
    // multi-assets mode. `ACCOUNT_UPDATE`'s: the balances and positions.
    let (mut ac, mut ai, mut acct) = (usize::MAX, usize::MAX, usize::MAX);
    // The keys read, a bit each: a second copy is refused (BX7).
    let mut seen = 0u16;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let bit: u16 = match k {
            b"e" => {
                ev = str_of(frame, a, z).map_err(bad)?;
                1
            }
            b"E" => {
                out.event_ms = u64_of(frame, a, z).map_err(bad)?;
                1 << 1
            }
            b"T" => {
                out.trade_ms = u64_of(frame, a, z).map_err(bad)?;
                1 << 2
            }
            b"o" => {
                inner = a;
                1 << 3
            }
            b"data" => {
                data = a;
                1 << 4
            }
            b"fs" => {
                out.fs = match str_of(frame, a, z).map_err(bad)? {
                    b"UM" => PRODUCT_USDM,
                    b"CM" => PRODUCT_COINM,
                    // A business unit this arm does not trade is no row's.
                    _ => u8::MAX,
                };
                1 << 5
            }
            b"s" => {
                (top_s, top_s_end) = (a, z);
                1 << 6
            }
            b"ac" => {
                ac = a;
                1 << 7
            }
            b"ai" => {
                ai = a;
                1 << 8
            }
            b"a" => {
                acct = a;
                1 << 9
            }
            b"m" => {
                (top_m, top_m_end) = (a, z);
                1 << 10
            }
            b"eq" => {
                (top_eq, top_eq_end) = (a, z);
                1 << 11
            }
            _ => 0,
        };
        if seen & bit != 0 {
            return Err(ScanErr::Malformed);
        }
        seen |= bit;
    }
    if ev.is_empty() {
        if data != usize::MAX {
            // The combined-stream wrapper: the event is `data`.
            return scan_data(frame, data, out);
        }
        return Err(ScanErr::NoEvent);
    }
    match ev {
        b"ORDER_TRADE_UPDATE" => {
            if inner == usize::MAX {
                return Err(ScanErr::Missing);
            }
            out.kind = UE_ORDER;
            let have = order_fields(frame, inner, out)?;
            if have & NEED_ORDER != NEED_ORDER {
                return Err(ScanErr::Missing);
            }
        }
        b"TRADE_LITE" => {
            out.kind = UE_TRADE_LITE;
            let have = order_fields(frame, 0, out)?;
            if have & NEED_LITE != NEED_LITE {
                return Err(ScanErr::Missing);
            }
            out.exec = X_TRADE;
            // The commission is the update's alone (BX11).
            out.commission_1e9 = 0;
            out.fee_asset = Span::default();
        }
        b"ACCOUNT_UPDATE" => {
            out.kind = UE_ACCOUNT;
            if acct != usize::MAX {
                account_positions(frame, acct, out)?;
            }
        }
        b"MARGIN_CALL" => out.kind = UE_MARGIN_CALL,
        b"listenKeyExpired" => out.kind = UE_KEY_EXPIRED,
        b"riskLevelChange" => {
            // Fail closed: the status is required; an unknown one reads as
            // a lock. The figures are optional (0 when absent).
            if top_s == usize::MAX {
                return Err(ScanErr::Missing);
            }
            out.kind = UE_RISK;
            out.risk = match str_of(frame, top_s, top_s_end).map_err(bad)? {
                b"NORMAL" => RISK_NORMAL,
                b"MARGIN_CALL" => RISK_MARGIN_CALL,
                b"REDUCE_ONLY" => RISK_REDUCE_ONLY,
                b"FORCE_LIQUIDATION" => RISK_FORCE_LIQUIDATION,
                _ => RISK_UNKNOWN,
            };
            if top_m != usize::MAX {
                out.qty_1e6 = dec_1e6(frame, top_m, top_m_end).map_err(bad)?;
            }
            if top_eq != usize::MAX {
                out.px_1e6 = dec_1e6(frame, top_eq, top_eq_end).map_err(bad)?;
            }
        }
        b"liabilityChange" => out.kind = UE_LIABILITY,
        b"ACCOUNT_CONFIG_UPDATE" => {
            // Fail closed: at least one of the two objects, each read whole
            // — a frame carrying both says both, and both are acted on.
            out.kind = UE_CONFIG;
            if ac == usize::MAX && ai == usize::MAX {
                return Err(ScanErr::Missing);
            }
            if ac != usize::MAX {
                let mut w = Pairs::new(frame, ac).map_err(malformed)?;
                let mut sym = None;
                while let Some((k, a, z)) = w.next().map_err(malformed)? {
                    if k == b"s" {
                        if sym.is_some() {
                            return Err(ScanErr::Malformed);
                        }
                        sym = Some(str_of(frame, a, z).map_err(bad)?);
                    }
                }
                match sym {
                    Some(s) if !s.is_empty() => out.symbol = Span::of(frame, s),
                    _ => return Err(ScanErr::Missing),
                }
                out.config |= CONFIG_LEVERAGE;
            }
            if ai != usize::MAX {
                let mut w = Pairs::new(frame, ai).map_err(malformed)?;
                let mut j = 0u8;
                while let Some((k, a, z)) = w.next().map_err(malformed)? {
                    if k == b"j" {
                        if j != 0 {
                            return Err(ScanErr::Malformed);
                        }
                        j = match frame.get(a..z) {
                            Some(b"true") => CONFIG_MULTI_ASSETS_ON,
                            Some(b"false") => CONFIG_MULTI_ASSETS_OFF,
                            _ => return Err(ScanErr::BadField),
                        };
                    }
                }
                if j == 0 {
                    return Err(ScanErr::Missing);
                }
                out.config |= j;
            }
        }
        _ => out.kind = UE_OTHER,
    }
    Ok(())
}

/// `ACCOUNT_UPDATE`'s `a.P[]`: the positions not cross-margined, counted
/// in `out.isolated`, the first one's symbol in `out.symbol`, the array's
/// place in `out.positions` (BX7). A second `P` is refused: the walk
/// ([`NotCross`]) reads one array, the one counted.
fn account_positions(f: &[u8], a: usize, out: &mut UserEvent) -> Result<(), ScanErr> {
    // Portfolio Margin's events name their business unit; its positions
    // carry no margin type.
    let pm = out.fs != 0;
    let mut w = Pairs::new(f, a).map_err(malformed)?;
    while let Some((k, v, _)) = w.next().map_err(malformed)? {
        if k != b"P" {
            continue;
        }
        if out.positions != 0 {
            return Err(ScanErr::Malformed);
        }
        out.positions = v as u32;
        let mut e = Elems::new(f, v).map_err(malformed)?;
        while let Some((pa, _)) = e.next().map_err(malformed)? {
            if let Some(sym) = not_cross(f, pa, pm)? {
                if out.isolated == 0 {
                    out.symbol = Span::of(f, sym);
                }
                out.isolated += 1;
                if out.isolated == ISOLATED_MAX {
                    return Err(ScanErr::BadField);
                }
            }
        }
    }
    Ok(())
}

/// One `P[]` entry: its symbol when it is not cross-margined — an `mt`
/// other than `"cross"`, or none at all on a classic stream (`pm` false;
/// a Portfolio Margin position carries none: cross by construction). Fail
/// closed: such an entry must name its symbol, and a key read twice is
/// refused. `None` when cross.
fn not_cross(f: &[u8], at: usize, pm: bool) -> Result<Option<&[u8]>, ScanErr> {
    let mut w = Pairs::new(f, at).map_err(malformed)?;
    let (mut sym, mut mt) = (None, None);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let slot = match k {
            b"s" => &mut sym,
            b"mt" => &mut mt,
            _ => continue,
        };
        if slot.is_some() {
            return Err(ScanErr::Malformed);
        }
        *slot = Some(str_of(f, a, z).map_err(bad)?);
    }
    let cross = match mt {
        Some(m) => m == b"cross",
        None => pm,
    };
    match (cross, sym) {
        (true, _) => Ok(None),
        (false, Some(s)) if !s.is_empty() => Ok(Some(s)),
        (false, _) => Err(ScanErr::Missing),
    }
}

/// **The positions an `ACCOUNT_UPDATE` names as not cross-margined** (BX7,
/// BX-19), in frame order, each one's symbol as a span of the frame — the
/// ones [`UserEvent::isolated`] counts, the first being
/// [`UserEvent::symbol`]. One pass over the event's `a.P` array from where
/// the scan found it ([`UserEvent::positions`]), by the scan's own reader:
/// the two cannot disagree. Nothing for any other event.
pub struct NotCross<'a> {
    frame: &'a [u8],
    elems: Option<Elems<'a>>,
    pm: bool,
}

impl<'a> NotCross<'a> {
    /// The walk over `ev`, scanned (`Ok`) from `frame`, raw or combined.
    #[must_use]
    pub fn new(frame: &'a [u8], ev: &UserEvent) -> Self {
        let elems = if ev.kind == UE_ACCOUNT && ev.positions != 0 {
            Elems::new(frame, ev.positions as usize).ok()
        } else {
            None
        };
        Self { frame, elems, pm: ev.fs != 0 }
    }

    /// The next one's symbol; `None` past the last.
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Span> {
        loop {
            let pa = match self.elems.as_mut()?.next() {
                Ok(Some((pa, _))) => pa,
                _ => {
                    self.elems = None;
                    return None;
                }
            };
            match not_cross(self.frame, pa, self.pm) {
                Ok(Some(s)) => return Some(Span::of(self.frame, s)),
                Ok(None) => {}
                Err(_) => {
                    // The scan read every entry with this reader: a frame it
                    // accepted cannot fail here. The walk stops, never skips.
                    debug_assert!(false, "an entry the scan accepted does not read");
                    self.elems = None;
                    return None;
                }
            }
        }
    }
}

/// The `data` object of a combined-stream frame, scanned as a raw event.
fn scan_data(frame: &[u8], at: usize, out: &mut UserEvent) -> Result<(), ScanErr> {
    let end = core_parse::skip_json_value(frame, at).ok_or(ScanErr::Malformed)?;
    let inner = frame.get(at..end).ok_or(ScanErr::Malformed)?;
    if inner.first() != Some(&b'{') {
        return Err(ScanErr::Malformed);
    }
    // One level only: a `data` inside `data` is not a venue shape. Scanned
    // straight into `out`, whose spans are then re-based onto the frame.
    let r = scan_user_event_flat(inner, out);
    out.symbol.at += at as u32;
    out.cid.at += at as u32;
    out.fee_asset.at += at as u32;
    if out.positions != 0 {
        out.positions += at as u32;
    }
    r
}

fn scan_user_event_flat(frame: &[u8], out: &mut UserEvent) -> Result<(), ScanErr> {
    let mut w = Pairs::new(frame, 0).map_err(malformed)?;
    while let Some((k, _, _)) = w.next().map_err(malformed)? {
        if k == b"data" {
            return Err(ScanErr::Malformed);
        }
    }
    scan_user_event(frame, out)
}

// -------------------------------------------------------------------------
// The listenKey and the socket's lifecycle
// -------------------------------------------------------------------------

/// The listenKey's capacity (the venue's keys are 64 characters).
pub const LISTEN_KEY_MAX: usize = 96;
/// A keep-alive every 30 minutes (the key lives 60).
pub const KEEPALIVE_NS: u64 = 30 * 60 * 1_000_000_000;

/// Where the stream is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UsPhase {
    /// No key: the gateway must `POST` one.
    NeedKey = 0,
    /// A `POST` is in flight.
    KeyPending = 1,
    /// A key is held; the socket may connect.
    Keyed = 2,
}

/// The key and its clocks (the socket is the gateway's `WsConn`).
#[derive(Clone, Debug)]
pub struct UserStream {
    key: [u8; LISTEN_KEY_MAX],
    key_len: usize,
    /// The phase.
    pub phase: UsPhase,
    /// Monotonic ns of the last successful create or keep-alive.
    pub kept_ns: u64,
    /// Monotonic ns of the last frame read.
    pub last_rx_ns: u64,
}

impl Default for UserStream {
    fn default() -> Self {
        Self::new()
    }
}

impl UserStream {
    /// No key yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            key: [0; LISTEN_KEY_MAX],
            key_len: 0,
            phase: UsPhase::NeedKey,
            kept_ns: 0,
            last_rx_ns: 0,
        }
    }

    /// Adopt a key from a `POST` answer: `Some(changed)`, or `None` — the
    /// key is refused whole, never truncated — if it is empty, too long or
    /// not `[A-Za-z0-9]` (it goes into a URL path).
    pub fn set_key(&mut self, key: &[u8], now_ns: u64) -> Option<bool> {
        if key.is_empty() || key.len() > LISTEN_KEY_MAX || !key.iter().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let changed = self.key() != key;
        // COPY: the listenKey (≤ 96 B) into the stream's own buffer — once
        // per create, cold; the REST answer that carried it is overwritten
        // by the next request — keeping a span into it was rejected.
        self.key[..key.len()].copy_from_slice(key);
        self.key_len = key.len();
        self.phase = UsPhase::Keyed;
        self.kept_ns = now_ns;
        Some(changed)
    }

    /// The key.
    #[must_use]
    pub fn key(&self) -> &[u8] {
        &self.key[..self.key_len]
    }

    /// The key expired (`listenKeyExpired`, or a keep-alive refused).
    pub fn expire(&mut self) {
        self.phase = UsPhase::NeedKey;
        self.key_len = 0;
    }

    /// A keep-alive is due.
    #[must_use]
    pub fn keepalive_due(&self, now_ns: u64) -> bool {
        self.phase == UsPhase::Keyed && now_ns.saturating_sub(self.kept_ns) >= KEEPALIVE_NS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    pub(crate) const OTU: &[u8] = br#"{"e":"ORDER_TRADE_UPDATE","E":1790000000123,"T":1790000000120,"o":{"s":"BTCUSDT","c":"mv0000000720000000000000005000000","S":"BUY","o":"LIMIT","f":"IOC","q":"0.012","p":"65000.1","ap":"65000.1","sp":"0","x":"TRADE","X":"FILLED","i":8886774,"l":"0.012","z":"0.012","L":"65000.1","N":"USDT","n":"0.312","T":1790000000119,"t":4242,"b":"0","a":"0","m":false,"R":false,"wt":"CONTRACT_PRICE","ot":"LIMIT","ps":"BOTH","cp":false,"rp":"0","pP":false,"si":0,"ss":0,"V":"EXPIRE_TAKER","pm":"NONE","gtd":0}}"#;
    const LITE: &[u8] = br#"{"e":"TRADE_LITE","E":1790000000100,"T":1790000000099,"s":"BTCUSDT","q":"0.012","p":"0.0","m":true,"c":"mvX","S":"SELL","L":"65000.2","l":"0.005","t":4243,"i":8886775}"#;

    #[test]
    fn an_order_trade_update() {
        let mut e = UserEvent::default();
        scan_user_event(OTU, &mut e).unwrap();
        assert_eq!((e.kind, e.exec, e.status, e.side), (UE_ORDER, X_TRADE, S_FILLED, Side::Bid as u8));
        assert_eq!((e.venue_oid, e.trade_id), (8_886_774, 4_242));
        assert_eq!((e.last_qty_1e6, e.last_px_1e6, e.cum_qty_1e6), (12_000, 65_000_100_000, 12_000));
        assert_eq!((e.qty_1e6, e.px_1e6, e.commission_1e9), (12_000, 65_000_100_000, 312_000_000));
        assert_eq!(e.fee_asset.get(OTU), b"USDT");
        assert_eq!(e.symbol.get(OTU), b"BTCUSDT");
        assert_eq!(e.cid.get(OTU), b"mv0000000720000000000000005000000");
        assert_eq!((e.event_ms, e.trade_ms), (1_790_000_000_123, 1_790_000_000_119));
    }

    #[test]
    fn a_trade_lite() {
        let mut e = UserEvent::default();
        scan_user_event(LITE, &mut e).unwrap();
        assert_eq!((e.kind, e.exec, e.side, e.maker), (UE_TRADE_LITE, X_TRADE, Side::Ask as u8, 1));
        assert_eq!((e.last_qty_1e6, e.last_px_1e6, e.trade_id, e.venue_oid), (5_000, 65_000_200_000, 4_243, 8_886_775));
        assert_eq!(e.cid.get(LITE), b"mvX");
        // BX11: a lite naming a commission is read past — the update's alone.
        let named = std::str::from_utf8(LITE).unwrap().replace(r#""t":4243"#, r#""t":4243,"n":"5","N":"BNB""#);
        scan_user_event(named.as_bytes(), &mut e).unwrap();
        assert_eq!((e.kind, e.commission_1e9, e.fee_asset.len), (UE_TRADE_LITE, 0, 0));
    }

    #[test]
    fn the_combined_form_rebases_its_spans() {
        let mut f = std::vec::Vec::new();
        f.extend_from_slice(br#"{"stream":"k","data":"#);
        f.extend_from_slice(OTU);
        f.extend_from_slice(b"}");
        let mut e = UserEvent::default();
        scan_user_event(&f, &mut e).unwrap();
        assert_eq!(e.symbol.get(&f), b"BTCUSDT");
        assert_eq!(e.cid.get(&f), b"mv0000000720000000000000005000000");
        // BX11 (the parser review's P-1): the commission's asset too.
        assert_eq!(e.fee_asset.get(&f), b"USDT");
        assert_eq!(e.kind, UE_ORDER);
    }

    #[test]
    fn a_known_event_missing_a_field_is_refused_never_skipped() {
        let cut = std::str::from_utf8(OTU).unwrap().replace(r#""t":4242,"#, "");
        let mut e = UserEvent::default();
        assert_eq!(scan_user_event(cut.as_bytes(), &mut e), Err(ScanErr::Missing));
        let bad_side = std::str::from_utf8(OTU).unwrap().replace(r#""S":"BUY""#, r#""S":"BOTH""#);
        assert_eq!(scan_user_event(bad_side.as_bytes(), &mut e), Err(ScanErr::BadField));
        assert_eq!(scan_user_event(br#"{"E":1}"#, &mut e), Err(ScanErr::NoEvent));
        assert_eq!(scan_user_event(br#"{"e":"ORDER_TRADE_UPDATE","E":1}"#, &mut e), Err(ScanErr::Missing));
        assert_eq!(scan_user_event(b"[1]", &mut e), Err(ScanErr::Malformed));
    }

    #[test]
    fn the_other_events() {
        let mut e = UserEvent::default();
        scan_user_event(br#"{"e":"MARGIN_CALL","E":1,"cw":"3.16","p":[]}"#, &mut e).unwrap();
        assert_eq!(e.kind, UE_MARGIN_CALL);
        scan_user_event(br#"{"e":"ACCOUNT_UPDATE","E":1,"T":1,"a":{"m":"ORDER","B":[],"P":[]}}"#, &mut e).unwrap();
        assert_eq!(e.kind, UE_ACCOUNT);
        scan_user_event(br#"{"e":"listenKeyExpired","E":1,"listenKey":"x"}"#, &mut e).unwrap();
        assert_eq!(e.kind, UE_KEY_EXPIRED);
        scan_user_event(br#"{"e":"STRATEGY_UPDATE","E":1}"#, &mut e).unwrap();
        assert_eq!(e.kind, UE_OTHER);
    }

    /// BX7 (C2): Portfolio Margin's one stream names each order's business
    /// unit (`fs`), and carries `riskLevelChange` — its status read fail
    /// closed (an unknown word is a lock).
    #[test]
    fn portfolio_margin_events() {
        let mut e = UserEvent::default();
        let pm = br#"{"e":"ORDER_TRADE_UPDATE","fs":"CM","E":1790000000123,"T":1790000000120,"i":"","o":{"s":"BTCUSD_PERP","c":"mvX","S":"SELL","o":"LIMIT","f":"IOC","q":"3","p":"65000.0","ap":"65000.0","sp":"0","x":"TRADE","X":"FILLED","i":77,"l":"3","z":"3","L":"65000.0","N":"BTC","n":"0.0000023","T":1790000000120,"t":9,"b":"0","a":"0","m":false,"R":true,"ps":"BOTH","rp":"0","V":"EXPIRE_TAKER","gtd":0}}"#;
        scan_user_event(pm, &mut e).unwrap();
        assert_eq!((e.kind, e.fs, e.status, e.last_qty_1e6), (UE_ORDER, PRODUCT_COINM, S_FILLED, 3_000_000));
        assert_eq!(e.symbol.get(pm), b"BTCUSD_PERP");
        let um = br#"{"e":"ORDER_TRADE_UPDATE","fs":"UM","E":1,"T":1,"o":{"s":"BTCUSDT","c":"mvY","S":"BUY","o":"LIMIT","f":"IOC","q":"0.002","p":"1","ap":"0","x":"NEW","X":"NEW","i":78,"l":"0","z":"0","L":"0","T":1,"t":0}}"#;
        scan_user_event(um, &mut e).unwrap();
        assert_eq!((e.kind, e.fs), (UE_ORDER, PRODUCT_USDM));
        let odd = br#"{"e":"ORDER_TRADE_UPDATE","fs":"XX","E":1,"T":1,"o":{"s":"X","c":"c","S":"BUY","o":"LIMIT","f":"IOC","q":"1","p":"1","ap":"0","x":"NEW","X":"NEW","i":1,"l":"0","z":"0","L":"0","T":1,"t":0}}"#;
        scan_user_event(odd, &mut e).unwrap();
        assert_eq!(e.fs, u8::MAX, "a business unit no lane trades is no row's");
        let risk = br#"{"e":"riskLevelChange","E":1587727187525,"u":"1.99999999","s":"MARGIN_CALL","eq":"30.23416728","ae":"30.23416728","m":"15.11708371"}"#;
        scan_user_event(risk, &mut e).unwrap();
        assert_eq!((e.kind, e.risk, e.qty_1e6, e.px_1e6), (UE_RISK, RISK_MARGIN_CALL, 15_117_083, 30_234_167));
        for (word, want) in [
            ("NORMAL", RISK_NORMAL),
            ("REDUCE_ONLY", RISK_REDUCE_ONLY),
            ("FORCE_LIQUIDATION", RISK_FORCE_LIQUIDATION),
            ("SOMETHING_NEW", RISK_UNKNOWN),
        ] {
            let f = format!(r#"{{"e":"riskLevelChange","E":1,"u":"1.1","s":"{word}","eq":"1","m":"1"}}"#);
            scan_user_event(f.as_bytes(), &mut e).unwrap();
            assert_eq!((e.kind, e.risk), (UE_RISK, want), "{word}");
        }
        let no_status = br#"{"e":"riskLevelChange","E":1,"u":"1.1","eq":"1","m":"1"}"#;
        assert_eq!(scan_user_event(no_status, &mut e), Err(ScanErr::Missing));
        let liab = br#"{"e":"liabilityChange","E":1,"a":"USDT","t":"BORROW","T":1,"p":"10","l":"10","i":1}"#;
        scan_user_event(liab, &mut e).unwrap();
        assert_eq!(e.kind, UE_LIABILITY);
    }

    /// The symbols [`NotCross`] names in `f`.
    fn walk(f: &[u8], e: &UserEvent) -> std::vec::Vec<std::vec::Vec<u8>> {
        let mut w = NotCross::new(f, e);
        let mut v = std::vec::Vec::new();
        while let Some(s) = w.next() {
            v.push(s.get(f).to_vec());
        }
        v
    }

    /// BX7 (BX-19): `ACCOUNT_UPDATE`'s positions not cross-margined are
    /// counted — an `isolated`, a word this build does not know, or no
    /// `mt` on a classic stream — the first's symbol in the event, all of
    /// them through `NotCross`, raw or combined. Refused: one with no
    /// symbol, a key read twice (a decoy `a`, a second `mt`), a second `P`,
    /// and a count that would saturate.
    #[test]
    fn account_update_margin_types() {
        let mut e = UserEvent::default();
        let cross = br#"{"e":"ACCOUNT_UPDATE","E":1,"T":1,"a":{"m":"ORDER","B":[{"a":"USDT","wb":"10","cw":"10","bc":"0"}],"P":[{"s":"BTCUSDT","pa":"0.002","ep":"65000","cr":"0","up":"0","mt":"cross","iw":"0","ps":"BOTH"}]}}"#;
        scan_user_event(cross, &mut e).unwrap();
        assert_eq!((e.kind, e.isolated), (UE_ACCOUNT, 0));
        assert!(walk(cross, &e).is_empty());
        let two = br#"{"e":"ACCOUNT_UPDATE","E":1,"T":1,"a":{"m":"FUNDING_FEE","B":[],"P":[{"s":"ETHUSDT","pa":"1","mt":"isolated","ps":"BOTH"},{"s":"XRPUSDT","pa":"1","mt":"cross","ps":"BOTH"},{"s":"BTCUSDT","pa":"1","mt":"margin?","ps":"BOTH"}]}}"#;
        scan_user_event(two, &mut e).unwrap();
        assert_eq!((e.isolated, e.symbol.get(two)), (2, &b"ETHUSDT"[..]));
        assert_eq!(walk(two, &e), [b"ETHUSDT".to_vec(), b"BTCUSDT".to_vec()], "an unknown word is not cross");
        assert_eq!(NotCross::new(two, &e).next(), Some(e.symbol), "the walk's first is the event's own span");
        // A classic position with no `mt` is not cross (fail closed).
        let bare = br#"{"e":"ACCOUNT_UPDATE","E":1,"T":1,"a":{"m":"ORDER","B":[],"P":[{"s":"BTCUSDT","pa":"1","ps":"BOTH"}]}}"#;
        scan_user_event(bare, &mut e).unwrap();
        assert_eq!((e.isolated, walk(bare, &e)), (1, std::vec![b"BTCUSDT".to_vec()]));
        // Portfolio Margin's positions carry no `mt` (cross by construction).
        let pm = br#"{"e":"ACCOUNT_UPDATE","fs":"UM","E":1,"T":1,"i":"","a":{"B":[],"P":[{"s":"BTCUSDT","pa":"0","ep":"0","cr":"200","up":"0","ps":"BOTH"}],"m":"ORDER"}}"#;
        scan_user_event(pm, &mut e).unwrap();
        assert_eq!((e.isolated, e.fs), (0, PRODUCT_USDM));
        assert!(walk(pm, &e).is_empty());
        // The combined form: the spans re-based onto the whole frame.
        let wrapped = br#"{"stream":"k","data":{"e":"ACCOUNT_UPDATE","E":1,"T":1,"a":{"m":"ORDER","B":[],"P":[{"s":"BTCUSD_PERP","pa":"1","mt":"isolated","ps":"BOTH"}]}}}"#;
        scan_user_event(wrapped, &mut e).unwrap();
        assert_eq!((e.isolated, e.symbol.get(wrapped)), (1, &b"BTCUSD_PERP"[..]));
        assert_eq!(walk(wrapped, &e), [b"BTCUSD_PERP".to_vec()]);
        assert_eq!(NotCross::new(wrapped, &e).next(), Some(e.symbol));
        for (bad, want) in [
            (&br#"{"e":"ACCOUNT_UPDATE","E":1,"T":1,"a":{"m":"ORDER","B":[],"P":[{"pa":"1","mt":"isolated"}]}}"#[..], ScanErr::Missing),
            (&br#"{"e":"ACCOUNT_UPDATE","E":1,"a":0,"a":{"P":[{"s":"ETHUSDT","mt":"isolated"}]}}"#[..], ScanErr::Malformed),
            (&br#"{"stream":"k","data":{"e":"ACCOUNT_UPDATE","E":1,"a":{"P":[]},"a":{"P":[{"s":"ETHUSDT","mt":"isolated"}]}}}"#[..], ScanErr::Malformed),
            (&br#"{"e":"ACCOUNT_UPDATE","E":1,"a":{"P":[{"s":"ETHUSDT","mt":"isolated","mt":"cross"}]}}"#[..], ScanErr::Malformed),
            (&br#"{"e":"ACCOUNT_UPDATE","E":1,"a":{"P":[],"P":[{"s":"ETHUSDT","mt":"isolated"}]}}"#[..], ScanErr::Malformed),
            (&br#"{"e":"ACCOUNT_UPDATE","e":"ACCOUNT_UPDATE","E":1,"a":{"P":[]}}"#[..], ScanErr::Malformed),
        ] {
            assert_eq!(scan_user_event(bad, &mut e), Err(want), "{}", std::str::from_utf8(bad).unwrap());
        }
        // The count is exact: one that would saturate is refused whole.
        let many = |n: usize| {
            let mut f = std::string::String::from(r#"{"e":"ACCOUNT_UPDATE","E":1,"a":{"P":["#);
            let mut i = 0;
            while i < n {
                f.push_str(&format!(r#"{}{{"s":"S{i}","mt":"isolated"}}"#, if i == 0 { "" } else { "," }));
                i += 1;
            }
            f.push_str("]}}");
            f
        };
        let max = usize::from(ISOLATED_MAX);
        let most = many(max - 1);
        scan_user_event(most.as_bytes(), &mut e).unwrap();
        assert_eq!((usize::from(e.isolated), walk(most.as_bytes(), &e).len()), (max - 1, max - 1));
        assert_eq!(scan_user_event(many(max).as_bytes(), &mut e), Err(ScanErr::BadField));
        // A balance-only update names no position.
        let bal = br#"{"e":"ACCOUNT_UPDATE","E":1,"T":1,"a":{"m":"DEPOSIT","B":[{"a":"USDT","wb":"10","cw":"10","bc":"10"}],"P":[]}}"#;
        scan_user_event(bal, &mut e).unwrap();
        assert_eq!((e.kind, e.isolated), (UE_ACCOUNT, 0));
        assert!(walk(bal, &e).is_empty());
        // No other event is walked.
        scan_user_event(OTU, &mut e).unwrap();
        assert!(walk(OTU, &e).is_empty());
    }

    /// BX7 (carried): `ACCOUNT_CONFIG_UPDATE` — a symbol's leverage, the
    /// multi-assets mode, or both in one frame (neither lost) — read fail
    /// closed; a key read twice is refused.
    #[test]
    fn account_config_updates() {
        let mut e = UserEvent::default();
        let lev = br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1611646737479,"T":1611646737476,"ac":{"s":"BTCUSDT","l":25}}"#;
        scan_user_event(lev, &mut e).unwrap();
        assert_eq!((e.kind, e.config, e.symbol.get(lev)), (UE_CONFIG, CONFIG_LEVERAGE, &b"BTCUSDT"[..]));
        let on = br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ai":{"j":true}}"#;
        scan_user_event(on, &mut e).unwrap();
        assert_eq!((e.kind, e.config), (UE_CONFIG, CONFIG_MULTI_ASSETS_ON));
        let off = br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ai":{"j":false}}"#;
        scan_user_event(off, &mut e).unwrap();
        assert_eq!(e.config, CONFIG_MULTI_ASSETS_OFF);
        for both in [
            &br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ac":{"s":"XRPUSDT","l":5},"ai":{"j":true}}"#[..],
            &br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ai":{"j":true},"ac":{"s":"XRPUSDT","l":5}}"#[..],
        ] {
            scan_user_event(both, &mut e).unwrap();
            assert_eq!((e.config, e.symbol.get(both)), (CONFIG_LEVERAGE | CONFIG_MULTI_ASSETS_ON, &b"XRPUSDT"[..]));
        }
        for (bad, want) in [
            (&br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1}"#[..], ScanErr::Missing),
            (&br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ac":{"l":25}}"#[..], ScanErr::Missing),
            (&br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ac":{"s":"","l":25}}"#[..], ScanErr::Missing),
            (&br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ai":{}}"#[..], ScanErr::Missing),
            (&br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ai":{"j":"yes"}}"#[..], ScanErr::BadField),
            (&br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ac":{"s":"BTCUSDT","s":"XRPUSDT"}}"#[..], ScanErr::Malformed),
            (&br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ai":{"j":true,"j":false}}"#[..], ScanErr::Malformed),
            (&br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1,"T":1,"ac":{"s":"BTCUSDT"},"ac":{"s":"XRPUSDT"}}"#[..], ScanErr::Malformed),
        ] {
            assert_eq!(scan_user_event(bad, &mut e), Err(want), "{}", std::str::from_utf8(bad).unwrap());
        }
    }

    #[test]
    fn the_key_lifecycle() {
        let mut u = UserStream::new();
        assert_eq!(u.phase, UsPhase::NeedKey);
        let key: &[u8] = b"pqia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65a1";
        assert_eq!(u.set_key(b"has space", 1), None);
        assert_eq!(u.set_key(&[b'k'; LISTEN_KEY_MAX + 1], 1), None, "refused whole, never truncated");
        assert_eq!(u.set_key(key, 10), Some(true));
        assert_eq!(u.phase, UsPhase::Keyed);
        assert_eq!(u.set_key(key, 10), Some(false), "the same key: no new socket");
        assert!(!u.keepalive_due(10 + KEEPALIVE_NS - 1));
        assert!(u.keepalive_due(10 + KEEPALIVE_NS));
        u.expire();
        assert_eq!((u.phase, u.key().len()), (UsPhase::NeedKey, 0));
    }

    proptest! {
        #[test]
        fn never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut e = UserEvent::default();
            if scan_user_event(&bytes, &mut e).is_ok() {
                let _ = (e.symbol.get(&bytes), e.cid.get(&bytes));
                prop_assert!((UE_ORDER..=UE_CONFIG).contains(&e.kind));
                let mut w = NotCross::new(&bytes, &e);
                let mut n = 0u32;
                while let Some(s) = w.next() {
                    prop_assert!(s.at as usize + s.len as usize <= bytes.len());
                    n += 1;
                }
                prop_assert_eq!(n, u32::from(e.isolated));
            }
        }

        /// A mutation of a real frame never panics and, when it still
        /// scans as an order event, carries every required field.
        #[test]
        fn mutations_of_a_real_frame(at in 0usize..600, byte in any::<u8>()) {
            let mut f = OTU.to_vec();
            let i = at % f.len();
            f[i] = byte;
            let mut e = UserEvent::default();
            if scan_user_event(&f, &mut e).is_ok() && e.kind == UE_ORDER {
                prop_assert!(!e.symbol.get(&f).is_empty() && !e.cid.get(&f).is_empty());
            }
        }
    }
}
