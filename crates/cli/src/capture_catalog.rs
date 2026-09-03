// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # capture-catalog — offline capture inventory + continuity report (M3)
//!
//! Walks a replay root (`MULTIVENUE_LOG_DIR`, or one `run-<epoch_ns>`
//! dir) and reports what is on disk and what it is good for:
//!
//! * per-run wall spans, per-venue tick coverage + file sizes;
//! * UTC-day continuity: per-day covered/dark time, per-venue per-day
//!   tick counts, the inter-run dark-gap map, gap-free-day streaks
//!   (the mvp-plan §4-M3 "N≥3 consecutive gap-free days" exit tell);
//! * the **backtest view** — the harness's own §3.1 acceptance law
//!   (dir-name parse, PMLR v2, `SlotKind::Tick`, header/dir epoch
//!   cross-check, no wall-overlapping runs) and its §4.5 UTC-day
//!   arithmetic (`wall_ns / NS_PER_DAY` over every tick, wall mapped
//!   exactly like `backtest::load_and_merge`: `epoch_ns + (ts −
//!   run_first_ts)`), plus the `GateThresholds.min_trading_days`
//!   necessary condition;
//! * the **monitor view** — the §8.3 trailing-window arithmetic of
//!   `claude-worker monitor.py` (trailing 24 h anchored at the
//!   capture's own end, run-granular selection, duration-0 runs never
//!   selected, in-window coverage judged against the 6 h floor).
//!
//! JSON (one line, hand-rendered, `catalog_version` 1) on stdout;
//! deterministic human summary on stderr. Determinism law: output is
//! a pure function of the directory contents — no wall-clock reads,
//! no map-iteration nondeterminism (everything sorted), fixed venue
//! order ([`crate::backtest::VENUE_LABELS`]).
//!
//! Non-tick capture files (events / signals / engine-fills / ai-cmds
//! / raw tap — and any FUTURE channel, e.g. the M2.3 options mark/IV
//! records) are aggregated into per-run `other_files` counts + byte
//! totals: a new channel needs NO catalog change to be size-visible;
//! a dedicated coverage row is the designated extension point.
//!
//! ## Doctrine note — this module ALLOCATES
//!
//! Offline tooling under the `audit_replay.rs` doctrine: never loaded
//! by the engine loop, `Vec`/`String` are used freely. Nothing here
//! is reachable from a hot path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use core_io::{PmlrReader, SlotKind};
use core_types::{Tick, TICK_FLAG_STALE};

use crate::backtest::{parse_run_dir_name, pmlr_version_accepted, VENUE_LABELS};

/// ns per UTC day — the harness's §4.5 day divisor, verbatim.
pub const NS_PER_DAY: u64 = 86_400_000_000_000;

/// Default per-day dark-time tolerance for the gap-free verdict:
/// 300 s. Sized for the M3 daily graceful restart (SIGTERM drain is
/// seconds, M1d-proven) plus reconnect blips; flag-tunable.
pub const DEFAULT_GAP_TOLERANCE_NS: u64 = 300_000_000_000;

/// Monitor trailing window, mirroring `claude-worker` `monitor.py`
/// `MONITOR_WINDOW_NS` (§8.3 — trailing 24 h of capture anchored at
/// the capture's own end). Divergence from the worker constant is a
/// doc bug; fix both sides together.
pub const MONITOR_WINDOW_NS: u64 = 86_400_000_000_000;

/// Monitor coverage floor, mirroring `monitor.py` `MONITOR_FLOOR_NS`
/// (§8.3 — below 6 h of in-window coverage the monitor SKIPS).
pub const MONITOR_FLOOR_NS: u64 = 21_600_000_000_000;

/// The worker's `GateThresholds.min_trading_days` default (frozen
/// `backtest.py`): the §5.1 days gate needs `oos_trading_days >= 2`.
/// The catalog reports the NECESSARY condition (capture spanning at
/// least this many UTC days); the gate itself counts OOS FILL days.
pub const MIN_TRADING_DAYS: u64 = 2;

/// Why no catalog could be produced. (An EMPTY root is NOT an error —
/// init-if-empty visibility is an M3 requirement, and "0 runs" is a
/// valid, reportable answer.)
#[derive(Debug)]
pub enum CatalogError {
    /// The root path is missing or not a directory.
    Root(String),
    /// A directory listing failed mid-walk.
    Io(String),
}

impl core::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Root(s) => write!(f, "root: {s}"),
            Self::Io(s) => write!(f, "io: {s}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// One catalog invocation.
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    /// Replay root (`MULTIVENUE_LOG_DIR`) or a single `run-<epoch_ns>`
    /// directory (same §3.1 resolution as `backtest --replay-dir`).
    pub dir: PathBuf,
    /// Max dark ns a UTC day may carry and still count gap-free.
    pub gap_tolerance_ns: u64,
}

