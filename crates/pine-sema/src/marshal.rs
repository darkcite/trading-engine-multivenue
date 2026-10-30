// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! CAR-R33-5 (r39, R-P28): a script `int` is a binary64 and lives in the f64 lane (TradingView
//! F011 F037 F040 F047 F120: no wrap at `2^63`, round to nearest above `2^53`). A builtin keeps
//! its `int` rows in the i64 lane — its body reads them with `Frame::i64_arg`, the argument
//! window names the lane per entry — so every builtin call converts at its boundary:
//!
//! * an `int` argument slot in the f64 lane becomes an i64 temporary written by `I_F2I` just
//!   before the call (truncation of an integral value: exact; `na`, ±inf and `|x| ≥ 2^63` are
//!   `na`, the one place a binary64 int beyond `i64` cannot follow); a `Const` one becomes the i64
//!   constant of the same value, no instruction;
//! * an `int` result in the f64 lane is written to an i64 temporary by the call and widened by
//!   `F_I2F` (`na` → NaN) right after.
//!
//! `request.security` is exempt: its element outputs are the parent's slots by identity (the cone
//! patches the call by them) and `pine-std`'s `write_typed` writes an f64-lane `int` directly.

use pine_ir::{Lane, Qual, Span, Ty, TypeQ};

use crate::ir::{Arg, Block, Ir, Op, SlotInfo, SlotKind, Stmt, UnOpK, VSlot};

/// Rewrite every builtin call of the program, the requests' detached blocks included.
pub(crate) fn run(ir: &mut Ir) {
    let mut body = std::mem::take(&mut ir.body);
    block(ir, &mut body);
    ir.body = body;
    let mut reqs = std::mem::take(&mut ir.requests);
    for r in &mut reqs {
        block(ir, &mut r.e);
        block(ir, &mut r.tail);
    }
    ir.requests = reqs;
}

/// Whether `v` is a script `int` in the f64 lane.
fn f64_int(ir: &Ir, v: VSlot) -> bool {
    ir.slots
        .get(v.0 as usize)
        .is_some_and(|s| s.ty.ty == Ty::Int && s.lane == Lane::F64)
}

/// An i64-lane `int` temporary shaped like `v`.
fn i64_temp(ir: &mut Ir, v: VSlot) -> VSlot {
    let s = ir.slots[v.0 as usize];
    ir.slot(SlotInfo {
        ty: TypeQ::scalar(
            Ty::Int,
            if s.ty.qual == Qual::Const {
                Qual::Series
            } else {
                s.ty.qual
            },
        ),
        lane: Lane::I64,
        kind: SlotKind::Temp,
        name: pine_ir::Sym::NONE,
        cell: false,
        ..s
    })
}

/// The i64 `Const` of an f64-lane `int` constant: its value, saturating (the `I_F2I` reading of a
/// value beyond `i64` would be `na`; a constant never is).
fn i64_const(ir: &mut Ir, v: VSlot) -> Option<VSlot> {
    let s = ir.slots[v.0 as usize];
    if s.kind != SlotKind::Const {
        return None;
    }
    let (bits, valid) = ir.const_of(v)?;
    let x = f64::from_bits(bits);
    let (bits, valid) = if valid && x.is_finite() {
        ((x as i64) as u64, true)
    } else {
        (0, false)
    };
    let info = SlotInfo {
        ty: TypeQ::scalar(Ty::Int, Qual::Const),
        lane: Lane::I64,
        kind: SlotKind::Const,
        name: pine_ir::Sym::NONE,
        cell: false,
        span: Span::default(),
        ..s
    };
    Some(ir.konst(info, bits, valid))
}

fn block(ir: &mut Ir, b: &mut Block) {
    let mut out: Vec<Stmt> = Vec::with_capacity(b.0.len());
    for mut s in std::mem::take(&mut b.0) {
        match &mut s {
            Stmt::Op(Op::Call { ov, args, outs, .. }) if !crate::security::is_security(*ov) => {
                for a in args.iter_mut() {
                    let Arg::Slot(v) = *a else { continue };
                    if !f64_int(ir, v) {
                        continue;
                    }
                    if let Some(k) = i64_const(ir, v) {
                        *a = Arg::Slot(k);
                        continue;
                    }
                    let t = i64_temp(ir, v);
                    out.push(Stmt::Op(Op::Un {
                        op: UnOpK::F2i { round: false },
                        dst: t,
                        a: v,
                    }));
                    *a = Arg::Slot(t);
                }
                let mut after: Vec<Stmt> = Vec::new();
                for o in outs.iter_mut() {
                    if !f64_int(ir, *o) {
                        continue;
                    }
                    let t = i64_temp(ir, *o);
                    after.push(Stmt::Op(Op::Un {
                        op: UnOpK::I2f,
                        dst: *o,
                        a: t,
                    }));
                    *o = t;
                }
                out.push(s);
                out.extend(after);
                continue;
            }
            Stmt::If { then, els, .. } => {
                block(ir, then);
                block(ir, els);
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                block(ir, head);
                block(ir, body);
                block(ir, step);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    block(ir, c);
                }
            }
            _ => {}
        }
        out.push(s);
    }
    b.0 = out;
}
