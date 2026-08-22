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
//! `kind=future`. A row carrying any other `kind` on a futures page is
//! a contract violation ([`DeribitDiscoveryErr::BadRow`]), not a
//! filter case. The venue rate-limits `public/get_instruments` to
//! 1 req/s (burst 50) — pacing between the per-currency fetches is the
//! **caller's** job (cli boot sequence), not this module's.
//!
//! ## M2.1 options (capped chain; docs/m2-progress.md design entry)
//!
//! `kind=option` pages are fetched per configured underlying into a
//! SEPARATE table instance via [`DeribitDiscovery::ingest_options_body`]
//! (same walker, option contract: `option_type` / `strike` /
//! `expiration_timestamp` required; cap
//! [`DERIBIT_OPT_DISCOVERY_ROWS_CAP`] — a live BTC chain is order-1k
//! rows). The capped universe policy is applied AFTER the full-page
//! parse by [`select_capped_chain`] (nearest-E expiries × K
//! nearest-ATM strikes, calls+puts), centered on
//! [`parse_index_price`]'s boot-time reference. Filtered-out
//! instruments are never allocated ordinals (options-plan §2); the 8e
//! boot-snapshot doctrine stands — chain rolls enter at the next boot.
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

use core_parse::{find_field, scan_number_sci_1e9, scan_u64, skip_json_value, skip_string, skip_ws};

use crate::DERIBIT_INSTR_MAX;

/// Hard cap on parsed instrument rows across all fetched currency
/// pages. Live universe 2026-08-14: BTC + ETH + USDC futures ≈ 92;
/// 10× headroom.
pub const DERIBIT_DISCOVERY_ROWS_CAP: usize = 1024;

