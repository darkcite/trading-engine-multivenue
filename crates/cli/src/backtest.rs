// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

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
//!   injection paths: inherent `receive_table_v2` (copy #2 seam) + a
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
//!   `detail_version` 2 since VT4 — operator surface, never worker-parsed).
//!
//! ## Doctrine note — this module ALLOCATES
//!
//! Offline tooling under the `audit_replay.rs` doctrine: never loaded
//! by the engine loop, `Vec`/`String`/`Box` are used freely (merged
//! timelines are copied out of the mmap'd capture, ~80 B per tick).
//! Nothing here is reachable from a hot path.

pub mod fill;
pub mod stale;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use core_io::{PmlrReader, SlotKind};
use core_types::{
    AiCmd, AiCmdKind, ChannelEvent, ChannelId, DepthTopK, FeatId, Fill, OptSummary, Order, Price,
    Qty, RuleTableV2, Tick, VenueId, AI_SIDE_NONE, STRATEGY_SLOT_VM, SYMBOL_ID_NONE,
};
use ingress_ai::{validate_ruleset, DescriptorTable, RulesetReject};
use strategy_core::{Ctx, Strategy, SubmitErr};
use strategy_vm::VmStrategy;

use crate::backtest::fill::{
    usd_1e12_to_1e6_ceil, usd_1e12_to_1e6_floor, FillEngine, ModelOutcome, SynthFill,
    MAX_OPEN_PER_SYM, MAX_OPEN_TOTAL,
};

/// Book capacity the backtest vm is monomorphized with (design §3.6):
/// the same code as the engine's vm member (VM2 V3: the book
/// generic retired — sym capacity is `features::FEAT_SYM_SLOTS`,
/// sized
/// up because a multi-run capture can carry more syms than one boot.
pub const BACKTEST_VM_SLOTS: usize = strategy_vm::features::FEAT_SYM_SLOTS;

/// House virtual-clock base (design §3.3; the G3 first-window lesson —
/// `now − 0 ≥ horizon` must hold at the first tick, so the base must
/// exceed the 24 h max horizon by orders of magnitude).
pub const VIRT_T0: u64 = 100_000_000_000_000_000;

/// Oldest PMLR header version the merge accepts: v1 tick slots carry
/// an undefined venue byte, and the §3.2 total order keys on it.
/// Newest = `core_io::VERSION` (v3 since VT1 — `Tick.flags` /
/// `venue_time_ms`; until VT4 lands its stale law the harness replays
/// v3 ticks under the v2 law, i.e. never stale). `pub(crate)`:
/// `audit_pnl` and `capture_catalog` share the same acceptance law
/// through [`pmlr_version_accepted`].
pub(crate) const MIN_PMLR_VERSION: u16 = 2;

/// The one acceptance law for capture files across backtest, audit-pnl
/// and capture-catalog: `MIN_PMLR_VERSION ..= core_io::VERSION`.
#[inline]
pub(crate) const fn pmlr_version_accepted(version: u16) -> bool {
    version >= MIN_PMLR_VERSION && version <= core_io::VERSION
}

/// Per-venue tick-capture file labels, in file-ordinal order (mirrors
/// `audit_replay::VENUE_LABELS` — the cli spawn labels exactly;
/// `bybit` appended at WS9).
/// `pub(crate)`: `capture_catalog` reports in this fixed order.
pub(crate) const VENUE_LABELS: [&str; 7] = ["pm", "bn", "okx", "rpc", "deribit", "hl", "bybit"];

/// Venue labels accepted by the §4.3/§4.4 model flags, mapped to the
/// wire-stable [`VenueId`] byte. `rpc` is absent by design: it is not
/// a tradeable venue (no `VenueId`, no orders route to it).
const MODEL_VENUE_LABELS: [(&str, VenueId); 6] = [
    ("pm", VenueId::Polymarket),
    ("bn", VenueId::Binance),
    ("okx", VenueId::Okx),
    ("deribit", VenueId::Deribit),
    ("hl", VenueId::Hyperliquid),
    ("bybit", VenueId::Bybit),
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
    /// VT4: repeatable `--stale-after-ms <venue>:<ms>` overrides of the
    /// harness's re-judge thresholds (defaults = the venue table).
    pub stale_after_ms: Vec<String>,
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
        Ok(Split {
            is_pct: n,
            oos_pct: m,
        })
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

/// Fee + latency-penalty tables, indexed by [`VenueId`] byte
/// (0..=6 since WS9; **slot 5 = Ai is a DEAD slot** — the command
/// feed never trades — kept so the venue byte indexes directly).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ModelParams {
    /// `(maker_bps, taker_bps)` per venue. §4.3 defaults: all 0/0
    /// (Polymarket's current CLOB fee schedule; CEX venues cannot
    /// execute until 8j).
    pub fee_bps: [(u32, u32); 7],
    /// Activation penalty Δ ns per venue (§4.4). **A MEASUREMENT of the
    /// deployment host + network, not a constant** — see
    /// `docs/venue-latency.md` and the provenance on [`Default`].
    pub latency_ns: [u64; 7],
    /// VT4: staleness threshold ms per venue — the harness re-judges
    /// every v3 tick from its venue stamp against this table (a
    /// threshold change is a replay, not a recapture); 0 = never
    /// stale. Defaults = `VenueId::stale_after_ms_defaults()` (the
    /// doctrine-4 table the ingress uses); `--stale-after-ms
    /// <venue>:<ms>` overrides.
    pub stale_after_ms: [u32; 7],
}

