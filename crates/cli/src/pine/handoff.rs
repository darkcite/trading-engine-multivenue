// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pine handoff` (impl-07 §3.11, §4.8): survivors of a screen → one hand-off directory each for
//! B'3 (`multivenue-engine backtest --strategy pine`). Pre-registered selection (I24: reads no
//! P&L column), the ≤ 2 h windows over the v3 tape (I23), the contract §3.14 manifest, the copy
//! of the script, and the cost before a byte moves.
//!
//! COPY-DOCTRINE: a cold operator tool over files, run by hand; never reached by the engine loop.
//! Offline path: it may allocate. It never calls S3: the archive listing is a FILE (or stdin) the
//! operator saved from `multivenue-archive list --json`.
//!
//! pin:07-archive-fields (recon 2026-10-16, `RECON-T13.md`, one real row read on the Mac):
//! `multivenue-archive list --json` prints JSON LINES, one object per archived run. The keys:
//!
//! | key | type | read here as |
//! |---|---|---|
//! | `run` | str | the run id `run-<epoch_ns>` — must equal `"run-" + epoch_ns` |
//! | `epoch_ns` | int | the run's wall epoch (UTC ns) — the ONLY wall anchor |
//! | `pmlr_version` | int | v3 when `>= 3` (venue time, VT1 — the same test as a local run) |
//! | `span_ns` | [int, int] | `[first_ns, last_ns]` in the capture's OWN clock: only the difference (the duration) is read |
//! | `size_bytes` | int | uncompressed bytes — the cost estimate |
//! | `local_path` | str or null | non-null: materialised in the archive cache (local, no pull) |
//! | `host_id`, `location`, `stored_bytes`, `uploaded_at_ns`, `venues`, `windows_2h_complete` | — | not read |
//!
//! pin:07-archive-span-duration: `span_ns` is not epoch-scale on the recon row, so it is never
//! mapped to UTC (no clock mapping is invented): a window is an OFFSET from the run's first record
//! and its wall time is `epoch_ns + offset` (§3.9, `backtest.rs:1379-1391`); the run's duration is
//! `span_ns[1] − span_ns[0]` — the same quantity `capture_catalog`'s `duration_ns` is for a local
//! run. A row whose span is not two integers with `last ≥ first` is refused (counted
//! `span_refused`, never windowed); a row whose `span_ns[0]` lies within a day of `epoch_ns` is
//! counted `span_epoch_scale`, any other `span_own_clock` — information, not a gate.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use core_time::barclock;
use pine_ir::ExecMode;
use pine_screen::handoff::{
    manifest_toml, null_trial_id, run_windows, t2_trial_id, write_handoff, Handoff, HandoffErr,
    ManifestNums, Window, MIN_SCORE_DAYS, TARGET_CLOSED_T2,
};
use pine_screen::ids::parse_hex32;
use pine_screen::rows::RowsFile;
use pine_screen::{compile_trial, RowsHdr, TrialSpec, SCREEN_LIMITS};

use super::run::{chart_info, compile_text, hex, load_priors, mode_label, open_libs, parse_fees};
use super::universe::{load_universe, UniverseRow};
use super::{json_str, PineErr, JSON_SCHEMA};
use crate::capture_catalog::run_spans;

const DAY_MS: i64 = 86_400_000;
const NS_PER_S: i128 = 1_000_000_000;
/// §4.8.3's fallback when a run's size is unknown: ~19 GiB a day ÷ 12 windows (REVIEW P5).
// pin:07-window-bytes-fallback: "~1.6 GiB per window" taken as 1.6 × 2^30 bytes.
pub const WINDOW_BYTES_FALLBACK: u64 = 1_717_986_918;
/// The 2 h window, ms (the ≤ 2 h law, §4.8.3's capacity arithmetic).
const WINDOW_MS: i64 = 7_200_000;

// ---------------------------------------------------------------------------------------------
// the run directory (shared with `pine report`)
// ---------------------------------------------------------------------------------------------

/// What `run.tsv` says (§3.12.1, plus T13's provenance keys — `run.rs` pin:07-run-tsv-provenance).
#[derive(Debug, Clone)]
pub struct RunDir {
    /// The run directory.
    pub dir: PathBuf,
    /// `screen` or `run`.
    pub verb: String,
    /// `--fee-bps` flags, verbatim, in order.
    pub fee_flags: Vec<String>,
    /// Their digest.
    pub fee_digest: [u8; 32],
    /// Selection notional.
    pub notional_1e6: i64,
    /// Screen window.
    pub from_ms: i64,
    /// Exclusive end.
    pub to_ms: i64,
    /// The candles bars root.
    pub bars_root: PathBuf,
    /// The screen universe TSV.
    pub universe: PathBuf,
    /// The scripts directory (a screen's).
    pub scripts: Option<PathBuf>,
    /// The priors table (a screen's).
    pub priors: Option<PathBuf>,
    /// The library root.
    pub lib_dir: PathBuf,
    /// Timeframes, flag order.
    pub timeframes: Vec<u32>,
    /// `bars <key> <tf> <digest>` lines: (key, tf, digest hex).
    pub bar_files: Vec<(String, u32, String)>,
}

