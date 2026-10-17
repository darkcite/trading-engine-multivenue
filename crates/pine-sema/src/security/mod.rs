// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! impl-02 §4.11.1 (T21, pass 3): `request.security` recognition and its targets and
//! parameters. A call is a `request.security` when its overload is `sx::REQUEST` with a
//! `TUPLE_OK` `expression` parameter. Every argument that decides the sub-program must fold
//! (`3027`); the symbol is the chart's (`""`, `syminfo.tickerid`, the chart key) or a
//! `ticker_map` key (`3042`); the timeframe parses and sits on the bar clock (`5001`); the
//! currency is no conversion (`3053`); `calc_bars_count` keeps its default (`3024`); the outputs
//! are `int`/`float`/`bool`/`color`/enum (`string` → `3044`, a reference → `3024`); every
//! request's elements together stay within `MAX_REQUEST_ELEMENTS` (`3032`). The cone, the child
//! IR and the keys are `cone.rs` (T22); leak-freedom is `leak.rs` (T23).

use pine_ir::{is_canonical_symbol_key, CompileOpts, Diag, NodeId, Span, SymTable, Ty, TypeQ};
use pine_lang::{Ast, Node};
use pine_spec::pf;

use crate::code::{diag, ReasonCode};
use crate::infer::{Callee, Inst, Typed, K};
use crate::limits::MAX_REQUEST_ELEMENTS;

pub(crate) mod cone;
pub(crate) mod leak;

/// Whether overload `ov` is `request.security` (§4.11).
pub(crate) fn is_security(ov: pine_ir::OverloadId) -> bool {
    let o = pine_spec::overload(ov);
    o.sx & pine_spec::sx::REQUEST != 0
        && pine_spec::params(o)
            .iter()
            .any(|p| p.name.as_str() == "expression" && p.flags & pf::TUPLE_OK != 0)
}

/// One bound argument of a request.
struct ArgOf {
    name: &'static str,
    node: NodeId,
    k: Option<K>,
    ty: Option<TypeQ>,
    span: Span,
    default: pine_spec::DefaultVal,
}

/// Pass 3's `request.security` checks over every TypeInst (§4.11.1).
pub(crate) fn check(
    typed: &Typed,
    files: &[Option<&Ast>],
    opts: &CompileOpts,
    syms: &SymTable,
    diags: &mut Vec<Diag>,
) {
    let mut elements = 0usize;
    let mut first_over: Option<Span> = None;
    for inst in &typed.insts {
        let Some(ast) = files.get(inst.file).copied().flatten() else {
            continue;
        };
        for &(n, callee) in &inst.calls {
            let Callee::Builtin(ov) = callee else {
                continue;
            };
            if ov == pine_spec::ov::REQUEST_CURRENCY_RATE {
                currency_rate(ast, inst, n, ov, syms, diags);
                continue;
            }
            if !is_security(ov) {
                continue;
            }
            let args = bound_args(ast, inst, n, ov);
            let k = one(ast, inst, &args, opts, syms, diags);
            // The root's top level and each function TypeInst are counted once each: a request
            // in a function called twice is two requests (two call sites, §4.6).
            elements += k;
            if elements > MAX_REQUEST_ELEMENTS && first_over.is_none() {
                first_over = Some(ast.span(n));
            }
        }
    }
    if let Some(at) = first_over {
        let msg = format!(
            "request.security calls return {elements} elements together; at most \
             {MAX_REQUEST_ELEMENTS}"
        );
        diags.push(diag(ReasonCode::SecurityTupleArity, at, msg));
    }
}

fn bound_args(ast: &Ast, inst: &Inst, n: NodeId, ov: pine_ir::OverloadId) -> Vec<ArgOf> {
    let ps = pine_spec::params(pine_spec::overload(ov));
    let given = inst.bound_args(ast, n);
    let mut out = Vec::with_capacity(given.len());
    for (r, v) in given {
        let Some(r) = r else {
            continue;
        };
        out.push(ArgOf {
            name: ps[usize::from(r)].name.as_str(),
            node: v,
            k: inst.k.get(v.0 as usize).copied().flatten(),
            ty: inst.ty.get(v.0 as usize).copied().flatten(),
            span: ast.span(v),
            default: ps[usize::from(r)].default,
        });
    }
    out
}

