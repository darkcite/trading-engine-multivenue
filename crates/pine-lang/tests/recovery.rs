// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! One-pass error recovery (§4.8, §7.9).

use pine_ir::{Diag, FileId, Severity, SymTable};
use pine_lang::{code, parse};

/// Planted fragments: a complete v6 snippet and the one code it plants.
const PLANTS: [(&str, u16); 19] = [
    ("x = 1 +\n", 2002),
    ("y = 2)\n", 1011),
    ("1 := 2\n", 2021),
    ("z = 3 $ 4\n", 1000),
    ("if c\nw = 0\n", 2005),
    ("break\n", 2010),
    ("switch\n    => 1\n    a => 2\n", 2006),
    ("q = 1\n    r = 2\n", 1008),
    ("k = #GG0000\n", 1003),
    ("import a/b\n", 2012),
    ("if c\n    f(x) => x\n", 2027),
    ("var [a, b] = f()\n", 2028),
    ("v = 1.2.3\n", 1004),
    ("if a\n    if b\n\t    x = 1\n", 1006),
    ("for i = 0 10\n    x = 1\n", 2000),
    ("x =\n", 2002),
    ("switch\n    => 1\n    => 2\n", 2007),
    ("float x\n", 2000),
    ("export 5\n", 2000),
];

fn diags(src: &str) -> Vec<Diag> {
    let mut syms = SymTable::new();
    parse(src, FileId::ROOT, &mut syms)
        .err()
        .unwrap_or_default()
}

fn codes(src: &str) -> Vec<u16> {
    diags(src).iter().map(|d| d.code.0).collect()
}

/// Every diagnostic is an error with a table code, sorted, within the source (I14, I17).
fn check_well_formed(src: &str) {
    let d = diags(src);
    let mut prev = (0u32, 0u16);
    for x in &d {
        assert_eq!(x.sev, Severity::Error);
        assert!(code::ALL.contains(&x.code), "{:?} not in the table", x.code);
        assert!(x.span.hi as usize <= src.len() && x.span.lo <= x.span.hi);
        if x.code != code::TOO_MANY_ERRORS {
            assert!((x.span.lo, x.code.0) >= prev);
            prev = (x.span.lo, x.code.0);
        }
    }
}

#[test]
fn planted_errors_counted_exactly() {
    let ks = [1usize, 1, 2, 2, 3, 3, 4, 5, 6, 8, 12, 19];
    let mut start = 0usize;
    for k in ks {
        let mut src = String::from("//@version=6\n");
        let mut want = Vec::new();
        let mut i = 0usize;
        while i < k {
            let (frag, c) = PLANTS[(start + i) % PLANTS.len()];
            src.push_str(frag);
            want.push(c);
            i += 1;
        }
        start += 3;
        let mut got = codes(&src);
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "{src}");
        check_well_formed(&src);
    }
    assert_eq!(codes("//@version=5\nonce\n    x = 1\n"), vec![2104]);
}

#[test]
fn recovery_never_crosses_a_dedent() {
    assert_eq!(
        codes("//@version=6\nif c\n    x = 1 +\n    continue\n"),
        vec![2002, 2010]
    );
}

#[test]
fn recovery_in_argument_list() {
    assert_eq!(codes("//@version=6\nf(1, , 3)\n"), vec![2002]);
}

#[test]
fn unclosed_paren_reports_once() {
    assert_eq!(codes("//@version=6\nx = f(1, 2\n"), vec![1010]);
}

#[test]
fn lexer_errors_are_not_reported_twice() {
    assert_eq!(codes("//@version=6\nx = 3 $ 4\n"), vec![1000]);
}

#[test]
fn one_diagnostic_per_position() {
    let d = diags("//@version=6\nx = (1 +\n");
    let mut los: Vec<u32> = d.iter().map(|x| x.span.lo).collect();
    let n = los.len();
    los.dedup();
    assert_eq!(los.len(), n);
}

#[test]
fn diag_budget_truncates_cleanly() {
    let src = format!("//@version=6\n{}", "x = 1 +\n".repeat(400));
    let d = diags(&src);
    assert_eq!(d.len(), pine_lang::MAX_DIAGS);
    assert_eq!(d[d.len() - 1].code, code::TOO_MANY_ERRORS);
    check_well_formed(&src);
}

#[test]
fn deep_nesting_is_refused_not_overflowed() {
    let run = || {
        let parens = format!(
            "//@version=6\n{}a{}\n",
            "(".repeat(100_000),
            ")".repeat(100_000)
        );
        assert_eq!(codes(&parens), vec![2026]);
        let chain = format!("//@version=6\n{}\n", vec!["a"; 100_000].join(" + "));
        assert_eq!(codes(&chain), vec![2026]);
        let neg = format!("//@version=6\n{}a\n", "-".repeat(10_000));
        assert_eq!(codes(&neg), vec![2026]);
        let tern = format!(
            "//@version=6\n{}b{}\n",
            "a ? ".repeat(1_000),
            " : c".repeat(1_000)
        );
        assert_eq!(codes(&tern), vec![2026]);
    };
    let h = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(run)
        .expect("spawn");
    h.join().expect("no overflow, no panic");
}

#[test]
fn every_diag_is_an_error_with_a_table_code() {
    for (frag, _) in PLANTS {
        check_well_formed(&format!("//@version=6\n{frag}"));
    }
    check_well_formed("//@version=4\n");
    check_well_formed("//@version=6\nx = 1 +\n@\n}\n");
}

proptest::proptest! {
    #[test]
    fn prop_parse_total_on_arbitrary_text(s in ".*") {
        for src in [s.clone(), format!("//@version=6\n{s}")] {
            let d = diags(&src);
            for x in &d {
                proptest::prop_assert!(x.span.hi as usize <= src.len());
            }
        }
    }
}
