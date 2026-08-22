//! # Binance European options — the M2.4 eapi half-ingress
//!
//! Options live on a DEDICATED endpoint family (`eapi.binance.com`
//! REST + the `nbstream.binance.com` WS host), not the spot WS this
//! crate speaks — hence "half-ingress": a second LANE inside the
//! Binance venue (the M1c usdm precedent), not a new venue. Market
//! data only (mvp-plan §4-M2 step 4): capped-chain discovery, option
//! BBO → `Tick`, mark/IV/greeks → `OptSummary` (M2.3 channel). No
//! order path anywhere in M2.
//!
//! ## Boot/offline doctrine
//!
//! Discovery + selection run at boot only — allocation permitted,
//! same as every 8e discovery module. The WS-lane parsers below the
//! discovery section are HOT (per-push on the Binance ingress
//! thread): zero-alloc flat scans over `&[u8]`, quoted-decimal
//! numbers exactly like the spot `bookTicker` wire.
//!
//! ## Wire shapes (documented; live-verified at the M2.4 smoke —
//! pitfall #11, raw tap armed)
//!
//! REST `GET /eapi/v1/exchangeInfo` (ONE page, ALL underlyings):
//! `{"optionSymbols":[{"symbol":"BTC-260327-100000-C",
//!   "underlying":"BTCUSDT","strikePrice":"100000.00000000",
//!   "expiryDate":1774598400000,"side":"CALL","filters":[…],…},…]}`
//! — `strikePrice` QUOTED decimal, `expiryDate` BARE ms integer,
//! `side` `"CALL"|"PUT"`; `filters`/noise skipped structurally.
//!
//! REST `GET /eapi/v1/index?underlying=BTCUSDT`:
//! `{"time":…,"indexPrice":"77000.12"}` — quoted decimal.
//!
//! WS combined stream (`/stream?streams=a/b/…` — NO subscribe
//! frames, the crate's standing no-ack pattern):
//! `{"stream":"btc-260327-100000-c@ticker","data":{…}}` with ticker
//! data carrying quoted decimals: `bo`/`ao`/`bq`/`aq` (BBO),
//! `mp` (mark px), `vo` (mark IV, fraction), `d`/`g`/`v`/`t`
//! (greeks). `{"stream":"btcusdt@index","data":{"p":"77000.1"}}`
//! feeds the per-underlying index cache (the record's underlying px).
//! eapi has NO open-interest stream — `OptSummary.flags` carries
//! MARK_PX only (the OKX-asymmetry mechanism, docs/wire-format.md).

use core_parse::{find_field, scan_number_sci_1e9, scan_price_1e6, scan_u64, skip_json_value, skip_string, skip_ws};
use core_types::SymbolId;

// ---------------------------------------------------------------
// Constants
// ---------------------------------------------------------------

/// Longest eapi option symbol accepted (`BTC-260327-100000-C` = 20;
/// margin for long underlyings).
pub const EAPI_SYM_MAX: usize = 32;

/// Longest underlying accepted (`BTCUSDT` = 7).
pub const EAPI_ULY_MAX: usize = 16;

/// Max boot-DISCOVERED option instruments on the eapi lane — the
/// default policy (2 underlyings × E2 × K8 × C/P = 64) exactly, the
/// Deribit/OKX precedent.
pub const EAPI_OPT_MAX: usize = 64;

/// Max configured underlyings (core-config caps at 16).
pub const EAPI_ULYS_MAX: usize = 16;

/// Hard cap on parsed exchangeInfo option rows. Live eapi universe is
/// order-1k symbols across all underlyings; 8× headroom.
pub const EAPI_DISCOVERY_ROWS_CAP: usize = 8192;

// ---------------------------------------------------------------
// Discovery (boot-only; allocation permitted)
// ---------------------------------------------------------------

/// One discovered eapi option instrument.
#[derive(Copy, Clone, Debug)]
pub struct EapiOptionRow {
    /// `symbol` bytes as listed (venue case; `symbol_len` valid).
    pub symbol: [u8; EAPI_SYM_MAX],
    /// Valid prefix length of `symbol`.
    pub symbol_len: u8,
    /// `underlying` bytes (`underlying_len` valid).
    pub underlying: [u8; EAPI_ULY_MAX],
    /// Valid prefix length of `underlying`.
    pub underlying_len: u8,
    /// `side == "CALL"`.
    pub is_call: bool,
    /// `strikePrice` ×1e9 (quoted decimal on this wire).
    pub strike_1e9: i64,
    /// `expiryDate` ms since epoch (bare integer on this wire).
    pub expiry_ms: i64,
}

impl EapiOptionRow {
    /// The symbol as a byte slice.
    #[inline]
    pub fn symbol(&self) -> &[u8] {
        &self.symbol[..self.symbol_len as usize]
    }

    /// The underlying as a byte slice.
    #[inline]
    pub fn underlying(&self) -> &[u8] {
        &self.underlying[..self.underlying_len as usize]
    }
}

