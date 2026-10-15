// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.1 — pass 2, lexical resolution (§4.3). Until T19 a script every existing pass
//! accepts returns only `3255` "not implemented: emission" (§6); `accepted` asserts exactly that.

mod common;

use common::{codes, compile_src, diags, v5, NOT_IMPLEMENTED};

fn script(body: &str) -> String {
    format!("//@version=5\nindicator(\"t\")\n{body}")
}

#[track_caller]
fn accepted(body: &str) {
    assert_eq!(
        codes(&compile_src(&script(body), &v5())),
        vec![NOT_IMPLEMENTED],
        "{body}"
    );
}

#[track_caller]
fn refused(body: &str, code: u16) -> pine_ir::Diag {
    let src = script(body);
    let ds = diags(compile_src(&src, &v5()));
    assert_eq!(ds[0].code.0, code, "{body}: {ds:?}");
    ds.into_iter().next().unwrap()
}

#[test]
fn resolve_global_then_local_shadowing_is_legal() {
    accepted("x = 1.0\nif close > open\n    x = 2.0\n    plot(x)\nplot(x)\n");
}

#[test]
fn resolve_use_before_declaration_is_unknown_name() {
    let src = script("plot(y)\ny = close\n");
    let ds = diags(compile_src(&src, &v5()));
    assert_eq!(ds[0].code.0, 3003);
    assert_eq!(&src[ds[0].span.lo as usize..ds[0].span.hi as usize], "y");
}

#[test]
fn resolve_functions_and_types_are_visible_file_wide() {
    accepted("plot(f(close))\nf(float x) => x * 2\n");
    accepted("var p = P.new()\ntype P\n    float v = 0.0\n");
}

#[test]
fn resolve_fn_body_cannot_see_caller_locals() {
    refused("f() => z\ng() =>\n    z = 1.0\n    f()\nplot(g())\n", 3003);
    refused("f() => z\nz = 1.0\nplot(f())\n", 3003);
}

#[test]
fn resolve_fn_body_can_see_globals() {
    accepted("z = 1.0\nf() => z + close\nplot(f())\n");
}

#[test]
fn resolve_duplicate_decl_in_same_scope_is_refused() {
    let d = refused("x = 1.0\nx = 2.0\nplot(x)\n", 3041);
    assert_eq!(d.notes.len(), 1);
    assert!(d.notes[0].0.lo < d.span.lo);
    refused("f(float a, float a) => a\nplot(f(1, 2))\n", 3041);
}

#[test]
fn resolve_underscore_may_repeat_and_is_unreadable() {
    accepted("[_, b] = [close, open]\n[_, c] = [high, low]\nplot(b + c)\n");
    refused("[_, b] = [close, open]\nplot(_)\n", 3003);
}

#[test]
fn resolve_assign_to_undeclared_is_unknown_name() {
    refused("x := 1\n", 3003);
}

#[test]
fn resolve_assign_to_param_is_refused() {
    refused("f(float a) =>\n    a := 1.0\n    a\nplot(f(close))\n", 3040);
}

#[test]
fn resolve_assign_to_const_is_refused() {
    refused("const float k = 1.0\nk := 2.0\nplot(k)\n", 3040);
}

#[test]
fn resolve_assign_to_for_counter_is_refused() {
    refused(
        "s = 0.0\nfor i = 0 to 3\n    i := 2\n    s += i\nplot(s)\n",
        3040,
    );
}

#[test]
fn resolve_assign_to_function_or_builtin_is_refused() {
    refused("f() => 1.0\nf := 2.0\nplot(f())\n", 3040);
    refused("close := 1.0\n", 3040);
}

#[test]
fn resolve_builtin_name_shadowed_and_used_is_reserved() {
    let d = refused("a = close\nclose = 1.0\nplot(a + close)\n", 3004);
    assert!(d.msg.contains("close"));
}

#[test]
fn resolve_builtin_name_shadowed_and_unused_is_legal() {
    accepted("close = 1.0\nplot(close)\n");
}

#[test]
fn resolve_na_and_type_names_cannot_be_declared() {
    refused("na = 1.0\nplot(na)\n", 3004);
}

#[test]
fn resolve_bare_namespace_in_expression_is_unknown_name() {
    let d = refused("x = ta\nplot(close)\n", 3003);
    assert_eq!(d.notes[0].1, "a namespace is not a value");
}

#[test]
fn resolve_unknown_builtin_path_is_unknown_name() {
    refused("plot(ta.nonesuch(close, 3))\n", 3003);
    accepted("plot(ta.sma(close, 3), color = color.red)\n");
}

#[test]
fn resolve_switch_arm_opens_a_block_scope() {
    refused(
        "switch\n    close > open =>\n        v = 1.0\n        plot(v)\nplot(v)\n",
        3003,
    );
}

#[test]
fn resolve_loop_and_if_bodies_open_block_scopes() {
    refused("for i = 0 to 2\n    w = i\nplot(w)\n", 3003);
    refused("if close > open\n    u = 1.0\nplot(u)\n", 3003);
    accepted("for i = 0 to 2\n    w = i\nfor i = 0 to 2\n    w = i\nplot(close)\n");
}

