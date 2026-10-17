// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Lowering to the IR (impl-02 §4.14 + §4.6), part 1: expressions — constants (every folded node,
//! §4.17), variables, built-in values (`Reserved` / `Prelude` slots), unary and binary operators
//! (W1, M1, U06, U15, `pin:06-na-compare`, mixed int/float widening), the casts and the built-ins
//! lowered to opcodes (§4.14.6), and every other built-in call as one `Op::Call` — over plain
//! declarations, `:=` and expression statements; user-function inlining at its Instantiation.
//! Part 2 (T11, §4.14.3–§4.14.4): `if` / `switch` / the ternary (U14) / `and` / `or` (W3) with
//! `pin:02-na-condition`, `for` (MG-23, `pin:02-for-step`), `for…in`, `while`, loop values,
//! `break` / `continue`, `var` / `varip` / `once` guards (`pin:02-once-flag`), compound assignment
//! and U16's block-locals. Objects, strings' staging and argument windows (T12) are refused here
//! as `3255` "not implemented: lowering …" until their task lands.

use std::collections::HashMap;

use pine_ir::{Diag, Dialect, Lane, NodeId, OverloadId, Qual, Span, Sym, SymTable, Ty, TypeQ};
use pine_lang::{AssignOp, Ast, BinOp, DeclMode, Node, UnOp};
use pine_spec::{swr, ValueSrc};

use crate::code::{diag, ReasonCode};
use crate::infer::{Callee, Inst, Typed, K};
use crate::ir::{
    Arg, BinOpK, Block, CmpK, CondKind, Ctr, Ir, LoopKind, Op, SlotInfo, SlotKind, Stmt, UnOpK,
    VSlot,
};
use crate::mono::{InstId, Tree};
use crate::pins::{ForStep, IntCast, NaCompare, NaCondition, OnceFlag};
use crate::resolve::Resolved;
use crate::scope::Binding;
use crate::sw::Readings;

/// The IR's lane of a type.
pub(crate) const fn lane_of(t: Ty) -> Lane {
    match t {
        Ty::Float => Lane::F64,
        _ => Lane::I64,
    }
}

const fn scalar(t: Ty, q: Qual) -> TypeQ {
    TypeQ::scalar(t, q)
}

/// The compilation's knobs lowering reads (§4.14.5): a creator's `__cap`, `sym_precision`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Knobs<'a> {
    /// `CompileLimits::collection_cap`.
    pub(crate) cap: u32,
    /// `ChartInfo::mintick_1e6`.
    pub(crate) mintick_1e6: i64,
    /// `ChartInfo::pointvalue_1e6` (`syminfo.pointvalue`, §4.17).
    pub(crate) pointvalue_1e6: i64,
    /// `ChartInfo::timeframe_s` (`timeframe.multiplier` / `timeframe.is*`, §4.17).
    pub(crate) timeframe_s: u32,
    /// The folded strings (§4.17), interned before lowering — [`FOLD_STR_TICKERID`]: `symbol_key`,
    /// then `syminfo_strs[0..4]`, then `timeframe.period`; `Sym::NONE` when the program reads none.
    pub(crate) strs: [Sym; 6],
    /// The interned target texts that resolve to the chart's own symbol — `symbol_key` and every
    /// `ticker_map` ticker mapped to it (as 5a's `target` resolves them) — for the child's
    /// capture defaults (`pin:02-child-capture-decl-defaults`).
    pub(crate) own: &'a [Sym],
}

/// [`Knobs::strs`]' index of `syminfo.tickerid`; `ticker` … `type` follow, `timeframe.period` last.
pub(crate) const FOLD_STR_TICKERID: usize = 0;
/// [`Knobs::strs`]' index of `timeframe.period`.
pub(crate) const FOLD_STR_PERIOD: usize = 5;

/// The `K` of a `ValueSrc::Fold` row (§4.17, impl-00 §3.9) from the compilation's knobs, with
/// [`crate::ir::CTX_SYMINFO`] / [`crate::ir::CTX_TIMEFRAME`] for §4.11.4's `3029`. `None` for a
/// code the table does not know or a timeframe `tf_parts` refuses (pass 0 checked it).
pub(crate) fn fold_value(bits: u64, k: &Knobs<'_>) -> Option<(K, u8)> {
    use pine_spec::Fold as F;
    use pine_std::TfUnit as U;
    let code = bits as u8;
    let sym = crate::ir::CTX_SYMINFO;
    let tf = crate::ir::CTX_TIMEFRAME;
    let s = |i: usize| (k.strs[i] != Sym::NONE).then_some(K::Str(k.strs[i]));
    if code == F::SyminfoMintick as u8 {
        return Some((K::Float(pine_ir::scale::from_1e6(k.mintick_1e6)), sym));
    }
    if code == F::SyminfoPointvalue as u8 {
        return Some((K::Float(pine_ir::scale::from_1e6(k.pointvalue_1e6)), sym));
    }
    if (F::SyminfoTickerid as u8..=F::SyminfoType as u8).contains(&code) {
        let i = FOLD_STR_TICKERID + usize::from(code - F::SyminfoTickerid as u8);
        return s(i).map(|v| (v, sym));
    }
    if code == F::TimeframePeriod as u8 {
        return s(FOLD_STR_PERIOD).map(|v| (v, tf));
    }
    let p = pine_std::tf_parts(k.timeframe_s)?;
    let is = |units: &[U]| K::Bool(units.contains(&p.unit));
    let v = match code {
        c if c == F::TimeframeMultiplier as u8 => K::Int(i64::from(p.mult)),
        c if c == F::TimeframeIsSeconds as u8 => is(&[U::Second]),
        c if c == F::TimeframeIsMinutes as u8 => is(&[U::Minute]),
        // §4.17: `isintraday` = seconds or minutes; `isdwm` = day, week or month.
        c if c == F::TimeframeIsIntraday as u8 => is(&[U::Second, U::Minute]),
        c if c == F::TimeframeIsDaily as u8 => is(&[U::Day]),
        c if c == F::TimeframeIsWeekly as u8 => is(&[U::Week]),
        c if c == F::TimeframeIsMonthly as u8 => is(&[U::Month]),
        c if c == F::TimeframeIsDwm as u8 => is(&[U::Day, U::Week, U::Month]),
        _ => return None,
    };
    Some((v, tf))
}

/// A §4.14.6 rule: a built-in call lowered to opcodes instead of `CALL_BUILTIN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpRule {
    /// `na(x)` → `NA_CHECK`.
    Na,
    /// `nz(x[, y])` → `NZ_F` / `NZ_I`.
    Nz,
    /// `int(float)` → `I_F2I` (or `$op.int_strict` under `IntOfNa = fault`).
    IntOfFloat,
    /// `float(int)` → `F_I2F`.
    FloatOfInt,
    /// `bool(int | float)` → `I_NE` / `F_NE` against zero.
    BoolOfNum,
    /// `math.abs` / `math.sqrt` of one argument.
    AbsSqrt,
    /// `math.pow` / two-argument `math.max` / `math.min`.
    PowMaxMin,
    /// `math.round(float)` under `RoundTies = half_away`.
    Round,
}

/// The §4.14.6 rule of a call of `full` with `n` arguments, the first of type `t0`, if any —
/// the one table [`Lower`]'s calls and `lowering_table_covers_folded_rows` both read.
pub(crate) fn op_rule(full: &str, n: usize, t0: Option<Ty>, rd: &Readings) -> Option<OpRule> {
    Some(match (full, n) {
        ("na", 1) => OpRule::Na,
        ("nz", 1 | 2) => OpRule::Nz,
        ("int", 1) if t0 == Some(Ty::Float) => OpRule::IntOfFloat,
        ("float", 1) if t0 == Some(Ty::Int) => OpRule::FloatOfInt,
        ("bool", 1) if matches!(t0, Some(Ty::Int | Ty::Float)) => OpRule::BoolOfNum,
        ("math.abs" | "math.sqrt", 1) => OpRule::AbsSqrt,
        ("math.pow" | "math.max" | "math.min", 2) => OpRule::PowMaxMin,
        ("math.round", 1)
            if rd.round_ties == swr::ROUND_TIES_HALF_AWAY && t0 == Some(Ty::Float) =>
        {
            OpRule::Round
        }
        _ => return None,
    })
}

/// Lower the root Instantiation's top level.
#[allow(
    clippy::too_many_arguments,
    reason = "the passes' products and the compilation's knobs"
)]
pub(crate) fn lower(
    t: &Typed,
    tree: &Tree,
    res: &Resolved,
    files: &[Option<&Ast>],
    syms: &SymTable,
    rd: &Readings,
    d: Dialect,
    knobs: Knobs<'_>,
) -> Result<Ir, Diag> {
    let ast = files[0].ok_or_else(|| {
        diag(
            ReasonCode::Internal,
            Span::default(),
            "no root Ast".to_owned(),
        )
    })?;
    let mut lw = Lower {
        t,
        tree,
        files,
        res,
        ast,
        syms,
        rd,
        d,
        inst: InstId(0),
        ti: tree.nodes.first().map_or(t.root, |n| n.type_inst as usize),
        ir: Ir::default(),
        vars: HashMap::new(),
        globals: HashMap::new(),
        depth: 0,
        params: Vec::new(),
        out: Vec::new(),
        prelude: Vec::new(),
        region: 0,
        regions: 0,
        declared: Vec::new(),
        knobs,
        hlines: Vec::new(),
        obj_sites: 0,
        has_requests: t.insts.iter().any(|i| {
            i.calls
                .iter()
                .any(|c| matches!(c.1, Callee::Builtin(ov) if crate::security::is_security(ov)))
        }),
    };
    if let Node::Script { body } = ast.node(ast.root()) {
        for &s in body {
            lw.stmt(s)?;
        }
    }
    let mut ir = lw.ir;
    let mut body = lw.prelude;
    body.extend(lw.out);
    ir.body = Block(body);
    // §4.14.8: every `hline` after every other index — shift their `__plot` past the others.
    let base = ir.plots.len() as u64;
    fix_hlines(&mut ir.body, base, d);
    ir.plots.extend(lw.hlines);
    Ok(ir)
}

fn fix_hlines(b: &mut Block, base: u64, d: Dialect) {
    for s in &mut b.0 {
        match s {
            Stmt::Op(Op::Call { ov, args, .. }) => {
                let o = pine_spec::overload(*ov);
                if o.plot != Some(pine_ir::PlotKind::Hline) {
                    continue;
                }
                let at = pine_spec::param_index(o, "__plot", d).map(usize::from);
                if let Some(Arg::Const { bits, .. }) = at.and_then(|k| args.get_mut(k)) {
                    *bits += base;
                }
            }
            Stmt::If { then, els, .. } => {
                fix_hlines(then, base, d);
                fix_hlines(els, base, d);
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                fix_hlines(head, base, d);
                fix_hlines(body, base, d);
                fix_hlines(step, base, d);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    fix_hlines(c, base, d);
                }
            }
            _ => {}
        }
    }
}

struct Lower<'a> {
    t: &'a Typed,
    tree: &'a Tree,
    files: &'a [Option<&'a Ast>],
    res: &'a Resolved,
    ast: &'a Ast,
    syms: &'a SymTable,
    rd: &'a Readings,
    d: Dialect,
    inst: InstId,
    /// The TypeInst being lowered.
    ti: usize,
    ir: Ir,
    /// `var id` → its slot (this Instantiation's).
    vars: HashMap<u32, VSlot>, // INDEX-ONLY
    /// The root Instantiation's `vars` while a user call is inlined (CI-012, F030 F051–F053): a
    /// function body reads the script's global variables (Pine's scope rule); `vars` then holds
    /// the callee's own. Empty at depth 0.
    globals: HashMap<u32, VSlot>, // INDEX-ONLY
    /// User-call inlining depth.
    depth: u32,
    /// This Instantiation's parameters, by index (§4.6.2: by value, the argument's own slot).
    params: Vec<VSlot>,
    /// The block being built.
    out: Vec<Stmt>,
    /// §4.14.1's prelude: one `CALL_BUILTIN` per referenced `ValueSrc::Call` value, in first-read
    /// order, every argument defaulted; placed before the body.
    prelude: Vec<Stmt>,
    /// The region being built (0 = the top level).
    region: u32,
    /// Regions opened so far.
    regions: u32,
    /// The plain variables declared in the current region and the regions inside it (U16).
    declared: Vec<VSlot>,
    /// The compilation's knobs.
    knobs: Knobs<'a>,
    /// `hline` plot sites in code order (indexed after every other plot, §4.14.8).
    hlines: Vec<(pine_ir::PlotKind, Span)>,
    /// Object sites so far, in code order over the expanded program (`pin:02-obj-site-order` —
    /// T14's numbering keeps this order).
    obj_sites: u32,
    /// The program holds a `request.security` (folded context values then keep a slot, §4.11.4).
    has_requests: bool,
}

