//! # universe_boot — flag/config resolution for the M1 boot universe
//!
//! BOOT DOCTRINE: runs once in the bin before any thread spawns;
//! allocations permitted. Pure logic — file IO stays in the one thin
//! [`read_universe_source`] helper so [`resolve_boot_universe`] is
//! fully unit-testable.
//!
//! Precedence law (docs/mvp-progress.md M1a design):
//! - No config source ⇒ **legacy mode**, byte-identical to the pre-M1
//!   flag-driven boot: `--polymarket-asset-id` REQUIRED, Binance spot
//!   defaults `btcusdt`, legacy anchors 42/7 (or the `*-sym-id`
//!   flags), venue specs from their flags.
//! - Config source present ⇒ the file drives; per-venue CLI flags
//!   OVERRIDE that venue's section when explicitly passed. A PM/BN
//!   single-value override replaces that venue's list with the
//!   one-element list (and drops the config `[pairs]` map — the
//!   default pair (0,0) applies; explicit pair indices were validated
//!   against the replaced lists).
//! - `--polymarket-sym-id` / `--binance-sym-id` with an active config
//!   require their market/symbol flag — a bare id flag would silently
//!   re-anchor a config list, which is exactly the ambiguity M1 bans.

use std::path::Path;

use core_config::universe;

/// Everything the resolver needs from the CLI surface. `config_src`
/// is the universe file CONTENT (already read) — `None` = legacy
/// flag-driven boot.
#[derive(Debug, Default)]
pub struct UniverseFlags<'a> {
    /// Universe file content, when a config drives the boot.
    pub config_src: Option<&'a str>,
    /// `--polymarket-asset-id` (legacy-required; config-mode override).
    pub pm_asset_id: Option<&'a str>,
    /// `--polymarket-sym-id` (None = legacy anchor 42).
    pub pm_sym_id: Option<u32>,
    /// `--binance-symbol` (None = legacy `btcusdt` / config list).
    pub bn_symbol: Option<&'a str>,
    /// `--binance-sym-id` (None = legacy anchor 7).
    pub bn_sym_id: Option<u32>,
    /// `--okx-symbols` comma spec.
    pub okx_symbols: Option<&'a str>,
    /// `--deribit-symbols` comma spec.
    pub deribit_symbols: Option<&'a str>,
    /// `--hl-coins` comma spec.
    pub hl_coins: Option<&'a str>,
    /// `--okx-depth` flag.
    pub okx_depth: bool,
    /// `--deribit-depth` flag.
    pub deribit_depth: bool,
}

/// The resolved boot universe. `allocated` carries the PM tokens,
/// Binance spot/usdm instruments and latency-arb pairs (the M1 id
/// law applied); OKX/Deribit/HL stay STRING SPECS feeding the
/// existing discovery/table machinery — their ids keep coming from
/// the venue tables exactly as before M1 (same arithmetic, one
/// owner).
#[derive(Debug)]
pub struct BootUniverse {
    /// PM + Binance ids/descriptors + pairs per the M1 law.
    pub allocated: universe::AllocatedUniverse,
    /// OKX comma spec for discovery/spawn (None = venue off).
    pub okx_spec: Option<String>,
    /// Deribit comma spec (None = venue off).
    pub deribit_spec: Option<String>,
    /// Hyperliquid comma spec (None = venue off).
    pub hl_spec: Option<String>,
    /// Effective OKX depth-channel toggle (flag OR config).
    pub okx_depth: bool,
    /// Effective Deribit depth-channel toggle (flag OR config).
    pub deribit_depth: bool,
    /// M2.1 capped options-chain policy for Deribit (config-file
    /// only; disabled by default — legacy boots stay byte-identical).
    /// An explicit `--deribit-symbols` override replaces the whole
    /// `[deribit]` section per the M1a override law, dropping the
    /// policy (see [`BootUniverse::deribit_options_dropped`]).
    pub deribit_options: universe::OptionsPolicy,
    /// True when `--deribit-symbols` dropped an ENABLED config
    /// options policy — the bin logs the consequence.
    pub deribit_options_dropped: bool,
    /// True when a universe config file drove this resolution.
    pub from_config: bool,
}

