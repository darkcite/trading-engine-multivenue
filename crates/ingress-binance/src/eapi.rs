// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Binance European options — the M2.4 options half-ingress
//!
//! Options live on their own endpoint family: `eapi.binance.com` REST
//! for discovery, and the WS streams the 2025-12 options migration
//! moved onto fstream's ROUTED paths (BX0-F2) — hence "half-ingress":
//! a second LANE inside the Binance venue (the M1c usdm precedent),
//! not a new venue. Market data only (mvp-plan §4-M2 step 4):
//! capped-chain discovery, option BBO → `Tick`, mark/IV/greeks plus
//! the underlying's index → `OptSummary` (the M2.3 channel). No order
//! path here.
//!
//! ## Boot/offline doctrine
//!
//! Discovery + selection run at boot only — allocation permitted,
//! same as every 8e discovery module. The WS-lane scanners below the
//! discovery section are HOT (per push on the Binance ingress
//! thread): zero-alloc, zero-copy flat scans over `&[u8]` that return
//! borrowed spans, quoted decimals exactly like the spot `bookTicker`
//! wire.
//!
//! ## Wire shapes (live-verified 2026-09-23, BX0 K6 — pitfall #11)
//!
//! REST `GET /eapi/v1/exchangeInfo` (ONE page, ALL underlyings):
//! `{"optionSymbols":[{"symbol":"BTC-260327-100000-C",
//!   "underlying":"BTCUSDT","strikePrice":"100000.00000000",
//!   "expiryDate":1774598400000,"side":"CALL","filters":[…],…},…]}`
//! — `strikePrice` QUOTED decimal, `expiryDate` BARE ms integer,
//! `side` `"CALL"|"PUT"`; `filters`/noise skipped structurally.
//!
//! REST `GET /eapi/v1/index?underlying=BTCUSDT`:
//! `{"time":…,"indexPrice":"77000.12"}` — quoted decimal; the boot
//! ATM reference.
//!
//! WS: `wss://fstream.binance.com/market/stream?streams=`
//! `btcusdt@optionMarkPrice/ethusdt@optionMarkPrice` — combined, no
//! subscribe frames (the crate's standing direct-URL pattern). Each
//! push is the WHOLE listed chain of one underlying, about once a
//! second, as ONE unfragmented text frame (measured: 752 BTC
//! elements ≈ 246 KB, 600 ETH ≈ 194 KB, no element over 336 B):
//!
//! ```text
//! {"stream":"btcusdt@optionMarkPrice","data":[{"s":"BTC-260925-86000-C",
//!  "mp":"809.784","E":1790161477974,"e":"markPrice","i":"85879.82826087",
//!  "P":"0.000","bo":"800.000","ao":"810.000","bq":"5.08","aq":"1.10",
//!  "b":"0.34500957","a":"0.34908772","hl":"1455.000","ll":"165.000",
//!  "vo":"0.349","rf":"0.0558","d":"0.48723426","t":"-227.33432684",
//!  "g":"0.00018682","v":"24.52223767"},…]}
//! ```
//!
//! Every value is a quoted decimal. `vo` is the mark IV (fraction);
//! `b`/`a` are the bid/ask IVs (not read); `i` is the underlying's
//! index, one value per push. The lane keeps the elements whose `s`
//! is in its boot table and skips the rest after one symbol compare.
//! Not subscribed, both measured: `!index@arr` (redundant with `i`)
//! and the real-time per-option `<symbol>@bookTicker` on `/public`
//! (the lane keeps the ~1 s BBO cadence it always had). A wrong route
//! is SILENT — `/stream?…` and `/public/stream?…@optionMarkPrice`
//! upgrade (101) and carry nothing — and the retired nbstream
//! `/eoptions/…` `@ticker`/`@index` streams answer HTTP 404. The
//! stream carries no open interest: `OptSummary.flags` is MARK_PX
//! only (the OKX-asymmetry mechanism, docs/wire-format.md).

use core_parse::{
    find_field, scan_number_sci_1e9, scan_price_1e6, scan_u64, skip_json_value, skip_string,
    skip_ws,
};
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
        let arr_pos = find_field(body, b"\"optionSymbols\":").ok_or(EapiDiscoveryErr::Envelope)?;
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
                let key_end_q = skip_string(body, key_start).ok_or(EapiDiscoveryErr::Truncated)?;
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

