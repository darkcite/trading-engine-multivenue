// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # har_boot — `har.toml` + the seeds + the engine's own state → the long-tenor set (HAR H3.4)
//!
//! The engine runs the long-tenor HAR over whole UTC days for the series
//! `~/multivenue/har.toml` names (vault plan `har-h3-plan-2026-09-26.md`
//! §13): `core_vol::LongVolSet`, held by `StrategySet` beside the regime
//! detector. This module resolves the file against the boot universe and
//! restores every engine from two sources in `--har-dir`
//! (`~/multivenue/har` by default):
//!
//! * `seed-<NAME>.tsv` — cut hourly by `candles-cycle.sh` from candles.db
//!   (`claude_worker.har_seed seed-out --har-toml`): the feed spliced onto
//!   its fallbacks, the whole 240-day history replayed;
//! * `state-<NAME>.tsv` — the engine's own rows, written at each of the
//!   series' UTC day closes by the state writer thread (`har_writer`,
//!   H3.7) and at shutdown on the engine thread (`paper::write_har_state`).
//!
//! The two are merged by `core_vol::merge_rows` (the §6 day-merge law) and
//! applied through the engine's `seed_*` entry points; one refresh follows
//! (`LongVolSet::restored`).
//!
//! ## Failure isolation (the 09-19 plan §4.5 — the F19 lesson)
//!
//! Nothing here takes the engine down except an EXPLICIT `--har <path>`
//! that cannot be read. An absent default `har.toml` is the pre-H3 boot,
//! bit for bit. A file that does not parse refuses the HAR service with a
//! named error and the engine boots without it. A series whose feed the
//! boot universe does not carry is DROPPED with a named error (the rest
//! run — the seven TradFi perps are appended to `[binance] usdm` at a
//! restart of the operator's choosing, and a forgotten line must not take
//! BTC's forecasts with it). A seed or state file that does not parse is
//! dropped with a named error and the series restores from the other, or
//! boots cold: absent data holds.
//!
//! BOOT DOCTRINE: once per boot; allocation is fine.

use std::path::{Path, PathBuf};

use core_time::{NsTs, WallAnchor};
use core_types::SymbolId;
use core_vol::{LongRows, LongSeries, DAY_NS};
use tracing::{error, info, warn};

/// Where the seeds and the engine's state live unless `--har-dir` says.
pub const HAR_DIR_DEFAULT: &str = "~/multivenue/har";

/// The tenors the boot tell names (the §7 panel's): fitted or not.
const TELL_TENORS: [u64; 9] = [1, 2, 3, 5, 7, 14, 21, 30, 40];

/// One series, resolved and merged.
#[derive(Debug, Clone)]
pub struct HarSeriesBoot {
    /// `har.toml` `name`.
    pub name: String,
    /// `har.toml` `feed`.
    pub feed: String,
    /// The feed's symbol in the boot universe.
    pub sym: SymbolId,
    /// The merged rows the engine is restored from.
    pub rows: LongRows,
    /// Rows the seed contributed before the merge (`None`: absent or dropped).
    pub seed_rows: Option<usize>,
    /// Rows the state contributed before the merge (`None`: absent or dropped).
    pub state_rows: Option<usize>,
    /// `state-<NAME>.tsv`: where the engine writes this series.
    pub state_path: PathBuf,
}

/// Everything the set needs, resolved at boot.
#[derive(Debug, Clone)]
pub struct HarBoot {
    /// The `har.toml` read.
    pub path: PathBuf,
    /// SHA-256 of its exact bytes.
    pub hash: [u8; 32],
    /// The seeds' and states' directory.
    pub dir: PathBuf,
    /// The series whose feeds resolved, in file order.
    pub series: Vec<HarSeriesBoot>,
    /// Series the file names whose feeds the boot universe does not carry.
    pub dropped: Vec<String>,
}

fn expand(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

fn rows_count(r: &LongRows) -> usize {
    r.days.len() + usize::from(r.open.is_some()) + r.arms.len() + r.pairs.len() + r.qlike.len()
}

/// Read one rows file: absent is `None`, unreadable or unparsable is
/// `None` with a named error (the series falls back to the other source).
fn read_rows(path: &Path, kind: &str, name: &str) -> Option<LongRows> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            error!(series = name, kind, path = %path.display(), error = %e, "har: file unreadable — dropped");
            return None;
        }
    };
    match core_vol::parse_rows(&src) {
        Ok(r) => Some(r),
        Err(e) => {
            error!(series = name, kind, path = %path.display(), error = %e, "har: file refused — dropped");
            None
        }
    }
}

