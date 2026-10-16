// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-pine
//!
//! Slot 8 (D15): N Pine scripts on one router slot. Every order goes through
//! `pine_broker::NativeBroker` (REVIEW S3) and the engine's I1 laws (D17).
//! PAPER ONLY by operator ruling (2026-10-16): `core_config::exec` refuses a live slot 8 at
//! parse and `cli::exec_boot` refuses `--arm-live 8` at boot.
//!
//! COPY-DOCTRINE: `boot`, `load`, `checks`, `warm` and `rt` (impl-08 T5–T9, not yet landed) are
//! boot-only and may allocate and copy. Every other module is hot and allocates nothing after
//! `on_start` (I1, I4).
//!
//! **This is the T4 skeleton** (impl-08 §6): the constants, the const and inert
//! [`PineStrategy::new`], and an inert `Strategy` impl. `configure` and the load pipeline are T5;
//! until then `on_start` refuses (F19), so a boot that names `pine` in its `STRATEGY` fails
//! closed at the set's `on_start`.
//!
//! Literal readings (each a `pin:08-<slug>`):
//! - pin:08-slot8-rename: impl-08 was written for slot 7; every "slot 7" reads slot 8.
//! - pin:08-inert-counts-ignored: the unconfigured shell drops every callback it is handed and
//!   counts each in `callbacks_ignored` (§3.1's "counted and dropped", applied to a shell with
//!   no core). A fresh shell's counters are all zero (I3).
//! - pin:08-new-callbacks-default: `on_trade` and `on_order_event` joined the trait after
//!   impl-08's snapshot; impl-08 is silent on them, so the member keeps the trait's no-op
//!   defaults (impl-08's trade input is `ChannelEvent { channel: Trade }`, B'1).

#![deny(missing_docs, unused_must_use, unreachable_pub, unsafe_op_in_unsafe_fn)]
#![deny(
    clippy::float_arithmetic,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use core_time::NsTs;
use core_types::{AiCmd, ChannelEvent, DepthTopK, Fill, OptSummary, RuleTableV2, Signal, Tick};
use strategy_core::{
    Ctx, PineCounters, RegimeGate, Strategy, StrategyCounters, StrategyError, PINE_VIEW_SCRIPTS,
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

/// The slot-8 member. **Small by value**: `strategy-set` holds members by value, and the
/// configured member's state (rings, arenas, brokers) is boxed at `configure` (T5). `new` is
/// `const` and inert.
#[derive(Debug)]
pub struct PineStrategy {
    /// The observables; all zero for a fresh shell.
    counters: PineCounters,
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
        }
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
    // `pine_scripts_view`, `pine_incidents_view`, `pine_tape_view`, `pine_seq_view`: the trait's
    // defaults (0 rows) are exact for the shell — nothing is loaded and no ring exists (I3).
}

impl Strategy for PineStrategy {
    /// Refuses until `configure` (T5) exists and has run: an operator asked for slot 8 and the
    /// engine cannot guess the manifest (F19).
    fn on_start<C: Ctx>(&mut self, ctx: &mut C) -> Result<(), StrategyError> {
        let _ = ctx;
        Err(StrategyError::Config(NOT_CONFIGURED))
    }

    #[inline]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        let _ = (tick, ctx);
        self.ignore();
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

    #[inline]
    fn on_venue_event<C: Ctx>(&mut self, event: &ChannelEvent, ctx: &mut C) {
        let _ = (event, ctx);
        self.ignore();
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
