// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Each non-default `SyntaxPolicy` value landed so far, so flipping a pin is a tested one-line
//! change (§4.12, §7.3). Parser-level pins join when the parser lands.

use pine_ir::Dialect::{V5, V6};
use pine_ir::FileId;

use super::{first_str, lex_with, show};
use crate::policy::{MixedIndent, MultilineEdges, SyntaxPolicy, UnknownEscape, POLICY};
use crate::version::{version_of_with, VersionProbe};

#[test]
fn policy_alternatives_behave() {
    // pin:01-mixed-indent: the default TabIsUnit accepts the line, measured with a tab as one
    // level (Z001); Refuse is 1006 with the same layout.
    let (lexed, codes, _) = lex_with("if a\n    if b\n\t    x\n", V6, &POLICY);
    assert!(codes.is_empty());
    let shape = "If Ident Newline Indent If Ident Newline Indent Ident Newline Dedent Dedent Eof";
    assert_eq!(show(&lexed), shape);
    let p = SyntaxPolicy {
        mixed_indent: MixedIndent::Refuse,
        ..POLICY
    };
    let (lexed, codes, _) = lex_with("if a\n    if b\n\t    x\n", V6, &p);
    assert_eq!(codes, vec![1006]);
    assert_eq!(show(&lexed), shape);

    // pin:01-unknown-escape: KeepBoth (the default drops the backslash, Z017).
    let p = SyntaxPolicy {
        unknown_escape: UnknownEscape::KeepBoth,
        ..POLICY
    };
    let (lexed, _, syms) = lex_with("\"a\\qb\"", V6, &p);
    assert_eq!(first_str(&lexed, &syms).as_deref(), Some("a\\qb"));

    // pin:01-multiline-string-value: Verbatim (the default, Z018) keeps everything; KeepBreaks
    // drops the closing line's indentation; TrimBreaks also drops both edge breaks.
    let src = "\"\"\"\r\nab\n  \"\"\"";
    let (lexed, _, syms) = lex_with(src, V6, &POLICY);
    assert_eq!(first_str(&lexed, &syms).as_deref(), Some("\nab\n  "));
    let p = SyntaxPolicy {
        multiline_edges: MultilineEdges::KeepBreaks,
        ..POLICY
    };
    let (lexed, _, syms) = lex_with(src, V6, &p);
    assert_eq!(first_str(&lexed, &syms).as_deref(), Some("\nab\n"));
    let p = SyntaxPolicy {
        multiline_edges: MultilineEdges::TrimBreaks,
        ..POLICY
    };
    let (lexed, _, syms) = lex_with(src, V6, &p);
    assert_eq!(first_str(&lexed, &syms).as_deref(), Some("ab"));

    // pin:01-first-line-indent: R2 read literally refuses an indented first line (1008); the
    // default accepts it (Z014).
    assert!(lex_with(" x = 1\n", V6, &POLICY).1.is_empty());
    let p = SyntaxPolicy {
        first_line_indent_refused: true,
        ..POLICY
    };
    assert_eq!(lex_with(" x = 1\n", V6, &p).1, vec![1008]);

    // pin:01-bracket-wrap: `[ ]` no longer follows `( )`: column 4 is refused, column 2 wraps.
    let p = SyntaxPolicy {
        bracket_wrap_like_paren: false,
        ..POLICY
    };
    assert_eq!(lex_with("x = [a,\n    b]\n", V6, &p).1, vec![1008]);
    assert!(lex_with("x = [a,\n  b]\n", V6, &p).1.is_empty());

    // pin:01-v5-paren-wrap: v5 wraps inside `( )` like v6.
    let p = SyntaxPolicy {
        v5_paren_wrap_strict: false,
        ..POLICY
    };
    assert!(lex_with("f(a,\n    b)\n", V5, &p).1.is_empty());

    // pin:01-version-form: spaced forms accepted by the default (Z007); strict is 2103.
    assert!(matches!(
        version_of_with("//@version = 6", FileId::ROOT, &POLICY),
        VersionProbe::Supported { .. }
    ));
    assert!(matches!(
        version_of_with("//@version=6x", FileId::ROOT, &POLICY),
        VersionProbe::Malformed { .. }
    ));
    let p = SyntaxPolicy {
        version_form_strict: true,
        ..POLICY
    };
    assert!(matches!(
        version_of_with("//@version = 6", FileId::ROOT, &p),
        VersionProbe::Malformed { .. }
    ));

    // pin:01-version-conflict: not refused by the default, the first annotation decides (Z008);
    // the refusing alternative is 2105.
    assert!(matches!(
        version_of_with("//@version=5\n//@version=6", FileId::ROOT, &POLICY),
        VersionProbe::Supported {
            dialect: pine_ir::Dialect::V5,
            ..
        }
    ));
    let p = SyntaxPolicy {
        version_conflict_refused: true,
        ..POLICY
    };
    assert!(matches!(
        version_of_with("//@version=5\n//@version=6", FileId::ROOT, &p),
        VersionProbe::Conflict { .. }
    ));
}

