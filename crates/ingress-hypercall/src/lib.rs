// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-hypercall (HC3 + HC4 — venue byte 9)
//!
//! Hypercall public market data, DATA-ONLY (operator ruling O-HC1: no
//! exec arm, no keys, nothing here can place an order). Hypercall lists
//! USD cash-settled options on Hyperliquid oracles (BTC, ETH and ten
//! `xyz:*` equity series); its public book is a quote-PROVIDER layer
//! (RFQ indicative quotes), not a matching-engine L2.
//!
//! ## Wire (`wss://api.hypercall.xyz/ws`, captured 2026-09-25)
//!
//! Every message is a JSON object whose `"type"` names it; every price
//! and size is a JSON STRING (`"165.1175"`) scanned in place after the
//! quote. Channels, one `Subscribe` frame EACH (the frame takes ONE
//! `channel`):
//!
//! | channel | filter | message | emits |
//! |---|---|---|---|
//! | `indicative_market_data` | the boot universe, ONE frame | `IndicativeMarketData` | `Tick` (BBO change) + `ProviderQuote` events (≥ 2 providers) |
//! | `index_prices` | none | `IndexPriceUpdate` (every ~2 s, all underlyings) | `Mark` per `hypercall-idx:<U>` sym |
//! | `trades` | none | `Trade` | `Trade` (our universe only) |
//! | `market_updates` | none | `MarketUpdate` (listing / expiry) | counters; an expired sym stops emitting |
//!
//! plus `ClockSync {nonce}` → `ClockSynced {nonce, server_at}` (the
//! venue has no REST time endpoint), `Subscribed {channel}` acks and
//! `Error {message}`.
//!
//! ## THE subscribe law (plan D3, CONFIRMED on the Mac 2026-09-25)
//!
//! The indicative symbol set goes in exactly ONE `Subscribe` frame —
//! at connect and on every resubscribe. 3 frames of 192 symbols, or 12
//! of 48, were closed within 0.12 s with code 1008
//! `{"error":"slow_consumer","class":"replaceable_public","cause":"message_limit","recovery":"resubscribe"}`;
//! one frame of 576 streamed for 60 s. A bare underlying in `symbols`
//! matches nothing: the filter names full instruments.
//!
//! ## Quotes are the venue's, crossed or not
//!
//! `best_bid` / `best_ask` are the best across providers, and providers
//! disagree: measured 2026-09-25, 5.3 % of two-sided pushes were CROSSED
//! (every one a two-provider frame; the second provider quoted MU at
//! about half the main provider's premium). A crossed quote is emitted
//! as the venue published it — never "fixed" — and counted; the
//! per-provider sides ride `ChannelId::ProviderQuote` events so the
//! capture shows who quoted what. One-sided quotes are legal (the
//! missing side is `0 / 0`, the Deribit options convention); a push
//! with neither side (`num_providers: 0`) emits no tick and is counted.
//!
//! ## Units
//!
//! Tick prices are USD PREMIUM per 1-unit contract ×1e6; sizes are
//! contracts ×1e6; `venue_time_ms` is the quote's own `timestamp` (the
//! provider's `updated_at`) so the stale judgement sees a republished
//! old quote as old. Decimal strings longer than six fractional digits
//! (`"12.274000000000001"`, provider float noise) truncate at 1e-6.
//!
//! Everything after the handshake is zero-alloc and zero-copy: parsers
//! return spans into the rx buffer; the only copies are the marked
//! `// COPY:` sites (the boot symbol table, the subscribe render) and
//! the 64-byte PODs moved into their ring slots.

#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod counters;
pub mod discovery;
pub mod rest;
pub mod run_loop;

pub use counters::{HcCloseCause, HcCounters};

use core_parse::{scan_price_1e6, scan_u64, skip_json_value, skip_string, skip_ws};
use core_types::{fnv1a_64, SymbolId};

// ---------------------------------------------------------------
// Constants
// ---------------------------------------------------------------

/// Default WS host (`HYPERCALL_WS_HOST` overrides).
pub const HC_WS_HOST: &[u8] = b"api.hypercall.xyz";
/// WS path.
pub const HC_WS_PATH: &[u8] = b"/ws";
/// Default REST host (`HYPERCALL_REST_HOST` overrides).
pub const HC_REST_HOST: &str = "api.hypercall.xyz";

