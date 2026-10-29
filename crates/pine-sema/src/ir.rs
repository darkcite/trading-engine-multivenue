// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! The IR (impl-02 §4.6.4): structured blocks over virtual slots, one instance per program.
//! `lower.rs` builds it per Instantiation (§4.6.3); the back half (sites, references, `na`
//! capability, series, layout, emission) walks it. The opcode-level flags (`VLD`, `NA_AWARE`) are
//! emission's, from §4.8's slot classes; the IR carries only what lowering decides (W1, M1, U06,
//! U15, `pin:06-na-compare`).
#![allow(
    dead_code,
    reason = "the §4.6.4 vocabulary; T11–T19 build and read the remaining variants"
)]

use std::collections::HashMap;

use pine_ir::{Lane, OverloadId, Span, Sym, TypeQ};

/// A virtual slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct VSlot(pub(crate) u32);

/// What a virtual slot holds (§4.6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotKind {
    /// A declared variable (plain or `var`).
    Var,
    /// A `varip` variable.
    Varip,
    /// A user-function parameter, bound per call.
    Param,
    /// A `for` / `for…in` counter, bound per iteration.
    Counter,
    /// An expression temporary.
    Temp,
    /// Written once, in the init block (§4.14.1).
    Const,
    /// A `var` / `varip` / `once` guard; `varip` iff the declaration is (or `pin:02-once-flag`
    /// reads `VaripLike`).
    Flag {
        /// Survives a tick's rollback.
        varip: bool,
    },
    /// A `ValueSrc::Call` value (§4.14.1): `value` indexes `pine_spec::VALUES`.
    Prelude {
        /// The `pine_spec::VALUES` row.
        value: u32,
    },
    /// A UDT string field's staging slot (CAR-03-1 (a)).
    Stage {
        /// The object site.
        site: u32,
        /// The field.
        field: u16,
    },
    /// A reserved slot read by a `ValueSrc::Slot` value (`close`, `bar_index`, …): never written
    /// (verifier rules 8 and 22). `pin:02-ir-reserved-slot` — §4.6.4's sketch names no variant for
    /// them; the IR names the reserved slot it reads so emission addresses it directly.
    Reserved {
        /// The reserved slot index (`Value::bits`).
        slot: u16,
    },
}

/// One virtual slot's facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotInfo {
    /// Type and qualifier.
    pub(crate) ty: TypeQ,
    /// `F64` for `float`, `I64` for every other type.
    pub(crate) lane: Lane,
    /// What it holds.
    pub(crate) kind: SlotKind,
    /// The owning Instantiation (`mono::InstId`).
    pub(crate) inst: u32,
    /// The innermost region that declares it (0 = top level).
    pub(crate) region: u32,
    /// The declared name, `Sym::NONE` for temporaries.
    pub(crate) name: Sym,
    /// Where it comes from.
    pub(crate) span: Span,
    /// Owns a persistent string cell (§4.13.2).
    pub(crate) cell: bool,
}

/// A call argument: a slot, or a constant that becomes a `Const` slot at emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arg {
    /// A slot.
    Slot(VSlot),
    /// A pine-spec string (a `DefaultKind::Str` default or a string `ValueSrc::Const`): emission
    /// interns it as a `CONST` `StrRef` (§4.14.5 rule 2, T19). `pin:02-ir-spec-str` — the IR's
    /// `SymTable` is read-only in lowering, so the bytes ride the argument.
    Str(&'static str),
    /// A constant in lane encoding; `valid == false` is `na`.
    Const {
        /// The lane.
        lane: Lane,
        /// The bits (`f64::to_bits` on `F64`).
        bits: u64,
        /// Not `na`.
        valid: bool,
    },
}

/// A history offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum K {
    /// A constant offset.
    Const(u32),
    /// A dynamic offset in a slot.
    Dyn(VSlot),
}

/// Unary operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnOpK {
    /// `F_NEG` / `I_NEG`.
    Neg,
    /// `F_ABS` / `I_ABS`.
    Abs,
    /// `B_NOT`.
    Not,
    /// `F_SQRT`.
    Sqrt,
    /// `F_I2F`.
    I2f,
    /// `I_F2I`; `round` sets `ROUND` (half away from zero).
    F2i {
        /// Round half away instead of truncating.
        round: bool,
    },
    /// `F_FINITE`.
    Finite,
}