/// Per-venue tick-file facts of one run (present files only).
#[derive(Debug, Clone)]
struct VenueFileStat {
    /// Index into [`VENUE_LABELS`].
    lord: usize,
    /// Readable record count (torn tails already excluded by the
    /// reader's tail-tolerant length).
    records: u64,
    /// File size in bytes.
    bytes: u64,
    /// First record's monotonic ts (0 when `records == 0`).
    first_ts_ns: u64,
    /// Last record's monotonic ts (0 when `records == 0`).
    last_ts_ns: u64,
    /// VT4: records the ingress CAPTURED as stale (`TICK_FLAG_STALE`,
    /// the boot-time thresholds — the harness re-judges, this does
    /// not); `None` on a v2 file, which is stale-blind.
    stale_captured: Option<u64>,
    /// Why the harness would refuse this file, when it would
    /// (deterministic text — no OS error strings).
    note: Option<String>,
}

/// One discovered run's catalog facts.
#[derive(Debug, Clone)]
struct RunReport {
    /// Directory basename (`run-<epoch_ns>`).
    dir_name: String,
    /// Boot wall stamp from the dir name.
    epoch_ns: u64,
    /// Wall span start — `epoch_ns` (the harness's §3.3 anchor).
    wall_start_ns: u64,
    /// `epoch_ns + duration` (monitor `RunSpan.end_ns` law).
    wall_end_ns: u64,
    /// `max(last_ts) − min(first_ts)` across tick files — the
    /// monitor's exact `RunSpan.duration_ns`.
    duration_ns: u64,
    /// Total bytes of every direct child file.
    bytes: u64,
    /// Non-tick capture files (events/signals/fills/ai-cmds/tap/…).
    other_files: u64,
    /// Tick records summed over venues.
    ticks: u64,
    /// M2.3: `<venue>-opt-summary.pmlr` records summed over venues —
    /// the options-channel's dedicated coverage row (the C1-designated
    /// extension point; files also remain size-visible in
    /// `other_files`/`bytes`). Best-effort count: an unreadable or
    /// wrong-kind file contributes 0 and never gates `harness_ok`
    /// (the backtest harness reads ticks only, §9.9).
    opt_summaries: u64,
    /// Would `backtest::load_run` accept this run.
    harness_ok: bool,
    /// Present venue tick files, [`VENUE_LABELS`] order.
    venues: Vec<VenueFileStat>,
}

/// One UTC day's continuity facts.
#[derive(Debug, Clone)]
struct DayReport {
    /// Days since the epoch (`wall_ns / NS_PER_DAY`).
    day_index: u64,
    /// Wall time covered by run spans within this day.
    covered_ns: u64,
    /// `NS_PER_DAY − covered_ns`.
    dark_ns: u64,
    /// Ticks whose wall stamp lands in this day.
    ticks: u64,
    /// Per-venue tick counts, [`VENUE_LABELS`] order.
    venue_ticks: [u64; VENUE_LABELS.len()],
    /// Runs whose wall span touches this day.
    runs: u64,
    /// `dark_ns <= tolerance && ticks > 0`.
    gap_free: bool,
}

/// An inter-run dark interval.
#[derive(Debug, Clone)]
struct GapReport {
    /// Wall ns where coverage stopped.
    from_ns: u64,
    /// Wall ns where the next run began.
    to_ns: u64,
    /// Run dir whose END opens the gap.
    after_dir: String,
}

/// A wall-time overlap between runs — the §3.3 condition the harness
/// refuses whole-root replay over.
#[derive(Debug, Clone)]
struct OverlapReport {
    /// Previous coverage end.
    prev_end_ns: u64,
    /// Overlapping run's wall start.
    start_ns: u64,
    /// The overlapping run's dir name.
    dir: String,
}

/// The numeric core of one catalog run — tests assert on these, and
/// both renders are pure functions of them.
#[derive(Debug, Clone, Copy)]
pub struct CatalogFacts {
    /// Discovered `run-*` directories.
    pub runs: u64,
    /// Runs `backtest::load_run` would accept.
    pub harness_ok_runs: u64,
    /// Tick records across all runs.
    pub ticks: u64,
    /// Bytes across all run-dir files.
    pub bytes: u64,
    /// Distinct UTC days holding at least one tick — the harness's
    /// §4.5 `capture_utc_days`, exactly.
    pub capture_utc_days: u64,
    /// Days meeting the gap-free verdict.
    pub gap_free_days: u64,
    /// Longest run of CONSECUTIVE gap-free days.
    pub longest_streak: u64,
    /// Consecutive gap-free days ending at the most recent gap-free
    /// day.
    pub trailing_streak: u64,
    /// Inter-run dark gaps recorded.
    pub gaps: u64,
    /// Wall-overlapping run pairs (harness refuses the root when > 0).
    pub overlaps: u64,
    /// `max(wall_end)` over all runs (0 when empty).
    pub capture_end_ns: u64,
    /// Monitor-selected runs (trailing window, duration > 0).
    pub monitor_selected_runs: u64,
    /// Monitor in-window coverage ns.
    pub monitor_coverage_ns: u64,
    /// `monitor_coverage_ns >= MONITOR_FLOOR_NS`.
    pub monitor_would_run: bool,
    /// Every run harness-ok, no overlaps, ≥ 1 tick — the whole root
    /// can be handed to `backtest --replay-dir` as-is.
    pub whole_root_backtestable: bool,
    /// `capture_utc_days >= MIN_TRADING_DAYS` (necessary, NOT
    /// sufficient — the gate counts OOS fill days).
    pub days_gate_coverage_sufficient: bool,
}

