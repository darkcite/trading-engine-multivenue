// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # gamma15 — one rolling window's market, from Gamma, under the venue law (PM15-2)
//!
//! A 15-minute window's market is looked up by its slug,
//! `GET /events?slug=<asset>-updown-15m-<open epoch>` on
//! `gamma-api.polymarket.com`; every window exists about a day ahead
//! (measured 2026-10-17). The answer is a one-event array whose event holds
//! one market; the market object is the static discovery's row
//! ([`crate::discovery`]), plus the fields the venue law reads
//! ([`crate::discovery::PmRollLaw`]).
//!
//! ## The venue law ([`parse_window_spec`])
//!
//! A window becomes a [`PmWindowSpec`] — and so a family's next binding —
//! only when its market is the market the H90 law was measured on. Any
//! failure leaves the family DORMANT for that window (no roll is created),
//! counted by cause ([`PmLawErr`]):
//!
//! | check | wanted |
//! |---|---|
//! | slug | `<asset>-updown-15m-<open>` exactly |
//! | outcomes | `["Up", "Down"]` (so `clobTokenIds[0]` is Up) and two token ids |
//! | tradable | `active`, not `closed`, `acceptingOrders`, `enableOrderBook` |
//! | `negRisk` | false |
//! | settlement | `cryptoMarketConfig`: this asset, `15m`, `twapEnabled`, 60 s lookback |
//! | fee | `feeSchedule`: rate 0.07, exponent 1, taker only |
//! | tick | `orderPriceMinTickSize` 0.01 or 0.001 (the ladder's grid) |
//! | minimum | `orderMinSize` > 0 |
//!
//! Cold path (the discovery thread, a few calls a window): the spec is a
//! fixed POD; the token and condition ids are copied once into it.

use crate::discovery::{next_member, parse_row, PmDiscoveryErr, PmRollLaw, PM_TOKEN_MAX};
use core_parse::{skip_json_value, skip_ws};
use core_types::pm_roll::{pm_window_open_s, PM_TWAP_S};

/// Longest asset name of a family (the universe's `[a-z0-9]{1,15}`).
pub const PM_ASSET_MAX: usize = 15;

/// The fee rate the H90 law was measured under, ×1e9 (0.07).
pub const PM15_FEE_RATE_1E9: i64 = 70_000_000;

/// The two price ticks a 15-minute market quotes on, ×1e9.
const TICK_01_1E9: i64 = 10_000_000;
const TICK_001_1E9: i64 = 1_000_000;

/// A token id, ASCII decimal, `len` bytes valid.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PmTokenId {
    /// The digits.
    pub b: [u8; PM_TOKEN_MAX],
    /// Valid prefix of `b`.
    pub len: u8,
}

impl PmTokenId {
    /// No token.
    pub const NONE: Self = Self {
        b: [0; PM_TOKEN_MAX],
        len: 0,
    };

    /// The id's bytes.
    #[inline(always)]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.b[..(self.len as usize).min(PM_TOKEN_MAX)]
    }
}

/// One window's market, venue-law checked: what the ingress binds.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PmWindowSpec {
    /// The family index (boot `[polymarket] rolling` order).
    pub family: u8,
    /// Valid prefix of `condition_id`.
    pub condition_id_len: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 2],
    /// The window index (open epoch / 900).
    pub window: u32,
    /// `orderPriceMinTickSize` ×1e6 (10 000 or 1 000).
    pub tick_1e6: i64,
    /// `orderMinSize`, shares ×1e6.
    pub min_size_1e6: i64,
    /// PM15-5b: the venue's published strike, `eventMetadata.priceToBeat`
    /// ×1e6 — 0 until the PREVIOUS window resolves (≈ open + 116 s).
    pub strike_1e6: i64,
    /// The Up token.
    pub up: PmTokenId,
    /// The Down token.
    pub down: PmTokenId,
    /// `conditionId`, `0x` + 64 hex.
    pub condition_id: [u8; 66],
}

