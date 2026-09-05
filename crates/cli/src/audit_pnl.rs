// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # audit-pnl — M4.2 shadow-P&L attribution over LOGGED intents
//! (mvp-plan §4-M4; consumers per §9.9: PMLR ticks + logged intents +
//! fills; design + rulings in docs/m4-progress.md)
//!
//! Replays a capture root's `engine-orders.pmlr` intents (M4.1)
//! through THE §4 strict-cross fill model — [`crate::backtest::fill`]
//! REUSED verbatim, never twinned — and reports per-strategy (and,
//! for the vm, per-ruleset-hash) modeled P&L beside the engine's
//! paper-fill view for the same window. Two views, one report.
//!
//! DOCTRINE (audit_replay.rs): offline tool — never loaded by the
//! engine loop; allocates freely; deterministic byte-identical output
//! for identical inputs (BTree iteration, fixed sort keys, integer
//! math, [`crate::backtest::fmt_usd_1e6`] rendering).
//!
//! Laws inherited verbatim:
//!
//! * **Discovery + ordering**: [`crate::backtest::discover_runs`] —
//!   the harness's own root/run resolution and epoch order (§3.1).
//! * **Merge + virtual clock (§3.2/§3.3)**: per-run §3.2 total order,
//!   VIRT_T0 rebase, runs never interleaved, cross-run overlap =
//!   refusal. The run anchor is min-first-ts across the run's TICK
//!   files; orders/fills/ai-cmds share the engine clock and rebase
//!   with the SAME anchor. At equal ts, ticks sort before orders —
//!   the engine's own "the fill pass precedes the emit" law; ruleset
//!   commits sort before orders so an in-stream flip attributes the
//!   very next intent to the NEW hash (8g §6 ordering).
//! * **Model**: one [`fill::FillEngine`] per attribution key with
//!   `boundary_virt_ns = 0` — the OOS book IS the whole-window book,
//!   so `oos_*` outcomes are the window stats. Same `ModelParams`
//!   flags as the backtest (`--fee-bps`, `--latency-ns`,
//!   `--latency-ns-venue`).
//! * **§6 keying — NEVER bare SymbolId across runs**: ordinals
//!   reshuffle per boot by design, so every sym is rewritten to a
//!   DENSE root-scoped id through its run's manifest descriptor
//!   (`instrument-manifest.tsv`, D3; `options-manifest.tsv`
//!   fallback). A manifest-less run resolves into a PER-RUN namespace
//!   (`run-<epoch>/sym-0x…`) — its instruments can never silently
//!   merge with another run's. Dense ids preserve the venue byte
//!   (bits 31..24) so the model's Δ/fee lookup is untouched.
//!
//! Attribution: `Order.strategy_id` (M4.1; `0xFF` = unattributed —
//! bare single-strategy boots). vm intents (slot 5) additionally
//! bucket by the ruleset hash ACTIVE at emit, reconstructed from the
//! ai-cmds `RulesetCommit` timeline via `AiCmd::ruleset_hash128` (the
//! shared helper — no second decoder). Per-hash books are INDEPENDENT
//! replays (a per-hash row is "this ruleset alone"), beside the
//! slot-5 aggregate.
//!
//! Paper view: `engine-fills.pmlr` folded into signed cash flow +
//! position mark-out at last mids (`net = cash + Σ pos × mark`; paper
//! charges no fees). Empty in every paper run today — reported
//! honestly as zero.
//!
//! RG3 (`docs/regime-and-dashboard-plan.md` §4.8): the report gains an
//! ADDITIVE `regime` section. The same `RegimeState` the engine runs
//! replays over the window's ticks, the funding reference's prints
//! (`<venue>-events.pmlr`) and the `SetRegime` frames of `ai-cmds.pmlr`
//! ([`crate::backtest::regime`]); every intent is bucketed by the
//! EFFECTIVE word of each profile at its emit instant into an
//! independent fill-model replay per `(profile, word, strategy)`, and
//! the minutes each word held are counted per profile. `--regime`
//! follows the backtest's law (default artifact if usable, `off`,
//! or a path); without a detector the section says `blind`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use core_io::{PmlrReader, SlotKind};
use core_types::{
    AiCmd, AiCmdKind, ChannelEvent, ChannelId, Fill, OptSummary, Order, Price, Qty, Side, Tick,
    VenueId, OPT_SUMMARY_FLAG_MARK_PX, REGIME_PROFILES, SYMBOL_ID_NONE,
};

use crate::backtest::fill::{usd_1e12_to_1e6_ceil, usd_1e12_to_1e6_floor};
use crate::backtest::fill::{FillEngine, ModelOutcome, DAY_NS};
use crate::backtest::regime::{
    load_set_regime_frames, profile_name, word_string, RegimeMode, RegimeReplay,
};
use crate::backtest::{
    discover_runs, fmt_usd_1e6, parse_model_params, pmlr_version_accepted, HarnessError,
    ModelParams, RunDir, MIN_PMLR_VERSION, VENUE_LABELS, VIRT_T0,
};
use crate::options_manifest::{INSTRUMENT_MANIFEST_FILE, OPTIONS_MANIFEST_FILE};

/// Report schema version (stdout JSON `audit_pnl_version`). RG3 added
/// the `regime` section ADDITIVELY — every pre-RG3 key is unchanged, so
/// the version stays 1 (the nightly merge reads by key).
pub const AUDIT_PNL_VERSION: u32 = 1;

/// `Order.strategy_id` display names (strategy-set slot order; the
/// wire slots are pinned in core-types / strategy-set).
fn strategy_label(id: u8) -> &'static str {
    match id {
        0 => "latency-arb",
        1 => "ev",
        2 => "cross-arb",
        3 => "rule-tree",
        4 => "ai-exec",
        5 => "vm",
        6 => "icdp",
        0xFF => "unattributed",
        _ => "unknown",
    }
}

