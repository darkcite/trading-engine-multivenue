// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-09 §7.2 `canon.rs` (I23, I25, §4.9).

mod common;

use common::{fixture, ind};
use pine_corpus::canon::{stream, DECL_DROP, INPUT_DROP, TAGS};
use pine_corpus::Profile;
use proptest::prelude::*;

fn h(src: &str, p: Profile) -> Vec<u8> {
    let mut b = Vec::new();
    stream(src, p, &mut b).unwrap_or_else(|c| panic!("parse {c:?}:\n{src}"));
    b
}

fn same(a: &str, b: &str, p: Profile) -> bool {
    h(a, p) == h(b, p)
}

#[test]
fn canon_alpha_renaming_is_invisible_on_the_fixtures() {
    for b in ["b01", "b04", "b06", "b10", "b11"] {
        let a = fixture(&format!("fork_{b}.pine"));
        let v = fixture(&format!("fork_{b}_v1.pine"));
        assert!(same(&a, &v, Profile::Fork), "{b}");
    }
    let a = fixture("fork_b04.pine");
    let v = fixture("fork_b04_v1.pine");
    assert!(
        same(&a, &v, Profile::Exact),
        "a pure renaming is Exact-equal"
    );
}

const NAMES: [&str; 6] = ["alpha", "beta", "gamma", "delta", "eps", "zeta"];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(debug_assertions) { 200 } else { 10_000 }))]
    #[test]
    fn canon_alpha_renaming_is_invisible(perm in Just(()).prop_perturb(|_, mut rng| {
        let mut v: Vec<usize> = (0..NAMES.len()).collect();
        for i in (1..v.len()).rev() {
            let j = (rng.next_u32() as usize) % (i + 1);
            v.swap(i, j);
        }
        v
    })) {
        let tpl = "x0 = input.int(5, \"n\")\nx1 = ta.sma(close, x0)\nx2(x3, x4) =>\n    x3 * x4\nfor x5 = 0 to 2\n    x1 := x1 + x5\nplot(x2(x1, 2))";
        let render = |names: &[&str]| {
            let mut t = tpl.to_string();
            for (i, n) in names.iter().enumerate() {
                t = t.replace(&format!("x{i}"), n);
            }
            ind(&t)
        };
        let base = render(&NAMES);
        let names: Vec<&str> = perm.iter().map(|&i| NAMES[i]).collect();
        let renamed = render(&names);
        prop_assert_eq!(h(&base, Profile::Exact), h(&renamed, Profile::Exact));
        prop_assert_eq!(h(&base, Profile::Fork), h(&renamed, Profile::Fork));
    }
}

#[test]
fn canon_free_names_are_never_renamed() {
    let a = "//@version=5\nindicator(\"t\")\nimport alice/Utils/3 as u\nplot(u.double(close))\n";
    let b = "//@version=5\nindicator(\"t\")\nimport alice/Utils/3 as w\nplot(w.double(close))\n";
    let c = "//@version=5\nindicator(\"t\")\nimport alice/Utils/3 as u\nplot(u.triple(close))\n";
    assert!(same(a, b, Profile::Exact));
    assert!(!same(a, c, Profile::Exact));
    assert!(!same(
        &ind("plot(ta.sma(close, 3))"),
        &ind("plot(ta.ema(close, 3))"),
        Profile::Fork
    ));
}

#[test]
fn canon_literal_forms_canonicalise_to_one_encoding() {
    let a = ind("plot(close * 1.0)");
    assert!(same(&a, &ind("plot(close * 1.)"), Profile::Exact));
    assert!(same(&a, &ind("plot(close * 10e-1)"), Profile::Exact));
}

#[test]
fn canon_int_literal_is_not_a_float_literal() {
    assert!(!same(
        &ind("plot(close * 1)"),
        &ind("plot(close * 1.0)"),
        Profile::Exact
    ));
}

#[test]
fn canon_positional_and_named_arguments_hash_alike() {
    assert!(same(
        &ind("plot(close, \"t\")"),
        &ind("plot(close, title = \"t\")"),
        Profile::Exact
    ));
    assert!(same(
        &ind("plot(ta.vwma(close, 20))"),
        &ind("plot(ta.vwma(source = close, length = 20))"),
        Profile::Exact
    ));
    assert!(same(
        &ind("plot(ta.vwma(length = 20, source = close))"),
        &ind("plot(ta.vwma(close, 20))"),
        Profile::Exact
    ));
}

#[test]
fn canon_unsure_positions_stay_positional() {
    // A variadic position: `math.max` takes `number0, number1, ...`; a position at or past the
    // variadic never takes a name, so a 4-argument call keeps its count of positionals.
    let mut b = Vec::new();
    stream(
        &ind("plot(math.max(1, 2, 3, close))"),
        Profile::Exact,
        &mut b,
    )
    .unwrap();
    let mut r = String::new();
    pine_corpus::render(&b, &mut r);
    assert!(r.contains("FreePath \"math.max\""), "{r}");
    // A name-only entry (no overloads): every argument stays positional.
    let mut b = Vec::new();
    stream(
        &ind("x = line.new(1, 2, 3, 4)\nplot(close)"),
        Profile::Exact,
        &mut b,
    )
    .unwrap();
    let mut r = String::new();
    pine_corpus::render(&b, &mut r);
    assert!(r.contains("(Call pos=4"), "{r}");
}

