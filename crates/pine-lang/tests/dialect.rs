// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Dialects (§4.11, §7.8): one front end, four v6-only deltas.

use std::fs;
use std::path::Path;

use pine_ir::{Dialect, FileId, SymTable};
use pine_lang::{parse, Ast, Node};

fn parse_v(v: u8, body: &str) -> Result<(Ast, SymTable), Vec<u16>> {
    let mut syms = SymTable::new();
    match parse(&format!("//@version={v}\n{body}"), FileId::ROOT, &mut syms) {
        Ok(a) => Ok((a, syms)),
        Err(d) => Err(d.iter().map(|x| x.code.0).collect()),
    }
}

#[test]
fn v6_only_constructs_are_refused_in_v5() {
    let sd = [
        ("SD1", "x = f(a,\n    b)\n"),
        ("SD1 col 0", "x = f(a,\nb)\n"),
        ("SD2", "library(\"l\")\nexport const float K = 1.0\n"),
        ("SD3", "s = \"\"\"\nab\n\"\"\"\n"),
        ("SD4", "once\n    x = 1\n"),
    ];
    for (id, body) in sd {
        assert_eq!(parse_v(5, body).err(), Some(vec![2104]), "{id} in v5");
        assert!(parse_v(6, body).is_ok(), "{id} in v6");
    }
}

#[test]
fn v5_and_v6_parse_the_common_syntax_identically() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts");
    let mut compared = 0usize;
    for e in fs::read_dir(&dir).expect("scripts dir") {
        let p = e.expect("entry").path();
        if p.extension().is_none_or(|x| x != "pine") {
            continue;
        }
        let src = fs::read_to_string(&p).expect("read");
        let body: String = src
            .lines()
            .filter(|l| !l.starts_with("//@version="))
            .map(|l| format!("{l}\n"))
            .collect();
        let (Ok((a5, s5)), Ok((a6, s6))) = (parse_v(5, &body), parse_v(6, &body)) else {
            continue; // holds an SD construct (refused in v5) — covered above
        };
        assert_eq!(a5.dialect(), Dialect::V5);
        assert_eq!(a6.dialect(), Dialect::V6);
        // Dialect aside (`structurally_eq` compares it): same kinds, widths and names.
        assert_eq!(a5.len(), a6.len(), "{p:?}");
        for i in 0..a5.len() as u32 {
            let i = pine_ir::NodeId(i);
            assert_eq!(a5.kind(i), a6.kind(i), "{p:?}");
            assert_eq!(
                a5.span(i).hi - a5.span(i).lo,
                a6.span(i).hi - a6.span(i).lo,
                "{p:?}"
            );
            if let (
                Node::Ident { sym: x } | Node::Name { sym: x },
                Node::Ident { sym: y } | Node::Name { sym: y },
            ) = (a5.node(i), a6.node(i))
            {
                assert_eq!(s5.resolve(x), s6.resolve(y), "{p:?}");
            }
        }
        compared += 1;
    }
    assert!(compared >= 6, "only {compared} scripts are dialect-neutral");
}

#[test]
fn union_tokenisation_never_misreads_v5() {
    assert!(parse_v(5, "s = '\"\"\"'\n").is_ok());
    assert!(parse_v(5, "once = 1\nx = once + 1\n").is_ok());
    assert!(parse_v(5, "x = \"\"\ny = \"a\"\n").is_ok());
}

#[test]
fn enum_is_not_v6_only() {
    assert!(parse_v(5, "enum E\n    a\n    b\n").is_ok());
}
