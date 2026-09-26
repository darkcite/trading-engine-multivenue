// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `xmm.toml` — the slot-6 XMM member's artifact (plan
//! `xmm-hl-maker-plan` Appendix A).
//!
//! Integer-only TOML subset in the `bin15.rs` / `vrp.rs` style: one
//! `[xmm]` section through the shared single-section loop
//! ([`crate::icdp::parse_single_section`]), so every message an operator
//! sees reads like the other artifacts', and every bound checked at parse
//! rather than clamped at boot. The engine hashes the file BYTES, so the
//! boot tell names the artifact actually in force.
//!
//! **BOOT/OFFLINE DOCTRINE:** runs once at process boot (and once per
//! offline `backtest --member xmm`); allocations are fine. Nothing on the
//! engine loop can reach it: the member takes no `core-config` dependency.
//!
//! **Laws** (inherited, all FATAL, all naming `xmm.toml`): integers only ·
//! an unknown key refuses (a typo'd knob that silently took a default is
//! a member trading an edge nobody chose) · a duplicate key or section
//! refuses · EVERY key is required — nothing defaults silently · 0/1
//! flags only · each bound below refuses rather than clamps. The upper
//! bounds of θ and the clip catch a scale slip (a `_1e6` field written as
//! `_1e9`: the probe's $15 clip would read $15 000). The three money caps
//! are bounded by the largest configuration the plan names (the $5 000
//! parity clip): raising any ceiling is a scale step (XH7) — a code change
//! with its risk-policy entry, never an edit of the artifact alone. Their
//! cross-check against `exec.toml` slot 6 (E6: a mismatch refuses the
//! boot) lands with the slot's own arm (XH4).
//!
//! The perps are named by their keys (`quote_<coin>`, [`XMM_COINS`]):
//! the Hyperliquid perp `hyperliquid:<COIN>` is quoted and the Binance
//! USDⓈ-M perp `binance-usdm:<coin>usdt` leads it. Resolving those
//! descriptors against the boot universe is `cli::xmm_boot`'s job.

use crate::icdp::{parse_single_section, Value};
use std::path::Path;

/// Perps one artifact may configure. Mirrors
/// `strategy_xmm::XMM_MAX_PERPS` (const-asserted in the cli).
pub const XMM_MAX_PERPS: usize = 8;

/// LAW E-8 / XH-1: the longest quote lifetime, ms. Mirrors
/// `strategy_xmm::XMM_LIFETIME_MAX_MS`.
pub const XMM_LIFETIME_MAX_MS: i64 = 30_000;

/// The venue's minimum order notional, USD ×1e6. Mirrors
/// `strategy_xmm::XMM_MIN_CLIP_USD_1E6`.
pub const XMM_MIN_CLIP_USD_1E6: i64 = 10_000_000;

/// The highest `ab_mode`. Mirrors `strategy_xmm::XMM_AB_MODE_MAX`.
pub const XMM_AB_MODE_MAX: i64 = 3;

/// θ's upper bound, bps ×1e6 (10 bps). The measured θ is 0.25–0.5 bps;
/// anything near the bound is a scale slip, not a policy.
pub const XMM_THETA_MAX_BPS_1E6: i64 = 10_000_000;

/// Upper bound of every millisecond knob (one minute).
pub const XMM_MS_MAX: i64 = 60_000;

/// `alo_priority_1e8`'s upper bound: 1 % of resting notional. The XH5
/// A/B runs 1 bp (10 000).
pub const XMM_ALO_PRIORITY_MAX_1E8: i64 = 1_000_000;

/// The clip's ceiling, USD ×1e6 ($10 000): twice the $5 000 clip the
/// XH2 parity gate runs at, and below the $15 000 a probe clip written
/// at ×1e9 would read.
pub const XMM_CLIP_MAX_USD_1E6: i64 = 10_000_000_000;

/// The per-perp inventory cap's ceiling, USD ×1e6 ($5 000 000).
pub const XMM_INV_CAP_MAX_USD_1E6: i64 = 5_000_000_000_000;

/// The gross inventory cap's ceiling, USD ×1e6 ($20 000 000).
pub const XMM_GROSS_INV_CAP_MAX_USD_1E6: i64 = 20_000_000_000_000;

/// The resting-notional cap's ceiling, USD ×1e6 ($1 000 000) — already
/// six times what two quotes on each of eight perps at the clip ceiling
/// can rest.
pub const XMM_RESTING_CAP_MAX_USD_1E6: i64 = 1_000_000_000_000;