fn data(m: String) -> PineErr {
    PineErr::Data(m)
}

/// Read `<dir>/run.tsv` (exit 5 when absent, malformed, or older than T13's provenance keys).
pub fn read_run_dir(dir: &Path) -> Result<RunDir, PineErr> {
    let p = dir.join("run.tsv");
    let text = std::fs::read_to_string(&p).map_err(|e| data(format!("{}: {e}", p.display())))?;
    let mut lines = text.lines();
    if lines.next() != Some("schema\tpvm7-run-2") {
        return Err(data(format!(
            "{}: not a pvm7-run-2 run directory",
            p.display()
        )));
    }
    let mut kv: BTreeMap<&str, &str> = BTreeMap::new();
    let mut fee_flags = Vec::new();
    let mut bar_files = Vec::new();
    for (i, line) in lines.enumerate() {
        let bad = || data(format!("{} line {}: malformed", p.display(), i + 2));
        let (k, v) = line.split_once('\t').ok_or_else(bad)?;
        match k {
            "fee_bps" => fee_flags.push(v.to_owned()),
            "bars" => {
                let f: Vec<&str> = v.split('\t').collect();
                if f.len() != 3 {
                    return Err(bad());
                }
                bar_files.push((
                    f[0].to_owned(),
                    f[1].parse().map_err(|_| bad())?,
                    f[2].to_owned(),
                ));
            }
            _ => {
                kv.insert(k, v);
            }
        }
    }
    let get = |k: &str| -> Result<&str, PineErr> {
        kv.get(k).copied().ok_or_else(|| {
            data(format!(
                "{}: no `{k}` (a run directory written before T13 — re-run the screen)",
                p.display()
            ))
        })
    };
    let int = |k: &str| -> Result<i64, PineErr> {
        get(k)?
            .parse()
            .map_err(|_| data(format!("{}: bad `{k}`", p.display())))
    };
    let fee_digest = parse_hex32(get("fee_digest")?.as_bytes())
        .ok_or_else(|| data(format!("{}: bad fee_digest", p.display())))?;
    let mut timeframes = Vec::new();
    for t in get("timeframes")?.split(',') {
        timeframes.push(
            t.parse()
                .map_err(|_| data(format!("{}: bad timeframes", p.display())))?,
        );
    }
    Ok(RunDir {
        dir: dir.to_path_buf(),
        verb: get("verb")?.to_owned(),
        fee_flags,
        fee_digest,
        notional_1e6: int("notional_1e6")?,
        from_ms: int("from_ms")?,
        to_ms: int("to_ms")?,
        bars_root: PathBuf::from(get("bars_root")?),
        universe: PathBuf::from(get("universe")?),
        scripts: kv.get("scripts").map(PathBuf::from),
        priors: kv.get("priors").map(PathBuf::from),
        lib_dir: PathBuf::from(get("lib_dir")?),
        timeframes,
        bar_files,
    })
}

/// One `trials.tsv` line (the columns the selector and the report read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialLine {
    /// Trial id (`-` for a dropped / compile-refused trial → `None`).
    pub id: Option<[u8; 32]>,
    /// Script name.
    pub script: String,
    /// Source sha (hex) or `-`.
    pub sha: String,
    /// Canonical key.
    pub key: String,
    /// Timeframe.
    pub tf: u32,
    /// `bar_close` / `realtime` / `-`.
    pub mode: String,
    /// Hurdle verdict.
    pub verdict: String,
    /// `completed` / `faulted` / `refused` / `dropped` / `compile_refused`.
    pub status: String,
}

/// Parse `<dir>/trials.tsv`.
pub fn read_trials(dir: &Path) -> Result<Vec<TrialLine>, PineErr> {
    let p = dir.join("trials.tsv");
    let text = std::fs::read_to_string(&p).map_err(|e| data(format!("{}: {e}", p.display())))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let bad = || data(format!("{} line {}: malformed", p.display(), i + 1));
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 10 {
            return Err(bad());
        }
        out.push(TrialLine {
            id: if f[0] == "-" {
                None
            } else {
                Some(parse_hex32(f[0].as_bytes()).ok_or_else(bad)?)
            },
            script: f[1].to_owned(),
            sha: f[2].to_owned(),
            key: f[4].to_owned(),
            tf: f[5].parse().map_err(|_| bad())?,
            mode: f[6].to_owned(),
            verdict: f[7].to_owned(),
            status: f[8].to_owned(),
        });
    }
    Ok(out)
}