/// Binary operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOpK {
    /// `F_ADD` / `I_ADD` (int: wrapping, `pin:06-int-width`).
    Add,
    /// `F_SUB` / `I_SUB`.
    Sub,
    /// `F_MUL` / `I_MUL`.
    Mul,
    /// `F_DIV`; `na_on_nonfinite` under U15 `na`.
    Div {
        /// `NA_ON_NONFINITE`.
        na_on_nonfinite: bool,
    },
    /// `F_MOD` / `I_MOD`: truncated, the dividend's sign (CAR-R31-1); float `NA_ON_NONFINITE` per U15.
    Mod {
        /// `NA_ON_NONFINITE` (float only).
        na_on_nonfinite: bool,
    },
    /// `F_MIN` / `I_MIN` (NaN-propagating, `pin:06-minmax-na`).
    Min,
    /// `F_MAX` / `I_MAX`.
    Max,
    /// `F_POW`; `NA_ON_NONFINITE` per U15.
    Pow {
        /// `NA_ON_NONFINITE`.
        na_on_nonfinite: bool,
    },
    /// `B_AND` (strict; the lazy forms are `CondKind::LazyAndOr` regions).
    And,
    /// `B_OR`.
    Or,
}

/// Comparisons (`false` on NaN for floats).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpK {
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
}

/// One operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    /// A unary operation (the lane is the operand's).
    Un {
        /// The operation.
        op: UnOpK,
        /// The result.
        dst: VSlot,
        /// The operand.
        a: VSlot,
    },
    /// A binary operation (both operands in the result's lane).
    Bin {
        /// The operation.
        op: BinOpK,
        /// The result.
        dst: VSlot,
        /// Left.
        a: VSlot,
        /// Right.
        b: VSlot,
    },
    /// A comparison (both operands in one lane); the result is a `bool`.
    Cmp {
        /// The comparison.
        op: CmpK,
        /// The result.
        dst: VSlot,
        /// Left.
        a: VSlot,
        /// Right.
        b: VSlot,
        /// A script's float comparison under U06 `abs_1e10` (CAR-R33-7): `flg::TOL` on the
        /// `F_*` opcode. Sema's own comparisons (loop bounds, `bool(x)`, `switch`) keep IEEE.
        tol: bool,
    },
    /// `dst ← a`.
    Mov {
        /// The destination.
        dst: VSlot,
        /// The source.
        a: VSlot,
    },
    /// `dst ← c`.
    Load {
        /// The destination.
        dst: VSlot,
        /// The constant (or slot).
        c: Arg,
    },
    /// `NA_CHECK`.
    NaCheck {
        /// The `bool` result.
        dst: VSlot,
        /// The operand.
        a: VSlot,
    },
    /// `NA_SET`.
    NaSet {
        /// The slot set to `na`.
        a: VSlot,
    },
    /// `NZ_F` / `NZ_I`.
    Nz {
        /// The result.
        dst: VSlot,
        /// The value.
        a: VSlot,
        /// The replacement.
        b: VSlot,
    },
    /// `dst ← cond ? a : dst`.
    Sel {
        /// The destination.
        dst: VSlot,
        /// The value.
        a: VSlot,
        /// The condition.
        cond: VSlot,
    },
    /// `dst ← src[k]`.
    Hist {
        /// The destination.
        dst: VSlot,
        /// The series.
        src: VSlot,
        /// The offset.
        k: K,
    },
    /// `CALL_BUILTIN`: one per call site.
    Call {
        /// The overload.
        ov: OverloadId,
        /// Its window's inputs (§4.14.5): one per parameter row of the dialect in order — the
        /// synthetics first (`Arg::Const`), a variadic row once per argument.
        args: Vec<Arg>,
        /// Its results.
        outs: Vec<VSlot>,
        /// The call's span.
        span: Span,
    },
    /// An object creation site.
    ObjNew {
        /// The handle.
        dst: VSlot,
        /// The UDT.
        udt: u16,
        /// The site's span.
        span: Span,
    },
    /// A field read.
    ObjGet {
        /// The result.
        dst: VSlot,
        /// The handle.
        h: VSlot,
        /// The field.
        field: u16,
    },
    /// A field write.
    ObjSet {
        /// The handle.
        h: VSlot,
        /// The field.
        field: u16,
        /// The value.
        src: VSlot,
    },
    /// `S_PERSIST` into `dst`'s cell.
    Persist {
        /// The cell owner.
        dst: VSlot,
        /// The scratch string.
        src: VSlot,
    },
    /// `BUDGET`.
    Budget {
        /// Units.
        units: u32,
    },
}

