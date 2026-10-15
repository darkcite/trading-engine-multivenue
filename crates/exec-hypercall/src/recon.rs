// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Reconciliation: what the VENUE says the wallet holds and has
//! working (plan §6.2 — "the single most valuable safety net").
//!
//! Three public reads, each fail-closed — an unreadable body is an
//! ERROR, never an empty book (an empty book that agrees with an empty
//! engine reconciles by reading nothing at all):
//!
//! * `GET /orders?wallet=&status=open` — `{success, data:[Order…],
//!   pagination}`; each row's `client_id` tells ours from not ours
//!   ([`crate::cloid::decode`]).
//! * `GET /portfolio?wallet=` — `{success, data:{positions:[{symbol,
//!   amount (signed), entry_price…}], available_balance…}, error}`.
//! * `GET /fills?wallet=&limit=` — `{success, data:[{fill_id, symbol,
//!   side, price, size, timestamp, fee…}], pagination}` (newest first).
//!
//! The scanners hand each row to a closure over borrowed spans; the arm
//! decides what agreement means.

use core::ops::Range;

use crate::json::{self, Kind, Val};
use crate::num::{scan_1e6_exact, scan_signed_1e6_exact};

/// Why a read did not reconcile.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ReconErr {
    /// The body is not the envelope the venue documents.
    Unreadable,
    /// `success` was `false` (or the HTTP status was not 200).
    Refused,
}

fn envelope(http: u16, b: &[u8]) -> Result<Val, ReconErr> {
    if http != 200 {
        return Err(ReconErr::Refused);
    }
    let root = json::root(b).ok_or(ReconErr::Unreadable)?;
    match json::field_in(b, &root, b"success").and_then(|v| v.as_bool(b)) {
        Some(true) => Ok(root),
        Some(false) => Err(ReconErr::Refused),
        None => Err(ReconErr::Unreadable),
    }
}

fn str_in<'a>(b: &'a [u8], obj: &Val, k: &[u8]) -> Option<&'a [u8]> {
    json::field_in(b, obj, k)
        .filter(|v| v.kind == Kind::Str)
        .map(|v| &b[v.span])
}

fn side_buy(s: &[u8]) -> Option<bool> {
    match s {
        b"buy" | b"Buy" | b"BUY" => Some(true),
        b"sell" | b"Sell" | b"SELL" => Some(false),
        _ => None,
    }
}

/// One open order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOrder {
    /// The venue's id.
    pub order_id: u64,
    /// Its `client_id` span (possibly empty).
    pub client_id: Range<usize>,
    /// Its instrument's span.
    pub symbol: Range<usize>,
    /// A buy.
    pub buy: bool,
    /// Size ×1e6.
    pub size_1e6: i64,
    /// Filled so far ×1e6.
    pub filled_1e6: i64,
}

/// Walk `GET /orders`: `f(order)` per row; the row count.
///
/// # Errors
///
/// [`ReconErr`] — including any row that does not read.
pub fn scan_orders<F: FnMut(&OpenOrder)>(http: u16, b: &[u8], mut f: F) -> Result<usize, ReconErr> {
    let root = envelope(http, b)?;
    let data = json::field_in(b, &root, b"data")
        .filter(|v| v.kind == Kind::Arr)
        .ok_or(ReconErr::Unreadable)?;
    let mut it = json::items(b, &data);
    let mut n = 0usize;
    while let Some(row) = it.next_item() {
        let row = row.map_err(|_| ReconErr::Unreadable)?;
        if row.kind != Kind::Obj {
            return Err(ReconErr::Unreadable);
        }
        let order_id = json::field_in(b, &row, b"order_id")
            .and_then(|v| v.as_u64(b))
            .ok_or(ReconErr::Unreadable)?;
        let client_id = match json::field_in(b, &row, b"client_id") {
            Some(v) if v.kind == Kind::Str => v.span,
            _ => 0..0,
        };
        let symbol = json::field_in(b, &row, b"symbol")
            .filter(|v| v.kind == Kind::Str)
            .ok_or(ReconErr::Unreadable)?
            .span;
        let buy = str_in(b, &row, b"side").and_then(side_buy).ok_or(ReconErr::Unreadable)?;
        let size_1e6 = str_in(b, &row, b"size")
            .and_then(scan_1e6_exact)
            .ok_or(ReconErr::Unreadable)?;
        let filled_1e6 = match str_in(b, &row, b"filled_size") {
            Some(s) => scan_1e6_exact(s).ok_or(ReconErr::Unreadable)?,
            None => 0,
        };
        f(&OpenOrder {
            order_id,
            client_id,
            symbol,
            buy,
            size_1e6,
            filled_1e6,
        });
        n += 1;
    }
    Ok(n)
}

