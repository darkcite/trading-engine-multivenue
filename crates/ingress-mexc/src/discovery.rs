// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # MEXC boot-time REST discovery (MX5 — the crate half)
//!
//! Three bodies, all fetched once at boot:
//!
//! * `GET https://api.mexc.com/api/v3/exchangeInfo` →
//!   [`MexcSpotDiscovery::ingest_body`]. `{"timezone":…,"serverTime":…,
//!   "symbols":[{…},…]}` — ~1.6 MB, 1 966 rows measured. A row is LIVE
//!   when `status == "1"` (not `"TRADING"`) AND `permissions` contains
//!   `"SPOT"`; `baseAssetPrecision`/`quotePrecision` are decimal-place
//!   counts → lot/tick steps ×1e9; `makerCommission`/`takerCommission`
//!   are quoted decimals ×1e9 (published, UNVERIFIED — plan §1.5).
//!   xStocks equities are ordinary rows (plan §1.3: no equity branch).
//! * `GET https://contract.mexc.com/api/v1/contract/detail` →
//!   [`MexcPerpDiscovery::ingest_body`]. `{"success":true,"code":0,
//!   "data":[{…}]}` — 1 174 rows measured. LIVE when `state == 0` AND
//!   `apiAllowed == true`; `contractSize`, `priceUnit`, `volUnit`,
//!   `makerFeeRate`, `takerFeeRate` are BARE numbers → ×1e9 via the
//!   exponent-tolerant scanner. TradFi/equity/FX/metal perps are
//!   ordinary rows.
//! * `GET https://contract.mexc.com/api/v1/contract/funding_rate/{SYM}`
//!   → [`parse_funding_rate`] → [`MexcFundingSeed`] (ruling Q-MX3: the
//!   seed of the futures `Funding.v1` clock —
//!   `run_loop::Driver::set_funding_seed`).
//!
//! Every walk is STRUCTURAL (key by key, every other value skipped with
//! `core_parse::skip_json_value`), never a key search, so a key name
//! that recurs inside a nested value cannot be mistaken for a row's.
//!
//! ## Leniency law (the Bybit WS13 lesson, pitfall 7)
//!
//! One exotic row must not kill a 1 966-row boot: an OPTIONAL metadata
//! value that does not read as its type (`null`, an empty string, a
//! precision past 1e-9) reads as ABSENT (0 — the boot audit's
//! "step-absent" sentinel) and is skipped structurally. Envelope
//! failures, structural JSON damage, a row without its `symbol` or its
//! liveness key, truncation and the row cap are typed errors
//! ([`MexcDiscoveryErr`]) — all fatal at boot. A failed ingest rolls the
//! table back to its state before the call.
//!
//! ## Allocation note (doctrine)
//!
//! Boot only — allocation allowed. Row storage is one `Vec` reserved at
//! construction, capped at [`MEXC_DISCOVERY_ROWS_CAP`] (fail-fast
//! beyond). The tables drop before the engine loop starts; nothing here
//! is reachable from a hot path.

use core_parse::{scan_number_sci_1e9, skip_json_value, skip_string, skip_ws};

use crate::scan_u64_checked;

/// Spot REST host (the cli's `MEXC_REST_HOST` overrides it).
pub const SPOT_REST_HOST: &str = "api.mexc.com";
/// Futures REST host (the cli's `MEXC_FUT_REST_HOST` overrides it).
pub const FUT_REST_HOST: &str = "contract.mexc.com";
/// Spot instrument inventory path.
pub const SPOT_EXCHANGE_INFO_PATH: &str = "/api/v3/exchangeInfo";
/// Perp instrument inventory path.
pub const FUT_CONTRACT_DETAIL_PATH: &str = "/api/v1/contract/detail";
/// Per-symbol funding path PREFIX — append the perp symbol
/// (`…/funding_rate/BTC_USDT`; rate limit 20 / 2 s, plan §1.4).
pub const FUT_FUNDING_RATE_PATH: &str = "/api/v1/contract/funding_rate/";

/// Hard cap on parsed rows per table across all ingested bodies. Live
/// spot universe 1 966, perps 1 174 (plan §1.3); ≥ 4× headroom.
pub const MEXC_DISCOVERY_ROWS_CAP: usize = 8_192;

/// Longest venue symbol stored (bytes — MEXC lists UTF-8 symbols).
pub const MEXC_DISCOVERY_SYMBOL_MAX: usize = 48;

/// Why a discovery ingest failed. All fatal at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MexcDiscoveryErr {
    /// Not a JSON object, `success`/`code` not the success pair, or the
    /// row container (`symbols` / `data`) missing or of the wrong type.
    Envelope,
    /// A row violated the instrument-object contract (or the JSON is
    /// structurally damaged).
    BadRow,
    /// The body ended inside the document.
    Truncated,
    /// More than [`MEXC_DISCOVERY_ROWS_CAP`] rows in one table.
    TooMany,
}

use MexcDiscoveryErr as E;

// ---------------------------------------------------------------
// Structural walkers + value readers (boot-only)
// ---------------------------------------------------------------

/// Walk the JSON object at `pos` (leading whitespace tolerated),
/// calling `on_key(key, value_pos)` per member; the callback returns
/// the position AFTER the value. Returns the position after `}`.
fn walk_object<F>(body: &[u8], pos: usize, mut on_key: F) -> Result<usize, E>
where
    F: FnMut(&[u8], usize) -> Result<usize, E>,
{
    let mut i = skip_ws(body, pos);
    match body.get(i) {
        Some(b'{') => i += 1,
        None => return Err(E::Truncated),
        Some(_) => return Err(E::BadRow),
    }
    loop {
        i = skip_ws(body, i);
        match body.get(i) {
            None => return Err(E::Truncated),
            Some(b'}') => return Ok(i + 1),
            Some(b',') => i += 1,
            Some(b'"') => {
                let key_start = i + 1;
                let key_end_q = skip_string(body, key_start).ok_or(E::Truncated)?;
                let key = body.get(key_start..key_end_q - 1).ok_or(E::BadRow)?;
                i = skip_ws(body, key_end_q);
                match body.get(i) {
                    Some(b':') => {}
                    None => return Err(E::Truncated),
                    Some(_) => return Err(E::BadRow),
                }
                let vpos = skip_ws(body, i + 1);
                if vpos >= body.len() {
                    return Err(E::Truncated);
                }
                let end = on_key(key, vpos)?;
                if end <= vpos {
                    return Err(E::BadRow);
                }
                i = end;
            }
            Some(_) => return Err(E::BadRow),
        }
    }
}

