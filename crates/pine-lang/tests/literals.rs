// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Literal values through the public view (§4.4, §7.4).

use pine_ir::{FileId, SymTable};
use pine_lang::{parse, Ast, Node, StrForm};

/// The value node of `x = <lit>` (v6), rendered by `f`.
fn with_value<R>(lit: &str, f: impl Fn(&Ast, &SymTable, Node<'_>) -> R) -> R {
    let mut syms = SymTable::new();
    let ast = parse(
        &format!("//@version=6\nx = {lit}\n"),
        FileId::ROOT,
        &mut syms,
    )
    .unwrap_or_else(|d| panic!("{lit}: {d:?}"));
    let Node::Script { body } = ast.node(ast.root()) else {
        panic!()
    };
    let Node::VarDecl { init, .. } = ast.node(body[0]) else {
        panic!()
    };
    f(&ast, &syms, ast.node(init))
}

#[test]
fn literal_values() {
    with_value("42", |_, _, n| {
        assert!(matches!(n, Node::IntLit { value: 42 }))
    });
    with_value("1.5e3", |_, _, n| {
        assert!(matches!(n, Node::FloatLit { value } if value == 1500.0))
    });
    with_value("true", |_, _, n| {
        assert!(matches!(n, Node::BoolLit { value: true }))
    });
    with_value("'a\\tb'", |_, s, n| match n {
        Node::StrLit { value, form } => {
            assert_eq!(s.resolve(value), "a\tb");
            assert_eq!(form, StrForm::Single);
        }
        other => panic!("{other:?}"),
    });
    with_value("#12abEF", |_, _, n| {
        assert!(matches!(
            n,
            Node::ColorLit {
                rgb: 0x12ABEF,
                alpha: None
            }
        ))
    });
    with_value("#12abEF80", |_, _, n| {
        assert!(matches!(
            n,
            Node::ColorLit {
                rgb: 0x12ABEF,
                alpha: Some(0x80)
            }
        ))
    });
}

#[test]
fn int_literal_above_i64_max_is_kept_as_u64() {
    with_value("-9223372036854775808", |a, _, n| match n {
        Node::Unary { operand, .. } => {
            assert!(matches!(a.node(operand), Node::IntLit { value } if value == 1u64 << 63))
        }
        other => panic!("{other:?}"),
    });
}

#[test]
fn unknown_escape_default_keeps_backslash() {
    with_value("\"a\\qb\"", |_, s, n| match n {
        Node::StrLit { value, .. } => assert_eq!(s.resolve(value), "a\\qb"),
        other => panic!("{other:?}"),
    });
}

#[test]
fn multiline_string_is_v6_only() {
    let src = "x = \"\"\"\nab\n\"\"\"\n";
    let mut syms = SymTable::new();
    assert!(parse(&format!("//@version=6\n{src}"), FileId::ROOT, &mut syms).is_ok());
    let mut syms = SymTable::new();
    let codes: Vec<u16> = parse(&format!("//@version=5\n{src}"), FileId::ROOT, &mut syms)
        .err()
        .unwrap_or_default()
        .iter()
        .map(|d| d.code.0)
        .collect();
    assert_eq!(codes, vec![2104]);
}