/// A conditional region's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CondKind {
    /// `if`.
    If,
    /// A `switch` arm.
    Switch,
    /// `?:`.
    Ternary,
    /// Lazy `and` / `or` (W3).
    LazyAndOr,
    /// A `var` / `varip` initialiser or a `once` body.
    Guard,
}

/// A loop's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopKind {
    /// `for i = a to b [by s]`.
    For,
    /// `for x in c`.
    ForIn,
    /// `while`.
    While,
}

/// One statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stmt {
    /// An operation.
    Op(Op),
    /// A conditional region.
    If {
        /// The condition.
        cond: VSlot,
        /// Taken when true.
        then: Block,
        /// Taken otherwise.
        els: Block,
        /// Its kind.
        kind: CondKind,
        /// An `na` condition takes `els` (`pin:02-na-condition` `AsFalse`); `false`: neither arm
        /// runs (`NaResult`).
        na_else: bool,
    },
    /// A loop region.
    Loop {
        /// The loop's span (refusals name it).
        span: Span,
        /// A `for` with a constant trip: its counter and the counter's start and signed step
        /// (U13 `per_iteration` unrolling sets the counter per copy, §4.7.4).
        ctr: Option<Ctr>,
        /// Evaluated before each test.
        head: Block,
        /// The test.
        cond: VSlot,
        /// The body.
        body: Block,
        /// After each iteration.
        step: Block,
        /// A constant trip count.
        trip: Option<u32>,
        /// Its kind.
        kind: LoopKind,
    },
    /// U13 `per_iteration` copies.
    Unrolled {
        /// One block per iteration.
        copies: Vec<Block>,
    },
    /// `break`.
    Break,
    /// `continue`.
    Continue,
}

/// A constant-trip `for` counter (§4.7.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ctr {
    /// The counter slot.
    pub(crate) slot: VSlot,
    /// Its first value.
    pub(crate) from: f64,
    /// Its signed step.
    pub(crate) delta: f64,
}

impl Eq for Ctr {}

/// A block of statements (a region inside an `If` or `Loop`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Block(pub(crate) Vec<Stmt>);

/// One program's IR.
#[derive(Debug, Default)]
pub(crate) struct Ir {
    /// `slots[v.0]`.
    pub(crate) slots: Vec<SlotInfo>,
    /// The root Instantiation's body.
    pub(crate) body: Block,
    /// Every `Const` slot with its `(bits, valid)`, in creation order (the init block's order).
    pub(crate) const_list: Vec<(VSlot, u64, bool)>,
    /// `Const` slots keyed `(lane, bits, valid, string)`, program-wide (a lookup index, never
    /// iterated). A string constant's bits are its `Sym` until emission makes them a `CONST`
    /// `StrRef`, so `string` is part of the key (an `int` 11 is not `Sym(11)`).
    consts: HashMap<(u8, u64, bool, bool), VSlot>,
    /// `Reserved` / `Prelude` slots by value row (a lookup index).
    values: HashMap<u32, VSlot>,
    /// Plot indices in code order (§4.14.8): every non-`hline` index, then every `hline`.
    pub(crate) plots: Vec<(pine_ir::PlotKind, Span)>,
    /// Amendment A6: one row per `strategy.entry` / `strategy.order` site whose `id` folds —
    /// `(str_key(id), entry_dir bits)`; [`Ir::entry_dirs`] merges them.
    pub(crate) entry_sites: Vec<(pine_ir::StrKey, u8)>,
    /// Every `var` / `varip` declaration: `(its variable slot, its Guard flag)` — §4.9's
    /// persistent storage and var-born classification read them.
    pub(crate) var_decls: Vec<(VSlot, VSlot)>,
    /// `max_bars_back(x, n)` (§4.10.4): `(x's slot, n)`, the variable's own cap.
    pub(crate) var_caps: Vec<(VSlot, u32)>,
    /// `pin:03-hist-dynamic-offset` (CAR-R33-2): every dynamic `x[k]`'s limit constant (an
    /// un-interned `Const` slot) with its series `x`; lowering's end sets the constant to `x`'s
    /// own `max_bars_back(x, n)` once every call is seen.
    pub(crate) hist_guards: Vec<(VSlot, VSlot)>,
    /// `request.security` calls (§4.11.2–§4.11.3): each one's detached expression block and its
    /// element calls' outputs, in lowering order.
    pub(crate) requests: Vec<Request>,
    /// Each request's raw `timeframe` text (`requests[i]`'s), resolved in 5a.
    pub(crate) request_tfs: Vec<String>,
    /// Slots holding a folded `syminfo.*` / `timeframe.*` value ([`CTX_SYMINFO`] /
    /// [`CTX_TIMEFRAME`] bits) — `3029` in a cone whose target differs (§4.11.4).
    pub(crate) ctx_slots: Vec<(VSlot, u8)>,
}

