// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The compiled program and the compile inputs (contract §3.8).
//!
//! COPY-DOCTRINE: compile/load time only. `PineProgram` is built once by
//! `pine_sema::compile`, verified once by `pine_vm::verify`, and then only
//! ever borrowed — it is never cloned per instance (P1).

use crate::ids::{Dialect, Span, Sym};
use crate::isa::Insn;
use crate::layout::{FrameLayout, PlotDesc};
use crate::symtab::SymTable;
use crate::ty::Ty;

/// How the host steps a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum ExecMode {
    /// One evaluation per confirmed bar. No journal.
    #[default]
    BarClose = 0,
    /// Plus intrabar re-evaluations with rollback (D16). Journal sized.
    Realtime = 1,
}

/// What the script declared itself to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum ScriptKind {
    /// `strategy()`.
    #[default]
    Strategy = 0,
    /// `indicator()` — PVM-10 wraps it in a generated shell.
    Indicator = 1,
    /// `library()` — never stepped on its own.
    Library = 2,
}

/// The chart a program was compiled FOR: the main program's symbol and
/// timeframe (the fee hurdle, `timeframe.period` and every emitted row use
/// them) and the `syminfo.*` numbers. 48 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct ChartInfo {
    /// [`crate::symbol_hash`] of the canonical `venue:symbol` key.
    pub symbol_hash: u64,
    /// Bar length in seconds; one of `core_time::barclock::ALLOWED_TF_S`.
    pub timeframe_s: u32,
    /// `core_types::VenueId` as a raw byte.
    pub venue: u8,
    /// Always zero.
    pub _pad: [u8; 3],
    /// `syminfo.mintick` ×1e6 (≥ 1: an instrument whose tick is finer than
    /// 1e-6 cannot be represented at the engine's scale and is refused).
    pub mintick_1e6: i64,
    /// `syminfo.pointvalue` ×1e6 (1e6 for a linear perp or spot).
    pub pointvalue_1e6: i64,
    /// The venue's quantity step ×1e6 (0 = none).
    pub qty_step_1e6: i64,
    /// The venue's minimum order notional, USD ×1e6 (0 = none).
    pub min_notional_1e6: i64,
}

/// `strategy(default_qty_type = …)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum QtyType {
    /// `strategy.fixed` — contracts.
    #[default]
    FixedContracts = 0,
    /// `strategy.percent_of_equity`.
    PercentOfEquity = 1,
    /// `strategy.cash`.
    Cash = 2,
}

/// `strategy(commission_type = …)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum CommissionType {
    /// `strategy.commission.percent`.
    #[default]
    Percent = 0,
    /// `strategy.commission.cash_per_contract`.
    CashPerContract = 1,
    /// `strategy.commission.cash_per_order`.
    CashPerOrder = 2,
}

/// `strategy.risk.allow_entry_in(…)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum AllowEntry {
    /// Both directions.
    #[default]
    All = 0,
    /// Long only.
    Long = 1,
    /// Short only.
    Short = 2,
}

/// [`StrategyDecl::flags`] bits.
pub mod decl_flag {
    /// `process_orders_on_close = true`.
    pub const ORDERS_ON_CLOSE: u16 = 1 << 0;
    /// `calc_on_order_fills = true`.
    pub const CALC_ON_ORDER_FILLS: u16 = 1 << 1;
    /// `calc_on_every_tick = true`.
    pub const CALC_ON_EVERY_TICK: u16 = 1 << 2;
    /// `fill_orders_on_standard_ohlc = true` (recorded; inert on standard charts).
    pub const FILL_ON_STANDARD_OHLC: u16 = 1 << 3;
    /// `close_entries_rule = "ANY"` (clear = `"FIFO"`, the default).
    pub const CLOSE_ANY: u16 = 1 << 4;
    /// `use_bar_magnifier = true` — refused by both brokers.
    pub const BAR_MAGNIFIER: u16 = 1 << 5;
    /// `risk.max_drawdown` is a percentage of peak equity.
    pub const DRAWDOWN_PCT: u16 = 1 << 6;
    /// `risk.max_intraday_loss` is a percentage of the day's opening equity.
    pub const INTRADAY_LOSS_PCT: u16 = 1 << 7;
    /// `calc_on_every_history_tick = true` — refused by both brokers (`5109`).
    pub const CALC_ON_EVERY_HISTORY_TICK: u16 = 1 << 8;
}

