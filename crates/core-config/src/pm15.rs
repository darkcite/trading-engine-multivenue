// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pm15.toml` — the PM15 member's artifact (slot 9; build plan
//! `claude/pm15-build-plan-2026-10-17.md`).
//!
//! The `bin15.toml` grammar, one `[pm15]` section
//! ([`crate::icdp::parse_single_section`]), every bound checked at parse;
//! the engine hashes the file BYTES so the boot tell names the artifact in
//! force. The frame is ruling R-PM-8's — $250 per order and per market, a
//! −$500 session loss bound — and paper runs at the same frame.

use crate::bin15::{int, opt_int, pos_u64, strs, table};
use crate::icdp::parse_single_section;
use std::path::Path;

/// Points in the late Φ table (`core_twaplock::late::LATE_PHI_POINTS`).
pub const PM15_PHI_POINTS: usize = 4097;

/// The least `phi_lut[4096]` (Φ at `z = 8.192`) the grammar admits.
pub const PM15_PHI_LAST_MIN_1E6: u32 = 999_999;

/// Families one artifact may configure (`core_types::pm_roll::PM_ROLLING_MAX`).
pub const PM15_MAX_FAMILIES: usize = 8;

/// The widest late window, ns to the end.
pub const PM15_TAU_MAX_NS_LIMIT: u64 = 60_000_000_000;

/// The most one order or one market may spend, ×1e6 USD — a guard on a
/// slipped scale, not the frame (the frame is the artifact's).
pub const PM15_USD_LIMIT_1E6: i64 = 10_000_000_000;

/// A `pm15.toml` that could not be read or did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pm15Error(pub String);

impl std::fmt::Display for Pm15Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pm15.toml: {}", self.0)
    }
}

impl std::error::Error for Pm15Error {}

fn err(msg: impl Into<String>) -> Pm15Error {
    Pm15Error(msg.into())
}

/// Every key the grammar accepts; an unknown key is a refusal.
const PM15_KEYS: [&str; 15] = [
    "families",
    "perp",
    "tau_min_ns",
    "tau_max_ns",
    "p_min_1e6",
    "e_1e6",
    "persist",
    "sigma_k_1e3",
    "usd_order_1e6",
    "usd_market_1e6",
    "session_loss_usd_1e6",
    "max_orders_market",
    "order_ttl_ns",
    "live_assets",
    "phi_lut",
];

/// `pm15.toml` as parsed. Descriptors stay strings: `pm15_boot` resolves
/// them against the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pm15File {
    /// The families' assets (the slug's spelling), equal to the universe's
    /// `[polymarket] rolling` assets as a sequence.
    pub families: Vec<String>,
    /// Per family, the fast-venue instrument descriptor of the nowcast.
    pub perp: Vec<String>,
    /// The late window's floor, ns to the end.
    pub tau_min_ns: u64,
    /// The late window's ceiling, ns to the end.
    pub tau_max_ns: u64,
    /// The least belief on the favoured side ×1e6.
    pub p_min_1e6: i64,
    /// The margin the ask must leave under it ×1e6.
    pub e_1e6: i64,
    /// Consecutive report seconds the test must hold on, `1..=8`.
    pub persist: u8,
    /// The pricer's σ scale ×1e3, `250..=4000`.
    pub sigma_k_1e3: i64,
    /// Most one order spends ×1e6 USD.
    pub usd_order_1e6: i64,
    /// Most one market holds ×1e6 USD.
    pub usd_market_1e6: i64,
    /// The session loss bound ×1e6 USD (positive).
    pub session_loss_usd_1e6: i64,
    /// IoCs at most per market (absent = 4).
    pub max_orders_market: u8,
    /// An IoC's time-to-live, ns (absent = 1 s).
    pub order_ttl_ns: u64,
    /// The assets ruling R-PM-4 lets arm live (absent = none); a subset of
    /// `families`. The member trades every family in paper; the execution
    /// side refuses a live order on any other.
    pub live_assets: Vec<String>,
    /// `Φ(z) ×1e6` for `z = 0, 0.002, …, 8.192`.
    pub phi_lut: Vec<u32>,
}

