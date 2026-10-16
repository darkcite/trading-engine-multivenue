// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 5h (impl-02 §4.14): one walk over the final IR with symbolic labels — the init block
//! (`JNZ g`, every `Const` slot once, every class-V `NA_CLEAR`, `I_CONST g ← 1`), the body, `HALT`.
//! Forward jumps are patched when their label is placed; `consts` is deduplicated by bits in
//! first-use order; every `CALL_BUILTIN` gets its own window, appended in code order.
//!
//! The prelude (§4.14.1) is lowered at the head of the body (`lower.rs`), so it is emitted in
//! code order before the body. Not emitted yet (refused `3255` naming the gap, never emitted
//! wrong): `request.*` sub-programs (T22–T23). `break` / `continue` inside U13 copies jump past
//! the copies / to the next copy. UDT
//! objects: `O_NEW` (every field then stored), `O_GET_*`, `O_SET_*`; a `string` field's store
//! arrives staged by refs.rs (§4.9). U17:
//! pine-spec carries no `isnew` reading, so this is the `contract` reading — no fixup, reads go
//! to the reserved slots (§4.14.1).

use std::collections::HashMap;

use pine_ir::{
    abi, argwin, flg, op, ring_flag, Diag, Dialect, Insn, Lane, Qual, SiteId, Span, StrRef, Sym,
    SymTable, Ty, TypeQ,
};

use crate::code::{diag, ReasonCode};
use crate::ir::{Arg, BinOpK, Block, CmpK, Ir, Op, SlotInfo, SlotKind, Stmt, UnOpK, VSlot, K};
use crate::layout::{Placed, NONE};
use crate::nacap::{Class, NaCap};
use crate::sites::Site;

fn not_yet(what: &str, at: Span) -> Diag {
    diag(
        ReasonCode::Internal,
        at,
        format!("not implemented: emission of {what}"),
    )
}

fn info(ty: Ty, lane: Lane, kind: SlotKind) -> SlotInfo {
    SlotInfo {
        ty: TypeQ::scalar(ty, Qual::Const),
        lane,
        kind,
        inst: 0,
        region: 0,
        name: Sym::NONE,
        span: Span::default(),
        cell: false,
    }
}

/// Before the analyses: every constant call argument becomes a `Const` slot (§4.14.5 rule 2; a
/// pine-spec string interned as a `CONST` string), a dynamic history offset is guarded by
/// `NZ_I kk, k, −1` (§4.10.2), and the init flag `g` is allocated. Refuses what emission does
/// not cover yet. Returns `g`.
pub(crate) fn prepare(ir: &mut Ir, syms: &mut SymTable, d: Dialect) -> Result<VSlot, Diag> {
    let mut body = std::mem::take(&mut ir.body);
    let r = prep_block(ir, syms, &mut body, d);
    ir.body = body;
    r?;
    Ok(ir.slot(info(Ty::Int, Lane::I64, SlotKind::Flag { varip: false })))
}