impl Default for ModelParams {
    fn default() -> Self {
        Self {
            fee_bps: [(0, 0); 7],
            stale_after_ms: VenueId::stale_after_ms_defaults(),
            // Δ_venue = feed one-way p50 + REST request RTT p50 / 2,
            // rounded up to 10 ms (docs/venue-latency.md §2), MEASURED
            // 2026-09-03 17:07–17:32Z by `claude_worker.latency_probe`
            // on the MacBook Pro M4 / operator's home network (UTC+7):
            //   bn  71 + 107/2 → 130 ms   okx  67 + 120/2 → 130 ms
            //   deribit 108 + 208/2 → 220 ms   hl 272 + 124/2 → 340 ms
            //   bybit 29 + 44/2 → 60 ms
            //   pm: CLOB feed UNMEASURED (socket needs an asset id);
            //       REST one-way 114 ms → the §4.4 200 ms stays.
            // Slot 5 = Ai is dead (0). Pre-2026-09-03 the table was
            // the §4.4 assumption (pm 200 / bn·okx·deribit·bybit 100 /
            // hl 600). RE-MEASURE ON EVERY DEPLOYMENT AND LOCATION.
            latency_ns: [200 * MS, 130 * MS, 130 * MS, 220 * MS, 340 * MS, 0, 60 * MS],
        }
    }
}

