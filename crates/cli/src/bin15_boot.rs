// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BIN15 boot: `bin15.toml` + the per-underlying seeds → `Bin15Params`.
//!
//! The member takes no `core-config` dependency (spec §6.4), so this is
//! where the artifact becomes the member's configuration: descriptors
//! resolve against the SAME table the ruleset validator uses, the
//! family list is checked against `universe.toml`'s `rolling` IN ORDER,
//! and the seeds are read with `core_config::vrp::parse_seed` — the
//! same grammar the VRP seeds use, deliberately reused rather than
//! re-written.
//!
//! Two laws from the other lanes apply verbatim:
//!
//! * **Requested-but-absent REFUSES** (the icdp/F19 law). Booting
//!   `ai+bin15` as `ai` because the default `bin15.toml` is missing is
//!   how an operator comes to watch a member that was never there.
//! * **A present-and-unreadable artifact refuses too.** An absent seed
//!   is legal — a cold boot must be, the engine restarts about three
//!   times a day, and the member simply holds until its window warms.

use std::path::{Path, PathBuf};

use core_types::SymbolId;
use tracing::{info, warn};

/// One underlying's boot seed.
///
/// **A pair belongs to ONE tenor.** `x` is `ln har_tau` and `y` is `ln`
/// realised vol over that same `tau`; the 15 m tenor folds four HAR
/// windows and the 8 h tenor folds three, so a quarter-hour pair is not
/// an eight-hour pair. Pushing one cloud into both forecast engines —
/// which is what this struct replaced — fits the daily line on the
/// wrong regressor, and nothing downstream can see that it happened:
/// both are log-vols of the same series, so the fit looks plausible and
/// is not the one the research measured.
///
/// The minute window is different: it IS a property of the price
/// series and not of the horizon, so one series serves both engines.
/// That is why the worker writes it once, in the 15 m file.
#[derive(Debug, Default, Clone)]
pub struct Bin15Seed {
    /// The rolling minute window — one price series, both tenors.
    pub returns: Vec<(u64, i64)>,
    /// The 15 m tenor's fitted pairs (`bin15-seed-<COIN>.tsv`).
    pub pairs_15m: Vec<(u64, i64, i64)>,
    /// The 8 h tenor's fitted pairs (`bin15-seed-<COIN>-1d.tsv`), which
    /// the `native:<COIN>:1d` families price on. Absent file ⇒ empty ⇒
    /// that tenor holds until its own pairs accrue, which is the same
    /// law an absent seed has always had.
    pub pairs_daily: Vec<(u64, i64, i64)>,
}

impl Bin15Seed {
    /// Whether anything was read at all (the boot tell's `seeds=`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.returns.is_empty() && self.pairs_15m.is_empty() && self.pairs_daily.is_empty()
    }
}

/// The file suffix the daily tenor's pair file carries. Mirrored by
/// `claude_worker.bin15_seed.DAILY_SUFFIX`.
pub const DAILY_SUFFIX: &str = "-1d";

/// Everything slot 3 needs to be configured.
#[derive(Debug)]
pub struct Bin15Boot {
    /// The member's configuration.
    pub params: strategy_bin15::Bin15Params,
    /// The lookup tables, boxed once here and moved into the member.
    pub luts: Box<strategy_bin15::price::Bin15Luts>,
    /// sha256 of the artifact BYTES — the boot tell's identity.
    pub hash: [u8; 32],
    /// Per-underlying seeds, in `underlying` order.
    pub seeds: Vec<Bin15Seed>,
    /// The artifact path actually read.
    pub path: PathBuf,
    /// Families whose slot pair resolved.
    pub resolved: usize,
}

/// Whether the operator asked for the member.
#[must_use]
pub fn bin15_wanted(requested: u8) -> bool {
    requested & strategy_set::BIT_BIN15 != 0
}

