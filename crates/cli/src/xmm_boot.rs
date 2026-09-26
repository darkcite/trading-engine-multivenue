// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! XMM XH1 boot: `xmm.toml` → `strategy_xmm::XmmParams`.
//!
//! COPY-DOCTRINE: boot-only module (runs once before the engine loop, and
//! once per offline `backtest --member xmm`; allocates and copies freely).
//!
//! The member takes no `core-config` dependency, so this is where the
//! artifact becomes its configuration: each quoted perp's Hyperliquid
//! descriptor (`hyperliquid:<COIN>`) and its Binance USDⓈ-M leader
//! (`binance-usdm:<coin>usdt`) resolve against the SAME table the ruleset
//! validator uses — the boot universe live, the capture's newest manifest
//! offline.
//!
//! Laws, from the other lanes verbatim:
//!
//! * **Requested-but-absent REFUSES** (the icdp/F19 law) — the caller
//!   turns `Ok(None)` into a refusal when slot 6 was requested.
//! * **Present-and-unreadable refuses too**, and so does a descriptor the
//!   universe does not list: a perp nobody subscribed, or a leader nobody
//!   streams, is a member quoting blind.

use std::path::{Path, PathBuf};

use core_config::xmm::XmmFile;
use core_types::SymbolId;
use strategy_xmm::{XmmParams, XmmPerp};
use tracing::info;

// The artifact's bounds are the member's: one statement of each, held
// together here because the member cannot see `core-config`.
const _: () = assert!(core_config::xmm::XMM_MAX_PERPS == strategy_xmm::XMM_MAX_PERPS);
const _: () =
    assert!(core_config::xmm::XMM_LIFETIME_MAX_MS == strategy_xmm::XMM_LIFETIME_MAX_MS as i64);
const _: () = assert!(core_config::xmm::XMM_MIN_CLIP_USD_1E6 == strategy_xmm::XMM_MIN_CLIP_USD_1E6);
const _: () = assert!(core_config::xmm::XMM_AB_MODE_MAX == strategy_xmm::XMM_AB_MODE_MAX as i64);
// XMM XH2: the queue law's tables hold every order the member can have
// out — each perp tracked, and two sides × (an order + a modify's
// predecessor) per perp — so an eighth perp can never meet a full table.
const _: () = assert!(strategy_xmm::XMM_MAX_PERPS <= core_fill::QUEUE_MAX_SYMS);
const _: () = assert!(4 * strategy_xmm::XMM_MAX_PERPS <= core_fill::QUEUE_MAX_ORDERS);

/// Everything slot 6 needs to be configured.
#[derive(Debug, Clone)]
pub struct XmmBoot {
    /// The member's configuration.
    pub params: XmmParams,
    /// sha256 of the artifact BYTES — the boot tell's identity.
    pub hash: [u8; 32],
    /// The artifact path actually read.
    pub path: PathBuf,
    /// The quoted Hyperliquid coins, in artifact order.
    pub coins: Vec<&'static str>,
}

/// Whether the operator asked for the member.
#[must_use]
pub fn xmm_wanted(requested: u8) -> bool {
    requested & strategy_set::BIT_XMM != 0
}

/// Load and resolve. `Ok(None)` = the artifact is absent at its DEFAULT
/// path, which leaves the member unconfigured and its bit unset; the
/// caller turns that into a refusal when the bit was requested.
///
/// * `artifact` — `--xmm`, or `None` for `~/multivenue/xmm.toml`.
/// * `resolve` — descriptor → `SymbolId` (the AI descriptor table).
pub fn load_xmm_boot(
    artifact: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
) -> Result<Option<XmmBoot>, String> {
    let explicit = artifact.is_some();
    let path: PathBuf = match artifact {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(core_config::xmm::default_xmm_path().map_err(|e| e.to_string())?),
    };
    if !path.exists() {
        if explicit {
            return Err(format!("{}: no such file", path.display()));
        }
        info!(path = %path.display(), "xmm: no artifact — member unconfigured");
        return Ok(None);
    }
    let (file, bytes) = core_config::xmm::load(&path).map_err(|e| e.to_string())?;
    let hash = core_crypto::sha256(&bytes);
    let (params, coins) = build_params(&file, resolve)?;
    Ok(Some(XmmBoot {
        params,
        hash,
        path,
        coins,
    }))
}