/// The dotted path of `n` (`syminfo.tickerid`), if it is one.
fn path(ast: &Ast, syms: &SymTable, n: NodeId) -> Option<String> {
    match ast.node(n) {
        Node::Ident { sym } => Some(syms.resolve(sym).to_owned()),
        Node::Field { base, member } => {
            let b = path(ast, syms, base)?;
            Some(format!("{b}.{}", syms.resolve(ast.sym(member))))
        }
        _ => None,
    }
}

/// `request.currency_rate(from, to)` with two constant, byte-different currencies is `3053`
/// (impl-02 §3.2 row 53, CAR-02-1: the engine has no FX source; impl-06 D062). Anything else is
/// an ordinary call (§4.11): pine-std's `1.0` for byte-equal strings, else `na` (impl-04 §4.14).
fn currency_rate(
    ast: &Ast,
    inst: &Inst,
    n: NodeId,
    ov: pine_ir::OverloadId,
    syms: &SymTable,
    diags: &mut Vec<Diag>,
) {
    let args = bound_args(ast, inst, n, ov);
    let text = |name: &str| {
        args.iter()
            .find(|a| a.name == name)
            .and_then(|a| match a.k {
                Some(K::Str(s)) => Some(syms.resolve(s)),
                _ => None,
            })
    };
    if let (Some(from), Some(to)) = (text("from"), text("to")) {
        if from != to {
            let msg = format!(
                "request.currency_rate(\"{from}\", \"{to}\"): no currency conversion source"
            );
            diags.push(diag(ReasonCode::CurrencyConversion, ast.span(n), msg));
        }
    }
}

fn dynamic(a: &ArgOf, diags: &mut Vec<Diag>) {
    let msg = format!(
        "request.security's `{}` must be a constant: a sub-program needs a static target",
        a.name
    );
    diags.push(diag(ReasonCode::DynamicSecurityTarget, a.span, msg));
}

/// One request's checks; returns its element count.
fn one(
    ast: &Ast,
    inst: &Inst,
    args: &[ArgOf],
    opts: &CompileOpts,
    syms: &SymTable,
    diags: &mut Vec<Diag>,
) -> usize {
    let mut k = 1usize;
    for a in args {
        match a.name {
            "symbol" => symbol(ast, a, opts, syms, diags),
            "timeframe" => timeframe(ast, a, syms, diags),
            "gaps" | "lookahead" | "ignore_invalid_symbol" => {
                if a.k.is_none() {
                    dynamic(a, diags);
                }
            }
            "currency" => match a.k {
                None => dynamic(a, diags),
                Some(K::Str(s)) => {
                    let t = syms.resolve(s);
                    // `currency.NONE`'s value is the empty string in pine-spec's row
                    // (pin:02-currency-none-text — the row's literal reading).
                    if !(t.is_empty() || t == "NONE" || t == opts.syminfo_strs[2]) {
                        let msg = format!(
                            "request.security(currency = \"{t}\"): no currency conversion source"
                        );
                        diags.push(diag(ReasonCode::CurrencyConversion, a.span, msg));
                    }
                }
                Some(_) => {}
            },
            "calc_bars_count" => {
                let default = match (a.default.kind, a.k) {
                    (pine_spec::DefaultKind::Na, Some(K::Na)) => true,
                    (pine_spec::DefaultKind::Int, Some(K::Int(v))) => v as u64 == a.default.bits,
                    _ => false,
                };
                if !default {
                    let msg = "request.security(calc_bars_count = …) is not served".to_owned();
                    diags.push(diag(ReasonCode::DeferredBuiltin, a.span, msg));
                }
            }
            "expression" => k = expression(ast, inst, a, diags),
            _ => {}
        }
    }
    k
}

