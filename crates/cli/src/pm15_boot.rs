// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PM15 boot: `pm15.toml` → slot 9's `Pm15Params` (the `bin15_boot` laws).
//!
//! - The artifact's `families` must equal the assets of the universe's
//!   `[polymarket] rolling` keys AS A SEQUENCE: the member indexes families
//!   by the ingress's own index (it rides in the roll's instance id and the
//!   slot syms).
//! - Every `perp` descriptor resolves against the same table the ruleset
//!   validator uses: the nowcast has to be an instrument the engine
//!   subscribes to.
//! - Requested-but-absent REFUSES the boot (the caller's F19 check); a
//!   present-and-wrong artifact refuses too.

use std::path::{Path, PathBuf};

use core_types::SymbolId;
use tracing::info;

/// Everything slot 9 needs to be configured.
#[derive(Debug)]
pub struct Pm15Boot {
    /// The member's configuration.
    pub params: strategy_pm15::Pm15Params,
    /// The late Φ table.
    pub lut: Box<core_twaplock::late::LateLut>,
    /// The assets ruling R-PM-4 lets arm live.
    pub live_assets: Vec<String>,
    /// The families' assets, in order (the boot tell).
    pub families: Vec<String>,
    /// sha256 of the artifact BYTES — the boot tell's identity.
    pub hash: [u8; 32],
    /// The artifact path actually read.
    pub path: PathBuf,
}

/// Whether the operator asked for the member.
#[must_use]
pub fn pm15_wanted(requested: core_types::SlotMask) -> bool {
    requested & strategy_set::BIT_PM15 != 0
}

/// Load and resolve. `Ok(None)` = the artifact is absent at its DEFAULT
/// path. `rolling` is the universe's `[polymarket] rolling` keys, in order.
///
/// # Errors
/// The reason, as the boot log states it.
pub fn load_pm15_boot(
    artifact: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    rolling: &[String],
) -> Result<Option<Pm15Boot>, String> {
    let explicit = artifact.is_some();
    let path: PathBuf = match artifact {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(core_config::pm15::default_pm15_path().map_err(|e| e.to_string())?),
    };
    if !path.exists() {
        if explicit {
            return Err(format!("{}: no such file", path.display()));
        }
        info!(path = %path.display(), "pm15: no artifact — member unconfigured");
        return Ok(None);
    }
    let (file, bytes) = core_config::pm15::load(&path).map_err(|e| e.to_string())?;
    let hash = core_crypto::sha256(&bytes);
    resolve_file(file, hash, path, resolve, rolling).map(Some)
}

/// The resolution half of [`load_pm15_boot`] (unit-testable).
fn resolve_file(
    file: core_config::pm15::Pm15File,
    hash: [u8; 32],
    path: PathBuf,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    rolling: &[String],
) -> Result<Pm15Boot, String> {
    if file.families.len() != rolling.len() {
        return Err(format!(
            "pm15.toml `families` has {} assets but universe.toml `[polymarket] rolling` has {} \
             families — the member indexes families by the ingress's own index, so the two lists \
             must match exactly and in order",
            file.families.len(),
            rolling.len()
        ));
    }
    let mut params = strategy_pm15::Pm15Params::default();
    let mut f = 0usize;
    while f < rolling.len() {
        let asset = core_config::universe::pm_rolling_asset(&rolling[f]).unwrap_or("");
        if file.families[f] != asset {
            return Err(format!(
                "pm15.toml `families`[{f}] = `{}` but universe.toml `rolling`[{f}] = `{}` — same \
                 list, same order",
                file.families[f], rolling[f]
            ));
        }
        let d = &file.perp[f];
        let Some(sym) = resolve(d) else {
            return Err(format!(
                "pm15.toml `perp`[{f}] = `{d}` does not resolve against the instrument manifest — \
                 the nowcast has to be an instrument the engine subscribes to"
            ));
        };
        params.perp_sym[f] = sym;
        f += 1;
    }
    params.n_families = rolling.len();
    params.tau_min_ns = file.tau_min_ns;
    params.tau_max_ns = file.tau_max_ns;
    params.p_min_1e6 = file.p_min_1e6;
    params.e_1e6 = file.e_1e6;
    params.persist = file.persist;
    params.max_orders_market = file.max_orders_market;
    params.sigma_k_1e3 = file.sigma_k_1e3;
    params.usd_order_1e6 = file.usd_order_1e6;
    params.usd_market_1e6 = file.usd_market_1e6;
    params.session_loss_1e6 = file.session_loss_usd_1e6;
    params.order_ttl_ns = file.order_ttl_ns;
    let mut lut = Box::new(core_twaplock::late::LateLut::flat());
    let mut i = 0usize;
    while i < core_config::pm15::PM15_PHI_POINTS {
        lut.phi[i] = file.phi_lut[i];
        i += 1;
    }
    Ok(Pm15Boot {
        params,
        lut,
        live_assets: file.live_assets,
        families: file.families,
        hash,
        path,
    })
}

