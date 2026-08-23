//! # backtest — the 8h offline harness substrate (design §3–§5, H1 slice)
//!
//! The REAL `multivenue-engine backtest` subcommand behind the frozen
//! worker contract (`claude-worker/src/claude_worker/backtest.py`):
//! argv `backtest --ruleset R --replay-dir D --split N/M`, schema-1
//! JSON on stdout, human summary on stderr, exit 0 only when a
//! trustworthy report was printed. **The harness conforms to the
//! worker, never vice versa.**
//!
//! What this module does (design §13 items 1 + 2):
//!
//! * capture discovery — one `run-<epoch_ns>` dir or a log root of
//!   `run-*` children, epoch-ordered, dir-name epochs cross-checked
//!   against every PMLR header (§3.1);
//! * deterministic multi-venue merge per run over the total key
//!   `(ts_ns, venue byte, file ordinal, per-file record index)` —
//!   runs are NEVER interleaved (§3.2; door-closer §16.3.1);
//! * VIRT_T0 virtual-clock rebase (§3.3): intra-run deltas exact,
//!   inter-run gaps = wall epoch deltas;
//! * capture-observed universe (sorted, deduped syms) feeding the
//!   REUSED `ingress_ai::validate_ruleset` (§3.5 — no second parser);
//! * the real `strategy_vm::VmStrategy` driven through the real
//!   injection paths: inherent `receive_table` (copy #2 seam) + a
//!   synthesized `RulesetCommit` through `on_ai`, then per merged
//!   record `on_tick` with a [`BacktestCtx`] virtual clock and the
//!   synthesized fills fed back through `on_fill` (§3.6);
//! * the §4 fill / fee / latency model + accounting ([`fill`], H2):
//!   strict-cross maker fills over a preallocated open-order table
//!   (4/sym, 32 total), per-venue Δ activation, maker fees, i64×1e6 /
//!   i128×1e12 fixed-point books, §3.4 order-emit bucketing, OOS
//!   equity/drawdown, end-of-replay mark-out, §4.6 observed bounds;
//! * schema-1 stdout, byte-exact and fixed-point-rendered (§5), with
//!   the REAL `oos` numbers; optional `--emit-detail` sidecar (JSON,
//!   `detail_version` 1 — operator surface, never worker-parsed).
//!
//! ## Doctrine note — this module ALLOCATES
//!
//! Offline tooling under the `audit_replay.rs` doctrine: never loaded
//! by the engine loop, `Vec`/`String`/`Box` are used freely (merged
//! timelines are copied out of the mmap'd capture, ~80 B per tick).
//! Nothing here is reachable from a hot path.

pub mod fill;

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use core_io::{PmlrReader, SlotKind};
use core_types::{
    AiCmd, AiCmdKind, Fill, Order, Price, Qty, RuleTable, Tick, VenueId, AI_SIDE_NONE,
    STRATEGY_SLOT_VM, SYMBOL_ID_NONE,
};
use ingress_ai::{validate_ruleset, RulesetReject};
use strategy_core::{Ctx, Strategy, SubmitErr};
use strategy_vm::VmStrategy;

use crate::backtest::fill::{
    usd_1e12_to_1e6_ceil, usd_1e12_to_1e6_floor, FillEngine, ModelOutcome, SynthFill,
    MAX_OPEN_PER_SYM, MAX_OPEN_TOTAL,
};

/// Book capacity the backtest vm is monomorphized with (design §3.6):
/// the same generic code as the engine's `SET_VM_SLOTS = 512`, sized
/// up because a multi-run capture can carry more syms than one boot.
pub const BACKTEST_VM_SLOTS: usize = 4096;

/// House virtual-clock base (design §3.3; the G3 first-window lesson —
/// `now − 0 ≥ horizon` must hold at the first tick, so the base must
/// exceed the 24 h max horizon by orders of magnitude).
pub const VIRT_T0: u64 = 100_000_000_000_000_000;

/// PMLR header version the merge requires: v1 tick slots carry an
/// undefined venue byte, and the §3.2 total order keys on it.
/// `pub(crate)`: `capture_catalog` mirrors the same acceptance law.
pub(crate) const REQUIRED_PMLR_VERSION: u16 = 2;

/// Per-venue tick-capture file labels, in file-ordinal order (mirrors
/// `audit_replay::VENUE_LABELS` — the cli spawn labels exactly).
/// `pub(crate)`: `capture_catalog` reports in this fixed order.
pub(crate) const VENUE_LABELS: [&str; 6] = ["pm", "bn", "okx", "rpc", "deribit", "hl"];

/// Venue labels accepted by the §4.3/§4.4 model flags, mapped to the
/// wire-stable [`VenueId`] byte. `rpc` is absent by design: it is not
/// a tradeable venue (no `VenueId`, no orders route to it).
const MODEL_VENUE_LABELS: [(&str, VenueId); 5] = [
    ("pm", VenueId::Polymarket),
    ("bn", VenueId::Binance),
    ("okx", VenueId::Okx),
    ("deribit", VenueId::Deribit),
    ("hl", VenueId::Hyperliquid),
];

/// ns per millisecond, for the §4.4 default table.
const MS: u64 = 1_000_000;

// ---------------------------------------------------------------
// Errors
// ---------------------------------------------------------------

/// Why no trustworthy schema-1 report can be printed (design §5 exit
/// mapping — every variant is a nonzero process exit with the reason
/// on stderr and NOTHING on stdout).
#[derive(Debug)]
pub enum HarnessError {
    /// Bad flag values: split grammar, model-flag spec, unreadable
    /// ruleset file.
    Usage(String),
    /// Capture missing, corrupt, cross-check-failed, or empty.
    Capture(String),
    /// The candidate failed the §4.2 validator (identical reject set
    /// as the live side path — no second parser exists to drift).
    Reject(RulesetReject),
    /// An internal invariant broke (e.g. the synthesized commit did
    /// not flip). Indicates a harness bug, never a bad input.
    Internal(String),
}

impl core::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Usage(s) => write!(f, "usage: {s}"),
            Self::Capture(s) => write!(f, "capture: {s}"),
            Self::Reject(r) => write!(f, "ruleset rejected by validator: {r:?}"),
            Self::Internal(s) => write!(f, "internal: {s}"),
        }
    }
}

impl std::error::Error for HarnessError {}

// ---------------------------------------------------------------
// Invocation config (mirrors the bin's clap surface)
// ---------------------------------------------------------------

/// One backtest invocation, as parsed by the bin. The three mandatory
/// fields are the FROZEN worker argv; the rest are operator-side
/// options with design-§4 defaults (declared in H1, consumed by the
/// H2 fill model).
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// Candidate ruleset JSON artifact (§4.1 grammar).
    pub ruleset: PathBuf,
    /// `run-<epoch_ns>` directory or a log root containing `run-*`.
    pub replay_dir: PathBuf,
    /// Split spec, echoed VERBATIM into schema-1 after validation.
    pub split: String,
    /// Repeatable `--fee-bps <venue>:<maker>:<taker>` overrides.
    pub fee_bps: Vec<String>,
    /// Global `--latency-ns` override (applies to all venues).
    pub latency_ns: Option<u64>,
    /// Repeatable `--latency-ns-venue <venue>:<ns>` overrides
    /// (win over the global one).
    pub latency_ns_venue: Vec<String>,
    /// `--emit-detail` sidecar path — declared per §5; written in H2.
    pub emit_detail: Option<PathBuf>,
}

// ---------------------------------------------------------------
// Split (§3.4)
// ---------------------------------------------------------------

/// Validated `N/M` split. `is_pct == 0` only via the carved `0/100`
/// all-OOS monitor form.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Split {
    /// In-sample percent (warm-up window).
    pub is_pct: u32,
    /// Out-of-sample percent.
    pub oos_pct: u32,
}