/// What `run_catalog` hands the bin: the exact stdout line, the
/// stderr summary, and the facts both were rendered from.
#[derive(Debug, Clone)]
pub struct CatalogOutput {
    /// One-line JSON, `catalog_version` 1, no trailing newline.
    pub json: String,
    /// Human summary (multi-line, deterministic).
    pub summary: String,
    /// The numeric core.
    pub facts: CatalogFacts,
}

// ---------------------------------------------------------------
// Discovery (harness §3.1 name law; empty root is VALID here)
// ---------------------------------------------------------------

/// A named run dir, pre-inspection.
struct RunEntry {
    path: PathBuf,
    dir_name: String,
    epoch_ns: u64,
}

/// Same resolution as `backtest::discover_runs` — a dir whose own
/// name parses as `run-<ns>` is a single run, otherwise its `run-*`
/// children are the runs — except that an empty root yields an EMPTY
/// list, not an error (init-if-empty visibility).
fn discover(root: &Path) -> Result<Vec<RunEntry>, CatalogError> {
    if !root.is_dir() {
        return Err(CatalogError::Root(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let own_name = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if let Some(epoch_ns) = parse_run_dir_name(own_name) {
        return Ok(vec![RunEntry {
            path: root.to_path_buf(),
            dir_name: own_name.to_owned(),
            epoch_ns,
        }]);
    }
    let rd = std::fs::read_dir(root)
        .map_err(|e| CatalogError::Io(format!("read_dir {}: {e}", root.display())))?;
    let mut runs: Vec<RunEntry> = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| CatalogError::Io(format!("read_dir entry: {e}")))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };
        if let Some(epoch_ns) = parse_run_dir_name(&name) {
            runs.push(RunEntry {
                path,
                dir_name: name,
                epoch_ns,
            });
        }
    }
    // The harness's deterministic order: epoch, then path (§3.1).
    runs.sort_by(|a, b| (a.epoch_ns, &a.dir_name).cmp(&(b.epoch_ns, &b.dir_name)));
    Ok(runs)
}

// ---------------------------------------------------------------
// Per-run inspection
// ---------------------------------------------------------------