/// One `hurdle.tsv` verdict: (script, key, tf, verdict) — the line's cost fields are not read.
pub type HurdleVerdictLine = (String, String, u32, String);

/// Parse `<dir>/hurdle.tsv` down to its verdicts.
pub fn read_hurdle_verdicts(dir: &Path) -> Result<Vec<HurdleVerdictLine>, PineErr> {
    let p = dir.join("hurdle.tsv");
    let text = std::fs::read_to_string(&p).map_err(|e| data(format!("{}: {e}", p.display())))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let bad = || data(format!("{} line {}: malformed", p.display(), i + 1));
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 4 {
            return Err(bad());
        }
        out.push((
            f[0].to_owned(),
            f[1].to_owned(),
            f[2].parse().map_err(|_| bad())?,
            f[f.len() - 1].to_owned(),
        ));
    }
    Ok(out)
}

/// `--select lowest-clearing-tf` (I24): the lowest timeframe whose hurdle verdict is `clear` and
/// whose tier-1 trial `completed`. Reads verdicts, statuses and the timeframe list — never a row.
pub fn select_lowest_clearing_tf<'a>(
    script: &str,
    symbol: &str,
    timeframes: &[u32],
    hurdles: &[HurdleVerdictLine],
    trials: &'a [TrialLine],
) -> Option<&'a TrialLine> {
    let mut tfs = timeframes.to_vec();
    tfs.sort_unstable();
    for tf in tfs {
        let clear = hurdles
            .iter()
            .any(|h| h.0 == script && h.1 == symbol && h.2 == tf && h.3 == "clear");
        if !clear {
            continue;
        }
        let t = trials.iter().find(|t| {
            t.script == script && t.key == symbol && t.tf == tf && t.status == "completed"
        });
        if let Some(t) = t {
            if t.id.is_some() {
                return Some(t);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// the archive listing (JSON lines) and the local log root
// ---------------------------------------------------------------------------------------------

/// A JSON value of a listing row (flat objects only).
#[derive(Debug, Clone, PartialEq)]
enum JVal {
    Int(i128),
    Str(String),
    Null,
    Bool,
    Num,
    Arr(Vec<JVal>),
}

struct Jp<'a> {
    b: &'a [u8],
    i: usize,
}

impl Jp<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected {:?} at byte {}", c as char, self.i))
        }
    }
    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|_| "bad utf-8".to_owned()),
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("bad escape")?;
                    self.i += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let h = self.b.get(self.i..self.i + 4).ok_or("bad \\u")?;
                            let h = std::str::from_utf8(h).map_err(|_| "bad \\u")?;
                            let cp = u32::from_str_radix(h, 16).map_err(|_| "bad \\u")?;
                            self.i += 4;
                            let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err("bad escape".to_owned()),
                    }
                }
                _ => out.push(c),
            }
        }
        Err("unterminated string".to_owned())
    }
    fn value(&mut self, depth: u32) -> Result<JVal, String> {
        self.ws();
        match self.b.get(self.i) {
            Some(b'"') => Ok(JVal::Str(self.string()?)),
            Some(b'n') if self.b[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(JVal::Null)
            }
            Some(b't') if self.b[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(JVal::Bool)
            }
            Some(b'f') if self.b[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(JVal::Bool)
            }
            Some(b'[') if depth < 2 => {
                self.i += 1;
                let mut v = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(JVal::Arr(v));
                }
                loop {
                    v.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(JVal::Arr(v));
                        }
                        _ => return Err(format!("bad array at byte {}", self.i)),
                    }
                }
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let s = self.i;
                self.i += 1;
                while self.i < self.b.len()
                    && matches!(
                        self.b[self.i],
                        b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'
                    )
                {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[s..self.i]).map_err(|_| "bad number")?;
                Ok(t.parse::<i128>().map_or(JVal::Num, JVal::Int))
            }
            _ => Err(format!("unsupported value at byte {}", self.i)),
        }
    }
}

/// One JSON line → its (key, value) pairs (a flat object; nested objects are refused).
fn parse_json_object(line: &str) -> Result<Vec<(String, JVal)>, String> {
    let mut p = Jp {
        b: line.as_bytes(),
        i: 0,
    };
    p.eat(b'{')?;
    let mut out = Vec::new();
    p.ws();
    if p.b.get(p.i) == Some(&b'}') {
        p.i += 1;
    } else {
        loop {
            let k = p.string()?;
            p.eat(b':')?;
            let v = p.value(0)?;
            out.push((k, v));
            p.ws();
            match p.b.get(p.i) {
                Some(b',') => p.i += 1,
                Some(b'}') => {
                    p.i += 1;
                    break;
                }
                _ => return Err(format!("bad object at byte {}", p.i)),
            }
        }
    }
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("trailing bytes at {}", p.i));
    }
    Ok(out)
}

