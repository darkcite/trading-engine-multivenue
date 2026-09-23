// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! HYPARB H5 boot: `hyparb.toml` → `strategy_hyparb::HyparbParams`.
//!
//! COPY-DOCTRINE: boot-only module (runs once before the engine loop;
//! allocates and copies freely).
//!
//! The member takes no `core-config` dependency (plan §8.1), so this is
//! where the artifact becomes its configuration: the coins' Hyperliquid
//! descriptors resolve against the SAME table the ruleset validator
//! uses, and each pool's address resolves against `universe.toml
//! [hyperevm] pools` — the universe allocates the symbol (and carries
//! the family and the decimals the ingress verifies on chain); the
//! artifact only says how the member treats the pool.
//!
//! Laws, from the other lanes verbatim:
//!
//! * **Requested-but-absent REFUSES** (the icdp/F19 law) — the caller
//!   turns `Ok(None)` into a refusal when slot 0 was requested.
//! * **Present-and-unreadable refuses too**, and so does anything that
//!   does not resolve: a pool the universe does not list, a descriptor
//!   the manifest does not know.
//! * **Two switches for the EVM write path** (O-H5): `mode = "testnet"`
//!   in the artifact AND `--evm-testnet` on the command line, both or
//!   neither — and refused while the path is not linked
//!   ([`EVM_WRITE_PATH_LINKED`]).
//!
//! Deviation from plan §9.3, stated: the archive probe is NOT run here.
//! The ingress probes the endpoint in-session at every snapshot, and a
//! dishonest endpoint disables the member, not the engine (O-H15) — a
//! boot-time probe that refused the boot would take every other slot
//! down with it. Likewise there are no boot-time maps: the member builds
//! them from the snapshot on the tape, which is what a replay rebuilds.

use std::path::{Path, PathBuf};

use core_config::hyparb::{HyparbFile, HyparbHedgeVenue, HyparbMode, COIN_USD_NAME};
use core_config::universe::Instrument;
use core_types::SymbolId;
use strategy_hyparb::{CoinParams, HedgeMode, HyparbParams, PoolParams, COIN_USD};
use tracing::info;

/// Whether the EVM write path (plan §11, H7/H8) is linked into this
/// build. Until it is, `mode = "testnet"` refuses even with both
/// switches set: a boot that silently ran paper while its artifact said
/// testnet would be a member nobody chose.
pub const EVM_WRITE_PATH_LINKED: bool = false;

const _: () = assert!(core_config::hyparb::HYPARB_MAX_COINS == strategy_hyparb::HYPARB_MAX_COINS);
const _: () = assert!(core_config::hyparb::HYPARB_MAX_POOLS == strategy_hyparb::HYPARB_MAX_POOLS);
const _: () =
    assert!(core_config::hyparb::HYPARB_MAX_POOLS == core_config::universe::HYPEREVM_POOLS_MAX);

/// Everything slot 0 needs to be configured.
#[derive(Debug, Clone)]
pub struct HyparbBoot {
    /// The member's configuration.
    pub params: HyparbParams,
    /// sha256 of the artifact BYTES — the boot tell's identity.
    pub hash: [u8; 32],
    /// The artifact path actually read.
    pub path: PathBuf,
    /// Paper or testnet.
    pub mode: HyparbMode,
    /// Pools configured to trade (the rest are observed only).
    pub traded: usize,
}

/// Whether the operator asked for the member.
#[must_use]
pub fn hyparb_wanted(requested: u8) -> bool {
    requested & strategy_set::BIT_HYPARB != 0
}