/// Strict §3.4 parse: `N/M`, ASCII digits only, no leading zeros
/// (except a lone `0`), `N + M == 100`, both ≥ 10 — with exactly one
/// carved degenerate form, `0/100` (the §8.3 walk-forward monitor's
/// all-OOS scoring mode). Everything else is a usage error.
pub fn parse_split(s: &str) -> Result<Split, HarnessError> {
    fn part(p: &str) -> Option<u32> {
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if p.len() > 1 && p.as_bytes()[0] == b'0' {
            return None; // no leading zeros — the echo is verbatim
        }
        p.parse::<u32>().ok()
    }
    let mut it = s.split('/');
    let (a, b) = match (it.next(), it.next(), it.next()) {
        (Some(a), Some(b), None) => (a, b),
        _ => return Err(HarnessError::Usage(format!("bad --split {s:?}: want N/M"))),
    };
    let (n, m) = match (part(a), part(b)) {
        (Some(n), Some(m)) => (n, m),
        _ => return Err(HarnessError::Usage(format!("bad --split {s:?}: want N/M"))),
    };
    let carved_all_oos = n == 0 && m == 100;
    let regular = n >= 10 && m >= 10 && n + m == 100;
    if carved_all_oos || regular {
        Ok(Split { is_pct: n, oos_pct: m })
    } else {
        Err(HarnessError::Usage(format!(
            "bad --split {s:?}: N+M must be 100 with both >= 10 (or the carved 0/100)"
        )))
    }
}

// ---------------------------------------------------------------
// §4.3 / §4.4 model parameters — declared + parsed in H1, CONSUMED
// by the H2 fill model. The hold model ignores them (stated on
// stderr so a stubbed override is never silently "applied").
// ---------------------------------------------------------------

/// Fee + latency-penalty tables, indexed by [`VenueId`] byte (0..=4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ModelParams {
    /// `(maker_bps, taker_bps)` per venue. §4.3 defaults: all 0/0
    /// (Polymarket's current CLOB fee schedule; CEX venues cannot
    /// execute until 8j).
    pub fee_bps: [(u32, u32); 5],
    /// Activation penalty Δ ns per venue (§4.4, deliberately
    /// conservative defaults).
    pub latency_ns: [u64; 5],
}

impl Default for ModelParams {
    fn default() -> Self {
        Self {
            fee_bps: [(0, 0); 5],
            // PM 200 ms, BN/OKX/Deribit 100 ms, HL 600 ms (§4.4).
            latency_ns: [200 * MS, 100 * MS, 100 * MS, 100 * MS, 600 * MS],
        }
    }
}

fn model_venue(label: &str) -> Option<usize> {
    let mut i = 0usize;
    while i < MODEL_VENUE_LABELS.len() {
        if MODEL_VENUE_LABELS[i].0 == label {
            return Some(MODEL_VENUE_LABELS[i].1 as usize);
        }
        i += 1;
    }
    None
}

/// Fold the §4 flag overrides onto the defaults. Precedence: defaults
/// → `--latency-ns` (global) → `--latency-ns-venue` / `--fee-bps`
/// (later occurrences of a repeated flag win).
pub fn parse_model_params(
    fee_specs: &[String],
    latency_global: Option<u64>,
    latency_specs: &[String],
) -> Result<ModelParams, HarnessError> {
    let mut p = ModelParams::default();
    if let Some(ns) = latency_global {
        p.latency_ns = [ns; 5];
    }
    for spec in latency_specs {
        let mut it = spec.split(':');
        let (v, ns) = match (it.next(), it.next(), it.next()) {
            (Some(v), Some(ns), None) => (v, ns),
            _ => {
                return Err(HarnessError::Usage(format!(
                    "bad --latency-ns-venue {spec:?}: want <venue>:<ns>"
                )))
            }
        };
        let vi = model_venue(v).ok_or_else(|| {
            HarnessError::Usage(format!("bad --latency-ns-venue {spec:?}: unknown venue {v:?}"))
        })?;
        let ns: u64 = ns.parse().map_err(|_| {
            HarnessError::Usage(format!("bad --latency-ns-venue {spec:?}: unparseable ns"))
        })?;
        p.latency_ns[vi] = ns;
    }
    for spec in fee_specs {
        let mut it = spec.split(':');
        let (v, mk, tk) = match (it.next(), it.next(), it.next(), it.next()) {
            (Some(v), Some(mk), Some(tk), None) => (v, mk, tk),
            _ => {
                return Err(HarnessError::Usage(format!(
                    "bad --fee-bps {spec:?}: want <venue>:<maker_bps>:<taker_bps>"
                )))
            }
        };
        let vi = model_venue(v).ok_or_else(|| {
            HarnessError::Usage(format!("bad --fee-bps {spec:?}: unknown venue {v:?}"))
        })?;
        let mk: u32 = mk.parse().map_err(|_| {
            HarnessError::Usage(format!("bad --fee-bps {spec:?}: unparseable maker bps"))
        })?;
        let tk: u32 = tk.parse().map_err(|_| {
            HarnessError::Usage(format!("bad --fee-bps {spec:?}: unparseable taker bps"))
        })?;
        p.fee_bps[vi] = (mk, tk);
    }
    Ok(p)
}

// ---------------------------------------------------------------
// Capture discovery (§3.1)
// ---------------------------------------------------------------

/// One discovered capture run. `pub(crate)`: `audit_pnl` inherits the
/// SAME discovery + ordering law (one name law, one epoch order — no
/// drift; the catalog precedent).
#[derive(Debug)]
pub(crate) struct RunDir {
    pub(crate) path: PathBuf,
    pub(crate) epoch_ns: u64,
}

/// Parse `run-<epoch_ns>` (ASCII digits, u64). Anything else is not a
/// capture run directory. `pub(crate)`: the ONE name law, shared with
/// `capture_catalog` (its discovery differs only in treating an empty
/// root as a valid zero-run report).
pub(crate) fn parse_run_dir_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("run-")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// `--replay-dir` resolution: a dir whose own name parses as
/// `run-<ns>` is a single run; otherwise it is a log root whose
/// `run-*` children are the runs, ordered by `epoch_ns` (name order
/// breaks the — physically impossible — epoch tie deterministically).
/// `pub(crate)`: shared with `audit_pnl` (same law, one home).
pub(crate) fn discover_runs(replay_dir: &Path) -> Result<Vec<RunDir>, HarnessError> {
    if !replay_dir.is_dir() {
        return Err(HarnessError::Capture(format!(
            "--replay-dir {} is not a directory",
            replay_dir.display()
        )));
    }
    let own_name = replay_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if let Some(epoch_ns) = parse_run_dir_name(own_name) {
        return Ok(vec![RunDir {
            path: replay_dir.to_path_buf(),
            epoch_ns,
        }]);
    }
    let rd = std::fs::read_dir(replay_dir).map_err(|e| {
        HarnessError::Capture(format!("read_dir {} failed: {e}", replay_dir.display()))
    })?;
    let mut runs: Vec<RunDir> = Vec::new();
    for entry in rd {
        let entry =
            entry.map_err(|e| HarnessError::Capture(format!("read_dir entry failed: {e}")))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };
        if let Some(epoch_ns) = parse_run_dir_name(&name) {
            runs.push(RunDir { path, epoch_ns });
        }
    }
    if runs.is_empty() {
        return Err(HarnessError::Capture(format!(
            "{} contains no run-<epoch_ns> capture directories",
            replay_dir.display()
        )));
    }
    runs.sort_by(|a, b| {
        (a.epoch_ns, a.path.as_os_str()).cmp(&(b.epoch_ns, b.path.as_os_str()))
    });
    Ok(runs)
}

// ---------------------------------------------------------------
// Merge (§3.2) + virtual clock rebase (§3.3)
// ---------------------------------------------------------------

/// Merge-key carrier for one captured tick. The §3.2 total order is
/// `(ts_ns, venue byte, per-file record index)`; `lord` (the fixed
/// file ordinal, [`VENUE_LABELS`] order) sits between venue byte and
/// index purely as a totality backstop — with one tick file per venue
/// per run it never differs from ranking by venue byte, but it keeps
/// the order total even on adversarial fixtures where two files carry
/// equal venue bytes.
#[derive(Copy, Clone, Debug)]
struct MergeKeyed {
    ts_ns: u64,
    venue: u8,
    lord: u8,
    idx: u64,
    tick: Tick,
}

