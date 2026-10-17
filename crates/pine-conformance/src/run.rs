// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! How a fixture reaches `pine-vm` (impl-06 §3.3, §4.5.1): parse → compile → verify → instantiate →
//! the §3.15.3 loop (`host`) → collect, and `check` — every assertion a fixture makes (§4.5.4).
//! Everything before and after the loop allocates freely (load / compare time); the loop does not.

use std::path::{Path, PathBuf};

use pine_ir::{
    Bar, CompileLimits, Diag, DiagCode, Dialect, FaultKind, FileId, PineProgram, RunRecord,
    ScriptKind, SizingMode, SubProgram, SymTable, TradeRec, TvCfg, VmCounters,
};
use pine_vm::VmInstance;

use crate::bars::{ChartSource, SynBars, SynTape};
use crate::chart::{self, spec, Syminfo};
use crate::cmp::compare_cell;
use crate::fixture::{Align, Dialects, Driver, Expect, FixError, Fixture, Oracle, Row, Window};
use crate::host::{
    last_bar, step_chart_bar, step_rt_bar, AlignedBuf, HostFault, SeriesStore, SubRun,
};
use crate::report::{Divergence, Verdict, NO_ROW};

/// Why a fixture did not run. Counted and printed, never a pass or a failure (I20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `RunnerOptions::bars_root` is `None`.
    NoBarsRoot,
    /// No prepared bar file at `bar_path(root, key, tf_s)`.
    NoBarFile {
        /// The timeframe asked for.
        tf_s: u32,
    },
    /// The fixture's tier is outside `CompileLimits::active_tiers`.
    TierInactive {
        /// The fixture's tier.
        tier: u8,
    },
    /// `tv_only` (E106, E110, D060).
    TvOnly,
}

/// Why a run failed. `Debug` is written by hand: the derive lists of the frozen
/// `pine_vm::VerifyError` and `pine_broker::BrokerFault` are impl-03's and impl-05's, and this enum
/// assumes none of them.
pub enum RunError {
    /// 7003.
    Fixture(crate::fixture::FixError),
    /// 7004.
    ChartHash {
        /// The fixture's `chart_sha256`.
        fixture: [u8; 32],
        /// `pine_spec::chart::SHA256`.
        chart: [u8; 32],
    },
    /// 7001 (`err.why()`, impl-07 v4 §3.3).
    BarFile {
        /// The file (or the root, when no path could be formed).
        path: std::path::PathBuf,
        /// `pine-screen`'s refusal.
        err: pine_screen::BarFileErr,
    },
    /// Not run (§4.5.9).
    Skip(SkipReason),
    /// 1000–2999.
    Parse(Vec<Diag>),
    /// 3000–3999, 5001–5007.
    Compile(Vec<Diag>),
    /// 4000 + rule — always OUR defect.
    Verify(pine_vm::VerifyError),
    /// 5100–5199 from `TvBroker::new`.
    BrokerDecl(DiagCode),
    /// A script fault at `pc` on chart bar `bar_index` (`sub`: inside a sub-program).
    Fault {
        /// The fault.
        kind: FaultKind,
        /// `VmInstance::last_fault()`'s pc.
        pc: u32,
        /// The evaluated bar's index.
        bar_index: u32,
        /// Raised by a sub-program.
        sub: bool,
    },
    /// The broker refused a commit.
    Broker(pine_broker::BrokerFault),
    /// The broker dropped closed trades before collection (`closed_retained < closedtrades`).
    TradesEvicted,
}

impl RunError {
    /// The primary `DiagCode`, where one exists (first `Diag`, `DiagCode::verify(rule)`,
    /// 7001/7003/7004, the broker's declaration code).
    pub fn code(&self) -> Option<DiagCode> {
        match self {
            RunError::Fixture(_) => Some(crate::code::FIXTURE_PARSE),
            RunError::ChartHash { .. } => Some(crate::code::CHART_HASH),
            RunError::BarFile { err, .. } => Some(err.code()),
            RunError::Parse(ds) | RunError::Compile(ds) => ds.first().map(|d| d.code),
            RunError::Verify(e) => Some(DiagCode::verify(e.rule)),
            RunError::BrokerDecl(c) => Some(*c),
            RunError::Skip(_)
            | RunError::Fault { .. }
            | RunError::Broker(_)
            | RunError::TradesEvicted => None,
        }
    }
}

impl core::fmt::Debug for RunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RunError::Fixture(e) => write!(f, "Fixture({e})"),
            RunError::ChartHash { .. } => f.write_str("ChartHash"),
            RunError::BarFile { path, err } => {
                write!(f, "BarFile({}: {err})", path.display())
            }
            RunError::Skip(s) => write!(f, "Skip({s:?})"),
            RunError::Parse(ds) => write!(f, "Parse({:?})", first_code(ds)),
            RunError::Compile(ds) => write!(f, "Compile({:?})", first_code(ds)),
            RunError::Verify(e) => write!(f, "Verify(rule {}, pc {})", e.rule, e.pc),
            RunError::BrokerDecl(c) => write!(f, "BrokerDecl({})", c.0),
            RunError::Fault {
                kind,
                pc,
                bar_index,
                sub,
            } => write!(f, "Fault({kind:?}, pc {pc}, bar {bar_index}, sub {sub})"),
            RunError::Broker(b) => write!(f, "Broker({b:?})"),
            RunError::TradesEvicted => f.write_str("TradesEvicted"),
        }
    }
}

fn first_code(ds: &[Diag]) -> Option<u16> {
    ds.first().map(|d| d.code.0)
}

/// The manifest-equivalent limits every fixture compiles under (contract §3.14's example values).
pub const DEFAULT_LIMITS: CompileLimits = CompileLimits {
    max_bars_back: 5000,
    insn_budget: 2_000_000,
    collection_cap: 4096,
    str_persist_bytes: 4096,
    str_cell_bytes: 128,
    str_scratch_bytes: 8192,
    intent_cap: 32,
    max_frame_bytes: 64 << 20,
    active_tiers: 0b0011,
};

/// `pin:06-syminfo-standin` (lead ruling) — until probe-00 writes `syminfo.tsv` (§4.1.4), runs
/// read this clearly-labelled stand-in (ours, `#` header lines say it is not a capture); only a
/// fixture whose oracle is not a TradingView capture may run against it. A `tv` / `tv_only` fixture
/// refuses `7003` naming the missing file. The real file, once present, always wins.
pub const SYMINFO_STANDIN_TSV: &str = "tests/conformance/syminfo.standin.tsv";
/// The root of every run's one `LibCache` (impl-02 v4 §3.3).
pub const LIB_ROOT: &str = "tests/conformance/lib";
/// `capture/signals.tsv` (§4.7.3), read when present.
pub const SIGNALS_TSV: &str = "tests/conformance/capture/signals.tsv";
/// The realtime chart's timeframe (§4.5.3: the live capture chart is 1-minute).
pub const RT_TIMEFRAME_S: u32 = 60;

/// `pin:06-tv-run-tier3` (R-P5, 2026-10-17: tier 3 now) — the TradingView run compiles under
/// [`DEFAULT_LIMITS`] with tier 3 active (`0b0111`): a tier-3 fixture is judged, never skipped.
pub const TV_LIMITS: CompileLimits = CompileLimits {
    active_tiers: 0b0111,
    ..DEFAULT_LIMITS
};

