// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pmj.toml` — the PMJ member's artifact (slot 11; risk-policy "PMJ — slot
//! 11", project doc `claude/pmj-plan-2026-10-18.md`).
//!
//! The `pm15.toml` grammar ([`crate::pm15`]), one `[pmj]` section, every
//! bound checked at parse; the engine hashes the file BYTES so the boot tell
//! names the artifact in force. It carries:
//!
//! - **the law** — the H90 values (provisional: the 15-minute law's until
//!   the 5-minute research freezes its own) plus `price_floor_1e6`;
//! - **the Jupiter keys** — `min_tau_s` (the keeper's lead), the deposit
//!   per order and the per-order maximum the arm enforces, the market cap,
//!   the session loss bound, `max_slippage_bps` (PARSED, UNUSED until J0
//!   shows the venue lets an order set it);
//! - **the paper law's key** — `paper_delay_ms`, the keeper's delay the
//!   paper dispatcher judges an order at;
//! - **the transaction guard** (LAW PMJ-G, `exec_jupiter::tx::guard`) —
//!   `guard_programs`, `guard_alts`, `guard_deposit_account`,
//!   `guard_max_sol_lamports`, `guard_max_cu_price`: base58 keys kept as
//!   text here (checked for the alphabet and length) and decoded by the
//!   live arm's boot (J5). **Empty lists refuse every transaction** — the
//!   default until J0's decoded programs and tables are reviewed in.

use crate::bin15::{int, opt_int, pos_u64, strs, table};
use crate::icdp::{parse_single_section, Value};
use std::path::Path;

/// Points in the late Φ table (`core_twaplock::late::LATE_PHI_POINTS`).
pub const PMJ_PHI_POINTS: usize = 4097;

/// The least `phi_lut[4096]` the grammar admits.
pub const PMJ_PHI_LAST_MIN_1E6: u32 = 999_999;

/// Families one artifact may configure (`core_types::pm_roll::PM5_ROLLING_MAX`).
pub const PMJ_MAX_FAMILIES: usize = 8;

/// The widest late window, ns to the end.
pub const PMJ_TAU_MAX_NS_LIMIT: u64 = 60_000_000_000;

/// The most one order or one market may spend, ×1e6 USD — a guard on a
/// slipped scale, not the frame.
pub const PMJ_USD_LIMIT_1E6: i64 = 10_000_000_000;

/// The venue's least deposit, ×1e6 USD ($5).
pub const PMJ_MIN_DEPOSIT_1E6: i64 = 5_000_000;

/// What the member's sizing may add above the deposit, ×1e6 USD: it buys
/// whole hundredths of a share and one more when the rounding left the
/// order under $5, and a hundredth at a price under $1 costs under $0.01.
/// The per-order maximum is at least the deposit plus this (risk review
/// J4 round 1: the arm refuses an order above it).
pub const PMJ_SIZING_SLACK_1E6: i64 = 10_000;

/// Program ids / lookup tables the guard may list
/// (`exec_jupiter::tx::{GUARD_PROGRAMS_MAX, GUARD_ALTS_MAX}`).
pub const PMJ_GUARD_PROGRAMS_MAX: usize = 16;
/// See [`PMJ_GUARD_PROGRAMS_MAX`].
pub const PMJ_GUARD_ALTS_MAX: usize = 8;

/// A `pmj.toml` that could not be read or did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmjError(pub String);

impl std::fmt::Display for PmjError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pmj.toml: {}", self.0)
    }
}

impl std::error::Error for PmjError {}

fn err(msg: impl Into<String>) -> PmjError {
    PmjError(msg.into())
}

