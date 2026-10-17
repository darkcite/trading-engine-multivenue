// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The layout machine (§4.5, §7.3): the token table and the I2/I3 properties.

use pine_ir::Dialect::{V5, V6};
use proptest::prelude::*;

use super::{kinds, lex_with};
use crate::lex::token::{TokKind, CONTINUATION, LINE_START};
use crate::policy::POLICY;

#[test]
fn layout_token_table() {
    let table: [(&str, pine_ir::Dialect, &str, &[u16]); 26] = [
        // 1. one statement
        ("x = 1", V6, "Ident Eq Int Newline Eof", &[]),
        // 2. the worked if/else stream
        (
            "if c\n    x := 1\nelse\n    x := 2\n",
            V6,
            "If Ident Newline Indent Ident ColonEq Int Newline Dedent Else Newline Indent Ident \
             ColonEq Int Newline Dedent Eof",
            &[],
        ),
        // 3. the worked wrap stream: R3 at column 2, R4 inside ( ) at column 4
        (
            "x = a +\n  b\nplot(x,\n    color = c)\n",
            V6,
            "Ident Eq Ident Plus Ident Newline Ident LParen Ident Comma Ident Eq Ident RParen \
             Newline Eof",
            &[],
        ),
        // 4. nested blocks, dedent by two levels
        (
            "if a\n    if b\n        x\ny",
            V6,
            "If Ident Newline Indent If Ident Newline Indent Ident Newline Dedent Dedent Ident \
             Newline Eof",
            &[],
        ),
        // 5. a blank line inside a block (L1)
        (
            "if a\n    x\n\n    y\n",
            V6,
            "If Ident Newline Indent Ident Newline Ident Newline Dedent Eof",
            &[],
        ),
        // 6. a comment at column 0 inside a block (R5)
        (
            "if a\n    x\n// c\n    y\n",
            V6,
            "If Ident Newline Indent Ident Newline Ident Newline Dedent Eof",
            &[],
        ),
        // 7. an annotation line inside a block
        (
            "if a\n    x\n//@variable v\n    y\n",
            V6,
            "If Ident Newline Indent Ident Newline Ident Newline Dedent Eof",
            &[],
        ),
        // 8. a wrapped line at column 6 inside a block (R3)
        (
            "if a\n    x = a +\n      b\n",
            V6,
            "If Ident Newline Indent Ident Eq Ident Plus Ident Newline Dedent Eof",
            &[],
        ),
        // 9. R4 in v6: a wrapped argument at column 0
        (
            "f(a,\nb)\n",
            V6,
            "Ident LParen Ident Comma Ident RParen Newline Eof",
            &[],
        ),
        // 10. R4 in v6: at column 4
        (
            "f(a,\n    b)\n",
            V6,
            "Ident LParen Ident Comma Ident RParen Newline Eof",
            &[],
        ),
        // 11. SD1 in v5: column 4 inside ( ) is 2104, then a continuation
        (
            "f(a,\n    b)\n",
            V5,
            "Ident LParen Ident Comma Ident RParen Newline Eof",
            &[2104],
        ),
        // 12. SD1 in v5: column 0 inside ( ) is 2104 too
        (
            "f(a,\nb)\n",
            V5,
            "Ident LParen Ident Comma Ident RParen Newline Eof",
            &[2104],
        ),
        // 13. v5: column 2 inside ( ) is a plain wrap (R3)
        (
            "f(a,\n  b)\n",
            V5,
            "Ident LParen Ident Comma Ident RParen Newline Eof",
            &[],
        ),
        // 14. column 4 outside ( ): an Indent; the parser reports 1008
        (
            "x = a +\n    b\n",
            V6,
            "Ident Eq Ident Plus Newline Indent Ident Newline Dedent Eof",
            &[],
        ),
        // 15. an indented first line: no line is open, so it starts a statement
        //     (pin:01-first-line-indent — TradingView accepts Z014; R2 read literally was 1008)
        ("  x = 1\n", V6, "Ident Eq Int Newline Eof", &[]),
        // 16. a block at 8 spaces (1009), then column 4 stays in it, then column 0 closes it
        (
            "if a\n        x\n    y\n    w\nz\n",
            V6,
            "If Ident Newline Indent Ident Newline Ident Newline Ident Newline Dedent Ident \
             Newline Eof",
            &[1009],
        ),
        // 17. a tab-indented block (R1)
        (
            "if a\n\tx\n",
            V6,
            "If Ident Newline Indent Ident Newline Dedent Eof",
            &[],
        ),
        // 18. a mixed tab/space line on a real block column, a tab one level
        //     (pin:01-mixed-indent `TabIsUnit` — TradingView accepts Z001)
        (
            "if a\n    if b\n\t    x\n",
            V6,
            "If Ident Newline Indent If Ident Newline Indent Ident Newline Dedent Dedent Eof",
            &[],
        ),
        // 19. EOF inside a block
        (
            "if a\n    x",
            V6,
            "If Ident Newline Indent Ident Newline Dedent Eof",
            &[],
        ),
        // 20. a file holding only the pragma
        ("//@version=6", V6, "Eof", &[]),
        // 21. CRLF
        (
            "if a\r\n    x\r\ny\r\n",
            V6,
            "If Ident Newline Indent Ident Newline Dedent Ident Newline Eof",
            &[],
        ),
        // 22. lone CR
        (
            "if a\r    x\ry\r",
            V6,
            "If Ident Newline Indent Ident Newline Dedent Ident Newline Eof",
            &[],
        ),
        // 23. `f(x) =>` with a block body
        (
            "f(x) =>\n    x + 1\n",
            V6,
            "Ident LParen Ident RParen FatArrow Newline Indent Ident Plus Int Newline Dedent Eof",
            &[],
        ),
        // 24. switch arms with block bodies
        (
            "switch\n    a =>\n        b\n",
            V6,
            "Switch Newline Indent Ident FatArrow Newline Indent Ident Newline Dedent Dedent Eof",
            &[],
        ),
        // 25. a comment line inside a wrapped expression (R5)
        (
            "x = a +\n // c\n  b\n",
            V6,
            "Ident Eq Ident Plus Ident Newline Eof",
            &[],
        ),
        // 26. `[ ]` follows the `( )` rule of its dialect (pin:01-bracket-wrap): v5 col 4
        (
            "x = [a,\n    b]\n",
            V5,
            "Ident Eq LBracket Ident Comma Ident RBracket Newline Eof",
            &[2104],
        ),
    ];
    for (src, d, want, codes) in table {
        let (got, got_codes) = kinds(src, d);
        assert_eq!(got, want, "tokens of {src:?} ({d:?})");
        assert_eq!(got_codes, codes, "codes of {src:?} ({d:?})");
    }
}