/// Captured fixtures left out of the TradingView run by id. Empty: a fixture waiting for a
/// re-capture (C044 / D053 / D054, R-P3) or a re-paste (F007 / C029 / F085 / E056 / E110, R-P2) is
/// `pending` and the `pending` filter already skips it; once captured it must be judged again, so
/// nothing is excluded by id (r27 review fix 2).
pub const TV_EXCLUDED: &[&str] = &[];

/// A captured TradingView fixture of the run: oracle `tv` / `tv_only`, a captured body (not
/// `pending`), not in [`TV_EXCLUDED`].
pub fn is_tv_captured(f: &Fixture) -> bool {
    f.oracle.is_captured_kind()
        && f.expect != Expect::Pending
        && !TV_EXCLUDED.contains(&f.id.as_str())
}

/// Every `.fix` under `<root>/tests/conformance/fixtures/<group>/`, sorted by group then id.
pub fn load_tree(root: &Path) -> Result<Vec<Fixture>, String> {
    let dir = root.join(crate::FIXTURE_ROOT).join("fixtures");
    let mut groups: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    groups.sort();
    let mut out = Vec::new();
    for g in groups {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&g)
            .map_err(|e| format!("{}: {e}", g.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "fix"))
            .collect();
        files.sort();
        for p in files {
            // COPY: one .fix's bytes (≤ ~1 MiB) per fixture at load — Fixture::parse owns no file —
            // mapping the tree rejected (load time, never a bar step).
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            out.push(Fixture::parse(&text, &p.display().to_string()).map_err(|e| e.to_string())?);
        }
    }
    Ok(out)
}

/// How the runner is pointed at the tree.
#[derive(Debug, Clone)]
pub struct RunnerOptions {
    /// Workspace root: `tests/conformance/` and every fixture's `script` resolve against it.
    pub root: PathBuf,
    /// `pine-screen` bar-file root (`<root>/<venue>/<symbol>/<tf_s>.bars`). `None` skips every
    /// `Chart` / `SynSig` fixture with a counted reason (§4.5.9).
    pub bars_root: Option<PathBuf>,
    /// Compile limits; `active_tiers` is the PVM-4 tier mask in force (T3 lands → `0b0111`).
    pub limits: CompileLimits,
    /// Bars for an align-`Step` fixture whose `bars` is still pending (smoke runs).
    pub syn_bars_default: u32,
    /// The captured `syminfo.tsv` the runner looks for, resolved against `root` (an absolute path
    /// stays as it is); absent, the stand-in is read (`pin:06-syminfo-standin`).
    ///
    /// `pin:06-runner-syminfo-path` — §3.3 fixes the file at [`chart::SYMINFO_TSV`]; the path is
    /// injectable so a test can point the runner at an explicitly absent file and hold the
    /// stand-in's behaviour whether or not the captured file exists in the tree.
    pub syminfo_tsv: PathBuf,
    /// Run `tv_only` fixtures instead of skipping them (`SkipReason::TvOnly`, §4.5.9).
    ///
    /// `pin:06-tv-only-runs-in-report` — §4.5.9 skips `tv_only` in the suite; R-P1 (2026-10-17)
    /// makes every captured fixture, `tv_only` included, an oracle for the divergence report
    /// (`pine-conf report --tv`). The suite keeps the skip (`false`).
    pub run_tv_only: bool,
}

impl RunnerOptions {
    /// `root`, no bars root, [`DEFAULT_LIMITS`], the captured syminfo at [`chart::SYMINFO_TSV`].
    ///
    /// `pin:06-syn-bars-default` — §3.3 names `syn_bars_default`, not its value: 64.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            bars_root: None,
            limits: DEFAULT_LIMITS,
            syn_bars_default: 64,
            syminfo_tsv: PathBuf::from(chart::SYMINFO_TSV),
            run_tv_only: false,
        }
    }
}

/// Where the run's `syminfo.tsv` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyminfoSource {
    /// `tests/conformance/syminfo.tsv` (probe-00's capture).
    Captured,
    /// [`SYMINFO_STANDIN_TSV`].
    StandIn,
    /// Neither file: every fixture refuses `7003`.
    Missing,
}

/// The closed trades a TV broker dropped (it retains the newest 9 000, impl-05 I12), and their
/// totals — what `cum_pnl` of the first kept trade already holds (CI-029, E109).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Evicted {
    /// `closedtrades − closed_retained`.
    pub trades: u32,
    /// The dropped trades' net P&L (`netprofit` − the retained closed trades').
    pub net_1e12: i128,
    /// The dropped trades' commission (`commission_paid` − every listed trade's fee).
    pub fee_1e12: i128,
}

/// What one run produced.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The dialect run.
    pub dialect: Dialect,
    /// Compared rows, row-major: `keys.len() × plot_ix.len()` values; NaN is `na`.
    pub plots: Vec<f64>,
    /// Row keys of the compared window (step index for align `Step`, bar open ms for `Time`).
    pub keys: Vec<i64>,
    /// `ProgramMeta::plots` index of each `Fixture::plots` entry, matched by title.
    pub plot_ix: Vec<u16>,
    /// Closed trades oldest first, then open trades oldest first (strategies only).
    pub trades: Vec<TradeRec>,
    /// The broker's run record (strategies whose expectation carries trades).
    pub record: Option<RunRecord>,
    /// The closed trades the broker dropped from its retained list (`pin:06-trade-trim-kept`).
    pub evicted: Evicted,
    /// REQ-r33g-1: the broker's net profit ×1e12 just after each retained closed trade was booked
    /// (`TvBroker::closed_cum_net`), parallel to the closed part of `trades` — TradingView's List
    /// of Trades `cum_pnl`.
    pub cum_net: Vec<i128>,
    /// Chart bars stepped.
    pub bars: u32,
    /// Evaluations (parent and sub-programs, intrabar included).
    pub evaluations: u64,
    /// The parent instance's counters.
    pub counters: VmCounters,
    /// SHA-256 of the compiled text.
    pub source_sha256: [u8; 32],
    /// The program's symbol table.
    ///
    /// `pin:06-outcome-carries-syms` — §3.7's `compare_trades` reads entry/exit ids through the
    /// program's `SymTable`, and §3.3's outcome is all `check` holds after the run.
    pub syms: SymTable,
}

/// Called around the §3.15.3 loop and nowhere else. Generic, never `dyn` (HFT law).
pub trait LoopProbe {
    /// Just before the first chart bar.
    fn before_loop(&mut self);
    /// Just after the last chart bar (or the fault).
    fn after_loop(&mut self);
}

/// The probe of every normal run.
pub struct NoProbe;

impl LoopProbe for NoProbe {
    fn before_loop(&mut self) {}
    fn after_loop(&mut self) {}
}

/// Parses, compiles, runs and checks fixtures. One `LibCache` and one `ChartSource` per runner.
pub struct Runner {
    opts: RunnerOptions,
    si: Option<Syminfo>,
    si_src: SyminfoSource,
    signals: crate::trades::SignalTable,
    libs: pine_sema::LibCache,
    chart: ChartSource,
    fault_key: Option<i64>,
}

fn fix_err(path: &str, line: u32, what: String) -> RunError {
    RunError::Fixture(FixError {
        path: path.to_string(),
        line,
        what,
    })
}