impl PmWindowSpec {
    /// The empty spec.
    pub const EMPTY: Self = Self {
        family: 0,
        condition_id_len: 0,
        _pad: [0; 2],
        window: 0,
        tick_1e6: 0,
        min_size_1e6: 0,
        strike_1e6: 0,
        up: PmTokenId::NONE,
        down: PmTokenId::NONE,
        condition_id: [0; 66],
    };

    /// The condition id's bytes.
    #[inline]
    #[must_use]
    pub fn condition_id(&self) -> &[u8] {
        &self.condition_id[..(self.condition_id_len as usize).min(66)]
    }
}

/// Why a window's answer did not become a spec.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmLawErr {
    /// The body is not the one-event, one-market shape (or did not parse).
    Shape,
    /// The answer named another market.
    Slug,
    /// Not `["Up", "Down"]` with two token ids.
    Outcomes,
    /// Not open for trading on the book.
    NotTradable,
    /// A neg-risk market.
    NegRisk,
    /// Not the 60 s Chainlink TWAP settlement of this asset on 15 m.
    Settlement,
    /// Not the 0.07 / exponent 1 / taker-only fee.
    Fee,
    /// A tick the ladder cannot hold, or no minimum size.
    OrderLaw,
}

impl From<PmDiscoveryErr> for PmLawErr {
    #[inline]
    fn from(_: PmDiscoveryErr) -> Self {
        Self::Shape
    }
}

/// PM15-5b: the published strike of a `/events?slug=` answer —
/// `eventMetadata.priceToBeat` ×1e6 (the venue writes a JSON number with
/// up to 14 decimals: truncated to 6). 0 while it is unpublished (`null`,
/// absent) or does not read as a positive number. The key appears in the
/// event's metadata alone.
#[must_use]
pub fn parse_price_to_beat(body: &[u8]) -> i64 {
    let Some(p) = core_parse::find_field(body, b"\"priceToBeat\":") else {
        return 0;
    };
    match core_parse::scan_number_sci_1e6(body, core_parse::skip_ws(body, p)) {
        Some((v, _)) if v > 0 => v,
        _ => 0,
    }
}

/// Render the slug of `asset`'s window `window` into `dst`; its length.
/// `dst` must hold `asset.len() + 25` bytes.
#[must_use]
pub fn render_slug(dst: &mut [u8], asset: &[u8], window: u32) -> Option<usize> {
    const MID: &[u8] = b"-updown-15m-";
    let mut digits = [0u8; 20];
    let mut v = pm_window_open_s(window);
    let mut n = 0usize;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let len = asset.len() + MID.len() + n;
    if asset.is_empty() || asset.len() > PM_ASSET_MAX || dst.len() < len {
        return None;
    }
    // COPY: the asset name and the slug's fixed middle (≤ 27 B) into the
    // request target, once per window lookup (cold).
    dst[..asset.len()].copy_from_slice(asset);
    dst[asset.len()..asset.len() + MID.len()].copy_from_slice(MID);
    let mut k = 0usize;
    while k < n {
        dst[asset.len() + MID.len() + k] = digits[n - 1 - k];
        k += 1;
    }
    Some(len)
}

