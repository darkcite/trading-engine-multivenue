// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Pass 3's output (impl-02 §7.2's private twins): fold values, result types, temporaries and
//! the TypeInst cache.

use pine_ir::{Diag, Dialect, FileId, Qual, SymTable, Ty, TypeQ};
use pine_lang::{Ast, Node};

use crate::infer::{Callee, Typed, K};
use crate::sw::Readings;
use crate::{imports, infer, resolve, LibCache};

fn typed(d: Dialect, body: &str) -> (Ast, SymTable, Typed, Vec<Diag>) {
    let src = format!("//@version={}\nindicator(\"t\")\n{body}", d as u8);
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(&src, FileId::ROOT, &mut syms).expect("parses");
    let rd = Readings::of(d);
    let mut diags = Vec::new();
    let graph = imports::load(&ast, &mut syms, &mut LibCache::empty(), &mut diags);
    let res = resolve::resolve(&ast, &graph, &syms, &rd, &mut diags);
    assert!(diags.is_empty(), "{diags:?}");
    let t = infer::infer(&ast, &graph, &res, &mut syms, &rd, 0b0111, &[], &mut diags);
    // Rows the lane's pine-std snapshot does not serve yet (§4.5 step 9) are not the script's.
    diags.retain(|d| !(d.code.0 == 3024 && d.msg.contains(" unserved=1")));
    (ast, syms, t, diags)
}

/// The type and folded value of top-level variable `name`'s initialiser.
fn init_of(d: Dialect, body: &str, name: &str) -> (Option<TypeQ>, Option<K>, SymTable) {
    let (ast, syms, t, diags) = typed(d, body);
    assert!(diags.is_empty(), "{diags:?}");
    let Node::Script { body } = ast.node(ast.root()) else {
        unreachable!()
    };
    let root = &t.insts[t.root];
    for &s in body {
        if let Node::VarDecl { name: n, init, .. } = ast.node(s) {
            if syms.resolve(ast.sym(n)) == name {
                let i = init.0 as usize;
                return (root.ty[i], root.k[i], syms);
            }
        }
    }
    panic!("no variable {name}");
}

#[test]
fn infer_int_div_int_v5_const_truncates() {
    let (t, k, _) = init_of(Dialect::V5, "x = 5 / 2\n", "x");
    assert_eq!(t.map(|t| (t.ty, t.qual)), Some((Ty::Int, Qual::Const)));
    assert_eq!(k, Some(K::Int(2)));
}

#[test]
fn infer_int_div_int_v6_is_float() {
    let (t, k, _) = init_of(Dialect::V6, "x = 5 / 2\n", "x");
    assert_eq!(t.map(|t| t.ty), Some(Ty::Float));
    assert_eq!(k, Some(K::Float(2.5)));
}

#[test]
fn infer_int_div_int_v5_series_is_float() {
    let (t, k, _) = init_of(Dialect::V5, "x = bar_index / 2\n", "x");
    assert_eq!(t.map(|t| (t.ty, t.qual)), Some((Ty::Float, Qual::Series)));
    assert_eq!(k, None);
}

#[test]
fn infer_int_literal_beyond_i64_is_type_mismatch() {
    let (_, k, _) = init_of(Dialect::V6, "x = -9223372036854775808\n", "x");
    assert_eq!(k, Some(K::Int(i64::MIN)));
}

#[test]
fn infer_folds_wrap_mod_and_concat() {
    let (_, k, _) = init_of(Dialect::V6, "x = 9223372036854775807 + 1\n", "x");
    assert_eq!(k, Some(K::Int(i64::MIN)));
    let (_, k, _) = init_of(Dialect::V6, "x = -1 % 100\n", "x");
    assert_eq!(k, Some(K::Int(99)));
    let (_, k, _) = init_of(Dialect::V6, "x = 5 % 0\n", "x");
    assert_eq!(k, Some(K::Na));
    let (_, k, syms) = init_of(Dialect::V6, "x = \"a\" + \"b\"\n", "x");
    match k {
        Some(K::Str(s)) => assert_eq!(syms.resolve(s), "ab"),
        k => panic!("{k:?}"),
    }
}

#[test]
fn infer_history_on_expression_materialises_a_temp() {
    let (_, _, t, diags) = typed(
        Dialect::V6,
        "a = close\nb = open\nplot((a + b)[1])\nplot(a[1])\n",
    );
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(t.insts[t.root].temps.len(), 1);
}

#[test]
fn infer_user_function_typeinsts_are_cached_per_arg_key() {
    let (_, _, t, diags) = typed(
        Dialect::V6,
        "f(x) => x * 2\na = f(1)\nb = f(2)\nc = f(1.5)\nplot(c)\n",
    );
    assert!(diags.is_empty(), "{diags:?}");
    let users: Vec<u32> = t.insts[t.root]
        .calls
        .iter()
        .filter_map(|c| match c.1 {
            Callee::User(i) => Some(i),
            _ => None,
        })
        .collect();
    assert_eq!(users.len(), 3);
    assert_eq!(users[0], users[1], "f(int) is one TypeInst");
    assert_ne!(users[0], users[2], "f(float) is another");
    assert_eq!(t.insts[users[2] as usize].ret.ty, Ty::Float);
}
