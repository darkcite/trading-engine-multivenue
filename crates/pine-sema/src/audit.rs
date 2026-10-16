// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! impl-02 §4.13.2 (I5, I6): `recount` rebuilds every region's demand from the IR by a separate
//! walk that shares no code with the builder — slots per lane and class, ring cells, blocks per
//! kind, cells per class, plots, intents — and compares it with the layout. Any inequality is
//! `3255`: an `Ok` program's layout is by construction the one the VM allocates.

use pine_ir::{abi, Diag, FrameLayout, Lane, Span, StateKind};

use crate::code::{diag, ReasonCode};
use crate::ir::{Arg, Block, Ir, Op, SlotKind, Stmt, VSlot, K};

/// What the audit compares against besides the IR.
pub(crate) struct Want<'a> {
    /// The layout under audit.
    pub(crate) l: &'a FrameLayout,
    /// The init flag (referenced by the init block only).
    pub(crate) flag: VSlot,
    /// `max_intents_per_eval`.
    pub(crate) intents: u32,
    /// The effective cap `C` a dynamic offset sizes its ring with (§4.10.4).
    pub(crate) cap: u32,
}

fn bad(what: &str, got: u64, want: u64) -> Diag {
    let msg = format!("layout audit: {what} is {got}, the IR needs {want}");
    diag(ReasonCode::Internal, Span::default(), msg)
}

/// Recount and compare.
pub(crate) fn recount(ir: &Ir, w: &Want<'_>) -> Result<(), Diag> {
    let mut seen = vec![false; ir.slots.len()];
    let mut hist: Vec<(u32, u32)> = Vec::new();
    let mut builtin = 0u64;
    let mut pooled = 0u64;
    walk(&ir.body, &mut seen, &mut hist, &mut builtin, &mut pooled);
    seen[w.flag.0 as usize] = true;
    // [f64 plain, f64 varip, i64 plain, i64 varip], cells [plain, varip]
    let mut n = [0u64; 4];
    let mut cells = [0u64; 2];
    for (i, s) in ir.slots.iter().enumerate() {
        if !seen[i] || matches!(s.kind, SlotKind::Reserved { .. }) {
            continue;
        }
        let varip = matches!(s.kind, SlotKind::Varip | SlotKind::Flag { varip: true });
        let k = if s.lane == Lane::F64 { 0 } else { 2 } + usize::from(varip);
        n[k] += 1;
        if s.cell {
            cells[usize::from(matches!(s.kind, SlotKind::Varip))] += 1;
        }
    }
    let l = w.l;
    let rf = u64::from(abi::RESERVED_F);
    let ri = u64::from(abi::RESERVED_I);
    let eq = |what: &str, got: u64, want: u64| {
        if got == want {
            Ok(())
        } else {
            Err(bad(what, got, want))
        }
    };
    eq("f64 plain slots", u64::from(l.f64_varip_from) - rf, n[0])?;
    eq(
        "f64 varip slots",
        u64::from(l.f64_len - l.f64_varip_from),
        n[1],
    )?;
    eq(
        "i64 varip slots",
        u64::from(l.i64_len - l.i64_varip_from),
        n[3],
    )?;
    let plain = u64::from(l.i64_varip_from) - ri;
    let pad_ok = if n[3] == 0 {
        plain == n[2]
    } else {
        plain >= n[2] && plain - n[2] < 64
    };
    if !pad_ok {
        return Err(bad("i64 plain slots (with the I-6 pad)", plain, n[2]));
    }
    // Rings: one per read slot, capacity from the largest constant offset or the cap.
    let mut caps: Vec<(u32, u32)> = Vec::new();
    for &(v, k) in &hist {
        let k = if k == u32::MAX { w.cap } else { k };
        match caps.iter_mut().find(|c| c.0 == v) {
            Some(c) => c.1 = c.1.max(k),
            None => caps.push((v, k)),
        }
    }
    eq("rings", l.rings.len() as u64, caps.len() as u64)?;
    let mut ring_cells = 0u64;
    let mut str_cells = 0u64;
    for r in &l.rings {
        let c = u64::from(r.mask) + 1;
        ring_cells += c;
        if r.flags & pine_ir::ring_flag::STRING != 0 {
            str_cells += c;
        }
    }
    let need: u64 = caps
        .iter()
        .map(|c| u64::from((1 + c.1).next_power_of_two().max(2)))
        .sum();
    eq("ring cells", ring_cells, need)?;
    // Blocks per kind.
    let (mut lb, mut lp) = (0u64, 0u64);
    for s in &l.states {
        if s.kind == StateKind::Builtin {
            lb += 1;
        } else {
            lp += 1;
        }
    }
    eq("builtin blocks", lb, builtin)?;
    eq("pool blocks", lp, pooled)?;
    // Cells per class.
    let cb = u64::from(l.str_cell_bytes);
    if cb > 0 {
        let all = u64::from(l.str_persist_bytes) / cb;
        let varip = (u64::from(l.str_persist_off) + u64::from(l.str_persist_bytes)
            - u64::from(l.str_persist_varip_off))
            / cb;
        eq("varip cells", varip, cells[1])?;
        eq("variable cells", all - varip - str_cells, cells[0])?;
    } else {
        eq("cells", 0, cells[0] + cells[1] + str_cells)?;
    }
    eq("plots", u64::from(l.n_plots), ir.plots.len() as u64)?;
    eq("intents", u64::from(l.intent_cap), u64::from(w.intents))
}