/// Subcommand config (bin arm).
#[derive(Debug, Default)]
pub struct AuditPnlConfig {
    /// Replay root (`MULTIVENUE_LOG_DIR`) or one `run-<epoch_ns>` dir.
    pub replay_dir: PathBuf,
    /// Repeatable `--fee-bps <venue>:<maker>:<taker>` overrides.
    pub fee_bps: Vec<String>,
    /// Global `--latency-ns` override.
    pub latency_ns: Option<u64>,
    /// Repeatable `--latency-ns-venue <venue>:<ns>` overrides.
    pub latency_ns_venue: Vec<String>,
    /// VT4: repeatable `--stale-after-ms <venue>:<ms>` overrides.
    pub stale_after_ms: Vec<String>,
    /// RG3: `--regime` (the backtest's law, [`RegimeMode`]).
    pub regime: RegimeMode,
    /// RG3: `--regime-seed <path>` (default = the first run's own
    /// `regime-seed.tsv`, else warm live).
    pub regime_seed: Option<PathBuf>,
}

// ---------------------------------------------------------------
// Event stream
// ---------------------------------------------------------------

/// Event class rank at equal ts (module docs: ticks fill before
/// emits; commits flip before emits; RG3: funding prints and regime
/// declarations land between ticks and fills — the detector sees them
/// before the intents of the same instant are bucketed).
const CLASS_TICK: u8 = 0;
const CLASS_REGIME: u8 = 1;
const CLASS_FILL: u8 = 2;
const CLASS_COMMIT: u8 = 3;
const CLASS_ORDER: u8 = 4;

#[derive(Copy, Clone, Debug)]
enum Payload {
    Tick(Tick),
    Order(Order),
    Fill(Fill),
    Commit([u8; 16]),
    /// RG3: a Funding / AssetCtx print (sym dense) for the detector.
    Funding(ChannelEvent),
    /// RG3: a captured `SetRegime` frame.
    Regime(AiCmd),
}

#[derive(Copy, Clone, Debug)]
struct Ev {
    ts_ns: u64,
    class: u8,
    lord: u8,
    idx: u64,
    payload: Payload,
}

#[derive(Copy, Clone, Debug)]
struct MergedEv {
    virt_ns: u64,
    wall_ns: u64,
    payload: Payload,
}

// ---------------------------------------------------------------
// Descriptor resolution (§6 keying)
// ---------------------------------------------------------------

/// Root-scoped descriptor→dense-sym interner. Dense ids preserve the
/// venue byte; ordinals are per-venue counters (cap 2^24 — beyond any
/// real universe).
struct SymInterner {
    by_desc: BTreeMap<String, u32>,
    desc_by_dense: BTreeMap<u32, String>,
    next_ordinal: [u32; 256],
}

impl Default for SymInterner {
    fn default() -> Self {
        Self {
            by_desc: BTreeMap::new(),
            desc_by_dense: BTreeMap::new(),
            next_ordinal: [0; 256],
        }
    }
}

impl SymInterner {
    fn intern(&mut self, venue_byte: u8, descriptor: &str) -> Result<u32, HarnessError> {
        if let Some(d) = self.by_desc.get(descriptor) {
            return Ok(*d);
        }
        let ord = self.next_ordinal[venue_byte as usize];
        if ord >= 0x00FF_FFFF {
            return Err(HarnessError::Capture(format!(
                "descriptor space overflow on venue byte {venue_byte}"
            )));
        }
        self.next_ordinal[venue_byte as usize] = ord + 1;
        let dense = ((venue_byte as u32) << 24) | (ord + 1);
        self.by_desc.insert(descriptor.to_owned(), dense);
        self.desc_by_dense.insert(dense, descriptor.to_owned());
        Ok(dense)
    }

    fn descriptor(&self, dense: u32) -> &str {
        self.desc_by_dense
            .get(&dense)
            .map(|s| s.as_str())
            .unwrap_or("?")
    }
}

/// One run's sym→descriptor map (docs/wire-format.md manifests).
/// Strict per line; malformed lines counted. `None` = no manifest —
/// the caller namespaces the run's syms per run (§6 conservative arm).
fn read_run_manifest(dir: &Path) -> (Option<BTreeMap<u32, String>>, u64) {
    let mut malformed = 0u64;
    let inst = dir.join(INSTRUMENT_MANIFEST_FILE);
    if let Ok(text) = std::fs::read_to_string(&inst) {
        let mut out = BTreeMap::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let mut it = line.split('\t');
            let (Some(sym_s), Some(desc), None) = (it.next(), it.next(), it.next()) else {
                malformed += 1;
                continue;
            };
            let Ok(sym) = sym_s.parse::<u32>() else {
                malformed += 1;
                continue;
            };
            if desc.is_empty() || sym == 0 {
                malformed += 1;
                continue;
            }
            out.insert(sym, desc.to_owned());
        }
        return (Some(out), malformed);
    }
    let opts = dir.join(OPTIONS_MANIFEST_FILE);
    if let Ok(text) = std::fs::read_to_string(&opts) {
        let mut out = BTreeMap::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let mut it = line.split('\t');
            let (Some(label), Some(sym_s), Some(name), None) =
                (it.next(), it.next(), it.next(), it.next())
            else {
                malformed += 1;
                continue;
            };
            let prefix = match label {
                "deribit" => "deribit:",
                "okx" => "okx:",
                "bn" => "binance-opt:",
                _ => {
                    malformed += 1;
                    continue;
                }
            };
            let Ok(sym) = sym_s.parse::<u32>() else {
                malformed += 1;
                continue;
            };
            if name.is_empty() || sym == 0 {
                malformed += 1;
                continue;
            }
            out.insert(sym, format!("{prefix}{name}"));
        }
        return (Some(out), malformed);
    }
    (None, 0)
}

// ---------------------------------------------------------------
// Load + merge (harness §3.2/§3.3, extended to the event classes)
// ---------------------------------------------------------------

