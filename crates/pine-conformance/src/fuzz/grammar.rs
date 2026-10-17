// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The type-directed grammar (impl-06 §4.8.2) and its derived builtin alphabet (§4.8.3).
//!
//! An expression is generated *from* a demanded type: every production admissible for the demand
//! is a candidate, chosen uniformly; a builtin's parameters become the next demands. A demanded
//! qualifier below `series` (a `simple int` length) is met by a literal, so qualifiers hold by
//! construction and a well-typed program compiles.
//!
//! `pin:06-fuzz-grammar-v1-subset` — `GRAMMAR_VERSION` 1 emits the `Indicator` profile's
//! `x = e`, `var x = e`, `x := e`, `if`/`else`, literal `for`, `plot`, and every expression form of
//! §4.8.2 except field access; `while`, `switch`, user functions and `type` (and the `Broker`,
//! `Security`, `Collections` additions) are a later grammar version — each bumps the version.

use pine_ir::{Dialect, Qual, Ty};
use pine_spec::{params, pf, rets};

use super::ast::{BinOp, CmpOp, Expr, HistRef, Program, Sc, Stmt, SynVar};
use super::rng::Xoshiro256StarStar;
use super::{GrammarConfig, Profile};

/// Entries the alphabet never contains (§4.8.3: clock-adjacent, drawing, alert, log names) —
/// by namespace, then by full name.
///
/// `pin:06-fuzz-deny` — the spec names the classes, not the list; this is the literal reading of
/// those classes plus the declaration/capture statements (`plot*`, `input.*`, `strategy*`), which
/// the profiles emit as statements, never as expressions. The bare `input(…)` stays denied
/// (r21, 2026-10-17): with it admitted, 2 000 seeds compile with zero panics since the
/// `pin:02-input-series-defval` fix, but 153 well-typed programs are refused — 150 `3008` (the
/// `series` row `INPUT_F_SRC` fed a non-constant) and 3 `3050` (`vN := input(…)` names a second
/// input `vN`) — so the generator cannot yet emit only valid `input()` calls, and admitting it
/// re-draws every grammar-1 program (the `regressions.tsv` seeds) — a `GRAMMAR_VERSION` decision.
pub const FUZZ_DENY_NS: [&str; 18] = [
    "input",
    "strategy",
    "request",
    "matrix",
    "line",
    "label",
    "box",
    "table",
    "polyline",
    "linefill",
    "chart",
    "log",
    "runtime",
    "syminfo",
    "timeframe",
    "session",
    "ticker",
    "barstate",
];

/// Full names denied (see [`FUZZ_DENY_NS`]).
pub const FUZZ_DENY: [&str; 31] = [
    "input",
    "plot",
    "plotshape",
    "plotchar",
    "plotarrow",
    "plotcandle",
    "plotbar",
    "hline",
    "fill",
    "bgcolor",
    "barcolor",
    "alert",
    "alertcondition",
    "indicator",
    "strategy",
    "library",
    "timestamp",
    "time",
    "time_close",
    "time_tradingday",
    "timenow",
    "year",
    "month",
    "weekofyear",
    "dayofmonth",
    "dayofweek",
    "hour",
    "minute",
    "second",
    "max_bars_back",
    "math.random",
];

/// One alphabet row: an overload of `pine_spec::OVERLOADS` in the active tiers and the dialect,
/// not denied.
#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    /// `Entry::full_str()`.
    pub name: &'static str,
    /// Index into `pine_spec::OVERLOADS`.
    pub ov: u16,
    /// The single scalar result, when the overload is generable (`None`: in the alphabet, never
    /// emitted — a non-scalar parameter or result).
    pub ret: Option<Sc>,
}

fn sc_of(t: Ty) -> Option<Sc> {
    match t {
        Ty::Int => Some(Sc::Int),
        Ty::Float => Some(Sc::Float),
        Ty::Bool => Some(Sc::Bool),
        _ => None,
    }
}

/// Also every `$`-namespaced row (`$op.*`: operator overloads the language spells as operators,
/// never as calls).
pub fn denied(e: &pine_spec::Entry) -> bool {
    let ns = e.ns.as_str();
    let head = ns.split('.').next().unwrap_or("");
    ns.starts_with('$')
        || e.full_str().starts_with('$')
        || FUZZ_DENY_NS.contains(&head)
        || FUZZ_DENY.contains(&e.full_str())
}

