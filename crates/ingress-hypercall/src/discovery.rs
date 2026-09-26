// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Boot discovery (HC4): `GET /markets` — every listed instrument in one
//! call (≈ 4.3 MB, 927 ms from the Mac on 2026-09-25, `Content-Length`
//! framed) — scanned in ONE forward pass over the body, then the M2
//! capped-chain law (`options_select::select_capped_chain`) per
//! configured underlying: the nearest `E` expiries OUTSIDE the
//! provider's pre-expiry quoting blackout, the `K` strikes nearest the
//! index on each (position-based, calls and puts).
//!
//! Body shape: `{"success":true,"data":[{"underlying","expiry"(s),
//! "index_price"(string),"instruments":[{"id","strike"(string, may be
//! decimal),"expiry"(s),"option_type","status","trading_mode",…}]},…]}`
//! — one group per (underlying, expiry). A row is a candidate when it is
//! `ACTIVE`, its `trading_mode` includes `rfq`, and it expires later than
//! `now + blackout`.
//!
//! A configured underlying the venue does not list is a FATAL boot error
//! (the Deribit-combo precedent): a typo must never boot a silently
//! smaller universe.
//!
//! DOCTRINE: boot-only — allocation is fine here (`Vec` of candidate
//! rows); nothing from this module runs after the ingress thread starts.

use core_parse::{scan_price_1e9, scan_u64, skip_json_value, skip_string};
use options_select::{select_capped_chain, ChainRow};

use crate::{span_bytes, walk_array, walk_object, HC_MAX_UNDERLYINGS, HC_SYMBOL_MAX};

/// The discovery path.
pub const MARKETS_PATH: &str = "/markets";
/// Body cap (the body was 4.3 MB; room for the venue to grow).
pub const MARKETS_MAX_BODY: usize = 32 * 1024 * 1024;
/// Default pre-expiry blackout: the main provider stops quoting an
/// expiring series before its window (0 of 464 SP500 quoted at T−38 min,
/// 2026-09-25) — a series inside this is never selected.
pub const DEFAULT_BLACKOUT_MS: i64 = 2 * 3_600_000;

/// Why discovery refused to boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryErr {
    /// The body is not a `/markets` answer.
    Malformed,
    /// A configured underlying is not listed (its index in the config).
    NotListed(usize),
    /// More configured underlyings than [`HC_MAX_UNDERLYINGS`].
    TooManyUnderlyings,
}

impl core::fmt::Display for DiscoveryErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed => f.write_str("hypercall /markets: body is not a markets answer"),
            Self::NotListed(i) => write!(f, "hypercall /markets: configured underlying #{i} is not listed"),
            Self::TooManyUnderlyings => write!(
                f,
                "hypercall: more than {HC_MAX_UNDERLYINGS} underlyings configured"
            ),
        }
    }
}

impl std::error::Error for DiscoveryErr {}

/// One candidate instrument (`Copy` — the chain law returns rows by
/// value).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HcMarketRow {
    /// Instrument name bytes (`BTC-20261002-100000-C`).
    pub name: [u8; HC_SYMBOL_MAX],
    /// Name length.
    pub name_len: u8,
    /// Index of the underlying in the configured list.
    pub underlying: u8,
    /// Call (true) / put (false).
    pub call: bool,
    /// Expiry, unix ms.
    pub exp_ms: i64,
    /// Strike ×1e9 (decimal strikes are exact).
    pub strike_1e9: i64,
}