/// The family kind a `rolling` key names. `None` for anything the
/// grammar does not define — the same two forms
/// `ingress_hyperliquid::family::HlFamilyTable::parse_key` accepts, and
/// crossed forms (`out:BTC:1d`) are refused on both sides.
fn kind_of_key(key: &str) -> Option<u8> {
    let mut parts = key.split(':');
    let head = parts.next()?;
    let _coin = parts.next()?;
    let period = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    match (head, period) {
        ("out", "15m") => Some(strategy_bin15::FAMILY_OUT_15M),
        ("native", "1d") => Some(strategy_bin15::FAMILY_NATIVE_DAILY),
        _ => None,
    }
}

/// The coin a family key names (`out:BTC:15m` → `BTC`).
fn coin_of_key(key: &str) -> Option<&str> {
    key.split(':').nth(1)
}

/// Load and resolve. `Ok(None)` = the artifact is absent at its DEFAULT
/// path, which leaves the member unconfigured and its bit unset; the
/// caller turns that into a refusal when the bit was requested.
///
/// * `artifact` — `--bin15`, or `None` for the default path.
/// * `seed_dir` — `--bin15-seed-dir`, or `None` for `~/multivenue/`.
/// * `resolve` — descriptor → `SymbolId` (the AI descriptor table).
/// * `rolling` — `universe.toml`'s `[hyperliquid] rolling`, in order.
/// * `rolling_syms` — the Yes slot of each rolling family, in the same
///   order, as the universe allocated them.
pub fn load_bin15_boot(
    artifact: Option<&Path>,
    seed_dir: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    rolling: &[String],
    rolling_syms: &[SymbolId],
) -> Result<Option<Bin15Boot>, String> {
    let explicit = artifact.is_some();
    let path: PathBuf = match artifact {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(
            core_config::bin15::default_bin15_path().map_err(|e| e.to_string())?,
        ),
    };
    if !path.exists() {
        if explicit {
            return Err(format!("{}: no such file", path.display()));
        }
        info!(path = %path.display(), "bin15: no artifact — member unconfigured");
        return Ok(None);
    }
    let (file, bytes) =
        core_config::bin15::load(&path).map_err(|e| e.to_string())?;
    let hash = core_crypto::sha256(&bytes);

    // THE ORDER CHECK. The member indexes families by the ingress's own
    // family index (it rides in `InstrumentRoll`'s `venue_seq`), and the
    // ingress indexes by position in `universe.toml`'s `rolling`. So the
    // two lists must be equal AS SEQUENCES, not as sets: a permuted
    // `bin15.toml` would price BTC's book against ETH's forecast and
    // nothing downstream would notice.
    if file.families.len() != rolling.len() {
        return Err(format!(
            "bin15.toml `families` has {} entries but universe.toml `[hyperliquid] \
             rolling` has {} — the member indexes families by the ingress's own \
             index, so the two lists must match exactly and in order",
            file.families.len(),
            rolling.len()
        ));
    }
    let mut i = 0usize;
    while i < rolling.len() {
        if file.families[i] != rolling[i] {
            return Err(format!(
                "bin15.toml `families`[{i}] = `{}` but universe.toml `rolling`[{i}] \
                 = `{}` — same list, same order, or the member prices one family's \
                 book against another's forecast",
                file.families[i], rolling[i]
            ));
        }
        i += 1;
    }
    if rolling_syms.len() != rolling.len() {
        return Err(format!(
            "bin15: the universe allocated {} rolling slot pairs for {} families",
            rolling_syms.len(),
            rolling.len()
        ));
    }

    let mut params = strategy_bin15::Bin15Params::default();
    // The underlyings, resolved against the descriptor table.
    let mut u = 0usize;
    while u < file.underlying.len() {
        let d = &file.underlying[u];
        let Some(sym) = resolve(d) else {
            return Err(format!(
                "bin15.toml `underlying`[{u}] = `{d}` does not resolve against the \
                 instrument manifest — the mark source has to be an instrument the \
                 engine actually subscribes to"
            ));
        };
        params.underlying_sym[u] = sym;
        u += 1;
    }
    params.n_underlyings = file.underlying.len();

    // The families. The Yes slot comes from the UNIVERSE (the rolling
    // ordinals it allocated), not from a descriptor: a rolling slot's
    // venue coin is rebound per instance, so it has no stable
    // descriptor to resolve.
    let mut f = 0usize;
    while f < rolling.len() {
        let key = &file.families[f];
        let Some(kind) = kind_of_key(key) else {
            return Err(format!(
                "bin15.toml `families`[{f}] = `{key}` is not a family key. The \
                 grammar is `out:<COIN>:15m` or `native:<COIN>:1d`; crossed forms \
                 are refused"
            ));
        };
        let Some(coin) = coin_of_key(key) else {
            return Err(format!("bin15.toml `families`[{f}] = `{key}` names no coin"));
        };
        // Which configured underlying this family prices off: the coin
        // in its own key, matched against the tail of the underlying
        // descriptor (`hyperliquid:BTC` → `BTC`).
        let mut found: Option<u8> = None;
        let mut k = 0usize;
        while k < file.underlying.len() {
            if file.underlying[k].rsplit(':').next() == Some(coin) {
                found = Some(k as u8);
                break;
            }
            k += 1;
        }
        let Some(ui) = found else {
            return Err(format!(
                "bin15.toml `families`[{f}] = `{key}` prices off `{coin}`, which is \
                 not in `underlying` — every family needs a mark source"
            ));
        };
        let yes = rolling_syms[f];
        params.sym_yes[f] = yes;
        // The No leg is the NEXT ordinal, which is how the universe
        // allocates the pair and how `InstrumentRoll` names it.
        params.sym_no[f] = yes + 1;
        params.family_underlying[f] = ui;
        params.family_kind[f] = kind;
        f += 1;
    }
    params.n_families = rolling.len();

    params.e_take_1e6 = file.e_take_1e6;
    params.h_quote_1e6 = file.h_quote_1e6;
    // One artifact pair of floors, applied to both kinds — except that
    // a daily binary needs an hour, not a minute (spec §6.4.6, ruling
    // O-Q7). The 15 m floors come from the file; the daily ones are the
    // stated 3600 s, and the file cannot lower them, because a daily
    // quoted a minute before expiry is pure gamma to whoever crosses it.
    params.tau_min_take_ns = [file.tau_min_take_ns, 3_600_000_000_000];
    params.tau_min_quote_ns = [file.tau_min_quote_ns, 3_600_000_000_000];
    params.tail_refuse_ns = file.tail_refuse_ns;
    params.requote_ttl_ns = file.requote_ttl_ns;
    params.requote_thr_1e6 = file.requote_thr_1e6;
    params.clip_qty_1e6 = file.clip_qty_1e6;
    params.cap_instance_usd_1e6 = file.cap_instance_usd_1e6;
    params.cap_day_usd_1e6 = file.cap_day_usd_1e6;
    params.maker_enabled = file.maker_enabled;
    params.null_arm = file.null_arm;
    params.hour_ln_off_1e9 = file.hour_ln_off_1e9;
    params.scale_1e9 = file.scale_1e9;

    let mut luts = Box::new(strategy_bin15::price::Bin15Luts::identity());
    let mut i = 0usize;
    while i < core_config::bin15::PHI_POINTS {
        luts.phi[i] = file.phi_lut[i];
        i += 1;
    }
    let mut ph = 0usize;
    while ph < core_config::bin15::PHASES {
        let mut k = 0usize;
        while k < core_config::bin15::RECAL_POINTS {
            luts.recal[ph][k] = file.recal[ph][k];
            k += 1;
        }
        ph += 1;
    }

    let dir: PathBuf = match seed_dir {
        Some(d) => d.to_path_buf(),
        None => {
            PathBuf::from(core_config::bin15::default_seed_dir().map_err(|e| e.to_string())?)
        }
    };
    let mut seeds: Vec<Bin15Seed> = Vec::new();
    let mut u = 0usize;
    while u < file.underlying.len() {
        let coin = file.underlying[u].rsplit(':').next().unwrap_or("");
        let mut seed = Bin15Seed::default();
        // The 15 m file: the minute window (both tenors read it) plus
        // the quarter-hour tenor's own pairs.
        let sp = dir.join(format!("bin15-seed-{coin}.tsv"));
        if sp.exists() {
            let text = std::fs::read_to_string(&sp)
                .map_err(|e| format!("{}: {e}", sp.display()))?;
            seed.returns = core_config::vrp::parse_returns(&text)
                .map_err(|e| format!("{}: {e}", sp.display()))?;
            seed.pairs_15m = core_config::vrp::parse_pairs(&text)
                .map_err(|e| format!("{}: {e}", sp.display()))?;
            info!(
                path = %sp.display(),
                returns = seed.returns.len(),
                pairs = seed.pairs_15m.len(),
                "bin15: seed loaded"
            );
        } else {
            // A cold boot is legal and the member holds until its own
            // window warms — 15 h of live quarter-hours, or instantly
            // from a seed the worker cuts later.
            warn!(
                path = %sp.display(),
                coin,
                "bin15: no seed — this underlying's forecast starts COLD and the \
                 member holds until the HAR window warms"
            );
        }
        // The daily file: the 8 h tenor's pairs ONLY. Its `R` rows, if
        // a hand-cut file carries any, are deliberately not read — the
        // minute window has one source per underlying, and two would
        // replay the same minutes into a ring that assumes
        // chronological order.
        let dp = dir.join(format!("bin15-seed-{coin}{DAILY_SUFFIX}.tsv"));
        if dp.exists() {
            let text = std::fs::read_to_string(&dp)
                .map_err(|e| format!("{}: {e}", dp.display()))?;
            seed.pairs_daily = core_config::vrp::parse_pairs(&text)
                .map_err(|e| format!("{}: {e}", dp.display()))?;
            info!(
                path = %dp.display(),
                pairs = seed.pairs_daily.len(),
                "bin15: daily seed loaded"
            );
        } else {
            warn!(
                path = %dp.display(),
                coin,
                "bin15: no daily seed — the 8 h tenor starts COLD. Its pairs accrue \
                 one a day, so a native:*:1d family will not price for two months \
                 without this file"
            );
        }
        seeds.push(seed);
        u += 1;
    }

    Ok(Some(Bin15Boot {
        params,
        luts,
        hash,
        seeds,
        path,
        resolved: rolling.len(),
    }))
}