/// The portfolio's account figures.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct Account {
    /// Free collateral, USD ×1e6.
    pub available_1e6: i64,
    /// Positions listed.
    pub positions: usize,
}

/// Walk `GET /portfolio`: `f(symbol, amount_1e6 (signed), entry_1e6)`
/// per position.
///
/// # Errors
///
/// [`ReconErr`].
pub fn scan_portfolio<F: FnMut(&[u8], i64, i64)>(
    http: u16,
    b: &[u8],
    mut f: F,
) -> Result<Account, ReconErr> {
    let root = envelope(http, b)?;
    let data = json::field_in(b, &root, b"data")
        .filter(|v| v.kind == Kind::Obj)
        .ok_or(ReconErr::Unreadable)?;
    let available_1e6 = match str_in(b, &data, b"available_balance") {
        Some(s) => scan_signed_1e6_exact(s).ok_or(ReconErr::Unreadable)?,
        None => 0,
    };
    let pos = json::field_in(b, &data, b"positions")
        .filter(|v| v.kind == Kind::Arr)
        .ok_or(ReconErr::Unreadable)?;
    let mut it = json::items(b, &pos);
    let mut n = 0usize;
    while let Some(row) = it.next_item() {
        let row = row.map_err(|_| ReconErr::Unreadable)?;
        if row.kind != Kind::Obj {
            return Err(ReconErr::Unreadable);
        }
        let sym = str_in(b, &row, b"symbol").ok_or(ReconErr::Unreadable)?;
        let amount = str_in(b, &row, b"amount")
            .and_then(scan_signed_1e6_exact)
            .ok_or(ReconErr::Unreadable)?;
        let entry = match str_in(b, &row, b"entry_price") {
            Some(s) => scan_signed_1e6_exact(s).ok_or(ReconErr::Unreadable)?,
            None => 0,
        };
        f(sym, amount, entry);
        n += 1;
    }
    Ok(Account {
        available_1e6,
        positions: n,
    })
}

/// One historical fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillRow {
    /// The venue's fill id.
    pub fill_id: u64,
    /// Its instrument's span.
    pub symbol: Range<usize>,
    /// A buy.
    pub buy: bool,
    /// USD per contract ×1e6.
    pub px_1e6: i64,
    /// Contracts ×1e6.
    pub qty_1e6: i64,
    /// Venue time, ms.
    pub ts_ms: u64,
}

/// Walk `GET /fills`: `f(row)` per fill; the row count.
///
/// # Errors
///
/// [`ReconErr`].
pub fn scan_fills<F: FnMut(&FillRow)>(http: u16, b: &[u8], mut f: F) -> Result<usize, ReconErr> {
    let root = envelope(http, b)?;
    let data = json::field_in(b, &root, b"data")
        .filter(|v| v.kind == Kind::Arr)
        .ok_or(ReconErr::Unreadable)?;
    let mut it = json::items(b, &data);
    let mut n = 0usize;
    while let Some(row) = it.next_item() {
        let row = row.map_err(|_| ReconErr::Unreadable)?;
        if row.kind != Kind::Obj {
            return Err(ReconErr::Unreadable);
        }
        let fill_id = json::field_in(b, &row, b"fill_id")
            .and_then(|v| v.as_u64(b))
            .ok_or(ReconErr::Unreadable)?;
        let symbol = json::field_in(b, &row, b"symbol")
            .filter(|v| v.kind == Kind::Str)
            .ok_or(ReconErr::Unreadable)?
            .span;
        let buy = str_in(b, &row, b"side").and_then(side_buy).ok_or(ReconErr::Unreadable)?;
        let px_1e6 = str_in(b, &row, b"price")
            .and_then(scan_1e6_exact)
            .ok_or(ReconErr::Unreadable)?;
        let qty_1e6 = str_in(b, &row, b"size")
            .and_then(scan_1e6_exact)
            .ok_or(ReconErr::Unreadable)?;
        let ts_ms = json::field_in(b, &row, b"timestamp")
            .and_then(|v| v.as_u64(b))
            .ok_or(ReconErr::Unreadable)?;
        f(&FillRow {
            fill_id,
            symbol,
            buy,
            px_1e6,
            qty_1e6,
            ts_ms,
        });
        n += 1;
    }
    Ok(n)
}

