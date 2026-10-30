// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! §4.5's picks on pass 3's output (impl-02 §7.3's private twins), the load-checked parameter
//! census and the `Derived` defaults (A4.1).

use pine_ir::{Dialect, FileId, NodeId, SymTable, Ty};
use pine_lang::Node;
use pine_spec::{ov, pf, DefaultKind};

use crate::infer::{Callee, Typed};
use crate::overload::{
    default_src, sym_precision, DefaultSrc, LOAD_CHECKED, LOAD_CHECKED_ELSEWHERE,
};
use crate::sw::Readings;
use crate::{imports, infer, resolve, LibCache};

fn typed(d: Dialect, body: &str) -> (pine_lang::Ast, Typed) {
    let src = format!("//@version={}\nindicator(\"t\")\n{body}", d as u8);
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(&src, FileId::ROOT, &mut syms).expect("parses");
    let rd = Readings::of(d);
    let mut diags = Vec::new();
    let graph = imports::load(&ast, &mut syms, &mut LibCache::empty(), &mut diags);
    let res = resolve::resolve(&ast, &graph, &syms, &rd, &mut diags);
    let t = infer::infer(&ast, &graph, &res, &mut syms, &rd, 0b0111, &[], &mut diags);
    diags.retain(|d| !(d.code.0 == 3024 && d.msg.contains(" unserved=1")));
    assert!(diags.is_empty(), "{diags:?}");
    (ast, t)
}

/// The built-in overloads the root's top level called, in call order.
fn builtins(t: &Typed) -> Vec<(NodeId, pine_ir::OverloadId)> {
    t.insts[t.root]
        .calls
        .iter()
        .filter_map(|&(n, c)| match c {
            Callee::Builtin(o) => Some((n, o)),
            _ => None,
        })
        .collect()
}

#[test]
fn overload_widening_costs_more_than_exact() {
    let (_, t) = typed(Dialect::V6, "x = nz(1)\ny = nz(1.5)\n");
    let b = builtins(&t);
    assert!(b.iter().any(|x| x.1 == ov::NZ_I));
    assert!(b.iter().any(|x| x.1 == ov::NZ_F));
    assert_eq!(b.iter().filter(|x| x.1 == ov::NZ_F).count(), 1);
}

#[test]
fn overload_defaulted_param_loses_to_explicit_signature() {
    let (_, t) = typed(Dialect::V5, "x = time(\"D\")\n");
    assert!(builtins(&t).iter().any(|x| x.1 == ov::TIME_I));
}

#[test]
fn overload_v5_repeated_param_uses_the_first() {
    let (_, t) = typed(Dialect::V5, "x = ta.sma(close, length = 5, length = 6)\n");
    let root = &t.insts[t.root];
    let (_, bound) = root
        .bound
        .iter()
        .find(|b| {
            root.calls
                .iter()
                .any(|c| c.0 == b.0 && c.1 == Callee::Builtin(ov::TA_SMA))
        })
        .expect("ta.sma bound");
    assert_eq!(bound.len(), 3);
    assert!(bound[1].is_some(), "the first `length` binds");
    assert_eq!(bound[2], None, "the repeat is dropped (MG-18)");
}

#[test]
fn overload_user_method_overrides_builtin_method() {
    let (_, t) = typed(
        Dialect::V6,
        "method push(array<float> a, float x) => x\narr = array.new<float>(2)\nv = arr.push(1.0)\n",
    );
    let root = &t.insts[t.root];
    assert!(root.calls.iter().any(|c| matches!(c.1, Callee::User(_))));
    assert!(!root
        .calls
        .iter()
        .any(|c| c.1 == Callee::Builtin(ov::ARRAY_PUSH)));
}

#[test]
fn overload_tightest_qualifier_row_wins() {
    let (ast, t) = typed(
        Dialect::V6,
        "f(simple float x) => 1\nf(series float x) => 2\ny = f(1.0)\n",
    );
    let Node::Script { body } = ast.node(ast.root()) else {
        unreachable!()
    };
    let user = t.insts[t.root]
        .calls
        .iter()
        .find_map(|c| match c.1 {
            Callee::User(i) => Some(i),
            _ => None,
        })
        .expect("a user call");
    assert_eq!(
        t.insts[user as usize].decl,
        Some(body[1]),
        "the `simple` row"
    );
}

#[test]
fn load_checked_params_cover_pine_spec() {
    let mut seen = [false; 5];
    let mut k = 0;
    while k < pine_spec::OVERLOADS.len() {
        let o = &pine_spec::OVERLOADS[k];
        let e = pine_spec::entry_of(o);
        for p in pine_spec::params(o) {
            let Some(i) = LOAD_CHECKED.iter().position(|n| *n == p.name.as_str()) else {
                continue;
            };
            seen[i] = true;
            if p.sig.ty.ty != Ty::String {
                continue;
            }
            // Reached by §4.5 step 10, or by §4.11.1 / §4.12.3 for the named entries.
            let elsewhere = LOAD_CHECKED_ELSEWHERE.contains(&e.full_str());
            let step10 = !elsewhere && p.flags & pf::SYNTHETIC == 0;
            assert!(step10 || elsewhere, "{} {}", e.full_str(), p.name.as_str());
        }
        k += 1;
    }
    for (i, s) in seen.iter().enumerate() {
        assert!(
            *s,
            "`{}` occurs in no pine_spec::PARAMS row",
            LOAD_CHECKED[i]
        );
    }
}

#[test]
fn derived_defaults_resolve_at_the_call_site() {
    let mut n = 0;
    for o in pine_spec::OVERLOADS.iter() {
        let ps = pine_spec::params(o);
        for (r, p) in ps.iter().enumerate() {
            if p.default.kind != DefaultKind::Derived {
                assert!(matches!(
                    default_src(o, r as u16, &[]),
                    DefaultSrc::Literal(_)
                ));
                continue;
            }
            n += 1;
            match default_src(o, r as u16, &[]) {
                DefaultSrc::Literal(_) => panic!("{} is derived", p.name.as_str()),
                DefaultSrc::Param { row, arg } => {
                    assert_eq!(arg, None);
                    assert_eq!(ps[row as usize].name.as_str(), "title");
                    assert_eq!(
                        default_src(o, r as u16, &[None, Some(row)]),
                        DefaultSrc::Param { row, arg: Some(1) }
                    );
                }
                DefaultSrc::NoOptions => assert!(p.flags & pf::OPTIONS != 0),
                _ => {}
            }
        }
    }
    assert!(n >= 20, "{n} derived defaults");
    assert_eq!(sym_precision(100_000), 1);
    assert_eq!(sym_precision(10), 5);
    assert_eq!(sym_precision(1_000_000), 0);
    assert_eq!(sym_precision(1), 6);
}
