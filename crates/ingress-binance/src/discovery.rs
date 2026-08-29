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
    /// WS4 (gaps §1 tick/lot): `PRICE_FILTER.tickSize` ×1e9
    /// (0 = filter/field absent — the row parses either way; spot
    /// and USDS-M share the filter shape). Brings Binance up to the
    /// OKX/Deribit/PM static-metadata parity line.
    pub tick_size_1e9: i64,
    /// WS4: `LOT_SIZE.stepSize` ×1e9 (0 = filter/field absent).
    pub lot_step_1e9: i64,
    /// WS5 (gaps §2.1 dated futures): the USDS-M `contractType`
    /// class ([`BnContractType::None`] on spot bodies, which carry no
    /// such field).
    pub contract_type: BnContractType,
    /// WS5: `deliveryDate` ms since epoch (0 = absent; Binance uses
    /// a far-future sentinel ~2100 on perpetuals).
    pub delivery_ms: i64,
}

/// WS5: USDS-M `contractType` classes (spot rows carry none).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BnContractType {
    /// Field absent (spot exchangeInfo).
    None = 0,
    /// `PERPETUAL`.
    Perpetual = 1,
    /// `CURRENT_QUARTER` — the front dated future.
    CurrentQuarter = 2,
    /// `NEXT_QUARTER` — the back dated future.
    NextQuarter = 3,
    /// Any other value (venue classes drift; named at the audit, not
    /// fatal).
    Other = 4,
}

impl BnContractType {
    /// WS5: true for the dated (delivery) classes.
    #[inline]
    pub fn is_dated(self) -> bool {
        matches!(self, Self::CurrentQuarter | Self::NextQuarter | Self::Other)
    }
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
    let mut tick_size_1e9 = 0i64;
    let mut lot_step_1e9 = 0i64;
    let mut contract_type = BnContractType::None;
    let mut delivery_ms = 0i64;

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
                    b"filters" => {
                        // WS4: walk the filters array for
                        // PRICE_FILTER.tickSize / LOT_SIZE.stepSize;
                        // everything else skips structurally. Absent
                        // filters leave the 0 defaults (old fixtures
                        // keep parsing).
                        i = parse_filters(body, i, &mut tick_size_1e9, &mut lot_step_1e9)?;
                    }
                    b"contractType" => {
                        // WS5: USDS-M contract class (dated-future
                        // semantics; unknown values are Other, never
                        // fatal — venue classes drift).
                        let (s, end) = quoted_span(body, i)?;
                        contract_type = match s {
                            b"PERPETUAL" => BnContractType::Perpetual,
                            b"CURRENT_QUARTER" => BnContractType::CurrentQuarter,
                            b"NEXT_QUARTER" => BnContractType::NextQuarter,
                            _ => BnContractType::Other,
                        };
                        i = end;
                    }
                    b"deliveryDate" => {
                        // WS5: bare ms integer (perpetuals carry a
                        // far-future sentinel).
                        let (v, end) = bare_u64(body, i)?;
                        if v > i64::MAX as u64 {
                            return Err(BnDiscoveryErr::BadRow);
                        }
                        delivery_ms = v as i64;
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
            tick_size_1e9,
            lot_step_1e9,
            contract_type,
            delivery_ms,
        },
        i,
    ))
}

/// WS5: parse a bare (unquoted) non-negative integer value at `pos`.
fn bare_u64(body: &[u8], pos: usize) -> Result<(u64, usize), BnDiscoveryErr> {
    let mut i = pos;
    let mut v: u64 = 0;
    let mut seen = false;
    while i < body.len() && body[i].is_ascii_digit() {
        v = v
            .checked_mul(10)
            .and_then(|x| x.checked_add((body[i] - b'0') as u64))
            .ok_or(BnDiscoveryErr::BadRow)?;
        seen = true;
        i += 1;
    }
    if !seen {
        return Err(BnDiscoveryErr::BadRow);
    }
    Ok((v, i))
}