#[test]
fn brackets_at_eof_and_stray_closers() {
    assert_eq!(
        kinds("f(a\n", V6),
        ("Ident LParen Ident Newline Eof".to_string(), vec![1010])
    );
    assert_eq!(
        kinds("a)\n", V6),
        ("Ident Newline Eof".to_string(), vec![1011])
    );
    assert_eq!(
        kinds("(a]\n", V6),
        ("LParen Ident Newline Eof".to_string(), vec![1010, 1011])
    );
}

#[test]
fn line_flags_mark_statement_and_wrapped_lines() {
    let (lexed, _, _) = lex_with("x = a +\n  b\ny\n", V6, &POLICY);
    let t = &lexed.toks;
    assert_eq!(t[0].flags & LINE_START, LINE_START);
    assert_eq!(t[4].kind, TokKind::Ident);
    assert_eq!(t[4].flags & CONTINUATION, CONTINUATION);
    assert_eq!(t[6].flags & LINE_START, LINE_START);
}

#[test]
fn block_nesting_overflow_is_refused_and_stays_balanced() {
    let mut src = String::new();
    let mut k = 0usize;
    while k < 70 {
        src.push_str(&" ".repeat(4 * k));
        src.push_str("if a\n");
        k += 1;
    }
    let (lexed, codes, _) = lex_with(&src, V6, &POLICY);
    assert!(codes.contains(&1014));
    let ind = lexed
        .toks
        .iter()
        .filter(|t| t.kind == TokKind::Indent)
        .count();
    let ded = lexed
        .toks
        .iter()
        .filter(|t| t.kind == TokKind::Dedent)
        .count();
    assert_eq!(ind, crate::MAX_BLOCK_NESTING as usize);
    assert_eq!(ind, ded);
}

/// One generated physical line: an indentation and a content shape.
fn line() -> impl Strategy<Value = String> {
    let indent = prop_oneof![
        (0usize..13).prop_map(|n| " ".repeat(n)),
        (0usize..4).prop_map(|n| "\t".repeat(n)),
    ];
    let body = prop_oneof![
        Just("x"),
        Just("x = a +"),
        Just("if a"),
        Just("// c"),
        Just(""),
        Just("f(a,"),
        Just("b)"),
        Just("[a,"),
        Just("b]"),
        Just("s = \"\"\"m"),
        Just("n\"\"\""),
    ];
    (
        indent,
        body,
        prop_oneof![Just("\n"), Just("\r\n"), Just("\r")],
    )
        .prop_map(|(i, b, t)| format!("{i}{b}{t}"))
}

proptest! {
    #[test]
    fn prop_indent_dedent_balanced(lines in proptest::collection::vec(line(), 0..40),
                                   v5 in any::<bool>()) {
        let src: String = lines.concat();
        let d = if v5 { V5 } else { V6 };
        let (lexed, _, _) = lex_with(&src, d, &POLICY);
        let mut open = 0i64;
        for t in &lexed.toks {
            match t.kind {
                TokKind::Indent => open += 1,
                TokKind::Dedent => open -= 1,
                _ => {}
            }
            prop_assert!(open >= 0);
        }
        prop_assert_eq!(open, 0);
    }

    #[test]
    fn prop_stream_ends_with_dedents_then_eof(lines in proptest::collection::vec(line(), 0..40)) {
        let src: String = lines.concat();
        let (lexed, _, _) = lex_with(&src, V6, &POLICY);
        let t = &lexed.toks;
        prop_assert!(!t.is_empty());
        prop_assert_eq!(t[t.len() - 1].kind, TokKind::Eof);
        let mut k = t.len() - 1;
        while k > 0 && t[k - 1].kind == TokKind::Dedent {
            k -= 1;
        }
        // Before the trailing Dedents: nothing, or the last line's Newline.
        prop_assert!(k == 0 || t[k - 1].kind == TokKind::Newline);
        prop_assert_eq!(t.iter().filter(|x| x.kind == TokKind::Eof).count(), 1);
    }
}
