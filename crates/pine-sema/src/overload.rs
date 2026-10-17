// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Overload resolution (impl-02 §4.5, pass 3): a call's candidates against its typed arguments.
//! It never guesses: a tie is `3009`, no survivor `3005` / `3006` / `3008` / `3010`.

use pine_ir::{elem, Diag, Dialect, OverloadId, Qual, Span, Sym, SymTable, Ty, TypeQ};
use pine_spec::{kind, pf, sx, tv, Entry, Overload, Scope, SigTy};

use crate::code::{diag, ReasonCode};
use crate::infer::K;
use crate::limits::MAX_FORMAT_ARGS;
use crate::sw::Readings;
use crate::types::{admits, conv, join, Conv};

/// One typed argument as overload resolution sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Arg {
    /// `name = …`, or positional.
    pub(crate) name: Option<Sym>,
    /// Its type; `Ty::Void` with `na` is the untyped `na`.
    pub(crate) t: TypeQ,
    /// The untyped `na` literal.
    pub(crate) is_na: bool,
    /// May be `na` structurally (`na`, an `if` without `else`, a `switch` without default).
    pub(crate) may_na: bool,
    /// A tuple expression.
    pub(crate) tuple: bool,
    /// A tuple's common element type (qualifier = the join), `Ty::Void` when the elements
    /// disagree: what an `options` parameter checks (A4.3).
    pub(crate) tuple_elem: TypeQ,
    /// Its folded value, when it folds (§4.17).
    pub(crate) k: Option<K>,
    /// A string literal written without escapes: a `LoadErr::at` narrows inside it (§3.7).
    pub(crate) plain_literal: bool,
    /// Its span.
    pub(crate) span: Span,
}

/// The chosen built-in overload.
#[derive(Debug, Clone)]
pub(crate) struct Picked {
    /// The overload.
    pub(crate) ov: OverloadId,
    /// `bound[k]`: the parameter row (index into `params(ov)`) argument `k` binds, `None` for a
    /// v5 repeated named argument that is dropped (MG-18).
    pub(crate) bound: Vec<Option<u16>>,
    /// The result types (`n_out` of them; empty for void).
    pub(crate) rets: Vec<TypeQ>,
}

/// How far a candidate got (§4.5 steps 3–5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fail {
    Bind,
    Type,
    Qual,
}

/// A survivor's rank (§4.5 step 6), lowest wins.
type Rank = (u32, u32, u32, u32);

/// A whole-value type-variable binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bind {
    ty: Ty,
    a: u16,
    b: u16,
}

/// The binding of one candidate's parameters: `slots[k]` = parameter row of argument `k`.
struct Bound {
    slots: Vec<Option<u16>>,
    defaults: u32,
}

/// Bind arguments to the dialect-visible rows of `ps` (§4.5 step 3); `Err` names the problem.
fn bind(
    ps: &[pine_spec::Param],
    args: &[Arg],
    d: Dialect,
    syms: &SymTable,
) -> Result<Bound, String> {
    let visible = |p: &pine_spec::Param| p.versions.has(d) && p.flags & pf::SYNTHETIC == 0;
    let mut filled = vec![false; ps.len()];
    let mut slots = vec![None; args.len()];
    let mut next = 0usize;
    for (k, a) in args.iter().enumerate() {
        if a.name.is_some() {
            continue;
        }
        while next < ps.len() && !(visible(&ps[next]) && ps[next].flags & pf::NAMED_ONLY == 0) {
            next += 1;
        }
        if next >= ps.len() {
            return Err(format!("too many arguments ({})", args.len()));
        }
        slots[k] = Some(next as u16);
        filled[next] = true;
        if ps[next].flags & pf::VARIADIC == 0 {
            next += 1;
        }
    }
    for (k, a) in args.iter().enumerate() {
        let Some(name) = a.name else { continue };
        let text = syms.resolve(name);
        let Some(p) = (0..ps.len()).find(|&p| visible(&ps[p]) && ps[p].name.as_str() == text)
        else {
            return Err(format!("no parameter named `{text}`"));
        };
        if filled[p] {
            if d == Dialect::V6 {
                return Err(format!("parameter `{text}` is given twice (MG-18)"));
            }
            continue; // v5: "only the first value will be used" (MG-18)
        }
        filled[p] = true;
        slots[k] = Some(p as u16);
    }
    let mut defaults = 0;
    for (p, row) in ps.iter().enumerate() {
        if !visible(row) || filled[p] {
            continue;
        }
        if row.flags & (pf::OPTIONAL | pf::VARIADIC) == 0 {
            return Err(format!("missing argument `{}`", row.name.as_str()));
        }
        if row.flags & pf::OPTIONAL != 0 {
            defaults += 1;
        }
    }
    Ok(Bound { slots, defaults })
}

/// The type of an `elem` code as a [`Bind`].
fn elem_bind(e: u16) -> Bind {
    match elem::as_scalar(e) {
        Some(ty) => Bind { ty, a: 0, b: 0 },
        None if e & elem::ENUM != 0 => Bind {
            ty: Ty::Enum,
            a: e & elem::DECL_MASK,
            b: 0,
        },
        None => Bind {
            ty: Ty::Udt,
            a: e & elem::DECL_MASK,
            b: 0,
        },
    }
}

