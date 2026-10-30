// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The runner (§3.5, §4.3): `compile_trial` and the contract §3.15.3 step loop over `TvBroker`,
//! sub-programs first (flattened, deduplicated across the group — law P4; §4.3.2).

use crate::bars::{bar_path, BarFile, BarFileErr, Clamp};
use crate::ctx::{ScreenCtx, SeriesSpec, SeriesStore, SlotKey};
use crate::rows::{AccumCfg, TrialAccum, TrialEnd};
use crate::{CAPITAL_UNITS, MAX_GROUP};
use core_crypto::Sha256;
use core_time::barclock;
use pine_broker::{BrokerFault, InternalDefect, ScriptRefusal, TvBroker, TvStep};
use pine_ir::{
    barflag, op, Bar, ChartInfo, CommissionType, CompileLimits, CompileOpts, Diag, DiagCode,
    Dialect, ExecMode, FaultKind, FileId, InputOverride, OverloadId, PineProgram, ProgramMeta,
    RunRecord, Severity, SizingMode, StepOutcome, StrategyDecl, SubProgram, SymTable, TrialSummary,
    TvCfg,
};
use pine_sema::LibCache;
use pine_vm::{VerifyError, VmInstance};

const DAY_MS: i64 = 86_400_000;

/// One trial to compile: a script on a chart.
pub struct TrialSpec<'a> {
    /// The `.pine` source (UTF-8).
    pub source: &'a str,
    /// Canonical `venue:symbol` key of the chart (`syminfo.tickerid`).
    pub symbol_key: &'a str,
    /// `symbol_hash`, `timeframe_s`, venue byte, mintick, point value, qty step, min notional.
    pub chart: ChartInfo,
    /// `syminfo.ticker`, `.basecurrency`, `.currency`, `.type`.
    pub syminfo_strs: [&'a str; 4],
    /// `[script.inputs]`-equivalent overrides (a variant); empty = the declared defaults.
    pub inputs: &'a [InputOverride],
    /// TradingView ticker → canonical key, for `request.security` targets.
    pub ticker_map: &'a [(String, String)],
    /// Normally [`crate::SCREEN_LIMITS`].
    pub limits: CompileLimits,
}

/// A parsed, compiled (`live_bound = true`) and verified program plus what the screen needs.
pub struct Loaded {
    program: PineProgram,
    source_sha256: [u8; 32],
    input_digest: [u8; 32],
    overloads: Vec<OverloadId>,
    htf: Vec<(String, u32)>,
}

impl Loaded {
    /// The compiled program (shared by every instance — P1).
    pub fn program(&self) -> &PineProgram {
        &self.program
    }

    /// `program().meta`.
    pub fn meta(&self) -> &ProgramMeta {
        &self.program.meta
    }

    /// SHA-256 of the source bytes.
    pub fn source_sha256(&self) -> &[u8; 32] {
        &self.source_sha256
    }

    /// SHA-256 over every `InputDesc` in source order: name bytes, 0, kind, ty, `default_bits` LE —
    /// the input vector in effect after overrides (§4.3.1).
    pub fn input_digest(&self) -> &[u8; 32] {
        &self.input_digest
    }

    /// `BarClose`, or `Realtime` when sema refused the bar-close compilation with `3052`.
    pub fn mode(&self) -> ExecMode {
        self.program.mode
    }

    /// Sorted distinct `OverloadId`s of every `CALL_BUILTIN`, sub-programs included (coverage).
    pub fn overloads_reached(&self) -> &[OverloadId] {
        &self.overloads
    }

    /// Distinct `(SubProgram::symbol_key, timeframe_s)` of every sub-program, nested included —
    /// the HTF bar files a run of this program must open.
    pub fn htf_needs(&self) -> &[(String, u32)] {
        &self.htf
    }

    /// **Integration tests only**: wrap a hand-built program (`pine_std::testing`'s builder) —
    /// `verify` runs (a refusal is `CompileRefusal::Verify`), the digests come from `source` and
    /// `meta.inputs`, the mode from the program. Not a compile path: nothing in `src/` calls it
    /// (`hygiene::from_program_is_test_only`).
    #[doc(hidden)]
    pub fn from_program_for_tests(
        program: PineProgram,
        source: &str,
    ) -> Result<Loaded, CompileRefusal> {
        pine_vm::verify(&program, &pine_std::BUILTINS).map_err(CompileRefusal::Verify)?;
        Ok(Loaded::wrap(
            program,
            core_crypto::sha256(source.as_bytes()),
        ))
    }

    fn wrap(program: PineProgram, source_sha256: [u8; 32]) -> Loaded {
        let input_digest = input_digest(&program);
        let mut overloads = Vec::new();
        let mut htf = Vec::new();
        collect(&program, &mut overloads, &mut htf);
        overloads.sort_unstable();
        overloads.dedup();
        Loaded {
            program,
            source_sha256,
            input_digest,
            overloads,
            htf,
        }
    }
}

/// Overloads and HTF needs of `p` and every nested sub-program (load time).
fn collect(p: &PineProgram, ov: &mut Vec<OverloadId>, htf: &mut Vec<(String, u32)>) {
    for i in &p.code {
        if i.op == op::CALL_BUILTIN {
            ov.push(OverloadId(i.a));
        }
    }
    for s in &p.subprograms {
        let known = htf
            .iter()
            .any(|n| n.1 == s.timeframe_s && n.0 == s.symbol_key);
        if !known {
            // COPY: one HTF symbol key (a short string), once per distinct need at load —
            // `Loaded` outlives nothing it could borrow it from but its own program, and
            // `htf_needs` is §3.5's owned `&[(String, u32)]` — a self-borrowing `Loaded` rejected.
            htf.push((s.symbol_key.clone(), s.timeframe_s));
        }
        collect(&s.program, ov, htf);
    }
}