/// Why eapi discovery ingestion failed. All fatal at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EapiDiscoveryErr {
    /// Missing `"optionSymbols":[` array.
    Envelope,
    /// A row violated the option-object contract (missing required
    /// key, over-long symbol/underlying, bad side, malformed value).
    BadRow,
    /// Body ended inside the array.
    Truncated,
    /// More than [`EAPI_DISCOVERY_ROWS_CAP`] rows.
    TooMany,
}

/// Boot-only eapi option table (ONE exchangeInfo page carries every
/// underlying — [`select_capped_chain`] filters per underlying).
pub struct EapiDiscovery {
    rows: Vec<EapiOptionRow>,
}

impl EapiDiscovery {
    /// Empty table with capacity reserved once.
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(EAPI_DISCOVERY_ROWS_CAP),
        }
    }

    /// Parse one `exchangeInfo` body into the table. Returns rows
    /// added.
    pub fn ingest_exchange_info(&mut self, body: &[u8]) -> Result<u32, EapiDiscoveryErr> {
        let arr_pos =
            find_field(body, b"\"optionSymbols\":").ok_or(EapiDiscoveryErr::Envelope)?;
        let mut i = skip_ws(body, arr_pos);
        if i >= body.len() || body[i] != b'[' {
            return Err(EapiDiscoveryErr::Envelope);
        }
        i += 1;
        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(EapiDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b',' => i += 1,
                b'{' => {
                    let (row, end) = parse_option_row(body, i)?;
                    if self.rows.len() >= EAPI_DISCOVERY_ROWS_CAP {
                        return Err(EapiDiscoveryErr::TooMany);
                    }
                    self.rows.push(row);
                    added += 1;
                    i = end;
                }
                _ => return Err(EapiDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// All parsed rows in wire order.
    #[inline]
    pub fn rows(&self) -> &[EapiOptionRow] {
        &self.rows
    }

    /// Total rows parsed.
    #[inline]
    pub fn universe_total(&self) -> u32 {
        self.rows.len() as u32
    }
}

impl Default for EapiDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse one option object at `pos` (must point at `{`). Returns the
/// row and the position after the closing `}`.
fn parse_option_row(body: &[u8], pos: usize) -> Result<(EapiOptionRow, usize), EapiDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut symbol = [0u8; EAPI_SYM_MAX];
    let mut symbol_len = 0u8;
    let mut underlying = [0u8; EAPI_ULY_MAX];
    let mut underlying_len = 0u8;
    let mut is_call: Option<bool> = None;
    let mut strike: Option<i64> = None;
    let mut expiry: Option<i64> = None;

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(EapiDiscoveryErr::Truncated);
        }
        match body[i] {
            b'}' => {
                i += 1;
                break;
            }
            b',' => i += 1,
            b'"' => {
                let key_start = i + 1;
                let key_end_q =
                    skip_string(body, key_start).ok_or(EapiDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                if i >= body.len() || body[i] != b':' {
                    return Err(EapiDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                match key {
                    b"symbol" => {
                        let (s, end) = quoted_span(body, i)?;
                        if s.is_empty() || s.len() > EAPI_SYM_MAX {
                            return Err(EapiDiscoveryErr::BadRow);
                        }
                        symbol[..s.len()].copy_from_slice(s);
                        symbol_len = s.len() as u8;
                        i = end;
                    }
                    b"underlying" => {
                        let (s, end) = quoted_span(body, i)?;
                        if s.is_empty() || s.len() > EAPI_ULY_MAX {
                            return Err(EapiDiscoveryErr::BadRow);
                        }
                        underlying[..s.len()].copy_from_slice(s);
                        underlying_len = s.len() as u8;
                        i = end;
                    }
                    b"side" => {
                        let (s, end) = quoted_span(body, i)?;
                        is_call = Some(match s {
                            b"CALL" => true,
                            b"PUT" => false,
                            _ => return Err(EapiDiscoveryErr::BadRow),
                        });
                        i = end;
                    }
                    b"strikePrice" => {
                        // Quoted decimal on this wire.
                        let (s, end) = quoted_span(body, i)?;
                        if s.is_empty() {
                            return Err(EapiDiscoveryErr::BadRow);
                        }
                        let (v, used) =
                            scan_number_sci_1e9(s, 0).ok_or(EapiDiscoveryErr::BadRow)?;
                        if used != s.len() {
                            return Err(EapiDiscoveryErr::BadRow);
                        }
                        strike = Some(v);
                        i = end;
                    }
                    b"expiryDate" => {
                        // Bare ms integer — too large for the ×1e9
                        // scanners.
                        let (v, end) = scan_u64(body, i).ok_or(EapiDiscoveryErr::BadRow)?;
                        if v > i64::MAX as u64 {
                            return Err(EapiDiscoveryErr::BadRow);
                        }
                        expiry = Some(v as i64);
                        i = end;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(EapiDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(EapiDiscoveryErr::BadRow),
        }
    }

    if symbol_len == 0 || underlying_len == 0 {
        return Err(EapiDiscoveryErr::BadRow);
    }
    let row = EapiOptionRow {
        symbol,
        symbol_len,
        underlying,
        underlying_len,
        is_call: is_call.ok_or(EapiDiscoveryErr::BadRow)?,
        strike_1e9: strike.ok_or(EapiDiscoveryErr::BadRow)?,
        expiry_ms: expiry.ok_or(EapiDiscoveryErr::BadRow)?,
    };
    Ok((row, i))
}

/// Read a quoted string value at `pos` (must point at `"`).
fn quoted_span(body: &[u8], pos: usize) -> Result<(&[u8], usize), EapiDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'"' {
        return Err(EapiDiscoveryErr::BadRow);
    }
    let start = pos + 1;
    let end_q = skip_string(body, start).ok_or(EapiDiscoveryErr::Truncated)?;
    let span = &body[start..end_q - 1];
    if span.contains(&b'\\') {
        return Err(EapiDiscoveryErr::BadRow);
    }
    Ok((span, end_q))
}

/// Parse a `GET /eapi/v1/index?underlying=<uly>` body into the index
/// price ×1e9 — the boot-time ATM reference. `indexPrice` is a quoted
/// decimal; missing/empty/nonpositive → [`EapiDiscoveryErr::BadRow`].
pub fn parse_index_price(body: &[u8]) -> Result<i64, EapiDiscoveryErr> {
    let pos = find_field(body, b"\"indexPrice\":").ok_or(EapiDiscoveryErr::BadRow)?;
    let i = skip_ws(body, pos);
    if i >= body.len() || body[i] != b'"' {
        return Err(EapiDiscoveryErr::BadRow);
    }
    let end_q = skip_string(body, i + 1).ok_or(EapiDiscoveryErr::Truncated)?;
    let span = &body[i + 1..end_q - 1];
    if span.is_empty() || span.contains(&b'\\') {
        return Err(EapiDiscoveryErr::BadRow);
    }
    let (px, used) = scan_number_sci_1e9(span, 0).ok_or(EapiDiscoveryErr::BadRow)?;
    if used != span.len() || px <= 0 {
        return Err(EapiDiscoveryErr::BadRow);
    }
    Ok(px)
}

/// Apply the capped universe policy to ONE underlying's option rows —
/// the M2 selection LAW, third twin of
/// `ingress_deribit::discovery::select_capped_chain` (the law source;
/// identical property invariants pin all three; the eapi variant adds
/// the `underlying` filter because ONE exchangeInfo page carries every
/// family):
///
/// - candidates: `row.underlying == underlying && expiry_ms > now_ms`
///   (eapi lists tradable symbols only — no per-row state field);
/// - nearest `expiries_e` distinct expiries asc; per expiry the
///   `strikes_k` nearest-ATM strikes POSITION-BASED (last K/2
///   at-or-below + first K/2 above; no backfill); calls then puts.
///
/// Deterministic order = the allocation order; ≤ `E × K × 2` by
/// construction. Boot-only: allocates freely.
pub fn select_capped_chain(
    rows: &[EapiOptionRow],
    underlying: &[u8],
    index_px_1e9: i64,
    expiries_e: u32,
    strikes_k: u32,
    now_ms: i64,
) -> Vec<EapiOptionRow> {
    let mut out: Vec<EapiOptionRow> = Vec::new();
    if expiries_e == 0 || strikes_k == 0 {
        return out;
    }
    let is_candidate =
        |r: &EapiOptionRow| r.underlying() == underlying && r.expiry_ms > now_ms;

    let mut expiries: Vec<i64> = Vec::new();
    for r in rows {
        if is_candidate(r) && !expiries.contains(&r.expiry_ms) {
            expiries.push(r.expiry_ms);
        }
    }
    expiries.sort_unstable();
    expiries.truncate(expiries_e as usize);

    let half = (strikes_k / 2) as usize;
    for &exp in &expiries {
        let mut strikes: Vec<i64> = Vec::new();
        for r in rows {
            if is_candidate(r) && r.expiry_ms == exp && !strikes.contains(&r.strike_1e9) {
                strikes.push(r.strike_1e9);
            }
        }
        strikes.sort_unstable();
        let below_end = strikes.partition_point(|&s| s <= index_px_1e9);
        let lo = below_end.saturating_sub(half);
        let hi = (below_end + half).min(strikes.len());
        for &strike in &strikes[lo..hi] {
            for want_call in [true, false] {
                for r in rows {
                    if is_candidate(r)
                        && r.expiry_ms == exp
                        && r.strike_1e9 == strike
                        && r.is_call == want_call
                    {
                        out.push(*r);
                        break;
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------
// WS-lane state (boot-built, hot-read)
// ---------------------------------------------------------------

/// Fixed-capacity `lowercased stream symbol → (SymbolId, uly idx)`
/// map for the eapi combined stream. Built at boot; read per push on
/// the Binance ingress thread (linear scan ≤ 64 rows — the
/// Deribit-table cost note applies).
pub struct EapiSymbolTable {
    rows: [(u8, [u8; EAPI_SYM_MAX], SymbolId, u8); EAPI_OPT_MAX],
    len: usize,
}

/// Why an [`EapiSymbolTable::insert`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EapiTableErr {
    /// All [`EAPI_OPT_MAX`] rows in use.
    Full,
    /// Symbol longer than [`EAPI_SYM_MAX`] or empty.
    BadSymbol,
    /// `uly_idx` out of the configured-underlyings range.
    BadUly,
}

impl EapiSymbolTable {
    /// Empty table.
    pub const fn new() -> Self {
        Self {
            rows: [(0, [0; EAPI_SYM_MAX], 0, 0); EAPI_OPT_MAX],
            len: 0,
        }
    }

    /// Register `symbol → (sym, uly_idx)`, LOWERCASING the symbol to
    /// the stream-name form. Boot-time only.
    pub fn insert(
        &mut self,
        symbol: &[u8],
        sym: SymbolId,
        uly_idx: u8,
    ) -> Result<(), EapiTableErr> {
        if symbol.is_empty() || symbol.len() > EAPI_SYM_MAX {
            return Err(EapiTableErr::BadSymbol);
        }
        if uly_idx as usize >= EAPI_ULYS_MAX {
            return Err(EapiTableErr::BadUly);
        }
        if self.len >= EAPI_OPT_MAX {
            return Err(EapiTableErr::Full);
        }
        let row = &mut self.rows[self.len];
        row.0 = symbol.len() as u8;
        let mut j = 0;
        while j < symbol.len() {
            row.1[j] = symbol[j].to_ascii_lowercase();
            j += 1;
        }
        row.2 = sym;
        row.3 = uly_idx;
        self.len += 1;
        Ok(())
    }

    /// Resolve a lowercased stream symbol. Hot path: length gate then
    /// bytewise compare.
    #[inline]
    pub fn lookup(&self, stream_sym: &[u8]) -> Option<(SymbolId, u8)> {
        let n = stream_sym.len();
        let mut i = 0;
        while i < self.len {
            let row = &self.rows[i];
            if row.0 as usize == n && &row.1[..n] == stream_sym {
                return Some((row.2, row.3));
            }
            i += 1;
        }
        None
    }

    /// Row accessor (combined-path building): `(stream_sym, sym)`.
    #[inline]
    pub fn get(&self, idx: usize) -> Option<(&[u8], SymbolId)> {
        if idx >= self.len {
            return None;
        }
        let row = &self.rows[idx];
        Some((&row.1[..row.0 as usize], row.2))
    }

    /// Registered rows.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when no rows are registered.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for EapiSymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// WS-lane parsers (HOT: zero-alloc flat scans)
// ---------------------------------------------------------------

/// Split a combined-stream envelope into `(stream_name, data_tail)`.
/// The tail starts at the `data` value — parsers scan within it.
/// Returns `None` when either key is absent (control frames etc.).
#[inline]
pub fn split_combined(payload: &[u8]) -> Option<(&[u8], &[u8])> {
    let s_pos = find_field(payload, b"\"stream\":")?;
    let s = skip_ws(payload, s_pos);
    if s >= payload.len() || payload[s] != b'"' {
        return None;
    }
    let end_q = skip_string(payload, s + 1)?;
    let name = &payload[s + 1..end_q - 1];
    let d_pos = find_field(payload, b"\"data\":")?;
    Some((name, &payload[d_pos..]))
}

/// Parsed eapi option `<symbol>@ticker` data (the ONE stream carrying
/// BOTH the BBO and the mark/IV/greeks surface). `Copy` POD.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct EapiTickerFrame {
    /// `bo`/`bq` best bid ×1e6 (USDT premium).
    pub bid_px_1e6: i64,
    /// Best bid quantity ×1e6.
    pub bid_qty_1e6: i64,
    /// `ao`/`aq` best ask ×1e6.
    pub ask_px_1e6: i64,
    /// Best ask quantity ×1e6.
    pub ask_qty_1e6: i64,
    /// `mp` mark price ×1e9.
    pub mark_px_1e9: i64,
    /// `vo` mark implied volatility, fraction ×1e9.
    pub mark_iv_1e9: i64,
    /// `d` delta ×1e9.
    pub delta_1e9: i64,
    /// `g` gamma ×1e9.
    pub gamma_1e9: i64,
    /// `v` vega ×1e6.
    pub vega_1e6: i64,
    /// `t` theta ×1e6.
    pub theta_1e6: i64,
}

/// Parse one eapi option ticker `data` object. Every captured value
/// is a QUOTED decimal (this venue quotes its numbers); single-char
/// keys are anchored `"x":` so they can never alias the two-char
/// forms (`"b":` ≠ `"bo":`/`"bq":`). The mark/IV/greeks surface is
/// REQUIRED (missing/malformed ⇒ `None`); the four BBO fields are
/// OPTIONAL — a quiet far option can carry empty/absent quotes, which
/// parse as 0 (the one-sided/empty-book precedent; the lane skips the
/// `Tick` when both sides are zero and still captures the summary).
#[inline]
pub fn parse_eapi_ticker(data: &[u8]) -> Option<EapiTickerFrame> {
    #[inline]
    fn q_span<'a>(data: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
        let pos = find_field(data, key)?;
        let i = skip_ws(data, pos);
        if i >= data.len() || data[i] != b'"' {
            return None;
        }
        let end_q = skip_string(data, i + 1)?;
        Some(&data[i + 1..end_q - 1])
    }
    #[inline]
    fn q_1e9(data: &[u8], key: &[u8]) -> Option<i64> {
        let span = q_span(data, key)?;
        if span.is_empty() {
            return None;
        }
        let (v, used) = scan_number_sci_1e9(span, 0)?;
        if used != span.len() {
            return None;
        }
        Some(v)
    }
    #[inline]
    fn q_1e6_or_zero(data: &[u8], key: &[u8]) -> i64 {
        match q_span(data, key) {
            None => 0,
            Some(span) if span.is_empty() => 0,
            Some(span) => match scan_price_1e6(span, 0) {
                Some((v, used)) if used == span.len() => v,
                _ => 0,
            },
        }
    }
    Some(EapiTickerFrame {
        bid_px_1e6: q_1e6_or_zero(data, b"\"bo\":"),
        bid_qty_1e6: q_1e6_or_zero(data, b"\"bq\":"),
        ask_px_1e6: q_1e6_or_zero(data, b"\"ao\":"),
        ask_qty_1e6: q_1e6_or_zero(data, b"\"aq\":"),
        mark_px_1e9: q_1e9(data, b"\"mp\":")?,
        mark_iv_1e9: q_1e9(data, b"\"vo\":")?,
        delta_1e9: q_1e9(data, b"\"d\":")?,
        gamma_1e9: q_1e9(data, b"\"g\":")?,
        vega_1e6: q_1e9(data, b"\"v\":")? / 1000,
        theta_1e6: q_1e9(data, b"\"t\":")? / 1000,
    })
}

// ---------------------------------------------------------------
// WS lane state (boot-built; index cache written on index pushes)
// ---------------------------------------------------------------

/// The eapi combined-stream lane state carried by a Binance `Driver`
/// slot (M2.4): the option symbol table + the per-underlying index
/// cache that fills `OptSummary.underlying_px_1e9`. Boot-built;
/// single-writer on the Binance ingress thread. The index cache
/// PERSISTS across reconnects (last-known reference; refreshed by the
/// first index push of the new session).
pub struct EapiLane {
    /// Lowercased stream-symbol → (sym, uly idx).
    pub table: EapiSymbolTable,
    ulys: [(u8, [u8; EAPI_ULY_MAX]); EAPI_ULYS_MAX],
    n_ulys: u8,
    idx_px_1e9: [i64; EAPI_ULYS_MAX],
}

impl EapiLane {
    /// Build from the boot table + configured underlyings (lowercased
    /// to the stream form). Over-long/overflowing entries are dropped
    /// with a debug assert (config caps both upstream).
    pub fn new(table: EapiSymbolTable, ulys: &[&[u8]]) -> Self {
        let mut u: [(u8, [u8; EAPI_ULY_MAX]); EAPI_ULYS_MAX] =
            [(0, [0; EAPI_ULY_MAX]); EAPI_ULYS_MAX];
        let mut n = 0usize;
        let mut i = 0;
        while i < ulys.len() {
            let s = ulys[i];
            if s.is_empty() || s.len() > EAPI_ULY_MAX || n >= EAPI_ULYS_MAX {
                debug_assert!(false, "uly entry dropped (len/cap) — config caps this");
                i += 1;
                continue;
            }
            u[n].0 = s.len() as u8;
            let mut j = 0;
            while j < s.len() {
                u[n].1[j] = s[j].to_ascii_lowercase();
                j += 1;
            }
            n += 1;
            i += 1;
        }
        Self {
            table,
            ulys: u,
            n_ulys: n as u8,
            idx_px_1e9: [0; EAPI_ULYS_MAX],
        }
    }

    /// Resolve a lowercased stream underlying (`btcusdt` from
    /// `btcusdt@index`) to its cache index.
    #[inline]
    pub fn uly_lookup(&self, stream_uly: &[u8]) -> Option<u8> {
        let n = stream_uly.len();
        let mut i = 0;
        while (i as u8) < self.n_ulys {
            let row = &self.ulys[i];
            if row.0 as usize == n && &row.1[..n] == stream_uly {
                return Some(i as u8);
            }
            i += 1;
        }
        None
    }

    /// Last-known index price ×1e9 for `uly_idx` (0 = none seen yet —
    /// the record carries 0 until the first index push).
    #[inline]
    pub fn index_px(&self, uly_idx: u8) -> i64 {
        debug_assert!((uly_idx as usize) < EAPI_ULYS_MAX);
        self.idx_px_1e9[(uly_idx as usize) & (EAPI_ULYS_MAX - 1)]
    }

    /// Record an index push.
    #[inline]
    pub fn set_index_px(&mut self, uly_idx: u8, px_1e9: i64) {
        debug_assert!((uly_idx as usize) < EAPI_ULYS_MAX);
        self.idx_px_1e9[(uly_idx as usize) & (EAPI_ULYS_MAX - 1)] = px_1e9;
    }
}

/// Parse an eapi `<underlying>@index` data object into the index
/// price ×1e9 (`p`, quoted decimal). Feeds the per-underlying cache
/// that fills `OptSummary.underlying_px_1e9`.
#[inline]
pub fn parse_eapi_index(data: &[u8]) -> Option<i64> {
    let pos = find_field(data, b"\"p\":")?;
    let i = skip_ws(data, pos);
    if i >= data.len() || data[i] != b'"' {
        return None;
    }
    let end_q = skip_string(data, i + 1)?;
    let span = &data[i + 1..end_q - 1];
    if span.is_empty() {
        return None;
    }
    let (v, used) = scan_number_sci_1e9(span, 0)?;
    if used != span.len() || v <= 0 {
        return None;
    }
    Some(v)
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn info(rows: &str) -> Vec<u8> {
        let mut b = Vec::with_capacity(rows.len() + 128);
        b.extend_from_slice(br#"{"timezone":"UTC","serverTime":1774000000000,"optionContracts":[],"optionAssets":[],"optionSymbols":["#);
        b.extend_from_slice(rows.as_bytes());
        b.extend_from_slice(br#"],"rateLimits":[]}"#);
        b
    }

    fn opt_row(sym: &str, uly: &str, side: &str, strike: &str, exp: i64) -> String {
        format!(
            r#"{{"contractId":3,"expiryDate":{exp},"filters":[{{"filterType":"PRICE_FILTER","minPrice":"0.02","maxPrice":"80000.01","tickSize":"0.01"}},{{"filterType":"LOT_SIZE","minQty":"0.01","maxQty":"100","stepSize":"0.01"}}],"id":2474,"symbol":"{sym}","side":"{side}","strikePrice":"{strike}","underlying":"{uly}","unit":1,"makerFeeRate":"0.0002","takerFeeRate":"0.0002","minQty":"0.01","maxQty":"100","initialMargin":"0.15","maintenanceMargin":"0.075","minInitialMargin":"0.1","minMaintenanceMargin":"0.05","priceScale":2,"quantityScale":2,"quoteAsset":"USDT"}}"#
        )
    }

    const EXP1: i64 = 1_774_598_400_000;
    const EXP2: i64 = 1_775_203_200_000;
    const NOW: i64 = 1_774_000_000_000;

    #[test]
    fn exchange_info_parses_option_rows_and_skips_filters() {
        let rows = [
            opt_row("BTC-260327-100000-C", "BTCUSDT", "CALL", "100000.00000000", EXP1),
            opt_row("BTC-260327-100000-P", "BTCUSDT", "PUT", "100000.00000000", EXP1),
            opt_row("ETH-260327-2400-C", "ETHUSDT", "CALL", "2400.5", EXP2),
        ]
        .join(",");
        let mut d = EapiDiscovery::new();
        let n = d.ingest_exchange_info(&info(&rows)).expect("parses");
        assert_eq!(n, 3);
        let r = &d.rows()[0];
        assert_eq!(r.symbol(), b"BTC-260327-100000-C");
        assert_eq!(r.underlying(), b"BTCUSDT");
        assert!(r.is_call);
        assert_eq!(r.strike_1e9, 100_000_000_000_000);
        assert_eq!(r.expiry_ms, EXP1);
        assert!(!d.rows()[1].is_call);
        assert_eq!(d.rows()[2].strike_1e9, 2_400_500_000_000);
    }

    #[test]
    fn exchange_info_rejects_contract_violations() {
        // Missing side.
        let bad = opt_row("BTC-1-C", "BTCUSDT", "CALL", "1", EXP1).replacen(r#""side":"CALL","#, "", 1);
        let mut d = EapiDiscovery::new();
        assert_eq!(d.ingest_exchange_info(&info(&bad)).unwrap_err(), EapiDiscoveryErr::BadRow);
        // Bad side value.
        let bad = opt_row("BTC-1-C", "BTCUSDT", "STRADDLE", "1", EXP1);
        let mut d = EapiDiscovery::new();
        assert_eq!(d.ingest_exchange_info(&info(&bad)).unwrap_err(), EapiDiscoveryErr::BadRow);
        // Bare (unquoted) strike = contract change.
        let bad = opt_row("BTC-1-C", "BTCUSDT", "CALL", "1", EXP1)
            .replacen(r#""strikePrice":"1""#, r#""strikePrice":1"#, 1);
        let mut d = EapiDiscovery::new();
        assert_eq!(d.ingest_exchange_info(&info(&bad)).unwrap_err(), EapiDiscoveryErr::BadRow);
        // No optionSymbols array.
        let mut d = EapiDiscovery::new();
        assert_eq!(
            d.ingest_exchange_info(br#"{"symbols":[]}"#).unwrap_err(),
            EapiDiscoveryErr::Envelope
        );
        // Truncated inside the array.
        let mut d = EapiDiscovery::new();
        assert_eq!(
            d.ingest_exchange_info(br#"{"optionSymbols":[{"symbol":"X""#).unwrap_err(),
            EapiDiscoveryErr::Truncated
        );
    }

    #[test]
    fn index_price_parses_and_rejects() {
        assert_eq!(
            parse_index_price(br#"{"time":1774000000000,"indexPrice":"77000.12"}"#).unwrap(),
            77_000_120_000_000
        );
        assert!(parse_index_price(br#"{"indexPrice":""}"#).is_err());
        assert!(parse_index_price(br#"{"indexPrice":77000.12}"#).is_err());
        assert!(parse_index_price(br#"{"indexPrice":"0"}"#).is_err());
        assert!(parse_index_price(br#"{"time":1}"#).is_err());
    }

    fn grid() -> EapiDiscovery {
        let mut rows: Vec<String> = Vec::new();
        for (e, tag) in [(EXP1, "260327"), (EXP2, "260403")] {
            for s in ["90000", "95000", "100000", "105000"] {
                for (side, suf) in [("CALL", "C"), ("PUT", "P")] {
                    rows.push(opt_row(
                        &format!("BTC-{tag}-{s}-{suf}"),
                        "BTCUSDT",
                        side,
                        s,
                        e,
                    ));
                }
            }
        }
        // A second family that must never leak into BTCUSDT selection.
        rows.push(opt_row("ETH-260327-2400-C", "ETHUSDT", "CALL", "2400", EXP1));
        // An expired row.
        rows.push(opt_row("BTC-OLD-90000-C", "BTCUSDT", "CALL", "90000", NOW - 1_000));
        let mut d = EapiDiscovery::new();
        d.ingest_exchange_info(&info(&rows.join(","))).expect("grid parses");
        d
    }

    fn names(sel: &[EapiOptionRow]) -> Vec<String> {
        sel.iter()
            .map(|r| String::from_utf8(r.symbol().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn capped_chain_selects_per_underlying_deterministically() {
        let d = grid();
        let sel = select_capped_chain(d.rows(), b"BTCUSDT", 101_000_000_000_000, 1, 2, NOW);
        assert_eq!(
            names(&sel),
            vec![
                "BTC-260327-100000-C",
                "BTC-260327-100000-P",
                "BTC-260327-105000-C",
                "BTC-260327-105000-P",
            ]
        );
        // The other family and expired rows never leak; cap law holds.
        let all = select_capped_chain(d.rows(), b"BTCUSDT", 100_000_000_000_000, 4, 32, NOW);
        assert!(all.len() as u32 <= 4 * 32 * 2);
        assert_eq!(all.len(), 16); // 2 expiries × 4 strikes × 2
        assert!(!names(&all).iter().any(|n| n.contains("ETH") || n.contains("OLD")));
        // Determinism.
        let again = select_capped_chain(d.rows(), b"BTCUSDT", 100_000_000_000_000, 4, 32, NOW);
        assert_eq!(names(&all), names(&again));
    }

    #[test]
    fn symbol_table_lowercases_and_resolves() {
        let mut t = EapiSymbolTable::new();
        t.insert(b"BTC-260327-100000-C", (1 << 24) | 1025, 0).unwrap();
        assert_eq!(t.lookup(b"btc-260327-100000-c"), Some(((1 << 24) | 1025, 0)));
        assert_eq!(t.lookup(b"BTC-260327-100000-C"), None); // stream form only
        assert_eq!(t.lookup(b"missing"), None);
        assert_eq!(t.insert(b"", 1, 0), Err(EapiTableErr::BadSymbol));
        assert_eq!(t.insert(b"X", 1, 16), Err(EapiTableErr::BadUly));
        let mut full = EapiSymbolTable::new();
        for i in 0..EAPI_OPT_MAX {
            full.insert(format!("S{i}").as_bytes(), i as u32, 0).unwrap();
        }
        assert_eq!(full.insert(b"OVER", 99, 0), Err(EapiTableErr::Full));
    }

    #[test]
    fn combined_split_and_ticker_parse() {
        let payload = br#"{"stream":"btc-260327-100000-c@ticker","data":{"e":"24hrTicker","E":1774000001000,"T":1774000000900,"s":"BTC-260327-100000-C","o":"2000","h":"2100","l":"1900","c":"2050","V":"10","A":"20000","P":"0.025","p":"50","Q":"0.5","F":"1","L":"99","n":99,"bo":"2040.5","ao":"2060.1","bq":"1.25","aq":"0.75","b":"0.62","a":"0.68","d":"0.512","t":"-85.3","g":"0.0000123","v":"152.3","vo":"0.6543","mp":"2051.2","hl":"4000","ll":"100","eep":"77000"}}"#;
        let (stream, data) = split_combined(payload).expect("splits");
        assert_eq!(stream, b"btc-260327-100000-c@ticker");
        let f = parse_eapi_ticker(data).expect("parses");
        assert_eq!(f.bid_px_1e6, 2_040_500_000);
        assert_eq!(f.ask_px_1e6, 2_060_100_000);
        assert_eq!(f.bid_qty_1e6, 1_250_000);
        assert_eq!(f.ask_qty_1e6, 750_000);
        assert_eq!(f.mark_px_1e9, 2_051_200_000_000);
        assert_eq!(f.mark_iv_1e9, 654_300_000); // vo — NOT b/a (bid/ask IV)
        assert_eq!(f.delta_1e9, 512_000_000);
        assert_eq!(f.gamma_1e9, 12_300);
        assert_eq!(f.vega_1e6, 152_300_000);
        assert_eq!(f.theta_1e6, -85_300_000);
        // Missing any required field rejects.
        let no_mp = payload
            .iter()
            .copied()
            .collect::<Vec<u8>>();
        let no_mp = String::from_utf8(no_mp).unwrap().replacen(r#""mp":"2051.2","#, "", 1);
        let (_, data2) = split_combined(no_mp.as_bytes()).unwrap();
        assert!(parse_eapi_ticker(data2).is_none());
        // Index push.
        let idx = br#"{"stream":"btcusdt@index","data":{"e":"index","E":1774000001000,"s":"BTCUSDT","p":"77000.15"}}"#;
        let (s2, d2) = split_combined(idx).unwrap();
        assert_eq!(s2, b"btcusdt@index");
        assert_eq!(parse_eapi_index(d2), Some(77_000_150_000_000));
        assert!(parse_eapi_index(br#"{"p":"0"}"#).is_none());
        assert!(parse_eapi_index(br#"{"p":77000}"#).is_none());
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// §21.3: none of the eapi byte scanners panic on arbitrary
        /// bytes.
        #[test]
        fn eapi_parsers_never_panic(input in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut d = EapiDiscovery::new();
            let _ = d.ingest_exchange_info(&input);
            let _ = parse_index_price(&input);
            let _ = split_combined(&input);
            let _ = parse_eapi_ticker(&input);
            let _ = parse_eapi_index(&input);
        }

        /// M2 selection invariants — the SAME properties pinning the
        /// Deribit/OKX twins (law parity): ≤ E×K×2, candidate filter
        /// (underlying + unexpired), deterministic expiry→strike→C/P
        /// order.
        #[test]
        fn capped_selection_invariants(
            strikes in proptest::collection::vec(1i64..2_000_000, 1..24),
            exps in proptest::collection::vec(1i64..1_000_000, 1..6),
            e in 1u32..=4,
            k_half in 1u32..=16,
            idx in 1i64..2_000_000,
        ) {
            let now_ms = 500_000i64;
            let mut rows: Vec<EapiOptionRow> = Vec::new();
            for &exp in &exps {
                for &s in &strikes {
                    for call in [true, false] {
                        let mut symbol = [0u8; EAPI_SYM_MAX];
                        let tag = format!("O-{exp}-{s}-{}", if call { "C" } else { "P" });
                        let tb = tag.as_bytes();
                        let n = tb.len().min(EAPI_SYM_MAX);
                        symbol[..n].copy_from_slice(&tb[..n]);
                        let mut underlying = [0u8; EAPI_ULY_MAX];
                        underlying[..7].copy_from_slice(b"BTCUSDT");
                        rows.push(EapiOptionRow {
                            symbol,
                            symbol_len: n as u8,
                            underlying,
                            underlying_len: 7,
                            is_call: call,
                            strike_1e9: s,
                            expiry_ms: exp,
                        });
                    }
                }
            }
            let k = k_half * 2;
            let sel = select_capped_chain(&rows, b"BTCUSDT", idx, e, k, now_ms);
            prop_assert!(sel.len() as u32 <= e * k * 2);
            for r in &sel {
                prop_assert!(r.expiry_ms > now_ms);
                prop_assert_eq!(r.underlying(), b"BTCUSDT");
            }
            for w in sel.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                let ka = (a.expiry_ms, a.strike_1e9, !a.is_call);
                let kb = (b.expiry_ms, b.strike_1e9, !b.is_call);
                prop_assert!(ka < kb, "order law violated");
            }
            // A foreign underlying never selects.
            let foreign = select_capped_chain(&rows, b"ETHUSDT", idx, e, k, now_ms);
            prop_assert_eq!(foreign.len(), 0);
        }
    }
}