fn prep_block(ir: &mut Ir, syms: &mut SymTable, b: &mut Block, d: Dialect) -> Result<(), Diag> {
    let mut out: Vec<Stmt> = Vec::with_capacity(b.0.len());
    for mut s in std::mem::take(&mut b.0) {
        match &mut s {
            Stmt::Op(Op::Call { ov, args, span, .. }) => {
                let o = pine_spec::overload(*ov);
                if o.sx & pine_spec::sx::REQUEST != 0 {
                    return Err(not_yet("a `request.*` sub-program (T21–T23)", *span));
                }
                time_bars_back(ir, o, args, d, *span)?;
                // A row pine-std has no body for (a prelude value's overload is never resolved
                // through §4.5 step 9) — the snapshot's `3024 … unserved=1`.
                if let Some(e) = crate::overload::unserved(*ov, *span) {
                    return Err(e);
                }
                for a in args.iter_mut() {
                    match *a {
                        Arg::Const { lane, bits, valid } => {
                            let ty = if lane == Lane::F64 {
                                Ty::Float
                            } else {
                                Ty::Int
                            };
                            *a = Arg::Slot(ir.konst(info(ty, lane, SlotKind::Const), bits, valid));
                        }
                        Arg::Str(t) => {
                            let sym = syms.intern(t);
                            let i = info(Ty::String, Lane::I64, SlotKind::Const);
                            *a = Arg::Slot(ir.konst(i, u64::from(sym.0), true));
                        }
                        Arg::Slot(_) => {}
                    }
                }
            }
            Stmt::Op(Op::Load { c, .. }) => {
                if let Arg::Str(t) = *c {
                    let sym = syms.intern(t);
                    *c = Arg::Const {
                        lane: Lane::I64,
                        bits: u64::from(sym.0),
                        valid: true,
                    };
                }
            }
            Stmt::Op(Op::Hist { k, .. }) => {
                if let K::Dyn(v) = *k {
                    let kk = ir.slot(SlotInfo {
                        kind: SlotKind::Temp,
                        ..info(Ty::Int, Lane::I64, SlotKind::Temp)
                    });
                    let m1 = ir.konst(info(Ty::Int, Lane::I64, SlotKind::Const), u64::MAX, true);
                    out.push(Stmt::Op(Op::Nz {
                        dst: kk,
                        a: v,
                        b: m1,
                    }));
                    *k = K::Dyn(kk);
                }
            }
            Stmt::Op(_) => {}
            Stmt::If { then, els, .. } => {
                prep_block(ir, syms, then, d)?;
                prep_block(ir, syms, els, d)?;
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                prep_block(ir, syms, head, d)?;
                prep_block(ir, syms, body, d)?;
                prep_block(ir, syms, step, d)?;
            }
            Stmt::Unrolled { copies } => {
                for c in copies.iter_mut() {
                    prep_block(ir, syms, c, d)?;
                }
            }
            Stmt::Break | Stmt::Continue => {}
        }
        out.push(s);
    }
    b.0 = out;
    Ok(())
}

/// pine-std's request (`pin:04-time-bars-back-open`): `time()` / `time_close()` with a nonzero
/// constant `bars_back` or `timeframe_bars_back` is refused at load (`3024`) — pine-std would
/// return `ScriptRefused` on the first evaluation. A non-constant one is left to that run-time
/// refusal.
fn time_bars_back(
    ir: &Ir,
    o: &pine_spec::Overload,
    args: &[Arg],
    d: Dialect,
    at: Span,
) -> Result<(), Diag> {
    let full = pine_spec::entry_of(o).full.as_str();
    if full != "time" && full != "time_close" {
        return Ok(());
    }
    for name in ["bars_back", "timeframe_bars_back"] {
        let Some(p) = pine_spec::param_index(o, name, d) else {
            continue;
        };
        let k = match args.get(usize::from(p)) {
            Some(Arg::Const { bits, valid, .. }) => valid.then_some(*bits),
            Some(Arg::Slot(v)) if ir.slots[v.0 as usize].kind == SlotKind::Const => ir
                .const_of(*v)
                .and_then(|(bits, valid)| valid.then_some(bits)),
            _ => None,
        };
        if let Some(n) = k.filter(|&n| n != 0) {
            let msg = format!(
                "`{full}({name} = {})` is not served (pin:04-time-bars-back-open)",
                n as i64
            );
            return Err(diag(ReasonCode::DeferredBuiltin, at, msg));
        }
    }
    Ok(())
}

/// The emitted tables.
#[derive(Debug, Clone, Default)]
pub(crate) struct Emitted {
    /// `PineProgram::code`.
    pub(crate) code: Vec<Insn>,
    /// `PineProgram::consts`.
    pub(crate) consts: Vec<u64>,
    /// `PineProgram::argwin`.
    pub(crate) argwin: Vec<u16>,
}

/// What emission reads.
pub(crate) struct Input<'a> {
    /// The final IR.
    pub(crate) ir: &'a Ir,
    /// The slot classes.
    pub(crate) na: &'a NaCap,
    /// The layout and its maps.
    pub(crate) pl: &'a Placed,
    /// The numbered sites.
    pub(crate) sites: &'a [Site],
    /// The interner (`CONST` strings).
    pub(crate) syms: &'a SymTable,
    /// The init flag.
    pub(crate) flag: VSlot,
    /// `limits.insn_budget`.
    pub(crate) budget: u32,
    /// The dialect.
    pub(crate) d: Dialect,
}

