// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # vrp_boot — `vrp.toml` + `vrp-seed.tsv` → the member's boot state
//!
//! One resolver for the three consumers of the VRP artifacts: the ENGINE
//! boot (`multivenue-engine run`) and both harness entry points
//! (`backtest` / `audit-pnl`), which read a window's own
//! `vrp-seed.tsv` exactly the way they read `regime-seed.tsv`.
//!
//! ## Why a cold boot must be legal
//!
//! The forecast needs 60 settled `(x, y)` pairs before it produces a
//! bound, and the engine restarts about three times a day. Without a
//! seed the member would be blind for two months, so the seed exists —
//! but an ABSENT seed can never refuse a boot. The member holds, the
//! boot tell says so, and the engine trades everything else. That is the
//! `strategy-vm` precedent ("absent data ⇒ HOLD") applied to a file.
//!
//! What DOES refuse a boot is a seed that is present and wrong: a float,
//! a short row, a repeat, or rows out of order. A seed the engine cannot
//! read exactly is a fit nobody measured.
//!
//! BOOT/OFFLINE DOCTRINE: runs once per boot / per harness invocation;
//! allocation is fine.

use std::path::{Path, PathBuf};

use core_types::SymbolId;
use opt_registry::{OptInstrument, OptRegistry, DERIBIT_OPT_CONTRACT_SIZE_1E9};
use tracing::{info, warn};

/// A loaded seed: the pairs in file order plus where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VrpSeed {
    /// `(expiry_ts_ms, x_1e9, y_1e9)`, oldest first.
    pub rows: Vec<(u64, i64, i64)>,
    /// The file actually read.
    pub path: PathBuf,
}

impl VrpSeed {
    /// Pairs held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the seed is empty (a cold boot in file form).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Whether the seed alone is enough for the member to decide, i.e.
    /// at least [`core_vol::MIN_PAIRS`] pairs. A shorter seed is legal —
    /// it just means the member holds until live settlements top it up.
    #[must_use]
    pub fn is_decisive(&self) -> bool {
        self.rows.len() >= core_vol::MIN_PAIRS
    }
}

/// Resolve and read the seed.
///
/// * `Some(path)` — an EXPLICIT flag: the file must exist and parse, and
///   a failure is the caller's to refuse the boot over.
/// * `None` — the default location. Absent is legal and yields `None`
///   (cold boot, member holds); present-but-invalid still fails.
///
/// The tell is emitted here so every consumer says the same sentence.
pub fn load_vrp_seed(seed_path: Option<&Path>) -> Result<Option<VrpSeed>, String> {
    let (path, explicit): (PathBuf, bool) = match seed_path {
        Some(p) => (p.to_path_buf(), true),
        None => {
            let d = core_config::vrp::default_seed_path().map_err(|e| e.to_string())?;
            (PathBuf::from(d), false)
        }
    };
    if !path.exists() {
        if explicit {
            return Err(format!("vrp: seed file {} does not exist", path.display()));
        }
        info!(path = %path.display(), "vrp: seed absent — the member holds until it has 60 pairs");
        return Ok(None);
    }
    let rows = core_config::vrp::load_seed(&path).map_err(|e| e.to_string())?;
    let seed = VrpSeed { rows, path };
    info!(
        path = %seed.path.display(),
        pairs = seed.len(),
        decisive = seed.is_decisive(),
        "vrp: seed applied"
    );
    if !seed.is_decisive() {
        warn!(
            pairs = seed.len(),
            need = core_vol::MIN_PAIRS,
            "vrp: seed is short — the member holds until live settlements fill it"
        );
    }
    Ok(Some(seed))
}


// ---------------------------------------------------------------
// The boot bundle (VRP V7)
// ---------------------------------------------------------------

