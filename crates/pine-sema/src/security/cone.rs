// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 5a (impl-02 §4.11.4–§4.11.9, T22): each `request.security` of a program becomes a child
//! program. The child's IR is the backward slice of the request's results over the parent's final
//! IR together with its detached block `E` — flow-insensitive (every writer of a read slot),
//! with the enclosing regions' conditions (a loop's head and step whole), stopping at the
//! reserved bar slots and constants; a nested request stays a `Call` with its own detached block.
//! Refusals while slicing: `3030` (the slice reaches the request's own element call), `3028`
//! (`E` captures, emits an intent or assigns a parent slot), `3029` (a `varip` or `barstate.*`
//! read, a `NEEDS_CTX` call other than `request.security`, a slot written inside a loop that
//! encloses the request). The child — slots renumbered in first-use order, strings re-interned
//! into its own `SymTable` (§4.11.9), `E` at the top level right after the statement holding the
//! request, then per element the `F_I2F` and the `plot` capture — runs the whole back half under
//! its own options (§4.11.8). Keys from the child's digest (§4.11.5) are patched into the
//! parent's `__key` constants; equal keys share one entry, ordered by first reference in code
//! order; depth and the tree-wide count are `3031` (§4.11.6). Leak-freedom (`3043`) is
//! `leak.rs`'s.

use std::collections::{HashMap, HashSet};

use pine_ir::{
    symbol_hash, ChartInfo, CompileOpts, Diag, ExecMode, Lane, SubProgram, Sym, SymTable, Ty,
};

use crate::code::{diag, ReasonCode};
use crate::ir::{Arg, Block, Ir, Op, Request, SlotKind, Stmt, VSlot, K};
use crate::limits::{MAX_SECURITY_DEPTH, MAX_SUBPROGRAMS};

/// `pine_vm::bf` bits (contract §3.7) as `Overload::bf` carries them.
const BF_NEEDS_CTX: u8 = 1 << 2;
const BF_CAPTURE: u8 = 1 << 3;
const BF_INTENT: u8 = 1 << 6;

/// The pseudo-slot every object / collection mutation writes and every object read reads: the
/// heap is one location to the slice (over-inclusion is safe, §4.11.4 rule 1).
const HEAP: VSlot = VSlot(u32::MAX - 1);

/// What extraction reads besides the IR.
pub(crate) struct Cx<'a> {
    /// The program's options (a child's own, §4.11.8).
    pub(crate) opts: &'a CompileOpts,
    /// The program's symbol table (string constants are its `Sym`s).
    pub(crate) syms: &'a SymTable,
    /// The effective cap `C` (§4.10.4), the parent's for every child.
    pub(crate) cap: u32,
    /// Each user type's field count.
    pub(crate) udt_fields: &'a [u16],
    /// This program's depth (the root is 0).
    pub(crate) depth: u32,
    /// The dialect's argument-less `strategy()` fold — every child's `decl` (§4.16).
    pub(crate) bare: pine_ir::StrategyDecl,
    /// U13 reads `restart` (CAR-R33-9, §4.7.4) — the root's reading, for every child's sites.
    pub(crate) ta_restart: bool,
}

/// Every key the whole tree has extracted so far (`3031`, §4.11.6).
#[derive(Default)]
pub(crate) struct Tree {
    keys: Vec<u64>,
}

/// Extract every request of `ir` (§4.11.4–§4.11.6): patch the parent's `__key` constants and
/// return the program's `subprograms`, deduplicated by key in first-reference order.
pub(crate) fn extract(ir: &mut Ir, cx: &Cx<'_>, tree: &mut Tree) -> Result<Vec<SubProgram>, Diag> {
    if ir.requests.is_empty() {
        return Ok(Vec::new());
    }
    let flat = Flat::of(ir);
    // The requests whose element calls this program's body holds, in code order.
    let mut order: Vec<usize> = Vec::new();
    for n in &flat.nodes {
        if let Some(r) = n.req {
            if !order.contains(&r) {
                order.push(r);
            }
        }
    }
    let mut subs: Vec<SubProgram> = Vec::new();
    let mut keys: Vec<(usize, u64)> = Vec::with_capacity(order.len());
    for &ri in &order {
        let req = &ir.requests[ri];
        if cx.depth + 1 > MAX_SECURITY_DEPTH {
            let msg = format!(
                "request.security nested {} levels deep; at most {MAX_SECURITY_DEPTH}",
                cx.depth + 1
            );
            return Err(diag(ReasonCode::SecurityDepth, req.span, msg));
        }
        let sub = one(ir, &flat, ri, cx, tree)?;
        keys.push((ri, sub.key));
        if !tree.keys.contains(&sub.key) {
            tree.keys.push(sub.key);
            if tree.keys.len() > MAX_SUBPROGRAMS {
                let msg = format!(
                    "{} distinct request.security sub-programs; at most {MAX_SUBPROGRAMS}",
                    tree.keys.len()
                );
                return Err(diag(ReasonCode::SecurityDepth, req.span, msg));
            }
        }
        if !subs.iter().any(|s| s.key == sub.key) {
            subs.push(sub);
        }
    }
    // Patch every element call's `__key` (window position 0).
    let outs: Vec<(VSlot, u64)> = keys
        .iter()
        .flat_map(|&(ri, k)| ir.requests[ri].outs.iter().map(move |&o| (o, k)))
        .collect();
    patch(&mut ir.body, &outs);
    Ok(subs)
}

