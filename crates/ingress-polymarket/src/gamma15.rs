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
//!
//! PM15-9b: Gamma's law is the first of two. The discovery then reads the
//! same market's record from the CLOB itself ([`crate::clob_law`]: the
//! taker hold, the fee, the live tick) and refuses the window on a breach
//! ([`PmLawErr::ClobShape`] … [`PmLawErr::ClobTick`]); the CLOB's tick
//! replaces Gamma's in the spec.
//!
//! ## PMJ J1b: the 5-minute windows
//!
//! [`parse_window_spec_for`] / [`render_slug_for`] take the family's
//! [`PmPeriod`]: the slug is `<asset>-updown-5m-<open>` on the 300 s grid
//! and the settlement must say `5m`; every other check is the 15-minute
//! one. [`parse_window_spec`] and [`render_slug`] are the 15-minute calls,
//! unchanged. The spec also carries the market's Gamma id
//! ([`PmWindowSpec::market_id`]), which Jupiter Predict names its markets
//! by.

use crate::discovery::{next_member, parse_row, PmDiscoveryErr, PmRollLaw, PM_TOKEN_MAX};
use core_parse::{skip_json_value, skip_ws};
use core_types::pm_roll::{PmPeriod, PM_TWAP_S};

/// Longest asset name of a family (the universe's `[a-z0-9]{1,15}`).
pub const PM_ASSET_MAX: usize = 15;

/// The fee rate the H90 law was measured under, ×1e9 (0.07).
pub const PM15_FEE_RATE_1E9: i64 = 70_000_000;

/// The two price ticks a 15-minute market quotes on, ×1e9 (PM15-9b: the
/// CLOB law ([`crate::clob_law`]) holds `mts` to the same pair).
pub(crate) const TICK_01_1E9: i64 = 10_000_000;
pub(crate) const TICK_001_1E9: i64 = 1_000_000;

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
    /// The window index (open epoch / the period's length).
    pub window: u32,
    /// The tick ×1e6 (10 000 or 1 000): Gamma's `orderPriceMinTickSize`,
    /// which the discovery OVERRIDES with the CLOB's `mts` whenever it
    /// reads the CLOB record (PM15-9b, [`crate::clob_law`]).
    pub tick_1e6: i64,
    /// `orderMinSize`, shares ×1e6.
    pub min_size_1e6: i64,
    /// PMJ J1b: the market's Gamma `id` (0 when the answer had none).
    pub market_id: u64,
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
        market_id: 0,
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
    /// Not the 60 s Chainlink TWAP settlement of this asset on the
    /// family's period.
    Settlement,
    /// Not the 0.07 / exponent 1 / taker-only fee.
    Fee,
    /// A tick the ladder cannot hold, or no minimum size.
    OrderLaw,
    // PM15-9b: the CLOB's own record ([`crate::clob_law`]). Appended, so
    // the causes above keep their discriminants (the `last_law_refusal`
    // encoding, `cause as u8`): 8..=11.
    /// The CLOB record did not parse, named another condition id, or the
    /// book is not accepting orders (`ao`).
    ClobShape,
    /// The CLOB record carries no crypto taker hold (`itode` missing or
    /// false) — not the market H90 was measured on.
    ClobHold,
    /// The CLOB's fee (`fd`) is not 0.07 / exponent 1 / taker only.
    ClobFee,
    /// The CLOB's tick (`mts`) is not 0.01 / 0.001, or its minimum order
    /// size (`mos`) is not positive.
    ClobTick,
}

impl PmLawErr {
    /// Every cause, in discriminant order (`cause as u8` is the index).
    pub const ALL: [Self; 12] = [
        Self::Shape,
        Self::Slug,
        Self::Outcomes,
        Self::NotTradable,
        Self::NegRisk,
        Self::Settlement,
        Self::Fee,
        Self::OrderLaw,
        Self::ClobShape,
        Self::ClobHold,
        Self::ClobFee,
        Self::ClobTick,
    ];

    /// PM15-9b: the cause a `cause as u8` code names (the counters'
    /// `last_law_refusal` / `last_law_change` encoding); `None` for a code
    /// no cause has.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        if (code as usize) < Self::ALL.len() {
            Some(Self::ALL[code as usize])
        } else {
            None
        }
    }
}

impl From<PmDiscoveryErr> for PmLawErr {
    #[inline]
    fn from(_: PmDiscoveryErr) -> Self {
        Self::Shape
    }
}

/// Render the slug of `asset`'s 15-minute window `window` into `dst`; its
/// length. `dst` must hold `asset.len() + 25` bytes.
#[must_use]
pub fn render_slug(dst: &mut [u8], asset: &[u8], window: u32) -> Option<usize> {
    render_slug_for(PmPeriod::M15, dst, asset, window)
}

/// PMJ J1b: render the slug of `asset`'s `period` window `window` into
/// `dst`; its length. `dst` must hold `asset.len() + 25` bytes.
#[must_use]
pub fn render_slug_for(period: PmPeriod, dst: &mut [u8], asset: &[u8], window: u32) -> Option<usize> {
    let mid: &[u8] = period.slug_mid();
    let mut digits = [0u8; 20];
    let mut v = period.window_open_s(window);
    let mut n = 0usize;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let len = asset.len() + mid.len() + n;
    if asset.is_empty() || asset.len() > PM_ASSET_MAX || dst.len() < len {
        return None;
    }
    // COPY: the asset name and the slug's fixed middle (≤ 27 B) into the
    // request target, once per window lookup (cold).
    dst[..asset.len()].copy_from_slice(asset);
    dst[asset.len()..asset.len() + mid.len()].copy_from_slice(mid);
    let mut k = 0usize;
    while k < n {
        dst[asset.len() + mid.len() + k] = digits[n - 1 - k];
        k += 1;
    }
    Some(len)
}