/// Whether `ov` can be emitted by the type-directed grammar in `d`: required parameters scalar
/// and positional, one scalar result, no type variable; and it reads no bar field implicitly
/// (`gen::group_b::IMPLICIT_BAR_READERS`: the drives of §4.8.4 a differ in their bars, never in
/// the SYN-1 series).
///
/// `pin:06-fuzz-implicit-bar` — §4.8.4 a's realtime run "carries the whole bar's values"; ours
/// steps `SynTape` against `SynBars`' constant bars, so a row whose body reads the bar
/// (`pine_spec::sx::IMPLICIT_BAR`: OHLCV/time slots, `bar()`, the host) is never emitted — the
/// first sweep's 52 mode-vs-mode "divergences" all shrank to `hlcc4()` / `ohlc4()` reads.
fn generable(ov: &pine_spec::Overload, name: &str, d: Dialect) -> Option<Sc> {
    if ov.sx & pine_spec::sx::IMPLICIT_BAR != 0
        || crate::gen::group_b::IMPLICIT_BAR_READERS
            .iter()
            .any(|r| r.0 == name)
    {
        return None;
    }
    let rs = rets(ov);
    if rs.len() != 1 || rs[0].tv != 0 {
        return None;
    }
    let ret = sc_of(rs[0].ty.ty)?;
    for p in params(ov) {
        if !p.versions.has(d) || p.flags & (pf::OPTIONAL | pf::SYNTHETIC) != 0 {
            continue;
        }
        if p.flags & (pf::VARIADIC | pf::NAMED_ONLY) != 0 || p.sig.tv != 0 {
            return None;
        }
        sc_of(p.sig.ty.ty)?;
    }
    Some(ret)
}

/// The derived alphabet (§4.8.3): `pine_spec::OVERLOADS ∩ active_tiers ∩ dialect − FUZZ_DENY`,
/// in overload order. Load-time; allocates.
pub fn alphabet(d: Dialect, active_tiers: u8) -> Vec<Builtin> {
    let mut out = Vec::new();
    for (i, ov) in pine_spec::OVERLOADS.iter().enumerate() {
        let e = &pine_spec::ENTRIES[ov.entry as usize];
        if e.tier.bit() & active_tiers == 0 || !ov.versions.has(d) || !e.versions.has(d) {
            continue;
        }
        // pin:06-fuzz-alphabet-scope — §4.8.3 stays "inside the implemented tiers": an entry
        // `pine_spec` marks `Scope::Declined` is refused by design (3024 `declined=1`), so it is
        // outside the alphabet like a denied name.
        if e.is_type || denied(e) || e.scope == pine_spec::Scope::Declined {
            continue;
        }
        // pin:06-fuzz-alphabet-served — "implemented" is pine-std's knowledge (`pine_spec::Scope`):
        // a row `pine_std::served` calls `Unreachable` is refused `3024 … unserved=1` by the
        // snapshot, not by the program, so it waits outside the alphabet until pine-std serves it.
        if pine_std::served(pine_ir::OverloadId(i as u16)) == pine_std::Served::Unreachable {
            continue;
        }
        let name = e.full_str();
        out.push(Builtin {
            name,
            ov: i as u16,
            ret: generable(ov, name, d),
        });
    }
    out
}

/// A visible slot.
#[derive(Debug, Clone, Copy)]
struct Var {
    slot: u16,
    ty: Sc,
    global: bool,
}

struct Gen<'a> {
    rng: Xoshiro256StarStar,
    cfg: &'a GrammarConfig,
    alpha: Vec<&'a Builtin>,
    scopes: Vec<Vec<Var>>,
    loops: Vec<u16>,
    next_slot: u16,
    next_loop: u16,
    budget: u16,
    ill_pending: bool,
    ill_typed: bool,
}