fn patch(b: &mut Block, outs: &[(VSlot, u64)]) {
    for s in &mut b.0 {
        match s {
            Stmt::Op(Op::Call {
                ov, args, outs: o, ..
            }) if super::is_security(*ov) => {
                if let Some(&(_, k)) = o.first().and_then(|x| outs.iter().find(|y| y.0 == *x)) {
                    if let Some(a) = args.first_mut() {
                        *a = Arg::Const {
                            lane: Lane::I64,
                            bits: k,
                            valid: true,
                        };
                    }
                }
            }
            Stmt::If { then, els, .. } => {
                patch(then, outs);
                patch(els, outs);
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                patch(head, outs);
                patch(body, outs);
                patch(step, outs);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    patch(c, outs);
                }
            }
            _ => {}
        }
    }
}

// ---- the flattened program ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Plain,
    Head,
    Body,
    Step,
}

enum Nk {
    Op,
    If { cond: VSlot },
    Loop { cond: VSlot, ctr: Option<VSlot> },
    Unrolled,
    Jump,
}

struct FlatNode {
    parent: u32,
    part: Part,
    top: u32,
    kind: Nk,
    reads: Vec<VSlot>,
    writes: Vec<VSlot>,
    /// `Some(bf)` for a `Call`.
    call: Option<(pine_ir::OverloadId, pine_ir::Span)>,
    /// The request whose element call this is.
    req: Option<usize>,
}

/// The parent's body in pre-order (`then` before `els`, `head` before `body` before `step`).
struct Flat {
    nodes: Vec<FlatNode>,
    writers: HashMap<VSlot, Vec<u32>>, // INDEX-ONLY
}

impl Flat {
    fn of(ir: &Ir) -> Flat {
        let mut f = Flat {
            nodes: Vec::new(),
            writers: HashMap::new(),
        };
        f.block(ir, &ir.body, u32::MAX, Part::Plain, None);
        for (i, n) in f.nodes.iter().enumerate() {
            for &w in &n.writes {
                f.writers.entry(w).or_default().push(i as u32);
            }
        }
        f
    }

    fn block(&mut self, ir: &Ir, b: &Block, parent: u32, part: Part, top: Option<u32>) {
        for (k, s) in b.0.iter().enumerate() {
            let me = self.nodes.len() as u32;
            let top = top.unwrap_or(k as u32);
            let mut n = FlatNode {
                parent,
                part,
                top,
                kind: Nk::Op,
                reads: Vec::new(),
                writes: Vec::new(),
                call: None,
                req: None,
            };
            match s {
                Stmt::Op(op) => {
                    rw(ir, op, &mut n.reads, &mut n.writes);
                    if let Op::Call { ov, outs, span, .. } = op {
                        n.call = Some((*ov, *span));
                        if super::is_security(*ov) {
                            n.req = outs
                                .first()
                                .and_then(|o| ir.requests.iter().position(|r| r.outs.contains(o)));
                        }
                    }
                    self.nodes.push(n);
                }
                Stmt::If {
                    cond, then, els, ..
                } => {
                    n.kind = Nk::If { cond: *cond };
                    self.nodes.push(n);
                    self.block(ir, then, me, Part::Plain, Some(top));
                    self.block(ir, els, me, Part::Plain, Some(top));
                }
                Stmt::Loop {
                    ctr,
                    head,
                    cond,
                    body,
                    step,
                    ..
                } => {
                    let c = ctr.map(|c| c.slot);
                    n.kind = Nk::Loop {
                        cond: *cond,
                        ctr: c,
                    };
                    if let Some(c) = c {
                        n.writes.push(c);
                    }
                    self.nodes.push(n);
                    self.block(ir, head, me, Part::Head, Some(top));
                    self.block(ir, body, me, Part::Body, Some(top));
                    self.block(ir, step, me, Part::Step, Some(top));
                }
                Stmt::Unrolled { copies } => {
                    n.kind = Nk::Unrolled;
                    self.nodes.push(n);
                    for c in copies {
                        self.block(ir, c, me, Part::Plain, Some(top));
                    }
                }
                Stmt::Break | Stmt::Continue => {
                    n.kind = Nk::Jump;
                    self.nodes.push(n);
                }
            }
        }
    }