/// The parsed artifact → the member's params, every descriptor resolved.
/// Shared by the boot and the offline harness (`backtest --member xmm`),
/// so both refuse exactly the same artifacts. The member's own
/// [`XmmParams::validate`] runs last, as the second opinion.
pub fn build_params(
    file: &XmmFile,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
) -> Result<(XmmParams, Vec<&'static str>), String> {
    let mut p = XmmParams::EMPTY;
    let mut coins = Vec::new();
    let mut n = 0usize;
    for coin in file.quoted() {
        let follower = format!("hyperliquid:{}", coin.hl_coin);
        let leader = format!("binance-usdm:{}", coin.lead);
        let hl_sym = resolve(&follower).ok_or_else(|| {
            format!(
                "xmm.toml: `{} = 1` but `{follower}` is not in the boot universe \
                 (universe.toml [hyperliquid] coins)",
                coin.key
            )
        })?;
        let lead_sym = resolve(&leader).ok_or_else(|| {
            format!(
                "xmm.toml: `{} = 1` but its leader `{leader}` is not in the boot universe \
                 (universe.toml [binance] usdm)",
                coin.key
            )
        })?;
        p.perps[n] = XmmPerp {
            hl_sym,
            lead_sym,
            lot_1e6: coin.lot_1e6,
        };
        n += 1;
        coins.push(coin.hl_coin);
    }
    p.n_perps = u8::try_from(n).map_err(|_| "xmm.toml: too many perps".to_owned())?;
    p.maker_enabled = u8::from(file.maker_enabled);
    p.ab_mode = file.ab_mode;
    p.theta_bps_1e6 = file.theta_bps_1e6;
    p.gate_window_ms = file.gate_window_ms;
    p.lifetime_ms = file.lifetime_ms;
    p.lead_stale_ms = file.lead_stale_ms;
    p.follower_stale_ms = file.follower_stale_ms;
    p.rtt_pull_ms = file.rtt_pull_ms;
    p.requote_min_ms = file.requote_min_ms;
    p.alo_priority_1e8 = file.alo_priority_1e8;
    p.clip_usd_1e6 = file.clip_usd_1e6;
    p.inv_cap_usd_1e6 = file.inv_cap_usd_1e6;
    p.gross_inv_cap_usd_1e6 = file.gross_inv_cap_usd_1e6;
    p.resting_cap_usd_1e6 = file.resting_cap_usd_1e6;
    p.skew_kappa_1e6 = file.skew_kappa_1e6;
    p.validate().map_err(|e| format!("xmm.toml: {e}"))?;
    Ok((p, coins))
}