/// Emit the program.
pub(crate) fn emit(x: &Input<'_>) -> Result<Emitted, Diag> {
    let mut e = Em {
        x,
        out: Emitted::default(),
        cidx: HashMap::new(),
        next_site: 0,
        loops: Vec::new(),
    };
    e.init()?;
    e.block(&x.ir.body)?;
    e.ins(op::HALT, 0, 0, 0, 0);
    if e.out.code.len() as u64 > u64::from(x.budget) {
        let msg = format!(
            "{} instructions exceed insn_budget {}",
            e.out.code.len(),
            x.budget
        );
        return Err(diag(ReasonCode::BudgetTooSmall, Span::default(), msg));
    }
    if e.out.consts.len() > 65_536 || e.out.argwin.len() > 65_536 {
        let msg = "consts or argwin exceed 65 536 entries".to_owned();
        return Err(diag(ReasonCode::FrameTooLarge, Span::default(), msg));
    }
    Ok(e.out)
}

struct Em<'a, 'b> {
    x: &'b Input<'a>,
    out: Emitted,
    cidx: HashMap<u64, u16>,
    next_site: u16,
    /// Per enclosing loop: `break` fixups, `continue` fixups.
    loops: Vec<(Vec<usize>, Vec<usize>)>,
}

impl Em<'_, '_> {
    fn ins(&mut self, o: u8, f: u8, dst: u16, a: u16, b: u16) -> usize {
        self.out.code.push(Insn::new(o, f, dst, a, b));
        self.out.code.len() - 1
    }

    /// A jump whose target is patched later.
    fn jump(&mut self, o: u8, b: u16) -> usize {
        self.ins(o, 0, 0, 0, b)
    }

    fn here(&self) -> u32 {
        self.out.code.len() as u32
    }

    fn patch(&mut self, at: usize, target: u32) {
        let i = &mut self.out.code[at];
        i.dst = (target >> 16) as u16;
        i.a = target as u16;
    }

    fn k(&mut self, bits: u64) -> u16 {
        if let Some(&i) = self.cidx.get(&bits) {
            return i;
        }
        let i = self.out.consts.len() as u16;
        self.out.consts.push(bits);
        self.cidx.insert(bits, i);
        i
    }

    fn info(&self, v: VSlot) -> &SlotInfo {
        &self.x.ir.slots[v.0 as usize]
    }

    fn lane(&self, v: VSlot) -> Lane {
        self.info(v).lane
    }

    fn i64(&self, v: VSlot) -> bool {
        self.lane(v) == Lane::I64
    }

    fn a(&self, v: VSlot) -> bool {
        self.x.na.a(v)
    }

    fn vld(&self, dst: VSlot) -> u8 {
        if self.a(dst) {
            flg::VLD
        } else {
            0
        }
    }

    /// The physical slot of a read.
    fn p(&self, v: VSlot) -> Result<u16, Diag> {
        let s = self.x.pl.phys[v.0 as usize];
        if s == NONE {
            return Err(diag(
                ReasonCode::Internal,
                self.info(v).span,
                "emission reached a slot the layout did not place".to_owned(),
            ));
        }
        Ok(s)
    }

    /// The physical slot of a write: never a reserved one (rules 8 and 22).
    fn w(&self, v: VSlot) -> Result<u16, Diag> {
        if matches!(self.info(v).kind, SlotKind::Reserved { .. }) {
            return Err(diag(
                ReasonCode::Internal,
                self.info(v).span,
                "emission would write a reserved slot".to_owned(),
            ));
        }
        self.p(v)
    }

    /// The validity index of an i64 slot (I-2).
    fn vidx(&self, v: VSlot) -> Result<u16, Diag> {
        Ok((self.x.pl.layout.f64_len + u32::from(self.p(v)?)) as u16)
    }

    /// A `Const` slot's i64 bits: a string constant's `Sym` becomes its `CONST` `StrRef`.
    fn ibits(&self, v: VSlot, bits: u64) -> u64 {
        if self.info(v).ty.ty == Ty::String {
            let (o, l) = self.x.syms.span_of(Sym(bits as u32));
            StrRef::konst(o, l).0
        } else {
            bits
        }
    }