/// One perp the artifact can quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmmCoin {
    /// The enable key (`quote_btc`, …).
    pub key: &'static str,
    /// The Hyperliquid coin — the follower is `hyperliquid:<hl_coin>`.
    pub hl_coin: &'static str,
    /// The Binance USDⓈ-M stream symbol — the leader is
    /// `binance-usdm:<lead>`.
    pub lead: &'static str,
}

/// The perps XMM measured (plan §1.1), in key order.
pub const XMM_COINS: [XmmCoin; 7] = [
    XmmCoin { key: "quote_btc", hl_coin: "BTC", lead: "btcusdt" },
    XmmCoin { key: "quote_eth", hl_coin: "ETH", lead: "ethusdt" },
    XmmCoin { key: "quote_sol", hl_coin: "SOL", lead: "solusdt" },
    XmmCoin { key: "quote_xrp", hl_coin: "XRP", lead: "xrpusdt" },
    XmmCoin { key: "quote_ada", hl_coin: "ADA", lead: "adausdt" },
    XmmCoin { key: "quote_ltc", hl_coin: "LTC", lead: "ltcusdt" },
    XmmCoin { key: "quote_doge", hl_coin: "DOGE", lead: "dogeusdt" },
];

const _: () = assert!(XMM_COINS.len() <= XMM_MAX_PERPS);

/// A `xmm.toml` that could not be read or did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmmError(pub String);

impl std::fmt::Display for XmmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "xmm.toml: {}", self.0)
    }
}

impl std::error::Error for XmmError {}

fn err(msg: impl Into<String>) -> XmmError {
    XmmError(msg.into())
}

/// Every key the grammar accepts — and requires.
const XMM_KEYS: [&str; 22] = [
    "maker_enabled",
    "quote_btc",
    "quote_eth",
    "quote_sol",
    "quote_xrp",
    "quote_ada",
    "quote_ltc",
    "quote_doge",
    "theta_bps_1e6",
    "gate_window_ms",
    "lifetime_ms",
    "clip_usd_1e6",
    "inv_cap_usd_1e6",
    "gross_inv_cap_usd_1e6",
    "resting_cap_usd_1e6",
    "lead_stale_ms",
    "follower_stale_ms",
    "rtt_pull_ms",
    "requote_min_ms",
    "skew_kappa_1e6",
    "alo_priority_1e8",
    "ab_mode",
];

/// The parsed artifact. Every field is a key of the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmmFile {
    /// Master switch (the ablation pattern).
    pub maker_enabled: bool,
    /// `quote_<coin>`, indexed like [`XMM_COINS`].
    pub quote: [bool; XMM_COINS.len()],
    /// θ, bps ×1e6.
    pub theta_bps_1e6: i64,
    /// The gate's window, ms.
    pub gate_window_ms: u32,
    /// Quote lifetime cap, ms.
    pub lifetime_ms: u32,
    /// Order size, USD ×1e6.
    pub clip_usd_1e6: i64,
    /// Per-perp inventory cap at mark, USD ×1e6.
    pub inv_cap_usd_1e6: i64,
    /// Gross inventory cap, USD ×1e6.
    pub gross_inv_cap_usd_1e6: i64,
    /// Resting-notional cap, USD ×1e6.
    pub resting_cap_usd_1e6: i64,
    /// Leader staleness that pulls both sides, ms (local clock).
    pub lead_stale_ms: u32,
    /// Follower staleness that pulls both sides, ms.
    pub follower_stale_ms: u32,
    /// ACK RTT p99 that pulls both sides, ms.
    pub rtt_pull_ms: u32,
    /// Minimum quote life before a price requote, ms.
    pub requote_min_ms: u32,
    /// Inventory skew κ ×1e6 — refused unless 0 until XH7.
    pub skew_kappa_1e6: i64,
    /// ALO priority rate (p / 1e8 of resting notional).
    pub alo_priority_1e8: u32,
    /// A/B selector.
    pub ab_mode: u8,
}

impl XmmFile {
    /// The perps the artifact quotes, in [`XMM_COINS`] order.
    pub fn quoted(&self) -> impl Iterator<Item = &'static XmmCoin> + '_ {
        XMM_COINS
            .iter()
            .zip(self.quote.iter())
            .filter(|(_, on)| **on)
            .map(|(c, _)| c)
    }
}

