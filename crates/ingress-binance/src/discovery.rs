// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Binance boot-time REST discovery (M1, mvp-plan §4-M1 "discovery
//! audit"; the 8e §6.1 pattern applied to Binance)
//!
//! Parses `exchangeInfo` bodies into a boot-only symbol table:
//!
//! - spot `GET /api/v3/exchangeInfo?symbol=<UPPER>` — one body per
//!   configured symbol (the venue 400s unknown symbols, which the cli
//!   maps to MISSING rather than fatal);
//! - USDS-M `GET /fapi/v1/exchangeInfo` — one body listing the whole
//!   futures universe.
//!
//! Both shapes share the `"symbols":[{…}]` array; each symbol object
//! carries `"symbol"` and `"status"` among many other fields (nested
//! `filters`/`permissionSets` arrays included) which the walker skips
//! structurally ([`core_parse::skip_json_value`]). Field order is not
//! assumed.
//!
//! ## Allocation note (doctrine)
//!
//! Boot only — allocation allowed. Row storage is one `Vec` reserved
//! at [`BnDiscovery::new`], capped at [`BN_DISCOVERY_ROWS_CAP`]
//! (fail-fast beyond). The table drops before the engine loop starts;
//! nothing here is reachable from a hot path.

use core_parse::{find_field, skip_json_value, skip_string, skip_ws};

/// Hard cap on parsed symbol rows across all ingested bodies. Live
/// USDS-M universe ≈ 500 symbols; spot probes add one row each; 16×
/// headroom.
pub const BN_DISCOVERY_ROWS_CAP: usize = 8_192;

/// Longest venue symbol we accept (`BTCUSDT_260327` delivery names
/// included).
pub const BN_DISCOVERY_SYMBOL_MAX: usize = 32;

/// One discovered symbol.
#[derive(Copy, Clone, Debug)]
pub struct BnSymbolRow {
    /// Venue symbol bytes, UPPERCASE (`symbol_len` valid).
    pub symbol: [u8; BN_DISCOVERY_SYMBOL_MAX],
    /// Valid prefix length of `symbol`.
    pub symbol_len: u8,
    /// `status == "TRADING"` (anything else is not subscribable).
    pub trading: bool,
}

impl BnSymbolRow {
    /// The venue symbol as a byte slice.
    #[inline]
    pub fn symbol(&self) -> &[u8] {
        &self.symbol[..self.symbol_len as usize]
    }
}

/// Why discovery ingestion failed. All fatal at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BnDiscoveryErr {
    /// Missing `"symbols":[` array.
    Envelope,
    /// A row violated the symbol-object contract (missing key,
    /// over-long symbol, malformed value).
    BadRow,
    /// Body ended inside the `symbols` array.
    Truncated,
    /// More than [`BN_DISCOVERY_ROWS_CAP`] rows across all bodies.
    TooMany,
}

/// Boot-only Binance symbol table. See module docs.
pub struct BnDiscovery {
    rows: Vec<BnSymbolRow>,
    universe_trading: u32,
}

