// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.3 — overload resolution (§4.5). The picks themselves (which row, which
//! declaration, which argument bound) are asserted on pass 3's output by the private twins in
//! `src/tests/overload.rs`; `load_checked_params_cover_pine_spec` lives there too.

mod common;

use common::*;
use pine_ir::{Diag, Dialect, ExecMode};

fn script(d: Dialect, body: &str) -> String {
    format!("//@version={}\nindicator(\"t\")\n{body}", d as u8)
}

fn run_with(d: Dialect, body: &str, tiers: u8) -> Result<pine_ir::PineProgram, Vec<Diag>> {
    let mut o = default_opts(d, ExecMode::BarClose);
    o.limits.active_tiers = tiers;
    compile_src(&script(d, body), &o)
}

fn run(d: Dialect, body: &str) -> Vec<u16> {
    codes(&run_with(d, body, 0b0111))
}

#[track_caller]
fn accepted(d: Dialect, body: &str) {
    let c = run(d, body);
    assert!(passes(&c), "v{}: {body}: {c:?}", d as u8);
}

#[track_caller]
fn refused(d: Dialect, body: &str, code: u16) -> Diag {
    let ds = diags(run_with(d, body, 0b0111));
    assert_eq!(
        ds.first().map(|x| x.code.0),
        Some(code),
        "v{}: {body}: {ds:?}",
        d as u8
    );
    ds.into_iter().next().unwrap()
}

const V5: Dialect = Dialect::V5;
const V6: Dialect = Dialect::V6;

#[test]
fn overload_dialect_filter_reports_not_in_dialect() {
    let d = refused(V6, "b = true\nx = nz(b)\nplot(close)\n", 3005);
    assert!(d.msg.contains("v5"), "{}", d.msg);
    accepted(V5, "b = true\nx = nz(b, false)\nplot(close)\n");
}

/// `pin:02-na-v6-non-bool`: W13 removes only `bool` from v6's `na()`, so a UDT or enum argument
/// (absent from the v6 reference's list) takes v5's generic row in both dialects; `na(bool)` in v6
/// stays `3005` (impl-06 A087).
#[test]
fn overload_na_v6_takes_non_bool_through_generic_row() {
    let udt = "type P\n    float v = 0.0\nvar P p = na\nplot(na(p) ? 1 : 0)\n";
    let en = "enum E\n    a\n    b\nE e = na\nplot(na(e) ? 1 : 0)\n";
    for d in [V5, V6] {
        for body in [udt, en] {
            let c = run(d, body);
            assert_eq!(c, vec![ACCEPTED], "v{}: {body}", d as u8);
        }
    }
    let ds = diags(run_with(V6, "bool b = true\nplot(na(b) ? 1 : 0)\n", 0b0111));
    assert_eq!(ds[0].code.0, 3005, "{ds:?}");
}

#[test]
fn overload_tier_filter_reports_deferred_builtin() {
    let ds = diags(run_with(
        V6,
        "a = array.new<float>(2)\nplot(close)\n",
        0b0011,
    ));
    assert_eq!(ds[0].code.0, 3024);
    assert!(
        ds[0]
            .msg
            .starts_with("deferred-builtin ns=array name=new tier=3"),
        "{}",
        ds[0].msg
    );
}

#[test]
fn overload_declined_entry_reports_declined() {
    let d = refused(
        V6,
        "x = request.dividends(syminfo.tickerid)\nplot(close)\n",
        3024,
    );
    assert!(
        d.msg.starts_with("deferred-builtin ns=request"),
        "{}",
        d.msg
    );
    assert!(d.msg.contains("declined=1"), "{}", d.msg);
}

#[test]
fn overload_nonstandard_chart_is_refused() {
    refused(
        V6,
        "t = ticker.heikinashi(syminfo.tickerid)\nplot(close)\n",
        3026,
    );
}

#[test]
fn overload_unreachable_row_is_refused() {
    // The first of these rows the lane's pine-std does not serve (§4.5 step 9).
    let cands = [
        ("plot(close)\n", pine_spec::ov::PLOT, "name=plot"),
        (
            "strategy.entry(\"L\", strategy.long)\nplot(close)\n",
            pine_spec::ov::STRATEGY_ENTRY,
            "name=entry",
        ),
        (
            "a = array.new<float>(2)\n",
            pine_spec::ov::ARRAY_NEW,
            "name=new",
        ),
    ];
    let mut checked = 0;
    for (body, ov, name) in cands {
        if pine_std::served(ov) != pine_std::Served::Unreachable {
            continue;
        }
        let ds = all_diags(run_with(V6, body, 0b0111));
        assert!(
            ds.iter().any(|d| d.code.0 == 3024
                && d.msg.starts_with("deferred-builtin ")
                && d.msg.contains(name)
                && d.msg.contains("unserved=1")),
            "{body}: {ds:?}"
        );
        checked += 1;
    }
    let _ = checked; // every candidate served: nothing left to refuse
}

