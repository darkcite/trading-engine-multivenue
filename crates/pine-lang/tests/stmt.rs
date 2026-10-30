// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Statements (§4.7, §7.6), structure-based until the printer lands (T11).

use pine_ir::{FileId, NodeId, SymTable};
use pine_lang::{parse, Ast, DeclMode, Node, NodeKind};

/// Parse `body` as a v6 script: the `Ast` and table, or the refusal codes.
fn parse6(body: &str) -> Result<(Ast, SymTable), Vec<u16>> {
    let src = format!("//@version=6\n{body}");
    let mut syms = SymTable::new();
    match parse(&src, FileId::ROOT, &mut syms) {
        Ok(ast) => {
            assert!(ast.validate().is_ok(), "{body:?}");
            Ok((ast, syms))
        }
        Err(d) => Err(d.iter().map(|x| x.code.0).collect()),
    }
}

/// The top-level statements of an accepted `body`.
fn stmts(body: &str) -> (Ast, Vec<NodeId>) {
    let (ast, _) = parse6(body).unwrap_or_else(|c| panic!("{body:?} refused: {c:?}"));
    let Node::Script { body: b } = ast.node(ast.root()) else {
        panic!("root")
    };
    let v = b.to_vec();
    (ast, v)
}

fn kinds(body: &str) -> Vec<NodeKind> {
    let (ast, v) = stmts(body);
    v.iter().map(|&s| ast.kind(s)).collect()
}

fn codes(body: &str) -> Vec<u16> {
    parse6(body).err().unwrap_or_default()
}

#[test]
fn declarations_and_assignments() {
    use NodeKind::*;
    assert_eq!(
        kinds("x = 1\nvar y = 2\nvarip z = 3\n"),
        vec![VarDecl, VarDecl, VarDecl]
    );
    assert_eq!(
        kinds("float x = 1.0\nseries int n = 2\narray<float> a = na\n"),
        vec![VarDecl; 3]
    );
    assert_eq!(kinds("series = close\nfloat(x)\n"), vec![VarDecl, Call]);
    assert_eq!(
        kinds("x := 1\nx += 2\np.x := 3\n"),
        vec![Assign, Assign, Assign]
    );
    let (ast, v) = stmts("var series float x = 1\n");
    let Node::VarDecl { mode, qual, ty, .. } = ast.node(v[0]) else {
        panic!()
    };
    assert_eq!(mode, DeclMode::Var);
    assert_eq!(qual, Some(pine_ir::Qual::Series));
    assert!(ty.is_some());
    assert_eq!(codes("1 := 2\n"), vec![2021]);
    assert_eq!(codes("p.x = 1\n"), vec![2021]);
    assert_eq!(codes("var [a, b] = f()\n"), vec![2028]);
    assert_eq!(codes("float x\n"), vec![2000]);
}

#[test]
fn structures_and_blocks() {
    use NodeKind::*;
    let src = "if c\n    x := 1\nelse if d\n    x := 2\nelse\n    x := 3\ny = 1\n";
    assert_eq!(kinds(src), vec![If, VarDecl]);
    assert_eq!(kinds("x = if c\n    1\nelse\n    2\n"), vec![VarDecl]);
    assert_eq!(kinds("for i = 0 to 9 by 2\n    break\n"), vec![For]);
    assert_eq!(kinds("for int i = 0 to 9\n    continue\n"), vec![For]);
    assert_eq!(
        kinds("for x in xs\n    x\nfor [k, v] in m\n    v\n"),
        vec![ForIn, ForIn]
    );
    assert_eq!(kinds("while c\n    if d\n        break\n"), vec![While]);
    let sw = "s = switch x\n    1 => a\n    2 =>\n        b\n    => c\n";
    assert_eq!(kinds(sw), vec![VarDecl]);
    assert_eq!(kinds("switch\n    a => 1\n    => 2\n"), vec![Switch]);
    assert_eq!(kinds("a = 1, b = 2\n"), vec![VarDecl, VarDecl]);
    assert_eq!(
        kinds("[a, b] = f()\n[a, b] := g()\n[x] = h()\n"),
        vec![TupleDecl, TupleAssign, TupleDecl]
    );
    assert_eq!(kinds("[a, b]\n"), vec![Tuple]);
    assert_eq!(
        kinds("once\n    x\nonce c\n    y\nonce = 1\n"),
        vec![Once, Once, VarDecl]
    );
}