fn input_digest(p: &PineProgram) -> [u8; 32] {
    let mut h = Sha256::new();
    for d in &p.meta.inputs {
        h.update(p.syms.resolve_bytes(d.name));
        h.update(&[0, d.kind as u8, d.ty as u8]);
        h.update(&d.default_bits.to_le_bytes());
    }
    h.finalize()
}

/// Why a trial never compiled.
#[derive(Debug, Clone)]
pub enum CompileRefusal {
    /// `pine_lang::parse` (1000–2999, `3001`).
    Parse(Vec<Diag>),
    /// `pine_sema::compile` (3000–3999, 5001–5007).
    Compile(Vec<Diag>),
    /// `pine_vm::verify` — sema emitted what the verifier refuses: **our defect**.
    Verify(VerifyError),
    /// `6004` (contract §3.3, "unknown key"): `TrialSpec::inputs` names the program has no input
    /// for, in `inputs` order. An override is refused, never silently ignored.
    Inputs(Vec<String>),
}

impl CompileRefusal {
    /// The bucket code: the first `Severity::Error` diagnostic's (`Parse`, `Compile`),
    /// `VerifyError::code()` (`Verify`), `DiagCode(6004)` (`Inputs`).
    pub fn code(&self) -> DiagCode {
        let first = |ds: &[Diag]| {
            ds.iter()
                .find(|d| d.sev == Severity::Error)
                .or(ds.first())
                .map_or(DiagCode(0), |d| d.code)
        };
        match self {
            CompileRefusal::Parse(ds) | CompileRefusal::Compile(ds) => first(ds),
            CompileRefusal::Verify(e) => e.code(),
            CompileRefusal::Inputs(_) => DiagCode(6004),
        }
    }
}

/// Parse `spec.source` into a fresh table and compile it under `opts` (`opts.dialect` is set from
/// the parse).
fn parse_compile(
    spec: &TrialSpec<'_>,
    libs: &mut LibCache,
    opts: &mut CompileOpts,
) -> Result<PineProgram, CompileRefusal> {
    let mut syms = SymTable::new();
    let ast =
        pine_lang::parse(spec.source, FileId::ROOT, &mut syms).map_err(CompileRefusal::Parse)?;
    opts.dialect = ast.dialect();
    pine_sema::compile(&ast, syms, opts, libs).map_err(CompileRefusal::Compile)
}

/// Parse → compile (`live_bound = true`, `BarClose`; on a `3052` refusal re-parse and recompile
/// `Realtime`) → the `6004` input check (`pine_sema::unmatched_input_overrides`, impl-02 v4
/// §4.12.5) → verify. Load time; allocates. **The lane's only tier-1 compile path** (review S5,
/// row 28): `cli pine` and `pvm-select` (impl-10 v4 §4.2.3) both call it.
pub fn compile_trial(spec: &TrialSpec<'_>, libs: &mut LibCache) -> Result<Loaded, CompileRefusal> {
    let sha = core_crypto::sha256(spec.source.as_bytes());
    // COPY: the chart key, the 4 syminfo strings, the overrides and the ticker map (each ≤ a few
    // hundred bytes), once per trial — `CompileOpts` (pine-ir, canonical) owns them and
    // `pine_sema::compile` takes `&CompileOpts` — a borrowed `CompileOpts<'a>` rejected: a contract
    // change (CAR), not this lane's. Built once; the `3052` retry only flips `mode`.
    let mut opts = CompileOpts {
        symbol_key: spec.symbol_key.to_owned(),
        syminfo_strs: spec.syminfo_strs.map(str::to_owned),
        inputs: spec.inputs.to_vec(),
        ticker_map: spec.ticker_map.to_vec(),
        dialect: Dialect::V6, // overwritten by each parse (`parse_compile`)
        mode: ExecMode::BarClose,
        live_bound: true,
        chart: spec.chart,
        limits: spec.limits,
        source_sha256: sha,
    };
    let program = match parse_compile(spec, libs, &mut opts) {
        Err(CompileRefusal::Compile(ds))
            if ds
                .iter()
                .any(|d| d.code.0 == 3052 && d.sev == Severity::Error) =>
        {
            opts.mode = ExecMode::Realtime;
            parse_compile(spec, libs, &mut opts)?
        }
        r => r?,
    };
    let unmatched = pine_sema::unmatched_input_overrides(&program, &opts);
    if !unmatched.is_empty() {
        // COPY: the unmatched override names (≤ `inputs.len()` short strings), once, on the refusal
        // path — `CompileRefusal::Inputs` owns its names and `opts` dies here — a refusal
        // borrowing `spec.inputs` rejected (the enum is `'static`, §3.5).
        let names = unmatched.iter().map(|n| (*n).to_owned()).collect();
        return Err(CompileRefusal::Inputs(names));
    }
    pine_vm::verify(&program, &pine_std::BUILTINS).map_err(CompileRefusal::Verify)?;
    Ok(Loaded::wrap(program, sha))
}

/// The fee row a trial is charged under — resolved by the host from the `--fee-bps` flag set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeRow {
    /// `core_types::VenueId` raw byte.
    pub venue: u8,
    /// `core_types::InstrumentClass` raw byte.
    pub class: u8,
    /// Maker bps (recorded; tier 1 charges taker on every fill, §4.6).
    pub maker_bps: u32,
    /// Taker bps — the commission of every tier-1 fill.
    pub taker_bps: u32,
}

/// One screen configuration (a scoring window, a notional, a fee row), shared by every trial of a
/// `run_group` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenCfg {
    /// Scoring window start, a UTC day boundary. Bars before it are warm-up.
    pub from_ms: i64,
    /// Scoring window end (exclusive), a UTC day boundary. Bars with `close_ms ≤ to_ms` are stepped.
    pub to_ms: i64,
    /// Selection notional, USD ×1e6 (normally [`crate::SEL_NOTIONAL_1E6`]).
    pub notional_1e6: i64,
    /// Fee row.
    pub fee: FeeRow,
}