    fn init(&mut self) -> Result<(), Diag> {
        let g = self.p(self.x.flag)?;
        let skip = self.jump(op::JNZ, g);
        for &(v, bits, valid) in &self.x.ir.const_list {
            if self.x.pl.phys[v.0 as usize] == NONE {
                continue;
            }
            let s = self.w(v)?;
            if self.lane(v) == Lane::F64 {
                let c = self.k(if valid { bits } else { f64::NAN.to_bits() });
                self.ins(op::F_CONST, 0, s, c, 0);
            } else if valid {
                let c = self.k(self.ibits(v, bits));
                self.ins(op::I_CONST, flg::VLD, s, c, 0);
            }
            // An i64 `na` constant is a slot never written: its bit stays clear (§4.14.5).
        }
        for i in 0..self.x.ir.slots.len() {
            let v = VSlot(i as u32);
            let s = &self.x.ir.slots[i];
            let placed = self.x.pl.phys[i] != NONE;
            let skip_kind = matches!(s.kind, SlotKind::Const | SlotKind::Reserved { .. });
            if placed && !skip_kind && self.x.na.of(v) == Class::V {
                let p = self.w(v)?;
                self.ins(op::NA_CLEAR, 0, 0, p, 0);
            }
        }
        let one = self.k(1);
        self.ins(op::I_CONST, 0, g, one, 0);
        let t = self.here();
        self.patch(skip, t);
        Ok(())
    }

    fn block(&mut self, b: &Block) -> Result<(), Diag> {
        for s in &b.0 {
            self.stmt(s)?;
        }
        Ok(())
    }

    /// Branch to a patched label when the i64 bool `c` is false (or `na`, per `na_else`).
    /// Returns `(to_else, to_end)` fixups.
    fn test(&mut self, c: VSlot, na_else: bool) -> Result<(Vec<usize>, Vec<usize>), Diag> {
        if !self.i64(c) {
            return Err(not_yet("a float condition", self.info(c).span));
        }
        let (mut to_else, mut to_end) = (Vec::new(), Vec::new());
        if self.a(c) {
            let vi = self.vidx(c)?;
            let j = self.jump(op::JNA_I, vi);
            if na_else {
                to_else.push(j);
            } else {
                to_end.push(j);
            }
        }
        let pc = self.p(c)?;
        to_else.push(self.jump(op::JZ, pc));
        Ok((to_else, to_end))
    }

    fn stmt(&mut self, s: &Stmt) -> Result<(), Diag> {
        match s {
            Stmt::Op(o) => self.op(o),
            Stmt::If {
                cond,
                then,
                els,
                na_else,
                ..
            } => {
                let (to_else, mut to_end) = self.test(*cond, *na_else)?;
                self.block(then)?;
                if !els.0.is_empty() {
                    to_end.push(self.jump(op::JMP, 0));
                }
                let t = self.here();
                for j in to_else {
                    self.patch(j, t);
                }
                self.block(els)?;
                let t = self.here();
                for j in to_end {
                    self.patch(j, t);
                }
                Ok(())
            }
            Stmt::Loop {
                head,
                cond,
                body,
                step,
                ..
            } => {
                let top = self.here();
                self.block(head)?;
                // A loop test: an `na` condition ends the loop (`pin:02-na-condition` AsFalse is
                // the only reading under which a loop can continue past it — none does).
                let (mut exits, ends) = self.test(*cond, true)?;
                exits.extend(ends);
                self.loops.push((Vec::new(), Vec::new()));
                self.block(body)?;
                let cont = self.here();
                self.block(step)?;
                self.ins(op::JMP, 0, (top >> 16) as u16, top as u16, 0);
                let exit = self.here();
                let (brk, cnt) = self.loops.pop().unwrap_or_default();
                for j in exits.into_iter().chain(brk) {
                    self.patch(j, exit);
                }
                for j in cnt {
                    self.patch(j, cont);
                }
                Ok(())
            }
            Stmt::Unrolled { copies } => {
                // U13 copies (§4.7.4): each copy is one iteration — `continue` jumps to the next
                // copy, `break` past the last one.
                let mut brk_all = Vec::new();
                for c in copies {
                    self.loops.push((Vec::new(), Vec::new()));
                    self.block(c)?;
                    let (brk, cnt) = self.loops.pop().unwrap_or_default();
                    let next = self.here();
                    for j in cnt {
                        self.patch(j, next);
                    }
                    brk_all.extend(brk);
                }
                let end = self.here();
                for j in brk_all {
                    self.patch(j, end);
                }
                Ok(())
            }
            Stmt::Break | Stmt::Continue => {
                let j = self.jump(op::JMP, 0);
                let brk = matches!(s, Stmt::Break);
                match self.loops.last_mut() {
                    Some(l) if brk => l.0.push(j),
                    Some(l) => l.1.push(j),
                    None => return Err(not_yet("`break` outside a loop", Span::default())),
                }
                Ok(())
            }
        }
    }

