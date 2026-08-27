// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # OKX boot-time REST discovery (Phase 8e, plan §4.1 + §6.1)
//!
//! Parses `GET /api/v5/public/instruments?instType=SPOT|SWAP|FUTURES`
//! bodies into a boot-only instrument table: full venue universe count
//! for the §6.1 coverage report, plus per-instrument `instType` (drives
//! WS channel gating — replaces the retired `-SWAP` suffix hack),
//! `state`, and the `tickSz`/`lotSz`/`ctVal` contract metadata that 8j
//! execution sizing will consume.
//!
//! ## Allocation note (doctrine)
//!
//! This module runs **at boot only**, where allocation is allowed. Row
//! storage is a `Vec` reserved once at [`OkxDiscovery::new`] and capped
//! at [`OKX_DISCOVERY_ROWS_CAP`] (fail-fast beyond — a venue suddenly
//! listing 10× instruments is a contract change we want to see loudly).
//! The table is dropped before the engine loop starts; nothing here is
//! reachable from a hot path.
//!
//! ## Wire shape (live-probed 2026-08-14)
//!
//! ```json
//! {"code":"0","data":[{"instId":"BTC-USDT-SWAP","instType":"SWAP",
//!   "state":"live","tickSz":"0.1","lotSz":"0.01","ctVal":"0.01",
//!   "instIdCode":10459,"futureSettlement":false,
//!   "tradeQuoteCcyList":[],...}],"msg":""}
//! ```
//!
//! Captured fields are quoted decimal strings; rows also carry bare
//! numbers, bare booleans and nested arrays which the walker skips
//! structurally ([`core_parse::skip_json_value`]). Field order is not
//! assumed. `ctVal` is empty for SPOT instruments → stored as 0.
//!
//! Pre-listing rows (`state:"preopen"`, observed live 2026-08-15 on
//! `JP225-USDT-SWAP` the day before its listing) carry EMPTY
//! `tickSz`/`lotSz` strings and a bare-`null` `instIdCode`. Empty
//! numerics are accepted (as 0) only on non-live rows; a live row
//! missing its tick/lot size is a contract violation → `BadRow`.

use core_parse::{find_field, scan_price_1e9, skip_json_value, skip_string, skip_ws};

use crate::{OkxInstType, OKX_INST_ID_MAX};

/// Hard cap on parsed instrument rows across all fetched `instType`
/// pages. Live universe 2026-08-14: SPOT 1354 + SWAP 447 + FUTURES 153
/// ≈ 2 000; 8× headroom.
pub const OKX_DISCOVERY_ROWS_CAP: usize = 16_384;

/// One discovered instrument.
#[derive(Copy, Clone, Debug)]
pub struct OkxInstrumentRow {
    /// `instId` bytes (`inst_id_len` valid).
    pub inst_id: [u8; OKX_INST_ID_MAX],
    /// Valid prefix length of `inst_id`.
    pub inst_id_len: u8,
    /// Instrument class — drives WS channel gating.
    pub inst_type: OkxInstType,
    /// `state == "live"` (anything else is not subscribable).
    pub live: bool,
    /// `tickSz` ×1e9 (`"0.1"` → `100_000_000`).
    pub tick_sz_1e9: i64,
    /// `lotSz` ×1e9.
    pub lot_sz_1e9: i64,
    /// `ctVal` ×1e9; `0` for SPOT (venue sends `""`).
    pub ct_val_1e9: i64,
    /// `optType == "C"` (M2.2 OPTION rows; false otherwise).
    pub is_call: bool,
    /// `stk` ×1e9 (OPTION rows; 0 otherwise).
    pub strike_1e9: i64,
    /// `expTime` ms since epoch (OPTION rows; captured on any row
    /// that carries it — dated futures do; 0 when absent).
    pub exp_ms: i64,
}

impl OkxInstrumentRow {
    /// The instrument id as a byte slice.
    #[inline]
    pub fn inst_id(&self) -> &[u8] {
        &self.inst_id[..self.inst_id_len as usize]
    }
}

/// Why discovery ingestion failed. All fatal at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OkxDiscoveryErr {
    /// Envelope violation: missing/failed `"code":"0"` or missing
    /// `"data":[` array.
    Envelope,
    /// A row violated the instrument-object contract (missing required
    /// key, over-long `instId`, unknown `instType`, malformed value).
    BadRow,
    /// Body ended inside the `data` array.
    Truncated,
    /// More than [`OKX_DISCOVERY_ROWS_CAP`] rows across all pages.
    TooMany,
}

/// Boot-only OKX instrument table. See module docs.
pub struct OkxDiscovery {
    rows: Vec<OkxInstrumentRow>,
    universe_live: u32,
}