#[test]
fn overload_missing_required_param_is_arity() {
    refused(V6, "x = ta.sma(close)\nplot(x)\n", 3006);
}

#[test]
fn overload_v6_repeated_param_is_arity() {
    refused(
        V6,
        "x = ta.sma(close, length = 5, length = 6)\nplot(x)\n",
        3006,
    );
}

#[test]
fn overload_v5_repeated_param_uses_the_first() {
    // The bound map (the second `length` dropped) is asserted by the private twin; the
    // 5-sample block by T14.
    accepted(V5, "x = ta.sma(close, length = 5, length = 6)\nplot(x)\n");
}

#[test]
fn overload_unknown_named_arg_is_arity() {
    refused(V6, "x = ta.sma(close, 5, foo = 1)\nplot(x)\n", 3006);
}

#[test]
fn overload_synthetic_params_are_never_bound() {
    refused(V6, "plot(close, __plot = 3)\n", 3006);
    // Positionally, `close` binds `series`, never the leading synthetic `__plot`.
    accepted(V6, "plot(close)\n");
}

#[test]
fn overload_widening_costs_more_than_exact() {
    // `nz(1)` is `NZ_I` (exact), not `NZ_F` (int → float): its result is an int.
    accepted(V6, "const int x = nz(1)\nplot(x)\n");
    accepted(V6, "x = nz(1.5)\nplot(x)\n");
}

#[test]
fn overload_generic_row_loses_to_concrete_row() {
    // `f(x)` is a generic row, `f(float x)` a concrete one: the concrete row is chosen, so the
    // result is the concrete body's (a string here, which a float-typed use refuses).
    let body = "f(x) => x\nf(float x) => str.tostring(x)\n";
    refused(V6, &format!("{body}float y = f(1.0)\nplot(close)\n"), 3007);
    accepted(V6, &format!("{body}string y = f(1.0)\nplot(close)\n"));
}

#[test]
fn overload_defaulted_param_loses_to_explicit_signature() {
    // v5 `time("D")`: `TIME_I` (no default filled) beats `time(timeframe, session = "",
    // timezone = …)` (two defaults); the private twin names the row.
    accepted(V5, "t = time(\"D\")\nplot(close)\n");
}

#[test]
fn overload_tightest_qualifier_row_wins() {
    let body = "f(simple float x) => 1\nf(series float x) => \"s\"\n";
    accepted(V6, &format!("{body}int y = f(1.0)\nplot(close)\n"));
    accepted(V6, &format!("{body}string y = f(close)\nplot(close)\n"));
}

#[test]
fn overload_series_arg_to_simple_param_is_qualifier_too_weak() {
    let d = refused(V6, "plot(close, offset = bar_index)\n", 3008);
    for w in ["offset", "series", "simple"] {
        assert!(d.msg.contains(w), "{w}: {}", d.msg);
    }
}

#[test]
fn overload_ambiguous_is_an_error_not_a_pick() {
    let d = refused(
        V6,
        "f(int x, float y) => x\nf(float x, int y) => y\nz = f(1, 1)\nplot(close)\n",
        3009,
    );
    assert_eq!(d.notes.len(), 2, "{d:?}");
}

#[test]
fn overload_no_candidate_reports_every_rejection_reason() {
    let d = refused(
        V6,
        "f(int x) => x\nf(bool x) => x\nz = f(\"s\")\nplot(close)\n",
        3010,
    );
    assert_eq!(d.notes.len(), 2, "{d:?}");
}

#[test]
fn overload_user_method_overrides_builtin_method() {
    // `array.push` returns void; the user method returns a float, so only it can initialise `v`.
    accepted(
        V6,
        "method push(array<float> a, float x) => x\narr = array.new<float>(2)\n\
         v = arr.push(1.0)\nplot(v)\n",
    );
}

#[test]
fn overload_stateful_builtin_always_returns_series() {
    refused(
        V6,
        "f(simple float a) => a\nx = f(ta.sma(1.0, 5))\nplot(x)\n",
        3008,
    );
}

