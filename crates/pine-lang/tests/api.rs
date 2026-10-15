// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The read API over every fixture script (§3, §7.10).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use pine_ir::{FileId, NodeId, SymTable};
use pine_lang::{line_col, parse, print, structurally_eq, AnnotKind, Ast, Node, NodeKind, Visit};

/// Every `tests/scripts/*.pine` parsed: (name, source, ast, table).
fn parsed() -> Vec<(String, String, Ast, SymTable)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts");
    let mut names: Vec<_> = fs::read_dir(&dir)
        .expect("scripts dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "pine"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|p| {
            let src = fs::read_to_string(&p).expect("read");
            let mut syms = SymTable::new();
            let ast =
                parse(&src, FileId::ROOT, &mut syms).unwrap_or_else(|d| panic!("{p:?}: {d:?}"));
            (p.to_string_lossy().into_owned(), src, ast, syms)
        })
        .collect()
}

fn ids(ast: &Ast) -> impl Iterator<Item = NodeId> {
    (0..ast.len() as u32).map(NodeId)
}

#[test]
fn every_script_ast_validates() {
    for (name, _, ast, _) in parsed() {
        assert!(ast.validate().is_ok(), "{name}");
    }
}

#[test]
fn ok_ast_has_no_error_nodes() {
    for (name, _, ast, _) in parsed() {
        assert!(ids(&ast).all(|i| ast.kind(i) != NodeKind::Error), "{name}");
    }
}

#[test]
fn children_are_in_source_order() {
    for (name, _, ast, _) in parsed() {
        for id in ids(&ast) {
            let mut prev = ast.span(id).lo;
            for c in ast.children(id) {
                assert!(ast.span(c).lo >= prev, "{name}: {id:?}");
                prev = ast.span(c).hi;
            }
        }
    }
}

#[test]
fn subtree_is_contiguous_and_ends_at_the_node() {
    for (name, _, ast, _) in parsed() {
        for id in ids(&ast) {
            let r = ast.subtree(id);
            assert_eq!(r.end, id.0 + 1, "{name}");
            for c in ast.children(id) {
                let cr = ast.subtree(c);
                assert!(cr.start >= r.start && cr.end <= r.end, "{name}");
            }
        }
    }
}

#[test]
fn walk_is_balanced_and_visits_every_node_once() {
    for (name, _, ast, _) in parsed() {
        let (mut enter, mut leave, mut depth) = (0usize, 0usize, 0i64);
        let mut seen = vec![false; ast.len()];
        for v in ast.walk(ast.root()) {
            match v {
                Visit::Enter(i) => {
                    assert!(!seen[i.0 as usize], "{name}: twice");
                    seen[i.0 as usize] = true;
                    enter += 1;
                    depth += 1;
                }
                Visit::Leave(_) => {
                    leave += 1;
                    depth -= 1;
                }
            }
            assert!(depth >= 0);
        }
        assert_eq!(enter, leave);
        assert_eq!(enter, ast.len(), "{name}");
    }
}

#[test]
fn view_covers_every_kind() {
    let mut kinds = BTreeSet::new();
    for (_, _, ast, _) in parsed() {
        for id in ids(&ast) {
            kinds.insert(ast.kind(id));
        }
    }
    let missing: Vec<NodeKind> = NodeKind::ALL
        .iter()
        .copied()
        .filter(|k| *k != NodeKind::Error && !kinds.contains(k))
        .collect();
    assert!(missing.is_empty(), "never produced: {missing:?}");
}

#[test]
fn sym_bound_bounds_every_sym() {
    for (name, _, ast, syms) in parsed() {
        assert!(ast.sym_bound() as usize <= syms.len());
        for id in ids(&ast) {
            match ast.node(id) {
                Node::Ident { sym } | Node::Name { sym } | Node::StrLit { value: sym, .. } => {
                    assert!(sym.0 < ast.sym_bound(), "{name}")
                }
                _ => {}
            }
        }
        for a in ast.annotations() {
            assert!(a.text.0 < ast.sym_bound(), "{name}");
        }
    }
}