/// The `elem` code of a whole-value binding (`None` for a collection).
fn bind_elem(b: Bind) -> Option<u16> {
    match b.ty {
        Ty::Udt => Some(elem::udt(b.a)),
        Ty::Enum => Some(elem::enumeration(b.a)),
        Ty::Array | Ty::Map | Ty::Matrix => None,
        ty => Some(elem::scalar(ty)),
    }
}

/// Unify a type-variable position with `got`; `false` on a clash or a kind outside `kinds`.
fn unify(vars: &mut [Option<Bind>; 3], v: u8, got: Bind, kinds: &[u8; 3]) -> bool {
    let i = (v - 1) as usize;
    let k = if matches!(got.ty, Ty::Array | Ty::Map | Ty::Matrix) {
        kind::COLL
    } else {
        kind::of(got.ty)
    };
    if kinds[i] & k == 0 {
        return false;
    }
    match vars[i] {
        Some(b) => b == got,
        None => {
            vars[i] = Some(got);
            true
        }
    }
}

/// The parameter type with its variables substituted from `arg` (binding them), or `None` when
/// they cannot bind. `generic` counts a variable match.
fn instantiate(
    sig: SigTy,
    arg: TypeQ,
    vars: &mut [Option<Bind>; 3],
    kinds: &[u8; 3],
    generic: &mut u32,
) -> Option<TypeQ> {
    if sig.tv == tv::NONE {
        return Some(sig.ty);
    }
    *generic += 1;
    let mut t = sig.ty;
    let w = tv::whole(sig.tv);
    if w != tv::NONE {
        if arg.ty == Ty::Void {
            return vars[(w - 1) as usize].map(|b| TypeQ {
                ty: b.ty,
                a: b.a,
                b: b.b,
                ..t
            });
        }
        let got = Bind {
            ty: arg.ty,
            a: arg.a,
            b: arg.b,
        };
        if !unify(vars, w, got, kinds) {
            return None;
        }
        t.ty = got.ty;
        t.a = got.a;
        t.b = got.b;
        return Some(t);
    }
    if arg.ty != sig.ty.ty {
        return None;
    }
    let (va, vb) = (tv::a(sig.tv), tv::b(sig.tv));
    if va != tv::NONE {
        if !unify(vars, va, elem_bind(arg.a), kinds) {
            return None;
        }
        t.a = arg.a;
    }
    if vb != tv::NONE {
        if !unify(vars, vb, elem_bind(arg.b), kinds) {
            return None;
        }
        t.b = arg.b;
    }
    Some(t)
}

/// The cost of a conversion (§4.5 step 6, `s0`).
const fn cost(c: Conv) -> u32 {
    match c {
        Conv::Same => 0,
        Conv::IntToFloat | Conv::NumToBool => 1,
        Conv::Na => 2,
    }
}

/// Steps 3–5 for one built-in candidate: its rank, or how it failed and why.
fn try_overload(
    ov: &Overload,
    args: &[Arg],
    targs: &[TypeQ],
    d: Dialect,
    rd: &Readings,
    syms: &SymTable,
) -> Result<(Rank, Bound, [Option<Bind>; 3], Qual), (Fail, String)> {
    let ps = pine_spec::params(ov);
    let b = bind(ps, args, d, syms).map_err(|m| (Fail::Bind, m))?;
    // `f<T, …>(…)`: the written type arguments bind the overload's variables in order
    // (`array.new<float>` → T, `map.new<string, float>` → K, V).
    let mut vars = [None; 3];
    let mut next = 0;
    for (i, &k) in ov.tv_kinds.iter().enumerate() {
        if k != 0 && next < targs.len() {
            let t = targs[next];
            let got = Bind {
                ty: t.ty,
                a: t.a,
                b: t.b,
            };
            if !unify(&mut vars, i as u8 + 1, got, &ov.tv_kinds) {
                return Err((Fail::Type, "a type argument of the wrong kind".to_owned()));
            }
            next += 1;
        }
    }
    if next < targs.len() {
        let m = format!("{} type arguments, {} accepted", targs.len(), next);
        return Err((Fail::Bind, m));
    }
    let (mut s0, mut s1, mut s3) = (0u32, 0u32, 0u32);
    let mut arg_join = Qual::Const;
    let mut qual_fail = None;
    for (k, a) in args.iter().enumerate() {
        let Some(p) = b.slots[k] else { continue };
        let row = ps[p as usize];
        let pname = row.name.as_str();
        if a.tuple && row.flags & pf::OPTIONS != 0 {
            // A4.3 / A4.4d: a bracketed tuple of `const` values of the element type (`T` binds
            // the script's enum for `input.enum`).
            let el = a.tuple_elem;
            let ok = el.ty != Ty::Void
                && instantiate(row.sig, el, &mut vars, &ov.tv_kinds, &mut s1)
                    .is_some_and(|w| conv(el, w, d, rd).is_some());
            if !ok {
                let m = format!("`{pname}` takes a tuple of {}", ty_name(row.sig.ty));
                return Err((Fail::Type, m));
            }
            if el.qual != Qual::Const && qual_fail.is_none() {
                qual_fail = Some(format!("`{pname}` takes `const` values"));
            }
            continue;
        }
        if a.tuple {
            if row.flags & pf::TUPLE_OK == 0 {
                return Err((Fail::Type, format!("`{pname}` takes no tuple")));
            }
            continue;
        }
        let Some(want) = instantiate(row.sig, a.t, &mut vars, &ov.tv_kinds, &mut s1) else {
            return Err((Fail::Type, format!("`{pname}` cannot take this type")));
        };
        if row.flags & pf::UNIQUE != 0 && d == Dialect::V6 && a.may_na {
            return Err((
                Fail::Type,
                format!("`{pname}` takes a unique type, which is never `na` in v6 (MG-6)"),
            ));
        }
        let Some(c) = conv(a.t, want, d, rd) else {
            return Err((
                Fail::Type,
                format!(
                    "`{pname}` is {}, the argument {}",
                    ty_name(want),
                    ty_name(a.t)
                ),
            ));
        };
        s0 += cost(c);
        s3 += row.sig.ty.qual as u32;
        arg_join = join(arg_join, a.t.qual);
        if !admits(row.sig.ty.qual, a.t.qual) && qual_fail.is_none() {
            qual_fail = Some(format!(
                "`{pname}` accepts at most {}, the argument is {}",
                qual_name(row.sig.ty.qual),
                qual_name(a.t.qual)
            ));
        }
    }
    if let Some(m) = qual_fail {
        return Err((Fail::Qual, m));
    }
    let defaults = b.defaults;
    Ok(((s0, s1, defaults, s3), b, vars, arg_join))
}

