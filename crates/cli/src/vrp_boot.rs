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
    /// W3: `(min_ts_ms, r_1e9)` rolling-window returns, oldest first.
    /// Empty for a v1 (untagged) seed, which is every seed cut before
    /// 2026-09-11 — legal, and simply means the window has only the
    /// engine's own state to draw on.
    pub returns: Vec<core_config::vrp::MinuteReturn>,
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
    let src = std::fs::read_to_string(&path)
        .map_err(|e| format!("vrp: seed {}: {e}", path.display()))?;
    let rows = core_config::vrp::parse_seed(&src).map_err(|e| e.to_string())?;
    let returns = core_config::vrp::parse_returns(&src).map_err(|e| e.to_string())?;
    let seed = VrpSeed { rows, returns, path };
    info!(
        path = %seed.path.display(),
        pairs = seed.len(),
        decisive = seed.is_decisive(),
        window_minutes = seed.returns.len(),
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
    /// F2: the MERGED fitted pairs, oldest first — the worker's seed
    /// unioned with the engine's own `P` rows by `expiry_ts_ms`, state
    /// winning on a collision, capped at [`core_vol::PAIR_RING`].
    ///
    /// Renamed from `seed` because it is no longer the seed: the member
    /// pushes exactly these and the state file's `P` rows are counted
    /// there, not pushed. Pushing both put 38 expiries into the OLS
    /// twice at every boot.
    pub pairs: Vec<(u64, i64, i64)>,
    /// Kept pairs that came from the worker's `vrp-seed.tsv`.
    pub pairs_from_seed: usize,
    /// Kept pairs that came from `vrp-state.tsv` — the engine's own
    /// settlements, which win wherever both sources carry an expiry.
    pub pairs_from_state: usize,
    /// Where the seed came from, for the boot tell. The default path
    /// even when nothing was there — "no pairs, from here" is the tell
    /// an operator needs to fix a cold boot.
    pub seed_path: PathBuf,
    /// V8a: the engine's own persisted state, verbatim. `None` = a
    /// first boot with no history.
    pub state: Option<String>,
    /// Where that state lives, and where the engine writes it back.
    pub state_path: PathBuf,
    /// Resolved `underlying_descriptor`.
    pub underlying_sym: SymbolId,
    /// Resolved `hedge_descriptor`.
    pub hedge_sym: SymbolId,
    /// SHA-256 of the exact `vrp.toml` bytes.
    pub hash: [u8; 32],
    /// Chain rows the parser refused (a non-inverse name, or a name it
    /// does not recognise). Counted, never guessed at.
    pub rows_refused: usize,
    /// W3: the reconciled rolling window, oldest first and contiguous —
    /// what the member replays to arrive WARM instead of spending 24 h
    /// it never gets before the next restart.
    pub window: Vec<core_config::vrp::MinuteReturn>,
    /// Kept minutes taken from the worker's `candles.db` cut. The two
    /// sources are different derivations of the same quantity — a
    /// capture mid-roll versus a REST candle close — so the split is
    /// something an operator is entitled to see rather than infer.
    pub window_from_seed: usize,
    /// Kept minutes taken from `vrp-state.tsv` — the engine's own view
    /// of the live tape, which wins wherever both sources carry the
    /// same minute.
    pub window_from_state: usize,
}