/// The declaration the screen's `TvBroker` sees (§4.3.4): commission = `Percent` at the taker rate,
/// slippage 0, initial capital `CAPITAL_UNITS × max(1, pyramiding) × notional`; the rest as declared.
pub fn screen_decl(decl: &StrategyDecl, cfg: &ScreenCfg) -> StrategyDecl {
    let mut d = *decl;
    d.commission_type = CommissionType::Percent;
    d.commission_value_1e6 = i64::from(cfg.fee.taker_bps).saturating_mul(10_000);
    d.slippage_ticks = 0;
    d.initial_capital_1e6 = CAPITAL_UNITS
        .saturating_mul(i64::from(decl.pyramiding.max(1)))
        .saturating_mul(cfg.notional_1e6);
    d
}

/// The bar files of one chart and every HTF file its programs need, opened once.
pub struct TrialBars {
    chart: BarFile,
    htf: Vec<(u64, u32, BarFile)>,
    /// Needs whose file did not exist: `(symbol_hash, tf_s)` (`Refusal::HtfMissing`).
    absent: Vec<(u64, u32)>,
}

impl TrialBars {
    /// Open `<root>/…/<tf_s>.bars` for the chart and for each `(key, tf)` of `needs`, whole-file
    /// mapped (`clamp == None`) or clamped — the same clamp for all. A need whose file does not
    /// exist is recorded absent: `run_group` refuses its readers (`Refusal::HtfMissing`) and runs
    /// the rest. Any other refusal of any file, and a missing chart file, is the call's (7001).
    pub fn open(
        root: &std::path::Path,
        symbol_key: &str,
        tf_s: u32,
        needs: &[(String, u32)],
        clamp: Option<Clamp>,
    ) -> Result<TrialBars, BarFileErr> {
        let open1 = |p: &std::path::Path| match clamp {
            None => BarFile::open(p),
            Some(c) => BarFile::open_clamped(p, c),
        };
        let chart = open1(&bar_path(root, symbol_key, tf_s)?)?;
        let mut htf = Vec::with_capacity(needs.len());
        let mut absent = Vec::new();
        let mut i = 0;
        while i < needs.len() {
            let (key, tf) = (&needs[i].0, needs[i].1);
            let h = pine_ir::symbol_hash(key.as_bytes());
            let p = bar_path(root, key, tf)?;
            // pin:07-htf-absent: "does not exist" is `stat` → `NotFound` (literal); any other
            // error of an HTF file is the call's.
            match std::fs::metadata(&p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => absent.push((h, tf)),
                _ => htf.push((h, tf, open1(&p)?)),
            }
            i += 1;
        }
        Ok(TrialBars { chart, htf, absent })
    }

    /// Build from already-open files (tests, `pine report`'s shadow runs).
    pub fn from_files(chart: BarFile, htf: Vec<(u64, u32, BarFile)>) -> TrialBars {
        TrialBars {
            chart,
            htf,
            absent: Vec::new(),
        }
    }

    /// Whether `(symbol_hash, tf_s)` was a need whose file did not exist.
    pub(crate) fn is_absent(&self, symbol_hash: u64, tf_s: u32) -> bool {
        self.absent
            .iter()
            .any(|a| a.0 == symbol_hash && a.1 == tf_s)
    }

    /// The chart file.
    pub fn chart(&self) -> &BarFile {
        &self.chart
    }

    /// The HTF file for `(symbol_hash, tf_s)`.
    pub fn htf(&self, symbol_hash: u64, tf_s: u32) -> Option<&BarFile> {
        self.htf
            .iter()
            .find(|h| h.0 == symbol_hash && h.1 == tf_s)
            .map(|h| &h.2)
    }
}

/// Where a trial's fault came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultOrigin {
    /// `StepOutcome::Fault` of the program or one of its sub-programs.
    Vm(FaultKind),
    /// `BrokerFault::Script` from `submit`.
    Broker(ScriptRefusal),
}

/// Contract §3.13's incident: `(FaultKind, pc, bar.index, source sha)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultRecord {
    /// Origin.
    pub origin: FaultOrigin,
    /// `VmInstance::last_fault().1`; `u32::MAX` for a broker refusal.
    pub pc: u32,
    /// `Bar::index` of the evaluated (chart or HTF) bar.
    pub bar_index: u32,
    /// The fault was a sub-program's.
    pub sub: bool,
    /// SHA-256 of the trial's source.
    pub source_sha256: [u8; 32],
}

/// Why a trial was not stepped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `TvBroker::new` refused the declaration (5100–5199).
    Broker(DiagCode),
    /// Fewer rows before `from_ms` than `meta.warmup_bars`.
    ShortWarmup {
        /// Rows before the window.
        have: u32,
        /// `ProgramMeta::warmup_bars`.
        need: u32,
    },
    /// The stepped rows do not reach into the first and the last scored day.
    Coverage {
        /// First stepped open.
        first_open_ms: i64,
        /// Last stepped close.
        last_close_ms: i64,
    },
    /// The unit notional floors to zero at the window's highest close (`qty_step` too coarse).
    UnitBelowStep {
        /// `notional × 1e6 / max_close`, qty ×1e6.
        unit_qty_1e6: i64,
        /// `ChartInfo::qty_step_1e6`.
        step_1e6: i64,
    },
    /// No HTF bar file for a sub-program the trial reads, directly or through a nested one.
    HtfMissing {
        /// `SubProgram::symbol_hash`.
        symbol_hash: u64,
        /// `SubProgram::timeframe_s`.
        tf_s: u32,
    },
    /// That sub-program's HTF file starts after the containing HTF bar of the chart's first row.
    HtfCoverage {
        /// `SubProgram::symbol_hash`.
        symbol_hash: u64,
        /// `SubProgram::timeframe_s`.
        tf_s: u32,
    },
}