/// One run of the tape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeRun {
    /// `run-<epoch_ns>`.
    pub run: String,
    /// Wall epoch, ns.
    pub epoch_ns: i64,
    /// Duration in the run's own clock, ns.
    pub dur_ns: i64,
    /// On local disk (the local root, or materialised in the archive cache).
    pub local: bool,
    /// Uncompressed bytes, when known.
    pub bytes: Option<u64>,
}

/// What reading the listing found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListingStats {
    /// Rows read.
    pub rows: u32,
    /// Rows with PMLR version < 3 (not judged, CLAUDE.md pitfall 12).
    pub not_v3: u32,
    /// Rows whose span is not two integers with last ≥ first.
    pub span_refused: u32,
    /// Rows with a zero duration (no window).
    pub empty: u32,
    /// `span_ns[0]` within a day of `epoch_ns`.
    pub span_epoch_scale: u32,
    /// Any other span (the capture's own clock).
    pub span_own_clock: u32,
    /// v3 runs found under `--local-root`.
    pub local_v3: u32,
}

/// Parse the listing text (JSON lines; blank lines skipped). A row missing a read key, or whose
/// `run` is not `run-<epoch_ns>`, refuses the listing (exit 5) naming its line.
pub fn parse_listing(text: &str, st: &mut ListingStats) -> Result<Vec<TapeRun>, PineErr> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let ln = i + 1;
        let obj = parse_json_object(line)
            .map_err(|e| data(format!("--listing line {ln}: {e} (want JSON lines)")))?;
        let get = |k: &str| obj.iter().find(|(n, _)| n == k).map(|(_, v)| v);
        let int = |k: &str| match get(k) {
            Some(JVal::Int(v)) => i64::try_from(*v).ok(),
            _ => None,
        };
        let need = |k: &str| data(format!("--listing line {ln}: no integer/string `{k}`"));
        st.rows += 1;
        let run = match get("run") {
            Some(JVal::Str(s)) => s.clone(),
            _ => return Err(need("run")),
        };
        let epoch_ns = int("epoch_ns").ok_or_else(|| need("epoch_ns"))?;
        if run != format!("run-{epoch_ns}") || epoch_ns <= 0 {
            return Err(data(format!(
                "--listing line {ln}: run {run:?} is not run-<epoch_ns {epoch_ns}>"
            )));
        }
        let ver = int("pmlr_version").ok_or_else(|| need("pmlr_version"))?;
        let local = matches!(get("local_path"), Some(JVal::Str(_)));
        let bytes = int("size_bytes").and_then(|b| u64::try_from(b).ok());
        let span = match get("span_ns") {
            Some(JVal::Arr(v)) if v.len() == 2 => match (&v[0], &v[1]) {
                (JVal::Int(a), JVal::Int(b)) if b >= a => Some((*a, *b)),
                _ => None,
            },
            _ => None,
        };
        // pin:07-v3-is-venue-time: "v3 runs (PMLR version 3)" is read as `pmlr_version >= 3`, the
        // same test a local run passes (`has_venue_time`: header ≥ 3); core_io::VERSION is 3.
        if ver < 3 {
            st.not_v3 += 1;
            continue;
        }
        let Some((a, b)) = span else {
            st.span_refused += 1;
            continue;
        };
        if (a - i128::from(epoch_ns)).abs() <= 86_400_000_000_000 {
            st.span_epoch_scale += 1;
        } else {
            st.span_own_clock += 1;
        }
        let Ok(dur_ns) = i64::try_from(b - a) else {
            st.span_refused += 1;
            continue;
        };
        if dur_ns == 0 {
            st.empty += 1;
            continue;
        }
        out.push(TapeRun {
            run,
            epoch_ns,
            dur_ns,
            local,
            bytes,
        });
    }
    Ok(out)
}