/// `strategy.risk.*`. A zero field means "not declared". 40 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct RiskRules {
    /// Cash USD ×1e6, or percent ×1e6 with `DRAWDOWN_PCT`.
    pub max_drawdown_1e6: i64,
    /// Cash USD ×1e6, or percent ×1e6 with `INTRADAY_LOSS_PCT`.
    pub max_intraday_loss_1e6: i64,
    /// Contracts ×1e6.
    pub max_position_size_1e6: i64,
    /// Filled orders per day.
    pub max_intraday_filled_orders: u32,
    /// Consecutive losing days.
    pub max_cons_loss_days: u32,
    /// Allowed entry direction.
    pub allow_entry_in: AllowEntry,
    /// Always zero.
    pub _pad: [u8; 7],
}

/// Every `strategy()` parameter that changes a fill or a size (A27 → v3 B2,
/// v4 superset of impl-05's). 96 bytes. Computed by `pine-sema` from the
/// declaration with the **dialect's** defaults (W18: v6 `margin_long` /
/// `margin_short` default 100, v5 default 0); consumed by `pine-broker`.
/// `syminfo.*` is not here — it is [`ChartInfo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct StrategyDecl {
    /// `initial_capital`, USD ×1e6.
    pub initial_capital_1e6: i64,
    /// `default_qty_value` ×1e6, read per `default_qty_type`.
    pub default_qty_value_1e6: i64,
    /// `commission_value` ×1e6 (percent or cash per `commission_type`).
    pub commission_value_1e6: i64,
    /// `margin_long`, percent ×1e6.
    pub margin_long_pct_1e6: i64,
    /// `margin_short`, percent ×1e6.
    pub margin_short_pct_1e6: i64,
    /// `slippage`, ticks.
    pub slippage_ticks: u32,
    /// `pyramiding` as declared (0 reads as 1, TradingView's own rule).
    pub pyramiding: u32,
    /// `default_qty_type`.
    pub default_qty_type: QtyType,
    /// `commission_type`.
    pub commission_type: CommissionType,
    /// [`decl_flag`] bits.
    pub flags: u16,
    /// `backtest_fill_limits_assumption`: ticks the price must move beyond a
    /// limit order's level before it fills at that level; 0 = the default.
    pub fill_limits_ticks: u32,
    /// `strategy.risk.*`.
    pub risk: RiskRules,
}

/// What an `input.*` call site is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum InputKind {
    /// `input.int`.
    #[default]
    Int = 0,
    /// `input.float`.
    Float = 1,
    /// `input.bool`.
    Bool = 2,
    /// `input.string`.
    String = 3,
    /// `input.color`.
    Color = 4,
    /// `input.source` — resolves to a slot alias at load, not a value.
    Source = 5,
    /// `input.timeframe`.
    Timeframe = 6,
    /// `input.session`.
    Session = 7,
    /// `input.symbol`.
    Symbol = 8,
    /// `input.price`.
    Price = 9,
    /// `input.time`.
    Time = 10,
    /// `input.text_area`.
    TextArea = 11,
    /// `input.enum` (v6).
    Enum = 12,
}

/// [`InputDesc::flags`] bits.
pub mod input_flag {
    /// `min_bits` is meaningful.
    pub const HAS_MIN: u16 = 1 << 0;
    /// `max_bits` is meaningful.
    pub const HAS_MAX: u16 = 1 << 1;
    /// `step_bits` is meaningful.
    pub const HAS_STEP: u16 = 1 << 2;
    /// The call declared `options = […]`.
    pub const HAS_OPTIONS: u16 = 1 << 3;
    /// The script never reads the value (a dead input — not a plateau axis).
    pub const DEAD: u16 = 1 << 4;
    /// The manifest supplied the value in effect (`[script.inputs]`).
    pub const OVERRIDDEN: u16 = 1 << 5;
}

/// One `input.*` call site, in source order. 64 bytes.
///
/// Value encoding of `default_bits` / `min_bits` / `max_bits` / `step_bits`
/// by `ty`: `Int`/`Enum` — the `i64`; `Float` — `f64::to_bits`; `Bool` —
/// 0/1; `Color` — [`crate::color`] packing; `String` — the `Sym` in the low
/// 32 bits. `default_bits` is the value **in effect** (the manifest override
/// when [`input_flag::OVERRIDDEN`] is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct InputDesc {
    /// The manifest key: the variable the input is assigned to, else the
    /// title. Unique per program; a duplicate is a load-time refusal.
    pub name: Sym,
    /// The `title` argument, or `Sym::NONE`.
    pub title: Sym,
    /// The value type.
    pub ty: Ty,
    /// Which `input.*` function.
    pub kind: InputKind,
    /// [`input_flag`] bits.
    pub flags: u16,
    /// Always zero.
    pub _pad0: u32,
    /// The value in effect.
    pub default_bits: u64,
    /// `minval`.
    pub min_bits: u64,
    /// `maxval`.
    pub max_bits: u64,
    /// `step`.
    pub step_bits: u64,
    /// The call site.
    pub span: Span,
    /// Always zero.
    pub _pad1: u32,
}

