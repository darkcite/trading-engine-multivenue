// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Literals, operators, terminators and the import-path mode (§4.4, §7.3).

use pine_ir::Dialect::{V5, V6};
use proptest::prelude::*;

use super::{first_str, kinds, lex_with, show, sym_of};
use crate::lex::token::{SoftKw, HAS_ALPHA};
use crate::policy::POLICY;

#[test]
fn literal_table() {
    // (source, dialect, token kinds, codes, value of the first Str)
    let table: &[(&str, pine_ir::Dialect, &str, &[u16], Option<&str>)] = &[
        ("0", V6, "Int Newline Eof", &[], None),
        ("42", V6, "Int Newline Eof", &[], None),
        ("007", V6, "Int Newline Eof", &[], None),
        ("18446744073709551615", V6, "Int Newline Eof", &[], None),
        ("9223372036854775808", V6, "Int Newline Eof", &[], None),
        (
            "18446744073709551616",
            V6,
            "Error Newline Eof",
            &[1005],
            None,
        ),
        ("3.14159", V6, "Float Newline Eof", &[], None),
        ("0.0", V6, "Float Newline Eof", &[], None),
        ("6.02E+23", V6, "Float Newline Eof", &[], None),
        ("1.6e-19", V6, "Float Newline Eof", &[], None),
        ("1e5", V6, "Float Newline Eof", &[], None),
        ("1E-5", V6, "Float Newline Eof", &[], None),
        (".5", V6, "Float Newline Eof", &[], None),
        (".5e3", V6, "Float Newline Eof", &[], None),
        ("1.", V6, "Float Newline Eof", &[], None),
        ("1.e5", V6, "Float Newline Eof", &[], None),
        ("5.e-3", V6, "Float Newline Eof", &[], None),
        ("1. + 2", V6, "Float Plus Int Newline Eof", &[], None),
        ("1.abs", V6, "Int Dot Ident Newline Eof", &[], None),
        ("12abc", V6, "Error Newline Eof", &[1004], None),
        ("1_000", V6, "Error Newline Eof", &[1004], None),
        ("1e", V6, "Error Newline Eof", &[1004], None),
        ("1e+", V6, "Error Newline Eof", &[1004], None),
        ("1.5e", V6, "Error Newline Eof", &[1004], None),
        ("1.2.3", V6, "Error Newline Eof", &[1004], None),
        ("1e999", V6, "Error Newline Eof", &[1005], None),
        ("\"abc\"", V6, "Str Newline Eof", &[], Some("abc")),
        ("'abc'", V6, "Str Newline Eof", &[], Some("abc")),
        ("\"\"", V6, "Str Newline Eof", &[], Some("")),
        ("\"it's\"", V6, "Str Newline Eof", &[], Some("it's")),
        (
            "'say \"hi\"'",
            V6,
            "Str Newline Eof",
            &[],
            Some("say \"hi\""),
        ),
        ("\"a\\nb\"", V6, "Str Newline Eof", &[], Some("a\nb")),
        ("\"a\\tb\"", V6, "Str Newline Eof", &[], Some("a\tb")),
        ("\"a\\\\b\"", V6, "Str Newline Eof", &[], Some("a\\b")),
        ("\"a\\\"b\"", V6, "Str Newline Eof", &[], Some("a\"b")),
        ("'a\\'b'", V6, "Str Newline Eof", &[], Some("a'b")),
        // pin:01-unknown-escape `DropBackslash` (Z017: TradingView's length is 3).
        ("\"a\\qb\"", V6, "Str Newline Eof", &[], Some("aqb")),
        ("\"abc", V6, "Error Newline Eof", &[1001], None),
        // pin:01-string-line-wrap (Z006): a single-line string may wrap, so an unclosed one runs
        // to the end of the file.
        ("\"abc\nx", V6, "Error Newline Eof", &[1001], None),
        ("\"a\\\nx", V6, "Error Newline Eof", &[1001], None),
        (
            "\"\"\"\nline1\r\n  line2\n\"\"\"",
            V6,
            "Str Newline Eof",
            &[],
            Some("\nline1\n  line2\n"),
        ),
        // pin:01-multiline-string-value `Verbatim` (Z018: the closing indentation is kept).
        (
            "'''\n  a\n  '''",
            V6,
            "Str Newline Eof",
            &[],
            Some("\n  a\n  "),
        ),
        (
            "\"\"\"a\\tb\"\"\"",
            V6,
            "Str Newline Eof",
            &[],
            Some("a\tb"),
        ),
        ("\"\"\"x\"\"\"", V5, "Str Newline Eof", &[2104], Some("x")),
        ("\"\"\"abc", V6, "Error Newline Eof", &[1001], None),
        ("#FF0000", V6, "Color Newline Eof", &[], None),
        ("#ff0000a0", V6, "Color Newline Eof", &[], None),
        ("#GG0000", V6, "Error Newline Eof", &[1003], None),
        ("#FF00001", V6, "Error Newline Eof", &[1003], None),
        ("#FFF", V6, "Error Newline Eof", &[1003], None),
        ("#FF0000x", V6, "Error Newline Eof", &[1003], None),
        ("{", V6, "Eof", &[1012], None),
        ("}", V6, "Eof", &[1012], None),
        ("!", V6, "Error Newline Eof", &[1000], None),
        ("@", V6, "Error Newline Eof", &[1000], None),
        ("$", V6, "Error Newline Eof", &[1000], None),
        (";", V6, "Error Newline Eof", &[1000], None),
        ("`", V6, "Error Newline Eof", &[1000], None),
        ("\\", V6, "Error Newline Eof", &[1000], None),
        ("\u{1}", V6, "Error Newline Eof", &[1000], None),
        ("\u{1F600}", V6, "Error Newline Eof", &[1000], None),
        ("café = 1", V6, "Ident Eq Int Newline Eof", &[], None),
        ("日本 = 1", V6, "Ident Eq Int Newline Eof", &[], None),
        (
            "\"a\u{1F600}b\"",
            V6,
            "Str Newline Eof",
            &[],
            Some("a\u{1F600}b"),
        ),
        ("\u{FEFF}x = 1", V6, "Ident Eq Int Newline Eof", &[], None),
        ("a.if", V6, "Ident Dot Ident Newline Eof", &[], None),
        ("type = na", V6, "Ident Eq Ident Newline Eof", &[], None),
        ("x != y", V6, "Ident BangEq Ident Newline Eof", &[], None),
        (
            "and or not true false",
            V6,
            "And Or Not True False Newline Eof",
            &[],
            None,
        ),
        (
            "break continue var varip",
            V6,
            "Break Continue Var Varip Newline Eof",
            &[],
            None,
        ),
        (
            "for while switch else",
            V6,
            "For While Switch Else Newline Eof",
            &[],
            None,
        ),
        (
            "a ? b : c",
            V6,
            "Ident Question Ident Colon Ident Newline Eof",
            &[],
            None,
        ),
    ];
    assert!(table.len() >= 60);
    for &(src, d, want, codes, value) in table {
        let (lexed, got_codes, syms) = lex_with(src, d, &POLICY);
        assert_eq!(show(&lexed), want, "tokens of {src:?}");
        assert_eq!(got_codes, codes, "codes of {src:?}");
        assert_eq!(
            first_str(&lexed, &syms).as_deref(),
            value,
            "value of {src:?}"
        );
    }
}