/// The `options_select::ChainRow` view of an eapi option row (M2-close
/// extraction — the shared law's read surface).
impl options_select::ChainRow for EapiOptionRow {
    #[inline]
    fn exp_ms(&self) -> i64 {
        self.expiry_ms
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

/// Apply the capped universe policy to ONE underlying's option rows.
/// The selection LAW lives in `options-select` since the M2-close
/// extraction (`ingress-deribit` = law source; identical property
/// invariants keep pinning all three venue surfaces) — here lives only
/// the VENUE candidacy predicate, which for eapi adds the `underlying`
/// filter because ONE exchangeInfo page carries every family
/// (`row.underlying == underlying && expiry_ms > now_ms`; eapi lists
/// tradable symbols only — no per-row state field). Deterministic
/// order = the allocation order; ≤ `E × K × 2` by construction.
/// Boot-only: allocates freely.
pub fn select_capped_chain(
    rows: &[EapiOptionRow],
    underlying: &[u8],
    index_px_1e9: i64,
    expiries_e: u32,
    strikes_k: u32,
    now_ms: i64,
) -> Vec<EapiOptionRow> {
    options_select::select_capped_chain(
        rows,
        |r: &EapiOptionRow| r.underlying() == underlying && r.expiry_ms > now_ms,
        index_px_1e9,
        expiries_e,
        strikes_k,
    )
}

// ---------------------------------------------------------------
// WS-lane state (boot-built, hot-read)
// ---------------------------------------------------------------

/// The per-underlying stream name suffix on fstream's `/market` path
/// (`btcusdt@optionMarkPrice`) — shared by the boot's path builder
/// and the lane's stream check.
pub const EAPI_MARK_STREAM: &str = "@optionMarkPrice";

/// Fixed-capacity `venue symbol → SymbolId` map for the options lane.
/// Built at boot from the selected chain; read once per array element
/// on the Binance ingress thread (a length-gated linear scan of ≤ 64
/// rows — the Deribit-table cost note applies). Keys are the symbols
/// AS LISTED (`BTC-260925-86000-C`): the mark array's `"s"` carries
/// the venue's own case, so a lookup compares the wire bytes where
/// they lie — no lowercasing, no copy.
pub struct EapiSymbolTable {
    rows: [(u8, [u8; EAPI_SYM_MAX], SymbolId); EAPI_OPT_MAX],
    len: usize,
}

/// Why an [`EapiSymbolTable::insert`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EapiTableErr {
    /// All [`EAPI_OPT_MAX`] rows in use.
    Full,
    /// Symbol longer than [`EAPI_SYM_MAX`] or empty.
    BadSymbol,
}

impl EapiSymbolTable {
    /// Empty table.
    pub const fn new() -> Self {
        Self {
            rows: [(0, [0; EAPI_SYM_MAX], 0); EAPI_OPT_MAX],
            len: 0,
        }
    }

    /// Register `symbol → sym`, the symbol exactly as the venue lists
    /// it. Boot-time only.
    pub fn insert(&mut self, symbol: &[u8], sym: SymbolId) -> Result<(), EapiTableErr> {
        if symbol.is_empty() || symbol.len() > EAPI_SYM_MAX {
            return Err(EapiTableErr::BadSymbol);
        }
        if self.len >= EAPI_OPT_MAX {
            return Err(EapiTableErr::Full);
        }
        let row = &mut self.rows[self.len];
        row.0 = symbol.len() as u8;
        // COPY: ≤ 32 B symbol into its boot-table row, once per
        // selected option at boot — the table moves to the ingress
        // thread and cannot borrow the discovery strings — rejected: a
        // 'static intern pool, for 64 keys built once.
        row.1[..symbol.len()].copy_from_slice(symbol);
        row.2 = sym;
        self.len += 1;
        Ok(())
    }

    /// Resolve a wire symbol (an element's `"s"`, venue case). Hot
    /// path: length gate, then a bytewise compare.
    #[inline]
    pub fn lookup(&self, wire_sym: &[u8]) -> Option<SymbolId> {
        debug_assert!(self.len <= EAPI_OPT_MAX);
        let n = wire_sym.len();
        let mut i = 0;
        while i < self.len {
            let row = &self.rows[i];
            if row.0 as usize == n && &row.1[..n] == wire_sym {
                return Some(row.2);
            }
            i += 1;
        }
        None
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
// WS-lane scanners (HOT: zero-alloc, zero-copy flat scans)
// ---------------------------------------------------------------

/// Split a combined-stream envelope into `(stream_name, data_tail)`.
/// The tail starts at the `data` value — the scanners work within it.
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

/// One step of an [`EapiArrayCursor`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArrayStep<'a> {
    /// The next element, `{` … `}` inclusive, borrowed from the frame.
    Elem(&'a [u8]),
    /// The closing `]`: every element has been seen.
    End,
    /// The bytes stopped being an array of flat objects (or ended
    /// inside it). Terminal, like `End`.
    Malformed,
}

/// Zero-alloc, zero-copy cursor over a `<uly>@optionMarkPrice` array:
/// each step borrows ONE element's bytes from the frame and nothing is
/// ever copied out. Every step either advances strictly or is
/// terminal (`End`/`Malformed`), so a walk ends in at most one step
/// per input byte.
///
/// The elements are FLAT objects — every value is a quoted decimal, a
/// symbol or a bare integer, with no nested object and no brace inside
/// a string — so the first `}` after an element's `{` closes it, found
/// with `memchr` at SIMD speed over the ~330 B element (a full JSON
/// skipper would walk every byte of the ~246 KB push on the ingress
/// thread). Were a string ever to hold a `}`, the element would be cut
/// short: every field before the cut still parses to its true value,
/// a required one after it rejects the element, and the cursor turns
/// `Malformed` at the cut — it cannot yield a wrong row.
#[derive(Copy, Clone, Debug)]
pub struct EapiArrayCursor<'a> {
    buf: &'a [u8],
    pos: usize,
    after_elem: bool,
}

impl<'a> EapiArrayCursor<'a> {
    /// A cursor over the array at the start of `data` (a combined
    /// frame's tail from [`split_combined`], or a raw-stream payload);
    /// leading whitespace tolerated. `None` when no array starts there.
    #[inline]
    pub fn new(data: &'a [u8]) -> Option<Self> {
        let i = skip_ws(data, 0);
        if i >= data.len() || data[i] != b'[' {
            return None;
        }
        Some(Self {
            buf: data,
            pos: i + 1,
            after_elem: false,
        })
    }