    fn mov(&mut self, dst: VSlot, a: VSlot) -> Result<(), Diag> {
        let (d, s) = (self.w(dst)?, self.p(a)?);
        if self.i64(dst) {
            let f = self.vld(dst);
            self.ins(op::I_MOV, f, d, s, 0);
        } else {
            self.ins(op::F_MOV, 0, d, s, 0);
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines, reason = "one arm per IR op, §4.14.2–§4.14.7")]
    fn op(&mut self, o: &Op) -> Result<(), Diag> {
        match o {
            Op::Un { op: u, dst, a } => {
                let (d, s) = (self.w(*dst)?, self.p(*a)?);
                let i = self.i64(*dst);
                let f = self.vld(*dst);
                match u {
                    UnOpK::Neg if i => self.ins(op::I_NEG, f, d, s, 0),
                    UnOpK::Neg => self.ins(op::F_NEG, 0, d, s, 0),
                    UnOpK::Abs if i => self.ins(op::I_ABS, f, d, s, 0),
                    UnOpK::Abs => self.ins(op::F_ABS, 0, d, s, 0),
                    UnOpK::Not => self.ins(op::B_NOT, f, d, s, 0),
                    UnOpK::Sqrt => self.ins(op::F_SQRT, 0, d, s, 0),
                    UnOpK::Finite => self.ins(op::F_FINITE, 0, d, s, 0),
                    UnOpK::I2f => {
                        let f = if self.a(*a) { flg::VLD } else { 0 };
                        self.ins(op::F_I2F, f, d, s, 0)
                    }
                    UnOpK::F2i { round } => {
                        let f = if *round { flg::ROUND } else { 0 };
                        self.ins(op::I_F2I, f, d, s, 0)
                    }
                };
            }
            Op::Bin {
                op: b,
                dst,
                a,
                b: r,
            } => {
                let (d, x, y) = (self.w(*dst)?, self.p(*a)?, self.p(*r)?);
                let f = self.vld(*dst);
                let nf = |on: bool| if on { flg::NA_ON_NONFINITE } else { 0 };
                let (o, fl) = if self.i64(*dst) {
                    match b {
                        BinOpK::Add => (op::I_ADD, f),
                        BinOpK::Sub => (op::I_SUB, f),
                        BinOpK::Mul => (op::I_MUL, f),
                        BinOpK::Div { .. } => (op::I_DIV, 0),
                        BinOpK::Mod { .. } => (op::I_MOD, 0),
                        BinOpK::Min => (op::I_MIN, f),
                        BinOpK::Max => (op::I_MAX, f),
                        BinOpK::And => (op::B_AND, f),
                        BinOpK::Or => (op::B_OR, f),
                        BinOpK::Pow { .. } => {
                            return Err(not_yet("an int `pow`", self.info(*dst).span))
                        }
                    }
                } else {
                    match b {
                        BinOpK::Add => (op::F_ADD, 0),
                        BinOpK::Sub => (op::F_SUB, 0),
                        BinOpK::Mul => (op::F_MUL, 0),
                        BinOpK::Div { na_on_nonfinite } => (op::F_DIV, nf(*na_on_nonfinite)),
                        BinOpK::Mod { na_on_nonfinite } => (op::F_MOD, nf(*na_on_nonfinite)),
                        BinOpK::Pow { na_on_nonfinite } => (op::F_POW, nf(*na_on_nonfinite)),
                        BinOpK::Min => (op::F_MIN, 0),
                        BinOpK::Max => (op::F_MAX, 0),
                        BinOpK::And | BinOpK::Or => {
                            return Err(not_yet("a float `and`/`or`", self.info(*dst).span))
                        }
                    }
                };
                self.ins(o, fl, d, x, y);
            }
            Op::Cmp { op: c, dst, a, b } => {
                let (d, x, y) = (self.w(*dst)?, self.p(*a)?, self.p(*b)?);
                let n = match c {
                    CmpK::Lt => 0,
                    CmpK::Le => 1,
                    CmpK::Gt => 2,
                    CmpK::Ge => 3,
                    CmpK::Eq => 4,
                    CmpK::Ne => 5,
                };
                if self.i64(*a) {
                    let f = if self.a(*a) || self.a(*b) {
                        flg::NA_AWARE
                    } else {
                        0
                    };
                    self.ins(op::I_LT + n, f, d, x, y);
                } else {
                    self.ins(op::F_LT + n, 0, d, x, y);
                }
            }
            Op::Mov { dst, a } => self.mov(*dst, *a)?,
            Op::Load { dst, c } => match *c {
                Arg::Slot(s) => self.mov(*dst, s)?,
                Arg::Const { lane, bits, valid } => {
                    let d = self.w(*dst)?;
                    if lane == Lane::F64 {
                        let k = self.k(if valid { bits } else { f64::NAN.to_bits() });
                        self.ins(op::F_CONST, 0, d, k, 0);
                    } else if valid {
                        let k = self.k(self.ibits(*dst, bits));
                        let f = self.vld(*dst);
                        self.ins(op::I_CONST, f, d, k, 0);
                    } else {
                        self.ins(op::NA_SET, flg::LANE_I, 0, d, 0);
                    }
                }
                Arg::Str(_) => return Err(not_yet("an unprepared string", Span::default())),
            },
            Op::NaCheck { dst, a } => {
                let (d, s) = (self.w(*dst)?, self.p(*a)?);
                let f = if self.i64(*a) { flg::LANE_I } else { 0 };
                self.ins(op::NA_CHECK, f, d, s, 0);
            }
            Op::NaSet { a } => {
                let s = self.w(*a)?;
                let f = if self.i64(*a) { flg::LANE_I } else { 0 };
                self.ins(op::NA_SET, f, 0, s, 0);
            }
            Op::Nz { dst, a, b } => {
                let (d, x, y) = (self.w(*dst)?, self.p(*a)?, self.p(*b)?);
                let o = if self.i64(*dst) { op::NZ_I } else { op::NZ_F };
                self.ins(o, 0, d, x, y);
            }
            Op::Sel { dst, a, cond } => {
                let (d, x, c) = (self.w(*dst)?, self.p(*a)?, self.p(*cond)?);
                if self.i64(*dst) {
                    let f = self.vld(*dst);
                    self.ins(op::I_SEL, f, d, x, c);
                } else {
                    self.ins(op::F_SEL, 0, d, x, c);
                }
            }
            Op::Hist { dst, src, k } => match *k {
                K::Const(0) => self.mov(*dst, *src)?,
                K::Const(n) => {
                    let (d, r) = (self.w(*dst)?, self.ring(*src)?);
                    let kc = self.k(u64::from(n));
                    self.hist(*dst, d, r, flg::K_CONST, kc);
                }
                K::Dyn(kk) => {
                    let (d, r, s) = (self.w(*dst)?, self.ring(*src)?, self.p(kk)?);
                    self.hist(*dst, d, r, flg::K_DYNAMIC, s);
                }
            },
            Op::Call {
                ov,
                args,
                outs,
                span,
            } => {
                let o = pine_spec::overload(*ov);
                let site = if o.is_stateful() || o.is_collection() {
                    let s = self.next_site;
                    self.next_site += 1;
                    s
                } else {
                    SiteId::NONE.0
                };
                if o.sx & pine_spec::sx::WORK_LINEAR != 0 && site != SiteId::NONE.0 {
                    let l = self.x.sites.get(usize::from(site)).map_or(0, |s| s.len);
                    self.ins(op::BUDGET, 0, 0, (l & 0xFFFF) as u16, (l >> 16) as u16);
                }
                let w = self.out.argwin.len();
                if w > usize::from(u16::MAX) {
                    let msg = "argwin exceeds 65 536 entries".to_owned();
                    return Err(diag(ReasonCode::FrameTooLarge, *span, msg));
                }
                self.out.argwin.push((args.len() + outs.len()) as u16);
                for a in args {
                    let Arg::Slot(v) = *a else {
                        return Err(not_yet("an unprepared argument", *span));
                    };
                    let e = self.entry(v, false)?;
                    self.out.argwin.push(e);
                }
                for &v in outs {
                    let e = self.entry(v, true)?;
                    self.out.argwin.push(e);
                }
                self.ins(op::CALL_BUILTIN, 0, site, ov.0, w as u16);
            }
            Op::Persist { dst, src } => {
                let (d, s) = (self.w(*dst)?, self.p(*src)?);
                let c = self.x.pl.cell_of[dst.0 as usize];
                if c == NONE {
                    return Err(not_yet("a string store with no cell", self.info(*dst).span));
                }
                self.ins(op::S_PERSIST, 0, d, s, c);
            }
            Op::Budget { units } => {
                self.ins(
                    op::BUDGET,
                    0,
                    0,
                    (units & 0xFFFF) as u16,
                    (units >> 16) as u16,
                );
            }
            Op::ObjNew { dst, span, .. } => {
                let site = self.next_site;
                self.next_site += 1;
                let n = match self.x.sites.get(usize::from(site)) {
                    Some(s) if s.kind == pine_ir::StateKind::Object => (s.bytes - 32) / 8,
                    _ => return Err(not_yet("an object site the numbering did not give", *span)),
                };
                let d = self.w(*dst)?;
                self.ins(op::O_NEW, 0, d, site, n as u16);
            }
            Op::ObjGet { dst, h, field } => {
                let (d, hs) = (self.w(*dst)?, self.p(*h)?);
                if self.i64(*dst) {
                    let f = self.tracked(*dst);
                    self.ins(op::O_GET_I, f, d, hs, *field);
                } else {
                    self.ins(op::O_GET_F, 0, d, hs, *field);
                }
            }
            Op::ObjSet { h, field, src } => {
                let si = self.info(*src);
                if si.ty.ty == Ty::String && !matches!(si.kind, SlotKind::Stage { .. }) {
                    // CAR-03-1 (a): a field never holds a SCRATCH ref (refs.rs stages it).
                    return Err(not_yet("an unstaged string field store", si.span));
                }
                let (hs, v) = (self.p(*h)?, self.p(*src)?);
                if self.i64(*src) {
                    let f = self.tracked(*src);
                    self.ins(op::O_SET_I, f, hs, *field, v);
                } else {
                    self.ins(op::O_SET_F, 0, hs, *field, v);
                }
            }
        }
        Ok(())
    }