impl BnDiscovery {
    /// Empty table with capacity reserved once.
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(BN_DISCOVERY_ROWS_CAP),
            universe_trading: 0,
        }
    }

    /// Parse one `exchangeInfo` body (spot single-symbol or the full
    /// USDS-M page) into the table. Returns the number of rows added;
    /// counts accumulate across calls.
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, BnDiscoveryErr> {
        let sym_pos = find_field(body, b"\"symbols\":").ok_or(BnDiscoveryErr::Envelope)?;
        let mut i = skip_ws(body, sym_pos);
        if i >= body.len() || body[i] != b'[' {
            return Err(BnDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(BnDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b'{' => {
                    let (row, end) = parse_row(body, i)?;
                    if self.rows.len() >= BN_DISCOVERY_ROWS_CAP {
                        return Err(BnDiscoveryErr::TooMany);
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
                _ => return Err(BnDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Look up a discovered symbol by exact UPPERCASE bytes.
    pub fn find(&self, symbol_upper: &[u8]) -> Option<&BnSymbolRow> {
        self.rows.iter().find(|r| r.symbol() == symbol_upper)
    }

    /// Total rows parsed (all statuses).
    #[inline]
    pub fn universe_total(&self) -> u32 {
        self.rows.len() as u32
    }

    /// Rows with `status == "TRADING"` — the §6.1 coverage-report
    /// `universe=` figure.
    #[inline]
    pub fn universe_trading(&self) -> u32 {
        self.universe_trading
    }
}

impl Default for BnDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse one symbol object starting at `pos` (must point at `{`).
/// Returns the row and the position after the closing `}`.
fn parse_row(body: &[u8], pos: usize) -> Result<(BnSymbolRow, usize), BnDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut symbol = [0u8; BN_DISCOVERY_SYMBOL_MAX];
    let mut symbol_len = 0u8;
    let mut trading: Option<bool> = None;

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(BnDiscoveryErr::Truncated);
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
                let key_end_q = skip_string(body, key_start).ok_or(BnDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                if i >= body.len() || body[i] != b':' {
                    return Err(BnDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                match key {
                    b"symbol" => {
                        let (s, end) = quoted_span(body, i)?;
                        if s.is_empty() || s.len() > BN_DISCOVERY_SYMBOL_MAX {
                            return Err(BnDiscoveryErr::BadRow);
                        }
                        symbol[..s.len()].copy_from_slice(s);
                        symbol_len = s.len() as u8;
                        i = end;
                    }
                    b"status" => {
                        let (s, end) = quoted_span(body, i)?;
                        trading = Some(s == b"TRADING");
                        i = end;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(BnDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(BnDiscoveryErr::BadRow),
        }
    }

    if symbol_len == 0 {
        return Err(BnDiscoveryErr::BadRow);
    }
    let trading = trading.ok_or(BnDiscoveryErr::BadRow)?;
    Ok((
        BnSymbolRow {
            symbol,
            symbol_len,
            trading,
        },
        i,
    ))
}

/// Read a quoted string value at `pos`. Returns the in-quote span and
/// the position after the closing quote. The captured fields never
/// contain escapes; a backslash is rejected rather than unescaped.
fn quoted_span(body: &[u8], pos: usize) -> Result<(&[u8], usize), BnDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'"' {
        return Err(BnDiscoveryErr::BadRow);
    }
    let start = pos + 1;
    let end_q = skip_string(body, start).ok_or(BnDiscoveryErr::Truncated)?;
    let span = &body[start..end_q - 1];
    if span.contains(&b'\\') {
        return Err(BnDiscoveryErr::BadRow);
    }
    Ok((span, end_q))
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed spot single-symbol shape: the noise fields the walker
    /// must skip (numbers, bools, nested object arrays) retained.
    const SPOT_ONE: &[u8] = br#"{"timezone":"UTC","serverTime":1787000000000,"rateLimits":[{"rateLimitType":"REQUEST_WEIGHT","limit":6000}],"exchangeFilters":[],"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","baseAssetPrecision":8,"quoteAsset":"USDT","isSpotTradingAllowed":true,"filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.00001"}],"permissionSets":[["SPOT","MARGIN"]]}]}"#;

    /// Trimmed USDS-M page shape: perpetual + delivery + a halted row.
    const FAPI_PAGE: &[u8] = br#"{"timezone":"UTC","serverTime":1787000000001,"futuresType":"U_MARGINED","rateLimits":[],"exchangeFilters":[],"assets":[{"asset":"USDT","marginAvailable":true}],"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","deliveryDate":4133404800000,"status":"TRADING","maintMarginPercent":"2.5","filters":[{"filterType":"PRICE_FILTER"}]},{"symbol":"BTCUSDT_260327","pair":"BTCUSDT","contractType":"CURRENT_QUARTER","status":"TRADING"},{"symbol":"OLDCOIN","pair":"OLDCOIN","contractType":"PERPETUAL","status":"SETTLING"}]}"#;

    #[test]
    fn spot_single_symbol_body_parses() {
        let mut d = BnDiscovery::new();
        assert_eq!(d.ingest_body(SPOT_ONE).expect("parse ok"), 1);
        let row = d.find(b"BTCUSDT").expect("row");
        assert!(row.trading);
        assert_eq!(d.universe_total(), 1);
        assert_eq!(d.universe_trading(), 1);
    }

    #[test]
    fn fapi_page_parses_and_counts_trading_universe() {
        let mut d = BnDiscovery::new();
        assert_eq!(d.ingest_body(FAPI_PAGE).expect("parse ok"), 3);
        assert_eq!(d.universe_total(), 3);
        assert_eq!(d.universe_trading(), 2, "SETTLING row is not tradable");
        assert!(d.find(b"BTCUSDT").unwrap().trading);
        assert!(d.find(b"BTCUSDT_260327").unwrap().trading);
        assert!(!d.find(b"OLDCOIN").unwrap().trading);
        assert!(d.find(b"MISSING").is_none());
    }

    #[test]
    fn bodies_accumulate_across_calls() {
        let mut d = BnDiscovery::new();
        d.ingest_body(SPOT_ONE).unwrap();
        d.ingest_body(FAPI_PAGE).unwrap();
        assert_eq!(d.universe_total(), 4);
    }

    #[test]
    fn missing_symbols_array_is_envelope_error() {
        let mut d = BnDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"timezone":"UTC"}"#).unwrap_err(),
            BnDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_body(br#"{"symbols":{}}"#).unwrap_err(),
            BnDiscoveryErr::Envelope
        );
    }

    #[test]
    fn row_contract_violations_rejected() {
        let mut d = BnDiscovery::new();
        // Missing symbol.
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"status":"TRADING"}]}"#).unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Missing status.
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"symbol":"BTCUSDT"}]}"#).unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Over-long symbol (33 > 32).
        assert_eq!(
            d.ingest_body(
                br#"{"symbols":[{"symbol":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","status":"TRADING"}]}"#
            )
            .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Escape inside a captured field.
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"symbol":"BTC\USDT","status":"TRADING"}]}"#)
                .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
    }

    #[test]
    fn truncated_array_is_rejected() {
        let mut d = BnDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"symbol":"BTCUSDT""#).unwrap_err(),
            BnDiscoveryErr::Truncated
        );
        assert_eq!(
            d.ingest_body(br#"{"symbols":["#).unwrap_err(),
            BnDiscoveryErr::Truncated
        );
    }

    #[test]
    fn rows_cap_enforced() {
        let mut d = BnDiscovery::new();
        let row = br#"{"symbol":"AAAUSDT","status":"TRADING"}"#;
        let per_page = 1024usize;
        let mut body = Vec::with_capacity(1 << 16);
        body.extend_from_slice(br#"{"symbols":["#);
        for k in 0..per_page {
            if k > 0 {
                body.push(b',');
            }
            body.extend_from_slice(row);
        }
        body.extend_from_slice(b"]}");
        for _ in 0..(BN_DISCOVERY_ROWS_CAP / per_page) {
            d.ingest_body(&body).expect("under cap");
        }
        assert_eq!(d.ingest_body(&body).unwrap_err(), BnDiscoveryErr::TooMany);
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
            let mut d = BnDiscovery::new();
            match d.ingest_body(&input) {
                Ok(n) => {
                    prop_assert_eq!(n, d.universe_total());
                    prop_assert!(d.universe_trading() <= d.universe_total());
                }
                Err(_) => {}
            }
        }
    }
}
