// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `pine-ir` — the PVM frozen contract, as Rust
//!
//! Every type that crosses a `pine-*` crate boundary is defined here and
//! nowhere else (contract v4, `00-INDEX.md` §3). A crate that needs a
//! different shape does not invent one: it states the workaround in its
//! spec's §10 and the contract is amended.
//!
//! ## What lives here
//!
//! * identifiers and spans ([`Sym`], [`SiteId`], [`Span`], [`StrKey`], …)
//!   and the per-compilation [`SymTable`];
//! * the stable FNV-1a keys: [`symbol_hash`] (the `venue:symbol` key that
//!   shared series and bar files are indexed by) and [`str_key`] (order
//!   ids, OCA names);
//! * types and qualifiers ([`Ty`], [`Qual`], [`TypeQ`]);
//! * diagnostics ([`Diag`], [`DiagCode`] and its frozen ranges);
//! * value encodings — the `na` model, [`StrRef`], [`CollRef`],
//!   [`CollHeader`], colours;
//! * the frame layout ([`FrameLayout`] and its descriptors) and the D16
//!   journal record ([`JrnlHdr`]);
//! * the instruction encoding ([`Insn`], [`op`], [`flg`], [`abi`]);
//! * the program ([`PineProgram`], [`SubProgram`], [`ProgramMeta`],
//!   [`StrategyDecl`], [`InputDesc`]) and the compile inputs
//!   ([`CompileOpts`]);
//! * the bar and its context ([`Bar`], [`barflag`], [`BarCtx`],
//!   [`SharedSeries`]);
//! * the step outcome ([`StepOutcome`], [`FaultKind`]);
//! * order intents and the broker surface every host touches
//!   ([`OrderIntent`], [`TradeRec`], [`BrokerView`], [`EngineIntent`],
//!   [`Quote`], [`NativeCfg`], [`TvCfg`], [`BrokerCounters`],
//!   [`RunRecord`]);
//! * the tier-1 → selection row ([`TrialSummary`]);
//! * the fixed-point scales of ruling D18 ([`scale`]).
//!
//! ## What does not
//!
//! `Frame`, `BuiltinFn`, `BuiltinTable`, `StateBlock`, `Pod`,
//! `VmInstance` and `verify` name the arena view that only `pine-vm` can
//! construct, so they are defined in `pine-vm`; their signatures are frozen
//! by the contract document all the same (§3.7, §3.11).
//!
//! ## Doctrine
//!
//! * **No dependencies.** This is the leaf of the lane's dependency graph.
//! * **No clock** (law P5) and **no global state** (law P2).
//! * **No `unsafe`.**
//! * Every `#[repr(C)]` type carries a `const` size and alignment assertion,
//!   so a layout drift is a compile error rather than a silent ABI break.
//! * The only allocating types are the compile-time ones ([`SymTable`],
//!   [`Diag`], [`PineProgram`] and the vectors inside [`FrameLayout`] /
//!   [`ProgramMeta`]); none of them is touched on a hot path except through
//!   `&` borrows.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(unused_must_use)]

mod bar;
mod broker;
mod diag;
mod hash;
mod ids;
mod intent;
mod isa;
mod journal;
mod layout;
mod outcome;
mod program;
mod symtab;
mod trial;
mod ty;
mod value;

pub mod scale;

pub use bar::{barflag, Bar, BarCtx, SharedSeries};
pub use broker::{
    BrokerCounters, EngineIntent, EngineVerb, LimitPrimitive, NativeCfg, Primitive, Quote,
    RunRecord, SizingMode, TvCfg,
};
pub use diag::{Diag, DiagCode, DiagRange, Severity};
pub use hash::{
    fnv1a64, fnv1a64_continue, is_canonical_symbol_key, str_key, symbol_hash, FNV1A64_OFFSET,
    FNV1A64_PRIME, SYMBOL_KEY_MAX_LEN,
};
pub use ids::{Dialect, FileId, NodeId, OverloadId, SiteId, Span, StrKey, Sym};
pub use intent::{
    intent_flag, trade_flag, BrokerView, IntentKind, IntentSide, OcaType, OrderIntent, TradeRec,
};
pub use isa::{abi, argwin, flg, op, Insn};
pub use journal::JrnlHdr;
pub use layout::{
    ring_flag, state_flag, FrameLayout, Lane, PlotDesc, PlotKind, RingDesc, StateDesc, StateKind,
};
pub use outcome::{FaultKind, StepOutcome, VmCounters};
pub use program::{
    decl_flag, input_flag, AllowEntry, ChartInfo, CommissionType, CompileLimits, CompileOpts,
    ExecMode, InputDesc, InputKind, InputOverride, InputValue, PineProgram, ProgramMeta, QtyType,
    RiskRules, ScriptKind, StrategyDecl, SubProgram,
};
pub use symtab::SymTable;
pub use trial::{trial_flag, TrialSummary, RET_SERIES_EXT};
pub use ty::{elem, Qual, Ty, TypeQ};
pub use value::{color, str_region, CollHeader, CollKind, CollRef, StrRef, NA_F64, NA_I64};

/// The contract version this crate implements. Bumped only together with
/// `00-INDEX.md`'s header; a spec written against an older contract names
/// its version and is re-issued rather than patched.
pub const CONTRACT_VERSION: u32 = 4;
