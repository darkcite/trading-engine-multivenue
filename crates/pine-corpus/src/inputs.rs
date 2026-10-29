// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! impl-09 §4.8.4: `pine_ir::InputDesc` from literals. Positional arguments are named from the
//! one overload the call's arity and literal shapes admit; when several are admitted, a position
//! is named only where every admitted overload names it alike (so `input.int(14)` decodes its
//! `defval`, and `input.int(14, "t", x)` leaves `x` unnamed between `minval` and `options`).
//! The generic `input(defval, …)` takes its overload from the kind of its `defval` literal.

use pine_ir::{input_flag, InputDesc, InputKind, NodeId, Span, Sym, Ty};

use crate::ast_view::Node;
use crate::canon::{fold, split_args, Lit};
use crate::scan::{Callee, Ctx};
use crate::spec_view::{SOv, SParam};

/// `CorpusInput::decoded` bits: which `desc` fields were read from literals.
pub mod decoded {
    /// `desc.kind` and `desc.ty` are established.
    pub const KIND: u8 = 1 << 0;
    /// `desc.default_bits` holds the literal `defval`.
    pub const DEFAULT: u8 = 1 << 1;
    /// `desc.name` is the variable the call initialises (else the title, else `Sym::NONE`).
    pub const NAME: u8 = 1 << 2;
}

/// `CorpusInput::present` bits: arguments the call carries, whether or not they were decoded.
pub mod present {
    /// `defval`.
    pub const DEFVAL: u8 = 1 << 0;
    /// `minval`.
    pub const MINVAL: u8 = 1 << 1;
    /// `maxval`.
    pub const MAXVAL: u8 = 1 << 2;
    /// `step`.
    pub const STEP: u8 = 1 << 3;
    /// `options`.
    pub const OPTIONS: u8 = 1 << 4;
    /// `title`.
    pub const TITLE: u8 = 1 << 5;
}

/// One `input*()` call site, as the corpus can see it without `pine-sema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CorpusInput {
    /// Contract §3.8 `InputDesc`, filled from literals only (§4.8.4). Its `Sym`s index the
    /// `SymTable` of the `Scan` that produced it. `input_flag::OVERRIDDEN` is never set.
    pub desc: InputDesc,
    /// `decoded::*`.
    pub decoded: u8,
    /// `present::*`.
    pub present: u8,
}

/// Free numeric axes: `kind ∈ {Int, Float}`, `KIND` decoded, neither `DEAD` nor `HAS_OPTIONS`.
/// This is the corpus's forecast of PVM-10's plateau dimension; PVM-10 recounts from sema's
/// `ProgramMeta::inputs`.
#[must_use]
pub fn k_numeric(inputs: &[CorpusInput]) -> u32 {
    inputs
        .iter()
        .filter(|i| {
            matches!(i.desc.kind, InputKind::Int | InputKind::Float)
                && i.decoded & decoded::KIND != 0
                && i.desc.flags & (input_flag::DEAD | input_flag::HAS_OPTIONS) == 0
        })
        .count() as u32
}

/// The literal shape of an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Int,
    Float,
    Bool,
    Str,
    Color,
    Tuple,
    /// `na`, a name, any other expression: binds anywhere.
    Any,
}

fn shape(cx: &Ctx<'_>, id: NodeId) -> Shape {
    match cx.ast.node(id) {
        Node::BoolLit { .. } => return Shape::Bool,
        Node::ColorLit { .. } => return Shape::Color,
        Node::Tuple { .. } => return Shape::Tuple,
        _ => {}
    }
    match fold(cx, id) {
        Some(Lit::Int(_)) => Shape::Int,
        Some(Lit::Float(_)) => Shape::Float,
        Some(Lit::Str(_)) => Shape::Str,
        None => Shape::Any,
    }
}

fn accepts(p: SParam, s: Shape) -> bool {
    if s == Shape::Any {
        return true;
    }
    if s == Shape::Tuple {
        return p.takes_tuple();
    }
    if p.takes_tuple() {
        return false;
    }
    if p.generic() {
        return true;
    }
    matches!(
        (s, p.ty()),
        (Shape::Int, Ty::Int | Ty::Float)
            | (Shape::Float, Ty::Float)
            | (Shape::Bool, Ty::Bool)
            | (Shape::Str, Ty::String)
            | (Shape::Color, Ty::Color)
    )
}