#[test]
fn canon_method_call_equals_function_call() {
    let a = ind("array<float> a = array.new<float>()\na.push(close)\nplot(close)");
    let b = ind("array<float> a = array.new<float>()\narray.push(a, close)\nplot(close)");
    assert!(same(&a, &b, Profile::Exact));
}

#[test]
fn canon_folds_literal_arithmetic_only() {
    assert!(same(
        &ind("plot(ta.sma(close, 7 * 2))"),
        &ind("plot(ta.sma(close, 14))"),
        Profile::Exact
    ));
    assert!(same(
        &ind("plot(close + (2 - 7))"),
        &ind("plot(close + -5)"),
        Profile::Exact
    ));
    assert!(!same(
        &ind("len = 7\nplot(ta.sma(close, len * 2))"),
        &ind("len = 7\nplot(ta.sma(close, 14))"),
        Profile::Exact
    ));
    assert!(same(
        &ind("plot(close * (1.5 + 1))"),
        &ind("plot(close * 2.5)"),
        Profile::Exact
    ));
    assert!(same(
        &ind("x = \"a\" + \"b\"\nplot(close)"),
        &ind("x = \"ab\"\nplot(close)"),
        Profile::Exact
    ));
}

#[test]
fn canon_never_folds_int_division_or_modulo() {
    for d in [5u8, 6] {
        let v = |b: &str| format!("//@version={d}\nindicator(\"t\")\nplot(close * {b})\n");
        assert!(!same(&v("(7 / 2)"), &v("3"), Profile::Exact));
        assert!(!same(&v("(7 / 2)"), &v("3.5"), Profile::Exact));
        assert!(!same(&v("(7 % 2)"), &v("1"), Profile::Exact));
    }
}

#[test]
fn canon_never_folds_division_by_zero() {
    assert!(!same(
        &ind("plot(close * (1.0 / 0.0))"),
        &ind("plot(close * 1.0)"),
        Profile::Exact
    ));
    let mut b = Vec::new();
    stream(&ind("plot(close * (1.0 / 0.0))"), Profile::Exact, &mut b).unwrap();
    let mut r = String::new();
    pine_corpus::render(&b, &mut r);
    assert!(r.contains("(Binary 3)"), "{r}");
}

#[test]
fn canon_never_reorders_and_or() {
    let a = ind("a = close > open\nb = high > low\nplot(a and b ? 1 : 0)");
    let b = ind("a = close > open\nb = high > low\nplot(b and a ? 1 : 0)");
    assert!(!same(&a, &b, Profile::Exact));
    assert!(!same(&a, &b, Profile::Fork));
}

#[test]
fn canon_dialect_separates_exact_but_not_fork() {
    let a = fixture("fork_b12.pine");
    let b = fixture("fork_b12_v2.pine");
    assert!(!same(&a, &b, Profile::Exact));
    assert!(same(&a, &b, Profile::Fork));
}

#[test]
fn canon_optional_capture_args_are_dropped_only_under_fork() {
    let a = ind("plot(close, \"a\", color = color.red, linewidth = 2)");
    let b = ind("plot(close)");
    assert!(same(&a, &b, Profile::Fork));
    assert!(!same(&a, &b, Profile::Exact));
}

#[test]
fn canon_fork_keeps_the_plot_series_and_the_hline_price() {
    assert!(!same(
        &ind("plot(close)"),
        &ind("plot(open)"),
        Profile::Fork
    ));
    assert!(!same(&ind("hline(30)"), &ind("hline(70)"), Profile::Fork));
}

#[test]
fn canon_behavioural_declaration_args_are_never_dropped() {
    let base = "//@version=5\nstrategy(\"s\"{X})\nplot(close)\n";
    for arg in [
        ", pyramiding = 2",
        ", calc_on_every_tick = true",
        ", commission_value = 0.1",
        ", default_qty_type = strategy.percent_of_equity",
    ] {
        assert!(
            !same(
                &base.replace("{X}", ""),
                &base.replace("{X}", arg),
                Profile::Fork
            ),
            "{arg}"
        );
    }
    let ib = "//@version=5\nindicator(\"s\"{X})\nplot(close)\n";
    assert!(!same(
        &ib.replace("{X}", ""),
        &ib.replace("{X}", ", timeframe_gaps = false"),
        Profile::Fork
    ));
    assert!(same(
        &ib.replace("{X}", ""),
        &ib.replace("{X}", ", overlay = true, precision = 3"),
        Profile::Fork
    ));
}

