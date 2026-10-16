// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 5c (impl-02 §4.9): references. Every creation site (`ObjNew`, a collection creator
//! `Call`) is classified **var-born** — its op lies in the initialiser (the `Guard` region) of a
//! `var` declaration of the same Instantiation — or **fresh**. A flow-insensitive,
//! field-sensitive points-to fixpoint over sets of sites gives `prov(v)` per reference slot,
//! `fld(s, f)` per reference field of an Object site and `elems(s)` per Collection site. A store
//! into persistent storage (a `var` variable; a field of a var-born object; an element of a
//! var-born collection) whose value's provenance holds a fresh site is `3047`.
//!
//! The pass then lowers every `string` field store `h.f := v` (lowering leaves it as an `ObjSet`
//! whose source is not a staging slot) per CAR-03-1 (a), §4.14.7: one Object site in `prov(h)` —
//! `S_PERSIST t, v` into that site's staging slot, then `O_SET_I h, f, t`; several — the site
//! dispatch `u ← h % 65536`, `u == s ? arm : …`, the last candidate the fall-through arm.
//! `pin:02-refs-typed-flow` — a call's reference result takes the provenance of its reference
//! arguments **of its own type** (and the `elems` of its collections of that element type): a
//! slot of type `T` can only ever hold a block some `T` site created, so the filter removes only
//! impossible provenance.

use std::collections::{BTreeSet, HashMap, HashSet};

use pine_ir::{Diag, Qual, Span, Ty, TypeQ};

use crate::code::{diag, ReasonCode};
use crate::ir::{Arg, BinOpK, Block, CmpK, CondKind, Ir, Op, SlotInfo, SlotKind, Stmt, VSlot};
use crate::sites::Site;

/// A set of sites (`SiteId`s).
type Set = BTreeSet<u32>;

/// Where a contribution lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Target {
    /// `prov(v)`.
    Slot(VSlot),
    /// `fld(s, f)`.
    Field(u32, u16),
    /// `elems(s)`.
    Elems(u32),
}

/// One op as the fixpoint reads it, with the site it creates (if any).
struct Item<'a> {
    op: &'a Op,
    site: Option<u32>,
}

/// Whether `t` is a reference type (a block in the pool).
pub(crate) fn is_ref(t: Ty) -> bool {
    matches!(t, Ty::Udt | Ty::Array | Ty::Map | Ty::Matrix)
}

fn is_coll(t: Ty) -> bool {
    matches!(t, Ty::Array | Ty::Map | Ty::Matrix)
}

/// The element (map: value) type of a collection as a UDT declaration, if it is one.
fn elem_udt(t: TypeQ) -> Option<u16> {
    let e = match t.ty {
        Ty::Array | Ty::Matrix => t.a,
        Ty::Map => t.b,
        _ => return None,
    };
    let scalar_or_enum = e & (pine_ir::elem::SCALAR | pine_ir::elem::ENUM) != 0;
    (!scalar_or_enum).then_some(e & pine_ir::elem::DECL_MASK)
}

struct Facts<'a> {
    ir: &'a Ir,
    /// Ops in walk (`SiteId`) order.
    items: Vec<Item<'a>>,
    /// Per site: var-born.
    var_born: Vec<bool>,
    /// Per site: its span.
    span: Vec<Span>,
    /// `var` / `varip` variable slots.
    vars: HashSet<VSlot>,
    /// `(Object site, field)` → its staging slot.
    stage: HashMap<(u32, u16), VSlot>,
}

