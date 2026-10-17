// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.2 — pass 3's types and qualifiers. An accepted script is an `Ok` program (verified
//! by `common`) or `[NOT_IMPLEMENTED]` for a back-half gap (`passes`); the fold values and the materialised temporaries are asserted on pass 3's
//! output by the private twins in `src/tests/infer.rs`.

mod common;

use common::*;
use pine_ir::{Dialect, ExecMode};

fn script(d: Dialect, body: &str) -> String {
    format!("//@version={}\nindicator(\"t\")\n{body}", d as u8)
}

fn run(d: Dialect, body: &str) -> Vec<u16> {
    codes(&compile_src(
        &script(d, body),
        &default_opts(d, ExecMode::BarClose),
    ))
}

#[track_caller]
fn accepted(d: Dialect, body: &str) {
    let c = run(d, body);
    assert!(passes(&c), "v{}: {body}: {c:?}", d as u8);
}

#[track_caller]
fn refused(d: Dialect, body: &str, code: u16) {
    let c = run(d, body);
    assert_eq!(c.first(), Some(&code), "v{}: {body}: {c:?}", d as u8);
}

const V5: Dialect = Dialect::V5;
const V6: Dialect = Dialect::V6;

#[test]
fn infer_literal_qualifiers() {
    for d in [V5, V6] {
        accepted(
            d,
            "f(const int a, const float b, const bool c, const string s, const color k) => a\n\
             plot(f(1, 1.5, true, \"s\", #FF0000))\n",
        );
    }
}

#[test]
fn infer_input_is_input_qualified_and_source_is_series() {
    let probes = "s(simple int a) => a\nk(const int a) => a\nfs(simple float a) => a\n";
    accepted(V6, &format!("{probes}plot(s(input.int(5)))\n"));
    refused(V6, &format!("{probes}plot(k(input.int(5)))\n"), 3008);
    refused(
        V6,
        &format!("{probes}plot(fs(input.source(close)))\n"),
        3008,
    );
}

#[test]
fn infer_binary_join_is_max() {
    // const, input, simple, series int sources; the probe of each qualifier admits it, the next
    // weaker refuses it (`input` has no keyword: `plot`'s `linewidth` is `input int`).
    let src = ["1", "input.int(1)", "timeframe.multiplier", "bar_index"];
    let admit = ["pc(e)", "plot(close, linewidth = e)", "ps(e)", "pr(e)"];
    let probes = "pc(const int v) => v\nps(simple int v) => v\npr(series int v) => v\n";
    for i in 0..4 {
        for j in 0..4 {
            let q = i.max(j);
            let body = format!("{probes}e = {} + {}\nplot(close)\n", src[i], src[j]);
            accepted(V6, &format!("{body}x = {}\n", admit[q]));
            if q > 0 {
                refused(V6, &format!("{body}x = {}\n", admit[q - 1]), 3008);
            }
        }
    }
}

#[test]
fn infer_int_plus_float_widens_to_float() {
    // The result is a float (`F_I2F` then `F_ADD` is asserted by T19's emission tests).
    accepted(V6, "float x = 1 + 1.5\nplot(x)\n");
    refused(V6, "int x = 1 + 1.5\nplot(x)\n", 3007);
}

#[test]
fn infer_int_div_int_v5_const_truncates() {
    accepted(V5, "const int x = 5 / 2\nplot(x)\n");
}

#[test]
fn infer_int_div_int_v5_series_is_float() {
    refused(V5, "int x = bar_index / 2\nplot(x)\n", 3007);
    accepted(V5, "float x = bar_index / 2\nplot(x)\n");
}

#[test]
fn infer_int_div_int_v6_is_float() {
    refused(V6, "const int x = 5 / 2\nplot(x)\n", 3007);
    accepted(V6, "const float x = 5 / 2\nplot(x)\n");
}