/// Walk the JSON array at `pos`, calling `on_elem(elem_pos)` per
/// element (returns the position after it). Returns the position after
/// `]`.
fn walk_array<F>(body: &[u8], pos: usize, mut on_elem: F) -> Result<usize, E>
where
    F: FnMut(usize) -> Result<usize, E>,
{
    let mut i = skip_ws(body, pos);
    match body.get(i) {
        Some(b'[') => i += 1,
        None => return Err(E::Truncated),
        Some(_) => return Err(E::BadRow),
    }
    loop {
        i = skip_ws(body, i);
        match body.get(i) {
            None => return Err(E::Truncated),
            Some(b']') => return Ok(i + 1),
            Some(b',') => i += 1,
            Some(_) => {
                let end = on_elem(i)?;
                if end <= i {
                    return Err(E::BadRow);
                }
                i = end;
            }
        }
    }
}

/// Skip one value structurally.
#[inline]
fn skip(body: &[u8], pos: usize) -> Result<usize, E> {
    skip_json_value(body, pos).ok_or(E::BadRow)
}

/// A quoted string's raw bytes (escapes kept raw: a symbol carrying one
/// simply never matches a configured `[A-Z0-9_]` name).
fn read_str(body: &[u8], pos: usize) -> Result<(&[u8], usize), E> {
    if body.get(pos) != Some(&b'"') {
        return Err(E::BadRow);
    }
    let end_q = skip_string(body, pos + 1).ok_or(E::Truncated)?;
    Ok((body.get(pos + 1..end_q - 1).ok_or(E::BadRow)?, end_q))
}

/// A number, bare or quoted, ×1e9 (exponents tolerated, sub-1e-9
/// digits truncate). `None` = not a number (the caller skips it).
fn read_decimal_1e9(body: &[u8], pos: usize) -> Option<(i64, usize)> {
    if body.get(pos) == Some(&b'"') {
        let end_q = skip_string(body, pos + 1)?;
        let inner = body.get(pos + 1..end_q - 1)?;
        let (v, e) = scan_number_sci_1e9(inner, 0)?;
        return if e == inner.len() { Some((v, end_q)) } else { None };
    }
    scan_number_sci_1e9(body, pos)
}

/// A non-negative integer, bare or quoted (checked). `None` = not one
/// (a fraction or exponent is not an integer).
fn read_u64(body: &[u8], pos: usize) -> Option<(u64, usize)> {
    if body.get(pos) == Some(&b'"') {
        let (v, e) = scan_u64_checked(body, pos + 1)?;
        return if body.get(e) == Some(&b'"') { Some((v, e + 1)) } else { None };
    }
    let (v, e) = scan_u64_checked(body, pos)?;
    if matches!(body.get(e), Some(b'.' | b'e' | b'E')) {
        return None;
    }
    Some((v, e))
}

/// Leniency law: an optional decimal field — latched when it reads,
/// skipped (absent = 0) when it does not.
fn opt_decimal(body: &[u8], pos: usize, slot: &mut i64) -> Result<usize, E> {
    match read_decimal_1e9(body, pos) {
        Some((v, e)) => {
            *slot = v;
            Ok(e)
        }
        None => skip(body, pos),
    }
}

/// Leniency law: an optional non-negative integer field.
fn opt_u64(body: &[u8], pos: usize, slot: &mut Option<u64>) -> Result<usize, E> {
    match read_u64(body, pos) {
        Some((v, e)) => {
            *slot = Some(v);
            Ok(e)
        }
        None => skip(body, pos),
    }
}

/// A JSON boolean literal (`None` = not a boolean; the caller skips).
fn read_bool(body: &[u8], pos: usize) -> Option<(bool, usize)> {
    let rest = body.get(pos..)?;
    if rest.starts_with(b"true") {
        Some((true, pos + 4))
    } else if rest.starts_with(b"false") {
        Some((false, pos + 5))
    } else {
        None
    }
}

/// Decimal places → step ×1e9 (`3` → 0.001 → 1 000 000). Past 9
/// places the step floors to 0 = the absent sentinel (the Bybit
/// sub-1e-9 law).
#[inline]
fn places_to_step_1e9(places: Option<u64>) -> i64 {
    match places {
        Some(p) if p <= 9 => {
            let mut v: i64 = 1_000_000_000;
            let mut k = 0;
            while k < p {
                v /= 10;
                k += 1;
            }
            v
        }
        _ => 0,
    }
}

/// Copy a validated symbol into its fixed row slot.
fn store_symbol(s: &[u8]) -> Result<([u8; MEXC_DISCOVERY_SYMBOL_MAX], u8), E> {
    if s.is_empty() || s.len() > MEXC_DISCOVERY_SYMBOL_MAX {
        return Err(E::BadRow);
    }
    let mut symbol = [0u8; MEXC_DISCOVERY_SYMBOL_MAX];
    // COPY: venue symbol ≤ 48 B per row, boot only — the row outlives
    // the REST body it was parsed from (the body buffer is reused for
    // the next fetch) — borrowing the body rejected: that would pin a
    // 1.6 MB buffer for the table's lifetime.
    symbol[..s.len()].copy_from_slice(s);
    Ok((symbol, s.len() as u8))
}