/// Longest instrument name accepted (`BTC-20261002-100000-C` is 21 B;
/// decimal strikes add a few).
pub const HC_SYMBOL_MAX: usize = 32;
/// Instrument capacity of one symbol table — the O-HC2 cap is 12 × 3 ×
/// 8 × 2 = 576; the headroom lets an operator widen the policy without
/// a code change.
pub const HC_MAX_INSTRUMENTS: usize = 1024;
/// Open-addressing slots (power of two, load ≤ ½ at capacity).
const HC_HASH_SLOTS: usize = 2 * HC_MAX_INSTRUMENTS;
const _: () = assert!(HC_HASH_SLOTS.is_power_of_two());
/// Longest underlying name (`SP500`, `SPCX`; room to spare).
pub const HC_UNDERLYING_MAX: usize = 12;
/// Underlyings one index table (and one `IndexPriceUpdate` walk) holds.
pub const HC_MAX_UNDERLYINGS: usize = 16;
/// Provider entries one quote walk reads; more are counted, not read.
pub const HC_MAX_PROVIDERS: usize = 8;

// ---------------------------------------------------------------
// Symbol tables (boot-built, hot lookups)
// ---------------------------------------------------------------

/// Why a table insert failed (boot-time; fatal in the cli).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbolTableErr {
    /// Table full.
    Full,
    /// Name longer than the table's row width.
    TooLong,
    /// Name empty.
    Empty,
    /// Name already present (a duplicate would double-subscribe and
    /// alias the per-row state).
    Duplicate,
}

/// One instrument row: the name inline (no pointer chase on the hot
/// compare), its sym.
#[derive(Copy, Clone)]
#[repr(C)]
struct SymRow {
    name: [u8; HC_SYMBOL_MAX],
    sym: SymbolId,
    len: u8,
    _pad: [u8; 3],
}

impl SymRow {
    const EMPTY: Self = Self {
        name: [0; HC_SYMBOL_MAX],
        sym: 0,
        len: 0,
        _pad: [0; 3],
    };

    #[inline]
    fn name(&self) -> &[u8] {
        &self.name[..self.len as usize]
    }
}

/// `INSTRUMENT → (row, SymbolId)` for the whole Hypercall universe: an
/// open-addressing FNV-1a table over dense rows. Boot-built (the two
/// boxed arrays are the only allocations); every lookup is one hash, a
/// short linear probe and an in-place compare. `Clone` (boot-time) so
/// the REST poller thread holds its own copy.
#[derive(Clone)]
pub struct HcSymbolTable {
    rows: Box<[SymRow]>,
    /// `row + 1`, 0 = empty.
    slots: Box<[u16]>,
    len: usize,
}

impl HcSymbolTable {
    /// Empty table (boot-time allocation).
    #[must_use]
    pub fn new() -> Self {
        Self {
            rows: vec![SymRow::EMPTY; HC_MAX_INSTRUMENTS].into_boxed_slice(),
            slots: vec![0u16; HC_HASH_SLOTS].into_boxed_slice(),
            len: 0,
        }
    }

    #[inline]
    fn home(name: &[u8]) -> usize {
        (fnv1a_64(name) as usize) & (HC_HASH_SLOTS - 1)
    }

    /// Register `name → sym` (boot-time). Returns the row index — rows
    /// are dense and in insertion order, so they index per-row state.
    pub fn insert(&mut self, name: &[u8], sym: SymbolId) -> Result<usize, SymbolTableErr> {
        if name.is_empty() {
            return Err(SymbolTableErr::Empty);
        }
        if name.len() > HC_SYMBOL_MAX {
            return Err(SymbolTableErr::TooLong);
        }
        if self.lookup(name).is_some() {
            return Err(SymbolTableErr::Duplicate);
        }
        if self.len >= HC_MAX_INSTRUMENTS {
            return Err(SymbolTableErr::Full);
        }
        let row = self.len;
        let r = &mut self.rows[row];
        // COPY: instrument name ≤ 32 B, once per instrument at boot — the
        // row owns it inline so the hot compare needs no pointer chase —
        // rejected: borrowing the discovery body (a 4 MB boot buffer the
        // ingress thread must not pin).
        r.name[..name.len()].copy_from_slice(name);
        r.len = name.len() as u8;
        r.sym = sym;
        let mut s = Self::home(name);
        while self.slots[s] != 0 {
            s = (s + 1) & (HC_HASH_SLOTS - 1);
        }
        self.slots[s] = (row + 1) as u16;
        self.len += 1;
        Ok(row)
    }

    /// Resolve an instrument name to `(row, sym)`. Hot path.
    #[inline]
    #[must_use]
    pub fn lookup(&self, name: &[u8]) -> Option<(usize, SymbolId)> {
        if name.is_empty() || name.len() > HC_SYMBOL_MAX {
            return None;
        }
        let mut s = Self::home(name);
        loop {
            let v = *self.slots.get(s)?;
            if v == 0 {
                return None;
            }
            let row = (v - 1) as usize;
            let r = self.rows.get(row)?;
            if r.len as usize == name.len() && r.name() == name {
                return Some((row, r.sym));
            }
            s = (s + 1) & (HC_HASH_SLOTS - 1);
        }
    }

    /// Row accessor: `(name, sym)`.
    #[inline]
    #[must_use]
    pub fn get(&self, row: usize) -> Option<(&[u8], SymbolId)> {
        if row >= self.len {
            return None;
        }
        let r = self.rows.get(row)?;
        Some((r.name(), r.sym))
    }

