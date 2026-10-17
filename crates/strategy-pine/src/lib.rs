// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-pine
//!
//! Slot 8 (D15): N Pine scripts on one router slot. Every order goes through
//! `pine_broker::NativeBroker` (REVIEW S3) and the engine's I1 laws (D17).
//! PAPER ONLY by operator ruling (2026-10-16): `core_config::exec` refuses a live slot 8 at
//! parse and `cli::exec_boot` refuses `--arm-live 8` at boot.
//!
//! COPY-DOCTRINE: `boot`, `bootrec`, `load`, `checks`, `warm` and `rt` are boot-only and may
//! allocate and copy. Every other module is hot and allocates nothing after `on_start` (I1, I4).
//!
//! **State (impl-08 T1–T10):** [`PineStrategy::configure`] runs the 8a load pipeline (§4.1:
//! `load` steps 1–8, `checks` steps 9–11 and the cross-script refusals); `supply_warm` validates
//! the warm slices (`warm`, §4.3.6); `on_start` leaks the verified programs and 64-aligned arenas
//! into `VmInstance<'static>`s, P4-deduped sub-program instances and the `SeriesStore` (`rt`,
//! §4.2), then runs the warm-up merge-walk. After it, Trade prints build bars (`bars`, §4.3.3),
//! quote ticks confirm them by time, and every confirm runs the §4.3.9 step order (`step`,
//! `ctx`, `series`). 8c (T11, `coalesce`): realtime scripts `step_tick` the forming bar under
//! event-time coalescing and the per-event instruction budget, round-robin; `calc_on_order_fills`
//! owes one extra tick per fill callback. 8d (T12–T14, `clock`, `oid`, `host`, `settle`,
//! `fills`): every committing evaluation goes through `NativeBroker::apply_intents`, then
//! `reconcile` + dispatch to `ctx.submit` / `modify` / `cancel` (the set's `StampCtx` over the
//! router and its risk gate); dispositions wait for the settle point; fills book through
//! `NativeBroker::on_fill` only. 8e (T15, `fault`): a script fault disables that script (sticky),
//! pushes one incident into the ring and chases a reduce-only flatten (retries, `flatten_stuck`,
//! the slow phase). 8f (T16, `arm`): `reconcile_boot` once per committing script, the four
//! boot-exit policies and the host `CloseAll` (boot guard, late-flip guard). Orders carry
//! `ORDER_FLAG_REDUCE_ONLY` from the broker's sizing and never `ORDER_FLAG_POST_ONLY`
//! (pin:08-order-flags-set, its POST_ONLY half retired; pin:08-no-post-only-paper). PAPER ONLY:
//! a `Live` route refuses `on_start` (pin:08-live-refused-at-start). Signals, AI, timers and
//! depth are counted and dropped. The cli boots the member from `run --pine <manifest>` (T19,
//! paper half): a `--strategy` naming `pine` without `--pine`, or `--pine` without `pine` in the
//! mask, refuses the boot (F19 both ways). impl-Bp §3.9's read-backs for the replay host (R2, R4,
//! R7, R8) are cold accessors on [`PineStrategy`]; bar marks are recorded only on a replay host
//! (`marks`).
//!
//! **I5, `pop_stamp_has_no_reader`.** The crate's one clock read is `tape::PopStamp::take`, and
//! the stamp is write-only: nothing outside `tape` can name or read it.
//!
//! ```compile_fail
//! // `PopStamp` (and `tape`) are private: no reader exists outside the module.
//! fn read(p: &strategy_pine::tape::PopStamp) -> u64 { p.0 }
//! ```
//!
//! Literal readings (each a `pin:08-<slug>`):
//! - pin:08-slot8-rename: impl-08 was written for slot 7; every "slot 7" reads slot 8.
//! - pin:08-inert-counts-ignored: the unconfigured shell drops every callback it is handed and
//!   counts each in `callbacks_ignored` (§3.1's "counted and dropped", applied to a shell with
//!   no core). A fresh shell's counters are all zero (I3).
//! - pin:08-new-callbacks-default: `on_trade` and `on_order_event` joined the trait after
//!   impl-08's snapshot; impl-08 is silent on them, so the member keeps the trait's no-op
//!   defaults (impl-08's trade input is `ChannelEvent { channel: Trade }`, B'1).
//! - pin:08-boot-sequencing-asserts: `configure` after `on_start`, and `supply_warm` before
//!   `configure`, are host sequencing defects and panic (fail fast); impl-08 names no code.
//! - pin:08-non-pine-untouched: once started, a tick or Trade of a symbol no row reads is
//!   ignored and NOT counted (§4.4.2 "ignored, untaped"); `callbacks_ignored` counts the
//!   callbacks the member has no path for yet (fills, signals, AI, timers, depth, …).

#![deny(missing_docs, unused_must_use, unreachable_pub, unsafe_op_in_unsafe_fn)]
#![deny(
    clippy::float_arithmetic,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

mod arm;
mod bars;
mod boot;
pub mod bootrec;
mod checks;
mod clock;
mod coalesce;
mod counters;
mod ctx;
pub mod delivery;
mod fault;
mod fills;
mod host;
mod load;
mod marks;
pub mod oid;
mod rt;
mod series;
mod settle;
mod step;
mod tape;
mod warm;