    /// The unwalked bytes from the cursor on — after a `Malformed`
    /// step, the bytes the walk stopped at (what a bounded reject tap
    /// records instead of the whole push).
    #[inline]
    pub fn rest(&self) -> &'a [u8] {
        let buf = self.buf;
        &buf[self.pos.min(buf.len())..]
    }

    /// The next element, or the end of the array.
    #[inline]
    pub fn next_elem(&mut self) -> ArrayStep<'a> {
        let buf = self.buf;
        let mut i = skip_ws(buf, self.pos);
        if i >= buf.len() {
            return ArrayStep::Malformed;
        }
        if buf[i] == b']' {
            self.pos = i + 1;
            return ArrayStep::End;
        }
        if self.after_elem {
            if buf[i] != b',' {
                return ArrayStep::Malformed;
            }
            i = skip_ws(buf, i + 1);
            if i >= buf.len() {
                return ArrayStep::Malformed;
            }
        }
        if buf[i] != b'{' {
            return ArrayStep::Malformed;
        }
        match memchr::memchr(b'}', &buf[i + 1..]) {
            Some(off) => {
                let end = i + off + 2;
                self.pos = end;
                self.after_elem = true;
                ArrayStep::Elem(&buf[i..end])
            }
            None => ArrayStep::Malformed,
        }
    }
}

/// The `"s"` symbol of one array element, borrowed in place (venue
/// case). `None` when the key is absent, its value is not a string, or
/// the string holds an escape — a listed symbol never does (discovery
/// refuses one too), and an escape-blind span would be a truncated
/// name rather than a refusal.
#[inline]
pub fn eapi_elem_symbol(elem: &[u8]) -> Option<&[u8]> {
    let pos = find_field(elem, b"\"s\":")?;
    let i = skip_ws(elem, pos);
    if i >= elem.len() || elem[i] != b'"' {
        return None;
    }
    let end_q = skip_string(elem, i + 1)?;
    let span = &elem[i + 1..end_q - 1];
    if memchr::memchr(b'\\', span).is_some() {
        return None;
    }
    Some(span)
}

/// One parsed element of a `<uly>@optionMarkPrice` array: the BBO, the
/// mark/IV/greeks surface and the underlying's index, from ONE venue
/// push. `Copy` POD, 88 B — over the one-line by-value budget, so
/// [`parse_eapi_mark`] FILLS a caller-owned frame instead of returning
/// one (the lane keeps a single frame for its whole walk).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct EapiMarkFrame {
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
    /// `i` the underlying's index price ×1e9.
    pub index_px_1e9: i64,
    /// `d` delta ×1e9.
    pub delta_1e9: i64,
    /// `g` gamma ×1e9.
    pub gamma_1e9: i64,
    /// `v` vega ×1e6.
    pub vega_1e6: i64,
    /// `t` theta ×1e6.
    pub theta_1e6: i64,
}

const _: () = assert!(core::mem::size_of::<EapiMarkFrame>() == 88);

impl EapiMarkFrame {
    /// All-zero frame — the lane's reusable parse target.
    pub const ZERO: Self = Self {
        bid_px_1e6: 0,
        bid_qty_1e6: 0,
        ask_px_1e6: 0,
        ask_qty_1e6: 0,
        mark_px_1e9: 0,
        mark_iv_1e9: 0,
        index_px_1e9: 0,
        delta_1e9: 0,
        gamma_1e9: 0,
        vega_1e6: 0,
        theta_1e6: 0,
    };
}

/// Parse one mark-array element (an [`ArrayStep::Elem`] span — the
/// field lookups never leave it) INTO `out`, field by field; `false`
/// when the element fails its contract (then `out` holds a partial
/// parse and must not be read). Every captured value is a QUOTED
/// decimal; single-char keys are anchored `"x":` so they can never
/// alias the two-char forms (`"b":` ≠ `"bo":`/`"bq":`, `"v":` ≠
/// `"vo":`). The mark/IV/index/greeks surface is REQUIRED (the index
/// must be positive); the four BBO fields are OPTIONAL — an unquoted
/// side reads `"0.000"`, which parses as 0 like an absent or empty one
/// (the one-sided/empty-book precedent: the lane skips the `Tick` when
/// both sides are zero and still captures the summary).
#[inline]
pub fn parse_eapi_mark(elem: &[u8], out: &mut EapiMarkFrame) -> bool {
    parse_mark_fields(elem, out).is_some()
}