/// Resolve `har.toml` and read every series' seed and state.
///
/// * `Err` — ONLY an explicit `--har <path>` that cannot be read: the
///   caller refuses the boot (the operator asked for a file that is not
///   there).
/// * `Ok(None)` — no HAR service this boot: the default file is absent
///   (the pre-H3 boot), or the file is refused (logged, named).
/// * `Ok(Some(_))` — at least one series resolved.
pub fn load_har_boot(
    explicit: Option<&Path>,
    dir: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
) -> Result<Option<HarBoot>, String> {
    let (path, is_explicit) = match explicit {
        Some(p) => (p.to_path_buf(), true),
        None => (
            PathBuf::from(core_config::har::default_har_path().map_err(|e| e.to_string())?),
            false,
        ),
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if is_explicit => return Err(format!("har: {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!(path = %path.display(), "har: no har.toml — the long-tenor HAR is off");
            return Ok(None);
        }
        Err(e) => {
            error!(path = %path.display(), error = %e, "har: har.toml unreadable — the HAR service is OFF");
            return Ok(None);
        }
    };
    let file = match std::str::from_utf8(&bytes)
        .map_err(|_| core_config::har::HarError("not UTF-8".to_owned()))
        .and_then(core_config::har::parse)
    {
        Ok(f) => f,
        Err(e) => {
            error!(path = %path.display(), error = %e, "har: har.toml refused — the HAR service is OFF");
            return Ok(None);
        }
    };
    let dir = dir.map_or_else(|| expand(HAR_DIR_DEFAULT), Path::to_path_buf);
    let mut series = Vec::with_capacity(file.series.len());
    let mut dropped = Vec::new();
    for s in &file.series {
        let Some(sym) = resolve(&s.feed) else {
            error!(
                series = %s.name,
                feed = %s.feed,
                "har: the feed is not in the boot universe — this series is DROPPED (append it \
                 to universe.toml and restart)"
            );
            dropped.push(s.name.clone());
            continue;
        };
        let seed = read_rows(&dir.join(format!("seed-{}.tsv", s.name)), "seed", &s.name);
        let state_path = dir.join(format!("state-{}.tsv", s.name));
        let state = read_rows(&state_path, "state", &s.name);
        let rows = core_vol::merge_rows(seed.as_ref(), state.as_ref());
        series.push(HarSeriesBoot {
            name: s.name.clone(),
            feed: s.feed.clone(),
            sym,
            rows,
            seed_rows: seed.as_ref().map(rows_count),
            state_rows: state.as_ref().map(rows_count),
            state_path,
        });
    }
    if series.is_empty() {
        error!(path = %path.display(), "har: no series resolved — the HAR service is OFF");
        return Ok(None);
    }
    Ok(Some(HarBoot {
        path,
        hash: core_crypto::sha256(&bytes),
        dir,
        series,
        dropped,
    }))
}

/// Configure the set, restore every engine, refresh once, and print the
/// boot tells. `false`: the set refused the configuration (logged) and the
/// engine runs without the HAR service.
pub fn install(
    set: &mut strategy_set::StrategySet,
    boot: &HarBoot,
    anchor: WallAnchor,
    now: NsTs,
) -> bool {
    let series: Vec<LongSeries<'_>> = boot
        .series
        .iter()
        .map(|s| LongSeries {
            name: s.name.as_bytes(),
            feed: s.sym,
        })
        .collect();
    if let Err(e) = set.har_mut().configure(&series, anchor, now) {
        error!(error = %e, "har: the series were refused by the set — the HAR service is OFF");
        return false;
    }
    if let Err(e) = std::fs::create_dir_all(&boot.dir) {
        warn!(dir = %boot.dir.display(), error = %e, "har: state directory missing — state writes will fail");
    }
    let mut refused = [0u32; core_vol::LONG_SET_MAX];
    for (i, s) in boot.series.iter().enumerate() {
        if let Some(e) = set.har_mut().engine_mut(i) {
            let st = core_vol::apply_rows(e, &s.rows);
            refused[i] = st.refused;
        }
    }
    set.har_mut().restored();
    info!(
        hash = %hex32(&boot.hash),
        path = %boot.path.display(),
        dir = %boot.dir.display(),
        series = boot.series.len(),
        dropped = boot.dropped.len(),
        "har: the long-tenor HAR is ON"
    );
    for (i, s) in boot.series.iter().enumerate() {
        let Some(e) = set.har().engine(i) else { continue };
        let fitted: Vec<u64> = TELL_TENORS
            .iter()
            .copied()
            .filter(|&d| e.fit(d * DAY_NS).is_some())
            .collect();
        let source = match (s.seed_rows, s.state_rows) {
            (Some(_), Some(_)) => "seed+state",
            (Some(_), None) => "seed",
            (None, Some(_)) => "state",
            (None, None) => "cold",
        };
        info!(
            series = %s.name,
            feed = %s.feed,
            source,
            days = e.n_resident(),
            warm = e.is_warm(),
            fitted = ?fitted,
            last_min_ts_ms = e.last_min_ts_ms(),
            seed_rows = s.seed_rows.unwrap_or(0),
            state_rows = s.state_rows.unwrap_or(0),
            refused = refused[i],
            "har: series restored"
        );
        if refused[i] > 0 {
            warn!(
                series = %s.name,
                refused = refused[i],
                "har: the engine refused restored rows — re-cut the seed (har_seed seed-out)"
            );
        }
    }
    true
}

fn hex32(h: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in h {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, VenueId};

    /// 2026-01-01 00:00Z.
    const DAY0: u64 = 1_767_225_600_000;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("har-boot-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn resolver(d: &str) -> Option<SymbolId> {
        match d {
            "binance-usdm:btcusdt" => Some(make_symbol_id(VenueId::Binance, 1)),
            "binance-usdm:ethusdt" => Some(make_symbol_id(VenueId::Binance, 2)),
            _ => None,
        }
    }

    const TOML: &str = "[[series]]\nname = \"BTC\"\nfeed = \"binance-usdm:btcusdt\"\n\
                        fallback = [\"binance:btcusdt\"]\n[[series]]\nname = \"MU\"\n\
                        feed = \"binance-usdm:muusdt\"\n[[series]]\nname = \"ETH\"\n\
                        feed = \"binance-usdm:ethusdt\"\n";

    fn engine(days: u64, seed: u64) -> Box<core_vol::LongVolEngine> {
        let mut e = Box::new(core_vol::LongVolEngine::new());
        let mut s = seed;
        let mut px: i64 = 79_000_000_000;
        for m in 0..days * 1440 {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            px = (px + ((s >> 32) % 40_000_001) as i64 - 20_000_000).max(1_000_000_000);
            e.on_minute_close_at(px, DAY0 + m * 60_000);
        }
        e
    }

    fn write_rows(path: &Path, e: &core_vol::LongVolEngine) {
        let mut t = String::from("# test\n");
        e.write_rows(&mut t).unwrap();
        std::fs::write(path, t).unwrap();
    }

    #[test]
    fn an_absent_explicit_file_refuses_the_boot() {
        let d = tmp("absent");
        let err = load_har_boot(Some(&d.join("har.toml")), Some(&d), &resolver).unwrap_err();
        assert!(err.starts_with("har: ") && err.contains("har.toml"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_refused_file_turns_the_service_off_and_never_the_boot() {
        let d = tmp("refused");
        let p = d.join("har.toml");
        std::fs::write(&p, "[[series]]\nname = \"btc\"\n").unwrap();
        assert!(load_har_boot(Some(&p), Some(&d), &resolver).unwrap().is_none());
        std::fs::write(&p, "[[series]]\nname = \"MU\"\nfeed = \"binance-usdm:muusdt\"\n").unwrap();
        assert!(
            load_har_boot(Some(&p), Some(&d), &resolver).unwrap().is_none(),
            "no series resolved"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unresolved_feed_drops_its_series_and_the_rest_restore() {
        let d = tmp("restore");
        let p = d.join("har.toml");
        std::fs::write(&p, TOML).unwrap();
        // BTC: a seed of 100 days and an older state of 40 days; ETH: a
        // corrupt seed and no state — it boots cold.
        let full = engine(100, 7);
        write_rows(&d.join("seed-BTC.tsv"), &full);
        write_rows(&d.join("state-BTC.tsv"), &engine(40, 7));
        std::fs::write(d.join("seed-ETH.tsv"), "V\t1\nD\tx\t1\t1\n").unwrap();
        let boot = load_har_boot(Some(&p), Some(&d), &resolver).unwrap().unwrap();
        assert_eq!(boot.dropped, vec!["MU".to_owned()]);
        assert_eq!(boot.series.len(), 2);
        assert_eq!(boot.hash, core_crypto::sha256(TOML.as_bytes()));
        let btc = &boot.series[0];
        assert!(btc.seed_rows.is_some() && btc.state_rows.is_some());
        assert_eq!(btc.state_path, d.join("state-BTC.tsv"));
        assert_eq!((boot.series[1].seed_rows, boot.series[1].state_rows), (None, None));
        let mut set = strategy_set::StrategySet::new(strategy_set::BIT_AI_EXEC);
        let anchor = WallAnchor::new(0, (DAY0 + 100 * core_vol::DAY_MS) * 1_000_000);
        assert!(install(&mut set, &boot, anchor, 0));
        assert_eq!(set.har().len(), 2);
        assert_eq!(set.har().name(1), Some(&b"ETH"[..]));
        let e = set.har().engine(0).unwrap();
        // The seed's newer days won; the engine forecasts as the seed's did.
        assert_eq!(e.last_min_ts_ms(), full.last_min_ts_ms());
        assert!(e.is_warm());
        for d in [1u64, 7, 30] {
            assert_eq!(e.x_1e9(d * DAY_NS), full.x_1e9(d * DAY_NS), "{d}");
        }
        assert_eq!(set.har().series_epoch(0), 1, "the restore bumps the epoch");
        assert_eq!(set.har().engine(1).unwrap().n_resident(), 0, "ETH cold");
        // The state the set renders reads back through the same reader.
        let mut out = String::new();
        assert!(strategy_core::StrategyCounters::render_har_series(&set, 0, &mut out));
        assert!(out.starts_with("# har-state.tsv v1 (HAR H3) -- BTC:"));
        let back = core_vol::parse_rows(&out).unwrap();
        assert_eq!(back, core_vol::rows_of(e));
        assert!(!strategy_core::StrategyCounters::render_har_series(&set, 2, &mut out));
        let _ = std::fs::remove_dir_all(&d);
    }
}
