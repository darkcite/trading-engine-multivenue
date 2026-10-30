// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 5d (impl-02 §4.8): `can_be_na` over the final IR — a monotone fixpoint, each i64 slot
//! flips at most once — then every i64 slot's class: **A** (`can_be_na`: every write maintains
//! validity), **V** (never `na`, validity read: `NA_CLEAR` once in the init block), **N** (never
//! `na`, validity never read). f64 slots are always N (NaN is `na`, I-2). Reserved slots are
//! written valid by the VM before every evaluation (impl-03 `step_bar`), so they are never `na`
//! and never need a class-V clear.

use pine_ir::{Dialect, Lane, Ty};

use crate::ir::{Arg, Block, Ir, Op, SlotKind, Stmt, UnOpK, VSlot, K};

/// An i64 slot's class (§4.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// `can_be_na`.
    A,
    /// Never `na`, validity read.
    V,
    /// Never `na`, validity never read (and every f64 slot).
    N,
}

/// The result of the pass.
#[derive(Debug, Clone)]
pub(crate) struct NaCap {
    /// `class[v.0]`.
    pub(crate) class: Vec<Class>,
}

impl NaCap {
    /// The class of `v`.
    pub(crate) fn of(&self, v: VSlot) -> Class {
        self.class[v.0 as usize]
    }

    /// `v` is class A.
    pub(crate) fn a(&self, v: VSlot) -> bool {
        self.of(v) == Class::A
    }
}

/// Run the fixpoint and classify.
pub(crate) fn run(ir: &Ir, d: Dialect) -> NaCap {
    let n = ir.slots.len();
    let i64s: Vec<bool> = ir.slots.iter().map(|s| s.lane == Lane::I64).collect();
    // A v6 `bool` is never `na` (W13); a reserved slot is written valid by the VM.
    let never: Vec<bool> = ir
        .slots
        .iter()
        .map(|s| {
            s.lane == Lane::F64
                || (d == Dialect::V6 && s.ty.ty == Ty::Bool)
                || matches!(s.kind, SlotKind::Reserved { .. })
        })
        .collect();
    let mut na = vec![false; n];
    for &(v, _, valid) in &ir.const_list {
        if !valid && !never[v.0 as usize] {
            na[v.0 as usize] = true;
        }
    }
    loop {
        let mut cx = Fix {
            na: &mut na,
            never: &never,
            changed: false,
        };
        cx.block(&ir.body);
        if !cx.changed {
            break;
        }
    }
    let mut class: Vec<Class> = (0..n)
        .map(|i| if i64s[i] && na[i] { Class::A } else { Class::N })
        .collect();
    let mut rd = Reads {
        ir,
        class: &mut class,
        i64s: &i64s,
    };
    rd.block(&ir.body);
    // A ringed i64 slot's validity is read by the bar commit (§4.10.1).
    let mut ringed = Vec::new();
    hist_srcs(&ir.body, &mut ringed);
    for v in ringed {
        rd.read(v);
    }
    NaCap { class }
}

struct Fix<'a> {
    na: &'a mut Vec<bool>,
    never: &'a [bool],
    changed: bool,
}

impl Fix<'_> {
    fn set(&mut self, v: VSlot, x: bool) {
        let i = v.0 as usize;
        if x && !self.never[i] && !self.na[i] {
            self.na[i] = true;
            self.changed = true;
        }
    }

    fn is(&self, v: VSlot) -> bool {
        self.na[v.0 as usize]
    }

    fn block(&mut self, b: &Block) {
        for s in &b.0 {
            match s {
                Stmt::Op(o) => self.op(o),
                Stmt::If { then, els, .. } => {
                    self.block(then);
                    self.block(els);
                }
                Stmt::Loop {
                    head, body, step, ..
                } => {
                    self.block(head);
                    self.block(body);
                    self.block(step);
                }
                Stmt::Unrolled { copies } => {
                    for c in copies {
                        self.block(c);
                    }
                }
                Stmt::Break | Stmt::Continue => {}
            }
        }
    }

    fn op(&mut self, o: &Op) {
        match o {
            Op::Un { op, dst, a } => {
                let x = match op {
                    UnOpK::F2i { .. } => true,
                    UnOpK::Neg | UnOpK::Abs | UnOpK::Not => self.is(*a),
                    UnOpK::I2f | UnOpK::Sqrt | UnOpK::Finite => false,
                };
                self.set(*dst, x);
            }
            Op::Bin { op, dst, a, b } => {
                use crate::ir::BinOpK;
                let x = matches!(op, BinOpK::Div { .. } | BinOpK::Mod { .. })
                    || self.is(*a)
                    || self.is(*b);
                self.set(*dst, x);
            }
            Op::Mov { dst, a } | Op::Sel { dst, a, .. } | Op::Persist { dst, src: a } => {
                let x = self.is(*a);
                self.set(*dst, x);
            }
            Op::Load { dst, c } => {
                let x = match c {
                    Arg::Slot(s) => self.is(*s),
                    Arg::Const { valid, .. } => !valid,
                    Arg::Str(_) => false,
                };
                self.set(*dst, x);
            }
            Op::NaSet { a } => self.set(*a, true),
            Op::Hist { dst, .. } | Op::ObjGet { dst, .. } => self.set(*dst, true),
            Op::Call { outs, .. } => {
                for &v in outs {
                    self.set(v, true);
                }
            }
            Op::Cmp { .. }
            | Op::NaCheck { .. }
            | Op::Nz { .. }
            | Op::ObjNew { .. }
            | Op::ObjSet { .. }
            | Op::Budget { .. } => {}
        }
    }
}