    /// Whether node `i` lies under node `anc`.
    fn under(&self, mut i: u32, anc: u32) -> bool {
        while i != u32::MAX {
            if i == anc {
                return true;
            }
            i = self.nodes[i as usize].parent;
        }
        false
    }
}

fn is_ref(ir: &Ir, v: VSlot) -> bool {
    ir.slots
        .get(v.0 as usize)
        .is_some_and(|s| matches!(s.ty.ty, Ty::Array | Ty::Map | Ty::Matrix | Ty::Udt))
}

/// Every slot `op` reads and writes (§4.11.4 rule 1); an object or collection mutation writes
/// [`HEAP`], an object read reads it.
pub(super) fn rw(ir: &Ir, op: &Op, r: &mut Vec<VSlot>, w: &mut Vec<VSlot>) {
    match op {
        Op::Un { dst, a, .. }
        | Op::Mov { dst, a }
        | Op::NaCheck { dst, a }
        | Op::Persist { dst, src: a } => {
            r.push(*a);
            w.push(*dst);
        }
        Op::Bin { dst, a, b, .. } | Op::Cmp { dst, a, b, .. } | Op::Nz { dst, a, b } => {
            r.push(*a);
            r.push(*b);
            w.push(*dst);
        }
        Op::Load { dst, c } => {
            if let Arg::Slot(v) = c {
                r.push(*v);
            }
            w.push(*dst);
        }
        Op::NaSet { a } => w.push(*a),
        Op::Sel { dst, a, cond } => {
            r.push(*a);
            r.push(*cond);
            r.push(*dst);
            w.push(*dst);
        }
        Op::Hist { dst, src, k } => {
            r.push(*src);
            if let K::Dyn(v) = k {
                r.push(*v);
            }
            w.push(*dst);
        }
        Op::Call { args, outs, .. } => {
            let mut heap = false;
            for a in args {
                if let Arg::Slot(v) = a {
                    r.push(*v);
                    heap |= is_ref(ir, *v);
                }
            }
            if heap {
                r.push(HEAP);
                w.push(HEAP);
            }
            w.extend(outs.iter().copied());
        }
        Op::ObjNew { dst, .. } => {
            w.push(*dst);
            w.push(HEAP);
        }
        Op::ObjGet { dst, h, .. } => {
            r.push(*h);
            r.push(HEAP);
            w.push(*dst);
        }
        Op::ObjSet { h, src, .. } => {
            r.push(*h);
            r.push(*src);
            w.push(HEAP);
        }
        Op::Budget { .. } => {}
    }
}

/// Every read and write of a whole block (nested structures included).
fn block_rw(ir: &Ir, b: &Block, r: &mut Vec<VSlot>, w: &mut Vec<VSlot>, calls: &mut Vec<Op>) {
    for s in &b.0 {
        match s {
            Stmt::Op(op) => {
                rw(ir, op, r, w);
                if matches!(op, Op::Call { .. }) {
                    calls.push(op.clone());
                }
            }
            Stmt::If {
                cond, then, els, ..
            } => {
                r.push(*cond);
                block_rw(ir, then, r, w, calls);
                block_rw(ir, els, r, w, calls);
            }
            Stmt::Loop {
                ctr,
                head,
                cond,
                body,
                step,
                ..
            } => {
                if let Some(c) = ctr {
                    w.push(c.slot);
                }
                r.push(*cond);
                block_rw(ir, head, r, w, calls);
                block_rw(ir, body, r, w, calls);
                block_rw(ir, step, r, w, calls);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    block_rw(ir, c, r, w, calls);
                }
            }
            Stmt::Break | Stmt::Continue => {}
        }
    }
}

fn refuse(code: ReasonCode, at: pine_ir::Span, what: &str) -> Diag {
    diag(code, at, format!("request.security: {what}"))
}