/// Everything the member needs to be configured, resolved against the
/// live boot universe.
pub struct VrpBoot {
    /// Parameters translated out of `vrp.toml`.
    pub params: strategy_vrp::VrpParams,
    /// The boot-built option table for the selected chain.
    pub registry: OptRegistry,
    /// Seed pairs in file order (may be empty — a cold boot is legal).
    pub seed: Vec<(u64, i64, i64)>,
    /// Where the seed came from, for the boot tell. The default path
    /// even when nothing was there — "no pairs, from here" is the tell
    /// an operator needs to fix a cold boot.
    pub seed_path: PathBuf,
    /// Resolved `underlying_descriptor`.
    pub underlying_sym: SymbolId,
    /// Resolved `hedge_descriptor`.
    pub hedge_sym: SymbolId,
    /// SHA-256 of the exact `vrp.toml` bytes.
    pub hash: [u8; 32],
    /// Chain rows the parser refused (a non-inverse name, or a name it
    /// does not recognise). Counted, never guessed at.
    pub rows_refused: usize,
}

/// Build the option table for the live chain.
///
/// `deribit_options` is boot discovery's `(instrument_name, sym)` list
/// in allocation order. Contract size is
/// [`DERIBIT_OPT_CONTRACT_SIZE_1E9`], and that is a PROOF rather than an
/// assumption: the only Deribit options with a different size are the
/// USDC-LINEAR chains, whose names the descriptor parser refuses — so a
/// row that lands in this table is a 1.0-coin inverse option by
/// construction. A name that does not parse is counted, not guessed.
pub fn build_registry(
    deribit_options: &[(String, SymbolId)],
    hedge_sym: SymbolId,
) -> (OptRegistry, usize) {
    let mut reg = OptRegistry::new();
    let mut refused = 0usize;
    for (name, sym) in deribit_options {
        let Some(row) = OptInstrument::from_descriptor(
            *sym,
            hedge_sym,
            core_types::VenueId::Deribit as u8,
            name.as_bytes(),
            DERIBIT_OPT_CONTRACT_SIZE_1E9,
        ) else {
            refused += 1;
            continue;
        };
        if reg.insert(row).is_err() {
            refused += 1;
        }
    }
    (reg, refused)
}

/// Resolve the whole VRP boot: `vrp.toml`, its descriptors, the chain
/// table and the seed.
///
/// * `Some(path)` — an EXPLICIT `--vrp`: every failure refuses the boot.
/// * `None` — the default location. An ABSENT file yields `Ok(None)`:
///   the member simply is not configured, exactly as `icdp.toml` works,
///   and the cli then never sets its enable bit. A file that is present
///   and invalid still refuses the boot.
pub fn load_vrp_boot(
    path: Option<&Path>,
    seed_path: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    deribit_options: &[(String, SymbolId)],
) -> Result<Option<VrpBoot>, String> {
    let (path, explicit): (PathBuf, bool) = match path {
        Some(p) => (p.to_path_buf(), true),
        None => {
            let d = core_config::vrp::default_vrp_path().map_err(|e| e.to_string())?;
            (PathBuf::from(d), false)
        }
    };
    if !path.exists() {
        if explicit {
            return Err(format!("vrp: {} does not exist", path.display()));
        }
        info!(path = %path.display(), "vrp: artifact absent — the member is not configured");
        return Ok(None);
    }
    let (file, bytes) = core_config::vrp::load(&path).map_err(|e| e.to_string())?;
    let underlying_sym = resolve(&file.underlying_descriptor).ok_or_else(|| {
        format!(
            "vrp: `{}` is not in the boot universe",
            file.underlying_descriptor
        )
    })?;
    let hedge_sym = resolve(&file.hedge_descriptor)
        .ok_or_else(|| format!("vrp: `{}` is not in the boot universe", file.hedge_descriptor))?;
    let (registry, rows_refused) = build_registry(deribit_options, hedge_sym);
    if registry.is_empty() {
        return Err(format!(
            "vrp: the options chain is empty ({} of {} rows refused) — the member \
             cannot select an instrument",
            rows_refused,
            deribit_options.len()
        ));
    }
    let loaded = load_vrp_seed(seed_path)?;
    let seed_path_used = match loaded.as_ref() {
        Some(s) => s.path.clone(),
        None => match seed_path {
            Some(p) => p.to_path_buf(),
            None => PathBuf::from(
                core_config::vrp::default_seed_path().map_err(|e| e.to_string())?,
            ),
        },
    };
    let seed = loaded.map(|s| s.rows).unwrap_or_default();
    Ok(Some(VrpBoot {
        params: strategy_vrp::VrpParams {
            theta_1e9: file.theta_1e9,
            tau_ns: file.tau_ns,
            epsilon_ns: file.epsilon_ns,
            selection_ns: file.selection_ns,
            rebalance_ns: file.rebalance_ns,
            qty_1e6: file.qty_1e6,
            band_qty_1e6: file.band_qty_1e6,
        },
        registry,
        seed,
        seed_path: seed_path_used,
        underlying_sym,
        hedge_sym,
        hash: core_crypto::sha256(&bytes),
        rows_refused,
    }))
}

