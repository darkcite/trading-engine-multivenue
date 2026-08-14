//! # Deribit boot-time REST discovery (Phase 8e, plan §4.2 + §6.1)
//!
//! Parses `GET /api/v2/public/get_instruments?currency=<ccy>&kind=future`
//! bodies into a boot-only instrument table: full venue universe count
//! for the §6.1 coverage report, plus per-instrument liveness
//! (`state == "open" && is_active`), perpetual/dated classification
//! (`settlement_period`), and the `tick_size` / `tick_size_steps` /
//! `contract_size` / `min_trade_amount` contract metadata that
//! execution sizing will consume (price-banded tick rounding, §4.2).
//!
//! One page is fetched per currency — `BTC`, `ETH`, `USDC` — with
//! `kind=future` only; options are excluded in v1 (chain size, §4.2).
//! A row carrying any other `kind` is therefore a contract violation
//! ([`DeribitDiscoveryErr::BadRow`]), not a filter case. The venue
//! rate-limits `public/get_instruments` to 1 req/s (burst 50) — pacing
//! between the per-currency fetches is the **caller's** job (cli boot
//! sequence), not this module's.
//!
//! ## Allocation note (doctrine)
//!
//! This module runs **at boot only**, where allocation is allowed. Row
//! storage is a `Vec` reserved once at [`DeribitDiscovery::new`] and
//! capped at [`DERIBIT_DISCOVERY_ROWS_CAP`] (fail-fast beyond — a
//! venue suddenly listing 10× instruments is a contract change we want
//! to see loudly). The table is dropped before the engine loop starts;
//! nothing here is reachable from a hot path.
//!
//! ## Wire shape (live-probed 2026-08-14)
//!
//! ```json
//! {"jsonrpc":"2.0","result":[{"min_trade_amount": 10.0,
//!   "settlement_period": "perpetual", "contract_size": 10.0,
//!   "state": "open", "kind": "future", "instrument_type": "reversed",
//!   "is_active": true, "tick_size_steps": [], "tick_size": 0.5,
//!   "instrument_name": "BTC-PERPETUAL", ...}],
//!  "usIn":1755150000000000,"usOut":1755150000003000,"usDiff":3000,
//!  "testnet":false}
//! ```
//!
//! Captured numbers are **bare** JSON numbers, including scientific
//! notation (`"tick_size": 1e-05` on USDC pages) —
//! [`core_parse::scan_number_sci_1e9`]. A *quoted* number in a
//! captured field means the venue contract changed →
//! [`DeribitDiscoveryErr::BadRow`]. Rows also carry bare booleans,
//! huge bare integers (`expiration_timestamp`) and quoted noise fields
//! which the walker skips structurally
//! ([`core_parse::skip_json_value`]); field order is not assumed. The
//! walk stops at the `result` array's closing `]` — the trailing
//! `usIn`/`usOut`/`usDiff` envelope integers are never visited.
//! `tick_size_steps` is `[]` on every current instrument; the official
//! non-empty shape `[{"above_price": 100000, "tick_size": 10}, …]` is
//! supported up to [`DERIBIT_TICK_STEPS_CAP`] entries.
//!
//! ## Amounts (USD notional)
//!
//! Deribit amounts for perps/futures are **USD notional** (crate-level
//! "Amount normalization" docs):
//! [`DeribitInstrumentRow::min_trade_amount_1e9`] carries USD × 1e9.
//! Normalization to `Qty(1e6)` happens downstream in execution
//! sizing — discovery capture does not quantize.

use core_parse::{find_field, scan_number_sci_1e9, skip_json_value, skip_string, skip_ws};

use crate::DERIBIT_INSTR_MAX;

/// Hard cap on parsed instrument rows across all fetched currency
/// pages. Live universe 2026-08-14: BTC + ETH + USDC futures ≈ 92;
/// 10× headroom.
pub const DERIBIT_DISCOVERY_ROWS_CAP: usize = 1024;

/// Maximum `tick_size_steps` entries per instrument. Every current
/// instrument sends `[]`; the official non-empty shape is a small band
/// table — more than this is a venue contract change
/// ([`DeribitDiscoveryErr::BadRow`]).
pub const DERIBIT_TICK_STEPS_CAP: usize = 4;

