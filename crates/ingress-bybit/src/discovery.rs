// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Bybit boot-time REST discovery (WS9 — the 8e §6.1 pattern)
//!
//! Parses `GET /v5/market/instruments-info?category=spot|linear`
//! bodies into a boot-only symbol table: liveness
//! (`status == "Trading"`) + static metadata (`priceFilter.tickSize`,
//! `lotSizeFilter.qtyStep` on linear / `basePrecision` on spot — the
//! WS4 tick/lot parity line). The endpoint pages via
//! `nextPageCursor`; [`next_page_cursor`] hands the caller the
//! cursor to append (`&cursor=…`) until it comes back empty.
//!
//! ## Allocation note (doctrine)
//!
//! Boot only — allocation allowed. Row storage is one `Vec` reserved
//! at [`BybitDiscovery::new`], capped at
//! [`BYBIT_DISCOVERY_ROWS_CAP`] (fail-fast beyond). The table drops
//! before the engine loop starts; nothing here is reachable from a
//! hot path.

use core_parse::{find_field, skip_json_value, skip_string, skip_ws};

/// Hard cap on parsed rows across all ingested pages. Live linear
/// universe ≈ 500, spot ≈ 700; 8× headroom.
pub const BYBIT_DISCOVERY_ROWS_CAP: usize = 8_192;

/// Longest venue symbol accepted.
pub const BYBIT_DISCOVERY_SYMBOL_MAX: usize = 24;

/// One discovered instrument.
#[derive(Copy, Clone, Debug)]
pub struct BybitInstrumentRow {
    /// Venue symbol bytes, UPPERCASE (`symbol_len` valid).
    pub symbol: [u8; BYBIT_DISCOVERY_SYMBOL_MAX],
    /// Valid prefix length of `symbol`.
    pub symbol_len: u8,
    /// `status == "Trading"`.
    pub trading: bool,
    /// `priceFilter.tickSize` ×1e9 (0 = absent).
    pub tick_size_1e9: i64,
    /// Lot step ×1e9: `lotSizeFilter.qtyStep` (linear) or
    /// `basePrecision` (spot); 0 = absent.
    pub lot_step_1e9: i64,
}

impl BybitInstrumentRow {
    /// The venue symbol as a byte slice.
    #[inline]
    pub fn symbol(&self) -> &[u8] {
        &self.symbol[..self.symbol_len as usize]
    }
}

/// Why discovery ingestion failed. All fatal at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BybitDiscoveryErr {
    /// `retCode` ≠ 0, or the `result.list` array is missing.
    Envelope,
    /// A row violated the instrument-object contract.
    BadRow,
    /// Body ended inside the `list` array.
    Truncated,
    /// More than [`BYBIT_DISCOVERY_ROWS_CAP`] rows across all pages.
    TooMany,
}

/// Boot-only Bybit instrument table. See module docs.
pub struct BybitDiscovery {
    rows: Vec<BybitInstrumentRow>,
    universe_trading: u32,
}

