// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # Polymarket Gamma boot-time REST discovery (Phase 8e, plan §6.1)
//!
//! Parses `GET /markets?clob_token_ids=<decimal-token-id>` bodies from
//! `gamma-api.polymarket.com` into a boot-only market table. This is
//! the venue-wide Gamma/CLOB REST discovery promised by the 8a D1 note:
//! it validates the operator's `--polymarket-asset-id` against the
//! venue (an unknown id returns `[]` → the §6.1 coverage audit
//! fail-fasts under `--live`), captures the market metadata that 8j
//! execution sizing will consume (`conditionId`, trading flags, tick /
//! min-size), and resolves the **sibling** token — the No side of a
//! Yes token — for the multi-asset market subscribe.
//!
//! ## Allocation note (doctrine)
//!
//! This module runs **at boot only**, where allocation is allowed. Row
//! storage is a `Vec` reserved once at [`PmDiscovery::new`] and capped
//! at [`PM_DISCOVERY_ROWS_CAP`] (fail-fast beyond — the query form
//! returns only the markets containing the requested token ids, so
//! rows stay single-digit in practice). The table is dropped before
//! the engine loop starts; nothing here is reachable from a hot path.
//!
//! ## Wire shape (live-probed 2026-08-14)
//!
//! The body is a **bare JSON array** of market objects — no envelope:
//!
//! ```json
//! [{"id":"559651","question":"Xi Jinping out before 2027?",
//!   "conditionId":"0xa467...743b7","active":true,"closed":false,
//!   "acceptingOrders":true,"enableOrderBook":true,"negRisk":false,
//!   "orderPriceMinTickSize":0.001,"orderMinSize":5,
//!   "clobTokenIds":"[\"32338...93401\", \"25659...31962\"]",...}]
//! ```
//!
//! Objects carry ≈ 90 keys; we capture a handful and skip the rest
//! structurally ([`core_parse::skip_json_value`]) — including nested
//! arrays/objects (`"events":[{...}]`, `"clobRewards":[{...}]`),
//! `null`s, and strings with escapes (`"question"` is *not* captured
//! for exactly that reason). Field order is not assumed.
//!
//! ### The double-encoded `clobTokenIds`
//!
//! `clobTokenIds` is a JSON **string** whose content is itself a JSON
//! array of decimal strings — on the wire the inner quotes arrive
//! backslash-escaped. We never unescape: the raw in-quote span (found
//! with the escape-aware [`core_parse::skip_string`]) is scanned for
//! maximal ASCII-digit runs of ≥ [`PM_TOKEN_RUN_MIN`] digits — digits
//! are never escaped in JSON, so the runs *are* the token ids, in wire
//! order. Real ids render a uint256 to 77–78 digits. The other
//! double-encoded fields (`outcomes`, `outcomePrices`) are plain
//! strings to the structural skipper.
//!
//! ### Units
//!
//! `order_price_min_tick_1e9` uses the ×1e9 metadata scale
//! (`0.001` → `1_000_000`, matching OKX `tick_sz_1e9`);
//! `order_min_size_1e6` uses the trading `Qty` ×1e6 scale
//! (`5` → `5_000_000`). Some market states omit `negRisk` /
//! `orderPriceMinTickSize` / `orderMinSize` — they default to
//! `false` / `0` / `0`.

use core_parse::{scan_number_sci_1e9, skip_json_value, skip_string, skip_ws};

/// Longest CLOB token id we accept: a uint256 renders to ≤ 78 decimal
/// digits (observed 77–78 live) + margin. Same bound as
/// [`crate::PM_ASSET_ID_MAX`].
pub const PM_TOKEN_MAX: usize = 80;

/// Hard cap on parsed market rows across all ingested bodies. The
/// `clob_token_ids` query form returns only the markets containing the
/// requested ids (live 2026-08-14: 1 row per id); 64 is ample headroom
/// for boot-validating a full subscription set.
pub const PM_DISCOVERY_ROWS_CAP: usize = 64;

/// Shortest ASCII-digit run inside the `clobTokenIds` span accepted as
/// a token id. Shields against digits in the inner-JSON syntax or in
/// hypothetical `\uXXXX` escape fragments (4 digits) — a body carrying
/// only sub-threshold runs fails loudly as [`PmDiscoveryErr::BadRow`]
/// rather than mis-parsing. Real ids are 77–78 digits.
const PM_TOKEN_RUN_MIN: usize = 10;

