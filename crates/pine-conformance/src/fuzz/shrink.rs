// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Greedy multi-pass delta-debugging over the generator's own tree (impl-06 §4.8.5).
//!
//! Passes, each over every candidate site in source order: (1) delete a statement; (2) replace a
//! subexpression by a literal of its type; (3) move numeric literals toward 0, then 1; (4) loop
//! bounds toward one trip; (5) drop a plot other than the divergent one (pass 1 over `plot`
//! statements; the divergent plot's deletion changes the signature and is refused); (7) collapse a
//! conditional to one of its arms. A change is kept iff the predicate reports the **same**
//! `Signature` and the program's [`Measure`] strictly decreases; fixed point when a full round keeps
//! nothing; bounded by `ShrinkBudget::max_steps` predicate calls (steps, never wall time).
//!
//! `pin:06-shrink-passes-v1` — pass (6) inlines user functions, which grammar version 1 does not
//! emit; pass (9) (`bars` toward the first divergent bar + 1) is a property of the predicate, which
//! §3.11's `Predicate::diverges(&Program)` cannot vary: both are no-ops here.
//! `pin:06-shrink-arm-order` — pass (7) tries the `then` arm before the `else` arm (the predicate
//! does not report which arm ran on the first divergent bar).

use pine_spec::rets;

use super::ast::{self, Expr, Program, Sc, Stmt, SynVar};
use super::{Generated, Predicate, Signature};

/// The step budget.
#[derive(Debug, Clone, Copy)]
pub struct ShrinkBudget {
    /// Predicate calls; default 2000.
    pub max_steps: u32,
}

impl Default for ShrinkBudget {
    fn default() -> Self {
        Self { max_steps: 2000 }
    }
}

/// `(body lines, expression nodes + statements, Σ literal magnitudes)` — lexicographic; every
/// kept change strictly decreases it, so shrinking terminates without the budget too.
pub type Measure = (u32, u32, u64);

/// What `shrink` returns.
#[derive(Debug, Clone)]
pub struct ShrinkOutcome {
    /// The smallest program found.
    pub program: Program,
    /// Its rendering.
    pub source: String,
    /// Predicate calls spent (`== max_steps` on exhaustion).
    pub steps: u32,
    /// Body lines before.
    pub lines_before: u32,
    /// Body lines after.
    pub lines_after: u32,
    /// The preserved signature.
    pub signature: Signature,
    /// The measure after each kept change, in order.
    ///
    /// `pin:06-shrink-trace` — §3.11's outcome plus this trace, so monotonicity is observable.
    pub trace: Vec<Measure>,
}

fn expr_children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Bin(_, a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => vec![a, b],
        Expr::Not(a) | Expr::IsNa(a) | Expr::Nz(a) => vec![a],
        Expr::Tern(c, a, b) => vec![c, a, b],
        Expr::Call { args, .. } => args.iter().collect(),
        _ => Vec::new(),
    }
}

fn expr_children_mut(e: &mut Expr) -> Vec<&mut Expr> {
    match e {
        Expr::Bin(_, a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            vec![a.as_mut(), b.as_mut()]
        }
        Expr::Not(a) | Expr::IsNa(a) | Expr::Nz(a) => vec![a.as_mut()],
        Expr::Tern(c, a, b) => vec![c.as_mut(), a.as_mut(), b.as_mut()],
        Expr::Call { args, .. } => args.iter_mut().collect(),
        _ => Vec::new(),
    }
}

fn size(e: &Expr) -> usize {
    1 + expr_children(e).into_iter().map(size).sum::<usize>()
}

fn lit_mag(e: &Expr) -> u64 {
    let own = match e {
        Expr::LitI(v) => u64::from(v.unsigned_abs()),
        Expr::LitF(h) => u64::from(h.unsigned_abs()),
        _ => 0,
    };
    own + expr_children(e).into_iter().map(lit_mag).sum::<u64>()
}

fn nth_mut(e: &mut Expr, k: usize) -> &mut Expr {
    if k == 0 {
        return e;
    }
    let mut k = k - 1;
    for c in expr_children_mut(e) {
        let s = size(c);
        if k < s {
            return nth_mut(c, k);
        }
        k -= s;
    }
    unreachable!("nth_mut past the subtree")
}

fn roots<'a>(list: &'a [Stmt], out: &mut Vec<&'a Expr>) {
    for s in list {
        match s {
            Stmt::Decl { e, .. } | Stmt::Assign { e, .. } | Stmt::Plot { e, .. } => out.push(e),
            Stmt::If { cond, then, els } => {
                out.push(cond);
                roots(then, out);
                roots(els, out);
            }
            Stmt::For { body, .. } => roots(body, out),
        }
    }
}

fn roots_mut<'a>(list: &'a mut [Stmt], out: &mut Vec<&'a mut Expr>) {
    for s in list.iter_mut() {
        match s {
            Stmt::Decl { e, .. } | Stmt::Assign { e, .. } | Stmt::Plot { e, .. } => out.push(e),
            Stmt::If { cond, then, els } => {
                out.push(cond);
                roots_mut(then, out);
                roots_mut(els, out);
            }
            Stmt::For { body, .. } => roots_mut(body, out),
        }
    }
}