/// A folded `syminfo.*` value.
pub(crate) const CTX_SYMINFO: u8 = 1;
/// A folded `timeframe.*` value.
pub(crate) const CTX_TIMEFRAME: u8 = 2;

/// One `request.security` call (§4.11.2–§4.11.3).
#[derive(Debug, Clone)]
pub(crate) struct Request {
    /// The detached expression block `E` (never emitted in the parent).
    pub(crate) e: Block,
    /// `E`'s region id (a slot of a lower region written in `E` is a parent slot, `3028`).
    pub(crate) region: u32,
    /// `E`'s result slots `r_0 … r_{k−1}`.
    pub(crate) rs: Vec<VSlot>,
    /// The element calls' outputs `out_0 … out_{k−1}` (the parent's slots).
    pub(crate) outs: Vec<VSlot>,
    /// The child's tail: per element the `F_I2F` (when not `float`) and the `plot` capture at
    /// plot index `j` (§4.11.4).
    pub(crate) tail: Block,
    /// The target canonical key.
    pub(crate) symbol: String,
    /// The target timeframe in seconds.
    pub(crate) tf_s: u32,
    /// `barmerge.lookahead_on`.
    pub(crate) lookahead: bool,
    /// `barmerge.gaps_on`.
    pub(crate) gaps: bool,
    /// The call's span.
    pub(crate) span: Span,
}

impl Ir {
    /// A fresh slot.
    pub(crate) fn slot(&mut self, info: SlotInfo) -> VSlot {
        self.slots.push(info);
        VSlot((self.slots.len() - 1) as u32)
    }

    /// The program-wide `Const` slot of `(lane, bits, valid)`, written once (§4.14.1).
    pub(crate) fn konst(&mut self, info: SlotInfo, bits: u64, valid: bool) -> VSlot {
        let key = (
            info.lane as u8,
            bits,
            valid,
            valid && info.ty.ty == pine_ir::Ty::String,
        );
        if let Some(&v) = self.consts.get(&key) {
            return v;
        }
        let v = self.slot(SlotInfo {
            kind: SlotKind::Const,
            ..info
        });
        self.consts.insert(key, v);
        self.const_list.push((v, bits, valid));
        v
    }

    /// A `Const` slot of its own (never shared through the interning index), so lowering may
    /// set its bits later ([`Ir::hist_guards`]).
    pub(crate) fn konst_own(&mut self, info: SlotInfo, bits: u64) -> VSlot {
        let v = self.slot(SlotInfo {
            kind: SlotKind::Const,
            ..info
        });
        self.const_list.push((v, bits, true));
        v
    }

    /// Whether value row `row` already has its slot.
    pub(crate) fn has_value(&self, row: u32) -> bool {
        self.values.contains_key(&row)
    }

    /// The slot of a built-in value row (`Reserved` or `Prelude`), one per row program-wide.
    pub(crate) fn value(&mut self, row: u32, info: SlotInfo) -> VSlot {
        if let Some(&v) = self.values.get(&row) {
            return v;
        }
        let v = self.slot(info);
        self.values.insert(row, v);
        v
    }

    /// `ProgramMeta::entry_dirs` (A6.2): one row per id, sorted by id, the sites' bits OR-ed.
    pub(crate) fn entry_dirs(&self) -> Vec<pine_ir::EntryDir> {
        let mut rows: Vec<pine_ir::EntryDir> = Vec::with_capacity(self.entry_sites.len());
        let mut sites = self.entry_sites.clone();
        sites.sort_by_key(|s| s.0);
        for (id, dirs) in sites {
            match rows.last_mut() {
                Some(r) if r.id == id => r.dirs |= dirs,
                _ => rows.push(pine_ir::EntryDir {
                    id,
                    dirs,
                    _pad: [0; 7],
                }),
            }
        }
        rows
    }

    /// The constant a `Const` slot holds: `(bits, valid)`.
    pub(crate) fn const_of(&self, v: VSlot) -> Option<(u64, bool)> {
        self.const_list
            .iter()
            .find(|c| c.0 == v)
            .map(|c| (c.1, c.2))
    }
}