fn open_checked<R: core_types::AsBytes>(
    path: &Path,
    want: SlotKind,
    epoch_ns: u64,
) -> Result<Option<PmlrReader<R>>, HarnessError> {
    if !path.is_file() {
        return Ok(None);
    }
    let reader = PmlrReader::<R>::open(path)
        .map_err(|e: io::Error| HarnessError::Capture(format!("{}: {e}", path.display())))?;
    if reader.slot_kind() != want {
        return Err(HarnessError::Capture(format!(
            "{}: slot_kind {:?} is not {:?}",
            path.display(),
            reader.slot_kind(),
            want
        )));
    }
    if !pmlr_version_accepted(reader.version()) {
        return Err(HarnessError::Capture(format!(
            "{}: PMLR v{} — audit-pnl accepts v{}..=v{}",
            path.display(),
            reader.version(),
            MIN_PMLR_VERSION,
            core_io::VERSION
        )));
    }
    if reader.epoch_ns() != epoch_ns {
        return Err(HarnessError::Capture(format!(
            "{}: header epoch_ns {} != directory epoch_ns {epoch_ns} (§3.1 cross-check)",
            path.display(),
            reader.epoch_ns()
        )));
    }
    Ok(Some(reader))
}

/// Per-run load stats (stderr surface).
#[derive(Clone, Debug, Default)]
struct RunLoad {
    epoch_ns: u64,
    ticks: u64,
    orders: u64,
    fills: u64,
    commits: u64,
    manifest: bool,
    manifest_malformed: u64,
    unresolved_namespaced: u64,
    clamped_pre_anchor: u64,
    /// VM2 V5 (D-7): synthetic option mark-ticks synthesized from
    /// `<venue>-opt-summary.pmlr` for option syms without a tick
    /// lane; these syms execute under the mark-fill law.
    opt_synth_ticks: u64,
    /// VT4: per-lane stale accounting (the harness re-judge).
    stale: [crate::backtest::stale::StaleStats; VENUE_LABELS.len()],
    /// RG3: funding prints loaded, `SetRegime` frames loaded (clamped
    /// ones included) and dropped as expired at the run's tick anchor.
    funding_events: u64,
    regime_cmds: u64,
    regime_cmds_dropped: u64,
}