fn load_syminfo(
    root: &Path,
    captured: &Path,
) -> Result<(Option<Syminfo>, SyminfoSource), RunError> {
    // COPY: syminfo.tsv's bytes (< 1 KiB) once per Runner, at load — the parser owns no file —
    // mapping the file rejected (load time, eight lines).
    match std::fs::read_to_string(root.join(captured)) {
        Ok(t) => {
            return chart::parse_syminfo(&t)
                .map(|s| (Some(s), SyminfoSource::Captured))
                .map_err(|w| fix_err(chart::SYMINFO_TSV, 0, w));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(fix_err(chart::SYMINFO_TSV, 0, e.to_string())),
    }
    // COPY: the stand-in's bytes (< 1 KiB) once per Runner, at load — the parser owns no file —
    // mapping the file rejected (load time, twelve lines).
    match std::fs::read_to_string(root.join(SYMINFO_STANDIN_TSV)) {
        Ok(t) => {
            let mut body = String::with_capacity(t.len());
            for l in t.lines().filter(|l| !l.starts_with('#')) {
                body.push_str(l);
                body.push('\n');
            }
            chart::parse_syminfo(&body)
                .map(|s| (Some(s), SyminfoSource::StandIn))
                .map_err(|w| fix_err(SYMINFO_STANDIN_TSV, 0, w))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, SyminfoSource::Missing)),
        Err(e) => Err(fix_err(SYMINFO_STANDIN_TSV, 0, e.to_string())),
    }
}

/// v6 when declared, else v5 — the dialect whose outcome the expectation is held to (§4.5.4).
pub fn primary_dialect(ds: Dialects) -> Dialect {
    if ds.has(Dialect::V6) {
        Dialect::V6
    } else {
        Dialect::V5
    }
}

/// Sub-programs of `p`, depth-first, children before parents, one per distinct key (§4.5.1 step 9).
fn flatten<'a>(p: &'a PineProgram, out: &mut Vec<&'a SubProgram>) {
    let mut i = 0usize;
    while i < p.subprograms.len() {
        let sp = &p.subprograms[i];
        flatten(&sp.program, out);
        if !out.iter().any(|o| o.key == sp.key) {
            out.push(sp);
        }
        i += 1;
    }
}

/// The `SynBars` length of a `Syn` fixture: its `bars`, else `default`.
///
/// `pin:06-fault-bars-pending` (r33g, E110) — a `fault` expectation whose `bars` is pending (the
/// `--paste runtime-error` capture: TradingView halted, so no row count was exported) steps at
/// least `row + 1` bars: TradingView reached its fault row, so it had that many; nothing after the
/// fault is observable on either side.
fn syn_bars(f: &Fixture, default: u32) -> u32 {
    match (f.bars, &f.expect) {
        (Some(n), _) => n,
        (None, Expect::Fault { row, .. }) => {
            let need = u32::try_from(row.saturating_add(1)).unwrap_or(u32::MAX);
            default.max(need)
        }
        (None, _) => default,
    }
}

/// `[lo, hi)` of the compared rows among `n` rows whose open times ascend as `open_of(i)`.
fn window_range(w: Window, n: u32, open_of: &impl Fn(u32) -> i64) -> (u32, u32) {
    match w {
        Window::Head(k) => (0, k.min(n)),
        Window::Tail(k) => (n - k.min(n), n),
        Window::Time { from_ms, to_ms } => {
            let mut lo = 0u32;
            while lo < n && open_of(lo) < from_ms {
                lo += 1;
            }
            let mut hi = lo;
            while hi < n && open_of(hi) < to_ms {
                hi += 1;
            }
            (lo, hi)
        }
    }
}

/// The compared cells of one evaluated row into the plot buffer.
#[inline]
fn copy_row(buf: &mut [f64], row: usize, src: &[usize], plots: &[f64]) {
    let np = src.len();
    let mut p = 0usize;
    while p < np {
        // COPY: 8 B × asserted plots per compared row — the plot region is overwritten by the next
        // evaluation — an instance per row rejected (§4.5.7).
        buf[row * np + p] = plots[src[p]];
        p += 1;
    }
}

/// The bars a run steps.
enum Drive {
    Syn(SynBars),
    Chart(pine_screen::Clamp),
    Rt(SynTape),
}

fn host_err(h: HostFault, key: &impl Fn(u32) -> i64, fk: &mut Option<i64>) -> RunError {
    match h {
        HostFault::Script {
            kind,
            pc,
            bar_index,
            sub,
        } => {
            *fk = Some(key(bar_index));
            RunError::Fault {
                kind,
                pc,
                bar_index,
                sub,
            }
        }
        HostFault::Broker { fault, bar_index } => {
            *fk = Some(key(bar_index));
            RunError::Broker(fault)
        }
    }
}

