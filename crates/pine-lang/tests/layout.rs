// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Branch membership through the canonical print (§4.5, §7.2).

use pine_ir::{FileId, SymTable};
use pine_lang::{parse, print};

/// The canonical print of a v`v` source body, or its codes.
fn printed(v: u8, body: &str) -> Result<String, Vec<u16>> {
    let src = format!("//@version={v}\n{body}");
    let mut syms = SymTable::new();
    match parse(&src, FileId::ROOT, &mut syms) {
        Ok(ast) => {
            let mut out = String::new();
            print(&ast, &syms, &mut out).expect("print");
            Ok(out
                .trim_start_matches(&format!("//@version={v}\n"))
                .to_string())
        }
        Err(d) => Err(d.iter().map(|x| x.code.0).collect()),
    }
}

#[test]
fn layout_membership_table() {
    let ok: [(u8, &str, &str); 14] = [
        (
            6,
            "if c\n    x := 1\ny := 2\n",
            "if c\n    x := 1\ny := 2\n",
        ),
        (
            6,
            "if a\n    if b\n        x := 1\n    y := 2\n",
            "if a\n    if b\n        x := 1\n    y := 2\n",
        ),
        (
            6,
            "if a\n    if b\n        x := 1\ny := 2\n",
            "if a\n    if b\n        x := 1\ny := 2\n",
        ),
        (
            6,
            "if a\n    x := 1\n\n    y := 2\n",
            "if a\n    x := 1\n    y := 2\n",
        ),
        (
            6,
            "if a\n    x := 1\n// note\n    y := 2\n",
            "if a\n    x := 1\n    y := 2\n",
        ),
        (6, "x = a +\n  b\n", "x = (a + b)\n"),
        (
            6,
            "if c\n    x = a +\n      b\n    y = 1\n",
            "if c\n    x = (a + b)\n    y = 1\n",
        ),
        (6, "x = f(a,\nb)\n", "x = f(a, b)\n"),
        (6, "x = f(a,\n    b)\n", "x = f(a, b)\n"),
        (6, "if a\n\tx := 1\n", "if a\n    x := 1\n"),
        (6, "if a\n    x := 1", "if a\n    x := 1\n"),
        (
            6,
            "if a\r\n    x := 1\r\ny := 2\r",
            "if a\n    x := 1\ny := 2\n",
        ),
        (6, "f(x) =>\n    x + 1\n", "f(x) =>\n    (x + 1)\n"),
        (6, "x = a +\n // c\n  b\n", "x = (a + b)\n"),
    ];
    for (v, src, want) in ok {
        assert_eq!(printed(v, src), Ok(want.to_string()), "{src:?}");
    }
    let refused: [(u8, &str, &[u16]); 6] = [
        (5, "x = f(a,\n    b)\n", &[2104]),
        (6, "x = a +\n    b\n", &[1008]),
        (6, "  x = 1\n", &[1008]),
        (6, "if a\n        x := 1\n    y := 2\n", &[1009]),
        (6, "if a\n    if b\n\t    x := 1\n", &[1006]),
        (6, "x = 1\n    y = 2\n    z = 3\n", &[1008]),
    ];
    for (v, src, want) in refused {
        assert_eq!(printed(v, src), Err(want.to_vec()), "{src:?}");
    }
    assert_eq!(printed(6, ""), Ok(String::new()));
}

#[test]
fn wrapped_lines_never_open_blocks() {
    for col in [1usize, 2, 3, 5, 6, 7, 9, 10, 11] {
        let src = format!("x = a +\n{}b\ny = 1\n", " ".repeat(col));
        assert_eq!(
            printed(6, &src),
            Ok("x = (a + b)\ny = 1\n".to_string()),
            "col {col}"
        );
    }
}

#[test]
fn tab_block_equals_four_space_block() {
    assert_eq!(
        printed(6, "if a\n\tx := 1\n"),
        printed(6, "if a\n    x := 1\n")
    );
}