/// Load one run: every §9.9 input, syms rewritten to root-dense ids,
/// §3.2-ordered.
fn load_run_events(
    run: &RunDir,
    interner: &mut SymInterner,
    mark_fill_syms: &mut std::collections::BTreeSet<u32>,
    stale_after_ms: [u32; 7],
) -> Result<(Vec<Ev>, RunLoad), HarnessError> {
    let mut load = RunLoad {
        epoch_ns: run.epoch_ns,
        ..RunLoad::default()
    };
    // VT4: one re-judge per run (connections and their clock offsets
    // are per run); a threshold change is a replay, never a recapture.
    let mut judge = crate::backtest::stale::StaleJudge::new(stale_after_ms);
    let (manifest, malformed) = read_run_manifest(&run.path);
    load.manifest = manifest.is_some();
    load.manifest_malformed = malformed;

    let resolve =
        |sym: u32, interner: &mut SymInterner, load: &mut RunLoad| -> Result<u32, HarnessError> {
            // Model venue byte: the M1 anchor id 7 (`binance:btcusdt`)
            // interns under Binance so the Δ / fee lookup keyed on the
            // dense id's venue byte is Binance's, not Polymarket's.
            let venue_byte = crate::backtest::fill::model_venue_byte(sym);
            match manifest.as_ref().and_then(|m| m.get(&sym)) {
                Some(desc) => interner.intern(venue_byte, desc),
                None => {
                    load.unresolved_namespaced += 1;
                    let ns = format!("run-{}/sym-{:#010x}", run.epoch_ns, sym);
                    interner.intern(venue_byte, &ns)
                }
            }
        };

    let mut evs: Vec<Ev> = Vec::new();

    // Ticks (per-venue files, harness acceptance law).
    for (lord, label) in VENUE_LABELS.iter().enumerate() {
        let path = run.path.join(format!("{label}-ticks.pmlr"));
        let Some(reader) = open_checked::<Tick>(&path, SlotKind::Tick, run.epoch_ns)? else {
            continue;
        };
        let has_venue_time = reader.has_venue_time();
        for (i, t) in reader.records().iter().enumerate() {
            if t.sym == SYMBOL_ID_NONE {
                continue;
            }
            let mut tick = *t;
            // Judged in FILE order on the RAW sym (the estimator is per
            // connection; interning is a naming concern).
            judge.judge(lord, &mut tick, has_venue_time);
            tick.sym = resolve(t.sym, interner, &mut load)?;
            evs.push(Ev {
                ts_ns: t.ts_ns,
                class: CLASS_TICK,
                lord: lord as u8,
                idx: i as u64,
                payload: Payload::Tick(tick),
            });
            load.ticks += 1;
        }
    }
    // VM2 V5 (D-7): option syms with a MARK but no tick lane get a
    // synthetic zero-spread mark tick per OptSummary record — they
    // anchor, mark and (mark-law) fill like any instrument, valued
    // at mark. okx's markless summaries stay honestly unpriceable.
    let mut tick_syms: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    for e in &evs {
        if let Payload::Tick(t) = &e.payload {
            tick_syms.insert(t.sym);
        }
    }
    for (lord_off, label) in VENUE_LABELS.iter().enumerate() {
        let path = run.path.join(format!("{label}-opt-summary.pmlr"));
        let Some(reader) = open_checked::<OptSummary>(&path, SlotKind::OptSummary, run.epoch_ns)?
        else {
            continue;
        };
        for (i, o) in reader.records().iter().enumerate() {
            if o.flags & OPT_SUMMARY_FLAG_MARK_PX == 0 || o.mark_px_1e9 <= 0 {
                continue;
            }
            let dense = resolve(o.sym, interner, &mut load)?;
            if tick_syms.contains(&dense) {
                continue;
            }
            let mark_1e6 = o.mark_px_1e9 / 1_000;
            if mark_1e6 <= 0 {
                continue;
            }
            mark_fill_syms.insert(dense);
            let venue = VenueId::from_u8(o.venue).unwrap_or(VenueId::Deribit);
            let t = Tick::new(
                o.ts_ns,
                venue,
                dense,
                0,
                Price::from_raw(mark_1e6),
                Qty::from_raw(1_000_000_000_000),
                Price::from_raw(mark_1e6),
                Qty::from_raw(1_000_000_000_000),
            );
            evs.push(Ev {
                ts_ns: o.ts_ns,
                class: CLASS_TICK,
                lord: 200 + lord_off as u8,
                idx: i as u64,
                payload: Payload::Tick(t),
            });
            load.opt_synth_ticks += 1;
            load.ticks += 1;
        }
    }
    load.stale = judge.stats;
    if load.ticks == 0 {
        // No tick anchor: nothing can mark or fill — the run
        // contributes nothing (reported, not fatal).
        return Ok((Vec::new(), load));
    }

    // Order intents (M4.1; absent on pre-M4.1 runs).
    let orders_path = run.path.join("engine-orders.pmlr");
    if let Some(reader) = open_checked::<Order>(&orders_path, SlotKind::Order, run.epoch_ns)? {
        for (i, o) in reader.records().iter().enumerate() {
            let mut order = *o;
            order.sym = resolve(o.sym, interner, &mut load)?;
            evs.push(Ev {
                ts_ns: o.ts_ns,
                class: CLASS_ORDER,
                lord: 255,
                idx: i as u64,
                payload: Payload::Order(order),
            });
            load.orders += 1;
        }
    }

    // Paper/venue fills (Phase 8f file; header-only in paper mode).
    let fills_path = run.path.join("engine-fills.pmlr");
    if let Some(reader) = open_checked::<Fill>(&fills_path, SlotKind::Fill, run.epoch_ns)? {
        for (i, f) in reader.records().iter().enumerate() {
            let mut fl = *f;
            fl.sym = resolve(f.sym, interner, &mut load)?;
            evs.push(Ev {
                ts_ns: f.ts_ns,
                class: CLASS_FILL,
                lord: 254,
                idx: i as u64,
                payload: Payload::Fill(fl),
            });
            load.fills += 1;
        }
    }

    // Ruleset-commit timeline (8f ai-cmds capture; kind 8 only).
    let ai_path = run.path.join("ai-cmds.pmlr");
    if let Some(reader) = open_checked::<AiCmd>(&ai_path, SlotKind::AiCmd, run.epoch_ns)? {
        for (i, c) in reader.records().iter().enumerate() {
            if c.kind() != Some(AiCmdKind::RulesetCommit) {
                continue;
            }
            evs.push(Ev {
                ts_ns: c.ts_ns,
                class: CLASS_COMMIT,
                lord: 253,
                idx: i as u64,
                payload: Payload::Commit(c.ruleset_hash128()),
            });
            load.commits += 1;
        }
    }

    // RG3: the regime detector's inputs — the anchor law stays "the
    // run's first TICK": a funding print or a declaration stamped
    // before it is clamped to it (a declaration keeps its remaining
    // TTL; the backtest's loader applies the same clamp).
    let tick_anchor = evs
        .iter()
        .filter(|e| e.class == CLASS_TICK)
        .map(|e| e.ts_ns)
        .min()
        .unwrap_or(0);
    for (lord_off, label) in VENUE_LABELS.iter().enumerate() {
        let path = run.path.join(format!("{label}-events.pmlr"));
        let Some(reader) = open_checked::<ChannelEvent>(&path, SlotKind::Event, run.epoch_ns)?
        else {
            continue;
        };
        for (i, e) in reader.records().iter().enumerate() {
            let keep =
                e.channel == ChannelId::Funding as u8 || e.channel == ChannelId::AssetCtx as u8;
            if !keep || e.sym == SYMBOL_ID_NONE {
                continue;
            }
            let mut ev = *e;
            ev.sym = resolve(e.sym, interner, &mut load)?;
            if ev.ts_ns < tick_anchor {
                ev.ts_ns = tick_anchor;
                load.clamped_pre_anchor += 1;
            }
            evs.push(Ev {
                ts_ns: ev.ts_ns,
                class: CLASS_REGIME,
                lord: 100 + lord_off as u8,
                idx: i as u64,
                payload: Payload::Funding(ev),
            });
            load.funding_events += 1;
        }
    }
    let (frames, dropped) = load_set_regime_frames(&run.path, run.epoch_ns, tick_anchor)?;
    load.regime_cmds = frames.len() as u64;
    load.regime_cmds_dropped = dropped;
    for (i, c) in frames.into_iter().enumerate() {
        evs.push(Ev {
            ts_ns: c.ts_ns,
            class: CLASS_REGIME,
            lord: 252,
            idx: i as u64,
            payload: Payload::Regime(c),
        });
    }

    // §3.2 total order, extended: (ts, class, lord, idx) — unique by
    // construction, so sort_unstable stays deterministic.
    evs.sort_unstable_by_key(|e| (e.ts_ns, e.class, e.lord, e.idx));
    Ok((evs, load))
}