    /// Configured instrument count.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when no instrument is configured.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for HcSymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

/// `UNDERLYING → index sym` (`hypercall-idx:<U>`): ≤ 16 rows, linear.
#[derive(Copy, Clone)]
pub struct HcUnderlyings {
    rows: [([u8; HC_UNDERLYING_MAX], u8, SymbolId); HC_MAX_UNDERLYINGS],
    len: usize,
}

impl HcUnderlyings {
    /// Empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rows: [([0; HC_UNDERLYING_MAX], 0, 0); HC_MAX_UNDERLYINGS],
            len: 0,
        }
    }

    /// Register `underlying → index sym` (boot-time). Returns the row.
    pub fn insert(&mut self, name: &[u8], sym: SymbolId) -> Result<usize, SymbolTableErr> {
        if name.is_empty() {
            return Err(SymbolTableErr::Empty);
        }
        if name.len() > HC_UNDERLYING_MAX {
            return Err(SymbolTableErr::TooLong);
        }
        if self.lookup(name).is_some() {
            return Err(SymbolTableErr::Duplicate);
        }
        let Some(r) = self.rows.get_mut(self.len) else {
            return Err(SymbolTableErr::Full);
        };
        // COPY: underlying name ≤ 12 B, once per underlying at boot — the
        // row owns it inline — rejected: borrowing the boot config.
        r.0[..name.len()].copy_from_slice(name);
        r.1 = name.len() as u8;
        r.2 = sym;
        self.len += 1;
        Ok(self.len - 1)
    }

    /// Resolve an underlying name to `(row, index sym)`.
    #[inline]
    #[must_use]
    pub fn lookup(&self, name: &[u8]) -> Option<(usize, SymbolId)> {
        let mut i = 0;
        while i < self.len {
            let r = &self.rows[i];
            if r.1 as usize == name.len() && &r.0[..name.len()] == name {
                return Some((i, r.2));
            }
            i += 1;
        }
        None
    }

    /// Row accessor: `(name, index sym)`.
    #[inline]
    #[must_use]
    pub fn get(&self, row: usize) -> Option<(&[u8], SymbolId)> {
        if row >= self.len {
            return None;
        }
        let r = &self.rows[row];
        Some((&r.0[..r.1 as usize], r.2))
    }

    /// Configured underlying count.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True when no underlying is configured.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for HcUnderlyings {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// JSON primitives (in place, no DOM)
// ---------------------------------------------------------------

/// A span `[start, end)` of the payload.
pub type Span = (u32, u32);

/// Walk the members of the JSON object whose `{` is at (or after
/// whitespace from) `pos`, calling `visit(key, value_pos)` for each;
/// the visitor returns the position after the value it consumed
/// (`skip_json_value` for keys it does not want) or `None` to reject.
/// Returns the position after the closing `}`. One forward pass:
/// member order is irrelevant, nested values are skipped, keys are
/// compared in place.
#[inline]
pub fn walk_object<F: FnMut(&[u8], usize) -> Option<usize>>(
    buf: &[u8],
    pos: usize,
    mut visit: F,
) -> Option<usize> {
    let mut i = skip_ws(buf, pos);
    if *buf.get(i)? != b'{' {
        return None;
    }
    i = skip_ws(buf, i + 1);
    if *buf.get(i)? == b'}' {
        return Some(i + 1);
    }
    loop {
        if *buf.get(i)? != b'"' {
            return None;
        }
        let key_start = i + 1;
        let after_key = skip_string(buf, key_start)?;
        let key = buf.get(key_start..after_key - 1)?;
        i = skip_ws(buf, after_key);
        if *buf.get(i)? != b':' {
            return None;
        }
        i = visit(key, skip_ws(buf, i + 1))?;
        i = skip_ws(buf, i);
        match *buf.get(i)? {
            b',' => i = skip_ws(buf, i + 1),
            b'}' => return Some(i + 1),
            _ => return None,
        }
    }
}

/// Walk the elements of the JSON array whose `[` is at `pos`, calling
/// `visit(elem_pos)` for each (it returns the position after the
/// element). Returns the position after the closing `]`.
#[inline]
pub fn walk_array<F: FnMut(usize) -> Option<usize>>(
    buf: &[u8],
    pos: usize,
    mut visit: F,
) -> Option<usize> {
    let mut i = skip_ws(buf, pos);
    if *buf.get(i)? != b'[' {
        return None;
    }
    i = skip_ws(buf, i + 1);
    if *buf.get(i)? == b']' {
        return Some(i + 1);
    }
    loop {
        i = skip_ws(buf, visit(i)?);
        match *buf.get(i)? {
            b',' => i = skip_ws(buf, i + 1),
            b']' => return Some(i + 1),
            _ => return None,
        }
    }
}