/// Read and parse the artifact, returning it with its RAW BYTES.
///
/// # Errors
/// [`Pm15Error`] — unreadable, not UTF-8, or the grammar refused it.
pub fn load(path: &Path) -> Result<(Pm15File, Vec<u8>), Pm15Error> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|e| err(format!("{}: not UTF-8 ({e})", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

/// The default artifact path: `~/multivenue/pm15.toml`.
///
/// # Errors
/// `HOME` is not set.
pub fn default_pm15_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/pm15.toml")
}

fn in_range(key: &str, v: i64, lo: i64, hi: i64) -> Result<i64, Pm15Error> {
    if !(lo..=hi).contains(&v) {
        return Err(err(format!("`{key}` = {v} is outside {lo}..={hi}")));
    }
    Ok(v)
}

/// Parse the artifact.
///
/// # Errors
/// [`Pm15Error`] — the first rule the source breaks.
pub fn parse(src: &str) -> Result<Pm15File, Pm15Error> {
    let kv = parse_single_section(src, "pm15", &PM15_KEYS).map_err(|e| err(e.0))?;
    let m = |e: crate::bin15::Bin15Error| err(e.0.replace("bin15", "pm15"));
    let families = strs(&kv, "families").map_err(m)?;
    let perp = strs(&kv, "perp").map_err(m)?;
    if families.is_empty() || families.len() > PM15_MAX_FAMILIES {
        return Err(err(format!(
            "`families` must name 1..={PM15_MAX_FAMILIES} assets"
        )));
    }
    let mut i = 0usize;
    while i < families.len() {
        let a = &families[i];
        if a.is_empty()
            || a.len() > 15
            || !a
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Err(err(format!(
                "`families`[{i}] = `{a}`: an asset is 1..=15 bytes of [a-z0-9]"
            )));
        }
        if families[..i].contains(a) {
            return Err(err(format!("`families`[{i}] = `{a}` repeats")));
        }
        i += 1;
    }
    if perp.len() != families.len() {
        return Err(err(format!(
            "`perp` carries {} descriptors for {} families — one fast-venue instrument per family",
            perp.len(),
            families.len()
        )));
    }
    let tau_min_ns = pos_u64(&kv, "tau_min_ns").map_err(m)?;
    let tau_max_ns = pos_u64(&kv, "tau_max_ns").map_err(m)?;
    if tau_min_ns == 0 || tau_min_ns >= tau_max_ns || tau_max_ns > PM15_TAU_MAX_NS_LIMIT {
        return Err(err(format!(
            "the late window must satisfy 0 < tau_min_ns < tau_max_ns ≤ {PM15_TAU_MAX_NS_LIMIT}"
        )));
    }
    let p_min_1e6 = in_range(
        "p_min_1e6",
        int(&kv, "p_min_1e6").map_err(m)?,
        500_000,
        999_999,
    )?;
    let e_1e6 = in_range("e_1e6", int(&kv, "e_1e6").map_err(m)?, 0, p_min_1e6 - 1)?;
    let persist = in_range("persist", int(&kv, "persist").map_err(m)?, 1, 8)? as u8;
    let sigma_k_1e3 = in_range(
        "sigma_k_1e3",
        int(&kv, "sigma_k_1e3").map_err(m)?,
        250,
        4_000,
    )?;
    let usd_order_1e6 = in_range(
        "usd_order_1e6",
        int(&kv, "usd_order_1e6").map_err(m)?,
        1,
        PM15_USD_LIMIT_1E6,
    )?;
    let usd_market_1e6 = in_range(
        "usd_market_1e6",
        int(&kv, "usd_market_1e6").map_err(m)?,
        usd_order_1e6,
        PM15_USD_LIMIT_1E6,
    )?;
    let session_loss_usd_1e6 = in_range(
        "session_loss_usd_1e6",
        int(&kv, "session_loss_usd_1e6").map_err(m)?,
        1,
        PM15_USD_LIMIT_1E6,
    )?;
    let max_orders_market = in_range(
        "max_orders_market",
        opt_int(&kv, "max_orders_market", 4).map_err(m)?,
        1,
        16,
    )? as u8;
    let order_ttl_ns = in_range(
        "order_ttl_ns",
        opt_int(&kv, "order_ttl_ns", 1_000_000_000).map_err(m)?,
        200_000_000,
        10_000_000_000,
    )? as u64;
    let live_assets = if kv.iter().any(|(k, _, _)| k == "live_assets") {
        strs(&kv, "live_assets").map_err(m)?
    } else {
        Vec::new()
    };
    let mut i = 0usize;
    while i < live_assets.len() {
        if !families.contains(&live_assets[i]) {
            return Err(err(format!(
                "`live_assets`[{i}] = `{}` is not a family",
                live_assets[i]
            )));
        }
        i += 1;
    }
    let phi_lut = table(
        &kv,
        "phi_lut",
        PM15_PHI_POINTS,
        Some(500_000),
        Some(PM15_PHI_LAST_MIN_1E6),
        false,
    )
    .map_err(m)?;
    Ok(Pm15File {
        families,
        perp,
        tau_min_ns,
        tau_max_ns,
        p_min_1e6,
        e_1e6,
        persist,
        sigma_k_1e3,
        usd_order_1e6,
        usd_market_1e6,
        session_loss_usd_1e6,
        max_orders_market,
        order_ttl_ns,
        live_assets,
        phi_lut,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phi() -> String {
        let mut v: Vec<String> = Vec::with_capacity(PM15_PHI_POINTS);
        let mut i = 0usize;
        while i < PM15_PHI_POINTS {
            v.push(format!("{}", 500_000 + (i as u64 * 499_999 / 4_096)));
            i += 1;
        }
        format!("[{}]", v.join(", "))
    }

    fn src(extra: &str) -> String {
        format!(
            "[pm15]\nfamilies = [\"btc\", \"eth\"]\nperp = [\"hyperliquid:BTC\", \"hyperliquid:ETH\"]\n\
             tau_min_ns = 2000000000\ntau_max_ns = 20000000000\np_min_1e6 = 900000\ne_1e6 = 20000\n\
             persist = 2\nsigma_k_1e3 = 1550\nusd_order_1e6 = 250000000\nusd_market_1e6 = 250000000\n\
             session_loss_usd_1e6 = 500000000\n{extra}phi_lut = {}\n",
            phi()
        )
    }

    #[test]
    fn the_ruled_frame_parses() {
        let f = parse(&src("live_assets = [\"btc\", \"eth\"]\n")).expect("valid");
        assert_eq!(f.families, vec!["btc", "eth"]);
        assert_eq!(f.perp[1], "hyperliquid:ETH");
        assert_eq!(
            (f.tau_min_ns, f.tau_max_ns),
            (2_000_000_000, 20_000_000_000)
        );
        assert_eq!(
            (f.usd_order_1e6, f.usd_market_1e6, f.session_loss_usd_1e6),
            (250_000_000, 250_000_000, 500_000_000)
        );
        assert_eq!(
            (f.max_orders_market, f.order_ttl_ns),
            (4, 1_000_000_000),
            "the stated defaults"
        );
        assert_eq!(f.live_assets, vec!["btc", "eth"]);
        assert_eq!(f.phi_lut.len(), PM15_PHI_POINTS);
        assert!(parse(&src(""))
            .expect("live_assets optional")
            .live_assets
            .is_empty());
    }

    #[test]
    fn every_bound_refuses() {
        let base = src("");
        for (from, to) in [
            ("tau_max_ns = 20000000000", "tau_max_ns = 2000000000"),
            ("tau_max_ns = 20000000000", "tau_max_ns = 70000000000"),
            ("p_min_1e6 = 900000", "p_min_1e6 = 400000"),
            ("e_1e6 = 20000", "e_1e6 = 900000"),
            ("persist = 2", "persist = 9"),
            ("sigma_k_1e3 = 1550", "sigma_k_1e3 = 100"),
            ("usd_market_1e6 = 250000000", "usd_market_1e6 = 100000000"),
            (
                "session_loss_usd_1e6 = 500000000",
                "session_loss_usd_1e6 = 0",
            ),
            (
                "perp = [\"hyperliquid:BTC\", \"hyperliquid:ETH\"]",
                "perp = [\"hyperliquid:BTC\"]",
            ),
            (
                "families = [\"btc\", \"eth\"]",
                "families = [\"btc\", \"btc\"]",
            ),
            (
                "families = [\"btc\", \"eth\"]",
                "families = [\"BTC\", \"eth\"]",
            ),
        ] {
            assert!(base.contains(from), "{from}");
            assert!(parse(&base.replacen(from, to, 1)).is_err(), "{to}");
        }
        assert!(
            parse(&src("live_assets = [\"sol\"]\n")).is_err(),
            "not a family"
        );
        assert!(
            parse(&src("cap_day_usd_1e6 = 1\n")).is_err(),
            "no day cap key exists"
        );
        assert!(parse(&src("order_ttl_ns = 100\n")).is_err());
        assert!(parse(&base.replacen("phi_lut = [500000", "phi_lut = [500001", 1)).is_err());
    }
}