/// Load every run, rebase per §3.3 (anchor = the run's min tick ts —
/// evs[0] is a tick by construction: CLASS_TICK sorts first at the
/// minimum ts), refuse wall-overlapping runs, concatenate.
fn load_and_merge_events(
    runs: &[RunDir],
    interner: &mut SymInterner,
    mark_fill_syms: &mut std::collections::BTreeSet<u32>,
    stale_after_ms: [u32; 7],
) -> Result<(Vec<MergedEv>, Vec<RunLoad>), HarnessError> {
    let epoch_0 = runs[0].epoch_ns;
    let mut merged: Vec<MergedEv> = Vec::new();
    let mut loads: Vec<RunLoad> = Vec::with_capacity(runs.len());
    let mut prev_last_virt: u64 = 0;
    for run in runs {
        let (evs, mut load) = load_run_events(run, interner, mark_fill_syms, stale_after_ms)?;
        if evs.is_empty() {
            loads.push(load);
            continue;
        }
        let base = VIRT_T0 + (run.epoch_ns - epoch_0);
        if base < prev_last_virt {
            return Err(HarnessError::Capture(format!(
                "run-{} overlaps the previous run on the virtual timeline \
                 (base {} < previous last {}) — overlapping captures are untrustworthy",
                run.epoch_ns, base, prev_last_virt
            )));
        }
        debug_assert!(matches!(evs[0].payload, Payload::Tick(_)));
        let ts_first = evs[0].ts_ns;
        merged.reserve(evs.len());
        for e in &evs {
            // Defensive: an order/fill logged before the first tick
            // clamps to the anchor (counted; cannot occur in real
            // capture — strategies act on ticks).
            let delta = e.ts_ns.checked_sub(ts_first).unwrap_or_else(|| {
                load.clamped_pre_anchor += 1;
                0
            });
            let mut payload = e.payload;
            match payload {
                Payload::Tick(ref mut t) => t.ts_ns = base + delta,
                Payload::Funding(ref mut f) => f.ts_ns = base + delta,
                Payload::Regime(ref mut c) => c.ts_ns = base + delta,
                _ => {}
            }
            merged.push(MergedEv {
                virt_ns: base + delta,
                wall_ns: run.epoch_ns + delta,
                payload,
            });
        }
        prev_last_virt = merged[merged.len() - 1].virt_ns;
        loads.push(load);
    }
    if merged.is_empty() {
        return Err(HarnessError::Capture(
            "no replayable events — every run is tick-less or the root is empty".to_owned(),
        ));
    }
    Ok((merged, loads))
}

// ---------------------------------------------------------------
// Replay + report
// ---------------------------------------------------------------

/// One attribution row (per strategy, or per vm ruleset hash).
struct KeyRow {
    label: String,
    outcome: ModelOutcome,
    per_day_net_1e6: Vec<(u64, i64)>,
    per_sym: Vec<(String, i64, i128, u64)>, // descriptor, pos, realized, fills
}