#[test]
fn canon_drop_list_names_exist_in_pine_spec() {
    let decl: Vec<String> = ["indicator", "strategy", "library"]
        .iter()
        .flat_map(|e| param_names(e))
        .collect();
    for n in DECL_DROP {
        assert!(
            decl.iter().any(|x| x == n),
            "declaration drop-list name `{n}`"
        );
    }
    let inp: Vec<String> = [
        "input",
        "input.int",
        "input.float",
        "input.bool",
        "input.string",
        "input.source",
    ]
    .iter()
    .flat_map(|e| param_names(e))
    .collect();
    for n in INPUT_DROP {
        assert!(inp.iter().any(|x| x == n), "input drop-list name `{n}`");
    }
}

fn param_names(entry: &str) -> Vec<String> {
    let e = pine_spec::lookup(entry).unwrap();
    e.overloads()
        .iter()
        .flat_map(|o| {
            pine_spec::params(o)
                .iter()
                .map(|p| p.name.as_str().to_string())
        })
        .collect()
}

#[test]
fn canon_input_defaults_are_holed_only_under_fork() {
    let a = ind("x = input.int(14)\nplot(close * x)");
    let b = ind("x = input.int(20)\nplot(close * x)");
    assert!(same(&a, &b, Profile::Fork));
    assert!(!same(&a, &b, Profile::Exact));
}

#[test]
fn canon_input_function_is_never_holed() {
    let a = ind("x = input.int(14)\nplot(close * x)");
    let b = ind("x = input.float(14)\nplot(close * x)");
    assert!(!same(&a, &b, Profile::Fork));
    assert!(!same(&a, &b, Profile::Exact));
}

#[test]
fn canon_tag_table_is_frozen() {
    let frozen: [(u8, &str); 40] = [
        (0x01, "Script"),
        (0x02, "Block"),
        (0x03, "VarDecl"),
        (0x04, "TupleDecl"),
        (0x05, "Assign"),
        (0x06, "TupleAssign"),
        (0x07, "If"),
        (0x08, "Switch"),
        (0x09, "SwitchArm"),
        (0x0A, "For"),
        (0x0B, "ForIn"),
        (0x0C, "While"),
        (0x0D, "Break"),
        (0x0E, "Continue"),
        (0x0F, "Once"),
        (0x10, "FuncDecl"),
        (0x11, "Param"),
        (0x12, "TypeDecl"),
        (0x13, "FieldDecl"),
        (0x14, "EnumDecl"),
        (0x15, "EnumMember"),
        (0x16, "Import"),
        (0x17, "TypeRef"),
        (0x20, "IntLit"),
        (0x21, "FloatLit"),
        (0x22, "BoolLit"),
        (0x23, "StrLit"),
        (0x24, "ColorLit"),
        (0x26, "Hole"),
        (0x30, "IdentBound"),
        (0x31, "FreePath"),
        (0x32, "AliasPath"),
        (0x33, "Field"),
        (0x40, "Call"),
        (0x41, "NamedArg"),
        (0x42, "Hist"),
        (0x43, "Unary"),
        (0x44, "Binary"),
        (0x45, "Ternary"),
        (0x46, "Tuple"),
    ];
    assert_eq!(TAGS, frozen);
    assert_eq!(pine_corpus::CANON_VERSION, 1);
    // A pinned stream: any change to the encoding changes this hash and bumps CANON_VERSION.
    let b = h(
        &ind("x = input.int(14, \"n\")\nplot(ta.sma(close, x))"),
        Profile::Exact,
    );
    assert_eq!(pine_corpus::hex64(&pine_corpus::sha256(&b)), PINNED_STREAM);
}

const PINNED_STREAM: &str = "390d42d4c0164569c13bebc8934574a447126048d027d4bd94e7acc75c4aaba2";

#[test]
fn canon_h_fork_collapses_the_fixture_fork_pairs() {
    let mut n = 0;
    for b in 1..=12 {
        let base = fixture(&format!("fork_b{b:02}.pine"));
        for v in 1..=2 {
            let f = fixture(&format!("fork_b{b:02}_v{v}.pine"));
            assert!(same(&base, &f, Profile::Fork), "fork_b{b:02}_v{v}");
            n += 1;
        }
    }
    assert_eq!(n, 24);
}

#[test]
fn canon_h_fork_keeps_the_declared_non_forks_apart() {
    for n in 1..=4 {
        let a = fixture(&format!("nonfork_n{n}_a.pine"));
        let b = fixture(&format!("nonfork_n{n}_b.pine"));
        assert!(!same(&a, &b, Profile::Fork), "nonfork_n{n}");
    }
}

#[test]
fn canon_render_reads_both_profiles() {
    for p in [Profile::Exact, Profile::Fork] {
        let b = h(&fixture("fork_b01.pine"), p);
        let mut r = String::new();
        pine_corpus::render(&b, &mut r);
        assert!(r.starts_with("(Script"), "{r}");
        assert!(!r.contains("not a canonical stream"));
    }
}