/// One request: slice, checks, the child IR, its compilation and its `SubProgram`.
fn one(ir: &Ir, flat: &Flat, ri: usize, cx: &Cx<'_>, tree: &mut Tree) -> Result<SubProgram, Diag> {
    let req = &ir.requests[ri];
    let at = req.span;
    // §4.11.4 `3028`: `E` captures, emits an intent, or assigns a slot of the parent.
    let (mut er, mut ew, mut ecalls) = (Vec::new(), Vec::new(), Vec::new());
    block_rw(ir, &req.e, &mut er, &mut ew, &mut ecalls);
    for c in &ecalls {
        if let Op::Call { ov, span, .. } = c {
            let o = pine_spec::overload(*ov);
            if o.bf & (BF_CAPTURE | BF_INTENT) != 0 {
                return Err(refuse(
                    ReasonCode::SecuritySideEffect,
                    *span,
                    "the expression plots, alerts or emits an order intent",
                ));
            }
        }
    }
    for &w in &ew {
        if let Some(s) = ir.slots.get(w.0 as usize) {
            if matches!(s.kind, SlotKind::Var | SlotKind::Varip) && s.region < req.region {
                return Err(refuse(
                    ReasonCode::SecuritySideEffect,
                    at,
                    "the expression assigns a variable of the calling script",
                ));
            }
        }
    }
    // The loops that enclose the request call (`3029`): every slot written inside them.
    let site = flat.nodes.iter().position(|n| n.req == Some(ri));
    let mut loop_w: HashSet<VSlot> = HashSet::new(); // INDEX-ONLY
    if let Some(site) = site {
        let mut p = flat.nodes[site].parent;
        while p != u32::MAX {
            if let Nk::Loop { ctr, .. } = flat.nodes[p as usize].kind {
                if let Some(c) = ctr {
                    loop_w.insert(c);
                }
                for (i, n) in flat.nodes.iter().enumerate() {
                    if flat.under(i as u32, p) && i as u32 != p {
                        loop_w.extend(n.writes.iter().copied());
                    }
                }
            }
            p = flat.nodes[p as usize].parent;
        }
    }
    // The target (§4.11.1): a folded `syminfo.*` / `timeframe.*` value or `math.round_to_mintick`
    // in the cone is `3029` when it differs from the chart's.
    let target = target(&req.symbol, cx.opts);
    let tf_s = timeframe(ir, ri, cx.opts);
    let own = target == cx.opts.symbol_key;
    let ctx = Ctx {
        loop_w: &loop_w,
        other_sym: !own,
        other_tf: tf_s != cx.opts.chart.timeframe_s,
    };
    // ---- the slice (§4.11.4) ----------------------------------------------------------------
    let mut inc = vec![false; flat.nodes.len()];
    let mut work: Vec<VSlot> = req.rs.clone();
    work.extend(er.iter().copied());
    let (mut tr, mut tw, mut tcalls) = (Vec::new(), Vec::new(), Vec::new());
    block_rw(ir, &req.tail, &mut tr, &mut tw, &mut tcalls);
    work.extend(tr.iter().copied());
    // Nested requests inside `E`: the child computes what their expressions read.
    let mut nested: Vec<usize> = Vec::new();
    for c in &ecalls {
        check_call(c, &ctx, at)?;
        if let Op::Call { ov, outs, .. } = c {
            if super::is_security(*ov) {
                if let Some(r2) = outs
                    .first()
                    .and_then(|o| ir.requests.iter().position(|r| r.outs.contains(o)))
                {
                    if !nested.contains(&r2) {
                        nested.push(r2);
                        seed_nested(ir, r2, &mut work, &mut nested);
                    }
                }
            }
        }
    }
    let mut seen: HashSet<VSlot> = HashSet::new(); // INDEX-ONLY
    let mut nodes: Vec<u32> = Vec::new();
    loop {
        while let Some(v) = work.pop() {
            if !seen.insert(v) {
                continue;
            }
            check_read(ir, v, &ctx, at)?;
            if let Some(ws) = flat.writers.get(&v) {
                nodes.extend(ws.iter().copied());
            }
        }
        let Some(i) = nodes.pop() else { break };
        if inc[i as usize] {
            continue;
        }
        let n = &flat.nodes[i as usize];
        if n.req == Some(ri) {
            return Err(refuse(
                ReasonCode::SecurityCycle,
                at,
                "the result feeds its own expression",
            ));
        }
        inc[i as usize] = true;
        work.extend(n.reads.iter().copied());
        if let Some((ov, span)) = n.call {
            check_call(
                &Op::Call {
                    ov,
                    args: Vec::new(),
                    outs: Vec::new(),
                    span,
                },
                &ctx,
                at,
            )?;
        }
        if let Some(r2) = n.req {
            if !nested.contains(&r2) {
                nested.push(r2);
                seed_nested(ir, r2, &mut work, &mut nested);
            }
        }
        match n.kind {
            Nk::If { cond } => work.push(cond),
            Nk::Loop { cond, .. } => {
                work.push(cond);
                // A loop's head and step whole, and every jump of its body.
                for (k, m) in flat.nodes.iter().enumerate() {
                    if (m.parent == i && matches!(m.part, Part::Head | Part::Step))
                        || (matches!(m.kind, Nk::Jump) && flat.under(k as u32, i))
                    {
                        nodes.push(k as u32);
                    }
                }
            }
            _ => {}
        }
        if n.parent != u32::MAX {
            nodes.push(n.parent);
        }
        // Everything under an included head / step is included too.
        if matches!(n.part, Part::Head | Part::Step) {
            for (k, m) in flat.nodes.iter().enumerate() {
                if m.parent == i {
                    nodes.push(k as u32);
                }
            }
        }
    }
    // ---- the child (§4.11.4, §4.11.8, §4.11.9) ----------------------------------------------
    let mut m = Map {
        p: ir,
        psyms: cx.syms,
        c: Ir::default(),
        csyms: SymTable::new(),
        m: HashMap::new(),
    };
    let e_top = site.map(|s| flat.nodes[s].top);
    let mut idx = 0u32;
    let mut body = m.rebuild(&ir.body, &inc, &mut idx);
    // `E` at the top level right after the statement that holds the request call
    // (pin:02-cone-e-position: §4.11.4 keeps "the original region structure" and runs `E` on
    // every child bar, whatever the parent's branches do).
    let e = m.all(&req.e);
    let pos = e_top.map_or(body.len(), |t| m.top_pos(&inc, flat, t));
    let tail = m.all(&req.tail);
    let mut out = Vec::with_capacity(body.len() + e.0.len() + tail.0.len());
    let rest = body.split_off(pos.min(body.len()));
    out.extend(body);
    out.extend(e.0);
    out.extend(rest);
    out.extend(tail.0);
    m.c.body = Block(out);
    let rs: Vec<VSlot> = req.rs.iter().map(|&r| m.slot(r)).collect();
    for &r2 in &nested {
        let q = &ir.requests[r2];
        let nq = Request {
            e: m.all(&q.e),
            region: q.region,
            rs: q.rs.iter().map(|&r| m.slot(r)).collect(),
            outs: q.outs.iter().map(|&r| m.slot(r)).collect(),
            tail: m.all(&q.tail),
            symbol: q.symbol.clone(),
            tf_s: q.tf_s,
            lookahead: q.lookahead,
            gaps: q.gaps,
            span: q.span,
        };
        m.c.requests.push(nq);
        m.c.request_tfs
            .push(ir.request_tfs.get(r2).cloned().unwrap_or_default());
    }
    for &(v, f) in &ir.var_decls {
        if let (Some(&a), Some(&b)) = (m.m.get(&v), m.m.get(&f)) {
            m.c.var_decls.push((a, b));
        }
    }
    for &(v, k) in &ir.ctx_slots {
        if let Some(&a) = m.m.get(&v) {
            m.c.ctx_slots.push((a, k));
        }
    }
    for &(v, c) in &ir.var_caps {
        if let Some(&a) = m.m.get(&v) {
            m.c.var_caps.push((a, c));
        }
    }
    for _ in 0..req.rs.len() {
        m.c.plots
            .push((pine_ir::PlotKind::SubOut, pine_ir::Span::default()));
    }
    let Map { c, csyms, .. } = m;
    // §4.11.7: leak-freedom under lookahead and a live bound.
    if req.lookahead && cx.opts.live_bound {
        super::leak::check(&c, &rs, at)?;
    }
    let copts = child_opts(cx.opts, &target, tf_s, req.lookahead, own);
    let child = crate::child(c, csyms, &copts, cx, tree).map_err(|mut d| {
        if d.span == pine_ir::Span::default() {
            d.span = at;
        }
        d
    })?;
    let digest = pine_ir::fnv1a64(&crate::canon::canonical_bytes(&child));
    let key = crate::canon::sub_key(&target, tf_s, req.lookahead, req.gaps, digest);
    let chart_tf = cx.opts.chart.timeframe_s;
    let history = if tf_s >= chart_tf {
        2
    } else {
        chart_tf.div_ceil(tf_s.max(1)) + 1
    };
    Ok(SubProgram {
        key,
        symbol_hash: symbol_hash(target.as_bytes()),
        symbol_key: target,
        timeframe_s: tf_s,
        lookahead: req.lookahead,
        gaps: req.gaps,
        n_out: req.rs.len() as u16,
        history,
        program: Box::new(child),
    })
}