/// A JSON string value at `pos`: `(span of its body, position after the
/// closing quote)`.
#[inline]
fn string_span(buf: &[u8], pos: usize) -> Option<(Span, usize)> {
    if *buf.get(pos)? != b'"' {
        return None;
    }
    let end = skip_string(buf, pos + 1)?;
    Some(((pos as u32 + 1, end as u32 - 1), end))
}

/// A decimal STRING (`"12.5"`) or bare number at `pos` → ×1e6; `null`
/// → `None` value. Returns `(value, position after it)`.
#[inline]
fn opt_decimal_1e6(buf: &[u8], pos: usize) -> Option<(Option<i64>, usize)> {
    match *buf.get(pos)? {
        b'n' => {
            if buf.get(pos..pos + 4)? == b"null" {
                Some((None, pos + 4))
            } else {
                None
            }
        }
        b'"' => {
            let (v, end) = scan_price_1e6(buf, pos + 1)?;
            if *buf.get(end)? != b'"' {
                return None;
            }
            Some((Some(v), end + 1))
        }
        _ => {
            let (v, end) = scan_price_1e6(buf, pos)?;
            Some((Some(v), end))
        }
    }
}

/// A bare unsigned integer at `pos`.
#[inline]
fn uint(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    scan_u64(buf, pos)
}

/// The payload bytes of a span.
#[inline]
#[must_use]
pub fn span_bytes(buf: &[u8], s: Span) -> &[u8] {
    buf.get(s.0 as usize..s.1 as usize).unwrap_or(&[])
}

// ---------------------------------------------------------------
// Classification
// ---------------------------------------------------------------

/// What a WS text message is, by its `"type"`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HcMsg {
    /// `IndicativeMarketData` — one instrument's provider quote.
    Indicative,
    /// `IndexPriceUpdate` — every underlying's index.
    IndexPrice,
    /// `Trade` — a public print.
    Trade,
    /// `MarketUpdate` — listing / expiry / deletion.
    MarketUpdate,
    /// `Subscribed` — a channel ack.
    Subscribed,
    /// `ClockSynced` — the answer to our `ClockSync`.
    ClockSynced,
    /// `Error` — a venue refusal notice.
    Error,
    /// Any other type (the venue has 32 message kinds; the public
    /// channels we subscribe use the ones above).
    Other,
    /// No `"type"` string at all — malformed.
    Malformed,
}

/// Classify one text message by its `"type"` value. The captured frames
/// all START with `{"type":"`; that prefix is read in place, and a
/// message whose `type` is elsewhere falls back to one object walk.
#[inline]
#[must_use]
pub fn classify(payload: &[u8]) -> HcMsg {
    const PREFIX: &[u8] = b"{\"type\":\"";
    let ty: &[u8] = if payload.starts_with(PREFIX) {
        let rest = &payload[PREFIX.len()..];
        match memchr::memchr(b'"', rest) {
            Some(e) => &rest[..e],
            None => return HcMsg::Malformed,
        }
    } else {
        let mut span: Option<Span> = None;
        let walked = walk_object(payload, 0, |k, v| {
            if k == b"type" {
                let (s, end) = string_span(payload, v)?;
                span = Some(s);
                Some(end)
            } else {
                skip_json_value(payload, v)
            }
        });
        match (walked, span) {
            (Some(_), Some(s)) => span_bytes(payload, s),
            _ => return HcMsg::Malformed,
        }
    };
    match ty {
        b"IndicativeMarketData" => HcMsg::Indicative,
        b"IndexPriceUpdate" => HcMsg::IndexPrice,
        b"Trade" => HcMsg::Trade,
        b"MarketUpdate" => HcMsg::MarketUpdate,
        b"Subscribed" => HcMsg::Subscribed,
        b"ClockSynced" => HcMsg::ClockSynced,
        b"Error" => HcMsg::Error,
        _ => HcMsg::Other,
    }
}

// ---------------------------------------------------------------
// IndicativeMarketData
// ---------------------------------------------------------------

/// `HcQuote::sides` bit: `best_bid` present.
pub const SIDE_BID: u8 = 1;
/// `HcQuote::sides` bit: `best_ask` present.
pub const SIDE_ASK: u8 = 2;

/// One parsed `IndicativeMarketData`. One cache line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct HcQuote {
    /// Best bid ×1e6 (0 when absent).
    pub bid_px_1e6: i64,
    /// Indicative bid size ×1e6 contracts (0 when absent).
    pub bid_qty_1e6: i64,
    /// Best ask ×1e6 (0 when absent).
    pub ask_px_1e6: i64,
    /// Indicative ask size ×1e6 contracts (0 when absent).
    pub ask_qty_1e6: i64,
    /// API clock at publish (ms).
    pub published_at_ms: u64,
    /// The quote's own stamp (ms) — the tick's venue time.
    pub timestamp_ms: u64,
    /// `instrument` span.
    pub instrument: Span,
    /// `rfq_provider_quotes` array span (`(0, 0)` when absent/null).
    pub providers: Span,
}