fn symbol(ast: &Ast, a: &ArgOf, opts: &CompileOpts, syms: &SymTable, diags: &mut Vec<Diag>) {
    let text = match a.k {
        Some(K::Str(s)) => syms.resolve(s).to_owned(),
        _ => match path(ast, syms, a.node).as_deref() {
            Some("syminfo.tickerid") => return,
            _ => return dynamic(a, diags),
        },
    };
    if text.is_empty() || text == opts.symbol_key {
        return;
    }
    let mapped = opts
        .ticker_map
        .iter()
        .find(|m| m.0 == text)
        .map(|m| m.1.as_str());
    match mapped {
        Some(key) if is_canonical_symbol_key(key.as_bytes()) => {}
        _ => {
            let msg = format!("`{text}` is not a mapped ticker (ticker_map)");
            diags.push(diag(ReasonCode::UnmappedTicker, a.span, msg));
        }
    }
}

fn timeframe(ast: &Ast, a: &ArgOf, syms: &SymTable, diags: &mut Vec<Diag>) {
    let text = match a.k {
        Some(K::Str(s)) => syms.resolve_bytes(s).to_vec(),
        _ => match path(ast, syms, a.node).as_deref() {
            Some("timeframe.period") => return,
            _ => return dynamic(a, diags),
        },
    };
    let r = pine_std::tf_seconds(&text).and_then(|s| match s {
        Some(sec) => pine_std::tf_on_bar_clock(sec).map(|_| ()),
        None => Ok(()),
    });
    if let Err(le) = r {
        diags.push(Diag {
            code: le.code,
            ..diag(ReasonCode::Internal, a.span, le.why.to_owned())
        });
    }
}

/// The element count, with `3044` / `3024` on an element type the plots cannot carry.
fn expression(ast: &Ast, inst: &Inst, a: &ArgOf, diags: &mut Vec<Diag>) -> usize {
    let items: Vec<(Option<TypeQ>, Span)> = match ast.node(a.node) {
        Node::Tuple { items } => items
            .iter()
            .map(|&i| (inst.ty.get(i.0 as usize).copied().flatten(), ast.span(i)))
            .collect(),
        _ => vec![(a.ty, a.span)],
    };
    for &(t, at) in &items {
        let Some(t) = t else { continue };
        match t.ty {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Color | Ty::Enum | Ty::Void => {}
            Ty::String => {
                let msg = "request.security cannot return a string".to_owned();
                diags.push(diag(ReasonCode::SecurityStringOut, at, msg));
            }
            _ => {
                let msg = "request.security cannot return a collection, an object or a \
                           drawing (its outputs travel as f64 plots)"
                    .to_owned();
                diags.push(diag(ReasonCode::DeferredBuiltin, at, msg));
            }
        }
    }
    items.len()
}

/// §4.11.1's `expression` row at overload resolution: pine-spec's `expression` signature admits
/// only the plottable types, so a `string` (`3044`) or a reference / drawing (`3024`) output
/// would read as a bare `3007`; the request's own codes replace it.
pub(crate) fn expression_refusal(
    full: &str,
    argv: &[crate::overload::Arg],
    syms: &SymTable,
) -> Option<Diag> {
    if full != "request.security" {
        return None;
    }
    let e = argv
        .iter()
        .find(|a| a.name.is_some_and(|n| syms.resolve(n) == "expression"))
        .or_else(|| argv.iter().filter(|a| a.name.is_none()).nth(2))?;
    let t = if e.tuple { e.tuple_elem } else { e.t };
    match t.ty {
        Ty::String => Some(diag(
            ReasonCode::SecurityStringOut,
            e.span,
            "request.security cannot return a string".to_owned(),
        )),
        Ty::Array | Ty::Map | Ty::Matrix | Ty::Udt | Ty::Plot | Ty::Hline => Some(diag(
            ReasonCode::DeferredBuiltin,
            e.span,
            "request.security cannot return a collection, an object or a drawing (its outputs \
             travel as f64 plots)"
                .to_owned(),
        )),
        _ => None,
    }
}