impl HcMarketRow {
    /// The instrument name.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

impl ChainRow for HcMarketRow {
    fn exp_ms(&self) -> i64 {
        self.exp_ms
    }
    fn strike_1e9(&self) -> i64 {
        self.strike_1e9
    }
    fn is_call(&self) -> bool {
        self.call
    }
}

/// Everything one `/markets` pass yields for the configured underlyings.
#[derive(Debug, Default)]
pub struct Markets {
    /// Candidate rows (every configured underlying, unselected).
    pub rows: Vec<HcMarketRow>,
    /// Index price ×1e9 per configured underlying (the ATM reference;
    /// the first listed group's — every group of one underlying carries
    /// the same index).
    pub index_1e9: [i64; HC_MAX_UNDERLYINGS],
    /// Rows the venue listed but the candidacy test refused (not
    /// `ACTIVE`, not RFQ-tradeable, or inside the blackout).
    pub refused: usize,
}

/// A string value's body span at `pos` and the position after it.
fn string_at(buf: &[u8], pos: usize) -> Option<((u32, u32), usize)> {
    if *buf.get(pos)? != b'"' {
        return None;
    }
    let end = skip_string(buf, pos + 1)?;
    Some(((pos as u32 + 1, end as u32 - 1), end))
}

/// A decimal string (`"2.5"`, `"340.3600000000000136…"`) → ×1e9.
fn decimal_1e9(buf: &[u8], pos: usize) -> Option<(i64, usize)> {
    let ((s, e), end) = string_at(buf, pos)?;
    let body = buf.get(s as usize..e as usize)?;
    let (v, used) = scan_price_1e9(body, 0)?;
    if used != body.len() {
        return None;
    }
    Some((v, end))
}

/// One forward pass over a `/markets` body, keeping the instruments of
/// `underlyings` that pass the candidacy test at `now_ms`.
pub fn parse_markets(
    body: &[u8],
    underlyings: &[&[u8]],
    now_ms: i64,
    blackout_ms: i64,
) -> Result<Markets, DiscoveryErr> {
    if underlyings.len() > HC_MAX_UNDERLYINGS {
        return Err(DiscoveryErr::TooManyUnderlyings);
    }
    let mut m = Markets::default();
    let mut listed = [false; HC_MAX_UNDERLYINGS];
    let mut have_data = false;
    let which = |name: &[u8]| underlyings.iter().position(|u| *u == name);
    walk_object(body, 0, |k, v| match k {
        b"data" => {
            have_data = true;
            walk_array(body, v, |g| {
                // One group: find its underlying first (the object walk
                // is order-independent, so the instruments are gathered
                // into a span and walked after).
                let mut und: Option<usize> = None;
                let mut index_1e9 = 0i64;
                let mut instruments: Option<(usize, usize)> = None;
                let end = walk_object(body, g, |gk, gv| match gk {
                    b"underlying" => {
                        let (s, end) = string_at(body, gv)?;
                        und = which(span_bytes(body, s));
                        Some(end)
                    }
                    b"index_price" => {
                        let (x, end) = decimal_1e9(body, gv)?;
                        index_1e9 = x;
                        Some(end)
                    }
                    b"instruments" => {
                        let end = skip_json_value(body, gv)?;
                        instruments = Some((gv, end));
                        Some(end)
                    }
                    _ => skip_json_value(body, gv),
                })?;
                let Some(u) = und else {
                    return Some(end);
                };
                if !listed[u] {
                    listed[u] = true;
                    m.index_1e9[u] = index_1e9;
                }
                if let Some((start, _)) = instruments {
                    walk_array(body, start, |e| {
                        let r = parse_instrument(body, e, u, now_ms, blackout_ms);
                        match r {
                            Some((Some(row), end)) => {
                                m.rows.push(row);
                                Some(end)
                            }
                            Some((None, end)) => {
                                m.refused += 1;
                                Some(end)
                            }
                            None => None,
                        }
                    })?;
                }
                Some(end)
            })
        }
        _ => skip_json_value(body, v),
    })
    .ok_or(DiscoveryErr::Malformed)?;
    if !have_data {
        return Err(DiscoveryErr::Malformed);
    }
    let mut i = 0;
    while i < underlyings.len() {
        if !listed[i] {
            return Err(DiscoveryErr::NotListed(i));
        }
        i += 1;
    }
    Ok(m)
}

/// One instrument row: `(Some(row) | None = refused by candidacy, end)`;
/// `None` = malformed.
fn parse_instrument(
    body: &[u8],
    pos: usize,
    und: usize,
    now_ms: i64,
    blackout_ms: i64,
) -> Option<(Option<HcMarketRow>, usize)> {
    let mut name: (u32, u32) = (0, 0);
    let mut strike_1e9: Option<i64> = None;
    let mut exp_s: Option<u64> = None;
    let mut call: Option<bool> = None;
    let mut active = false;
    let mut rfq = false;
    let end = walk_object(body, pos, |k, v| match k {
        b"id" => {
            let (s, end) = string_at(body, v)?;
            name = s;
            Some(end)
        }
        b"strike" => {
            let (x, end) = decimal_1e9(body, v)?;
            strike_1e9 = Some(x);
            Some(end)
        }
        b"expiry" => {
            let (x, end) = scan_u64(body, v)?;
            exp_s = Some(x);
            Some(end)
        }
        b"option_type" => {
            let (s, end) = string_at(body, v)?;
            call = match span_bytes(body, s) {
                b"call" => Some(true),
                b"put" => Some(false),
                _ => None,
            };
            Some(end)
        }
        b"status" => {
            let (s, end) = string_at(body, v)?;
            active = span_bytes(body, s) == b"ACTIVE";
            Some(end)
        }
        b"trading_mode" => {
            let (s, end) = string_at(body, v)?;
            rfq = memchr::memmem::find(span_bytes(body, s), b"rfq").is_some();
            Some(end)
        }
        _ => skip_json_value(body, v),
    })?;
    let n = span_bytes(body, name);
    let (Some(strike_1e9), Some(exp_s), Some(call)) = (strike_1e9, exp_s, call) else {
        return Some((None, end));
    };
    let exp_ms = i64::try_from(exp_s).ok()?.checked_mul(1000)?;
    if n.is_empty() || n.len() > HC_SYMBOL_MAX || !active || !rfq || exp_ms - now_ms <= blackout_ms {
        return Some((None, end));
    }
    let mut row = HcMarketRow {
        name: [0; HC_SYMBOL_MAX],
        name_len: n.len() as u8,
        underlying: und as u8,
        call,
        exp_ms,
        strike_1e9,
    };
    // COPY: instrument name ≤ 32 B (HC_SYMBOL_MAX) per candidate row, boot
    // only — the row outlives the 4.3 MB `/markets` body, which is dropped
    // once discovery returns, and its name becomes the symbol table's key
    // — rejected: keeping the body alive for the process to borrow spans.
    row.name[..n.len()].copy_from_slice(n);
    Some((Some(row), end))
}

/// The O-HC2 universe: per configured underlying, in config order, the
/// capped chain (`expiries_e` × `strikes_k` × {C, P}) around its index.
/// Output order is the deterministic allocation order (underlying →
/// expiry asc → strike asc → call before put) — ordinals are assigned
/// in it, append-never-reorder within a boot.
#[must_use]
pub fn select_universe(m: &Markets, underlyings: usize, expiries_e: u32, strikes_k: u32) -> Vec<HcMarketRow> {
    let mut out = Vec::with_capacity(underlyings * expiries_e as usize * strikes_k as usize * 2);
    let mut u = 0;
    while u < underlyings {
        let chain = select_capped_chain(
            &m.rows,
            |r: &HcMarketRow| r.underlying as usize == u,
            m.index_1e9[u],
            expiries_e,
            strikes_k,
        );
        // COPY: one underlying's selected rows (≤ E × K × 2 = 48 × 56 B),
        // boot only — the law returns each chain owned and the universe is
        // their concatenation in allocation order — rejected: a sink-taking
        // variant of the shared M2 selection law, for one boot-time copy.
        out.extend_from_slice(&chain);
        u += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed REAL `/markets` body (2026-09-25 19:07Z): three BTC
    /// expiries, one BOT (decimal strikes), two SP500 — 24 strikes each
    /// around the index; one BTC row set to `SETTLED` and one to
    /// orderbook-only so the candidacy test has something to refuse.
    const MARKETS: &[u8] = include_bytes!("../tests/fixtures/markets_trimmed.json");
    /// 2026-09-25 15:26:40Z: every fixture expiry is ≥ 4.5 h away.
    const NOW: i64 = 1_790_350_000_000;
    const BTC: &[u8] = b"BTC";
    const BOT: &[u8] = b"BOT";
    const SP500: &[u8] = b"SP500";

    #[test]
    fn one_pass_keeps_the_configured_candidates_and_their_index() {
        let m = parse_markets(MARKETS, &[BTC, BOT, SP500], NOW, DEFAULT_BLACKOUT_MS).unwrap();
        assert_eq!(m.rows.len(), 6 * 24 - 2);
        assert_eq!(m.refused, 2, "the SETTLED row and the orderbook-only row");
        assert_eq!(m.index_1e9[0], 84_000_000_000_000);
        assert_eq!(m.index_1e9[1], 28_152_999_999, "28.152999999999998692… truncates at 1e-9");
        assert_eq!(m.index_1e9[2], 7_736_600_000_000);
        let bot: Vec<&HcMarketRow> = m.rows.iter().filter(|r| r.underlying == 1).collect();
        assert_eq!(bot[0].name(), b"BOT-20260925-7.5-C");
        assert_eq!((bot[0].strike_1e9, bot[0].call), (7_500_000_000, true));
        assert_eq!(bot[0].exp_ms, 1_790_366_400_000);
        // Only what was configured.
        let m = parse_markets(MARKETS, &[SP500], NOW, DEFAULT_BLACKOUT_MS).unwrap();
        assert!(m.rows.iter().all(|r| r.underlying == 0 && r.name().starts_with(b"SP500-")));
        assert_eq!(m.rows.len(), 48);
    }

    #[test]
    fn the_capped_chain_is_e_expiries_by_k_strikes_by_two_rights_in_ordinal_order() {
        let m = parse_markets(MARKETS, &[BTC, BOT, SP500], NOW, DEFAULT_BLACKOUT_MS).unwrap();
        let u = select_universe(&m, 3, 2, 4);
        // BTC 2 expiries, BOT only 1 listed, SP500 2 — × 4 strikes × 2.
        assert_eq!(u.len(), 16 + 8 + 16);
        let names: Vec<String> = u.iter().map(|r| String::from_utf8_lossy(r.name()).to_string()).collect();
        // Position-based (the M2 law): the last K/2 strikes at-or-below
        // the 84 000 index, then the first K/2 above. (BTC's first
        // expiry lost its 81000-C / -P to candidacy — outside anyway.)
        assert_eq!(
            &names[..8],
            &[
                "BTC-20260926-83500-C",
                "BTC-20260926-83500-P",
                "BTC-20260926-84000-C",
                "BTC-20260926-84000-P",
                "BTC-20260926-84500-C",
                "BTC-20260926-84500-P",
                "BTC-20260926-85000-C",
                "BTC-20260926-85000-P",
            ]
        );
        assert!(names[8].starts_with("BTC-20260927-"), "then the next expiry");
        assert!(names[16].starts_with("BOT-20260925-"), "then the next underlying");
        assert!(names[24].starts_with("SP500-20260925-"));
        // Expiry asc → strike asc → call before put within an underlying.
        let mut i = 1;
        while i < 16 {
            let (a, b) = (&u[i - 1], &u[i]);
            assert!((a.exp_ms, a.strike_1e9, !a.call) < (b.exp_ms, b.strike_1e9, !b.call), "{i}");
            i += 1;
        }
    }

    #[test]
    fn a_series_inside_the_blackout_is_never_selected() {
        // One hour before BTC's first expiry (2026-09-26 08:00Z).
        let now = 1_790_409_600_000 - 3_600_000;
        let m = parse_markets(MARKETS, &[BTC], now, DEFAULT_BLACKOUT_MS).unwrap();
        assert!(m.rows.iter().all(|r| r.exp_ms > 1_790_409_600_000));
        let u = select_universe(&m, 1, 3, 4);
        assert_eq!(u.len(), 16, "only the two later expiries remain");
        assert!(u[0].name().starts_with(b"BTC-20260927-"));
    }

    #[test]
    fn an_unlisted_underlying_or_a_foreign_body_refuses_to_boot() {
        assert_eq!(
            parse_markets(MARKETS, &[BTC, b"ETH"], NOW, DEFAULT_BLACKOUT_MS).unwrap_err(),
            DiscoveryErr::NotListed(1)
        );
        assert_eq!(
            parse_markets(br#"{"success":true}"#, &[BTC], NOW, 0).unwrap_err(),
            DiscoveryErr::Malformed
        );
        assert_eq!(
            parse_markets(b"<html>", &[BTC], NOW, 0).unwrap_err(),
            DiscoveryErr::Malformed
        );
        let too_many: Vec<&[u8]> = vec![BTC; HC_MAX_UNDERLYINGS + 1];
        assert_eq!(
            parse_markets(MARKETS, &too_many, NOW, 0).unwrap_err(),
            DiscoveryErr::TooManyUnderlyings
        );
        assert!(DiscoveryErr::NotListed(1).to_string().contains("#1"));
    }
}