impl HcQuote {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        bid_px_1e6: 0,
        bid_qty_1e6: 0,
        ask_px_1e6: 0,
        ask_qty_1e6: 0,
        published_at_ms: 0,
        timestamp_ms: 0,
        instrument: (0, 0),
        providers: (0, 0),
    };
}

const _: () = assert!(core::mem::size_of::<HcQuote>() == 64);

/// The quote's non-price facts, returned beside the frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct HcQuoteMeta {
    /// [`SIDE_BID`] | [`SIDE_ASK`].
    pub sides: u8,
    /// `num_providers`.
    pub num_providers: u8,
}

/// Parse one `IndicativeMarketData` into `out`, IN PLACE. Required:
/// `instrument`, `published_at`, `timestamp`, `num_providers`;
/// `best_*` / sizes may be `null` or absent (one-sided and empty quotes
/// are legal). `None` = malformed (the frame is counted and dropped)
/// and `out` is left UNTOUCHED — the in-place contract the fuzz targets
/// check from a poisoned start: the walk fills locals and `out` is
/// written once, on success.
#[must_use]
pub fn parse_indicative(payload: &[u8], out: &mut HcQuote) -> Option<HcQuoteMeta> {
    let mut q = HcQuote::ZERO;
    let mut meta = HcQuoteMeta::default();
    let mut seen = 0u8;
    walk_object(payload, 0, |k, v| match k {
        b"instrument" => {
            let (s, end) = string_span(payload, v)?;
            q.instrument = s;
            seen |= 1;
            Some(end)
        }
        b"best_bid" => {
            let (x, end) = opt_decimal_1e6(payload, v)?;
            if let Some(px) = x {
                q.bid_px_1e6 = px;
                meta.sides |= SIDE_BID;
            }
            Some(end)
        }
        b"best_ask" => {
            let (x, end) = opt_decimal_1e6(payload, v)?;
            if let Some(px) = x {
                q.ask_px_1e6 = px;
                meta.sides |= SIDE_ASK;
            }
            Some(end)
        }
        b"indicative_bid_size" => {
            let (x, end) = opt_decimal_1e6(payload, v)?;
            q.bid_qty_1e6 = x.unwrap_or(0);
            Some(end)
        }
        b"indicative_ask_size" => {
            let (x, end) = opt_decimal_1e6(payload, v)?;
            q.ask_qty_1e6 = x.unwrap_or(0);
            Some(end)
        }
        b"num_providers" => {
            let (n, end) = uint(payload, v)?;
            meta.num_providers = n.min(u8::MAX as u64) as u8;
            seen |= 2;
            Some(end)
        }
        b"published_at" => {
            let (t, end) = uint(payload, v)?;
            q.published_at_ms = t;
            seen |= 4;
            Some(end)
        }
        b"timestamp" => {
            let (t, end) = uint(payload, v)?;
            q.timestamp_ms = t;
            seen |= 8;
            Some(end)
        }
        b"rfq_provider_quotes" => {
            let end = skip_json_value(payload, v)?;
            if payload.get(v) == Some(&b'[') {
                q.providers = (v as u32, end as u32);
            }
            Some(end)
        }
        _ => skip_json_value(payload, v),
    })?;
    if seen != 0b1111 || q.instrument.1 <= q.instrument.0 {
        return None;
    }
    // A side is only a quote with its size: a missing side zeroes it.
    if meta.sides & SIDE_BID == 0 {
        q.bid_qty_1e6 = 0;
    }
    if meta.sides & SIDE_ASK == 0 {
        q.ask_qty_1e6 = 0;
    }
    *out = q;
    Some(meta)
}

/// One provider's indicative quote (a `rfq_provider_quotes` entry).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct HcProvider {
    /// Bid ×1e6.
    pub bid_px_1e6: i64,
    /// Ask ×1e6.
    pub ask_px_1e6: i64,
    /// Max bid size ×1e6.
    pub bid_qty_1e6: i64,
    /// Max ask size ×1e6.
    pub ask_qty_1e6: i64,
    /// The provider's `updated_at` (ms).
    pub updated_at_ms: u64,
    /// The wallet's LOW 32 bits (its last 8 hex digits).
    pub wallet_lo32: u32,
}