/// The boot tell.
#[must_use]
pub fn render_boot_tell(boot: &Bin15Boot, dormant: usize) -> String {
    let seeded = boot.seeds.iter().filter(|s| !s.is_empty()).count();
    let daily = boot
        .seeds
        .iter()
        .filter(|s| !s.pairs_daily.is_empty())
        .count();
    let mut hex = String::with_capacity(64);
    for b in &boot.hash {
        hex.push_str(&format!("{b:02x}"));
    }
    format!(
        "bin15: artifact configured hash={hex} families={} dormant={dormant} \
         seeds={seeded} daily_seeds={daily}",
        boot.resolved
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_family_key_grammar_accepts_two_forms_and_refuses_crossed_ones() {
        assert_eq!(
            kind_of_key("out:BTC:15m"),
            Some(strategy_bin15::FAMILY_OUT_15M)
        );
        assert_eq!(
            kind_of_key("native:ETH:1d"),
            Some(strategy_bin15::FAMILY_NATIVE_DAILY)
        );
        // Crossed forms are the ones that would silently price a daily
        // off a 15 m forecast.
        assert_eq!(kind_of_key("out:BTC:1d"), None);
        assert_eq!(kind_of_key("native:BTC:15m"), None);
        assert_eq!(kind_of_key("out:BTC"), None);
        assert_eq!(kind_of_key("out:BTC:15m:extra"), None);
        assert_eq!(kind_of_key(""), None);
    }

    #[test]
    fn the_coin_is_the_middle_field() {
        assert_eq!(coin_of_key("out:BTC:15m"), Some("BTC"));
        assert_eq!(coin_of_key("native:HYPE:1d"), Some("HYPE"));
        assert_eq!(coin_of_key("out"), None);
    }

    /// The committed example, which is the artifact the fitter writes.
    const EXAMPLE: &str = include_str!("../../../bin15.toml.example");

    /// The eight families the example configures, in order.
    const ROLLING: [&str; 8] = [
        "out:BTC:15m",
        "out:ETH:15m",
        "out:SOL:15m",
        "out:HYPE:15m",
        "native:BTC:1d",
        "native:ETH:1d",
        "native:SOL:1d",
        "native:HYPE:1d",
    ];

    /// A scratch dir holding the example artifact and whatever seed
    /// files the caller asked for. Returns `(dir, artifact path)`.
    fn scratch(tag: &str, seeds: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("bin15-boot-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let artifact = dir.join("bin15.toml");
        std::fs::write(&artifact, EXAMPLE).expect("artifact");
        for (name, body) in seeds {
            std::fs::write(dir.join(name), body).expect("seed");
        }
        (dir, artifact)
    }

    fn rolling() -> Vec<String> {
        ROLLING.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Yes slots as the universe allocates them: one pair per family.
    fn rolling_syms() -> Vec<SymbolId> {
        (0..ROLLING.len() as u32).map(|f| 900 + f * 2).collect()
    }

    /// Resolves exactly the four descriptors the example names.
    fn resolver(d: &str) -> Option<SymbolId> {
        match d {
            "hyperliquid:BTC" => Some(700),
            "hyperliquid:ETH" => Some(701),
            "hyperliquid:SOL" => Some(702),
            "hyperliquid:HYPE" => Some(703),
            _ => None,
        }
    }

    #[test]
    fn the_committed_example_boots_against_the_universe_it_documents() {
        let (dir, artifact) = scratch("example", &[]);
        let boot = load_bin15_boot(
            Some(&artifact),
            Some(&dir),
            &resolver,
            &rolling(),
            &rolling_syms(),
        )
        .expect("load")
        .expect("configured");
        assert_eq!(boot.resolved, 8);
        assert_eq!(boot.params.n_families, 8);
        assert_eq!(boot.params.n_underlyings, 4);
        // The Yes slot comes from the universe and the No leg is the
        // NEXT ordinal, which is how `InstrumentRoll` names the pair.
        assert_eq!(boot.params.sym_yes[0], 900);
        assert_eq!(boot.params.sym_no[0], 901);
        assert_eq!(boot.params.sym_yes[7], 914);
        // Each family prices off the coin in its own key.
        assert_eq!(boot.params.family_underlying[0], 0, "out:BTC -> hyperliquid:BTC");
        assert_eq!(boot.params.family_underlying[3], 3, "out:HYPE -> hyperliquid:HYPE");
        assert_eq!(boot.params.family_underlying[4], 0, "native:BTC -> hyperliquid:BTC");
        assert_eq!(boot.params.family_kind[0], strategy_bin15::FAMILY_OUT_15M);
        assert_eq!(boot.params.family_kind[4], strategy_bin15::FAMILY_NATIVE_DAILY);
        // The stated fit reaches the member.
        assert_eq!(boot.params.scale_1e9, 980_000_000);
        assert_eq!(boot.luts.phi[0], 500_000);
        assert_eq!(boot.luts.recal[0][0], 0);
        // The daily floors are the member's, and the file cannot lower
        // them: a daily quoted a minute before expiry is pure gamma to
        // whoever crosses it.
        assert_eq!(boot.params.tau_min_take_ns, [60_000_000_000, 3_600_000_000_000]);
        assert_eq!(boot.params.tau_min_quote_ns, [120_000_000_000, 3_600_000_000_000]);
        // A cold boot is legal: four underlyings, no seed files.
        assert_eq!(boot.seeds.len(), 4);
        assert!(boot.seeds.iter().all(Bin15Seed::is_empty));
        assert!(render_boot_tell(&boot, 3).contains("families=8 dormant=3 seeds=0 daily_seeds=0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two-file law. A pair belongs to ONE tenor, so the 15 m file
    /// and the daily file land in different slots — and the minute
    /// window comes from the 15 m file alone, because replaying one
    /// underlying's minutes twice would push them through a ring that
    /// assumes chronological order.
    #[test]
    fn each_tenors_pairs_come_from_its_own_file_and_the_window_from_one() {
        let fifteen = "V\t2\nP\t1000\t11\t12\nP\t2000\t21\t22\nR\t60000\t7\nR\t120000\t8\n";
        // Deliberately carries `R` rows too: a hand-cut daily file must
        // not be able to double the window.
        let daily = "V\t2\nP\t3000\t31\t32\nR\t180000\t9\n";
        let (dir, artifact) = scratch(
            "seeds",
            &[
                ("bin15-seed-BTC.tsv", fifteen),
                ("bin15-seed-BTC-1d.tsv", daily),
            ],
        );
        let boot = load_bin15_boot(
            Some(&artifact),
            Some(&dir),
            &resolver,
            &rolling(),
            &rolling_syms(),
        )
        .expect("load")
        .expect("configured");
        let btc = &boot.seeds[0];
        assert_eq!(btc.returns, vec![(60_000, 7), (120_000, 8)], "the window, once");
        assert_eq!(btc.pairs_15m, vec![(1_000, 11, 12), (2_000, 21, 22)]);
        assert_eq!(btc.pairs_daily, vec![(3_000, 31, 32)]);
        // ETH/SOL/HYPE have no files: cold, and counted as such.
        assert!(boot.seeds[1].is_empty());
        let tell = render_boot_tell(&boot, 0);
        assert!(tell.contains("seeds=1 daily_seeds=1"), "{tell}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_seed_refuses_the_boot_rather_than_booting_blind() {
        let (dir, artifact) = scratch(
            "bad-seed",
            &[("bin15-seed-BTC.tsv", "V\t2\nP\t2000\t1\t2\nP\t1000\t3\t4\n")],
        );
        let e = load_bin15_boot(
            Some(&artifact),
            Some(&dir),
            &resolver,
            &rolling(),
            &rolling_syms(),
        )
        .expect_err("disordered pairs");
        assert!(e.contains("increasing"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_permuted_family_list_is_refused_because_the_index_is_the_identity() {
        let (dir, artifact) = scratch("permuted", &[]);
        let mut permuted = rolling();
        permuted.swap(0, 1);
        let e = load_bin15_boot(
            Some(&artifact),
            Some(&dir),
            &resolver,
            &permuted,
            &rolling_syms(),
        )
        .expect_err("permutation");
        assert!(e.contains("same list, same order"), "{e}");
        // And a shorter universe is refused before any slot is bound.
        let e = load_bin15_boot(
            Some(&artifact),
            Some(&dir),
            &resolver,
            &rolling()[..4],
            &rolling_syms()[..4],
        )
        .expect_err("length");
        assert!(e.contains("must match exactly and in order"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unresolvable_mark_source_is_refused() {
        let (dir, artifact) = scratch("unresolved", &[]);
        let e = load_bin15_boot(
            Some(&artifact),
            Some(&dir),
            &|_d: &str| None,
            &rolling(),
            &rolling_syms(),
        )
        .expect_err("unresolved");
        assert!(e.contains("does not resolve against the instrument manifest"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_explicit_artifact_that_is_absent_is_an_error_not_a_silent_default() {
        let missing = std::env::temp_dir().join("bin15-that-does-not-exist.toml");
        let e = load_bin15_boot(Some(&missing), None, &resolver, &rolling(), &rolling_syms())
            .expect_err("absent");
        assert!(e.contains("no such file"), "{e}");
    }

    #[test]
    fn a_wanted_mask_is_exactly_the_bin15_bit() {
        assert!(bin15_wanted(strategy_set::BIT_BIN15));
        assert!(bin15_wanted(strategy_set::BIT_AI_EXEC | strategy_set::BIT_BIN15));
        assert!(!bin15_wanted(strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM));
        assert!(!bin15_wanted(0));
    }
}