/// A `request.security` sub-program: an expression cone extracted by sema
/// and compiled against the requested symbol's own bar stream.
#[derive(Debug, Clone)]
pub struct SubProgram {
    /// Content key: FNV-1a 64 over `symbol_key`, `timeframe_s`, `lookahead`,
    /// `gaps` and the sub-program's own `source_sha256`-equivalent code
    /// digest. Two programs requesting the same thing get the same key and
    /// share one host series (P4).
    pub key: u64,
    /// The canonical `venue:symbol` key (compile time only).
    pub symbol_key: String,
    /// [`crate::symbol_hash`] of `symbol_key`.
    pub symbol_hash: u64,
    /// HTF bar length in seconds.
    pub timeframe_s: u32,
    /// `barmerge.lookahead_on`. Admitted only where sema proves the value
    /// cannot leak (the expression reads history `≥ 1` on every path);
    /// otherwise refused at load.
    pub lookahead: bool,
    /// `barmerge.gaps_on`.
    pub gaps: bool,
    /// Outputs: the sub-program's plots `[0, n_out)` (a tuple request has
    /// `n_out > 1`). The host publishes them per HTF bar.
    pub n_out: u16,
    /// HTF bars the host must retain for the parent (≥ 2).
    pub history: u32,
    /// The compiled cone. Nested requests live in its own `subprograms`.
    pub program: Box<PineProgram>,
}

/// Everything the host, the broker and the harnesses need to know about a
/// program, computed by `pine-sema`.
#[derive(Debug, Clone)]
pub struct ProgramMeta {
    /// SHA-256 of the `.pine` source bytes.
    pub source_sha256: [u8; 32],
    /// `pine_spec::SPEC_SHA256` of the builtin table the overload ids
    /// index. `pine_vm::verify` refuses a `BuiltinTable` with another hash.
    pub spec_sha256: [u8; 32],
    /// What the script declared itself to be.
    pub kind: ScriptKind,
    /// The dialect the script declared (`//@version=`); equal to
    /// `PineProgram::dialect`. Hosts and `pine-broker` read it here (W23
    /// differs by dialect; the broker never sees `PineProgram`). Amendment A4.5.
    pub dialect: Dialect,
    /// Deepest history any slot or window needs.
    pub max_bars_back: u32,
    /// Bars before the script's outputs stop being provisional.
    pub warmup_bars: u32,
    /// The instruction budget per evaluation (manifest `insn_budget`).
    pub insn_budget_per_eval: u32,
    /// Static upper bound on intents per evaluation.
    pub max_intents_per_eval: u32,
    /// Any sub-program with `lookahead`.
    pub uses_lookahead: bool,
    /// `varip`, `barstate.isrealtime`/`isconfirmed`, `calc_on_every_tick`,
    /// `calc_on_order_fills` or `calc_on_every_history_tick` — tier-1 results
    /// do not predict tier-2 or live behaviour for such a script.
    pub uses_intrabar: bool,
    /// Depends on a non-standard chart type (refused before this is ever
    /// true on a loaded program; recorded for the PVM-9 buckets).
    pub nonstandard_chart: bool,
    /// The chart the program was compiled for.
    pub chart: ChartInfo,
    /// The `strategy()` declaration (defaults for an indicator).
    pub decl: StrategyDecl,
    /// Every `input.*` call site, in source order.
    pub inputs: Vec<InputDesc>,
    /// Every captured plot index; `plots[i].index == i`.
    pub plots: Vec<PlotDesc>,
}

impl Default for ProgramMeta {
    /// All-zero / empty, with `dialect` = [`Dialect::V6`] (`Dialect` has no
    /// zero value; sema always writes the declared one).
    fn default() -> Self {
        Self {
            source_sha256: [0; 32],
            spec_sha256: [0; 32],
            kind: ScriptKind::default(),
            dialect: Dialect::V6,
            max_bars_back: 0,
            warmup_bars: 0,
            insn_budget_per_eval: 0,
            max_intents_per_eval: 0,
            uses_lookahead: false,
            uses_intrabar: false,
            nonstandard_chart: false,
            chart: ChartInfo::default(),
            decl: StrategyDecl::default(),
            inputs: Vec::new(),
            plots: Vec::new(),
        }
    }
}