/// The v3 tape: the listing ∪ the local root, by run id, epoch order; overlapping runs refused.
// pin:07-local-wins: a run in both is the local one (its measured duration and bytes, local).
// pin:07-local-harness-ok: a local run counts when v3 AND `harness_ok` (B'3 replays through the
// harness's loader) and its duration is > 0.
// pin:07-listing-duplicate: a run listed twice with different durations refuses the listing.
// pin:07-overlap-refuses: "overlapping runs refused (the harness's own rule)" — any two v3 runs
// whose wall spans `[epoch, epoch + dur)` overlap refuse the hand-off (exit 5), naming them.
pub fn tape(
    listed: Vec<TapeRun>,
    local_root: Option<&Path>,
    st: &mut ListingStats,
) -> Result<Vec<TapeRun>, PineErr> {
    let mut by: BTreeMap<String, TapeRun> = BTreeMap::new();
    for r in listed {
        if let Some(prev) = by.get(&r.run) {
            if prev.dur_ns != r.dur_ns {
                return Err(data(format!(
                    "--listing names {} twice with different spans",
                    r.run
                )));
            }
            continue;
        }
        by.insert(r.run.clone(), r);
    }
    if let Some(root) = local_root {
        let spans = run_spans(root).map_err(|e| data(format!("--local-root: {e}")))?;
        for s in spans {
            if !(s.v3 && s.harness_ok) || s.duration_ns == 0 {
                continue;
            }
            st.local_v3 += 1;
            let r = TapeRun {
                run: s.dir_name.clone(),
                epoch_ns: i64::try_from(s.epoch_ns).unwrap_or(i64::MAX),
                dur_ns: i64::try_from(s.duration_ns).unwrap_or(i64::MAX),
                local: true,
                bytes: Some(s.bytes),
            };
            by.insert(s.dir_name, r);
        }
    }
    let mut runs: Vec<TapeRun> = by.into_values().collect();
    runs.sort_by(|a, b| (a.epoch_ns, &a.run).cmp(&(b.epoch_ns, &b.run)));
    for w in runs.windows(2) {
        if i128::from(w[0].epoch_ns) + i128::from(w[0].dur_ns) > i128::from(w[1].epoch_ns) {
            return Err(data(format!(
                "overlapping runs {} and {} (the harness's own rule refuses them)",
                w[0].run, w[1].run
            )));
        }
    }
    Ok(runs)
}

/// The tape a survivor is given (§4.8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapePlan {
    /// The windows, ascending, chain = the chosen run's ordinal.
    pub windows: Vec<Window>,
    /// Score window `[from, to)`, UTC day boundaries.
    pub score_from_ms: i64,
    /// Exclusive end.
    pub score_to_ms: i64,
    /// Σ durations of the chosen runs, ns.
    pub tape_ns: i128,
    /// Runs chosen (ascending).
    pub runs: Vec<TapeRun>,
}

/// `ceil(TARGET_CLOSED_T2 × 1000 / prior)`.
// pin:07-prior-zero-exhausts: a prior of 0 trades per 1k bars sizes no finite tape: the target is
// `u64::MAX` and the walk takes every v3 run.
pub fn tape_bars_target(prior_trades_per_1k_bars: u32) -> u64 {
    if prior_trades_per_1k_bars == 0 {
        return u64::MAX;
    }
    (u64::from(TARGET_CLOSED_T2) * 1000).div_ceil(u64::from(prior_trades_per_1k_bars))
}

fn score_of(windows: &[Window]) -> (i64, i64) {
    match (windows.first(), windows.last()) {
        (Some(f), Some(l)) => {
            let from = f.from_ms.div_euclid(DAY_MS) * DAY_MS
                + if f.from_ms.rem_euclid(DAY_MS) == 0 {
                    0
                } else {
                    DAY_MS
                };
            let to = l.to_ms.div_euclid(DAY_MS) * DAY_MS;
            (from, to)
        }
        _ => (0, 0),
    }
}

/// Walk the newest runs backwards until `Σ durations ≥ target × tf` AND the chosen runs cover
/// `MIN_SCORE_DAYS` whole UTC days, or the runs are exhausted; cut each into ≤ 2 h windows.
/// `None` when the score window is shorter than `MIN_SCORE_DAYS` (exit 7).
// pin:07-walk-both-conditions: §4.8.3 states the duration target and "the chosen runs cover
// ≥ MIN_SCORE_DAYS whole UTC days" together; the walk stops only when both hold.
pub fn plan_tape(runs: &[TapeRun], target_bars: u64, tf_s: u32) -> Option<TapePlan> {
    let target_ns = i128::from(target_bars).saturating_mul(i128::from(tf_s) * NS_PER_S);
    let n = runs.len();
    let mut m = 0;
    let mut best: Option<TapePlan> = None;
    while m < n {
        m += 1;
        let chosen = &runs[n - m..];
        let mut windows = Vec::new();
        let mut tape_ns = 0i128;
        for (chain, r) in chosen.iter().enumerate() {
            run_windows(&r.run, r.dur_ns, chain as u32, r.local, &mut windows);
            tape_ns += i128::from(r.dur_ns);
        }
        let (sf, stt) = score_of(&windows);
        let days = (stt - sf).div_euclid(DAY_MS);
        let plan = TapePlan {
            windows,
            score_from_ms: sf,
            score_to_ms: stt,
            tape_ns,
            runs: chosen.to_vec(),
        };
        let done = tape_ns >= target_ns && days >= i64::from(MIN_SCORE_DAYS);
        best = Some(plan);
        if done {
            break;
        }
    }
    let p = best?;
    if (p.score_to_ms - p.score_from_ms).div_euclid(DAY_MS) < i64::from(MIN_SCORE_DAYS) {
        return None;
    }
    Some(p)
}