/// The low 32 bits of a `0x…` hex address (its last 8 hex digits).
#[inline]
fn wallet_lo32(hex: &[u8]) -> Option<u32> {
    if hex.len() < 10 || !hex.starts_with(b"0x") {
        return None;
    }
    let tail = &hex[hex.len() - 8..];
    let mut v = 0u32;
    let mut i = 0;
    while i < tail.len() {
        let d = match tail[i] {
            b @ b'0'..=b'9' => b - b'0',
            b @ b'a'..=b'f' => b - b'a' + 10,
            b @ b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u32;
        i += 1;
    }
    Some(v)
}

/// Walk a quote's `rfq_provider_quotes` array (the span
/// [`parse_indicative`] recorded) into `out`. Returns
/// `(entries read, entries present)` — past [`HC_MAX_PROVIDERS`] the
/// rest are counted, not read. `None` = malformed, and then `out`'s
/// contents are unspecified (entries before the malformed one may have
/// been written; the caller reads none of them).
#[must_use]
pub fn walk_providers(
    payload: &[u8],
    providers: Span,
    out: &mut [HcProvider; HC_MAX_PROVIDERS],
) -> Option<(u8, u8)> {
    if providers.1 <= providers.0 {
        return Some((0, 0));
    }
    let mut read = 0usize;
    let mut present = 0usize;
    walk_array(payload, providers.0 as usize, |e| {
        let mut p = HcProvider::default();
        let mut seen = 0u8;
        let end = walk_object(payload, e, |k, v| match k {
            b"wallet" => {
                let (s, end) = string_span(payload, v)?;
                p.wallet_lo32 = wallet_lo32(span_bytes(payload, s))?;
                seen |= 1;
                Some(end)
            }
            b"bid_price" => {
                let (x, end) = opt_decimal_1e6(payload, v)?;
                p.bid_px_1e6 = x.unwrap_or(0);
                Some(end)
            }
            b"ask_price" => {
                let (x, end) = opt_decimal_1e6(payload, v)?;
                p.ask_px_1e6 = x.unwrap_or(0);
                Some(end)
            }
            b"max_bid_size" => {
                let (x, end) = opt_decimal_1e6(payload, v)?;
                p.bid_qty_1e6 = x.unwrap_or(0);
                Some(end)
            }
            b"max_ask_size" => {
                let (x, end) = opt_decimal_1e6(payload, v)?;
                p.ask_qty_1e6 = x.unwrap_or(0);
                Some(end)
            }
            b"updated_at" => {
                let (t, end) = uint(payload, v)?;
                p.updated_at_ms = t;
                seen |= 2;
                Some(end)
            }
            _ => skip_json_value(payload, v),
        })?;
        if seen != 0b11 {
            return None;
        }
        if read < HC_MAX_PROVIDERS {
            out[read] = p;
            read += 1;
        }
        present += 1;
        Some(end)
    })?;
    Some((read as u8, present.min(u8::MAX as usize) as u8))
}

/// Pack a [`core_types::ChannelId::ProviderQuote`] event's `venue_seq`.
#[inline]
#[must_use]
pub const fn provider_quote_seq(index: u8, ask: bool, num_providers: u8, wallet_lo32: u32) -> u64 {
    (index as u64) | ((ask as u64) << 8) | ((num_providers as u64) << 16) | ((wallet_lo32 as u64) << 32)
}

// ---------------------------------------------------------------
// IndexPriceUpdate
// ---------------------------------------------------------------

/// One `prices[]` entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct HcIndexEntry {
    /// Index ×1e6 (USD).
    pub price_1e6: i64,
    /// The entry's source observation time (ms).
    pub ts_ms: u64,
    /// `underlying` span.
    pub underlying: Span,
}

/// Parse an `IndexPriceUpdate` in place: every `prices[]` entry into
/// `out` (past [`HC_MAX_UNDERLYINGS`] entries are counted, not read).
/// Returns `(entries read, entries present, the frame's own timestamp)`;
/// on `None` `out`'s contents are unspecified (the caller reads none).
#[must_use]
pub fn parse_index_update(
    payload: &[u8],
    out: &mut [HcIndexEntry; HC_MAX_UNDERLYINGS],
) -> Option<(u8, u8, u64)> {
    let mut read = 0usize;
    let mut present = 0usize;
    let mut frame_ts = 0u64;
    let mut have_prices = false;
    walk_object(payload, 0, |k, v| match k {
        b"prices" => {
            have_prices = true;
            walk_array(payload, v, |e| {
                let mut x = HcIndexEntry::default();
                let mut seen = 0u8;
                let end = walk_object(payload, e, |k2, v2| match k2 {
                    b"underlying" => {
                        let (s, end) = string_span(payload, v2)?;
                        x.underlying = s;
                        seen |= 1;
                        Some(end)
                    }
                    b"price" => {
                        let (p, end) = opt_decimal_1e6(payload, v2)?;
                        x.price_1e6 = p?;
                        seen |= 2;
                        Some(end)
                    }
                    b"timestamp" => {
                        let (t, end) = uint(payload, v2)?;
                        x.ts_ms = t;
                        seen |= 4;
                        Some(end)
                    }
                    _ => skip_json_value(payload, v2),
                })?;
                if seen != 0b111 {
                    return None;
                }
                if read < HC_MAX_UNDERLYINGS {
                    out[read] = x;
                    read += 1;
                }
                present += 1;
                Some(end)
            })
        }
        b"timestamp" => {
            let (t, end) = uint(payload, v)?;
            frame_ts = t;
            Some(end)
        }
        _ => skip_json_value(payload, v),
    })?;
    if !have_prices {
        return None;
    }
    Some((read as u8, present.min(u8::MAX as usize) as u8, frame_ts))
}