/// Read and parse the artifact, returning it with its RAW BYTES so the
/// caller can hash exactly what it read.
pub fn load(path: &Path) -> Result<(XmmFile, Vec<u8>), XmmError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|e| err(format!("{}: not UTF-8 ({e})", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

/// The default artifact path: `~/multivenue/xmm.toml`.
pub fn default_xmm_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/xmm.toml")
}

type Kv = [(String, Value, usize)];

fn int(kv: &Kv, key: &str) -> Result<i64, XmmError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => Ok(*v),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Err(err(format!("missing `{key}`"))),
    }
}

/// One 0/1 flag. "2" in a boolean field means the operator believed
/// something the grammar does not say.
fn flag(kv: &Kv, key: &str) -> Result<bool, XmmError> {
    match int(kv, key)? {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(err(format!("`{key}` must be 0 or 1 (got {v})"))),
    }
}

/// An integer inside `[lo, hi]`.
fn bounded(kv: &Kv, key: &str, lo: i64, hi: i64) -> Result<i64, XmmError> {
    let v = int(kv, key)?;
    if v < lo || v > hi {
        return Err(err(format!("`{key}` must be in [{lo}, {hi}] (got {v})")));
    }
    Ok(v)
}

/// A millisecond knob inside `[lo, XMM_MS_MAX]`, as `u32`.
fn ms(kv: &Kv, key: &str, lo: i64) -> Result<u32, XmmError> {
    let v = bounded(kv, key, lo, XMM_MS_MAX)?;
    u32::try_from(v).map_err(|_| err(format!("`{key}` does not fit u32 (got {v})")))
}