/// Load and resolve. `Ok(None)` = the artifact is absent at its DEFAULT
/// path, which leaves the member unconfigured and its bit unset; the
/// caller turns that into a refusal when the bit was requested.
///
/// * `artifact` — `--hyparb`, or `None` for `~/multivenue/hyparb.toml`.
/// * `resolve` — descriptor → `SymbolId` (the AI descriptor table).
/// * `universe_pools` — the allocated `[hyperevm]` instruments.
/// * `evm_testnet` — the `--evm-testnet` switch.
pub fn load_hyparb_boot(
    artifact: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    universe_pools: &[Instrument],
    evm_testnet: bool,
) -> Result<Option<HyparbBoot>, String> {
    let explicit = artifact.is_some();
    let path: PathBuf = match artifact {
        Some(p) => p.to_path_buf(),
        None => {
            PathBuf::from(core_config::hyparb::default_hyparb_path().map_err(|e| e.to_string())?)
        }
    };
    if !path.exists() {
        if explicit {
            return Err(format!("{}: no such file", path.display()));
        }
        info!(path = %path.display(), "hyparb: no artifact — member unconfigured");
        return Ok(None);
    }
    let (file, bytes) = core_config::hyparb::load(&path).map_err(|e| e.to_string())?;
    let hash = core_crypto::sha256(&bytes);
    check_switches(file.mode, evm_testnet)?;
    let params = build_params(&file, resolve, universe_pools)?;
    let traded = file.pools.iter().filter(|p| p.trade).count();
    Ok(Some(HyparbBoot {
        params,
        hash,
        path,
        mode: file.mode,
        traded,
    }))
}

/// O-H5: the artifact's mode and `--evm-testnet` agree, and testnet is
/// linked.
fn check_switches(mode: HyparbMode, evm_testnet: bool) -> Result<(), String> {
    match (mode, evm_testnet) {
        (HyparbMode::Paper, false) => Ok(()),
        (HyparbMode::Paper, true) => Err("hyparb: --evm-testnet with `mode = \"paper\"` — the \
             two switches of the EVM write path must agree (O-H5)"
            .to_owned()),
        (HyparbMode::Testnet, false) => Err("hyparb: `mode = \"testnet\"` without \
             --evm-testnet — the EVM write path needs BOTH switches (O-H5)"
            .to_owned()),
        (HyparbMode::Testnet, true) if !EVM_WRITE_PATH_LINKED => Err(
            "hyparb: `mode = \"testnet\"` — the EVM write path is not linked into \
                 this build; refusing rather than running paper under a testnet artifact"
                .to_owned(),
        ),
        (HyparbMode::Testnet, true) => Ok(()),
    }
}

/// The artifact, resolved, as the member's parameters.
fn build_params(
    file: &HyparbFile,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    universe_pools: &[Instrument],
) -> Result<HyparbParams, String> {
    let mut p = HyparbParams::EMPTY;
    let mut c = 0usize;
    while c < file.coins.len() {
        let coin = &file.coins[c];
        let book = |d: &Option<String>| -> Result<SymbolId, String> {
            match d {
                None => Ok(core_types::SYMBOL_ID_NONE),
                Some(d) => resolve(d).ok_or_else(|| {
                    format!(
                        "hyparb.toml coin `{}`: `{d}` does not resolve against the \
                         instrument manifest — a hedge book has to be one the engine \
                         actually subscribes to",
                        coin.name
                    )
                }),
            }
        };
        p.coins[c] = CoinParams {
            perp_sym: book(&coin.perp)?,
            spot_sym: book(&coin.spot)?,
            lot_1e6: coin.lot_1e6,
            min_notional_usd_1e6: coin.min_notional_usd_1e6,
        };
        c += 1;
    }
    p.n_coins = file.coins.len();

    let coin_index = |name: &str| -> u8 {
        if name == COIN_USD_NAME {
            return COIN_USD;
        }
        // The parser proved every pool coin names a [[coin]].
        file.coins
            .iter()
            .position(|k| k.name == name)
            .map_or(COIN_USD, |i| i as u8)
    };
    let mut i = 0usize;
    while i < file.pools.len() {
        let pool = &file.pools[i];
        let Some(inst) = universe_pools.iter().find(|u| u.name == pool.address) else {
            return Err(format!(
                "hyparb.toml pool {}: not in universe.toml `[hyperevm] pools` — the \
                 universe allocates a pool's symbol and carries its family and \
                 decimals; append it there first",
                pool.address
            ));
        };
        p.pools[i] = PoolParams {
            sym: inst.sym,
            coin0: coin_index(&pool.coin0),
            coin1: coin_index(&pool.coin1),
            trade: pool.trade,
            max_notional_usd_1e6: pool
                .max_notional_usd_1e6
                .unwrap_or(file.cap_instance_usd_1e6),
        };
        i += 1;
    }
    p.n_pools = file.pools.len();

    p.lag_ns = file.lag_ns;
    p.basis_window_ns = file.basis_window_ns;
    p.basis_enabled = file.basis_enabled;
    p.depth_cap_enabled = file.depth_cap_enabled;
    p.gas_p50_usd_1e6 = file.gas_p50_usd_1e6;
    p.gas_p99_usd_1e6 = file.gas_p99_usd_1e6;
    p.max_order_usd_1e6 = file.max_order_usd_1e6;
    p.cap_day_usd_1e6 = file.cap_day_usd_1e6;
    p.min_net_bps_1e6 = file.min_net_bps_1e6;
    p.inventory_cap_usd_1e6 = file.inventory_cap_usd_1e6;
    p.hedge_mode = match file.hedge_venue {
        HyparbHedgeVenue::Auto => HedgeMode::Auto,
        HyparbHedgeVenue::Perp => HedgeMode::Perp,
        HyparbHedgeVenue::Spot => HedgeMode::Spot,
    };
    p.hedge_switch_hysteresis_bps_1e6 = file.hedge_switch_hysteresis_bps_1e6;
    p.perp_taker_bps_1e6 = file.perp_taker_bps_1e6;
    p.spot_taker_bps_1e6 = file.spot_taker_bps_1e6;
    p.funding_window_ns = file.funding_window_ns;
    p.cooldown_ns = file.cooldown_ns;
    // A forced venue with no such book on some coin could never hedge it.
    let mut k = 0usize;
    while k < p.n_coins {
        let missing = match p.hedge_mode {
            HedgeMode::Perp => p.coins[k].perp_sym == core_types::SYMBOL_ID_NONE,
            HedgeMode::Spot => p.coins[k].spot_sym == core_types::SYMBOL_ID_NONE,
            HedgeMode::Auto => false,
        };
        if missing {
            return Err(format!(
                "hyparb.toml: `hedge_venue` forces a book coin `{}` does not configure",
                file.coins[k].name
            ));
        }
        k += 1;
    }
    p.validate().map_err(|e| format!("hyparb.toml: {e}"))?;
    Ok(p)
}