/// `int`, `float`, … for messages.
pub(crate) fn ty_name(t: TypeQ) -> &'static str {
    match t.ty {
        Ty::Void => "void",
        Ty::Int => "int",
        Ty::Float => "float",
        Ty::Bool => "bool",
        Ty::Color => "color",
        Ty::String => "string",
        Ty::Array => "an array",
        Ty::Map => "a map",
        Ty::Matrix => "a matrix",
        Ty::Udt => "a user-defined type",
        Ty::Enum => "an enum",
        Ty::Plot => "plot",
        Ty::Hline => "hline",
    }
}

/// `const`, `input`, … for messages.
pub(crate) const fn qual_name(q: Qual) -> &'static str {
    match q {
        Qual::Const => "const",
        Qual::Input => "input",
        Qual::Simple => "simple",
        Qual::Series => "series",
    }
}

/// The result types of `ov` with `vars` substituted, and the result qualifier (§4.5 tail).
fn results(ov: &Overload, vars: &[Option<Bind>; 3], arg_join: Qual) -> Vec<TypeQ> {
    let q = match ov.ret_qual.fixed() {
        Some(q) => q,
        None => arg_join,
    };
    let q = if ov.is_stateful() { Qual::Series } else { q };
    let mut out = Vec::with_capacity(ov.n_out as usize);
    for r in pine_spec::rets(ov) {
        let mut t = r.ty;
        t.qual = q;
        let w = tv::whole(r.tv);
        if w != tv::NONE {
            if let Some(b) = vars[(w - 1) as usize] {
                t.ty = b.ty;
                t.a = b.a;
                t.b = b.b;
            }
        }
        let (va, vb) = (ret_elem_var(ov, tv::a(r.tv)), tv::b(r.tv));
        if va != tv::NONE {
            if let Some(e) = vars[(va - 1) as usize].and_then(bind_elem) {
                t.a = e;
            }
        }
        if vb != tv::NONE {
            if let Some(e) = vars[(vb - 1) as usize].and_then(bind_elem) {
                t.b = e;
            }
        }
        out.push(t);
    }
    out
}

/// The variable a result's element position reads. pin:02-map-keys-values-elem — R-P2 CI-046
/// (F107, TradingView compiles `array.join(map.keys(m), ",")` and `array.get(map.values(m), 1)` in
/// v6 and v5): the reference's `map.keys(id)` returns "an array of all the keys", `map.values(id)`
/// "of all the values", i.e. `array<K>` / `array<V>` of the `map<K,V>` argument. The spec's
/// signature encoding (impl-00 §3.8) admits only `array<T>` in an array position, so both rows are
/// written `array<T>` with `T` never bound by a parameter; read as `K` / `V` here. Every other
/// overload reads its own variable.
fn ret_elem_var(ov: &Overload, v: u8) -> u8 {
    if v != tv::T {
        v
    } else if ov.id == pine_spec::ov::MAP_KEYS {
        tv::K
    } else if ov.id == pine_spec::ov::MAP_VALUES {
        tv::V
    } else {
        v
    }
}

