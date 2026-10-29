// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PMJ boot: `pmj.toml` → slot 11's `PmjParams` (the `pm15_boot` laws).
//!
//! - The artifact's `families` must equal the assets of the universe's
//!   `[polymarket] rolling` **5-minute** keys AS A SEQUENCE: the member
//!   indexes families by the 5-minute ingress's own index (it rides in the
//!   roll's instance id and the 5-minute slot syms).
//! - Every `perp` descriptor resolves against the same table the ruleset
//!   validator uses.
//! - Requested-but-absent REFUSES the boot (the caller's F19 check); a
//!   present-and-wrong artifact refuses too.
//! - Nothing here builds or arms the Jupiter arm; the paper law's keeper
//!   delay (`paper_delay_ms`) goes to the paper dispatcher, and the guard's
//!   and the wallet's keys ride in [`PmjLiveKeys`], read only by the live
//!   arm's boot (`cli::pmj_live`, PMJ J16) when slot 11 is armed.

use std::path::{Path, PathBuf};

use core_types::SymbolId;
use tracing::info;

/// Everything slot 11 needs to be configured.
#[derive(Debug)]
pub struct PmjBoot {
    /// The member's configuration.
    pub params: strategy_pmj::PmjParams,
    /// The late Φ table.
    pub lut: Box<core_twaplock::late::LateLut>,
    /// The assets that may arm live (none arms in this pass).
    pub live_assets: Vec<String>,
    /// The families' assets, in order (the boot tell).
    pub families: Vec<String>,
    /// The paper law's keeper delay, ns (`paper_delay_ms`).
    pub paper_delay_ns: u64,
    /// The per-order maximum the arm enforces, ×1e6 USD.
    pub max_order_usd_1e6: i64,
    /// Programs and lookup tables the guard lists, and whether the
    /// deposit account is set (the boot tell; the arm's boot decodes them).
    pub guard: (usize, usize, bool),
    /// PMJ J16: the keys only the live arm's boot reads (the guard's, the
    /// wallet's, the frame's) — a paper boot never decodes them.
    pub live_keys: PmjLiveKeys,
    /// sha256 of the artifact BYTES — the boot tell's identity.
    pub hash: [u8; 32],
    /// The artifact path actually read.
    pub path: PathBuf,
}

/// **PMJ J16** — the `pmj.toml` keys the live arm's boot and `pmj-exec
/// --key-check` read, kept as the artifact wrote them (base58 text; the
/// live boot decodes and refuses). Cold; a paper boot carries them unread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PmjLiveKeys {
    /// `guard_programs`.
    pub guard_programs: Vec<String>,
    /// `guard_alts`.
    pub guard_alts: Vec<String>,
    /// `guard_deposit_account`.
    pub guard_deposit_account: String,
    /// `guard_deposit_mint` (USDC or JupUSD; empty: unset).
    pub guard_deposit_mint: String,
    /// `guard_max_sol_lamports`.
    pub guard_max_sol_lamports: u64,
    /// `guard_max_cu_price`.
    pub guard_max_cu_price: u64,
    /// `guard_vault`.
    pub guard_vault: String,
    /// `guard_vault_alt` (`<table>:<entry>`; empty: a static vault).
    pub guard_vault_alt: String,
    /// `guard_cosigners` (≤ 1; empty: LAW PMJ-G as written).
    pub guard_cosigners: Vec<String>,
    /// `build_extra`.
    pub build_extra: Vec<String>,
    /// `min_sol_lamports` (0: unset — the live boot refuses).
    pub min_sol_lamports: u64,
    /// `guard_program_slot` (0: unpinned).
    pub guard_program_slot: u64,
    /// `deposit_usd_1e6` (the frame check).
    pub deposit_usd_1e6: i64,
    /// `usd_market_1e6` (the frame check).
    pub usd_market_1e6: i64,
    /// `session_loss_usd_1e6` (the frame check).
    pub session_loss_usd_1e6: i64,
}

/// The live keys of a parsed artifact (`resolve_file`, and the key check,
/// which reads `pmj.toml` without the universe).
#[must_use]
pub fn live_keys_of(file: &core_config::pmj::PmjFile) -> PmjLiveKeys {
    PmjLiveKeys {
        guard_programs: file.guard_programs.clone(),
        guard_alts: file.guard_alts.clone(),
        guard_deposit_account: file.guard_deposit_account.clone(),
        guard_deposit_mint: file.guard_deposit_mint.clone(),
        guard_max_sol_lamports: file.guard_max_sol_lamports,
        guard_max_cu_price: file.guard_max_cu_price,
        guard_vault: file.guard_vault.clone(),
        guard_vault_alt: file.guard_vault_alt.clone(),
        guard_cosigners: file.guard_cosigners.clone(),
        build_extra: file.build_extra.clone(),
        min_sol_lamports: file.min_sol_lamports,
        guard_program_slot: file.guard_program_slot,
        deposit_usd_1e6: file.deposit_usd_1e6,
        usd_market_1e6: file.usd_market_1e6,
        session_loss_usd_1e6: file.session_loss_usd_1e6,
    }
}

