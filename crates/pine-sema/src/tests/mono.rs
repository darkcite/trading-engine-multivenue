// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! §4.6's two keys on pass 3–4 output: TypeInsts shared by equal `ArgKey`s (impl-02 §7.12
//! `ir_shapes.rs`'s `mono_*` rows) and the Instantiation tree (§4.6.3).

use pine_ir::{Dialect, FileId, NodeId, SymTable};
use pine_lang::Ast;

use crate::infer::{Callee, Typed};
use crate::mono::{self, InstId, Tree};
use crate::sw::Readings;
use crate::{imports, infer, resolve, LibCache};

fn expand(body: &str) -> (Typed, Tree, Vec<u16>) {
    let src = format!("//@version=6\nindicator(\"t\")\n{body}");
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(&src, FileId::ROOT, &mut syms).expect("parses");
    let rd = Readings::of(Dialect::V6);
    let mut diags = Vec::new();
    let graph = imports::load(&ast, &mut syms, &mut LibCache::empty(), &mut diags);
    let res = resolve::resolve(&ast, &graph, &syms, &rd, &mut diags);
    let t = infer::infer(&ast, &graph, &res, &mut syms, &rd, 0b0111, &[], &mut diags);
    let files: Vec<Option<&Ast>> = vec![Some(&ast)];
    let tree = mono::build(&t, &files, &syms, &mut diags);
    diags.retain(|d| !(d.code.0 == 3024 && d.msg.contains(" unserved=1")));
    (t, tree, diags.iter().map(|d| d.code.0).collect())
}

/// The TypeInsts of user functions the root's top level calls, in call order.
fn user_calls(t: &Typed) -> Vec<(NodeId, u32)> {
    t.insts[t.root]
        .calls
        .iter()
        .filter_map(|&(n, c)| match c {
            Callee::User(i) => Some((n, i)),
            _ => None,
        })
        .collect()
}

#[test]
fn mono_type_inst_is_shared_by_equal_arg_keys() {
    let (t, _, c) = expand("f(x, n) => x * n\na = f(close, 10)\nb = f(open, 20)\nplot(a + b)\n");
    assert!(c.is_empty(), "{c:?}");
    let u = user_calls(&t);
    assert_eq!(u.len(), 2);
    assert_eq!(u[0].1, u[1].1, "one TypeInst for two call sites");
    assert_ne!(u[0].0, u[1].0);
}

#[test]
fn mono_qualifier_is_part_of_the_arg_key() {
    // `10` is `const int`, `bar_index` `series int`: two ArgKeys, two TypeInsts.
    let (t, _, c) = expand("f(n) => n + 1\na = f(10)\nb = f(bar_index)\nplot(a + b)\n");
    assert!(c.is_empty(), "{c:?}");
    let u = user_calls(&t);
    assert_eq!(u.len(), 2);
    assert_ne!(u[0].1, u[1].1);
    let k0 = &t.insts[u[0].1 as usize].key;
    let k1 = &t.insts[u[1].1 as usize].key;
    assert_eq!(k0[0].ty, k1[0].ty);
    assert_ne!(k0[0].qual, k1[0].qual);
}

#[test]
fn mono_two_call_sites_get_two_instantiations() {
    let (t, tree, c) =
        expand("f(x, n) => ta.sma(x, n)\na = f(close, 10)\nb = f(close, 10)\nplot(a + b)\n");
    assert!(c.is_empty(), "{c:?}");
    assert_eq!(tree.nodes.len(), 3);
    let u = user_calls(&t);
    let i1 = tree.child(InstId(0), u[0].0).expect("first site");
    let i2 = tree.child(InstId(0), u[1].0).expect("second site");
    assert_eq!((i1, i2), (InstId(1), InstId(2)));
    assert_eq!(tree.nodes[1].type_inst, tree.nodes[2].type_inst);
    assert_eq!(tree.nodes[1].parent, Some(InstId(0)));
}

#[test]
fn mono_tree_is_depth_first_in_execution_order() {
    // root → a=g() (→ h, h), b=h(): pre-order 0 root, 1 g, 2 h, 3 h, 4 h.
    let (_, tree, c) = expand(
        "h() => close\ng() =>\n    p = h()\n    q = h()\n    p + q\na = g()\nb = h()\nplot(a + b)\n",
    );
    assert!(c.is_empty(), "{c:?}");
    let parents: Vec<Option<u32>> = tree.nodes.iter().map(|n| n.parent.map(|p| p.0)).collect();
    assert_eq!(parents, [None, Some(0), Some(1), Some(1), Some(0)]);
    assert_eq!(tree.nodes[0].call, None);
    assert_eq!(tree.nodes[2].type_inst, tree.nodes[4].type_inst);
    // The arguments' calls run before the call that takes them: `g(h())` numbers `h` first.
    let (t, tree, c) = expand("h() => close\ng(x) => x\nplot(g(h()))\n");
    assert!(c.is_empty(), "{c:?}");
    let parents: Vec<Option<u32>> = tree.nodes.iter().map(|n| n.parent.map(|p| p.0)).collect();
    assert_eq!(parents, [None, Some(0), Some(0)]);
    let arity = |i: usize| t.insts[tree.nodes[i].type_inst as usize].key.len();
    assert_eq!(
        (arity(1), arity(2)),
        (0, 1),
        "`h` (no parameter) before `g`"
    );
}
