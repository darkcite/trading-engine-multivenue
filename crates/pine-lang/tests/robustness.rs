// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The parser-property-tester invariants (impl-01 §7.12): whatever the input, `parse` never
//! panics and never reads out of bounds. The allocation invariants (bounded parse, zero-alloc
//! reads) are measured in `tests/alloc.rs`, a binary of its own.

use std::fs;
use std::path::Path;

use pine_ir::{FileId, SymTable};
use pine_lang::{parse, print, structurally_eq};
use proptest::prelude::*;

/// Every fixture and probe script.
fn fixtures() -> Vec<String> {
    let mut out = Vec::new();
    for sub in ["tests/scripts", "tests/probes"] {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(sub);
        for e in fs::read_dir(&dir).expect("dir") {
            let p = e.expect("entry").path();
            if p.extension().is_some_and(|x| x == "pine") {
                out.push(fs::read_to_string(&p).expect("read"));
            }
        }
    }
    out
}

/// Parse `src`; on `Ok` validate and round-trip, on `Err` check the diagnostics' shape.
fn check_total(src: &str) {
    let mut syms = SymTable::new();
    match parse(src, FileId::ROOT, &mut syms) {
        Ok(a) => {
            assert!(a.validate().is_ok(), "{src:?}");
            let mut text = String::new();
            print(&a, &syms, &mut text).expect("print");
            let mut s2 = SymTable::new();
            let b = parse(&text, FileId::ROOT, &mut s2)
                .unwrap_or_else(|d| panic!("reparse of {text:?} (from {src:?}) failed: {d:?}"));
            assert!(structurally_eq(&a, &syms, &b, &s2), "{src:?}");
        }
        Err(d) => {
            assert!(!d.is_empty() && d.len() <= pine_lang::MAX_DIAGS);
            for x in &d {
                assert!(
                    x.span.lo <= x.span.hi && x.span.hi as usize <= src.len(),
                    "{src:?}"
                );
            }
        }
    }
}

#[test]
fn truncations_of_every_script_never_panic() {
    for src in fixtures() {
        let mut k = 0usize;
        while k <= src.len() {
            if src.is_char_boundary(k) {
                check_total(&src[..k]);
            }
            k += 1;
        }
    }
}

#[test]
fn huge_literals_are_clean() {
    let p = "//@version=6\n";
    let codes = |s: &str| -> Vec<u16> {
        let mut syms = SymTable::new();
        parse(s, FileId::ROOT, &mut syms)
            .err()
            .unwrap_or_default()
            .iter()
            .map(|d| d.code.0)
            .collect()
    };
    assert!(codes(&format!("{p}s = \"{}\"\n", "a".repeat(1 << 20))).is_empty());
    assert_eq!(
        codes(&format!("{p}n = {}\n", "9".repeat(100_000))),
        vec![1005]
    );
    assert!(codes(&format!("{p}{} = 1\n", "v".repeat(200_000))).is_empty());
    assert!(codes(&format!("{p}// {}\nx = 1\n", "c".repeat(1 << 20))).is_empty());
    assert!(codes(&format!("{p}f = 1.{}\n", "0".repeat(100_000))).is_empty());
    assert_eq!(codes(&format!("{p}f = 1e99999\n")), vec![1005]);
    assert_eq!(
        codes(&format!("{p}s = \"{}", "a".repeat(1 << 20))),
        vec![1001]
    );
}

#[test]
fn deep_and_long_constructs_are_refused_not_overflowed() {
    let run = || {
        let p = "//@version=6\n";
        let codes = |s: &str| -> Vec<u16> {
            let mut syms = SymTable::new();
            let mut c: Vec<u16> = parse(s, FileId::ROOT, &mut syms)
                .err()
                .unwrap_or_default()
                .iter()
                .map(|d| d.code.0)
                .collect();
            c.dedup();
            c
        };
        assert_eq!(
            codes(&format!(
                "{p}x = {}1{}\n",
                "[".repeat(100_000),
                "]".repeat(100_000)
            )),
            vec![2026]
        );
        assert_eq!(
            codes(&format!(
                "{p}x = {}1{}\n",
                "f(".repeat(100_000),
                ")".repeat(100_000)
            )),
            vec![2026]
        );
        assert_eq!(
            codes(&format!("{p}x = a{}\n", ".b".repeat(100_000))),
            vec![2026]
        );
        assert_eq!(
            codes(&format!("{p}x = a{}\n", "[0]".repeat(100_000))),
            vec![2026]
        );
        assert_eq!(
            codes(&format!("{p}x = {}a\n", "not ".repeat(100_000))),
            vec![2026]
        );
        let chain = format!(
            "{p}if a\n    x = 1\n{}else\n    x = 3\n",
            "else if b\n    x = 2\n".repeat(10_000)
        );
        assert_eq!(codes(&chain), vec![2026]);
        let mut nested = String::from(p);
        for d in 0..100usize {
            nested.push_str(&" ".repeat(4 * d));
            nested.push_str("if a\n");
        }
        assert!(codes(&nested).contains(&1014));
    };
    let h = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(run)
        .expect("spawn");
    h.join().expect("no overflow, no panic");
}

/// One line with a random terminator and indentation.
fn mixed_line() -> impl Strategy<Value = String> {
    let indent = prop_oneof![
        Just(""),
        Just("\t"),
        Just("    "),
        Just("\t    "),
        Just("  "),
        Just("        ")
    ];
    let body = prop_oneof![
        Just("x = 1"),
        Just("if c"),
        Just("else"),
        Just("for i = 0 to 3"),
        Just("y := x +"),
        Just("f(a,"),
        Just("b)"),
        Just("// c"),
        Just(""),
        Just("s = \"\"\"t"),
        Just("u\"\"\""),
        Just("g(x) =>"),
    ];
    let term = prop_oneof![Just("\n"), Just("\r\n"), Just("\r")];
    (indent, body, term).prop_map(|(i, b, t)| format!("{i}{b}{t}"))
}

proptest! {
    #[test]
    fn byte_soup_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        // Invalid UTF-8 is refused at the gate: `parse` takes `&str`, the caller's conversion fails.
        match core::str::from_utf8(&bytes) {
            Ok(s) => {
                check_total(s);
                check_total(&format!("//@version=6\n{s}"));
            }
            Err(e) => {
                prop_assert!(e.valid_up_to() <= bytes.len());
                let lossy = String::from_utf8_lossy(&bytes);
                check_total(&format!("//@version=6\n{lossy}"));
            }
        }
    }

    #[test]
    fn crlf_and_tab_mixes_never_panic(lines in proptest::collection::vec(mixed_line(), 0..24),
                                      v5 in any::<bool>()) {
        let pragma = if v5 { "//@version=5\r\n" } else { "//@version=6\n" };
        check_total(&format!("{pragma}{}", lines.concat()));
    }
}