/// Whether the operator asked for the member.
#[must_use]
pub fn pmj_wanted(requested: core_types::SlotMask) -> bool {
    requested & strategy_set::BIT_PMJ != 0
}

/// Load and resolve. `Ok(None)` = the artifact is absent at its DEFAULT
/// path. `rolling` is the universe's 5-minute `[polymarket] rolling` keys,
/// in order.
///
/// # Errors
/// The reason, as the boot log states it.
pub fn load_pmj_boot(
    artifact: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    rolling: &[String],
) -> Result<Option<PmjBoot>, String> {
    let explicit = artifact.is_some();
    let path: PathBuf = match artifact {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(core_config::pmj::default_pmj_path().map_err(|e| e.to_string())?),
    };
    if !path.exists() {
        if explicit {
            return Err(format!("{}: no such file", path.display()));
        }
        info!(path = %path.display(), "pmj: no artifact — member unconfigured");
        return Ok(None);
    }
    let (file, bytes) = core_config::pmj::load(&path).map_err(|e| e.to_string())?;
    let hash = core_crypto::sha256(&bytes);
    resolve_file(file, hash, path, resolve, rolling).map(Some)
}

/// The resolution half of [`load_pmj_boot`] (unit-testable).
fn resolve_file(
    file: core_config::pmj::PmjFile,
    hash: [u8; 32],
    path: PathBuf,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    rolling: &[String],
) -> Result<PmjBoot, String> {
    if file.families.len() != rolling.len() {
        return Err(format!(
            "pmj.toml `families` has {} assets but universe.toml `[polymarket] rolling` has {} \
             5-minute families — the member indexes families by the 5-minute ingress's own index, \
             so the two lists must match exactly and in order",
            file.families.len(),
            rolling.len()
        ));
    }
    let mut params = strategy_pmj::PmjParams::default();
    let mut f = 0usize;
    while f < rolling.len() {
        let asset = core_config::universe::pm5_rolling_asset(&rolling[f]).unwrap_or("");
        if file.families[f] != asset {
            return Err(format!(
                "pmj.toml `families`[{f}] = `{}` but universe.toml's 5-minute `rolling`[{f}] = `{}` \
                 — same list, same order",
                file.families[f], rolling[f]
            ));
        }
        let d = &file.perp[f];
        let Some(sym) = resolve(d) else {
            return Err(format!(
                "pmj.toml `perp`[{f}] = `{d}` does not resolve against the instrument manifest — \
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
    params.price_floor_1e6 = file.price_floor_1e6;
    params.min_tau_ns = file.min_tau_s.saturating_mul(1_000_000_000);
    params.deposit_usd_1e6 = file.deposit_usd_1e6;
    params.usd_market_1e6 = file.usd_market_1e6;
    params.session_loss_1e6 = file.session_loss_usd_1e6;
    params.order_ttl_ns = file.order_ttl_ns;
    // PMJ J12: the strike law and law (iii)'s residual.
    params.strike_law = file.strike_law.law;
    params.max_side_families = file.max_side_families;
    params.res_ratio_1e3 = file.strike_law.sigma_res_ratio_1e3;
    let mut f = 0usize;
    while f < file.strike_law.sigma_res_min_ppm.len() {
        params.res_min_ppm[f] = file.strike_law.sigma_res_min_ppm[f];
        f += 1;
    }
    let live_keys = live_keys_of(&file);
    let mut lut = Box::new(core_twaplock::late::LateLut::flat());
    let mut i = 0usize;
    while i < core_config::pmj::PMJ_PHI_POINTS {
        lut.phi[i] = file.phi_lut[i];
        i += 1;
    }
    Ok(PmjBoot {
        params,
        lut,
        live_assets: file.live_assets,
        families: file.families,
        paper_delay_ns: file.paper_delay_ms.saturating_mul(1_000_000),
        max_order_usd_1e6: file.max_order_usd_1e6,
        guard: (file.guard_programs.len(), file.guard_alts.len(), !file.guard_deposit_account.is_empty()),
        live_keys,
        hash,
        path,
    })
}

/// PMJ J13: the side cap's tail of the boot tell — empty when off (so the
/// tell is unchanged), `:side<=3` when on.
fn side_tell(cap: u8) -> String {
    if cap == 0 {
        String::new()
    } else {
        format!(":side<={cap}")
    }
}

/// PMJ J12: the law's tail of the boot tell — empty under the first law (so
/// its tell is unchanged), `:strike=trap62:res=130/3,5,…` under law (iii).
fn strike_tell(law: u8, ratio_1e3: i64, floors: &[i64]) -> String {
    if law == core_twaplock::report::STRIKE_LAW_MEAN60 {
        return String::new();
    }
    let mut s = format!(":strike=trap62:res={ratio_1e3}/");
    let mut i = 0usize;
    while i < floors.len() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&floors[i].to_string());
        i += 1;
    }
    s
}

/// The boot tell: `pmj: artifact configured (PAPER) hash=… families=btc+eth
/// law=3-20s:p>=0.900:ask<=p-0.020:floor0.900:persist2:k1.550
/// frame=$5/order:$10/market:-$50/session max=$5.01/order paper=+2000ms
/// guard=0p/0alt/acct-unset live=none`.
#[must_use]
pub fn render_boot_tell(boot: &PmjBoot) -> String {
    let mut hex = String::with_capacity(64);
    for b in &boot.hash {
        hex.push_str(&format!("{b:02x}"));
    }
    let p = &boot.params;
    let tau_floor = p.tau_min_ns.max(p.min_tau_ns);
    format!(
        "pmj: artifact configured (PAPER) hash={hex} families={} law={}-{}s:p>={:.3}:ask<=p-{:.3}:floor{:.3}:persist{}:k{:.3}{}{} \
         frame=${}/order:${}/market:-${}/session max=${}.{:02}/order paper=+{}ms guard={}p/{}alt/acct-{} live={}",
        boot.families.join("+"),
        tau_floor / 1_000_000_000,
        p.tau_max_ns / 1_000_000_000,
        p.p_min_1e6 as f64 / 1e6,
        p.e_1e6 as f64 / 1e6,
        p.price_floor_1e6 as f64 / 1e6,
        p.persist,
        p.sigma_k_1e3 as f64 / 1e3,
        strike_tell(p.strike_law, p.res_ratio_1e3, &p.res_min_ppm[..p.n_families]),
        side_tell(p.max_side_families),
        p.deposit_usd_1e6 / 1_000_000,
        p.usd_market_1e6 / 1_000_000,
        p.session_loss_1e6 / 1_000_000,
        boot.max_order_usd_1e6 / 1_000_000,
        boot.max_order_usd_1e6 % 1_000_000 / 10_000,
        boot.paper_delay_ns / 1_000_000,
        boot.guard.0,
        boot.guard.1,
        if boot.guard.2 { "set" } else { "unset" },
        if boot.live_assets.is_empty() { String::from("none") } else { boot.live_assets.join("+") },
    )
}

/// `multivenue-engine pmj-check`: the deploy's dry check, with no socket
/// and no thread — the universe file (`--universe`, else the boot's own
/// default) and `pmj.toml` (`--pmj`, else the default) resolved exactly as
/// `run` resolves them. Returns the boot tell.
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
    let rolling = core_config::universe::pm_rolling_5m(&boot.pm_rolling);
    if rolling.is_empty() {
        return Err(String::from(
            "universe: `[polymarket] rolling` has no 5m family — slot 11 has no family",
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
    match load_pmj_boot(
        artifact,
        &|d: &str| table.resolve(d.as_bytes()).map(|(sym, _)| sym),
        &rolling,
    )? {
        Some(b) => Ok(format!("rolling={} {}", rolling.join(","), render_boot_tell(&b))),
        None => Err(String::from("pmj: no artifact at the default path (~/multivenue/pmj.toml)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> core_config::pmj::PmjFile {
        core_config::pmj::PmjFile {
            strike_law: core_config::strike_law::StrikeLaw::default(),
            max_side_families: 0,
            families: vec!["btc".into(), "eth".into()],
            perp: vec!["hyperliquid:BTC".into(), "hyperliquid:ETH".into()],
            tau_min_ns: 2_000_000_000,
            tau_max_ns: 20_000_000_000,
            p_min_1e6: 900_000,
            e_1e6: 20_000,
            persist: 2,
            sigma_k_1e3: 1_550,
            price_floor_1e6: 900_000,
            min_tau_s: 3,
            deposit_usd_1e6: 5_000_000,
            max_order_usd_1e6: 5_010_000,
            usd_market_1e6: 10_000_000,
            session_loss_usd_1e6: 50_000_000,
            max_orders_market: 4,
            order_ttl_ns: 5_000_000_000,
            max_slippage_bps: 0,
            paper_delay_ms: 2_000,
            live_assets: Vec::new(),
            phi_lut: vec![500_000; core_config::pmj::PMJ_PHI_POINTS],
            guard_programs: Vec::new(),
            guard_alts: Vec::new(),
            guard_deposit_account: String::new(),
            guard_max_sol_lamports: 0,
            guard_max_cu_price: 0,
            guard_vault: String::new(),
            guard_vault_alt: String::new(),
            guard_cosigners: Vec::new(),
            build_extra: Vec::new(),
            guard_deposit_mint: String::new(),
            min_sol_lamports: 0,
            guard_program_slot: 0,
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
    fn the_artifact_resolves_against_the_five_minute_universe_in_order() {
        let rolling = vec!["updown:btc:5m".to_string(), "updown:eth:5m".to_string()];
        let b = resolve_file(file(), [0; 32], PathBuf::from("x"), &resolve, &rolling).expect("valid");
        assert_eq!(b.params.n_families, 2);
        assert_eq!((b.params.perp_sym[0], b.params.perp_sym[1]), (11, 12));
        assert_eq!((b.params.min_tau_ns, b.params.deposit_usd_1e6, b.paper_delay_ns), (3_000_000_000, 5_000_000, 2_000_000_000));
        let tell = render_boot_tell(&b);
        assert!(tell.contains("(PAPER)") && tell.contains("families=btc+eth"), "{tell}");
        assert!(tell.contains("law=3-20s:p>=0.900:ask<=p-0.020:floor0.900"), "{tell}");
        assert!(tell.contains("frame=$5/order:$10/market:-$50/session max=$5.01/order paper=+2000ms"), "{tell}");
        assert!(tell.contains("guard=0p/0alt/acct-unset live=none"), "{tell}");
        // PMJ J16: the live keys ride along, unread on paper.
        assert_eq!(b.live_keys, live_keys_of(&file()));
        assert_eq!((b.live_keys.min_sol_lamports, b.live_keys.deposit_usd_1e6), (0, 5_000_000));
        // A 15-minute key, a permutation, a short list, an unresolvable perp.
        let fifteen = vec!["updown:btc:15m".to_string(), "updown:eth:15m".to_string()];
        assert!(resolve_file(file(), [0; 32], PathBuf::from("x"), &resolve, &fifteen).is_err());
        let swapped = vec!["updown:eth:5m".to_string(), "updown:btc:5m".to_string()];
        assert!(resolve_file(file(), [0; 32], PathBuf::from("x"), &resolve, &swapped).is_err());
        assert!(resolve_file(file(), [0; 32], PathBuf::from("x"), &resolve, &rolling[..1]).is_err());
        let mut bad = file();
        bad.perp[1] = "hyperliquid:XRP".into();
        assert!(resolve_file(bad, [0; 32], PathBuf::from("x"), &resolve, &rolling).is_err());
    }

    #[test]
    fn the_member_is_wanted_only_by_its_own_bit() {
        assert!(pmj_wanted(strategy_set::BIT_PMJ));
        assert!(!pmj_wanted(strategy_set::BIT_PM15 | strategy_set::ALL_MASK));
    }

    #[test]
    fn the_strike_laws_agree_across_crates_and_tell_themselves() {
        assert_eq!(
            core_config::strike_law::STRIKE_LAW_MEAN60,
            core_twaplock::report::STRIKE_LAW_MEAN60
        );
        assert_eq!(
            core_config::strike_law::STRIKE_LAW_TRAP62,
            core_twaplock::report::STRIKE_LAW_TRAP62
        );
        assert_eq!(super::strike_tell(0, 0, &[]), "", "the first law's tell is unchanged");
        assert_eq!(super::strike_tell(1, 130, &[3, 5, 6]), ":strike=trap62:res=130/3,5,6");
    }

    #[test]
    fn the_side_cap_tells_itself_only_when_on() {
        assert_eq!(super::side_tell(0), "", "off: the tell is unchanged");
        assert_eq!(super::side_tell(3), ":side<=3");
        let rolling = vec!["updown:btc:5m".to_string(), "updown:eth:5m".to_string()];
        let mut f = file();
        f.max_side_families = 2;
        let b = resolve_file(f, [0; 32], PathBuf::from("x"), &resolve, &rolling).expect("valid");
        assert_eq!(b.params.max_side_families, 2);
        assert!(render_boot_tell(&b).contains(":k1.550:side<=2 "), "{}", render_boot_tell(&b));
        // Under law (iii) the cap's tail follows the strike's.
        let mut f = file();
        f.max_side_families = 2;
        f.strike_law = core_config::strike_law::StrikeLaw {
            law: core_config::strike_law::STRIKE_LAW_TRAP62,
            sigma_res_ratio_1e3: 130,
            sigma_res_min_ppm: vec![3, 5],
        };
        let b = resolve_file(f, [0; 32], PathBuf::from("x"), &resolve, &rolling).expect("valid");
        assert!(
            render_boot_tell(&b).contains(":k1.550:strike=trap62:res=130/3,5:side<=2 "),
            "{}",
            render_boot_tell(&b)
        );
    }
}