/// Order one run's records by the §3.2 total key. Keys are unique by
/// construction (`(lord, idx)` is injective), so `sort_unstable` is
/// deterministic; the per-file `idx` component IS the stability
/// guarantee for equal `(ts_ns, venue)` records of one file.
fn order_run(recs: &mut [MergeKeyed]) {
    recs.sort_unstable_by_key(|r| (r.ts_ns, r.venue, r.lord, r.idx));
}

/// One record of the merged, rebased timeline. `tick.ts_ns` has been
/// rewritten to the virtual clock; `wall_ns` is the §3.3 wall mapping
/// (`epoch_ns_0 + (virt − VIRT_T0)`), kept for UTC-day reporting.
#[derive(Copy, Clone, Debug)]
struct MergedRec {
    tick: Tick,
    virt_ns: u64,
    wall_ns: u64,
}

/// Per-run load summary for the stderr report.
#[derive(Clone, Debug)]
struct RunSummary {
    epoch_ns: u64,
    venue_records: [u64; 6],
}

/// Open every present per-venue tick file of `run`, cross-check the
/// header, and return the run's §3.2-ordered records.
fn load_run(run: &RunDir) -> Result<(Vec<MergeKeyed>, RunSummary), HarnessError> {
    let mut recs: Vec<MergeKeyed> = Vec::new();
    let mut venue_records = [0u64; 6];
    let mut any_file = false;
    for (lord, label) in VENUE_LABELS.iter().enumerate() {
        let path = run.path.join(format!("{label}-ticks.pmlr"));
        if !path.is_file() {
            continue; // a run captures only spawned venues (§3.1)
        }
        any_file = true;
        let reader = PmlrReader::<Tick>::open(&path).map_err(|e: io::Error| {
            HarnessError::Capture(format!("{}: {e}", path.display()))
        })?;
        if reader.slot_kind() != SlotKind::Tick {
            return Err(HarnessError::Capture(format!(
                "{}: slot_kind {:?} is not Tick",
                path.display(),
                reader.slot_kind()
            )));
        }
        if reader.version() != REQUIRED_PMLR_VERSION {
            return Err(HarnessError::Capture(format!(
                "{}: PMLR v{} tick capture — the merge keys on the venue byte, which v1 \
                 files leave undefined; backtest requires v2",
                path.display(),
                reader.version()
            )));
        }
        if reader.epoch_ns() != run.epoch_ns {
            return Err(HarnessError::Capture(format!(
                "{}: header epoch_ns {} != directory epoch_ns {} (cross-check §3.1)",
                path.display(),
                reader.epoch_ns(),
                run.epoch_ns
            )));
        }
        let records = reader.records();
        venue_records[lord] = records.len() as u64;
        recs.reserve(records.len());
        for (i, t) in records.iter().enumerate() {
            recs.push(MergeKeyed {
                ts_ns: t.ts_ns,
                venue: t.venue,
                lord: lord as u8,
                idx: i as u64,
                tick: *t,
            });
        }
    }
    if !any_file {
        return Err(HarnessError::Capture(format!(
            "{}: no <venue>-ticks.pmlr files present",
            run.path.display()
        )));
    }
    order_run(&mut recs);
    Ok((
        recs,
        RunSummary {
            epoch_ns: run.epoch_ns,
            venue_records,
        },
    ))
}

/// Load every run, order each with the §3.2 key, rebase onto the
/// continuous VIRT_T0 timeline (§3.3), and concatenate in epoch order
/// — runs are never interleaved. Cross-run virtual time must be
/// non-decreasing; a regression means two capture runs overlap in
/// wall time (two engines writing one log root), which no continuous
/// replay can honestly represent — untrustworthy, nonzero exit.
fn load_and_merge(
    runs: &[RunDir],
) -> Result<(Vec<MergedRec>, Vec<RunSummary>), HarnessError> {
    let epoch_0 = runs[0].epoch_ns;
    let mut merged: Vec<MergedRec> = Vec::new();
    let mut summaries: Vec<RunSummary> = Vec::with_capacity(runs.len());
    let mut prev_last_virt: u64 = 0;
    for run in runs {
        let (recs, summary) = load_run(run)?;
        summaries.push(summary);
        if recs.is_empty() {
            continue; // header-only files everywhere: run holds no ticks
        }
        let base = VIRT_T0 + (run.epoch_ns - epoch_0);
        if base < prev_last_virt {
            return Err(HarnessError::Capture(format!(
                "run-{} overlaps the previous run on the virtual timeline \
                 (base {} < previous last {}) — overlapping captures are untrustworthy",
                run.epoch_ns, base, prev_last_virt
            )));
        }
        let ts_first = recs[0].ts_ns; // §3.2 order ⇒ min ts of the run
        merged.reserve(recs.len());
        for r in &recs {
            let virt_ns = base + (r.ts_ns - ts_first);
            let wall_ns = run.epoch_ns + (r.ts_ns - ts_first);
            let mut tick = r.tick;
            tick.ts_ns = virt_ns;
            merged.push(MergedRec {
                tick,
                virt_ns,
                wall_ns,
            });
        }
        prev_last_virt = merged[merged.len() - 1].virt_ns;
    }
    if merged.is_empty() {
        return Err(HarnessError::Capture(
            "merged capture stream is empty — nothing to replay".to_owned(),
        ));
    }
    Ok((merged, summaries))
}

/// Capture-observed universe (§3.5): sorted, deduplicated syms across
/// every merged tick. `SYMBOL_ID_NONE` can never be a real instrument
/// and is skipped defensively.
fn derive_universe(merged: &[MergedRec]) -> Vec<u32> {
    let mut set: BTreeSet<u32> = BTreeSet::new();
    for r in merged {
        if r.tick.sym != SYMBOL_ID_NONE {
            set.insert(r.tick.sym);
        }
    }
    set.into_iter().collect()
}

// ---------------------------------------------------------------
// BacktestCtx (§3.6) — virtual clock + captured order log
// ---------------------------------------------------------------

/// Preallocated order-log capacity. The log grows past this only on
/// extreme fixtures (offline path — growth is allowed, just not the
/// steady state).
const ORDER_LOG_CAPACITY: usize = 1 << 16;

/// The harness-side [`Ctx`]: `submit` captures emitted orders into an
/// order log (the H2 fill engine's intake; hold-only this session) and
/// `now_ns` returns the current record's virtual timestamp. Accepts
/// orders for ANY venue byte (door-closer §16.3.6).
pub struct BacktestCtx {
    now_ns: u64,
    orders: Vec<Order>,
    /// Running max of emitted-order notional (×1e6 USD) — §4.6
    /// `bounds.max_order_notional_usd`, full window by design.
    max_order_notional_1e6: i64,
}

impl BacktestCtx {
    /// Fresh context at the virtual epoch.
    pub fn new() -> Self {
        Self {
            now_ns: VIRT_T0,
            orders: Vec::with_capacity(ORDER_LOG_CAPACITY),
            max_order_notional_1e6: 0,
        }
    }

    /// Orders captured so far.
    pub fn orders(&self) -> &[Order] {
        &self.orders
    }

    /// Observed max emitted-order notional, ×1e6 USD.
    pub fn max_order_notional_1e6(&self) -> i64 {
        self.max_order_notional_1e6
    }
}

impl Default for BacktestCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl Ctx for BacktestCtx {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        // notional = px × qty / 1e6, exact in i128, floored like the
        // vm's own sizing arithmetic.
        let notional_1e6 =
            ((order.px.raw() as i128 * order.qty.raw() as i128) / 1_000_000) as i64;
        if notional_1e6 > self.max_order_notional_1e6 {
            self.max_order_notional_1e6 = notional_1e6;
        }
        self.orders.push(order);
        Ok(())
    }

    fn now_ns(&self) -> u64 {
        self.now_ns
    }
}

