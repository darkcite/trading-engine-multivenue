// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-pine
//!
//! Slot 8 (D15): N Pine scripts on one router slot. Every order goes through
//! `pine_broker::NativeBroker` (REVIEW S3) and the engine's I1 laws (D17).
//! PAPER ONLY by operator ruling (2026-10-16): `core_config::exec` refuses a live slot 8 at
//! parse and `cli::exec_boot` refuses `--arm-live 8` at boot.
//!
//! COPY-DOCTRINE: `boot`, `load`, `checks`, `warm` and `rt` are boot-only and may allocate and
//! copy. Every other module is hot and allocates nothing after `on_start` (I1, I4).
//!
//! **State (impl-08 T1–T10):** [`PineStrategy::configure`] runs the 8a load pipeline (§4.1:
//! `load` steps 1–8, `checks` steps 9–11 and the cross-script refusals); `supply_warm` validates
//! the warm slices (`warm`, §4.3.6); `on_start` leaks the verified programs and 64-aligned arenas
//! into `VmInstance<'static>`s, P4-deduped sub-program instances and the `SeriesStore` (`rt`,
//! §4.2), then runs the warm-up merge-walk. After it, Trade prints build bars (`bars`, §4.3.3),
//! quote ticks confirm them by time, and every confirm runs the §4.3.9 step order (`step`,
//! `ctx`, `series`). The member stays **inert for trading**: no order path exists (8d, T12+) —
//! a committing evaluation's intents are counted (`RtProbe::intents_unrouted`) and dropped at
//! `step::commit`, where 8d's `apply_intents` + `reconcile` + dispatch go. Fills, signals, AI,
//! timers and depth are counted and dropped; the cli refuses a boot naming `pine` until T19.
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

mod bars;
mod boot;
mod checks;
mod ctx;
mod load;
mod rt;
mod series;
mod step;
mod warm;

pub use boot::{
    FeeModel, HostKind, PineBootCfg, PineLoadError, PineSymbolFacts, SlotCaps8, SlotRoute,
    VenueBootState, WarmNeed, WarmSlice,
};
#[doc(hidden)]
pub use rt::RtProbe;

use core_time::NsTs;
use core_types::{
    AiCmd, ChannelEvent, ChannelId, DepthTopK, Fill, OptSummary, RuleTableV2, Signal, Tick,
};
use strategy_core::{
    Ctx, PineCounters, PineScriptView, RegimeGate, Strategy, StrategyCounters, StrategyError,
    PINE_VIEW_SCRIPTS,
};

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

// The view's row count is this member's script ceiling (strategy-core restates it).
const _: () = assert!(PINE_VIEW_SCRIPTS == MAX_SCRIPTS);
// The slot the member owns is the one `exec.toml`'s parser refuses live.
const _: () = assert!(SLOT_PINE as usize == core_config::pine::PINE_SLOT);
// `[[script]]` blocks fit the oid's script bits later (T12): eight scripts, three bits.
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
    fn load(cfg: PineBootCfg) -> Result<(Box<rt::Staged>, Vec<WarmNeed>), PineLoadError> {
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
        Ok((
            Box::new(rt::Staged {
                cfg,
                scripts,
                plan,
                warm: (0..n_rows).map(|_| None).collect(),
            }),
            needs,
        ))
    }

    /// Boot only. Hands over the warm-up bars the host could read (any subset, possibly none).
    /// Each slice is validated (§4.3.6): an invalid, stale or short one makes its row cold.
    /// A later call replaces an earlier one's result.
    ///
    /// # Errors
    /// None: validation makes a row cold, never a refusal (§4.3.6).
    ///
    /// # Panics
    /// Before `configure` (pin:08-boot-sequencing-asserts).
    pub fn supply_warm(&mut self, slices: Vec<WarmSlice>) -> Result<(), PineLoadError> {
        let st = self
            .staged
            .as_mut()
            .expect("pine: supply_warm before configure");
        st.warm = warm::validate(
            &st.plan,
            slices,
            st.cfg.boot_utc_ms,
            st.cfg.slot.max_warm_lag_bars,
        );
        Ok(())
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
        let Some(core) = self.core.as_ref() else {
            return 0;
        };
        let n = core.scripts.len().min(out.len());
        let mut i = 0usize;
        while i < n {
            let s = &core.scripts[i];
            let c = s.broker.counters();
            let mut sha = [0u8; 8];
            sha.copy_from_slice(&s.sha256[..8]);
            // COPY: one 128 B view row per script (§4.12 #9) — cold, every 5 s — the cli must not
            // name the member.
            out[i] = PineScriptView {
                sha_lo: u64::from_be_bytes(sha),
                skipped_cap: c.skipped_cap,
                skipped_max_open: c.skipped_max_open,
                ioc_remainder_cancelled: c.ioc_remainder_cancelled,
                timeframe_s: s.timeframe_s,
                coalesce_ms: s.coalesce_ms,
                max_insns: s.vm.counters().max_insns,
                sym: s.sym,
                script: u8::try_from(i).unwrap_or(u8::MAX),
                enabled: u8::from(s.enabled),
                realtime: u8::from(s.program.mode == pine_ir::ExecMode::Realtime),
                cold: u8::from(s.cold),
                ..PineScriptView::default()
            };
            i += 1;
        }
        u32::try_from(core.scripts.len()).unwrap_or(u32::MAX)
    }
    // `pine_incidents_view`, `pine_tape_view`, `pine_seq_view`: the trait's defaults (0 rows) are
    // exact until T15/T18 — no ring exists yet.
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
        match rt::build(*st) {
            Ok(mut core) => {
                let n = core.scripts.len() as u64;
                let enabled = core.scripts.iter().filter(|s| s.enabled).count() as u64;
                self.counters.scripts_loaded = n;
                self.counters.scripts_enabled = enabled;
                // §4.2 steps 5–6 (warm-up, the cold scripts), then step 9: the slices are freed.
                warm::walk(&mut core, &mut self.counters, &slices);
                drop(slices);
                self.core = Some(core);
                Ok(())
            }
            Err(e) => {
                self.boot_err = Some(e);
                Err(StrategyError::Config(SEE_LAST_BOOT_ERROR))
            }
        }
    }

    /// §4.4.2's bar half: a quote tick of a pine symbol confirms its rows by time (I12).
    #[inline]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        let _ = ctx;
        let Some(core) = self.core.as_mut() else {
            self.ignore();
            return;
        };
        if step::is_pine_sym(core, tick.sym) {
            step::on_quote(core, &mut self.counters, tick);
        }
    }

    #[inline]
    fn on_signal<C: Ctx>(&mut self, signal: &Signal, ctx: &mut C) {
        let _ = (signal, ctx);
        self.ignore();
    }

    #[inline]
    fn on_fill<C: Ctx>(&mut self, fill: &Fill, ctx: &mut C) {
        let _ = (fill, ctx);
        self.ignore();
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

    /// §4.4.2's Trade arm: a print of a pine symbol into its rows (B'1, §3.15.2). No settle can
    /// be pending before 8d, so any other event is ignored, untaped.
    #[inline]
    fn on_venue_event<C: Ctx>(&mut self, event: &ChannelEvent, ctx: &mut C) {
        let _ = ctx;
        let Some(core) = self.core.as_mut() else {
            self.ignore();
            return;
        };
        if event.channel == ChannelId::Trade as u8 && step::is_pine_sym(core, event.sym) {
            step::on_trade(core, &mut self.counters, event);
        }
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