#[test]
fn structure_errors_report_once() {
    assert_eq!(codes("break\n"), vec![2010]);
    assert_eq!(codes("if c\nx = 1\n"), vec![2005]);
    assert_eq!(codes("for i = 0 10\n    x\n"), vec![2000]);
    assert_eq!(codes("x = 1\n    y = 2\n    z = 3\n"), vec![1008]);
    assert_eq!(codes("switch\n    => 1\n    a => 2\n"), vec![2006]);
    assert_eq!(codes("switch\n    => 1\n    => 2\n"), vec![2007]);
    assert_eq!(codes("[float a, b] = f()\n"), vec![2028]);
    assert_eq!(codes("[a.b, c] = f()\n"), vec![2003]);
}

#[test]
fn once_is_v6_only() {
    let mut syms = SymTable::new();
    let r = parse("//@version=5\nonce\n    x\n", FileId::ROOT, &mut syms);
    let codes: Vec<u16> = r
        .err()
        .unwrap_or_default()
        .iter()
        .map(|d| d.code.0)
        .collect();
    assert_eq!(codes, vec![2104]);
}

#[test]
fn loop_depth_resets_outside_the_loop() {
    assert_eq!(codes("for i = 0 to 1\n    x\nbreak\n"), vec![2010]);
}

/// One generated line of token soup: Pine-ish words and punctuation at random indentation.
fn soup_line() -> impl proptest::strategy::Strategy<Value = String> {
    use proptest::prelude::*;
    let word = prop_oneof![
        Just("x"),
        Just("f"),
        Just("if"),
        Just("else"),
        Just("for"),
        Just("to"),
        Just("in"),
        Just("while"),
        Just("switch"),
        Just("var"),
        Just("break"),
        Just("float"),
        Just("series"),
        Just("="),
        Just(":="),
        Just("+="),
        Just("=>"),
        Just("("),
        Just(")"),
        Just("["),
        Just("]"),
        Just(","),
        Just("."),
        Just("?"),
        Just(":"),
        Just("<"),
        Just(">"),
        Just("+"),
        Just("-"),
        Just("not"),
        Just("1"),
        Just("2.5"),
        Just("\"s\""),
        Just("#ff0000"),
        Just("@"),
        Just("once"),
        Just("import"),
        Just("type"),
    ];
    let indent = prop_oneof![
        Just(""),
        Just("  "),
        Just("    "),
        Just("        "),
        Just("\t")
    ];
    (indent, proptest::collection::vec(word, 0..8))
        .prop_map(|(i, ws)| format!("{i}{}\n", ws.join(" ")))
}

proptest::proptest! {
    #[test]
    fn parse_never_panics_and_ok_validates(
        lines in proptest::collection::vec(soup_line(), 0..12),
        src in ".*",
    ) {
        for body in [lines.concat(), src] {
            let full = format!("//@version=6\n{body}");
            let mut syms = SymTable::new();
            match parse(&full, FileId::ROOT, &mut syms) {
                Ok(ast) => {
                    proptest::prop_assert!(ast.validate().is_ok());
                    // Every accepted program round-trips through the printer (I9).
                    let mut text = String::new();
                    pine_lang::print(&ast, &syms, &mut text).expect("print");
                    let mut s2 = SymTable::new();
                    let again = parse(&text, FileId::ROOT, &mut s2);
                    proptest::prop_assert!(again.is_ok(), "reparse of {text:?} failed: {again:?}");
                    if let Ok(b) = again {
                        proptest::prop_assert!(pine_lang::structurally_eq(&ast, &syms, &b, &s2), "{text}");
                    }
                }
                Err(d) => {
                    proptest::prop_assert!(!d.is_empty());
                    proptest::prop_assert!(d.len() <= pine_lang::MAX_DIAGS);
                }
            }
        }
    }
}