/// The one-line boot tell: the artifact's identity and the knobs that
/// decide what the member does.
#[must_use]
pub fn render_boot_tell(boot: &HyparbBoot) -> String {
    let mut hex = String::with_capacity(64);
    for b in &boot.hash {
        hex.push_str(&format!("{b:02x}"));
    }
    let p = &boot.params;
    let venue = match p.hedge_mode {
        HedgeMode::Auto => "auto",
        HedgeMode::Perp => "perp",
        HedgeMode::Spot => "spot",
    };
    let mode = match boot.mode {
        HyparbMode::Paper => "paper",
        HyparbMode::Testnet => "testnet",
    };
    format!(
        "hyparb: artifact configured hash={hex} path={} mode={mode} coins={} pools={} \
         traded={} hedge={venue} depth_cap={} basis={} lag_ns={} gas_p50_usd_1e6={} \
         min_net_bps_1e6={} max_order_usd_1e6={} cap_day_usd_1e6={} inventory_cap_usd_1e6={}",
        boot.path.display(),
        p.n_coins,
        p.n_pools,
        boot.traded,
        u8::from(p.depth_cap_enabled),
        u8::from(p.basis_enabled),
        p.lag_ns,
        p.gas_p50_usd_1e6,
        p.min_net_bps_1e6,
        p.max_order_usd_1e6,
        p.cap_day_usd_1e6,
        p.inventory_cap_usd_1e6,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, VenueId};

    const A: &str = "0x6c9a33e3b592c0d65b3ba59355d5be0d38259285";
    const EXAMPLE: &str = include_str!("../../../hyparb.toml.example");

    fn universe() -> Vec<Instrument> {
        vec![Instrument {
            sym: make_symbol_id(VenueId::HyperEvm, 1),
            name: A.to_owned(),
            descriptor: format!("hyperevm:{A}"),
        }]
    }

    fn resolve(d: &str) -> Option<SymbolId> {
        match d {
            "hyperliquid:HYPE" => Some(make_symbol_id(VenueId::Hyperliquid, 5)),
            "hyperliquid:@107" => Some(make_symbol_id(VenueId::Hyperliquid, 10_107)),
            _ => None,
        }
    }

    fn write(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hyparb-boot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn the_example_boots_and_its_tell_names_the_hash() {
        let p = write("ok.toml", EXAMPLE);
        let b = load_hyparb_boot(Some(&p), &resolve, &universe(), false)
            .unwrap()
            .expect("configured");
        assert_eq!(b.hash, core_crypto::sha256(EXAMPLE.as_bytes()));
        assert_eq!(b.params.n_pools, 1);
        assert_eq!(b.params.pools[0].sym, make_symbol_id(VenueId::HyperEvm, 1));
        assert_eq!(b.params.pools[0].coin0, 0);
        assert_eq!(b.params.pools[0].coin1, COIN_USD);
        assert_eq!(
            b.params.pools[0].max_notional_usd_1e6, 100_000_000,
            "cap_instance"
        );
        assert_eq!(
            b.params.coins[0].perp_sym,
            make_symbol_id(VenueId::Hyperliquid, 5)
        );
        assert_eq!(b.params.coins[0].spot_sym, core_types::SYMBOL_ID_NONE);
        assert_eq!(b.traded, 1);
        let tell = render_boot_tell(&b);
        assert!(
            tell.contains("mode=paper") && tell.contains("hedge=auto"),
            "{tell}"
        );
        let mut hex = String::new();
        for x in &b.hash {
            hex.push_str(&format!("{x:02x}"));
        }
        assert!(tell.contains(&hex));
        assert!(hyparb_wanted(strategy_set::BIT_HYPARB));
        assert!(!hyparb_wanted(strategy_set::BIT_BIN15));
    }

    #[test]
    fn an_explicit_absent_artifact_refuses_and_a_bad_one_names_its_line() {
        let e = load_hyparb_boot(
            Some(Path::new("/nonexistent/hyparb.toml")),
            &resolve,
            &universe(),
            false,
        )
        .unwrap_err();
        assert!(e.contains("no such file"), "{e}");
        let p = write(
            "float.toml",
            &EXAMPLE.replace("lag_ns = 500000000", "lag_ns = 0.5"),
        );
        let e = load_hyparb_boot(Some(&p), &resolve, &universe(), false).unwrap_err();
        assert!(e.starts_with("hyparb.toml: line"), "{e}");
    }

    #[test]
    fn a_pool_or_a_book_that_does_not_resolve_refuses() {
        let e =
            load_hyparb_boot(Some(&write("ok2.toml", EXAMPLE)), &resolve, &[], false).unwrap_err();
        assert!(e.contains("not in universe.toml"), "{e}");
        let p = write(
            "book.toml",
            &EXAMPLE.replace("perp = \"hyperliquid:HYPE\"", "perp = \"hyperliquid:NOPE\""),
        );
        let e = load_hyparb_boot(Some(&p), &resolve, &universe(), false).unwrap_err();
        assert!(e.contains("does not resolve"), "{e}");
        // Forced spot with no spot book on the coin can never hedge.
        let p = write(
            "spot.toml",
            &EXAMPLE.replace("hedge_venue = \"auto\"", "hedge_venue = \"spot\""),
        );
        let e = load_hyparb_boot(Some(&p), &resolve, &universe(), false).unwrap_err();
        assert!(e.contains("forces a book"), "{e}");
        // With the spot book configured, the forced venue boots.
        let p = write(
            "spot2.toml",
            &EXAMPLE
                .replace("hedge_venue = \"auto\"", "hedge_venue = \"spot\"")
                .replace(
                    "# spot = \"hyperliquid:@107\"",
                    "spot = \"hyperliquid:@107\"",
                ),
        );
        let b = load_hyparb_boot(Some(&p), &resolve, &universe(), false)
            .unwrap()
            .unwrap();
        assert_eq!(b.params.hedge_mode, HedgeMode::Spot);
    }

    #[test]
    fn the_two_evm_switches_must_agree_and_testnet_is_not_linked_yet() {
        let paper = write("p.toml", EXAMPLE);
        let e = load_hyparb_boot(Some(&paper), &resolve, &universe(), true).unwrap_err();
        assert!(e.contains("must agree"), "{e}");
        let testnet = write(
            "t.toml",
            &EXAMPLE.replace("mode = \"paper\"", "mode = \"testnet\""),
        );
        let e = load_hyparb_boot(Some(&testnet), &resolve, &universe(), false).unwrap_err();
        assert!(e.contains("BOTH switches"), "{e}");
        let e = load_hyparb_boot(Some(&testnet), &resolve, &universe(), true).unwrap_err();
        assert!(e.contains("not linked"), "{e}");
    }
}