fn mark(seen: &mut [bool], v: VSlot) {
    seen[v.0 as usize] = true;
}

fn walk(b: &Block, seen: &mut [bool], hist: &mut Vec<(u32, u32)>, bl: &mut u64, po: &mut u64) {
    for s in &b.0 {
        match s {
            Stmt::Op(o) => op(o, seen, hist, bl, po),
            Stmt::If {
                cond, then, els, ..
            } => {
                mark(seen, *cond);
                walk(then, seen, hist, bl, po);
                walk(els, seen, hist, bl, po);
            }
            Stmt::Loop {
                head,
                cond,
                body,
                step,
                ctr,
                ..
            } => {
                if let Some(c) = ctr {
                    mark(seen, c.slot);
                }
                mark(seen, *cond);
                walk(head, seen, hist, bl, po);
                walk(body, seen, hist, bl, po);
                walk(step, seen, hist, bl, po);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    walk(c, seen, hist, bl, po);
                }
            }
            Stmt::Break | Stmt::Continue => {}
        }
    }
}

fn op(o: &Op, seen: &mut [bool], hist: &mut Vec<(u32, u32)>, bl: &mut u64, po: &mut u64) {
    match o {
        Op::Un { dst, a, .. } | Op::Mov { dst, a } | Op::NaCheck { dst, a } => {
            mark(seen, *dst);
            mark(seen, *a);
        }
        Op::Bin { dst, a, b, .. } | Op::Cmp { dst, a, b, .. } | Op::Nz { dst, a, b } => {
            mark(seen, *dst);
            mark(seen, *a);
            mark(seen, *b);
        }
        Op::Sel { dst, a, cond } => {
            mark(seen, *dst);
            mark(seen, *a);
            mark(seen, *cond);
        }
        Op::Load { dst, c } => {
            mark(seen, *dst);
            if let Arg::Slot(s) = c {
                mark(seen, *s);
            }
        }
        Op::NaSet { a } => mark(seen, *a),
        Op::Hist { dst, src, k } => {
            mark(seen, *dst);
            mark(seen, *src);
            match k {
                K::Const(0) => {}
                K::Const(n) => hist.push((src.0, *n)),
                K::Dyn(v) => {
                    mark(seen, *v);
                    hist.push((src.0, u32::MAX));
                }
            }
        }
        Op::Call { ov, args, outs, .. } => {
            let ov = pine_spec::overload(*ov);
            if ov.is_stateful() {
                *bl += 1;
            } else if ov.is_collection() {
                *po += 1;
            }
            for a in args {
                if let Arg::Slot(v) = a {
                    mark(seen, *v);
                }
            }
            for &v in outs {
                mark(seen, v);
            }
        }
        Op::ObjNew { dst, .. } => {
            mark(seen, *dst);
            *po += 1;
        }
        Op::ObjGet { dst, h, .. } => {
            mark(seen, *dst);
            mark(seen, *h);
        }
        Op::ObjSet { h, src, .. } => {
            mark(seen, *h);
            mark(seen, *src);
        }
        Op::Persist { dst, src } => {
            mark(seen, *dst);
            mark(seen, *src);
        }
        Op::Budget { .. } => {}
    }
}