/// Hard cap on parsed OPTION rows per underlying page (M2.1). A live
/// BTC chain is order-1k instruments (expiries × strikes × 2); 4×
/// headroom. The table may realloc past its initial reserve on an
/// options page — boot-only, allocation permitted by doctrine.
pub const DERIBIT_OPT_DISCOVERY_ROWS_CAP: usize = 4096;

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
    /// `kind == "option"` (M2.1 options pages; futures rows: false).
    pub is_option: bool,
    /// `option_type == "call"` (required on option rows; false
    /// otherwise).
    pub is_call: bool,
    /// `strike` ×1e9 (required on option rows; 0 otherwise).
    pub strike_1e9: i64,
    /// `expiration_timestamp` in ms since epoch — required on option
    /// rows; captured on any row that carries it (dated futures do;
    /// perpetuals send the year-3000 sentinel `32503708800000`).
    pub expiration_ts_ms: i64,
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

    /// Parse one `kind=future` `get_instruments` body (one currency
    /// page) into the table. Returns the number of rows added. Call
    /// once per fetched page (BTC, ETH, USDC — caller paces at
    /// 1 req/s, §4.2); counts accumulate.
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, DeribitDiscoveryErr> {
        self.ingest_inner(body, RowKind::Future)
    }

    /// Parse one `kind=option` `get_instruments` body (one UNDERLYING
    /// page, M2.1) into the table. Option rows additionally require
    /// `option_type` / `strike` / `expiration_timestamp`; the row cap
    /// is [`DERIBIT_OPT_DISCOVERY_ROWS_CAP`]. Use a SEPARATE table
    /// instance per options page (the futures coverage counters must
    /// not mix with chain rows); the caller then applies
    /// [`select_capped_chain`] to `rows()`.
    pub fn ingest_options_body(&mut self, body: &[u8]) -> Result<u32, DeribitDiscoveryErr> {
        self.ingest_inner(body, RowKind::Option)
    }

    fn ingest_inner(&mut self, body: &[u8], want: RowKind) -> Result<u32, DeribitDiscoveryErr> {
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
                    let (row, end) = parse_row(body, i, want)?;
                    let cap = match want {
                        RowKind::Future => DERIBIT_DISCOVERY_ROWS_CAP,
                        RowKind::Option => DERIBIT_OPT_DISCOVERY_ROWS_CAP,
                    };
                    if self.rows.len() >= cap {
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

    /// All parsed rows in wire order (M2.1: [`select_capped_chain`]
    /// input for an options table).
    #[inline]
    pub fn rows(&self) -> &[DeribitInstrumentRow] {
        &self.rows
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

// ---------------------------------------------------------------
// M2.1: index price (ATM reference) + capped-chain selection
// ---------------------------------------------------------------

/// The venue index name for an options underlying currency —
/// `"BTC"` → `"btc_usd"` (the coin-settled BTC/ETH families in M2.1
/// scope; a wrong name fails the REST fetch loudly at boot, which is
/// the intended fail-fast). Boot-only: allocates.
pub fn index_name(ccy: &str) -> String {
    let mut s = ccy.to_ascii_lowercase();
    s.push_str("_usd");
    s
}

/// Parse a `GET /api/v2/public/get_index_price?index_name=<idx>` body
/// into the index price ×1e9 — the boot-time ATM reference for
/// [`select_capped_chain`]. Envelope law matches `ingest_body`
/// (`"jsonrpc":"2.0"` + `"result"`; JSON-RPC error bodies carry no
/// `result` → [`DeribitDiscoveryErr::Envelope`]). `index_price` is a
/// bare number (sci notation accepted); quoted/missing/nonpositive →
/// [`DeribitDiscoveryErr::BadRow`].
pub fn parse_index_price(body: &[u8]) -> Result<i64, DeribitDiscoveryErr> {
    let ver_pos = find_field(body, b"\"jsonrpc\":").ok_or(DeribitDiscoveryErr::Envelope)?;
    let v = skip_ws(body, ver_pos);
    if body.len() < v + 5 || &body[v..v + 5] != b"\"2.0\"" {
        return Err(DeribitDiscoveryErr::Envelope);
    }
    let res_pos = find_field(body, b"\"result\":").ok_or(DeribitDiscoveryErr::Envelope)?;
    let i = skip_ws(body, res_pos);
    if i >= body.len() || body[i] != b'{' {
        return Err(DeribitDiscoveryErr::Envelope);
    }
    let px_pos = find_field(body, b"\"index_price\":").ok_or(DeribitDiscoveryErr::BadRow)?;
    let j = skip_ws(body, px_pos);
    let (px, _) = scan_number_sci_1e9(body, j).ok_or(DeribitDiscoveryErr::BadRow)?;
    if px <= 0 {
        return Err(DeribitDiscoveryErr::BadRow);
    }
    Ok(px)
}

/// The `options_select::ChainRow` view of a Deribit discovery row —
/// what the shared selection law reads (M2-close extraction; this crate
/// was the law source and remains the behavioral reference via its
/// tests + proptests + fuzz corpus).
impl options_select::ChainRow for DeribitInstrumentRow {
    #[inline]
    fn exp_ms(&self) -> i64 {
        self.expiration_ts_ms
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

/// Apply the capped universe policy (M2 design entry; mvp-plan §4-M2
/// / options-plan §2) to ONE underlying's parsed option rows.
///
/// The selection LAW (nearest-E distinct expiries asc; per expiry the
/// K nearest-ATM strikes position-based, last K/2 at-or-below + first
/// K/2 above, no backfill; C before P; deterministic allocation order;
/// ≤ E×K×2) lives in `options-select` since the M2-close extraction —
/// this wrapper owns only the VENUE candidacy predicate
/// (`is_option && live && expiration_ts_ms > now_ms`) and the frozen
/// public signature every call site / test / fuzz target pins.
/// Precondition: `rows` is one underlying's page, ingested once (the
/// venue lists each instrument once). Boot-only: allocates freely.
pub fn select_capped_chain(
    rows: &[DeribitInstrumentRow],
    index_px_1e9: i64,
    expiries_e: u32,
    strikes_k: u32,
    now_ms: i64,
) -> Vec<DeribitInstrumentRow> {
    options_select::select_capped_chain(
        rows,
        |r: &DeribitInstrumentRow| r.is_option && r.live && r.expiration_ts_ms > now_ms,
        index_px_1e9,
        expiries_e,
        strikes_k,
    )
}

/// Which `kind=` page a row is being parsed from (M2.1). Determines
/// the accepted `kind` value and the required-field set.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum RowKind {
    Future,
    Option,
}

/// Parse one instrument object starting at `pos` (must point at `{`).
/// Returns the row and the position after the closing `}`.
fn parse_row(
    body: &[u8],
    pos: usize,
    want: RowKind,
) -> Result<(DeribitInstrumentRow, usize), DeribitDiscoveryErr> {
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
    let mut is_call: Option<bool> = None;
    let mut strike: Option<i64> = None;
    let mut expiration_ts_ms: Option<i64> = None;

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
                        // A page carries exactly the `kind` it was
                        // fetched with; anything else is a contract
                        // violation, not a filter case.
                        let (s, end) = quoted_span(body, i)?;
                        let expect: &[u8] = match want {
                            RowKind::Future => b"future",
                            RowKind::Option => b"option",
                        };
                        if s != expect {
                            return Err(DeribitDiscoveryErr::BadRow);
                        }
                        kind_seen = true;
                        i = end;
                    }
                    b"option_type" => {
                        let (s, end) = quoted_span(body, i)?;
                        is_call = Some(match s {
                            b"call" => true,
                            b"put" => false,
                            _ => return Err(DeribitDiscoveryErr::BadRow),
                        });
                        i = end;
                    }
                    b"strike" => {
                        let (v, end) = bare_1e9(body, i)?;
                        strike = Some(v);
                        i = end;
                    }
                    b"expiration_timestamp" => {
                        // Bare integer ms since epoch — too large for
                        // the ×1e9 scanners (1.8e12 × 1e9 overflows
                        // i64); plain u64 scan, checked into i64.
                        let (v, end) =
                            scan_u64(body, i).ok_or(DeribitDiscoveryErr::BadRow)?;
                        if v > i64::MAX as u64 {
                            return Err(DeribitDiscoveryErr::BadRow);
                        }
                        expiration_ts_ms = Some(v as i64);
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
    // Option-row contract (M2.1): the chain-defining fields are
    // required — a chain row without them is unusable for the capped
    // filter and means the venue contract changed.
    if want == RowKind::Option
        && (is_call.is_none() || strike.is_none() || expiration_ts_ms.is_none())
    {
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
        is_option: want == RowKind::Option,
        is_call: is_call.unwrap_or(false),
        strike_1e9: strike.unwrap_or(0),
        expiration_ts_ms: expiration_ts_ms.unwrap_or(0),
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

    // -----------------------------------------------------------
    // M2.1: options pages + index price + capped-chain selection
    // -----------------------------------------------------------

    /// One option row in the live wire shape (kind=option page).
    /// Test-only allocation; boot code never builds bodies.
    fn opt_row(name: &str, typ: &str, strike: &str, exp_ms: i64, open: bool) -> String {
        format!(
            r#"{{"instrument_name":"{name}","kind":"option","option_type":"{typ}","strike":{strike},"expiration_timestamp":{exp_ms},"is_active":true,"state":"{}","settlement_period":"week","tick_size":0.0001,"tick_size_steps":[{{"above_price":0.005,"tick_size":0.0005}}],"contract_size":1.0,"min_trade_amount":0.1,"base_currency":"BTC","quote_currency":"USD","price_index":"btc_usd"}}"#,
            if open { "open" } else { "closed" }
        )
    }

    const EXP1: i64 = 1_774_598_400_000; // nearer expiry (ms)
    const EXP2: i64 = 1_775_203_200_000; // next expiry
    const EXP3: i64 = 1_775_808_000_000; // third expiry
    const NOW: i64 = 1_774_000_000_000;

    /// A 3-expiry × 4-strike × C/P grid around index 100k, plus noise:
    /// a closed row and an already-expired expiry.
    fn opt_grid() -> DeribitDiscovery {
        let mut rows: Vec<String> = Vec::new();
        for (e, tag) in [(EXP1, "27MAR26"), (EXP2, "3APR26"), (EXP3, "10APR26")] {
            for s in ["90000", "95000", "100000", "105000"] {
                for (t, suf) in [("call", "C"), ("put", "P")] {
                    rows.push(opt_row(&format!("BTC-{tag}-{s}-{suf}"), t, s, e, true));
                }
            }
        }
        rows.push(opt_row("BTC-27MAR26-110000-C", "call", "110000", EXP1, false)); // closed
        rows.push(opt_row("BTC-OLD-90000-C", "call", "90000", NOW - 1_000, true)); // expired
        let mut d = DeribitDiscovery::new();
        d.ingest_options_body(&page(&rows.join(","))).expect("grid parses");
        d
    }

    #[test]
    fn options_page_parses_chain_fields() {
        let d = opt_grid();
        assert_eq!(d.universe_total(), 26);
        let c = d.find(b"BTC-27MAR26-100000-C").expect("call row");
        assert!(c.is_option && c.is_call && c.live);
        assert_eq!(c.strike_1e9, 100_000_000_000_000); // 100000 × 1e9
        assert_eq!(c.expiration_ts_ms, EXP1);
        assert_eq!(c.tick_size_1e9, 100_000); // 0.0001
        assert_eq!(c.n_tick_steps, 1);
        assert_eq!(c.tick_size_steps[0], (5_000_000, 500_000)); // 0.005 / 0.0005
        let p = d.find(b"BTC-27MAR26-100000-P").expect("put row");
        assert!(p.is_option && !p.is_call);
        assert!(!d.find(b"BTC-27MAR26-110000-C").expect("closed row kept").live);
    }

    #[test]
    fn options_page_rejects_future_rows_and_vice_versa() {
        let fut = r#"{"instrument_name":"BTC-PERPETUAL","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":0.5,"contract_size":10.0,"min_trade_amount":10.0}"#;
        let mut d = DeribitDiscovery::new();
        assert_eq!(
            d.ingest_options_body(&page(fut)).unwrap_err(),
            DeribitDiscoveryErr::BadRow
        );
        // The reverse direction is pinned by
        // `ingest_rejects_row_contract_violations` (kind:"option" on a
        // futures page).
    }

    #[test]
    fn option_row_missing_chain_fields_rejected() {
        let full = opt_row("BTC-27MAR26-100000-C", "call", "100000", EXP1, true);
        for gone in ["\"option_type\":\"call\",", "\"strike\":100000,", "\"expiration_timestamp\":1774598400000,"] {
            let broken = full.replacen(gone, "", 1);
            assert_ne!(broken, full, "field `{gone}` must exist in the fixture");
            let mut d = DeribitDiscovery::new();
            assert_eq!(
                d.ingest_options_body(&page(&broken)).unwrap_err(),
                DeribitDiscoveryErr::BadRow,
                "missing {gone} must reject"
            );
        }
        // Bad option_type value.
        let bad = full.replacen("\"call\"", "\"straddle\"", 1);
        let mut d = DeribitDiscovery::new();
        assert_eq!(d.ingest_options_body(&page(&bad)).unwrap_err(), DeribitDiscoveryErr::BadRow);
    }

    #[test]
    fn option_sci_notation_strike_parses() {
        // Cheap-coin chains quote fractional strikes; sci notation is
        // live-observed on this venue's number fields (8e).
        let row = opt_row("XRP_USDC-27MAR26-5000-C", "call", "5e-1", EXP1, true);
        let mut d = DeribitDiscovery::new();
        d.ingest_options_body(&page(&row)).expect("parses");
        assert_eq!(d.rows()[0].strike_1e9, 500_000_000); // 0.5 × 1e9
    }

    #[test]
    fn index_price_parses_and_rejects() {
        let ok = br#"{"jsonrpc":"2.0","result":{"index_price":109731.42,"estimated_delivery_price":109731.42},"usIn":1,"usOut":2,"usDiff":1,"testnet":false}"#;
        assert_eq!(parse_index_price(ok).expect("parses"), 109_731_420_000_000);
        let sci = br#"{"jsonrpc":"2.0","result":{"index_price":1.0973142e5}}"#;
        assert_eq!(parse_index_price(sci).expect("sci parses"), 109_731_420_000_000);
        // JSON-RPC error body: no result.
        let err_body = br#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"bad index"}}"#;
        assert_eq!(parse_index_price(err_body).unwrap_err(), DeribitDiscoveryErr::Envelope);
        // Quoted number = contract change.
        let quoted = br#"{"jsonrpc":"2.0","result":{"index_price":"109731.42"}}"#;
        assert_eq!(parse_index_price(quoted).unwrap_err(), DeribitDiscoveryErr::BadRow);
        // Nonpositive.
        let zero = br#"{"jsonrpc":"2.0","result":{"index_price":0}}"#;
        assert_eq!(parse_index_price(zero).unwrap_err(), DeribitDiscoveryErr::BadRow);
        // Missing field.
        let none = br#"{"jsonrpc":"2.0","result":{"estimated_delivery_price":1.0}}"#;
        assert_eq!(parse_index_price(none).unwrap_err(), DeribitDiscoveryErr::BadRow);
    }

    #[test]
    fn index_name_maps_ccy() {
        assert_eq!(index_name("BTC"), "btc_usd");
        assert_eq!(index_name("ETH"), "eth_usd");
    }

    fn names(sel: &[DeribitInstrumentRow]) -> Vec<String> {
        sel.iter()
            .map(|r| String::from_utf8(r.instrument_name().to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn capped_chain_selects_e2_k2_deterministically() {
        let d = opt_grid();
        // Index 101k: at-or-below = 90/95/100k (take last 1), above =
        // 105k (take first 1). E=2 → EXP1 + EXP2 only.
        let sel = select_capped_chain(d.rows(), 101_000_000_000_000, 2, 2, NOW);
        assert_eq!(
            names(&sel),
            vec![
                "BTC-27MAR26-100000-C",
                "BTC-27MAR26-100000-P",
                "BTC-27MAR26-105000-C",
                "BTC-27MAR26-105000-P",
                "BTC-3APR26-100000-C",
                "BTC-3APR26-100000-P",
                "BTC-3APR26-105000-C",
                "BTC-3APR26-105000-P",
            ]
        );
        // Determinism: identical rerun, identical output.
        let sel2 = select_capped_chain(d.rows(), 101_000_000_000_000, 2, 2, NOW);
        assert_eq!(names(&sel), names(&sel2));
    }

    #[test]
    fn capped_chain_excludes_dead_expired_and_respects_cap() {
        let d = opt_grid();
        // K larger than the grid: everything live at E=1 EXP1 — the
        // closed 110k row and the expired row must NOT appear.
        let sel = select_capped_chain(d.rows(), 100_000_000_000_000, 1, 8, NOW);
        assert_eq!(sel.len(), 8); // 4 strikes × C/P
        for r in &sel {
            assert!(r.live && r.expiration_ts_ms == EXP1);
        }
        assert!(!names(&sel).iter().any(|n| n.contains("110000") || n.contains("OLD")));
        // Cap law: never more than E×K×2.
        let all = select_capped_chain(d.rows(), 100_000_000_000_000, 4, 32, NOW);
        assert!(all.len() as u32 <= 4 * 32 * 2);
        assert_eq!(all.len(), 24); // 3 expiries × 4 strikes × 2
    }

    #[test]
    fn capped_chain_one_sided_market_takes_what_exists() {
        // Index far below every strike: no at-or-below side. K=4 →
        // first 2 above only; no backfill.
        let d = opt_grid();
        let sel = select_capped_chain(d.rows(), 1_000_000_000, 1, 4, NOW);
        assert_eq!(
            names(&sel),
            vec!["BTC-27MAR26-90000-C", "BTC-27MAR26-90000-P", "BTC-27MAR26-95000-C", "BTC-27MAR26-95000-P"]
        );
    }

    #[test]
    fn capped_chain_missing_twin_emits_single_leg() {
        let rows = [
            opt_row("BTC-27MAR26-100000-C", "call", "100000", EXP1, true),
            // No 100000-P twin.
        ]
        .join(",");
        let mut d = DeribitDiscovery::new();
        d.ingest_options_body(&page(&rows)).unwrap();
        let sel = select_capped_chain(d.rows(), 100_000_000_000_000, 2, 8, NOW);
        assert_eq!(names(&sel), vec!["BTC-27MAR26-100000-C"]);
    }

    #[test]
    fn options_rows_cap_enforced_above_futures_cap() {
        // The options cap (4096) admits chains the futures cap (1024)
        // refuses. 1200 distinct minimal option rows: futures-cap
        // sized ingest would refuse; options ingest accepts.
        let mut rows: Vec<String> = Vec::with_capacity(1200);
        for k in 0..1200 {
            rows.push(opt_row(&format!("BTC-X-{k}-C"), "call", "1", EXP1, true));
        }
        let mut d = DeribitDiscovery::new();
        let n = d.ingest_options_body(&page(&rows.join(","))).expect("under options cap");
        assert_eq!(n, 1200);
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

        /// M2.1: the options walker + index-price parser never panic
        /// on arbitrary bytes either (same §21.3 bar).
        #[test]
        fn options_ingest_and_index_price_never_panic(
            input in proptest::collection::vec(any::<u8>(), 0..2048),
        ) {
            let mut d = DeribitDiscovery::new();
            if let Ok(n) = d.ingest_options_body(&input) {
                prop_assert_eq!(n, d.universe_total());
                prop_assert!(d.universe_live() <= d.universe_total());
            }
            let _ = parse_index_price(&input);
        }

        /// M2.1 selection invariants over generated chains: output
        /// ≤ E×K×2, only live/unexpired candidate rows, deterministic
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
            // Build synthetic rows directly (unit selection law —
            // wire parsing is covered above).
            let now_ms = 500_000i64;
            let mut rows: Vec<DeribitInstrumentRow> = Vec::new();
            let mut m = 0usize;
            for &exp in &exps {
                for &s in &strikes {
                    for call in [true, false] {
                        let mut name = [0u8; DERIBIT_INSTR_MAX];
                        let tag = format!("O-{exp}-{s}-{}", if call { "C" } else { "P" });
                        let tb = tag.as_bytes();
                        let n = tb.len().min(DERIBIT_INSTR_MAX);
                        name[..n].copy_from_slice(&tb[..n]);
                        rows.push(DeribitInstrumentRow {
                            instrument_name: name,
                            instrument_name_len: n as u8,
                            perpetual: false,
                            live: live_mask[m % live_mask.len()],
                            tick_size_1e9: 100_000,
                            tick_size_steps: [(0, 0); DERIBIT_TICK_STEPS_CAP],
                            n_tick_steps: 0,
                            contract_size_1e9: 1_000_000_000,
                            min_trade_amount_1e9: 100_000_000,
                            is_option: true,
                            is_call: call,
                            strike_1e9: s,
                            expiration_ts_ms: exp,
                        });
                        m += 1;
                    }
                }
            }
            let k = k_half * 2;
            let sel = select_capped_chain(&rows, idx, e, k, now_ms);
            prop_assert!(sel.len() as u32 <= e * k * 2);
            for r in &sel {
                prop_assert!(r.live && r.expiration_ts_ms > now_ms);
            }
            // Deterministic order law.
            for w in sel.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                let ka = (a.expiration_ts_ms, a.strike_1e9, !a.is_call);
                let kb = (b.expiration_ts_ms, b.strike_1e9, !b.is_call);
                prop_assert!(ka < kb, "order law violated");
            }
            // Rerun = identical.
            let sel2 = select_capped_chain(&rows, idx, e, k, now_ms);
            prop_assert_eq!(sel.len(), sel2.len());
        }
    }
}