impl OkxDiscovery {
    /// Empty table with capacity reserved once.
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(OKX_DISCOVERY_ROWS_CAP),
            universe_live: 0,
        }
    }

    /// Parse one LEGACY instruments-endpoint body (one
    /// `instType=SPOT|SWAP|FUTURES` page) into the table. An OPTION
    /// row here is a contract violation, exactly as before M2.2.
    /// Returns the number of rows added. Call once per fetched page;
    /// counts accumulate.
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, OkxDiscoveryErr> {
        self.ingest_inner(body, RowMode::Legacy)
    }

    /// Parse one `instType=OPTION&uly=<uly>` page (M2.2) into the
    /// table. Option rows additionally require `stk` / `expTime` /
    /// `optType`; non-OPTION rows are a contract violation. Use a
    /// SEPARATE table instance per underlying page (the legacy
    /// coverage counters must not mix with chain rows); the caller
    /// then applies [`select_capped_chain`] to `rows()`.
    pub fn ingest_options_body(&mut self, body: &[u8]) -> Result<u32, OkxDiscoveryErr> {
        self.ingest_inner(body, RowMode::Option)
    }

    fn ingest_inner(&mut self, body: &[u8], mode: RowMode) -> Result<u32, OkxDiscoveryErr> {
        // Envelope: `"code":"0"`.
        let code_pos = find_field(body, b"\"code\":").ok_or(OkxDiscoveryErr::Envelope)?;
        let c = skip_ws(body, code_pos);
        if body.len() < c + 3 || &body[c..c + 3] != b"\"0\"" {
            return Err(OkxDiscoveryErr::Envelope);
        }
        // `"data":[` array.
        let data_pos = find_field(body, b"\"data\":").ok_or(OkxDiscoveryErr::Envelope)?;
        let mut i = skip_ws(body, data_pos);
        if i >= body.len() || body[i] != b'[' {
            return Err(OkxDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(OkxDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b'{' => {
                    let (row, end) = parse_row(body, i, mode)?;
                    if self.rows.len() >= OKX_DISCOVERY_ROWS_CAP {
                        return Err(OkxDiscoveryErr::TooMany);
                    }
                    if row.live {
                        self.universe_live += 1;
                    }
                    self.rows.push(row);
                    added += 1;
                    i = skip_ws(body, end);
                    if i < body.len() && body[i] == b',' {
                        i += 1;
                    }
                }
                _ => return Err(OkxDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Look up a discovered instrument by exact `instId`.
    pub fn find(&self, inst_id: &[u8]) -> Option<&OkxInstrumentRow> {
        self.rows
            .iter()
            .find(|r| r.inst_id() == inst_id)
    }

    /// All parsed rows in wire order (M2.2: [`select_capped_chain`]
    /// input for an options table).
    #[inline]
    pub fn rows(&self) -> &[OkxInstrumentRow] {
        &self.rows
    }

    /// Total rows parsed (all states).
    #[inline]
    pub fn universe_total(&self) -> u32 {
        self.rows.len() as u32
    }

    /// Rows with `state == "live"` — the §6.1 coverage-report
    /// `universe=` figure.
    #[inline]
    pub fn universe_live(&self) -> u32 {
        self.universe_live
    }
}

impl Default for OkxDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// M2.2: index price (ATM reference) + capped-chain selection
// ---------------------------------------------------------------

/// Parse a `GET /api/v5/market/index-tickers?instId=<uly>` body into
/// the index price ×1e9 — the boot-time ATM reference for
/// [`select_capped_chain`]. Envelope law matches `ingest_body`
/// (`"code":"0"` + `"data":[`); `idxPx` is a quoted decimal string
/// (this venue quotes its numbers); empty/malformed/nonpositive →
/// [`OkxDiscoveryErr::BadRow`]. The configured underlying (`uly`,
/// e.g. `"BTC-USD"`) IS the index instId — no name mapping (unlike
/// Deribit's `ccy → ccy_usd`).
pub fn parse_index_price(body: &[u8]) -> Result<i64, OkxDiscoveryErr> {
    let code_pos = find_field(body, b"\"code\":").ok_or(OkxDiscoveryErr::Envelope)?;
    let c = skip_ws(body, code_pos);
    if body.len() < c + 3 || &body[c..c + 3] != b"\"0\"" {
        return Err(OkxDiscoveryErr::Envelope);
    }
    let data_pos = find_field(body, b"\"data\":").ok_or(OkxDiscoveryErr::Envelope)?;
    let i = skip_ws(body, data_pos);
    if i >= body.len() || body[i] != b'[' {
        return Err(OkxDiscoveryErr::Envelope);
    }
    let px_pos = find_field(body, b"\"idxPx\":").ok_or(OkxDiscoveryErr::BadRow)?;
    let j = skip_ws(body, px_pos);
    if j >= body.len() || body[j] != b'"' {
        return Err(OkxDiscoveryErr::BadRow);
    }
    let end_q = skip_string(body, j + 1).ok_or(OkxDiscoveryErr::Truncated)?;
    let span = &body[j + 1..end_q - 1];
    if span.is_empty() || span.contains(&b'\\') {
        return Err(OkxDiscoveryErr::BadRow);
    }
    let (px, used) = scan_price_1e9(span, 0).ok_or(OkxDiscoveryErr::BadRow)?;
    if used != span.len() || px <= 0 {
        return Err(OkxDiscoveryErr::BadRow);
    }
    Ok(px)
}

/// The `options_select::ChainRow` view of an OKX discovery row (M2-close
/// extraction — the shared law's read surface).
impl options_select::ChainRow for OkxInstrumentRow {
    #[inline]
    fn exp_ms(&self) -> i64 {
        self.exp_ms
    }
    #[inline]
    fn strike_1e9(&self) -> i64 {
        self.strike_1e9
    }
    #[inline]
    fn is_call(&self) -> bool {
        self.is_call
    }
}

/// Apply the capped universe policy to ONE underlying's parsed OPTION
/// rows. The selection LAW lives in `options-select` since the M2-close
/// extraction (`ingress-deribit` was the law source; this crate's tests
/// + proptests keep pinning the same invariants through this wrapper) —
/// here lives only the VENUE candidacy predicate
/// (`inst_type == Option && live && exp_ms > now_ms`) and the frozen
/// public signature. Deterministic order (expiry asc → strike asc → C
/// before P), ≤ `E × K × 2`. Precondition: `rows` is one underlying's
/// page, ingested once. Boot-only: allocates freely.
pub fn select_capped_chain(
    rows: &[OkxInstrumentRow],
    index_px_1e9: i64,
    expiries_e: u32,
    strikes_k: u32,
    now_ms: i64,
) -> Vec<OkxInstrumentRow> {
    options_select::select_capped_chain(
        rows,
        |r: &OkxInstrumentRow| {
            r.inst_type == OkxInstType::Option && r.live && r.exp_ms > now_ms
        },
        index_px_1e9,
        expiries_e,
        strikes_k,
    )
}

/// Which page a row is being parsed from (M2.2). Determines the
/// accepted `instType` set and the required-field set.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum RowMode {
    /// `instType=SPOT|SWAP|FUTURES` pages — the pre-M2.2 contract.
    Legacy,
    /// `instType=OPTION&uly=…` pages — requires `stk` / `expTime` /
    /// `optType`.
    Option,
}

/// Parse one instrument object starting at `pos` (must point at `{`).
/// Returns the row and the position after the closing `}`.
fn parse_row(
    body: &[u8],
    pos: usize,
    mode: RowMode,
) -> Result<(OkxInstrumentRow, usize), OkxDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut inst_id = [0u8; OKX_INST_ID_MAX];
    let mut inst_id_len = 0u8;
    let mut inst_type: Option<OkxInstType> = None;
    let mut live: Option<bool> = None;
    let mut is_call: Option<bool> = None;
    let mut strike: Option<i64> = None;
    let mut exp_ms: Option<i64> = None;
    // Presence and value tracked separately: pre-listing rows
    // (`state:"preopen"`, observed live 2026-08-15 on JP225-USDT-SWAP)
    // carry EMPTY `tickSz`/`lotSz` strings (and `instIdCode:null`).
    // The keys must exist on every row; empty values are only legal on
    // non-live rows (enforced at assembly below).
    let mut tick_sz_seen = false;
    let mut tick_sz: Option<i64> = None; // None = present-but-empty
    let mut lot_sz_seen = false;
    let mut lot_sz: Option<i64> = None;
    let mut ct_val: i64 = 0; // optional — SPOT sends ""

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(OkxDiscoveryErr::Truncated);
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
                // Key.
                let key_start = i + 1;
                let key_end_q = skip_string(body, key_start).ok_or(OkxDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                if i >= body.len() || body[i] != b':' {
                    return Err(OkxDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                match key {
                    b"instId" => {
                        let (s, end) = quoted_span(body, i)?;
                        if s.is_empty() || s.len() > OKX_INST_ID_MAX {
                            return Err(OkxDiscoveryErr::BadRow);
                        }
                        inst_id[..s.len()].copy_from_slice(s);
                        inst_id_len = s.len() as u8;
                        i = end;
                    }
                    b"instType" => {
                        let (s, end) = quoted_span(body, i)?;
                        inst_type =
                            Some(OkxInstType::from_bytes(s).ok_or(OkxDiscoveryErr::BadRow)?);
                        i = end;
                    }
                    b"state" => {
                        let (s, end) = quoted_span(body, i)?;
                        live = Some(s == b"live");
                        i = end;
                    }
                    b"tickSz" => {
                        let (v, end) = quoted_1e9_allow_empty(body, i)?;
                        tick_sz_seen = true;
                        tick_sz = v;
                        i = end;
                    }
                    b"lotSz" => {
                        let (v, end) = quoted_1e9_allow_empty(body, i)?;
                        lot_sz_seen = true;
                        lot_sz = v;
                        i = end;
                    }
                    b"ctVal" => {
                        // Empty string on SPOT → 0.
                        let (s, end) = quoted_span(body, i)?;
                        if !s.is_empty() {
                            let (v, used) =
                                scan_price_1e9(s, 0).ok_or(OkxDiscoveryErr::BadRow)?;
                            if used != s.len() {
                                return Err(OkxDiscoveryErr::BadRow);
                            }
                            ct_val = v;
                        }
                        i = end;
                    }
                    b"stk" => {
                        // OPTION rows: quoted decimal strike. Empty on
                        // non-option rows (venue sends "") → skipped.
                        let (s, end) = quoted_span(body, i)?;
                        if !s.is_empty() {
                            let (v, used) =
                                scan_price_1e9(s, 0).ok_or(OkxDiscoveryErr::BadRow)?;
                            if used != s.len() {
                                return Err(OkxDiscoveryErr::BadRow);
                            }
                            strike = Some(v);
                        }
                        i = end;
                    }
                    b"expTime" => {
                        // Quoted integer ms since epoch ("" on
                        // perpetual-style rows → skipped). Too large
                        // for the ×1e9 scanners — plain digit parse.
                        let (s, end) = quoted_span(body, i)?;
                        if !s.is_empty() {
                            if s.len() > 16 || !s.iter().all(|b| b.is_ascii_digit()) {
                                return Err(OkxDiscoveryErr::BadRow);
                            }
                            let mut v: i64 = 0;
                            for &d in s {
                                v = v * 10 + (d - b'0') as i64;
                            }
                            exp_ms = Some(v);
                        }
                        i = end;
                    }
                    b"optType" => {
                        // "C" / "P" on OPTION rows; "" on others.
                        let (s, end) = quoted_span(body, i)?;
                        match s {
                            b"C" => is_call = Some(true),
                            b"P" => is_call = Some(false),
                            b"" => {}
                            _ => return Err(OkxDiscoveryErr::BadRow),
                        }
                        i = end;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(OkxDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(OkxDiscoveryErr::BadRow),
        }
    }

    if inst_id_len == 0 {
        return Err(OkxDiscoveryErr::BadRow);
    }
    if !tick_sz_seen || !lot_sz_seen {
        return Err(OkxDiscoveryErr::BadRow);
    }
    let live = live.ok_or(OkxDiscoveryErr::BadRow)?;
    // A LIVE instrument without a tick/lot size is a venue contract
    // violation; a pre-listing (`preopen`) row legitimately has none —
    // stored as 0 and excluded from the live universe anyway.
    if live && (tick_sz.is_none() || lot_sz.is_none()) {
        return Err(OkxDiscoveryErr::BadRow);
    }
    let inst_type = inst_type.ok_or(OkxDiscoveryErr::BadRow)?;
    // M2.2 per-page contract: a page carries exactly the instType
    // family it was fetched with — cross rows are violations, not
    // filter cases (the Deribit RowKind precedent).
    match mode {
        RowMode::Legacy => {
            if inst_type == OkxInstType::Option {
                return Err(OkxDiscoveryErr::BadRow);
            }
        }
        RowMode::Option => {
            if inst_type != OkxInstType::Option {
                return Err(OkxDiscoveryErr::BadRow);
            }
            // Chain-defining fields are REQUIRED on option rows — a
            // row without them is unusable for the capped filter.
            if is_call.is_none() || strike.is_none() || exp_ms.is_none() {
                return Err(OkxDiscoveryErr::BadRow);
            }
        }
    }
    let row = OkxInstrumentRow {
        inst_id,
        inst_id_len,
        inst_type,
        live,
        tick_sz_1e9: tick_sz.unwrap_or(0),
        lot_sz_1e9: lot_sz.unwrap_or(0),
        ct_val_1e9: ct_val,
        is_call: is_call.unwrap_or(false),
        strike_1e9: strike.unwrap_or(0),
        exp_ms: exp_ms.unwrap_or(0),
    };
    Ok((row, i))
}

/// Read a quoted string value at `pos` (must point at `"`). Returns
/// the in-quote span and the position after the closing quote. The
/// captured OKX fields never contain escapes; a backslash inside the
/// span is rejected rather than unescaped.
fn quoted_span(body: &[u8], pos: usize) -> Result<(&[u8], usize), OkxDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'"' {
        return Err(OkxDiscoveryErr::BadRow);
    }
    let start = pos + 1;
    let end_q = skip_string(body, start).ok_or(OkxDiscoveryErr::Truncated)?;
    let span = &body[start..end_q - 1];
    if span.contains(&b'\\') {
        return Err(OkxDiscoveryErr::BadRow);
    }
    Ok((span, end_q))
}

/// Read a quoted decimal value at `pos` scaled ×1e9, tolerating an
/// empty string (`""` → `Ok((None, end))` — the pre-listing case).
/// A non-empty span must parse in full.
fn quoted_1e9_allow_empty(
    body: &[u8],
    pos: usize,
) -> Result<(Option<i64>, usize), OkxDiscoveryErr> {
    let (s, end) = quoted_span(body, pos)?;
    if s.is_empty() {
        return Ok((None, end));
    }
    let (v, used) = scan_price_1e9(s, 0).ok_or(OkxDiscoveryErr::BadRow)?;
    if used != s.len() {
        return Err(OkxDiscoveryErr::BadRow);
    }
    Ok((Some(v), end))
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed live-probe rows (2026-08-14) — one per instType, with
    /// the noise fields the walker must skip (bare number, bare bool,
    /// nested array) retained.
    const PAGE_MIXED: &[u8] = br#"{"code":"0","data":[
      {"alias":"","instId":"BTC-USDT","instIdCode":3,"instType":"SPOT","futureSettlement":false,"tradeQuoteCcyList":["USDT"],"state":"live","tickSz":"0.1","lotSz":"0.00000001","ctVal":"","upcChg":[]},
      {"instId":"BTC-USDT-SWAP","instType":"SWAP","state":"live","ctType":"linear","ctVal":"0.01","ctValCcy":"BTC","lotSz":"0.01","tickSz":"0.1","instIdCode":10459},
      {"instId":"BTC-USD-260821","instType":"FUTURES","state":"suspend","ctVal":"100","lotSz":"0.1","tickSz":"0.1"}
    ],"msg":""}"#;

    #[test]
    fn ingest_parses_all_inst_types_and_states() {
        let mut d = OkxDiscovery::new();
        let added = d.ingest_body(PAGE_MIXED).expect("parse ok");
        assert_eq!(added, 3);
        assert_eq!(d.universe_total(), 3);
        assert_eq!(d.universe_live(), 2); // FUTURES row is suspended

        let spot = d.find(b"BTC-USDT").expect("spot row");
        assert_eq!(spot.inst_type, OkxInstType::Spot);
        assert!(spot.live);
        assert_eq!(spot.tick_sz_1e9, 100_000_000);
        assert_eq!(spot.lot_sz_1e9, 10); // "0.00000001" × 1e9
        assert_eq!(spot.ct_val_1e9, 0); // "" → 0

        let swap = d.find(b"BTC-USDT-SWAP").expect("swap row");
        assert_eq!(swap.inst_type, OkxInstType::Swap);
        assert_eq!(swap.ct_val_1e9, 10_000_000); // "0.01"

        let fut = d.find(b"BTC-USD-260821").expect("futures row");
        assert_eq!(fut.inst_type, OkxInstType::Futures);
        assert!(!fut.live);
        assert_eq!(fut.ct_val_1e9, 100_000_000_000);
    }

    #[test]
    fn ingest_accumulates_across_pages() {
        let mut d = OkxDiscovery::new();
        d.ingest_body(br#"{"code":"0","data":[{"instId":"A-B","instType":"SPOT","state":"live","tickSz":"1","lotSz":"1","ctVal":""}],"msg":""}"#).unwrap();
        d.ingest_body(br#"{"code":"0","data":[{"instId":"C-D-SWAP","instType":"SWAP","state":"live","tickSz":"1","lotSz":"1","ctVal":"1"}],"msg":""}"#).unwrap();
        assert_eq!(d.universe_total(), 2);
        assert!(d.find(b"A-B").is_some());
        assert!(d.find(b"C-D-SWAP").is_some());
        assert!(d.find(b"MISSING").is_none());
    }

    #[test]
    fn ingest_rejects_error_code_envelope() {
        let mut d = OkxDiscovery::new();
        let e = d
            .ingest_body(br#"{"code":"50011","data":[],"msg":"rate limited"}"#)
            .unwrap_err();
        assert_eq!(e, OkxDiscoveryErr::Envelope);
    }

    #[test]
    fn ingest_rejects_missing_data_array() {
        let mut d = OkxDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"code":"0","msg":""}"#).unwrap_err(),
            OkxDiscoveryErr::Envelope
        );
        assert_eq!(
            d.ingest_body(br#"{"code":"0","data":{},"msg":""}"#).unwrap_err(),
            OkxDiscoveryErr::Envelope
        );
    }

    #[test]
    fn ingest_rejects_row_contract_violations() {
        let mut d = OkxDiscovery::new();
        // Missing instId.
        assert_eq!(
            d.ingest_body(br#"{"code":"0","data":[{"instType":"SPOT","state":"live","tickSz":"1","lotSz":"1"}]}"#)
                .unwrap_err(),
            OkxDiscoveryErr::BadRow
        );
        // Unknown instType (OPTION not fetched in Phase 8).
        assert_eq!(
            d.ingest_body(br#"{"code":"0","data":[{"instId":"X","instType":"OPTION","state":"live","tickSz":"1","lotSz":"1"}]}"#)
                .unwrap_err(),
            OkxDiscoveryErr::BadRow
        );
        // Missing tickSz.
        assert_eq!(
            d.ingest_body(br#"{"code":"0","data":[{"instId":"X-Y","instType":"SPOT","state":"live","lotSz":"1"}]}"#)
                .unwrap_err(),
            OkxDiscoveryErr::BadRow
        );
        // Over-long instId (33 bytes > OKX_INST_ID_MAX = 32).
        assert_eq!(
            d.ingest_body(br#"{"code":"0","data":[{"instId":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","instType":"SPOT","state":"live","tickSz":"1","lotSz":"1"}]}"#)
                .unwrap_err(),
            OkxDiscoveryErr::BadRow
        );
    }

    #[test]
    fn ingest_accepts_preopen_row_with_empty_numerics() {
        // Real wire bytes (trimmed) from the 2026-08-15 live boot:
        // pre-listing SWAP with empty tickSz/lotSz + null instIdCode.
        let mut d = OkxDiscovery::new();
        d.ingest_body(br#"{"code":"0","data":[{"alias":"","ctVal":"","ctValCcy":"","instId":"JP225-USDT-SWAP","instIdCode":null,"instType":"SWAP","listTime":"1788926400000","lotSz":"","state":"preopen","tickSz":"","uly":"","upcChg":[],"futureSettlement":false}]}"#)
            .expect("preopen row parses");
        let row = d.find(b"JP225-USDT-SWAP").expect("row stored");
        assert!(!row.live);
        assert_eq!(row.tick_sz_1e9, 0);
        assert_eq!(row.lot_sz_1e9, 0);
        assert_eq!(d.universe_live(), 0, "preopen is not tradable universe");
    }

    #[test]
    fn ingest_accepts_long_xperp_inst_ids() {
        // Live 2026-08-15: FUTURES page lists pre-market perps with
        // 27-byte ids — the old 24-byte cap rejected the whole page.
        let mut d = OkxDiscovery::new();
        d.ingest_body(br#"{"code":"0","data":[{"instId":"MOODENG-USD_UM_XPERP-310815","instType":"FUTURES","state":"live","tickSz":"0.0001","lotSz":"1","ctVal":"10"}]}"#)
            .expect("27-byte instId parses");
        assert!(d.find(b"MOODENG-USD_UM_XPERP-310815").is_some());
    }

    #[test]
    fn ingest_rejects_live_row_with_empty_tick_or_lot() {
        let mut d = OkxDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"code":"0","data":[{"instId":"X-Y","instType":"SPOT","state":"live","tickSz":"","lotSz":"1"}]}"#)
                .unwrap_err(),
            OkxDiscoveryErr::BadRow
        );
        assert_eq!(
            d.ingest_body(br#"{"code":"0","data":[{"instId":"X-Y","instType":"SPOT","state":"live","tickSz":"1","lotSz":""}]}"#)
                .unwrap_err(),
            OkxDiscoveryErr::BadRow
        );
    }

    #[test]
    fn ingest_rejects_truncated_array() {
        let mut d = OkxDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"code":"0","data":[{"instId":"X-Y","instType":"SPOT""#)
                .unwrap_err(),
            OkxDiscoveryErr::Truncated
        );
        assert_eq!(
            d.ingest_body(br#"{"code":"0","data":["#).unwrap_err(),
            OkxDiscoveryErr::Truncated
        );
    }

    #[test]
    fn ingest_enforces_rows_cap() {
        let mut d = OkxDiscovery::new();
        // Synthesize pages of tiny rows until the cap trips. Test-only
        // allocation; boot code never builds bodies.
        let row = br#"{"instId":"A-B","instType":"SPOT","state":"live","tickSz":"1","lotSz":"1"}"#;
        let mut body = Vec::with_capacity(1 << 20);
        body.extend_from_slice(br#"{"code":"0","data":["#);
        let per_page = 4096;
        for k in 0..per_page {
            if k > 0 {
                body.push(b',');
            }
            body.extend_from_slice(row);
        }
        body.extend_from_slice(b"]}");
        for _ in 0..(OKX_DISCOVERY_ROWS_CAP / per_page) {
            d.ingest_body(&body).expect("under cap");
        }
        assert_eq!(d.ingest_body(&body).unwrap_err(), OkxDiscoveryErr::TooMany);
    }

    // -----------------------------------------------------------
    // M2.2: OPTION pages + index price + capped-chain selection
    // -----------------------------------------------------------

    /// One OPTION row in the live wire shape (quoted-string numbers,
    /// per this venue). Test-only allocation.
    fn opt_row(inst_id: &str, opt_type: &str, stk: &str, exp_ms: i64, state: &str) -> String {
        format!(
            r#"{{"instId":"{inst_id}","instType":"OPTION","uly":"BTC-USD","instFamily":"BTC-USD","optType":"{opt_type}","stk":"{stk}","expTime":"{exp_ms}","state":"{state}","tickSz":"0.0001","lotSz":"1","ctVal":"1","ctType":"","listTime":"1770000000000"}}"#
        )
    }

    fn page(rows: &str) -> Vec<u8> {
        let mut body = Vec::with_capacity(rows.len() + 64);
        body.extend_from_slice(br#"{"code":"0","data":["#);
        body.extend_from_slice(rows.as_bytes());
        body.extend_from_slice(br#"],"msg":""}"#);
        body
    }

    const EXP1: i64 = 1_774_598_400_000;
    const EXP2: i64 = 1_775_203_200_000;
    const EXP3: i64 = 1_775_808_000_000;
    const NOW: i64 = 1_774_000_000_000;

    /// A 3-expiry × 4-strike × C/P grid around index 100k + noise
    /// (one suspended row, one already-expired expiry).
    fn opt_grid() -> OkxDiscovery {
        let mut rows: Vec<String> = Vec::new();
        for (e, tag) in [(EXP1, "260327"), (EXP2, "260403"), (EXP3, "260410")] {
            for s in ["90000", "95000", "100000", "105000"] {
                for t in ["C", "P"] {
                    rows.push(opt_row(&format!("BTC-USD-{tag}-{s}-{t}"), t, s, e, "live"));
                }
            }
        }
        rows.push(opt_row("BTC-USD-260327-110000-C", "C", "110000", EXP1, "suspend"));
        rows.push(opt_row("BTC-USD-OLD-90000-C", "C", "90000", NOW - 1_000, "live"));
        let mut d = OkxDiscovery::new();
        d.ingest_options_body(&page(&rows.join(","))).expect("grid parses");
        d
    }

    #[test]
    fn options_page_parses_chain_fields() {
        let d = opt_grid();
        assert_eq!(d.universe_total(), 26);
        let c = d.find(b"BTC-USD-260327-100000-C").expect("call row");
        assert_eq!(c.inst_type, OkxInstType::Option);
        assert!(c.is_call && c.live);
        assert_eq!(c.strike_1e9, 100_000_000_000_000); // "100000" × 1e9
        assert_eq!(c.exp_ms, EXP1);
        assert_eq!(c.tick_sz_1e9, 100_000); // "0.0001"
        let p = d.find(b"BTC-USD-260327-100000-P").expect("put row");
        assert!(!p.is_call);
        assert!(!d.find(b"BTC-USD-260327-110000-C").expect("suspended kept").live);
    }

    #[test]
    fn option_page_contract_both_directions() {
        // A legacy row on an OPTION page is a violation…
        let mut d = OkxDiscovery::new();
        let legacy =
            r#"{"instId":"BTC-USDT","instType":"SPOT","state":"live","tickSz":"1","lotSz":"1"}"#;
        assert_eq!(d.ingest_options_body(&page(legacy)).unwrap_err(), OkxDiscoveryErr::BadRow);
        // …and an OPTION row on a legacy page stays one (pre-M2.2 law).
        let mut d2 = OkxDiscovery::new();
        let opt = opt_row("BTC-USD-260327-100000-C", "C", "100000", EXP1, "live");
        assert_eq!(d2.ingest_body(&page(&opt)).unwrap_err(), OkxDiscoveryErr::BadRow);
    }

    #[test]
    fn option_row_missing_chain_fields_rejected() {
        let full = opt_row("BTC-USD-260327-100000-C", "C", "100000", EXP1, "live");
        for gone in [
            r#""optType":"C","#,
            r#""stk":"100000","#,
            r#""expTime":"1774598400000","#,
        ] {
            let broken = full.replacen(gone, "", 1);
            assert_ne!(broken, full, "field `{gone}` must exist in the fixture");
            let mut d = OkxDiscovery::new();
            assert_eq!(
                d.ingest_options_body(&page(&broken)).unwrap_err(),
                OkxDiscoveryErr::BadRow,
                "missing {gone} must reject"
            );
        }
        // Bad optType value; bad expTime digits.
        for (from, to) in [(r#""optType":"C""#, r#""optType":"X""#), (r#""expTime":"1774598400000""#, r#""expTime":"17abc""#)] {
            let bad = full.replacen(from, to, 1);
            assert_ne!(bad, full);
            let mut d = OkxDiscovery::new();
            assert_eq!(d.ingest_options_body(&page(&bad)).unwrap_err(), OkxDiscoveryErr::BadRow);
        }
    }

    #[test]
    fn option_fractional_strike_parses() {
        let row = opt_row("XRP-USD-260327-0d5-C", "C", "0.5", EXP1, "live");
        let mut d = OkxDiscovery::new();
        d.ingest_options_body(&page(&row)).expect("parses");
        assert_eq!(d.rows()[0].strike_1e9, 500_000_000);
    }

    #[test]
    fn index_price_parses_and_rejects() {
        let ok = br#"{"code":"0","msg":"","data":[{"instId":"BTC-USD","idxPx":"77275.53","high24h":"78000","open24h":"76000"}]}"#;
        assert_eq!(parse_index_price(ok).expect("parses"), 77_275_530_000_000);
        // Error envelope.
        let err_body = br#"{"code":"51001","msg":"instrument not exist","data":[]}"#;
        assert_eq!(parse_index_price(err_body).unwrap_err(), OkxDiscoveryErr::Envelope);
        // Missing idxPx.
        let none = br#"{"code":"0","data":[{"instId":"BTC-USD"}]}"#;
        assert_eq!(parse_index_price(none).unwrap_err(), OkxDiscoveryErr::BadRow);
        // Empty / bare-number / nonpositive forms.
        let empty = br#"{"code":"0","data":[{"idxPx":""}]}"#;
        assert_eq!(parse_index_price(empty).unwrap_err(), OkxDiscoveryErr::BadRow);
        let bare = br#"{"code":"0","data":[{"idxPx":77275.53}]}"#;
        assert_eq!(parse_index_price(bare).unwrap_err(), OkxDiscoveryErr::BadRow);
        let zero = br#"{"code":"0","data":[{"idxPx":"0"}]}"#;
        assert_eq!(parse_index_price(zero).unwrap_err(), OkxDiscoveryErr::BadRow);
    }

    fn names(sel: &[OkxInstrumentRow]) -> Vec<String> {
        sel.iter()
            .map(|r| String::from_utf8(r.inst_id().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn capped_chain_selects_e2_k2_deterministically() {
        let d = opt_grid();
        // Index 101k: at-or-below take last 1 (100k), above take
        // first 1 (105k); E=2 → EXP1 + EXP2.
        let sel = select_capped_chain(d.rows(), 101_000_000_000_000, 2, 2, NOW);
        assert_eq!(
            names(&sel),
            vec![
                "BTC-USD-260327-100000-C",
                "BTC-USD-260327-100000-P",
                "BTC-USD-260327-105000-C",
                "BTC-USD-260327-105000-P",
                "BTC-USD-260403-100000-C",
                "BTC-USD-260403-100000-P",
                "BTC-USD-260403-105000-C",
                "BTC-USD-260403-105000-P",
            ]
        );
        let sel2 = select_capped_chain(d.rows(), 101_000_000_000_000, 2, 2, NOW);
        assert_eq!(names(&sel), names(&sel2));
    }

    #[test]
    fn capped_chain_excludes_dead_expired_and_respects_cap() {
        let d = opt_grid();
        let sel = select_capped_chain(d.rows(), 100_000_000_000_000, 1, 8, NOW);
        assert_eq!(sel.len(), 8);
        for r in &sel {
            assert!(r.live && r.exp_ms == EXP1);
        }
        assert!(!names(&sel).iter().any(|n| n.contains("110000") || n.contains("OLD")));
        let all = select_capped_chain(d.rows(), 100_000_000_000_000, 4, 32, NOW);
        assert!(all.len() as u32 <= 4 * 32 * 2);
        assert_eq!(all.len(), 24);
    }

    #[test]
    fn capped_chain_one_sided_and_missing_twin() {
        let d = opt_grid();
        // Index far below every strike: first K/2 above only.
        let sel = select_capped_chain(d.rows(), 1_000_000_000, 1, 4, NOW);
        assert_eq!(
            names(&sel),
            vec![
                "BTC-USD-260327-90000-C",
                "BTC-USD-260327-90000-P",
                "BTC-USD-260327-95000-C",
                "BTC-USD-260327-95000-P"
            ]
        );
        // Missing put twin emits the call alone.
        let solo = opt_row("BTC-USD-260327-100000-C", "C", "100000", EXP1, "live");
        let mut d2 = OkxDiscovery::new();
        d2.ingest_options_body(&page(&solo)).unwrap();
        let sel2 = select_capped_chain(d2.rows(), 100_000_000_000_000, 2, 8, NOW);
        assert_eq!(names(&sel2), vec!["BTC-USD-260327-100000-C"]);
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
            let mut d = OkxDiscovery::new();
            match d.ingest_body(&input) {
                Ok(n) => {
                    prop_assert_eq!(n, d.universe_total());
                    prop_assert!(d.universe_live() <= d.universe_total());
                }
                Err(_) => {}
            }
        }

        /// M2.2: the options walker + index-price parser never panic
        /// on arbitrary bytes either (same §21.3 bar).
        #[test]
        fn options_ingest_and_index_price_never_panic(
            input in proptest::collection::vec(any::<u8>(), 0..2048),
        ) {
            let mut d = OkxDiscovery::new();
            if let Ok(n) = d.ingest_options_body(&input) {
                prop_assert_eq!(n, d.universe_total());
                prop_assert!(d.universe_live() <= d.universe_total());
            }
            let _ = parse_index_price(&input);
        }

        /// M2.2 selection invariants — the SAME properties that pin
        /// the Deribit twin (law parity): output ≤ E×K×2, only
        /// live/unexpired option candidates, deterministic order
        /// (expiry asc → strike asc → call before put).
        #[test]
        fn capped_selection_invariants(
            strikes in proptest::collection::vec(1i64..2_000_000, 1..24),
            exps in proptest::collection::vec(1i64..1_000_000, 1..6),
            e in 1u32..=4,
            k_half in 1u32..=16,
            idx in 1i64..2_000_000,
            live_mask in proptest::collection::vec(proptest::bool::ANY, 48),
        ) {
            let now_ms = 500_000i64;
            let mut rows: Vec<OkxInstrumentRow> = Vec::new();
            let mut m = 0usize;
            for &exp in &exps {
                for &s in &strikes {
                    for call in [true, false] {
                        let tag = format!("O-{exp}-{s}-{}", if call { "C" } else { "P" });
                        let tb = tag.as_bytes();
                        let mut inst_id = [0u8; crate::OKX_INST_ID_MAX];
                        let n = tb.len().min(crate::OKX_INST_ID_MAX);
                        inst_id[..n].copy_from_slice(&tb[..n]);
                        rows.push(OkxInstrumentRow {
                            inst_id,
                            inst_id_len: n as u8,
                            inst_type: OkxInstType::Option,
                            live: live_mask[m % live_mask.len()],
                            tick_sz_1e9: 100_000,
                            lot_sz_1e9: 1_000_000_000,
                            ct_val_1e9: 1_000_000_000,
                            is_call: call,
                            strike_1e9: s,
                            exp_ms: exp,
                        });
                        m += 1;
                    }
                }
            }
            let k = k_half * 2;
            let sel = select_capped_chain(&rows, idx, e, k, now_ms);
            prop_assert!(sel.len() as u32 <= e * k * 2);
            for r in &sel {
                prop_assert!(r.live && r.exp_ms > now_ms);
                prop_assert_eq!(r.inst_type, OkxInstType::Option);
            }
            for w in sel.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                let ka = (a.exp_ms, a.strike_1e9, !a.is_call);
                let kb = (b.exp_ms, b.strike_1e9, !b.is_call);
                prop_assert!(ka < kb, "order law violated");
            }
            let sel2 = select_capped_chain(&rows, idx, e, k, now_ms);
            prop_assert_eq!(sel.len(), sel2.len());
        }
    }
}
