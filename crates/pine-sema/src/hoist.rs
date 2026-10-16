// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 4's per-call-site transforms (impl-02 §4.7.4–§4.7.5), run on the IR before numbering:
//! U13 `per_iteration` unrolls every loop whose body holds a stateful call into one copy per
//! iteration, each preceded by the counter's constant for that iteration (the copies share every
//! slot; only the stateful calls become per-copy sites). A loop without a constant trip count, or
//! with more than `MAX_UNROLL` iterations, is `3013` naming the loop. `pin:06-cond-call-state`
//! `EveryBar` moves every stateful call of an `if` / `switch` region, with the pure ops of the
//! region that compute its arguments, to just before the outermost such region of its loop body
//! (or the top level); a call whose slice reaches a slot the region writes outside the slice, or
//! an op that cannot move, is `3039` naming it. Never hoisted from: a ternary arm, a lazy
//! `and`/`or` operand, a `var` / `varip` / `once` guard, a loop body (U13).

use pine_ir::{Diag, Lane, Qual, Ty};
use pine_spec::swr;

use crate::code::{diag, ReasonCode};
use crate::ir::{Arg, Block, CondKind, Ctr, Ir, Op, SlotInfo, SlotKind, Stmt, VSlot, K};
use crate::limits::MAX_UNROLL;
use crate::pins::CondCallState;
use crate::sw::Readings;

/// Run the transforms the readings select — over the body and every request's detached `E`
/// block (§4.11.2: `E` is lowered like any expression, so its loops and conditional calls follow
/// the same readings; the child's cone then slices the transformed block).
pub(crate) fn run(ir: &mut Ir, rd: &Readings) -> Result<(), Diag> {
    if rd.ta_in_loop == swr::TA_IN_LOOP_PER_ITERATION {
        let mut body = std::mem::take(&mut ir.body);
        let r = unroll(ir, &mut body);
        ir.body = body;
        r?;
        for i in 0..ir.requests.len() {
            let mut e = std::mem::take(&mut ir.requests[i].e);
            let r = unroll(ir, &mut e);
            ir.requests[i].e = e;
            r?;
        }
    }
    if rd.pins.cond_call_state == CondCallState::EveryBar {
        let temps: Vec<bool> = ir.slots.iter().map(|s| s.kind == SlotKind::Temp).collect();
        every_bar(&mut ir.body, &temps)?;
        for i in 0..ir.requests.len() {
            every_bar(&mut ir.requests[i].e, &temps)?;
        }
    }
    Ok(())
}

fn is_stateful(o: &Op) -> bool {
    matches!(o, Op::Call { ov, .. } if pine_spec::overload(*ov).is_stateful())
}

/// The slots an op writes.
pub(crate) fn writes(o: &Op) -> Vec<VSlot> {
    match o {
        Op::Un { dst, .. }
        | Op::Bin { dst, .. }
        | Op::Cmp { dst, .. }
        | Op::Mov { dst, .. }
        | Op::Load { dst, .. }
        | Op::NaCheck { dst, .. }
        | Op::Nz { dst, .. }
        | Op::Sel { dst, .. }
        | Op::Hist { dst, .. }
        | Op::ObjNew { dst, .. }
        | Op::ObjGet { dst, .. }
        | Op::Persist { dst, .. } => vec![*dst],
        Op::NaSet { a } => vec![*a],
        Op::Call { outs, .. } => outs.clone(),
        Op::ObjSet { .. } | Op::Budget { .. } => Vec::new(),
    }
}

/// The slots an op reads.
pub(crate) fn reads(o: &Op) -> Vec<VSlot> {
    match o {
        Op::Un { a, .. } | Op::Mov { a, .. } | Op::NaCheck { a, .. } => vec![*a],
        Op::Bin { a, b, .. } | Op::Cmp { a, b, .. } | Op::Nz { a, b, .. } => vec![*a, *b],
        Op::Sel { dst, a, cond } => vec![*dst, *a, *cond],
        Op::Load {
            c: Arg::Slot(s), ..
        } => vec![*s],
        Op::Hist { src, k, .. } => match k {
            K::Dyn(d) => vec![*src, *d],
            K::Const(_) => vec![*src],
        },
        Op::Call { args, .. } => args
            .iter()
            .filter_map(|a| match a {
                Arg::Slot(s) => Some(*s),
                _ => None,
            })
            .collect(),
        Op::ObjGet { h, .. } => vec![*h],
        Op::ObjSet { h, src, .. } => vec![*h, *src],
        Op::Persist { src, .. } => vec![*src],
        _ => Vec::new(),
    }
}