/// Why discovery ingestion failed. All fatal at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmDiscoveryErr {
    /// Body is not a bare JSON array (no `[` after optional
    /// whitespace).
    Envelope,
    /// A market object violated the row contract (missing required
    /// key, no parseable token id, > 2 token ids, over-long token,
    /// malformed `conditionId`, malformed value).
    BadRow,
    /// Body ended inside the array / an object / a string.
    Truncated,
    /// More than [`PM_DISCOVERY_ROWS_CAP`] rows across all bodies.
    TooMany,
}

/// One discovered market (one Gamma `markets` array element).
#[derive(Copy, Clone, Debug)]
pub struct PmMarketRow {
    /// Up to two CLOB token ids (Yes/No sides) as `(bytes, len)`
    /// pairs, ASCII decimal, in wire order. Use [`PmMarketRow::token`].
    pub tokens: [([u8; PM_TOKEN_MAX], u8); 2],
    /// How many entries of `tokens` are valid (1 or 2).
    pub n_tokens: u8,
    /// `conditionId` bytes (`condition_id_len` valid) — `0x` + hex,
    /// ≤ 66 bytes total.
    pub condition_id: [u8; 66],
    /// Valid prefix length of `condition_id`.
    pub condition_id_len: u8,
    /// `active` — market is live on the venue.
    pub active: bool,
    /// `closed` — market has resolved / ended.
    pub closed: bool,
    /// `acceptingOrders` — the CLOB accepts new orders right now.
    pub accepting_orders: bool,
    /// `enableOrderBook` — market is CLOB-tradable at all.
    pub enable_order_book: bool,
    /// `negRisk` — multi-outcome netting market. Optional on the
    /// wire; defaults `false`.
    pub neg_risk: bool,
    /// `orderPriceMinTickSize` ×1e9 (`0.001` → `1_000_000`) — the
    /// ×1e9 metadata scale. Optional (omitted in some market states);
    /// defaults `0`.
    pub order_price_min_tick_1e9: i64,
    /// `orderMinSize` ×1e6 (`5` → `5_000_000`) — the trading `Qty`
    /// ×1e6 scale. Optional (omitted in some market states);
    /// defaults `0`.
    pub order_min_size_1e6: i64,
}

impl PmMarketRow {
    /// The `i`-th token id as a byte slice; `None` if `i >= n_tokens`.
    #[inline]
    pub fn token(&self, i: usize) -> Option<&[u8]> {
        if i >= self.n_tokens as usize {
            return None;
        }
        let (buf, len) = &self.tokens[i];
        Some(&buf[..*len as usize])
    }
}

/// Boot-only Polymarket market table. See module docs.
pub struct PmDiscovery {
    rows: Vec<PmMarketRow>,
}

impl PmDiscovery {
    /// Empty table with capacity reserved once.
    pub fn new() -> Self {
        Self {
            rows: Vec::with_capacity(PM_DISCOVERY_ROWS_CAP),
        }
    }

    /// Parse one markets-endpoint body into the table. Returns the
    /// number of rows added — `[]` (unknown token id) is `Ok(0)`, and
    /// the caller decides whether that is fatal (§6.1: it is, under
    /// `--live`). Call once per fetched body; rows accumulate.
    pub fn ingest_body(&mut self, body: &[u8]) -> Result<u32, PmDiscoveryErr> {
        // Bare array — no envelope keys on this endpoint.
        let mut i = skip_ws(body, 0);
        if i >= body.len() || body[i] != b'[' {
            return Err(PmDiscoveryErr::Envelope);
        }
        i += 1;

        let mut added = 0u32;
        loop {
            i = skip_ws(body, i);
            if i >= body.len() {
                return Err(PmDiscoveryErr::Truncated);
            }
            match body[i] {
                b']' => break,
                b'{' => {
                    let (row, end) = parse_row(body, i)?;
                    if self.rows.len() >= PM_DISCOVERY_ROWS_CAP {
                        return Err(PmDiscoveryErr::TooMany);
                    }
                    self.rows.push(row);
                    added += 1;
                    i = skip_ws(body, end);
                    if i < body.len() && body[i] == b',' {
                        i += 1;
                    }
                }
                _ => return Err(PmDiscoveryErr::BadRow),
            }
        }
        Ok(added)
    }