impl Gen<'_> {
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.rng.below(xs.len() as u32) as usize]
    }

    fn visible(&self, ty: Sc) -> Vec<Var> {
        let mut v = Vec::new();
        for s in &self.scopes {
            for x in s {
                if x.ty == ty {
                    v.push(*x);
                }
            }
        }
        v
    }

    fn lit(&mut self, ty: Sc) -> Expr {
        match ty {
            Sc::Int => Expr::LitI(self.rng.range_i32(0, 100)),
            Sc::Float => Expr::LitF(self.rng.range_i32(0, 10_000)),
            Sc::Bool => Expr::LitB(self.rng.below(2) == 1),
        }
    }

    /// A literal meeting a parameter demand below `series` (lengths, `simple` arguments).
    fn const_arg(&mut self, ty: Sc) -> Expr {
        match ty {
            Sc::Int => Expr::LitI(self.rng.range_i32(1, i32::from(self.cfg.max_hist_k))),
            Sc::Float => Expr::LitF(self.rng.range_i32(1, 1_000)),
            Sc::Bool => Expr::LitB(self.rng.below(2) == 1),
        }
    }

    fn leaf(&mut self, ty: Sc) -> Expr {
        let vars = self.visible(ty);
        let mut opts: u32 = 2; // literal, prelude
        if !vars.is_empty() {
            opts += 1;
        }
        let has_loop = ty != Sc::Bool && !self.loops.is_empty();
        if has_loop {
            opts += 1;
        }
        match self.rng.below(opts) {
            0 => self.lit(ty),
            1 => Expr::Syn(match ty {
                Sc::Int => SynVar::Syni,
                Sc::Bool => SynVar::Synb,
                Sc::Float => self.pick(&[SynVar::Syn, SynVar::Synna, SynVar::Syni]),
            }),
            2 if !vars.is_empty() => Expr::Slot(self.pick(&vars).slot),
            _ => Expr::Loop(self.pick(&self.loops.clone())),
        }
    }

    fn hist(&mut self, ty: Sc) -> Expr {
        let k = self.rng.range_i32(1, i32::from(self.cfg.max_hist_k)) as u16;
        let globals: Vec<Var> = self.visible(ty).into_iter().filter(|v| v.global).collect();
        let r = if !globals.is_empty() && self.rng.below(2) == 0 {
            HistRef::Slot(self.pick(&globals).slot)
        } else if ty == Sc::Int {
            HistRef::Syn(SynVar::Syni)
        } else {
            HistRef::Syn(self.pick(&[SynVar::Syn, SynVar::Synna]))
        };
        Expr::Hist(r, k)
    }

    /// An expression of type `ty`; `calls` false below an `and`/`or` right operand (W3).
    fn expr(&mut self, ty: Sc, depth: u8, calls: bool) -> Expr {
        if depth == 0 {
            return self.leaf(ty);
        }
        let d = depth - 1;
        let cands: Vec<&Builtin> = if calls {
            self.alpha
                .iter()
                .copied()
                .filter(|b| b.ret == Some(ty) || (ty == Sc::Float && b.ret == Some(Sc::Int)))
                .collect()
        } else {
            Vec::new()
        };
        // 0 leaf, 1 binary/compare, 2 ternary, 3 history/not, 4 na/nz, 5 call.
        let n = if cands.is_empty() { 5 } else { 6 };
        match self.rng.below(n) {
            0 => self.leaf(ty),
            1 => match ty {
                Sc::Float => {
                    let op = self.pick(&[BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::Div]);
                    let a = self.expr(Sc::Float, d, calls);
                    let b = self.expr(Sc::Float, d, calls);
                    Expr::Bin(op, Box::new(a), Box::new(b))
                }
                Sc::Int => {
                    let op = self.pick(&[BinOp::Add, BinOp::Sub, BinOp::Mul]);
                    let a = self.expr(Sc::Int, d, calls);
                    // `*` takes a small literal right operand: no i64 overflow by construction.
                    let b = if op == BinOp::Mul {
                        Expr::LitI(self.rng.range_i32(0, 9))
                    } else {
                        self.expr(Sc::Int, d, calls)
                    };
                    Expr::Bin(op, Box::new(a), Box::new(b))
                }
                Sc::Bool => match self.rng.below(3) {
                    0 => {
                        let op = self.pick(&[
                            CmpOp::Lt,
                            CmpOp::Gt,
                            CmpOp::Le,
                            CmpOp::Ge,
                            CmpOp::Eq,
                            CmpOp::Ne,
                        ]);
                        let a = self.expr(Sc::Float, d, calls);
                        let b = self.expr(Sc::Float, d, calls);
                        Expr::Cmp(op, Box::new(a), Box::new(b))
                    }
                    1 => {
                        let a = self.expr(Sc::Bool, d, calls);
                        let b = self.expr(Sc::Bool, d, false);
                        Expr::And(Box::new(a), Box::new(b))
                    }
                    _ => {
                        let a = self.expr(Sc::Bool, d, calls);
                        let b = self.expr(Sc::Bool, d, false);
                        Expr::Or(Box::new(a), Box::new(b))
                    }
                },
            },
            2 => {
                let c = self.expr(Sc::Bool, d, calls);
                let a = self.expr(ty, d, calls);
                let b = self.expr(ty, d, calls);
                Expr::Tern(Box::new(c), Box::new(a), Box::new(b))
            }
            3 => match ty {
                Sc::Bool => Expr::Not(Box::new(self.expr(Sc::Bool, d, calls))),
                _ => self.hist(ty),
            },
            4 => match ty {
                Sc::Bool => {
                    let x = self.expr(Sc::Float, d, calls);
                    Expr::IsNa(Box::new(x))
                }
                _ => Expr::Nz(Box::new(self.expr(ty, d, calls))),
            },
            _ => {
                let b = *self.pick(&cands);
                let ov = &pine_spec::OVERLOADS[b.ov as usize];
                let mut args = Vec::new();
                for p in params(ov) {
                    if !p.versions.has(self.cfg.dialect)
                        || p.flags & (pf::OPTIONAL | pf::SYNTHETIC) != 0
                    {
                        continue;
                    }
                    let pt = sc_of(p.sig.ty.ty).expect("generable");
                    // pin:06-fuzz-int-args-literal — every `int` parameter (lengths, offsets,
                    // counts) takes a literal in `1..=max_hist_k`: a folded non-positive length is
                    // a compile refusal (3007), which §4.8.2's "never fails to compile" excludes.
                    args.push(if p.sig.ty.qual < Qual::Series || pt == Sc::Int {
                        self.const_arg(pt)
                    } else {
                        self.expr(pt, d, calls)
                    });
                }
                Expr::Call {
                    name: b.name,
                    ov: b.ov,
                    args,
                }
            }
        }
    }

    fn depth(&mut self) -> u8 {
        self.rng.range_i32(1, i32::from(self.cfg.max_expr_depth)) as u8
    }

    fn any_ty(&mut self) -> Sc {
        self.pick(&[Sc::Float, Sc::Float, Sc::Int, Sc::Bool])
    }

    fn block(&mut self, max: u32) -> Vec<Stmt> {
        self.scopes.push(Vec::new());
        let n = 1 + self.rng.below(max);
        let mut out = Vec::new();
        let mut i = 0;
        while i < n {
            out.push(self.stmt(false));
            i += 1;
        }
        self.scopes.pop();
        out
    }

    fn stmt(&mut self, global: bool) -> Stmt {
        self.budget = self.budget.saturating_sub(1);
        let nested = self.budget > 0 && self.scopes.len() < 3;
        let assignable: Vec<Var> = self.scopes.iter().flatten().copied().collect();
        let k = self.rng.below(if nested { 5 } else { 3 });
        match k {
            1 if !assignable.is_empty() => {
                let v = self.pick(&assignable);
                let dep = self.depth();
                let e = self.expr(v.ty, dep, true);
                Stmt::Assign { slot: v.slot, e }
            }
            3 => {
                let dep = self.depth();
                let cond = self.expr(Sc::Bool, dep, true);
                let then = self.block(3);
                let els = if self.rng.below(2) == 0 {
                    Vec::new()
                } else {
                    self.block(2)
                };
                Stmt::If { cond, then, els }
            }
            4 => {
                let var = self.next_loop;
                self.next_loop += 1;
                let hi = self
                    .rng
                    .range_i32(0, i32::from(self.cfg.max_loop_trips.min(16)) - 1);
                self.loops.push(var);
                let body = self.block(3);
                self.loops.pop();
                Stmt::For {
                    var,
                    lo: 0,
                    hi,
                    body,
                }
            }
            _ => {
                let ty = self.any_ty();
                let slot = self.next_slot;
                self.next_slot += 1;
                let var = global && self.rng.below(4) == 0;
                let dep = self.depth();
                // §4.8.2's deliberate `ill_typed_permille` (decided once per program): a bool
                // initialiser for the program's first numeric slot.
                let e = if ty != Sc::Bool && self.ill_pending && !self.ill_typed {
                    self.ill_typed = true;
                    self.expr(Sc::Bool, dep, true)
                } else {
                    self.expr(ty, dep, true)
                };
                self.scopes
                    .last_mut()
                    .expect("a scope")
                    .push(Var { slot, ty, global });
                Stmt::Decl { slot, ty, var, e }
            }
        }
    }
}