/// One discovered instrument.
#[derive(Copy, Clone, Debug)]
pub struct DeribitInstrumentRow {
    /// `instrument_name` bytes (`instrument_name_len` valid).
    pub instrument_name: [u8; DERIBIT_INSTR_MAX],
    /// Valid prefix length of `instrument_name`.
    pub instrument_name_len: u8,
    /// `settlement_period == "perpetual"` (dated futures carry
    /// `"month"` / `"week"` / `"day"`).
    pub perpetual: bool,
    /// `state == "open" && is_active` (anything else is not tradable).
    pub live: bool,
    /// `tick_size` ×1e9 (`0.5` → `500_000_000`, `1e-05` → `10_000`).
    pub tick_size_1e9: i64,
    /// Price-banded tick overrides as `(above_price_1e9,
    /// tick_size_1e9)` pairs in wire order (`n_tick_steps` valid).
    pub tick_size_steps: [(i64, i64); DERIBIT_TICK_STEPS_CAP],
    /// Valid prefix length of `tick_size_steps`.
    pub n_tick_steps: u8,
    /// `contract_size` ×1e9.
    pub contract_size_1e9: i64,
    /// `min_trade_amount` ×1e9 — **USD notional** × 1e9 for
    /// perps/futures (see module docs; not quantized here).
    pub min_trade_amount_1e9: i64,
}

impl DeribitInstrumentRow {
    /// The instrument name as a byte slice.
    #[inline]
    pub fn instrument_name(&self) -> &[u8] {
        &self.instrument_name[..self.instrument_name_len as usize]
    }
}

/// Why discovery ingestion failed. All fatal at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeribitDiscoveryErr {
    /// Envelope violation: missing `"jsonrpc":"2.0"`, or missing
    /// `"result":` array (JSON-RPC error bodies land here — they
    /// carry `"error":{...}` and no `result`), or `result` is not an
    /// array.
    Envelope,
    /// A row violated the instrument-object contract (missing required
    /// key, non-`"future"` `kind`, dotted/over-long `instrument_name`,
    /// quoted number in a bare-number field, more than
    /// [`DERIBIT_TICK_STEPS_CAP`] tick steps, malformed value).
    BadRow,
    /// Body ended inside the `result` array.
    Truncated,
    /// More than [`DERIBIT_DISCOVERY_ROWS_CAP`] rows across all pages.
    TooMany,
}

/// Boot-only Deribit instrument table. See module docs.
pub struct DeribitDiscovery {
    rows: Vec<DeribitInstrumentRow>,
    universe_live: u32,
}