/// Ops a slice may carry out of a region: no side effect, no state — the operators and a
/// built-in call that neither keeps state, creates, captures, pushes intents nor mutates its
/// first argument.
fn pure(o: &Op) -> bool {
    match o {
        Op::Un { .. }
        | Op::Bin { .. }
        | Op::Cmp { .. }
        | Op::Mov { .. }
        | Op::Load { .. }
        | Op::NaCheck { .. }
        | Op::Nz { .. }
        | Op::Sel { .. }
        | Op::Hist { .. } => true,
        Op::Call { ov, .. } => {
            let x = pine_spec::overload(*ov);
            !x.is_stateful()
                && !x.is_collection()
                && !x.is_capture()
                && !x.pushes_intents()
                && x.sx & pine_spec::sx::MUTATES_ARG0 == 0
        }
        _ => false,
    }
}

fn all_writes(b: &Block, out: &mut Vec<VSlot>) {
    for s in &b.0 {
        stmt_writes(s, out);
    }
}

fn stmt_writes(s: &Stmt, out: &mut Vec<VSlot>) {
    match s {
        Stmt::Op(o) => out.extend(writes(o)),
        Stmt::If { then, els, .. } => {
            all_writes(then, out);
            all_writes(els, out);
        }
        Stmt::Loop {
            head, body, step, ..
        } => {
            all_writes(head, out);
            all_writes(body, out);
            all_writes(step, out);
        }
        Stmt::Unrolled { copies } => copies.iter().for_each(|c| all_writes(c, out)),
        Stmt::Break | Stmt::Continue => {}
    }
}