/// Steps 1–8 over a built-in entry's overloads.
pub(crate) fn builtin(
    e: &Entry,
    args: &[Arg],
    targs: &[TypeQ],
    at: Span,
    rd: &Readings,
    d: Dialect,
    active_tiers: u8,
    syms: &SymTable,
) -> Result<Picked, Diag> {
    let full = e.full_str();
    if e.scope == Scope::Declined && e.overloads().iter().any(|o| o.sx & sx::NONSTD_CHART != 0) {
        let msg = format!("`{full}` needs a non-standard chart");
        return Err(diag(ReasonCode::NonStandardChart, at, msg));
    }
    if e.scope == Scope::Declined {
        let msg = format!(
            "deferred-builtin ns={} name={} tier={} declined=1",
            e.ns.as_str(),
            e.name.as_str(),
            e.tier as u8
        );
        return Err(diag(ReasonCode::DeferredBuiltin, at, msg));
    }
    if e.tier.bit() & active_tiers == 0 {
        let msg = format!(
            "deferred-builtin ns={} name={} tier={}",
            e.ns.as_str(),
            e.name.as_str(),
            e.tier as u8
        );
        return Err(diag(ReasonCode::DeferredBuiltin, at, msg));
    }
    let mut best: Option<(Rank, &Overload, Bound, [Option<Bind>; 3], Qual)> = None;
    let mut tied: Vec<&Overload> = Vec::new();
    let mut fails: Vec<(&Overload, Fail, String)> = Vec::new();
    let mut removed: Vec<&Overload> = Vec::new();
    for ov in e.overloads() {
        if !ov.versions.has(d) {
            removed.push(ov);
            continue;
        }
        match try_overload(ov, args, targs, d, rd, syms) {
            Ok((r, b, v, j)) => match &best {
                Some((br, ..)) if r > *br => {}
                Some((br, ..)) if r == *br => tied.push(ov),
                _ => {
                    tied.clear();
                    best = Some((r, ov, b, v, j));
                }
            },
            Err((f, m)) => fails.push((ov, f, m)),
        }
    }
    if let Some((_, ov, b, vars, j)) = best {
        if !tied.is_empty() {
            let mut dg = diag(
                ReasonCode::AmbiguousOverload,
                at,
                format!("`{full}`: several overloads fit equally"),
            );
            dg.notes.push((at, ov.sym.as_str().to_owned()));
            for t in &tied {
                dg.notes.push((at, t.sym.as_str().to_owned()));
            }
            return Err(dg);
        }
        after_pick(e, ov, &b.slots, args, at, syms)?;
        return Ok(Picked {
            ov: ov.id,
            bound: b.slots,
            rets: results(ov, &vars, j),
        });
    }
    // pin:02-na-v6-non-bool — W13 (contract §3.17, the v6 migration guide: "the na(), nz(), and
    // fixnan() functions no longer accept 'bool' types") is the only change to `na()` between the
    // dialects. The v6 reference's argument list (pine-spec's v6 rows) omits UDTs, enums and
    // `chart.point`, a change the migration guide does not list, so it is invariant (impl-06
    // §4.2.0; C030 is `dialects 56`, I9): in v6, v5's generic row still takes every non-`bool`
    // argument no v6 row takes. A `bool` keeps step 8's `3005` (A087).
    if d == Dialect::V6
        && e.ns.as_str().is_empty()
        && e.name.as_str() == "na"
        && args.first().is_some_and(|a| a.t.ty != Ty::Bool)
    {
        for ov in removed.iter().copied() {
            if !ov.versions.has(Dialect::V5) {
                continue;
            }
            // The row binds as v5 binds it (its parameter rows are v5's); a non-`bool` argument
            // converts the same in both dialects.
            if let Ok((_, b, vars, j)) = try_overload(ov, args, targs, Dialect::V5, rd, syms) {
                after_pick(e, ov, &b.slots, args, at, syms)?;
                return Ok(Picked {
                    ov: ov.id,
                    bound: b.slots,
                    rets: results(ov, &vars, j),
                });
            }
        }
    }
    // Step 8: an overload of the other dialect that would pass is `3005`.
    let other = match d {
        Dialect::V5 => Dialect::V6,
        Dialect::V6 => Dialect::V5,
    };
    for ov in removed {
        if ov.versions.has(other) && try_overload(ov, args, targs, other, rd, syms).is_ok() {
            let msg = format!(
                "this call of `{full}` exists only in Pine v{} (not v{})",
                other as u8, d as u8
            );
            return Err(diag(ReasonCode::NotInDialect, at, msg));
        }
    }
    Err(no_survivor(full, &fails, at))
}

// pin:02-sole-candidate-type-3007 — §4.5 step 8 says `3010` when every candidate fails on types;
// §4.4.3 / §4.4.5 / §7.2 name `3007` for a conversion that does not exist (`plot(style = <may be
// na>)` in v6). Literal reading of both: one candidate failing on its types is the conversion's
// `3007`; two or more are step 8's `3010`.