pub use boot::{
    FeeModel, HostKind, PineBootCfg, PineLoadError, PineSymbolFacts, SlotCaps8, SlotRoute,
    VenueBootState, WarmNeed, WarmSlice,
};
pub use bootrec::{BootRecord, BootRecordScript, BOOT_RECORD_FILE, BOOT_RECORD_MAGIC};
#[doc(hidden)]
pub use rt::RtProbe;

use core_time::NsTs;
use core_types::{
    AiCmd, ChannelEvent, ChannelId, DepthTopK, Fill, OptSummary, RuleTableV2, Signal, Tick,
};
use strategy_core::{
    pine_seq, Ctx, PineCounters, PineIncident, PineScriptView, PineSeqRec, PineTapeRec, RegimeGate,
    Strategy, StrategyCounters, StrategyError, PINE_VIEW_SCRIPTS,
};

/// impl-Bp §3.9 **R1** (verbatim signature; for B'1's Trade route): resolve the manifest's symbols
/// WITHOUT booting the member — every script's chart symbol, then its `[script.symbols]` targets,
/// manifest order, deduplicated by `SymbolId` — through `resolve` (canonical key →
/// `(SymbolId, capture descriptor)`). Every script counts, `enabled = 0` included: it still
/// evaluates on its bars. Boot only; allocates.
///
/// # Errors
/// `6006` (an unresolved symbol, §4.1.1) for the first canonical key `resolve` cannot map.
pub fn route_symbols(
    manifest: &core_config::pine::PineManifest,
    resolve: impl Fn(&str) -> Option<(core_types::SymbolId, &str)>,
) -> Result<Vec<(core_types::SymbolId, String)>, pine_ir::DiagCode> {
    let mut out: Vec<(core_types::SymbolId, String)> = Vec::new();
    for sc in &manifest.scripts {
        let keys =
            core::iter::once(sc.symbol.as_str()).chain(sc.symbols.iter().map(|t| t.1.as_str()));
        for key in keys {
            let Some((sym, desc)) = resolve(key) else {
                return Err(pine_ir::DiagCode(6006));
            };
            if !out.iter().any(|(s, _)| *s == sym) {
                // COPY: the capture descriptor (≤ 64 B) per symbol, boot only — R1's owned
                // `String` — a borrow rejected: the universe outlives the call, not the result.
                out.push((sym, desc.to_owned()));
            }
        }
    }
    Ok(out)
}

/// R4: a VM `FaultKind` byte back to its variant (pine-ir has no `from_u8`). A byte no variant
/// names cannot come out of `VmInstance::last_fault`; read as `Internal`
/// (pin:08-fault-unknown-internal: our defect, never the script's).
fn fault_kind_of(b: u8) -> pine_ir::FaultKind {
    use pine_ir::FaultKind as K;
    match b {
        1 => K::InsnBudget,
        2 => K::StrArenaFull,
        3 => K::PoolFull,
        4 => K::JournalFull,
        5 => K::CollectionBounds,
        6 => K::DivideByZeroInt,
        7 => K::InvalidCast,
        9 => K::ScriptRefused,
        _ => K::Internal,
    }
}

/// The router slot this member owns (`strategy-set` slot map; operator ruling 2026-10-16).
pub const SLOT_PINE: u8 = 8;
/// Scripts one slot hosts (`[[script]]` blocks); more is `6011`.
pub const MAX_SCRIPTS: usize = 8;
/// Distinct engine symbols across chart and sub-program symbols; more is `6011`.
pub const MAX_SYMBOLS: usize = 16;
/// Distinct `(symbol, timeframe)` bar rows; more is `6011`.
pub const MAX_ROWS: usize = 32;
/// Sub-program instances after P4 dedupe; more is `6011`.
pub const MAX_SUBS: usize = 32;
/// Ceiling on `SubProgram::history`; more is `6011`.
pub const MAX_SERIES_HISTORY: u32 = 4096;
/// D13: fast flatten attempts before `flatten_stuck` is raised (impl-05 §4.14.2 names it).
pub const FLATTEN_RETRIES: u8 = 8;
/// Dispositions one script can have pending (= `pine_broker`'s sent-table size).
pub const MAX_SETTLES: usize = 16;
/// Incident ring entries.
pub const INCIDENT_CAP: usize = 64;
/// Sequencing-tape ring (`PineSeqRec`, 128 B): 2 MiB.
pub const SEQ_TAPE_CAP: usize = 16_384;
/// Decision-tape ring (`PineTapeRec`, 64 B): 1 MiB.
pub const DECISION_TAPE_CAP: usize = 16_384;
/// R6 delivery-journal ring (`PineDeliveryRec`, 32 B): 1 MiB — the sequencing tape's records
/// plus room for the kind-3 dispositions between two 5 s drains.
pub const DELIVERY_CAP: usize = 32_768;
/// R7: bar marks one script holds between two [`PineStrategy::take_bar_marks`] calls (replay
/// host only; a row confirms at most one bar per callback, pin:08-bar-marks-undrained-panics).
pub const BAR_MARKS_CAP: usize = 16;

// The view's row count is this member's script ceiling (strategy-core restates it).
const _: () = assert!(PINE_VIEW_SCRIPTS == MAX_SCRIPTS);
// The slot the member owns is the one `exec.toml`'s parser refuses live.
const _: () = assert!(SLOT_PINE as usize == core_config::pine::PINE_SLOT);
// `[[script]]` blocks fit the oid's script bits (§4.5.3): eight scripts, three bits.
const _: () = assert!(MAX_SCRIPTS.is_power_of_two());