/// A trial's end state. `Default` (`Completed`) exists only so a host can preallocate
/// `run_group`'s `out`; it is never a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrialStatus {
    /// Stepped to the end; the summary is a number.
    #[default]
    Completed,
    /// Stopped by a script fault; the summary carries `FAULTED` and no P&L (I13).
    Faulted(FaultRecord),
    /// Never stepped; the summary is default and must not be written as a row.
    Refused(Refusal),
}

/// One trial's result. The daily return series is `Workspace::day_returns(k)` until the next run.
/// `TrialOutcome::default()` is placeholder storage for `run_group`'s `out`, never a result.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TrialOutcome {
    /// End state.
    pub status: TrialStatus,
    /// The row (FIXED_NOTIONAL always set; see §4.5).
    pub summary: TrialSummary,
    /// `TvBroker::record()` at the end.
    pub record: RunRecord,
}

/// Work counters of one group, for the host's throughput meter (the only clock is the host's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GroupStats {
    /// Chart bars read.
    pub chart_bars: u64,
    /// HTF bars stepped (deduplicated sub-programs).
    pub htf_bars: u64,
    /// `step_bar` evaluations (chart and HTF).
    pub bar_evals: u64,
    /// `step_tick` evaluations (TV `calc_on_order_fills` re-evaluations).
    pub tick_evals: u64,
    /// Candidates in the group.
    pub candidates: u32,
}

/// A failure of the call, not of a trial. `is_defect()` ones are ours: fail fast (§4.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenErr {
    /// Window not day-aligned, empty, or a notional ≤ 0.
    Window,
    /// 0 or more than `MAX_GROUP` candidates, or `out` too short.
    GroupSize,
    /// `verify` refused a program that `compile_trial` accepted — defect.
    Verify(VerifyError),
    /// `FaultKind::Internal` or `JournalFull` — defect.
    VmDefect {
        /// The kind.
        kind: FaultKind,
        /// `last_fault().1`.
        pc: u32,
        /// `Bar::index`.
        bar_index: u32,
    },
    /// `BrokerFault::Internal` — defect.
    BrokerDefect(InternalDefect),
    /// Two sub-programs with one `SubProgram::key` and different code — defect (P4's key law).
    SubKeyCollision {
        /// The key.
        key: u64,
    },
}

impl ScreenErr {
    /// `Verify`, `VmDefect`, `BrokerDefect`, `SubKeyCollision`.
    pub const fn is_defect(&self) -> bool {
        matches!(
            self,
            ScreenErr::Verify(_)
                | ScreenErr::VmDefect { .. }
                | ScreenErr::BrokerDefect(_)
                | ScreenErr::SubKeyCollision { .. }
        )
    }
}

/// The loop's test seam. Generic, never `dyn`; [`NoProbe`]'s bodies are empty and inline away, so a
/// normal run pays nothing.
pub trait LoopProbe {
    /// Just before the first bar of the group.
    fn before_loop(&mut self);
    /// Just after the last bar of the group.
    fn after_loop(&mut self);
    /// Sub-program slot `slot` published HTF bar `htf` while chart row `chart_row` is current.
    #[inline(always)]
    fn on_publish(&mut self, _slot: u32, _htf: &Bar, _chart_row: u32) {}
    /// Candidate `k` was just evaluated on `bar` (`intrabar`: `step_tick`), before its `submit`;
    /// `plots` is that instance's `VmInstance::plots()`, borrowed.
    #[inline(always)]
    fn on_eval(&mut self, _k: u32, _bar: &Bar, _intrabar: bool, _plots: &[f64]) {}
}

/// The probe of every normal run.
pub struct NoProbe;

impl LoopProbe for NoProbe {
    #[inline(always)]
    fn before_loop(&mut self) {}
    #[inline(always)]
    fn after_loop(&mut self) {}
}

/// Arenas and accumulators reused across groups. Grows at group setup, never in the loop.
pub struct Workspace {
    /// One VM arena per candidate; each holds `total_bytes + 63` so a 64-aligned window fits.
    arenas: Vec<Vec<u8>>,
    /// One VM arena per distinct sub-program of a group, the same way.
    sub_arenas: Vec<Vec<u8>>,
    accums: Vec<TrialAccum>,
}

impl Workspace {
    /// Empty; grows on first use.
    pub fn new() -> Workspace {
        Workspace {
            arenas: Vec::new(),
            sub_arenas: Vec::new(),
            accums: Vec::new(),
        }
    }

    /// Candidate `k`'s daily return series from the last `run_group`: that day's net P&L in bps
    /// of the notional, `0.0` on a flat day (contract §3.16.2; §4.5.2). Empty for a FAULTED trial.
    pub fn day_returns(&self, k: usize) -> &[f64] {
        self.accums.get(k).map_or(&[], |a| a.day_returns())
    }
}

impl Default for Workspace {
    fn default() -> Workspace {
        Workspace::new()
    }
}

/// A candidate's run state inside `run_group`.
struct Cand<'p> {
    vm: VmInstance<'p>,
    broker: Option<TvBroker>,
    /// Closed trades already handed to the accumulator.
    seen: u32,
    running: bool,
    /// This trial's run bounds (§4.3.6): rows `[0, end)` are stepped, `from_row` opens the window.
    end: usize,
    from_row: usize,
    /// Its own direct sub-programs' `SlotKey`s: `keys[k0..k1]` of the group's key arena.
    k0: usize,
    k1: usize,
}

/// One distinct sub-program of the group (flattened children first, deduplicated by key — P4).
struct Flat<'p> {
    sp: &'p SubProgram,
    /// Its own direct sub-programs' `SlotKey`s: `keys[k0..k1]`.
    k0: usize,
    k1: usize,
}

/// A distinct sub-program's run state inside `run_group`; its store slot is its index.
struct Sub<'p> {
    vm: Option<VmInstance<'p>>,
    rows: &'p [Bar],
    next: usize,
    lookahead: bool,
    n_out: usize,
    k0: usize,
    k1: usize,
    /// Stepped: an instance exists, it never faulted, and a running trial reads it.
    live: bool,
}