/// The boot tell: `pm15: artifact configured hash=… families=btc+eth+…
/// law=2-20s:p>=0.900:ask<=p-0.020:persist2:k1.550 frame=$250/order:$250/market:-$500/session live=btc+eth`.
#[must_use]
pub fn render_boot_tell(boot: &Pm15Boot) -> String {
    let mut hex = String::with_capacity(64);
    for b in &boot.hash {
        hex.push_str(&format!("{b:02x}"));
    }
    let p = &boot.params;
    format!(
        "pm15: artifact configured hash={hex} families={} law={}-{}s:p>={:.3}:ask<=p-{:.3}:persist{}:k{:.3} \
         frame=${}/order:${}/market:-${}/session live={}",
        boot.families.join("+"),
        p.tau_min_ns / 1_000_000_000,
        p.tau_max_ns / 1_000_000_000,
        p.p_min_1e6 as f64 / 1e6,
        p.e_1e6 as f64 / 1e6,
        p.persist,
        p.sigma_k_1e3 as f64 / 1e3,
        p.usd_order_1e6 / 1_000_000,
        p.usd_market_1e6 / 1_000_000,
        p.session_loss_1e6 / 1_000_000,
        if boot.live_assets.is_empty() { String::from("none") } else { boot.live_assets.join("+") },
    )
}

/// PM15-8, `multivenue-engine pm15-check`: the deploy's dry check, with
/// no socket and no thread — the universe file (`--universe`, else the
/// boot's own default) and `pm15.toml` (`--pm15`, else the default)
/// resolved exactly as `run` resolves them, minus the options discovery
/// (no `pm15.toml` `perp` names an option). Returns the boot tell.
///
/// # Errors
/// The refusal `run` would log, with its file.
pub fn check(universe: Option<&Path>, artifact: Option<&Path>) -> Result<String, String> {
    let src = crate::universe_boot::read_universe_source(universe)?;
    let boot = crate::universe_boot::resolve_boot_universe(&crate::universe_boot::UniverseFlags {
        config_src: src.as_deref(),
        pm_asset_id: None,
        pm_sym_id: None,
        bn_symbol: None,
        bn_sym_id: None,
        okx_symbols: None,
        deribit_symbols: None,
        hl_coins: None,
        okx_depth: false,
        deribit_depth: false,
    })?;
    // PMJ J1b: slot 9's families are the 15-minute keys only.
    let rolling = core_config::universe::pm_rolling_15m(&boot.pm_rolling);
    if rolling.is_empty() {
        return Err(String::from(
            "universe: `[polymarket] rolling` has no 15m family — slot 9 has no family",
        ));
    }
    let table = ingress_ai::DescriptorTable::from_entries(
        crate::options_manifest::build_descriptor_entries(
            &boot.allocated,
            &[],
            &[],
            &[],
            &[],
            boot.okx_depth,
            boot.deribit_depth,
        ),
    );
    match load_pm15_boot(
        artifact,
        &|d: &str| table.resolve(d.as_bytes()).map(|(sym, _)| sym),
        &rolling,
    )? {
        Some(b) => Ok(format!(
            "rolling={} {}",
            rolling.join(","),
            render_boot_tell(&b)
        )),
        None => Err(String::from(
            "pm15: no artifact at the default path (~/multivenue/pm15.toml)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> core_config::pm15::Pm15File {
        core_config::pm15::Pm15File {
            families: vec!["btc".into(), "eth".into()],
            perp: vec!["hyperliquid:BTC".into(), "hyperliquid:ETH".into()],
            tau_min_ns: 2_000_000_000,
            tau_max_ns: 20_000_000_000,
            p_min_1e6: 900_000,
            e_1e6: 20_000,
            persist: 2,
            sigma_k_1e3: 1_550,
            usd_order_1e6: 250_000_000,
            usd_market_1e6: 250_000_000,
            session_loss_usd_1e6: 500_000_000,
            max_orders_market: 4,
            order_ttl_ns: 1_000_000_000,
            live_assets: vec!["btc".into(), "eth".into()],
            phi_lut: vec![500_000; core_config::pm15::PM15_PHI_POINTS],
        }
    }

    fn resolve(d: &str) -> Option<SymbolId> {
        match d {
            "hyperliquid:BTC" => Some(11),
            "hyperliquid:ETH" => Some(12),
            _ => None,
        }
    }

    #[test]
    fn the_artifact_resolves_against_the_universe_in_order() {
        let rolling = vec!["updown:btc:15m".to_string(), "updown:eth:15m".to_string()];
        let b =
            resolve_file(file(), [0; 32], PathBuf::from("x"), &resolve, &rolling).expect("valid");
        assert_eq!(b.params.n_families, 2);
        assert_eq!((b.params.perp_sym[0], b.params.perp_sym[1]), (11, 12));
        assert_eq!(b.params.usd_market_1e6, 250_000_000);
        let tell = render_boot_tell(&b);
        assert!(tell.contains("families=btc+eth"), "{tell}");
        assert!(
            tell.contains("frame=$250/order:$250/market:-$500/session"),
            "{tell}"
        );
        assert!(tell.contains("live=btc+eth"), "{tell}");
        // A permutation, a short list, an unresolvable perp.
        let swapped = vec!["updown:eth:15m".to_string(), "updown:btc:15m".to_string()];
        assert!(resolve_file(file(), [0; 32], PathBuf::from("x"), &resolve, &swapped).is_err());
        assert!(
            resolve_file(file(), [0; 32], PathBuf::from("x"), &resolve, &rolling[..1]).is_err()
        );
        let mut bad = file();
        bad.perp[1] = "hyperliquid:XRP".into();
        assert!(resolve_file(bad, [0; 32], PathBuf::from("x"), &resolve, &rolling).is_err());
    }
}