/// Run pass 5c on `ir`: refuse `3047`, then lower the `string` field stores.
pub(crate) fn run(ir: &mut Ir, sites: &[Site]) -> Result<(), Diag> {
    let rewrites = {
        let mut f = Facts {
            ir,
            items: Vec::new(),
            var_born: Vec::new(),
            span: Vec::new(),
            vars: ir.var_decls.iter().map(|d| d.0).collect(),
            stage: HashMap::new(),
        };
        let guards: HashMap<VSlot, u32> = ir // INDEX-ONLY
            .var_decls
            .iter()
            .map(|d| (d.1, ir.slots[d.1 .0 as usize].inst))
            .collect();
        let mut stack = Vec::new();
        f.walk(&ir.body, &guards, &mut stack);
        if f.var_born.len() != sites.len() {
            let msg = format!(
                "references counted {} creation sites, numbering {}",
                f.var_born.len(),
                sites.len()
            );
            return Err(diag(ReasonCode::Internal, Span::default(), msg));
        }
        let prov = f.fixpoint();
        f.check(&prov)?;
        f.rewrites(&prov)
    };
    if rewrites.is_empty() {
        return Ok(());
    }
    let mut body = std::mem::take(&mut ir.body);
    let mut k = 0usize;
    rewrite_block(ir, &mut body, &rewrites, &mut k);
    ir.body = body;
    Ok(())
}

impl<'a> Facts<'a> {
    fn info(&self, v: VSlot) -> &SlotInfo {
        &self.ir.slots[v.0 as usize]
    }

    fn ty(&self, v: VSlot) -> TypeQ {
        self.info(v).ty
    }