fn count_stmts(list: &[Stmt]) -> u32 {
    let mut n = 0;
    for s in list {
        n += 1;
        match s {
            Stmt::If { then, els, .. } => n += count_stmts(then) + count_stmts(els),
            Stmt::For { body, .. } => n += count_stmts(body),
            _ => {}
        }
    }
    n
}

/// The program's [`Measure`].
pub fn measure(p: &Program) -> Measure {
    let mut rs = Vec::new();
    roots(&p.stmts, &mut rs);
    let nodes: usize = rs.iter().map(|e| size(e)).sum();
    let lits: u64 = rs.iter().map(|e| lit_mag(e)).sum();
    (
        ast::body_lines(p),
        nodes as u32 + count_stmts(&p.stmts),
        lits + loop_trips(&p.stmts),
    )
}

fn loop_trips(list: &[Stmt]) -> u64 {
    let mut n = 0u64;
    for s in list {
        match s {
            Stmt::For { hi, lo, body, .. } => {
                n += u64::from((hi - lo).unsigned_abs()) + loop_trips(body)
            }
            Stmt::If { then, els, .. } => n += loop_trips(then) + loop_trips(els),
            _ => {}
        }
    }
    n
}

/// The `k`-th statement in source order: delete it.
fn delete_stmt(list: &mut Vec<Stmt>, k: &mut usize) -> bool {
    let mut i = 0usize;
    while i < list.len() {
        if *k == 0 {
            list.remove(i);
            return true;
        }
        *k -= 1;
        let hit = match &mut list[i] {
            Stmt::If { then, els, .. } => delete_stmt(then, k) || delete_stmt(els, k),
            Stmt::For { body, .. } => delete_stmt(body, k),
            _ => false,
        };
        if hit {
            return true;
        }
        i += 1;
    }
    false
}

/// The `k`-th `if` in source order: replace it by its `then` (`arm` 0) or `else` (1) statements.
fn collapse_if(list: &mut Vec<Stmt>, k: &mut usize, arm: u8) -> bool {
    let mut i = 0usize;
    while i < list.len() {
        if let Stmt::If { then, els, .. } = &list[i] {
            if *k == 0 {
                let rep = if arm == 0 { then.clone() } else { els.clone() };
                list.splice(i..=i, rep);
                return true;
            }
            *k -= 1;
        }
        let hit = match &mut list[i] {
            Stmt::If { then, els, .. } => collapse_if(then, k, arm) || collapse_if(els, k, arm),
            Stmt::For { body, .. } => collapse_if(body, k, arm),
            _ => false,
        };
        if hit {
            return true;
        }
        i += 1;
    }
    false
}

fn count_fors(list: &[Stmt]) -> usize {
    let mut n = 0;
    for s in list {
        match s {
            Stmt::For { body, .. } => n += 1 + count_fors(body),
            Stmt::If { then, els, .. } => n += count_fors(then) + count_fors(els),
            _ => {}
        }
    }
    n
}