/// The refusal `on_start` returns before `configure` has run (F19, I3).
pub const NOT_CONFIGURED: &str =
    "pine: slot 8 is not configured — the member boots only from a manifest (F19)";
/// The refusal `on_start` returns when completing the core refused (§4.1.7 step 3).
pub const SEE_LAST_BOOT_ERROR: &str = "pine: see last_boot_error";

/// The slot-8 member. **Small by value**: `strategy-set` holds members by value, and the
/// configured member's state (programs, brokers, arenas, rings) is boxed: `Staged` at
/// `configure`, `Core` at `on_start` (pin:08-staged-then-core). `new` is `const` and inert.
/// P1: one engine thread owns the member for the process lifetime.
pub struct PineStrategy {
    /// The observables; all zero for a fresh shell.
    counters: PineCounters,
    /// `configure`'s output, consumed by `on_start`.
    staged: Option<Box<rt::Staged>>,
    /// The run-time core, after `on_start`.
    core: Option<Box<rt::Core>>,
    /// The last load refusal (`configure` or `on_start`).
    boot_err: Option<PineLoadError>,
}

impl core::fmt::Debug for PineStrategy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PineStrategy")
            .field("counters", &self.counters)
            .field("staged", &self.staged.is_some())
            .field("core", &self.core.is_some())
            .field("boot_err", &self.boot_err)
            .finish()
    }
}

