// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `scan.rs`.

mod common;

use common::ind;
use pine_corpus::scan::{scan, LibKey};
use pine_ir::{DiagCode, ScriptKind, Sym};
use proptest::prelude::*;

fn uses(s: &pine_corpus::scan::Scan, full: &str) -> (u32, u32, u32) {
    s.uses
        .iter()
        .find(|u| u.full == full)
        .map_or((0, 0, 0), |u| (u.calls, u.values, u.types))
}

#[test]
fn scan_parses_through_a_caller_owned_symtable() {
    let s = scan(&ind("x = input.int(3, \"Len\")\nplot(close * x)"));
    assert!(s.parse_codes.is_empty());
    assert_eq!(s.syms.resolve(Sym(0)), "");
    for i in &s.inputs {
        let _ = s.syms.resolve(i.desc.name);
        let _ = s.syms.resolve(i.desc.title);
    }
    assert_eq!(s.syms.resolve(s.inputs[0].desc.title), "Len");
    assert_eq!(s.syms.resolve(s.inputs[0].desc.name), "x");
}

#[test]
fn scan_parse_failure_records_the_parse_codes() {
    let s = scan("//@version=5\nindicator(\"t\"\nplot(\n");
    assert!(!s.parse_codes.is_empty());
    let mut sorted = s.parse_codes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(s.parse_codes, sorted);
    assert!(s.h_exact.is_none() && s.h_fork.is_none() && s.dialect.is_none());
}

#[test]
fn scan_v4_script_records_3001() {
    let s = scan("//@version=4\nstudy(\"t\")\nplot(close)\n");
    assert!(s.parse_codes.contains(&DiagCode::sema(1)));
}

#[test]
fn scan_declared_kind_is_read_from_the_declaration_statement() {
    assert_eq!(scan(&ind("plot(close)")).kind, Some(ScriptKind::Indicator));
    let s = scan("//@version=5\nstrategy(\"s\")\nstrategy.risk.allow_entry_in(strategy.direction.long)\nplot(close)\n");
    assert_eq!(s.kind, Some(ScriptKind::Strategy));
    let l = scan("//@version=5\nlibrary(\"L\")\nexport f(float x) =>\n    x\n");
    assert_eq!(l.kind, Some(ScriptKind::Library));
}

#[test]
fn scan_missing_or_repeated_declaration_gives_no_kind() {
    assert_eq!(scan("//@version=5\nplot(close)\n").kind, None);
    assert_eq!(
        scan("//@version=5\nindicator(\"a\")\nindicator(\"b\")\nplot(close)\n").kind,
        None
    );
}

#[test]
fn scan_imports_are_recorded_in_source_order() {
    let s = scan("//@version=5\nindicator(\"t\")\nimport bob/Helpers/1 as h\nimport alice/Utils/3\nplot(h.quad(Utils.double(close)))\n");
    assert_eq!(
        s.imports,
        vec![
            LibKey {
                user: "bob".into(),
                lib: "Helpers".into(),
                ver: 1
            },
            LibKey {
                user: "alice".into(),
                lib: "Utils".into(),
                ver: 3
            },
        ]
    );
    // The implicit alias `Utils` binds: its members are library members, not unknown names.
    assert!(s.unknown.is_empty(), "{:?}", s.unknown);
}

#[test]
fn scan_binding_hoists_functions_and_types() {
    let s = scan(&ind("plot(f(close))\nf(x) =>\n    x * 2"));
    assert!(s.unknown.is_empty(), "{:?}", s.unknown);
}

#[test]
fn scan_binding_uses_the_latest_preceding_declaration() {
    // Inside the block the inner `close2` shadows; outside, the outer one is read.
    let src = ind("close2 = close\nif true\n    close2 = open\n    plot(close2)\nplot(close2)");
    let s = scan(&src);
    assert!(s.unknown.is_empty());
    let mut a = Vec::new();
    let mut b = Vec::new();
    pine_corpus::canon::stream(&src, pine_corpus::Profile::Exact, &mut a).unwrap();
    // Renaming only the inner binding leaves the stream unchanged (ordinals, not names).
    let src2 = src.replace(
        "    close2 = open\n    plot(close2)",
        "    zz = open\n    plot(zz)",
    );
    pine_corpus::canon::stream(&src2, pine_corpus::Profile::Exact, &mut b).unwrap();
    assert_eq!(a, b);
}