// ---------------------------------------------------------------
// Fixed-point rendering (§5)
// ---------------------------------------------------------------

/// Render an i64 ×1e6 USD value as a JSON number with NO float
/// round-trip: sign, integer part, fractional digits with trailing
/// zeros trimmed — always at least one fractional digit, so whole
/// dollars render `"X.0"` (bit-identical reruns; `_strict_float`
/// accepts any JSON number).
pub fn fmt_usd_1e6(v_1e6: i64) -> String {
    let sign = if v_1e6 < 0 { "-" } else { "" };
    let a = v_1e6.unsigned_abs();
    let int = a / 1_000_000;
    let mut frac = a % 1_000_000;
    if frac == 0 {
        return format!("{sign}{int}.0");
    }
    let mut width = 6usize;
    while frac % 10 == 0 {
        frac /= 10;
        width -= 1;
    }
    format!("{sign}{int}.{frac:0width$}")
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0F) as usize] as char);
    }
    s
}

/// Exact schema-1 line (§5): the frozen field set, nothing more. The
/// two counts are JSON integer literals (`_strict_int` rejects bools
/// and floats); `split` was digit/slash-validated before it is
/// embedded, so the string is JSON-safe verbatim.
fn render_schema1(hash_hex: &str, split: &str, vals: &ReportValues) -> String {
    format!(
        concat!(
            "{{\"schema_version\":1,",
            "\"ruleset_hash\":\"{hash}\",",
            "\"split\":\"{split}\",",
            "\"oos\":{{",
            "\"net_pnl_usd\":{pnl},",
            "\"trades\":{trades},",
            "\"trading_days\":{days},",
            "\"max_drawdown_usd\":{dd}}},",
            "\"bounds\":{{",
            "\"max_order_notional_usd\":{mo},",
            "\"max_symbol_notional_usd\":{ms},",
            "\"max_total_notional_usd\":{mt}}}}}"
        ),
        hash = hash_hex,
        split = split,
        pnl = fmt_usd_1e6(vals.oos_net_pnl_1e6),
        trades = vals.oos_trades,
        days = vals.oos_trading_days,
        dd = fmt_usd_1e6(vals.oos_max_drawdown_1e6),
        mo = fmt_usd_1e6(vals.max_order_notional_1e6),
        ms = fmt_usd_1e6(vals.max_symbol_notional_1e6),
        mt = fmt_usd_1e6(vals.max_total_notional_1e6),
    )
}

/// The schema-1 value set, rendered from the §4 model (×1e6 fixed
/// point, conversion direction per [`fill`] module docs: net floors,
/// drawdown/position bounds ceil, max-order keeps the H1 emit-time
/// floor). Was `HoldReport` in H1 — same field set, real numbers.
#[derive(Copy, Clone, Debug)]
struct ReportValues {
    oos_net_pnl_1e6: i64,
    oos_trades: u64,
    oos_trading_days: u64,
    oos_max_drawdown_1e6: i64,
    max_order_notional_1e6: i64,
    max_symbol_notional_1e6: i64,
    max_total_notional_1e6: i64,
}

// ---------------------------------------------------------------
// Result surface
// ---------------------------------------------------------------

/// Deterministic numeric facts of one harness run — the stderr
/// summary renders from these, and tests assert on them directly.
#[derive(Copy, Clone, Debug)]
pub struct HarnessStats {
    /// Discovered capture runs.
    pub runs: u64,
    /// Records on the merged timeline.
    pub merged_records: u64,
    /// Capture-observed universe size.
    pub universe_syms: u64,
    /// First / last virtual timestamps of the merged timeline.
    pub first_virt_ns: u64,
    /// Last virtual timestamp.
    pub last_virt_ns: u64,
    /// §3.4 wall-time split boundary on the virtual clock.
    pub boundary_virt_ns: u64,
    /// Merged records with `virt ts >= boundary` (the OOS window).
    pub oos_records: u64,
    /// Distinct UTC days spanned by the capture (stderr surface ONLY —
    /// schema-1 `trading_days` is §4.5 OOS-trade days, which the hold
    /// model correctly reports as 0).
    pub capture_utc_days: u64,
    /// vm rows-evaluated counter after replay.
    pub vm_evals: u64,
    /// vm trigger-fired counter (pre-clamp).
    pub vm_fires: u64,
    /// Orders the vm emitted into the [`BacktestCtx`] log.
    pub vm_orders_emitted: u64,
    /// vm-side dispatcher rejects (structurally 0 in H1 — the ctx
    /// never refuses; the §4.1 open-order caps arrive with H2).
    pub vm_orders_dropped: u64,
    /// Referenced syms that could not claim one of the
    /// [`BACKTEST_VM_SLOTS`] book slots (fail-closed, counted).
    pub vm_book_track_failed: u64,
    /// Observed max emitted-order notional ×1e6 (§4.6).
    pub max_order_notional_1e6: i64,
    /// Synthesized fills, IS + OOS (§4.2).
    pub fills_total: u64,
    /// Fills of OOS-emitted orders (`oos.trades`, §3.4/§4.5).
    pub fills_oos: u64,
    /// Orders accepted into the open-order table before the boundary.
    pub orders_is: u64,
    /// Orders accepted into the table at/after the boundary.
    pub orders_oos: u64,
    /// §4.1 per-sym-cap drops (counted, never surfaced to the vm).
    pub orders_rejected_sym_cap: u64,
    /// §4.1 total-cap drops.
    pub orders_rejected_total_cap: u64,
    /// Orders on a venue byte with no Δ/fee row (cannot execute).
    pub orders_unroutable: u64,
    /// Orders still resting at replay end (canceled, zero P&L).
    pub orders_canceled_end: u64,
    /// Peak simultaneous open orders (total / one sym).
    pub peak_open_total: u64,
    /// Peak simultaneous open orders on one sym.
    pub peak_open_per_sym: u64,
    /// OOS net P&L ×1e6 (floored from ×1e12 — [`fill`] direction).
    pub oos_net_pnl_1e6: i64,
    /// OOS realized component ×1e6 (stderr surface; floor).
    pub oos_realized_1e6: i64,
    /// OOS fees ×1e6 (stderr surface; ceil — risk direction).
    pub oos_fees_1e6: i64,
    /// OOS mark-out (unrealized at last mids) ×1e6 (floor).
    pub oos_unreal_1e6: i64,
    /// OOS max peak-to-trough drawdown ×1e6 (ceil).
    pub oos_max_drawdown_1e6: i64,
    /// Distinct UTC days with ≥ 1 OOS fill.
    pub oos_trading_days: u64,
    /// §4.6 peak per-sym |position|×mark ×1e6 (ceil), full window.
    pub max_symbol_notional_1e6: i64,
    /// §4.6 peak Σ|position|×mark ×1e6 (ceil), full window.
    pub max_total_notional_1e6: i64,
}

/// What `run` hands the bin: the exact stdout line, the stderr
/// summary, and the numeric facts both were rendered from.
#[derive(Clone, Debug)]
pub struct BacktestOutput {
    /// Schema-1 JSON, one line, no trailing newline.
    pub schema1: String,
    /// Human summary for stderr (multi-line, deterministic).
    pub summary: String,
    /// The facts behind both strings.
    pub stats: HarnessStats,
}

// ---------------------------------------------------------------
// The harness
// ---------------------------------------------------------------