/// Run the whole report. Returns the stdout JSON line; human summary
/// goes through `report` (stderr).
pub fn run(cfg: &AuditPnlConfig, report: &mut dyn FnMut(&str)) -> Result<String, HarnessError> {
    let params: ModelParams = parse_model_params(
        &cfg.fee_bps,
        cfg.latency_ns,
        &cfg.latency_ns_venue,
        &cfg.stale_after_ms,
    )?;
    let runs = discover_runs(&cfg.replay_dir)?;
    let mut interner = SymInterner::default();
    let mut mark_fill_syms: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let (merged, loads) =
        load_and_merge_events(&runs, &mut interner, &mut mark_fill_syms, params.stale_after_ms)?;

    for l in &loads {
        report(&format!(
            "audit-pnl: run-{}: ticks={} orders={} fills={} commits={} manifest={}{}{}{}",
            l.epoch_ns,
            l.ticks,
            l.orders,
            l.fills,
            l.commits,
            if l.manifest {
                "yes"
            } else {
                "NO (per-run namespace)"
            },
            if l.manifest_malformed > 0 {
                format!(" malformed={}", l.manifest_malformed)
            } else {
                String::new()
            },
            if l.unresolved_namespaced > 0 {
                format!(" unresolved-namespaced={}", l.unresolved_namespaced)
            } else {
                String::new()
            },
            if l.clamped_pre_anchor > 0 {
                format!(" clamped-pre-anchor={}", l.clamped_pre_anchor)
            } else {
                String::new()
            },
        ));
        if l.opt_synth_ticks > 0 {
            report(&format!(
                "audit-pnl: run-{}: opt-synth-ticks={} (D-7 mark books)",
                l.epoch_ns, l.opt_synth_ticks
            ));
        }
        // VT4: the per-lane stale verdict of the re-judge — a stale
        // tick neither fills nor marks; v2 lanes are stale-blind.
        report(&format!(
            "audit-pnl: run-{}:{}",
            l.epoch_ns,
            crate::backtest::render_stale_line(&l.stale)
        ));
    }
    if !mark_fill_syms.is_empty() {
        // D-7 obligation: the assumption is PRINTED wherever it can
        // shape numbers.
        report(&format!(
            "audit-pnl: OPTIONS MARK-FILL LAW (D-7): {} option sym(s) execute at              mark ± max(0.5%, 1 tick) with TAKER fees and value at mark — no real              options book exists in the capture; assumption applies wherever these              syms filled",
            mark_fill_syms.len()
        ));
    }

    // Engines: per strategy_id, plus per (vm, hash128). ModelParams
    // fields are Copy arrays — rebuild per engine (derive-agnostic).
    let mk_engine = || {
        let mut e = FillEngine::new(
            ModelParams {
                fee_bps: params.fee_bps,
                latency_ns: params.latency_ns,
                stale_after_ms: params.stale_after_ms,
            },
            0,
        );
        // D-7: every engine executes registered option syms under
        // the mark-fill law.
        for sym in &mark_fill_syms {
            e.set_mark_fill_sym(*sym);
        }
        e
    };
    let mut engines: BTreeMap<u8, FillEngine> = BTreeMap::new();
    let mut vm_hash_engines: BTreeMap<[u8; 16], FillEngine> = BTreeMap::new();
    let mut active_hash: Option<[u8; 16]> = None;
    let mut vm_orders_no_hash: u64 = 0;

    // RG3: the detector over the merged stream (descriptors resolve
    // through the interner — a member the root never observed is
    // dropped, as the backtest does) + one independent replay per
    // (profile, effective word at emit, strategy).
    let mut regime = {
        let resolve = |d: &str| interner.by_desc.get(d).copied();
        let default_seed = runs[0].path.join("regime-seed.tsv");
        RegimeReplay::build(
            &cfg.regime,
            cfg.regime_seed.as_deref(),
            Some(&default_seed),
            &resolve,
            merged[0].virt_ns,
            merged[0].wall_ns,
            report,
        )?
    };
    let mut regime_engines: BTreeMap<(u8, u64, u8), FillEngine> = BTreeMap::new();
    let mut regime_orders: BTreeMap<(u8, u64, u8), u64> = BTreeMap::new();

    // Paper view: signed cash flow + positions at last mids, no fees.
    let mut paper_qty: BTreeMap<u32, i64> = BTreeMap::new();
    let mut paper_cash_1e12: i128 = 0;
    let mut paper_fills: u64 = 0;
    let mut marks_1e6: BTreeMap<u32, i64> = BTreeMap::new();

    // Per-UTC-day equity snapshots per strategy engine.
    let mut day_equity: BTreeMap<u8, Vec<(u64, i128)>> = BTreeMap::new();
    let mut cur_day: Option<u64> = None;

    let wall_first = merged[0].wall_ns;
    let wall_last = merged[merged.len() - 1].wall_ns;
    let mut scratch = Vec::new();

    for ev in &merged {
        let day = ev.wall_ns / DAY_NS;
        if let Some(prev) = cur_day {
            if day != prev {
                for (sid, eng) in &engines {
                    day_equity
                        .entry(*sid)
                        .or_default()
                        .push((prev, eng.oos_equity_1e12()));
                }
            }
        }
        cur_day = Some(day);

        if let Some(rg) = regime.as_mut() {
            let _ = rg.on_time(ev.virt_ns);
        }
        match &ev.payload {
            Payload::Tick(t) => {
                if let Some(rg) = regime.as_mut() {
                    rg.on_tick(t);
                }
                if t.bid_px.raw() > 0 && t.ask_px.raw() > 0 {
                    marks_1e6.insert(t.sym, t.mid().raw());
                }
                for eng in engines.values_mut() {
                    eng.on_record(t, ev.virt_ns, ev.wall_ns, &mut scratch);
                }
                for eng in vm_hash_engines.values_mut() {
                    eng.on_record(t, ev.virt_ns, ev.wall_ns, &mut scratch);
                }
                for eng in regime_engines.values_mut() {
                    eng.on_record(t, ev.virt_ns, ev.wall_ns, &mut scratch);
                }
            }
            Payload::Funding(f) => {
                if let Some(rg) = regime.as_mut() {
                    rg.on_event(f);
                }
            }
            Payload::Regime(c) => {
                if let Some(rg) = regime.as_mut() {
                    let _ = rg.on_set_regime(c, ev.virt_ns);
                }
            }
            Payload::Order(o) => {
                engines
                    .entry(o.strategy_id)
                    .or_insert_with(mk_engine)
                    .intake(o, ev.virt_ns);
                if o.strategy_id == 5 {
                    match active_hash {
                        Some(h) => {
                            vm_hash_engines
                                .entry(h)
                                .or_insert_with(mk_engine)
                                .intake(o, ev.virt_ns);
                        }
                        None => vm_orders_no_hash += 1,
                    }
                }
                if let Some(rg) = regime.as_ref() {
                    let mut p = 0u8;
                    while (p as usize) < REGIME_PROFILES {
                        let key = (p, rg.effective(p).0, o.strategy_id);
                        regime_engines
                            .entry(key)
                            .or_insert_with(mk_engine)
                            .intake(o, ev.virt_ns);
                        *regime_orders.entry(key).or_insert(0) += 1;
                        p += 1;
                    }
                }
            }
            Payload::Fill(f) => {
                paper_fills += 1;
                let flow = f.px.raw() as i128 * f.qty.raw() as i128;
                match f.side {
                    Side::Bid => {
                        paper_cash_1e12 -= flow;
                        *paper_qty.entry(f.sym).or_default() += f.qty.raw();
                    }
                    Side::Ask => {
                        paper_cash_1e12 += flow;
                        *paper_qty.entry(f.sym).or_default() -= f.qty.raw();
                    }
                }
            }
            Payload::Commit(h) => {
                active_hash = Some(*h);
            }
        }
    }
    // Final day snapshot.
    if let Some(prev) = cur_day {
        for (sid, eng) in &engines {
            day_equity
                .entry(*sid)
                .or_default()
                .push((prev, eng.oos_equity_1e12()));
        }
    }

    // Paper mark-out.
    let mut paper_mark_value_1e12: i128 = 0;
    for (sym, qty) in &paper_qty {
        if *qty != 0 {
            let mark = *marks_1e6.get(sym).unwrap_or(&0);
            paper_mark_value_1e12 += *qty as i128 * mark as i128;
        }
    }
    let paper_net_1e12 = paper_cash_1e12 + paper_mark_value_1e12;

    // Rows.
    let day0 = wall_first / DAY_NS;
    let mut rows: Vec<(u8, KeyRow)> = Vec::new();
    let engine_ids: Vec<u8> = engines.keys().copied().collect();
    for sid in engine_ids {
        let eng = engines.get_mut(&sid).expect("keyed");
        let per_sym: Vec<(String, i64, i128, u64)> = eng
            .per_sym_detail()
            .iter()
            .map(|d| {
                (
                    interner.descriptor(d.sym).to_owned(),
                    d.pos_qty_1e6,
                    d.realized_1e12,
                    d.fills,
                )
            })
            .collect();
        let outcome = eng.finish();
        // Day buckets: equity deltas between consecutive snapshots.
        let mut per_day: Vec<(u64, i64)> = Vec::new();
        let mut prev_eq: i128 = 0;
        if let Some(snaps) = day_equity.get(&sid) {
            for (day, eq) in snaps {
                per_day.push((*day - day0, usd_1e12_to_1e6_floor(*eq - prev_eq)));
                prev_eq = *eq;
            }
        }
        rows.push((
            sid,
            KeyRow {
                label: strategy_label(sid).to_owned(),
                outcome,
                per_day_net_1e6: per_day,
                per_sym,
            },
        ));
    }
    let mut vm_rows: Vec<(String, ModelOutcome)> = Vec::new();
    let hashes: Vec<[u8; 16]> = vm_hash_engines.keys().copied().collect();
    for h in hashes {
        let eng = vm_hash_engines.get_mut(&h).expect("keyed");
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        vm_rows.push((hex, eng.finish()));
    }
    // RG3: per (profile, word, strategy) rows — BTree order = profile,
    // word bits, strategy id (deterministic).
    let mut regime_rows: Vec<((u8, u64, u8), u64, ModelOutcome)> = Vec::new();
    let regime_keys: Vec<(u8, u64, u8)> = regime_engines.keys().copied().collect();
    for k in regime_keys {
        let eng = regime_engines.get_mut(&k).expect("keyed");
        let orders = *regime_orders.get(&k).unwrap_or(&0);
        regime_rows.push((k, orders, eng.finish()));
    }
    let regime_minutes: Vec<((u8, u64), u64)> = regime
        .as_ref()
        .map(|r| r.minutes_by_word.iter().map(|(k, v)| (*k, *v)).collect())
        .unwrap_or_default();
    let regime_mode = if cfg.regime.is_off() {
        "off"
    } else if regime.is_some() {
        "artifact"
    } else {
        "blind"
    };

    // ---- human summary (stderr) ----
    let utc_days = (wall_last / DAY_NS) - day0 + 1;
    report(&format!(
        "audit-pnl: window wall=[{wall_first}, {wall_last}] utc_days={utc_days} runs={} \
         model: latency_ns pm={} bn={} okx={} deribit={} hl={}",
        loads.len(),
        params.latency_ns[0],
        params.latency_ns[1],
        params.latency_ns[2],
        params.latency_ns[3],
        params.latency_ns[4],
    ));
    report(&format!(
        "audit-pnl: paper view: fills={paper_fills} net={} (cash={} markout={}; fees none in paper)",
        fmt_usd_1e6(usd_1e12_to_1e6_floor(paper_net_1e12)),
        fmt_usd_1e6(usd_1e12_to_1e6_floor(paper_cash_1e12)),
        fmt_usd_1e6(usd_1e12_to_1e6_floor(paper_mark_value_1e12)),
    ));
    for (sid, row) in &rows {
        let o = &row.outcome;
        report(&format!(
            "audit-pnl: strategy {sid} ({}): orders={} fills={} trades={} days={} net={} \
             (realized={} fees={} markout={}) max_dd={} canceled_end={} caps_rejected={} unroutable={}",
            row.label,
            o.orders_is + o.orders_oos,
            o.fills_total,
            o.oos_trades,
            o.oos_trading_days,
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_realized_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_ceil(o.oos_fees_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_unreal_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_ceil(o.oos_max_dd_1e12)),
            o.canceled_end,
            o.rejected_sym_cap + o.rejected_total_cap,
            o.unroutable,
        ));
        // I1: taker surface + the §4.3 fee ladder — printed for EVERY
        // strategy so a number positive only at 0 bps is visible as such.
        report(&format!(
            "audit-pnl:   ioc_fills={} ioc_canceled={} ttl_expired={} | fee ladder (net, flat \
             bps/side): 0={} 1={} 2={} tier={}",
            o.ioc_fills,
            o.ioc_canceled,
            o.ttl_expired,
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[0])),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[1])),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[2])),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_1e12)),
        ));
        for (desc, pos, realized, fills) in &row.per_sym {
            report(&format!(
                "audit-pnl:   {desc}: fills={fills} pos={} realized={}",
                pos,
                fmt_usd_1e6(usd_1e12_to_1e6_floor(*realized)),
            ));
        }
    }
    for (hex, o) in &vm_rows {
        report(&format!(
            "audit-pnl: vm ruleset {hex}: orders={} trades={} net={} max_dd={}",
            o.orders_is + o.orders_oos,
            o.oos_trades,
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_ceil(o.oos_max_dd_1e12)),
        ));
    }
    if vm_orders_no_hash > 0 {
        report(&format!(
            "audit-pnl: NOTE {vm_orders_no_hash} vm order(s) before any RulesetCommit in-window \
             — counted in the slot-5 aggregate only"
        ));
    }
    // RG3: the regime section — minutes per word per profile, then one
    // line per (profile, word, strategy) with the same fee ladder.
    report(&format!(
        "audit-pnl: regime mode={regime_mode} minutes_judged={} declared={} funding_events={} \
         set_regime_frames={} (expired-at-anchor {})",
        regime.as_ref().map(|r| r.minutes_judged()).unwrap_or(0),
        regime.as_ref().map(|r| r.declared_applied).unwrap_or(0),
        loads.iter().map(|l| l.funding_events).sum::<u64>(),
        loads.iter().map(|l| l.regime_cmds).sum::<u64>(),
        loads.iter().map(|l| l.regime_cmds_dropped).sum::<u64>(),
    ));
    for ((p, w), m) in &regime_minutes {
        report(&format!(
            "audit-pnl: regime {} [{}] minutes={m}",
            profile_name(*p),
            word_string(core_types::RegimeWord(*w))
        ));
    }
    for ((p, w, sid), orders, o) in &regime_rows {
        report(&format!(
            "audit-pnl: regime {} [{}] strategy {sid} ({}): orders={orders} fills={} trades={} \
             net={} | ladder 0={} 1={} 2={}",
            profile_name(*p),
            word_string(core_types::RegimeWord(*w)),
            strategy_label(*sid),
            o.fills_total,
            o.oos_trades,
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[0])),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[1])),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[2])),
        ));
    }

    // ---- stdout JSON (one line, hand-rendered, deterministic) ----
    let mut json = String::new();
    json.push_str(&format!(
        "{{\"audit_pnl_version\":{AUDIT_PNL_VERSION},\"runs\":{},\"window\":{{\"wall_first_ns\":{wall_first},\
         \"wall_last_ns\":{wall_last},\"utc_days\":{utc_days}}},\"paper\":{{\"fills\":{paper_fills},\
         \"net_usd\":\"{}\"}},\"strategies\":[",
        loads.len(),
        fmt_usd_1e6(usd_1e12_to_1e6_floor(paper_net_1e12)),
    ));
    for (i, (sid, row)) in rows.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        let o = &row.outcome;
        json.push_str(&format!(
            "{{\"strategy_id\":{sid},\"label\":\"{}\",\"orders\":{},\"fills\":{},\"trades\":{},\
             \"trading_days\":{},\"net_usd\":\"{}\",\"realized_usd\":\"{}\",\"fees_usd\":\"{}\",\
             \"markout_usd\":\"{}\",\"max_drawdown_usd\":\"{}\",\"canceled_end\":{},\
             \"rejected_caps\":{},\"unroutable\":{},\"ioc_fills\":{},\"ioc_canceled\":{},\
             \"ttl_expired\":{},\"fee_ladder_net_usd\":[\"{}\",\"{}\",\"{}\"],\
             \"per_day_net_usd\":[",
            row.label,
            o.orders_is + o.orders_oos,
            o.fills_total,
            o.oos_trades,
            o.oos_trading_days,
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_realized_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_ceil(o.oos_fees_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_unreal_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_ceil(o.oos_max_dd_1e12)),
            o.canceled_end,
            o.rejected_sym_cap + o.rejected_total_cap,
            o.unroutable,
            o.ioc_fills,
            o.ioc_canceled,
            o.ttl_expired,
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[0])),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[1])),
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[2])),
        ));
        for (j, (day_idx, net)) in row.per_day_net_1e6.iter().enumerate() {
            if j > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                "{{\"day\":{day_idx},\"net_usd\":\"{}\"}}",
                fmt_usd_1e6(*net)
            ));
        }
        json.push_str("]}");
    }
    json.push_str("],\"vm_by_ruleset\":[");
    for (i, (hex, o)) in vm_rows.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"hash128\":\"{hex}\",\"orders\":{},\"trades\":{},\"net_usd\":\"{}\",\
             \"max_drawdown_usd\":\"{}\"}}",
            o.orders_is + o.orders_oos,
            o.oos_trades,
            fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_1e12)),
            fmt_usd_1e6(usd_1e12_to_1e6_ceil(o.oos_max_dd_1e12)),
        ));
    }
    json.push_str(&format!("],\"vm_orders_no_hash\":{vm_orders_no_hash}"));
    // RG3: the additive `regime` section.
    let artifact_hex: String = regime
        .as_ref()
        .map(|r| r.hash.iter().map(|b| format!("{b:02x}")).collect())
        .unwrap_or_default();
    json.push_str(&format!(
        ",\"regime\":{{\"mode\":\"{regime_mode}\",\"artifact_sha256\":\"{artifact_hex}\",\
         \"seed_rows\":{},\"minutes_judged\":{},\"declared_applied\":{},\"funding_events\":{},\
         \"set_regime_frames\":{},\"set_regime_expired\":{},\"profiles\":[",
        regime.as_ref().map(|r| r.seed_rows).unwrap_or(0),
        regime.as_ref().map(|r| r.minutes_judged()).unwrap_or(0),
        regime.as_ref().map(|r| r.declared_applied).unwrap_or(0),
        loads.iter().map(|l| l.funding_events).sum::<u64>(),
        loads.iter().map(|l| l.regime_cmds).sum::<u64>(),
        loads.iter().map(|l| l.regime_cmds_dropped).sum::<u64>(),
    ));
    let mut p = 0u8;
    while (p as usize) < REGIME_PROFILES {
        if p > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"profile\":\"{}\",\"words\":[",
            profile_name(p)
        ));
        // Every word seen on this profile: judged minutes ∪ emit words.
        let mut words: Vec<u64> = regime_minutes
            .iter()
            .filter(|((pp, _), _)| *pp == p)
            .map(|((_, w), _)| *w)
            .chain(
                regime_rows
                    .iter()
                    .filter(|((pp, _, _), _, _)| *pp == p)
                    .map(|((_, w, _), _, _)| *w),
            )
            .collect();
        words.sort_unstable();
        words.dedup();
        for (wi, w) in words.iter().enumerate() {
            if wi > 0 {
                json.push(',');
            }
            let minutes = regime_minutes
                .iter()
                .find(|((pp, ww), _)| *pp == p && ww == w)
                .map(|(_, m)| *m)
                .unwrap_or(0);
            json.push_str(&format!(
                "{{\"word\":\"{}\",\"bits\":\"{w:016x}\",\"minutes\":{minutes},\"strategies\":[",
                word_string(core_types::RegimeWord(*w))
            ));
            let mut first = true;
            for ((pp, ww, sid), orders, o) in &regime_rows {
                if *pp != p || ww != w {
                    continue;
                }
                if !first {
                    json.push(',');
                }
                first = false;
                json.push_str(&format!(
                    "{{\"strategy_id\":{sid},\"label\":\"{}\",\"orders\":{orders},\"fills\":{},\
                     \"trades\":{},\"net_usd\":\"{}\",\"fee_ladder_net_usd\":[\"{}\",\"{}\",\"{}\"]}}",
                    strategy_label(*sid),
                    o.fills_total,
                    o.oos_trades,
                    fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_1e12)),
                    fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[0])),
                    fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[1])),
                    fmt_usd_1e6(usd_1e12_to_1e6_floor(o.oos_net_ladder_1e12[2])),
                ));
            }
            json.push_str("]}");
        }
        json.push_str("]}");
        p += 1;
    }
    json.push_str("]}}");
    Ok(json)
}