/// [`parse_eapi_mark`]'s body, `?`-shaped.
#[inline]
fn parse_mark_fields(elem: &[u8], out: &mut EapiMarkFrame) -> Option<()> {
    #[inline]
    fn q_span<'a>(elem: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
        let pos = find_field(elem, key)?;
        let i = skip_ws(elem, pos);
        if i >= elem.len() || elem[i] != b'"' {
            return None;
        }
        let end_q = skip_string(elem, i + 1)?;
        Some(&elem[i + 1..end_q - 1])
    }
    #[inline]
    fn q_1e9(elem: &[u8], key: &[u8]) -> Option<i64> {
        let span = q_span(elem, key)?;
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
    fn q_1e6_or_zero(elem: &[u8], key: &[u8]) -> i64 {
        match q_span(elem, key) {
            None | Some([]) => 0,
            Some(span) => match scan_price_1e6(span, 0) {
                Some((v, used)) if used == span.len() => v,
                _ => 0,
            },
        }
    }
    out.index_px_1e9 = q_1e9(elem, b"\"i\":")?;
    if out.index_px_1e9 <= 0 {
        return None;
    }
    out.mark_px_1e9 = q_1e9(elem, b"\"mp\":")?;
    out.mark_iv_1e9 = q_1e9(elem, b"\"vo\":")?;
    out.delta_1e9 = q_1e9(elem, b"\"d\":")?;
    out.gamma_1e9 = q_1e9(elem, b"\"g\":")?;
    out.vega_1e6 = q_1e9(elem, b"\"v\":")? / 1000;
    out.theta_1e6 = q_1e9(elem, b"\"t\":")? / 1000;
    out.bid_px_1e6 = q_1e6_or_zero(elem, b"\"bo\":");
    out.bid_qty_1e6 = q_1e6_or_zero(elem, b"\"bq\":");
    out.ask_px_1e6 = q_1e6_or_zero(elem, b"\"ao\":");
    out.ask_qty_1e6 = q_1e6_or_zero(elem, b"\"aq\":");
    Some(())
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Test convenience over the out-param parser.
    fn mark(elem: &[u8]) -> Option<EapiMarkFrame> {
        let mut f = EapiMarkFrame::ZERO;
        parse_eapi_mark(elem, &mut f).then_some(f)
    }

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
            opt_row(
                "BTC-260327-100000-C",
                "BTCUSDT",
                "CALL",
                "100000.00000000",
                EXP1,
            ),
            opt_row(
                "BTC-260327-100000-P",
                "BTCUSDT",
                "PUT",
                "100000.00000000",
                EXP1,
            ),
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
        let bad =
            opt_row("BTC-1-C", "BTCUSDT", "CALL", "1", EXP1).replacen(r#""side":"CALL","#, "", 1);
        let mut d = EapiDiscovery::new();
        assert_eq!(
            d.ingest_exchange_info(&info(&bad)).unwrap_err(),
            EapiDiscoveryErr::BadRow
        );
        // Bad side value.
        let bad = opt_row("BTC-1-C", "BTCUSDT", "STRADDLE", "1", EXP1);
        let mut d = EapiDiscovery::new();
        assert_eq!(
            d.ingest_exchange_info(&info(&bad)).unwrap_err(),
            EapiDiscoveryErr::BadRow
        );
        // Bare (unquoted) strike = contract change.
        let bad = opt_row("BTC-1-C", "BTCUSDT", "CALL", "1", EXP1).replacen(
            r#""strikePrice":"1""#,
            r#""strikePrice":1"#,
            1,
        );
        let mut d = EapiDiscovery::new();
        assert_eq!(
            d.ingest_exchange_info(&info(&bad)).unwrap_err(),
            EapiDiscoveryErr::BadRow
        );
        // No optionSymbols array.
        let mut d = EapiDiscovery::new();
        assert_eq!(
            d.ingest_exchange_info(br#"{"symbols":[]}"#).unwrap_err(),
            EapiDiscoveryErr::Envelope
        );
        // Truncated inside the array.
        let mut d = EapiDiscovery::new();
        assert_eq!(
            d.ingest_exchange_info(br#"{"optionSymbols":[{"symbol":"X""#)
                .unwrap_err(),
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
        rows.push(opt_row(
            "ETH-260327-2400-C",
            "ETHUSDT",
            "CALL",
            "2400",
            EXP1,
        ));
        // An expired row.
        rows.push(opt_row(
            "BTC-OLD-90000-C",
            "BTCUSDT",
            "CALL",
            "90000",
            NOW - 1_000,
        ));
        let mut d = EapiDiscovery::new();
        d.ingest_exchange_info(&info(&rows.join(",")))
            .expect("grid parses");
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
        assert!(!names(&all)
            .iter()
            .any(|n| n.contains("ETH") || n.contains("OLD")));
        // Determinism.
        let again = select_capped_chain(d.rows(), b"BTCUSDT", 100_000_000_000_000, 4, 32, NOW);
        assert_eq!(names(&all), names(&again));
    }

    /// The K6 frame (2026-09-23, fstream `/market/stream`), trimmed to
    /// three of its 752 elements: the push's first row and the ATM
    /// pair — byte-for-byte as they arrived.
    const LIVE_BTC: &[u8] = br#"{"stream":"btcusdt@optionMarkPrice","data":[{"s":"BTC-261225-92000-C","mp":"4696.169","E":1790161477975,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"4670.000","ao":"4760.000","bq":"3.52","aq":"3.52","b":"0.38545907","a":"0.39075673","hl":"8450.000","ll":"940.000","vo":"0.387","rf":"0.0529","d":"0.42618971","t":"-35.49083602","g":"0.00002332","v":"169.85468453"},{"s":"BTC-260925-86000-P","mp":"905.351","E":1790161477974,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"905.000","ao":"920.000","bq":"4.43","aq":"12.00","b":"0.34885705","a":"0.35497367","hl":"1625.000","ll":"185.000","vo":"0.349","rf":"0.0558","d":"-0.51276574","t":"-230.5222729","g":"0.00018424","v":"24.52223767"},{"s":"BTC-260925-86000-C","mp":"809.784","E":1790161477974,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"800.000","ao":"810.000","bq":"5.08","aq":"1.10","b":"0.34500957","a":"0.34908772","hl":"1455.000","ll":"165.000","vo":"0.349","rf":"0.0558","d":"0.48723426","t":"-227.33432684","g":"0.00018682","v":"24.52223767"}]}"#;

    #[test]
    fn symbol_table_resolves_the_venue_case_only() {
        let mut t = EapiSymbolTable::new();
        t.insert(b"BTC-260925-86000-C", (1 << 24) | 1025).unwrap();
        assert_eq!(t.lookup(b"BTC-260925-86000-C"), Some((1 << 24) | 1025));
        // The mark array carries the listed case; nothing else resolves.
        assert_eq!(t.lookup(b"btc-260925-86000-c"), None);
        assert_eq!(t.lookup(b"BTC-260925-86000-P"), None);
        assert_eq!(t.lookup(b""), None);
        // Length-gated: neither a prefix of a key nor a key's extension
        // resolves.
        assert_eq!(t.lookup(b"BTC-260925-86000-"), None);
        assert_eq!(t.lookup(b"BTC-260925-86000-C2"), None);
        assert_eq!(t.insert(b"", 1), Err(EapiTableErr::BadSymbol));
        assert_eq!(t.insert(&[b'X'; EAPI_SYM_MAX + 1], 1), Err(EapiTableErr::BadSymbol));
        let mut full = EapiSymbolTable::new();
        for i in 0..EAPI_OPT_MAX {
            full.insert(format!("S{i}").as_bytes(), i as u32).unwrap();
        }
        assert_eq!(full.insert(b"OVER", 99), Err(EapiTableErr::Full));
        assert_eq!(full.len(), EAPI_OPT_MAX);
    }

    /// BX0-F2: the live frame splits, walks element by element, and
    /// each element parses to its own values — the ATM call checked
    /// field by field.
    #[test]
    fn the_live_mark_array_walks_and_parses() {
        let (stream, data) = split_combined(LIVE_BTC).expect("combined envelope");
        assert_eq!(stream, b"btcusdt@optionMarkPrice");
        assert!(stream.ends_with(EAPI_MARK_STREAM.as_bytes()));
        let mut cur = EapiArrayCursor::new(data).expect("an array");
        let mut syms: Vec<&[u8]> = Vec::new();
        let mut rows: Vec<EapiMarkFrame> = Vec::new();
        loop {
            match cur.next_elem() {
                ArrayStep::Elem(e) => {
                    assert!(e.starts_with(b"{") && e.ends_with(b"}"));
                    syms.push(eapi_elem_symbol(e).expect("every element names itself"));
                    rows.push(mark(e).expect("every live element parses"));
                }
                ArrayStep::End => break,
                ArrayStep::Malformed => panic!("the live frame walked as malformed"),
            }
        }
        assert_eq!(
            syms,
            [&b"BTC-261225-92000-C"[..], b"BTC-260925-86000-P", b"BTC-260925-86000-C"]
        );
        let c = &rows[2];
        assert_eq!(c.bid_px_1e6, 800_000_000);
        assert_eq!(c.ask_px_1e6, 810_000_000);
        assert_eq!(c.bid_qty_1e6, 5_080_000);
        assert_eq!(c.ask_qty_1e6, 1_100_000);
        assert_eq!(c.mark_px_1e9, 809_784_000_000);
        assert_eq!(c.mark_iv_1e9, 349_000_000, "vo — NOT b/a (bid/ask IV)");
        assert_eq!(c.index_px_1e9, 85_879_828_260_870);
        assert_eq!(c.delta_1e9, 487_234_260);
        assert_eq!(c.gamma_1e9, 186_820);
        assert_eq!(c.vega_1e6, 24_522_237);
        assert_eq!(c.theta_1e6, -227_334_326);
        assert_eq!(rows[1].delta_1e9, -512_765_740, "the put's delta keeps its sign");
        // One push, one index.
        assert!(rows.iter().all(|r| r.index_px_1e9 == 85_879_828_260_870));
    }

    /// The cursor walks only an array of flat objects and says where
    /// that stops; every refusal is terminal and nothing is guessed.
    #[test]
    fn the_cursor_refuses_what_is_not_an_array_of_flat_objects() {
        assert!(EapiArrayCursor::new(b"").is_none());
        assert!(EapiArrayCursor::new(br#"{"s":"A"}"#).is_none());
        let mut c = EapiArrayCursor::new(b" [ ] ").unwrap();
        assert_eq!(c.next_elem(), ArrayStep::End);
        let mut c = EapiArrayCursor::new(br#"[{"s":"A"}, {"s":"B"}]"#).unwrap();
        assert_eq!(c.next_elem(), ArrayStep::Elem(br#"{"s":"A"}"#));
        assert_eq!(c.next_elem(), ArrayStep::Elem(br#"{"s":"B"}"#));
        assert_eq!(c.next_elem(), ArrayStep::End);
        for bad in [
            &br#"[{"s":"A"}{"s":"B"}]"#[..], // no separator
            br#"[{"s":"A"},]"#,              // trailing comma
            br#"[,{"s":"A"}]"#,              // leading comma
            br#"[{"s":"A""#,                 // truncated inside an element
            br#"[{"s":"A"},"#,               // truncated after a separator
            b"[1,2]",                        // not objects
        ] {
            let mut c = EapiArrayCursor::new(bad).unwrap();
            let mut steps = 0;
            loop {
                match c.next_elem() {
                    ArrayStep::Elem(_) => steps += 1,
                    ArrayStep::End => panic!("{:?} walked to a clean end", String::from_utf8_lossy(bad)),
                    ArrayStep::Malformed => break,
                }
                assert!(steps <= 2);
            }
        }
    }

    /// The symbol is read where it lies — a span of the element, not a
    /// copy of it.
    #[test]
    fn a_symbol_is_borrowed_from_its_element() {
        let e = br#"{"s":"BTC-260925-86000-C","mp":"1"}"#;
        let s = eapi_elem_symbol(e).unwrap();
        assert_eq!(s, b"BTC-260925-86000-C");
        assert!(e.as_ptr_range().contains(&s.as_ptr()), "the span must point into the frame");
        assert_eq!(eapi_elem_symbol(br#"{"mp":"1"}"#), None);
        assert_eq!(eapi_elem_symbol(br#"{"s":7}"#), None);
        assert_eq!(eapi_elem_symbol(br#"{"s":"unterminated}"#), None);
        // An escape is refused outright — never a truncated name.
        assert_eq!(eapi_elem_symbol(br#"{"s":"AB\"C","mp":"1"}"#), None);
        assert_eq!(eapi_elem_symbol(br#"{"s":"AB\\","mp":"1"}"#), None);
    }

    /// The cursor's one design bet, pinned: elements are FLAT, so the
    /// first `}` closes one. A brace inside a string (never on this
    /// wire) cuts that element short — and the walk then turns
    /// `Malformed` at the cut instead of reading on, so the handler
    /// counts one rejection and the rest of that push is dropped. No
    /// row is invented: the cut element still names the symbol it
    /// carried and lacks its required fields.
    #[test]
    fn a_brace_inside_a_string_stops_the_walk_and_invents_nothing() {
        let data = br#"[{"s":"A}B","mp":"1","i":"1","vo":"1","d":"1","g":"1","v":"1","t":"1"},{"s":"C","mp":"2","i":"1","vo":"1","d":"1","g":"1","v":"1","t":"1"}]"#;
        let mut c = EapiArrayCursor::new(data).unwrap();
        let ArrayStep::Elem(cut) = c.next_elem() else {
            panic!("the cut element is still an element");
        };
        assert_eq!(cut, br#"{"s":"A}"#);
        assert_eq!(eapi_elem_symbol(cut), None, "the cut string is unterminated");
        assert!(mark(cut).is_none());
        assert_eq!(c.next_elem(), ArrayStep::Malformed);
        assert!(c.rest().starts_with(br#"B","mp""#), "rest() is where the walk stopped");
    }

    /// Whitespace around the array's tokens and after each colon (the
    /// venue sends none) walks and parses exactly like the compact
    /// form. The one place it is NOT tolerated is between a key and its
    /// colon: the house `find_field` anchors `"key":` as one token, on
    /// this wire as on every other.
    #[test]
    fn a_pretty_printed_push_walks_like_a_compact_one() {
        let data = b"[\n  {\n    \"s\": \"X\",\n    \"mp\": \"1.5\",\n    \"i\":\t\"100\",\n    \"vo\": \"0.5\",\n    \"d\": \"0.1\",\n    \"g\": \"0.001\",\n    \"v\": \"3.0\",\n    \"t\": \"-2.0\"\n  } ,\n  { \"s\": \"Y\" }\n]\n";
        let mut c = EapiArrayCursor::new(data).unwrap();
        let ArrayStep::Elem(e) = c.next_elem() else {
            panic!("first element");
        };
        assert_eq!(eapi_elem_symbol(e), Some(&b"X"[..]));
        let f = mark(e).expect("parses across whitespace");
        assert_eq!((f.mark_px_1e9, f.index_px_1e9, f.theta_1e6), (1_500_000_000, 100_000_000_000, -2_000_000));
        let ArrayStep::Elem(e2) = c.next_elem() else {
            panic!("second element");
        };
        assert_eq!(eapi_elem_symbol(e2), Some(&b"Y"[..]));
        assert_eq!(c.next_elem(), ArrayStep::End);
    }

    /// Build one element from `(key, value)` pairs, leaving out `skip`.
    fn elem_without(skip: &str, pairs: &[(&str, &str)]) -> String {
        let mut s = String::from("{");
        for (k, v) in pairs {
            if *k == skip {
                continue;
            }
            if s.len() > 1 {
                s.push(',');
            }
            s.push_str(&format!("\"{k}\":{v}"));
        }
        s.push('}');
        s
    }

    /// The mark/IV/index/greeks surface is REQUIRED; the book is not.
    #[test]
    fn a_mark_element_needs_its_surface_and_tolerates_an_empty_book() {
        let pairs = [
            ("s", "\"X\""),
            ("mp", "\"1.5\""),
            ("i", "\"100.0\""),
            ("bo", "\"0.000\""),
            ("ao", "\"0.000\""),
            ("bq", "\"0.00\""),
            ("aq", "\"0.00\""),
            ("vo", "\"0.5\""),
            ("d", "\"0.1\""),
            ("t", "\"-2.0\""),
            ("g", "\"0.001\""),
            ("v", "\"3.0\""),
        ];
        let f = mark(elem_without("", &pairs).as_bytes()).expect("complete");
        assert_eq!((f.bid_px_1e6, f.ask_px_1e6, f.bid_qty_1e6, f.ask_qty_1e6), (0, 0, 0, 0));
        assert_eq!(f.mark_px_1e9, 1_500_000_000);
        assert_eq!(f.theta_1e6, -2_000_000);
        for optional in ["bo", "ao", "bq", "aq"] {
            assert!(
                mark(elem_without(optional, &pairs).as_bytes()).is_some(),
                "`{optional}` is optional"
            );
        }
        for required in ["mp", "vo", "i", "d", "g", "v", "t"] {
            assert!(
                mark(elem_without(required, &pairs).as_bytes()).is_none(),
                "`{required}` is required"
            );
        }
        let zero_index = elem_without("", &pairs).replace("\"i\":\"100.0\"", "\"i\":\"0\"");
        assert!(mark(zero_index.as_bytes()).is_none(), "an index must be positive");
        let bare_mark = elem_without("", &pairs).replace("\"mp\":\"1.5\"", "\"mp\":1.5");
        assert!(mark(bare_mark.as_bytes()).is_none(), "an unquoted mark is a contract change");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Test convenience over the out-param parser.
    fn mark(elem: &[u8]) -> Option<EapiMarkFrame> {
        let mut f = EapiMarkFrame::ZERO;
        parse_eapi_mark(elem, &mut f).then_some(f)
    }

    proptest! {
        /// §21.3: no eapi scanner panics on arbitrary bytes, and a
        /// cursor walk always terminates within one step per byte.
        #[test]
        fn eapi_scanners_never_panic(input in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut d = EapiDiscovery::new();
            let _ = d.ingest_exchange_info(&input);
            let _ = parse_index_price(&input);
            let _ = eapi_elem_symbol(&input);
            let _ = mark(&input);
            let tail = split_combined(&input).map_or(&input[..], |(_, t)| t);
            for data in [tail, &input[..]] {
                if let Some(mut c) = EapiArrayCursor::new(data) {
                    let mut steps = 0usize;
                    while let ArrayStep::Elem(e) = c.next_elem() {
                        let braced = e.first() == Some(&b'{') && e.last() == Some(&b'}');
                        prop_assert!(braced, "an element must be brace-delimited");
                        let _ = eapi_elem_symbol(e);
                        let _ = mark(e);
                        steps += 1;
                        prop_assert!(steps <= data.len(), "the walk failed to advance");
                    }
                }
            }
        }

        /// Numeric edges the element parser meets only through its
        /// spans: signs, `+`, huge digit runs and exponents never
        /// panic, and a value that is not wholly a number never parses.
        #[test]
        fn mark_numbers_at_the_edges_never_panic(
            sign in "[-+]?",
            digits in "[0-9]{0,400}",
            frac in "[0-9]{0,40}",
            exp in "([eE][-+]?[0-9]{0,6})?",
            tail in "[ a-z]{0,2}",
        ) {
            let num = format!("{sign}{digits}.{frac}{exp}{tail}");
            let elem = format!(
                r#"{{"s":"X","mp":"{num}","i":"{num}","vo":"1","d":"{num}","g":"1","v":"{num}","t":"{num}","bo":"{num}"}}"#
            );
            let parsed = mark(elem.as_bytes());
            if !tail.is_empty() {
                prop_assert!(parsed.is_none(), "a trailing non-digit must refuse the field");
            }
            if let Some(f) = parsed {
                prop_assert!(f.index_px_1e9 > 0);
            }
        }

        /// BX0-F2 round trip: any chain, in any key order, with any
        /// subset selected — the walk visits every element exactly
        /// once, and exactly the selected ones parse back to the values
        /// that were written.
        #[test]
        fn mark_array_roundtrips_in_any_key_order(
            rows in proptest::collection::vec(
                (
                    (0u32..100_000, 0u32..1_000, 1u32..200_000, 0u32..100_000_000),
                    (0u32..100_000, 0u32..1_000, 0u32..100_000, 0u32..1_000),
                    (0u32..100_000, 0u32..100, 0u32..100_000, 0u32..100),
                    (0u32..10_000, any::<bool>(), 0u32..100_000_000, 0u32..100_000_000),
                    (0u32..1_000, 0u32..100_000_000, 0u32..1_000, 0u32..100_000_000),
                    any::<u64>(),
                ),
                1..24,
            ),
            mask in any::<u32>(),
        ) {
            let n = rows.len();
            let mut table = EapiSymbolTable::new();
            let mut frame = String::from(r#"{"stream":"btcusdt@optionMarkPrice","data":["#);
            let mut expect: Vec<Option<EapiMarkFrame>> = Vec::with_capacity(n);
            for (k, row) in rows.iter().enumerate() {
                let ((mp_i, mp_f, ix_i, ix_f), (bo_i, bo_f, ao_i, ao_f), (bq_i, bq_f, aq_i, aq_f),
                    (vo_f, d_neg, d_f, g_f), (v_i, v_f, t_i, t_f), seed) = *row;
                let name = format!("BTC-2609{:02}-{}-C", k % 30, 10_000 + k);
                let mut pairs: Vec<(String, String)> = vec![
                    ("s".into(), format!("\"{name}\"")),
                    ("mp".into(), format!("\"{mp_i}.{mp_f:03}\"")),
                    ("E".into(), "1790161477974".into()),
                    ("e".into(), "\"markPrice\"".into()),
                    ("i".into(), format!("\"{ix_i}.{ix_f:08}\"")),
                    ("P".into(), "\"0.000\"".into()),
                    ("bo".into(), format!("\"{bo_i}.{bo_f:03}\"")),
                    ("ao".into(), format!("\"{ao_i}.{ao_f:03}\"")),
                    ("bq".into(), format!("\"{bq_i}.{bq_f:02}\"")),
                    ("aq".into(), format!("\"{aq_i}.{aq_f:02}\"")),
                    ("b".into(), "\"0.34\"".into()),
                    ("a".into(), "\"-1.0\"".into()),
                    ("hl".into(), "\"1.000\"".into()),
                    ("ll".into(), "\"0.500\"".into()),
                    ("vo".into(), format!("\"0.{vo_f:04}\"")),
                    ("rf".into(), "\"0.05\"".into()),
                    ("d".into(), format!("\"{}0.{d_f:08}\"", if d_neg { "-" } else { "" })),
                    ("t".into(), format!("\"-{t_i}.{t_f:08}\"")),
                    ("g".into(), format!("\"0.{g_f:08}\"")),
                    ("v".into(), format!("\"{v_i}.{v_f:08}\"")),
                ];
                // Deterministic Fisher-Yates from the row's own seed
                // (proptest shrinks the seed like any other input).
                let mut x = seed | 1;
                for j in (1..pairs.len()).rev() {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    pairs.swap(j, (x % (j as u64 + 1)) as usize);
                }
                if k > 0 {
                    frame.push(',');
                }
                frame.push('{');
                for (j, (key, val)) in pairs.iter().enumerate() {
                    if j > 0 {
                        frame.push(',');
                    }
                    frame.push_str(&format!("\"{key}\":{val}"));
                }
                frame.push('}');
                let selected = k < 32 && mask & (1u32 << k) != 0;
                if selected {
                    table.insert(name.as_bytes(), 1_000 + k as u32).unwrap();
                }
                let d_1e9 = i64::from(d_f) * 10;
                expect.push(selected.then_some(EapiMarkFrame {
                    bid_px_1e6: i64::from(bo_i) * 1_000_000 + i64::from(bo_f) * 1_000,
                    bid_qty_1e6: i64::from(bq_i) * 1_000_000 + i64::from(bq_f) * 10_000,
                    ask_px_1e6: i64::from(ao_i) * 1_000_000 + i64::from(ao_f) * 1_000,
                    ask_qty_1e6: i64::from(aq_i) * 1_000_000 + i64::from(aq_f) * 10_000,
                    mark_px_1e9: i64::from(mp_i) * 1_000_000_000 + i64::from(mp_f) * 1_000_000,
                    mark_iv_1e9: i64::from(vo_f) * 100_000,
                    index_px_1e9: i64::from(ix_i) * 1_000_000_000 + i64::from(ix_f) * 10,
                    delta_1e9: if d_neg { -d_1e9 } else { d_1e9 },
                    gamma_1e9: i64::from(g_f) * 10,
                    vega_1e6: (i64::from(v_i) * 1_000_000_000 + i64::from(v_f) * 10) / 1000,
                    theta_1e6: -(i64::from(t_i) * 1_000_000_000 + i64::from(t_f) * 10) / 1000,
                }));
            }
            frame.push_str("]}");

            let (_, data) = split_combined(frame.as_bytes()).unwrap();
            let mut cur = EapiArrayCursor::new(data).unwrap();
            let mut seen = 0usize;
            let mut hits = 0usize;
            loop {
                match cur.next_elem() {
                    ArrayStep::Elem(e) => {
                        let sym = eapi_elem_symbol(e).unwrap();
                        if let Some(id) = table.lookup(sym) {
                            let k = (id - 1_000) as usize;
                            prop_assert_eq!(k, seen, "the lookup resolved another row");
                            let want = expect[k].expect("only a selected row resolves");
                            let got = mark(e).unwrap();
                            prop_assert_eq!(got.bid_px_1e6, want.bid_px_1e6);
                            prop_assert_eq!(got.bid_qty_1e6, want.bid_qty_1e6);
                            prop_assert_eq!(got.ask_px_1e6, want.ask_px_1e6);
                            prop_assert_eq!(got.ask_qty_1e6, want.ask_qty_1e6);
                            prop_assert_eq!(got.mark_px_1e9, want.mark_px_1e9);
                            prop_assert_eq!(got.mark_iv_1e9, want.mark_iv_1e9);
                            prop_assert_eq!(got.index_px_1e9, want.index_px_1e9);
                            prop_assert_eq!(got.delta_1e9, want.delta_1e9);
                            prop_assert_eq!(got.gamma_1e9, want.gamma_1e9);
                            prop_assert_eq!(got.vega_1e6, want.vega_1e6);
                            prop_assert_eq!(got.theta_1e6, want.theta_1e6);
                            hits += 1;
                        } else {
                            prop_assert!(expect[seen].is_none(), "a selected row did not resolve");
                        }
                        seen += 1;
                    }
                    ArrayStep::End => break,
                    ArrayStep::Malformed => prop_assert!(false, "a generated chain walked as malformed"),
                }
            }
            prop_assert_eq!(seen, n);
            prop_assert_eq!(hits, expect.iter().filter(|e| e.is_some()).count());
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