/// Envelope check shared by the two contract endpoints:
/// `"success":true` and `"code":0`.
#[derive(Default)]
struct ContractEnvelope {
    success: Option<bool>,
    code: Option<u64>,
}

impl ContractEnvelope {
    /// Latch `success`/`code`; returns `None` for any other key.
    fn try_key(&mut self, body: &[u8], key: &[u8], v: usize) -> Option<Result<usize, E>> {
        match key {
            b"success" => Some(match read_bool(body, v) {
                Some((b, e)) => {
                    self.success = Some(b);
                    Ok(e)
                }
                None => skip(body, v),
            }),
            b"code" => Some(opt_u64(body, v, &mut self.code)),
            _ => None,
        }
    }

    fn ok(&self) -> bool {
        self.success == Some(true) && self.code == Some(0)
    }
}

// ---------------------------------------------------------------
// Spot — /api/v3/exchangeInfo
// ---------------------------------------------------------------

/// One discovered spot instrument.
#[derive(Copy, Clone, Debug)]
pub struct MexcSpotRow {
    /// Venue symbol bytes (`symbol_len` valid), UPPERCASE on the wire.
    pub symbol: [u8; MEXC_DISCOVERY_SYMBOL_MAX],
    /// Valid prefix length of `symbol`.
    pub symbol_len: u8,
    /// `status == "1"` AND `permissions` contains `"SPOT"`.
    pub trading: bool,
    /// Price tick ×1e9 from `quotePrecision` (0 = absent).
    pub tick_size_1e9: i64,
    /// Lot step ×1e9 from `baseAssetPrecision` (0 = absent).
    pub lot_step_1e9: i64,
    /// `makerCommission` ×1e9, signed (published, unverified; 0 =
    /// absent).
    pub maker_fee_1e9: i64,
    /// `takerCommission` ×1e9 (published, unverified; 0 = absent).
    pub taker_fee_1e9: i64,
}

impl MexcSpotRow {
    /// The venue symbol as a byte slice.
    #[inline]
    pub fn symbol(&self) -> &[u8] {
        &self.symbol[..self.symbol_len as usize]
    }
}

/// Parse one exchangeInfo row at `pos`.
// COPY: `Result<(MexcSpotRow, usize), E>` ≥ 96 B by value, once per row
// of the boot-time REST discovery (cold; the row is pushed into the
// discovery table next) — rejected: an out-param into the table's next
// slot, bookkeeping a boot-once path does not earn.
fn parse_spot_row(body: &[u8], pos: usize) -> Result<(MexcSpotRow, usize), E> {
    let mut sym: Option<&[u8]> = None;
    let mut status: Option<bool> = None;
    let mut spot_perm = false;
    let mut base_places: Option<u64> = None;
    let mut quote_places: Option<u64> = None;
    let mut maker = 0i64;
    let mut taker = 0i64;
    let end = walk_object(body, pos, |key, v| match key {
        b"symbol" => {
            let (s, e) = read_str(body, v)?;
            sym = Some(s);
            Ok(e)
        }
        b"status" => {
            if body.get(v) == Some(&b'"') {
                let (s, e) = read_str(body, v)?;
                status = Some(s == b"1");
                Ok(e)
            } else {
                match read_u64(body, v) {
                    Some((x, e)) => {
                        status = Some(x == 1);
                        Ok(e)
                    }
                    None => {
                        status = Some(false);
                        skip(body, v)
                    }
                }
            }
        }
        b"permissions" => {
            if body.get(v) == Some(&b'[') {
                walk_array(body, v, |ep| {
                    if body.get(ep) == Some(&b'"') {
                        let (s, e) = read_str(body, ep)?;
                        if s == b"SPOT" {
                            spot_perm = true;
                        }
                        Ok(e)
                    } else {
                        skip(body, ep)
                    }
                })
            } else {
                skip(body, v)
            }
        }
        b"baseAssetPrecision" => opt_u64(body, v, &mut base_places),
        b"quotePrecision" => opt_u64(body, v, &mut quote_places),
        b"makerCommission" => opt_decimal(body, v, &mut maker),
        b"takerCommission" => opt_decimal(body, v, &mut taker),
        _ => skip(body, v),
    })?;
    let (symbol, symbol_len) = store_symbol(sym.ok_or(E::BadRow)?)?;
    let status = status.ok_or(E::BadRow)?;
    Ok((
        MexcSpotRow {
            symbol,
            symbol_len,
            trading: status && spot_perm,
            tick_size_1e9: places_to_step_1e9(quote_places),
            lot_step_1e9: places_to_step_1e9(base_places),
            maker_fee_1e9: maker,
            taker_fee_1e9: taker,
        },
        end,
    ))
}

/// Boot-only MEXC spot instrument table. See module docs.
pub struct MexcSpotDiscovery {
    rows: Vec<MexcSpotRow>,
    universe_trading: u32,
}