/// Every key the grammar accepts; an unknown key is a refusal.
const PMJ_KEYS: [&str; 28] = [
    "families",
    "perp",
    "tau_min_ns",
    "tau_max_ns",
    "p_min_1e6",
    "e_1e6",
    "persist",
    "sigma_k_1e3",
    "price_floor_1e6",
    "min_tau_s",
    "deposit_usd_1e6",
    "max_order_usd_1e6",
    "usd_market_1e6",
    "session_loss_usd_1e6",
    "max_orders_market",
    "order_ttl_ns",
    "max_slippage_bps",
    "paper_delay_ms",
    "live_assets",
    "phi_lut",
    "guard_programs",
    "guard_alts",
    "guard_deposit_account",
    "guard_max_sol_lamports",
    "guard_max_cu_price",
    // PMJ J12: crate::strike_law's three keys.
    "strike_law",
    "sigma_res_ratio_1e3",
    "sigma_res_min_ppm",
];

/// `pmj.toml` as parsed. Descriptors and keys stay strings: `pmj_boot`
/// resolves the descriptors, the live arm's boot decodes the keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmjFile {
    /// The families' assets, equal to the universe's `:5m` rolling assets
    /// as a sequence.
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
    /// Consecutive report seconds, `1..=8`.
    pub persist: u8,
    /// The pricer's σ scale ×1e3, `250..=4000`.
    pub sigma_k_1e3: i64,
    /// The favourite's ask must be at least this ×1e6 (absent = 0.90).
    pub price_floor_1e6: i64,
    /// No order closer to the end than this, s (absent = 3).
    pub min_tau_s: u64,
    /// What one order deposits ×1e6 USD (≥ $5).
    pub deposit_usd_1e6: i64,
    /// The per-order maximum the arm enforces ×1e6 USD (absent = the
    /// deposit + [`PMJ_SIZING_SLACK_1E6`]; never under that).
    pub max_order_usd_1e6: i64,
    /// Most one market holds ×1e6 USD.
    pub usd_market_1e6: i64,
    /// The session loss bound ×1e6 USD (positive).
    pub session_loss_usd_1e6: i64,
    /// IoCs at most per market (absent = 4).
    pub max_orders_market: u8,
    /// An IoC's time-to-live, ns (absent = 5 s).
    pub order_ttl_ns: u64,
    /// The slippage bound, bps — parsed, UNUSED (absent = 0).
    pub max_slippage_bps: u32,
    /// The keeper's delay the paper law judges an order at, ms (absent =
    /// 2 000).
    pub paper_delay_ms: u64,
    /// The assets that may arm live (absent = none); a subset of
    /// `families`. Nothing arms in this pass.
    pub live_assets: Vec<String>,
    /// `Φ(z) ×1e6` for `z = 0, 0.002, …, 8.192`.
    pub phi_lut: Vec<u32>,
    /// The guard's program ids, base58 (absent = none: refuse all).
    pub guard_programs: Vec<String>,
    /// The guard's lookup tables, base58 (absent = none).
    pub guard_alts: Vec<String>,
    /// The wallet's USDC token account, base58 (absent = none: every token
    /// debit refused).
    pub guard_deposit_account: String,
    /// The most lamports the wallet may send per transaction (absent = 0).
    pub guard_max_sol_lamports: u64,
    /// The highest compute unit price, micro-lamports (absent = 0).
    pub guard_max_cu_price: u64,
    /// PMJ J12: the strike law and law (iii)'s residual
    /// ([`crate::strike_law`]; absent = the first law).
    pub strike_law: crate::strike_law::StrikeLaw,
}

/// Read and parse the artifact, returning it with its RAW BYTES.
///
/// # Errors
/// [`PmjError`] — unreadable, not UTF-8, or the grammar refused it.
pub fn load(path: &Path) -> Result<(PmjFile, Vec<u8>), PmjError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|e| err(format!("{}: not UTF-8 ({e})", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

/// The default artifact path: `~/multivenue/pmj.toml`.
///
/// # Errors
/// `HOME` is not set.
pub fn default_pmj_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/pmj.toml")
}

fn in_range(key: &str, v: i64, lo: i64, hi: i64) -> Result<i64, PmjError> {
    if !(lo..=hi).contains(&v) {
        return Err(err(format!("`{key}` = {v} is outside {lo}..={hi}")));
    }
    Ok(v)
}

/// An optional string array (absent or `[]` = empty).
fn opt_strs(kv: &[(String, Value, usize)], key: &str) -> Result<Vec<String>, PmjError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        None => Ok(Vec::new()),
        Some((_, Value::Strs(v), _)) => Ok(v.clone()),
        Some((_, Value::Ints(v), _)) if v.is_empty() => Ok(Vec::new()),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be a string array"))),
    }
}