/// The `k`-th `for`: one trip.
fn one_trip(list: &mut [Stmt], k: &mut usize) -> bool {
    for s in list.iter_mut() {
        match s {
            Stmt::For { lo, hi, body, .. } => {
                if *k == 0 {
                    if *hi == *lo {
                        return false;
                    }
                    *hi = *lo;
                    return true;
                }
                *k -= 1;
                if one_trip(body, k) {
                    return true;
                }
            }
            Stmt::If { then, els, .. } => {
                if one_trip(then, k) || one_trip(els, k) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn slot_types(list: &[Stmt], out: &mut Vec<(u16, Sc)>) {
    for s in list {
        match s {
            Stmt::Decl { slot, ty, .. } => out.push((*slot, *ty)),
            Stmt::If { then, els, .. } => {
                slot_types(then, out);
                slot_types(els, out);
            }
            Stmt::For { body, .. } => slot_types(body, out),
            _ => {}
        }
    }
}

fn ty_of(e: &Expr, slots: &[(u16, Sc)]) -> Sc {
    let slot_ty = |s: u16| {
        slots
            .iter()
            .find(|(x, _)| *x == s)
            .map(|(_, t)| *t)
            .unwrap_or(Sc::Float)
    };
    match e {
        Expr::LitI(_) | Expr::Loop(_) => Sc::Int,
        Expr::LitF(_) => Sc::Float,
        Expr::LitB(_)
        | Expr::Cmp(..)
        | Expr::And(..)
        | Expr::Or(..)
        | Expr::Not(_)
        | Expr::IsNa(_) => Sc::Bool,
        Expr::Syn(v) => match v {
            SynVar::Syni => Sc::Int,
            SynVar::Synb => Sc::Bool,
            _ => Sc::Float,
        },
        Expr::Slot(s) => slot_ty(*s),
        Expr::Hist(r, _) => match r {
            ast::HistRef::Syn(SynVar::Syni) => Sc::Int,
            ast::HistRef::Syn(_) => Sc::Float,
            ast::HistRef::Slot(s) => slot_ty(*s),
        },
        Expr::Bin(_, a, _) | Expr::Nz(a) => ty_of(a, slots),
        Expr::Tern(_, a, _) => ty_of(a, slots),
        Expr::Call { ov, .. } => match rets(&pine_spec::OVERLOADS[*ov as usize])[0].ty.ty {
            pine_ir::Ty::Int => Sc::Int,
            pine_ir::Ty::Bool => Sc::Bool,
            _ => Sc::Float,
        },
    }
}

fn is_lit(e: &Expr) -> bool {
    matches!(e, Expr::LitI(_) | Expr::LitF(_) | Expr::LitB(_))
}

/// Candidate `c` of pass `pass` over `p`, or `None` when `c` is past the pass's sites.
fn candidate(p: &Program, pass: u8, c: usize) -> Option<Program> {
    let mut q = p.clone();
    match pass {
        1 => {
            let mut k = c;
            delete_stmt(&mut q.stmts, &mut k).then_some(q)
        }
        2 | 3 => {
            // Expression sites × 2 variants.
            let (site, var) = (c / 2, c % 2);
            let mut slots = Vec::new();
            slot_types(&p.stmts, &mut slots);
            let total: usize = {
                let mut rs = Vec::new();
                roots(&p.stmts, &mut rs);
                rs.iter().map(|e| size(e)).sum()
            };
            if site >= total {
                return None;
            }
            let mut rs = Vec::new();
            roots_mut(&mut q.stmts, &mut rs);
            let mut k = site;
            for r in rs {
                let s = size(r);
                if k < s {
                    let e = nth_mut(r, k);
                    let rep = if pass == 2 {
                        if is_lit(e) {
                            None
                        } else {
                            Some(match (ty_of(e, &slots), var) {
                                (Sc::Int, v) => Expr::LitI(v as i32),
                                (Sc::Float, v) => Expr::LitF(100 * v as i32),
                                (Sc::Bool, v) => Expr::LitB(v == 1),
                            })
                        }
                    } else {
                        match (&*e, var) {
                            (Expr::LitI(v), 0) if *v > 1 => Some(Expr::LitI(*v / 2)),
                            (Expr::LitI(v), _) if *v > 1 => Some(Expr::LitI(1)),
                            (Expr::LitI(1), 0) => Some(Expr::LitI(0)),
                            (Expr::LitF(h), 0) if *h > 100 => Some(Expr::LitF(*h / 2)),
                            (Expr::LitF(h), _) if *h > 100 => Some(Expr::LitF(100)),
                            (Expr::LitF(h), 0) if *h > 0 => Some(Expr::LitF(0)),
                            _ => None,
                        }
                    };
                    return match rep {
                        Some(x) => {
                            *e = x;
                            Some(q)
                        }
                        // A site with no edit: an unchanged program, which the measure refuses.
                        None => Some(p.clone()),
                    };
                }
                k -= s;
            }
            None
        }
        4 => {
            if c >= count_fors(&p.stmts) {
                return None;
            }
            let mut k = c;
            // An already-one-trip loop is a no-op site, which the measure refuses.
            Some(if one_trip(&mut q.stmts, &mut k) {
                q
            } else {
                p.clone()
            })
        }
        7 => {
            let (site, arm) = (c / 2, (c % 2) as u8);
            let mut k = site;
            collapse_if(&mut q.stmts, &mut k, arm).then_some(q)
        }
        _ => None,
    }
}

/// Greedy multi-pass delta-debugging over the generator's own tree (§4.8.5).
pub fn shrink<P: Predicate>(
    g: &Generated,
    sig: &Signature,
    p: &mut P,
    b: ShrinkBudget,
) -> ShrinkOutcome {
    let mut best = g.ast.clone();
    let mut m = measure(&best);
    let lines_before = ast::body_lines(&best);
    let mut steps = 0u32;
    let mut trace = Vec::new();
    'rounds: loop {
        let mut kept = false;
        for pass in [1u8, 2, 3, 4, 7] {
            let mut c = 0usize;
            while let Some(q) = candidate(&best, pass, c) {
                let mq = measure(&q);
                if mq < m {
                    if steps >= b.max_steps {
                        break 'rounds;
                    }
                    steps += 1;
                    if p.diverges(&q).as_ref() == Some(sig) {
                        best = q;
                        m = mq;
                        trace.push(m);
                        kept = true;
                        // Sites shifted: re-scan this pass from the same index.
                        continue;
                    }
                }
                c += 1;
            }
        }
        if !kept {
            break;
        }
    }
    ShrinkOutcome {
        source: ast::render(&best),
        lines_after: ast::body_lines(&best),
        program: best,
        steps,
        lines_before,
        signature: sig.clone(),
        trace,
    }
}