/// A nested request the child carries: what its expression and tail read is the child's to
/// compute, and the requests nested in its own expression ride along (each level extracts its own).
fn seed_nested(ir: &Ir, r2: usize, work: &mut Vec<VSlot>, nested: &mut Vec<usize>) {
    let q = &ir.requests[r2];
    let (mut r, mut w, mut c) = (Vec::new(), Vec::new(), Vec::new());
    block_rw(ir, &q.e, &mut r, &mut w, &mut c);
    block_rw(ir, &q.tail, &mut r, &mut w, &mut c);
    work.extend(q.rs.iter().copied());
    work.extend(r);
    for op in &c {
        if let Op::Call { ov, outs, .. } = op {
            if !super::is_security(*ov) {
                continue;
            }
            let r3 = outs
                .first()
                .and_then(|o| ir.requests.iter().position(|r| r.outs.contains(o)));
            if let Some(r3) = r3 {
                if !nested.contains(&r3) {
                    nested.push(r3);
                    seed_nested(ir, r3, work, nested);
                }
            }
        }
    }
}

/// What `3029` reads besides the slot.
struct Ctx<'a> {
    /// Every slot written inside a loop enclosing the request call.
    loop_w: &'a HashSet<VSlot>,
    /// The target symbol is not the chart's.
    other_sym: bool,
    /// The target timeframe is not the chart's.
    other_tf: bool,
}