#[test]
fn parser_policy_alternatives_behave() {
    use crate::ast::{Node, NodeKind};
    use crate::parse::parse_with_policy;
    use pine_ir::SymTable;

    // pin:01-ternary-assoc: left-associative.
    let p = SyntaxPolicy {
        ternary_right_assoc: false,
        ..POLICY
    };
    let mut syms = SymTable::new();
    let (partial, diags) = parse_with_policy(
        "//@version=6\na ? b : c ? d : e\n",
        FileId::ROOT,
        &mut syms,
        &p,
    );
    assert!(diags.is_empty());
    let ast = partial.ast.expect("an arena");
    let Node::Script { body } = ast.node(ast.root()) else {
        panic!("root")
    };
    let Node::Ternary { cond, .. } = ast.node(body[0]) else {
        panic!("not a ternary")
    };
    assert_eq!(ast.kind(cond), NodeKind::Ternary);

    // pin:01-array-suffix-type: `float[]` is no type, so the `<` is a comparison.
    let p = SyntaxPolicy {
        array_suffix_type: false,
        ..POLICY
    };
    let mut syms = SymTable::new();
    let (_, diags) = parse_with_policy(
        "//@version=6\narray.new<float[]>(0)\n",
        FileId::ROOT,
        &mut syms,
        &p,
    );
    let codes: Vec<u16> = diags.iter().map(|d| d.code.0).collect();
    assert_eq!(codes, vec![2002]);
}

/// The codes of `body` (after a v6 pragma) under `p`.
fn codes_under(p: &SyntaxPolicy, body: &str) -> Vec<u16> {
    let mut syms = pine_ir::SymTable::new();
    let src = format!("//@version=6\n{body}");
    let (_, diags) = crate::parse::parse_with_policy(&src, FileId::ROOT, &mut syms, p);
    diags.iter().map(|d| d.code.0).collect()
}

#[test]
fn declaration_policy_alternatives_behave() {
    // pin:01-local-type-decl: local `type` accepted.
    let p = SyntaxPolicy {
        local_type_decl: true,
        ..POLICY
    };
    assert!(codes_under(&p, "if c\n    type T\n").is_empty());
    assert_eq!(codes_under(&POLICY, "if c\n    type T\n"), vec![2027]);

    // pin:01-var-field: `var` on a field accepted.
    let p = SyntaxPolicy {
        var_field: true,
        ..POLICY
    };
    assert!(codes_under(&p, "type T\n    var int x\n").is_empty());

    // pin:01-import-version-form: a non-integer version accepted.
    let p = SyntaxPolicy {
        import_version_int_only: false,
        ..POLICY
    };
    assert!(codes_under(&p, "import u/l/1.2\n").is_empty());

    // pin:01-string-line-wrap: by default (Z006) a single-line string may wrap; the break stays
    // in the value; the refusing alternative is 1001.
    let (lexed, codes, syms) = lex_with("s = \"ab\r\n  cd\"\n", V6, &POLICY);
    assert!(codes.is_empty());
    assert_eq!(first_str(&lexed, &syms).as_deref(), Some("ab\n  cd"));
    assert!(matches!(
        version_of_with(
            "s = \"a\n//@version=4\"\n//@version=6",
            FileId::ROOT,
            &POLICY
        ),
        VersionProbe::Supported { .. }
    ));
    let p = SyntaxPolicy {
        string_line_wrap: false,
        ..POLICY
    };
    let (_, codes, _) = lex_with("s = \"ab\n  cd\"\n", V6, &p);
    assert!(codes.contains(&1001), "{codes:?}");
}