fn admits(cx: &Ctx<'_>, o: SOv, pos: &[NodeId], named: &[(String, NodeId)]) -> bool {
    let d = cx.dialect;
    let ps = o.positions(d);
    if ps.len() < pos.len() && !ps.iter().any(|p| p.variadic()) {
        return false;
    }
    for (i, &a) in pos.iter().enumerate() {
        if let Some(p) = ps.get(i) {
            if !accepts(*p, shape(cx, a)) {
                return false;
            }
        }
    }
    for (n, v) in named {
        match o.param(n, d) {
            Some(p) if accepts(p, shape(cx, *v)) => {}
            _ => return false,
        }
    }
    true
}

/// The argument named `n` after naming the positional ones.
fn arg<'a>(
    args: &'a [(Option<&'static str>, NodeId)],
    named: &'a [(String, NodeId)],
    n: &str,
) -> Option<NodeId> {
    args.iter()
        .find(|(k, _)| *k == Some(n))
        .map(|(_, v)| *v)
        .or_else(|| named.iter().find(|(k, _)| k == n).map(|(_, v)| *v))
}

fn encode(cx: &Ctx<'_>, kind: InputKind, id: NodeId) -> Option<u64> {
    let node = cx.ast.node(id);
    match kind {
        InputKind::Bool => match node {
            Node::BoolLit { value } => Some(u64::from(value)),
            _ => None,
        },
        InputKind::String
        | InputKind::TextArea
        | InputKind::Timeframe
        | InputKind::Session
        | InputKind::Symbol => match node {
            Node::StrLit { value, .. } => Some(u64::from(value.0)),
            _ => None,
        },
        InputKind::Color | InputKind::Source | InputKind::Enum => None,
        InputKind::Int | InputKind::Time => match fold(cx, id) {
            Some(Lit::Int(x)) => Some(x as u64),
            _ => None,
        },
        InputKind::Float | InputKind::Price => match fold(cx, id) {
            Some(Lit::Int(x)) => Some((x as f64).to_bits()),
            Some(Lit::Float(x)) => Some(x.to_bits()),
            _ => None,
        },
    }
}

fn bound_encode(cx: &Ctx<'_>, ty: Ty, id: NodeId) -> Option<u64> {
    match (ty, fold(cx, id)) {
        (Ty::Int, Some(Lit::Int(x))) => Some(x as u64),
        (Ty::Float, Some(Lit::Int(x))) => Some((x as f64).to_bits()),
        (Ty::Float, Some(Lit::Float(x))) => Some(x.to_bits()),
        _ => None,
    }
}

