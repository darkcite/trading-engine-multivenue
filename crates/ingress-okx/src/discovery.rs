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

    /// Parse one instruments-endpoint body (one `instType` page) into
    /// the table. Returns the number of rows added. Call once per
    /// fetched page; counts accumulate.
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, OkxDiscoveryErr> {
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
                    let (row, end) = parse_row(body, i)?;
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

/// Parse one instrument object starting at `pos` (must point at `{`).
/// Returns the row and the position after the closing `}`.
fn parse_row(body: &[u8], pos: usize) -> Result<(OkxInstrumentRow, usize), OkxDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut inst_id = [0u8; OKX_INST_ID_MAX];
    let mut inst_id_len = 0u8;
    let mut inst_type: Option<OkxInstType> = None;
    let mut live: Option<bool> = None;
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
    let row = OkxInstrumentRow {
        inst_id,
        inst_id_len,
        inst_type: inst_type.ok_or(OkxDiscoveryErr::BadRow)?,
        live,
        tick_sz_1e9: tick_sz.unwrap_or(0),
        lot_sz_1e9: lot_sz.unwrap_or(0),
        ct_val_1e9: ct_val,
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
    }
}