/// Parse one `/events?slug=` answer for `family` (`asset`, window
/// `window`) into `out` under the venue law (module docs). `out` is
/// written only on `Ok`.
///
/// # Errors
/// [`PmLawErr`] — the first check that failed.
pub fn parse_window_spec(
    body: &[u8],
    family: usize,
    asset: &[u8],
    window: u32,
    out: &mut PmWindowSpec,
) -> Result<(), PmLawErr> {
    let mut law = PmRollLaw::default();
    let row = one_market(body, &mut law)?;
    // The slug names this asset's window.
    let mut want = [0u8; PM_ASSET_MAX + 25];
    let n = render_slug(&mut want, asset, window).ok_or(PmLawErr::Slug)?;
    if law.slug() != &want[..n] {
        return Err(PmLawErr::Slug);
    }
    if !law.outcomes_up_down || row.n_tokens != 2 {
        return Err(PmLawErr::Outcomes);
    }
    if !row.active || row.closed || !row.accepting_orders || !row.enable_order_book {
        return Err(PmLawErr::NotTradable);
    }
    if row.neg_risk {
        return Err(PmLawErr::NegRisk);
    }
    if !law.cmc_seen
        || law.cmc_asset() != asset
        || !law.cmc_15m
        || !law.cmc_twap_enabled
        || law.cmc_twap_s_1e9 != i64::from(PM_TWAP_S) * 1_000_000_000
    {
        return Err(PmLawErr::Settlement);
    }
    if !law.fee_seen
        || law.fee_rate_1e9 != PM15_FEE_RATE_1E9
        || law.fee_exponent_1e9 != 1_000_000_000
        || !law.fee_taker_only
    {
        return Err(PmLawErr::Fee);
    }
    let tick = row.order_price_min_tick_1e9;
    if (tick != TICK_01_1E9 && tick != TICK_001_1E9) || row.order_min_size_1e6 <= 0 {
        return Err(PmLawErr::OrderLaw);
    }
    let (Some(up), Some(down)) = (row.token(0), row.token(1)) else {
        return Err(PmLawErr::Outcomes);
    };
    let mut spec = PmWindowSpec::EMPTY;
    spec.family = family as u8;
    spec.window = window;
    spec.tick_1e6 = tick / 1_000;
    spec.min_size_1e6 = row.order_min_size_1e6;
    // COPY: the two token ids (≤ 80 B each) and the condition id (66 B)
    // into the spec, once per window (cold) — the spec outlives the body.
    spec.up.b[..up.len()].copy_from_slice(up);
    spec.up.len = up.len() as u8;
    spec.down.b[..down.len()].copy_from_slice(down);
    spec.down.len = down.len() as u8;
    let cid = &row.condition_id[..row.condition_id_len as usize];
    spec.condition_id[..cid.len()].copy_from_slice(cid);
    spec.condition_id_len = row.condition_id_len;
    *out = spec;
    Ok(())
}