/// The cost of a plan: (runs to pull, estimated raw bytes).
// pin:07-cost-fraction: every chosen run is cut whole, so "the fraction used" is 1: a run's bytes
// are its listing `size_bytes` (local: the catalog's), else WINDOW_BYTES_FALLBACK per window.
pub fn cost_of(p: &TapePlan) -> (u32, u64) {
    let mut pull = 0u32;
    let mut bytes = 0u64;
    for r in &p.runs {
        if !r.local {
            pull += 1;
        }
        bytes = bytes.saturating_add(match r.bytes {
            Some(b) => b,
            None => {
                let w = p.windows.iter().filter(|w| w.run == r.run).count() as u64;
                w.saturating_mul(WINDOW_BYTES_FALLBACK)
            }
        });
    }
    (pull, bytes)
}

// ---------------------------------------------------------------------------------------------
// the verb
// ---------------------------------------------------------------------------------------------

/// The manifest `venue`: the canonical key's venue with its `_<class>` suffix removed.
// pin:07-manifest-venue: canonical venues are `[a-z][a-z0-9_]*`, the `universe.toml` section
// suffixed `_<class>` (pine_ir hash.rs, CAR-00-1) and no section name holds `_`, so the venue is
// the key's venue up to its first `_` (`binance_usdm:btcusdt` → `binance`). impl-08 owns the
// manifest vocabulary; the round-trip test pins this reading.
pub fn manifest_venue(key: &str) -> &str {
    let v = key.split(':').next().unwrap_or(key);
    v.split('_').next().unwrap_or(v)
}

fn read_listing(path: &Path) -> Result<String, PineErr> {
    if path.as_os_str() == "-" {
        // pin:07-listing-stdin: `--listing -` reads the saved listing from stdin.
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| data(format!("--listing -: {e}")))?;
        return Ok(s);
    }
    std::fs::read_to_string(path).map_err(|e| data(format!("--listing {}: {e}", path.display())))
}

fn herr(e: HandoffErr) -> PineErr {
    PineErr::Defect(format!("write_handoff: {e:?}"))
}

/// One survivor's result line.
struct Survivor {
    script: String,
    tf: u32,
    t1: [u8; 32],
    t2: [u8; 32],
    dir: PathBuf,
    plan: TapePlan,
    target: u64,
    pull: u32,
    bytes: u64,
}