impl PineStrategy {
    /// Inert: `pine_counters()` is all zeros, and `on_start` without `configure` refuses (F19).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counters: PineCounters {
                scripts_loaded: 0,
                scripts_enabled: 0,
                scripts_armed: 0,
                scripts_faulted: 0,
                scripts_cold: 0,
                bars_confirmed: 0,
                bars_gapped: 0,
                bars_partial_boot: 0,
                prints_late: 0,
                event_ts_regressions: 0,
                bar_evals: 0,
                tick_evals: 0,
                ticks_coalesced: 0,
                ticks_budgeted: 0,
                cof_evals: 0,
                intents_discarded: 0,
                submits: 0,
                modifies: 0,
                cancels: 0,
                submit_errors: 0,
                cancel_errors: 0,
                acks: 0,
                cancelled_reported: 0,
                settle_points: 0,
                fills: 0,
                fills_prior_boot: 0,
                fills_foreign: 0,
                late_fills_after_cancel: 0,
                late_reducer_flips: 0,
                fee_modelled_live: 0,
                refusal_trips: 0,
                entries_suppressed: 0,
                flattens_started: 0,
                flatten_attempts: 0,
                flatten_stuck: 0,
                boot_guard_flattens: 0,
                incidents: 0,
                seq_records: 0,
                seq_dropped: 0,
                tape_records: 0,
                tape_dropped: 0,
                callbacks_ignored: 0,
            },
            staged: None,
            core: None,
            boot_err: None,
        }
    }

    /// 8a, boot only (allocates). Runs the whole load pipeline (§4.1) for every script and every
    /// cross-script check. Returns the warm-up bars the member wants, one entry per row.
    ///
    /// # Errors
    /// The first refusal, with its code (`6001`–`6013`, or the parse / sema / verifier / std /
    /// broker code that refused); it is also kept as [`Self::last_boot_error`].
    ///
    /// # Panics
    /// After `on_start` (pin:08-boot-sequencing-asserts).
    pub fn configure(&mut self, cfg: PineBootCfg) -> Result<Vec<WarmNeed>, PineLoadError> {
        assert!(self.core.is_none(), "pine: configure after on_start");
        self.staged = None;
        self.boot_err = None;
        match Self::load(cfg) {
            Ok((staged, needs)) => {
                self.staged = Some(staged);
                Ok(needs)
            }
            Err(e) => {
                self.boot_err = Some(e.clone());
                Err(e)
            }
        }
    }

    /// §4.1.1 for every script, then §4.1.6.
    fn load(mut cfg: PineBootCfg) -> Result<(Box<rt::Staged>, Vec<WarmNeed>), PineLoadError> {
        // R5: a replay's record first — it sets this boot's client-oid epoch.
        let record = Self::replay_inputs(&mut cfg)?;
        // pin:08-epoch-zero-refused (risk review): with epoch 0 every own fill decodes as a prior
        // boot's and is dropped, so paper re-fires without bound — a hard load refusal.
        if cfg.oid_epoch == 0 {
            return Err(PineLoadError::new(
                6006,
                None,
                "oid_epoch",
                "the client-oid epoch must be non-zero (§4.5.3)".to_string(),
            ));
        }
        // Risk item (a), pin:08-boot-state-live-only: off Live the venue boot state must be empty
        // (paper and replay restart flat, §4.7.5); a host that hands one over is refused.
        if cfg.route != SlotRoute::Live && !cfg.boot_state.is_empty() {
            return Err(PineLoadError::new(
                6013,
                None,
                "boot_state",
                format!(
                    "{} venue boot state rows off Live: paper and replay boot flat",
                    cfg.boot_state.len()
                ),
            ));
        }
        if cfg.route == SlotRoute::Live {
            core_config::pine::require_live(&cfg.slot, 0).map_err(|e| {
                PineLoadError::new(
                    e.code,
                    None,
                    "[exec.slot.8.pine]",
                    format!("{}: {}", e.key, e.why),
                )
            })?;
        }
        let mut libs = pine_sema::LibCache::open(cfg.artifact_dir.join("lib"), core_crypto::sha256)
            .map_err(|d| PineLoadError {
                code: d.code,
                script: None,
                key: "lib",
                detail: d.msg.clone(),
                diags: vec![d],
            })?;
        let mut scripts = Vec::with_capacity(cfg.manifest.scripts.len());
        for (i, sc) in cfg.manifest.scripts.iter().enumerate() {
            let l = load::load_script(i, sc, &cfg, &mut libs)?;
            let broker = checks::per_script(i, sc, &l, &cfg)?;
            scripts.push((l, broker));
        }
        drop(libs);
        // References only (no copy): the plan and the checks borrow the loaded programs.
        let loaded: Vec<&load::LoadedScript> = scripts.iter().map(|s| &s.0).collect();
        let plan = rt::plan(&cfg, &loaded);
        checks::cross(&cfg, &loaded, &plan)?;
        drop(loaded);
        let n_rows = plan.rows.len();
        let needs = plan
            .rows
            .iter()
            .map(|r| WarmNeed {
                // COPY: the row key (≤ 64 B) per row, once — `WarmNeed` is §3.2's owned row — a
                // borrow rejected: the needs outlive `configure`'s plan borrow.
                key: r.key.clone(),
                symbol_hash: r.symbol_hash,
                tf_s: r.tf_s,
                want: r.want,
            })
            .collect();
        // R5: every script must hash to the record's line (the manifest was checked before load).
        if let Some(rec) = record.as_ref() {
            for (i, (l, _)) in scripts.iter().enumerate() {
                if rec.scripts[i].sha256 != l.sha256 {
                    return Err(PineLoadError::new(
                        6001,
                        Some(i),
                        "boot_record",
                        "the script's source does not hash to the boot record".to_string(),
                    ));
                }
            }
        }
        Ok((
            Box::new(rt::Staged {
                cfg,
                scripts,
                plan,
                warm: (0..n_rows).map(|_| None).collect(),
                warm_digest: vec![0; n_rows],
                record,
            }),
            needs,
        ))
    }

    /// R5 (impl-Bp §3.9, §3.10): the replay inputs. Off replay, `boot_record` / `warmup` are
    /// refused (`6006`, key `replay`, pin:08-r5-replay-only-inputs). A record is read, must name
    /// this manifest (`6001`) and one line per script, must be flat (`6013`,
    /// pin:08-boot-record-flat-off-live: paper and replay boot flat, risk item (a)), and its
    /// client-oid bases must agree on one non-zero epoch (`6013`), which becomes this boot's.
    fn replay_inputs(cfg: &mut PineBootCfg) -> Result<Option<BootRecord>, PineLoadError> {
        if !cfg.replay {
            if cfg.boot_record.is_some() || cfg.warmup.is_some() {
                return Err(PineLoadError::new(
                    6006,
                    None,
                    "replay",
                    "boot_record / warmup are replay inputs (replay = false)".to_string(),
                ));
            }
            return Ok(None);
        }
        let Some(path) = cfg.boot_record.as_ref() else {
            return Ok(None);
        };
        let bad = |code: u16, script: Option<usize>, detail: String| {
            Err(PineLoadError::new(code, script, "boot_record", detail))
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => return bad(6006, None, format!("{}: {e}", path.display())),
        };
        let rec = match BootRecord::parse(&text) {
            Ok(r) => r,
            Err(e) => return bad(6006, None, format!("{}: {e}", path.display())),
        };
        if rec.manifest_sha256 != cfg.manifest_sha256 {
            return bad(
                6001,
                None,
                "the manifest does not hash to the boot record".into(),
            );
        }
        if rec.scripts.len() != cfg.manifest.scripts.len() {
            return bad(
                6001,
                None,
                format!(
                    "the record names {} scripts, the manifest {}",
                    rec.scripts.len(),
                    cfg.manifest.scripts.len()
                ),
            );
        }
        let epoch =
            u32::try_from(rec.scripts.first().map_or(0, |s| s.client_oid_base >> 32)).unwrap_or(0);
        for (i, s) in rec.scripts.iter().enumerate() {
            if s.position_1e6 != 0 || s.avg_px_1e6 != 0 || s.day_notional_1e6 != 0 {
                return bad(6013, Some(i), "a replayed boot is flat (paper)".into());
            }
            let ix = u8::try_from(i).unwrap_or(u8::MAX);
            if epoch == 0 || s.client_oid_base != bootrec::client_oid_base(epoch, ix) {
                return bad(
                    6013,
                    Some(i),
                    "client_oid_base is not epoch<<32 | script<<29".into(),
                );
            }
        }
        cfg.oid_epoch = epoch;
        Ok(Some(rec))
    }

    /// R5: the boot record of this boot (impl-Bp §3.10), after `on_start` — the host renders it
    /// (`BootRecord::render`) into the run's capture directory before the first evaluation.
    /// `run_epoch_ns` (the run directory) and `boot_mono_ns` (the engine clock after `on_start`)
    /// are the host's: this crate reads no clock (I5). `None` before `on_start`. Boot only
    /// (allocates).
    #[must_use]
    pub fn boot_record(&self, run_epoch_ns: u64, boot_mono_ns: u64) -> Option<BootRecord> {
        let core = self.core.as_ref()?;
        let mut scripts = Vec::with_capacity(core.scripts.len());
        for (i, s) in core.scripts.iter().enumerate() {
            let ix = u8::try_from(i).unwrap_or(u8::MAX);
            scripts.push(BootRecordScript {
                idx: ix,
                sha256: s.sha256,
                position_1e6: s.boot_st.0,
                avg_px_1e6: s.boot_st.1,
                day_notional_1e6: s.boot_st.2,
                client_oid_base: bootrec::client_oid_base(core.epoch, ix),
                warmup_digest: core.warm_digest.get(s.chart_row).copied().unwrap_or(0),
            });
        }
        Some(BootRecord {
            manifest_sha256: core.manifest_sha256,
            run_epoch_ns,
            boot_mono_ns,
            route: core.route,
            scripts,
        })
    }

    /// Boot only. Hands over the warm-up bars the host could read (any subset, possibly none).
    /// Each slice is validated (§4.3.6): an invalid, stale or short one makes its row cold.
    /// A later call replaces an earlier one's result.
    ///
    /// # Errors
    /// Validation makes a row cold, never a refusal (§4.3.6). R5: a replay with a boot record
    /// refuses (`6013`, key `warmup_digest`) when a script's chart-row digest (0 = cold) differs
    /// from the record's — the warm-up is not the recorded one.
    ///
    /// # Panics
    /// Before `configure` (pin:08-boot-sequencing-asserts).
    pub fn supply_warm(&mut self, slices: Vec<WarmSlice>) -> Result<(), PineLoadError> {
        let st = self
            .staged
            .as_mut()
            .expect("pine: supply_warm before configure");
        // pin:08-boot-record-warm-digest-chart-row: the digest word per row, taken before the
        // slices are consumed; a row the validation leaves cold reads 0.
        let mut dig = vec![0u64; st.plan.rows.len()];
        for sl in &slices {
            if let Some(r) = st
                .plan
                .rows
                .iter()
                .position(|p| p.symbol_hash == sl.symbol_hash && p.tf_s == sl.tf_s)
            {
                let mut w = [0u8; 8];
                // COPY: 8 B of the slice digest — boot only — the digest is an owned array.
                w.copy_from_slice(&sl.digest[..8]);
                dig[r] = u64::from_le_bytes(w);
            }
        }
        let cut = st.cfg.warmup.as_ref().map(|w| w.1);
        st.warm = warm::validate(
            &st.plan,
            slices,
            st.cfg.boot_utc_ms,
            cut,
            st.cfg.slot.max_warm_lag_bars,
        );
        for (r, d) in dig.iter_mut().enumerate() {
            if st.warm[r].is_none() {
                *d = 0;
            }
        }
        st.warm_digest = dig;
        if let Some(rec) = st.record.as_ref() {
            for (i, row) in st.plan.script_rows.iter().enumerate() {
                if rec.scripts[i].warmup_digest != st.warm_digest[*row] {
                    let e = PineLoadError::new(
                        6013,
                        Some(i),
                        "warmup_digest",
                        format!(
                            "chart-row warm digest {:016x} ≠ the boot record's {:016x}",
                            st.warm_digest[*row], rec.scripts[i].warmup_digest
                        ),
                    );
                    self.boot_err = Some(e.clone());
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// TEST SEAM (feature `test-seams`, enabled only by this crate's own tests): replace the staged
    /// venue boot state after `configure` (which refuses one off Live), so a paper member adopts it
    /// at `on_start` (I38, I40). No host enables the feature.
    ///
    /// # Panics
    /// Before `configure`.
    #[cfg(feature = "test-seams")]
    #[doc(hidden)]
    pub fn seed_boot_state_for_test(&mut self, states: Vec<VenueBootState>) {
        let st = self
            .staged
            .as_mut()
            .expect("pine: seed_boot_state_for_test before configure");
        st.cfg.boot_state = states;
    }

    /// The refusal behind the last `configure` `Err` or `on_start` `Err(StrategyError::Config(..))`.
    #[must_use]
    pub fn last_boot_error(&self) -> Option<&PineLoadError> {
        self.boot_err.as_ref()
    }

    /// Integration-test introspection (`tests/load.rs`): `None` before `on_start`.
    #[doc(hidden)]
    #[must_use]
    pub fn rt_probe(&self) -> Option<RtProbe> {
        self.core.as_ref().map(|c| c.probe())
    }

    /// Script `script` of the started core (`None` before `on_start`, or past the loaded scripts:
    /// pin:08-readback-after-start).
    fn script_rt(&self, script: usize) -> Option<&rt::ScriptRt> {
        self.core.as_ref().and_then(|c| c.scripts.get(script))
    }

    /// impl-Bp §3.9 **R2**: script `script`'s broker counters — the UNJUDGED gate's input ("the
    /// broker counts, impl-Bp judges"). Cold; never allocates.
    #[must_use]
    pub fn broker_counters(&self, script: usize) -> Option<&pine_ir::BrokerCounters> {
        self.script_rt(script).map(|s| s.broker.counters())
    }

    /// **R4**: the `NativeCfg` script `script`'s broker was built with (§4.1.5) — the null arms'
    /// gates and prices (`cross_ticks`, `tick_1e6`, caps). Cold.
    #[must_use]
    pub fn native_cfg(&self, script: usize) -> Option<pine_ir::NativeCfg> {
        self.script_rt(script).map(|s| s.ncfg)
    }

    /// **R4**: closed trade `i` of script `script`, RETAINED indexing (0 = oldest retained;
    /// absolute trade `k` is `i = k − (closed_count − view().closed_retained)`, impl-05 §3.6.1).
    /// Borrowed, never copied.
    #[must_use]
    pub fn closed_trade(&self, script: usize, i: u32) -> Option<&pine_ir::TradeRec> {
        self.script_rt(script)
            .and_then(|s| s.broker.closed_trade(i))
    }

    /// **R4**: every trade script `script` ever closed (`view().closedtrades`, impl-05 §3.6.1);
    /// 0 before `on_start` or past the loaded scripts.
    #[must_use]
    pub fn closed_count(&self, script: usize) -> u32 {
        self.script_rt(script)
            .map_or(0, |s| s.broker.view().closedtrades)
    }

    /// **R4b** (B'3 request): the closed trades script `script`'s broker still RETAINS
    /// (`view().closed_retained`, impl-05 §3.6.1) — [`Self::closed_trade`]'s index bound; absolute
    /// trade `k` is retained `k − (closed_count − closed_retained)`. 0 before `on_start` or past
    /// the loaded scripts. Cold.
    #[must_use]
    pub fn closed_retained(&self, script: usize) -> u32 {
        self.script_rt(script)
            .map_or(0, |s| s.broker.view().closed_retained)
    }

    /// **R4c** (B'3 request): the trades script `script` still holds open
    /// (`view().opentrades`); 0 before `on_start` or past the loaded scripts. Cold.
    #[must_use]
    pub fn open_count(&self, script: usize) -> u32 {
        self.script_rt(script)
            .map_or(0, |s| s.broker.view().opentrades)
    }

    /// **R4c** (B'3 request): open trade `i` of script `script` (0 = oldest open) — its entry
    /// fields, and the open profit at the last mark (`TradeRec::pnl_1e12`, impl-05) — what B'3
    /// feeds `TrialAccum::on_open_trade_at_end`. Borrowed, never copied. Cold.
    #[must_use]
    pub fn open_trade(&self, script: usize, i: u32) -> Option<&pine_ir::TradeRec> {
        self.script_rt(script).and_then(|s| s.broker.open_trade(i))
    }

    /// **R4**: script `script`'s run record (callable mid-run, impl-05 §3.4) — `TrialAccum`'s
    /// commission. Cold.
    #[must_use]
    pub fn broker_record(&self, script: usize) -> Option<pine_ir::RunRecord> {
        self.script_rt(script).map(|s| s.broker.record())
    }

    /// **R4**: the most instructions one evaluation of script `script`'s own VM charged so far
    /// (`VmCounters::max_insns`; pin:08-max-insns-own-vm: sub-program instances are shared and
    /// charged on their own). 0 before `on_start` or past the loaded scripts.
    #[must_use]
    pub fn max_insns(&self, script: usize) -> u32 {
        self.script_rt(script)
            .map_or(0, |s| s.vm.counters().max_insns)
    }

    /// **R4**: the chart script `script` was compiled for (`program.meta.chart`, §4.1.5) —
    /// `pointvalue_1e6` for `TrialAccum`. Cold.
    #[must_use]
    pub fn chart(&self, script: usize) -> Option<pine_ir::ChartInfo> {
        self.script_rt(script).map(|s| s.program.meta.chart)
    }

    /// **R4**: the fault that disabled script `script`: `(FaultKind, pc, bar index)`; `None` while
    /// it runs. A sub-program fault reports the instance's own kind and pc
    /// (pin:08-fault-sub-kind); a broker refusal reports `FaultKind::ScriptRefused` with pc
    /// `u32::MAX` (pin:08-fault-broker-refusal-script-refused: the script asked for something
    /// the broker refuses at run time — impl-05's `ScriptRefusal`, not our bug).
    #[must_use]
    pub fn fault(&self, script: usize) -> Option<(pine_ir::FaultKind, u32, u32)> {
        let s = self.script_rt(script)?;
        if !s.disabled {
            return None;
        }
        let kind = if s.fault_vm == 0 {
            pine_ir::FaultKind::ScriptRefused
        } else {
            fault_kind_of(s.fault_vm)
        };
        Some((kind, s.fault.1, s.fault.2))
    }

    /// **R7**: moves script `script`'s bar marks — each confirmed chart bar since the last call,
    /// with `BrokerView::equity_1e12` after its evaluation — oldest first into `out` (as many as
    /// fit; the rest stay). Returns the count. The replay host drains after every record; the
    /// engine host records none (pin:08-bar-marks-replay-only). Never allocates.
    pub fn take_bar_marks(&mut self, script: usize, out: &mut [(pine_ir::Bar, i128)]) -> usize {
        match self.core.as_mut() {
            Some(core) if script < core.scripts.len() => marks::take(core, script, out),
            _ => 0,
        }
    }

    /// **R8**: the fee ×1e12 of the fill the host delivers NEXT (`client_oid` = its
    /// `Fill::order_id`); the member books it through `NativeBroker::on_fill` in place of the
    /// model (pin:08-noted-fee-one-fill). Called immediately before `on_fill`. Before `on_start`
    /// there is nothing to book: dropped.
    #[inline]
    pub fn note_fill_fee(&mut self, client_oid: u64, fee_1e12: i128) {
        if let Some(core) = self.core.as_mut() {
            core.noted_fee = Some((client_oid, fee_1e12));
        }
    }

    /// R6 (impl-Bp §3.10): the delivery-journal records with ordinal `>= from` still in the
    /// ring, oldest first, into `out`; returns `(ordinal of out[0], n)` — a reader whose cursor
    /// is behind the first ordinal has lost `first − from` records (pin:08-delivery-ordinal).
    /// `(from, 0)` before `on_start` or without `--pine-tape`. Cold; never allocates.
    pub fn delivery_view(&self, out: &mut [delivery::PineDeliveryRec], from: u64) -> (u64, u32) {
        match self.core.as_ref() {
            Some(core) => delivery::view(core, out, from),
            None => (from, 0),
        }
    }

    /// R6: journal records overwritten before any [`Self::delivery_view`] returned them.
    #[must_use]
    pub fn delivery_dropped(&self) -> u64 {
        self.core.as_ref().map_or(0, |c| c.delivery.dropped())
    }

    /// R8, cold: notes ([`Self::note_fill_fee`]) consumed by a fill of another oid — a host
    /// sequencing defect, counted in release (pin:08-noted-fee-one-fill). 0 before `on_start`.
    #[must_use]
    pub fn noted_fee_mismatched(&self) -> u64 {
        self.core.as_ref().map_or(0, |c| c.noted_fee_mismatched)
    }

    /// One dropped callback (pin:08-inert-counts-ignored).
    #[inline(always)]
    fn ignore(&mut self) {
        self.counters.callbacks_ignored = self.counters.callbacks_ignored.wrapping_add(1);
    }
}

impl Default for PineStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl StrategyCounters for PineStrategy {
    fn strategy_kind(&self) -> &'static str {
        "pine"
    }

    #[inline]
    fn pine_counters(&self) -> PineCounters {
        self.counters
    }

    /// The boot tell's rows (§4.2): one per loaded script. Cold path; never allocates. 0 before
    /// `on_start` (I3).
    fn pine_scripts_view(&self, out: &mut [PineScriptView]) -> u32 {
        match self.core.as_ref() {
            Some(core) => counters::scripts_view(core, out),
            None => 0,
        }
    }

    /// §4.6: the incident ring, `seq >= from_seq`, oldest first (T15). Cold; never allocates.
    fn pine_incidents_view(&self, out: &mut [PineIncident], from_seq: u64) -> u32 {
        match self.core.as_ref() {
            Some(core) => fault::view(core, out, from_seq),
            None => 0,
        }
    }
    /// §4.9.2 (T18): the decision tape, `seq >= from_seq`, oldest first. Cold; never allocates.
    fn pine_tape_view(&self, out: &mut [PineTapeRec], from_seq: u64) -> u32 {
        match self.core.as_ref() {
            Some(core) => tape::dec_view(core, out, from_seq),
            None => 0,
        }
    }

    /// §4.9.2 (T18): the sequencing tape, `seq >= from_seq`, oldest first. Cold; never allocates.
    fn pine_seq_view(&self, out: &mut [PineSeqRec], from_seq: u64) -> u32 {
        match self.core.as_ref() {
            Some(core) => tape::seq_view(core, out, from_seq),
            None => 0,
        }
    }
}

impl Strategy for PineStrategy {
    /// §4.2: refuses unless `configure` has run (F19); otherwise leaks the programs and arenas,
    /// verifies again, builds every instance and the series store, runs the warm-up merge-walk
    /// and frees the slices — the last allocation (I1). No clock is read (`ctx` is unused).
    fn on_start<C: Ctx>(&mut self, ctx: &mut C) -> Result<(), StrategyError> {
        let _ = ctx;
        let Some(mut st) = self.staged.take() else {
            return Err(StrategyError::Config(NOT_CONFIGURED));
        };
        let slices = core::mem::take(&mut st.warm);
        // D19 (8f): the boot facts and the Q1 policy live only through `on_start`.
        let states = core::mem::take(&mut st.cfg.boot_state);
        let policy = st
            .cfg
            .slot
            .boot_exit_policy
            .unwrap_or(core_config::pine::BootExitPolicy::Accept);
        let boot_ns = u64::try_from(pine_ir::scale::ns_from_ms(st.cfg.boot_utc_ms)).unwrap_or(0);
        let mut sha_lo = [0u8; 8];
        sha_lo.copy_from_slice(&st.cfg.manifest_sha256[..8]);
        let hdr = tape::BootHdr {
            route: st.cfg.route as u8,
            host: st.cfg.host as u8,
            policy: policy as u8,
            anchor: st.cfg.anchor,
            epoch: st.cfg.oid_epoch,
            boot_utc_ms: st.cfg.boot_utc_ms,
            manifest_sha_lo: u64::from_be_bytes(sha_lo),
        };
        match rt::build(*st) {
            Ok(mut core) => {
                let n = core.scripts.len() as u64;
                let enabled = core.scripts.iter().filter(|s| s.enabled).count() as u64;
                self.counters.scripts_loaded = n;
                self.counters.scripts_enabled = enabled;
                // §4.2 steps 5–6 (warm-up, the cold scripts), then step 9: the slices are freed.
                // §4.9.2: the boot records (decision `seq_ref` 0).
                tape::boot(&mut core, &mut self.counters, &hdr);
                let boot = arm::BootArm {
                    states: &states,
                    policy,
                    boot_ns,
                };
                warm::walk(&mut core, &mut self.counters, &slices, &boot);
                // §4.7.5: the cold scripts adopt here (and `protective` / `flatten` guard them).
                arm::finish_boot(&mut core, &mut self.counters, &boot);
                drop(slices);
                drop(states);
                // The warm-up is not an engine callback: the per-event budget starts here.
                core.charged = 0;
                core.charged_max = 0;
                self.core = Some(core);
                Ok(())
            }
            Err(e) => {
                self.boot_err = Some(e);
                Err(StrategyError::Config(SEE_LAST_BOOT_ERROR))
            }
        }
    }

    /// §4.4.2 `on_tick`: a quote tick of a pine symbol — E, the quote, gone-evidence, the rows
    /// confirmed by time (I12), the pending realtime ticks, then reconcile + dispatch.
    #[inline]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        let Some(core) = self.core.as_mut() else {
            self.ignore();
            return;
        };
        if step::is_pine_sym(core, tick.sym) {
            tape::begin(
                core,
                &mut self.counters,
                pine_seq::TICK,
                tape::words(tick),
                ctx,
            );
            delivery::tick(core, tick);
            coalesce::begin_callback(core);
            if core.host == HostKind::Replay {
                // Replay: every callback is a settle point, evidence from earlier records only.
                settle::resolve_point(core, &mut self.counters);
            }
            step::on_quote(core, &mut self.counters, tick);
            // D13 step 5: a flattening script on this symbol is owed a `flatten` call.
            fault::due_sym(core, tick.sym);
            host::flush(core, &mut self.counters, ctx);
            tape::end(core);
        }
    }

    #[inline]
    fn on_signal<C: Ctx>(&mut self, signal: &Signal, ctx: &mut C) {
        let _ = (signal, ctx);
        self.ignore();
    }

    /// §4.5.7: decode, book through the broker, the paper remainder, the `calc_on_order_fills`
    /// evaluation, then reconcile + dispatch. Not a settle point (it runs mid-drain).
    #[inline]
    fn on_fill<C: Ctx>(&mut self, fill: &Fill, ctx: &mut C) {
        let Some(core) = self.core.as_mut() else {
            self.ignore();
            return;
        };
        // §4.9.2: every fill delivered is sequenced, foreign and prior-boot ones included.
        tape::begin(
            core,
            &mut self.counters,
            pine_seq::FILL,
            tape::words(fill),
            ctx,
        );
        delivery::fill(core, fill);
        coalesce::begin_callback(core);
        let took = fills::on_fill(core, &mut self.counters, fill).is_some();
        if took {
            coalesce::run_all_cof(core, &mut self.counters);
        }
        // A broker refusal on the fill disables the script (D13): its flatten is owed now.
        if took || core.flat_due != 0 {
            host::flush(core, &mut self.counters, ctx);
        }
        tape::end(core);
    }

    #[inline]
    fn on_ai<C: Ctx>(&mut self, cmd: &AiCmd, ctx: &mut C) {
        let _ = (cmd, ctx);
        self.ignore();
    }

    #[inline]
    fn on_ruleset_table(&mut self, table: &RuleTableV2) {
        let _ = table;
        self.ignore();
    }

    /// §4.4.2 `on_venue_event`, the settle point (after the paper-fill pump): pending
    /// dispositions are resolved FIRST, then a print of a pine symbol goes into its rows (B'1,
    /// §3.15.2) and its pending ticks; then reconcile + dispatch. Any other event with nothing
    /// pending is ignored, untaped.
    #[inline]
    fn on_venue_event<C: Ctx>(&mut self, event: &ChannelEvent, ctx: &mut C) {
        let Some(core) = self.core.as_mut() else {
            self.ignore();
            return;
        };
        let trade = event.channel == ChannelId::Trade as u8 && step::is_pine_sym(core, event.sym);
        if !trade && !settle::any_pending(core) {
            return;
        }
        tape::begin(
            core,
            &mut self.counters,
            pine_seq::EVENT,
            tape::words(event),
            ctx,
        );
        delivery::event(core, event);
        coalesce::begin_callback(core);
        settle::resolve_point(core, &mut self.counters);
        if trade {
            step::on_trade(core, &mut self.counters, event);
            fault::due_sym(core, event.sym);
        }
        host::flush(core, &mut self.counters, ctx);
        tape::end(core);
    }

    #[inline]
    fn on_depth<C: Ctx>(&mut self, depth: &DepthTopK, ctx: &mut C) {
        let _ = (depth, ctx);
        self.ignore();
    }

    #[inline]
    fn on_opt_summary<C: Ctx>(&mut self, opt: &OptSummary, ctx: &mut C) {
        let _ = (opt, ctx);
        self.ignore();
    }

    /// A timer is a clock (P5): counted and dropped.
    #[inline]
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C) {
        let _ = (now_ns, ctx);
        self.ignore();
    }

    /// `u64::MAX`: slot 8 never lowers the set's period and never fires.
    #[inline]
    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    #[inline]
    fn on_regime<C: Ctx>(&mut self, gate: RegimeGate, ctx: &mut C) {
        let _ = (gate, ctx);
        self.ignore();
    }

    /// Flushes nothing and places nothing.
    fn on_stop<C: Ctx>(&mut self, ctx: &mut C) {
        let _ = ctx;
    }
}