/// `code`, `consts`, `argwin`, `layout`, `mode`, `dialect` equal (§4.3.2's dedup identity).
fn same_code(a: &PineProgram, b: &PineProgram) -> bool {
    a.code == b.code
        && a.consts == b.consts
        && a.argwin == b.argwin
        && a.layout == b.layout
        && a.mode == b.mode
        && a.dialect == b.dialect
}

/// Flatten `p`'s sub-programs into `flat` — children first (contract §3.8's list order), one entry
/// per key across the group (P4) — and append `p`'s own direct `SlotKey`s to `keys`; returns their
/// range. Setup; allocates.
fn flatten<'p>(
    p: &'p PineProgram,
    flat: &mut Vec<Flat<'p>>,
    keys: &mut Vec<SlotKey>,
) -> Result<(usize, usize), ScreenErr> {
    let mut i = 0;
    while i < p.subprograms.len() {
        let sp = &p.subprograms[i];
        match flat.iter().position(|f| f.sp.key == sp.key) {
            Some(j) => {
                if !same_code(&flat[j].sp.program, &sp.program) {
                    return Err(ScreenErr::SubKeyCollision { key: sp.key });
                }
            }
            None => {
                let (k0, k1) = flatten(&sp.program, flat, keys)?;
                flat.push(Flat { sp, k0, k1 });
            }
        }
        i += 1;
    }
    let k0 = keys.len();
    i = 0;
    while i < p.subprograms.len() {
        let key = p.subprograms[i].key;
        let slot = flat.iter().position(|f| f.sp.key == key).unwrap_or(0) as u32;
        // pin:07-direct-key-once: a program that requests one key twice holds it once in its map.
        if !keys[k0..].iter().any(|s| s.key == key) {
            keys.push(SlotKey { key, slot });
        }
        i += 1;
    }
    Ok((k0, keys.len()))
}