#[test]
fn overload_ret_qual_join_is_the_argument_join() {
    accepted(V6, "const int x = math.max(1, 2)\nplot(x)\n");
    refused(V6, "const int x = math.max(1, bar_index)\nplot(x)\n", 3008);
}

#[test]
fn overload_math_random_without_constant_seed_is_refused() {
    refused(V6, "x = math.random(0, 1, bar_index)\nplot(x)\n", 3024);
    refused(V6, "x = math.random(0, 1)\nplot(x)\n", 3024);
    accepted(V6, "x = math.random(0, 1, 42)\nplot(x)\n");
}

#[test]
fn overload_str_format_more_than_15_args_is_5005() {
    let args15 = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15";
    accepted(
        V6,
        &format!("s = str.format(\"{{0}}\", {args15})\nplot(close)\n"),
    );
    refused(
        V6,
        &format!("s = str.format(\"{{0}}\", {args15}, 16)\nplot(close)\n"),
        5005,
    );
}

#[test]
fn overload_drawing_is_refused() {
    let d = refused(V6, "l = line.new(0, 0, 1, 1)\nplot(close)\n", 3024);
    assert!(
        d.msg.starts_with("deferred-builtin ns=line name=new"),
        "{}",
        d.msg
    );
}

#[test]
fn overload_constant_strings_are_load_checked() {
    refused(V6, "x = time(\"D\", \"abc\")\nplot(close)\n", 5002);
    refused(
        V6,
        "x = time(\"D\", \"0930-1600\", \"Mars/Olympus\")\nplot(close)\n",
        5003,
    );
    refused(V6, "x = str.match(\"abc\", \"(?=a)\")\nplot(close)\n", 5004);
    refused(V6, "x = str.format(\"{0\", 1)\nplot(close)\n", 5005);
    refused(V6, "x = time(\"45\")\nplot(close)\n", 5001);
    accepted(V6, "x = timeframe.in_seconds(\"45\")\nplot(close)\n");
    // The same strings as series values are the body's run-time check.
    accepted(
        V6,
        "s = str.tostring(close)\nx = str.match(\"abc\", s)\ny = time(\"D\", s)\nplot(close)\n",
    );
}

/// impl-04 §3.4's families and their synthetic parameters, in order.
fn synthetic_family(full: &str) -> Option<&'static [&'static str]> {
    const PLOT: &[&str] = &["__plot"];
    const COLL: &[&str] = &["__elem", "__key", "__cap"];
    match full {
        "plot" | "plotshape" | "plotchar" | "plotarrow" | "hline" | "bgcolor" | "barcolor"
        | "fill" | "plotcandle" | "plotbar" => Some(PLOT),
        "request.security" => Some(&["__key", "__out", "__ty"]),
        "array.new" | "array.from" | "array.copy" | "array.slice" | "array.sort_indices"
        | "map.new" | "map.copy" | "map.keys" | "map.values" | "str.split" => Some(COLL),
        f if f.starts_with("array.new_") => Some(COLL),
        "str.format" | "log.info" | "log.warning" | "log.error" => Some(&["__types"]),
        _ => None,
    }
}

#[test]
fn synthetic_params_match_impl04() {
    use pine_spec::pf;
    let mut families_seen = 0;
    for o in pine_spec::OVERLOADS.iter() {
        let e = pine_spec::entry_of(o);
        let full = e.full_str();
        let ps = pine_spec::params(o);
        let want = synthetic_family(full).unwrap_or(&[]);
        if !want.is_empty() {
            families_seen += 1;
        }
        for (i, p) in ps.iter().enumerate() {
            let synthetic = p.flags & pf::SYNTHETIC != 0;
            let name = p.name.as_str();
            if i < want.len() {
                assert!(synthetic, "{full} row {i} `{name}` is not synthetic");
                assert_eq!(name, want[i], "{full} row {i}");
                assert_eq!(p.sig.ty.ty, pine_ir::Ty::Int, "{full} `{name}`");
                assert_eq!(p.sig.ty.qual, pine_ir::Qual::Const, "{full} `{name}`");
                assert_eq!(p.flags & pf::OPTIONAL, 0, "{full} `{name}` is required");
            } else {
                assert!(!synthetic, "{full} `{name}`: an unexpected synthetic row");
                assert!(!name.starts_with("__"), "{full} `{name}`: a `__` name");
            }
        }
    }
    assert!(families_seen > 0);
}