#[test]
fn colour_alpha_and_soft_keywords_are_tagged() {
    let (lexed, _, _) = lex_with("#ff0000a0 #ff0000", V6, &POLICY);
    assert_eq!(lexed.toks[0].flags & HAS_ALPHA, HAS_ALPHA);
    assert_eq!(lexed.toks[1].flags & HAS_ALPHA, 0);
    let (lexed, _, syms) = lex_with("series method x.type", V6, &POLICY);
    assert_eq!(lexed.toks[0].kw, SoftKw::Series);
    assert_eq!(lexed.toks[1].kw, SoftKw::Method);
    assert_eq!(lexed.toks[4].kw, SoftKw::None);
    assert_eq!(sym_of(&lexed, &syms, 4), "type");
}

#[test]
fn operators_are_maximal_munch() {
    let ops = [
        (":=", "ColonEq"),
        ("==", "EqEq"),
        ("!=", "BangEq"),
        ("<=", "Le"),
        (">=", "Ge"),
        ("=>", "FatArrow"),
        ("+=", "PlusEq"),
        ("-=", "MinusEq"),
        ("*=", "StarEq"),
        ("/=", "SlashEq"),
        ("%=", "PercentEq"),
    ];
    for (op, name) in ops {
        let src = format!("a {op} b");
        let (got, codes) = kinds(&src, V6);
        assert_eq!(got, format!("Ident {name} Ident Newline Eof"), "{op}");
        assert!(codes.is_empty());
        let tight = format!("a{op}b");
        assert_eq!(
            kinds(&tight, V6).0,
            format!("Ident {name} Ident Newline Eof"),
            "{op}"
        );
    }
    assert_eq!(
        kinds("+ - * / % = < > ? : , .", V6).0,
        "Plus Minus Star Slash Percent Eq Lt Gt Question Colon Comma Dot Newline Eof"
    );
}