// ---------------------------------------------------------------
// Trade / MarketUpdate / ClockSynced / Error
// ---------------------------------------------------------------

/// One public print.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct HcTrade {
    /// Price ×1e6.
    pub px_1e6: i64,
    /// Size ×1e6, NEGATED when the aggressor sold (the §6.5 law).
    pub signed_qty_1e6: i64,
    /// Venue stamp (ms).
    pub ts_ms: u64,
    /// `symbol` span.
    pub symbol: Span,
}

/// Parse a `Trade`. `side` is `buy` / `sell`; anything else is
/// malformed (a print whose direction is unknown must not be signed).
#[must_use]
pub fn parse_trade(payload: &[u8]) -> Option<HcTrade> {
    let mut t = HcTrade::default();
    let mut qty = 0i64;
    let mut sell: Option<bool> = None;
    let mut seen = 0u8;
    walk_object(payload, 0, |k, v| match k {
        b"symbol" => {
            let (s, end) = string_span(payload, v)?;
            t.symbol = s;
            seen |= 1;
            Some(end)
        }
        b"price" => {
            let (p, end) = opt_decimal_1e6(payload, v)?;
            t.px_1e6 = p?;
            seen |= 2;
            Some(end)
        }
        b"size" => {
            let (q, end) = opt_decimal_1e6(payload, v)?;
            qty = q?;
            seen |= 4;
            Some(end)
        }
        b"side" => {
            let (s, end) = string_span(payload, v)?;
            sell = match span_bytes(payload, s) {
                b"buy" | b"Buy" | b"BUY" => Some(false),
                b"sell" | b"Sell" | b"SELL" => Some(true),
                _ => None,
            };
            seen |= 8;
            Some(end)
        }
        b"timestamp" => {
            let (ts, end) = uint(payload, v)?;
            t.ts_ms = ts;
            seen |= 16;
            Some(end)
        }
        _ => skip_json_value(payload, v),
    })?;
    let sell = sell?;
    if seen != 0b11111 {
        return None;
    }
    t.signed_qty_1e6 = if sell { -qty } else { qty };
    Some(t)
}

/// A `MarketUpdate` action.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HcListingAction {
    /// A new instrument listed.
    Created,
    /// An instrument expired (it settles next).
    Expired,
    /// An instrument was removed.
    Deleted,
    /// A combo was created (no instrument of ours).
    ComboCreated,
    /// Anything else.
    Other,
}

/// Parse a `MarketUpdate`: `(action, symbol span, venue ms)`.
#[must_use]
pub fn parse_market_update(payload: &[u8]) -> Option<(HcListingAction, Span, u64)> {
    let mut action = None;
    let mut symbol: Span = (0, 0);
    let mut ts = 0u64;
    walk_object(payload, 0, |k, v| match k {
        b"action" => {
            let (s, end) = string_span(payload, v)?;
            action = Some(match span_bytes(payload, s) {
                b"Created" => HcListingAction::Created,
                b"Expired" => HcListingAction::Expired,
                b"Deleted" => HcListingAction::Deleted,
                b"ComboCreated" => HcListingAction::ComboCreated,
                _ => HcListingAction::Other,
            });
            Some(end)
        }
        b"symbol" => {
            let (s, end) = string_span(payload, v)?;
            symbol = s;
            Some(end)
        }
        b"timestamp" => {
            let (t, end) = uint(payload, v)?;
            ts = t;
            Some(end)
        }
        _ => skip_json_value(payload, v),
    })?;
    Some((action?, symbol, ts))
}

/// Parse a `ClockSynced`: `(our decimal nonce, server_at ms)`. A nonce
/// that is not a decimal integer is not ours — `None`.
#[must_use]
pub fn parse_clock_synced(payload: &[u8]) -> Option<(u64, u64)> {
    let mut nonce = None;
    let mut server_at = None;
    walk_object(payload, 0, |k, v| match k {
        b"nonce" => {
            let (s, end) = string_span(payload, v)?;
            let b = span_bytes(payload, s);
            let (n, e) = scan_u64(b, 0)?;
            if e == b.len() {
                nonce = Some(n);
            }
            Some(end)
        }
        b"server_at" => {
            let (t, end) = uint(payload, v)?;
            server_at = Some(t);
            Some(end)
        }
        _ => skip_json_value(payload, v),
    })?;
    Some((nonce?, server_at?))
}