/// The one-line boot tell: the artifact's identity and every knob that
/// decides what the member does.
#[must_use]
pub fn render_boot_tell(boot: &XmmBoot) -> String {
    let mut hex = String::with_capacity(64);
    for b in &boot.hash {
        hex.push_str(&format!("{b:02x}"));
    }
    let p = &boot.params;
    format!(
        "xmm: artifact configured hash={hex} path={} coins={} maker_enabled={} theta_bps_1e6={} \
         gate_window_ms={} lifetime_ms={} clip_usd_1e6={} inv_cap_usd_1e6={} \
         gross_inv_cap_usd_1e6={} resting_cap_usd_1e6={} lead_stale_ms={} follower_stale_ms={} \
         rtt_pull_ms={} requote_min_ms={} alo_priority_1e8={} ab_mode={} phase=XH3(paper)",
        boot.path.display(),
        boot.coins.join(","),
        p.maker_enabled,
        p.theta_bps_1e6,
        p.gate_window_ms,
        p.lifetime_ms,
        p.clip_usd_1e6,
        p.inv_cap_usd_1e6,
        p.gross_inv_cap_usd_1e6,
        p.resting_cap_usd_1e6,
        p.lead_stale_ms,
        p.follower_stale_ms,
        p.rtt_pull_ms,
        p.requote_min_ms,
        p.alo_priority_1e8,
        p.ab_mode,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, VenueId};

    const EXAMPLE: &str = include_str!("../../../xmm.toml.example");

    /// The probe's four perps and their leaders.
    fn resolve(d: &str) -> Option<SymbolId> {
        match d {
            "hyperliquid:BTC" => Some(make_symbol_id(VenueId::Hyperliquid, 0)),
            "hyperliquid:ETH" => Some(make_symbol_id(VenueId::Hyperliquid, 1)),
            "hyperliquid:SOL" => Some(make_symbol_id(VenueId::Hyperliquid, 5)),
            "hyperliquid:XRP" => Some(make_symbol_id(VenueId::Hyperliquid, 25)),
            "binance-usdm:btcusdt" => Some(make_symbol_id(VenueId::Binance, 100)),
            "binance-usdm:ethusdt" => Some(make_symbol_id(VenueId::Binance, 101)),
            "binance-usdm:solusdt" => Some(make_symbol_id(VenueId::Binance, 102)),
            "binance-usdm:xrpusdt" => Some(make_symbol_id(VenueId::Binance, 103)),
            _ => None,
        }
    }

    fn write(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xmm-boot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn the_example_resolves_to_the_probe_member() {
        let p = write("probe.toml", EXAMPLE);
        let boot = load_xmm_boot(Some(&p), &resolve)
            .expect("resolves")
            .expect("present");
        assert_eq!(boot.coins, ["BTC", "ETH", "SOL", "XRP"]);
        assert_eq!(boot.params.n_perps, 4);
        assert_eq!(
            boot.params.perps[2],
            XmmPerp {
                hl_sym: make_symbol_id(VenueId::Hyperliquid, 5),
                lead_sym: make_symbol_id(VenueId::Binance, 102),
                lot_1e6: 10_000,
            }
        );
        assert_eq!(boot.params.perps[4], XmmPerp::EMPTY);
        assert_eq!(boot.hash, core_crypto::sha256(EXAMPLE.as_bytes()));
        assert_eq!(boot.params.validate(), Ok(()));
        let tell = render_boot_tell(&boot);
        assert!(tell.contains("coins=BTC,ETH,SOL,XRP"), "{tell}");
        assert!(tell.contains("clip_usd_1e6=15000000"), "{tell}");
        assert!(tell.starts_with("xmm: artifact configured hash="), "{tell}");
    }

    #[test]
    fn an_explicit_missing_artifact_refuses() {
        let e = load_xmm_boot(Some(Path::new("/nonexistent/xmm.toml")), &resolve)
            .expect_err("an explicit path that does not exist refuses");
        assert!(e.contains("no such file"), "{e}");
    }

    #[test]
    fn an_unresolvable_follower_or_leader_refuses_and_names_it() {
        let no_sol = |d: &str| if d == "hyperliquid:SOL" { None } else { resolve(d) };
        let file = core_config::xmm::parse(EXAMPLE).unwrap();
        let e = build_params(&file, &no_sol).expect_err("a perp nobody subscribed");
        assert!(e.contains("hyperliquid:SOL") && e.contains("quote_sol"), "{e}");
        let no_lead = |d: &str| if d == "binance-usdm:xrpusdt" { None } else { resolve(d) };
        let e = build_params(&file, &no_lead).expect_err("a leader nobody streams");
        assert!(e.contains("binance-usdm:xrpusdt"), "{e}");
    }

    #[test]
    fn a_malformed_artifact_refuses() {
        let p = write("bad.toml", &EXAMPLE.replace("ab_mode = 0", "ab_mode = 9"));
        let e = load_xmm_boot(Some(&p), &resolve).expect_err("a bound refuses");
        assert!(e.contains("ab_mode"), "{e}");
    }

    #[test]
    fn wanted_reads_slot_six_only() {
        assert!(xmm_wanted(strategy_set::BIT_XMM));
        assert!(xmm_wanted(strategy_set::BUILT_MASK));
        assert!(!xmm_wanted(strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM));
        assert!(!xmm_wanted(0));
    }
}