/// The refusal when no candidate survives: all on binding `3006`, all that typed failing on
/// qualifiers `3008`, otherwise `3010` with a note per candidate.
fn no_survivor(full: &str, fails: &[(&Overload, Fail, String)], at: Span) -> Diag {
    let worst = fails.iter().map(|f| f.1).max();
    let (rc, head) = match worst {
        Some(Fail::Bind) | None => (ReasonCode::Arity, "no overload accepts these arguments"),
        Some(Fail::Qual) if fails.iter().all(|f| f.1 != Fail::Type) => (
            ReasonCode::QualifierTooWeak,
            "an argument's qualifier is too strong",
        ),
        _ if fails.len() == 1 => (ReasonCode::TypeMismatch, "no conversion"),
        _ => (ReasonCode::NoOverload, "no overload accepts these types"),
    };
    let mut dg = if fails.len() == 1 || rc != ReasonCode::NoOverload {
        let best = fails.iter().max_by_key(|f| f.1);
        let m = best.map_or(String::new(), |f| f.2.clone());
        diag(rc, at, format!("`{full}`: {head}: {m}"))
    } else {
        diag(rc, at, format!("`{full}`: {head}"))
    };
    if rc == ReasonCode::NoOverload {
        for (ov, _, m) in fails {
            dg.notes.push((at, format!("{}: {m}", ov.sym.as_str())));
        }
    }
    dg
}

/// One user candidate's parameter as overload resolution sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UserParam {
    /// Its name.
    pub(crate) name: Sym,
    /// The written type (`qual` = the keyword, or `Series` when none), `None` when untyped.
    pub(crate) ty: Option<TypeQ>,
    /// The written qualifier keyword.
    pub(crate) qual: Option<Qual>,
    /// Has a default.
    pub(crate) optional: bool,
}

/// A user candidate's outcome: the argument row per parameter (`None` = its default) and the
/// parameter types this call instantiates.
#[derive(Debug, Clone)]
pub(crate) struct UserPicked {
    /// Index of the chosen candidate.
    pub(crate) index: usize,
    /// `arg_of[p]`: the argument bound to parameter `p`.
    pub(crate) arg_of: Vec<Option<usize>>,
}

/// §4.5 over user declarations: a typed parameter is a concrete row, an untyped one a
/// whole-value variable admitting every kind.
pub(crate) fn user(
    name: &str,
    cands: &[Vec<UserParam>],
    args: &[Arg],
    at: Span,
    d: Dialect,
    rd: &Readings,
    syms: &SymTable,
) -> Result<UserPicked, Diag> {
    let mut best: Option<(Rank, usize, Vec<Option<usize>>)> = None;
    let mut tied = Vec::new();
    let mut fails: Vec<(usize, Fail, String)> = Vec::new();
    'cand: for (ci, ps) in cands.iter().enumerate() {
        let mut arg_of: Vec<Option<usize>> = vec![None; ps.len()];
        let mut next = 0;
        for (k, a) in args.iter().enumerate() {
            let p = match a.name {
                None => {
                    next += 1;
                    next - 1
                }
                Some(n) => match ps.iter().position(|p| p.name == n) {
                    Some(p) => p,
                    None => {
                        let m = format!("no parameter named `{}`", syms.resolve(n));
                        fails.push((ci, Fail::Bind, m));
                        continue 'cand;
                    }
                },
            };
            if p >= ps.len() {
                fails.push((
                    ci,
                    Fail::Bind,
                    format!("too many arguments ({})", args.len()),
                ));
                continue 'cand;
            }
            if arg_of[p].is_some() {
                if d == Dialect::V6 {
                    let m = format!("parameter `{}` is given twice", syms.resolve(ps[p].name));
                    fails.push((ci, Fail::Bind, m));
                    continue 'cand;
                }
                continue;
            }
            arg_of[p] = Some(k);
        }
        let mut s2 = 0;
        for (p, row) in ps.iter().enumerate() {
            if arg_of[p].is_none() {
                if !row.optional {
                    let m = format!("missing argument `{}`", syms.resolve(row.name));
                    fails.push((ci, Fail::Bind, m));
                    continue 'cand;
                }
                s2 += 1;
            }
        }
        let (mut s0, mut s1, mut s3) = (0, 0, 0);
        let mut qfail = None;
        for (p, row) in ps.iter().enumerate() {
            let Some(k) = arg_of[p] else { continue };
            let a = args[k];
            let pname = syms.resolve(row.name);
            match row.ty {
                Some(want) => match conv(a.t, want, d, rd) {
                    Some(c) => s0 += cost(c),
                    None => {
                        let m = format!(
                            "`{pname}` is {}, the argument {}",
                            ty_name(want),
                            ty_name(a.t)
                        );
                        fails.push((ci, Fail::Type, m));
                        continue 'cand;
                    }
                },
                None => {
                    if a.is_na {
                        let m = format!("`{pname}` has no type to give `na`");
                        fails.push((ci, Fail::Type, m));
                        continue 'cand;
                    }
                    s1 += 1;
                }
            }
            let q = row.qual.unwrap_or(Qual::Series);
            s3 += q as u32;
            if !admits(q, a.t.qual) && qfail.is_none() {
                qfail = Some(format!(
                    "`{pname}` accepts at most {}, the argument is {}",
                    qual_name(q),
                    qual_name(a.t.qual)
                ));
            }
        }
        if let Some(m) = qfail {
            fails.push((ci, Fail::Qual, m));
            continue;
        }
        let r = (s0, s1, s2, s3);
        match &best {
            Some((br, ..)) if r > *br => {}
            Some((br, ..)) if r == *br => tied.push(ci),
            _ => {
                tied.clear();
                best = Some((r, ci, arg_of));
            }
        }
    }
    if let Some((_, index, arg_of)) = best {
        if !tied.is_empty() {
            let mut dg = diag(
                ReasonCode::AmbiguousOverload,
                at,
                format!("`{name}`: several overloads fit equally"),
            );
            dg.notes.push((at, format!("candidate {index}")));
            for t in tied {
                dg.notes.push((at, format!("candidate {t}")));
            }
            return Err(dg);
        }
        return Ok(UserPicked { index, arg_of });
    }
    let worst = fails.iter().map(|f| f.1).max();
    let rc = match worst {
        Some(Fail::Bind) | None => ReasonCode::Arity,
        Some(Fail::Qual) if fails.iter().all(|f| f.1 != Fail::Type) => ReasonCode::QualifierTooWeak,
        _ if cands.len() == 1 => ReasonCode::TypeMismatch,
        _ => ReasonCode::NoOverload,
    };
    let first = fails
        .iter()
        .max_by_key(|f| f.1)
        .map_or(String::new(), |f| f.2.clone());
    let mut dg = diag(rc, at, format!("`{name}`: {first}"));
    if rc == ReasonCode::NoOverload {
        for (ci, _, m) in &fails {
            dg.notes.push((at, format!("candidate {ci}: {m}")));
        }
    }
    Err(dg)
}

