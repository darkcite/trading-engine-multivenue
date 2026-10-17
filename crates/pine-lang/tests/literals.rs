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

/// pin:01-unknown-escape `DropBackslash`: TradingView's `str.length("a\qb")` is 3 (Z017).
#[test]
fn unknown_escape_drops_backslash_z017() {
    with_value("\"a\\qb\"", |_, s, n| match n {
        Node::StrLit { value, .. } => assert_eq!(s.resolve(value), "aqb"),
        other => panic!("{other:?}"),
    });
}

/// The string value of `x = <lit>` (v6).
fn str_value(lit: &str) -> String {
    with_value(lit, |_, s, n| match n {
        Node::StrLit { value, .. } => s.resolve(value).to_string(),
        other => panic!("{other:?}"),
    })
}

/// pin:01-multiline-string-value `Verbatim`: value probe v4 plots `str.length` 3 · 3 · 7 on
/// TradingView (Z018) — both edge breaks and the closing line's indentation are content.
#[test]
fn multiline_values_z018() {
    assert_eq!(str_value("\"\"\"\nab\"\"\"").chars().count(), 3);
    assert_eq!(str_value("\"\"\"ab\n\"\"\"").chars().count(), 3);
    assert_eq!(str_value("\"\"\"ab\n    \"\"\""), "ab\n    ");
    assert_eq!(str_value("\"\"\"ab\r\n    \"\"\""), "ab\n    ");
}

/// pin:01-string-line-wrap: TradingView accepts p06's wrapped single-line string (Z006); the break
/// (normalised) and the next line's indentation stay in the value, as in Z018's multiline literal.
#[test]
fn string_line_wrap_value_z006() {
    assert_eq!(str_value("\"ab\n  cd\""), "ab\n  cd");
    assert_eq!(str_value("'ab\r\n  cd'"), "ab\n  cd");
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