/// WS4: walk one `"filters":[…]` array. `pos` points at (whitespace
/// before) `[`; returns the position after the closing `]`. Captures
/// `PRICE_FILTER.tickSize` and `LOT_SIZE.stepSize` (quoted decimal
/// strings on both spot and USDS-M); all other filter objects and
/// fields skip structurally. Field order inside a filter object is
/// not assumed.
fn parse_filters(
    body: &[u8],
    pos: usize,
    tick_size_1e9: &mut i64,
    lot_step_1e9: &mut i64,
) -> Result<usize, BnDiscoveryErr> {
    let mut i = skip_ws(body, pos);
    if i >= body.len() || body[i] != b'[' {
        return Err(BnDiscoveryErr::BadRow);
    }
    i += 1;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(BnDiscoveryErr::Truncated);
        }
        match body[i] {
            b']' => return Ok(i + 1),
            b',' => {
                i += 1;
            }
            b'{' => {
                i += 1;
                // Per-object accumulation: order-independent.
                const FILTER_TYPE_MAX: usize = 32;
                let mut ftype = [0u8; FILTER_TYPE_MAX];
                let mut ftype_len = 0usize;
                let mut tick: Option<i64> = None;
                let mut step: Option<i64> = None;
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
                        }
                        b'"' => {
                            let key_start = i + 1;
                            let key_end_q =
                                skip_string(body, key_start).ok_or(BnDiscoveryErr::Truncated)?;
                            let key = &body[key_start..key_end_q - 1];
                            i = skip_ws(body, key_end_q);
                            if i >= body.len() {
                                // End-of-buffer mid-row is a pagination
                                // truncation, not a malformed row (the
                                // convention above).
                                return Err(BnDiscoveryErr::Truncated);
                            }
                            if body[i] != b':' {
                                return Err(BnDiscoveryErr::BadRow);
                            }
                            i = skip_ws(body, i + 1);
                            match key {
                                b"filterType" => {
                                    let (s, end) = quoted_span(body, i)?;
                                    if s.len() > FILTER_TYPE_MAX {
                                        return Err(BnDiscoveryErr::BadRow);
                                    }
                                    ftype[..s.len()].copy_from_slice(s);
                                    ftype_len = s.len();
                                    i = end;
                                }
                                b"tickSize" => {
                                    let (v, end) = quoted_1e9(body, i)?;
                                    tick = Some(v);
                                    i = end;
                                }
                                b"stepSize" => {
                                    let (v, end) = quoted_1e9(body, i)?;
                                    step = Some(v);
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
                match &ftype[..ftype_len] {
                    b"PRICE_FILTER" => {
                        if let Some(v) = tick {
                            *tick_size_1e9 = v;
                        }
                    }
                    b"LOT_SIZE" => {
                        if let Some(v) = step {
                            *lot_step_1e9 = v;
                        }
                    }
                    _ => {}
                }
            }
            _ => return Err(BnDiscoveryErr::BadRow),
        }
    }
}