/// `pine handoff`.
pub fn handoff_verb(a: &super::HandoffArgs) -> Result<String, PineErr> {
    if a.select != "lowest-clearing-tf" {
        return Err(PineErr::Usage(format!(
            "bad --select {:?}: lowest-clearing-tf is the only accepted value (I24)",
            a.select
        )));
    }
    if a.listing.is_none() && a.local_root.is_none() {
        return Err(PineErr::Usage(
            "pine handoff needs --listing <file|-> and/or --local-root <log root>".to_owned(),
        ));
    }
    let rd = read_run_dir(&a.run)?;
    if rd.verb != "screen" {
        return Err(data(format!(
            "{}: a `{}` run directory — pine handoff reads a screen's",
            a.run.display(),
            rd.verb
        )));
    }
    let fees = parse_fees(&rd.fee_flags)?;
    if fees.digest != rd.fee_digest {
        return Err(PineErr::Defect(
            "run.tsv fee_digest is not its flags'".to_owned(),
        ));
    }
    let rows_p = a.run.join("rows.t1");
    let rows = RowsFile::open(&rows_p).map_err(|e| data(format!("{}: {e:?}", rows_p.display())))?;
    if *rows.hdr() != RowsHdr::new(1, rd.notional_1e6, rd.fee_digest) {
        return Err(data(format!(
            "{}: header disagrees with run.tsv",
            rows_p.display()
        )));
    }
    let trials = read_trials(&a.run)?;
    let hurdles = read_hurdle_verdicts(&a.run)?;
    let priors_p = rd
        .priors
        .clone()
        .ok_or_else(|| data("run.tsv: no priors".to_owned()))?;
    let scripts_d = rd
        .scripts
        .clone()
        .ok_or_else(|| data("run.tsv: no scripts".to_owned()))?;
    let priors = load_priors(&priors_p)?;
    let universe = load_universe(&rd.universe)?;
    let row: Option<&UniverseRow> = universe.iter().find(|r| r.key == a.symbol);
    // Selection first (pure; reads no row).
    let mut picks = Vec::new();
    let mut unselected = Vec::new();
    for p in &priors {
        match select_lowest_clearing_tf(&p.script, &a.symbol, &rd.timeframes, &hurdles, &trials) {
            Some(t) => picks.push((p.clone(), t.clone())),
            None => unselected.push(p.script.clone()),
        }
    }
    let Some(row) = row.filter(|_| !picks.is_empty()) else {
        return Err(PineErr::Handoff(format!(
            "no survivor on {} (no timeframe with a clear hurdle and a completed trial)",
            a.symbol
        )));
    };
    // The tape.
    let mut st = ListingStats::default();
    let listed = match &a.listing {
        Some(p) => parse_listing(&read_listing(p)?, &mut st)?,
        None => Vec::new(),
    };
    let runs = tape(listed, a.local_root.as_deref(), &mut st)?;
    if runs.is_empty() {
        return Err(PineErr::Handoff(
            "no v3 run in the listing or the local root (no v3 window)".to_owned(),
        ));
    }
    let mut libs = open_libs(Some(&rd.lib_dir))?;
    let rows_out = a.t2_rows.clone().unwrap_or_else(|| a.run.join("rows.t2"));
    let ret_out = a.run.join("ret-t2");
    let syminfo = [
        row.ticker.as_str(),
        row.basecurrency.as_str(),
        row.currency.as_str(),
        row.sym_type.as_str(),
    ];
    let mut out = Vec::new();
    for (p, t) in &picks {
        let t1 = t.id.unwrap_or([0; 32]);
        let target = tape_bars_target(p.trades_per_1k_bars);
        let plan = plan_tape(&runs, target, t.tf).ok_or_else(|| {
            PineErr::Handoff(format!(
                "{}: the v3 tape covers fewer than {MIN_SCORE_DAYS} whole UTC days (no score window)",
                p.script
            ))
        })?;
        let script_name = format!("{}.pine", p.script);
        let script_path = scripts_d.join(&script_name);
        let bytes = std::fs::read(&script_path)
            .map_err(|e| data(format!("{}: {e}", script_path.display())))?;
        let source = String::from_utf8(bytes.clone())
            .map_err(|_| data(format!("{}: not UTF-8", script_path.display())))?;
        let spec = TrialSpec {
            source: &source,
            symbol_key: &row.key,
            chart: chart_info(row, t.tf),
            syminfo_strs: syminfo,
            inputs: &[],
            ticker_map: &[],
            limits: SCREEN_LIMITS,
        };
        let loaded = compile_trial(&spec, &mut libs).map_err(|e| {
            data(format!(
                "{}: no longer compiles: {}",
                p.script,
                compile_text(&e)
            ))
        })?;
        if hex(loaded.source_sha256()) != t.sha || mode_label(loaded.mode()) != t.mode {
            return Err(data(format!(
                "{}: the script or its mode changed since the screen (sha/mode differ from trials.tsv)",
                script_path.display()
            )));
        }
        let rec = rows.find(&t1).ok_or_else(|| {
            data(format!(
                "{}: completed trial {} has no rows.t1 row",
                p.script,
                hex(&t1)
            ))
        })?;
        let t2 = t2_trial_id(&t1, &plan.windows);
        let dir = a.run.join("handoff").join(hex(&t2));
        let first_from = plan.windows.first().map_or(0, |w| w.from_ms);
        let mut h = Handoff {
            dir: dir.clone(),
            t1_trial_id: t1,
            t2_trial_id: t2,
            null_trial_id: null_trial_id(&t2),
            script: script_name,
            source_sha256: *loaded.source_sha256(),
            manifest: "manifest.toml".to_owned(),
            symbol_key: row.key.clone(),
            timeframe_s: t.tf,
            mode: loaded.mode(),
            sizing_notional_1e6: rd.notional_1e6,
            fee_flags: rd.fee_flags.clone(),
            fee_digest: rd.fee_digest,
            warmup_bars_root: rd.bars_root.clone(),
            warmup_to_ms: barclock::open_ms(first_from, t.tf),
            score_from_ms: plan.score_from_ms,
            score_to_ms: plan.score_to_ms,
            rows_out: rows_out.clone(),
            ret_out: ret_out.clone(),
            prior_trades_per_1k_bars: p.trades_per_1k_bars,
            tape_bars_target: target,
            windows: plan.windows.clone(),
        };
        let nums = ManifestNums {
            pyramiding: loaded.meta().decl.pyramiding.max(1),
            t1_max_concurrent_1e6: rec.summary.max_concurrent_1e6,
            t1_turnover_day_p99_1e6: rec.summary.turnover_day_p99_1e6,
        };
        let manifest = manifest_toml(
            &h,
            loaded.meta().dialect as u8,
            manifest_venue(&row.key),
            &nums,
            &[],
            &[],
        );
        debug_assert!(matches!(h.mode, ExecMode::BarClose | ExecMode::Realtime));
        write_handoff(&h, &bytes, &manifest).map_err(herr)?;
        let (pull, est) = cost_of(&plan);
        // pin:07-handoff-cost-tsv: the cost is also written as `cost.tsv` (cli's own file beside
        // pine-screen's; `read_handoff` ignores it) so `pine report`'s wiring line can quote it.
        let mut c = String::new();
        let _ = writeln!(c, "windows\t{}", plan.windows.len());
        let _ = writeln!(c, "runs\t{}", plan.runs.len());
        let _ = writeln!(c, "runs_to_pull\t{pull}");
        let _ = writeln!(c, "est_bytes\t{est}");
        std::fs::write(dir.join("cost.tsv"), c)
            .map_err(|e| PineErr::Defect(format!("{}: {e}", dir.display())))?;
        h.dir = dir.clone();
        out.push(Survivor {
            script: p.script.clone(),
            tf: t.tf,
            t1,
            t2,
            dir,
            plan,
            target,
            pull,
            bytes: est,
        });
    }
    Ok(render(a, &st, runs.len(), &out, &unselected))
}

