// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Leak-freedom (impl-02 §4.11.7, T23): with `lookahead_on` under `live_bound`, a request is
//! admitted only when no output depends on the child's **current** bar. A monotone fixpoint over
//! the child's slots: the reserved bar slots, a call with `sx::IMPLICIT_BAR` or `bf::NEEDS_CTX`
//! and a `HIST_*` with a dynamic `k` are current sources; a `HIST_*` with a constant `k ≥ 1` is
//! past whatever its source; every other op — a stateful call included — is current iff an input
//! is or it is control-dependent on a current condition; a slot is current iff any write to it
//! is (flow-insensitive). Leak-free ⟺ no result is current; otherwise `3043`.

use std::collections::HashSet;

use pine_ir::{Diag, Span};

use crate::code::{diag, ReasonCode};
use crate::ir::{Block, Ir, Op, SlotKind, Stmt, VSlot, K};

/// `pine_vm::bf::NEEDS_CTX` (contract §3.7).
const BF_NEEDS_CTX: u8 = 1 << 2;

/// `3043` when any of `rs` depends on the child's current bar.
pub(crate) fn check(ir: &Ir, rs: &[VSlot], at: Span) -> Result<(), Diag> {
    let mut cur: HashSet<VSlot> = HashSet::new(); // INDEX-ONLY
    for (i, s) in ir.slots.iter().enumerate() {
        if matches!(s.kind, SlotKind::Reserved { .. }) {
            cur.insert(VSlot(i as u32));
        }
    }
    loop {
        let before = cur.len();
        walk(ir, &ir.body, false, &mut cur);
        if cur.len() == before {
            break;
        }
    }
    if rs.iter().any(|r| cur.contains(r)) {
        let msg = "request.security with lookahead_on reads the requested bar's own values: \
                   offset the expression by at least one bar (e.g. `close[1]`)"
            .to_owned();
        return Err(diag(ReasonCode::LookaheadLeak, at, msg));
    }
    Ok(())
}

fn walk(ir: &Ir, b: &Block, ctl: bool, cur: &mut HashSet<VSlot>) {
    for s in &b.0 {
        match s {
            Stmt::Op(op) => {
                if ctl || current(ir, op, cur) {
                    let (mut r, mut w) = (Vec::new(), Vec::new());
                    super::cone::rw(ir, op, &mut r, &mut w);
                    cur.extend(w);
                }
            }
            Stmt::If {
                cond, then, els, ..
            } => {
                let c = ctl || cur.contains(cond);
                walk(ir, then, c, cur);
                walk(ir, els, c, cur);
            }
            Stmt::Loop {
                ctr,
                head,
                cond,
                body,
                step,
                ..
            } => {
                walk(ir, head, ctl, cur);
                let c = ctl || cur.contains(cond);
                if c {
                    if let Some(k) = ctr {
                        cur.insert(k.slot);
                    }
                }
                walk(ir, body, c, cur);
                walk(ir, step, c, cur);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    walk(ir, c, ctl, cur);
                }
            }
            Stmt::Break | Stmt::Continue => {}
        }
    }
}

/// Whether `op` produces a current value from its inputs (control aside).
fn current(ir: &Ir, op: &Op, cur: &HashSet<VSlot>) -> bool {
    match op {
        Op::Hist { src, k, .. } => match k {
            K::Dyn(_) => true,
            K::Const(0) => cur.contains(src),
            K::Const(_) => false,
        },
        Op::Call { ov, .. } => {
            let o = pine_spec::overload(*ov);
            if o.sx & pine_spec::sx::IMPLICIT_BAR != 0 || o.bf & BF_NEEDS_CTX != 0 {
                return true;
            }
            reads_current(ir, op, cur)
        }
        _ => reads_current(ir, op, cur),
    }
}

fn reads_current(ir: &Ir, op: &Op, cur: &HashSet<VSlot>) -> bool {
    let (mut r, mut w) = (Vec::new(), Vec::new());
    super::cone::rw(ir, op, &mut r, &mut w);
    r.iter().any(|v| cur.contains(v))
}