#[test]
fn infer_float_to_int_assignment_is_type_mismatch() {
    for d in [V5, V6] {
        refused(d, "x = 0\nx := 1.5\nplot(x)\n", 3007);
    }
}

#[test]
fn infer_int_to_float_assignment_widens() {
    for d in [V5, V6] {
        accepted(d, "float x = 0\nx := 1\nplot(x)\n");
    }
}

#[test]
fn infer_mutable_variable_is_series_v6() {
    refused(
        V6,
        "x = 5\nx := 6\nf(simple int a) => a\nplot(f(x))\n",
        3008,
    );
    accepted(V6, "x = 5\nf(simple int a) => a\nplot(f(x))\n");
}

#[test]
fn infer_mutable_variable_v5_follows_var_qualifier_pin() {
    // The default reading (`Series`); `Join` is `readings_var_qualifier_both`.
    refused(
        V5,
        "x = 5\nx := 6\nf(simple int a) => a\nplot(f(x))\n",
        3008,
    );
}

#[test]
fn infer_const_declaration_refuses_series_init() {
    for d in [V5, V6] {
        refused(d, "const float x = close\nplot(x)\n", 3008);
    }
}

#[test]
fn infer_untyped_na_is_refused() {
    for d in [V5, V6] {
        refused(d, "x = na\nplot(close)\n", 3011);
    }
}

#[test]
fn infer_typed_na_declaration_is_accepted() {
    for d in [V5, V6] {
        accepted(d, "float x = na\nplot(x)\n");
    }
}

#[test]
fn infer_na_takes_type_from_parameter() {
    for d in [V5, V6] {
        accepted(d, "f(int a) => a\nplot(f(na))\n");
    }
}

#[test]
fn infer_v6_bool_na_is_type_mismatch() {
    refused(V6, "bool n = na\nplot(close)\n", 3007);
    accepted(V5, "bool n = na\nplot(close)\n");
}

#[test]
fn infer_v6_numeric_to_bool_needs_cast() {
    refused(V6, "x = 0\nif bar_index\n    x := 1\nplot(x)\n", 3007);
    accepted(V6, "x = 0\nif bool(bar_index)\n    x := 1\nplot(x)\n");
}

#[test]
fn infer_v5_numeric_to_bool_is_implicit() {
    accepted(V5, "x = 0\nif bar_index\n    x := 1\nplot(x)\n");
}

#[test]
fn infer_v6_unique_param_rejects_na() {
    let body = "s = if close > open\n    plot.style_line\nplot(close, style = s)\n";
    refused(V6, body, 3007);
    accepted(V5, body);
    accepted(
        V6,
        "s = if close > open\n    plot.style_line\nelse\n    plot.style_area\nplot(close, style = s)\n",
    );
}

#[test]
fn infer_history_on_expression_materialises_a_temp() {
    // The temporary is asserted on pass 3's output (src/tests/infer.rs); its ring by T17.
    for d in [V5, V6] {
        accepted(d, "a = close\nb = open\nplot((a + b)[1])\n");
    }
}

#[test]
fn infer_history_on_expression_inside_conditional_is_refused() {
    for d in [V5, V6] {
        refused(
            d,
            "x = 0.0\nif close > open\n    x := (close + open)[1]\nplot(x)\n",
            3039,
        );
        refused(
            d,
            "x = close > open ? (close + open)[1] : 0.0\nplot(x)\n",
            3039,
        );
    }
}

#[test]
fn infer_history_twice_is_refused() {
    for d in [V5, V6] {
        refused(d, "plot(close[1][2])\n", 3039);
    }
}

#[test]
fn infer_v6_history_on_literal_is_refused() {
    refused(V6, "plot(5[1])\n", 3039);
    refused(V6, "plot(math.pi[1])\n", 3039);
    accepted(V5, "plot(5[1])\n");
}