/// WS4: parse a QUOTED non-negative decimal (`"0.00050000"`) into
/// ×1e9 fixed point. Fraction digits beyond 9 must be zero (a finer
/// tick than 1e-9 would silently truncate — reject instead; no such
/// tick exists on this venue). Returns the value and the position
/// after the closing quote.
fn quoted_1e9(body: &[u8], pos: usize) -> Result<(i64, usize), BnDiscoveryErr> {
    let (span, end) = quoted_span(body, pos)?;
    if span.is_empty() {
        return Err(BnDiscoveryErr::BadRow);
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
                        return Err(BnDiscoveryErr::BadRow);
                    }
                } else {
                    int_part = int_part
                        .checked_mul(10)
                        .and_then(|v| v.checked_add(d))
                        .ok_or(BnDiscoveryErr::BadRow)?;
                }
            }
            b'.' if !seen_dot => seen_dot = true,
            _ => return Err(BnDiscoveryErr::BadRow),
        }
        k += 1;
    }
    if !seen_digit {
        return Err(BnDiscoveryErr::BadRow);
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
        .ok_or(BnDiscoveryErr::BadRow)?;
    Ok((v, end))
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
        // WS4: the fixture's PRICE_FILTER carries no tickSize (0 =
        // absent), LOT_SIZE.stepSize = 0.00001 → ×1e9.
        assert_eq!(row.tick_size_1e9, 0);
        assert_eq!(row.lot_step_1e9, 10_000);
    }

    #[test]
    fn filters_capture_tick_and_lot_sizes() {
        // WS4: full real-shape filters — tickSize + stepSize land in
        // the row; field order inside a filter object not assumed;
        // foreign filter types skip.
        let body = br#"{"symbols":[{"symbol":"ETHUSDT","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"1000000.00","tickSize":"0.01"},{"stepSize":"0.00100000","filterType":"LOT_SIZE","minQty":"0.00100000"},{"filterType":"MARKET_LOT_SIZE","stepSize":"9.99"},{"filterType":"NOTIONAL","minNotional":"5.0"}]}]}"#;
        let mut d = BnDiscovery::new();
        assert_eq!(d.ingest_body(body).expect("parse ok"), 1);
        let row = d.find(b"ETHUSDT").expect("row");
        assert_eq!(row.tick_size_1e9, 10_000_000, "0.01 ×1e9");
        assert_eq!(row.lot_step_1e9, 1_000_000, "0.001 ×1e9");
    }

    #[test]
    fn filters_bad_values_reject_the_row() {
        let mut d = BnDiscovery::new();
        // Non-decimal tickSize.
        assert_eq!(
            d.ingest_body(
                br#"{"symbols":[{"symbol":"X","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","tickSize":"abc"}]}]}"#
            )
            .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Sub-1e-9 precision would truncate silently — rejected.
        assert_eq!(
            d.ingest_body(
                br#"{"symbols":[{"symbol":"X","status":"TRADING","filters":[{"filterType":"LOT_SIZE","stepSize":"0.0000000001"}]}]}"#
            )
            .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Truncated inside the filters array.
        assert_eq!(
            d.ingest_body(
                br#"{"symbols":[{"symbol":"X","status":"TRADING","filters":[{"filterType""#
            )
            .unwrap_err(),
            BnDiscoveryErr::Truncated
        );
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
        // WS5: contractType/deliveryDate parsed where present.
        let perp = d.find(b"BTCUSDT").unwrap();
        assert_eq!(perp.contract_type, BnContractType::Perpetual);
        assert!(!perp.contract_type.is_dated());
        assert_eq!(
            perp.delivery_ms, 4_133_404_800_000,
            "the far-future sentinel"
        );
        let dated = d.find(b"BTCUSDT_260327").unwrap();
        assert_eq!(dated.contract_type, BnContractType::CurrentQuarter);
        assert!(dated.contract_type.is_dated());
        assert_eq!(dated.delivery_ms, 0, "fixture row carries no deliveryDate");
    }

    #[test]
    fn spot_rows_have_no_contract_class() {
        let mut d = BnDiscovery::new();
        d.ingest_body(SPOT_ONE).unwrap();
        let row = d.find(b"BTCUSDT").unwrap();
        assert_eq!(row.contract_type, BnContractType::None);
        assert_eq!(row.delivery_ms, 0);
    }

    #[test]
    fn unknown_contract_type_is_other_not_fatal() {
        let mut d = BnDiscovery::new();
        let body = br#"{"symbols":[{"symbol":"XUSDT_2701","status":"TRADING","contractType":"NEW_CLASS","deliveryDate":1798761600000}]}"#;
        d.ingest_body(body).unwrap();
        let row = d.find(b"XUSDT_2701").unwrap();
        assert_eq!(row.contract_type, BnContractType::Other);
        assert!(row.contract_type.is_dated());
        assert_eq!(row.delivery_ms, 1_798_761_600_000);
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
            d.ingest_body(br#"{"symbols":[{"status":"TRADING"}]}"#)
                .unwrap_err(),
            BnDiscoveryErr::BadRow
        );
        // Missing status.
        assert_eq!(
            d.ingest_body(br#"{"symbols":[{"symbol":"BTCUSDT"}]}"#)
                .unwrap_err(),
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
            d.ingest_body(br#"{"symbols":[{"symbol":"BTCUSDT""#)
                .unwrap_err(),
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
            if let Ok(n) = d.ingest_body(&input) {
                prop_assert_eq!(n, d.universe_total());
                prop_assert!(d.universe_trading() <= d.universe_total());
            }
        }
    }
}