#[test]
fn resolve_varip_field_is_refused() {
    refused("type P\n    varip float v = 0.0\nplot(close)\n", 3048);
}

#[test]
fn resolve_collecting_pass_reports_every_error() {
    let src = script("plot(nope1)\nplot(nope2)\nx := 1\n");
    assert_eq!(codes(&compile_src(&src, &v5())), vec![3003, 3003, 3003]);
}

#[test]
fn resolve_common_builtins_and_casts_resolve() {
    accepted(
        "a = int(close)\nb = float(a)\nc = nz(hl2, 0.0)\nd = bar_index + last_bar_index\n\
         e = na(close) ? 0.0 : close\nm = array.new<float>(3)\nm.push(1.0)\n\
         plot(math.max(b, c) + d + e, color = color.new(color.red, 50))\n",
    );
}

#[test]
fn resolve_name_without_a_row_in_the_dialect_is_not_in_dialect() {
    // `ask` and `currency.AED` exist only in v6 (pine-spec's transcription).
    refused("plot(ask)\n", 3005);
    refused("c = currency.AED\nplot(close)\n", 3005);
    let v6 = "//@version=6\nindicator(\"t\")\nplot(ask)\n";
    let o = common::default_opts(pine_ir::Dialect::V6, pine_ir::ExecMode::BarClose);
    assert_eq!(codes(&compile_src(v6, &o)), vec![NOT_IMPLEMENTED]);
}

#[test]
fn resolve_fn_assigning_a_global_is_refused_by_default() {
    refused(
        "var g = 0\nf() =>\n    g := g + 1\n    g\nplot(f())\n",
        3040,
    );
    // a global reassigned from an `if` block is documented and legal
    accepted("var g = 0\nif close > open\n    g := g + 1\nplot(g)\n");
    // a function's own local is not a global
    accepted("f() =>\n    l = 0\n    l := 1\n    l\nplot(f())\n");
}

#[test]
fn resolve_user_function_named_like_builtin_is_reserved_by_default() {
    refused("nz(float x) => x + 1\nplot(nz(1.0))\n", 3004);
    // a user method may share a built-in method's name ([methods])
    accepted("method push(array<float> a, float v) => a.size()\nplot(close)\n");
}

#[test]
fn resolve_redeclared_loop_counter_is_refused_by_default() {
    refused(
        "s = 0\nfor i = 0 to 2\n    i = 5\n    s += i\nplot(s)\n",
        3041,
    );
}

#[test]
fn resolve_duplicate_user_overload_is_refused() {
    let d = refused("f(float a) => a\nf(float b) => b * 2\nplot(f(1.0))\n", 3041);
    assert_eq!(d.notes.len(), 1);
    // a different required type, or a different count, is a valid overload
    accepted("f(float a) => a\nf(int a) => a\nf(float a, float b) => a + b\nplot(f(1.0))\n");
    // a parameter with a default is not required
    refused(
        "f(float a) => a\nf(float a, float b = 1.0) => a + b\nplot(f(1.0))\n",
        3041,
    );
    // the qualifier is part of the type
    accepted("f(simple float a) => a\nf(series float a) => a\nplot(close)\n");
}

#[test]
fn resolve_namespace_named_udt_variable_is_reserved() {
    refused(
        "type P\n    float v = 0.0\nP ta = P.new()\nplot(close)\n",
        3004,
    );
}

#[test]
fn resolve_written_types_are_checked() {
    refused("Q q = na\nplot(close)\n", 3003);
    refused("array<array<float>> a = na\nplot(close)\n", 3019);
    refused("type P\n    array<float> xs\nplot(close)\n", 3019);
    refused(
        "type P\n    float v = 0.0\nmap<P, float> m = na\nplot(close)\n",
        3019,
    );
    let d = refused("line l = na\nplot(close)\n", 3024);
    assert!(d.msg.starts_with("deferred-builtin ns="), "{}", d.msg);
    accepted("float[] a = array.new<float>(2)\nmap<string, float> m = map.new<string, float>()\nplot(close)\n");
}

#[test]
fn resolve_defaults_and_titles_must_fold() {
    accepted("f(float x = 1.5, int n = 2 * 3, color c = color.red) => x\nplot(f())\n");
    accepted("const float K = 2.0\nf(float x = K * 2) => x\nplot(f())\n");
    accepted("k = 3\nf(int n = k) => n\nplot(f())\n");
    accepted("type P\n    float v = math.sqrt(4.0)\n    color c = color.new(color.red, 50)\nplot(close)\n");
    accepted("enum E\n    a = \"A\"\n    b = \"B\" + \"!\"\nplot(close)\n");
    refused("f(float x = close) => x\nplot(f())\n", 3008);
    refused("k = 3\nk := 4\nf(int n = k) => n\nplot(f())\n", 3008);
    refused("type P\n    float v = high - low\nplot(close)\n", 3008);
    refused("enum E\n    a = 1\nplot(close)\n", 3008);
}