/// A compiled, not yet verified program. Immutable once built and shared
/// by every instance (P1); `Send + Sync`.
#[derive(Debug, Clone)]
pub struct PineProgram {
    /// The code; ends in `HALT` (verifier V10).
    pub code: Vec<Insn>,
    /// Constant pool: f64 via `to_bits`, i64 via `as u64`, a `StrRef` via
    /// its bits, a `Sym` via `as u64`.
    pub consts: Vec<u64>,
    /// Builtin argument windows: `argwin[b]` = count `n`, then `n` lane-local
    /// slot ids — inputs first, then the overload's outputs.
    pub argwin: Vec<u16>,
    /// Resolves every `Sym` in this program; its byte blob is the `CONST`
    /// string region.
    pub syms: SymTable,
    /// The arena map.
    pub layout: FrameLayout,
    /// The dialect the program was compiled in.
    pub dialect: Dialect,
    /// Bar-close or realtime.
    pub mode: ExecMode,
    /// Sub-programs this program reads, children before parents.
    pub subprograms: Vec<SubProgram>,
    /// Everything else.
    pub meta: ProgramMeta,
}

/// The manifest limits a compilation runs under (contract §3.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompileLimits {
    /// `max_bars_back`; 0 = no cap declared.
    pub max_bars_back: u32,
    /// `insn_budget` per evaluation.
    pub insn_budget: u32,
    /// `collection_cap`, elements per collection block.
    pub collection_cap: u32,
    /// `str_persist_bytes`.
    pub str_persist_bytes: u32,
    /// `str_cell_bytes`.
    pub str_cell_bytes: u32,
    /// `str_scratch_bytes`.
    pub str_scratch_bytes: u32,
    /// `intent_cap` per evaluation.
    pub intent_cap: u32,
    /// Hard ceiling on `FrameLayout::total_bytes`.
    pub max_frame_bytes: u32,
    /// Bitmask of implemented PVM-4 tiers: bit 0 = Tier 1 … bit 3 = Tier 4.
    pub active_tiers: u8,
}

/// A manifest `[script.inputs]` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputValue {
    /// An `int` (also `enum` ordinal, `color` packing, `time` ms).
    Int(i64),
    /// A `float` / `price`, ×1e6.
    Float1e6(i64),
    /// A `bool`.
    Bool(bool),
    /// A `string` / `timeframe` / `session` / `symbol` / `source` name.
    Str(String),
}

/// One manifest input override, keyed by [`InputDesc::name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputOverride {
    /// The input's manifest key.
    pub name: String,
    /// The value.
    pub value: InputValue,
}

/// Everything `pine_sema::compile` needs besides the AST, the interner and
/// the library cache. Compile time only.
#[derive(Debug, Clone)]
pub struct CompileOpts {
    /// Must equal the source's `//@version=`.
    pub dialect: Dialect,
    /// Bar-close or realtime (the manifest `mode`).
    pub mode: ExecMode,
    /// The program is judged or traded — a Live route, or a selection host
    /// (tier 1, tier 2): `lookahead` that is not proven leak-free is refused
    /// (`3043`). Only the conformance harness compiles with `false`.
    pub live_bound: bool,
    /// The chart: canonical key, hash, timeframe and `syminfo` numbers.
    pub chart: ChartInfo,
    /// The canonical `venue:symbol` key of the chart (`syminfo.tickerid`).
    pub symbol_key: String,
    /// `syminfo.ticker` / `syminfo.basecurrency` / `syminfo.currency` /
    /// `syminfo.type` strings, in that order.
    pub syminfo_strs: [String; 4],
    /// The manifest limits.
    pub limits: CompileLimits,
    /// `[script.inputs]`.
    pub inputs: Vec<InputOverride>,
    /// `[script.symbols]`: TradingView ticker → canonical key, for
    /// `request.security` targets. An unmapped ticker is refused.
    pub ticker_map: Vec<(String, String)>,
    /// SHA-256 of the source bytes (copied into `ProgramMeta`).
    pub source_sha256: [u8; 32],
}

const _: () = assert!(core::mem::size_of::<ExecMode>() == 1);
const _: () = assert!(core::mem::size_of::<ChartInfo>() == 48);
const _: () = assert!(core::mem::size_of::<RiskRules>() == 40);
const _: () = assert!(core::mem::size_of::<StrategyDecl>() == 96);
const _: () = assert!(core::mem::size_of::<InputDesc>() == 64);
const _: () = assert!(core::mem::align_of::<InputDesc>() == 8);