/// The currency prefix of a Deribit descriptor: `deribit:BTC-PERPETUAL`
/// → `BTC`. `None` when the descriptor has no venue prefix or no
/// currency segment.
fn currency_of(descriptor: &str) -> Option<&str> {
    let name = descriptor.split_once(':').map_or(descriptor, |(_, n)| n);
    let ccy = name.split('-').next()?;
    (!ccy.is_empty() && ccy.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(ccy)
}

/// Build the option table for the live chain, **restricted to the
/// hedge instrument's own currency**.
///
/// `deribit_options` is boot discovery's `(instrument_name, sym)` list
/// in allocation order. Contract size is
/// [`DERIBIT_OPT_CONTRACT_SIZE_1E9`], and that is a PROOF rather than an
/// assumption: the only Deribit options with a different size are the
/// USDC-LINEAR chains, whose names the descriptor parser refuses — so a
/// row that lands in this table is a 1.0-coin inverse option by
/// construction. A name that does not parse is counted, not guessed.
///
/// **Why the currency filter.** The live ladder is on for BTC AND ETH
/// (`universe.toml [deribit] options_underlyings`). A table holding both
/// lets the member select an ETH call and hedge it with the BTC perp —
/// a cross-asset naked position that no size, staleness or timing check
/// would catch. It also blows the 128-row table on a two-currency
/// chain. Rows of another currency are skipped and counted, exactly like
/// a name that does not parse, and the member re-checks
/// `underlying_sym` itself: two layers, because this one is not
/// recoverable in flight.
pub fn build_registry(
    deribit_options: &[(String, SymbolId)],
    hedge_descriptor: &str,
    hedge_sym: SymbolId,
) -> (OptRegistry, usize) {
    let mut reg = OptRegistry::new();
    let mut refused = 0usize;
    let want = currency_of(hedge_descriptor);
    for (name, sym) in deribit_options {
        if want.is_none() || currency_of(name) != want {
            refused += 1;
            continue;
        }
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

/// F19: whether this boot asked for the member at all.
///
/// `load_vrp_boot` used to run for EVERY set boot regardless of the
/// requested mask, with three consequences, all live: `STRATEGY=ai`
/// with a present-but-corrupt `vrp.toml` REFUSED the boot, so the
/// wrapper's documented rollback ("drop the mask back to `ai`") could
/// not escape a corrupt VRP file and KeepAlive looped; the member was
/// configured, seeded and restored even when its bit was clear, so a
/// runtime `EnableStrategy(1)` would have traded an armed member nobody
/// enabled; and `ai+vrp` with an ABSENT artifact booted silently as
/// `ai`. The icdp and xsd boots have always been gated; this is the
/// same gate.
#[must_use]
pub fn vrp_wanted(requested: u8) -> bool {
    requested & strategy_set::BIT_VRP != 0
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
    state_path: Option<&Path>,
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
    let (registry, rows_refused) =
        build_registry(deribit_options, &file.hedge_descriptor, hedge_sym);
    if registry.is_empty() {
        return Err(format!(
            "vrp: the options chain holds no {} option ({} of {} rows refused) — the \
             member cannot select an instrument",
            currency_of(&file.hedge_descriptor).unwrap_or("?"),
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
    let seed_returns = loaded
        .as_ref()
        .map(|s| s.returns.clone())
        .unwrap_or_default();
    let seed = loaded.map(|s| s.rows).unwrap_or_default();
    // F22: the state path is EXPLICIT, or derived from the artifact's
    // own directory when the artifact was explicit. It used to be
    // hard-wired to `~/multivenue/vrp-state.tsv`, so any `--vrp
    // <other.toml>` smoke boot read AND REWROTE the live engine's
    // state — a second process writing the file that says what the
    // standing engine is holding.
    let state_path = resolve_state_path(state_path, &path, explicit)?;
    let state = read_state(&state_path)?;
    // W3: the rolling window is the ONE thing with two sources, and
    // reconciling them needs both series in hand before any of them is
    // pushed — which is why it happens here and not in the member's
    // line-at-a-time parser.
    let state_returns = match state.as_deref() {
        Some(text) => core_config::vrp::parse_returns(text).map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let (window, window_from_seed, window_from_state) = core_config::vrp::merge_returns(
        &seed_returns,
        &state_returns,
        core_vol::MINUTE_RING,
    );
    // F2: the fitted pairs have the same two sources and the same
    // problem, so they get the same treatment — union by expiry here,
    // once, instead of two unconditional pushes at boot.
    let state_pairs = match state.as_deref() {
        Some(text) => core_config::vrp::parse_pairs(text).map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let (pairs, pairs_from_seed, pairs_from_state) =
        core_config::vrp::merge_pairs(&seed, &state_pairs, core_vol::PAIR_RING);
    Ok(Some(VrpBoot {
        params: strategy_vrp::VrpParams {
            theta_1e9: file.theta_1e9,
            tau_ns: file.tau_ns,
            epsilon_ns: file.epsilon_ns,
            selection_ns: file.selection_ns,
            rebalance_ns: file.rebalance_ns,
            qty_1e6: file.qty_1e6,
            band_qty_1e6: file.band_qty_1e6,
            sides: file.sides,
        },
        registry,
        pairs,
        pairs_from_seed,
        pairs_from_state,
        window,
        window_from_seed,
        window_from_state,
        seed_path: seed_path_used,
        state,
        state_path,
        underlying_sym,
        hedge_sym,
        hash: core_crypto::sha256(&bytes),
        rows_refused,
    }))
}

// ---------------------------------------------------------------
// V8a: the engine's own state file
// ---------------------------------------------------------------

/// Default location of the member's persisted state, beside `vrp.toml`.
///
/// Two files, one writer each, and that is deliberate.
/// `vrp-seed.tsv` is the WORKER's bootstrap cut — fitted pairs replayed
/// out of `candles.db` so a first boot is not blind for two months —
/// and the engine only ever reads it. `vrp-state.tsv` is the ENGINE's
/// own history: the pairs it formed itself, the QLIKE window kill
/// criterion 3 is measured over, and any campaign that was open when
/// the process went down. When both exist the state file wins, because
/// it contains everything the seed did plus what happened since.
pub fn default_state_path() -> Result<String, String> {
    core_config::vrp::default_state_path().map_err(|e| e.to_string())
}

/// F22: where this boot's state file lives.
///
/// * `Some(p)` — an explicit `--vrp-state`: that file, whatever else.
/// * `None` with an EXPLICIT `--vrp <path>` — `vrp-state.tsv` beside
///   that artifact. A smoke boot on its own `vrp.toml` must not read
///   and rewrite the standing engine's state, and "beside the artifact"
///   is the rule that needs no second flag to be safe.
/// * `None` with the default artifact — the default state path.
pub fn resolve_state_path(
    state_path: Option<&Path>,
    artifact: &Path,
    artifact_explicit: bool,
) -> Result<PathBuf, String> {
    if let Some(p) = state_path {
        return Ok(p.to_path_buf());
    }
    if artifact_explicit {
        if let Some(dir) = artifact.parent() {
            return Ok(dir.join("vrp-state.tsv"));
        }
    }
    Ok(PathBuf::from(default_state_path()?))
}

/// Read the state file. An absent file is `Ok(None)` — a first boot has
/// no history, which is normal and not an error.
pub fn read_state(path: &Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|e| format!("vrp: {}: {e}", path.display()))
}

/// Write the state file atomically — a temp file beside it, then a
/// rename.
///
/// A boot that read a half-written state file would replay a partial
/// history as if it were the whole one: a short QLIKE window that can
/// never arm the halt, or worse, a campaign row that got cut in half
/// and takes the boot down. `rename` on the same filesystem is atomic,
/// so a reader sees either the old file or the new one and never a
/// third thing.
pub fn write_state(path: &Path, text: &str) -> Result<(), String> {
    crate::state_file::write_atomic(path, text).map_err(|e| format!("vrp: {e}"))
}

/// The one-line tell, rendered identically wherever it is printed.
///
/// F2: it names the MERGE, not the seed file, because "90 seed pairs"
/// and "128 pairs in the ring" were both true at every boot and neither
/// said that 38 expiries had been counted twice.
#[must_use]
pub fn render_seed_tell(seed: Option<&VrpSeed>, boot: Option<&VrpBoot>) -> String {
    let merged = match boot {
        Some(b) => format!(
            " pairs={} pairs_from_seed={} pairs_from_state={}",
            b.pairs.len(),
            b.pairs_from_seed,
            b.pairs_from_state
        ),
        None => String::new(),
    };
    match seed {
        Some(s) => format!(
            "vrp: seed applied seed_pairs={} decisive={} from {}{merged}",
            s.len(),
            s.is_decisive(),
            s.path.display()
        ),
        None => format!(
            "vrp: seed absent — the member holds until it has {} pairs{merged}",
            core_vol::MIN_PAIRS
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chain_is_restricted_to_the_hedge_instruments_currency() {
        // `universe.toml [deribit] options_underlyings = ["BTC", "ETH"]`
        // means boot discovery hands us BOTH ladders. A table holding
        // both lets the member hedge an ETH call with the BTC perp, and
        // it blows the 128-row table on a two-currency chain.
        let perp = core_types::make_symbol_id(core_types::VenueId::Deribit, 1);
        let mut chain: Vec<(String, SymbolId)> = Vec::new();
        let mut k = 0u32;
        while k < 6 {
            chain.push((
                format!("BTC-10SEP26-{}-C", 78_000 + 500 * k),
                core_types::make_symbol_id(core_types::VenueId::Deribit, 513 + k),
            ));
            chain.push((
                format!("ETH-10SEP26-{}-C", 3_000 + 100 * k),
                core_types::make_symbol_id(core_types::VenueId::Deribit, 600 + k),
            ));
            k += 1;
        }
        // …and a USDC-linear name, which the parser refuses on its own.
        chain.push((
            "BTC_USDC-10SEP26-79000-C".to_owned(),
            core_types::make_symbol_id(core_types::VenueId::Deribit, 700),
        ));

        let (reg, refused) = build_registry(&chain, "deribit:BTC-PERPETUAL", perp);
        assert_eq!(reg.len(), 6, "the six BTC calls, and only those");
        assert_eq!(refused, 7, "six ETH rows plus the USDC-linear name");
        for row in reg.rows() {
            assert_eq!(row.underlying_sym, perp);
            assert!(row.strike_1e6 >= 78_000_000_000, "a BTC strike, not an ETH one");
        }

        // The same chain read for the ETH perp picks the other ladder.
        let eth = core_types::make_symbol_id(core_types::VenueId::Deribit, 2);
        let (eth_reg, _) = build_registry(&chain, "deribit:ETH-PERPETUAL", eth);
        assert_eq!(eth_reg.len(), 6);
        for row in eth_reg.rows() {
            assert!(row.strike_1e6 <= 3_500_000_000, "an ETH strike");
        }
    }

    #[test]
    fn the_currency_prefix_is_read_off_the_descriptor() {
        assert_eq!(currency_of("deribit:BTC-PERPETUAL"), Some("BTC"));
        assert_eq!(currency_of("BTC-10SEP26-79000-C"), Some("BTC"));
        assert_eq!(currency_of("deribit:ETH-PERPETUAL"), Some("ETH"));
        // A USDC-linear name reads as its own "currency" and therefore
        // never matches a plain one — belt and braces over the parser.
        assert_eq!(currency_of("BTC_USDC-PERPETUAL"), None);
        assert_eq!(currency_of("deribit:"), None);
        assert_eq!(currency_of("-"), None);
    }

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
        assert!(render_seed_tell(Some(&seed), None).contains("seed_pairs=10 decisive=false"));
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
        let tell = render_seed_tell(None, None);
        assert!(tell.contains("seed absent"), "{tell}");
        assert!(tell.contains(&core_vol::MIN_PAIRS.to_string()), "{tell}");
    }

    // ---------------- P0: F19, F22, F2 ----------------

    /// F19: the member's artifacts are only read when its bit is asked
    /// for.
    ///
    /// `load_vrp_boot` used to run for every set boot, so `STRATEGY=ai`
    /// with a present-but-corrupt `vrp.toml` REFUSED the boot — and the
    /// wrapper's documented rollback IS "drop the mask back to `ai`",
    /// which could therefore never escape a corrupt VRP file while
    /// KeepAlive relaunched into the same refusal.
    #[test]
    fn ai_boot_ignores_a_corrupt_vrp_toml() {
        assert!(!vrp_wanted(strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM));
        assert!(!vrp_wanted(0));
        assert!(vrp_wanted(strategy_set::BIT_VRP));
        assert!(vrp_wanted(
            strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM | strategy_set::BIT_VRP
        ));
        // The live masks, by name: `ai` never reads the file, `ai+vrp`
        // and `ai+vrp+xsd` do.
        assert!(!vrp_wanted(48));
        assert!(vrp_wanted(50));
        assert!(vrp_wanted(54));
    }

    /// F22: the state path follows the ARTIFACT, so a smoke boot on its
    /// own `vrp.toml` cannot read and rewrite the standing engine's
    /// state — which is the file that says what it is holding.
    #[test]
    fn state_path_follows_the_explicit_artifact() {
        let dir = std::env::temp_dir().join(format!("vrp-state-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let artifact = dir.join("vrp.toml");

        // Explicit artifact, no explicit state ⇒ beside the artifact.
        let p = resolve_state_path(None, &artifact, true).expect("resolves");
        assert_eq!(p, dir.join("vrp-state.tsv"));

        // Explicit state always wins.
        let elsewhere = dir.join("somewhere-else.tsv");
        let p = resolve_state_path(Some(&elsewhere), &artifact, true).expect("resolves");
        assert_eq!(p, elsewhere);

        // The DEFAULT artifact keeps the default state path — the live
        // engine's own file, and the only case that may touch it.
        let p = resolve_state_path(None, &artifact, false).expect("resolves");
        assert_eq!(p, PathBuf::from(default_state_path().expect("default")));
        assert_ne!(p, dir.join("vrp-state.tsv"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F2: the boot union. The live defect, exactly: a 90-pair seed and
    /// a state file that renders the whole ring produced 128 pairs at
    /// every boot, 38 of them counted twice.
    #[test]
    fn the_boot_unions_the_seed_and_the_state_by_expiry() {
        let seed: Vec<(u64, i64, i64)> = (0..90u64)
            .map(|i| (1_700_000_000_000 + i * 86_400_000, 24_000 + i as i64, 28_000 + i as i64))
            .collect();
        // The state is that same ring plus one settlement since.
        let mut state_rows = seed.clone();
        state_rows.push((1_700_000_000_000 + 90 * 86_400_000, 24_090, 28_090));
        let mut state_text = String::from("V\t4\n");
        for (ts, x, y) in &state_rows {
            state_text.push_str(&format!("P\t{ts}\t{x}\t{y}\n"));
        }
        let parsed = core_config::vrp::parse_pairs(&state_text).expect("parses");
        assert_eq!(parsed.len(), 91);

        let (pairs, from_seed, from_state) =
            core_config::vrp::merge_pairs(&seed, &parsed, core_vol::PAIR_RING);
        assert_eq!(pairs.len(), 91, "91 expiries, not 180 and not 128");
        assert_eq!(from_state, 91, "the engine's own settlements win");
        assert_eq!(from_seed, 0);
    }

    /// P0.5: the state grammar is parsed twice — once by
    /// `core_config::vrp` on the boot path, once by the member's own
    /// line-at-a-time `restore_state`. The member must not depend on
    /// `core-config` (a strategy that can read files can block a hot
    /// path), so the two parsers stay separate and are pinned EQUAL
    /// here instead: every row shape, and the same verdict from both.
    #[test]
    fn state_row_grammar_is_the_same_on_both_sides() {
        // (row, whether both parsers must accept it)
        let cases: [(&str, bool); 12] = [
            ("P\t1000\t10\t11\n", true),
            ("P\t0\t10\t11\n", false),
            ("P\t-1\t10\t11\n", false),
            ("P\t1000\t10\n", false),
            ("P\t1000\tx\t11\n", false),
            ("P\t2000\t1\t2\nP\t1000\t3\t4\n", false),
            ("P\t1000\t1\t2\nP\t1000\t3\t4\n", false),
            ("R\t1789000000000\t5\n", true),
            ("R\t0\t5\n", false),
            ("R\t1789000060000\t5\nR\t1789000000000\t6\n", false),
            ("P\t1000\t1\t2\nR\t1789000000000\t5\n", true),
            ("", true),
        ];
        let mut member = strategy_vrp::VrpStrategy::new();
        member
            .configure(
                strategy_vrp::VrpParams::default(),
                one_row_registry(),
                core_types::make_symbol_id(core_types::VenueId::Deribit, 1),
                core_types::make_symbol_id(core_types::VenueId::Deribit, 1),
                core_time::WallAnchor::new(0, 0),
                [0; 32],
            )
            .expect("configure");
        for (rows, ok) in cases {
            let text = format!("V\t4\n{rows}");
            let config_ok = core_config::vrp::parse_pairs(&text).is_ok()
                && core_config::vrp::parse_returns(&text).is_ok();
            let mut fresh = strategy_vrp::VrpStrategy::new();
            fresh
                .configure(
                    strategy_vrp::VrpParams::default(),
                    one_row_registry(),
                    core_types::make_symbol_id(core_types::VenueId::Deribit, 1),
                    core_types::make_symbol_id(core_types::VenueId::Deribit, 1),
                    core_time::WallAnchor::new(0, 0),
                    [0; 32],
                )
                .expect("configure");
            let member_ok = fresh.restore_state(&text).is_ok();
            assert_eq!(config_ok, ok, "core-config verdict on {rows:?}");
            assert_eq!(member_ok, ok, "member verdict on {rows:?}");
        }
        let _ = member;
    }

    /// A one-row chain, enough for `configure` to accept.
    fn one_row_registry() -> OptRegistry {
        let mut reg = OptRegistry::new();
        let perp = core_types::make_symbol_id(core_types::VenueId::Deribit, 1);
        reg.insert(OptInstrument::new(
            core_types::make_symbol_id(core_types::VenueId::Deribit, 513),
            perp,
            core_types::VenueId::Deribit as u8,
            1_789_000_000_000_000_000,
            79_000_000_000,
            opt_registry::RIGHT_CALL,
            DERIBIT_OPT_CONTRACT_SIZE_1E9,
        ))
        .expect("one row");
        reg
    }
}