/// The one market of a one-event answer.
// COPY: the row by value (≥ 264 B), once per window (cold) — as
// `discovery::parse_row`.
fn one_market(body: &[u8], law: &mut PmRollLaw) -> Result<crate::discovery::PmMarketRow, PmLawErr> {
    let mut i = skip_ws(body, 0);
    if body.get(i) != Some(&b'[') {
        return Err(PmLawErr::Shape);
    }
    i = skip_ws(body, i + 1);
    if body.get(i) != Some(&b'{') {
        return Err(PmLawErr::Shape);
    }
    i += 1;
    let mut row = None;
    while let Some(key) = next_member(body, &mut i)? {
        if key != b"markets" {
            i = skip_json_value(body, i).ok_or(PmLawErr::Shape)?;
            continue;
        }
        if body.get(i) != Some(&b'[') {
            return Err(PmLawErr::Shape);
        }
        i = skip_ws(body, i + 1);
        if body.get(i) != Some(&b'{') || row.is_some() {
            return Err(PmLawErr::Shape);
        }
        let (r, end) = parse_row(body, i, law)?;
        i = skip_ws(body, end);
        if body.get(i) != Some(&b']') {
            // A second market in the event: not the 15m shape.
            return Err(PmLawErr::Shape);
        }
        i += 1;
        row = Some(r);
    }
    i = skip_ws(body, i);
    if body.get(i) != Some(&b']') {
        return Err(PmLawErr::Shape);
    }
    row.ok_or(PmLawErr::Shape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::pm_roll::pm_window_of_open;

    /// The live 2026-10-17 answer for `btc-updown-15m-1792217700`, trimmed
    /// to the members the law reads plus a few it skips (nested arrays,
    /// escapes, the event-level duplicates of `negRisk` / `active`).
    const LIVE: &str = r#"[{"id":"1198359","slug":"btc-updown-15m-1792217700","description":"resolve to \"Up\" if","active":true,"closed":false,"negRisk":false,"enableOrderBook":true,"markets":[{"id":"5768487","question":"Bitcoin Up or Down - October 17, 2:15AM-2:30AM ET","conditionId":"0xa30ed80f53aa1c14b8ec38c9c5bd78d500e4702ac00d0135f330e26a2183a059","slug":"btc-updown-15m-1792217700","outcomes":"[\"Up\", \"Down\"]","outcomePrices":"[\"0.515\", \"0.485\"]","active":true,"closed":false,"enableOrderBook":true,"orderPriceMinTickSize":0.01,"orderMinSize":5,"clobTokenIds":"[\"34093056663108462497544643362722002462217972423461710020380621393557500264758\", \"88467165377337597854389991157504062307411366248534291856206167172694971782672\"]","acceptingOrders":true,"negRisk":false,"umaResolutionStatuses":"[]","feesEnabled":true,"feeSchedule":{"exponent":1,"rate":0.07,"takerOnly":true,"rebateRate":0.2},"cryptoMarketConfigId":"btc-15m-twap-60","cryptoMarketConfig":{"id":"btc-15m-twap-60","asset":"btc","duration":"15m","twapEnabled":true,"twapLookbackSeconds":60},"version":"v1"}],"series":[{"id":"1","active":true,"closed":false}],"tags":[{"id":"21"}],"version":"v1"}]"#;

    const OPEN: u64 = 1_792_217_700;

    fn spec_of(body: &str) -> Result<PmWindowSpec, PmLawErr> {
        let w = pm_window_of_open(OPEN).expect("grid");
        let mut s = PmWindowSpec::EMPTY;
        parse_window_spec(body.as_bytes(), 2, b"btc", w, &mut s).map(|()| s)
    }

    #[test]
    fn the_live_answer_becomes_a_spec() {
        let s = spec_of(LIVE).expect("venue law holds");
        assert_eq!(s.family, 2);
        assert_eq!(s.window, pm_window_of_open(OPEN).unwrap());
        assert_eq!(s.tick_1e6, 10_000);
        assert_eq!(s.min_size_1e6, 5_000_000);
        assert_eq!(
            s.up.as_bytes(),
            b"34093056663108462497544643362722002462217972423461710020380621393557500264758"
        );
        assert_eq!(
            s.down.as_bytes(),
            b"88467165377337597854389991157504062307411366248534291856206167172694971782672"
        );
        assert_eq!(
            s.condition_id(),
            b"0xa30ed80f53aa1c14b8ec38c9c5bd78d500e4702ac00d0135f330e26a2183a059"
        );
    }

    #[test]
    fn every_law_breach_is_named() {
        let w = pm_window_of_open(OPEN).unwrap();
        let mut s = PmWindowSpec::EMPTY;
        // Another asset's family asking for this answer: the slug.
        assert_eq!(
            parse_window_spec(LIVE.as_bytes(), 0, b"eth", w, &mut s),
            Err(PmLawErr::Slug)
        );
        assert_eq!(
            parse_window_spec(LIVE.as_bytes(), 0, b"btc", w + 1, &mut s),
            Err(PmLawErr::Slug)
        );
        assert_eq!(s, PmWindowSpec::EMPTY, "a refusal writes nothing");
        let cases = [
            (
                r#""outcomes":"[\"Up\", \"Down\"]""#,
                r#""outcomes":"[\"Down\", \"Up\"]""#,
                PmLawErr::Outcomes,
            ),
            (
                r#""outcomes":"[\"Up\", \"Down\"]""#,
                r#""outcomes":"[\"Yes\", \"No\"]""#,
                PmLawErr::Outcomes,
            ),
            (
                r#""acceptingOrders":true"#,
                r#""acceptingOrders":false"#,
                PmLawErr::NotTradable,
            ),
            (
                r#""closed":false,"enableOrderBook":true,"orderPrice"#,
                r#""closed":true,"enableOrderBook":true,"orderPrice"#,
                PmLawErr::NotTradable,
            ),
            (
                r#""acceptingOrders":true,"negRisk":false"#,
                r#""acceptingOrders":true,"negRisk":true"#,
                PmLawErr::NegRisk,
            ),
            (
                r#""twapLookbackSeconds":60"#,
                r#""twapLookbackSeconds":300"#,
                PmLawErr::Settlement,
            ),
            (
                r#""duration":"15m""#,
                r#""duration":"5m""#,
                PmLawErr::Settlement,
            ),
            (r#""asset":"btc""#, r#""asset":"eth""#, PmLawErr::Settlement),
            (
                r#""twapEnabled":true"#,
                r#""twapEnabled":false"#,
                PmLawErr::Settlement,
            ),
            (r#""rate":0.07"#, r#""rate":0.1"#, PmLawErr::Fee),
            (r#""takerOnly":true"#, r#""takerOnly":false"#, PmLawErr::Fee),
            (r#""exponent":1,"#, r#""exponent":2,"#, PmLawErr::Fee),
            (
                r#""orderPriceMinTickSize":0.01"#,
                r#""orderPriceMinTickSize":0.0001"#,
                PmLawErr::OrderLaw,
            ),
            (
                r#""orderMinSize":5"#,
                r#""orderMinSize":0"#,
                PmLawErr::OrderLaw,
            ),
            (
                r#"}],"series""#,
                r#"},{"conditionId":"0x1"}],"series""#,
                PmLawErr::Shape,
            ),
            (
                r#""markets":[{"#,
                r#""markets":[],"m2":[{"#,
                PmLawErr::Shape,
            ),
        ];
        let mut k = 0usize;
        while k < cases.len() {
            let (from, to, want) = cases[k];
            assert_eq!(
                LIVE.matches(from).count(),
                1,
                "case {k}: `{from}` is unique"
            );
            assert_eq!(
                spec_of(&LIVE.replacen(from, to, 1)),
                Err(want),
                "case {k}: {to}"
            );
            k += 1;
        }
        // The other tick is fine.
        let s = spec_of(&LIVE.replacen(
            r#""orderPriceMinTickSize":0.01"#,
            r#""orderPriceMinTickSize":0.001"#,
            1,
        ))
        .unwrap();
        assert_eq!(s.tick_1e6, 1_000);
        // Missing objects.
        let no_fee = LIVE.replacen(
            r#""feeSchedule":{"exponent":1,"rate":0.07,"takerOnly":true,"rebateRate":0.2},"#,
            "",
            1,
        );
        assert_eq!(spec_of(&no_fee), Err(PmLawErr::Fee));
        assert_eq!(spec_of("[]"), Err(PmLawErr::Shape));
        assert_eq!(spec_of(""), Err(PmLawErr::Shape));
        assert_eq!(spec_of(&LIVE[..LIVE.len() / 2]), Err(PmLawErr::Shape));
    }

    #[test]
    fn the_slug_renders_the_open_epoch() {
        let mut d = [0u8; 40];
        let w = pm_window_of_open(OPEN).unwrap();
        let n = render_slug(&mut d, b"hype", w).unwrap();
        assert_eq!(&d[..n], b"hype-updown-15m-1792217700");
        assert!(render_slug(&mut d[..10], b"hype", w).is_none());
        assert!(render_slug(&mut d, b"", w).is_none());
    }

    /// PM15-5b: the published strike, as the venue writes it (a JSON number
    /// of 14 decimals), truncated to 1e6; null, absent and non-positive are 0.
    #[test]
    fn the_published_strike_reads_from_event_metadata() {
        let body = br#"[{"id":"1","slug":"btc-updown-15m-1792216800","eventMetadata":{"finalPrice":81485.06303622595,"priceToBeat":81465.82952002053},"markets":[{"eventMetadata":null}]}]"#;
        assert_eq!(parse_price_to_beat(body), 81_465_829_520);
        assert_eq!(parse_price_to_beat(br#"[{"eventMetadata":{"priceToBeat":0.2036154}}]"#), 203_615);
        assert_eq!(parse_price_to_beat(br#"[{"eventMetadata":{"priceToBeat":1.5e3}}]"#), 1_500_000_000);
        assert_eq!(parse_price_to_beat(br#"[{"eventMetadata":null}]"#), 0);
        assert_eq!(parse_price_to_beat(br#"[{"eventMetadata":{"priceToBeat":null}}]"#), 0);
        assert_eq!(parse_price_to_beat(br#"[{"eventMetadata":{"priceToBeat":-1}}]"#), 0);
        assert_eq!(parse_price_to_beat(b"[]"), 0);
    }
}