/// `pin:06-cond-call-state` `EveryBar` over one loop-body level (or the top level).
fn every_bar(b: &mut Block, temps_slots: &[bool]) -> Result<(), Diag> {
    let mut i = 0;
    while i < b.0.len() {
        match &mut b.0[i] {
            Stmt::If {
                then,
                els,
                kind: CondKind::If | CondKind::Switch,
                ..
            } => {
                let mut region = Vec::new();
                all_writes(then, &mut region);
                all_writes(els, &mut region);
                let mut pre = Vec::new();
                lift(then, &mut pre, &mut region, temps_slots)?;
                lift(els, &mut pre, &mut region, temps_slots)?;
                let n = pre.len();
                b.0.splice(i..i, pre);
                i += n + 1;
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                every_bar(head, temps_slots)?;
                every_bar(body, temps_slots)?;
                every_bar(step, temps_slots)?;
                i += 1;
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    every_bar(c, temps_slots)?;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// Move every stateful call of `b` (and of its nested `if` / `switch` regions) with its slice
/// into `pre`, in order; `region` holds the writes still inside the outermost region.
fn lift(
    b: &mut Block,
    pre: &mut Vec<Stmt>,
    region: &mut Vec<VSlot>,
    temps_slots: &[bool],
) -> Result<(), Diag> {
    let mut j = 0;
    while j < b.0.len() {
        let call = match &mut b.0[j] {
            Stmt::Op(o) if is_stateful(o) => o.clone(),
            Stmt::If {
                then,
                els,
                kind: CondKind::If | CondKind::Switch,
                ..
            } => {
                lift(then, pre, region, temps_slots)?;
                lift(els, pre, region, temps_slots)?;
                j += 1;
                continue;
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                every_bar(head, temps_slots)?;
                every_bar(body, temps_slots)?;
                every_bar(step, temps_slots)?;
                j += 1;
                continue;
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    every_bar(c, temps_slots)?;
                }
                j += 1;
                continue;
            }
            _ => {
                j += 1;
                continue;
            }
        };
        let Op::Call { ov, span, .. } = &call else {
            j += 1;
            continue;
        };
        let refuse = || {
            let name = pine_spec::entry_of(pine_spec::overload(*ov)).full_str();
            let msg = format!(
                "`{name}` cannot run every bar: its arguments depend on its block                  (pin:06-cond-call-state)"
            );
            diag(ReasonCode::HistOnNonSlot, *span, msg)
        };
        // The backward slice: the ops of this block before the call that compute its arguments.
        let mut need = reads(&call);
        let mut read = need.clone();
        let mut slice = Vec::new();
        for k in (0..j).rev() {
            match &b.0[k] {
                Stmt::Op(o) => {
                    let w = writes(o);
                    if w.iter().any(|x| need.contains(x)) {
                        // `pin:02-hoist-slice-temps` — a slice moves temporaries only: moving a
                        // variable's store would run it on bars the region skips.
                        let temps = w
                            .iter()
                            .all(|x| temps_slots.get(x.0 as usize).copied().unwrap_or(false));
                        if !pure(o) || !temps {
                            return Err(refuse());
                        }
                        slice.push(k);
                        need.retain(|x| !w.contains(x));
                        let r = reads(o);
                        need.extend(r.iter().copied());
                        read.extend(r);
                    }
                }
                other => {
                    let mut w = Vec::new();
                    stmt_writes(other, &mut w);
                    if w.iter().any(|x| need.contains(x)) {
                        return Err(refuse());
                    }
                }
            }
        }
        // Every slot the slice and the call read is written in the region only by the slice.
        let mut moved_w = Vec::new();
        for &k in &slice {
            if let Stmt::Op(o) = &b.0[k] {
                moved_w.extend(writes(o));
            }
        }
        for x in &read {
            let inside = region.iter().filter(|y| *y == x).count();
            let ours = moved_w.iter().filter(|y| *y == x).count();
            if inside > ours {
                return Err(refuse());
            }
        }
        // Move: the slice in order, then the call.
        slice.sort_unstable();
        slice.push(j);
        for &k in slice.iter().rev() {
            if let Stmt::Op(o) = &b.0[k] {
                for w in writes(o) {
                    if let Some(p) = region.iter().position(|y| *y == w) {
                        region.swap_remove(p);
                    }
                }
            }
        }
        let start = pre.len();
        for &k in slice.iter().rev() {
            pre.insert(start, b.0.remove(k));
        }
        j -= slice.len() - 1;
    }
    Ok(())
}

/// Whether `b` holds a stateful call, at any depth.
fn stateful(b: &Block) -> bool {
    b.0.iter().any(|s| match s {
        Stmt::Op(Op::Call { ov, .. }) => pine_spec::overload(*ov).is_stateful(),
        Stmt::If { then, els, .. } => stateful(then) || stateful(els),
        Stmt::Loop {
            head, body, step, ..
        } => stateful(head) || stateful(body) || stateful(step),
        Stmt::Unrolled { copies } => copies.iter().any(stateful),
        _ => false,
    })
}

/// U13 `per_iteration`, innermost loops first.
fn unroll(ir: &mut Ir, b: &mut Block) -> Result<(), Diag> {
    for s in &mut b.0 {
        match s {
            Stmt::If { then, els, .. } => {
                unroll(ir, then)?;
                unroll(ir, els)?;
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    unroll(ir, c)?;
                }
            }
            Stmt::Loop {
                span,
                ctr,
                head,
                body,
                step,
                trip,
                ..
            } => {
                unroll(ir, head)?;
                unroll(ir, body)?;
                unroll(ir, step)?;
                if !stateful(body) && !stateful(head) {
                    continue;
                }
                let (Some(n), Some(c)) = (*trip, *ctr) else {
                    let msg = "a loop holding a stateful call needs a constant trip count \
                               under `ta_in_loop = per_iteration` (U13)"
                        .to_owned();
                    return Err(diag(ReasonCode::MonoExplosion, *span, msg));
                };
                if n > MAX_UNROLL {
                    let msg = format!("a loop of {n} iterations exceeds MAX_UNROLL {MAX_UNROLL}");
                    return Err(diag(ReasonCode::MonoExplosion, *span, msg));
                }
                let mut copies = Vec::with_capacity(n as usize);
                for k in 0..n {
                    let v = counter_const(ir, c, k);
                    let mut blk = Vec::with_capacity(body.0.len() + 1);
                    blk.push(Stmt::Op(Op::Mov { dst: c.slot, a: v }));
                    blk.extend(body.0.iter().cloned());
                    copies.push(Block(blk));
                }
                *s = Stmt::Unrolled { copies };
            }
            _ => {}
        }
    }
    Ok(())
}

/// The `Const` slot of the counter's value on iteration `k`.
fn counter_const(ir: &mut Ir, c: Ctr, k: u32) -> crate::ir::VSlot {
    let base = ir.slots[c.slot.0 as usize];
    let x = c.from + f64::from(k) * c.delta;
    let bits = if base.ty.ty == Ty::Float {
        x.to_bits()
    } else {
        x as i64 as u64
    };
    let info = SlotInfo {
        ty: pine_ir::TypeQ {
            qual: Qual::Const,
            ..base.ty
        },
        lane: if base.ty.ty == Ty::Float {
            Lane::F64
        } else {
            Lane::I64
        },
        kind: SlotKind::Const,
        ..base
    };
    ir.konst(info, bits, true)
}