    /// `NA_TRACKED` on an i64 field access iff the field's type can be `na` in the dialect (a
    /// v6 `bool` never is, §4.14.7).
    fn tracked(&self, v: VSlot) -> u8 {
        if self.info(v).ty.ty == Ty::Bool && self.x.d == Dialect::V6 {
            0
        } else {
            flg::NA_TRACKED
        }
    }

    fn ring(&self, v: VSlot) -> Result<u16, Diag> {
        let r = self.x.pl.ring_of[v.0 as usize];
        if r == NONE {
            return Err(diag(
                ReasonCode::Internal,
                self.info(v).span,
                "a history read of a slot without a ring".to_owned(),
            ));
        }
        Ok(r)
    }

    fn hist(&mut self, dst: VSlot, d: u16, r: u16, kf: u8, b: u16) {
        if self.i64(dst) {
            let nt = self.x.pl.layout.rings[usize::from(r)].flags & ring_flag::NA_TRACKED;
            let f = if nt != 0 { flg::NA_TRACKED } else { 0 };
            self.ins(op::HIST_I, kf | f, d, r, b);
        } else {
            self.ins(op::HIST_F, kf, d, r, b);
        }
    }

    /// An argument-window entry (§4.14.5); an output is never a reserved slot.
    fn entry(&self, v: VSlot, out: bool) -> Result<u16, Diag> {
        let s = if out { self.w(v)? } else { self.p(v)? };
        Ok(if self.i64(v) {
            argwin::i64(s)
        } else {
            argwin::f64(s)
        })
    }
}

const _: () = assert!(abi::RESERVED_I == 10);