impl MexcSpotDiscovery {
    /// Empty table with capacity reserved once.
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(MEXC_DISCOVERY_ROWS_CAP),
            universe_trading: 0,
        }
    }

    /// Parse one `exchangeInfo` body into the table. Returns the number
    /// of rows added. On error the table is unchanged.
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, MexcDiscoveryErr> {
        let start = skip_ws(body, 0);
        if body.get(start) != Some(&b'{') {
            return Err(E::Envelope);
        }
        let base_len = self.rows.len();
        let base_trading = self.universe_trading;
        let mut saw_symbols = false;
        let rows = &mut self.rows;
        let trading = &mut self.universe_trading;
        let res = walk_object(body, start, |key, v| {
            if key != b"symbols" {
                return skip(body, v);
            }
            if body.get(v) != Some(&b'[') {
                return Err(E::Envelope);
            }
            saw_symbols = true;
            walk_array(body, v, |ep| {
                let (row, end) = parse_spot_row(body, ep)?;
                if rows.len() >= MEXC_DISCOVERY_ROWS_CAP {
                    return Err(E::TooMany);
                }
                if row.trading {
                    *trading += 1;
                }
                rows.push(row);
                Ok(end)
            })
        });
        let res = match res {
            Ok(_) if saw_symbols => Ok((self.rows.len() - base_len) as u32),
            Ok(_) => Err(E::Envelope),
            Err(e) => Err(e),
        };
        if res.is_err() {
            self.rows.truncate(base_len);
            self.universe_trading = base_trading;
        }
        res
    }

    /// Look up a discovered symbol by exact bytes.
    pub fn find(&self, symbol: &[u8]) -> Option<&MexcSpotRow> {
        self.rows.iter().find(|r| r.symbol() == symbol)
    }

    /// Total rows parsed (all statuses).
    #[inline]
    pub fn universe_total(&self) -> u32 {
        self.rows.len() as u32
    }

    /// Live rows — the §6.1 coverage-report `universe=` figure.
    #[inline]
    pub fn universe_trading(&self) -> u32 {
        self.universe_trading
    }
}

impl Default for MexcSpotDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Perps — /api/v1/contract/detail
// ---------------------------------------------------------------

/// One discovered perpetual contract.
#[derive(Copy, Clone, Debug)]
pub struct MexcPerpRow {
    /// Venue symbol bytes (`symbol_len` valid), `BASE_QUOTE`.
    pub symbol: [u8; MEXC_DISCOVERY_SYMBOL_MAX],
    /// Valid prefix length of `symbol`.
    pub symbol_len: u8,
    /// `state == 0` AND `apiAllowed == true`.
    pub trading: bool,
    /// `contractSize` ×1e9 — base units per contract (0 = absent).
    pub contract_size_1e9: i64,
    /// `priceUnit` ×1e9 — the price tick (0 = absent).
    pub price_unit_1e9: i64,
    /// `volUnit` ×1e9 — the order-size step in contracts (0 = absent).
    pub vol_unit_1e9: i64,
    /// `makerFeeRate` ×1e9, signed (published, unverified).
    pub maker_fee_1e9: i64,
    /// `takerFeeRate` ×1e9 (published, unverified).
    pub taker_fee_1e9: i64,
}

impl MexcPerpRow {
    /// The venue symbol as a byte slice.
    #[inline]
    pub fn symbol(&self) -> &[u8] {
        &self.symbol[..self.symbol_len as usize]
    }
}

/// Parse one contract-detail row at `pos`.
// COPY: `Result<(MexcPerpRow, usize), E>` ≥ 104 B by value, once per row
// of the boot-time REST discovery (cold; the row is pushed into the
// discovery table next) — rejected: an out-param into the table's next
// slot, bookkeeping a boot-once path does not earn.
fn parse_perp_row(body: &[u8], pos: usize) -> Result<(MexcPerpRow, usize), E> {
    let mut sym: Option<&[u8]> = None;
    let mut state: Option<Option<u64>> = None;
    let mut api_allowed = false;
    let mut contract_size = 0i64;
    let mut price_unit = 0i64;
    let mut vol_unit = 0i64;
    let mut maker = 0i64;
    let mut taker = 0i64;
    let end = walk_object(body, pos, |key, v| match key {
        b"symbol" => {
            let (s, e) = read_str(body, v)?;
            sym = Some(s);
            Ok(e)
        }
        b"state" => {
            let mut st = None;
            let e = opt_u64(body, v, &mut st)?;
            state = Some(st);
            Ok(e)
        }
        b"apiAllowed" => match read_bool(body, v) {
            Some((b, e)) => {
                api_allowed = b;
                Ok(e)
            }
            None => skip(body, v),
        },
        b"contractSize" => opt_decimal(body, v, &mut contract_size),
        b"priceUnit" => opt_decimal(body, v, &mut price_unit),
        b"volUnit" => opt_decimal(body, v, &mut vol_unit),
        b"makerFeeRate" => opt_decimal(body, v, &mut maker),
        b"takerFeeRate" => opt_decimal(body, v, &mut taker),
        _ => skip(body, v),
    })?;
    let (symbol, symbol_len) = store_symbol(sym.ok_or(E::BadRow)?)?;
    // The liveness key is required; an unreadable value is "not live".
    let state = state.ok_or(E::BadRow)?;
    Ok((
        MexcPerpRow {
            symbol,
            symbol_len,
            trading: state == Some(0) && api_allowed,
            contract_size_1e9: contract_size,
            price_unit_1e9: price_unit,
            vol_unit_1e9: vol_unit,
            maker_fee_1e9: maker,
            taker_fee_1e9: taker,
        },
        end,
    ))
}

/// Boot-only MEXC perpetual-contract table. See module docs.
pub struct MexcPerpDiscovery {
    rows: Vec<MexcPerpRow>,
    universe_trading: u32,
}