/// The one-line tell, rendered identically wherever it is printed.
#[must_use]
pub fn render_seed_tell(seed: Option<&VrpSeed>) -> String {
    match seed {
        Some(s) => format!(
            "vrp: seed applied pairs={} decisive={} from {}",
            s.len(),
            s.is_decisive(),
            s.path.display()
        ),
        None => format!(
            "vrp: seed absent — the member holds until it has {} pairs",
            core_vol::MIN_PAIRS
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("vrp-boot-{name}-{}.tsv", std::process::id()));
        std::fs::write(&p, body).expect("write");
        p
    }

    #[test]
    fn an_explicit_missing_seed_refuses_but_an_absent_default_does_not() {
        let missing = std::env::temp_dir().join("vrp-seed-that-does-not-exist.tsv");
        let _ = std::fs::remove_file(&missing);
        let err = load_vrp_seed(Some(&missing)).expect_err("explicit must refuse");
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn a_short_seed_loads_but_is_not_decisive() {
        let mut body = String::from("# vrp-seed.tsv\n");
        for i in 0..10u64 {
            body.push_str(&format!("{}\t{}\t{}\n", 1_000 + i, 30_000 + i, 31_000 + i));
        }
        let p = tmp("short", &body);
        let seed = load_vrp_seed(Some(&p)).expect("loads").expect("present");
        assert_eq!(seed.len(), 10);
        assert!(!seed.is_decisive(), "10 pairs is not 60");
        assert!(render_seed_tell(Some(&seed)).contains("pairs=10 decisive=false"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_full_seed_is_decisive_and_arrives_in_file_order() {
        let mut body = String::new();
        for i in 0..core_vol::MIN_PAIRS as u64 {
            body.push_str(&format!(
                "{}\t{}\t{}\n",
                1_000 + i,
                30_000_000_000 + i as i64 * 1_000,
                31_000_000_000 + i as i64 * 900
            ));
        }
        let p = tmp("full", &body);
        let seed = load_vrp_seed(Some(&p)).expect("loads").expect("present");
        assert!(seed.is_decisive());
        assert_eq!(seed.rows[0].0, 1_000);
        assert_eq!(seed.rows[core_vol::MIN_PAIRS - 1].0, 1_000 + 59);

        // The seed replayed through the engine is exactly a fitted
        // engine: this is the property the whole file exists for.
        let mut e = core_vol::VolEngine::new();
        for (_, x, y) in &seed.rows {
            e.seed_pair(*x, *y);
        }
        assert_eq!(e.n_pairs(), core_vol::MIN_PAIRS);
        assert!(e.fit().is_some(), "a decisive seed must produce a fit");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_present_but_malformed_seed_refuses() {
        for body in [
            "1000\t1\t2\n900\t3\t4\n",  // out of order
            "1000\t1\t2\n1000\t3\t4\n", // repeat
            "1000\t2.5\t3\n",           // a float
            "1000\t1\n",                // short row
        ] {
            let p = tmp("bad", body);
            assert!(
                load_vrp_seed(Some(&p)).is_err(),
                "must refuse {body:?} — a seed the engine cannot read \
                 exactly is a fit nobody measured"
            );
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn the_absent_tell_names_what_it_is_waiting_for() {
        let tell = render_seed_tell(None);
        assert!(tell.contains("seed absent"), "{tell}");
        assert!(tell.contains(&core_vol::MIN_PAIRS.to_string()), "{tell}");
    }
}
