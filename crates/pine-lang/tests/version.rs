// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The version gate (§4.3, §7.1).

use pine_ir::{Dialect, FileId, SymTable};
use pine_lang::{parse, version_of, VersionProbe};

/// The codes of a refused parse, or the dialect of an accepted one.
fn gate(src: &str) -> Result<Dialect, Vec<u16>> {
    let mut syms = SymTable::new();
    match parse(src, FileId::ROOT, &mut syms) {
        Ok(ast) => Ok(ast.dialect()),
        Err(d) => Err(d.iter().map(|x| x.code.0).collect()),
    }
}

/// The table of `version_gate_table` and `probe_agrees_with_parse`.
const TABLE: [(&str, Result<Dialect, u16>); 20] = [
    ("//@version=6\n", Ok(Dialect::V6)),
    ("//@version=5\n", Ok(Dialect::V5)),
    ("//@version=1\n", Err(3001)),
    ("//@version=2\n", Err(3001)),
    ("//@version=3\n", Err(3001)),
    ("//@version=4\n", Err(3001)),
    ("// no annotation\n", Err(3001)),
    ("//@version=0\n", Err(2101)),
    ("//@version=7\n", Err(2101)),
    ("//@version=99\n", Err(2101)),
    ("//@version=99999999999\n", Err(2101)),
    ("//@version\n", Err(2103)),
    ("//@version=\n", Err(2103)),
    ("//@version=abc\n", Err(2103)),
    ("//@version = 6\n", Err(2103)),
    ("//@version=6x\n", Err(2103)),
    ("//@version=5\n//@version=6\n", Err(2105)),
    ("//@version=6\n//@version=6\n", Ok(Dialect::V6)),
    ("//@version=6  \t\n", Ok(Dialect::V6)),
    ("", Err(3001)),
];

#[test]
fn version_gate_table() {
    for (src, want) in TABLE {
        let got = gate(src);
        match want {
            Ok(d) => assert_eq!(got, Ok(d), "{src:?}"),
            Err(c) => assert_eq!(got, Err(vec![c]), "{src:?}"),
        }
    }
}

#[test]
fn pragma_is_read_anywhere() {
    let mut line40 = "// filler\n".repeat(39);
    line40.push_str("//@version=6\n");
    let after_call = "indicator(\"a\")\n//@version=6\n";
    let trailing = "x = 1 //@version=6\n";
    for src in [line40.as_str(), after_call, trailing] {
        assert!(
            matches!(
                version_of(src, FileId::ROOT),
                VersionProbe::Supported {
                    dialect: Dialect::V6,
                    ..
                }
            ),
            "{src:?}"
        );
        assert_eq!(gate(src), Ok(Dialect::V6), "{src:?}");
    }
}

#[test]
fn pragma_inside_a_string_is_not_a_pragma() {
    assert_eq!(gate("s = \"//@version=4\"\n"), Err(vec![3001]));
    assert_eq!(gate("s = \"\"\"\n//@version=4\n\"\"\"\n"), Err(vec![3001]));
    assert_eq!(
        version_of("s = '//@version=6'", FileId::ROOT),
        VersionProbe::Missing
    );
}

#[test]
fn refused_version_does_not_lex_the_body() {
    assert_eq!(gate("//@version=4\n@ $ ; { } 1e \"abc\n"), Err(vec![3001]));
}

#[test]
fn probe_agrees_with_parse() {
    for (src, _) in TABLE {
        let probe = version_of(src, FileId::ROOT);
        match gate(src) {
            Ok(d) => assert_eq!(probe.dialect(), Some(d), "{src:?}"),
            Err(c) => assert_eq!(probe.refusal().map(|x| x.0), Some(c[0]), "{src:?}"),
        }
    }
}

#[test]
fn declared_version_is_tradingviews() {
    assert_eq!(VersionProbe::Missing.declared(), Some(1));
    let malformed = version_of("//@version=", FileId::ROOT);
    assert!(matches!(malformed, VersionProbe::Malformed { .. }));
    assert_eq!(malformed.declared(), None);
    assert_eq!(version_of("//@version=4", FileId::ROOT).declared(), Some(4));
    assert_eq!(version_of("//@version=6", FileId::ROOT).declared(), Some(6));
}

#[test]
fn source_too_large_is_checked_first() {
    let src = " ".repeat(pine_lang::MAX_SOURCE_BYTES + 1);
    assert_eq!(gate(&src), Err(vec![1015]));
}