/// `deferred-builtin ns=<ns> name=<name> tier=<n>` (§3.7) plus `extra`.
pub(crate) fn deferred(e: &Entry, extra: &str, at: Span) -> Diag {
    let msg = format!(
        "deferred-builtin ns={} name={} tier={}{extra}",
        e.ns.as_str(),
        e.name.as_str(),
        e.tier as u8
    );
    diag(ReasonCode::DeferredBuiltin, at, msg)
}

/// §4.5 step 9 (built-ins only, after the pick), then step 10.
fn after_pick(
    e: &Entry,
    ov: &Overload,
    slots: &[Option<u16>],
    args: &[Arg],
    at: Span,
    syms: &SymTable,
) -> Result<(), Diag> {
    // `served(ov) == Unreachable` (the first check of step 9) is the caller's: it refuses the
    // call without failing its typing (`unserved`), so later refusals still surface.
    if ov.sx & sx::NONSTD_CHART != 0 {
        let msg = format!("`{}` needs a non-standard chart", e.full_str());
        return Err(diag(ReasonCode::NonStandardChart, at, msg));
    }
    let ps = pine_spec::params(ov);
    let arg_of = |name: &str| {
        (0..args.len()).find(|&k| slots[k].is_some_and(|p| ps[p as usize].name.as_str() == name))
    };
    if e.full_str() == "math.random" {
        // impl-04 §1.4 item 5: no entropy source. pin:02-random-unseeded — an omitted seed
        // (default `na`) does not fold either.
        let folds = arg_of("seed").is_some_and(|k| matches!(args[k].k, Some(K::Int(_))));
        if !folds {
            return Err(deferred(e, " seed=dynamic", at));
        }
    }
    let ns = e.ns.as_str();
    if e.full_str() == "str.format" || ns == "log" {
        // pin:02-format-arg-count — "variadic arguments" are every format argument (`arg0` and
        // the variadic tail, everything after `formatString`): `__types` holds 15 types.
        let mut n = 0u32;
        for s in slots {
            if s.is_some_and(|p| ps[p as usize].name.as_str() != "formatString") {
                n += 1;
            }
        }
        if n > MAX_FORMAT_ARGS {
            let msg = format!(
                "`{}` takes at most {MAX_FORMAT_ARGS} format arguments, not {n}",
                e.full_str()
            );
            return Err(Diag {
                code: pine_std::code::BAD_FORMAT,
                ..diag(ReasonCode::Internal, at, msg)
            });
        }
    }
    load_checks(e, ps, slots, args, syms)
}

/// The parameter names §4.5 step 10 checks at load (pine-spec's spelling).
pub(crate) const LOAD_CHECKED: [&str; 5] =
    ["timeframe", "session", "timezone", "regex", "formatString"];

/// Entries whose `timeframe` / `session` strings another pass checks: `request.security`'s
/// target (§4.11.1) and the inputs' values in effect (§4.12.3).
pub(crate) const LOAD_CHECKED_ELSEWHERE: [&str; 4] = [
    "request.security",
    "request.security_lower_tf",
    "input.timeframe",
    "input.session",
];