#[test]
fn infer_v6_history_on_udt_field_is_refused() {
    let body = "type P\n    float x = 0.0\np = P.new()\nplot(p.x[1])\n";
    refused(V6, body, 3039);
    accepted(V5, body);
}

#[test]
fn infer_history_on_reference_is_refused() {
    for d in [V5, V6] {
        refused(
            d,
            "arr = array.new<float>(2)\ny = arr[1]\nplot(close)\n",
            3045,
        );
        refused(
            d,
            "type P\n    float x = 0.0\np = P.new()\nq = p[1]\nplot(close)\n",
            3045,
        );
    }
}

#[test]
fn infer_negative_history_offset_is_type_mismatch() {
    for d in [V5, V6] {
        refused(d, "plot(close[-1])\n", 3007);
        refused(d, "plot(close[1.5])\n", 3007);
    }
}

#[test]
fn infer_nested_collection_is_refused() {
    for d in [V5, V6] {
        refused(d, "array<array<float>> a = na\nplot(close)\n", 3019);
        refused(d, "type P\n    array<float> xs\nplot(close)\n", 3019);
    }
}

#[test]
fn infer_int_literal_beyond_i64_is_type_mismatch() {
    for d in [V5, V6] {
        refused(d, "x = 9223372036854775808\nplot(close)\n", 3007);
        accepted(d, "x = -9223372036854775808\nplot(close)\n");
    }
}

/// The lane's end-to-end target (an SMA cross with `strategy.entry` / `strategy.close`) passes
/// passes 1–3 in both dialects; rows pine-std does not serve yet are filtered by `codes`.
#[test]
fn infer_sma_cross_strategy_types() {
    for d in [V5, V6] {
        let src = format!(
            "//@version={}\nstrategy(\"SMA cross\", overlay = true)\n\
             fastLen = input.int(10, \"Fast\", minval = 1)\nslowLen = input.int(30, \"Slow\")\n\
             fast = ta.sma(close, fastLen)\nslow = ta.sma(close, slowLen)\n\
             if ta.crossover(fast, slow)\n    strategy.entry(\"L\", strategy.long)\n\
             if ta.crossunder(fast, slow)\n    strategy.close(\"L\")\nplot(fast)\nplot(slow)\n",
            d as u8
        );
        let r = compile_src(&src, &default_opts(d, ExecMode::BarClose));
        assert_eq!(codes(&r), vec![ACCEPTED], "v{}: {r:?}", d as u8);
    }
}

/// `pin:02-once-is-void` (delta report 2026-10-17 §2.3): a user function whose body ends in `once`
/// is `void`; called as a statement it compiles end to end (was `3255` at lowering).
const ONCE_TAIL: &str = "f() =>\n    x = 0\n    once close > open\n        x := 1\n";

#[test]
fn infer_once_is_void_function_ending_in_once_compiles() {
    for body in [
        format!("{ONCE_TAIL}f()\nplot(close)\n"),
        format!("{ONCE_TAIL}g() => f()\ng()\nplot(close)\n"),
        "x = 0\nonce\n    x := 1\nplot(x)\n".to_owned(),
    ] {
        let r = compile_src(&script(V6, &body), &default_opts(V6, ExecMode::BarClose));
        assert_eq!(codes(&r), vec![ACCEPTED], "{body}: {r:?}");
    }
}

/// `pin:02-once-is-void`: assigning the call (plain, typed, `:=`, through a forwarding function)
/// or returning it as a tuple element is `3007`.
#[test]
fn infer_once_is_void_assigning_its_call_is_3007() {
    for tail in [
        "y = f()\nplot(close)\n",
        "float y = f()\nplot(close)\n",
        "y = 0\ny := f()\nplot(y)\n",
        "g() => f()\nz = g()\nplot(close)\n",
        "h() => [close, f()]\n[a, b] = h()\nplot(a)\n",
    ] {
        refused(V6, &format!("{ONCE_TAIL}{tail}"), 3007);
    }
}