/// Parse one `/events?slug=` answer for 15-minute `family` (`asset`,
/// window `window`) into `out` under the venue law (module docs). `out` is
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
    parse_window_spec_for(PmPeriod::M15, body, family, asset, window, out)
}

/// PMJ J1b: [`parse_window_spec`] for a family of `period` — its slug,
/// its window grid and its `cryptoMarketConfig.duration`.
///
/// # Errors
/// [`PmLawErr`] — the first check that failed.
pub fn parse_window_spec_for(
    period: PmPeriod,
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
    let n = render_slug_for(period, &mut want, asset, window).ok_or(PmLawErr::Slug)?;
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
    let duration_ok = match period {
        PmPeriod::M15 => law.cmc_15m,
        PmPeriod::M5 => law.cmc_5m,
    };
    if !law.cmc_seen
        || law.cmc_asset() != asset
        || !duration_ok
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
    spec.market_id = law.market_id;
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
        // PMJ J1b: the market's Gamma id, not the event's (1198359) nor
        // the settlement config's.
        assert_eq!(s.market_id, 5_768_487);
    }

    /// PMJ J1b: the same answer reshaped to a 5-minute window — the slug
    /// on the 300 s grid and the settlement's `5m` — is a 5 m spec; the
    /// 15-minute answer is no 5-minute window and the reverse.
    #[test]
    fn a_5m_answer_becomes_a_5m_spec_and_the_periods_never_cross() {
        use core_types::pm_roll::pm5_window_of_open;
        const OPEN5: u64 = 1_792_308_000;
        let live5 = LIVE
            .replace("btc-updown-15m-1792217700", "btc-updown-5m-1792308000")
            .replace(r#""duration":"15m""#, r#""duration":"5m""#)
            .replace("btc-15m-twap-60", "btc-5m-twap-60");
        let w5 = pm5_window_of_open(OPEN5).expect("grid");
        let mut s = PmWindowSpec::EMPTY;
        parse_window_spec_for(PmPeriod::M5, live5.as_bytes(), 1, b"btc", w5, &mut s)
            .expect("the 5 m law holds");
        assert_eq!((s.family, s.window, s.market_id), (1, w5, 5_768_487));
        assert_eq!(s.tick_1e6, 10_000);
        // A 5 m answer under the 15 m law, and the 15 m answer under the
        // 5 m law (with a 5 m slug, so the settlement decides).
        let w15 = pm_window_of_open(OPEN).expect("grid");
        let mut t = PmWindowSpec::EMPTY;
        assert!(parse_window_spec(live5.as_bytes(), 1, b"btc", w15, &mut t).is_err());
        let slug_only = LIVE.replace("btc-updown-15m-1792217700", "btc-updown-5m-1792308000");
        assert_eq!(
            parse_window_spec_for(PmPeriod::M5, slug_only.as_bytes(), 1, b"btc", w5, &mut t),
            Err(PmLawErr::Settlement)
        );
        assert_eq!(t, PmWindowSpec::EMPTY, "written only on Ok");
        let mut d = [0u8; 40];
        let n = render_slug_for(PmPeriod::M5, &mut d, b"eth", w5).expect("fits");
        assert_eq!(&d[..n], b"eth-updown-5m-1792308000");
        // A market id that is not a quoted decimal reads as 0, never a refusal.
        let odd = LIVE.replace(r#""id":"5768487""#, r#""id":5768487"#);
        assert_eq!(spec_of(&odd).expect("still a spec").market_id, 0);
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

    /// PM15-9b: the code of every cause names it back; the CLOB causes
    /// are appended after Gamma's (8..=11), so the old codes stand.
    #[test]
    fn every_cause_code_names_its_cause() {
        let mut k = 0usize;
        while k < PmLawErr::ALL.len() {
            let e = PmLawErr::ALL[k];
            // Exhaustive: a new cause stops this compiling until it joins
            // `ALL` (and this list).
            match e {
                PmLawErr::Shape
                | PmLawErr::Slug
                | PmLawErr::Outcomes
                | PmLawErr::NotTradable
                | PmLawErr::NegRisk
                | PmLawErr::Settlement
                | PmLawErr::Fee
                | PmLawErr::OrderLaw
                | PmLawErr::ClobShape
                | PmLawErr::ClobHold
                | PmLawErr::ClobFee
                | PmLawErr::ClobTick => {}
            }
            assert_eq!(e as u8, k as u8);
            assert_eq!(PmLawErr::from_code(k as u8), Some(e));
            k += 1;
        }
        assert_eq!(PmLawErr::OrderLaw as u8, 7);
        assert_eq!(PmLawErr::ClobShape as u8, 8);
        assert_eq!(PmLawErr::ClobTick as u8, 11);
        assert_eq!(PmLawErr::from_code(12), None);
        assert_eq!(PmLawErr::from_code(u8::MAX), None);
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
}