/// An optional string (absent = empty).
fn opt_str(kv: &[(String, Value, usize)], key: &str) -> Result<String, PmjError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        None => Ok(String::new()),
        Some((_, Value::Str(v), _)) => Ok(v.clone()),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be a string"))),
    }
}

/// A 32-byte key in base58: 32..=44 characters of the Bitcoin alphabet.
fn b58_key(key: &str, i: usize, v: &str) -> Result<(), PmjError> {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    if !(32..=44).contains(&v.len()) || !v.bytes().all(|b| ALPHABET.contains(&b)) {
        return Err(err(format!("`{key}`[{i}] = `{v}` is not a base58 key")));
    }
    Ok(())
}

/// Parse the artifact.
///
/// # Errors
/// [`PmjError`] — the first rule the source breaks.
pub fn parse(src: &str) -> Result<PmjFile, PmjError> {
    let kv = parse_single_section(src, "pmj", &PMJ_KEYS).map_err(|e| err(e.0))?;
    let m = |e: crate::bin15::Bin15Error| err(e.0.replace("bin15", "pmj"));
    let families = strs(&kv, "families").map_err(m)?;
    let perp = strs(&kv, "perp").map_err(m)?;
    if families.is_empty() || families.len() > PMJ_MAX_FAMILIES {
        return Err(err(format!("`families` must name 1..={PMJ_MAX_FAMILIES} assets")));
    }
    let mut i = 0usize;
    while i < families.len() {
        let a = &families[i];
        if a.is_empty()
            || a.len() > 15
            || !a.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
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
    if tau_min_ns == 0 || tau_min_ns >= tau_max_ns || tau_max_ns > PMJ_TAU_MAX_NS_LIMIT {
        return Err(err(format!(
            "the late window must satisfy 0 < tau_min_ns < tau_max_ns ≤ {PMJ_TAU_MAX_NS_LIMIT}"
        )));
    }
    let p_min_1e6 = in_range("p_min_1e6", int(&kv, "p_min_1e6").map_err(m)?, 500_000, 999_999)?;
    let e_1e6 = in_range("e_1e6", int(&kv, "e_1e6").map_err(m)?, 0, p_min_1e6 - 1)?;
    let persist = in_range("persist", int(&kv, "persist").map_err(m)?, 1, 8)? as u8;
    let sigma_k_1e3 = in_range("sigma_k_1e3", int(&kv, "sigma_k_1e3").map_err(m)?, 250, 4_000)?;
    let price_floor_1e6 = in_range(
        "price_floor_1e6",
        opt_int(&kv, "price_floor_1e6", 900_000).map_err(m)?,
        500_000,
        999_999,
    )?;
    let min_tau_s = in_range("min_tau_s", opt_int(&kv, "min_tau_s", 3).map_err(m)?, 0, 59)? as u64;
    if min_tau_s.saturating_mul(1_000_000_000) >= tau_max_ns {
        return Err(err("`min_tau_s` must be under the late window's ceiling (tau_max_ns)"));
    }
    let deposit_usd_1e6 = in_range(
        "deposit_usd_1e6",
        int(&kv, "deposit_usd_1e6").map_err(m)?,
        PMJ_MIN_DEPOSIT_1E6,
        PMJ_USD_LIMIT_1E6,
    )?;
    let max_order_usd_1e6 = in_range(
        "max_order_usd_1e6",
        opt_int(&kv, "max_order_usd_1e6", deposit_usd_1e6 + PMJ_SIZING_SLACK_1E6).map_err(m)?,
        deposit_usd_1e6 + PMJ_SIZING_SLACK_1E6,
        PMJ_USD_LIMIT_1E6,
    )?;
    let usd_market_1e6 = in_range(
        "usd_market_1e6",
        int(&kv, "usd_market_1e6").map_err(m)?,
        deposit_usd_1e6,
        PMJ_USD_LIMIT_1E6,
    )?;
    let session_loss_usd_1e6 = in_range(
        "session_loss_usd_1e6",
        int(&kv, "session_loss_usd_1e6").map_err(m)?,
        1,
        PMJ_USD_LIMIT_1E6,
    )?;
    let max_orders_market =
        in_range("max_orders_market", opt_int(&kv, "max_orders_market", 4).map_err(m)?, 1, 16)? as u8;
    let order_ttl_ns = in_range(
        "order_ttl_ns",
        opt_int(&kv, "order_ttl_ns", 5_000_000_000).map_err(m)?,
        1_000_000_000,
        30_000_000_000,
    )? as u64;
    let max_slippage_bps =
        in_range("max_slippage_bps", opt_int(&kv, "max_slippage_bps", 0).map_err(m)?, 0, 10_000)? as u32;
    let paper_delay_ms =
        in_range("paper_delay_ms", opt_int(&kv, "paper_delay_ms", 2_000).map_err(m)?, 0, 30_000)? as u64;
    if paper_delay_ms.saturating_mul(1_000_000) >= order_ttl_ns.saturating_mul(2) {
        return Err(err(
            "`paper_delay_ms` must be under twice `order_ttl_ns` (the member writes an order off then)",
        ));
    }
    // The keeper's lead bounds the paper delay: an order decided at the late
    // window's floor must still be judged before the close (risk review J4
    // round 1), or paper books fills the venue could never give.
    let lead_ns = tau_min_ns.max(min_tau_s.saturating_mul(1_000_000_000));
    if paper_delay_ms.saturating_mul(1_000_000) >= lead_ns {
        return Err(err(format!(
            "`paper_delay_ms` = {paper_delay_ms} must be under the keeper's lead, max(tau_min_ns, min_tau_s) = {} ms \
             — so an order decided at the window's floor is still judged before the close",
            lead_ns / 1_000_000
        )));
    }
    let live_assets = if kv.iter().any(|(k, _, _)| k == "live_assets") {
        strs(&kv, "live_assets").map_err(m)?
    } else {
        Vec::new()
    };
    let mut i = 0usize;
    while i < live_assets.len() {
        if !families.contains(&live_assets[i]) {
            return Err(err(format!("`live_assets`[{i}] = `{}` is not a family", live_assets[i])));
        }
        i += 1;
    }
    let phi_lut = table(&kv, "phi_lut", PMJ_PHI_POINTS, Some(500_000), Some(PMJ_PHI_LAST_MIN_1E6), false)
        .map_err(m)?;
    let guard_programs = opt_strs(&kv, "guard_programs")?;
    let guard_alts = opt_strs(&kv, "guard_alts")?;
    if guard_programs.len() > PMJ_GUARD_PROGRAMS_MAX || guard_alts.len() > PMJ_GUARD_ALTS_MAX {
        return Err(err(format!(
            "the guard lists at most {PMJ_GUARD_PROGRAMS_MAX} programs and {PMJ_GUARD_ALTS_MAX} lookup tables"
        )));
    }
    let mut i = 0usize;
    while i < guard_programs.len() {
        b58_key("guard_programs", i, &guard_programs[i])?;
        i += 1;
    }
    let mut i = 0usize;
    while i < guard_alts.len() {
        b58_key("guard_alts", i, &guard_alts[i])?;
        i += 1;
    }
    let guard_deposit_account = opt_str(&kv, "guard_deposit_account")?;
    if !guard_deposit_account.is_empty() {
        b58_key("guard_deposit_account", 0, &guard_deposit_account)?;
    }
    let strike_law = crate::strike_law::parse(&kv, families.len()).map_err(err)?;
    let guard_max_sol_lamports = in_range(
        "guard_max_sol_lamports",
        opt_int(&kv, "guard_max_sol_lamports", 0).map_err(m)?,
        0,
        1_000_000_000,
    )? as u64;
    let guard_max_cu_price = in_range(
        "guard_max_cu_price",
        opt_int(&kv, "guard_max_cu_price", 0).map_err(m)?,
        0,
        10_000_000,
    )? as u64;
    Ok(PmjFile {
        families,
        perp,
        tau_min_ns,
        tau_max_ns,
        p_min_1e6,
        e_1e6,
        persist,
        sigma_k_1e3,
        price_floor_1e6,
        min_tau_s,
        deposit_usd_1e6,
        max_order_usd_1e6,
        usd_market_1e6,
        session_loss_usd_1e6,
        max_orders_market,
        order_ttl_ns,
        max_slippage_bps,
        paper_delay_ms,
        live_assets,
        phi_lut,
        guard_programs,
        guard_alts,
        guard_deposit_account,
        guard_max_sol_lamports,
        guard_max_cu_price,
        strike_law,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phi() -> String {
        let mut v: Vec<String> = Vec::with_capacity(PMJ_PHI_POINTS);
        let mut i = 0usize;
        while i < PMJ_PHI_POINTS {
            v.push(format!("{}", 500_000 + (i as u64 * 499_999 / 4_096)));
            i += 1;
        }
        format!("[{}]", v.join(", "))
    }

    fn src(extra: &str) -> String {
        format!(
            "[pmj]\nfamilies = [\"btc\", \"eth\"]\nperp = [\"hyperliquid:BTC\", \"hyperliquid:ETH\"]\n\
             tau_min_ns = 2000000000\ntau_max_ns = 20000000000\np_min_1e6 = 900000\ne_1e6 = 20000\n\
             persist = 2\nsigma_k_1e3 = 1550\ndeposit_usd_1e6 = 5000000\nusd_market_1e6 = 10000000\n\
             session_loss_usd_1e6 = 50000000\n{extra}phi_lut = {}\n",
            phi()
        )
    }

    #[test]
    fn the_dust_frame_parses_with_its_stated_defaults() {
        let f = parse(&src("")).expect("valid");
        assert_eq!(f.families, vec!["btc", "eth"]);
        assert_eq!((f.price_floor_1e6, f.min_tau_s), (900_000, 3));
        assert_eq!((f.deposit_usd_1e6, f.max_order_usd_1e6, f.usd_market_1e6), (5_000_000, 5_010_000, 10_000_000));
        assert_eq!((f.max_orders_market, f.order_ttl_ns), (4, 5_000_000_000));
        assert_eq!((f.max_slippage_bps, f.paper_delay_ms), (0, 2_000));
        assert!(f.live_assets.is_empty());
        assert!(f.guard_programs.is_empty() && f.guard_alts.is_empty(), "refuse all by default");
        assert_eq!((f.guard_deposit_account.as_str(), f.guard_max_sol_lamports, f.guard_max_cu_price), ("", 0, 0));
        let g = parse(&src(
            "guard_programs = [\"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA\"]\n\
             guard_deposit_account = \"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v\"\n\
             guard_max_sol_lamports = 3000000\nguard_max_cu_price = 50000\nmax_order_usd_1e6 = 25000000\n\
             paper_delay_ms = 1500\nmin_tau_s = 4\nprice_floor_1e6 = 920000\nmax_slippage_bps = 50\n",
        ))
        .expect("valid");
        assert_eq!(g.guard_programs.len(), 1);
        let h = parse(&src("min_tau_s = 0\npaper_delay_ms = 1999\nmax_order_usd_1e6 = 5010000\n")).expect("valid");
        assert_eq!((h.min_tau_s, h.paper_delay_ms, h.max_order_usd_1e6), (0, 1_999, 5_010_000));
        assert_eq!((g.max_order_usd_1e6, g.paper_delay_ms, g.min_tau_s, g.price_floor_1e6), (25_000_000, 1_500, 4, 920_000));
    }

    #[test]
    fn every_bound_refuses() {
        let base = src("");
        for (from, to) in [
            ("deposit_usd_1e6 = 5000000", "deposit_usd_1e6 = 4999999"),
            ("usd_market_1e6 = 10000000", "usd_market_1e6 = 4000000"),
            ("tau_max_ns = 20000000000", "tau_max_ns = 2000000000"),
            ("p_min_1e6 = 900000", "p_min_1e6 = 400000"),
            ("persist = 2", "persist = 9"),
            ("session_loss_usd_1e6 = 50000000", "session_loss_usd_1e6 = 0"),
            ("families = [\"btc\", \"eth\"]", "families = [\"btc\", \"btc\"]"),
            ("perp = [\"hyperliquid:BTC\", \"hyperliquid:ETH\"]", "perp = [\"hyperliquid:BTC\"]"),
        ] {
            assert!(base.contains(from), "{from}");
            assert!(parse(&base.replacen(from, to, 1)).is_err(), "{to}");
        }
        for extra in [
            "price_floor_1e6 = 1000000\n",
            "min_tau_s = 20\n",
            "max_order_usd_1e6 = 4000000\n",
            "max_order_usd_1e6 = 5009999\n",
            "paper_delay_ms = 3000\n",
            "min_tau_s = 0\n",
            "order_ttl_ns = 100\n",
            "max_slippage_bps = 10001\n",
            "paper_delay_ms = 10000\n",
            "live_assets = [\"sol\"]\n",
            "guard_programs = [\"0OIl\"]\n",
            "guard_deposit_account = \"not-base58-0OIl-not-base58-0OIl-x\"\n",
            "cap_day_usd_1e6 = 1\n",
        ] {
            assert!(parse(&src(extra)).is_err(), "{extra}");
        }
    }

    /// A `pm15.toml` copied whole is not a `pmj.toml`: its section, its
    /// per-order size key (`usd_order_1e6`, not the deposit) and a missing
    /// deposit each refuse — the 5-minute artifact is derived, never aliased.
    #[test]
    fn a_pm15_artifact_is_not_a_pmj_one() {
        let base = src("");
        assert!(parse(&base.replacen("[pmj]", "[pm15]", 1)).is_err(), "the section");
        assert!(parse(&src("usd_order_1e6 = 250000000\n")).is_err(), "pm15's size key");
        assert!(parse(&base.replacen("deposit_usd_1e6 = 5000000\n", "", 1)).is_err(), "no deposit");
    }

    #[test]
    fn law_iii_parses_and_its_keys_need_it() {
        use crate::strike_law::{STRIKE_LAW_MEAN60, STRIKE_LAW_TRAP62};
        let f = parse(&src("")).expect("valid");
        assert_eq!(f.strike_law.law, STRIKE_LAW_MEAN60, "absent = the first law");
        let f = parse(&src("strike_law = \"mean60\"\n")).expect("valid");
        assert_eq!(f.strike_law, crate::strike_law::StrikeLaw::default());
        let f = parse(&src(
            "strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\nsigma_res_min_ppm = [3, 5]\n",
        ))
        .expect("valid");
        assert_eq!(
            (f.strike_law.law, f.strike_law.sigma_res_ratio_1e3, f.strike_law.sigma_res_min_ppm.clone()),
            (STRIKE_LAW_TRAP62, 130, vec![3, 5])
        );
        for (bad, why) in [
            ("strike_law = \"trap60\"\n", "an unknown law"),
            ("strike_law = 1\n", "not a string"),
            ("strike_law = \"trap62\"\n", "no residual"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\n", "no floors"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 0\nsigma_res_min_ppm = [3, 5]\n", "ratio 0"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 1001\nsigma_res_min_ppm = [3, 5]\n", "ratio > 1000"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\nsigma_res_min_ppm = [3]\n", "one floor for two families"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\nsigma_res_min_ppm = [3, 0]\n", "a floor of 0"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\nsigma_res_min_ppm = [3, 101]\n", "a floor over 100"),
            ("sigma_res_ratio_1e3 = 130\n", "law (iii)'s key under the first law"),
            ("strike_law = \"mean60\"\nsigma_res_min_ppm = [3, 5]\n", "law (iii)'s key under the first law"),
        ] {
            assert!(parse(&src(bad)).is_err(), "{why}: {bad}");
        }
    }
}