/// `(program, expect_refusal)` for `seed` — the body of [`super::generate`].
pub fn program(seed: u64, cfg: &GrammarConfig) -> (Program, bool) {
    debug_assert_eq!(
        cfg.profile,
        Profile::Indicator,
        "pin:06-fuzz-grammar-v1-subset"
    );
    let alpha_all = alphabet(cfg.dialect, cfg.active_tiers);
    let mut rng = Xoshiro256StarStar::seed_from(seed);
    let ill_pending = rng.permille(cfg.ill_typed_permille);
    let mut g = Gen {
        rng,
        ill_pending,
        cfg,
        alpha: alpha_all.iter().filter(|b| b.ret.is_some()).collect(),
        scopes: vec![Vec::new()],
        loops: Vec::new(),
        next_slot: 0,
        next_loop: 0,
        budget: cfg.max_stmts,
        ill_typed: false,
    };
    let mut stmts = Vec::new();
    let n = 1 + g.rng.below(u32::from(cfg.max_stmts.max(1)));
    let mut i = 0;
    while i < n && g.budget > 0 {
        stmts.push(g.stmt(true));
        i += 1;
    }
    let mut ix = 0u16;
    while ix < cfg.plots {
        let ty = g.pick(&[Sc::Float, Sc::Int]);
        let dep = g.depth();
        let e = g.expr(ty, dep, true);
        stmts.push(Stmt::Plot { e, ix });
        ix += 1;
    }
    (
        Program {
            dialect: cfg.dialect,
            stmts,
        },
        g.ill_typed,
    )
}