impl Runner {
    /// Reads `pine_spec::chart`; loads `syminfo.tsv` (or its stand-in, `pin:06-syminfo-standin`)
    /// and `capture/signals.tsv` when present; opens the one `LibCache` of the run:
    /// `pine_sema::LibCache::open(root.join("tests/conformance/lib"), core_crypto::sha256)` (a `Diag`
    /// there is `RunError::Compile`).
    ///
    /// `pin:06-capture-tables-optional` — `capture/format.tsv` and `signals.tsv` are T18 / capture-day
    /// outputs: absent, the P&L basis is `Net` (contract §3.12: `pnl_1e12` is net) and the exit-cause
    /// texts are empty (no id-less exit matches until probe-03 measures them).
    pub fn new(opts: RunnerOptions) -> Result<Self, RunError> {
        let (si, si_src) = load_syminfo(&opts.root, &opts.syminfo_tsv)?;
        // COPY: signals.tsv's bytes (< 1 KiB) once per Runner, at load — the parser owns no file —
        // mapping the file rejected (load time, four lines).
        let signals = match std::fs::read_to_string(opts.root.join(SIGNALS_TSV)) {
            Ok(t) => {
                crate::trades::SignalTable::parse(&t).map_err(|w| fix_err(SIGNALS_TSV, 0, w))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => return Err(fix_err(SIGNALS_TSV, 0, e.to_string())),
        };
        let libs = pine_sema::LibCache::open(opts.root.join(LIB_ROOT), core_crypto::sha256)
            .map_err(|d| RunError::Compile(vec![d]))?;
        let chart = ChartSource::new(opts.bars_root.clone());
        Ok(Self {
            opts,
            si,
            si_src,
            signals,
            libs,
            chart,
            fault_key: None,
        })
    }

    /// Where `syminfo` came from.
    pub fn syminfo_source(&self) -> SyminfoSource {
        self.si_src
    }

    /// The options.
    pub fn options(&self) -> &RunnerOptions {
        &self.opts
    }

    /// The row key of the last run's script fault (§4.5.4 `Fault { row }`).
    pub fn fault_row(&self) -> Option<i64> {
        self.fault_key
    }

    /// §4.5.1 steps 1–3: the chart pin, the script's bytes, the dialect's text and its SHA-256.
    fn source(&self, f: &Fixture, d: Dialect) -> Result<(String, [u8; 32]), RunError> {
        if f.chart_sha256 != spec::SHA256 {
            return Err(RunError::ChartHash {
                fixture: f.chart_sha256,
                chart: spec::SHA256,
            });
        }
        let bytes = std::fs::read(self.opts.root.join(&f.script))
            .map_err(|e| fix_err(&f.script, 0, format!("cannot read the script: {e}")))?;
        if core_crypto::sha256(&bytes) != f.script_sha256 {
            return Err(fix_err(
                &f.script,
                0,
                "script_sha256 ≠ the script's bytes (I2)".into(),
            ));
        }
        let src = String::from_utf8(bytes)
            .map_err(|_| fix_err(&f.script, 0, "the script is not UTF-8".into()))?;
        let text = crate::fixture::dialect_text(&src, f.dialects, d)
            .map_err(|mut e| {
                e.path = f.script.clone();
                RunError::Fixture(e)
            })?
            .into_owned();
        let sha = core_crypto::sha256(text.as_bytes());
        Ok((text, sha))
    }

    /// The script's text as written: the chart pin and I2 checked, no dialect rewrite.
    fn source_bytes(&self, f: &Fixture) -> Result<String, RunError> {
        if f.chart_sha256 != spec::SHA256 {
            return Err(RunError::ChartHash {
                fixture: f.chart_sha256,
                chart: spec::SHA256,
            });
        }
        let bytes = std::fs::read(self.opts.root.join(&f.script))
            .map_err(|e| fix_err(&f.script, 0, format!("cannot read the script: {e}")))?;
        if core_crypto::sha256(&bytes) != f.script_sha256 {
            return Err(fix_err(
                &f.script,
                0,
                "script_sha256 ≠ the script's bytes (I2)".into(),
            ));
        }
        String::from_utf8(bytes)
            .map_err(|_| fix_err(&f.script, 0, "the script is not UTF-8".into()))
    }

    /// The syminfo `f` may run against (`pin:06-syminfo-standin`).
    fn syminfo_for(&self, f: &Fixture) -> Result<&Syminfo, RunError> {
        match (&self.si, self.si_src) {
            (Some(si), SyminfoSource::Captured) => Ok(si),
            (Some(si), SyminfoSource::StandIn) if !f.oracle.is_captured_kind() => Ok(si),
            (Some(_), _) => Err(fix_err(
                chart::SYMINFO_TSV,
                0,
                format!(
                    "{} has oracle {}: it needs the captured syminfo.tsv, only the stand-in exists \
                     (pin:06-syminfo-standin)",
                    f.id.as_str(),
                    crate::fixture::oracle_text(f.oracle)
                ),
            )),
            (None, _) => Err(fix_err(
                chart::SYMINFO_TSV,
                0,
                "neither syminfo.tsv nor its stand-in exists".into(),
            )),
        }
    }

    /// §4.5.1 steps 5–7.
    fn compile_text(
        &mut self,
        f: &Fixture,
        text: &str,
        sha: [u8; 32],
        d: Dialect,
        live_bound: bool,
    ) -> Result<PineProgram, RunError> {
        let si = self.syminfo_for(f)?;
        let mut opts = chart::compile_opts(si, d, f.mode, live_bound, self.opts.limits, sha);
        if f.driver == Driver::Rt {
            opts.chart.timeframe_s = RT_TIMEFRAME_S;
        }
        let mut syms = SymTable::new();
        let ast = pine_lang::parse(text, FileId::ROOT, &mut syms).map_err(RunError::Parse)?;
        pine_sema::compile(&ast, syms, &opts, &mut self.libs).map_err(RunError::Compile)
    }

    /// Compile only, at `d` and `live_bound` — `Refuse` expectations and `live_refuse` (I12).
    pub fn compile_only(
        &mut self,
        f: &Fixture,
        d: Dialect,
        live_bound: bool,
    ) -> Result<PineProgram, RunError> {
        let (text, sha) = self.source(f, d)?;
        self.compile_text(f, &text, sha, d, live_bound)
    }

    /// Parse → compile → verify → instantiate → the §3.15.3 loop → collect.
    pub fn run(&mut self, f: &Fixture, d: Dialect) -> Result<RunOutcome, RunError> {
        self.run_with(f, d, &mut NoProbe)
    }

    /// `run` with a probe around the loop (the alloc test, §7.9). Steps 1–12 and 16 allocate; the
    /// loop (14) does not.
    pub fn run_with<P: LoopProbe>(
        &mut self,
        f: &Fixture,
        d: Dialect,
        probe: &mut P,
    ) -> Result<RunOutcome, RunError> {
        self.fault_key = None;
        // 1–3
        let (text, sha) = self.source(f, d)?;
        // 4
        let tier_bit = if (1..=8).contains(&f.tier) {
            1u8 << (f.tier - 1)
        } else {
            0
        };
        if tier_bit & self.opts.limits.active_tiers == 0 {
            return Err(RunError::Skip(SkipReason::TierInactive { tier: f.tier }));
        }
        if f.oracle == Oracle::TvOnly && !self.opts.run_tv_only {
            return Err(RunError::Skip(SkipReason::TvOnly));
        }
        let refuse = matches!(f.expect, Expect::Refuse { .. });
        let charted = matches!(f.driver, Driver::Chart | Driver::SynSig);
        if charted && self.opts.bars_root.is_none() && !refuse {
            return Err(RunError::Skip(SkipReason::NoBarsRoot));
        }
        // 5–7
        let prog = self.compile_text(f, &text, sha, d, false)?;
        let day_offset_s = self.syminfo_for(f)?.day_offset_s;
        // 8
        let v = pine_vm::verify(&prog, &pine_std::BUILTINS).map_err(RunError::Verify)?;
        let mut flat: Vec<&SubProgram> = Vec::new();
        flatten(&prog, &mut flat);
        // 9 — the driver and every sub-program's HTF rows.
        let drive = match f.driver {
            Driver::Syn => Drive::Syn(SynBars::new(
                syn_bars(f, self.opts.syn_bars_default),
                spec::FROM_MS,
                spec::TIMEFRAME_S,
            )),
            // pin:06-replay-from-file-start (`pin:00-dataset-start`, r33g) — a chart-driven run
            // replays from the chart file's first row, not from `REPLAY_FROM_MS`: TradingView runs
            // every script from its first loaded bar (D060's `t0`, 2024-01-01 on the capture chart),
            // so a value that carries the whole history (a cumulative kernel's level, `nvi`/`pvi`'s
            // product, `bar_index`) is TradingView's only when the bar root starts where its chart
            // does. §4.1.3's `REPLAY_FROM_MS` stays the floor every root must reach (barhash, P10).
            Driver::Chart | Driver::SynSig => Drive::Chart(pine_screen::Clamp {
                from_open_ms: i64::MIN,
                to_close_ms: spec::TO_MS,
            }),
            Driver::Rt => {
                let rt = f.rt.ok_or_else(|| {
                    fix_err(&f.script, 0, "driver rt without an `rt` line".into())
                })?;
                Drive::Rt(SynTape::new(rt, RT_TIMEFRAME_S, spec::FROM_MS))
            }
        };
        // The replay's first bar: the chart file's first row (`pin:06-replay-from-file-start`);
        // §4.5.2's HTF warm-up bound counts back from it.
        let replay_from_ms = match &drive {
            Drive::Chart(c) => self.chart.rows(spec::SYMBOL_KEY, spec::TIMEFRAME_S, *c)?[0].open_ms,
            _ => spec::REPLAY_FROM_MS,
        };
        let sub_clamp = |tf_s: u32| {
            let htf_ms = i64::from(tf_s) * 1000;
            pine_screen::Clamp {
                from_open_ms: replay_from_ms - 500 * htf_ms,
                to_close_ms: spec::TO_MS + htf_ms,
            }
        };
        // Owned HTF rows (Syn, Rt); chart files stay borrowed from the `ChartSource`.
        let mut owned: Vec<Vec<Bar>> = Vec::with_capacity(flat.len());
        match &drive {
            Drive::Chart(c) => {
                self.chart.rows(spec::SYMBOL_KEY, spec::TIMEFRAME_S, *c)?;
                let mut i = 0usize;
                while i < flat.len() {
                    let sp = flat[i];
                    self.chart
                        .rows(&sp.symbol_key, sp.timeframe_s, sub_clamp(sp.timeframe_s))?;
                    i += 1;
                }
            }
            Drive::Syn(sb) => {
                // pin:06-syn-htf-synbars — a Syn fixture's sub-program steps SynBars on its own
                // timeframe's grid from FROM_MS, enough bars to cover the chart plus one.
                let span_ms = i64::from(sb.len()) * spec::TF_MS;
                let mut i = 0usize;
                while i < flat.len() {
                    let tf_ms = i64::from(flat[i].timeframe_s) * 1000;
                    let n = (span_ms + tf_ms - 1) / tf_ms + 1;
                    let s = SynBars::new(n as u32, spec::FROM_MS, flat[i].timeframe_s);
                    let mut rows = vec![Bar::default(); n as usize];
                    let mut k = 0u32;
                    while k < s.len() {
                        s.bar(k, &mut rows[k as usize]);
                        k += 1;
                    }
                    owned.push(rows);
                    i += 1;
                }
            }
            Drive::Rt(tape) => {
                let mut i = 0usize;
                while i < flat.len() {
                    owned.push(tape.htf_bars());
                    i += 1;
                }
            }
        }
        let chart_rows: &[Bar] = match &drive {
            Drive::Chart(c) => self
                .chart
                .opened(spec::SYMBOL_KEY, spec::TIMEFRAME_S, *c)
                .expect("opened above"),
            _ => &[],
        };
        let mut htf: Vec<&[Bar]> = Vec::with_capacity(flat.len());
        let mut i = 0usize;
        while i < flat.len() {
            let sp = flat[i];
            htf.push(match &drive {
                Drive::Chart(_) => self
                    .chart
                    .opened(&sp.symbol_key, sp.timeframe_s, sub_clamp(sp.timeframe_s))
                    .expect("opened above"),
                _ => &owned[i],
            });
            i += 1;
        }
        // The rows of the run and the compared window.
        let n_rows: u32 = match &drive {
            Drive::Syn(sb) => sb.len(),
            Drive::Chart(_) => chart_rows.len() as u32,
            Drive::Rt(t) => t.spec().history + t.spec().realtime,
        };
        let open_of = |i: u32| -> i64 {
            match &drive {
                Drive::Syn(_) => spec::FROM_MS + i64::from(i) * spec::TF_MS,
                Drive::Chart(_) => chart_rows[i as usize].open_ms,
                Drive::Rt(_) => spec::FROM_MS + i64::from(i) * i64::from(RT_TIMEFRAME_S) * 1000,
            }
        };
        let key_of = |i: u32| -> i64 {
            match f.align {
                Align::Step => i64::from(i),
                Align::Time => open_of(i),
            }
        };
        let (lo, hi) = window_range(f.window, n_rows, &open_of);
        let keys: Vec<i64> = (lo..hi).map(key_of).collect();
        // 10 — store, plot buffer, plot_ix.
        let mut specs: Vec<(u64, u16, u32)> = Vec::with_capacity(flat.len());
        let mut i = 0usize;
        while i < flat.len() {
            specs.push((flat[i].key, flat[i].n_out, htf[i].len() as u32 + 1));
            i += 1;
        }
        let mut store = SeriesStore::with_slots(&specs);
        let mut plot_ix: Vec<u16> = Vec::with_capacity(f.plots.len());
        let mut src: Vec<usize> = Vec::with_capacity(f.plots.len());
        for p in &f.plots {
            let found = prog
                .syms
                .get(&p.title)
                .and_then(|s| prog.meta.plots.iter().position(|pd| pd.title == s));
            match found {
                Some(ix) => {
                    plot_ix.push(ix as u16);
                    src.push(usize::from(prog.meta.plots[ix].index));
                }
                None => {
                    return Err(fix_err(
                        &f.script,
                        0,
                        format!("plot title `{}` is not a plot of the program", p.title),
                    ))
                }
            }
        }
        let mut buf = vec![f64::NAN; keys.len() * src.len()];
        // 11
        // pin:06-rt-broker (CI-030) — an `Rt` strategy (E106) gets its `TvBroker` too.
        let mut broker = if prog.meta.kind == ScriptKind::Strategy {
            Some(
                pine_broker::TvBroker::new(
                    &prog.meta,
                    TvCfg {
                        sizing: SizingMode::AsDeclared,
                        day_offset_s,
                        _pad: 0,
                    },
                )
                .map_err(RunError::BrokerDecl)?,
            )
        } else {
            None
        };
        // 9 (instances) and 12.
        let mut verified = Vec::with_capacity(flat.len());
        let mut i = 0usize;
        while i < flat.len() {
            verified.push(
                pine_vm::verify(&flat[i].program, &pine_std::BUILTINS).map_err(RunError::Verify)?,
            );
            i += 1;
        }
        let mut arenas: Vec<AlignedBuf> = flat
            .iter()
            .map(|sp| AlignedBuf::new(sp.program.layout.total_bytes))
            .collect();
        let mut subs: Vec<SubRun<'_>> = Vec::with_capacity(flat.len());
        for (i, (vs, a)) in verified.into_iter().zip(arenas.iter_mut()).enumerate() {
            subs.push(SubRun {
                vm: VmInstance::new(vs, a.as_mut_slice()),
                slot: i,
                n_out: flat[i].n_out,
                lookahead: flat[i].lookahead,
                bars: htf[i],
                next: 0,
            });
        }
        let mut main = AlignedBuf::new(prog.layout.total_bytes);
        let mut vm = VmInstance::new(v, main.as_mut_slice());
        let mut evals = 0u64;
        let (lo_u, hi_u) = (lo as usize, hi as usize);
        // 13–15 — the loop: index loops, stack bars, preallocated buffers.
        probe.before_loop();
        let mut res: Result<(), HostFault> = Ok(());
        match &drive {
            Drive::Syn(sb) => {
                let mut b = Bar::default();
                let mut k = 0u32;
                while k < sb.len() {
                    sb.bar(k, &mut b);
                    if let Err(h) =
                        step_chart_bar(&b, &mut subs, &mut store, &mut vm, &mut broker, &mut evals)
                    {
                        res = Err(h);
                        break;
                    }
                    let ku = k as usize;
                    if ku >= lo_u && ku < hi_u {
                        copy_row(&mut buf, ku - lo_u, &src, vm.plots());
                    }
                    k += 1;
                }
            }
            Drive::Chart(_) => {
                let n = chart_rows.len();
                let mut k = 0usize;
                while k < n {
                    let lb;
                    let bar = if k + 1 == n {
                        lb = last_bar(&chart_rows[k]);
                        &lb
                    } else {
                        &chart_rows[k]
                    };
                    if let Err(h) =
                        step_chart_bar(bar, &mut subs, &mut store, &mut vm, &mut broker, &mut evals)
                    {
                        res = Err(h);
                        break;
                    }
                    if k >= lo_u && k < hi_u {
                        copy_row(&mut buf, k - lo_u, &src, vm.plots());
                    }
                    k += 1;
                }
            }
            Drive::Rt(tape) => {
                let rt = tape.spec();
                let mut b = Bar::default();
                let mut k = 0u32;
                while k < rt.history && res.is_ok() {
                    tape.history_bar(k, &mut b);
                    if let Err(h) =
                        step_chart_bar(&b, &mut subs, &mut store, &mut vm, &mut broker, &mut evals)
                    {
                        res = Err(h);
                        break;
                    }
                    let ku = k as usize;
                    if ku >= lo_u && ku < hi_u {
                        copy_row(&mut buf, ku - lo_u, &src, vm.plots());
                    }
                    k += 1;
                }
                let mut r = 0u32;
                while r < rt.realtime && res.is_ok() {
                    if let Err(h) = step_rt_bar(
                        tape,
                        r,
                        &mut subs,
                        &mut store,
                        &mut vm,
                        &mut broker,
                        &mut evals,
                    ) {
                        res = Err(h);
                        break;
                    }
                    let ku = (rt.history + r) as usize;
                    if ku >= lo_u && ku < hi_u {
                        copy_row(&mut buf, ku - lo_u, &src, vm.plots());
                    }
                    r += 1;
                }
            }
        }
        probe.after_loop();
        let mut fk = None;
        let res = res.map_err(|h| host_err(h, &key_of, &mut fk));
        self.fault_key = fk;
        res?;
        // 16
        let mut trades = Vec::new();
        let mut cum_net = Vec::new();
        let mut record = None;
        let mut evicted = Evicted::default();
        if let (Some(b), Expect::Strategy { .. }) = (&broker, &f.expect) {
            let view = b.view();
            // pin:06-trade-trim-kept (CI-029, E109) — a trimmed list is compared as TradingView
            // keeps it (the newest closed trades); the dropped trades' totals seed `cum_pnl`.
            evicted.trades = view.closedtrades - view.closed_retained;
            let mut i = 0u32;
            while i < view.closed_retained {
                trades.push(*b.closed_trade(i).expect("closed_retained bounds the list"));
                cum_net.push(
                    b.closed_cum_net(i)
                        .expect("closed_retained bounds the list"),
                );
                i += 1;
            }
            let mut i = 0u32;
            while i < view.opentrades {
                trades.push(*b.open_trade(i).expect("opentrades bounds the list"));
                i += 1;
            }
            let rec = b.record();
            if evicted.trades > 0 {
                let net = crate::trades::retained_pnl(&trades, crate::trades::PnlBasis::Net);
                let fees: i128 = trades.iter().map(|t| t.fee_1e12).sum();
                evicted.net_1e12 = view.netprofit_1e12 - net;
                evicted.fee_1e12 = rec.commission_paid_1e12 - fees;
            }
            record = Some(rec);
        }
        let counters = vm.counters();
        Ok(RunOutcome {
            dialect: d,
            plots: buf,
            keys,
            plot_ix,
            trades,
            record,
            evicted,
            cum_net,
            bars: n_rows,
            evaluations: evals,
            counters,
            source_sha256: sha,
            syms: prog.syms.clone(),
        })
    }

    /// D12 mode (b), §4.8.4: two `VmInstance`s of one `PineProgram` stepped alternately over the
    /// fixture's `SynBars`, then the first `reset()` and re-run alone — `[a, b, a_after_reset]`,
    /// every row of the run (the window is not applied). Fuzz-only; allocates outside its loops.
    ///
    /// `pin:06-interleaved-scope` — a `Syn`-driven indicator without sub-programs (the profiles
    /// that run (b) with a broker or `request.*` are a later grammar version).
    pub fn run_interleaved(
        &mut self,
        f: &Fixture,
        d: Dialect,
    ) -> Result<[RunOutcome; 3], RunError> {
        let (text, sha) = self.source(f, d)?;
        let prog = self.compile_text(f, &text, sha, d, false)?;
        if f.driver != Driver::Syn
            || !prog.subprograms.is_empty()
            || prog.meta.kind == ScriptKind::Strategy
        {
            return Err(fix_err(
                &f.script,
                0,
                "run_interleaved: a Syn-driven indicator without sub-programs only".into(),
            ));
        }
        let sb = SynBars::new(
            f.bars.unwrap_or(self.opts.syn_bars_default),
            spec::FROM_MS,
            spec::TIMEFRAME_S,
        );
        let n = sb.len();
        let mut plot_ix: Vec<u16> = Vec::with_capacity(f.plots.len());
        let mut src: Vec<usize> = Vec::with_capacity(f.plots.len());
        for p in &f.plots {
            let ix = prog
                .syms
                .get(&p.title)
                .and_then(|s| prog.meta.plots.iter().position(|pd| pd.title == s))
                .ok_or_else(|| {
                    fix_err(
                        &f.script,
                        0,
                        format!("plot title `{}` is not a plot", p.title),
                    )
                })?;
            plot_ix.push(ix as u16);
            src.push(usize::from(prog.meta.plots[ix].index));
        }
        let va = pine_vm::verify(&prog, &pine_std::BUILTINS).map_err(RunError::Verify)?;
        let vb = pine_vm::verify(&prog, &pine_std::BUILTINS).map_err(RunError::Verify)?;
        let mut ma = AlignedBuf::new(prog.layout.total_bytes);
        let mut mb = AlignedBuf::new(prog.layout.total_bytes);
        let mut a = VmInstance::new(va, ma.as_mut_slice());
        let mut b = VmInstance::new(vb, mb.as_mut_slice());
        let np = src.len();
        let mut bufs = [
            vec![f64::NAN; n as usize * np],
            vec![f64::NAN; n as usize * np],
            vec![f64::NAN; n as usize * np],
        ];
        let key_of = |i: u32| i64::from(i);
        let mut fk = None;
        let mut evals = [0u64; 3];
        let mut none = None;
        let mut no_subs: [SubRun<'_>; 0] = [];
        let mut sa = SeriesStore::with_slots(&[]);
        let mut sbs = SeriesStore::with_slots(&[]);
        let mut bar = Bar::default();
        let mut k = 0u32;
        while k < n {
            sb.bar(k, &mut bar);
            step_chart_bar(
                &bar,
                &mut no_subs,
                &mut sa,
                &mut a,
                &mut none,
                &mut evals[0],
            )
            .map_err(|h| host_err(h, &key_of, &mut fk))?;
            copy_row(&mut bufs[0], k as usize, &src, a.plots());
            step_chart_bar(
                &bar,
                &mut no_subs,
                &mut sbs,
                &mut b,
                &mut none,
                &mut evals[1],
            )
            .map_err(|h| host_err(h, &key_of, &mut fk))?;
            copy_row(&mut bufs[1], k as usize, &src, b.plots());
            k += 1;
        }
        a.reset();
        let mut sa = SeriesStore::with_slots(&[]);
        let mut k = 0u32;
        while k < n {
            sb.bar(k, &mut bar);
            step_chart_bar(
                &bar,
                &mut no_subs,
                &mut sa,
                &mut a,
                &mut none,
                &mut evals[2],
            )
            .map_err(|h| host_err(h, &key_of, &mut fk))?;
            copy_row(&mut bufs[2], k as usize, &src, a.plots());
            k += 1;
        }
        let keys: Vec<i64> = (0..n).map(key_of).collect();
        let counters = [a.counters(), b.counters(), a.counters()];
        let [p0, p1, p2] = bufs;
        let mut out = [p0, p1, p2]
            .into_iter()
            .zip(evals)
            .zip(counters)
            .map(|((plots, e), c)| RunOutcome {
                dialect: d,
                plots,
                keys: keys.clone(),
                plot_ix: plot_ix.clone(),
                trades: Vec::new(),
                record: None,
                evicted: Evicted::default(),
                cum_net: Vec::new(),
                bars: n,
                evaluations: e,
                counters: c,
                source_sha256: sha,
                syms: prog.syms.clone(),
            });
        let (x, y, z) = (
            out.next().expect("3"),
            out.next().expect("3"),
            out.next().expect("3"),
        );
        Ok([x, y, z])
    }

    /// Everything a fixture asserts: every declared dialect (and v5 ≡ v6 for `56`, I9), the
    /// `live_refuse` compile, the expectation under §4.4's tolerance.
    pub fn check(&mut self, f: &Fixture) -> Verdict {
        self.check_full(f).verdict
    }

    /// [`Runner::check`] with what it saw: the primary dialect's run and the expectation held
    /// against that run alone (the TradingView divergence report, `pine-conf report --tv`).
    pub fn check_full(&mut self, f: &Fixture) -> Checked {
        let d0 = primary_dialect(f.dialects);
        if let Expect::Parse { accept } = f.expect {
            let v = self.check_parse(f, d0, accept);
            return Checked {
                verdict: v.clone(),
                primary: None,
                expectation: v,
                fault_row: None,
            };
        }
        let r0 = self.run(f, d0);
        let fault_key = self.fault_key;
        let done = |verdict: Verdict, expectation: Verdict, r0| Checked {
            verdict,
            primary: Some(r0),
            expectation,
            fault_row: fault_key,
        };
        // A `Refuse` fixture reaches a bars skip only after it compiled: that is its failure.
        let compiled = matches!(
            r0,
            Err(RunError::Skip(
                SkipReason::NoBarsRoot | SkipReason::NoBarFile { .. }
            ))
        ) && matches!(f.expect, Expect::Refuse { .. });
        if let (Err(RunError::Skip(s)), false) = (&r0, compiled) {
            let v = Verdict::Skipped(*s);
            return done(v.clone(), v, r0);
        }
        let exp = self.expectation(f, &r0, fault_key);
        if f.dialects == Dialects::BOTH {
            let r5 = self.run(f, Dialect::V5);
            if let Some((first_row, plot)) = dialect_divergence(&r0, &r5) {
                return done(Verdict::DialectDivergence { first_row, plot }, exp, r0);
            }
        }
        if let Some(code) = f.live_refuse {
            for d in [Dialect::V6, Dialect::V5] {
                if !f.dialects.has(d) {
                    continue;
                }
                match self.compile_only(f, d, true) {
                    Err(e) if e.code() == Some(code) => {}
                    Err(e) => {
                        let v = fail(format!(
                            "live_refuse {}: v{} refused {e:?}",
                            code.0, d as u8
                        ));
                        return done(v, exp, r0);
                    }
                    Ok(_) => {
                        let v = fail(format!(
                            "live_refuse {}: v{} compiled live-bound",
                            code.0, d as u8
                        ));
                        return done(v, exp, r0);
                    }
                }
            }
        }
        done(exp.clone(), exp, r0)
    }

    fn check_parse(&mut self, f: &Fixture, d: Dialect, accept: bool) -> Verdict {
        if let Err(e) = self.syminfo_for(f) {
            return fail(format!("{e:?}"));
        }
        // pin:06-parse-raw-text — a `Parse` fixture in one dialect parses the script's bytes as
        // written (I2 still checked): the version line is what Z007 / Z008 probe, so the dialect
        // rewrite (which refuses a malformed or second version line) is not applied to it.
        let src = if f.dialects == Dialects::BOTH {
            self.source(f, d).map(|(t, _)| t)
        } else {
            self.source_bytes(f)
        };
        let text = match src {
            Ok(t) => t,
            Err(e) => return fail(format!("{e:?}")),
        };
        let mut syms = SymTable::new();
        let ok = pine_lang::parse(&text, FileId::ROOT, &mut syms).is_ok();
        if ok == accept {
            Verdict::Pass
        } else {
            fail(format!("parse accept = {ok}, expected {accept}"))
        }
    }

    fn expectation(
        &self,
        f: &Fixture,
        r: &Result<RunOutcome, RunError>,
        fault_key: Option<i64>,
    ) -> Verdict {
        match (&f.expect, r) {
            (Expect::Pending, Ok(_)) => Verdict::Pending,
            (Expect::Series(rows), Ok(o)) => match compare_rows(f, rows, o) {
                Some(d) => Verdict::Fail(d),
                None => Verdict::Pass,
            },
            (
                Expect::Strategy {
                    series,
                    trades,
                    summary,
                },
                Ok(o),
            ) => {
                if let Some(d) = compare_rows(f, series, o) {
                    return Verdict::Fail(d);
                }
                // pin:06-pnl-basis-net — `Net` until capture/format.tsv records probe-03's basis.
                // pin:06-trade-trim-kept (CI-029) — the kept trades; on a SYN tape (E109) only the
                // kept list's shape is TradingView's.
                // REQ-r33g-1 (r33h) — `cum_pnl` is the broker's net profit at each close
                // (`closed_cum_net`, the open trades' paid entry fees included), which already
                // carries the dropped trades: no seed.
                let td = if f.driver == Driver::Syn && o.evicted.trades > 0 {
                    crate::trades::compare_kept_shape(trades, &o.trades, o.evicted.trades)
                } else {
                    crate::trades::compare_trades_cum(
                        trades,
                        &o.trades,
                        o.evicted.trades,
                        crate::trades::CumPnl::Recorded(&o.cum_net),
                        &o.syms,
                        &self.signals,
                        crate::trades::PnlBasis::Net,
                    )
                }
                .or_else(|| {
                    let s = summary.as_ref()?;
                    let rec = o.record.as_ref()?;
                    let closed = o
                        .trades
                        .iter()
                        .filter(|t| t.flags & pine_ir::trade_flag::OPEN == 0)
                        .count() as u32
                        + o.evicted.trades;
                    crate::trades::compare_summary(s, rec, closed)
                });
                match td {
                    Some(t) => Verdict::Fail(Divergence {
                        plot: None,
                        first_row: NO_ROW,
                        first_out_of_bound: None,
                        trade: Some(t),
                        error: None,
                    }),
                    None => Verdict::Pass,
                }
            }
            (Expect::Refuse { code }, Err(e)) if e.code() == Some(*code) => Verdict::Pass,
            (Expect::Refuse { code }, Err(RunError::Skip(s))) => fail(format!(
                "compiled where {} was expected (then skipped: {s:?})",
                code.0
            )),
            (Expect::Refuse { code }, Err(e)) => {
                fail(format!("expected refusal {}, got {e:?}", code.0))
            }
            (Expect::Refuse { code }, Ok(_)) => {
                fail(format!("ran to the end where {} was expected", code.0))
            }
            (Expect::Fault { kind, row }, Err(RunError::Fault { kind: k, .. }))
                if k == kind && fault_key == Some(*row) =>
            {
                Verdict::Pass
            }
            // pin:06-broker-script-fault (r33g, CI-005 / E110) — a TV broker `Script` refusal is
            // TradingView's runtime error, `ScriptRefused` on the faulting bar's row: impl-05's
            // `pin:05-v5-order-limit` ("a plain runtime error (ScriptRefused; no new kind)") and
            // §4.5.2 (a `BrokerFault` aborts the run like a `StepOutcome::Fault`).
            (
                Expect::Fault {
                    kind: FaultKind::ScriptRefused,
                    row,
                },
                Err(RunError::Broker(pine_broker::BrokerFault::Script(_))),
            ) if fault_key == Some(*row) => Verdict::Pass,
            (Expect::Fault { kind, row }, Err(e)) => fail(format!(
                "expected fault {kind:?} on row {row}, got {e:?} on row {fault_key:?}"
            )),
            (Expect::Fault { kind, row }, Ok(_)) => fail(format!(
                "ran to the end where fault {kind:?} on row {row} was expected"
            )),
            (Expect::Relation(rel), Ok(o)) => relation(f, rel, o),
            (Expect::Parse { .. }, _) => unreachable!("check routes Parse to check_parse"),
            (_, Err(e)) => fail(format!("{e:?}")),
        }
    }
}

/// What [`Runner::check_full`] saw.
#[derive(Debug)]
pub struct Checked {
    /// [`Runner::check`]'s verdict.
    pub verdict: Verdict,
    /// The primary dialect's run (`None` for a `Parse` expectation).
    pub primary: Option<Result<RunOutcome, RunError>>,
    /// The expectation held against the primary run alone — it differs from `verdict` when I9
    /// (v5 ≢ v6) or `live_refuse` decided first.
    pub expectation: Verdict,
    /// The primary run's fault row key.
    pub fault_row: Option<i64>,
}

fn fail(what: String) -> Verdict {
    Verdict::Fail(Divergence::error(what))
}

/// `pin:06-rt-bookkeeping-columns` (R-P3, 2026-10-17) — on a `Driver::Rt` fixture the plots whose
/// title ends `_rt`, `_done`, `_upd` or `_k` are capture bookkeeping (which rows are realtime and
/// confirmed, the tick count): a `series` body never compares them; the report says so.
///
/// r27 review: one bookkeeping rule for the runner and distil — a plot is bookkeeping exactly when
/// `capture::distil::is_relation_column` says it is not a relation column (`RT_BOOKKEEPING` and the
/// `tick_` counters, `pin:06-distil-tick-columns`).
pub fn is_rt_bookkeeping(f: &Fixture, p: usize) -> bool {
    f.driver == Driver::Rt
        && f.plots
            .get(p)
            .is_some_and(|s| !crate::capture::distil::is_relation_column(f.id, &s.title))
}

/// The run row index of row key `key` (`Step`: the key; `Time`: from the open time).
fn row_index(f: &Fixture, key: i64) -> i64 {
    match (f.align, f.driver) {
        (Align::Step, _) => key,
        (Align::Time, Driver::Rt) => (key - spec::FROM_MS) / (i64::from(RT_TIMEFRAME_S) * 1000),
        (Align::Time, _) => (key - spec::FROM_MS) / spec::TF_MS,
    }
}

/// §4.4.2 over every expected row: the first divergent row, the first out of bound.
fn compare_rows(f: &Fixture, rows: &[Row], o: &RunOutcome) -> Option<Divergence> {
    let np = o.plot_ix.len();
    let mut div: Option<Divergence> = None;
    for row in rows {
        let Ok(r) = o.keys.binary_search(&row.key) else {
            let mut d = Divergence::error(format!("row {} is not in the run's window", row.key));
            d.first_row = row.key;
            d.first_out_of_bound = Some(row.key);
            return Some(d);
        };
        if row.cells.len() != np {
            return Some(Divergence::error(format!(
                "row {} has {} cells for {np} plots",
                row.key,
                row.cells.len()
            )));
        }
        let mut p = 0usize;
        while p < np {
            if is_rt_bookkeeping(f, p) {
                p += 1;
                continue;
            }
            let c = compare_cell(&row.cells[p], o.plots[r * np + p], f.plots[p].tol);
            if c != crate::cmp::Cmp::Equal {
                let d = div.get_or_insert(Divergence {
                    plot: Some(p),
                    first_row: row.key,
                    first_out_of_bound: None,
                    trade: None,
                    error: None,
                });
                if !c.ok() && d.first_out_of_bound.is_none() {
                    d.first_out_of_bound = Some(row.key);
                    d.plot = Some(p);
                }
            }
            p += 1;
        }
    }
    div.filter(|d| d.first_out_of_bound.is_some())
}

/// `Relation`: every realtime row of each title equals its constant (exactly).
///
/// `pin:06-relation-rows` (R-P3) — "realtime row" is a row whose run index is ≥ `rt.history`
/// (for `align time` the index comes from the open time, never the key itself); where the fixture
/// declares a `_done` bookkeeping plot, a row our run left unconfirmed (`done` ≠ 1, the open last
/// bar of `rt … open`) is not one, mirroring distil's `rt = 1 & done = 1` row set (§4.7.8).
fn relation(f: &Fixture, rel: &[(String, i64)], o: &RunOutcome) -> Verdict {
    let history = f.rt.map_or(0, |r| i64::from(r.history));
    let np = o.plot_ix.len();
    let done_p = (0..f.plots.len())
        .find(|&p| is_rt_bookkeeping(f, p) && f.plots[p].title.ends_with("_done"));
    for (title, c) in rel {
        let Some(p) = f.plots.iter().position(|ps| &ps.title == title) else {
            return fail(format!("relation names `{title}`, not a declared plot"));
        };
        for (r, key) in o.keys.iter().enumerate() {
            if row_index(f, *key) < history {
                continue;
            }
            if done_p.is_some_and(|dp| o.plots[r * np + dp] != 1.0) {
                continue;
            }
            let v = o.plots[r * np + p];
            if v != *c as f64 {
                let mut d = Divergence::error(format!("{title} = {v} ≠ {c}"));
                d.plot = Some(p);
                d.first_row = *key;
                d.first_out_of_bound = Some(*key);
                return Verdict::Fail(d);
            }
        }
    }
    Verdict::Pass
}

/// I9: `Some((first_row, plot))` when our v6 and v5 runs differ — a plot cell bit for bit, the
/// window, the trades or record, or the outcome itself (Ok vs Err, or two different errors).
///
/// `pin:06-dialect-divergence-no-row` — a difference that is not a plot cell reports
/// `(report::NO_ROW, usize::MAX)`.
pub fn dialect_divergence(
    v6: &Result<RunOutcome, RunError>,
    v5: &Result<RunOutcome, RunError>,
) -> Option<(i64, usize)> {
    match (v6, v5) {
        (Ok(a), Ok(b)) => {
            if a.keys != b.keys || a.plot_ix != b.plot_ix || a.plots.len() != b.plots.len() {
                return Some((NO_ROW, usize::MAX));
            }
            let np = a.plot_ix.len();
            let mut i = 0usize;
            while i < a.plots.len() {
                if a.plots[i].to_bits() != b.plots[i].to_bits() {
                    return Some((a.keys[i / np], i % np));
                }
                i += 1;
            }
            if a.trades != b.trades || a.record != b.record {
                return Some((NO_ROW, usize::MAX));
            }
            None
        }
        (Err(x), Err(y)) => {
            if format!("{x:?}") == format!("{y:?}") {
                None
            } else {
                Some((NO_ROW, usize::MAX))
            }
        }
        _ => Some((NO_ROW, usize::MAX)),
    }
}