#[test]
fn reparse_of_print_interns_nothing_new() {
    for (name, src, _, _) in parsed() {
        let mut syms = SymTable::new();
        let a = parse(&src, FileId::ROOT, &mut syms).expect("parses");
        let before = syms.len();
        let mut text = String::new();
        print(&a, &syms, &mut text).expect("print");
        let b = parse(&text, FileId::ROOT, &mut syms).expect("reparses");
        assert_eq!(syms.len(), before, "{name}");
        assert!(structurally_eq(&a, &syms, &b, &syms), "{name}");
    }
}

#[test]
fn parse_is_deterministic() {
    for (name, src, a, sa) in parsed() {
        let mut sb = SymTable::new();
        let b = parse(&src, FileId::ROOT, &mut sb).expect("parses");
        assert_eq!(sa.bytes(), sb.bytes(), "{name}");
        assert_eq!(a.len(), b.len(), "{name}");
        assert!(
            ids(&a).all(|i| a.kind(i) == b.kind(i) && a.span(i) == b.span(i)),
            "{name}"
        );
    }
}

#[test]
fn doc_runs_attach_to_the_next_statement() {
    let src = "//@version=6\n//@description d\n//@function f\n//@param x v\n//@returns r\n\
               f(x) => x\n//@variable a\n\ny = 1\n";
    let mut syms = SymTable::new();
    let ast = parse(src, FileId::ROOT, &mut syms).expect("parses");
    let Node::Script { body } = ast.node(ast.root()) else {
        panic!()
    };
    let d: Vec<AnnotKind> = ast.doc(body[0]).iter().map(|a| a.kind).collect();
    assert_eq!(
        d,
        vec![AnnotKind::Function, AnnotKind::Param, AnnotKind::Returns]
    );
    assert!(ast.doc(body[1]).is_empty(), "a blank line breaks the run");
}

#[test]
fn annotations_are_recorded_in_source_order() {
    let src = "//@version=6\n//@description d\nx = 1 //@variable trailing\n";
    let mut syms = SymTable::new();
    let ast = parse(src, FileId::ROOT, &mut syms).expect("parses");
    let k: Vec<AnnotKind> = ast.annotations().iter().map(|a| a.kind).collect();
    assert_eq!(
        k,
        vec![
            AnnotKind::Version,
            AnnotKind::Description,
            AnnotKind::Variable
        ]
    );
    let mut prev = 0;
    for a in ast.annotations() {
        assert!(a.span.lo >= prev);
        prev = a.span.hi;
    }
}

#[test]
fn names_and_idents_are_split_by_role() {
    let src = "//@version=6\nx = a.b\nf(p) => p\nx := g(n = 1)\n";
    let mut syms = SymTable::new();
    let ast = parse(src, FileId::ROOT, &mut syms).expect("parses");
    for id in ids(&ast) {
        match ast.node(id) {
            Node::VarDecl { name, .. } => assert_eq!(ast.kind(name), NodeKind::Name),
            Node::Field { base, member } => {
                assert_eq!(ast.kind(base), NodeKind::Ident);
                assert_eq!(ast.kind(member), NodeKind::Name);
            }
            Node::Param { name, .. } | Node::NamedArg { name, .. } => {
                assert_eq!(ast.kind(name), NodeKind::Name)
            }
            Node::FuncDecl { name, .. } => assert_eq!(ast.kind(name), NodeKind::Name),
            Node::Assign { target, .. } => assert_eq!(ast.kind(target), NodeKind::Ident),
            _ => {}
        }
    }
}

#[test]
fn ast_is_send_sync_clone() {
    fn check<T: Send + Sync + Clone>() {}
    check::<Ast>();
}

#[test]
fn line_col_matches_the_lexer_line_rule() {
    let src = "a\r\nb\rc\nd";
    assert_eq!(line_col(src, 3), (2, 1));
    assert_eq!(line_col(src, 5), (3, 1));
    assert_eq!(line_col(src, 7), (4, 1));
    assert_eq!(line_col(src, 99), (4, 2));
}
