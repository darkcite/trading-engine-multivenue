// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `orderUpdates` → order status changes, scanned in place (XH4b).
//!
//! ```text
//! {"channel":"orderUpdates","data":[
//!   {"order":{"coin":"BTC","side":"B","limitPx":"65000.0","sz":"0.01","oid":1,
//!             "timestamp":1,"origSz":"0.01","cloid":"0x…"},
//!    "status":"open","statusTimestamp":1}, …]}
//! ```
//!
//! Each row's `status` maps to an event kind and a reason
//! ([`status_of`]): `open` rests, `filled` fills out, every `…Canceled`
//! (and `scheduledCancel`) cancels, every `…Rejected` rejects, each with
//! the reason the venue's word names. A status the gateway does not
//! report (`triggered`, or one the venue adds) is kind 0: counted by the
//! caller, never guessed into a state.
//!
//! Fail-closed like every scanner here: a row without an `oid`, a side
//! or a status refuses the whole frame.

use core_parse::skip_ws;
use core_types::{
    ORDER_EVENT_REASON_BAD_ALO_PX, ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_EVENT_REASON_MARGIN,
    ORDER_EVENT_REASON_MIN_NOTIONAL, ORDER_EVENT_REASON_NONE, ORDER_EVENT_REASON_OPEN_INTEREST_CAP,
    ORDER_EVENT_REASON_OTHER, ORDER_EVENT_REASON_REDUCE_ONLY, ORDER_EVENT_REASON_SCHEDULED_CANCEL,
    ORDER_EVENT_REASON_SELF_TRADE, ORDER_EVENT_REASON_TICK_OR_LOT,
};

use crate::cloid::from_hex;
use crate::event::{EV_CANCELED, EV_FILLED, EV_REJECTED, EV_RESTING, REASON_IOC_MISS};
use crate::json::{array_start, decimal_field, object_end, object_field, string_field, u64_field};
use crate::response::{ScanErr, Span};

/// One row of `orderUpdates`.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct OrderUpdate {
    /// Venue order id.
    pub oid: u64,
    /// Our cloid, if the order carries one.
    pub cloid: Option<[u8; 16]>,
    /// Coin, relative to the scanned body.
    pub coin: Span,
    /// Limit price, 1e8.
    pub px_1e8: i64,
    /// Remaining size, 1e8.
    pub sz_1e8: i64,
    /// `statusTimestamp`, ms.
    pub status_ms: u64,
    /// `EV_*`, or 0 for a status the gateway does not report.
    pub kind: u8,
    /// The reason the status names.
    pub reason: u8,
    /// Buy side.
    pub is_buy: bool,
}

/// A venue status word → `(EV_* kind, reason)`; `(0, 0)` = not reported.
#[must_use]
pub fn status_of(s: &[u8]) -> (u8, u8) {
    match s {
        b"open" => (EV_RESTING, ORDER_EVENT_REASON_NONE),
        b"filled" => (EV_FILLED, ORDER_EVENT_REASON_NONE),
        b"canceled" => (EV_CANCELED, ORDER_EVENT_REASON_CANCEL_REQUESTED),
        b"marginCanceled" => (EV_CANCELED, ORDER_EVENT_REASON_MARGIN),
        b"openInterestCapCanceled" => (EV_CANCELED, ORDER_EVENT_REASON_OPEN_INTEREST_CAP),
        b"selfTradeCanceled" => (EV_CANCELED, ORDER_EVENT_REASON_SELF_TRADE),
        b"reduceOnlyCanceled" => (EV_CANCELED, ORDER_EVENT_REASON_REDUCE_ONLY),
        b"scheduledCancel" => (EV_CANCELED, ORDER_EVENT_REASON_SCHEDULED_CANCEL),
        b"tickRejected" => (EV_REJECTED, ORDER_EVENT_REASON_TICK_OR_LOT),
        b"minTradeNtlRejected" => (EV_REJECTED, ORDER_EVENT_REASON_MIN_NOTIONAL),
        b"perpMarginRejected" => (EV_REJECTED, ORDER_EVENT_REASON_MARGIN),
        b"reduceOnlyRejected" => (EV_REJECTED, ORDER_EVENT_REASON_REDUCE_ONLY),
        b"badAloPxRejected" => (EV_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX),
        b"iocCancelRejected" => (EV_REJECTED, REASON_IOC_MISS),
        b"openInterestIncreaseRejected"
        | b"positionIncreaseAtOpenInterestCapRejected"
        | b"positionFlipAtOpenInterestCapRejected"
        | b"tooAggressiveAtOpenInterestCapRejected" => {
            (EV_REJECTED, ORDER_EVENT_REASON_OPEN_INTEREST_CAP)
        }
        b"rejected" => (EV_REJECTED, ORDER_EVENT_REASON_OTHER),
        _ if s.ends_with(b"Rejected") => (EV_REJECTED, ORDER_EVENT_REASON_OTHER),
        _ if s.ends_with(b"Canceled") => (EV_CANCELED, ORDER_EVENT_REASON_OTHER),
        _ => (0, 0),
    }
}

