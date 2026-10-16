// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 5e (impl-02 §4.10, §4.18): rings, the effective cap, `max_bars_back` and the warm-up.
//! A slot gets a ring iff some `Hist` reads it; its demand is the largest offset read (a constant
//! `k`, or the cap `C` for a dynamic one; demands do not propagate). Capacity
//! `max(2, (1 + demand).next_power_of_two())`.

use pine_ir::{Diag, Span};

use crate::code::{diag, ReasonCode};
use crate::hoist::reads;
use crate::ir::{Block, Ir, Op, Stmt, VSlot, K};
use crate::sites::Site;

/// What the pass computes.
#[derive(Debug, Clone, Default)]
pub(crate) struct Series {
    /// `(slot, capacity)` per ringed slot, in first-read order.
    pub(crate) rings: Vec<(VSlot, u32)>,
    /// `ProgramMeta::max_bars_back` (§4.10.4).
    pub(crate) max_bars_back: u32,
    /// `ProgramMeta::warmup_bars` (§4.18).
    pub(crate) warmup: u32,
}

/// Run the pass. `cap` is the effective cap `C` (§4.10.4; `pin:02-site-cap-undeclared`).
pub(crate) fn run(ir: &Ir, sites: &[Site], cap: u32) -> Result<Series, Diag> {
    let mut cx = Cx {
        cap,
        spans: ir.slots.iter().map(|s| s.span).collect(),
        demand: Vec::new(),
        mbb: 0,
        err: None,
    };
    cx.block(&ir.body);
    if let Some(d) = cx.err {
        return Err(d);
    }
    let mut mbb = cx.mbb;
    for s in sites {
        mbb = mbb.max(s.len);
    }
    let rings = cx
        .demand
        .iter()
        .map(|&(v, k)| (v, (1 + k).next_power_of_two().max(2)))
        .collect();
    let mut w = Warm {
        w: vec![0; ir.slots.len()],
        sites,
        next: 0,
        guards: Vec::new(),
        out: 0,
    };
    w.block(&ir.body);
    Ok(Series {
        rings,
        max_bars_back: mbb,
        warmup: w.out,
    })
}

struct Cx {
    cap: u32,
    /// Each slot's span: a refusal names the series read.
    spans: Vec<Span>,
    demand: Vec<(VSlot, u32)>,
    mbb: u32,
    err: Option<Diag>,
}

impl Cx {
    fn want(&mut self, v: VSlot, k: u32) {
        match self.demand.iter_mut().find(|d| d.0 == v) {
            Some(d) => d.1 = d.1.max(k),
            None => self.demand.push((v, k)),
        }
        self.mbb = self.mbb.max(k);
    }

    fn block(&mut self, b: &Block) {
        for s in &b.0 {
            match s {
                Stmt::Op(Op::Hist { src, k, .. }) => match *k {
                    K::Const(0) => {}
                    K::Const(k) => {
                        if self.cap > 0 && k > self.cap && self.err.is_none() {
                            let msg = format!("history offset {k} exceeds the cap {}", self.cap);
                            let at = self.spans[src.0 as usize];
                            self.err = Some(diag(ReasonCode::LookbackExceedsCap, at, msg));
                        }
                        self.want(*src, k);
                    }
                    K::Dyn(_) => {
                        if self.cap == 0 && self.err.is_none() {
                            let msg = "a dynamic history offset with no max_bars_back".to_owned();
                            let at = self.spans[src.0 as usize];
                            self.err = Some(diag(ReasonCode::UnboundedLookback, at, msg));
                        }
                        let c = self.cap;
                        self.want(*src, c);
                    }
                },
                Stmt::Op(_) | Stmt::Break | Stmt::Continue => {}
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
            }
        }
    }
}

/// §4.18 as one forward pass in code order: a slot's warm-up is the largest of its inputs' plus
/// what the op adds (a stateful site its `warm`, `x[k]` its constant `k`); a value read before its
/// write in code order (a `var` carry, self-history) contributes what it held then, so each cycle
/// edge counts once. `pin:02-warmup-guard`: an output call (capture, intent, request) also
/// depends on the conditions of the regions enclosing it — whether it runs is part of its value.
/// `pin:02-warmup-dynamic-k`: a dynamic `x[k]` adds 0 (§4.18 names no `k`).
struct Warm<'a> {
    w: Vec<u32>,
    sites: &'a [Site],
    next: usize,
    guards: Vec<u32>,
    out: u32,
}

impl Warm<'_> {
    fn of(&self, v: VSlot) -> u32 {
        self.w[v.0 as usize]
    }

    fn block(&mut self, b: &Block) {
        for s in &b.0 {
            match s {
                Stmt::Op(o) => self.op(o),
                Stmt::If {
                    cond, then, els, ..
                } => {
                    self.guards.push(self.of(*cond));
                    self.block(then);
                    self.block(els);
                    self.guards.pop();
                }
                Stmt::Loop {
                    head,
                    cond,
                    body,
                    step,
                    ..
                } => {
                    self.block(head);
                    self.guards.push(self.of(*cond));
                    self.block(body);
                    self.block(step);
                    self.guards.pop();
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
        let base = reads(o).iter().map(|&v| self.of(v)).max().unwrap_or(0);
        let mut add = 0u32;
        match o {
            Op::Call { ov, outs, .. } => {
                let ov = pine_spec::overload(*ov);
                if ov.is_stateful() || ov.is_collection() {
                    add = self.sites.get(self.next).map_or(0, |s| s.warm);
                    self.next += 1;
                }
                let output =
                    ov.is_capture() || ov.pushes_intents() || ov.sx & pine_spec::sx::REQUEST != 0;
                if output {
                    let g = self.guards.iter().copied().max().unwrap_or(0);
                    self.out = self.out.max(base.max(g));
                }
                let x = base.saturating_add(add);
                for &v in outs {
                    let i = v.0 as usize;
                    self.w[i] = self.w[i].max(x);
                }
                return;
            }
            Op::ObjNew { .. } => self.next += 1,
            Op::Hist { k: K::Const(k), .. } => add = *k,
            _ => {}
        }
        let x = base.saturating_add(add);
        for v in crate::hoist::writes(o) {
            let i = v.0 as usize;
            self.w[i] = self.w[i].max(x);
        }
    }
}
