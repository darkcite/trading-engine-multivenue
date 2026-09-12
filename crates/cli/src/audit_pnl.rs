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

/// VX-A: one underlying observation awaiting a wall stamp. The rebase
/// `wall = run.epoch_ns + (raw_ts − ts_first)` needs the run's first
/// tick, which is only known once the run's events are sorted, so the
/// observations are collected first and resolved after.
#[derive(Copy, Clone, Debug)]
struct OptSettleCand {
    sym: u32,
    raw_ts_ns: u64,
    index_1e6: i64,
}

use std::path::{Path, PathBuf};

use core_io::{PmlrReader, SlotKind};
use core_types::{
    AiCmd, AiCmdKind, ChannelEvent, ChannelId, Fill, OptSummary, Order, Price, Qty, Side, Tick,
    VenueId, REGIME_PROFILES, SYMBOL_ID_NONE,
};

use crate::backtest::fill::{usd_1e12_to_1e6_ceil, usd_1e12_to_1e6_floor};
// VRP P2.1: the option model lives in ONE module; this surface
// shares its settlement law rather than carrying a second copy.
use crate::backtest::opt::{OptSettleRef, OptTerms};
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
///
/// **Slot 1 changed meaning on 2026-09-10 (VRP V7).** The slot NUMBER is
/// wire-stable, but the member behind it went from `strategy-ev` to
/// `strategy-vrp`, so rows in a capture taken BEFORE that date are EV
/// rows wearing this label. `docs/migration.md` records the boundary;
/// there is no way to tell from the row itself, which is exactly why the
/// boundary is written down.
///
/// **Slot 2 changed meaning on 2026-09-12 (XSD-S).** `strategy-cross-arb`
/// was unlinked and the slot is held for `strategy-xsd`: rows under slot
/// 2 in a capture taken BEFORE that date are cross-arb rows wearing this
/// label; between XSD-S and the XSD-3 wiring the slot emits nothing.
fn strategy_label(id: u8) -> &'static str {
    match id {
        0 => "latency-arb",
        1 => "vrp",
        2 => "xsd",
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
    /// VRP V2b: repeatable `--opt-fee <venue>:<index_bps>:<prem_bps>`
    /// (or `<venue>:off`).
    pub opt_fee: Vec<String>,
    /// VRP V3: `--option-spread-frac <ppm>` — the ASSUMED crossed
    /// option spread as parts-per-million of premium. `None` = 0 =
    /// the D-7 floor alone.
    pub option_spread_frac_1e6: Option<u32>,
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

/// VRP P2.1/P2.2: the option model's out-params for one audit root,
/// as ONE bundle. Four `&mut` maps threaded separately pushed the two
/// loaders past clippy's argument limit and read as noise at every
/// call site; the fields carry the names the model uses.
#[derive(Default)]
struct AuditOptOut {
    mark_fill_syms: std::collections::BTreeSet<u32>,
    index_1e6: BTreeMap<u32, i64>,
    expiry_ns: BTreeMap<u32, u64>,
    settle_ref: BTreeMap<u32, OptSettleRef>,
    /// F15: the WALL-stamped underlying timeline the capped option
    /// fee's index leg is read from at each fill instant.
    index_book: crate::backtest::opt::UnderlyingBook,
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
    /// VRP V2a: option records that CARRIED a mark and still could not
    /// be denominated in USD — a foreign venue, a sym this run's
    /// manifest never named, a missing underlying, or an
    /// unrepresentable premium. A venue that sends no mark at all (OKX)
    /// is deliberately NOT counted here.
    opts_unconverted: u64,
    /// VRP V2a: option QUOTE ticks converted from coin to USD, and
    /// those DROPPED because no underlying was known at their instant.
    opt_quotes_converted: u64,
    opt_quotes_dropped: u64,
    /// F13: option QUOTE ticks DROPPED because their sym produced a
    /// Deribit summary in this run and the registry has no row for it —
    /// the prices are coin-denominated and there is nothing honest to
    /// convert them with.
    opt_quotes_unregistered: u64,
    /// F13: manifest rows the parser accepted and the registry TABLE
    /// refused (full, out of window, venue mismatch).
    opt_registry_refused: u64,
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
    opt_out: &mut AuditOptOut,
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
    // VRP V2a: this run's option registry, built from the SAME manifest
    // the sym resolution uses. Per run by necessity — option ordinals
    // reshuffle at every boot (`options_manifest.rs:8-11`). A run with
    // no manifest gets an EMPTY registry and every option record then
    // skips as `Unregistered` and is counted: fail closed, never priced
    // against a guess.
    // F13: a row the parser accepted and the TABLE refused is counted;
    // its records would otherwise price through `Unregistered`, which
    // reads exactly like "not an option at all".
    let (opt_reg, registry_refused) = match manifest.as_ref() {
        Some(m) => {
            let rows: Vec<(u32, &String)> = m.iter().map(|(k, v)| (*k, v)).collect();
            crate::backtest::opt::registry_from_manifest_rows(&rows)
        }
        None => (opt_registry::OptRegistry::new(), 0),
    };
    load.opt_registry_refused += registry_refused;
    // VRP V2a: the per-sym underlying timeline the option QUOTE lane
    // needs, filled from this run's OptSummary records below.
    let mut und = crate::backtest::opt::UnderlyingBook::new();
    // F13: option syms that printed a Deribit summary in this run and
    // have no registry row — their quote ticks cannot be denominated.
    let mut unregistered_opt_syms: std::collections::BTreeSet<u32> =
        std::collections::BTreeSet::new();
    // VX-A: settlement-index observations for THIS run, resolved to
    // wall instants once the run's first tick is known (below).
    let mut settle_cand: Vec<OptSettleCand> = Vec::new();
    // F15: every underlying observation, for the wall-stamped fee book.
    let mut fee_cand: Vec<OptSettleCand> = Vec::new();

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
            // VRP V2a: every registered Deribit option contributes its
            // underlying/forward to the timeline, whether or not its own
            // mark converts — the QUOTE ticks for this sym are priced
            // off it. Keyed on the DENSE sym, which is what the ticks
            // already in `evs` carry.
            if o.venue == VenueId::Deribit as u8 {
                // F13: a Deribit option with no registry row has no
                // contract size and no underlying timeline, so its
                // QUOTE ticks carry coin numbers nothing can honestly
                // convert. Recorded here; dropped below.
                if opt_reg.get(o.sym).is_none() {
                    let dense = resolve(o.sym, interner, &mut load)?;
                    unregistered_opt_syms.insert(dense);
                }
                if let Some(row) = opt_reg.get(o.sym) {
                    let dense = resolve(o.sym, interner, &mut load)?;
                    und.observe(dense, o.ts_ns, o.underlying_px_1e9, row.contract_size_1e9);
                    // VRP V2b: the index leg of the capped option fee.
                    if o.underlying_px_1e9 > 0 {
                        opt_out.index_1e6.insert(dense, o.underlying_px_1e9 / 1_000);
                        // F15: the same observation, kept for the
                        // WALL-stamped book the fee reads at the fill
                        // instant. The wall rebase is only knowable
                        // once the run's first tick is, so it is
                        // resolved below beside the settlement index.
                        fee_cand.push(OptSettleCand {
                            sym: dense,
                            raw_ts_ns: o.ts_ns,
                            index_1e6: o.underlying_px_1e9 / 1_000,
                        });
                    }
                    // VX: the expiry, so a fill at or after it is
                    // charged the venue's SETTLEMENT rate.
                    opt_out.expiry_ns.insert(dense, row.expiry_ns);
                    // VX-A: the settlement reference. Only contracts
                    // whose expiry this run's clock can actually REACH
                    // are candidates — an expiry that fell before the
                    // run began belongs to an earlier run, and pinning
                    // it here would settle on the run's very first
                    // record. The index itself is stamped later, once
                    // the rebase is knowable.
                    if row.expiry_ns >= run.epoch_ns {
                        opt_out.settle_ref.entry(dense).or_insert(OptSettleRef::of(OptTerms {
                            strike_1e6: row.strike_1e6,
                            right: row.right,
                            expiry_ns: row.expiry_ns,
                        }));
                        if o.underlying_px_1e9 > 0 {
                            settle_cand.push(OptSettleCand {
                                sym: dense,
                                raw_ts_ns: o.ts_ns,
                                index_1e6: o.underlying_px_1e9 / 1_000,
                            });
                        }
                    }
                }
            }
            // VRP V2a — THE DENOMINATION LAW. This was a pure rescale
            // (`o.mark_px_1e9 / 1_000`) that booked a COIN premium as
            // if it were dollars, understating the option leg by the
            // underlying price (~79,000x for BTC).
            // `opt::synth_mark_usd_1e6` is shared with `backtest` so the
            // two entry points cannot drift, and it is keyed on the
            // run's OWN raw sym — the dense id below is a root-scoped
            // rename the registry knows nothing about.
            let mark_1e6 = match crate::backtest::opt::synth_mark_usd_1e6(o, &opt_reg) {
                Ok(px) => px,
                Err(skip) => {
                    if skip.is_unconverted() {
                        load.opts_unconverted += 1;
                    }
                    continue;
                }
            };
            let dense = resolve(o.sym, interner, &mut load)?;
            if tick_syms.contains(&dense) {
                continue;
            }
            opt_out.mark_fill_syms.insert(dense);
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
    // VRP V2a — THE OPTION QUOTE LANE. Deribit option rows subscribe to
    // `quote` AND `ticker` (`ingress-deribit/src/run_loop.rs:815-820`),
    // so every option sym has a REAL tick lane carrying COIN bid/ask —
    // the path option prices actually take into `FillEngine`, since the
    // D-7 synthesis above is suppressed for exactly these syms. Applied
    // here rather than at load time because a `Tick` has no underlying:
    // the timeline is complete only once this run's summaries are read.
    // Only REAL venue ticks are touched (lord < 200); the synthetic
    // mark ticks at lord 200+ are already USD.
    und.seal();
    if !und.is_empty() || !unregistered_opt_syms.is_empty() {
        let mut converted = 0u64;
        let mut dropped = 0u64;
        let mut unregistered = 0u64;
        evs.retain_mut(|e| {
            if e.class != CLASS_TICK || e.lord >= 200 {
                return true;
            }
            let Payload::Tick(t) = &mut e.payload else {
                return true;
            };
            // F13: before `convert_quote` — an unregistered sym is
            // `NotAnOption` to the book, which would leave a COIN
            // number in a USD field.
            if unregistered_opt_syms.contains(&t.sym) {
                unregistered += 1;
                return false;
            }
            match und.convert_quote(t) {
                crate::backtest::opt::QuoteFix::Converted => {
                    converted += 1;
                    true
                }
                crate::backtest::opt::QuoteFix::Unpriceable => {
                    dropped += 1;
                    false
                }
                crate::backtest::opt::QuoteFix::NotAnOption => true,
            }
        });
        load.opt_quotes_converted = converted;
        load.opt_quotes_dropped = dropped;
        load.opt_quotes_unregistered = unregistered;
        load.ticks = load.ticks.saturating_sub(dropped + unregistered);
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
    // VX-A: stamp every settlement candidate with its WALL instant and
    // keep, per contract, the latest one at or before its own expiry.
    // `wall = run.epoch_ns + (raw_ts − ts_first)` is the same rebase
    // the merge applies, so the two clocks agree by construction rather
    // than by coincidence. Deribit keeps printing an expired instrument
    // for 9–19 min after settlement, and on a $79k index the drift over
    // that lag is worth more than the option's whole premium — so the
    // cut-off is the point of the rung, not a nicety.
    if !evs.is_empty() {
        let ts_first = evs[0].ts_ns;
        for c in &settle_cand {
            let Some(r) = opt_out.settle_ref.get_mut(&c.sym) else {
                continue;
            };
            let wall = run.epoch_ns + c.raw_ts_ns.saturating_sub(ts_first);
            if wall <= r.expiry_ns && wall >= r.index_wall_ns {
                r.index_1e6 = c.index_1e6;
                r.index_wall_ns = wall;
            }
        }
        // F15: the same rebase, for the fee book. `index_1e6 * 1_000`
        // is the round trip of `underlying_px_1e9 / 1_000` that the
        // fee reads back, so the number the fee sees is exactly the
        // number the summary carried.
        for c in &fee_cand {
            let wall = run.epoch_ns + c.raw_ts_ns.saturating_sub(ts_first);
            opt_out.index_book.observe(
                c.sym,
                wall,
                c.index_1e6 * 1_000,
                crate::backtest::opt::DERIBIT_OPT_CONTRACT_SIZE_1E9,
            );
        }
    }
    Ok((evs, load))
}

/// Load every run, rebase per §3.3 (anchor = the run's min tick ts —
/// evs[0] is a tick by construction: CLASS_TICK sorts first at the
/// minimum ts), refuse wall-overlapping runs, concatenate.
fn load_and_merge_events(
    runs: &[RunDir],
    interner: &mut SymInterner,
    opt_out: &mut AuditOptOut,
    stale_after_ms: [u32; 7],
) -> Result<(Vec<MergedEv>, Vec<RunLoad>), HarnessError> {
    let epoch_0 = runs[0].epoch_ns;
    let mut merged: Vec<MergedEv> = Vec::new();
    let mut loads: Vec<RunLoad> = Vec::with_capacity(runs.len());
    let mut prev_last_virt: u64 = 0;
    for run in runs {
        let (evs, mut load) =
            load_run_events(run, interner, opt_out, stale_after_ms)?;
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
        &cfg.opt_fee,
        cfg.option_spread_frac_1e6,
    )?;
    let runs = discover_runs(&cfg.replay_dir)?;
    let mut interner = SymInterner::default();
    let mut opt_out = AuditOptOut::default();
    let (merged, loads) =
        load_and_merge_events(&runs, &mut interner, &mut opt_out, params.stale_after_ms)?;
    // F15: one seal for the whole root — the timelines are per sym and
    // the runs are concatenated in epoch order, so wall stamps are
    // already ascending; `seal` also collapses the repeats (the live
    // capture pushes a summary every 100 ms against an underlying that
    // moves far more slowly).
    opt_out.index_book.seal();

    // XSD-F: dense sym → fee class over the whole root's descriptor set.
    let sym_class: BTreeMap<u32, core_types::InstrumentClass> = interner
        .desc_by_dense
        .iter()
        .filter_map(|(dense, desc)| {
            core_config::instrument_class::class_of_descriptor(desc).map(|c| (*dense, c))
        })
        .collect();

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
        // VRP V2a: emitted only when it fires, so a run carrying no
        // option records reports exactly as it did before the
        // denomination law landed.
        if l.opt_quotes_converted > 0 || l.opt_quotes_dropped > 0 {
            report(&format!(
                "audit-pnl: run-{}: opt-quote-ticks-usd={} dropped={} (option quotes \
                 are COIN on the wire)",
                l.epoch_ns, l.opt_quotes_converted, l.opt_quotes_dropped
            ));
        }
        // F13: the same tell the backtest prints, in this surface's
        // shape — appended only when it fires.
        if l.opt_quotes_unregistered > 0 || l.opt_registry_refused > 0 {
            report(&format!(
                "audit-pnl: run-{}: opt-quote-ticks-unregistered={} registry-refused={} \
                 (a Deribit option summary with no registry row: its quotes are COIN \
                 and are DROPPED rather than booked as dollars)",
                l.epoch_ns, l.opt_quotes_unregistered, l.opt_registry_refused
            ));
        }
        if l.opts_unconverted > 0 {
            report(&format!(
                "audit-pnl: run-{}: opts-unconverted={} (marked option records \
                 not denominable in USD: foreign venue, unregistered sym, or \
                 no underlying)",
                l.epoch_ns, l.opts_unconverted
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
    if !opt_out.mark_fill_syms.is_empty() {
        // D-7 obligation: the assumption is PRINTED wherever it can
        // shape numbers.
        report(&format!(
            "audit-pnl: {}",
            crate::backtest::opt::render_opt_mark_law(
                opt_out.mark_fill_syms.len(),
                params.opt_spread_frac_1e6
            )
        ));
    }
    // VX-A obligation, the same shape as the D-7 one: a settlement
    // value SHAPES numbers, so it is printed with the reference it was
    // computed from and how stale that reference was. A settlement
    // priced off an index minutes old is not wrong, but the operator
    // is the one who gets to decide that.
    // The window's last wall instant bounds what can ever settle: the
    // engine pins on `wall >= expiry` and `finish` is capped at the
    // last record it saw, so a contract expiring after the window is
    // carried but never applied. Reporting its "value" anyway would
    // read as a settlement the report used, so only the ones the clock
    // actually reaches are printed.
    let window_end_ns = merged[merged.len() - 1].wall_ns;
    let reaches = |r: &OptSettleRef| r.expiry_ns <= window_end_ns;
    debug_assert!(
        opt_out.settle_ref
            .values()
            .all(|r| r.settleable(window_end_ns) == (reaches(r) && r.index_wall_ns > 0)),
        "the report's predicate and the applied law must agree"
    );
    let reached = opt_out.settle_ref.values().filter(|r| reaches(r)).count();
    let settleable = opt_out.settle_ref
        .values()
        .filter(|r| reaches(r) && r.index_wall_ns > 0)
        .count();
    if reached > 0 {
        report(&format!(
            "audit-pnl: opt settlement table: {settleable} of {reached} contract(s) reaching \
             expiry inside the window — European cash at the venue's 30-minute delivery \
             TWAP of the expiry's FORWARD (`settle=twap30`), or at the LAST index \
             at/before the expiry when the window carries under 10 min of samples \
             (`settle=last`); {} refused (no index at/before expiry in this root), {} \
             expire after the window and are never settled",
            reached - settleable,
            opt_out.settle_ref.len() - reached
        ));
        for (sym, r) in opt_out.settle_ref
            .iter()
            .filter(|(_, r)| reaches(r) && r.index_wall_ns > 0)
        {
            let lag_s = r.expiry_ns.saturating_sub(r.index_wall_ns) / 1_000_000_000;
            report(&format!(
                "audit-pnl:   sym={sym:#010x} {} K={} S={} value={} settle={} \
                 (covered {} s of 1800; last index {} s before expiry)",
                if r.right == opt_registry::RIGHT_CALL {
                    "call"
                } else {
                    "put"
                },
                fmt_usd_1e6(r.strike_1e6),
                fmt_usd_1e6(r.settle_index_1e6()),
                fmt_usd_1e6(r.value_1e6()),
                r.settle_law(),
                r.twap_sum_dt / 1_000_000_000,
                lag_s,
            ));
        }
    }

    // Engines: per strategy_id, plus per (vm, hash128). ModelParams
    // fields are Copy arrays — rebuild per engine (derive-agnostic).
    let mk_engine = || {
        let mut e = FillEngine::new(
            ModelParams {
                fee_bps: params.fee_bps,
                latency_ns: params.latency_ns,
                stale_after_ms: params.stale_after_ms,
                opt_fee: params.opt_fee,
                opt_spread_frac_1e6: params.opt_spread_frac_1e6,
            },
            0,
        );
        // D-7: every engine executes registered option syms under
        // the mark-fill law.
        for sym in &opt_out.mark_fill_syms {
            e.set_mark_fill_sym(*sym);
        }
        // VRP V2b: the index leg of the venue's capped option fee.
        for (sym, index_1e6) in &opt_out.index_1e6 {
            e.set_opt_index(*sym, *index_1e6);
        }
        // F15: and the wall-stamped timeline it is read from at each
        // fill instant. Cloned per engine — this surface builds one
        // per strategy, per ruleset hash and per regime word.
        if !opt_out.index_book.is_empty() {
            e.attach_underlying_book(opt_out.index_book.clone());
        }
        // VX: the settlement rate's classifier.
        for (sym, expiry_ns) in &opt_out.expiry_ns {
            e.set_opt_expiry(*sym, *expiry_ns);
        }
        // XSD-F: the per-sym fee class — every interned descriptor
        // through the descriptor law. The `run-<epoch>/sym-<hex>`
        // namespace of a manifest-less run is not a shape the law
        // knows, so those syms stay unclassed (dearest class, counted).
        for (dense, class) in &sym_class {
            e.set_sym_class(*dense, *class);
        }
        // VX-A: the European cash value each expiry settles at, so a
        // contract still held at expiry becomes cash instead of an
        // open position marked at a mid that no longer means anything.
        // No index at or before the expiry ⇒ no settlement: the
        // contract marks out as it did before the rung, which is the
        // honest outcome, since a settlement priced off a number the
        // capture does not contain is worse than a late mark-out.
        // VRP P2.1 (F10): the same call `backtest` and `--member` make.
        crate::backtest::opt::apply_settlements(&mut e, &opt_out.settle_ref, window_end_ns);
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
        // F14: `finish` is what runs the VX-A settlement sweep and the
        // end-of-window close, so reading the per-sym rows BEFORE it
        // reported an expired contract as an open position — the one
        // thing VX-A exists to stop — and a position the close had not
        // yet realised. `fill.rs`'s own tests always call it in this
        // order; this surface was the exception.
        let outcome = eng.finish();
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
             (realized={} fees={} markout={}) max_dd={} canceled_end={} caps_rejected={} \
             unroutable={} opt_settled={}",
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
            o.opt_settled,
        ));
        // I1: taker surface + the §4.3 fee ladder — printed for EVERY
        // strategy so a number positive only at 0 bps is visible as such.
        report(&format!(
            "audit-pnl:   ioc_fills={} ioc_canceled={} ttl_expired={}{} | fee ladder (net, flat \
             bps/side): 0={} 1={} 2={} tier={}",
            o.ioc_fills,
            o.ioc_canceled,
            o.ttl_expired,
            if o.fee_class_unknown_fills > 0 {
                format!(" fee_class_unknown={}", o.fee_class_unknown_fills)
            } else {
                String::new()
            },
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
         \"net_usd\":\"{}\"}},",
        loads.len(),
        fmt_usd_1e6(usd_1e12_to_1e6_floor(paper_net_1e12)),
    ));
    // XSD-F: additive — the fee table the run was priced under, per
    // venue × class, and the fills charged the dearest class for want
    // of a known class (summed over every engine). `audit_pnl_version`
    // stays 1: nothing existing moved.
    json.push_str(&format!(
        "\"fee_classes\":{},\"fee_class_unknown_fills\":{},",
        crate::backtest::render_fee_table_json(&params),
        rows.iter().map(|(_, r)| r.outcome.fee_class_unknown_fills).sum::<u64>(),
    ));
    // VRP V3: additive, and emitted ONLY when option syms were
    // registered — a root that captured no options renders exactly as
    // it did before the flag, so `audit_pnl_version` stays 1.
    if !opt_out.mark_fill_syms.is_empty() {
        json.push_str(&format!(
            "\"options\":{{\"mark_syms\":{},\"spread_frac_1e6\":{},\"law\":\"{}\"}},",
            opt_out.mark_fill_syms.len(),
            params.opt_spread_frac_1e6,
            crate::backtest::opt::render_opt_mark_law(
                opt_out.mark_fill_syms.len(),
                params.opt_spread_frac_1e6
            )
        ));
    }
    json.push_str("\"strategies\":[");
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
             \"ttl_expired\":{},\"opt_settled\":{},\
             \"fee_ladder_net_usd\":[\"{}\",\"{}\",\"{}\"],\
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
            o.opt_settled,
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