impl BybitDiscovery {
    /// Empty table with capacity reserved once.
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(BYBIT_DISCOVERY_ROWS_CAP),
            universe_trading: 0,
        }
    }

    /// Parse one `instruments-info` page into the table. Returns the
    /// number of rows added; counts accumulate across pages and
    /// categories (symbols are category-qualified by the CALLER'S
    /// query — the audit resolves spot and linear separately against
    /// separately-ingested tables when both classes are configured).
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, BybitDiscoveryErr> {
        // Envelope: `"retCode":0`.
        let rc_pos = find_field(body, b"\"retCode\":").ok_or(BybitDiscoveryErr::Envelope)?;
        let rc = skip_ws(body, rc_pos);
        if body.get(rc) != Some(&b'0') {
            return Err(BybitDiscoveryErr::Envelope);
        }
        let list_pos = find_field(body, b"\"list\":").ok_or(BybitDiscoveryErr::Envelope)?;
        let mut i = skip_ws(body, list_pos);
        if i >= body.len() || body[i] != b'[' {
            return Err(BybitDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(BybitDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b'{' => {
                    let (row, end) = parse_row(body, i)?;
                    if self.rows.len() >= BYBIT_DISCOVERY_ROWS_CAP {
                        return Err(BybitDiscoveryErr::TooMany);
                    }
                    if row.trading {
                        self.universe_trading += 1;
                    }
                    self.rows.push(row);
                    added += 1;
                    i = skip_ws(body, end);
                    if i < body.len() && body[i] == b',' {
                        i += 1;
                    }
                }
                _ => return Err(BybitDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Look up a discovered symbol by exact UPPERCASE bytes.
    pub fn find(&self, symbol_upper: &[u8]) -> Option<&BybitInstrumentRow> {
        self.rows.iter().find(|r| r.symbol() == symbol_upper)
    }

    /// Total rows parsed (all statuses).
    #[inline]
    pub fn universe_total(&self) -> u32 {
        self.rows.len() as u32
    }

    /// Rows with `status == "Trading"` — the §6.1 coverage-report
    /// `universe=` figure.
    #[inline]
    pub fn universe_trading(&self) -> u32 {
        self.universe_trading
    }
}

impl Default for BybitDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// The page's `nextPageCursor` value bytes, `None` when absent or
/// empty (= last page). A subslice of `body`; the caller copies it
/// into its next request URL.
pub fn next_page_cursor(body: &[u8]) -> Option<&[u8]> {
    let pos = find_field(body, b"\"nextPageCursor\":")?;
    let rest = body.get(pos..)?;
    if !rest.starts_with(b"\"") {
        return None;
    }
    let start = pos + 1;
    let rel_end = memchr::memchr(b'"', body.get(start..)?)?;
    if rel_end == 0 {
        return None; // empty cursor = last page
    }
    body.get(start..start + rel_end)
}

/// Parse one instrument object starting at `pos` (must point at
/// `{`). Returns the row and the position after the closing `}`.
fn parse_row(body: &[u8], pos: usize) -> Result<(BybitInstrumentRow, usize), BybitDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut symbol = [0u8; BYBIT_DISCOVERY_SYMBOL_MAX];
    let mut symbol_len = 0u8;
    let mut trading: Option<bool> = None;
    let mut tick_size_1e9 = 0i64;
    let mut lot_step_1e9 = 0i64;

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(BybitDiscoveryErr::Truncated);
        }
        match body[i] {
            b'}' => {
                i += 1;
                break;
            }
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {
                let key_start = i + 1;
                let key_end_q = skip_string(body, key_start).ok_or(BybitDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                if i >= body.len() || body[i] != b':' {
                    return Err(BybitDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                match key {
                    b"symbol" => {
                        let (s, end) = quoted_span(body, i)?;
                        if s.is_empty() || s.len() > BYBIT_DISCOVERY_SYMBOL_MAX {
                            return Err(BybitDiscoveryErr::BadRow);
                        }
                        symbol[..s.len()].copy_from_slice(s);
                        symbol_len = s.len() as u8;
                        i = end;
                    }
                    b"status" => {
                        let (s, end) = quoted_span(body, i)?;
                        trading = Some(s == b"Trading");
                        i = end;
                    }
                    b"priceFilter" => {
                        i = parse_filter_obj(body, i, &mut [(b"tickSize", &mut tick_size_1e9)])?;
                    }
                    b"lotSizeFilter" => {
                        // Linear pages carry `qtyStep`; spot pages
                        // `basePrecision` — first present wins.
                        let mut qty_step = 0i64;
                        let mut base_prec = 0i64;
                        i = parse_filter_obj(
                            body,
                            i,
                            &mut [
                                (b"qtyStep", &mut qty_step),
                                (b"basePrecision", &mut base_prec),
                            ],
                        )?;
                        lot_step_1e9 = if qty_step != 0 { qty_step } else { base_prec };
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(BybitDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(BybitDiscoveryErr::BadRow),
        }
    }

    if symbol_len == 0 {
        return Err(BybitDiscoveryErr::BadRow);
    }
    let trading = trading.ok_or(BybitDiscoveryErr::BadRow)?;
    Ok((
        BybitInstrumentRow {
            symbol,
            symbol_len,
            trading,
            tick_size_1e9,
            lot_step_1e9,
        },
        i,
    ))
}

/// Walk one nested filter OBJECT capturing the named quoted-decimal
/// keys (×1e9); everything else skips structurally. `pos` points at
/// (whitespace before) `{`; returns the position after `}`.
fn parse_filter_obj(
    body: &[u8],
    pos: usize,
    wants: &mut [(&[u8], &mut i64)],
) -> Result<usize, BybitDiscoveryErr> {
    let mut i = skip_ws(body, pos);
    if i >= body.len() || body[i] != b'{' {
        return Err(BybitDiscoveryErr::BadRow);
    }
    i += 1;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(BybitDiscoveryErr::Truncated);
        }
        match body[i] {
            b'}' => return Ok(i + 1),
            b',' => {
                i += 1;
            }
            b'"' => {
                let key_start = i + 1;
                let key_end_q = skip_string(body, key_start).ok_or(BybitDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                if i >= body.len() || body[i] != b':' {
                    return Err(BybitDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                let mut matched = false;
                for (want, slot) in wants.iter_mut() {
                    if key == *want {
                        let (v, end) = quoted_1e9(body, i)?;
                        **slot = v;
                        i = end;
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    i = skip_json_value(body, i).ok_or(BybitDiscoveryErr::BadRow)?;
                }
            }
            _ => return Err(BybitDiscoveryErr::BadRow),
        }
    }
}

/// Read a quoted string value at `pos`. No escapes accepted.
fn quoted_span(body: &[u8], pos: usize) -> Result<(&[u8], usize), BybitDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'"' {
        return Err(BybitDiscoveryErr::BadRow);
    }
    let start = pos + 1;
    let end_q = skip_string(body, start).ok_or(BybitDiscoveryErr::Truncated)?;
    let span = &body[start..end_q - 1];
    if span.contains(&b'\\') {
        return Err(BybitDiscoveryErr::BadRow);
    }
    Ok((span, end_q))
}

/// Parse a QUOTED non-negative decimal into ×1e9 fixed point (the
/// WS4 Binance-discovery rule: fraction digits beyond 9 must be
/// zero).
fn quoted_1e9(body: &[u8], pos: usize) -> Result<(i64, usize), BybitDiscoveryErr> {
    let (span, end) = quoted_span(body, pos)?;
    if span.is_empty() {
        return Err(BybitDiscoveryErr::BadRow);
    }
    let mut int_part = 0i64;
    let mut frac = 0i64;
    let mut frac_digits = 0u32;
    let mut seen_dot = false;
    let mut seen_digit = false;
    let mut k = 0usize;
    while k < span.len() {
        let b = span[k];
        match b {
            b'0'..=b'9' => {
                seen_digit = true;
                let d = (b - b'0') as i64;
                if seen_dot {
                    if frac_digits < 9 {
                        frac = frac * 10 + d;
                        frac_digits += 1;
                    } else if d != 0 {
                        return Err(BybitDiscoveryErr::BadRow);
                    }
                } else {
                    int_part = int_part
                        .checked_mul(10)
                        .and_then(|v| v.checked_add(d))
                        .ok_or(BybitDiscoveryErr::BadRow)?;
                }
            }
            b'.' if !seen_dot => seen_dot = true,
            _ => return Err(BybitDiscoveryErr::BadRow),
        }
        k += 1;
    }
    if !seen_digit {
        return Err(BybitDiscoveryErr::BadRow);
    }
    let mut scale = frac;
    let mut pad = frac_digits;
    while pad < 9 {
        scale *= 10;
        pad += 1;
    }
    let v = int_part
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(scale))
        .ok_or(BybitDiscoveryErr::BadRow)?;
    Ok((v, end))
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed real linear-page shape (nested filters + noise the
    /// walker must skip).
    const LINEAR_PAGE: &[u8] = br#"{"retCode":0,"retMsg":"OK","result":{"category":"linear","list":[{"symbol":"BTCUSDT","contractType":"LinearPerpetual","status":"Trading","baseCoin":"BTC","quoteCoin":"USDT","launchTime":"1584230400000","deliveryTime":"0","deliveryFeeRate":"","priceScale":"2","leverageFilter":{"minLeverage":"1","maxLeverage":"100.00","leverageStep":"0.01"},"priceFilter":{"minPrice":"0.10","maxPrice":"199999.80","tickSize":"0.10"},"lotSizeFilter":{"maxOrderQty":"1190.000","minOrderQty":"0.001","qtyStep":"0.001","postOnlyMaxOrderQty":"1190.000"},"unifiedMarginTrade":true,"fundingInterval":480,"settleCoin":"USDT"},{"symbol":"OLDUSDT","contractType":"LinearPerpetual","status":"Closed","priceFilter":{"tickSize":"0.01"},"lotSizeFilter":{"qtyStep":"1"}}],"nextPageCursor":"abc%3D%3D"},"retExtInfo":{},"time":1672712495660}"#;

    const SPOT_PAGE: &[u8] = br#"{"retCode":0,"retMsg":"OK","result":{"category":"spot","list":[{"symbol":"BTCUSDT","baseCoin":"BTC","quoteCoin":"USDT","innovation":"0","status":"Trading","marginTrading":"both","lotSizeFilter":{"basePrecision":"0.000001","quotePrecision":"0.00000001","minOrderQty":"0.000048","maxOrderQty":"71.73956243"},"priceFilter":{"tickSize":"0.01"}}],"nextPageCursor":""},"retExtInfo":{},"time":1}"#;

    #[test]
    fn linear_page_parses_rows_metadata_and_cursor() {
        let mut d = BybitDiscovery::new();
        assert_eq!(d.ingest_body(LINEAR_PAGE).expect("parse ok"), 2);
        assert_eq!(d.universe_total(), 2);
        assert_eq!(d.universe_trading(), 1, "Closed row not tradable");
        let row = d.find(b"BTCUSDT").expect("row");
        assert!(row.trading);
        assert_eq!(row.tick_size_1e9, 100_000_000, "0.10 ×1e9");
        assert_eq!(row.lot_step_1e9, 1_000_000, "qtyStep 0.001 ×1e9");
        assert!(!d.find(b"OLDUSDT").unwrap().trading);
        assert!(d.find(b"NOPE").is_none());
        assert_eq!(next_page_cursor(LINEAR_PAGE), Some(&b"abc%3D%3D"[..]));
    }

    #[test]
    fn spot_page_lot_falls_back_to_base_precision() {
        let mut d = BybitDiscovery::new();
        assert_eq!(d.ingest_body(SPOT_PAGE).expect("parse ok"), 1);
        let row = d.find(b"BTCUSDT").unwrap();
        assert_eq!(row.tick_size_1e9, 10_000_000, "0.01 ×1e9");
        assert_eq!(row.lot_step_1e9, 1_000, "basePrecision 0.000001 ×1e9");
        assert_eq!(
            next_page_cursor(SPOT_PAGE),
            None,
            "empty cursor = last page"
        );
    }

    #[test]
    fn envelope_violations_reject() {
        let mut d = BybitDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"retCode":10001,"retMsg":"bad","result":{}}"#)
                .unwrap_err(),
            BybitDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_body(br#"{"retCode":0,"result":{}}"#).unwrap_err(),
            BybitDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_body(br#"{"retCode":0,"result":{"list":[{"symbol":"X""#)
                .unwrap_err(),
            BybitDiscoveryErr::Truncated
        );
        // Row contract: status required.
        assert_eq!(
            d.ingest_body(br#"{"retCode":0,"result":{"list":[{"symbol":"X"}]}}"#)
                .unwrap_err(),
            BybitDiscoveryErr::BadRow
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The discovery parser never panics on arbitrary bytes and,
        /// on success, internal counts stay consistent.
        #[test]
        fn ingest_never_panics(input in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut d = BybitDiscovery::new();
            if let Ok(n) = d.ingest_body(&input) {
                prop_assert_eq!(n, d.universe_total());
                prop_assert!(d.universe_trading() <= d.universe_total());
            }
            let _ = next_page_cursor(&input);
        }
    }
}