    /// Look up the market containing `token` (matches either element
    /// of [`PmMarketRow::tokens`]).
    pub fn find_by_token(&self, token: &[u8]) -> Option<&PmMarketRow> {
        self.rows
            .iter()
            .find(|r| r.token(0) == Some(token) || r.token(1) == Some(token))
    }

    /// The **other** token of the market containing `token` — the No
    /// side of a Yes token and vice versa. `None` if `token` is
    /// unknown or its market carries a single token.
    pub fn sibling_of(&self, token: &[u8]) -> Option<&[u8]> {
        let row = self.find_by_token(token)?;
        if row.n_tokens < 2 {
            return None;
        }
        if row.token(0) == Some(token) {
            row.token(1)
        } else {
            row.token(0)
        }
    }

    /// Total rows parsed across all ingested bodies.
    #[inline]
    pub fn universe_total(&self) -> u32 {
        self.rows.len() as u32
    }
}

impl Default for PmDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse one market object starting at `pos` (must point at `{`).
/// Returns the row and the position after the closing `}`.
fn parse_row(body: &[u8], pos: usize) -> Result<(PmMarketRow, usize), PmDiscoveryErr> {
    debug_assert_eq!(body[pos], b'{');
    let mut i = pos + 1;

    let mut tokens: Option<([([u8; PM_TOKEN_MAX], u8); 2], u8)> = None;
    let mut condition_id = [0u8; 66];
    let mut condition_id_len = 0u8;
    let mut active: Option<bool> = None;
    let mut closed: Option<bool> = None;
    let mut accepting_orders: Option<bool> = None;
    let mut enable_order_book: Option<bool> = None;
    let mut neg_risk = false; // optional — absent on some market states
    let mut order_price_min_tick_1e9: i64 = 0; // optional
    let mut order_min_size_1e6: i64 = 0; // optional

    loop {
        i = skip_ws(body, i);
        if i >= body.len() {
            return Err(PmDiscoveryErr::Truncated);
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
                let key_end_q = skip_string(body, key_start).ok_or(PmDiscoveryErr::Truncated)?;
                let key = &body[key_start..key_end_q - 1];
                i = skip_ws(body, key_end_q);
                if i >= body.len() || body[i] != b':' {
                    return Err(PmDiscoveryErr::BadRow);
                }
                i = skip_ws(body, i + 1);
                match key {
                    b"clobTokenIds" => {
                        // Double-encoded: take the raw span WITH its
                        // escapes and extract the digit runs.
                        let (raw, end) = raw_quoted_span(body, i)?;
                        tokens = Some(extract_tokens(raw)?);
                        i = end;
                    }
                    b"conditionId" => {
                        let (s, end) = quoted_span(body, i)?;
                        if s.len() < 2 || s.len() > 66 || &s[..2] != b"0x" {
                            return Err(PmDiscoveryErr::BadRow);
                        }
                        condition_id[..s.len()].copy_from_slice(s);
                        condition_id_len = s.len() as u8;
                        i = end;
                    }
                    b"active" => {
                        let (v, end) = bare_bool(body, i)?;
                        active = Some(v);
                        i = end;
                    }
                    b"closed" => {
                        let (v, end) = bare_bool(body, i)?;
                        closed = Some(v);
                        i = end;
                    }
                    b"acceptingOrders" => {
                        let (v, end) = bare_bool(body, i)?;
                        accepting_orders = Some(v);
                        i = end;
                    }
                    b"enableOrderBook" => {
                        let (v, end) = bare_bool(body, i)?;
                        enable_order_book = Some(v);
                        i = end;
                    }
                    b"negRisk" => {
                        let (v, end) = bare_bool(body, i)?;
                        neg_risk = v;
                        i = end;
                    }
                    b"orderPriceMinTickSize" => {
                        // Bare number, ×1e9 metadata scale.
                        let (v, end) =
                            scan_number_sci_1e9(body, i).ok_or(PmDiscoveryErr::BadRow)?;
                        order_price_min_tick_1e9 = v;
                        i = end;
                    }
                    b"orderMinSize" => {
                        // Bare number, trading Qty ×1e6 scale.
                        let (v, end) = bare_1e6(body, i)?;
                        order_min_size_1e6 = v;
                        i = end;
                    }
                    _ => {
                        i = skip_json_value(body, i).ok_or(PmDiscoveryErr::BadRow)?;
                    }
                }
            }
            _ => return Err(PmDiscoveryErr::BadRow),
        }
    }

    let (tokens, n_tokens) = tokens.ok_or(PmDiscoveryErr::BadRow)?;
    if condition_id_len == 0 {
        return Err(PmDiscoveryErr::BadRow);
    }
    let row = PmMarketRow {
        tokens,
        n_tokens,
        condition_id,
        condition_id_len,
        active: active.ok_or(PmDiscoveryErr::BadRow)?,
        closed: closed.ok_or(PmDiscoveryErr::BadRow)?,
        accepting_orders: accepting_orders.ok_or(PmDiscoveryErr::BadRow)?,
        enable_order_book: enable_order_book.ok_or(PmDiscoveryErr::BadRow)?,
        neg_risk,
        order_price_min_tick_1e9,
        order_min_size_1e6,
    };
    Ok((row, i))
}