/// Run one backtest end to end (§3 substrate + §5 report; hold-model
/// accounting until H2). On `Err` the caller must print the reason to
/// stderr and exit nonzero WITHOUT touching stdout — the worker treats
/// any nonzero exit as "no trustworthy report exists".
pub fn run(cfg: &BacktestConfig) -> Result<BacktestOutput, HarnessError> {
    let split = parse_split(&cfg.split)?;
    let model = parse_model_params(&cfg.fee_bps, cfg.latency_ns, &cfg.latency_ns_venue)?;

    // Candidate bytes + identity (§3.5): full SHA-256 is schema-1's
    // `ruleset_hash`; its first 16 bytes are the wire hash128.
    let ruleset_bytes = std::fs::read(&cfg.ruleset).map_err(|e| {
        HarnessError::Usage(format!("cannot read --ruleset {}: {e}", cfg.ruleset.display()))
    })?;
    let full_hash = core_crypto::sha256(&ruleset_bytes);
    let mut hash128 = [0u8; 16];
    hash128.copy_from_slice(&full_hash[..16]);
    let hash_hex = hex_lower(&full_hash);

    // Capture discovery + merge + rebase (§3.1–§3.3).
    let runs = discover_runs(&cfg.replay_dir)?;
    let (merged, run_summaries) = load_and_merge(&runs)?;
    let universe = derive_universe(&merged);

    // The REUSED validator (§3.5) — same byte scanner, same reject
    // set as the live side path; the capture-observed universe stands
    // in for the boot universe.
    let mut table = Box::new(RuleTable::EMPTY);
    validate_ruleset(&ruleset_bytes, &hash128, &universe, &mut table)
        .map_err(HarnessError::Reject)?;

    // §3.4 boundary: N% of the merged wall-time span. Replay is
    // continuous through it; only accounting buckets on it (H2 —
    // interpretation pinned in the H1 log: OOS = [boundary, end],
    // boundary inclusive).
    let first_virt = merged[0].virt_ns;
    let last_virt = merged[merged.len() - 1].virt_ns;
    let span = last_virt - first_virt;
    let boundary_virt =
        first_virt + ((span as u128 * split.is_pct as u128) / 100) as u64;
    let mut oos_records = 0u64;
    for r in &merged {
        if r.virt_ns >= boundary_virt {
            oos_records += 1;
        }
    }
    if oos_records == 0 {
        // Structurally unreachable while OOS is [boundary, last] and
        // the merge is non-empty; kept as a §5 tripwire for H2's
        // bucketing changes.
        return Err(HarnessError::Capture(
            "OOS window contains zero ticks".to_owned(),
        ));
    }

    // Distinct UTC days spanned by the capture (stderr only, §4.5).
    let mut days: BTreeSet<u64> = BTreeSet::new();
    for r in &merged {
        days.insert(r.wall_ns / 86_400_000_000_000);
    }

    // ---- Evaluator drive (§3.6): the REAL vm, the REAL paths ----
    let mut vm: Box<VmStrategy<BACKTEST_VM_SLOTS>> = Box::new(VmStrategy::new());
    let mut ctx = BacktestCtx::new();
    vm.on_start(&mut ctx)
        .map_err(|e| HarnessError::Internal(format!("vm on_start failed: {e}")))?;
    // 1. Inherent receive_table — the copy-#2 seam the StrategySet
    //    forwarder would otherwise drive (the trait hook is a no-op on
    //    a bare vm by design).
    vm.receive_table(&table);
    // 2. The real hash-checked flip: a synthesized RulesetCommit with
    //    the hash128 riding px/qty as LE i64 halves (worker wire
    //    convention, `AiCmd::ruleset_hash128` reassembles it).
    let px = i64::from_le_bytes(hash128[0..8].try_into().expect("8-byte slice"));
    let qty = i64::from_le_bytes(hash128[8..16].try_into().expect("8-byte slice"));
    let commit = AiCmd::new(
        VIRT_T0,
        1,
        SYMBOL_ID_NONE,
        px,
        qty,
        0,
        AiCmdKind::RulesetCommit,
        VenueId::Ai,
        STRATEGY_SLOT_VM,
        AI_SIDE_NONE,
        0,
        0,
    );
    debug_assert!(commit.validate_shape().is_ok(), "synthesized commit shape");
    vm.on_ai(&commit, &mut ctx);
    if vm.commits_applied != 1 {
        return Err(HarnessError::Internal(
            "synthesized RulesetCommit did not flip the staged table".to_owned(),
        ));
    }
    // 3. The merged, rebased timeline through the real on_tick, with
    //    the §4 model in the loop: per record — (a) marks + strict-
    //    cross fill pass ([`fill::FillEngine::on_record`]), (b) the
    //    vm's on_tick, (c) the record's synthesized fills through the
    //    REAL `on_fill` (§3.6.4 — the design order: tick, then fills,
    //    matching the engine's pump; the vm ignores them today).
    //    Every vm callback's newly emitted orders drain into the
    //    model's §4.1 intake with `t_active = now + Δ_venue`; an order
    //    therefore can never fill on its own emitting tick.
    let mut engine = FillEngine::new(model, boundary_virt);
    let mut consumed = ctx.orders().len(); // on_start/on_ai emit nothing, but stay uniform
    debug_assert_eq!(consumed, 0);
    let mut fills_scratch: Vec<SynthFill> = Vec::with_capacity(MAX_OPEN_TOTAL);
    for rec in &merged {
        ctx.now_ns = rec.virt_ns;
        engine.on_record(&rec.tick, rec.virt_ns, rec.wall_ns, &mut fills_scratch);
        vm.on_tick(&rec.tick, &mut ctx);
        while consumed < ctx.orders().len() {
            let order = ctx.orders()[consumed];
            engine.intake(&order, rec.virt_ns);
            consumed += 1;
        }
        for f in &fills_scratch {
            let vm_fill = Fill::new(
                rec.virt_ns,
                f.sym,
                f.side,
                Price::from_raw(f.px_1e6),
                Qty::from_raw(f.qty_1e6),
                f.client_oid,
            );
            vm.on_fill(&vm_fill, &mut ctx);
            while consumed < ctx.orders().len() {
                let order = ctx.orders()[consumed];
                engine.intake(&order, rec.virt_ns);
                consumed += 1;
            }
        }
    }
    let outcome: ModelOutcome = engine.finish();

    // ---- §5 report from the §4 model (fixed-point renders only) ----
    let vals = ReportValues {
        oos_net_pnl_1e6: usd_1e12_to_1e6_floor(outcome.oos_net_1e12),
        oos_trades: outcome.oos_trades,
        oos_trading_days: outcome.oos_trading_days,
        oos_max_drawdown_1e6: usd_1e12_to_1e6_ceil(outcome.oos_max_dd_1e12),
        max_order_notional_1e6: ctx.max_order_notional_1e6(),
        max_symbol_notional_1e6: usd_1e12_to_1e6_ceil(outcome.max_symbol_notional_1e12),
        max_total_notional_1e6: usd_1e12_to_1e6_ceil(outcome.max_total_notional_1e12),
    };
    let stats = HarnessStats {
        runs: runs.len() as u64,
        merged_records: merged.len() as u64,
        universe_syms: universe.len() as u64,
        first_virt_ns: first_virt,
        last_virt_ns: last_virt,
        boundary_virt_ns: boundary_virt,
        oos_records,
        capture_utc_days: days.len() as u64,
        vm_evals: vm.evals,
        vm_fires: vm.fires,
        vm_orders_emitted: vm.orders_emitted,
        vm_orders_dropped: vm.orders_dropped,
        vm_book_track_failed: vm.book_track_failed,
        max_order_notional_1e6: ctx.max_order_notional_1e6(),
        fills_total: outcome.fills_total,
        fills_oos: outcome.oos_trades,
        orders_is: outcome.orders_is,
        orders_oos: outcome.orders_oos,
        orders_rejected_sym_cap: outcome.rejected_sym_cap,
        orders_rejected_total_cap: outcome.rejected_total_cap,
        orders_unroutable: outcome.unroutable,
        orders_canceled_end: outcome.canceled_end,
        peak_open_total: outcome.peak_open_total,
        peak_open_per_sym: outcome.peak_open_per_sym,
        oos_net_pnl_1e6: vals.oos_net_pnl_1e6,
        oos_realized_1e6: usd_1e12_to_1e6_floor(outcome.oos_realized_1e12),
        oos_fees_1e6: usd_1e12_to_1e6_ceil(outcome.oos_fees_1e12),
        oos_unreal_1e6: usd_1e12_to_1e6_floor(outcome.oos_unreal_1e12),
        oos_max_drawdown_1e6: vals.oos_max_drawdown_1e6,
        oos_trading_days: outcome.oos_trading_days,
        max_symbol_notional_1e6: vals.max_symbol_notional_1e6,
        max_total_notional_1e6: vals.max_total_notional_1e6,
    };
    debug_assert_eq!(stats.vm_orders_emitted as usize, ctx.orders().len());
    debug_assert_eq!(
        outcome.orders_is
            + outcome.orders_oos
            + outcome.rejected_sym_cap
            + outcome.rejected_total_cap
            + outcome.unroutable,
        stats.vm_orders_emitted,
        "every vm-emitted order is accounted: accepted, cap-dropped or unroutable"
    );

    // §5: the optional operator sidecar — written BEFORE stdout so a
    // failed write yields a nonzero exit with nothing on stdout.
    if let Some(detail_path) = &cfg.emit_detail {
        let detail = render_detail(&hash_hex, &cfg.split, &model, &stats, &outcome, &engine);
        std::fs::write(detail_path, detail).map_err(|e| {
            HarnessError::Usage(format!(
                "cannot write --emit-detail {}: {e}",
                detail_path.display()
            ))
        })?;
    }

    let schema1 = render_schema1(&hash_hex, &cfg.split, &vals);
    let summary = render_summary(cfg, &split, &model, &run_summaries, &stats, &hash_hex);
    Ok(BacktestOutput {
        schema1,
        summary,
        stats,
    })
}