/// `3029` for a call in the cone: a `NEEDS_CTX` call other than `request.security`, and
/// `math.round_to_mintick` (its body reads `meta().chart`) for another symbol.
fn check_call(c: &Op, ctx: &Ctx<'_>, at: pine_ir::Span) -> Result<(), Diag> {
    if let Op::Call { ov, span, .. } = c {
        let o = pine_spec::overload(*ov);
        if ctx.other_sym && *ov == pine_spec::ov::MATH_ROUND_TO_MINTICK {
            return Err(refuse(
                ReasonCode::SecurityContextMix,
                at,
                "math.round_to_mintick of another symbol reads the chart's tick",
            ));
        }
        if o.bf & BF_NEEDS_CTX != 0 && !super::is_security(*ov) {
            let s = if *span == pine_ir::Span::default() {
                at
            } else {
                *span
            };
            return Err(refuse(
                ReasonCode::SecurityContextMix,
                s,
                "the expression depends on a strategy or context value",
            ));
        }
    }
    Ok(())
}

/// `3029` for a slot the slice reads.
fn check_read(ir: &Ir, v: VSlot, ctx: &Ctx<'_>, at: pine_ir::Span) -> Result<(), Diag> {
    let Some(s) = ir.slots.get(v.0 as usize) else {
        return Ok(());
    };
    for &(c, kind) in &ir.ctx_slots {
        if c != v {
            continue;
        }
        if ctx.other_sym && kind & crate::ir::CTX_SYMINFO != 0 {
            let w = "the expression folds a `syminfo.*` value of the chart, not the target";
            return Err(refuse(ReasonCode::SecurityContextMix, at, w));
        }
        if ctx.other_tf && kind & crate::ir::CTX_TIMEFRAME != 0 {
            let w = "the expression folds a `timeframe.*` value of the chart, not the target";
            return Err(refuse(ReasonCode::SecurityContextMix, at, w));
        }
    }
    let why = match s.kind {
        SlotKind::Varip => Some("the expression reads a `varip` variable"),
        // A value row's slot carries the i64 lane in bit 15 (`0x8000 | I_BS_*`).
        SlotKind::Reserved { slot }
            if s.lane == Lane::I64
                && (pine_ir::abi::I_BS_ISFIRST..=pine_ir::abi::I_BS_ISLASTCONFHIST)
                    .contains(&(slot & 0x7FFF)) =>
        {
            Some("the expression reads `barstate.*`")
        }
        _ if ctx.loop_w.contains(&v) => {
            Some("the expression reads a variable of a loop enclosing the call")
        }
        _ => None,
    };
    match why {
        Some(w) => Err(refuse(ReasonCode::SecurityContextMix, at, w)),
        None => Ok(()),
    }
}

/// The target canonical key: `""` and the chart's key are the chart's; a `ticker_map` key maps
/// (pass 3 refused every other text).
fn target(text: &str, o: &CompileOpts) -> String {
    if text.is_empty() || text == o.symbol_key {
        return o.symbol_key.clone();
    }
    o.ticker_map
        .iter()
        .find(|m| m.0 == text)
        .map_or_else(|| o.symbol_key.clone(), |m| m.1.clone())
}

/// The target timeframe: `""` is the chart's; otherwise `tf_seconds` (pass 3 checked it).
fn timeframe(ir: &Ir, ri: usize, o: &CompileOpts) -> u32 {
    let text = ir.request_tfs.get(ri).map_or("", String::as_str);
    match pine_std::tf_seconds(text.as_bytes()) {
        Ok(Some(s)) => s,
        _ => o.chart.timeframe_s,
    }
}

/// The child's options (§4.11.8).
fn child_opts(p: &CompileOpts, target: &str, tf_s: u32, lookahead: bool, own: bool) -> CompileOpts {
    let base = if own { p.chart } else { ChartInfo::default() };
    CompileOpts {
        dialect: p.dialect,
        mode: if p.mode == ExecMode::Realtime || lookahead {
            ExecMode::Realtime
        } else {
            ExecMode::BarClose
        },
        live_bound: p.live_bound,
        chart: ChartInfo {
            symbol_hash: symbol_hash(target.as_bytes()),
            timeframe_s: tf_s,
            _pad: [0; 3],
            ..base
        },
        symbol_key: target.to_owned(),
        syminfo_strs: if own {
            p.syminfo_strs.clone()
        } else {
            Default::default()
        },
        limits: p.limits,
        inputs: Vec::new(),
        ticker_map: p.ticker_map.clone(),
        source_sha256: [0; 32],
    }
}