#[test]
fn crlf_and_lone_cr_are_one_terminator() {
    for src in ["a\nb", "a\r\nb", "a\rb", "a\r\n\r\nb", "a\r\rb", "a\n\r\nb"] {
        assert_eq!(
            kinds(src, V6),
            ("Ident Newline Ident Newline Eof".to_string(), vec![]),
            "{src:?}"
        );
    }
}

#[test]
fn scratch_copy_only_for_escaped_or_cr_literals() {
    use crate::lex::literal::needs_scratch;
    use crate::lex::token::{TokKind, STR_FORM_SHIFT};
    // The decision itself.
    assert!(!needs_scratch(b"abc"));
    assert!(needs_scratch(b"a\\nb"));
    assert!(needs_scratch(b"a\r\nb"));
    assert!(!needs_scratch(b"a\nb"));
    // Applied to every Str token's raw value: (source, literals taking the copy path).
    let cases: [(&str, usize); 7] = [
        ("\"abc\"", 0),
        ("'it''s'", 0),
        ("\"a\\nb\"", 1),
        ("\"a\\qb\"", 1),
        ("\"\"\"a\nb\"\"\"", 0),
        ("\"\"\"a\r\nb\"\"\"", 1),
        ("\"x\" + \"y\\\\\" + 'z'", 1),
    ];
    for (src, want) in cases {
        let (lexed, _, _) = lex_with(src, V6, &POLICY);
        let copies = lexed
            .toks
            .iter()
            .filter(|t| t.kind == TokKind::Str)
            .filter(|t| {
                let q = if (t.flags >> STR_FORM_SHIFT) & 2 == 0 {
                    1
                } else {
                    3
                };
                needs_scratch(&src.as_bytes()[t.lo as usize + q..t.hi as usize - q])
            })
            .count();
        assert_eq!(copies, want, "{src:?}");
    }
}

#[test]
fn import_path_mode_reads_three_segments() {
    let (lexed, codes, syms) = lex_with("import user-1/lib.x/7 as a", V6, &POLICY);
    assert_eq!(show(&lexed), "Ident Seg Seg Seg Ident Ident Newline Eof");
    assert!(codes.is_empty());
    assert_eq!(lexed.toks[0].kw, SoftKw::Import);
    assert_eq!(sym_of(&lexed, &syms, 1), "user-1");
    assert_eq!(sym_of(&lexed, &syms, 2), "lib.x");
    assert_eq!(lexed.toks[5].kw, SoftKw::None);
    assert_eq!(lexed.toks[4].kw, SoftKw::As);
    assert_eq!(kinds("import = 1", V6).0, "Ident Eq Int Newline Eof");
    assert_eq!(
        kinds("import(1)", V6).0,
        "Ident LParen Int RParen Newline Eof"
    );
    assert_eq!(kinds("import a/b", V6).0, "Ident Seg Seg Newline Eof");
    assert_eq!(kinds("x = import", V6).0, "Ident Eq Ident Newline Eof");
}

#[test]
fn annotations_become_rows_and_plain_comments_do_not() {
    let src = "//@version=6\n//@param len The period  \n// @param x\n//@types\n//@type T\n";
    let (lexed, codes, syms) = lex_with(src, V6, &POLICY);
    assert!(codes.is_empty());
    let texts: Vec<(crate::ast::AnnotKind, &str)> = lexed
        .annos
        .iter()
        .map(|a| (a.kind, syms.resolve(a.text)))
        .collect();
    assert_eq!(
        texts,
        vec![
            (crate::ast::AnnotKind::Version, "6"),
            (crate::ast::AnnotKind::Param, "len The period"),
            (crate::ast::AnnotKind::Type, "T"),
        ]
    );
}

proptest! {
    #[test]
    fn prop_lexer_never_reads_past_end(src in any::<String>(), v5 in any::<bool>()) {
        let d = if v5 { V5 } else { V6 };
        let (lexed, _, _) = lex_with(&src, d, &POLICY);
        for t in &lexed.toks {
            prop_assert!(t.lo <= t.hi);
            prop_assert!(t.hi as usize <= src.len());
        }
    }
}