/// Extract token ids from the raw (still-escaped) in-quote span of
/// `clobTokenIds`: every maximal ASCII-digit run of ≥
/// [`PM_TOKEN_RUN_MIN`] digits is one id, in wire order. Zero runs, a
/// run over [`PM_TOKEN_MAX`], or more than 2 runs ⇒ `BadRow`.
fn extract_tokens(
    span: &[u8],
) -> Result<([([u8; PM_TOKEN_MAX], u8); 2], u8), PmDiscoveryErr> {
    let mut tokens = [([0u8; PM_TOKEN_MAX], 0u8); 2];
    let mut n: u8 = 0;
    let mut i = 0usize;
    while i < span.len() {
        if !span[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < span.len() && span[i].is_ascii_digit() {
            i += 1;
        }
        let run = &span[start..i];
        if run.len() < PM_TOKEN_RUN_MIN {
            continue;
        }
        if run.len() > PM_TOKEN_MAX || n == 2 {
            return Err(PmDiscoveryErr::BadRow);
        }
        tokens[n as usize].0[..run.len()].copy_from_slice(run);
        tokens[n as usize].1 = run.len() as u8;
        n += 1;
    }
    if n == 0 {
        return Err(PmDiscoveryErr::BadRow);
    }
    Ok((tokens, n))
}

/// Read a quoted string value at `pos` (must point at `"`). Returns
/// the raw in-quote span — escapes **kept** — and the position after
/// the closing quote. Only `clobTokenIds` consumes this directly (its
/// digit runs are escape-immune); everything else goes through
/// [`quoted_span`].
fn raw_quoted_span(body: &[u8], pos: usize) -> Result<(&[u8], usize), PmDiscoveryErr> {
    if pos >= body.len() || body[pos] != b'"' {
        return Err(PmDiscoveryErr::BadRow);
    }
    let start = pos + 1;
    let end_q = skip_string(body, start).ok_or(PmDiscoveryErr::Truncated)?;
    Ok((&body[start..end_q - 1], end_q))
}

/// [`raw_quoted_span`] for captured plain-string fields
/// (`conditionId`): those never contain escapes on this wire, so a
/// backslash inside the span is rejected rather than unescaped.
fn quoted_span(body: &[u8], pos: usize) -> Result<(&[u8], usize), PmDiscoveryErr> {
    let (span, end) = raw_quoted_span(body, pos)?;
    if span.contains(&b'\\') {
        return Err(PmDiscoveryErr::BadRow);
    }
    Ok((span, end))
}

/// Read a bare JSON boolean at `pos`. Anything but exactly `true` /
/// `false` (including `null` — a captured flag must be a real bool)
/// is `BadRow`.
fn bare_bool(body: &[u8], pos: usize) -> Result<(bool, usize), PmDiscoveryErr> {
    if body.len() >= pos + 4 && &body[pos..pos + 4] == b"true" {
        return Ok((true, pos + 4));
    }
    if body.len() >= pos + 5 && &body[pos..pos + 5] == b"false" {
        return Ok((false, pos + 5));
    }
    Err(PmDiscoveryErr::BadRow)
}

/// Read a bare number at `pos` scaled ×1e6: parse ×1e9 via
/// [`scan_number_sci_1e9`], then divide by 1000 (truncating toward
/// zero — precision below 1e-6 drops, matching the trading `Qty`
/// scale).
fn bare_1e6(body: &[u8], pos: usize) -> Result<(i64, usize), PmDiscoveryErr> {
    let (v_1e9, end) = scan_number_sci_1e9(body, pos).ok_or(PmDiscoveryErr::BadRow)?;
    Ok((v_1e9 / 1000, end))
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Yes-side token id from the live probe (77 digits).
    const TOK_YES: &[u8] =
        b"32338220190071351435772801779725302244575775216413325951443816017994629993401";
    /// No-side token id from the live probe (77 digits).
    const TOK_NO: &[u8] =
        b"25659310674993675562345759665114759892400026242514633218387667107987341231962";
    /// `conditionId` — `0x` + 64 hex = 66 bytes.
    const COND: &[u8] = b"0xa4676a8f3ab4bd7b384ff3b1d92eb27529df6e7d1b0b18db81cd6e4d7ed743b7";

    /// One realistic market object trimmed from the live probe
    /// (2026-08-14): bare-array body, double-encoded `clobTokenIds`
    /// with escaped inner quotes, nested `events`/`clobRewards` noise
    /// (the latter with a *nested* `conditionId` the walker must not
    /// capture), `null`s, an escapes-bearing `description`, bare
    /// bools/numbers (one negative), and `negRisk:false`.
    const MARKET_XI: &[u8] = br#"[
      {"id":"559651","question":"Xi Jinping out before 2027?","conditionId":"0xa4676a8f3ab4bd7b384ff3b1d92eb27529df6e7d1b0b18db81cd6e4d7ed743b7","slug":"xi-jinping-out-before-2027","endDate":"2026-12-31T12:00:00Z","liquidity":"48123.40551","description":"This market will resolve to \"Yes\" if Xi Jinping ceases to hold the office of General Secretary.","outcomes":"[\"Yes\", \"No\"]","outcomePrices":"[\"0.0455\", \"0.9545\"]","volumeNum":1226299.965282,"active":true,"closed":false,"new":false,"archived":false,"restricted":true,"groupItemThreshold":"0","questionID":"0xe3b1bc389210504ebcb9cffe4b0ed07ccd5e8967919b2a82a9c05e35a1e50076","umaEndDate":null,"enableOrderBook":true,"orderPriceMinTickSize":0.001,"orderMinSize":5,"umaResolutionStatuses":null,"acceptingOrders":true,"negRisk":false,"events":[{"id":"16092","ticker":"xi-jinping-out-before-2027","title":"Xi Jinping out before 2027?","active":true,"closed":false}],"clobRewards":[{"id":"12513","conditionId":"0x0000000000000000000000000000000000000000000000000000000000000000","assetAddress":"0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174","rewardsAmount":0,"rewardsDailyRate":10}],"clobTokenIds":"[\"32338220190071351435772801779725302244575775216413325951443816017994629993401\", \"25659310674993675562345759665114759892400026242514633218387667107987341231962\"]","spread":0.001,"oneDayPriceChange":-0.0005,"lastTradePrice":0.0455,"bestBid":0.045,"bestAsk":0.046}
    ]"#;

    /// Minimal valid single-token row (required keys only) — also
    /// exercises the optional-field defaults.
    const SINGLE: &[u8] = br#"[{"clobTokenIds":"[\"11111111112222222222\"]","conditionId":"0xab","active":true,"closed":true,"acceptingOrders":false,"enableOrderBook":true}]"#;

    #[test]
    fn ingest_parses_live_market_row() {
        let mut d = PmDiscovery::new();
        let added = d.ingest_body(MARKET_XI).expect("parse ok");
        assert_eq!(added, 1);
        assert_eq!(d.universe_total(), 1);

        // Both tokens extracted exactly, in wire order.
        let row = d.find_by_token(TOK_YES).expect("yes-side hit");
        assert_eq!(row.n_tokens, 2);
        assert_eq!(row.token(0).unwrap(), TOK_YES);
        assert_eq!(row.token(1).unwrap(), TOK_NO);
        assert!(row.token(2).is_none());

        // find_by_token hits on BOTH tokens (same market).
        let row_no = d.find_by_token(TOK_NO).expect("no-side hit");
        assert!(std::ptr::eq(row, row_no));

        // conditionId captured — 0x + 64 hex, the row's own, not the
        // nested clobRewards one.
        assert_eq!(row.condition_id_len as usize, COND.len());
        assert_eq!(&row.condition_id[..row.condition_id_len as usize], COND);

        // Flags.
        assert!(row.active);
        assert!(!row.closed);
        assert!(row.accepting_orders);
        assert!(row.enable_order_book);
        assert!(!row.neg_risk);

        // Units: 0.001 ×1e9 and 5 ×1e6.
        assert_eq!(row.order_price_min_tick_1e9, 1_000_000);
        assert_eq!(row.order_min_size_1e6, 5_000_000);
    }

    #[test]
    fn sibling_of_returns_other_token_both_ways() {
        let mut d = PmDiscovery::new();
        d.ingest_body(MARKET_XI).unwrap();
        assert_eq!(d.sibling_of(TOK_YES).unwrap(), TOK_NO);
        assert_eq!(d.sibling_of(TOK_NO).unwrap(), TOK_YES);
        assert!(d.sibling_of(b"999999999999").is_none()); // unknown token
    }

    #[test]
    fn single_token_market_has_no_sibling_and_defaults() {
        let mut d = PmDiscovery::new();
        assert_eq!(d.ingest_body(SINGLE).unwrap(), 1);
        let row = d.find_by_token(b"11111111112222222222").expect("hit");
        assert_eq!(row.n_tokens, 1);
        assert!(row.token(1).is_none());
        assert!(d.sibling_of(b"11111111112222222222").is_none());
        // Optional fields absent → documented defaults.
        assert!(!row.neg_risk);
        assert_eq!(row.order_price_min_tick_1e9, 0);
        assert_eq!(row.order_min_size_1e6, 0);
        // Required flags captured.
        assert!(row.closed);
        assert!(!row.accepting_orders);
    }

    #[test]
    fn ingest_accumulates_across_bodies() {
        let mut d = PmDiscovery::new();
        d.ingest_body(MARKET_XI).unwrap();
        d.ingest_body(SINGLE).unwrap();
        assert_eq!(d.universe_total(), 2);
        assert!(d.find_by_token(TOK_YES).is_some());
        assert!(d.find_by_token(b"11111111112222222222").is_some());
    }

    #[test]
    fn ingest_empty_array_is_ok_zero_and_find_misses() {
        // Unknown token id → venue answers `[]`; the §6.1 audit turns
        // that into a fail-fast, not this parser.
        let mut d = PmDiscovery::new();
        assert_eq!(d.ingest_body(b"[]").unwrap(), 0);
        assert_eq!(d.universe_total(), 0);
        assert!(d.find_by_token(TOK_YES).is_none());
        assert!(d.sibling_of(TOK_YES).is_none());
    }

    #[test]
    fn ingest_rejects_non_array_envelope() {
        let mut d = PmDiscovery::new();
        assert_eq!(
            d.ingest_body(br#"{"data":[]}"#).unwrap_err(),
            PmDiscoveryErr::Envelope
        );
        assert_eq!(d.ingest_body(b"").unwrap_err(), PmDiscoveryErr::Envelope);
        assert_eq!(d.ingest_body(b"null").unwrap_err(), PmDiscoveryErr::Envelope);
    }

    #[test]
    fn ingest_rejects_row_contract_violations() {
        let mut d = PmDiscovery::new();
        // Missing clobTokenIds.
        assert_eq!(
            d.ingest_body(br#"[{"conditionId":"0xab","active":true,"closed":false,"acceptingOrders":true,"enableOrderBook":true}]"#)
                .unwrap_err(),
            PmDiscoveryErr::BadRow
        );
        // clobTokenIds with no digit runs.
        assert_eq!(
            d.ingest_body(br#"[{"clobTokenIds":"[\"\"]","conditionId":"0xab","active":true,"closed":false,"acceptingOrders":true,"enableOrderBook":true}]"#)
                .unwrap_err(),
            PmDiscoveryErr::BadRow
        );
        // More than 2 tokens.
        assert_eq!(
            d.ingest_body(br#"[{"clobTokenIds":"[\"11111111110000000001\", \"11111111110000000002\", \"11111111110000000003\"]","conditionId":"0xab","active":true,"closed":false,"acceptingOrders":true,"enableOrderBook":true}]"#)
                .unwrap_err(),
            PmDiscoveryErr::BadRow
        );
        // Token longer than PM_TOKEN_MAX (81 digits).
        let long = format!(
            r#"[{{"clobTokenIds":"[\"{}\"]","conditionId":"0xab","active":true,"closed":false,"acceptingOrders":true,"enableOrderBook":true}}]"#,
            "1".repeat(PM_TOKEN_MAX + 1)
        );
        assert_eq!(
            d.ingest_body(long.as_bytes()).unwrap_err(),
            PmDiscoveryErr::BadRow
        );
        // conditionId without the 0x prefix.
        assert_eq!(
            d.ingest_body(br#"[{"clobTokenIds":"[\"11111111110000000001\"]","conditionId":"a467","active":true,"closed":false,"acceptingOrders":true,"enableOrderBook":true}]"#)
                .unwrap_err(),
            PmDiscoveryErr::BadRow
        );
        // conditionId over 66 bytes (0x + 65 hex).
        let over = format!(
            r#"[{{"clobTokenIds":"[\"11111111110000000001\"]","conditionId":"0x{}","active":true,"closed":false,"acceptingOrders":true,"enableOrderBook":true}}]"#,
            "a".repeat(65)
        );
        assert_eq!(
            d.ingest_body(over.as_bytes()).unwrap_err(),
            PmDiscoveryErr::BadRow
        );
        // Missing required flag (acceptingOrders).
        assert_eq!(
            d.ingest_body(br#"[{"clobTokenIds":"[\"11111111110000000001\"]","conditionId":"0xab","active":true,"closed":false,"enableOrderBook":true}]"#)
                .unwrap_err(),
            PmDiscoveryErr::BadRow
        );
    }

    #[test]
    fn ingest_rejects_truncated_bodies() {
        let mut d = PmDiscovery::new();
        // Mid-object, inside a quoted value (skipper runs off the end).
        assert_eq!(
            d.ingest_body(br#"[{"conditionId":"0xa467"#).unwrap_err(),
            PmDiscoveryErr::Truncated
        );
        // Mid-object, after a complete value.
        assert_eq!(
            d.ingest_body(br#"[{"active":true"#).unwrap_err(),
            PmDiscoveryErr::Truncated
        );
        // Array never closed.
        assert_eq!(d.ingest_body(b"[").unwrap_err(), PmDiscoveryErr::Truncated);
    }

    #[test]
    fn ingest_enforces_rows_cap() {
        let mut d = PmDiscovery::new();
        // Synthesize a body of tiny valid rows up to the cap.
        // Test-only allocation; boot code never builds bodies.
        const ROW: &str = r#"{"clobTokenIds":"[\"11111111110000000001\"]","conditionId":"0xab","active":true,"closed":false,"acceptingOrders":true,"enableOrderBook":true}"#;
        let mut body = String::with_capacity(1 << 14);
        body.push('[');
        for k in 0..PM_DISCOVERY_ROWS_CAP {
            if k > 0 {
                body.push(',');
            }
            body.push_str(ROW);
        }
        body.push(']');
        assert_eq!(
            d.ingest_body(body.as_bytes()).expect("under cap"),
            PM_DISCOVERY_ROWS_CAP as u32
        );
        assert_eq!(d.ingest_body(SINGLE).unwrap_err(), PmDiscoveryErr::TooMany);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The discovery parser never panics on arbitrary bytes and,
        /// on success, the added-row count matches the table.
        #[test]
        fn ingest_never_panics(input in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut d = PmDiscovery::new();
            match d.ingest_body(&input) {
                Ok(n) => {
                    prop_assert_eq!(n, d.universe_total());
                    prop_assert!(n as usize <= PM_DISCOVERY_ROWS_CAP);
                }
                Err(_) => {}
            }
        }
    }
}