/// §4.5 step 10: a `const` string bound to a load-checked `string` parameter goes through its
/// pine-std service once; an `Err` is a `Diag` (§3.7), nothing is kept.
fn load_checks(
    e: &Entry,
    ps: &[pine_spec::Param],
    slots: &[Option<u16>],
    args: &[Arg],
    syms: &SymTable,
) -> Result<(), Diag> {
    if LOAD_CHECKED_ELSEWHERE.contains(&e.full_str()) {
        return Ok(());
    }
    for (k, a) in args.iter().enumerate() {
        let Some(p) = slots[k] else { continue };
        let row = ps[p as usize];
        if row.sig.ty.ty != Ty::String || !LOAD_CHECKED.contains(&row.name.as_str()) {
            continue;
        }
        let Some(K::Str(s)) = a.k else { continue };
        let text = syms.resolve_bytes(s);
        let r: Result<(), pine_std::LoadErr> = match row.name.as_str() {
            "timeframe" => pine_std::tf_seconds(text).and_then(|secs| {
                let on_clock = matches!(e.full_str(), "time" | "time_close" | "timeframe.change");
                match secs {
                    Some(sec) if on_clock => pine_std::tf_on_bar_clock(sec).map(|_| ()),
                    _ => Ok(()),
                }
            }),
            "session" => pine_std::session_parse(text).map(|_| ()),
            "timezone" => match pine_std::tz_lookup(text) {
                Some(_) => Ok(()),
                None => Err(pine_std::LoadErr {
                    code: pine_std::code::UNKNOWN_TZ,
                    at: 0,
                    why: "unknown time zone",
                }),
            },
            "regex" => pine_std::regex_compile(text).map(|_| ()),
            "formatString" => pine_std::fmt_compile(text).map(|_| ()),
            _ => Ok(()),
        };
        if let Err(le) = r {
            let mut span = a.span;
            if a.plain_literal {
                let lo = span.lo.saturating_add(1).saturating_add(le.at);
                if lo < span.hi.saturating_sub(1) {
                    span.lo = lo;
                    span.hi = lo + 1;
                }
            }
            return Err(Diag {
                code: le.code,
                ..diag(ReasonCode::Internal, span, le.why.to_owned())
            });
        }
    }
    Ok(())
}

/// §4.5 step 9's first check: a row pine-std has no body for is `3024`.
pub(crate) fn unserved(ov: OverloadId, at: Span) -> Option<Diag> {
    if pine_std::served(ov) != pine_std::Served::Unreachable {
        return None;
    }
    let e = pine_spec::entry_of(pine_spec::overload(ov));
    Some(deferred(e, " unserved=1", at))
}

/// The UDT field rows (R-P5, Pine 2026: `array.sort(id, order, sort_field)` and `sort_indices`
/// order an `array<UDT>` by one field of its elements; v6 only). R-P16 (CI-052, F108): the three
/// `binary_search*` field rows are gone — see [`BSEARCH_NAMES`].
const UDT_SORT_ROWS: [OverloadId; 2] = [
    pine_spec::ov::ARRAY_SORT_AT,
    pine_spec::ov::ARRAY_SORT_INDICES_AT,
];

/// `ov` is a UDT field row (`sort` / `sort_indices`).
pub(crate) fn udt_field_row(ov: OverloadId) -> bool {
    UDT_SORT_ROWS.contains(&ov)
}

/// The searches that take no field argument (R-P16).
///
/// `pin:02-bsearch-field-refused` — R-P16 (CI-052, operator 2026-10-17): TradingView refuses at
/// compile `array.binary_search(arr_of_udt, 777, "k_F108")` (and `_leftmost` / `_rightmost`
/// with a field argument): "The `Ob_F108` user-defined type does not include a field named
/// "#loc_42"" (F108). pine-spec has no field row for them, and pine-sema refuses such a call
/// `3007` (`TypeMismatch`, the class of TradingView's compile-time type refusals) before
/// overload resolution: an `array<UDT>` `id` with a third argument or a `sort_field` named one.
/// TradingView's text names an internal symbol (`#loc_42`), so the message states the refusal
/// and quotes its shape with the type's name. v6 is captured; v5 (no field rows ever) takes the
/// same refusal (literal reading: the function has no field parameter in either dialect).
pub(crate) const BSEARCH_NAMES: [&str; 3] = [
    "array.binary_search",
    "array.binary_search_leftmost",
    "array.binary_search_rightmost",
];

/// The row's own `sort_field` default (pine-spec `{"k": "int", "v": 0}`), as a folded value.
pub(crate) fn sort_field_default(o: &Overload) -> Option<K> {
    let p = pine_spec::params(o)
        .iter()
        .find(|p| p.name.as_str() == "sort_field")?;
    match p.default.kind {
        pine_spec::DefaultKind::Int => Some(K::Int(p.default.bits as i64)),
        _ => None,
    }
}