/// Inspect one run dir: per-venue tick files (harness §3.1 acceptance
/// law per file), sizes of everything, monitor-law span. Fills
/// `day_ticks` with per-day per-venue tick counts using the harness's
/// exact wall mapping.
fn inspect_run(
    entry: &RunEntry,
    day_ticks: &mut BTreeMap<u64, [u64; VENUE_LABELS.len()]>,
) -> Result<RunReport, CatalogError> {
    let mut venues: Vec<VenueFileStat> = Vec::new();
    let mut bytes = 0u64;
    let mut other_files = 0u64;
    let mut ticks = 0u64;
    let mut any_file = false;
    let mut all_files_ok = true;

    // Sizes: every direct child file counts (events / signals /
    // engine-fills / ai-cmds / raw tap / future channels).
    let rd = std::fs::read_dir(&entry.path)
        .map_err(|e| CatalogError::Io(format!("read_dir {}: {e}", entry.path.display())))?;
    let mut tick_file_names: [bool; VENUE_LABELS.len()] = [false; VENUE_LABELS.len()];
    let mut names: Vec<String> = Vec::new();
    for e in rd {
        let e = e.map_err(|e| CatalogError::Io(format!("read_dir entry: {e}")))?;
        let p = e.path();
        if p.is_file() {
            bytes += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                names.push(n.to_owned());
            }
        }
    }
    for (lord, label) in VENUE_LABELS.iter().enumerate() {
        let tick_name = format!("{label}-ticks.pmlr");
        if names.iter().any(|n| n == &tick_name) {
            tick_file_names[lord] = true;
        }
    }
    for n in &names {
        let is_tick = VENUE_LABELS
            .iter()
            .any(|label| n == &format!("{label}-ticks.pmlr"));
        if !is_tick {
            other_files += 1;
        }
    }

    // Pass 1: open every present tick file, harness acceptance law.
    struct Opened {
        lord: usize,
        stat: VenueFileStat,
        first: u64,
        last: u64,
        reader: Option<PmlrReader<Tick>>,
        ok: bool,
    }
    let mut opened: Vec<Opened> = Vec::new();
    for (lord, label) in VENUE_LABELS.iter().enumerate() {
        if !tick_file_names[lord] {
            continue;
        }
        any_file = true;
        let path = entry.path.join(format!("{label}-ticks.pmlr"));
        let fbytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut stat = VenueFileStat {
            lord,
            records: 0,
            bytes: fbytes,
            first_ts_ns: 0,
            last_ts_ns: 0,
            stale_captured: None,
            note: None,
        };
        let mut ok = false;
        let mut first = 0u64;
        let mut last = 0u64;
        let mut kept: Option<PmlrReader<Tick>> = None;
        match PmlrReader::<Tick>::open(&path) {
            Ok(reader) => {
                if reader.slot_kind() != SlotKind::Tick {
                    stat.note = Some("slot-kind-not-tick".to_owned());
                } else if !pmlr_version_accepted(reader.version()) {
                    stat.note = Some(format!("pmlr-v{}-unsupported", reader.version()));
                } else if reader.epoch_ns() != entry.epoch_ns {
                    stat.note = Some("header-epoch-mismatch".to_owned());
                } else {
                    ok = true;
                    let records = reader.records();
                    stat.records = records.len() as u64;
                    if !records.is_empty() {
                        first = records[0].ts_ns;
                        last = records[records.len() - 1].ts_ns;
                        stat.first_ts_ns = first;
                        stat.last_ts_ns = last;
                    }
                    if reader.has_venue_time() {
                        let mut stale = 0u64;
                        for t in records {
                            stale += u64::from(t.flags & TICK_FLAG_STALE != 0);
                        }
                        stat.stale_captured = Some(stale);
                    }
                    kept = Some(reader);
                }
            }
            Err(_) => {
                // Deterministic note — OS error strings vary.
                stat.note = Some("unreadable-header".to_owned());
            }
        }
        if !ok {
            all_files_ok = false;
        }
        ticks += stat.records;
        opened.push(Opened {
            lord,
            stat,
            first,
            last,
            reader: kept,
            ok,
        });
    }

    // Monitor RunSpan law: duration = max(last) − min(first) over the
    // run's readable tick files; 0 when no complete tick anywhere.
    let mut run_first: Option<u64> = None;
    let mut run_last: Option<u64> = None;
    for o in &opened {
        if o.ok && o.stat.records > 0 {
            run_first = Some(run_first.map_or(o.first, |v| v.min(o.first)));
            run_last = Some(run_last.map_or(o.last, |v| v.max(o.last)));
        }
    }
    let duration_ns = match (run_first, run_last) {
        (Some(f), Some(l)) if l >= f => l - f,
        _ => 0,
    };

    // Pass 2: per-day per-venue tick counts under the harness's §3.3
    // wall mapping — `wall = epoch_ns + (ts − run_first_ts)` with the
    // RUN-level (not per-file) first-ts anchor.
    if let Some(anchor) = run_first {
        for o in &opened {
            if !o.ok || o.stat.records == 0 {
                continue;
            }
            let reader = match &o.reader {
                Some(r) => r,
                None => continue,
            };
            let records = reader.records();
            for t in records {
                let wall = entry.epoch_ns + (t.ts_ns - anchor);
                let day = wall / NS_PER_DAY;
                let row = day_ticks.entry(day).or_insert([0u64; VENUE_LABELS.len()]);
                row[o.lord] += 1;
            }
        }
    }

    let harness_ok = any_file && all_files_ok;
    for o in opened {
        venues.push(o.stat);
    }

    // M2.3: options-channel record counts (best-effort, never gates).
    let mut opt_summaries = 0u64;
    for label in VENUE_LABELS.iter() {
        let path = entry.path.join(format!("{label}-opt-summary.pmlr"));
        if !path.exists() {
            continue;
        }
        if let Ok(r) = PmlrReader::<core_types::OptSummary>::open(&path) {
            if r.slot_kind() == SlotKind::OptSummary {
                opt_summaries += r.len() as u64;
            }
        }
    }

    Ok(RunReport {
        dir_name: entry.dir_name.clone(),
        epoch_ns: entry.epoch_ns,
        wall_start_ns: entry.epoch_ns,
        wall_end_ns: entry.epoch_ns + duration_ns,
        duration_ns,
        bytes,
        other_files,
        ticks,
        opt_summaries,
        harness_ok,
        venues,
    })
}

// ---------------------------------------------------------------
// Civil-date render (Howard Hinnant's civil_from_days)
// ---------------------------------------------------------------