/// Parse an `Error`: the `message` span.
#[must_use]
pub fn parse_error(payload: &[u8]) -> Option<Span> {
    let mut msg = None;
    walk_object(payload, 0, |k, v| match k {
        b"message" => {
            let (s, end) = string_span(payload, v)?;
            msg = Some(s);
            Some(end)
        }
        _ => skip_json_value(payload, v),
    })?;
    msg
}

/// Parse a CLOSE frame's reason text (the bytes after the 2-byte code):
/// the slow-consumer law's `cause`. A reason that is not the venue's
/// JSON is [`HcCloseCause::Other`].
#[must_use]
pub fn parse_close_reason(reason: &[u8]) -> HcCloseCause {
    let mut error: Span = (0, 0);
    let mut cause: Span = (0, 0);
    let ok = walk_object(reason, 0, |k, v| match k {
        b"error" => {
            let (s, end) = string_span(reason, v)?;
            error = s;
            Some(end)
        }
        b"cause" => {
            let (s, end) = string_span(reason, v)?;
            cause = s;
            Some(end)
        }
        _ => skip_json_value(reason, v),
    })
    .is_some();
    if !ok || span_bytes(reason, error) != b"slow_consumer" {
        return HcCloseCause::Other;
    }
    match span_bytes(reason, cause) {
        b"message_limit" => HcCloseCause::MessageLimit,
        b"byte_limit" => HcCloseCause::ByteLimit,
        b"queue_age" | b"age_limit" => HcCloseCause::QueueAge,
        b"write_timeout" => HcCloseCause::WriteTimeout,
        _ => HcCloseCause::SlowOther,
    }
}

// ---------------------------------------------------------------
// Outbound frames (rendered into caller scratch)
// ---------------------------------------------------------------

/// Channel names (the `Subscribe.channel` values).
pub const CH_INDICATIVE: &[u8] = b"indicative_market_data";
/// `index_prices`.
pub const CH_INDEX: &[u8] = b"index_prices";
/// `trades`.
pub const CH_TRADES: &[u8] = b"trades";
/// `market_updates`.
pub const CH_MARKET_UPDATES: &[u8] = b"market_updates";

/// Parts of one outbound frame: one per universe row, the separators
/// between them (rows − 1), and the envelope — opening, channel, the
/// `symbols` opener and the closing (the WS serialiser copies each part
/// straight into the masked tx buffer — no render scratch).
pub const SUBSCRIBE_PARTS_MAX: usize = 2 * HC_MAX_INSTRUMENTS + 3;

/// The payload of one `Subscribe` as PARTS (their concatenation is the
/// JSON): `{"type":"Subscribe","channel":"<ch>"}`, or — with a table —
/// `…,"symbols":["A","B",…]}` naming every row: the ONE frame the
/// subscribe law allows per channel. Returns the part count; `None`
/// when `parts` is too short or the table is empty (a filter naming
/// nothing would subscribe to nothing, silently).
#[must_use]
pub fn subscribe_parts<'a>(
    channel: &'static [u8],
    table: Option<&'a HcSymbolTable>,
    parts: &mut [&'a [u8]],
) -> Option<usize> {
    let mut n = 0usize;
    let mut put = |x: &'a [u8], n: &mut usize| -> Option<()> {
        *parts.get_mut(*n)? = x;
        *n += 1;
        Some(())
    };
    put(b"{\"type\":\"Subscribe\",\"channel\":\"", &mut n)?;
    put(channel, &mut n)?;
    match table {
        None => put(b"\"}", &mut n)?,
        Some(t) => {
            if t.is_empty() {
                return None;
            }
            put(b"\",\"symbols\":[\"", &mut n)?;
            let mut i = 0;
            while let Some((name, _)) = t.get(i) {
                if i > 0 {
                    put(b"\",\"", &mut n)?;
                }
                put(name, &mut n)?;
                i += 1;
            }
            put(b"\"]}", &mut n)?;
        }
    }
    Some(n)
}

/// The payload of `{"type":"ClockSync","nonce":"<digits>"}` as parts;
/// `digits` is the nonce's decimal ASCII.
#[inline]
#[must_use]
pub const fn clock_sync_parts(digits: &[u8]) -> [&[u8]; 3] {
    [b"{\"type\":\"ClockSync\",\"nonce\":\"", digits, b"\"}"]
}

/// Decimal ASCII of `v` into the tail of `scratch`.
#[inline]
pub fn fmt_u64(mut v: u64, scratch: &mut [u8; 20]) -> &[u8] {
    let mut i = scratch.len();
    loop {
        i -= 1;
        scratch[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &scratch[i..]
}

// ---------------------------------------------------------------
// Tests (golden frames: tests/fixtures, captured live 2026-09-25)
// ---------------------------------------------------------------

#[cfg(test)]
mod tests;