/// Every slot some `Hist` op reads (its ring owner).
pub(crate) fn hist_srcs(b: &Block, out: &mut Vec<VSlot>) {
    for s in &b.0 {
        match s {
            Stmt::Op(Op::Hist { src, .. }) => out.push(*src),
            Stmt::Op(_) | Stmt::Break | Stmt::Continue => {}
            Stmt::If { then, els, .. } => {
                hist_srcs(then, out);
                hist_srcs(els, out);
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                hist_srcs(head, out);
                hist_srcs(body, out);
                hist_srcs(step, out);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    hist_srcs(c, out);
                }
            }
        }
    }
}

/// The validity-read rule of §4.8's class V.
struct Reads<'a> {
    ir: &'a Ir,
    class: &'a mut Vec<Class>,
    i64s: &'a [bool],
}

impl Reads<'_> {
    fn read(&mut self, v: VSlot) {
        let i = v.0 as usize;
        let reserved = matches!(self.ir.slots[i].kind, SlotKind::Reserved { .. });
        if self.i64s[i] && !reserved && self.class[i] == Class::N {
            self.class[i] = Class::V;
        }
    }

    fn a(&self, v: VSlot) -> bool {
        self.class[v.0 as usize] == Class::A
    }

    fn block(&mut self, b: &Block) {
        for s in &b.0 {
            match s {
                Stmt::Op(o) => self.op(o),
                Stmt::If {
                    cond, then, els, ..
                } => {
                    // `JNA_I` reads the condition's bit only when it is class A.
                    let _ = cond;
                    self.block(then);
                    self.block(els);
                }
                Stmt::Loop {
                    head, body, step, ..
                } => {
                    self.block(head);
                    self.block(body);
                    self.block(step);
                }
                Stmt::Unrolled { copies } => {
                    for c in copies {
                        self.block(c);
                    }
                }
                Stmt::Break | Stmt::Continue => {}
            }
        }
    }

    fn op(&mut self, o: &Op) {
        match o {
            // A `VLD` opcode (its dst class A) reads its operands' bits; `F_I2F` carries `VLD`
            // iff its operand is A, so it never reads a non-A bit.
            Op::Un { op, dst, a } => {
                if !matches!(op, UnOpK::I2f | UnOpK::F2i { .. }) && self.a(*dst) {
                    self.read(*a);
                }
            }
            Op::Bin { dst, a, b, .. } => {
                if self.a(*dst) {
                    self.read(*a);
                    self.read(*b);
                }
            }
            Op::Mov { dst, a } => {
                if self.a(*dst) {
                    self.read(*a);
                }
            }
            Op::Load {
                dst,
                c: Arg::Slot(s),
            } => {
                if self.a(*dst) {
                    self.read(*s);
                }
            }
            Op::Sel { dst, a, .. } => {
                if self.a(*dst) {
                    self.read(*a);
                    self.read(*dst);
                }
            }
            // `NA_AWARE` (an operand is A) reads both operands' bits.
            Op::Cmp { a, b, .. } => {
                if self.a(*a) || self.a(*b) {
                    self.read(*a);
                    self.read(*b);
                }
            }
            Op::NaCheck { a, .. } => self.read(*a),
            Op::Nz { a, b, .. } => {
                self.read(*a);
                self.read(*b);
            }
            Op::Call { args, .. } => {
                for x in args {
                    if let Arg::Slot(s) = x {
                        self.read(*s);
                    }
                }
            }
            Op::Persist { src, .. } => self.read(*src),
            // `O_GET_*` / `O_SET_*` read the handle's validity bit (impl-03 `o_field`: an `na`
            // handle faults `ScriptRefused`).
            Op::ObjSet { h, src, .. } => {
                self.read(*h);
                self.read(*src);
            }
            Op::ObjGet { h, .. } => self.read(*h),
            Op::Hist { k: K::Dyn(_), .. }
            | Op::Hist { .. }
            | Op::Load { .. }
            | Op::NaSet { .. }
            | Op::ObjNew { .. }
            | Op::Budget { .. } => {}
        }
    }
}