/// Deterministic human summary (stderr; §10 harness observability).
fn render_summary(
    cfg: &BacktestConfig,
    split: &Split,
    model: &ModelParams,
    runs: &[RunSummary],
    stats: &HarnessStats,
    hash_hex: &str,
) -> String {
    let mut s = String::with_capacity(2048);
    s.push_str(&format!(
        "backtest H2 (strict-cross maker model): ruleset sha256 {hash_hex}\n"
    ));
    s.push_str(&format!(
        "capture: {} run(s), {} merged tick(s), universe {} sym(s), {} UTC day(s) spanned\n",
        stats.runs, stats.merged_records, stats.universe_syms, stats.capture_utc_days
    ));
    for (i, r) in runs.iter().enumerate() {
        s.push_str(&format!("  run[{i}] epoch_ns={}", r.epoch_ns));
        for (lord, label) in VENUE_LABELS.iter().enumerate() {
            s.push_str(&format!(" {label}={}", r.venue_records[lord]));
        }
        s.push('\n');
    }
    s.push_str(&format!(
        "window: virt [{}, {}], split {}/{} boundary {} ({} OOS tick(s))\n",
        stats.first_virt_ns,
        stats.last_virt_ns,
        split.is_pct,
        split.oos_pct,
        stats.boundary_virt_ns,
        stats.oos_records
    ));
    s.push_str(&format!(
        "vm: evals={} fires={} orders_emitted={} orders_dropped={} book_track_failed={}\n",
        stats.vm_evals,
        stats.vm_fires,
        stats.vm_orders_emitted,
        stats.vm_orders_dropped,
        stats.vm_book_track_failed
    ));
    s.push_str(&format!(
        "model: latency_ns pm={} bn={} okx={} deribit={} hl={}; fee_bps pm={}:{} bn={}:{} \
         okx={}:{} deribit={}:{} hl={}:{}; open-order caps {}/sym {} total\n",
        model.latency_ns[VenueId::Polymarket as usize],
        model.latency_ns[VenueId::Binance as usize],
        model.latency_ns[VenueId::Okx as usize],
        model.latency_ns[VenueId::Deribit as usize],
        model.latency_ns[VenueId::Hyperliquid as usize],
        model.fee_bps[VenueId::Polymarket as usize].0,
        model.fee_bps[VenueId::Polymarket as usize].1,
        model.fee_bps[VenueId::Binance as usize].0,
        model.fee_bps[VenueId::Binance as usize].1,
        model.fee_bps[VenueId::Okx as usize].0,
        model.fee_bps[VenueId::Okx as usize].1,
        model.fee_bps[VenueId::Deribit as usize].0,
        model.fee_bps[VenueId::Deribit as usize].1,
        model.fee_bps[VenueId::Hyperliquid as usize].0,
        model.fee_bps[VenueId::Hyperliquid as usize].1,
        MAX_OPEN_PER_SYM,
        MAX_OPEN_TOTAL,
    ));
    s.push_str(&format!(
        "orders: accepted is={} oos={}, rejected_caps={}+{} (sym+total), unroutable={}, \
         canceled_end={}, peak_open={} (per-sym {})\n",
        stats.orders_is,
        stats.orders_oos,
        stats.orders_rejected_sym_cap,
        stats.orders_rejected_total_cap,
        stats.orders_unroutable,
        stats.orders_canceled_end,
        stats.peak_open_total,
        stats.peak_open_per_sym
    ));
    s.push_str(&format!(
        "fills: total={} oos={}\n",
        stats.fills_total, stats.fills_oos
    ));
    s.push_str(&format!(
        "oos: net_pnl={} (realized={} fees={} markout={}), max_drawdown={}, trades={}, \
         trading_days={}\n",
        fmt_usd_1e6(stats.oos_net_pnl_1e6),
        fmt_usd_1e6(stats.oos_realized_1e6),
        fmt_usd_1e6(stats.oos_fees_1e6),
        fmt_usd_1e6(stats.oos_unreal_1e6),
        fmt_usd_1e6(stats.oos_max_drawdown_1e6),
        stats.fills_oos,
        stats.oos_trading_days
    ));
    s.push_str(&format!(
        "bounds (full window): max_order_notional={} max_symbol_notional={} \
         max_total_notional={}\n",
        fmt_usd_1e6(stats.max_order_notional_1e6),
        fmt_usd_1e6(stats.max_symbol_notional_1e6),
        fmt_usd_1e6(stats.max_total_notional_1e6)
    ));
    s.push_str(
        "reproducibility: zero RNG anywhere (determinism by construction); strict-cross \
         maker (trade-through only — no touch fills, no queue credit); fills/marks on \
         two-sided ticks only; unfilled remainders canceled at end (zero P&L); open \
         positions marked out at last mid into net_pnl\n",
    );
    if let Some(p) = &cfg.emit_detail {
        s.push_str(&format!("--emit-detail: sidecar written to {}\n", p.display()));
    }
    s
}