impl MexcPerpDiscovery {
    /// Empty table with capacity reserved once.
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(MEXC_DISCOVERY_ROWS_CAP),
            universe_trading: 0,
        }
    }

    /// Parse one `contract/detail` body (`data` an array of rows, or a
    /// single row object — the `?symbol=` form). Returns the number of
    /// rows added. On error the table is unchanged.
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, MexcDiscoveryErr> {
        let start = skip_ws(body, 0);
        if body.get(start) != Some(&b'{') {
            return Err(E::Envelope);
        }
        let base_len = self.rows.len();
        let base_trading = self.universe_trading;
        let mut env = ContractEnvelope::default();
        let mut saw_data = false;
        let rows = &mut self.rows;
        let trading = &mut self.universe_trading;
        let res = walk_object(body, start, |key, v| {
            if let Some(r) = env.try_key(body, key, v) {
                return r;
            }
            if key != b"data" {
                return skip(body, v);
            }
            saw_data = true;
            let mut push = |ep: usize| -> Result<usize, E> {
                let (row, end) = parse_perp_row(body, ep)?;
                if rows.len() >= MEXC_DISCOVERY_ROWS_CAP {
                    return Err(E::TooMany);
                }
                if row.trading {
                    *trading += 1;
                }
                rows.push(row);
                Ok(end)
            };
            match body.get(v) {
                Some(b'[') => walk_array(body, v, push),
                Some(b'{') => push(v),
                _ => Err(E::Envelope),
            }
        });
        let res = match res {
            Ok(_) if saw_data && env.ok() => Ok((self.rows.len() - base_len) as u32),
            Ok(_) => Err(E::Envelope),
            Err(e) => Err(e),
        };
        if res.is_err() {
            self.rows.truncate(base_len);
            self.universe_trading = base_trading;
        }
        res
    }

    /// Look up a discovered symbol by exact bytes.
    pub fn find(&self, symbol: &[u8]) -> Option<&MexcPerpRow> {
        self.rows.iter().find(|r| r.symbol() == symbol)
    }

    /// Total rows parsed (all states).
    #[inline]
    pub fn universe_total(&self) -> u32 {
        self.rows.len() as u32
    }

    /// Live rows — the §6.1 coverage-report `universe=` figure.
    #[inline]
    pub fn universe_trading(&self) -> u32 {
        self.universe_trading
    }
}

impl Default for MexcPerpDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Funding seed — /api/v1/contract/funding_rate/{SYM}
// ---------------------------------------------------------------

/// The Q-MX3 funding-clock seed for one perp.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MexcFundingSeed {
    /// `nextSettleTime` — the next settlement instant (ms).
    pub next_settle_ms: u64,
    /// `collectCycle` — the funding period in HOURS (8 measured).
    pub collect_cycle_h: u32,
    /// `fundingRate` ×1e9, signed (0 when absent).
    pub rate_1e9: i64,
}