#[test]
fn scan_counts_calls_values_and_types_separately() {
    let s = scan(&ind(
        "a = array.new<float>()\nfloat x = close\nplot(ta.sma(close, 10))",
    ));
    assert_eq!(uses(&s, "close"), (0, 2, 0));
    assert_eq!(uses(&s, "ta.sma"), (1, 0, 0));
    assert_eq!(uses(&s, "float"), (0, 0, 2));
    assert_eq!(uses(&s, "array.new"), (1, 0, 0));
}

#[test]
fn scan_method_call_on_a_typed_receiver_resolves_to_the_namespace() {
    let s = scan(&ind(
        "array<float> a = array.new<float>()\na.push(close)\nplot(close)",
    ));
    assert_eq!(uses(&s, "array.push"), (1, 0, 0));
    assert!(s.methods_unresolved.is_empty());
}

#[test]
fn scan_user_method_overriding_a_builtin_is_not_counted() {
    let s = scan(&ind(
        "method push(array<float> self, float v) =>\n    v\narray<float> a = array.new<float>()\na.push(close)\nplot(close)",
    ));
    assert_eq!(uses(&s, "array.push"), (0, 0, 0));
}

#[test]
fn scan_untyped_receiver_goes_to_the_question_mark_row() {
    let s = scan(&ind("f() => 1\nx = f()\nx.push(1)\nplot(close)"));
    assert_eq!(s.methods_unresolved, vec!["?.push".to_string()]);
}

#[test]
fn scan_unknown_namespace_member_is_reported() {
    let s = scan(&ind("plot(ta.nosuch(close))"));
    assert_eq!(s.unknown, vec!["ta.nosuch".to_string()]);
}

#[test]
fn scan_name_outside_the_scripts_dialect_is_unknown() {
    // `ta.rci` and `currency.PLN` are v6-only rows of pine-spec.
    let s5 = scan("//@version=5\nindicator(\"t\")\nx = currency.PLN\nplot(ta.rci(close, 10))\n");
    assert_eq!(
        s5.unknown,
        vec!["currency.PLN".to_string(), "ta.rci".to_string()]
    );
    let s6 = scan("//@version=6\nindicator(\"t\")\nx = currency.PLN\nplot(ta.rci(close, 10))\n");
    assert!(s6.unknown.is_empty(), "{:?}", s6.unknown);
}

#[test]
fn scan_library_members_are_not_builtins() {
    let s = scan("//@version=5\nindicator(\"t\")\nimport alice/Utils/3 as u\nplot(u.fn(close))\n");
    assert!(s.unknown.is_empty());
    assert!(s.uses.iter().all(|u| !u.full.contains("fn")));
}

#[test]
fn scan_plot_counts_split_top_level_from_nested() {
    let s = scan(&ind(
        "plot(close)\np = plot(open)\nif true\n    plot(high)\nhline(50)",
    ));
    assert_eq!(s.plots.plot_top, 2);
    assert_eq!(s.plots.plot_nested, 1);
    assert_eq!(s.plots.hline, 1);
}

#[test]
fn scan_every_fixture_parses() {
    for e in std::fs::read_dir(common::fixtures()).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "pine") {
            let s = scan(&std::fs::read_to_string(&p).unwrap());
            assert!(
                s.parse_codes.is_empty(),
                "{}: {:?}",
                p.display(),
                s.parse_codes
            );
            assert!(s.unknown.is_empty(), "{}: {:?}", p.display(), s.unknown);
        }
    }
}

const TOKENS: [&str; 24] = [
    "//@version=5\n",
    "//@version=6\n",
    "indicator(\"t\")\n",
    "plot(",
    "close",
    ")",
    "\n",
    "    ",
    "x",
    " = ",
    "if ",
    "for i = 0 to 3",
    "=> ",
    "ta.sma(",
    ", ",
    "1",
    "1.5",
    "\"s\"",
    "[",
    "]",
    "import a/b/1 as q\n",
    "q.f(",
    "method m(float s) =>",
    ".",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(debug_assertions) { 500 } else { 10_000 }))]
    #[test]
    fn scan_never_panics(seq in proptest::collection::vec(0usize..TOKENS.len(), 0..40)) {
        let mut src = String::from("//@version=5\nindicator(\"t\")\n");
        for i in seq {
            src.push_str(TOKENS[i]);
        }
        let _ = scan(&src);
    }
}