impl DeribitDiscovery {
    /// Empty table with capacity reserved once.
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(DERIBIT_DISCOVERY_ROWS_CAP),
            universe_live: 0,
        }
    }

    /// Parse one `get_instruments` body (one currency page) into the
    /// table. Returns the number of rows added. Call once per fetched
    /// page (BTC, ETH, USDC — caller paces at 1 req/s, §4.2); counts
    /// accumulate.
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, DeribitDiscoveryErr> {
        // Envelope: `"jsonrpc":"2.0"`.
        let ver_pos = find_field(body, b"\"jsonrpc\":").ok_or(DeribitDiscoveryErr::Envelope)?;
        let v = skip_ws(body, ver_pos);
        if body.len() < v + 5 || &body[v..v + 5] != b"\"2.0\"" {
            return Err(DeribitDiscoveryErr::Envelope);
        }
        // `"result":[` array. A JSON-RPC error body has no `result`.
        let res_pos = find_field(body, b"\"result\":").ok_or(DeribitDiscoveryErr::Envelope)?;
        let mut i = skip_ws(body, res_pos);
        if i >= body.len() || body[i] != b'[' {
            return Err(DeribitDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(DeribitDiscoveryErr::Truncated);
            }
            match body[i] {
                // The trailing `usIn`/`usOut`/`usDiff` envelope
                // integers sit beyond this `]` — never visited.
                b']' => break,
                b'{' => {
                    let (row, end) = parse_row(body, i)?;
                    if self.rows.len() >= DERIBIT_DISCOVERY_ROWS_CAP {
                        return Err(DeribitDiscoveryErr::TooMany);
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
                _ => return Err(DeribitDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Look up a discovered instrument by exact `instrument_name`.
    pub fn find(&self, instrument_name: &[u8]) -> Option<&DeribitInstrumentRow> {
        self.rows
            .iter()
            .find(|r| r.instrument_name() == instrument_name)
    }

    /// Total rows parsed (all states).
    #[inline]
    pub fn universe_total(&self) -> u32 {
        self.rows.len() as u32
    }

    /// Rows with `state == "open" && is_active` — the §6.1
    /// coverage-report `universe=` figure.
    #[inline]
    pub fn universe_live(&self) -> u32 {
        self.universe_live
    }
}

impl Default for DeribitDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse one instrument object starting at `pos` (must point at `{`).
/// Returns the row and the position after the closing `}`.
fn parse_row(body: &[u8], pos: usize) -> Result<(DeribitInstrumentRow, usize), DeribitDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut instrument_name = [0u8; DERIBIT_INSTR_MAX];
    let mut instrument_name_len = 0u8;
    let mut kind_seen = false;
    let mut is_active: Option<bool> = None;
    let mut state_open: Option<bool> = None;
    let mut perpetual: Option<bool> = None;
    let mut tick_size: Option<i64> = None;
    let mut contract_size: Option<i64> = None;
    let mut min_trade_amount: Option<i64> = None;
    let mut tick_size_steps = [(0i64, 0i64); DERIBIT_TICK_STEPS_CAP];
    let mut n_tick_steps = 0u8;

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(DeribitDiscoveryErr::Truncated);
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
                let key_end_q =
                    skip_string(body, key_start).ok_or(DeribitDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                if i >= body.len() || body[i] != b':' {
                    return Err(DeribitDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                match key {
                    b"instrument_name" => {
                        let (s, end) = quoted_span(body, i)?;
                        // Dotted names are impossible on Deribit and
                        // would corrupt channel-name parsing — the
                        // same invariant [`crate::DeribitSymbolTable`]
                        // enforces via `SymbolTableErr::HasDot`.
                        if s.is_empty() || s.len() > DERIBIT_INSTR_MAX || s.contains(&b'.') {
                            return Err(DeribitDiscoveryErr::BadRow);
                        }
                        instrument_name[..s.len()].copy_from_slice(s);
                        instrument_name_len = s.len() as u8;
                        i = end;
                    }
                    b"kind" => {
                        // Only `kind=future` pages are ever fetched
                        // (§4.2 — options excluded v1); any other
                        // kind is a contract violation, not a filter.
                        let (s, end) = quoted_span(body, i)?;
                        if s != b"future" {
                            return Err(DeribitDiscoveryErr::BadRow);
                        }
                        kind_seen = true;
                        i = end;
                    }
                    b"is_active" => {
                        let (b, end) = bare_bool(body, i)?;
                        is_active = Some(b);
                        i = end;
                    }
                    b"state" => {
                        let (s, end) = quoted_span(body, i)?;
                        state_open = Some(s == b"open");
                        i = end;
                    }
                    b"settlement_period" => {
                        let (s, end) = quoted_span(body, i)?;
                        perpetual = Some(s == b"perpetual");
                        i = end;
                    }
                    b"tick_size" => {
                        let (v, end) = bare_1e9(body, i)?;
                        tick_size = Some(v);
                        i = end;
                    }
                    b"contract_size" => {
                        let (v, end) = bare_1e9(body, i)?;
                        contract_size = Some(v);
                        i = end;
                    }
                    b"min_trade_amount" => {
                        let (v, end) = bare_1e9(body, i)?;
                        min_trade_amount = Some(v);
                        i = end;
                    }
                    b"tick_size_steps" => {
                        let (n, end) = parse_tick_steps(body, i, &mut tick_size_steps)?;
                        n_tick_steps = n;
                        i = end;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(DeribitDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(DeribitDiscoveryErr::BadRow),
        }
    }

    if instrument_name_len == 0 || !kind_seen {
        return Err(DeribitDiscoveryErr::BadRow);
    }
    let is_active = is_active.ok_or(DeribitDiscoveryErr::BadRow)?;
    let state_open = state_open.ok_or(DeribitDiscoveryErr::BadRow)?;
    let row = DeribitInstrumentRow {
        instrument_name,
        instrument_name_len,
        perpetual: perpetual.ok_or(DeribitDiscoveryErr::BadRow)?,
        live: state_open && is_active,
        tick_size_1e9: tick_size.ok_or(DeribitDiscoveryErr::BadRow)?,
        tick_size_steps,
        n_tick_steps,
        contract_size_1e9: contract_size.ok_or(DeribitDiscoveryErr::BadRow)?,
        min_trade_amount_1e9: min_trade_amount.ok_or(DeribitDiscoveryErr::BadRow)?,
    };
    Ok((row, i))
}

/// Parse the `tick_size_steps` array at `pos` (must point at `[`) into
/// `steps`. `[]` (every current instrument) yields 0 steps; the
/// official non-empty shape is objects with bare-number `above_price`
/// / `tick_size` keys. More than [`DERIBIT_TICK_STEPS_CAP`] entries is
/// a contract change → `BadRow`. Returns `(n_steps, pos after ])`.
fn parse_tick_steps(
    body: &[u8],
    pos: usize,
    steps: &mut [(i64, i64); DERIBIT_TICK_STEPS_CAP],
) -> Result<(u8, usize), DeribitDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'[' {
        return Err(DeribitDiscoveryErr::BadRow);
    }
    let mut i = pos + 1;
    let mut n = 0usize;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(DeribitDiscoveryErr::Truncated);
        }
        match body[i] {
            b']' => return Ok((n as u8, i + 1)),
            b',' => i += 1,
            b'{' => {
                let (step, end) = parse_one_step(body, i)?;
                if n >= DERIBIT_TICK_STEPS_CAP {
                    return Err(DeribitDiscoveryErr::BadRow);
                }
                steps[n] = step;
                n += 1;
                i = end;
            }
            _ => return Err(DeribitDiscoveryErr::BadRow),
        }
    }
}

/// Parse one `{"above_price": <n>, "tick_size": <n>}` step object at
/// `pos` (must point at `{`). Both keys required, bare numbers, any
/// order; unknown keys skipped structurally. Returns
/// `((above_price_1e9, tick_size_1e9), pos after })`.
fn parse_one_step(body: &[u8], pos: usize) -> Result<((i64, i64), usize), DeribitDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;
    let mut above_price: Option<i64> = None;
    let mut tick_size: Option<i64> = None;
    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(DeribitDiscoveryErr::Truncated);
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
                    skip_string(body, key_start).ok_or(DeribitDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                if i >= body.len() || body[i] != b':' {
                    return Err(DeribitDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                match key {
                    b"above_price" => {
                        let (v, end) = bare_1e9(body, i)?;
                        above_price = Some(v);
                        i = end;
                    }
                    b"tick_size" => {
                        let (v, end) = bare_1e9(body, i)?;
                        tick_size = Some(v);
                        i = end;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(DeribitDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(DeribitDiscoveryErr::BadRow),
        }
    }
    let a = above_price.ok_or(DeribitDiscoveryErr::BadRow)?;
    let t = tick_size.ok_or(DeribitDiscoveryErr::BadRow)?;
    Ok(((a, t), i))
}

/// Read a quoted string value at `pos` (must point at `"`). Returns
/// the in-quote span and the position after the closing quote. The
/// captured Deribit fields never contain escapes; a backslash inside
/// the span is rejected rather than unescaped.
fn quoted_span(body: &[u8], pos: usize) -> Result<(&[u8], usize), DeribitDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'"' {
        return Err(DeribitDiscoveryErr::BadRow);
    }
    let start = pos + 1;
    let end_q = skip_string(body, start).ok_or(DeribitDiscoveryErr::Truncated)?;
    let span = &body[start..end_q - 1];
    if span.contains(&b'\\') {
        return Err(DeribitDiscoveryErr::BadRow);
    }
    Ok((span, end_q))
}

/// Read a bare JSON number at `pos` scaled ×1e9
/// ([`core_parse::scan_number_sci_1e9`] — scientific notation
/// accepted). A quoted value fails here (a leading `"` is not a
/// number byte): captured numbers are bare on this wire, quoted means
/// the venue contract changed. Trailing junk after the digits is
/// caught by the object walker (next byte must be `,` or `}`).
fn bare_1e9(body: &[u8], pos: usize) -> Result<(i64, usize), DeribitDiscoveryErr> {
    scan_number_sci_1e9(body, pos).ok_or(DeribitDiscoveryErr::BadRow)
}

/// Read a bare `true` / `false` at `pos`.
fn bare_bool(body: &[u8], pos: usize) -> Result<(bool, usize), DeribitDiscoveryErr> {
    if body.len() >= pos + 4 && &body[pos..pos + 4] == b"true" {
        return Ok((true, pos + 4));
    }
    if body.len() >= pos + 5 && &body[pos..pos + 5] == b"false" {
        return Ok((false, pos + 5));
    }
    Err(DeribitDiscoveryErr::BadRow)
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Live-probe rows (2026-08-14): the real reversed BTC-PERPETUAL
    /// row with its noise fields retained (huge bare ints, bare
    /// floats, uuid string), a linear USDC perp with a
    /// scientific-notation tick (`1e-05`) and a synthetic non-empty
    /// `tick_size_steps`, and a dated future that is not tradable
    /// (`state:"closed"`) despite `is_active:true`. Trailing
    /// `usIn`/`usOut`/`usDiff` prove the walker stops at `]`.
    const PAGE_MIXED: &[u8] = br#"{"jsonrpc":"2.0","result":[
      {"min_trade_amount": 10.0, "settlement_period": "perpetual", "contract_size": 10.0, "state": "open", "expiration_timestamp": 32503708800000, "kind": "future", "instrument_type": "reversed", "is_active": true, "tick_size_steps": [], "creation_timestamp": 1534242287000, "base_currency_uuid": "5b71fc48-3dd3-540c-809b-f8c94d0e68b5", "index_id": 1000033, "settlement_currency": "BTC", "quote_currency": "USD", "instrument_id": 210838, "price_index": "btc_usd", "lot_size": 10.0, "base_currency": "BTC", "maker_commission": 0.00015, "max_leverage": 50, "tick_size": 0.5, "counter_currency": "USD", "instrument_name": "BTC-PERPETUAL", "taker_commission": 0.00035},
      {"min_trade_amount": 1, "settlement_period": "perpetual", "contract_size": 1.0, "state": "open", "expiration_timestamp": 32503708800000, "kind": "future", "instrument_type": "linear", "is_active": true, "tick_size_steps": [{"above_price": 100000, "tick_size": 10}], "settlement_currency": "USDC", "quote_currency": "USDC", "price_index": "xrp_usdc", "lot_size": 1.0, "base_currency": "XRP", "maker_commission": 0.00015, "max_leverage": 50, "tick_size": 1e-05, "counter_currency": "USDC", "instrument_name": "XRP_USDC-PERPETUAL", "taker_commission": 0.00035},
      {"min_trade_amount": 10.0, "settlement_period": "month", "contract_size": 10.0, "state": "closed", "expiration_timestamp": 1798185600000, "kind": "future", "instrument_type": "reversed", "is_active": true, "tick_size_steps": [], "settlement_currency": "BTC", "quote_currency": "USD", "price_index": "btc_usd", "lot_size": 10.0, "base_currency": "BTC", "tick_size": 2.5, "counter_currency": "USD", "instrument_name": "BTC-25DEC26", "taker_commission": 0.00035}
    ],"usIn":1755150000000000,"usOut":1755150000003000,"usDiff":3000,"testnet":false}"#;

    /// Wrap row objects into a valid envelope (test-only allocation;
    /// boot code never builds bodies).
    fn page(rows: &str) -> Vec<u8> {
        let mut body = Vec::with_capacity(rows.len() + 96);
        body.extend_from_slice(br#"{"jsonrpc":"2.0","result":["#);
        body.extend_from_slice(rows.as_bytes());
        body.extend_from_slice(br#"],"usIn":1,"usOut":2,"usDiff":1,"testnet":false}"#);
        body
    }

    #[test]
    fn ingest_parses_perpetual_linear_and_dated_rows() {
        let mut d = DeribitDiscovery::new();
        let added = d.ingest_body(PAGE_MIXED).expect("parse ok");
        assert_eq!(added, 3);
        assert_eq!(d.universe_total(), 3);
        assert_eq!(d.universe_live(), 2); // dated future is closed

        let perp = d.find(b"BTC-PERPETUAL").expect("reversed perp row");
        assert_eq!(perp.instrument_name(), b"BTC-PERPETUAL");
        assert!(perp.perpetual);
        assert!(perp.live);
        assert_eq!(perp.tick_size_1e9, 500_000_000); // 0.5
        assert_eq!(perp.n_tick_steps, 0); // [] on wire
        assert_eq!(perp.contract_size_1e9, 10_000_000_000); // 10.0
        assert_eq!(perp.min_trade_amount_1e9, 10_000_000_000); // USD × 1e9

        let usdc = d.find(b"XRP_USDC-PERPETUAL").expect("linear perp row");
        assert!(usdc.perpetual);
        assert!(usdc.live);
        assert_eq!(usdc.tick_size_1e9, 10_000); // 1e-05
        assert_eq!(usdc.n_tick_steps, 1);
        // above 100000 → tick 10, both ×1e9.
        assert_eq!(usdc.tick_size_steps[0], (100_000_000_000_000, 10_000_000_000));
        assert_eq!(usdc.contract_size_1e9, 1_000_000_000); // 1.0
        assert_eq!(usdc.min_trade_amount_1e9, 1_000_000_000); // bare int 1

        let dated = d.find(b"BTC-25DEC26").expect("dated future row");
        assert!(!dated.perpetual); // settlement_period "month"
        assert!(!dated.live); // state "closed" gates even when is_active
        assert_eq!(dated.tick_size_1e9, 2_500_000_000); // 2.5
    }

    #[test]
    fn ingest_accumulates_across_pages() {
        let mut d = DeribitDiscovery::default();
        d.ingest_body(&page(r#"{"instrument_name":"BTC-PERPETUAL","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":0.5,"contract_size":10.0,"min_trade_amount":10.0}"#)).unwrap();
        d.ingest_body(&page(r#"{"instrument_name":"ETH-PERPETUAL","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":0.05,"contract_size":1.0,"min_trade_amount":1}"#)).unwrap();
        assert_eq!(d.universe_total(), 2);
        assert_eq!(d.universe_live(), 2);
        assert!(d.find(b"BTC-PERPETUAL").is_some());
        assert!(d.find(b"ETH-PERPETUAL").is_some());
        assert!(d.find(b"SOL-PERPETUAL").is_none());
    }

    #[test]
    fn ingest_rejects_jsonrpc_error_body() {
        let mut d = DeribitDiscovery::new();
        // Error responses carry `"error":{...}` and no `result`.
        let e = d
            .ingest_body(br#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Invalid params"},"usIn":1,"usOut":2,"usDiff":1,"testnet":false}"#)
            .unwrap_err();
        assert_eq!(e, DeribitDiscoveryErr::Envelope);
    }

    #[test]
    fn ingest_rejects_missing_jsonrpc_and_non_array_result() {
        let mut d = DeribitDiscovery::new();
        // No jsonrpc version tag at all.
        assert_eq!(
            d.ingest_body(br#"{"result":[]}"#).unwrap_err(),
            DeribitDiscoveryErr::Envelope
        );
        // Wrong version.
        assert_eq!(
            d.ingest_body(br#"{"jsonrpc":"1.0","result":[]}"#).unwrap_err(),
            DeribitDiscoveryErr::Envelope
        );
        // Missing result.
        assert_eq!(
            d.ingest_body(br#"{"jsonrpc":"2.0","testnet":false}"#).unwrap_err(),
            DeribitDiscoveryErr::Envelope
        );
        // result is not an array.
        assert_eq!(
            d.ingest_body(br#"{"jsonrpc":"2.0","result":{},"testnet":false}"#)
                .unwrap_err(),
            DeribitDiscoveryErr::Envelope
        );
    }

    #[test]
    fn ingest_rejects_row_contract_violations() {
        let mut d = DeribitDiscovery::new();
        // Missing instrument_name.
        assert_eq!(
            d.ingest_body(&page(r#"{"kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":0.5,"contract_size":10.0,"min_trade_amount":10.0}"#)).unwrap_err(),
            DeribitDiscoveryErr::BadRow
        );
        // kind:"option" — only kind=future pages are fetched (§4.2).
        assert_eq!(
            d.ingest_body(&page(r#"{"instrument_name":"BTC-X","kind":"option","is_active":true,"state":"open","settlement_period":"week","tick_size":0.0005,"contract_size":1.0,"min_trade_amount":0.1}"#)).unwrap_err(),
            DeribitDiscoveryErr::BadRow
        );
        // Dotted instrument_name — the SymbolTableErr::HasDot
        // invariant (dots would corrupt channel-name parsing).
        assert_eq!(
            d.ingest_body(&page(r#"{"instrument_name":"BTC-PERP.X","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":0.5,"contract_size":10.0,"min_trade_amount":10.0}"#)).unwrap_err(),
            DeribitDiscoveryErr::BadRow
        );
        // Over-long instrument_name (36 > DERIBIT_INSTR_MAX = 32).
        assert_eq!(
            d.ingest_body(&page(r#"{"instrument_name":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":0.5,"contract_size":10.0,"min_trade_amount":10.0}"#)).unwrap_err(),
            DeribitDiscoveryErr::BadRow
        );
        // Quoted tick_size — captured numbers must be bare; quoted
        // means the venue contract changed.
        assert_eq!(
            d.ingest_body(&page(r#"{"instrument_name":"BTC-PERPETUAL","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":"0.5","contract_size":10.0,"min_trade_amount":10.0}"#)).unwrap_err(),
            DeribitDiscoveryErr::BadRow
        );
        // Missing tick_size.
        assert_eq!(
            d.ingest_body(&page(r#"{"instrument_name":"BTC-PERPETUAL","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","contract_size":10.0,"min_trade_amount":10.0}"#)).unwrap_err(),
            DeribitDiscoveryErr::BadRow
        );
        // Five tick steps (> DERIBIT_TICK_STEPS_CAP = 4).
        assert_eq!(
            d.ingest_body(&page(r#"{"instrument_name":"BTC-PERPETUAL","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":0.5,"tick_size_steps":[{"above_price":1,"tick_size":1},{"above_price":2,"tick_size":2},{"above_price":3,"tick_size":3},{"above_price":4,"tick_size":4},{"above_price":5,"tick_size":5}],"contract_size":10.0,"min_trade_amount":10.0}"#)).unwrap_err(),
            DeribitDiscoveryErr::BadRow
        );
    }

    #[test]
    fn ingest_rejects_truncated_array() {
        let mut d = DeribitDiscovery::new();
        // Body ends inside a row object.
        assert_eq!(
            d.ingest_body(br#"{"jsonrpc":"2.0","result":[{"instrument_name":"BTC-PERPETUAL","kind":"future""#)
                .unwrap_err(),
            DeribitDiscoveryErr::Truncated
        );
        // Body ends right after the array opens.
        assert_eq!(
            d.ingest_body(br#"{"jsonrpc":"2.0","result":["#).unwrap_err(),
            DeribitDiscoveryErr::Truncated
        );
        // Unterminated string value.
        assert_eq!(
            d.ingest_body(br#"{"jsonrpc":"2.0","result":[{"instrument_name":"BTC-PERP"#)
                .unwrap_err(),
            DeribitDiscoveryErr::Truncated
        );
    }

    #[test]
    fn ingest_enforces_rows_cap() {
        let mut d = DeribitDiscovery::new();
        // Synthesize pages of minimal rows until the cap trips.
        // Test-only allocation; boot code never builds bodies.
        let row = r#"{"instrument_name":"BTC-PERPETUAL","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":0.5,"contract_size":10.0,"min_trade_amount":10.0}"#;
        let per_page = 256;
        let mut rows = String::with_capacity(per_page * (row.len() + 1));
        for k in 0..per_page {
            if k > 0 {
                rows.push(',');
            }
            rows.push_str(row);
        }
        let body = page(&rows);
        for _ in 0..(DERIBIT_DISCOVERY_ROWS_CAP / per_page) {
            d.ingest_body(&body).expect("under cap");
        }
        assert_eq!(d.ingest_body(&body).unwrap_err(), DeribitDiscoveryErr::TooMany);
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
            let mut d = DeribitDiscovery::new();
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