fn windows_needed(target_bars: u64, tf: u32) -> u64 {
    let ms = u128::from(target_bars) * u128::from(tf) * 1000;
    u64::try_from(ms.div_ceil(WINDOW_MS as u128)).unwrap_or(u64::MAX)
}

fn render(
    a: &super::HandoffArgs,
    st: &ListingStats,
    v3_runs: usize,
    out: &[Survivor],
    unselected: &[String],
) -> String {
    let mut s = String::new();
    if a.json {
        s.push_str("{\"schema\":");
        json_str(&mut s, JSON_SCHEMA);
        s.push_str(",\"verb\":\"handoff\",\"run\":");
        json_str(&mut s, &a.run.display().to_string());
        s.push_str(",\"symbol\":");
        json_str(&mut s, &a.symbol);
        let _ = write!(
            s,
            ",\"listing\":{{\"rows\":{},\"not_v3\":{},\"span_refused\":{},\"empty\":{},\"span_epoch_scale\":{},\"span_own_clock\":{},\"local_v3\":{}}},\"v3_runs\":{v3_runs},\"survivors\":[",
            st.rows, st.not_v3, st.span_refused, st.empty, st.span_epoch_scale, st.span_own_clock,
            st.local_v3
        );
        for (i, x) in out.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str("{\"script\":");
            json_str(&mut s, &x.script);
            let _ = write!(s, ",\"timeframe_s\":{},\"t1_trial_id\":", x.tf);
            json_str(&mut s, &hex(&x.t1));
            s.push_str(",\"t2_trial_id\":");
            json_str(&mut s, &hex(&x.t2));
            s.push_str(",\"dir\":");
            json_str(&mut s, &x.dir.display().to_string());
            let _ = write!(
                s,
                ",\"windows\":{},\"runs\":{},\"runs_to_pull\":{},\"est_bytes\":{},\"score_from_ms\":{},\"score_to_ms\":{},\"tape_bars_target\":{},\"tape_bars\":{},\"windows_needed\":{}}}",
                x.plan.windows.len(),
                x.plan.runs.len(),
                x.pull,
                x.bytes,
                x.plan.score_from_ms,
                x.plan.score_to_ms,
                x.target,
                x.plan.tape_ns / (i128::from(x.tf) * NS_PER_S),
                windows_needed(x.target, x.tf)
            );
        }
        s.push_str("],\"unselected\":[");
        for (i, u) in unselected.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            json_str(&mut s, u);
        }
        s.push_str("]}\n");
        return s;
    }
    let _ = writeln!(
        s,
        "listing\trows={}\tnot_v3={}\tspan_refused={}\tempty={}\tspan_epoch_scale={}\tspan_own_clock={}\tlocal_v3={}\tv3_runs={v3_runs}",
        st.rows, st.not_v3, st.span_refused, st.empty, st.span_epoch_scale, st.span_own_clock,
        st.local_v3
    );
    for x in out {
        let days = (x.plan.score_to_ms - x.plan.score_from_ms) / DAY_MS;
        let _ = writeln!(
            s,
            "handoff\t{}\t{}\t{}\tt2={}\twindows={}\truns={}\truns_to_pull={}\test_bytes={}\tscore={}..{} ({days} d)\ttape_bars={}\ttape_bars_target={}\twindows_needed={}\t{}",
            x.script,
            a.symbol,
            x.tf,
            hex(&x.t2),
            x.plan.windows.len(),
            x.plan.runs.len(),
            x.pull,
            x.bytes,
            x.plan.score_from_ms,
            x.plan.score_to_ms,
            x.plan.tape_ns / (i128::from(x.tf) * NS_PER_S),
            x.target,
            windows_needed(x.target, x.tf),
            x.dir.display()
        );
    }
    for u in unselected {
        let _ = writeln!(
            s,
            "unselected\t{u}\t{}\tno clear+completed timeframe",
            a.symbol
        );
    }
    let _ = writeln!(
        s,
        "B'3 streams the runs one at a time (pull -> replay -> evict) under the 25 GiB pull floor; nothing is pulled by this verb"
    );
    s
}