/// The `--emit-detail` sidecar (§5): versioned SEPARATELY from
/// schema-1 (`detail_version` 1), operator/session surface, never
/// parsed by the worker. Hand-rendered like schema-1 — every value is
/// numeric, a fixed label, the validated split echo, or the hash hex;
/// USD values are the same fixed-point renders as the stderr summary.
fn render_detail(
    hash_hex: &str,
    split: &str,
    model: &ModelParams,
    stats: &HarnessStats,
    outcome: &ModelOutcome,
    engine: &FillEngine,
) -> String {
    let mut s = String::with_capacity(4096);
    s.push_str(&format!(
        concat!(
            "{{\"detail_version\":1,",
            "\"ruleset_hash\":\"{hash}\",",
            "\"split\":\"{split}\",",
            "\"model\":{{",
            "\"latency_ns\":{{\"pm\":{lpm},\"bn\":{lbn},\"okx\":{lokx},\"deribit\":{lde},\"hl\":{lhl}}},",
            "\"fee_bps\":{{\"pm\":[{fpm0},{fpm1}],\"bn\":[{fbn0},{fbn1}],\"okx\":[{fokx0},{fokx1}],",
            "\"deribit\":[{fde0},{fde1}],\"hl\":[{fhl0},{fhl1}]}},",
            "\"open_order_caps\":[{cap_sym},{cap_tot}]}},",
            "\"window\":{{\"first_virt_ns\":{fv},\"last_virt_ns\":{lv},\"boundary_virt_ns\":{bv},",
            "\"merged_records\":{mr},\"oos_records\":{or}}},",
            "\"orders\":{{\"emitted\":{oe},\"accepted_is\":{ois},\"accepted_oos\":{ooos},",
            "\"rejected_sym_cap\":{rsc},\"rejected_total_cap\":{rtc},\"unroutable\":{unr},",
            "\"canceled_end\":{cend},\"peak_open_total\":{pot},\"peak_open_per_sym\":{pos}}},",
            "\"fills\":{{\"total\":{ft},\"oos\":{fo}}},",
            "\"oos\":{{\"net_pnl_usd\":{onet},\"realized_usd\":{orl},\"fees_usd\":{ofe},",
            "\"markout_usd\":{oun},\"max_drawdown_usd\":{odd},\"trades\":{otr},",
            "\"trading_days\":{oda}}},",
            "\"full\":{{\"realized_usd\":{frl},\"fees_usd\":{ffe},\"unrealized_usd\":{fun}}},",
            "\"bounds\":{{\"max_order_notional_usd\":{bmo},\"max_symbol_notional_usd\":{bms},",
            "\"max_total_notional_usd\":{bmt}}},",
            "\"per_sym\":["
        ),
        hash = hash_hex,
        split = split,
        lpm = model.latency_ns[VenueId::Polymarket as usize],
        lbn = model.latency_ns[VenueId::Binance as usize],
        lokx = model.latency_ns[VenueId::Okx as usize],
        lde = model.latency_ns[VenueId::Deribit as usize],
        lhl = model.latency_ns[VenueId::Hyperliquid as usize],
        fpm0 = model.fee_bps[VenueId::Polymarket as usize].0,
        fpm1 = model.fee_bps[VenueId::Polymarket as usize].1,
        fbn0 = model.fee_bps[VenueId::Binance as usize].0,
        fbn1 = model.fee_bps[VenueId::Binance as usize].1,
        fokx0 = model.fee_bps[VenueId::Okx as usize].0,
        fokx1 = model.fee_bps[VenueId::Okx as usize].1,
        fde0 = model.fee_bps[VenueId::Deribit as usize].0,
        fde1 = model.fee_bps[VenueId::Deribit as usize].1,
        fhl0 = model.fee_bps[VenueId::Hyperliquid as usize].0,
        fhl1 = model.fee_bps[VenueId::Hyperliquid as usize].1,
        cap_sym = MAX_OPEN_PER_SYM,
        cap_tot = MAX_OPEN_TOTAL,
        fv = stats.first_virt_ns,
        lv = stats.last_virt_ns,
        bv = stats.boundary_virt_ns,
        mr = stats.merged_records,
        or = stats.oos_records,
        oe = stats.vm_orders_emitted,
        ois = stats.orders_is,
        ooos = stats.orders_oos,
        rsc = stats.orders_rejected_sym_cap,
        rtc = stats.orders_rejected_total_cap,
        unr = stats.orders_unroutable,
        cend = stats.orders_canceled_end,
        pot = stats.peak_open_total,
        pos = stats.peak_open_per_sym,
        ft = stats.fills_total,
        fo = stats.fills_oos,
        onet = fmt_usd_1e6(stats.oos_net_pnl_1e6),
        orl = fmt_usd_1e6(stats.oos_realized_1e6),
        ofe = fmt_usd_1e6(stats.oos_fees_1e6),
        oun = fmt_usd_1e6(stats.oos_unreal_1e6),
        odd = fmt_usd_1e6(stats.oos_max_drawdown_1e6),
        otr = stats.fills_oos,
        oda = stats.oos_trading_days,
        frl = fmt_usd_1e6(usd_1e12_to_1e6_floor(outcome.full_realized_1e12)),
        ffe = fmt_usd_1e6(usd_1e12_to_1e6_ceil(outcome.full_fees_1e12)),
        fun = fmt_usd_1e6(usd_1e12_to_1e6_floor(outcome.full_unreal_1e12)),
        bmo = fmt_usd_1e6(stats.max_order_notional_1e6),
        bms = fmt_usd_1e6(stats.max_symbol_notional_1e6),
        bmt = fmt_usd_1e6(stats.max_total_notional_1e6),
    ));
    for (i, row) in engine.per_sym_detail().iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            concat!(
                "{{\"sym\":{sym},\"venue\":{venue},\"pos_qty\":{q},\"last_mid\":{m},",
                "\"realized_usd\":{r},\"fees_usd\":{f},\"fills\":{n}}}"
            ),
            sym = row.sym,
            venue = row.venue,
            q = fmt_usd_1e6(row.pos_qty_1e6),
            m = fmt_usd_1e6(row.last_mid_1e6),
            r = fmt_usd_1e6(usd_1e12_to_1e6_floor(row.realized_1e12)),
            f = fmt_usd_1e6(usd_1e12_to_1e6_ceil(row.fees_1e12)),
            n = row.fills,
        ));
    }
    s.push_str("]}\n");
    s
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty};

    // ---------------- split (§3.4) ----------------

    #[test]
    fn split_accepts_the_contract_form() {
        assert_eq!(
            parse_split("70/30").unwrap(),
            Split {
                is_pct: 70,
                oos_pct: 30
            }
        );
        assert_eq!(
            parse_split("50/50").unwrap(),
            Split {
                is_pct: 50,
                oos_pct: 50
            }
        );
        assert_eq!(
            parse_split("10/90").unwrap(),
            Split {
                is_pct: 10,
                oos_pct: 90
            }
        );
    }

    #[test]
    fn split_accepts_the_carved_all_oos_form() {
        assert_eq!(
            parse_split("0/100").unwrap(),
            Split {
                is_pct: 0,
                oos_pct: 100
            }
        );
    }

    #[test]
    fn split_rejects_everything_else() {
        for bad in [
            "5/95", "95/5", "70/40", "100/0", "0/0", "70/30/0", "70", "", "/", "70/",
            "/30", "a/b", " 70/30", "70/30 ", "070/30", "70/030", "+70/30", "-70/170",
            "1000/0",
        ] {
            assert!(
                matches!(parse_split(bad), Err(HarnessError::Usage(_))),
                "{bad:?} must be a usage error"
            );
        }
    }

    // ---------------- model flags (§4.3/§4.4) ----------------

    #[test]
    fn model_params_defaults_pin_design_4() {
        let p = ModelParams::default();
        assert_eq!(p.fee_bps, [(0, 0); 5]);
        assert_eq!(
            p.latency_ns,
            [200 * MS, 100 * MS, 100 * MS, 100 * MS, 600 * MS]
        );
    }

    #[test]
    fn model_params_overrides_layer_correctly() {
        let p = parse_model_params(
            &["pm:0:10".to_owned(), "hl:3:4".to_owned()],
            Some(1_000),
            &["deribit:42".to_owned()],
        )
        .unwrap();
        // global latency replaced all five, then deribit won on top.
        assert_eq!(p.latency_ns, [1_000, 1_000, 1_000, 42, 1_000]);
        assert_eq!(p.fee_bps[VenueId::Polymarket as usize], (0, 10));
        assert_eq!(p.fee_bps[VenueId::Hyperliquid as usize], (3, 4));
        assert_eq!(p.fee_bps[VenueId::Binance as usize], (0, 0));
    }

    #[test]
    fn model_params_rejects_malformed_specs() {
        for bad_fee in ["pm:1", "pm:1:2:3", "rpc:1:2", "nope:1:2", "pm:x:2", "pm:1:y"] {
            assert!(
                matches!(
                    parse_model_params(&[bad_fee.to_owned()], None, &[]),
                    Err(HarnessError::Usage(_))
                ),
                "fee spec {bad_fee:?} must be a usage error"
            );
        }
        for bad_lat in ["pm", "pm:1:2", "rpc:5", "nope:5", "pm:x"] {
            assert!(
                matches!(
                    parse_model_params(&[], None, &[bad_lat.to_owned()]),
                    Err(HarnessError::Usage(_))
                ),
                "latency spec {bad_lat:?} must be a usage error"
            );
        }
    }

    // ---------------- run-dir names (§3.1) ----------------

    #[test]
    fn run_dir_name_parse_happy_and_sad() {
        assert_eq!(parse_run_dir_name("run-0"), Some(0));
        assert_eq!(
            parse_run_dir_name("run-1755838000123456789"),
            Some(1_755_838_000_123_456_789)
        );
        for bad in ["run-", "run", "run-abc", "run-12x", "xrun-12", "run--12"] {
            assert_eq!(parse_run_dir_name(bad), None, "{bad:?}");
        }
    }

    // ---------------- fixed-point rendering (§5) ----------------

    #[test]
    fn fmt_usd_renders_deterministic_decimals() {
        assert_eq!(fmt_usd_1e6(0), "0.0");
        assert_eq!(fmt_usd_1e6(50_000_000), "50.0");
        assert_eq!(fmt_usd_1e6(-3_250_000), "-3.25");
        assert_eq!(fmt_usd_1e6(12_500_000), "12.5");
        assert_eq!(fmt_usd_1e6(9_750_000), "9.75");
        assert_eq!(fmt_usd_1e6(-250_000), "-0.25");
        assert_eq!(fmt_usd_1e6(1), "0.000001");
        assert_eq!(fmt_usd_1e6(-1), "-0.000001");
        assert_eq!(fmt_usd_1e6(1_000_001), "1.000001");
        assert_eq!(fmt_usd_1e6(i64::MAX), "9223372036854.775807");
    }

    #[test]
    fn schema1_line_is_the_frozen_field_set() {
        let vals = ReportValues {
            oos_net_pnl_1e6: 0,
            oos_trades: 0,
            oos_trading_days: 0,
            oos_max_drawdown_1e6: 0,
            max_order_notional_1e6: 50_000_000,
            max_symbol_notional_1e6: 0,
            max_total_notional_1e6: 0,
        };
        let line = render_schema1("ab12", "70/30", &vals);
        assert_eq!(
            line,
            "{\"schema_version\":1,\"ruleset_hash\":\"ab12\",\"split\":\"70/30\",\
             \"oos\":{\"net_pnl_usd\":0.0,\"trades\":0,\"trading_days\":0,\
             \"max_drawdown_usd\":0.0},\"bounds\":{\"max_order_notional_usd\":50.0,\
             \"max_symbol_notional_usd\":0.0,\"max_total_notional_usd\":0.0}}"
        );
    }

    // ---------------- merge ordering (§3.2) ----------------

    fn keyed(ts: u64, venue: u8, lord: u8, idx: u64) -> MergeKeyed {
        MergeKeyed {
            ts_ns: ts,
            venue,
            lord,
            idx,
            tick: Tick::new(
                ts,
                VenueId::Polymarket,
                1,
                idx as u32,
                Price::from_raw(1),
                Qty::from_raw(1),
                Price::from_raw(2),
                Qty::from_raw(1),
            ),
        }
    }

    #[test]
    fn order_run_sorts_ts_then_venue_then_file_then_index() {
        let mut recs = vec![
            keyed(5, 1, 1, 0),
            keyed(5, 0, 0, 3),
            keyed(1, 4, 5, 0),
            keyed(5, 0, 0, 1),
            keyed(3, 0, 0, 0),
        ];
        order_run(&mut recs);
        let keys: Vec<(u64, u8, u8, u64)> =
            recs.iter().map(|r| (r.ts_ns, r.venue, r.lord, r.idx)).collect();
        assert_eq!(
            keys,
            vec![(1, 4, 5, 0), (3, 0, 0, 0), (5, 0, 0, 1), (5, 0, 0, 3), (5, 1, 1, 0)]
        );
    }

    proptest::proptest! {
        /// §12 merge proptest: the run order is total (unique keys ⇒
        /// exactly one output order), stable (equal-(ts,venue) records
        /// of one file keep their per-file index order), and sorted.
        #[test]
        fn merge_order_is_total_stable_sorted(
            files in proptest::collection::vec(
                (0u8..6, proptest::collection::vec(0u64..16, 0..40)),
                1..5
            )
        ) {
            let mut recs: Vec<MergeKeyed> = Vec::new();
            for (lord, (venue, tss)) in files.iter().enumerate() {
                for (idx, ts) in tss.iter().enumerate() {
                    recs.push(keyed(*ts, *venue, lord as u8, idx as u64));
                }
            }
            let input_len = recs.len();
            let mut a = recs.clone();
            let mut b = recs;
            order_run(&mut a);
            b.reverse(); // adversarial starting order
            order_run(&mut b);

            // Total: same output regardless of input order.
            let ka: Vec<_> = a.iter().map(|r| (r.ts_ns, r.venue, r.lord, r.idx)).collect();
            let kb: Vec<_> = b.iter().map(|r| (r.ts_ns, r.venue, r.lord, r.idx)).collect();
            proptest::prop_assert_eq!(&ka, &kb);
            proptest::prop_assert_eq!(a.len(), input_len);

            // Sorted by the §3.2 key.
            for w in ka.windows(2) {
                proptest::prop_assert!(w[0] <= w[1]);
            }

            // Stable: within one file, equal (ts, venue) preserve idx order —
            // and idx order per file is preserved, period, whenever ts is equal.
            for i in 0..ka.len() {
                for j in (i + 1)..ka.len() {
                    if ka[i].2 == ka[j].2 && ka[i].0 == ka[j].0 && ka[i].1 == ka[j].1 {
                        proptest::prop_assert!(ka[i].3 < ka[j].3);
                    }
                }
            }
        }
    }

    // ---------------- rebase (§3.3) ----------------

    #[test]
    fn rebase_math_matches_design_3_3() {
        // Two runs: epochs E0, E0+G. Run ticks start at arbitrary
        // monotonic bases; deltas must survive exactly and the
        // inter-run gap must equal the epoch gap.
        let e0 = 1_000_000_000u64;
        let gap = 4_000_000_000u64;
        // run 0: ts 500, 700  → virt T0+0, T0+200
        // run 1: ts 9000, 9050 → virt T0+gap+0, T0+gap+50
        let virt = |epoch: u64, ts_first: u64, ts: u64| {
            VIRT_T0 + (epoch - e0) + (ts - ts_first)
        };
        assert_eq!(virt(e0, 500, 500), VIRT_T0);
        assert_eq!(virt(e0, 500, 700), VIRT_T0 + 200);
        assert_eq!(virt(e0 + gap, 9_000, 9_000), VIRT_T0 + gap);
        assert_eq!(virt(e0 + gap, 9_000, 9_050), VIRT_T0 + gap + 50);
        // Cross-run monotonicity for disjoint runs.
        assert!(virt(e0 + gap, 9_000, 9_000) > virt(e0, 500, 700));
    }

    // ---------------- universe (§3.5) ----------------

    #[test]
    fn universe_is_sorted_deduped_and_skips_none() {
        let mk = |sym: u32| MergedRec {
            tick: Tick::new(
                0,
                VenueId::Polymarket,
                sym,
                0,
                Price::from_raw(1),
                Qty::from_raw(1),
                Price::from_raw(2),
                Qty::from_raw(1),
            ),
            virt_ns: VIRT_T0,
            wall_ns: 0,
        };
        let merged = vec![mk(42), mk(7), mk(42), mk(SYMBOL_ID_NONE), mk(7)];
        assert_eq!(derive_universe(&merged), vec![7, 42]);
    }

    // ---------------- ctx ----------------

    #[test]
    fn ctx_tracks_max_order_notional_and_log() {
        let mut ctx = BacktestCtx::new();
        assert_eq!(ctx.now_ns(), VIRT_T0);
        let o = |px: i64, qty: i64| {
            Order::new(
                VIRT_T0,
                VenueId::Polymarket,
                42,
                core_types::Side::Bid,
                0,
                Price::from_raw(px),
                Qty::from_raw(qty),
                1,
            )
        };
        ctx.submit(o(500_000, 100_000_000)).unwrap(); // $50
        ctx.submit(o(500_000, 10_000_000)).unwrap(); // $5
        assert_eq!(ctx.max_order_notional_1e6(), 50_000_000);
        assert_eq!(ctx.orders().len(), 2);
    }
}