/// Scan one `orderUpdates` frame into `out`; returns the row count.
///
/// # Errors
/// Not an `orderUpdates` frame, a row that does not scan, or more rows
/// than `out` holds — never degraded to "no updates".
pub fn scan_order_updates(body: &[u8], out: &mut [OrderUpdate]) -> Result<usize, ScanErr> {
    let ch = string_field(body, b"\"channel\"").ok_or(ScanErr::Malformed)?;
    if ch.of(body) != b"orderUpdates" {
        return Err(ScanErr::Malformed);
    }
    let mut i = array_start(body, b"\"data\"").ok_or(ScanErr::Malformed)?;
    let mut n = 0usize;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(ScanErr::Malformed);
        }
        match body[i] {
            b']' => return Ok(n),
            b',' => {
                i += 1;
                continue;
            }
            b'{' => {}
            _ => return Err(ScanErr::Malformed),
        }
        let end = object_end(body, i).ok_or(ScanErr::Malformed)?;
        let row = &body[i..end];
        let (os, oe) = object_field(row, b"\"order\"").ok_or(ScanErr::Malformed)?;
        let ord = &row[os..oe];
        let status = string_field(row, b"\"status\"").ok_or(ScanErr::Malformed)?;
        let (kind, reason) = status_of(status.of(row));
        let coin = string_field(ord, b"\"coin\"").ok_or(ScanErr::Malformed)?;
        let is_buy = match string_field(ord, b"\"side\"")
            .ok_or(ScanErr::Malformed)?
            .of(ord)
        {
            b"B" => true,
            b"A" => false,
            _ => return Err(ScanErr::Malformed),
        };
        let oid = u64_field(ord, b"\"oid\"").ok_or(ScanErr::Malformed)?;
        let px_1e8 = decimal_field(ord, b"\"limitPx\"").ok_or(ScanErr::Malformed)?;
        let sz_1e8 = decimal_field(ord, b"\"sz\"").ok_or(ScanErr::Malformed)?;
        let cloid = string_field(ord, b"\"cloid\"").and_then(|s| from_hex(s.of(ord)));
        if n >= out.len() {
            return Err(ScanErr::Malformed);
        }
        let base = (i + os) as u32;
        out[n] = OrderUpdate {
            oid,
            cloid,
            coin: Span {
                start: coin.start + base,
                end: coin.end + base,
            },
            px_1e8,
            sz_1e8,
            status_ms: u64_field(row, b"\"statusTimestamp\"").unwrap_or(0),
            kind,
            reason,
            is_buy,
        };
        n += 1;
        i = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloid;

    #[test]
    fn open_cancel_and_reject_rows_scan_in_place() {
        let body = br#"{"channel":"orderUpdates","data":[{"order":{"coin":"ETH","side":"B","limitPx":"2500.5","sz":"0.1","oid":11,"timestamp":1,"origSz":"0.1","cloid":"0x4d56060000000000000000000000002a"},"status":"open","statusTimestamp":1700},{"order":{"coin":"ETH","side":"A","limitPx":"2501","sz":"0.0","oid":12,"timestamp":1,"origSz":"0.2"},"status":"badAloPxRejected","statusTimestamp":1701},{"order":{"coin":"BTC","side":"A","limitPx":"65000","sz":"0.01","oid":13,"timestamp":1,"origSz":"0.01"},"status":"scheduledCancel","statusTimestamp":1702}]}"#;
        let mut out = [OrderUpdate::default(); 4];
        let n = scan_order_updates(body, &mut out).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            (out[0].kind, out[0].reason),
            (EV_RESTING, ORDER_EVENT_REASON_NONE)
        );
        assert_eq!(out[0].oid, 11);
        assert_eq!(out[0].cloid, Some(cloid::encode(6, 42)));
        assert_eq!(out[0].coin.of(body), b"ETH");
        assert_eq!(out[0].px_1e8, 250_050_000_000);
        assert_eq!(out[0].sz_1e8, 10_000_000);
        assert_eq!(out[0].status_ms, 1700);
        assert!(out[0].is_buy);
        assert_eq!(
            (out[1].kind, out[1].reason),
            (EV_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX)
        );
        assert_eq!(out[1].cloid, None);
        assert!(!out[1].is_buy);
        assert_eq!(
            (out[2].kind, out[2].reason),
            (EV_CANCELED, ORDER_EVENT_REASON_SCHEDULED_CANCEL)
        );
        assert_eq!(out[2].coin.of(body), b"BTC");
    }

    #[test]
    fn statuses_map_by_the_venues_word() {
        assert_eq!(status_of(b"filled"), (EV_FILLED, ORDER_EVENT_REASON_NONE));
        assert_eq!(
            status_of(b"canceled"),
            (EV_CANCELED, ORDER_EVENT_REASON_CANCEL_REQUESTED)
        );
        assert_eq!(
            status_of(b"selfTradeCanceled"),
            (EV_CANCELED, ORDER_EVENT_REASON_SELF_TRADE)
        );
        assert_eq!(
            status_of(b"marginCanceled"),
            (EV_CANCELED, ORDER_EVENT_REASON_MARGIN)
        );
        assert_eq!(
            status_of(b"iocCancelRejected"),
            (EV_REJECTED, REASON_IOC_MISS)
        );
        assert_eq!(
            status_of(b"tickRejected"),
            (EV_REJECTED, ORDER_EVENT_REASON_TICK_OR_LOT)
        );
        assert_eq!(
            status_of(b"positionFlipAtOpenInterestCapRejected"),
            (EV_REJECTED, ORDER_EVENT_REASON_OPEN_INTEREST_CAP)
        );
        assert_eq!(
            status_of(b"oracleRejected"),
            (EV_REJECTED, ORDER_EVENT_REASON_OTHER)
        );
        assert_eq!(
            status_of(b"liquidatedCanceled"),
            (EV_CANCELED, ORDER_EVENT_REASON_OTHER)
        );
        assert_eq!(
            status_of(b"triggered"),
            (0, 0),
            "not reported, never guessed"
        );
    }

    #[test]
    fn a_row_missing_what_routes_it_refuses_the_frame() {
        let mut out = [OrderUpdate::default(); 2];
        let no_oid = br#"{"channel":"orderUpdates","data":[{"order":{"coin":"ETH","side":"B","limitPx":"1","sz":"1"},"status":"open","statusTimestamp":1}]}"#;
        assert_eq!(
            scan_order_updates(no_oid, &mut out),
            Err(ScanErr::Malformed)
        );
        let bad_side = br#"{"channel":"orderUpdates","data":[{"order":{"coin":"ETH","side":"X","limitPx":"1","sz":"1","oid":1},"status":"open","statusTimestamp":1}]}"#;
        assert_eq!(
            scan_order_updates(bad_side, &mut out),
            Err(ScanErr::Malformed)
        );
        let fills = br#"{"channel":"userFills","data":{"fills":[]}}"#;
        assert_eq!(scan_order_updates(fills, &mut out), Err(ScanErr::Malformed));
        let three = br#"{"channel":"orderUpdates","data":[{"order":{"coin":"A","side":"B","limitPx":"1","sz":"1","oid":1},"status":"open"},{"order":{"coin":"A","side":"B","limitPx":"1","sz":"1","oid":2},"status":"open"},{"order":{"coin":"A","side":"B","limitPx":"1","sz":"1","oid":3},"status":"open"}]}"#;
        assert_eq!(
            scan_order_updates(three, &mut out),
            Err(ScanErr::Malformed),
            "more rows than the scratch holds is a refusal, not a truncation"
        );
        let empty = br#"{"channel":"orderUpdates","data":[]}"#;
        assert_eq!(scan_order_updates(empty, &mut out), Ok(0));
    }
}