/// Venue label (`pm`/`bn`/`okx`/`deribit`/`hl`/`bybit`) → `VenueId`
/// byte. `pub(crate)`: the engine's `--stale-after-ms` flag (VT2,
/// `paper.rs`) uses the same labels as the harness flags.
pub(crate) fn model_venue(label: &str) -> Option<usize> {
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
/// → `--latency-ns` (global) → `--latency-ns-venue` / `--fee-bps` /
/// `--stale-after-ms` (later occurrences of a repeated flag win).
pub fn parse_model_params(
    fee_specs: &[String],
    latency_global: Option<u64>,
    latency_specs: &[String],
    stale_specs: &[String],
) -> Result<ModelParams, HarnessError> {
    let mut p = ModelParams::default();
    for spec in stale_specs {
        let (v, ms) = spec.split_once(':').ok_or_else(|| {
            HarnessError::Usage(format!("bad --stale-after-ms {spec:?}: want <venue>:<ms>"))
        })?;
        let vi = model_venue(v).ok_or_else(|| {
            HarnessError::Usage(format!("bad --stale-after-ms {spec:?}: unknown venue {v:?}"))
        })?;
        let ms: u32 = ms.parse().map_err(|_| {
            HarnessError::Usage(format!("bad --stale-after-ms {spec:?}: unparseable ms"))
        })?;
        p.stale_after_ms[vi] = ms;
    }
    if let Some(ns) = latency_global {
        p.latency_ns = [ns; 7];
        p.latency_ns[VenueId::Ai as usize] = 0; // dead slot stays dead
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
            HarnessError::Usage(format!(
                "bad --latency-ns-venue {spec:?}: unknown venue {v:?}"
            ))
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
    runs.sort_by(|a, b| (a.epoch_ns, a.path.as_os_str()).cmp(&(b.epoch_ns, b.path.as_os_str())));
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
/// VM2 V5: one merged record's payload — the multi-channel replay
/// carries funding/ctx events, depth snapshots and OptSummary
/// records beside ticks, so every §1.1 feature evaluates in replay
/// exactly as live (§1.5).
#[derive(Copy, Clone, Debug)]
enum RecPayload {
    Tick(Tick),
    Event(ChannelEvent),
    Depth(DepthTopK),
    Opt(OptSummary),
}

#[derive(Copy, Clone, Debug)]
struct MergeKeyed {
    ts_ns: u64,
    venue: u8,
    lord: u8,
    idx: u64,
    payload: RecPayload,
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
    payload: RecPayload,
    virt_ns: u64,
    wall_ns: u64,
}

/// Per-run load summary for the stderr report.
#[derive(Clone, Debug)]
struct RunSummary {
    epoch_ns: u64,
    // Tied to the label set so a venue addition can never desync it
    // again (WS13 caught exactly that at bybit's arrival).
    venue_records: [u64; VENUE_LABELS.len()],
    /// VM2 V5: non-tick channel records loaded (funding/ctx events,
    /// depth, opt) + synthesized option mark-ticks + syms remapped
    /// through the per-run manifest join.
    events: u64,
    depths: u64,
    opts: u64,
    opt_synth_ticks: u64,
    remapped_syms: u64,
    /// VM2 V7: records DROPPED because their run-manifest descriptor
    /// is absent from the binding (newest) manifest — a DEAD
    /// instrument for this backtest. Ordinals reshuffle per boot
    /// (options, PM dailies): passing such syms through raw would
    /// interleave a foreign instrument's prices into whichever
    /// CURRENT instrument reuses the ordinal (the §6 never-bare-
    /// SymbolId-across-runs law; found live by the V7 iv proof —
    /// $248M phantom bounds from expired-option collisions).
    dropped_foreign: u64,
    /// VT4: per-lane stale accounting from the harness's own re-judge
    /// (v3 lanes) — `stale_blind` marks v2 lanes.
    stale: [stale::StaleStats; VENUE_LABELS.len()],
}

/// Open every present per-venue capture file of `run` (ticks +
/// VM2 V5: funding/ctx events, depth, OptSummary), cross-check
/// headers, remap syms through the per-run manifest join (`remap`),
/// synthesize option mark-ticks (D-7) for opt syms without a tick
/// lane, and return the run's §3.2-ordered records.
///
/// `dead` (VM2 V7) holds the run's manifest syms whose descriptors
/// the binding manifest no longer carries — their records are
/// dropped (see `RunSummary::dropped_foreign`). Manifest-less runs
/// pass both maps empty (identity, the legacy law).
///
/// Lane ordinals (`lord`): ticks = venue index, events = 8+vi,
/// depth = 16+vi, opt = 24+vi, synthetic mark-ticks = 40+vi — ticks
/// sort first at equal (ts, venue), preserving the book-before-
/// analytics reading order.
fn load_run(
    run: &RunDir,
    remap: &BTreeMap<u32, u32>,
    dead: &BTreeSet<u32>,
    stale_after_ms: [u32; 7],
) -> Result<(Vec<MergeKeyed>, RunSummary), HarnessError> {
    let mut recs: Vec<MergeKeyed> = Vec::new();
    let mut venue_records = [0u64; VENUE_LABELS.len()];
    // VT4: one re-judge per run — connections (and their clock
    // offsets) are per run; a threshold change is a replay.
    let mut judge = stale::StaleJudge::new(stale_after_ms);
    let mut summary_extra = (0u64, 0u64, 0u64, 0u64, 0u64); // ev, dp, op, synth, remapped
    let mut dropped_foreign = 0u64;
    let mut any_file = false;
    let map_sym = |sym: u32, remapped: &mut u64, dropped: &mut u64| -> Option<u32> {
        match remap.get(&sym) {
            Some(new) => {
                if *new != sym {
                    *remapped += 1;
                }
                Some(*new)
            }
            None => {
                if dead.contains(&sym) {
                    *dropped += 1;
                    None
                } else {
                    Some(sym)
                }
            }
        }
    };
    let mut tick_syms: BTreeSet<u32> = BTreeSet::new();
    for (vi, label) in VENUE_LABELS.iter().enumerate() {
        let path = run.path.join(format!("{label}-ticks.pmlr"));
        if !path.is_file() {
            continue; // a run captures only spawned venues (§3.1)
        }
        any_file = true;
        let reader = PmlrReader::<Tick>::open(&path)
            .map_err(|e: io::Error| HarnessError::Capture(format!("{}: {e}", path.display())))?;
        if reader.slot_kind() != SlotKind::Tick {
            return Err(HarnessError::Capture(format!(
                "{}: slot_kind {:?} is not Tick",
                path.display(),
                reader.slot_kind()
            )));
        }
        if !pmlr_version_accepted(reader.version()) {
            return Err(HarnessError::Capture(format!(
                "{}: PMLR v{} tick capture — the merge keys on the venue byte, which v1 \
                 files leave undefined; backtest accepts v{}..=v{}",
                path.display(),
                reader.version(),
                MIN_PMLR_VERSION,
                core_io::VERSION
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
        venue_records[vi] = records.len() as u64;
        recs.reserve(records.len());
        let has_venue_time = reader.has_venue_time();
        for (i, t) in records.iter().enumerate() {
            let mut tick = *t;
            // Judged in FILE order on the RAW sym (the estimator is
            // per connection; the remap below is a naming concern).
            judge.judge(vi, &mut tick, has_venue_time);
            tick.sym = match map_sym(t.sym, &mut summary_extra.4, &mut dropped_foreign) {
                Some(s) => s,
                None => continue,
            };
            tick_syms.insert(tick.sym);
            recs.push(MergeKeyed {
                ts_ns: t.ts_ns,
                venue: t.venue,
                lord: vi as u8,
                idx: i as u64,
                payload: RecPayload::Tick(tick),
            });
        }
    }
    if !any_file {
        return Err(HarnessError::Capture(format!(
            "{}: no <venue>-ticks.pmlr files present",
            run.path.display()
        )));
    }
    // VM2 V5: non-tick channels — absent files are normal (older
    // captures, unspawned lanes); headers cross-check like ticks.
    for (vi, label) in VENUE_LABELS.iter().enumerate() {
        // Funding/ctx events (the vm consumes Funding + AssetCtx —
        // gap/trade/book channels stay offline-audit material).
        let path = run.path.join(format!("{label}-events.pmlr"));
        if path.is_file() {
            let reader = PmlrReader::<ChannelEvent>::open(&path).map_err(|e: io::Error| {
                HarnessError::Capture(format!("{}: {e}", path.display()))
            })?;
            if reader.slot_kind() != SlotKind::Event {
                return Err(HarnessError::Capture(format!(
                    "{}: slot_kind {:?} is not ChannelEvent",
                    path.display(),
                    reader.slot_kind()
                )));
            }
            for (i, e) in reader.records().iter().enumerate() {
                let keep =
                    e.channel == ChannelId::Funding as u8 || e.channel == ChannelId::AssetCtx as u8;
                if !keep {
                    continue;
                }
                let mut ev = *e;
                if ev.sym != SYMBOL_ID_NONE {
                    ev.sym = match map_sym(ev.sym, &mut summary_extra.4, &mut dropped_foreign) {
                        Some(s) => s,
                        None => continue,
                    };
                }
                recs.push(MergeKeyed {
                    ts_ns: e.ts_ns,
                    venue: e.venue,
                    lord: 8 + vi as u8,
                    idx: i as u64,
                    payload: RecPayload::Event(ev),
                });
                summary_extra.0 += 1;
            }
        }
        // Depth snapshots (kind 7).
        let path = run.path.join(format!("{label}-depth.pmlr"));
        if path.is_file() {
            let reader = PmlrReader::<DepthTopK>::open(&path).map_err(|e: io::Error| {
                HarnessError::Capture(format!("{}: {e}", path.display()))
            })?;
            if reader.slot_kind() != SlotKind::Depth {
                return Err(HarnessError::Capture(format!(
                    "{}: slot_kind {:?} is not DepthTopK",
                    path.display(),
                    reader.slot_kind()
                )));
            }
            for (i, d) in reader.records().iter().enumerate() {
                let mut dp = *d;
                dp.sym = match map_sym(d.sym, &mut summary_extra.4, &mut dropped_foreign) {
                    Some(s) => s,
                    None => continue,
                };
                recs.push(MergeKeyed {
                    ts_ns: d.ts_ns,
                    venue: d.venue,
                    lord: 16 + vi as u8,
                    idx: i as u64,
                    payload: RecPayload::Depth(dp),
                });
                summary_extra.1 += 1;
            }
        }
        // OptSummary records (kind 6) + D-7 synthetic mark-ticks for
        // opt syms with no tick lane (mark present only — okx's
        // markless summaries stay feature-only, honestly unpriceable).
        let path = run.path.join(format!("{label}-opt-summary.pmlr"));
        if path.is_file() {
            let reader = PmlrReader::<OptSummary>::open(&path).map_err(|e: io::Error| {
                HarnessError::Capture(format!("{}: {e}", path.display()))
            })?;
            if reader.slot_kind() != SlotKind::OptSummary {
                return Err(HarnessError::Capture(format!(
                    "{}: slot_kind {:?} is not OptSummary",
                    path.display(),
                    reader.slot_kind()
                )));
            }
            for (i, o) in reader.records().iter().enumerate() {
                let mut op = *o;
                op.sym = match map_sym(o.sym, &mut summary_extra.4, &mut dropped_foreign) {
                    Some(s) => s,
                    None => continue,
                };
                recs.push(MergeKeyed {
                    ts_ns: o.ts_ns,
                    venue: o.venue,
                    lord: 24 + vi as u8,
                    idx: i as u64,
                    payload: RecPayload::Opt(op),
                });
                summary_extra.2 += 1;
                let has_mark =
                    op.flags & core_types::OPT_SUMMARY_FLAG_MARK_PX != 0 && op.mark_px_1e9 > 0;
                if has_mark && !tick_syms.contains(&op.sym) {
                    // Zero-spread mark tick: the fill engine executes
                    // these syms under the D-7 mark-fill law (the
                    // harness registers them); the vm prices its
                    // option legs at Mid = mark.
                    let mark_1e6 = op.mark_px_1e9 / 1_000;
                    if mark_1e6 > 0 {
                        let venue = VenueId::from_u8(op.venue).unwrap_or(VenueId::Deribit);
                        let t = Tick::new(
                            op.ts_ns,
                            venue,
                            op.sym,
                            0,
                            Price::from_raw(mark_1e6),
                            Qty::from_raw(1_000_000_000_000),
                            Price::from_raw(mark_1e6),
                            Qty::from_raw(1_000_000_000_000),
                        );
                        recs.push(MergeKeyed {
                            ts_ns: op.ts_ns,
                            venue: op.venue,
                            lord: 40 + vi as u8,
                            idx: i as u64,
                            payload: RecPayload::Tick(t),
                        });
                        summary_extra.3 += 1;
                    }
                }
            }
        }
    }
    order_run(&mut recs);
    Ok((
        recs,
        RunSummary {
            epoch_ns: run.epoch_ns,
            venue_records,
            events: summary_extra.0,
            depths: summary_extra.1,
            opts: summary_extra.2,
            opt_synth_ticks: summary_extra.3,
            remapped_syms: summary_extra.4,
            dropped_foreign,
            stale: judge.stats,
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
    stale_after_ms: [u32; 7],
) -> Result<(Vec<MergedRec>, Vec<RunSummary>), HarnessError> {
    // VM2 V5 (§6 replay half): per-run sym remap through the
    // manifest join — each run's `<sym>\t<descriptor>` rows joined
    // by DESCRIPTOR to the NEWEST run's manifest, so option ordinals
    // that reshuffle across boots evaluate as ONE instrument (the
    // row's validate-time binding is against the newest manifest).
    // Manifest-less runs get the identity map.
    let newest_by_desc: BTreeMap<String, u32> = read_manifest_rows(&runs[runs.len() - 1].path)
        .into_iter()
        .map(|(sym, desc)| (desc, sym))
        .collect();
    let epoch_0 = runs[0].epoch_ns;
    let mut merged: Vec<MergedRec> = Vec::new();
    let mut summaries: Vec<RunSummary> = Vec::with_capacity(runs.len());
    let mut prev_last_virt: u64 = 0;
    for run in runs {
        let mut remap: BTreeMap<u32, u32> = BTreeMap::new();
        let mut dead: BTreeSet<u32> = BTreeSet::new();
        for (sym, desc) in read_manifest_rows(&run.path) {
            match newest_by_desc.get(&desc) {
                Some(new_sym) => {
                    remap.insert(sym, *new_sym);
                }
                None => {
                    // VM2 V7: the binding manifest no longer carries
                    // this descriptor — DEAD instrument; its records
                    // drop rather than leak into whichever current
                    // instrument reuses the ordinal (§6 law).
                    dead.insert(sym);
                }
            }
        }
        let (recs, summary) = load_run(run, &remap, &dead, stale_after_ms)?;
        summaries.push(summary);
        if recs.is_empty() {
            continue; // header-only files everywhere: run holds no records
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
            let mut payload = r.payload;
            match &mut payload {
                RecPayload::Tick(t) => t.ts_ns = virt_ns,
                RecPayload::Event(e) => e.ts_ns = virt_ns,
                RecPayload::Depth(d) => d.ts_ns = virt_ns,
                RecPayload::Opt(o) => o.ts_ns = virt_ns,
            }
            merged.push(MergedRec {
                payload,
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

/// `<sym>\t<descriptor>` rows of one run's `instrument-manifest.tsv`
/// (empty when absent/unreadable; malformed lines skipped — the
/// manifest reader law).
fn read_manifest_rows(dir: &Path) -> Vec<(u32, String)> {
    let path = dir.join("instrument-manifest.tsv");
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            let mut it = line.splitn(2, '\t');
            if let (Some(sym_s), Some(desc)) = (it.next(), it.next()) {
                if let Ok(sym) = sym_s.parse::<u32>() {
                    if !desc.is_empty() {
                        out.push((sym, desc.to_owned()));
                    }
                }
            }
        }
    }
    out
}

/// VM2 V4: descriptor table from the NEWEST run's
/// `instrument-manifest.tsv` (`<sym>\t<descriptor>` rows, M4.2 D3).
/// Absent/malformed rows skip-and-count per the manifest's reader
/// law; an absent file yields the EMPTY table.
fn manifest_descriptor_table(runs: &[RunDir]) -> DescriptorTable {
    let newest = match runs.last() {
        Some(r) => r,
        None => return DescriptorTable::empty(),
    };
    let entries: Vec<(String, u32, u8)> = read_manifest_rows(&newest.path)
        .into_iter()
        .map(|(sym, desc)| {
            let caps = ingress_ai::caps_of_descriptor(&desc);
            (desc, sym, caps)
        })
        .collect();
    DescriptorTable::from_entries(entries)
}

/// Capture-observed universe (§3.5): sorted, deduplicated syms across
/// every merged tick. `SYMBOL_ID_NONE` can never be a real instrument
/// and is skipped defensively.
fn derive_universe(merged: &[MergedRec]) -> Vec<u32> {
    let mut set: BTreeSet<u32> = BTreeSet::new();
    for r in merged {
        // Tick-observed only (v1 rows' rule-6 law unchanged;
        // synthetic option mark-ticks count — a markable option IS
        // observable). v2 rows resolve through descriptors, never
        // this set.
        if let RecPayload::Tick(t) = &r.payload {
            if t.sym != SYMBOL_ID_NONE {
                set.insert(t.sym);
            }
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
        let notional_1e6 = ((order.px.raw() as i128 * order.qty.raw() as i128) / 1_000_000) as i64;
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
            "\"max_drawdown_usd\":{dd},",
            "\"round_trips\":{rt},",
            "\"legs\":{legs}}},",
            "\"bounds\":{{",
            "\"max_order_notional_usd\":{mo},",
            "\"max_symbol_notional_usd\":{ms},",
            "\"max_total_notional_usd\":{mt}}},",
            "\"position_rows\":{prows}}}"
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
        rt = vals.oos_round_trips,
        legs = vals.oos_legs,
        prows = vals.position_rows,
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
    /// VM2 V5 (D-3) additive fields — schema stays 1; the worker's
    /// get-based parser tolerates additions (verified; ruling cited
    /// in the worker pins).
    oos_round_trips: u64,
    oos_legs: u64,
    position_rows: u64,
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
    /// [`BACKTEST_VM_SLOTS`] feature sym slots (fail-closed,
    /// counted — VM2 V3: the feature engine's exhaustion counter).
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
    /// VM2 V5: non-tick channel records merged (funding/ctx events).
    pub merged_events: u64,
    /// Depth snapshots merged.
    pub merged_depths: u64,
    /// OptSummary records merged.
    pub merged_opts: u64,
    /// D-7 synthetic option mark-ticks synthesized.
    pub opt_synth_ticks: u64,
    /// Syms remapped through the per-run manifest join.
    pub remapped_syms: u64,
    /// VM2 V7: dead-descriptor records dropped (§6 law; see
    /// `RunSummary::dropped_foreign`).
    pub dropped_foreign: u64,
    /// D-7 mark-law fills executed (> 0 ⇒ the assumption printed).
    pub mark_fills: u64,
    /// VT4: ticks the fill model skipped as STALE (no mark, no fill).
    pub stale_ticks_skipped: u64,
    /// Warmup window end on the virtual clock (== first_virt when 0).
    pub warmup_end_virt_ns: u64,
    /// D-3: OOS round-trips (exit landed at/after the boundary).
    pub oos_round_trips: u64,
    /// Committed-table position rows.
    pub position_rows: u64,
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

/// VM2 V5 warmup (vm2-plan §1.5, refined at V5 — recorded in §8):
/// warmup = the longest window the TABLE actually references
/// (rolling windows in minutes; Apr24 ⇒ 1440; Apr72/72 h ⇒ 4320),
/// and 0 when the table references no windowed/funding feature — a
/// flat 24 h floor would zero out every short-capture v1 backtest
/// while warming nothing. During warmup the replay feeds FEATURES
/// only (books, windows, marks fill; the fill model runs); the vm
/// evaluates nothing, so no entries exist before every referenced
/// window is honestly full.
fn warmup_ns_of(table: &RuleTableV2) -> u64 {
    let mut max_min: u64 = 0;
    let mut i = 0usize;
    let len = (table.len as usize).min(core_types::RULE_TABLE_ROWS);
    while i < len {
        let r = &table.rows[i];
        let feats = [r.feat_a, r.feat_b, r.feat_c];
        let wins = [r.win_a, r.win_b, r.win_c];
        let mut k = 0;
        while k < feats.len() {
            if let Some(f) = FeatId::from_u8(feats[k]) {
                let need: u64 = match f {
                    FeatId::Apr24 => 1_440,
                    FeatId::Apr72 => 4_320,
                    _ if f.requires_window() => wins[k] as u64,
                    _ => 0,
                };
                if need > max_min {
                    max_min = need;
                }
            }
            k += 1;
        }
        i += 1;
    }
    max_min * 60 * 1_000_000_000
}

/// Run one backtest end to end (§3 substrate + §5 report; hold-model
/// accounting until H2). On `Err` the caller must print the reason to
/// stderr and exit nonzero WITHOUT touching stdout — the worker treats
/// any nonzero exit as "no trustworthy report exists".
pub fn run(cfg: &BacktestConfig) -> Result<BacktestOutput, HarnessError> {
    let split = parse_split(&cfg.split)?;
    let model = parse_model_params(
        &cfg.fee_bps,
        cfg.latency_ns,
        &cfg.latency_ns_venue,
        &cfg.stale_after_ms,
    )?;

    // Candidate bytes + identity (§3.5): full SHA-256 is schema-1's
    // `ruleset_hash`; its first 16 bytes are the wire hash128.
    let ruleset_bytes = std::fs::read(&cfg.ruleset).map_err(|e| {
        HarnessError::Usage(format!(
            "cannot read --ruleset {}: {e}",
            cfg.ruleset.display()
        ))
    })?;
    let full_hash = core_crypto::sha256(&ruleset_bytes);
    let mut hash128 = [0u8; 16];
    hash128.copy_from_slice(&full_hash[..16]);
    let hash_hex = hex_lower(&full_hash);

    // Capture discovery + merge + rebase (§3.1–§3.3).
    let runs = discover_runs(&cfg.replay_dir)?;
    let (merged, run_summaries) = load_and_merge(&runs, model.stale_after_ms)?;
    let universe = derive_universe(&merged);

    // The REUSED validator (§3.5) — same byte scanner, same reject
    // set as the live side path; the capture-observed universe stands
    // in for the boot universe. VM2 V4: v2 rows resolve descriptors
    // against the NEWEST run's `instrument-manifest.tsv` (capability
    // bits from the offline string law — permissive where the string
    // under-determines, docs on `caps_of_descriptor`); manifest-less
    // (pre-D3) captures resolve nothing — v2 rows then reject
    // `Descriptor` honestly, v1 rows are unaffected. Cross-run
    // options-ordinal reshuffle stays a documented V5 concern (the
    // multi-channel merge adds per-run rebind).
    let descriptors = manifest_descriptor_table(&runs);
    let mut table = Box::new(RuleTableV2::EMPTY);
    validate_ruleset(
        &ruleset_bytes,
        &hash128,
        &universe,
        &descriptors,
        &mut table,
    )
    .map_err(HarnessError::Reject)?;

    // §3.4 boundary: N% of the merged wall-time span. Replay is
    // continuous through it; only accounting buckets on it (H2 —
    // interpretation pinned in the H1 log: OOS = [boundary, end],
    // boundary inclusive).
    let first_virt = merged[0].virt_ns;
    let last_virt = merged[merged.len() - 1].virt_ns;
    let span = last_virt - first_virt;
    let boundary_virt = first_virt + ((span as u128 * split.is_pct as u128) / 100) as u64;
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
    let mut vm: Box<VmStrategy> = Box::new(VmStrategy::new());
    let mut ctx = BacktestCtx::new();
    vm.on_start(&mut ctx)
        .map_err(|e| HarnessError::Internal(format!("vm on_start failed: {e}")))?;
    // 1. Inherent receive_table_v2 — the copy-#2 seam the StrategySet
    //    forwarder would otherwise drive (the trait hook is a no-op on
    //    a bare vm by design).
    vm.receive_table_v2(&table);
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
    // D-7: register mark-filled option syms (any sym that produced a
    // synthetic mark-tick this root — collected from the payloads).
    let mut mark_fill_syms: BTreeSet<u32> = BTreeSet::new();
    for rec in &merged {
        if let RecPayload::Opt(o) = &rec.payload {
            if o.flags & core_types::OPT_SUMMARY_FLAG_MARK_PX != 0 && o.mark_px_1e9 > 0 {
                mark_fill_syms.insert(o.sym);
            }
        }
    }
    for sym in &mark_fill_syms {
        engine.set_mark_fill_sym(*sym);
    }
    // VM2 V5 warmup window (docs on `warmup_ns_of`).
    let warmup_end_virt = first_virt.saturating_add(warmup_ns_of(&table));
    let mut consumed = ctx.orders().len(); // on_start/on_ai emit nothing, but stay uniform
    debug_assert_eq!(consumed, 0);
    let mut fills_scratch: Vec<SynthFill> = Vec::with_capacity(MAX_OPEN_TOTAL);
    // D-3: round-trips completed before the boundary are IS; the OOS
    // figure is the total minus this snapshot (an IS-entered position
    // whose EXIT lands OOS counts OOS — the honest direction).
    let mut rt_at_boundary: Option<u64> = None;
    for rec in &merged {
        ctx.now_ns = rec.virt_ns;
        if rt_at_boundary.is_none() && rec.virt_ns >= boundary_virt {
            rt_at_boundary = Some(vm.round_trips);
        }
        let warm = rec.virt_ns < warmup_end_virt;
        match &rec.payload {
            RecPayload::Tick(t) => {
                engine.on_record(t, rec.virt_ns, rec.wall_ns, &mut fills_scratch);
                if warm {
                    vm.feats.on_tick(t, rec.virt_ns);
                } else {
                    vm.on_tick(t, &mut ctx);
                }
            }
            RecPayload::Event(e) => {
                if warm {
                    vm.feats.on_venue_event(e, rec.virt_ns);
                } else {
                    vm.on_venue_event(e, &mut ctx);
                }
                fills_scratch.clear();
            }
            RecPayload::Depth(d) => {
                if warm {
                    vm.feats.on_depth(d, rec.virt_ns);
                } else {
                    vm.on_depth(d, &mut ctx);
                }
                fills_scratch.clear();
            }
            RecPayload::Opt(o) => {
                if warm {
                    vm.feats.on_opt_summary(o, rec.virt_ns);
                } else {
                    vm.on_opt_summary(o, &mut ctx);
                }
                fills_scratch.clear();
            }
        }
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
    let oos_round_trips = vm.round_trips - rt_at_boundary.unwrap_or(vm.round_trips);
    // Position rows of the committed table (the D-3 report field the
    // worker keys the round-trip gate on).
    let mut position_rows = 0u64;
    {
        let len = (table.len as usize).min(core_types::RULE_TABLE_ROWS);
        let mut i = 0;
        while i < len {
            if table.rows[i].flags & core_types::ROW_FLAG_POSITION != 0 {
                position_rows += 1;
            }
            i += 1;
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
        oos_round_trips,
        oos_legs: outcome.oos_trades,
        position_rows,
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
        vm_book_track_failed: vm.feats.sym_slots_exhausted,
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
        merged_events: run_summaries.iter().map(|r| r.events).sum(),
        merged_depths: run_summaries.iter().map(|r| r.depths).sum(),
        merged_opts: run_summaries.iter().map(|r| r.opts).sum(),
        opt_synth_ticks: run_summaries.iter().map(|r| r.opt_synth_ticks).sum(),
        remapped_syms: run_summaries.iter().map(|r| r.remapped_syms).sum(),
        dropped_foreign: run_summaries.iter().map(|r| r.dropped_foreign).sum(),
        mark_fills: outcome.mark_fills,
        stale_ticks_skipped: outcome.stale_ticks_skipped,
        warmup_end_virt_ns: warmup_end_virt,
        oos_round_trips,
        position_rows,
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
        let detail = render_detail(
            &hash_hex,
            &cfg.split,
            &model,
            &stats,
            &outcome,
            &engine,
            &run_summaries,
        );
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

/// VT4: one deterministic per-run stale line — every present lane as
/// `label=stale/ticks (bps‰-style basis points of span)`; a v2 lane
/// prints `stale-blind` so nobody mistakes "0 stale" for "measured".
/// `pub(crate)`: audit-pnl prints the same shape.
pub(crate) fn render_stale_line(stale: &[stale::StaleStats; VENUE_LABELS.len()]) -> String {
    let mut s = String::with_capacity(160);
    s.push_str(" stale:");
    for (lord, label) in VENUE_LABELS.iter().enumerate() {
        let st = &stale[lord];
        if st.ticks == 0 {
            continue;
        }
        if st.stale_blind {
            s.push_str(&format!(" {label}=stale-blind(v2)"));
        } else {
            s.push_str(&format!(
                " {label}={}/{} ({}bps)",
                st.stale_ticks,
                st.ticks,
                st.stale_time_bps()
            ));
        }
    }
    s
}

/// VT4 sidecar block: one object per run, one entry per PRESENT lane —
/// `{"epoch_ns":…,"lanes":{"okx":{"ticks":n,"stale_ticks":n,
/// "stale_time_bps":n,"stale_blind":false},…}}` (BTree-free: the lane
/// order is the fixed [`VENUE_LABELS`] order).
fn render_stale_runs_json(runs: &[RunSummary]) -> String {
    let mut s = String::with_capacity(256);
    for (i, r) in runs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{{\"epoch_ns\":{},\"lanes\":{{", r.epoch_ns));
        let mut first = true;
        for (lord, label) in VENUE_LABELS.iter().enumerate() {
            let st = &r.stale[lord];
            if st.ticks == 0 {
                continue;
            }
            if !first {
                s.push(',');
            }
            first = false;
            s.push_str(&format!(
                "\"{label}\":{{\"ticks\":{},\"stale_ticks\":{},\"stale_time_bps\":{},\"stale_blind\":{}}}",
                st.ticks,
                st.stale_ticks,
                st.stale_time_bps(),
                st.stale_blind
            ));
        }
        s.push_str("}}");
    }
    s
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
    s.push_str(&format!(
        "channels: events={} depths={} opts={} opt_synth_ticks={} remapped_syms={} \
         dropped_foreign={}\n",
        stats.merged_events,
        stats.merged_depths,
        stats.merged_opts,
        stats.opt_synth_ticks,
        stats.remapped_syms,
        stats.dropped_foreign
    ));
    for (i, r) in runs.iter().enumerate() {
        s.push_str(&format!("  run[{i}] epoch_ns={}", r.epoch_ns));
        for (lord, label) in VENUE_LABELS.iter().enumerate() {
            s.push_str(&format!(" {label}={}", r.venue_records[lord]));
        }
        if r.dropped_foreign > 0 {
            s.push_str(&format!(" dropped_foreign={}", r.dropped_foreign));
        }
        s.push('\n');
        s.push_str("   ");
        s.push_str(&render_stale_line(&r.stale));
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
         okx={}:{} deribit={}:{} hl={}:{}; open-order caps {}/sym {} total; \
         stale_after_ms pm={} bn={} okx={} deribit={} hl={} bybit={} (stale ticks skipped: {})\n",
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
        model.stale_after_ms[VenueId::Polymarket as usize],
        model.stale_after_ms[VenueId::Binance as usize],
        model.stale_after_ms[VenueId::Okx as usize],
        model.stale_after_ms[VenueId::Deribit as usize],
        model.stale_after_ms[VenueId::Hyperliquid as usize],
        model.stale_after_ms[VenueId::Bybit as usize],
        stats.stale_ticks_skipped,
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
        "fills: total={} oos={} mark={}\n",
        stats.fills_total, stats.fills_oos, stats.mark_fills
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
        s.push_str(&format!(
            "--emit-detail: sidecar written to {}\n",
            p.display()
        ));
    }
    s
}

/// The `--emit-detail` sidecar (§5): versioned SEPARATELY from
/// schema-1 (`detail_version` 2 since VT4 — the `stale` block and the
/// model's `stale_after_ms` table), operator/session surface, never
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
    runs: &[RunSummary],
) -> String {
    let mut s = String::with_capacity(4096);
    s.push_str(&format!(
        concat!(
            "{{\"detail_version\":2,",
            "\"ruleset_hash\":\"{hash}\",",
            "\"split\":\"{split}\",",
            "\"model\":{{",
            "\"latency_ns\":{{\"pm\":{lpm},\"bn\":{lbn},\"okx\":{lokx},\"deribit\":{lde},\"hl\":{lhl}}},",
            "\"fee_bps\":{{\"pm\":[{fpm0},{fpm1}],\"bn\":[{fbn0},{fbn1}],\"okx\":[{fokx0},{fokx1}],",
            "\"deribit\":[{fde0},{fde1}],\"hl\":[{fhl0},{fhl1}]}},",
            "\"open_order_caps\":[{cap_sym},{cap_tot}],",
            "\"stale_after_ms\":{{\"pm\":{spm},\"bn\":{sbn},\"okx\":{sokx},\"deribit\":{sde},",
            "\"hl\":{shl},\"bybit\":{sby}}}}},",
            "\"stale\":{{\"ticks_skipped\":{sts},\"runs\":[{sruns}]}},",
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
        spm = model.stale_after_ms[VenueId::Polymarket as usize],
        sbn = model.stale_after_ms[VenueId::Binance as usize],
        sokx = model.stale_after_ms[VenueId::Okx as usize],
        sde = model.stale_after_ms[VenueId::Deribit as usize],
        shl = model.stale_after_ms[VenueId::Hyperliquid as usize],
        sby = model.stale_after_ms[VenueId::Bybit as usize],
        sts = stats.stale_ticks_skipped,
        sruns = render_stale_runs_json(runs),
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
            "5/95", "95/5", "70/40", "100/0", "0/0", "70/30/0", "70", "", "/", "70/", "/30", "a/b",
            " 70/30", "70/30 ", "070/30", "70/030", "+70/30", "-70/170", "1000/0",
        ] {
            assert!(
                matches!(parse_split(bad), Err(HarnessError::Usage(_))),
                "{bad:?} must be a usage error"
            );
        }
    }

    // ---------------- model flags (§4.3/§4.4) ----------------

    #[test]
    fn model_params_defaults_pin_measured_table() {
        let p = ModelParams::default();
        assert_eq!(p.fee_bps, [(0, 0); 7]);
        // The 2026-09-03 measurement (docs/venue-latency.md §3); slot 5
        // = Ai (dead, 0), slot 6 = Bybit. A new deployment re-measures
        // and re-pins — this test exists so the table never drifts
        // silently.
        assert_eq!(
            p.latency_ns,
            [200 * MS, 130 * MS, 130 * MS, 220 * MS, 340 * MS, 0, 60 * MS]
        );
    }

    #[test]
    fn model_params_overrides_layer_correctly() {
        let p = parse_model_params(
            &["pm:0:10".to_owned(), "hl:3:4".to_owned()],
            Some(1_000),
            &["deribit:42".to_owned()],
            &["okx:250".to_owned(), "bn:0".to_owned(), "okx:300".to_owned()],
        )
        .unwrap();
        // Global latency replaced every TRADEABLE slot (the Ai dead
        // slot stays 0 — WS9), then deribit won on top.
        assert_eq!(p.latency_ns, [1_000, 1_000, 1_000, 42, 1_000, 0, 1_000]);
        assert_eq!(p.fee_bps[VenueId::Polymarket as usize], (0, 10));
        assert_eq!(p.fee_bps[VenueId::Hyperliquid as usize], (3, 4));
        assert_eq!(p.fee_bps[VenueId::Binance as usize], (0, 0));
        // VT4: stale thresholds default to the venue table; overrides
        // replace only the named venue, the last spec wins, 0 is legal.
        assert_eq!(p.stale_after_ms[VenueId::Okx as usize], 300);
        assert_eq!(p.stale_after_ms[VenueId::Binance as usize], 0);
        assert_eq!(p.stale_after_ms[VenueId::Bybit as usize], 500);
        assert_eq!(p.stale_after_ms[VenueId::Ai as usize], 0);
    }

    #[test]
    fn model_params_rejects_malformed_specs() {
        for bad_fee in [
            "pm:1", "pm:1:2:3", "rpc:1:2", "nope:1:2", "pm:x:2", "pm:1:y",
        ] {
            assert!(
                matches!(
                    parse_model_params(&[bad_fee.to_owned()], None, &[], &[]),
                    Err(HarnessError::Usage(_))
                ),
                "fee spec {bad_fee:?} must be a usage error"
            );
        }
        for bad_lat in ["pm", "pm:1:2", "rpc:5", "nope:5", "pm:x"] {
            assert!(
                matches!(
                    parse_model_params(&[], None, &[bad_lat.to_owned()], &[]),
                    Err(HarnessError::Usage(_))
                ),
                "latency spec {bad_lat:?} must be a usage error"
            );
        }
        for bad_stale in ["okx", "mars:400", "okx:fast", "okx:-1"] {
            assert!(
                matches!(
                    parse_model_params(&[], None, &[], &[bad_stale.to_owned()]),
                    Err(HarnessError::Usage(_))
                ),
                "stale spec {bad_stale:?} must be a usage error"
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
            oos_round_trips: 2,
            oos_legs: 4,
            position_rows: 1,
        };
        let line = render_schema1("ab12", "70/30", &vals);
        // VM2 V5 (D-3): schema stays 1; `round_trips`/`legs` are
        // ADDITIVE inside "oos", `position_rows` additive top-level —
        // the worker's get-based parser tolerates both (its pins
        // cite the D-3 ruling).
        assert_eq!(
            line,
            "{\"schema_version\":1,\"ruleset_hash\":\"ab12\",\"split\":\"70/30\",\
             \"oos\":{\"net_pnl_usd\":0.0,\"trades\":0,\"trading_days\":0,\
             \"max_drawdown_usd\":0.0,\"round_trips\":2,\"legs\":4},\
             \"bounds\":{\"max_order_notional_usd\":50.0,\
             \"max_symbol_notional_usd\":0.0,\"max_total_notional_usd\":0.0},\
             \"position_rows\":1}"
        );
    }

    // ---------------- merge ordering (§3.2) ----------------

    fn keyed(ts: u64, venue: u8, lord: u8, idx: u64) -> MergeKeyed {
        MergeKeyed {
            ts_ns: ts,
            venue,
            lord,
            idx,
            payload: RecPayload::Tick(Tick::new(
                ts,
                VenueId::Polymarket,
                1,
                idx as u32,
                Price::from_raw(1),
                Qty::from_raw(1),
                Price::from_raw(2),
                Qty::from_raw(1),
            )),
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
        let keys: Vec<(u64, u8, u8, u64)> = recs
            .iter()
            .map(|r| (r.ts_ns, r.venue, r.lord, r.idx))
            .collect();
        assert_eq!(
            keys,
            vec![
                (1, 4, 5, 0),
                (3, 0, 0, 0),
                (5, 0, 0, 1),
                (5, 0, 0, 3),
                (5, 1, 1, 0)
            ]
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
        let virt = |epoch: u64, ts_first: u64, ts: u64| VIRT_T0 + (epoch - e0) + (ts - ts_first);
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
            payload: RecPayload::Tick(Tick::new(
                0,
                VenueId::Polymarket,
                sym,
                0,
                Price::from_raw(1),
                Qty::from_raw(1),
                Price::from_raw(2),
                Qty::from_raw(1),
            )),
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