impl Lower<'_> {
    fn cur(&self) -> &Inst {
        &self.t.insts[self.ti]
    }

    fn todo(&self, n: NodeId, what: &str) -> Diag {
        let msg = format!("not implemented: lowering {what}");
        diag(ReasonCode::Internal, self.ast.span(n), msg)
    }

    fn ty(&self, n: NodeId) -> Option<TypeQ> {
        self.cur().ty.get(n.0 as usize).copied().flatten()
    }

    fn info(&self, ty: TypeQ, kind: SlotKind, name: Sym, span: Span) -> SlotInfo {
        SlotInfo {
            ty,
            lane: lane_of(ty.ty),
            kind,
            inst: self.inst.0,
            region: self.region,
            name,
            span,
            cell: false,
        }
    }

    fn temp(&mut self, ty: TypeQ, span: Span) -> VSlot {
        let i = self.info(ty, SlotKind::Temp, Sym::NONE, span);
        self.ir.slot(i)
    }

    fn op(&mut self, o: Op) {
        self.out.push(Stmt::Op(o));
    }

    // ---- statements -------------------------------------------------------------------------

    fn stmt(&mut self, n: NodeId) -> Result<(), Diag> {
        match self.ast.node(n) {
            Node::VarDecl {
                mode, name, init, ..
            } => self.var_decl(n, mode, name, init),
            Node::Assign { op, target, value } => self.assign(n, op, target, value),
            Node::TupleDecl { binders, init } => self.tuple_bind(n, binders, init, true),
            Node::TupleAssign { targets, value } => self.tuple_bind(n, targets, value, false),
            Node::FuncDecl { .. }
            | Node::TypeDecl { .. }
            | Node::EnumDecl { .. }
            | Node::Import { .. } => Ok(()),
            Node::Break => {
                self.out.push(Stmt::Break);
                Ok(())
            }
            Node::Continue => {
                self.out.push(Stmt::Continue);
                Ok(())
            }
            Node::If { .. }
            | Node::Switch { .. }
            | Node::Ternary { .. }
            | Node::For { .. }
            | Node::ForIn { .. }
            | Node::While { .. }
            | Node::Once { .. }
            | Node::Block { .. } => self.structure(n, false).map(|_| ()),
            Node::Call { .. }
            | Node::Binary { .. }
            | Node::Unary { .. }
            | Node::Ident { .. }
            | Node::Field { .. }
            | Node::Hist { .. } => self.expr(n).map(|_| ()),
            _ => Err(self.todo(n, "this statement (T12)")),
        }
    }

    /// A declared variable's type (pass 3's `locals`), else its initialiser's.
    fn local_ty(&self, id: u32, init: NodeId) -> Option<TypeQ> {
        self.cur()
            .locals
            .iter()
            .find(|l| l.0 == id)
            .map(|l| l.1)
            .or_else(|| self.ty(init))
    }

    fn var_decl(
        &mut self,
        n: NodeId,
        mode: DeclMode,
        name: NodeId,
        init: NodeId,
    ) -> Result<(), Diag> {
        let Some(Binding::Var { id, .. }) = self.binding(name) else {
            return Err(self.todo(n, "a declaration without a variable binding"));
        };
        let ty = self
            .local_ty(id, init)
            .ok_or_else(|| self.todo(n, "an untyped initialiser"))?;
        let kind = if mode == DeclMode::Varip {
            SlotKind::Varip
        } else {
            SlotKind::Var
        };
        let sym = self.ast.sym(name);
        let info = self.info(ty, kind, sym, self.ast.span(name));
        let v = self.ir.slot(info);
        if mode == DeclMode::Plain {
            let src = self.value_as(init, ty)?;
            self.store(v, src);
            self.vars.insert(id, v);
            if self.region != 0 {
                self.declared.push(v);
            }
            return Ok(());
        }
        // §4.14.4 `var` / `varip`: a Guard region `JNZ f → skip; <init>; store; f ← 1; skip:`,
        // one Flag per declaration per Instantiation, `varip` iff the declaration is. A `var`
        // keeps its value on skipped evaluations by definition, so it is never a U16 block-local.
        let flag = self.flag(mode == DeclMode::Varip, self.ast.span(n));
        self.ir.var_decls.push((v, flag));
        let ((), els, _) = self.region(|s| {
            let src = s.value_as(init, ty)?;
            s.store(v, src);
            s.set_flag(flag);
            Ok(())
        })?;
        self.vars.insert(id, v);
        self.out.push(Stmt::If {
            cond: flag,
            then: Block::default(),
            els,
            kind: CondKind::Guard,
            na_else: true,
        });
        Ok(())
    }

    /// `:=` stores into the variable's slot; `x op= e` is `x := x op e` (§4.14.4).
    fn assign(
        &mut self,
        n: NodeId,
        op: AssignOp,
        target: NodeId,
        value: NodeId,
    ) -> Result<(), Diag> {
        if let Node::Field { base, member } = self.ast.node(target) {
            if self.ty(base).is_some_and(|t| t.ty == Ty::Udt) {
                return self.field_store(n, op, base, member, value);
            }
        }
        let Some(Binding::Var { id, .. }) = self.binding(target) else {
            return Err(self.todo(n, "an assignment to a non-variable"));
        };
        let Some(&v) = self.vars.get(&id) else {
            return Err(self.todo(n, "an assignment to an outer variable"));
        };
        let vt = self.slot_ty(v);
        let bop = match op {
            AssignOp::Reassign => None,
            AssignOp::Add => Some(BinOp::Add),
            AssignOp::Sub => Some(BinOp::Sub),
            AssignOp::Mul => Some(BinOp::Mul),
            AssignOp::Div => Some(BinOp::Div),
            AssignOp::Rem => Some(BinOp::Rem),
        };
        let src = match bop {
            None => self.value_as(value, vt)?,
            Some(b) => {
                let e = self.expr(value)?;
                let et = self.slot_ty(e);
                let r = self.apply(b, v, e, Some(vt), Some(et), vt, self.ast.span(n))?;
                self.widen_slot(r, vt.ty)
            }
        };
        self.store(v, src);
        Ok(())
    }

    fn binding(&self, n: NodeId) -> Option<Binding> {
        let f = self.cur().file;
        self.res.uses[f].get(n.0 as usize).copied().flatten()
    }

    // ---- expressions ------------------------------------------------------------------------

    /// The slot holding `n`'s value.
    fn expr(&mut self, n: NodeId) -> Result<VSlot, Diag> {
        let ty = self.ty(n).ok_or_else(|| self.todo(n, "an untyped node"))?;
        if let Some(k) = self.cur().k.get(n.0 as usize).copied().flatten() {
            let c = self.konst(k, ty, self.ast.span(n));
            // §4.11.4 `3029`: a folded `syminfo.*` / `timeframe.*` value keeps a slot of its own
            // in a program with a request, so 5a can tell the context it was folded in.
            if self.has_requests {
                if let Some(kind) = self.ctx_kind(n) {
                    let t = self.temp(ty, self.ast.span(n));
                    self.op(Op::Mov { dst: t, a: c });
                    self.ir.ctx_slots.push((t, kind));
                    return Ok(t);
                }
            }
            return Ok(c);
        }
        let at = self.ast.span(n);
        match self.ast.node(n) {
            Node::Field { base, member } if self.ty(base).is_some_and(|t| t.ty == Ty::Udt) => {
                let h = self.expr(base)?;
                let ut = self.slot_ty(h);
                let (field, _) = self.field_of(ut.a, self.ast.sym(member), n)?;
                let dst = self.temp(ty, at);
                self.op(Op::ObjGet { dst, h, field });
                Ok(dst)
            }
            Node::Ident { .. } | Node::Field { .. } => match self.binding(n) {
                Some(Binding::Var { id, .. } | Binding::Counter { id }) => self
                    .var_slot(id)
                    .ok_or_else(|| self.todo(n, "a variable of another scope")),
                Some(Binding::Param { index }) => self
                    .params
                    .get(index as usize)
                    .copied()
                    .ok_or_else(|| self.todo(n, "an unbound parameter")),
                Some(_) => Err(self.todo(n, "this name (T12)")),
                None => self.builtin_value(n, ty),
            },
            Node::Unary { op, operand } => {
                let a = self.expr(operand)?;
                let k = match op {
                    UnOp::Neg => UnOpK::Neg,
                    UnOp::Not => UnOpK::Not,
                    UnOp::Pos => return Ok(a),
                };
                let dst = self.temp(ty, at);
                self.op(Op::Un { op: k, dst, a });
                Ok(dst)
            }
            Node::Binary { op, lhs, rhs } => self.binary(n, op, lhs, rhs, ty),
            Node::Call { .. } => self.call(n, ty),
            Node::Hist { base, offset } => self.hist(n, base, offset, ty),
            Node::If { .. }
            | Node::Switch { .. }
            | Node::Ternary { .. }
            | Node::For { .. }
            | Node::ForIn { .. }
            | Node::While { .. }
            | Node::Once { .. }
            | Node::Block { .. } => self.structure(n, true),
            _ => Err(self.todo(n, "this expression (T12)")),
        }
    }

    /// The `Const` slot of a folded value (§4.14.1). A string constant carries its `Sym`; emission
    /// interns it as a `CONST` `StrRef` (T19).
    fn konst(&mut self, k: K, ty: TypeQ, span: Span) -> VSlot {
        let (lane, bits, valid) = match k {
            K::Int(i) | K::Color(i) => (Lane::I64, i as u64, true),
            K::Float(f) => (Lane::F64, f.to_bits(), true),
            K::Bool(b) => (Lane::I64, u64::from(b), true),
            K::Str(s) => (Lane::I64, u64::from(s.0), true),
            K::Na => (lane_of(ty.ty), 0, false),
        };
        let ty = TypeQ {
            qual: Qual::Const,
            ..ty
        };
        let info = SlotInfo {
            lane,
            ..self.info(ty, SlotKind::Const, Sym::NONE, span)
        };
        self.ir.konst(info, bits, valid)
    }

    /// A built-in value read: a reserved slot or a prelude slot, one per row program-wide.
    fn builtin_value(&mut self, n: NodeId, ty: TypeQ) -> Result<VSlot, Diag> {
        let path = self.path(n).ok_or_else(|| self.todo(n, "a name"))?;
        self.value_named(n, &path, ty)
    }

    fn value_named(&mut self, n: NodeId, path: &str, ty: TypeQ) -> Result<VSlot, Diag> {
        let Some(e) = pine_spec::lookup(path) else {
            return Err(self.todo(n, "an unknown value"));
        };
        let base = e.val_lo as u32;
        let Some((i, v)) = e
            .values()
            .iter()
            .enumerate()
            .find(|(_, v)| v.versions.has(self.d))
        else {
            return Err(self.todo(n, "a value outside the dialect"));
        };
        let kind = match v.src {
            ValueSrc::Slot => SlotKind::Reserved {
                slot: v.bits as u16,
            },
            ValueSrc::Call => SlotKind::Prelude {
                value: base + i as u32,
            },
            ValueSrc::Fold => return self.fold_slot(v.bits, ty, self.ast.span(n)),
            ValueSrc::Const => return Err(self.todo(n, "a constant row that did not fold")),
        };
        let info = self.info(ty, kind, Sym::NONE, self.ast.span(n));
        self.value_slot(n, base + i as u32, info)
    }

    /// A `ValueSrc::Fold` row's value (§4.17): its constant — in a program with a request, moved
    /// into a slot of its own recorded in `ir.ctx_slots` (§4.11.4 `3029`, as [`Self::expr`] does
    /// for a folded subtree).
    fn fold_slot(&mut self, bits: u64, ty: TypeQ, at: Span) -> Result<VSlot, Diag> {
        let Some((k, kind)) = fold_value(bits, &self.knobs) else {
            let msg = format!("value fold code {bits} did not fold");
            return Err(diag(ReasonCode::Internal, at, msg));
        };
        let c = self.konst(k, ty, at);
        if !self.has_requests {
            return Ok(c);
        }
        let t = self.temp(ty, at);
        self.op(Op::Mov { dst: t, a: c });
        self.ir.ctx_slots.push((t, kind));
        Ok(t)
    }

    /// [`crate::ir::CTX_SYMINFO`] / [`crate::ir::CTX_TIMEFRAME`] when the folded subtree of `n`
    /// reads a `syminfo.*` / `timeframe.*` value.
    fn ctx_kind(&self, n: NodeId) -> Option<u8> {
        let mut kind = 0u8;
        for m in self.ast.subtree(n) {
            let m = NodeId(m);
            if !matches!(self.ast.node(m), Node::Field { .. }) {
                continue;
            }
            match self.path(m) {
                Some(p) if p.starts_with("syminfo.") => kind |= crate::ir::CTX_SYMINFO,
                Some(p) if p.starts_with("timeframe.") => kind |= crate::ir::CTX_TIMEFRAME,
                _ => {}
            }
        }
        (kind != 0).then_some(kind)
    }

    fn path(&self, n: NodeId) -> Option<String> {
        match self.ast.node(n) {
            Node::Ident { sym } => Some(self.syms.resolve(sym).to_owned()),
            Node::Field { base, member } => {
                let mut s = self.path(base)?;
                s.push('.');
                s.push_str(self.syms.resolve(self.ast.sym(member)));
                Some(s)
            }
            _ => None,
        }
    }

    /// `int` → `float` when `to` is `float` and `from` is `int` (§4.14.2 "mixed int / float").
    fn widen(&mut self, v: VSlot, from: Option<TypeQ>, to: Ty) -> VSlot {
        match from {
            Some(f) if f.ty == Ty::Int && to == Ty::Float => {
                let span = self
                    .ir
                    .slots
                    .get(v.0 as usize)
                    .map_or(Span::default(), |s| s.span);
                let dst = self.temp(scalar(Ty::Float, f.qual), span);
                self.op(Op::Un {
                    op: UnOpK::I2f,
                    dst,
                    a: v,
                });
                dst
            }
            _ => v,
        }
    }

    fn call_op(&mut self, name: &str, args: &[VSlot], ty: TypeQ, at: Span) -> VSlot {
        let ov = pine_spec::lookup(name)
            .filter(|e| e.ov_lo < e.ov_hi)
            .map_or(OverloadId(u16::MAX), |e| OverloadId(e.ov_lo));
        let dst = self.temp(ty, at);
        let args = args.iter().map(|&a| Arg::Slot(a)).collect();
        self.op(Op::Call {
            ov,
            args,
            outs: vec![dst],
            span: at,
        });
        dst
    }

    fn binary(
        &mut self,
        n: NodeId,
        op: BinOp,
        lhs: NodeId,
        rhs: NodeId,
        ty: TypeQ,
    ) -> Result<VSlot, Diag> {
        if matches!(op, BinOp::And | BinOp::Or) {
            return self.and_or(n, op, lhs, rhs, ty);
        }
        let (lt, rt) = (self.ty(lhs), self.ty(rhs));
        let (a, b) = (self.expr(lhs)?, self.expr(rhs)?);
        self.apply(op, a, b, lt, rt, ty, self.ast.span(n))
    }

    /// A binary operator over two evaluated operands (§4.14.2).
    #[allow(
        clippy::too_many_arguments,
        reason = "the operator, its operands and their types"
    )]
    fn apply(
        &mut self,
        op: BinOp,
        a: VSlot,
        b: VSlot,
        lt: Option<TypeQ>,
        rt: Option<TypeQ>,
        ty: TypeQ,
        at: Span,
    ) -> Result<VSlot, Diag> {
        let is = |t: Option<TypeQ>, x: Ty| t.is_some_and(|t| t.ty == x);
        let na_nonfinite = self.rd.float_div_zero == swr::FLOAT_DIV_ZERO_NA;
        // `+` on strings: `$op.add_str` (a scratch result).
        if op == BinOp::Add && (is(lt, Ty::String) || is(rt, Ty::String)) {
            return Ok(self.call_op("$op.add_str", &[a, b], ty, at));
        }
        // The operand lane: float when either side is float, or for `/` of two ints (W1: a
        // `const_trunc` pair of constants already folded, §4.17; `fraction` always widens).
        // A `const_trunc` pair infer typed `int` without a value (`timestamp(…)` of literals:
        // `const`, not folded) stays in the int lane — `I_DIV`, truncating like `fold::i_div`
        // (§4.14.2's "both operands `const` → truncating, type `int`"); widening it fed `I_DIV`
        // two f64 entries (r21 I9 F093: v5 `na(x) ? na : x` plotted na for `x` = 0).
        let int_div = op == BinOp::Div && ty.ty == Ty::Int && is(lt, Ty::Int) && is(rt, Ty::Int);
        let float = is(lt, Ty::Float) || is(rt, Ty::Float) || (op == BinOp::Div && !int_div);
        let numeric =
            (is(lt, Ty::Int) || is(lt, Ty::Float)) && (is(rt, Ty::Int) || is(rt, Ty::Float));
        let (a, b) = if numeric && float {
            (self.widen(a, lt, Ty::Float), self.widen(b, rt, Ty::Float))
        } else {
            (a, b)
        };
        let cmp = |o: BinOp| match o {
            BinOp::Lt => Some(CmpK::Lt),
            BinOp::Le => Some(CmpK::Le),
            BinOp::Gt => Some(CmpK::Gt),
            BinOp::Ge => Some(CmpK::Ge),
            BinOp::Eq => Some(CmpK::Eq),
            BinOp::Ne => Some(CmpK::Ne),
            _ => None,
        };
        if let Some(c) = cmp(op) {
            let eqne = matches!(c, CmpK::Eq | CmpK::Ne);
            if eqne && is(lt, Ty::String) {
                let f = if c == CmpK::Eq {
                    "$op.eq_str"
                } else {
                    "$op.ne_str"
                };
                return Ok(self.call_op(f, &[a, b], ty, at));
            }
            if eqne
                && numeric
                && float
                && self.d == Dialect::V6
                && self.rd.float_eq_v6 == swr::FLOAT_EQ_V6_ROUND9
            {
                // U06 `round9`.
                let f = if c == CmpK::Eq {
                    "$op.eq_round9"
                } else {
                    "$op.ne_round9"
                };
                return Ok(self.call_op(f, &[a, b], ty, at));
            }
            if c == CmpK::Ne && self.rd.pins.na_compare == NaCompare::NeTrue {
                // pin:06-na-compare `NeTrue`: `B_NOT` of the `==` result.
                let eq = self.temp(ty, at);
                self.op(Op::Cmp {
                    op: CmpK::Eq,
                    dst: eq,
                    a,
                    b,
                });
                let dst = self.temp(ty, at);
                self.op(Op::Un {
                    op: UnOpK::Not,
                    dst,
                    a: eq,
                });
                return Ok(dst);
            }
            let dst = self.temp(ty, at);
            self.op(Op::Cmp { op: c, dst, a, b });
            return Ok(dst);
        }
        let k = match op {
            BinOp::Add => BinOpK::Add,
            BinOp::Sub => BinOpK::Sub,
            BinOp::Mul => BinOpK::Mul,
            BinOp::Div => BinOpK::Div {
                na_on_nonfinite: na_nonfinite,
            },
            // CAR-R31-1: truncated (the VM's `F_MOD` / `I_MOD`); U15 on the float form only.
            BinOp::Rem => BinOpK::Mod {
                na_on_nonfinite: float && na_nonfinite,
            },
            _ => {
                let msg = "not implemented: lowering this operator".to_owned();
                return Err(diag(ReasonCode::Internal, at, msg));
            }
        };
        if let Some(v) = self.fold_slots(k, a, b, ty, at) {
            return Ok(v);
        }
        let dst = self.temp(ty, at);
        self.op(Op::Bin { op: k, dst, a, b });
        Ok(dst)
    }

    /// `pin:02-site-const-fold` — `+ − *` over two valid `Const` slots folds at the call site
    /// (an inlined `const` parameter's value, §4.7.2: `ta.sma(src, len * 2)` under `len = 10`
    /// sizes a 20-sample window). Int: wrapping (`pin:06-int-width`); float: IEEE, as the VM's
    /// `F_ADD`/`F_SUB`/`F_MUL`. `/` and `%` keep their opcodes (W1, M1, U15).
    fn fold_slots(&mut self, k: BinOpK, a: VSlot, b: VSlot, ty: TypeQ, at: Span) -> Option<VSlot> {
        let kind = |s: &Self, v: VSlot| s.ir.slots.get(v.0 as usize).map(|x| (x.kind, x.lane));
        let (Some((SlotKind::Const, la)), Some((SlotKind::Const, lb))) =
            (kind(self, a), kind(self, b))
        else {
            return None;
        };
        let ((xa, true), (xb, true)) = (self.ir.const_of(a)?, self.ir.const_of(b)?) else {
            return None;
        };
        let r = match (la, lb, ty.ty) {
            (Lane::I64, Lane::I64, Ty::Int) => {
                let (x, y) = (xa as i64, xb as i64);
                K::Int(match k {
                    BinOpK::Add => x.wrapping_add(y),
                    BinOpK::Sub => x.wrapping_sub(y),
                    BinOpK::Mul => x.wrapping_mul(y),
                    _ => return None,
                })
            }
            (Lane::F64, Lane::F64, Ty::Float) => {
                let (x, y) = (f64::from_bits(xa), f64::from_bits(xb));
                K::Float(match k {
                    BinOpK::Add => x + y,
                    BinOpK::Sub => x - y,
                    BinOpK::Mul => x * y,
                    _ => return None,
                })
            }
            _ => return None,
        };
        Some(self.konst(r, ty, at))
    }

    /// §4.6.2: a user-function call expanded in place under its Instantiation. Arguments are
    /// evaluated in the caller, left to right, widened to the parameter's type, and bound by
    /// value (the argument's own slot: parameters are never written, §4.3.2).
    fn inline(&mut self, n: NodeId, ti: usize, ty: TypeQ) -> Result<VSlot, Diag> {
        let v = self.inline_with(n, ti, Self::body)?;
        // The callee's result in the caller's type (an `int` body read as `float`).
        let from = self.t.insts[ti].ret;
        Ok(self.widen(v, Some(from), ty.ty))
    }

    /// Bind a user call's arguments and run `f` over the callee's body under its Instantiation.
    fn inline_with<T>(
        &mut self,
        n: NodeId,
        ti: usize,
        f: impl FnOnce(&mut Self, NodeId) -> Result<T, Diag>,
    ) -> Result<T, Diag> {
        let Some(child) = self.tree.child(self.inst, n) else {
            return Err(self.todo(n, "a call with no Instantiation"));
        };
        let Node::Call {
            callee: fnode,
            args,
            ..
        } = self.ast.node(n)
        else {
            return Err(self.todo(n, "a call node"));
        };
        let callee = &self.t.insts[ti];
        let (Some(decl), Some(cast)) =
            (callee.decl, self.files.get(callee.file).copied().flatten())
        else {
            return Err(self.todo(n, "a callee without a declaration"));
        };
        let Node::FuncDecl {
            params,
            body,
            method,
            ..
        } = cast.node(decl)
        else {
            return Err(self.todo(n, "a callee that is not a function"));
        };
        // CI-012 (C007 C014 C021 C033): `recv.m(…)` binds the receiver to the method's first
        // parameter, as infer's `method_call` does (it inserts the receiver as argument 0); a
        // library path `lib.m(x)` is a function-style call.
        let recv = match self.ast.node(fnode) {
            Node::Field { base, .. }
                if method && !matches!(self.binding(base), Some(Binding::Lib { .. })) =>
            {
                Some(base)
            }
            _ => None,
        };
        let off = usize::from(recv.is_some());
        // Positional arguments bind in order, named ones by name; an omitted parameter takes its
        // declared default, folded in the callee's TypeInst (§4.6.2). Arguments are evaluated in
        // source order (the receiver first).
        let mut by_param: Vec<Option<NodeId>> = vec![None; params.len()];
        if let (Some(b), Some(p0)) = (recv, by_param.first_mut()) {
            *p0 = Some(b);
        }
        for (k, &a) in args.iter().enumerate() {
            let (slot, v) = match self.ast.node(a) {
                Node::NamedArg { name, value } => {
                    let want = self.ast.sym(name);
                    let at = params.iter().position(|&p| match cast.node(p) {
                        Node::Param { name: pn, .. } => {
                            self.syms.resolve(cast.sym(pn)) == self.syms.resolve(want)
                        }
                        _ => false,
                    });
                    (at, value)
                }
                _ => (Some(k + off), a),
            };
            match slot.and_then(|i| by_param.get_mut(i)) {
                Some(b) => *b = Some(v),
                None => return Err(self.todo(n, "an argument without a parameter")),
            }
        }
        let mut vals: Vec<Option<VSlot>> = vec![None; params.len()];
        for &a in recv.iter().chain(args) {
            let v = match self.ast.node(a) {
                Node::NamedArg { value, .. } => value,
                _ => a,
            };
            let Some(i) = by_param.iter().position(|&b| b == Some(v)) else {
                continue;
            };
            let to = callee
                .key
                .get(i)
                .copied()
                .unwrap_or(scalar(Ty::Void, Qual::Const));
            vals[i] = Some(self.value_as(v, to)?);
        }
        let mut bound = Vec::with_capacity(params.len());
        for (i, &p) in params.iter().enumerate() {
            if let Some(v) = vals[i] {
                bound.push(v);
                continue;
            }
            let to = callee
                .key
                .get(i)
                .copied()
                .unwrap_or(scalar(Ty::Void, Qual::Const));
            let def = match cast.node(p) {
                Node::Param { default, .. } => default,
                _ => None,
            };
            let k = def.and_then(|d| {
                callee
                    .k
                    .get(d.0 as usize)
                    .copied()
                    .flatten()
                    .or_else(|| self.literal_k(cast, d))
            });
            match (def, k) {
                (Some(d), Some(k)) => {
                    let v = self.konst(k, to, cast.span(d));
                    bound.push(self.widen(v, Some(to), to.ty));
                }
                _ => return Err(self.todo(n, "a parameter default that does not fold")),
            }
        }
        let mut outer = std::mem::take(&mut self.vars);
        if self.depth == 0 {
            // The caller is the script's top level: its variables are the globals.
            self.globals = std::mem::take(&mut outer);
        }
        let saved = (
            self.inst,
            self.ti,
            self.ast,
            outer,
            std::mem::replace(&mut self.params, bound),
        );
        self.inst = child;
        self.ti = ti;
        self.ast = cast;
        self.depth += 1;
        let r = f(self, body);
        self.depth -= 1;
        self.inst = saved.0;
        self.ti = saved.1;
        self.ast = saved.2;
        self.vars = if self.depth == 0 {
            std::mem::take(&mut self.globals)
        } else {
            saved.3
        };
        self.params = saved.4;
        r
    }

    /// A variable's slot: this Instantiation's, else (inside a user call of the root file) the
    /// script's global (CI-012, `pin:W3-lazy-and-or` fixtures F030 F051–F053).
    fn var_slot(&self, id: u32) -> Option<VSlot> {
        let own = self.vars.get(&id).copied();
        let root_file = self.cur().file == 0;
        own.or_else(|| (root_file && self.depth > 0).then(|| self.globals.get(&id).copied())?)
    }

    /// A tuple body: its statements, then the last one's elements.
    fn body_tuple(&mut self, b: NodeId) -> Result<Vec<VSlot>, Diag> {
        let ast = self.ast;
        let one = [b];
        let stmts: &[NodeId] = match ast.node(b) {
            Node::Block { stmts } => stmts,
            _ => &one,
        };
        let Some((&last, init)) = stmts.split_last() else {
            return Err(self.todo(b, "an empty tuple body"));
        };
        for &st in init {
            self.stmt(st)?;
        }
        let hints = self.t.insts[self.ti].ret_tuple.clone();
        self.tuple_values(last, &hints)
    }

    /// The elements of a tuple-valued expression: `[a, b]`, a tuple-returning built-in (one call,
    /// `n` outputs) or user function (§4.14.4 "Tuples"). `hints` types a type-variable output.
    fn tuple_values(&mut self, n: NodeId, hints: &[TypeQ]) -> Result<Vec<VSlot>, Diag> {
        match self.ast.node(n) {
            Node::Tuple { items } => {
                let mut v = Vec::with_capacity(items.len());
                for (k, &i) in items.iter().enumerate() {
                    let x = match hints.get(k) {
                        Some(&t) => self.value_as(i, t)?,
                        None => self.expr(i)?,
                    };
                    v.push(x);
                }
                Ok(v)
            }
            Node::Call { .. } => {
                let callee = self.cur().calls.iter().find(|c| c.0 == n).map(|c| c.1);
                match callee {
                    Some(Callee::User(ti)) => self.inline_with(n, ti as usize, Self::body_tuple),
                    Some(Callee::Builtin(ov)) if crate::security::is_security(ov) => {
                        self.security(n, ov, hints)
                    }
                    Some(Callee::Builtin(ov)) => {
                        let o = pine_spec::overload(ov);
                        let rets = pine_spec::rets(o);
                        let mut tys = Vec::with_capacity(rets.len());
                        for (k, r) in rets.iter().enumerate() {
                            let t = if r.tv == 0 {
                                TypeQ {
                                    qual: Qual::Series,
                                    ..r.ty
                                }
                            } else {
                                match hints.get(k) {
                                    Some(&t) => t,
                                    None => return Err(self.todo(n, "an untyped tuple output")),
                                }
                            };
                            tys.push(t);
                        }
                        self.builtin_call(n, ov, &tys)
                    }
                    _ => Err(self.todo(n, "this tuple call (T12)")),
                }
            }
            _ => Err(self.todo(n, "a tuple-valued structure (T12)")),
        }
    }

    /// `[a, b] = e` / `[a, b] := e`: the outputs bound in order; a count mismatch is `3006`.
    fn tuple_bind(
        &mut self,
        n: NodeId,
        names: &[NodeId],
        value: NodeId,
        decl: bool,
    ) -> Result<(), Diag> {
        let hints: Vec<TypeQ> = names
            .iter()
            .map(|&b| match self.binding(b) {
                Some(Binding::Var { id, .. }) => self
                    .cur()
                    .locals
                    .iter()
                    .find(|l| l.0 == id)
                    .map(|l| l.1)
                    .or_else(|| self.vars.get(&id).map(|&v| self.slot_ty(v)))
                    .unwrap_or(scalar(Ty::Void, Qual::Const)),
                _ => scalar(Ty::Void, Qual::Const),
            })
            .collect();
        let vals = self.tuple_values(value, &hints)?;
        if vals.len() != names.len() {
            let msg = format!("{} names for a tuple of {}", names.len(), vals.len());
            return Err(diag(ReasonCode::Arity, self.ast.span(n), msg));
        }
        for (k, &b) in names.iter().enumerate() {
            let Some(Binding::Var { id, .. }) = self.binding(b) else {
                continue; // `_`
            };
            let t = hints[k];
            let v = if decl {
                let info = self.info(t, SlotKind::Var, self.ast.sym(b), self.ast.span(b));
                let v = self.ir.slot(info);
                self.vars.insert(id, v);
                if self.region != 0 {
                    self.declared.push(v);
                }
                v
            } else {
                match self.vars.get(&id) {
                    Some(&v) => v,
                    None => return Err(self.todo(b, "an assignment to an outer variable")),
                }
            };
            let src = self.widen_slot(vals[k], t.ty);
            self.store(v, src);
        }
        Ok(())
    }

    /// A function body: its statements, the last one's value is the result.
    fn body(&mut self, b: NodeId) -> Result<VSlot, Diag> {
        Ok(self.block_value(b, true)?.unwrap_or(VSlot(u32::MAX)))
    }

    /// A built-in call: an opcode rule of §4.14.6, else one `Op::Call` whose arguments are the
    /// given ones in parameter-row order (the windows — synthetics, defaults — are T12's).
    fn call(&mut self, n: NodeId, ty: TypeQ) -> Result<VSlot, Diag> {
        let at = self.ast.span(n);
        let callee = self
            .cur()
            .calls
            .iter()
            .find(|c| c.0 == n)
            .map(|c| c.1)
            .ok_or_else(|| self.todo(n, "an unresolved call"))?;
        let ov = match callee {
            Callee::Builtin(ov) => ov,
            Callee::User(ti) => return self.inline(n, ti as usize, ty),
            Callee::Udt(decl) => return self.udt_call(n, decl, ty),
        };
        if !matches!(self.ast.node(n), Node::Call { .. }) {
            return Err(self.todo(n, "a call node"));
        }
        let given = self.given(n);
        let o = pine_spec::overload(ov);
        let full = pine_spec::entry_of(o).full_str();
        if o.abi == pine_spec::Abi::Folded {
            // §4.14.6's last row: inputs, declarations, risk rules and `max_bars_back` fold.
            // `input.source` aliases the series it names (§4.12.2); so does a bare `input()`
            // over a source-type built-in (pin:02-input-source-defval: typing refused any other
            // non-constant `INPUT_F_SRC` defval with `3008`, so this row reaches here aliased).
            if o.input == Some(pine_ir::InputKind::Source) || ov == pine_spec::ov::INPUT_F_SRC {
                let key = (self.cur().file, n);
                let Some(&(_, name)) = self.t.sources.iter().find(|x| x.0 == key) else {
                    return Err(self.todo(n, "an input.source without an alias"));
                };
                let path = self.syms.resolve(name).to_owned();
                return self.value_named(n, &path, ty);
            }
            if full == "max_bars_back" {
                // §4.10.4: `max_bars_back(x, n)` sets `x`'s own cap (series.rs reads it).
                // The variable and the length, in parameter order (after any synthetic row).
                let x = given.first().map(|g| g.1);
                let k = given
                    .get(1)
                    .and_then(|g| self.cur().k.get(g.1 .0 as usize).copied().flatten());
                if let (Some(x), Some(K::Int(c))) = (x, k) {
                    if let Some(Binding::Var { id, .. }) = self.binding(x) {
                        if let Some(&v) = self.vars.get(&id) {
                            self.ir
                                .var_caps
                                .push((v, c.clamp(0, i64::from(u32::MAX)) as u32));
                        }
                    }
                }
            }
            return Ok(VSlot(u32::MAX)); // a statement: nothing to evaluate
        }
        let arg_ty = |s: &Self, i: usize| given.get(i).and_then(|g| s.ty(g.1));
        let un = |s: &mut Self, k: UnOpK, a: VSlot| {
            let dst = s.temp(ty, at);
            s.op(Op::Un { op: k, dst, a });
            Ok(dst)
        };
        match op_rule(full, given.len(), arg_ty(self, 0).map(|t| t.ty), self.rd) {
            Some(OpRule::Na) => {
                let a = self.expr(given[0].1)?;
                let dst = self.temp(ty, at);
                self.op(Op::NaCheck { dst, a });
                return Ok(dst);
            }
            Some(OpRule::Nz) => {
                let a = self.expr(given[0].1)?;
                let a = self.widen(a, arg_ty(self, 0), ty.ty);
                let b = if given.len() == 2 {
                    let b = self.expr(given[1].1)?;
                    self.widen(b, arg_ty(self, 1), ty.ty)
                } else {
                    // `y` omitted → the parameter's default: `0` in the result's type.
                    let z = if ty.ty == Ty::Float {
                        K::Float(0.0)
                    } else {
                        K::Int(0)
                    };
                    self.konst(z, ty, at)
                };
                let dst = self.temp(ty, at);
                self.op(Op::Nz { dst, a, b });
                return Ok(dst);
            }
            Some(OpRule::IntOfFloat) => {
                let a = self.expr(given[0].1)?;
                if self.rd.int_of_na == swr::INT_OF_NA_FAULT {
                    return Ok(self.call_op("$op.int_strict", &[a], ty, at));
                }
                let round = self.rd.pins.int_cast == IntCast::RoundHalfAway;
                return un(self, UnOpK::F2i { round }, a);
            }
            Some(OpRule::FloatOfInt) => {
                let a = self.expr(given[0].1)?;
                return un(self, UnOpK::I2f, a);
            }
            Some(OpRule::BoolOfNum) => {
                let t0 = arg_ty(self, 0).map_or(Ty::Int, |t| t.ty);
                let a = self.expr(given[0].1)?;
                let zero = if t0 == Ty::Float {
                    K::Float(0.0)
                } else {
                    K::Int(0)
                };
                let z = self.konst(zero, scalar(t0, Qual::Const), at);
                let dst = self.temp(ty, at);
                self.op(Op::Cmp {
                    op: CmpK::Ne,
                    dst,
                    a,
                    b: z,
                });
                return Ok(dst);
            }
            Some(OpRule::AbsSqrt) => {
                let a = self.expr(given[0].1)?;
                let a = self.widen(a, arg_ty(self, 0), ty.ty);
                let k = if full == "math.abs" {
                    UnOpK::Abs
                } else {
                    UnOpK::Sqrt
                };
                return un(self, k, a);
            }
            Some(OpRule::PowMaxMin) => {
                let a = self.expr(given[0].1)?;
                let a = self.widen(a, arg_ty(self, 0), ty.ty);
                let b = self.expr(given[1].1)?;
                let b = self.widen(b, arg_ty(self, 1), ty.ty);
                let k = match full {
                    "math.pow" => BinOpK::Pow {
                        na_on_nonfinite: self.rd.float_div_zero == swr::FLOAT_DIV_ZERO_NA,
                    },
                    "math.max" => BinOpK::Max,
                    _ => BinOpK::Min,
                };
                let dst = self.temp(ty, at);
                self.op(Op::Bin { op: k, dst, a, b });
                return Ok(dst);
            }
            Some(OpRule::Round) => {
                let a = self.expr(given[0].1)?;
                return un(self, UnOpK::F2i { round: true }, a);
            }
            None => {}
        }
        if crate::security::is_security(ov) {
            let outs = self.security(n, ov, &[ty])?;
            return Ok(outs.first().copied().unwrap_or(VSlot(u32::MAX)));
        }
        if pine_spec::rets(o).len() > 1 {
            return Err(self.todo(n, "a tuple-returning call outside a tuple binding"));
        }
        let tys: &[TypeQ] = if ty.ty == Ty::Void { &[] } else { &[ty] };
        let outs = self.builtin_call(n, ov, tys)?;
        // A void call's value is never read; it still names a slot for uniformity.
        Ok(outs.first().copied().unwrap_or(VSlot(u32::MAX)))
    }

    /// Amendment A6.2: a `strategy.entry` / `strategy.order` site whose `id` folds records the
    /// side's bit when `direction` folds, both bits when it does not (`pin:02-entry-dir-int` —
    /// `strategy.long` is the int constant 0 and `strategy.short` 1 in pine-spec; any other
    /// folded int reads as both sides).
    fn entry_site(&mut self, o: &'static pine_spec::Overload, given: &[(u16, NodeId)]) {
        use pine_ir::entry_dir::{LONG, SHORT};
        let full = pine_spec::entry_of(o).full_str();
        if full != "strategy.entry" && full != "strategy.order" {
            return;
        }
        let ps = pine_spec::params(o);
        let row = |name: &str| {
            ps.iter()
                .position(|p| p.versions.has(self.d) && p.name.as_str() == name)
                .map(|r| r as u16)
        };
        let k_of = |s: &Self, r: Option<u16>| {
            let n = given.iter().find(|g| Some(g.0) == r)?.1;
            s.cur().k.get(n.0 as usize).copied().flatten()
        };
        let Some(K::Str(id)) = k_of(self, row("id")) else {
            return; // a non-constant id adds no row
        };
        let dirs = match k_of(self, row("direction")) {
            Some(K::Int(0)) => LONG,
            Some(K::Int(1)) => SHORT,
            _ => LONG | SHORT,
        };
        let key = pine_ir::str_key(self.syms.resolve(id).as_bytes());
        self.ir.entry_sites.push((key, dirs));
    }

    /// `request.security` (§4.11.2–§4.11.3): the `expression` lowered into a detached block `E`
    /// (a region of its own, never emitted in the parent), then one `Call` per tuple element with
    /// the window `[__key (patched in 5a), __out = j, __ty, gaps, lookahead]`, and the child's
    /// tail — per element the `F_I2F` when it is not `float` and a `plot` capture at index `j`,
    /// every other parameter defaulted. The target arguments passed pass 3 (§4.11.1), so each
    /// folds; their text is kept raw and resolved in 5a against the options.
    fn security(&mut self, n: NodeId, ov: OverloadId, hints: &[TypeQ]) -> Result<Vec<VSlot>, Diag> {
        let at = self.ast.span(n);
        if !matches!(self.ast.node(n), Node::Call { .. }) {
            return Err(self.todo(n, "a call node"));
        }
        let given = self.given(n);
        let o = pine_spec::overload(ov);
        let ps = pine_spec::params(o);
        let arg = |name: &str| -> Option<NodeId> {
            given
                .iter()
                .find(|g| ps.get(usize::from(g.0)).map(|p| p.name.as_str()) == Some(name))
                .map(|g| g.1)
        };
        let (sym_n, tf_n, e_n) = (arg("symbol"), arg("timeframe"), arg("expression"));
        let (gaps_n, look_n) = (arg("gaps"), arg("lookahead"));
        let Some(e_n) = e_n else {
            return Err(self.todo(n, "a request.security without an expression"));
        };
        let text = |s: &Self, a: Option<NodeId>| match a
            .and_then(|a| s.cur().k.get(a.0 as usize).copied().flatten())
        {
            Some(K::Str(t)) => s.syms.resolve(t).to_owned(),
            // `syminfo.tickerid` / `timeframe.period` (pass 3 refused every other non-constant).
            _ => String::new(),
        };
        let flag = |s: &Self, a: Option<NodeId>| match a
            .and_then(|a| s.cur().k.get(a.0 as usize).copied().flatten())
        {
            Some(K::Int(v)) => v == 1,
            Some(K::Bool(b)) => b,
            _ => false, // the defaults `barmerge.gaps_off` / `barmerge.lookahead_off` (0)
        };
        let symbol = text(self, sym_n);
        // The child's chart is the parent's for its own symbol (§4.11.8): `syminfo.tickerid`
        // (unfolded here), `""`, the chart's key or a ticker mapped to it.
        let own = match sym_n.and_then(|a| self.cur().k.get(a.0 as usize).copied().flatten()) {
            Some(K::Str(t)) => symbol.is_empty() || self.knobs.own.contains(&t),
            _ => true,
        };
        let tf = text(self, tf_n);
        let (gaps, lookahead) = (flag(self, gaps_n), flag(self, look_n));
        let tuple = matches!(self.ast.node(e_n), Node::Tuple { .. });
        let e_region = self.regions + 1;
        let (rs, e, _) = self.region(|s| {
            if tuple {
                s.tuple_values(e_n, hints)
            } else {
                let t = hints.first().copied().filter(|t| t.ty != Ty::Void);
                match t.or_else(|| s.ty(e_n)) {
                    Some(t) => s.value_as(e_n, t).map(|v| vec![v]),
                    None => s.expr(e_n).map(|v| vec![v]),
                }
            }
        })?;
        // The `plot` overload of this dialect (the capture).
        let plot_ov = pine_spec::lookup("plot").and_then(|e| {
            (e.ov_lo..e.ov_hi)
                .map(OverloadId)
                .find(|&o| pine_spec::overload(o).versions.has(self.d))
        });
        let mut outs = Vec::with_capacity(rs.len());
        let mut tail = Vec::new();
        for (j, &r) in rs.iter().enumerate() {
            let t = self.slot_ty(r);
            let code = match t.ty {
                Ty::Enum => pine_ir::elem::enumeration(t.a),
                other => pine_ir::elem::scalar(other),
            };
            let c = |bits: u64| Arg::Const {
                lane: Lane::I64,
                bits,
                valid: true,
            };
            let out = self.temp(
                TypeQ {
                    qual: Qual::Series,
                    ..t
                },
                at,
            );
            self.op(Op::Call {
                ov,
                args: vec![
                    c(0),
                    c(j as u64),
                    c(u64::from(code)),
                    c(u64::from(gaps)),
                    c(u64::from(lookahead)),
                ],
                outs: vec![out],
                span: at,
            });
            outs.push(out);
            // The child's tail (§4.11.4): `F_I2F` unless `float`, then `plot` at index `j`.
            let saved = std::mem::take(&mut self.out);
            let v = if t.ty == Ty::Float {
                r
            } else {
                let f = self.temp(scalar(Ty::Float, Qual::Series), at);
                self.op(Op::Un {
                    op: UnOpK::I2f,
                    dst: f,
                    a: r,
                });
                f
            };
            let r = match plot_ov {
                Some(pov) => self.capture(pov, j as u64, v, own, at),
                None => Err(self.todo(n, "the `plot` capture of a request")),
            };
            let mut t_ops = std::mem::replace(&mut self.out, saved);
            r?;
            tail.append(&mut t_ops);
        }
        self.ir.requests.push(crate::ir::Request {
            e,
            region: e_region,
            rs,
            outs: outs.clone(),
            tail: Block(tail),
            symbol,
            tf_s: 0,
            lookahead,
            gaps,
            span: at,
        });
        // The raw timeframe text rides in `symbol`'s sibling until 5a resolves it.
        self.ir.request_tfs.push(tf);
        Ok(outs)
    }

    /// A child's `plot` capture of `v` at plot index `j` (§4.11.4): `__plot = j`, `series = v`,
    /// every other parameter its default.
    ///
    /// `pin:02-child-capture-decl-defaults` — a child has no declaration (§4.11.8 lists its
    /// options, none of them the parent's `indicator()` / `strategy()`; §4.11.9: nothing in a child
    /// depends on the parent's source), so the capture's `decl_format` / `decl_precision` defaults
    /// read the declaration row's own defaults, not the parent's arguments: `format.inherit`, and
    /// `sym_precision` of the **child's** mintick — the parent's for its own symbol, else 0 (the
    /// zeroed `ChartInfo`), whose fewest exact decimals are 0. Two scripts requesting the same
    /// thing therefore share the child and its key (I18, law P4).
    fn capture(
        &mut self,
        pov: OverloadId,
        j: u64,
        v: VSlot,
        own: bool,
        at: Span,
    ) -> Result<(), Diag> {
        use crate::overload::DefaultSrc;
        let o = pine_spec::overload(pov);
        let ps = pine_spec::params(o);
        let mut av = Vec::with_capacity(ps.len());
        let mut first = true;
        for (k, p) in ps.iter().enumerate() {
            if !p.versions.has(self.d) {
                continue;
            }
            if p.flags & pine_spec::pf::SYNTHETIC != 0 {
                av.push(Arg::Const {
                    lane: Lane::I64,
                    bits: j,
                    valid: true,
                });
                continue;
            }
            if first {
                first = false;
                av.push(Arg::Slot(v));
                continue;
            }
            let a = match crate::overload::default_src(o, k as u16, &[]) {
                DefaultSrc::DeclFormat => self.child_decl_arg("format", p.sig.ty, own, at)?,
                DefaultSrc::DeclPrecision => self.child_decl_arg("precision", p.sig.ty, own, at)?,
                _ => self.default_arg(self.ast.root(), o, k as u16, &[], p.sig.ty, at)?,
            };
            av.push(a);
        }
        // `plot` returns its handle (the ordinal, `n_out = 1`): the window's last word is an
        // output the body writes, never one of the inputs.
        let mut outs = Vec::with_capacity(1);
        for r in pine_spec::rets(o) {
            outs.push(self.temp(
                TypeQ {
                    qual: Qual::Series,
                    ..r.ty
                },
                at,
            ));
        }
        self.op(Op::Call {
            ov: pov,
            args: av,
            outs,
            span: at,
        });
        Ok(())
    }

    /// A child capture's `decl_format` / `decl_precision` default: the `indicator()` row's own
    /// default for `name` (`strategy()`'s are the same), `sym_precision` over the child's mintick
    /// (`pin:02-child-capture-decl-defaults`).
    fn child_decl_arg(&mut self, name: &str, t: TypeQ, own: bool, at: Span) -> Result<Arg, Diag> {
        let d = self.d;
        let dv = pine_spec::lookup("indicator")
            .and_then(|e| {
                (e.ov_lo..e.ov_hi)
                    .map(|i| pine_spec::overload(OverloadId(i)))
                    .find(|o| o.versions.has(d))
            })
            .and_then(|o| {
                pine_spec::params(o)
                    .iter()
                    .find(|p| p.versions.has(d) && p.name.as_str() == name)
            })
            .map(|p| p.default);
        let Some(dv) = dv else {
            let msg = format!("the declaration row has no `{name}`");
            return Err(diag(ReasonCode::Internal, at, msg));
        };
        if dv.kind == pine_spec::DefaultKind::Derived
            && pine_spec::derived::code(dv.bits) == pine_spec::derived::SYM_PRECISION
        {
            let mintick = if own { self.knobs.mintick_1e6 } else { 0 };
            let p = crate::overload::sym_precision(mintick);
            let c = self.konst(K::Int(p), scalar(Ty::Int, Qual::Const), at);
            return Ok(Arg::Slot(c));
        }
        self.literal_arg(dv, t, at)
    }

    /// Built-in call `n`'s given arguments as `(parameter row, value node)`, sorted by row; a
    /// method-form call's receiver binds the first row (`Inst::bound_args`).
    fn given(&self, n: NodeId) -> Vec<(u16, NodeId)> {
        let mut given: Vec<(u16, NodeId)> = self
            .cur()
            .bound_args(self.ast, n)
            .into_iter()
            .filter_map(|(r, v)| Some((r?, v)))
            .collect();
        given.sort_by_key(|g| g.0);
        given
    }

    /// One `CALL_BUILTIN` with its window and one fresh output per `tys` (§4.14.5).
    fn builtin_call(
        &mut self,
        n: NodeId,
        ov: OverloadId,
        tys: &[TypeQ],
    ) -> Result<Vec<VSlot>, Diag> {
        let at = self.ast.span(n);
        if !matches!(self.ast.node(n), Node::Call { .. }) {
            return Err(self.todo(n, "a call node"));
        }
        let bound = self
            .cur()
            .bound
            .iter()
            .find(|b| b.0 == n)
            .map(|b| b.1.clone())
            .unwrap_or_default();
        let given = self.given(n);
        let o = pine_spec::overload(ov);
        self.entry_site(o, &given);
        let first = tys
            .first()
            .copied()
            .unwrap_or(scalar(Ty::Void, Qual::Const));
        let av = self.window(n, o, &given, &bound, first)?;
        let mut outs = Vec::with_capacity(tys.len());
        for &t in tys {
            outs.push(self.temp(t, at));
        }
        self.op(Op::Call {
            ov,
            args: av,
            outs: outs.clone(),
            span: at,
        });
        Ok(outs)
    }

    // ---- structures (T11, §4.14.3–§4.14.4) ---------------------------------------------------

    fn slot_ty(&self, v: VSlot) -> TypeQ {
        self.ir
            .slots
            .get(v.0 as usize)
            .map_or(scalar(Ty::Void, Qual::Const), |s| s.ty)
    }

    /// `v` in `to`'s lane (`int` → `float` only).
    fn widen_slot(&mut self, v: VSlot, to: Ty) -> VSlot {
        let from = self.slot_ty(v);
        self.widen(v, Some(from), to)
    }

    /// `n`'s value as a `t`: an `na` literal is `t`'s `na` constant, an `int` is widened.
    fn value_as(&mut self, n: NodeId, t: TypeQ) -> Result<VSlot, Diag> {
        if matches!(
            self.cur().k.get(n.0 as usize).copied().flatten(),
            Some(K::Na)
        ) {
            return Ok(self.konst(K::Na, t, self.ast.span(n)));
        }
        let v = self.expr(n)?;
        Ok(self.widen_slot(v, t.ty))
    }

    /// A folded numeric operand.
    fn k_num(&self, n: NodeId) -> Option<f64> {
        match self.cur().k.get(n.0 as usize).copied().flatten()? {
            K::Int(i) => Some(i as f64),
            K::Float(f) => Some(f),
            _ => None,
        }
    }

    /// Lower `f` into a fresh region: its block and the plain variables declared in it.
    fn region<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, Diag>,
    ) -> Result<(T, Block, Vec<VSlot>), Diag> {
        self.regions += 1;
        let outer = self.region;
        let out = std::mem::take(&mut self.out);
        let declared = std::mem::take(&mut self.declared);
        self.region = self.regions;
        let r = f(self);
        let inner = std::mem::replace(&mut self.out, out);
        let decl = std::mem::replace(&mut self.declared, declared);
        self.region = outer;
        r.map(|v| (v, Block(inner), decl))
    }

    /// A guard flag: a `bool` slot never `na` (§4.14.4).
    fn flag(&mut self, varip: bool, span: Span) -> VSlot {
        let info = self.info(
            scalar(Ty::Bool, Qual::Series),
            SlotKind::Flag { varip },
            Sym::NONE,
            span,
        );
        self.ir.slot(info)
    }

    /// `I_CONST f ← 1`.
    fn set_flag(&mut self, f: VSlot) {
        self.op(Op::Load {
            dst: f,
            c: Arg::Const {
                lane: Lane::I64,
                bits: 1,
                valid: true,
            },
        });
    }

    /// `na` into a structure's result — `false` for a `bool` under `bool_na = never` (§4.14.4).
    fn set_na(&mut self, r: VSlot, t: TypeQ) {
        if t.ty == Ty::Bool && self.rd.bool_na == swr::BOOL_NA_NEVER {
            let span = self
                .ir
                .slots
                .get(r.0 as usize)
                .map_or(Span::default(), |s| s.span);
            let f = self.konst(K::Bool(false), t, span);
            self.op(Op::Mov { dst: r, a: f });
        } else {
            self.op(Op::NaSet { a: r });
        }
    }

    /// `pin:02-na-condition` `NaResult`: no arm runs on an `na` condition, so the result is set to
    /// `na` before the branch (lanes are never cleared between evaluations).
    fn pre_na(&mut self, r: Option<(VSlot, TypeQ)>) {
        if let Some((r, t)) = r {
            if self.rd.pins.na_condition == NaCondition::NaResult {
                self.set_na(r, t);
            }
        }
    }

    /// A condition's `bool` slot; a v5 numeric is `x != 0` (§4.4.3, §4.14.3).
    fn cond(&mut self, n: NodeId) -> Result<VSlot, Diag> {
        let v = self.expr(n)?;
        let t = self.slot_ty(v);
        if !matches!(t.ty, Ty::Int | Ty::Float) {
            return Ok(v);
        }
        let at = self.ast.span(n);
        let zero = if t.ty == Ty::Float {
            K::Float(0.0)
        } else {
            K::Int(0)
        };
        let z = self.konst(zero, scalar(t.ty, Qual::Const), at);
        let dst = self.temp(scalar(Ty::Bool, t.qual), at);
        self.op(Op::Cmp {
            op: CmpK::Ne,
            dst,
            a: v,
            b: z,
        });
        Ok(dst)
    }

    /// Push a conditional region. U16 `na`: each arm ends by setting the other arm's plain
    /// locals to `na` (§4.14.4; `pin:02-block-local-ringless` — every plain local, the
    /// ring filter is series', T17); the locals join the enclosing region's.
    fn push_if(
        &mut self,
        cond: VSlot,
        mut then: Block,
        td: Vec<VSlot>,
        mut els: Block,
        ed: Vec<VSlot>,
        kind: CondKind,
    ) {
        if self.rd.block_local == swr::BLOCK_LOCAL_NA {
            for &v in &ed {
                then.0.push(Stmt::Op(Op::NaSet { a: v }));
            }
            for &v in &td {
                els.0.push(Stmt::Op(Op::NaSet { a: v }));
            }
        }
        self.declared.extend(td);
        self.declared.extend(ed);
        let na_else = matches!(kind, CondKind::Guard | CondKind::LazyAndOr)
            || self.rd.pins.na_condition == NaCondition::AsFalse;
        self.out.push(Stmt::If {
            cond,
            then,
            els,
            kind,
            na_else,
        });
    }

    /// Push a loop region. U16 `na`: the body's plain locals are set to `na` before the loop.
    #[allow(clippy::too_many_arguments, reason = "the loop region's parts")]
    fn push_loop(
        &mut self,
        span: Span,
        ctr: Option<Ctr>,
        head: Block,
        cond: VSlot,
        body: Block,
        bd: Vec<VSlot>,
        step: Block,
        trip: Option<u32>,
        kind: LoopKind,
    ) {
        if self.rd.block_local == swr::BLOCK_LOCAL_NA {
            for &v in &bd {
                self.op(Op::NaSet { a: v });
            }
        }
        self.declared.extend(bd);
        self.out.push(Stmt::Loop {
            span,
            ctr,
            head,
            cond,
            body,
            step,
            trip,
            kind,
        });
    }

    /// A block's statements; with `want`, the slot of its last statement's value (`None`: it
    /// yields nothing, or an `na` literal).
    fn block_value(&mut self, b: NodeId, want: bool) -> Result<Option<VSlot>, Diag> {
        let ast = self.ast;
        let one = [b];
        let stmts: &[NodeId] = match ast.node(b) {
            Node::Block { stmts } => stmts,
            _ => &one,
        };
        let Some((&last, init)) = stmts.split_last() else {
            return Ok(None);
        };
        for &st in init {
            self.stmt(st)?;
        }
        if !want {
            self.stmt(last)?;
            return Ok(None);
        }
        let var_of = |s: &Self, x: NodeId| match s.binding(x) {
            Some(Binding::Var { id, .. }) => s.vars.get(&id).copied(),
            _ => None,
        };
        match ast.node(last) {
            Node::VarDecl { name, .. } => {
                self.stmt(last)?;
                Ok(var_of(self, name))
            }
            Node::Assign { target, .. } => {
                self.stmt(last)?;
                Ok(var_of(self, target))
            }
            Node::TupleDecl { .. }
            | Node::TupleAssign { .. }
            | Node::FuncDecl { .. }
            | Node::TypeDecl { .. }
            | Node::EnumDecl { .. }
            | Node::Import { .. }
            | Node::Break
            | Node::Continue => {
                self.stmt(last)?;
                Ok(None)
            }
            _ if matches!(
                self.cur().k.get(last.0 as usize).copied().flatten(),
                Some(K::Na)
            ) =>
            {
                Ok(None)
            }
            _ => match self.ty(last) {
                Some(t) if t.ty != Ty::Void => self.expr(last).map(Some),
                _ => {
                    self.stmt(last)?;
                    Ok(None)
                }
            },
        }
    }

    /// A block whose value, when `r` is given, lands in `r` (`na` when it yields nothing).
    fn block_into(&mut self, b: NodeId, r: Option<(VSlot, TypeQ)>) -> Result<(), Diag> {
        let v = self.block_value(b, r.is_some())?;
        if let Some((r, t)) = r {
            match v {
                Some(v) => {
                    let v = self.widen_slot(v, t.ty);
                    self.op(Op::Mov { dst: r, a: v });
                }
                None => self.set_na(r, t),
            }
        }
        Ok(())
    }

    /// A structure; with `want`, its value's slot (a fresh result slot).
    fn structure(&mut self, n: NodeId, want: bool) -> Result<VSlot, Diag> {
        let at = self.ast.span(n);
        let r = match self.ty(n) {
            Some(t) if want && t.ty != Ty::Void => Some((self.temp(t, at), t)),
            _ => None,
        };
        match self.ast.node(n) {
            Node::If {
                cond,
                then,
                otherwise,
            } => self.if_(cond, then, otherwise, r)?,
            Node::Switch { subject, arms } => self.switch(subject, arms, r)?,
            Node::Ternary {
                cond,
                then,
                otherwise,
            } => self.ternary(cond, then, otherwise, r)?,
            Node::For {
                counter,
                from,
                to,
                step,
                body,
                ..
            } => self.for_(n, counter, [from, to], step, body, r)?,
            Node::ForIn {
                binders,
                collection,
                body,
            } => self.for_in(n, binders, collection, body, r)?,
            Node::While { cond, body } => {
                // Loop value: `na` before the loop, each iteration's last expression (§4.14.4).
                if let Some((r, t)) = r {
                    self.set_na(r, t);
                }
                let (c, head, _) = self.region(|s| s.cond(cond))?;
                let ((), b, bd) = self.region(|s| s.block_into(body, r))?;
                self.push_loop(
                    at,
                    None,
                    head,
                    c,
                    b,
                    bd,
                    Block::default(),
                    None,
                    LoopKind::While,
                );
            }
            Node::Once { cond, body } => {
                if r.is_some() {
                    return Err(self.todo(n, "a `once` block's value"));
                }
                self.once(n, cond, body)?;
            }
            Node::Block { .. } => self.block_into(n, r)?,
            _ => return Err(self.todo(n, "this structure")),
        }
        Ok(r.map_or(VSlot(u32::MAX), |r| r.0))
    }

    fn if_(
        &mut self,
        cond: NodeId,
        then: NodeId,
        otherwise: Option<NodeId>,
        r: Option<(VSlot, TypeQ)>,
    ) -> Result<(), Diag> {
        let c = self.cond(cond)?;
        self.pre_na(r);
        let ((), tb, td) = self.region(|s| s.block_into(then, r))?;
        let ((), eb, ed) = self.region(|s| match otherwise {
            Some(o) => match s.ast.node(o) {
                Node::If {
                    cond,
                    then,
                    otherwise,
                } => s.if_(cond, then, otherwise, r),
                _ => s.block_into(o, r),
            },
            None => {
                if let Some((r, t)) = r {
                    s.set_na(r, t);
                }
                Ok(())
            }
        })?;
        self.push_if(c, tb, td, eb, ed, CondKind::If);
        Ok(())
    }

    /// U14: `selected_only` — an `If` region per arm writing one result slot; `both` — both arms,
    /// then `SEL` (§4.14.3).
    fn ternary(
        &mut self,
        cond: NodeId,
        then: NodeId,
        otherwise: NodeId,
        r: Option<(VSlot, TypeQ)>,
    ) -> Result<(), Diag> {
        let c = self.cond(cond)?;
        if self.rd.ternary_arms == swr::TERNARY_ARMS_BOTH {
            let t = r.map_or(scalar(Ty::Void, Qual::Const), |r| r.1);
            let x = self.value_as(then, t)?;
            let y = self.value_as(otherwise, t)?;
            if let Some((r, _)) = r {
                self.op(Op::Mov { dst: r, a: y });
                self.op(Op::Sel {
                    dst: r,
                    a: x,
                    cond: c,
                });
            }
            return Ok(());
        }
        self.pre_na(r);
        let ((), tb, td) = self.region(|s| s.block_into(then, r))?;
        let ((), eb, ed) = self.region(|s| s.block_into(otherwise, r))?;
        self.push_if(c, tb, td, eb, ed, CondKind::Ternary);
        Ok(())
    }

    /// W3: `strict` — `B_AND` / `B_OR` over both operands; `lazy` — `t = a; JZ t → end; t = b;
    /// end:` (`JNZ` for `or`), a `LazyAndOr` region (§4.14.3).
    fn and_or(
        &mut self,
        n: NodeId,
        op: BinOp,
        lhs: NodeId,
        rhs: NodeId,
        ty: TypeQ,
    ) -> Result<VSlot, Diag> {
        let at = self.ast.span(n);
        let a = self.cond(lhs)?;
        if self.rd.and_or == swr::AND_OR_STRICT {
            let b = self.cond(rhs)?;
            let k = if op == BinOp::And {
                BinOpK::And
            } else {
                BinOpK::Or
            };
            let dst = self.temp(ty, at);
            self.op(Op::Bin { op: k, dst, a, b });
            return Ok(dst);
        }
        let t = self.temp(ty, at);
        self.op(Op::Mov { dst: t, a });
        let ((), rb, rd) = self.region(|s| {
            let b = s.cond(rhs)?;
            s.op(Op::Mov { dst: t, a: b });
            Ok(())
        })?;
        if op == BinOp::And {
            self.push_if(t, rb, rd, Block::default(), Vec::new(), CondKind::LazyAndOr);
        } else {
            self.push_if(t, Block::default(), Vec::new(), rb, rd, CondKind::LazyAndOr);
        }
        Ok(t)
    }

    /// `switch`: the subject once; arms test `subject == pattern` (or their own condition) in
    /// order, each in the previous arm's `else`; none → the default arm, else `na` (§4.14.4).
    fn switch(
        &mut self,
        subject: Option<NodeId>,
        arms: &[NodeId],
        r: Option<(VSlot, TypeQ)>,
    ) -> Result<(), Diag> {
        let s = match subject {
            Some(x) => Some(self.expr(x)?),
            None => None,
        };
        self.pre_na(r);
        let mut pats = Vec::with_capacity(arms.len());
        let mut default = None;
        for &a in arms {
            if let Node::SwitchArm { pattern, body } = self.ast.node(a) {
                match pattern {
                    Some(p) => pats.push((p, body)),
                    None => default = Some(body),
                }
            }
        }
        self.arms(s, &pats, default, r)
    }

    fn arms(
        &mut self,
        s: Option<VSlot>,
        pats: &[(NodeId, NodeId)],
        default: Option<NodeId>,
        r: Option<(VSlot, TypeQ)>,
    ) -> Result<(), Diag> {
        let Some((&(p, body), rest)) = pats.split_first() else {
            return match default {
                Some(b) => self.block_into(b, r),
                None => {
                    if let Some((r, t)) = r {
                        self.set_na(r, t);
                    }
                    Ok(())
                }
            };
        };
        let c = match s {
            Some(sv) => {
                let pv = self.expr(p)?;
                let (st, pt) = (self.slot_ty(sv), self.slot_ty(pv));
                let bt = scalar(Ty::Bool, Qual::Series);
                self.apply(BinOp::Eq, sv, pv, Some(st), Some(pt), bt, self.ast.span(p))?
            }
            None => self.cond(p)?,
        };
        let ((), tb, td) = self.region(|x| x.block_into(body, r))?;
        let ((), eb, ed) = self.region(|x| x.arms(s, rest, default, r))?;
        self.push_if(c, tb, td, eb, ed, CondKind::Switch);
        Ok(())
    }

    /// `for i = a to b [by s]` (§4.14.4): `to` inclusive, the direction decided by `a` against the
    /// initial `b`, MG-23's bound, `pin:02-for-step`.
    fn for_(
        &mut self,
        n: NodeId,
        counter: NodeId,
        [from, to]: [NodeId; 2],
        step: Option<NodeId>,
        body: NodeId,
        r: Option<(VSlot, TypeQ)>,
    ) -> Result<(), Diag> {
        let at = self.ast.span(n);
        let i = self.counter_slot(counter)?;
        let ct = self.slot_ty(i);
        let a = self.value_as(from, ct)?;
        let (ka, kb) = (self.k_num(from), self.k_num(to));
        let each = self.rd.for_bound == swr::FOR_BOUND_EACH_ITERATION && kb.is_none();
        let up = match (ka, kb) {
            (Some(x), Some(y)) => Some(x <= y),
            _ => None,
        };
        // The initial bound: the bound itself under `fixed_at_start`, the direction's operand.
        let b0 = self.value_as(to, ct)?;
        // MG-23 `fixed_at_start`: the bound is a value computed once before the loop — a snapshot,
        // so a body that assigns the bound's variable does not move it.
        let b0 = if !each && kb.is_none() {
            let t = self.temp(scalar(ct.ty, Qual::Series), at);
            self.op(Op::Mov { dst: t, a: b0 });
            t
        } else {
            b0
        };
        let one = if ct.ty == Ty::Float {
            K::Float(1.0)
        } else {
            K::Int(1)
        };
        let (s, ks, dynamic) = match step {
            None => (
                self.konst(one, scalar(ct.ty, Qual::Const), at),
                Some(1.0),
                false,
            ),
            Some(x) => match self.k_num(x) {
                Some(v) if v <= 0.0 => {
                    let msg = format!("a `for` step must be positive, not {v}");
                    return Err(diag(ReasonCode::TypeMismatch, self.ast.span(x), msg));
                }
                Some(v) => (self.value_as(x, ct)?, Some(v), false),
                None if self.rd.pins.for_step == ForStep::RefuseDynamic => {
                    let msg = "a `for` step must be a constant (pin:02-for-step)".to_owned();
                    return Err(diag(ReasonCode::QualifierTooWeak, self.ast.span(x), msg));
                }
                None if self.rd.pins.for_step == ForStep::RuntimeFault => {
                    // CI-004 / F085: the step as written, checked once on entering the `for`; an
                    // `na` step passes the check and runs one pass (R-P17, F119, `step_check`).
                    let v = self.value_as(x, ct)?;
                    self.step_check(v, ct.ty, self.ast.span(x))?;
                    (v, None, false)
                }
                None => {
                    // `RuntimeAbs`: the step's absolute value; zero runs no iteration.
                    let v = self.value_as(x, ct)?;
                    let d = self.temp(scalar(ct.ty, Qual::Series), at);
                    self.op(Op::Un {
                        op: UnOpK::Abs,
                        dst: d,
                        a: v,
                    });
                    (d, None, true)
                }
            },
        };
        self.op(Op::Mov { dst: i, a });
        if let Some((r, t)) = r {
            self.set_na(r, t);
        }
        // A dynamic direction: `up = a <= b0` and the signed step, before the loop.
        let series = scalar(ct.ty, Qual::Series);
        let bool_t = scalar(Ty::Bool, Qual::Series);
        let (dir, delta) = match up {
            None => {
                let u = self.temp(bool_t, at);
                self.op(Op::Cmp {
                    op: CmpK::Le,
                    dst: u,
                    a: i,
                    b: b0,
                });
                let neg = self.temp(series, at);
                self.op(Op::Un {
                    op: UnOpK::Neg,
                    dst: neg,
                    a: s,
                });
                let d = self.temp(series, at);
                self.op(Op::Mov { dst: d, a: neg });
                self.op(Op::Sel {
                    dst: d,
                    a: s,
                    cond: u,
                });
                (Some(u), d)
            }
            Some(_) => (None, s),
        };
        let (c, head, _) = self.region(|x| {
            let b = if each { x.value_as(to, ct)? } else { b0 };
            let c = x.temp(bool_t, at);
            match (up, dir) {
                (Some(u), _) => {
                    let op = if u { CmpK::Le } else { CmpK::Ge };
                    x.op(Op::Cmp {
                        op,
                        dst: c,
                        a: i,
                        b,
                    });
                }
                (None, Some(u)) => {
                    // `up ? i <= b : b <= i` through SEL-selected bounds.
                    let lo = x.temp(series, at);
                    x.op(Op::Mov { dst: lo, a: b });
                    x.op(Op::Sel {
                        dst: lo,
                        a: i,
                        cond: u,
                    });
                    let hi = x.temp(series, at);
                    x.op(Op::Mov { dst: hi, a: i });
                    x.op(Op::Sel {
                        dst: hi,
                        a: b,
                        cond: u,
                    });
                    x.op(Op::Cmp {
                        op: CmpK::Le,
                        dst: c,
                        a: lo,
                        b: hi,
                    });
                }
                (None, None) => return Err(x.todo(n, "a `for` without a direction")),
            }
            if !dynamic {
                return Ok(c);
            }
            let zero = if ct.ty == Ty::Float {
                K::Float(0.0)
            } else {
                K::Int(0)
            };
            let z = x.konst(zero, scalar(ct.ty, Qual::Const), at);
            let nz = x.temp(bool_t, at);
            x.op(Op::Cmp {
                op: CmpK::Ne,
                dst: nz,
                a: s,
                b: z,
            });
            let both = x.temp(bool_t, at);
            x.op(Op::Bin {
                op: BinOpK::And,
                dst: both,
                a: c,
                b: nz,
            });
            Ok(both)
        })?;
        let ((), b, bd) = self.region(|x| x.block_into(body, r))?;
        let ((), stepb, _) = self.region(|x| {
            let op = if up == Some(false) {
                BinOpK::Sub
            } else {
                BinOpK::Add
            };
            x.op(Op::Bin {
                op,
                dst: i,
                a: i,
                b: delta,
            });
            Ok(())
        })?;
        // A constant trip count when `from`, `to` and `by` fold (float counters included).
        let trip = match (ka, kb, ks) {
            (Some(x), Some(y), Some(s)) if s > 0.0 => {
                u32::try_from(((y - x).abs() / s).floor() as u64 + 1).ok()
            }
            _ => None,
        };
        let ctr = match (trip, ka, ks, up) {
            (Some(_), Some(from), Some(s), Some(u)) => Some(Ctr {
                slot: i,
                from,
                delta: if u { s } else { -s },
            }),
            _ => None,
        };
        self.push_loop(at, ctr, head, c, b, bd, stepb, trip, LoopKind::For);
        Ok(())
    }

    /// CI-004 / F085, `pin:02-for-step` `RuntimeFault`: `if s <= 0` call pine-std's
    /// `runtime.error` (`ScriptRefused`) with TradingView's text.
    ///
    /// R-P17 (CI-053, F119, operator 2026-10-17): an `na` step does NOT fault — TradingView runs
    /// `for i = 0 to 10 by s` with `s` na exactly once per bar (cnt = 1 on all 24 493 bars, v5 and
    /// v6): the first iteration runs, then `i + na` is na and the bound test fails. A comparison
    /// with `na` is false (contract §3.6.1), so testing `s <= 0` (not `not (s > 0)`) leaves an
    /// `na` step to the loop itself: the counter goes na after the first pass (the step slot is
    /// `can_be_na`, so the add and the head compare carry validity) and the head ends the loop.
    /// A zero or negative step still faults (F085).
    fn step_check(&mut self, s: VSlot, ty: Ty, at: Span) -> Result<(), Diag> {
        const MSG: &str = "'step' in loop must be greater than zero.";
        let zero = if ty == Ty::Float {
            K::Float(0.0)
        } else {
            K::Int(0)
        };
        let z = self.konst(zero, scalar(ty, Qual::Const), at);
        let bad = self.temp(scalar(Ty::Bool, Qual::Series), at);
        self.op(Op::Cmp {
            op: CmpK::Le,
            dst: bad,
            a: s,
            b: z,
        });
        let ov = self.helper("runtime.error", at)?;
        let ((), fault, _) = self.region(|x| {
            x.op(Op::Call {
                ov,
                args: vec![Arg::Str(MSG)],
                outs: Vec::new(),
                span: at,
            });
            Ok(())
        })?;
        self.push_if(
            bad,
            fault,
            Vec::new(),
            Block::default(),
            Vec::new(),
            CondKind::If,
        );
        Ok(())
    }

    /// A `for` counter's or `for…in` binder's slot.
    fn counter_slot(&mut self, b: NodeId) -> Result<VSlot, Diag> {
        let Some(Binding::Counter { id }) = self.binding(b) else {
            return Err(self.todo(b, "a loop binder without a binding"));
        };
        let ty = self
            .local_ty(id, b)
            .ok_or_else(|| self.todo(b, "an untyped loop binder"))?;
        let info = self.info(ty, SlotKind::Counter, self.ast.sym(b), self.ast.span(b));
        let v = self.ir.slot(info);
        self.vars.insert(id, v);
        Ok(v)
    }

    /// The one overload of a helper entry (`array.size`, `array.get`, `map.keys`, `map.get`).
    fn helper(&self, name: &str, at: Span) -> Result<OverloadId, Diag> {
        match pine_spec::lookup(name) {
            Some(e) if e.ov_hi == e.ov_lo + 1 => Ok(OverloadId(e.ov_lo)),
            _ => {
                let msg = format!("not implemented: lowering `for…in`'s helper `{name}`");
                Err(diag(ReasonCode::Internal, at, msg))
            }
        }
    }

    fn helper_call(
        &mut self,
        name: &str,
        args: &[VSlot],
        dst: VSlot,
        at: Span,
    ) -> Result<(), Diag> {
        let ov = self.helper(name, at)?;
        self.op(Op::Call {
            ov,
            args: args.iter().map(|&a| Arg::Slot(a)).collect(),
            outs: vec![dst],
            span: at,
        });
        Ok(())
    }

    /// `for x in arr` / `for [i, x] in arr`: an index loop whose head re-reads `array.size` and
    /// whose body starts with `x = array.get(arr, i)`; `for [k, v] in m`: `map.keys(m)` once,
    /// then the index loop with `v = map.get(m, k)` (§4.14.4).
    fn for_in(
        &mut self,
        n: NodeId,
        binders: &[NodeId],
        coll: NodeId,
        body: NodeId,
        r: Option<(VSlot, TypeQ)>,
    ) -> Result<(), Diag> {
        let at = self.ast.span(n);
        let c = self.expr(coll)?;
        let ct = self.slot_ty(c);
        let int_t = scalar(Ty::Int, Qual::Series);
        let is_map = ct.ty == Ty::Map;
        let arr = if is_map {
            let kt = TypeQ {
                ty: Ty::Array,
                a: ct.a,
                ..scalar(Ty::Array, Qual::Series)
            };
            let keys = self.temp(kt, at);
            self.helper_call("map.keys", &[c], keys, at)?;
            keys
        } else {
            c
        };
        let idx = if !is_map && binders.len() == 2 {
            self.counter_slot(binders[0])?
        } else {
            let info = self.info(int_t, SlotKind::Counter, Sym::NONE, at);
            self.ir.slot(info)
        };
        let zero = self.konst(K::Int(0), scalar(Ty::Int, Qual::Const), at);
        let one = self.konst(K::Int(1), scalar(Ty::Int, Qual::Const), at);
        self.op(Op::Mov { dst: idx, a: zero });
        if let Some((r, t)) = r {
            self.set_na(r, t);
        }
        let (cnd, head, _) = self.region(|x| {
            let size = x.temp(int_t, at);
            x.helper_call("array.size", &[arr], size, at)?;
            let c = x.temp(scalar(Ty::Bool, Qual::Series), at);
            x.op(Op::Cmp {
                op: CmpK::Lt,
                dst: c,
                a: idx,
                b: size,
            });
            Ok(c)
        })?;
        let ((), b, bd) = self.region(|x| {
            if is_map {
                let (Some(&kb), Some(&vb)) = (binders.first(), binders.get(1)) else {
                    return Err(x.todo(n, "a map loop without two binders"));
                };
                let k = x.counter_slot(kb)?;
                x.helper_call("array.get", &[arr, idx], k, at)?;
                let v = x.counter_slot(vb)?;
                x.helper_call("map.get", &[c, k], v, at)?;
            } else {
                let Some(&xb) = binders.last() else {
                    return Err(x.todo(n, "a loop without a binder"));
                };
                let v = x.counter_slot(xb)?;
                x.helper_call("array.get", &[arr, idx], v, at)?;
            }
            x.block_into(body, r)
        })?;
        let ((), stepb, _) = self.region(|x| {
            x.op(Op::Bin {
                op: BinOpK::Add,
                dst: idx,
                a: idx,
                b: one,
            });
            Ok(())
        })?;
        self.push_loop(at, None, head, cnd, b, bd, stepb, None, LoopKind::ForIn);
        Ok(())
    }

    /// `once [c]`: `JNZ f → skip; [c; JZ c → skip]; f ← 1; body; skip:`, the flag per
    /// `pin:02-once-flag` (§4.14.4).
    fn once(&mut self, n: NodeId, cond: Option<NodeId>, body: NodeId) -> Result<(), Diag> {
        let flag = self.flag(
            self.rd.pins.once_flag == OnceFlag::VaripLike,
            self.ast.span(n),
        );
        let ((), els, ed) = self.region(|x| match cond {
            Some(c) => {
                let cv = x.cond(c)?;
                let ((), tb, td) = x.region(|y| {
                    y.set_flag(flag);
                    y.block_into(body, None)
                })?;
                x.push_if(cv, tb, td, Block::default(), Vec::new(), CondKind::If);
                Ok(())
            }
            None => {
                x.set_flag(flag);
                x.block_into(body, None)
            }
        })?;
        self.push_if(flag, Block::default(), Vec::new(), els, ed, CondKind::Guard);
        Ok(())
    }

    /// A parameter default's constant: pass 3 types defaults in a throwaway frame (they are
    /// constants of the declaring file, §4.3.1), so lowering folds the literal forms here — a
    /// literal, a signed numeric literal, `na`.
    fn literal_k(&self, ast: &Ast, d: NodeId) -> Option<K> {
        match ast.node(d) {
            Node::IntLit { value } => i64::try_from(value).ok().map(K::Int),
            Node::FloatLit { value } => Some(K::Float(value)),
            Node::BoolLit { value } => Some(K::Bool(value)),
            Node::StrLit { value, .. } => Some(K::Str(value)),
            Node::ColorLit { rgb, alpha } => {
                let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
                Some(K::Color(crate::fold::color_literal(
                    r, g, b, alpha, self.rd,
                )))
            }
            Node::Ident { sym } if self.syms.resolve(sym) == "na" => Some(K::Na),
            Node::Unary { op, operand } => match (op, self.literal_k(ast, operand)?) {
                (UnOp::Pos, k) => Some(k),
                (UnOp::Neg, K::Int(i)) => Some(K::Int(i.wrapping_neg())),
                (UnOp::Neg, K::Float(f)) => Some(K::Float(-f)),
                _ => None,
            },
            _ => None,
        }
    }

    // ---- objects and strings (T12, §4.14.7) --------------------------------------------------

    /// `dst ← src` for a variable: a `string` variable's every store is `S_PERSIST` into its own
    /// cell (§4.14.7); temporaries and parameters are never persisted.
    fn store(&mut self, dst: VSlot, src: VSlot) {
        let persist = self.ir.slots.get(dst.0 as usize).is_some_and(|s| {
            s.ty.ty == Ty::String && matches!(s.kind, SlotKind::Var | SlotKind::Varip)
        });
        if persist {
            if let Some(s) = self.ir.slots.get_mut(dst.0 as usize) {
                s.cell = true;
            }
            self.op(Op::Persist { dst, src });
        } else {
            self.op(Op::Mov { dst, a: src });
        }
    }

    /// Field `m` of UDT `decl`: its index and type.
    fn field_of(&self, decl: u16, m: Sym, n: NodeId) -> Result<(u16, TypeQ), Diag> {
        let name = self.syms.resolve(m);
        self.t
            .udts
            .get(usize::from(decl))
            .and_then(|u| {
                u.fields
                    .iter()
                    .position(|f| self.syms.resolve(f.0) == name)
                    .map(|i| (i as u16, u.fields[i].1))
            })
            .ok_or_else(|| self.todo(n, "an unknown field"))
    }

    /// Store `v` into field `field` of the fresh object `h` at Object site `site`: a `string`
    /// field through its staging slot (CAR-03-1 (a): `S_PERSIST t, v, cell(s, f)`, `O_SET_I`).
    fn init_field(&mut self, h: VSlot, site: u32, field: u16, ft: TypeQ, v: VSlot, at: Span) {
        if ft.ty == Ty::String {
            let mut info = self.info(ft, SlotKind::Stage { site, field }, Sym::NONE, at);
            info.cell = true;
            let t = self.ir.slot(info);
            self.op(Op::Persist { dst: t, src: v });
            self.op(Op::ObjSet { h, field, src: t });
        } else {
            self.op(Op::ObjSet { h, field, src: v });
        }
    }

    /// `T.new(args)` — `O_NEW`, then every field stored: the argument, else the declared
    /// default, else `na` (`false` for a v6 `bool`); `T.copy(x)` / `x.copy()` — a shallow copy,
    /// string fields persisted into the copy's own cells (§4.14.7).
    fn udt_call(&mut self, n: NodeId, decl: u16, ty: TypeQ) -> Result<VSlot, Diag> {
        let at = self.ast.span(n);
        let Node::Call { callee, args, .. } = self.ast.node(n) else {
            return Err(self.todo(n, "a call node"));
        };
        let Node::Field { base, member } = self.ast.node(callee) else {
            return Err(self.todo(n, "an object call without a member"));
        };
        let what = self.syms.resolve(self.ast.sym(member)).to_owned();
        let Some(u) = self.t.udts.get(usize::from(decl)) else {
            return Err(self.todo(n, "an unknown type"));
        };
        let fields = u.fields.clone();
        let file = u.file;
        let site = self.obj_sites;
        self.obj_sites += 1;
        if what == "copy" {
            let src = if self.ty(base).is_some_and(|t| t.ty == Ty::Udt) {
                base
            } else {
                match args.first() {
                    Some(&a) => a,
                    None => return Err(self.todo(n, "`copy` without an object")),
                }
            };
            let s = self.expr(src)?;
            let h = self.temp(ty, at);
            self.op(Op::ObjNew {
                dst: h,
                udt: decl,
                span: at,
            });
            for (i, &(_, ft, _)) in fields.iter().enumerate() {
                let v = self.temp(ft, at);
                self.op(Op::ObjGet {
                    dst: v,
                    h: s,
                    field: i as u16,
                });
                self.init_field(h, site, i as u16, ft, v, at);
            }
            return Ok(h);
        }
        // `new`: positional arguments bind fields in order, named ones by name.
        let mut by_field: Vec<Option<NodeId>> = vec![None; fields.len()];
        for (k, &a) in args.iter().enumerate() {
            let (i, v) = match self.ast.node(a) {
                Node::NamedArg { name, value } => {
                    let want = self.syms.resolve(self.ast.sym(name));
                    let i = fields.iter().position(|f| self.syms.resolve(f.0) == want);
                    (i, value)
                }
                _ => (Some(k), a),
            };
            match i.and_then(|i| by_field.get_mut(i)) {
                Some(b) => *b = Some(v),
                None => return Err(self.todo(n, "an argument without a field")),
            }
        }
        let mut vals = Vec::with_capacity(fields.len());
        for (i, &(_, ft, def)) in fields.iter().enumerate() {
            let v = match by_field[i] {
                Some(a) => self.value_as(a, ft)?,
                None => {
                    let fast = self.files.get(file).copied().flatten();
                    let k = match (def, fast) {
                        (Some(d), Some(fa)) => match self.literal_k(fa, d) {
                            Some(k) => k,
                            None => return Err(self.todo(n, "a field default that does not fold")),
                        },
                        _ if ft.ty == Ty::Bool && self.rd.bool_na == swr::BOOL_NA_NEVER => {
                            K::Bool(false)
                        }
                        _ => K::Na,
                    };
                    let v = self.konst(k, ft, at);
                    self.widen_slot(v, ft.ty)
                }
            };
            vals.push(v);
        }
        let h = self.temp(ty, at);
        self.op(Op::ObjNew {
            dst: h,
            udt: decl,
            span: at,
        });
        for (i, &(_, ft, _)) in fields.iter().enumerate() {
            self.init_field(h, site, i as u16, ft, vals[i], at);
        }
        Ok(h)
    }

    /// `h.f := v` (and the compound forms): `O_SET_*`. A `string` field's store dispatches on
    /// the handle's provenance (CAR-03-1 (a)) in refs.rs.
    fn field_store(
        &mut self,
        n: NodeId,
        op: AssignOp,
        base: NodeId,
        member: NodeId,
        value: NodeId,
    ) -> Result<(), Diag> {
        let at = self.ast.span(n);
        let h = self.expr(base)?;
        let ut = self.slot_ty(h);
        let (field, ft) = self.field_of(ut.a, self.ast.sym(member), n)?;
        // A `string` field's store stays an `ObjSet` whose source is not a staging slot: refs.rs
        // (§4.9, pass 5c) lowers it through the staging slot of each site in `prov(h)`.
        let src = match op {
            AssignOp::Reassign => self.value_as(value, ft)?,
            _ => {
                let b = match op {
                    AssignOp::Add => BinOp::Add,
                    AssignOp::Sub => BinOp::Sub,
                    AssignOp::Mul => BinOp::Mul,
                    AssignOp::Div => BinOp::Div,
                    _ => BinOp::Rem,
                };
                let cur = self.temp(ft, at);
                self.op(Op::ObjGet { dst: cur, h, field });
                let e = self.expr(value)?;
                let et = self.slot_ty(e);
                let r = self.apply(b, cur, e, Some(ft), Some(et), ft, at)?;
                self.widen_slot(r, ft.ty)
            }
        };
        self.op(Op::ObjSet { h, field, src });
        Ok(())
    }

    // ---- history (T12, §4.4.6, §4.10.2) ------------------------------------------------------

    /// `e[k]`: `x[0]` is the slot itself; a constant `k ≥ 1` reads `K::Const`, a dynamic one its
    /// slot through `NZ_I kk, k, −1` (an `na` offset reads out of range). A base pass 3 listed as
    /// materialised reads a temporary written where the expression stands.
    fn hist(&mut self, n: NodeId, base: NodeId, offset: NodeId, ty: TypeQ) -> Result<VSlot, Diag> {
        use crate::ir::K as Off;
        let at = self.ast.span(n);
        let mut src = self.expr(base)?;
        let kind = self.ir.slots.get(src.0 as usize).map(|s| s.kind);
        if self.cur().temps.contains(&base) && kind != Some(SlotKind::Temp) {
            let t = self.temp(self.slot_ty(src), at);
            self.op(Op::Mov { dst: t, a: src });
            src = t;
        }
        let k = match self.cur().k.get(offset.0 as usize).copied().flatten() {
            Some(K::Int(0)) => return Ok(src),
            Some(K::Int(k)) if k > 0 => Off::Const(k as u32),
            Some(_) => return Err(self.todo(offset, "a non-int constant offset")),
            None => {
                let kv = self.expr(offset)?;
                if self.ir.slots.get(kv.0 as usize).map(|s| s.kind) == Some(SlotKind::Counter) {
                    Off::Dyn(kv)
                } else {
                    let m1 = self.konst(K::Int(-1), scalar(Ty::Int, Qual::Const), at);
                    let kk = self.temp(scalar(Ty::Int, Qual::Series), at);
                    self.op(Op::Nz {
                        dst: kk,
                        a: kv,
                        b: m1,
                    });
                    Off::Dyn(kk)
                }
            }
        };
        let dst = self.temp(ty, at);
        self.op(Op::Hist { dst, src, k });
        Ok(dst)
    }

    // ---- argument windows (T12, §4.14.5) ---------------------------------------------------

    /// The window's inputs: every parameter row of the dialect in order — a synthetic by its
    /// name, the given argument(s) (an `int` meeting a `float` row widened), else the row's
    /// default.
    fn window(
        &mut self,
        n: NodeId,
        o: &'static pine_spec::Overload,
        given: &[(u16, NodeId)],
        bound: &[Option<u16>],
        ty: TypeQ,
    ) -> Result<Vec<Arg>, Diag> {
        let at = self.ast.span(n);
        let ps = pine_spec::params(o);
        let mut av = Vec::with_capacity(ps.len() + given.len());
        for (k, p) in ps.iter().enumerate() {
            if !p.versions.has(self.d) {
                continue;
            }
            let row = k as u16;
            if p.flags & pine_spec::pf::SYNTHETIC != 0 {
                let v = self.synthetic(p.name.as_str(), o, given, ty, at)?;
                av.push(Arg::Const {
                    lane: Lane::I64,
                    bits: v,
                    valid: true,
                });
                continue;
            }
            if p.name.as_str() == "sort_field" && crate::overload::udt_field_row(o.id) {
                // R-P5: the folded field selector, never the argument as written. A non-UDT
                // array was refused `3024 … unserved=1` in pass 3: lowered as written.
                if let Some(bits) = self.udt_selector(n, o, given)? {
                    av.push(Arg::Const {
                        lane: Lane::I64,
                        bits,
                        valid: true,
                    });
                    continue;
                }
            }
            let mut any = false;
            for &(r, v) in given {
                if r == row {
                    any = true;
                    // A type-variable row takes the argument as typed.
                    let want = if p.sig.tv == 0 {
                        p.sig.ty
                    } else {
                        self.ty(v).unwrap_or(p.sig.ty)
                    };
                    av.push(Arg::Slot(self.value_as(v, want)?));
                }
            }
            if !any && p.flags & pine_spec::pf::VARIADIC == 0 {
                av.push(self.default_arg(n, o, row, bound, p.sig.ty, at)?);
            }
        }
        Ok(av)
    }

    /// The `sort_field` selector of a UDT field row (R-P5, `pin:02-udt-field-selector`): the
    /// same fold pass 3 checked (`overload::udt_selector`, refusals `3007` there), over the
    /// `id` argument's element type and the `sort_field` argument's folded value, else the row's
    /// default. `index | (field Ty as i64) << 32`, decoded by pine-std's `coll::udt_selector`.
    /// `None` when `id` is not an `array<UDT>` (`pin:02-udt-rows-udt-only`: refused in pass 3).
    fn udt_selector(
        &self,
        n: NodeId,
        o: &'static pine_spec::Overload,
        given: &[(u16, NodeId)],
    ) -> Result<Option<u64>, Diag> {
        let ps = pine_spec::params(o);
        let node = |name: &str| {
            given
                .iter()
                .find(|&&(r, _)| ps[usize::from(r)].name.as_str() == name)
                .map(|&(_, v)| v)
        };
        let Some(el) = node("id")
            .and_then(|v| self.ty(v))
            .filter(|t| t.ty == Ty::Array)
            .map(|t| t.a)
            .filter(|&a| a & (pine_ir::elem::SCALAR | pine_ir::elem::ENUM) == 0)
        else {
            return Ok(None);
        };
        let sel = match node("sort_field") {
            Some(v) => self.cur().k.get(v.0 as usize).copied().flatten(),
            None => crate::overload::sort_field_default(o),
        };
        let fields: Vec<(Sym, TypeQ)> = self
            .t
            .udts
            .get(usize::from(el & pine_ir::elem::DECL_MASK))
            .map(|u| u.fields.iter().map(|f| (f.0, f.1)).collect())
            .unwrap_or_default();
        crate::overload::udt_selector(&fields, sel, self.syms)
            .map(|s| Some(s as u64))
            .map_err(|m| diag(ReasonCode::Internal, self.ast.span(n), m))
    }

    /// A synthetic parameter's value (§4.14.5 rule 1, impl-04 §3.4).
    fn synthetic(
        &mut self,
        name: &str,
        o: &'static pine_spec::Overload,
        given: &[(u16, NodeId)],
        ty: TypeQ,
        at: Span,
    ) -> Result<u64, Diag> {
        use pine_ir::PlotKind;
        let map = ty.ty == Ty::Map;
        match name {
            "__plot" => {
                // §4.14.8: indices in code order over the expanded program; four for
                // `plotcandle` / `plotbar`; every `hline` after every other (fixed up at the end).
                let kind = o.plot.unwrap_or(PlotKind::Plot);
                if kind == PlotKind::Hline {
                    self.hlines.push((kind, at));
                    return Ok(self.hlines.len() as u64 - 1);
                }
                let first = self.ir.plots.len() as u64;
                let k = if matches!(kind, PlotKind::Candle | PlotKind::OhlcBar) {
                    4
                } else {
                    1
                };
                for _ in 0..k {
                    self.ir.plots.push((kind, at));
                }
                Ok(first)
            }
            "__elem" if o.is_collection() => Ok(u64::from(if map { ty.b } else { ty.a })),
            "__key" if o.is_collection() => Ok(if map { u64::from(ty.a) } else { 0 }),
            "__cap" if o.is_collection() => Ok(u64::from(self.knobs.cap)),
            "__types" => {
                // The 4-bit `Ty` code of each variadic argument, argument 0 in bits 0–3.
                let ps = pine_spec::params(o);
                let vrow = ps
                    .iter()
                    .position(|p| p.versions.has(self.d) && p.flags & pine_spec::pf::VARIADIC != 0);
                let mut bits = 0u64;
                let mut k = 0u32;
                for &(r, v) in given {
                    if Some(usize::from(r)) != vrow {
                        continue;
                    }
                    if k == 16 {
                        let msg =
                            "not implemented: `__types` over more than 16 arguments".to_owned();
                        return Err(diag(ReasonCode::Internal, at, msg));
                    }
                    let t = self.ty(v).map_or(0, |t| u64::from(t.ty as u8 & 0xF));
                    bits |= t << (4 * k);
                    k += 1;
                }
                Ok(bits)
            }
            _ => {
                let msg = format!(
                    "not implemented: the synthetic `{name}` (request.security: impl-02 T21–T23)"
                );
                Err(diag(ReasonCode::Internal, at, msg))
            }
        }
    }

    /// An omitted parameter's default (§4.14.5 rule 2).
    fn default_arg(
        &mut self,
        n: NodeId,
        o: &'static pine_spec::Overload,
        row: u16,
        bound: &[Option<u16>],
        t: TypeQ,
        at: Span,
    ) -> Result<Arg, Diag> {
        use crate::overload::DefaultSrc;
        match crate::overload::default_src(o, row, bound) {
            DefaultSrc::Literal(dv) => self.literal_arg(dv, t, at),
            DefaultSrc::Param { row: r, arg } => match arg {
                // `shorttitle` ← `title`: the other argument's value.
                Some(k) => {
                    let Some(&(_, v)) =
                        crate::infer::bound_args(self.ast, n, bound).get(usize::from(k))
                    else {
                        return Err(self.todo(n, "a missing argument"));
                    };
                    Ok(Arg::Slot(self.value_as(v, t)?))
                }
                None => self.default_arg(n, o, r, bound, t, at),
            },
            DefaultSrc::DeclFormat => self.decl_arg("format", t, at),
            DefaultSrc::DeclPrecision => self.decl_arg("precision", t, at),
            DefaultSrc::SymPrecision => {
                let p = crate::overload::sym_precision(self.knobs.mintick_1e6);
                Ok(Arg::Slot(self.konst(
                    K::Int(p),
                    scalar(Ty::Int, Qual::Const),
                    at,
                )))
            }
            DefaultSrc::TimeframeChange => {
                // CI-012 / CI-041 (B120 D059, `ta.vwap(src)`): "as if `timeframe.change()` were
                // passed" — the call itself, at this call site, with its own `timeframe` default.
                let ov = pine_spec::ov::TIMEFRAME_CHANGE;
                let dv = pine_spec::params(pine_spec::overload(ov))[0].default;
                let tf = self.literal_arg(dv, scalar(Ty::String, Qual::Series), at)?;
                let dst = self.temp(t, at);
                self.op(Op::Call {
                    ov,
                    args: vec![tf],
                    outs: vec![dst],
                    span: at,
                });
                Ok(Arg::Slot(dst))
            }
            other => {
                let msg = format!("not implemented: lowering the derived default {other:?} (T20)");
                Err(diag(ReasonCode::Internal, at, msg))
            }
        }
    }

    fn literal_arg(&mut self, dv: pine_spec::DefaultVal, t: TypeQ, at: Span) -> Result<Arg, Diag> {
        use pine_spec::DefaultKind as D;
        let int = |s: &mut Self, i: i64| {
            if t.ty == Ty::Float {
                s.konst(K::Float(i as f64), scalar(Ty::Float, Qual::Const), at)
            } else {
                s.konst(K::Int(i), scalar(Ty::Int, Qual::Const), at)
            }
        };
        let v = match dv.kind {
            D::Int => int(self, dv.bits as i64),
            D::Float => self.konst(
                K::Float(f64::from_bits(dv.bits)),
                scalar(Ty::Float, Qual::Const),
                at,
            ),
            D::Bool => self.konst(K::Bool(dv.bits != 0), scalar(Ty::Bool, Qual::Const), at),
            D::Color => self.konst(K::Color(dv.bits as i64), scalar(Ty::Color, Qual::Const), at),
            D::Str => return Ok(Arg::Str(spec_str(dv.bits))),
            D::Value => return self.value_row(dv.bits as u32, at),
            D::Na => self.konst(K::Na, t, at),
            D::None | D::Derived => {
                let msg = "a required parameter without an argument".to_owned();
                return Err(diag(ReasonCode::Internal, at, msg));
            }
        };
        Ok(Arg::Slot(v))
    }

    /// `decl_format` / `decl_precision` (A4.1): the declaration's own argument `name`, else that
    /// row's default on the declaration (`pin:02-decl-default-chain` — the reference's "the
    /// format set on indicator()" read literally: the declaration's value, given or defaulted).
    fn decl_arg(&mut self, name: &str, t: TypeQ, at: Span) -> Result<Arg, Diag> {
        let (Some(root_ast), Some(root)) = (
            self.files.first().copied().flatten(),
            self.t.insts.get(self.t.root),
        ) else {
            return Err(diag(ReasonCode::Internal, at, "no root".to_owned()));
        };
        let decl = root.calls.iter().find_map(|&(c, callee)| match callee {
            Callee::Builtin(ov) if pine_spec::overload(ov).sx & pine_spec::sx::DECL != 0 => {
                Some((c, ov))
            }
            _ => None,
        });
        let Some((c, ov)) = decl else {
            let msg = format!("not implemented: `{name}`'s default without a declaration");
            return Err(diag(ReasonCode::Internal, at, msg));
        };
        let o = pine_spec::overload(ov);
        let ps = pine_spec::params(o);
        let Some(row) = ps
            .iter()
            .position(|p| p.versions.has(self.d) && p.name.as_str() == name)
        else {
            let msg = format!("the declaration has no `{name}`");
            return Err(diag(ReasonCode::Internal, at, msg));
        };
        let given = root.bound_args(root_ast, c);
        if let Some(&(_, v)) = given.iter().find(|g| g.0 == Some(row as u16)) {
            return match root.k.get(v.0 as usize).copied().flatten() {
                Some(k) => {
                    let ty = root.ty.get(v.0 as usize).copied().flatten().unwrap_or(t);
                    Ok(Arg::Slot(self.konst(k, ty, at)))
                }
                None => Err(diag(
                    ReasonCode::Internal,
                    at,
                    format!("the declaration's `{name}` did not fold"),
                )),
            };
        }
        let dv = ps[row].default;
        if dv.kind == pine_spec::DefaultKind::Derived {
            if pine_spec::derived::code(dv.bits) == pine_spec::derived::SYM_PRECISION {
                let p = crate::overload::sym_precision(self.knobs.mintick_1e6);
                return Ok(Arg::Slot(self.konst(
                    K::Int(p),
                    scalar(Ty::Int, Qual::Const),
                    at,
                )));
            }
            let msg = format!("not implemented: the declaration's derived `{name}` default");
            return Err(diag(ReasonCode::Internal, at, msg));
        }
        self.literal_arg(dv, t, at)
    }

    /// A built-in value row as an argument: a reserved or prelude slot, or its constant.
    fn value_row(&mut self, i: u32, at: Span) -> Result<Arg, Diag> {
        let Some(v) = pine_spec::VALUES.get(i as usize) else {
            let msg = format!("value row {i} out of range");
            return Err(diag(ReasonCode::Internal, at, msg));
        };
        let kind = match v.src {
            ValueSrc::Slot => SlotKind::Reserved {
                slot: v.bits as u16,
            },
            ValueSrc::Call => SlotKind::Prelude { value: i },
            ValueSrc::Const => {
                let k = match v.ty.ty {
                    Ty::Float => K::Float(f64::from_bits(v.bits)),
                    Ty::Bool => K::Bool(v.bits != 0),
                    Ty::Color => K::Color(v.bits as i64),
                    Ty::String => return Ok(Arg::Str(spec_str(v.bits))),
                    _ => K::Int(v.bits as i64),
                };
                return Ok(Arg::Slot(self.konst(k, v.ty, at)));
            }
            ValueSrc::Fold => return Ok(Arg::Slot(self.fold_slot(v.bits, v.ty, at)?)),
        };
        let info = self.info(v.ty, kind, Sym::NONE, at);
        let root = self.ast.root();
        Ok(Arg::Slot(self.value_slot(root, i, info)?))
    }

    /// The slot of value row `row`; the first read of a `ValueSrc::Call` row also lowers its
    /// prelude call (§4.14.1) — `OverloadId(bits)` with every argument defaulted, its result
    /// written straight into the prelude slot. `n` only spans the window's defaults.
    fn value_slot(&mut self, n: NodeId, row: u32, info: SlotInfo) -> Result<VSlot, Diag> {
        let fresh = !self.ir.has_value(row);
        let v = self.ir.value(row, info);
        if !(fresh && matches!(info.kind, SlotKind::Prelude { .. })) {
            return Ok(v);
        }
        let Some(val) = pine_spec::VALUES.get(row as usize) else {
            return Err(self.todo(n, "a value row out of range"));
        };
        let ov = pine_ir::OverloadId(val.bits as u16);
        let o = pine_spec::overload(ov);
        let saved = std::mem::take(&mut self.out);
        let region = std::mem::replace(&mut self.region, 0);
        let r = self.window(n, o, &[], &[], info.ty);
        let mut pre = std::mem::replace(&mut self.out, saved);
        self.region = region;
        let args = r?;
        pre.push(Stmt::Op(Op::Call {
            ov,
            args,
            outs: vec![v],
            span: info.span,
        }));
        self.prelude.extend(pre);
        Ok(v)
    }
}

/// A pine-spec string packed `off | len << 32`.
fn spec_str(bits: u64) -> &'static str {
    pine_spec::SpecStr {
        off: bits as u32,
        len: (bits >> 32) as u32,
    }
    .as_str()
}