/// Read the universe file: an explicit `--universe <path>` must
/// exist (fatal otherwise); with no flag, the default path
/// (`~/multivenue/universe.toml`) is used IF present, else legacy
/// mode. Returns the file content, or `None` for legacy.
pub fn read_universe_source(flag: Option<&Path>) -> Result<Option<String>, String> {
    match flag {
        Some(p) => std::fs::read_to_string(p)
            .map(Some)
            .map_err(|e| format!("--universe {}: {e}", p.display())),
        None => {
            let def = universe::default_universe_path()
                .map_err(|e| format!("default universe path: {e}"))?;
            match std::fs::read_to_string(&def) {
                Ok(s) => Ok(Some(s)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(format!("universe config {def}: {e}")),
            }
        }
    }
}

fn join_or_none(list: &[String]) -> Option<String> {
    if list.is_empty() {
        None
    } else {
        Some(list.join(","))
    }
}

fn flag_spec(s: Option<&str>) -> Option<String> {
    match s.map(str::trim) {
        Some(t) if !t.is_empty() => Some(t.to_string()),
        _ => None,
    }
}

/// Resolve the boot universe from flags (+ optional config content).
/// See the module docs for the precedence law. Errors are operator
/// messages — the bin logs and exits non-zero.
pub fn resolve_boot_universe(f: &UniverseFlags<'_>) -> Result<BootUniverse, String> {
    let pm_anchor = f.pm_sym_id.unwrap_or(universe::LEGACY_PM_ANCHOR_SYM);
    let bn_anchor = f.bn_sym_id.unwrap_or(universe::LEGACY_BN_ANCHOR_SYM);

    let out = match f.config_src {
        None => {
            // ---- Legacy flag-driven boot (byte-identical pre-M1) ----
            let pm_id = f.pm_asset_id.map(str::trim).filter(|s| !s.is_empty()).ok_or(
                "no universe config found and --polymarket-asset-id not set — \
                 boot refuses to run venue-blind (provide the flag or \
                 ~/multivenue/universe.toml)",
            )?;
            let bn = f.bn_symbol.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("btcusdt");
            let u = universe::Universe {
                pm_markets: vec![universe::PmMarket::Single(pm_id.to_string())],
                binance_spot: vec![bn.to_string()],
                ..universe::Universe::default()
            };
            let allocated = universe::allocate_with_anchors(&u, pm_anchor, bn_anchor)
                .map_err(|e| e.to_string())?;
            BootUniverse {
                allocated,
                okx_spec: flag_spec(f.okx_symbols),
                deribit_spec: flag_spec(f.deribit_symbols),
                hl_spec: flag_spec(f.hl_coins),
                okx_depth: f.okx_depth,
                deribit_depth: f.deribit_depth,
                deribit_options: universe::OptionsPolicy::default(),
                deribit_options_dropped: false,
                from_config: false,
            }
        }
        Some(src) => {
            // ---- Config-driven boot + per-venue flag overrides ----
            if f.pm_sym_id.is_some() && f.pm_asset_id.is_none() {
                return Err(
                    "--polymarket-sym-id requires --polymarket-asset-id when a \
                     universe config is active"
                        .to_string(),
                );
            }
            if f.bn_sym_id.is_some() && f.bn_symbol.is_none() {
                return Err(
                    "--binance-sym-id requires --binance-symbol when a universe \
                     config is active"
                        .to_string(),
                );
            }
            let mut u = universe::parse(src).map_err(|e| e.to_string())?;
            let mut pairs_dropped = false;
            if let Some(pm_id) = f.pm_asset_id.map(str::trim).filter(|s| !s.is_empty()) {
                u.pm_markets = vec![universe::PmMarket::Single(pm_id.to_string())];
                pairs_dropped = true;
            }
            if let Some(bn) = f.bn_symbol.map(str::trim).filter(|s| !s.is_empty()) {
                u.binance_spot = vec![bn.to_string()];
                pairs_dropped = true;
            }
            if pairs_dropped {
                // Config pair indices were validated against the
                // replaced lists; the override lane takes the default
                // pair (0,0) injected by allocation.
                u.pairs.clear();
            }
            let allocated = universe::allocate_with_anchors(&u, pm_anchor, bn_anchor)
                .map_err(|e| e.to_string())?;
            universe::assert_bootable(&allocated).map_err(|e| e.to_string())?;
            // M1a override law extended (M2.1): an explicit
            // --deribit-symbols replaces the whole [deribit] section,
            // options policy included.
            let deribit_flag = flag_spec(f.deribit_symbols);
            let (deribit_options, deribit_options_dropped) = if deribit_flag.is_some() {
                (
                    universe::OptionsPolicy::default(),
                    u.deribit_options.enabled(),
                )
            } else {
                (u.deribit_options.clone(), false)
            };
            BootUniverse {
                okx_spec: flag_spec(f.okx_symbols).or_else(|| join_or_none(&u.okx_instruments)),
                deribit_spec: deribit_flag.or_else(|| join_or_none(&u.deribit_instruments)),
                hl_spec: flag_spec(f.hl_coins).or_else(|| join_or_none(&u.hl_coins)),
                okx_depth: f.okx_depth || u.okx_depth,
                deribit_depth: f.deribit_depth || u.deribit_depth,
                deribit_options,
                deribit_options_dropped,
                allocated,
                from_config: true,
            }
        }
    };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: &str = "57748138085022719760345772310040703848567377822400132842014290209986511882046";
    const T2: &str = "1234567890123456789";

    fn cfg_src() -> String {
        format!(
            "[polymarket]\nmarkets = [\"{T1}\", \"{T2}\"]\n\
             [binance]\nspot = [\"btcusdt\", \"ethusdt\"]\nusdm = [\"btcusdt\"]\n\
             [okx]\ninstruments = [\"BTC-USDT\", \"ETH-USDT-SWAP\"]\ndepth = true\n\
             [deribit]\ninstruments = [\"BTC-PERPETUAL\"]\n\
             [hyperliquid]\ncoins = [\"BTC\"]\n\
             [pairs]\nmap = [\"1:1\"]\n"
        )
    }

    #[test]
    fn legacy_defaults_match_pre_m1_boot() {
        let f = UniverseFlags {
            pm_asset_id: Some(T1),
            ..UniverseFlags::default()
        };
        let b = resolve_boot_universe(&f).expect("legacy resolves");
        assert!(!b.from_config);
        assert_eq!(b.allocated.pm_tokens.len(), 1);
        assert_eq!(b.allocated.pm_tokens[0].sym, 42);
        assert_eq!(b.allocated.bn_spot.len(), 1);
        assert_eq!(b.allocated.bn_spot[0].sym, 7);
        assert_eq!(b.allocated.bn_spot[0].name, "btcusdt");
        assert!(b.allocated.bn_usdm.is_empty());
        assert_eq!(b.allocated.pairs, vec![(42, 7)]);
        assert_eq!(b.okx_spec, None);
        assert_eq!(b.deribit_spec, None);
        assert_eq!(b.hl_spec, None);
    }

    #[test]
    fn legacy_without_pm_id_refuses_venue_blind() {
        let f = UniverseFlags::default();
        let e = resolve_boot_universe(&f).unwrap_err();
        assert!(e.contains("venue-blind"), "{e}");
    }

    #[test]
    fn legacy_honors_sym_id_and_symbol_flags() {
        let f = UniverseFlags {
            pm_asset_id: Some(T1),
            pm_sym_id: Some(99),
            bn_symbol: Some("ethusdt"),
            bn_sym_id: Some(1234),
            okx_symbols: Some("BTC-USDT"),
            okx_depth: true,
            ..UniverseFlags::default()
        };
        let b = resolve_boot_universe(&f).unwrap();
        assert_eq!(b.allocated.pm_tokens[0].sym, 99);
        assert_eq!(b.allocated.bn_spot[0].sym, 1234);
        assert_eq!(b.allocated.bn_spot[0].name, "ethusdt");
        assert_eq!(b.allocated.pairs, vec![(99, 1234)]);
        assert_eq!(b.okx_spec.as_deref(), Some("BTC-USDT"));
        assert!(b.okx_depth);
    }

    #[test]
    fn config_drives_full_universe() {
        let src = cfg_src();
        let f = UniverseFlags {
            config_src: Some(&src),
            ..UniverseFlags::default()
        };
        let b = resolve_boot_universe(&f).unwrap();
        assert!(b.from_config);
        assert_eq!(b.allocated.pm_tokens.len(), 2);
        assert_eq!(b.allocated.pm_tokens[0].sym, 42);
        assert_eq!(b.allocated.bn_spot.len(), 2);
        assert_eq!(b.allocated.bn_spot[0].sym, 7);
        assert_eq!(b.allocated.bn_usdm.len(), 1);
        // Explicit config pair 1:1 resolved to allocated syms.
        assert_eq!(b.allocated.pairs.len(), 1);
        assert_eq!(b.allocated.pairs[0].0, b.allocated.pm_tokens[1].sym);
        assert_eq!(b.allocated.pairs[0].1, b.allocated.bn_spot[1].sym);
        assert_eq!(b.okx_spec.as_deref(), Some("BTC-USDT,ETH-USDT-SWAP"));
        assert_eq!(b.deribit_spec.as_deref(), Some("BTC-PERPETUAL"));
        assert_eq!(b.hl_spec.as_deref(), Some("BTC"));
        assert!(b.okx_depth, "config depth=true flows through");
        assert!(!b.deribit_depth);
    }

    #[test]
    fn pm_flag_overrides_config_and_drops_config_pairs() {
        let src = cfg_src();
        let f = UniverseFlags {
            config_src: Some(&src),
            pm_asset_id: Some(T2),
            ..UniverseFlags::default()
        };
        let b = resolve_boot_universe(&f).unwrap();
        assert_eq!(b.allocated.pm_tokens.len(), 1);
        assert_eq!(b.allocated.pm_tokens[0].token_id, T2);
        assert_eq!(b.allocated.pm_tokens[0].sym, 42);
        // Config [pairs] "1:1" would dangle — dropped for the default.
        assert_eq!(b.allocated.pairs, vec![(42, 7)]);
        // Non-overridden venues keep the config lists.
        assert_eq!(b.allocated.bn_spot.len(), 2);
        assert_eq!(b.okx_spec.as_deref(), Some("BTC-USDT,ETH-USDT-SWAP"));
    }

    #[test]
    fn venue_flag_overrides_config_spec() {
        let src = cfg_src();
        let f = UniverseFlags {
            config_src: Some(&src),
            okx_symbols: Some("SOL-USDT"),
            ..UniverseFlags::default()
        };
        let b = resolve_boot_universe(&f).unwrap();
        assert_eq!(b.okx_spec.as_deref(), Some("SOL-USDT"));
        assert_eq!(b.deribit_spec.as_deref(), Some("BTC-PERPETUAL"));
    }

    #[test]
    fn bare_sym_id_flag_with_config_is_an_error() {
        let src = cfg_src();
        let f = UniverseFlags {
            config_src: Some(&src),
            pm_sym_id: Some(5),
            ..UniverseFlags::default()
        };
        let e = resolve_boot_universe(&f).unwrap_err();
        assert!(e.contains("--polymarket-sym-id requires"), "{e}");
        let f2 = UniverseFlags {
            config_src: Some(&src),
            bn_sym_id: Some(5),
            ..UniverseFlags::default()
        };
        let e2 = resolve_boot_universe(&f2).unwrap_err();
        assert!(e2.contains("--binance-sym-id requires"), "{e2}");
    }

    #[test]
    fn config_without_pm_refuses_venue_blind() {
        let src = "[binance]\nspot = [\"btcusdt\"]\n";
        let f = UniverseFlags {
            config_src: Some(src),
            ..UniverseFlags::default()
        };
        let e = resolve_boot_universe(&f).unwrap_err();
        assert!(e.contains("venue-blind"), "{e}");
    }

    #[test]
    fn config_without_bn_spot_refuses_pair_anchor() {
        let src = format!("[polymarket]\nmarkets = [\"{T1}\"]\n");
        let f = UniverseFlags {
            config_src: Some(&src),
            ..UniverseFlags::default()
        };
        let e = resolve_boot_universe(&f).unwrap_err();
        assert!(e.contains("latency-arb pair anchor"), "{e}");
    }

    #[test]
    fn config_parse_error_surfaces_with_line() {
        let src = "[binanse]\n";
        let f = UniverseFlags {
            config_src: Some(src),
            ..UniverseFlags::default()
        };
        let e = resolve_boot_universe(&f).unwrap_err();
        assert!(e.contains("line 1"), "{e}");
    }

    #[test]
    fn read_universe_source_explicit_missing_is_fatal() {
        let e = read_universe_source(Some(Path::new("/nonexistent/m1-universe.toml")))
            .unwrap_err();
        assert!(e.contains("--universe"), "{e}");
    }

    // ---- M2.1 options policy through the resolver ------------------

    fn cfg_src_with_options() -> String {
        format!(
            "{}[deribit]\ninstruments = [\"BTC-PERPETUAL\"]\n\
             options_underlyings = [\"BTC\", \"ETH\"]\n\
             options_expiries = 2\noptions_strikes = 8\n",
            cfg_src_no_deribit()
        )
    }

    fn cfg_src_no_deribit() -> String {
        format!(
            "[polymarket]\nmarkets = [\"{T1}\"]\n[binance]\nspot = [\"btcusdt\"]\n"
        )
    }

    #[test]
    fn config_options_policy_carried_through() {
        let src = cfg_src_with_options();
        let f = UniverseFlags {
            config_src: Some(&src),
            ..UniverseFlags::default()
        };
        let b = resolve_boot_universe(&f).expect("resolves");
        assert!(b.deribit_options.enabled());
        assert_eq!(b.deribit_options.underlyings, vec!["BTC", "ETH"]);
        assert_eq!(b.deribit_options.expiries, 2);
        assert_eq!(b.deribit_options.strikes, 8);
        assert!(!b.deribit_options_dropped);
        assert_eq!(b.deribit_spec.as_deref(), Some("BTC-PERPETUAL"));
    }

    #[test]
    fn deribit_flag_override_drops_config_options_policy() {
        let src = cfg_src_with_options();
        let f = UniverseFlags {
            config_src: Some(&src),
            deribit_symbols: Some("ETH-PERPETUAL"),
            ..UniverseFlags::default()
        };
        let b = resolve_boot_universe(&f).expect("resolves");
        assert!(!b.deribit_options.enabled());
        assert!(b.deribit_options_dropped); // bin logs the consequence
        assert_eq!(b.deribit_spec.as_deref(), Some("ETH-PERPETUAL"));
        // Flag override with NO enabled config policy: nothing to drop.
        let src2 = format!("{}[deribit]\ninstruments = [\"BTC-PERPETUAL\"]\n", cfg_src_no_deribit());
        let f2 = UniverseFlags {
            config_src: Some(&src2),
            deribit_symbols: Some("ETH-PERPETUAL"),
            ..UniverseFlags::default()
        };
        let b2 = resolve_boot_universe(&f2).expect("resolves");
        assert!(!b2.deribit_options_dropped);
    }

    #[test]
    fn legacy_boot_options_policy_disabled() {
        let f = UniverseFlags {
            pm_asset_id: Some(T1),
            ..UniverseFlags::default()
        };
        let b = resolve_boot_universe(&f).expect("resolves");
        assert!(!b.deribit_options.enabled());
        assert!(!b.deribit_options_dropped);
    }
}