/// `days since 1970-01-01` → `(year, month, day)`, proleptic
/// Gregorian.
fn civil_from_day(day_index: u64) -> (i64, u32, u32) {
    let z = day_index as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` for a day index.
fn fmt_date(day_index: u64) -> String {
    let (y, m, d) = civil_from_day(day_index);
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------
// Renders
// ---------------------------------------------------------------

/// Minimal JSON string escape (quote, backslash, control bytes).
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `h m s` render for a ns duration (deterministic, no floats).
fn fmt_dur(ns: u64) -> String {
    let s_total = ns / 1_000_000_000;
    let h = s_total / 3600;
    let m = (s_total % 3600) / 60;
    let s = s_total % 60;
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else if s > 0 {
        format!("{s}s")
    } else {
        format!("{}ms", ns / 1_000_000)
    }
}

/// Percent with one decimal, integer math.
fn fmt_pct(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0.0%".to_owned();
    }
    let x10 = (part as u128 * 1000 / whole as u128) as u64;
    format!("{}.{}%", x10 / 10, x10 % 10)
}

/// MiB with one decimal, integer math.
fn fmt_mib(bytes: u64) -> String {
    let x10 = bytes as u128 * 10 / (1024 * 1024);
    format!("{}.{}MiB", x10 / 10, x10 % 10)
}

// ---------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------

/// Run one catalog pass. `Err` only when the root itself is
/// unusable; an empty root reports zero runs.
pub fn run_catalog(cfg: &CatalogConfig) -> Result<CatalogOutput, CatalogError> {
    let entries = discover(&cfg.dir)?;

    // Per-run inspection + the global per-day tick map.
    let mut day_ticks: BTreeMap<u64, [u64; VENUE_LABELS.len()]> = BTreeMap::new();
    let mut runs: Vec<RunReport> = Vec::with_capacity(entries.len());
    for e in &entries {
        runs.push(inspect_run(e, &mut day_ticks)?);
    }

    // Coverage sweep (runs already epoch-ordered): effective
    // non-overlapping intervals, dark gaps, overlaps.
    let mut intervals: Vec<(u64, u64)> = Vec::new(); // clipped, disjoint
    let mut gaps: Vec<GapReport> = Vec::new();
    let mut overlaps: Vec<OverlapReport> = Vec::new();
    let mut cursor: Option<(u64, String)> = None; // (coverage end, dir that set it)
    for r in &runs {
        if let Some((end, ref after_dir)) = cursor {
            if r.wall_start_ns > end {
                gaps.push(GapReport {
                    from_ns: end,
                    to_ns: r.wall_start_ns,
                    after_dir: after_dir.clone(),
                });
            } else if r.wall_start_ns < end {
                overlaps.push(OverlapReport {
                    prev_end_ns: end,
                    start_ns: r.wall_start_ns,
                    dir: r.dir_name.clone(),
                });
            }
        }
        let eff_start = match cursor {
            Some((end, _)) => r.wall_start_ns.max(end),
            None => r.wall_start_ns,
        };
        if r.wall_end_ns > eff_start {
            intervals.push((eff_start, r.wall_end_ns));
        }
        let new_end = match cursor {
            Some((end, _)) => end.max(r.wall_end_ns),
            None => r.wall_end_ns,
        };
        let carrier = match &cursor {
            Some((end, dir)) if *end >= r.wall_end_ns => dir.clone(),
            _ => r.dir_name.clone(),
        };
        cursor = Some((new_end, carrier));
    }

    // Day table: from the first to the last day touched by coverage
    // or by a tick.
    let mut days: Vec<DayReport> = Vec::new();
    let day_lo_iv = intervals.first().map(|(s, _)| s / NS_PER_DAY);
    let day_hi_iv = intervals.last().map(|(_, e)| e / NS_PER_DAY);
    let day_lo_tk = day_ticks.keys().next().copied();
    let day_hi_tk = day_ticks.keys().next_back().copied();
    let day_lo = match (day_lo_iv, day_lo_tk) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let day_hi = match (day_hi_iv, day_hi_tk) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    if let (Some(lo), Some(hi)) = (day_lo, day_hi) {
        for day in lo..=hi {
            let day_start = day * NS_PER_DAY;
            let day_end = day_start + NS_PER_DAY;
            let mut covered = 0u64;
            for (s, e) in &intervals {
                let cs = (*s).max(day_start);
                let ce = (*e).min(day_end);
                if ce > cs {
                    covered += ce - cs;
                }
            }
            let mut runs_touching = 0u64;
            for r in &runs {
                if r.wall_start_ns < day_end && r.wall_end_ns >= day_start {
                    runs_touching += 1;
                }
            }
            let venue_ticks = day_ticks
                .get(&day)
                .copied()
                .unwrap_or([0u64; VENUE_LABELS.len()]);
            let ticks: u64 = venue_ticks.iter().sum();
            let dark = NS_PER_DAY - covered.min(NS_PER_DAY);
            days.push(DayReport {
                day_index: day,
                covered_ns: covered,
                dark_ns: dark,
                ticks,
                venue_ticks,
                runs: runs_touching,
                gap_free: dark <= cfg.gap_tolerance_ns && ticks > 0,
            });
        }
    }

    // Streaks over gap-free days.
    let mut longest = 0u64;
    let mut current = 0u64;
    let mut trailing = 0u64;
    let mut streak_end: Option<u64> = None;
    for d in &days {
        if d.gap_free {
            current += 1;
            if current > longest {
                longest = current;
            }
        } else {
            current = 0;
        }
    }
    for d in days.iter().rev() {
        if d.gap_free {
            if trailing == 0 {
                streak_end = Some(d.day_index);
            }
            trailing += 1;
        } else if trailing > 0 {
            break;
        }
    }

    // Monitor view (§8.3 arithmetic, run-granular, capture-anchored).
    let capture_end_ns = runs.iter().map(|r| r.wall_end_ns).max().unwrap_or(0);
    let window_start = capture_end_ns.saturating_sub(MONITOR_WINDOW_NS);
    let mut monitor_selected = 0u64;
    let mut monitor_coverage = 0u64;
    for r in &runs {
        if r.duration_ns == 0 || r.wall_end_ns <= window_start {
            continue; // monitor.py: tickless runs never selected
        }
        monitor_selected += 1;
        monitor_coverage += r.wall_end_ns - r.wall_start_ns.max(window_start);
    }
    let monitor_would_run = monitor_coverage >= MONITOR_FLOOR_NS;

    // Backtest view.
    let harness_ok_runs = runs.iter().filter(|r| r.harness_ok).count() as u64;
    let ticks_total: u64 = runs.iter().map(|r| r.ticks).sum();
    let bytes_total: u64 = runs.iter().map(|r| r.bytes).sum();
    let capture_utc_days = day_ticks.len() as u64;
    let whole_root_backtestable = !runs.is_empty()
        && harness_ok_runs == runs.len() as u64
        && overlaps.is_empty()
        && ticks_total > 0;
    let days_gate_coverage_sufficient = capture_utc_days >= MIN_TRADING_DAYS;

    let facts = CatalogFacts {
        runs: runs.len() as u64,
        harness_ok_runs,
        ticks: ticks_total,
        bytes: bytes_total,
        capture_utc_days,
        gap_free_days: days.iter().filter(|d| d.gap_free).count() as u64,
        longest_streak: longest,
        trailing_streak: trailing,
        gaps: gaps.len() as u64,
        overlaps: overlaps.len() as u64,
        capture_end_ns,
        monitor_selected_runs: monitor_selected,
        monitor_coverage_ns: monitor_coverage,
        monitor_would_run,
        whole_root_backtestable,
        days_gate_coverage_sufficient,
    };

    let json = render_json(cfg, &runs, &days, &gaps, &overlaps, &facts, streak_end);
    let summary = render_summary(cfg, &runs, &days, &gaps, &facts, streak_end);
    Ok(CatalogOutput {
        json,
        summary,
        facts,
    })
}

/// The one-line `catalog_version` 1 JSON (hand-rendered — no
/// serde_json in this crate by doctrine).
#[allow(clippy::too_many_arguments)]
fn render_json(
    cfg: &CatalogConfig,
    runs: &[RunReport],
    days: &[DayReport],
    gaps: &[GapReport],
    overlaps: &[OverlapReport],
    facts: &CatalogFacts,
    streak_end: Option<u64>,
) -> String {
    let mut s = String::with_capacity(4096);
    s.push_str(&format!(
        "{{\"catalog_version\":1,\"root\":\"{}\",\"gap_tolerance_ns\":{},",
        esc(&cfg.dir.display().to_string()),
        cfg.gap_tolerance_ns
    ));
    s.push_str(&format!(
        "\"totals\":{{\"runs\":{},\"ticks\":{},\"bytes\":{}}},",
        facts.runs, facts.ticks, facts.bytes
    ));

    // Per-venue totals, fixed label order.
    s.push_str("\"venue_totals\":[");
    for (lord, label) in VENUE_LABELS.iter().enumerate() {
        let mut present = 0u64;
        let mut ticks = 0u64;
        let mut bytes = 0u64;
        for r in runs {
            for v in &r.venues {
                if v.lord == lord {
                    present += 1;
                    ticks += v.records;
                    bytes += v.bytes;
                }
            }
        }
        if lord > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"venue\":\"{label}\",\"runs_present\":{present},\"ticks\":{ticks},\"bytes\":{bytes}}}"
        ));
    }
    s.push_str("],");

    s.push_str("\"runs\":[");
    for (i, r) in runs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            concat!(
                "{{\"dir\":\"{}\",\"epoch_ns\":{},\"wall_start_ns\":{},\"wall_end_ns\":{},",
                "\"duration_ns\":{},\"bytes\":{},\"other_files\":{},\"ticks\":{},",
                "\"opt_summaries\":{},\"harness_ok\":{},\"venues\":["
            ),
            esc(&r.dir_name),
            r.epoch_ns,
            r.wall_start_ns,
            r.wall_end_ns,
            r.duration_ns,
            r.bytes,
            r.other_files,
            r.ticks,
            r.opt_summaries,
            r.harness_ok
        ));
        for (j, v) in r.venues.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"venue\":\"{}\",\"records\":{},\"bytes\":{},\"first_ts_ns\":{},\"last_ts_ns\":{}",
                VENUE_LABELS[v.lord], v.records, v.bytes, v.first_ts_ns, v.last_ts_ns
            ));
            match v.stale_captured {
                Some(n) => s.push_str(&format!(",\"stale_captured\":{n}")),
                None => s.push_str(",\"stale_captured\":null"),
            }
            if let Some(note) = &v.note {
                s.push_str(&format!(",\"note\":\"{}\"", esc(note)));
            }
            s.push('}');
        }
        s.push_str("]}");
    }
    s.push_str("],");

    s.push_str("\"days\":[");
    for (i, d) in days.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            concat!(
                "{{\"date\":\"{}\",\"day_index\":{},\"covered_ns\":{},\"dark_ns\":{},",
                "\"ticks\":{},\"runs\":{},\"gap_free\":{},\"venue_ticks\":[{},{},{},{},{},{}]}}"
            ),
            fmt_date(d.day_index),
            d.day_index,
            d.covered_ns,
            d.dark_ns,
            d.ticks,
            d.runs,
            d.gap_free,
            d.venue_ticks[0],
            d.venue_ticks[1],
            d.venue_ticks[2],
            d.venue_ticks[3],
            d.venue_ticks[4],
            d.venue_ticks[5]
        ));
    }
    s.push_str("],");

    s.push_str("\"gaps\":[");
    for (i, g) in gaps.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"from_ns\":{},\"to_ns\":{},\"duration_ns\":{},\"after_dir\":\"{}\"}}",
            g.from_ns,
            g.to_ns,
            g.to_ns - g.from_ns,
            esc(&g.after_dir)
        ));
    }
    s.push_str("],");

    s.push_str("\"overlaps\":[");
    for (i, o) in overlaps.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"prev_end_ns\":{},\"start_ns\":{},\"dir\":\"{}\"}}",
            o.prev_end_ns,
            o.start_ns,
            esc(&o.dir)
        ));
    }
    s.push_str("],");

    let streak_end_json = match streak_end {
        Some(d) => format!("\"{}\"", fmt_date(d)),
        None => "null".to_owned(),
    };
    s.push_str(&format!(
        concat!(
            "\"continuity\":{{\"gap_free_days\":{},\"longest_streak\":{},",
            "\"trailing_streak\":{},\"streak_end_date\":{}}},"
        ),
        facts.gap_free_days, facts.longest_streak, facts.trailing_streak, streak_end_json
    ));
    s.push_str(&format!(
        concat!(
            "\"backtest_view\":{{\"harness_ok_runs\":{},\"harness_rejected_runs\":{},",
            "\"whole_root_backtestable\":{},\"capture_utc_days\":{},",
            "\"min_trading_days\":{},\"days_gate_coverage_sufficient\":{}}},"
        ),
        facts.harness_ok_runs,
        facts.runs - facts.harness_ok_runs,
        facts.whole_root_backtestable,
        facts.capture_utc_days,
        MIN_TRADING_DAYS,
        facts.days_gate_coverage_sufficient
    ));
    s.push_str(&format!(
        concat!(
            "\"monitor_view\":{{\"window_ns\":{},\"floor_ns\":{},\"capture_end_ns\":{},",
            "\"selected_runs\":{},\"coverage_ns\":{},\"would_run\":{}}}}}"
        ),
        MONITOR_WINDOW_NS,
        MONITOR_FLOOR_NS,
        facts.capture_end_ns,
        facts.monitor_selected_runs,
        facts.monitor_coverage_ns,
        facts.monitor_would_run
    ));
    s
}

/// Deterministic human summary (stderr).
fn render_summary(
    cfg: &CatalogConfig,
    runs: &[RunReport],
    days: &[DayReport],
    gaps: &[GapReport],
    facts: &CatalogFacts,
    streak_end: Option<u64>,
) -> String {
    let mut s = String::with_capacity(2048);
    s.push_str(&format!(
        "capture-catalog: {} — {} run(s), {} tick(s), {} across {} UTC day(s) with ticks\n",
        cfg.dir.display(),
        facts.runs,
        facts.ticks,
        fmt_mib(facts.bytes),
        facts.capture_utc_days
    ));
    for (i, r) in runs.iter().enumerate() {
        s.push_str(&format!(
            "  run[{i}] {} wall [{}, {}] dur {} {} ticks={} harness={}",
            r.dir_name,
            r.wall_start_ns,
            r.wall_end_ns,
            fmt_dur(r.duration_ns),
            fmt_mib(r.bytes),
            r.ticks,
            if r.harness_ok { "ok" } else { "REJECT" }
        ));
        for v in &r.venues {
            s.push_str(&format!(" {}={}", VENUE_LABELS[v.lord], v.records));
            match v.stale_captured {
                Some(n) if v.records > 0 => s.push_str(&format!("(stale {n})")),
                None if v.records > 0 => s.push_str("(stale-blind v2)"),
                _ => {}
            }
            if let Some(note) = &v.note {
                s.push_str(&format!("[{note}]"));
            }
        }
        s.push('\n');
    }
    if !days.is_empty() {
        s.push_str(&format!(
            "days (gap tolerance {}):\n",
            fmt_dur(cfg.gap_tolerance_ns)
        ));
        for d in days {
            s.push_str(&format!(
                "  {} covered {} dark {} ticks={} runs={} {}\n",
                fmt_date(d.day_index),
                fmt_pct(d.covered_ns, NS_PER_DAY),
                fmt_dur(d.dark_ns),
                d.ticks,
                d.runs,
                if d.gap_free { "GAP-FREE" } else { "gapped" }
            ));
        }
    }
    if !gaps.is_empty() {
        let mut max_gap = 0u64;
        for g in gaps {
            let dur = g.to_ns - g.from_ns;
            if dur > max_gap {
                max_gap = dur;
            }
        }
        s.push_str(&format!(
            "gaps: {} dark interval(s), max {}\n",
            gaps.len(),
            fmt_dur(max_gap)
        ));
    }
    if facts.overlaps > 0 {
        s.push_str(&format!(
            "OVERLAPS: {} run pair(s) overlap in wall time — the harness refuses this root\n",
            facts.overlaps
        ));
    }
    let streak_end_str = match streak_end {
        Some(d) => fmt_date(d),
        None => "-".to_owned(),
    };
    s.push_str(&format!(
        "continuity: {} gap-free day(s); longest streak {}; trailing streak {} (ends {})\n",
        facts.gap_free_days, facts.longest_streak, facts.trailing_streak, streak_end_str
    ));
    s.push_str(&format!(
        "backtest view: {}/{} runs harness-clean, whole-root replay {}; capture spans {} \
         UTC day(s) (days gate needs >= {} — coverage {}; fills still decide)\n",
        facts.harness_ok_runs,
        facts.runs,
        if facts.whole_root_backtestable {
            "OK"
        } else {
            "REFUSED"
        },
        facts.capture_utc_days,
        MIN_TRADING_DAYS,
        if facts.days_gate_coverage_sufficient {
            "sufficient"
        } else {
            "INSUFFICIENT"
        }
    ));
    s.push_str(&format!(
        "monitor view: trailing {} coverage {} vs floor {} — monitor would {}\n",
        fmt_dur(MONITOR_WINDOW_NS),
        fmt_dur(facts.monitor_coverage_ns),
        fmt_dur(MONITOR_FLOOR_NS),
        if facts.monitor_would_run {
            "RUN"
        } else {
            "SKIP"
        }
    ));
    s
}

// ---------------------------------------------------------------
// Tests (unit; fixture-driven integration tests live in
// crates/cli/tests/capture_catalog.rs)
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_exact() {
        assert_eq!(civil_from_day(0), (1970, 1, 1));
        assert_eq!(fmt_date(0), "1970-01-01");
        // 2026-08-22 = 20454 (1970→2026-01-01) + 233.
        assert_eq!(fmt_date(20_687), "2026-08-22");
        // Leap-day sanity: 2024-02-29 = 19782.
        assert_eq!(fmt_date(19_782), "2024-02-29");
    }

    #[test]
    fn duration_render_is_stable() {
        assert_eq!(fmt_dur(0), "0ms");
        assert_eq!(fmt_dur(1_500_000), "1ms");
        assert_eq!(fmt_dur(2_000_000_000), "2s");
        assert_eq!(fmt_dur(62_000_000_000), "1m02s");
        assert_eq!(fmt_dur(3_723_000_000_000), "1h02m03s");
    }

    #[test]
    fn pct_and_mib_renders_are_integer_math() {
        assert_eq!(fmt_pct(0, NS_PER_DAY), "0.0%");
        assert_eq!(fmt_pct(NS_PER_DAY, NS_PER_DAY), "100.0%");
        assert_eq!(fmt_pct(NS_PER_DAY / 2, NS_PER_DAY), "50.0%");
        assert_eq!(fmt_mib(0), "0.0MiB");
        assert_eq!(fmt_mib(1024 * 1024 * 3 / 2), "1.5MiB");
    }

    #[test]
    fn esc_handles_quotes_and_controls() {
        assert_eq!(esc("plain"), "plain");
        assert_eq!(esc("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(esc("x\ny"), "x\\u000ay");
    }

    #[test]
    fn root_must_be_a_directory() {
        let cfg = CatalogConfig {
            dir: PathBuf::from("/definitely/not/a/dir/anywhere-m3"),
            gap_tolerance_ns: DEFAULT_GAP_TOLERANCE_NS,
        };
        match run_catalog(&cfg) {
            Err(CatalogError::Root(_)) => {}
            other => panic!("want Root error, got {other:?}"),
        }
    }

    #[test]
    fn empty_root_is_a_valid_zero_report() {
        let dir = std::env::temp_dir().join(format!(
            "catalog-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cfg = CatalogConfig {
            dir: dir.clone(),
            gap_tolerance_ns: DEFAULT_GAP_TOLERANCE_NS,
        };
        let out = run_catalog(&cfg).expect("empty root catalogs");
        assert_eq!(out.facts.runs, 0);
        assert_eq!(out.facts.ticks, 0);
        assert!(!out.facts.whole_root_backtestable);
        assert!(!out.facts.monitor_would_run);
        assert!(out.json.contains("\"runs\":[]"));
        assert!(out.json.contains("\"catalog_version\":1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