/// USD ×1e6 of `qty_1e6` contracts at `px_1e6` (saturating).
#[inline]
#[must_use]
pub fn notional_1e6(qty_1e6: i64, px_1e6: i64) -> i64 {
    i64::try_from((i128::from(qty_1e6).abs() * i128::from(px_1e6.max(0))) / 1_000_000)
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_orders_read_and_a_bad_row_refuses_the_whole() {
        let b = br#"{"success":true,"data":[{"order_id":1,"wallet_address":"0x1","symbol":"BTC-20261002-100000-C","side":"buy","price":"0.0005","size":"0.000001","created_at":1,"client_id":"4843","reduce_only":false,"instrument_type":"option","filled_size":"0","status":"open"}],"pagination":{"limit":100,"offset":0,"count":1}}"#;
        let mut seen = 0u64;
        let n = scan_orders(200, b, |o| {
            seen = o.order_id;
            assert!(o.buy);
            assert_eq!(o.size_1e6, 1);
            assert_eq!(&b[o.client_id.clone()], b"4843");
        })
        .unwrap();
        assert_eq!((n, seen), (1, 1));
        let bad = br#"{"success":true,"data":[{"order_id":1,"symbol":"X","side":"up","size":"1"}],"pagination":{}}"#;
        assert_eq!(scan_orders(200, bad, |_| {}), Err(ReconErr::Unreadable));
        assert_eq!(scan_orders(200, br#"{"success":true}"#, |_| {}), Err(ReconErr::Unreadable));
        assert_eq!(scan_orders(500, b"", |_| {}), Err(ReconErr::Refused));
        let empty = br#"{"success":true,"data":[],"pagination":{"limit":100,"offset":0,"count":0}}"#;
        assert_eq!(scan_orders(200, empty, |_| {}), Ok(0));
    }

    #[test]
    fn the_portfolio_reads_signed_amounts() {
        let b = br#"{"success":true,"data":{"wallet_address":"0x1","positions":[{"symbol":"MU-20261002-1080-P","amount":"-0.5","entry_price":"12.5","margin_posted":"1","realized_pnl":"0","unrealized_pnl":"0","updated_at":"x","wallet_address":"0x1"}],"total_margin_used":"1","available_balance":"4.25","portfolio_snapshot_timestamp_ms":1,"margin_mode":"standard"},"error":null}"#;
        let mut got = (0i64, 0i64);
        let a = scan_portfolio(200, b, |s, amt, entry| {
            assert_eq!(s, b"MU-20261002-1080-P");
            got = (amt, entry);
        })
        .unwrap();
        assert_eq!(got, (-500_000, 12_500_000));
        assert_eq!(a, Account { available_1e6: 4_250_000, positions: 1 });
        let refused = br#"{"success":false,"data":null,"error":"x"}"#;
        assert_eq!(scan_portfolio(200, refused, |_, _, _| {}), Err(ReconErr::Refused));
    }

    #[test]
    fn fills_read_both_side_spellings() {
        let b = br#"{"success":true,"data":[{"fill_id":9,"trade_id":1,"wallet_address":"0x1","symbol":"X-1","price":"2","size":"0.5","fee":"0","side":"Sell","is_taker":true,"timestamp":1767225600000,"created_at":"x","instrument_type":"option"}],"pagination":{"limit":1,"offset":0,"count":1}}"#;
        let mut row = None;
        assert_eq!(scan_fills(200, b, |r| row = Some(r.clone())), Ok(1));
        let r = row.unwrap();
        assert_eq!((r.fill_id, r.buy, r.px_1e6, r.qty_1e6), (9, false, 2_000_000, 500_000));
        assert_eq!(notional_1e6(r.qty_1e6, r.px_1e6), 1_000_000);
    }
}