// ---- the child's IR -----------------------------------------------------------------------

/// The parent → child slot map: first-use order, constants re-registered (strings re-interned
/// into the child's own table, §4.11.9), prelude rows kept as rows.
struct Map<'a> {
    p: &'a Ir,
    psyms: &'a SymTable,
    c: Ir,
    csyms: SymTable,
    m: HashMap<VSlot, VSlot>, // INDEX-ONLY
}

impl Map<'_> {
    fn slot(&mut self, v: VSlot) -> VSlot {
        if v.0 >= HEAP.0 {
            return v;
        }
        if let Some(&x) = self.m.get(&v) {
            return x;
        }
        let Some(&info) = self.p.slots.get(v.0 as usize) else {
            return v;
        };
        let info = pine_ir_info(info);
        let x = match info.kind {
            SlotKind::Const => {
                let (bits, valid) = self.p.const_of(v).unwrap_or((0, false));
                let bits = if info.ty.ty == Ty::String && valid {
                    let t = self.psyms.resolve(Sym(bits as u32));
                    u64::from(self.csyms.intern(t).0)
                } else {
                    bits
                };
                self.c.konst(info, bits, valid)
            }
            SlotKind::Prelude { value } => self.c.value(value, info),
            _ => self.c.slot(info),
        };
        self.m.insert(v, x);
        x
    }

    fn arg(&mut self, a: &Arg) -> Arg {
        match *a {
            Arg::Slot(v) => Arg::Slot(self.slot(v)),
            other => other,
        }
    }

    fn op(&mut self, op: &Op) -> Op {
        match op {
            Op::Un { op, dst, a } => {
                let a = self.slot(*a);
                Op::Un {
                    op: *op,
                    dst: self.slot(*dst),
                    a,
                }
            }
            Op::Bin { op, dst, a, b } => {
                let (a, b) = (self.slot(*a), self.slot(*b));
                Op::Bin {
                    op: *op,
                    dst: self.slot(*dst),
                    a,
                    b,
                }
            }
            Op::Cmp { op, dst, a, b, tol } => {
                let (a, b) = (self.slot(*a), self.slot(*b));
                Op::Cmp {
                    op: *op,
                    dst: self.slot(*dst),
                    a,
                    b,
                    tol: *tol,
                }
            }
            Op::Mov { dst, a } => {
                let a = self.slot(*a);
                Op::Mov {
                    dst: self.slot(*dst),
                    a,
                }
            }
            Op::Load { dst, c } => {
                let c = self.arg(c);
                Op::Load {
                    dst: self.slot(*dst),
                    c,
                }
            }
            Op::NaCheck { dst, a } => {
                let a = self.slot(*a);
                Op::NaCheck {
                    dst: self.slot(*dst),
                    a,
                }
            }
            Op::NaSet { a } => Op::NaSet { a: self.slot(*a) },
            Op::Nz { dst, a, b } => {
                let (a, b) = (self.slot(*a), self.slot(*b));
                Op::Nz {
                    dst: self.slot(*dst),
                    a,
                    b,
                }
            }
            Op::Sel { dst, a, cond } => {
                let (a, cond) = (self.slot(*a), self.slot(*cond));
                Op::Sel {
                    dst: self.slot(*dst),
                    a,
                    cond,
                }
            }
            Op::Hist { dst, src, k } => {
                let src = self.slot(*src);
                let k = match k {
                    K::Dyn(v) => K::Dyn(self.slot(*v)),
                    K::Const(c) => K::Const(*c),
                };
                Op::Hist {
                    dst: self.slot(*dst),
                    src,
                    k,
                }
            }
            Op::Call {
                ov,
                args,
                outs,
                span,
            } => {
                let args = args.iter().map(|a| self.arg(a)).collect();
                let outs = outs.iter().map(|&o| self.slot(o)).collect();
                Op::Call {
                    ov: *ov,
                    args,
                    outs,
                    span: *span,
                }
            }
            Op::ObjNew { dst, udt, span } => Op::ObjNew {
                dst: self.slot(*dst),
                udt: *udt,
                span: *span,
            },
            Op::ObjGet { dst, h, field } => {
                let h = self.slot(*h);
                Op::ObjGet {
                    dst: self.slot(*dst),
                    h,
                    field: *field,
                }
            }
            Op::ObjSet { h, field, src } => {
                let (h, src) = (self.slot(*h), self.slot(*src));
                Op::ObjSet {
                    h,
                    field: *field,
                    src,
                }
            }
            Op::Persist { dst, src } => {
                let src = self.slot(*src);
                Op::Persist {
                    dst: self.slot(*dst),
                    src,
                }
            }
            Op::Budget { units } => Op::Budget { units: *units },
        }
    }

    /// The included part of `b` (pre-order indices from `idx`), regions kept.
    fn rebuild(&mut self, b: &Block, inc: &[bool], idx: &mut u32) -> Vec<Stmt> {
        let mut out = Vec::new();
        for s in &b.0 {
            let i = *idx as usize;
            *idx += 1;
            let on = inc.get(i).copied().unwrap_or(false);
            match s {
                Stmt::Op(op) => {
                    if on {
                        let o = self.op(op);
                        out.push(Stmt::Op(o));
                    }
                }
                Stmt::If {
                    cond,
                    then,
                    els,
                    kind,
                    na_else,
                } => {
                    let c = if on { Some(self.slot(*cond)) } else { None };
                    let t = self.rebuild(then, inc, idx);
                    let e = self.rebuild(els, inc, idx);
                    if let Some(cond) = c {
                        out.push(Stmt::If {
                            cond,
                            then: Block(t),
                            els: Block(e),
                            kind: *kind,
                            na_else: *na_else,
                        });
                    }
                }
                Stmt::Loop {
                    span,
                    ctr,
                    head,
                    cond,
                    body,
                    step,
                    trip,
                    kind,
                } => {
                    let ctr = if on {
                        ctr.map(|c| crate::ir::Ctr {
                            slot: self.slot(c.slot),
                            ..c
                        })
                    } else {
                        None
                    };
                    let h = self.rebuild(head, inc, idx);
                    let c = if on { Some(self.slot(*cond)) } else { None };
                    let bd = self.rebuild(body, inc, idx);
                    let st = self.rebuild(step, inc, idx);
                    if let Some(cond) = c {
                        out.push(Stmt::Loop {
                            span: *span,
                            ctr,
                            head: Block(h),
                            cond,
                            body: Block(bd),
                            step: Block(st),
                            trip: *trip,
                            kind: *kind,
                        });
                    }
                }
                Stmt::Unrolled { copies } => {
                    let mut cs = Vec::with_capacity(copies.len());
                    for c in copies {
                        cs.push(Block(self.rebuild(c, inc, idx)));
                    }
                    if on {
                        out.push(Stmt::Unrolled { copies: cs });
                    }
                }
                Stmt::Break => {
                    if on {
                        out.push(Stmt::Break);
                    }
                }
                Stmt::Continue => {
                    if on {
                        out.push(Stmt::Continue);
                    }
                }
            }
        }
        out
    }

    /// The whole of `b`, mapped.
    fn all(&mut self, b: &Block) -> Block {
        let mut out = Vec::with_capacity(b.0.len());
        for s in &b.0 {
            out.push(match s {
                Stmt::Op(op) => Stmt::Op(self.op(op)),
                Stmt::If {
                    cond,
                    then,
                    els,
                    kind,
                    na_else,
                } => {
                    let cond = self.slot(*cond);
                    Stmt::If {
                        cond,
                        then: self.all(then),
                        els: self.all(els),
                        kind: *kind,
                        na_else: *na_else,
                    }
                }
                Stmt::Loop {
                    span,
                    ctr,
                    head,
                    cond,
                    body,
                    step,
                    trip,
                    kind,
                } => {
                    let ctr = ctr.map(|c| crate::ir::Ctr {
                        slot: self.slot(c.slot),
                        ..c
                    });
                    let head = self.all(head);
                    let cond = self.slot(*cond);
                    Stmt::Loop {
                        span: *span,
                        ctr,
                        head,
                        cond,
                        body: self.all(body),
                        step: self.all(step),
                        trip: *trip,
                        kind: *kind,
                    }
                }
                Stmt::Unrolled { copies } => Stmt::Unrolled {
                    copies: copies.iter().map(|c| self.all(c)).collect(),
                },
                Stmt::Break => Stmt::Break,
                Stmt::Continue => Stmt::Continue,
            });
        }
        Block(out)
    }

    /// The child's top-level position right after parent top-level statement `t`: the count of
    /// included top-level statements up to and including `t`.
    fn top_pos(&self, inc: &[bool], flat: &Flat, t: u32) -> usize {
        let mut n = 0usize;
        for (i, node) in flat.nodes.iter().enumerate() {
            if node.parent == u32::MAX && node.top <= t && inc.get(i).copied().unwrap_or(false) {
                n += 1;
            }
        }
        n
    }
}

/// A child slot's descriptor: the parent's, without the parent's name (its `Sym` is the parent
/// table's, §4.11.9).
fn pine_ir_info(mut info: crate::ir::SlotInfo) -> crate::ir::SlotInfo {
    info.name = Sym::NONE;
    info
}