/// Mark in `reads` (one flag per distinct sub-program) every sub-program a reader with direct keys
/// `keys[k0..k1]` reads, directly or through a parent. Children precede parents in `flat`, so one
/// descending sweep closes the set.
fn mark_reads(flat: &[Flat<'_>], keys: &[SlotKey], k0: usize, k1: usize, reads: &mut [bool]) {
    let mut i = k0;
    while i < k1 {
        reads[keys[i].slot as usize] = true;
        i += 1;
    }
    let mut s = flat.len();
    while s > 0 {
        s -= 1;
        if reads[s] {
            let mut j = flat[s].k0;
            while j < flat[s].k1 {
                reads[keys[j].slot as usize] = true;
                j += 1;
            }
        }
    }
}

/// Feed the closed trades `broker` closed since `seen` to `acc` (index loop, no allocation).
#[inline]
fn drain_closed(broker: &TvBroker, acc: &mut TrialAccum, seen: &mut u32) {
    let v = broker.view();
    let first = v.closedtrades - v.closed_retained;
    let mut j = *seen;
    while j < v.closedtrades {
        if let Some(t) = broker.closed_trade(j - first) {
            acc.on_closed_trade(t);
        }
        j += 1;
    }
    *seen = v.closedtrades;
}

/// Recompute every sub-program's `live` flag: an instance, never faulted (`vm` kept, `live` cleared
/// by the fault), and some running trial reads it. Cold (setup and the fault path); no allocation.
fn relive(subs: &mut [Sub<'_>], cands: &[Cand<'_>], reads: &[bool], faulted: &[bool]) {
    let n = subs.len();
    let mut s = 0;
    while s < n {
        let mut any = false;
        let mut k = 0;
        while k < cands.len() {
            any |= cands[k].running && reads[k * n + s];
            k += 1;
        }
        subs[s].live = subs[s].vm.is_some() && !faulted[s] && any;
        s += 1;
    }
}

/// Step `trials` (K ≥ 1 candidates on the chart of `bars`, one `cfg`) through the contract §3.15.3
/// loop, one bar read per K evaluations (§4.3, §4.4) — **the one runner**. Sub-programs are stepped
/// first, deduplicated across the group (§4.3.2). `out` is caller storage of ≥ K entries
/// (`TrialOutcome::default()`); `out[..K]` is overwritten on `Ok`, one outcome per trial in
/// `trials` order, and is meaningless on `Err`.
pub fn run_group<P: LoopProbe>(
    trials: &[&Loaded],
    bars: &TrialBars,
    cfg: &ScreenCfg,
    ws: &mut Workspace,
    out: &mut [TrialOutcome],
    probe: &mut P,
) -> Result<GroupStats, ScreenErr> {
    if cfg.from_ms.rem_euclid(DAY_MS) != 0
        || cfg.to_ms.rem_euclid(DAY_MS) != 0
        || cfg.to_ms <= cfg.from_ms
        || cfg.notional_1e6 <= 0
    {
        return Err(ScreenErr::Window);
    }
    let k_n = trials.len();
    if k_n == 0 || k_n > MAX_GROUP || out.len() < k_n {
        return Err(ScreenErr::GroupSize);
    }
    let rows = bars.chart().rows();
    // §4.3.6 run bounds: `end` rows close inside the window's end; `from_row` opens the window.
    let end = rows.partition_point(|b| b.close_ms <= cfg.to_ms);
    let from_row = bars.chart().seek(cfg.from_ms).min(end);
    let days = ((cfg.to_ms - cfg.from_ms) / DAY_MS) as u32;
    let mut stats = GroupStats {
        candidates: k_n as u32,
        ..GroupStats::default()
    };

    // ---- setup (allocates) ----
    // 1. flatten + dedup (§4.3.2): `flat` children first; each reader's direct keys in `keys`.
    let mut flat: Vec<Flat<'_>> = Vec::new();
    let mut keys: Vec<SlotKey> = Vec::new();
    let mut cand_keys: Vec<(usize, usize)> = Vec::with_capacity(k_n);
    let mut k = 0;
    while k < k_n {
        cand_keys.push(flatten(&trials[k].program, &mut flat, &mut keys)?);
        k += 1;
    }
    let n_sub = flat.len();
    // `reads[k * n_sub + s]`: trial k reads distinct sub-program s, directly or through a parent.
    let mut reads = vec![false; k_n * n_sub];
    k = 0;
    while k < k_n {
        let (k0, k1) = cand_keys[k];
        mark_reads(&flat, &keys, k0, k1, &mut reads[k * n_sub..(k + 1) * n_sub]);
        k += 1;
    }
    // Each distinct sub-program's HTF rows, or its per-trial refusal (I32).
    let mut sub_rows: Vec<&[Bar]> = Vec::with_capacity(n_sub);
    let mut sub_refusal: Vec<Option<Refusal>> = Vec::with_capacity(n_sub);
    let mut s = 0;
    while s < n_sub {
        let sp = flat[s].sp;
        let (h, tf) = (sp.symbol_hash, sp.timeframe_s);
        // `TrialBars::open` recorded the need absent (`stat` → NotFound): `HtfMissing`.
        // pin:07-htf-unopened-is-missing: a need `TrialBars` neither opened nor recorded absent
        // (`from_files` without it) has no HTF bar file either — the same refusal, literal.
        let missing = Some(Refusal::HtfMissing {
            symbol_hash: h,
            tf_s: tf,
        });
        let (r, refusal) = match bars.htf(h, tf) {
            None if bars.is_absent(h, tf) => (&[][..], missing),
            None => (&[][..], missing),
            Some(f) => {
                let r = f.rows();
                // §4.3.6: the HTF file starts at or before the containing HTF open of chart row 0.
                let late = !rows.is_empty()
                    && (r.is_empty() || r[0].open_ms > barclock::open_ms(rows[0].open_ms, tf));
                if late {
                    (
                        &[][..],
                        Some(Refusal::HtfCoverage {
                            symbol_hash: h,
                            tf_s: tf,
                        }),
                    )
                } else {
                    (r, None)
                }
            }
        };
        sub_rows.push(r);
        sub_refusal.push(refusal);
        s += 1;
    }

    let Workspace {
        arenas,
        sub_arenas,
        accums,
    } = ws;
    if arenas.len() < k_n {
        arenas.resize_with(k_n, Vec::new);
    }
    if sub_arenas.len() < n_sub {
        sub_arenas.resize_with(n_sub, Vec::new);
    }
    if accums.len() < k_n {
        accums.resize_with(k_n, TrialAccum::new);
    }
    let store_specs: Vec<SeriesSpec> = flat
        .iter()
        .map(|f| SeriesSpec {
            n_out: f.sp.n_out,
            history: f.sp.history,
        })
        .collect();
    let mut store = SeriesStore::with_slots(&store_specs);
    let sizing = SizingMode::FixedNotional {
        notional_1e6: cfg.notional_1e6,
    };
    let mut cands: Vec<Cand<'_>> = Vec::with_capacity(k_n);
    let mut group_end = 0usize;
    for (k, arena) in arenas.iter_mut().take(k_n).enumerate() {
        let t = trials[k];
        let prog = &t.program;
        let need = prog.layout.total_bytes as usize + 63;
        if arena.len() < need {
            arena.resize(need, 0);
        }
        out[k] = TrialOutcome::default();
        let v = pine_vm::verify(prog, &pine_std::BUILTINS).map_err(ScreenErr::Verify)?;
        let off = arena.as_ptr().align_offset(64);
        let len = prog.layout.total_bytes as usize;
        let vm = VmInstance::new(v, &mut arena[off..off + len]);
        let meta = &prog.meta;
        let my = &reads[k * n_sub..(k + 1) * n_sub];
        // The trial's HTF refusal: the first (children-first order) sub-program it reads that has
        // none (pin:07-htf-refusal-order); the lookahead tail clamp (§4.3.6) of the rest.
        // pin:07-lookahead-clamp-per-trial: §4.3.6's clamp bounds the run of each trial that reads a
        // lookahead sub-program, not the group's (I15: grouping never changes a trial's row).
        let mut htf = None;
        let mut end_k = end;
        let mut s = 0;
        while s < n_sub {
            if my[s] {
                if htf.is_none() {
                    htf = sub_refusal[s];
                }
                let r = sub_rows[s];
                if flat[s].sp.lookahead && !r.is_empty() {
                    let last_close = r[r.len() - 1].close_ms;
                    end_k = end_k.min(rows[..end].partition_point(|b| b.open_ms < last_close));
                }
            }
            s += 1;
        }
        let from_k = from_row.min(end_k);
        accums[k].reserve(end_k as u32, days);
        let refusal = htf.or_else(|| refuse(t, rows, end_k, from_k, cfg));
        let broker = match refusal {
            Some(r) => {
                out[k].status = TrialStatus::Refused(r);
                None
            }
            None => {
                // COPY: the trial's ProgramMeta (decl + input descriptors, ≤ a few KiB), once per
                // trial at setup — `TvBroker::new` reads the declaration from `&ProgramMeta` and
                // the screen's differs (§4.3.4) — a broker ctor taking the decl apart rejected:
                // pine-broker's frozen signature.
                let mut m2 = meta.clone();
                m2.decl = screen_decl(&meta.decl, cfg);
                match TvBroker::new(
                    &m2,
                    TvCfg {
                        sizing,
                        day_offset_s: 0,
                        _pad: 0,
                    },
                ) {
                    Ok(b) => Some(b),
                    Err(code) => {
                        out[k].status = TrialStatus::Refused(Refusal::Broker(code));
                        None
                    }
                }
            }
        };
        let running = broker.is_some();
        if let Some(b) = &broker {
            accums[k].begin(&AccumCfg {
                sizing,
                pointvalue_1e6: meta.chart.pointvalue_1e6,
                from_ms: cfg.from_ms,
                to_ms: cfg.to_ms,
                intrabar_approx: meta.uses_intrabar,
            });
            if from_k == 0 {
                accums[k].baseline(b.view().equity_1e12, b.record().commission_paid_1e12);
            }
            group_end = group_end.max(end_k);
        }
        let (k0, k1) = cand_keys[k];
        cands.push(Cand {
            vm,
            broker,
            seen: 0,
            running,
            end: end_k,
            from_row: from_k,
            k0,
            k1,
        });
    }
    // 3. one instance per distinct sub-program some running trial reads (P4).
    let mut subs: Vec<Sub<'_>> = Vec::with_capacity(n_sub);
    let mut faulted = vec![false; n_sub];
    for (s, arena) in sub_arenas.iter_mut().take(n_sub).enumerate() {
        let f = &flat[s];
        let mut read = false;
        let mut k = 0;
        while k < k_n {
            read |= cands[k].running && reads[k * n_sub + s];
            k += 1;
        }
        let vm = if read && sub_refusal[s].is_none() {
            let prog: &PineProgram = &f.sp.program;
            let need = prog.layout.total_bytes as usize + 63;
            if arena.len() < need {
                arena.resize(need, 0);
            }
            let v = pine_vm::verify(prog, &pine_std::BUILTINS).map_err(ScreenErr::Verify)?;
            let off = arena.as_ptr().align_offset(64);
            let len = prog.layout.total_bytes as usize;
            Some(VmInstance::new(v, &mut arena[off..off + len]))
        } else {
            None
        };
        subs.push(Sub {
            vm,
            rows: sub_rows[s],
            next: 0,
            lookahead: f.sp.lookahead,
            n_out: f.sp.n_out as usize,
            k0: f.k0,
            k1: f.k1,
            live: false,
        });
    }
    relive(&mut subs, &cands, &reads, &faulted);

    // ---- the loop (allocation-free) ----
    probe.before_loop();
    let mut dirty = false;
    let mut b = 0usize;
    while b < group_end {
        let row = &rows[b];
        // 1. sub-programs first, children before parents.
        let mut s = 0;
        while s < n_sub {
            if !subs[s].live {
                s += 1;
                continue;
            }
            let sub = &mut subs[s];
            let Some(vm) = sub.vm.as_mut() else {
                s += 1;
                continue;
            };
            while sub.next < sub.rows.len() {
                let h = &sub.rows[sub.next];
                // CAR-04-1: a lookahead sub-program also steps the HTF bar containing the open.
                let due = if sub.lookahead {
                    h.open_ms <= row.open_ms
                } else {
                    h.close_ms <= row.close_ms
                };
                if !due {
                    break;
                }
                let o = {
                    let mut ctx = ScreenCtx {
                        store: &store,
                        keys: &keys[sub.k0..sub.k1],
                        broker: None,
                        now_ms: h.close_ms,
                    };
                    vm.step_bar(h, &mut ctx)
                };
                stats.htf_bars += 1;
                stats.bar_evals += 1;
                if let StepOutcome::Fault(kind) = o {
                    let pc = vm.last_fault().map_or(u32::MAX, |f| f.1);
                    if matches!(kind, FaultKind::Internal | FaultKind::JournalFull) {
                        return Err(ScreenErr::VmDefect {
                            kind,
                            pc,
                            bar_index: h.index,
                        });
                    }
                    // Every trial still being stepped that reads slot s (directly or through a
                    // parent). pin:07-sub-fault-after-end: a trial past its own `end` is complete
                    // and keeps its row (I15: a group never changes a trial's row).
                    let mut k = 0;
                    while k < k_n {
                        if cands[k].running && b < cands[k].end && reads[k * n_sub + s] {
                            out[k].status = TrialStatus::Faulted(FaultRecord {
                                origin: FaultOrigin::Vm(kind),
                                pc,
                                bar_index: h.index,
                                sub: true,
                                source_sha256: trials[k].source_sha256,
                            });
                            cands[k].running = false;
                        }
                        k += 1;
                    }
                    faulted[s] = true;
                    break;
                }
                debug_assert!(
                    vm.bars_seen() == u64::from(h.index) + 1,
                    "I31: a sub-program's bars_seen tracks its Bar::index"
                );
                store.publish(
                    s as u32,
                    &vm.plots()[..sub.n_out],
                    h.open_ms,
                    h.close_ms,
                    true,
                );
                probe.on_publish(s as u32, h, b as u32);
                sub.next += 1;
            }
            if faulted[s] && subs[s].live {
                relive(&mut subs, &cands, &reads, &faulted);
            }
            s += 1;
        }
        // 2-5. the candidates.
        stats.chart_bars += 1;
        let mut k = 0;
        while k < k_n {
            let c = &mut cands[k];
            if !c.running || b >= c.end {
                k += 1;
                continue;
            }
            let Some(broker) = c.broker.as_mut() else {
                k += 1;
                continue;
            };
            // §4.3.6 / I30: the trial's final stepped row is stepped as a LAST-flagged copy.
            // COPY: one 64-B Bar, once per trial — the file is read-only and the flags differ — a
            // flagged file row rejected: no file row carries LAST (I30).
            let last_copy;
            let bar: &Bar = if b + 1 == c.end {
                last_copy = Bar {
                    flags: row.flags | barflag::LAST | barflag::LAST_CONFIRMED_HISTORY,
                    ..*row
                };
                &last_copy
            } else {
                row
            };
            let mut step = broker.open_bar(bar);
            loop {
                let (eb, intrabar) = match &step {
                    TvStep::Done => break,
                    TvStep::Evaluate { bar, intrabar } => (bar, *intrabar),
                };
                let o = {
                    let mut ctx = ScreenCtx {
                        store: &store,
                        keys: &keys[c.k0..c.k1],
                        broker: Some(&*broker),
                        now_ms: eb.close_ms,
                    };
                    if intrabar {
                        stats.tick_evals += 1;
                        c.vm.step_tick(eb, &mut ctx)
                    } else {
                        stats.bar_evals += 1;
                        c.vm.step_bar(eb, &mut ctx)
                    }
                };
                probe.on_eval(k as u32, eb, intrabar, c.vm.plots());
                if let StepOutcome::Fault(kind) = o {
                    let pc = c.vm.last_fault().map_or(u32::MAX, |f| f.1);
                    if matches!(kind, FaultKind::Internal | FaultKind::JournalFull) {
                        return Err(ScreenErr::VmDefect {
                            kind,
                            pc,
                            bar_index: eb.index,
                        });
                    }
                    out[k].status = TrialStatus::Faulted(FaultRecord {
                        origin: FaultOrigin::Vm(kind),
                        pc,
                        bar_index: eb.index,
                        sub: false,
                        source_sha256: trials[k].source_sha256,
                    });
                    c.running = false;
                    break;
                }
                debug_assert!(
                    intrabar || c.vm.bars_seen() == u64::from(eb.index) + 1,
                    "I31: bars_seen tracks Bar::index"
                );
                let eb_index = eb.index;
                // COMMIT after every evaluation (W21, I11): the intents go in unfiltered.
                step = match broker.submit(c.vm.intents()) {
                    Ok(s) => s,
                    Err(BrokerFault::Script(r)) => {
                        out[k].status = TrialStatus::Faulted(FaultRecord {
                            origin: FaultOrigin::Broker(r),
                            pc: u32::MAX,
                            bar_index: eb_index,
                            sub: false,
                            source_sha256: trials[k].source_sha256,
                        });
                        c.running = false;
                        break;
                    }
                    Err(BrokerFault::Internal(d)) => return Err(ScreenErr::BrokerDefect(d)),
                };
            }
            if c.running {
                let acc = &mut accums[k];
                acc.on_bar(bar, broker.view().equity_1e12);
                drain_closed(broker, acc, &mut c.seen);
                if b + 1 == c.from_row {
                    acc.baseline(
                        broker.view().equity_1e12,
                        broker.record().commission_paid_1e12,
                    );
                }
            } else {
                dirty = true;
            }
            k += 1;
        }
        if dirty {
            relive(&mut subs, &cands, &reads, &faulted);
            dirty = false;
        }
        b += 1;
    }
    probe.after_loop();

    // ---- finish (cold) ----
    let mut k = 0;
    while k < k_n {
        let c = &cands[k];
        if let Some(broker) = &c.broker {
            let acc = &mut accums[k];
            let end_kind = match out[k].status {
                TrialStatus::Faulted(_) => TrialEnd::Faulted,
                _ => TrialEnd::Completed,
            };
            if end_kind == TrialEnd::Completed {
                let mut i = 0;
                while i < broker.view().opentrades {
                    if let Some(t) = broker.open_trade(i) {
                        acc.on_open_trade_at_end(t);
                    }
                    i += 1;
                }
            }
            // §4.3.2: max over the trial's instance and every sub-program it reads.
            let mut max_insns = c.vm.counters().max_insns;
            let mut s = 0;
            while s < n_sub {
                if reads[k * n_sub + s] {
                    if let Some(vm) = &subs[s].vm {
                        max_insns = max_insns.max(vm.counters().max_insns);
                    }
                }
                s += 1;
            }
            let record = broker.record();
            out[k].summary = acc.finish(record.commission_paid_1e12, max_insns, end_kind, 0);
            out[k].record = record;
        }
        k += 1;
    }
    Ok(stats)
}

/// The per-trial setup refusals of §4.3.6 / §4.3.4 after the HTF ones (§4.3.2), in that order
/// (load time).
fn refuse(
    t: &Loaded,
    rows: &[Bar],
    end: usize,
    from_row: usize,
    cfg: &ScreenCfg,
) -> Option<Refusal> {
    let meta = &t.program.meta;
    if (from_row as u32) < meta.warmup_bars {
        return Some(Refusal::ShortWarmup {
            have: from_row as u32,
            need: meta.warmup_bars,
        });
    }
    // pin:07-coverage-reach: the stepped rows "reach into" a day when some stepped row's
    // `[open, close)` overlaps it: first scored day `[from, from + 1 d)`, last `[to − 1 d, to)`.
    let (first_open_ms, last_close_ms) = if end == 0 {
        (0, 0)
    } else {
        (rows[0].open_ms, rows[end - 1].close_ms)
    };
    if end == 0
        || first_open_ms >= cfg.from_ms + DAY_MS
        || last_close_ms <= cfg.to_ms - DAY_MS
        || from_row >= end
    {
        return Some(Refusal::Coverage {
            first_open_ms,
            last_close_ms,
        });
    }
    let step = meta.chart.qty_step_1e6;
    if step > 0 {
        let mut max_close = 0i64;
        let mut i = from_row;
        while i < end {
            max_close = max_close.max(rows[i].close_1e6);
            i += 1;
        }
        let unit = (i128::from(cfg.notional_1e6) * 1_000_000 / i128::from(max_close.max(1))) as i64;
        if unit < step {
            return Some(Refusal::UnitBelowStep {
                unit_qty_1e6: unit,
                step_1e6: step,
            });
        }
    }
    None
}

/// `run_group` with one trial and [`NoProbe`].
pub fn run_trial(
    t: &Loaded,
    bars: &TrialBars,
    cfg: &ScreenCfg,
    ws: &mut Workspace,
) -> Result<TrialOutcome, ScreenErr> {
    let mut out = [TrialOutcome::default()];
    run_group(&[t], bars, cfg, ws, &mut out, &mut NoProbe)?;
    Ok(out[0])
}