/// Decode one input call (§4.8.4). `var` is the declaring `Name` when the call is the whole
/// initialiser of a `VarDecl`.
pub(crate) fn decode(cx: &Ctx<'_>, call: NodeId, var: Option<NodeId>) -> CorpusInput {
    let mut ci = CorpusInput::default();
    let Node::Call { callee, args, .. } = cx.ast.node(call) else {
        return ci;
    };
    let Callee::Spec { entry, recv } = cx.classify(callee) else {
        return ci;
    };
    let d = cx.dialect;
    let (mut pos, named) = split_args(cx, args);
    if let Some(r) = recv {
        pos.insert(0, r);
    }
    let cands = entry.overloads_in(d);
    let generic = entry.ns().is_empty() && entry.name() == "input";
    // The defval node, for the generic input's overload choice.
    let defval0 = named
        .iter()
        .find(|(n, _)| n == "defval")
        .map(|(_, v)| *v)
        .or_else(|| pos.first().copied());
    let mut kind: Option<(InputKind, Ty)> = None;
    let mut chosen: Option<SOv> = None;
    if generic {
        if let Some(dv) = defval0 {
            let src_like = cx.chain(dv).is_some_and(|(r, _)| {
                cx.bind.resolved(r).is_none()
                    && cx
                        .spec_hit(&cx.chain_text(dv))
                        .is_some_and(|e| e.has_series_float_value(d))
            });
            if src_like {
                kind = Some((InputKind::Source, Ty::Float));
                chosen = cands.iter().copied().find(|o| o.input().is_some());
            } else {
                let want = match shape(cx, dv) {
                    Shape::Int => Some(Ty::Int),
                    Shape::Float => Some(Ty::Float),
                    Shape::Bool => Some(Ty::Bool),
                    Shape::Str => Some(Ty::String),
                    Shape::Color => Some(Ty::Color),
                    _ => None,
                };
                if let Some(t) = want {
                    chosen = cands
                        .iter()
                        .copied()
                        .find(|o| o.param("defval", d).is_some_and(|p| p.ty() == t));
                    if let Some(o) = chosen {
                        if let Some(k) = o.input() {
                            kind = Some((k, o.ret_ty()));
                        }
                    }
                }
            }
        }
    } else {
        let mut agree: Option<(InputKind, Ty)> = None;
        let mut ok = !cands.is_empty();
        for o in &cands {
            match (o.input(), agree) {
                (Some(k), None) => agree = Some((k, o.ret_ty())),
                (Some(k), Some((a, t))) if a == k && t == o.ret_ty() => {}
                _ => ok = false,
            }
        }
        if ok {
            kind = agree;
        }
    }
    // Name the positional arguments.
    let names: Vec<Option<&'static str>> = match chosen {
        Some(o) => {
            let ps = o.positions(d);
            (0..pos.len())
                .map(|i| ps.get(i).map(|p| p.name()))
                .collect()
        }
        None => {
            let admitted: Vec<SOv> = cands
                .iter()
                .copied()
                .filter(|o| admits(cx, *o, &pos, &named))
                .collect();
            let pool = if admitted.is_empty() {
                &cands
            } else {
                &admitted
            };
            crate::canon::agreed_names(pool, d, pos.len())
        }
    };
    let pargs: Vec<(Option<&'static str>, NodeId)> =
        names.iter().copied().zip(pos.iter().copied()).collect();
    let get = |n: &str| arg(&pargs, &named, n);

    let (k, ty) = match kind {
        Some(kt) => {
            ci.decoded |= decoded::KIND;
            kt
        }
        None => (InputKind::Int, Ty::Void),
    };
    ci.desc.kind = k;
    ci.desc.ty = ty;
    if let Some(v) = get("defval") {
        ci.present |= present::DEFVAL;
        if ci.decoded & decoded::KIND != 0 {
            if let Some(b) = encode(cx, k, v) {
                ci.desc.default_bits = b;
                ci.decoded |= decoded::DEFAULT;
            }
        }
    }
    for (n, bit, flag) in [
        ("minval", present::MINVAL, input_flag::HAS_MIN),
        ("maxval", present::MAXVAL, input_flag::HAS_MAX),
        ("step", present::STEP, input_flag::HAS_STEP),
    ] {
        if let Some(v) = get(n) {
            ci.present |= bit;
            if ci.decoded & decoded::KIND != 0 {
                if let Some(b) = bound_encode(cx, ty, v) {
                    match n {
                        "minval" => ci.desc.min_bits = b,
                        "maxval" => ci.desc.max_bits = b,
                        _ => ci.desc.step_bits = b,
                    }
                    ci.desc.flags |= flag;
                }
            }
        }
    }
    if get("options").is_some() {
        ci.present |= present::OPTIONS;
        ci.desc.flags |= input_flag::HAS_OPTIONS;
    }
    if let Some(t) = get("title") {
        ci.present |= present::TITLE;
        if let Node::StrLit { value, .. } = cx.ast.node(t) {
            ci.desc.title = value;
        }
    }
    match var.and_then(|n| match cx.ast.node(n) {
        Node::Name { sym } => Some((n, sym)),
        _ => None,
    }) {
        Some((n, sym)) => {
            ci.desc.name = sym;
            ci.decoded |= decoded::NAME;
            let dead = cx
                .bind
                .decl_idx_at(n)
                .is_some_and(|i| cx.bind.decls[i as usize].reads == 0);
            if dead {
                ci.desc.flags |= input_flag::DEAD;
            }
        }
        None => {
            ci.desc.name = if ci.desc.title != Sym::NONE {
                ci.desc.title
            } else {
                Sym::NONE
            }
        }
    }
    let sp = cx.ast.span(call);
    ci.desc.span = Span::new(pine_ir::FileId::ROOT, sp.lo, sp.hi);
    ci
}