/// Parse the artifact text.
pub fn parse(src: &str) -> Result<XmmFile, XmmError> {
    let kv = parse_single_section(src, "xmm", &XMM_KEYS).map_err(|e| err(e.0))?;

    let mut quote = [false; XMM_COINS.len()];
    let mut i = 0usize;
    while i < XMM_COINS.len() {
        quote[i] = flag(&kv, XMM_COINS[i].key)?;
        i += 1;
    }
    if !quote.iter().any(|q| *q) {
        return Err(err(
            "no perp is quoted — set at least one `quote_<coin> = 1` \
             (`maker_enabled = 0` is the switch that stops quoting)",
        ));
    }

    let clip_usd_1e6 = bounded(&kv, "clip_usd_1e6", XMM_MIN_CLIP_USD_1E6, XMM_CLIP_MAX_USD_1E6)?;
    let inv_cap_usd_1e6 =
        bounded(&kv, "inv_cap_usd_1e6", clip_usd_1e6, XMM_INV_CAP_MAX_USD_1E6)?;
    let gross_inv_cap_usd_1e6 = bounded(
        &kv,
        "gross_inv_cap_usd_1e6",
        inv_cap_usd_1e6,
        XMM_GROSS_INV_CAP_MAX_USD_1E6,
    )?;
    let resting_cap_usd_1e6 =
        bounded(&kv, "resting_cap_usd_1e6", clip_usd_1e6, XMM_RESTING_CAP_MAX_USD_1E6)?;

    let skew_kappa_1e6 = int(&kv, "skew_kappa_1e6")?;
    if skew_kappa_1e6 != 0 {
        return Err(err(format!(
            "`skew_kappa_1e6` must be 0 until XH7 — the simulation the member \
             is measured against had no skew (got {skew_kappa_1e6})"
        )));
    }

    let alo = bounded(&kv, "alo_priority_1e8", 0, XMM_ALO_PRIORITY_MAX_1E8)?;
    let ab = bounded(&kv, "ab_mode", 0, XMM_AB_MODE_MAX)?;

    Ok(XmmFile {
        maker_enabled: flag(&kv, "maker_enabled")?,
        quote,
        theta_bps_1e6: bounded(&kv, "theta_bps_1e6", 1, XMM_THETA_MAX_BPS_1E6)?,
        gate_window_ms: ms(&kv, "gate_window_ms", 1)?,
        lifetime_ms: {
            let v = bounded(&kv, "lifetime_ms", 1, XMM_LIFETIME_MAX_MS)?;
            u32::try_from(v).map_err(|_| err("`lifetime_ms` does not fit u32"))?
        },
        clip_usd_1e6,
        inv_cap_usd_1e6,
        gross_inv_cap_usd_1e6,
        resting_cap_usd_1e6,
        lead_stale_ms: ms(&kv, "lead_stale_ms", 1)?,
        follower_stale_ms: ms(&kv, "follower_stale_ms", 1)?,
        rtt_pull_ms: ms(&kv, "rtt_pull_ms", 1)?,
        requote_min_ms: ms(&kv, "requote_min_ms", 0)?,
        skew_kappa_1e6,
        alo_priority_1e8: u32::try_from(alo)
            .map_err(|_| err("`alo_priority_1e8` does not fit u32"))?,
        ab_mode: u8::try_from(ab).map_err(|_| err("`ab_mode` does not fit u8"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../xmm.toml.example");

    /// Replace one `key = value` line of the example.
    fn with(key: &str, value: &str) -> String {
        let mut out = String::new();
        let mut hit = false;
        for line in EXAMPLE.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")) {
                out.push_str(&format!("{key} = {value}\n"));
                hit = true;
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        assert!(hit, "the example must carry `{key}`");
        out
    }

    /// The example without one key's line.
    fn without(key: &str) -> String {
        let mut out = String::new();
        for line in EXAMPLE.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")) {
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// The shipped example is the probe artifact (plan Appendix A,
    /// O-XH5 caps, O-XH10 coins).
    #[test]
    fn the_example_parses_to_the_probe_artifact() {
        let f = parse(EXAMPLE).expect("the example is the grammar's contract");
        assert!(f.maker_enabled);
        assert_eq!(f.quote, [true, true, true, true, false, false, false]);
        let coins: Vec<&str> = f.quoted().map(|c| c.hl_coin).collect();
        assert_eq!(coins, ["BTC", "ETH", "SOL", "XRP"]);
        assert_eq!(f.theta_bps_1e6, 500_000);
        assert_eq!(f.gate_window_ms, 500);
        assert_eq!(f.lifetime_ms, 30_000);
        assert_eq!(f.clip_usd_1e6, 15_000_000);
        assert_eq!(f.inv_cap_usd_1e6, 150_000_000);
        assert_eq!(f.gross_inv_cap_usd_1e6, 400_000_000);
        assert_eq!(f.resting_cap_usd_1e6, 300_000_000);
        assert_eq!(f.lead_stale_ms, 300);
        assert_eq!(f.follower_stale_ms, 2_000);
        assert_eq!(f.rtt_pull_ms, 1_500);
        assert_eq!(f.skew_kappa_1e6, 0);
        assert_eq!(f.alo_priority_1e8, 0);
        assert_eq!(f.ab_mode, 0);
    }

    #[test]
    fn coin_table_names_each_leader_once() {
        let mut i = 0usize;
        while i < XMM_COINS.len() {
            let c = XMM_COINS[i];
            assert_eq!(c.key, format!("quote_{}", c.hl_coin.to_lowercase()));
            assert_eq!(c.lead, format!("{}usdt", c.hl_coin.to_lowercase()));
            assert!(XMM_KEYS.contains(&c.key));
            i += 1;
        }
    }

    #[test]
    fn every_key_is_required() {
        let mut i = 0usize;
        while i < XMM_KEYS.len() {
            let e = parse(&without(XMM_KEYS[i])).expect_err("a missing key refuses");
            assert!(e.0.contains(XMM_KEYS[i]), "{} → {}", XMM_KEYS[i], e.0);
            i += 1;
        }
    }

    #[test]
    fn unknown_key_section_and_duplicates_refuse() {
        let e = parse(&format!("{EXAMPLE}\ntheta_bps = 1\n")).expect_err("unknown key");
        assert!(e.0.contains("unknown key"), "{}", e.0);
        let e = parse(&format!("{EXAMPLE}\n[xmm2]\n")).expect_err("unknown section");
        assert!(e.0.contains("unknown section"), "{}", e.0);
        let e = parse(&format!("{EXAMPLE}\nab_mode = 0\n")).expect_err("duplicate key");
        assert!(e.0.contains("duplicate"), "{}", e.0);
        let e = parse("").expect_err("no section");
        assert!(e.0.contains("[xmm]"), "{}", e.0);
    }

    #[test]
    fn non_integers_and_bad_flags_refuse() {
        assert!(parse(&with("theta_bps_1e6", "0.5")).is_err(), "a float refuses");
        assert!(parse(&with("theta_bps_1e6", "\"1\"")).is_err(), "a string refuses");
        assert!(parse(&with("maker_enabled", "2")).is_err());
        assert!(parse(&with("quote_btc", "-1")).is_err());
    }

    #[test]
    fn bounds_refuse_rather_than_clamp() {
        let bad = [
            ("theta_bps_1e6", "0"),
            ("theta_bps_1e6", "10000001"),
            ("gate_window_ms", "0"),
            ("gate_window_ms", "60001"),
            ("lifetime_ms", "0"),
            ("lifetime_ms", "30001"),
            ("clip_usd_1e6", "9999999"),
            // The probe's $15 clip written at ×1e9: $15 000.
            ("clip_usd_1e6", "15000000000"),
            ("inv_cap_usd_1e6", "14999999"),
            ("inv_cap_usd_1e6", "5000000000001"),
            ("gross_inv_cap_usd_1e6", "149999999"),
            ("gross_inv_cap_usd_1e6", "20000000000001"),
            ("resting_cap_usd_1e6", "14999999"),
            ("resting_cap_usd_1e6", "1000000000001"),
            ("lead_stale_ms", "0"),
            ("follower_stale_ms", "0"),
            ("rtt_pull_ms", "0"),
            ("requote_min_ms", "-1"),
            ("requote_min_ms", "60001"),
            ("skew_kappa_1e6", "1"),
            ("alo_priority_1e8", "-1"),
            ("alo_priority_1e8", "1000001"),
            ("ab_mode", "4"),
        ];
        let mut i = 0usize;
        while i < bad.len() {
            let (k, v) = bad[i];
            assert!(parse(&with(k, v)).is_err(), "`{k} = {v}` must refuse");
            i += 1;
        }
        // The edges themselves are legal.
        assert!(parse(&with("lifetime_ms", "30000")).is_ok());
        assert!(parse(&with("resting_cap_usd_1e6", "1000000000000")).is_ok());
        assert!(parse(&with("gross_inv_cap_usd_1e6", "20000000000000")).is_ok());
        // Every money ceiling at once — the largest artifact the law admits.
        let mut top = with("clip_usd_1e6", "10000000000");
        for (k, v) in [
            ("inv_cap_usd_1e6", "5000000000000"),
            ("gross_inv_cap_usd_1e6", "20000000000000"),
            ("resting_cap_usd_1e6", "1000000000000"),
        ] {
            top = top
                .lines()
                .map(|l| {
                    if l.starts_with(&format!("{k} ")) {
                        format!("{k} = {v}\n")
                    } else {
                        format!("{l}\n")
                    }
                })
                .collect();
        }
        let f = parse(&top).expect("every ceiling is itself legal");
        assert_eq!(f.clip_usd_1e6, XMM_CLIP_MAX_USD_1E6);
        assert_eq!(f.inv_cap_usd_1e6, XMM_INV_CAP_MAX_USD_1E6);
        assert_eq!(f.gross_inv_cap_usd_1e6, XMM_GROSS_INV_CAP_MAX_USD_1E6);
        assert_eq!(f.resting_cap_usd_1e6, XMM_RESTING_CAP_MAX_USD_1E6);
        assert!(parse(&with("requote_min_ms", "0")).is_ok());
        assert!(parse(&with("ab_mode", "3")).is_ok());
        assert!(parse(&with("alo_priority_1e8", "10000")).is_ok());
    }

    #[test]
    fn an_artifact_that_quotes_nothing_refuses() {
        let mut src = EXAMPLE.to_owned();
        let mut i = 0usize;
        while i < XMM_COINS.len() {
            src = src.replace(&format!("{} = 1", XMM_COINS[i].key), &format!("{} = 0", XMM_COINS[i].key));
            i += 1;
        }
        let e = parse(&src).expect_err("nothing quoted");
        assert!(e.0.contains("no perp is quoted"), "{}", e.0);
    }

    #[test]
    fn load_hashes_what_it_read_and_refuses_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("xmm-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("xmm.toml");
        std::fs::write(&p, EXAMPLE).unwrap();
        let (f, bytes) = load(&p).expect("loads");
        assert_eq!(bytes, EXAMPLE.as_bytes());
        assert!(f.maker_enabled);
        let e = load(&dir.join("absent.toml")).expect_err("absent");
        assert!(e.0.contains("absent.toml"), "{}", e.0);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(default_xmm_path().unwrap().ends_with("multivenue/xmm.toml"));
    }
}