/// Parse one `funding_rate/{SYM}` body. `nextSettleTime` and
/// `collectCycle` are required (BadRow without them).
pub fn parse_funding_rate(body: &[u8]) -> Result<MexcFundingSeed, MexcDiscoveryErr> {
    let start = skip_ws(body, 0);
    if body.get(start) != Some(&b'{') {
        return Err(E::Envelope);
    }
    let mut env = ContractEnvelope::default();
    let mut seed: Option<MexcFundingSeed> = None;
    walk_object(body, start, |key, v| {
        if let Some(r) = env.try_key(body, key, v) {
            return r;
        }
        if key != b"data" {
            return skip(body, v);
        }
        if body.get(v) != Some(&b'{') {
            return Err(E::Envelope);
        }
        let mut next: Option<u64> = None;
        let mut cycle: Option<u64> = None;
        let mut rate = 0i64;
        let end = walk_object(body, v, |k, dv| match k {
            b"nextSettleTime" => opt_u64(body, dv, &mut next),
            b"collectCycle" => opt_u64(body, dv, &mut cycle),
            b"fundingRate" => opt_decimal(body, dv, &mut rate),
            _ => skip(body, dv),
        })?;
        seed = Some(MexcFundingSeed {
            next_settle_ms: next.ok_or(E::BadRow)?,
            collect_cycle_h: u32::try_from(cycle.ok_or(E::BadRow)?).map_err(|_| E::BadRow)?,
            rate_1e9: rate,
        });
        Ok(end)
    })?;
    if !env.ok() {
        return Err(E::Envelope);
    }
    seed.ok_or(E::Envelope)
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed real exchangeInfo shape (plan §1.3 worked examples +
    /// the noise the walker must skip: 30-digit quoted precisions,
    /// nested filters, a UTF-8 symbol, a `null` commission, a precision
    /// past 1e-9, a non-SPOT row, a delisted row).
    fn spot_body() -> String {
        String::from(concat!(
            r#"{"timezone":"CST","serverTime":1789897303485,"rateLimits":[],"exchangeFilters":[],"symbols":["#,
            r#"{"symbol":"BTCUSDT","status":"1","baseAsset":"BTC","baseAssetPrecision":8,"quoteAsset":"USDT","quotePrecision":2,"quoteAssetPrecision":2,"orderTypes":["LIMIT","MARKET","LIMIT_MAKER"],"isSpotTradingAllowed":true,"quoteAmountPrecision":"1.000000000000000000000000000000","baseSizePrecision":"0","permissions":["SPOT"],"filters":[{"filterType":"PERCENT_PRICE_BY_SIDE","bidMultiplierUp":"5","askMultiplierDown":"0.2"}],"maxQuoteAmount":"2000000.000000000000000000000000000000","makerCommission":"0","takerCommission":"0.0005","fullName":"Bitcoin","tradeSideType":1,"contractAddress":"","st":false},"#,
            r#"{"symbol":"AAPLXUSDT","status":"1","baseAsset":"AAPLX","baseAssetPrecision":3,"quotePrecision":2,"permissions":["SPOT"],"makerCommission":"0","takerCommission":"0.0005","fullName":"Apple xStock","conceptPlates":["Innovation","Tokenized Stocks"]},"#,
            r#"{"symbol":"SPYXUSDT","status":"1","baseAssetPrecision":3,"quotePrecision":2,"permissions":["SPOT"],"makerCommission":"0","takerCommission":"0.0005","fullName":"SP500 xStock"},"#,
            r#"{"symbol":"OLDUSDT","status":"2","baseAssetPrecision":2,"quotePrecision":6,"permissions":["SPOT"],"makerCommission":"0","takerCommission":"0.002"},"#,
            r#"{"symbol":"NOSPOTUSDT","status":"1","permissions":[],"makerCommission":"-0.0001"},"#,
            "{\"symbol\":\"\u{5e01}\u{5b89}\u{4eba}\u{751f}USDT\",\"status\":\"1\",\"permissions\":[\"SPOT\"],\"baseAssetPrecision\":20,\"quotePrecision\":\"4\",\"makerCommission\":null,\"takerCommission\":\"\"}",
            r#"]}"#
        ))
    }

    #[test]
    fn spot_body_parses_rows_liveness_and_metadata() {
        let body = spot_body();
        let mut d = MexcSpotDiscovery::new();
        assert_eq!(d.ingest_body(body.as_bytes()).expect("parse ok"), 6);
        assert_eq!(d.universe_total(), 6);
        assert_eq!(d.universe_trading(), 4, "OLD (status 2) and NOSPOT are not live");
        let btc = d.find(b"BTCUSDT").unwrap();
        assert!(btc.trading);
        assert_eq!(btc.tick_size_1e9, 10_000_000, "quotePrecision 2 → 0.01");
        assert_eq!(btc.lot_step_1e9, 10, "baseAssetPrecision 8 → 1e-8");
        assert_eq!((btc.maker_fee_1e9, btc.taker_fee_1e9), (0, 500_000));
        let aapl = d.find(b"AAPLXUSDT").expect("xStocks are ordinary rows");
        assert!(aapl.trading);
        assert_eq!(aapl.lot_step_1e9, 1_000_000, "basePrec 3 → 0.001");
        assert!(d.find(b"SPYXUSDT").unwrap().trading);
        assert!(!d.find(b"OLDUSDT").unwrap().trading);
        let ns = d.find(b"NOSPOTUSDT").unwrap();
        assert!(!ns.trading, "no SPOT permission");
        assert_eq!(ns.maker_fee_1e9, -100_000, "rebates are signed");
        assert_eq!(ns.lot_step_1e9, 0, "absent precision");
        let cjk = d.find("\u{5e01}\u{5b89}\u{4eba}\u{751f}USDT".as_bytes()).expect("UTF-8 symbol row");
        assert!(cjk.trading);
        assert_eq!(cjk.lot_step_1e9, 0, "precision 20 floors to absent");
        assert_eq!(cjk.tick_size_1e9, 100_000, "quoted precision \"4\"");
        assert_eq!((cjk.maker_fee_1e9, cjk.taker_fee_1e9), (0, 0), "null / empty = absent");
        assert!(d.find(b"NOPE").is_none());
    }

    #[test]
    fn spot_envelope_and_row_failures_are_typed_and_roll_back() {
        let mut d = MexcSpotDiscovery::new();
        let ok = br#"{"symbols":[{"symbol":"AUSDT","status":"1","permissions":["SPOT"]}]}"#;
        assert_eq!(d.ingest_body(ok).unwrap(), 1);
        let cases: [(&[u8], MexcDiscoveryErr); 9] = [
            (b"<html>", E::Envelope),
            (b"", E::Envelope),
            (br#"{"code":10072,"msg":"invalid"}"#, E::Envelope),
            (br#"{"symbols":{}}"#, E::Envelope),
            (br#"{"symbols":[{"symbol":"X","status":"1"},"#, E::Truncated),
            (br#"{"symbols":[{"status":"1"}]}"#, E::BadRow),
            (br#"{"symbols":[{"symbol":"X"}]}"#, E::BadRow),
            (br#"{"symbols":[{"symbol":7,"status":"1"}]}"#, E::BadRow),
            (br#"{"symbols":[{"symbol":"X","status":"1"} 7]}"#, E::BadRow),
        ];
        for (body, want) in cases {
            assert_eq!(d.ingest_body(body), Err(want), "{:?}", core::str::from_utf8(body));
            assert_eq!(d.universe_total(), 1, "rolled back");
            assert_eq!(d.universe_trading(), 1);
        }
        let long = format!(r#"{{"symbols":[{{"symbol":"{}","status":"1"}}]}}"#, "A".repeat(MEXC_DISCOVERY_SYMBOL_MAX + 1));
        assert_eq!(d.ingest_body(long.as_bytes()), Err(E::BadRow));
    }

    #[test]
    fn row_cap_is_enforced() {
        let mut body = String::from(r#"{"symbols":["#);
        let mut k = 0;
        while k <= MEXC_DISCOVERY_ROWS_CAP {
            if k > 0 {
                body.push(',');
            }
            body.push_str(&format!(r#"{{"symbol":"S{k}","status":"1"}}"#));
            k += 1;
        }
        body.push_str("]}");
        let mut d = MexcSpotDiscovery::default();
        assert_eq!(d.ingest_body(body.as_bytes()), Err(E::TooMany));
        assert_eq!(d.universe_total(), 0);
    }

    fn perp_body() -> String {
        String::from(concat!(
            r#"{"success":true,"code":0,"data":["#,
            "{\"symbol\":\"BTC_USDT\",\"displayName\":\"BTC_USDT\u{6c38}\u{7eed}\",\"displayNameEn\":\"BTC_USDT PERPETUAL\",\"positionOpenType\":3,\"baseCoin\":\"BTC\",\"quoteCoin\":\"USDT\",\"settleCoin\":\"USDT\",\"contractSize\":0.0001,\"minLeverage\":1,\"maxLeverage\":500,\"priceScale\":1,\"volScale\":0,\"amountScale\":4,\"priceUnit\":0.1,\"volUnit\":1,\"minVol\":1,\"maxVol\":1300000,\"takerFeeRate\":0.0002,\"makerFeeRate\":0,\"maintenanceMarginRate\":0.004,\"indexOrigin\":[\"BINANCE\",\"GATEIO\"],\"state\":0,\"isNew\":false,\"isHot\":true,\"conceptPlate\":[\"mc-trade-zone-pow\"],\"riskLimitType\":\"BY_VOLUME\",\"maxNumOrders\":[200,50],\"apiAllowed\":true,\"futureType\":1},",
            r#"{"symbol":"XAU_USDT","contractSize":0.001,"priceUnit":0.01,"volUnit":1,"takerFeeRate":0.0002,"makerFeeRate":0,"state":0,"apiAllowed":true,"conceptPlate":["mc-trade-zone-tradfi","mc-trade-zone-metals"]},"#,
            r#"{"symbol":"AAPLSTOCK_USDT","contractSize":0.01,"priceUnit":0.01,"volUnit":1,"takerFeeRate":0,"makerFeeRate":0,"state":0,"apiAllowed":true,"indexOrigin":["BINANCE_FUTURE","BITGET_FUTURE","BINANCETICKER","PYTH","KAIKO"]},"#,
            r#"{"symbol":"EUR_USDT","contractSize":1,"priceUnit":0.0001,"volUnit":1,"takerFeeRate":0.0004,"makerFeeRate":0.0001,"state":0,"apiAllowed":true},"#,
            r#"{"symbol":"SPY_USDT","contractSize":1e-3,"priceUnit":0.01,"volUnit":1,"state":0,"apiAllowed":true},"#,
            r#"{"symbol":"DEAD_USDT","contractSize":1,"state":4,"apiAllowed":true},"#,
            r#"{"symbol":"NOAPI_USDT","contractSize":1,"state":0,"apiAllowed":false},"#,
            r#"{"symbol":"ODD_USDT","contractSize":null,"state":"x","apiAllowed":"yes"}"#,
            r#"]}"#
        ))
    }

    #[test]
    fn perp_body_parses_rows_liveness_and_metadata() {
        let body = perp_body();
        let mut d = MexcPerpDiscovery::new();
        assert_eq!(d.ingest_body(body.as_bytes()).expect("parse ok"), 8);
        assert_eq!(d.universe_total(), 8);
        assert_eq!(d.universe_trading(), 5);
        let btc = d.find(b"BTC_USDT").unwrap();
        assert!(btc.trading);
        assert_eq!(btc.contract_size_1e9, 100_000, "0.0001 ×1e9");
        assert_eq!(btc.price_unit_1e9, 100_000_000, "0.1");
        assert_eq!(btc.vol_unit_1e9, 1_000_000_000, "integer 1");
        assert_eq!((btc.maker_fee_1e9, btc.taker_fee_1e9), (0, 200_000));
        let xau = d.find(b"XAU_USDT").expect("TradFi rows are ordinary");
        assert_eq!(xau.contract_size_1e9, 1_000_000);
        assert_eq!(xau.price_unit_1e9, 10_000_000);
        assert!(d.find(b"AAPLSTOCK_USDT").unwrap().trading);
        let eur = d.find(b"EUR_USDT").unwrap();
        assert_eq!((eur.maker_fee_1e9, eur.taker_fee_1e9), (100_000, 400_000));
        assert_eq!(d.find(b"SPY_USDT").unwrap().contract_size_1e9, 1_000_000, "1e-3");
        assert!(!d.find(b"DEAD_USDT").unwrap().trading, "state 4");
        assert!(!d.find(b"NOAPI_USDT").unwrap().trading, "apiAllowed false");
        let odd = d.find(b"ODD_USDT").unwrap();
        assert!(!odd.trading, "unreadable state / apiAllowed = not live");
        assert_eq!(odd.contract_size_1e9, 0);
        // The single-object `?symbol=` form.
        let mut one = MexcPerpDiscovery::default();
        assert_eq!(one.ingest_body(br#"{"success":true,"code":0,"data":{"symbol":"BTC_USDT","state":0,"apiAllowed":true}}"#).unwrap(), 1);
        assert!(one.find(b"BTC_USDT").unwrap().trading);
    }

    #[test]
    fn perp_envelope_and_row_failures_are_typed_and_roll_back() {
        let mut d = MexcPerpDiscovery::new();
        assert_eq!(d.ingest_body(br#"{"success":true,"code":0,"data":[]}"#).unwrap(), 0);
        let cases: [(&[u8], MexcDiscoveryErr); 9] = [
            (br#"{"success":false,"code":1002,"message":"x"}"#, E::Envelope),
            (br#"{"success":false,"code":0,"data":[{"symbol":"A_USDT","state":0}]}"#, E::Envelope),
            (br#"{"success":true,"code":1,"data":[]}"#, E::Envelope),
            (br#"{"success":true,"data":[]}"#, E::Envelope),
            (br#"{"success":true,"code":0}"#, E::Envelope),
            (br#"{"success":true,"code":0,"data":"x"}"#, E::Envelope),
            (br#"{"success":true,"code":0,"data":[{"symbol":"A_USDT"}]}"#, E::BadRow),
            (br#"{"success":true,"code":0,"data":[{"state":0}]}"#, E::BadRow),
            (br#"{"success":true,"code":0,"data":[{"symbol":"A_USDT","state":0"#, E::Truncated),
        ];
        for (body, want) in cases {
            assert_eq!(d.ingest_body(body), Err(want), "{:?}", core::str::from_utf8(body));
            assert_eq!(d.universe_total(), 0, "rolled back");
            assert_eq!(d.universe_trading(), 0);
        }
        assert_eq!(d.ingest_body(b"[]"), Err(E::Envelope));
    }

    const FUNDING: &[u8] = br#"{"success":true,"code":0,"data":{"symbol":"BTC_USDT","fundingRate":0.0001,"maxFundingRate":0.0018,"minFundingRate":-0.0018,"collectCycle":8,"nextSettleTime":1789920000000,"timestamp":1789897303485,"idxPrice":80564.4,"fairPrice":80536.9}}"#;

    #[test]
    fn funding_rate_body_seeds_the_clock() {
        let s = parse_funding_rate(FUNDING).unwrap();
        assert_eq!(s.next_settle_ms, 1_789_920_000_000);
        assert_eq!(s.collect_cycle_h, 8);
        assert_eq!(s.rate_1e9, 100_000, "not maxFundingRate — structural keys");
        let neg = br#"{"success":true,"code":0,"data":{"collectCycle":4,"nextSettleTime":5,"fundingRate":-0.00025}}"#;
        assert_eq!(parse_funding_rate(neg).unwrap(), MexcFundingSeed { next_settle_ms: 5, collect_cycle_h: 4, rate_1e9: -250_000 });
    }

    #[test]
    fn funding_rate_failures_are_typed() {
        assert_eq!(parse_funding_rate(br#"{"success":false,"code":1001,"message":"x"}"#), Err(E::Envelope));
        assert_eq!(parse_funding_rate(br#"{"success":true,"code":0,"data":[]}"#), Err(E::Envelope));
        assert_eq!(parse_funding_rate(br#"{"success":true,"code":0}"#), Err(E::Envelope));
        assert_eq!(parse_funding_rate(b"nope"), Err(E::Envelope));
        assert_eq!(parse_funding_rate(br#"{"success":true,"code":0,"data":{"collectCycle":8}}"#), Err(E::BadRow));
        assert_eq!(parse_funding_rate(br#"{"success":true,"code":0,"data":{"nextSettleTime":5}}"#), Err(E::BadRow));
        assert_eq!(parse_funding_rate(br#"{"success":true,"code":0,"data":{"nextSettleTime":5,"collectCycle":99999999999}}"#), Err(E::BadRow));
        assert_eq!(parse_funding_rate(&FUNDING[..FUNDING.len() - 2]), Err(E::Truncated));
    }

    #[test]
    fn value_readers() {
        assert_eq!(read_u64(b"12,", 0), Some((12, 2)));
        assert_eq!(read_u64(b"\"12\"", 0), Some((12, 4)));
        assert_eq!(read_u64(b"1.5", 0), None);
        assert_eq!(read_u64(b"\"1", 0), None);
        assert_eq!(read_decimal_1e9(b"\"0.0005\"", 0), Some((500_000, 8)));
        assert_eq!(read_decimal_1e9(b"\"0.0005x\"", 0), None);
        assert_eq!(read_decimal_1e9(b"1e-3,", 0), Some((1_000_000, 4)));
        assert_eq!(read_bool(b"true", 0), Some((true, 4)));
        assert_eq!(read_bool(b"null", 0), None);
        assert_eq!(places_to_step_1e9(Some(0)), 1_000_000_000);
        assert_eq!(places_to_step_1e9(Some(9)), 1);
        assert_eq!(places_to_step_1e9(Some(10)), 0);
        assert_eq!(places_to_step_1e9(None), 0);
    }

    #[test]
    fn rest_endpoint_constants() {
        assert_eq!(format!("https://{SPOT_REST_HOST}{SPOT_EXCHANGE_INFO_PATH}"), "https://api.mexc.com/api/v3/exchangeInfo");
        assert_eq!(format!("https://{FUT_REST_HOST}{FUT_CONTRACT_DETAIL_PATH}"), "https://contract.mexc.com/api/v1/contract/detail");
        assert_eq!(format!("{FUT_FUNDING_RATE_PATH}BTC_USDT"), "/api/v1/contract/funding_rate/BTC_USDT");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The three parsers never panic on arbitrary bytes; on success
        /// the counts stay consistent.
        #[test]
        fn ingest_never_panics(input in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut s = MexcSpotDiscovery::new();
            if let Ok(n) = s.ingest_body(&input) {
                prop_assert_eq!(n, s.universe_total());
                prop_assert!(s.universe_trading() <= s.universe_total());
            }
            let mut p = MexcPerpDiscovery::new();
            if let Ok(n) = p.ingest_body(&input) {
                prop_assert_eq!(n, p.universe_total());
                prop_assert!(p.universe_trading() <= p.universe_total());
            }
            let _ = parse_funding_rate(&input);
        }

        /// Structured: the envelope with random bytes spliced into a
        /// row never panics (the walkers see real structure around the
        /// garbage).
        #[test]
        fn spliced_rows_never_panic(junk in "[ -~]{0,40}") {
            let spot = format!(r#"{{"symbols":[{{"symbol":"A","status":"1",{junk}}}]}}"#);
            let _ = MexcSpotDiscovery::new().ingest_body(spot.as_bytes());
            let perp = format!(r#"{{"success":true,"code":0,"data":[{{"symbol":"A","state":0,{junk}}}]}}"#);
            let _ = MexcPerpDiscovery::new().ingest_body(perp.as_bytes());
            let fr = format!(r#"{{"success":true,"code":0,"data":{{"nextSettleTime":1,"collectCycle":8,{junk}}}}}"#);
            let _ = parse_funding_rate(fr.as_bytes());
        }

        /// Spot rows round-trip their precisions and commissions.
        #[test]
        fn spot_rows_roundtrip(base in 0u64..=9, quote in 0u64..=9, taker in 0u32..1_000_000u32, live in any::<bool>()) {
            let body = format!(
                r#"{{"symbols":[{{"symbol":"XUSDT","status":"{}","baseAssetPrecision":{base},"quotePrecision":{quote},"permissions":["SPOT"],"takerCommission":"0.{taker:06}"}}]}}"#,
                if live { "1" } else { "2" }
            );
            let mut d = MexcSpotDiscovery::new();
            prop_assert_eq!(d.ingest_body(body.as_bytes()), Ok(1));
            let r = d.find(b"XUSDT").unwrap();
            prop_assert_eq!(r.trading, live);
            prop_assert_eq!(r.lot_step_1e9, 10i64.pow(9 - base as u32));
            prop_assert_eq!(r.tick_size_1e9, 10i64.pow(9 - quote as u32));
            prop_assert_eq!(r.taker_fee_1e9, taker as i64 * 1_000);
        }
    }
}