    /// Pre-order, exactly `sites.rs`'s order: `then` before `els`, `head` before `body` before
    /// `step`, copies in order. `stack` holds the Instantiations of the enclosing `var` guards.
    fn walk(&mut self, b: &'a Block, guards: &HashMap<VSlot, u32>, stack: &mut Vec<u32>) {
        for s in &b.0 {
            match s {
                Stmt::Op(o) => {
                    let creates = match o {
                        Op::ObjNew { dst, span, .. } => Some((*dst, *span)),
                        Op::Call { ov, outs, span, .. } => {
                            let ov = pine_spec::overload(*ov);
                            if ov.is_stateful() || ov.is_collection() {
                                // A stateful built-in is a site without a block of references.
                                let out = if ov.is_collection() {
                                    outs.first().copied()
                                } else {
                                    None
                                };
                                Some((out.unwrap_or(VSlot(u32::MAX)), *span))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    let site = creates.map(|(dst, span)| {
                        let id = self.var_born.len() as u32;
                        let inst = (dst.0 != u32::MAX).then(|| self.info(dst).inst);
                        self.var_born.push(inst.is_some_and(|i| stack.contains(&i)));
                        self.span.push(span);
                        id
                    });
                    self.items.push(Item { op: o, site });
                }
                Stmt::If {
                    cond,
                    then,
                    els,
                    kind,
                    ..
                } => {
                    self.walk(then, guards, stack);
                    let g = (*kind == CondKind::Guard)
                        .then(|| guards.get(cond).copied())
                        .flatten();
                    if let Some(i) = g {
                        stack.push(i);
                    }
                    self.walk(els, guards, stack);
                    if g.is_some() {
                        stack.pop();
                    }
                }
                Stmt::Loop {
                    head, body, step, ..
                } => {
                    self.walk(head, guards, stack);
                    self.walk(body, guards, stack);
                    self.walk(step, guards, stack);
                }
                Stmt::Unrolled { copies } => {
                    for c in copies {
                        self.walk(c, guards, stack);
                    }
                }
                Stmt::Break | Stmt::Continue => {}
            }
        }
    }

    fn get(prov: &HashMap<Target, Set>, t: Target) -> Option<&Set> {
        prov.get(&t)
    }

    /// The contributions of one op under the current state (§4.9's table).
    fn contrib(&self, it: &Item<'_>, prov: &HashMap<Target, Set>, out: &mut Vec<(Target, Set)>) {
        let slot = |v: VSlot| {
            Self::get(prov, Target::Slot(v))
                .cloned()
                .unwrap_or_default()
        };
        match it.op {
            Op::ObjNew { dst, .. } => {
                if let Some(s) = it.site {
                    out.push((Target::Slot(*dst), Set::from([s])));
                }
            }
            Op::Mov { dst, a } | Op::Sel { dst, a, .. } if is_ref(self.ty(*dst).ty) => {
                out.push((Target::Slot(*dst), slot(*a)));
            }
            Op::Load {
                dst,
                c: Arg::Slot(a),
            } if is_ref(self.ty(*dst).ty) => out.push((Target::Slot(*dst), slot(*a))),
            Op::ObjGet { dst, h, field } if is_ref(self.ty(*dst).ty) => {
                let mut acc = Set::new();
                for &s in &slot(*h) {
                    if let Some(x) = Self::get(prov, Target::Field(s, *field)) {
                        acc.extend(x);
                    }
                }
                out.push((Target::Slot(*dst), acc));
            }
            Op::ObjSet { h, field, src } if is_ref(self.ty(*src).ty) => {
                let v = slot(*src);
                for &s in &slot(*h) {
                    out.push((Target::Field(s, *field), v.clone()));
                }
            }
            Op::Call { ov, args, outs, .. } => self.call(it, *ov, args, outs, prov, out),
            _ => {}
        }
    }

    fn call(
        &self,
        it: &Item<'_>,
        ov: pine_ir::OverloadId,
        args: &[Arg],
        outs: &[VSlot],
        prov: &HashMap<Target, Set>,
        out: &mut Vec<(Target, Set)>,
    ) {
        let slot = |v: VSlot| {
            Self::get(prov, Target::Slot(v))
                .cloned()
                .unwrap_or_default()
        };
        let elems_of = |c: VSlot| {
            let mut acc = Set::new();
            for &s in &slot(c) {
                if let Some(x) = Self::get(prov, Target::Elems(s)) {
                    acc.extend(x);
                }
            }
            acc
        };
        let refs: Vec<VSlot> = args
            .iter()
            .filter_map(|a| match a {
                Arg::Slot(v) if is_ref(self.ty(*v).ty) => Some(*v),
                _ => None,
            })
            .collect();
        let o = pine_spec::overload(ov);
        if o.is_collection() {
            let Some(s) = it.site else { return };
            if let Some(&c) = outs.first() {
                out.push((Target::Slot(c), Set::from([s])));
                let want = elem_udt(self.ty(c));
                for &a in &refs {
                    let at = self.ty(a);
                    if at.ty == Ty::Udt && Some(at.a) == want {
                        out.push((Target::Elems(s), slot(a)));
                    } else if is_coll(at.ty) && elem_udt(at) == want && want.is_some() {
                        out.push((Target::Elems(s), elems_of(a)));
                    }
                }
            }
            return;
        }
        if o.sx & pine_spec::sx::MUTATES_ARG0 != 0 {
            if let Some(Arg::Slot(c)) = args.first() {
                let want = elem_udt(self.ty(*c));
                for &a in refs.iter().filter(|&&a| a != *c) {
                    let at = self.ty(a);
                    if want.is_some() && at.ty == Ty::Udt && Some(at.a) == want {
                        for &s in &slot(*c) {
                            out.push((Target::Elems(s), slot(a)));
                        }
                    }
                }
            }
        }
        for &v in outs {
            let vt = self.ty(v);
            if !is_ref(vt.ty) {
                continue;
            }
            let mut acc = Set::new();
            for &a in &refs {
                let at = self.ty(a);
                if at.ty == vt.ty && at.a == vt.a && at.b == vt.b {
                    acc.extend(slot(a));
                }
                if vt.ty == Ty::Udt && is_coll(at.ty) && elem_udt(at) == Some(vt.a) {
                    acc.extend(elems_of(a));
                }
            }
            out.push((Target::Slot(v), acc));
        }
    }

    fn fixpoint(&self) -> HashMap<Target, Set> {
        let mut prov: HashMap<Target, Set> = HashMap::new(); // INDEX-ONLY
        let mut buf = Vec::new();
        loop {
            let mut changed = false;
            for it in &self.items {
                buf.clear();
                self.contrib(it, &prov, &mut buf);
                for (t, s) in buf.drain(..) {
                    let e = prov.entry(t).or_default();
                    let before = e.len();
                    e.extend(s);
                    changed |= e.len() != before;
                }
            }
            if !changed {
                return prov;
            }
        }
    }

    /// `3047`: a store into persistent storage whose value holds a fresh site.
    fn check(&self, prov: &HashMap<Target, Set>) -> Result<(), Diag> {
        let mut buf = Vec::new();
        for it in &self.items {
            if let Op::ObjGet { h, .. } | Op::ObjSet { h, .. } = it.op {
                if prov.get(&Target::Slot(*h)).is_none_or(BTreeSet::is_empty) {
                    // pin:02-field-of-no-object — no creation site reaches the handle: it is
                    // `na` whenever the access runs (a run-time `ScriptRefused`), and verifier
                    // rule 15 refuses a field op no `O_NEW` precedes. impl-02 names no code.
                    let msg = "not implemented: a field access no object creation reaches \
                               (the handle is always `na`; pin:02-field-of-no-object)"
                        .to_owned();
                    return Err(diag(ReasonCode::Internal, self.info(*h).span, msg));
                }
            }
            buf.clear();
            self.contrib(it, prov, &mut buf);
            for (t, set) in &buf {
                let (persistent, at) = match *t {
                    Target::Slot(v) => (self.vars.contains(&v), self.info(v).span),
                    Target::Field(s, _) | Target::Elems(s) => {
                        let at = match it.op {
                            Op::ObjSet { h, .. } => self.info(*h).span,
                            Op::Call { span, .. } => *span,
                            _ => self.span[s as usize],
                        };
                        (self.var_born[s as usize], at)
                    }
                };
                if !persistent {
                    continue;
                }
                if let Some(&f) = set.iter().find(|&&s| !self.var_born[s as usize]) {
                    let what = match t {
                        Target::Slot(_) => "a `var` variable",
                        Target::Field(..) => "a field of an object created by a `var` initialiser",
                        Target::Elems(_) => {
                            "an element of a collection created by a `var` initialiser"
                        }
                    };
                    let msg = format!(
                        "a reference created on every execution is stored into {what}: it \
                         would go stale when its creation site runs again"
                    );
                    let mut d = diag(ReasonCode::ReferenceEscape, at, msg);
                    d.notes.push((
                        self.span[f as usize],
                        "created here (not a `var` initialiser of this scope)".to_owned(),
                    ));
                    d.notes.push((at, "stored here".to_owned()));
                    return Err(d);
                }
            }
        }
        Ok(())
    }

    /// Per `string` field store (in walk order): the candidate staging slots, by site.
    fn rewrites(&mut self, prov: &HashMap<Target, Set>) -> Vec<Vec<(u32, VSlot)>> {
        // The staging slot of every `(site, string field)`: the `ObjSet` of a creation.
        let mut handle: HashMap<VSlot, u32> = HashMap::new(); // INDEX-ONLY
        for it in &self.items {
            match it.op {
                Op::ObjNew { dst, .. } => {
                    if let Some(s) = it.site {
                        handle.insert(*dst, s);
                    }
                }
                Op::ObjSet { h, field, src }
                    if matches!(self.info(*src).kind, SlotKind::Stage { .. }) =>
                {
                    if let Some(&s) = handle.get(h) {
                        self.stage.insert((s, *field), *src);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for it in &self.items {
            if let Op::ObjSet { h, field, src } = it.op {
                if self.pending(*src) {
                    let mut c = Vec::new();
                    if let Some(set) = prov.get(&Target::Slot(*h)) {
                        for &s in set {
                            if let Some(&t) = self.stage.get(&(s, *field)) {
                                c.push((s, t));
                            }
                        }
                    }
                    out.push(c);
                }
            }
        }
        out
    }

    fn pending(&self, src: VSlot) -> bool {
        pending(self.ir, src)
    }
}

/// A `string` field store lowering left for this pass: its source is not a staging slot.
fn pending(ir: &Ir, src: VSlot) -> bool {
    let i = &ir.slots[src.0 as usize];
    i.ty.ty == Ty::String && !matches!(i.kind, SlotKind::Stage { .. })
}

fn scalar_info(ir: &Ir, like: VSlot, ty: Ty) -> SlotInfo {
    let s = ir.slots[like.0 as usize];
    SlotInfo {
        ty: TypeQ::scalar(ty, Qual::Series),
        lane: pine_ir::Lane::I64,
        kind: SlotKind::Temp,
        name: pine_ir::Sym::NONE,
        cell: false,
        ..s
    }
}

fn rewrite_block(ir: &mut Ir, b: &mut Block, rw: &[Vec<(u32, VSlot)>], k: &mut usize) {
    let mut out = Vec::with_capacity(b.0.len());
    for mut s in std::mem::take(&mut b.0) {
        match &mut s {
            Stmt::Op(Op::ObjSet { h, field, src }) if pending(ir, *src) => {
                let cands = rw.get(*k).cloned().unwrap_or_default();
                *k += 1;
                let (h, field, src) = (*h, *field, *src);
                out.extend(dispatch(ir, h, field, src, &cands));
                continue;
            }
            Stmt::Op(_) | Stmt::Break | Stmt::Continue => {}
            Stmt::If { then, els, .. } => {
                rewrite_block(ir, then, rw, k);
                rewrite_block(ir, els, rw, k);
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                rewrite_block(ir, head, rw, k);
                rewrite_block(ir, body, rw, k);
                rewrite_block(ir, step, rw, k);
            }
            Stmt::Unrolled { copies } => {
                for c in copies.iter_mut() {
                    rewrite_block(ir, c, rw, k);
                }
            }
        }
        out.push(s);
    }
    b.0 = out;
}

/// The store `h.f := v` over its candidate sites (§4.14.7).
fn dispatch(ir: &mut Ir, h: VSlot, f: u16, v: VSlot, cands: &[(u32, VSlot)]) -> Vec<Stmt> {
    let arm = |t: VSlot| {
        vec![
            Stmt::Op(Op::Persist { dst: t, src: v }),
            Stmt::Op(Op::ObjSet {
                h,
                field: f,
                src: t,
            }),
        ]
    };
    match cands {
        // `check` refused an empty candidate set (pin:02-field-of-no-object).
        [] => Vec::new(),
        [(_, t)] => arm(*t),
        _ => {
            let u = ir.slot(scalar_info(ir, h, Ty::Int));
            let ci = SlotInfo {
                kind: SlotKind::Const,
                ty: TypeQ::scalar(Ty::Int, Qual::Const),
                ..scalar_info(ir, h, Ty::Int)
            };
            let m = ir.konst(ci, 65_536, true);
            let mut out = vec![Stmt::Op(Op::Bin {
                op: BinOpK::Mod {
                    na_on_nonfinite: false,
                },
                dst: u,
                a: h,
                b: m,
            })];
            // Build from the last (the fall-through arm) outwards.
            let mut tail = Block(arm(cands[cands.len() - 1].1));
            for &(s, t) in cands[..cands.len() - 1].iter().rev() {
                let c = ir.konst(ci, u64::from(s), true);
                let e = ir.slot(scalar_info(ir, h, Ty::Bool));
                let blk = vec![
                    Stmt::Op(Op::Cmp {
                        op: CmpK::Eq,
                        dst: e,
                        a: u,
                        b: c,
                    }),
                    Stmt::If {
                        cond: e,
                        then: Block(arm(t)),
                        els: tail,
                        kind: CondKind::If,
                        na_else: true,
                    },
                ];
                tail = Block(blk);
            }
            out.extend(tail.0);
            out
        }
    }
}
