// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `pine-screen` — the one tier-1 runner (PVM-7; contract v4 §2, §3.15, §3.16)
//!
//! Bar files whose rows are `pine_ir::Bar`, mapped read-only; the contract §3.15.3 step loop over
//! `TvBroker` with sub-programs first; `TrialSummary` rows and daily return series; the fee hurdle;
//! the tier-2 hand-off. Used by `multivenue-engine pine` and by `pvm-select`. No clock (P5): every
//! time is a bar's or a row's. The bar loop allocates nothing (I16).
//!
//! INCREMENT (impl-07 §6): T3–T10 are in (T9: sub-programs stepped first, deduplicated per group).

#![deny(missing_docs, unused_must_use, unreachable_pub, unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod bars;
mod codec;
pub mod ctx;
pub mod delta;
pub mod handoff;
pub mod hurdle;
pub mod ids;
pub mod ret;
pub mod rows;
pub mod screen;

pub use bars::{bar_path, bar_source, BarFile, BarFileErr, BarFileHdr, Clamp};
pub use ctx::{ScreenCtx, SeriesSpec, SeriesStore, SlotKey};
pub use rows::{AccumCfg, RowRec, RowsFile, RowsHdr, RowsWriter, TrialAccum, TrialEnd};
pub use screen::{
    compile_trial, run_group, run_trial, screen_decl, CompileRefusal, FaultOrigin, FaultRecord,
    FeeRow, GroupStats, Loaded, LoopProbe, NoProbe, Refusal, ScreenCfg, ScreenErr, TrialBars,
    TrialOutcome, TrialSpec, TrialStatus, Workspace,
};

use pine_ir::CompileLimits;

/// **The one selection notional** (review row 11): every tier-1 and tier-2 row that selection
/// reads is sized `SizingMode::FixedNotional { notional_1e6: SEL_NOTIONAL_1E6 }` unless the
/// operator rules another value (§10 Q1). $1 000 (impl-10 v1's `SEL_NOTIONAL_1E6`).
pub const SEL_NOTIONAL_1E6: i64 = 1_000_000_000;

/// Screen initial capital, in unit notionals per open-trade slot: `initial_capital =
/// CAPITAL_UNITS × max(1, pyramiding) × notional` (§4.3.4).
pub const CAPITAL_UNITS: i64 = 4;

/// The compile limits every screened trial runs under — contract §3.14's example values, the same as
/// impl-06 v4's `DEFAULT_LIMITS`; the hand-off manifest repeats them (§4.8.4).
pub const SCREEN_LIMITS: CompileLimits = CompileLimits {
    max_bars_back: 5_000,
    insn_budget: 2_000_000,
    collection_cap: 4_096,
    str_persist_bytes: 4_096,
    str_cell_bytes: 128,
    str_scratch_bytes: 8_192,
    intent_cap: 32,
    max_frame_bytes: 64 << 20,
    active_tiers: 0b0011, // Tiers 1–2; bit 2 when lane D lands (impl-04)
};

/// Most candidates one group steps per bar read (L1 interleave, §4.4).
pub const MAX_GROUP: usize = 64;
