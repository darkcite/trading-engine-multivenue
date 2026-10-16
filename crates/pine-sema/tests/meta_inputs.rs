// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.11.1's `meta_inputs_*` refusals (§4.12). The descriptors themselves are asserted
//! on pass 3's output by the private twins in `src/tests/inputs.rs` until `ProgramMeta` (T20).

mod common;

use common::*;
use pine_ir::{Dialect, ExecMode, InputOverride, InputValue};

fn run(body: &str, inputs: Vec<InputOverride>) -> Vec<u16> {
    let mut o = default_opts(Dialect::V6, ExecMode::BarClose);
    o.inputs = inputs;
    codes(&compile_src(
        &format!("//@version=6\nindicator(\"t\")\n{body}"),
        &o,
    ))
}

fn ov(name: &str, value: InputValue) -> Vec<InputOverride> {
    vec![InputOverride {
        name: name.to_owned(),
        value,
    }]
}

#[test]
fn meta_inputs_duplicate_name_is_refused() {
    let body = "a = input.float(1.0, \"w\")\nplot(close, linewidth = input.int(1, \"w\"))\n\
                plot(close, linewidth = input.int(2, \"w\"))\n";
    assert_eq!(run(body, Vec::new()), vec![3050]);
    let ok = "a = input.int(1, \"t\")\nb = input.int(2, \"t\")\nplot(a + b)\n";
    assert_eq!(run(ok, Vec::new()), vec![NOT_IMPLEMENTED]);
}

#[test]
fn meta_inputs_override_out_of_range_is_5006() {
    let body = "len = input.int(14, \"L\", minval = 1, maxval = 50)\nplot(ta.sma(close, len))\n";
    assert_eq!(run(body, ov("len", InputValue::Int(99))), vec![5006]);
    assert_eq!(
        run(body, ov("len", InputValue::Int(20))),
        vec![NOT_IMPLEMENTED]
    );
}

#[test]
fn meta_inputs_override_wrong_kind_is_5007() {
    let body = "m = input.float(1.5, \"M\")\nplot(close * m)\n";
    assert_eq!(run(body, ov("m", InputValue::Int(2))), vec![5007]);
    assert_eq!(
        run(body, ov("m", InputValue::Float1e6(2_000_000))),
        vec![NOT_IMPLEMENTED]
    );
}

#[test]
fn meta_inputs_timeframe_and_session_defaults_are_checked() {
    assert_eq!(
        run("tf = input.timeframe(\"7Q\")\nplot(close)\n", Vec::new()),
        vec![5001]
    );
    assert_eq!(
        run("s = input.session(\"abc\")\nplot(close)\n", Vec::new()),
        vec![5002]
    );
    assert_eq!(
        run(
            "tf = input.timeframe(\"60\")\ns = input.session(\"0930-1600\")\nplot(close)\n",
            Vec::new()
        ),
        vec![NOT_IMPLEMENTED]
    );
}

#[test]
fn meta_inputs_override_outside_options_is_5006() {
    let body = "n = input.int(2, \"N\", options = [1, 2, 3])\nplot(close * n)\n";
    assert_eq!(run(body, ov("n", InputValue::Int(4))), vec![5006]);
    assert_eq!(
        run(body, ov("n", InputValue::Int(3))),
        vec![NOT_IMPLEMENTED]
    );
    let body = "m = input.float(1.5, \"M\", options = [1.5, 2.5])\nplot(close * m)\n";
    assert_eq!(
        run(body, ov("m", InputValue::Float1e6(2_000_000))),
        vec![5006]
    );
    assert_eq!(
        run(body, ov("m", InputValue::Float1e6(2_500_000))),
        vec![NOT_IMPLEMENTED]
    );
    let body = "s = input.string(\"a\", \"S\", options = [\"a\", \"b\"])\nplot(close)\n";
    assert_eq!(
        run(body, ov("s", InputValue::Str("c".to_owned()))),
        vec![5006]
    );
    assert_eq!(
        run(body, ov("s", InputValue::Str("b".to_owned()))),
        vec![NOT_IMPLEMENTED]
    );
}

#[test]
fn meta_inputs_source_override_aliases_a_series() {
    let body = "src = input.source(close, \"Src\")\nplot(ta.sma(src, 10))\n";
    assert_eq!(run(body, Vec::new()), vec![NOT_IMPLEMENTED]);
    for ok in ["hlc3", "open", "ohlc4"] {
        assert_eq!(
            run(body, ov("src", InputValue::Str(ok.to_owned()))),
            vec![NOT_IMPLEMENTED],
            "{ok}"
        );
    }
    // `time` is an int series, `bar_index` an int, `nope` no entry: "names no built-in float
    // series".
    for bad in ["time", "bar_index", "nope"] {
        assert_eq!(
            run(body, ov("src", InputValue::Str(bad.to_owned()))),
            vec![5006],
            "{bad}"
        );
    }
    assert_eq!(run(body, ov("src", InputValue::Int(1))), vec![5007]);
    // pin:02-source-default-5006: a default naming no built-in series is the same `5006`.
    let body = "src = input.source(close * 2, \"Src\")\nplot(src)\n";
    assert_eq!(run(body, Vec::new()), vec![5006]);
}