/// The selector a UDT field row passes to pine-std for its `sort_field` argument `sel` (the
/// folded `const int` index or `const string` name; `None` when it does not fold), over the
/// element type's fields `(name, type)` in declaration order.
///
/// `pin:02-udt-field-selector` — the reference: "An "int" `sort_field` argument specifies a
/// field by its index, where 0 refers to the first field in the type declaration. A "string"
/// argument specifies a field by its name"; `sort` / `sort_indices` take "any field of the type
/// "int", "float", or "string"".
/// Folded here to one `int` constant, `index | (field Ty as i64) << 32` — the index in bits
/// 0..32, the field's `pine_ir::Ty` byte in bits 32..40 (`Int` 1, `Float` 2, `String` 5), bits
/// 40..64 zero — which pine-std's `coll::udt_selector` decodes (its `Internal` on any other
/// shape). The reference is silent on a bad selector; the literal reading refuses at compile,
/// all as `3007`: a selector that does not fold, a negative or out-of-range index, a name no
/// field has, and a field of any other type (`bool`, `color`, an enum, a UDT, a collection).
pub(crate) fn udt_selector(
    fields: &[(Sym, TypeQ)],
    sel: Option<K>,
    syms: &SymTable,
) -> Result<i64, String> {
    let i = match sel {
        Some(K::Int(i)) if i >= 0 && (i as u64) < fields.len() as u64 => i as usize,
        Some(K::Int(i)) => {
            return Err(format!(
                "`sort_field` {i} names no field (the type has {})",
                fields.len()
            ))
        }
        Some(K::Str(s)) => {
            let want = syms.resolve_bytes(s);
            match fields.iter().position(|f| syms.resolve_bytes(f.0) == want) {
                Some(i) => i,
                None => {
                    let name = String::from_utf8_lossy(want);
                    return Err(format!(
                        "`sort_field` \"{name}\" names no field of the type"
                    ));
                }
            }
        }
        _ => return Err("`sort_field` is a constant int index or string name".to_owned()),
    };
    let ft = fields[i].1.ty;
    if !matches!(ft, Ty::Int | Ty::Float | Ty::String) {
        return Err(format!(
            "`sort_field` names field {i} of type {}; this function takes an int, float or \
             string field",
            ty_name(fields[i].1)
        ));
    }
    Ok(i as i64 | (ft as i64) << 32)
}

/// Where an omitted parameter's value comes from (A4.1: pine-sema resolves every `Derived`
/// default at the call site; pass 4 lowers it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultSrc {
    /// A literal default (`int`, `float`, `bool`, `color`, `string`, a built-in value, `na`).
    Literal(pine_spec::DefaultVal),
    /// `param:<name>`: the argument bound to that row (`Some(k)`), else that row's own default.
    Param {
        /// The referenced parameter row.
        row: u16,
        /// The argument bound to it at this call.
        arg: Option<u16>,
    },
    /// `decl_format`: the declaration's `format` (§4.13.1).
    DeclFormat,
    /// `decl_precision`: the declaration's `precision` (§4.13.1).
    DeclPrecision,
    /// `sym_precision`: [`sym_precision`] of the chart's mintick.
    SymPrecision,
    /// `chart_bar_count`: the number of chart bars.
    ChartBarCount,
    /// `timeframe_change`: as if `timeframe.change()` were passed.
    TimeframeChange,
    /// `source_length`: the length of parameter 0.
    SourceLength,
    /// `no_options`: no restriction.
    NoOptions,
}

/// The source of row `row`'s default at a call whose arguments bind `slots`.
pub(crate) fn default_src(ov: &Overload, row: u16, slots: &[Option<u16>]) -> DefaultSrc {
    use pine_spec::{derived, DefaultKind};
    let dv = pine_spec::params(ov)[row as usize].default;
    if dv.kind != DefaultKind::Derived {
        return DefaultSrc::Literal(dv);
    }
    match derived::code(dv.bits) {
        derived::DECL_FORMAT => DefaultSrc::DeclFormat,
        derived::DECL_PRECISION => DefaultSrc::DeclPrecision,
        derived::PARAM => {
            let r = u16::from(derived::param(dv.bits));
            let arg = slots.iter().position(|&s| s == Some(r)).map(|k| k as u16);
            DefaultSrc::Param { row: r, arg }
        }
        derived::SYM_PRECISION => DefaultSrc::SymPrecision,
        derived::CHART_BAR_COUNT => DefaultSrc::ChartBarCount,
        derived::TIMEFRAME_CHANGE => DefaultSrc::TimeframeChange,
        derived::SOURCE_LENGTH => DefaultSrc::SourceLength,
        // R9 refuses every other code in pine-spec; `no_options` is the last.
        _ => DefaultSrc::NoOptions,
    }
}

/// `sym_precision` (A4.1): the decimals of the chart's mintick, "inherited from the symbol's
/// mintick" — the fewest decimals that write `mintick_1e6 / 1e6` exactly (0..=6).
pub(crate) const fn sym_precision(mintick_1e6: i64) -> i64 {
    let mut p = 0;
    let mut unit = 1_000_000i64;
    while p < 6 && mintick_1e6 % unit != 0 {
        p += 1;
        unit /= 10;
    }
    p
}
