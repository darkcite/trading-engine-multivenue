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
    assert_eq!(run(ok, Vec::new()), vec![ACCEPTED]);
}

#[test]
fn meta_inputs_override_out_of_range_is_5006() {
    let body = "len = input.int(14, \"L\", minval = 1, maxval = 50)\nplot(ta.sma(close, len))\n";
    assert_eq!(run(body, ov("len", InputValue::Int(99))), vec![5006]);
    assert_eq!(run(body, ov("len", InputValue::Int(20))), vec![ACCEPTED]);
}

#[test]
fn meta_inputs_override_wrong_kind_is_5007() {
    let body = "m = input.float(1.5, \"M\")\nplot(close * m)\n";
    assert_eq!(run(body, ov("m", InputValue::Int(2))), vec![5007]);
    assert_eq!(
        run(body, ov("m", InputValue::Float1e6(2_000_000))),
        vec![ACCEPTED]
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
        vec![ACCEPTED]
    );
}

#[test]
fn meta_inputs_override_outside_options_is_5006() {
    let body = "n = input.int(2, \"N\", options = [1, 2, 3])\nplot(close * n)\n";
    assert_eq!(run(body, ov("n", InputValue::Int(4))), vec![5006]);
    assert_eq!(run(body, ov("n", InputValue::Int(3))), vec![ACCEPTED]);
    let body = "m = input.float(1.5, \"M\", options = [1.5, 2.5])\nplot(close * m)\n";
    assert_eq!(
        run(body, ov("m", InputValue::Float1e6(2_000_000))),
        vec![5006]
    );
    assert_eq!(
        run(body, ov("m", InputValue::Float1e6(2_500_000))),
        vec![ACCEPTED]
    );
    let body = "s = input.string(\"a\", \"S\", options = [\"a\", \"b\"])\nplot(close)\n";
    assert_eq!(
        run(body, ov("s", InputValue::Str("c".to_owned()))),
        vec![5006]
    );
    assert_eq!(
        run(body, ov("s", InputValue::Str("b".to_owned()))),
        vec![ACCEPTED]
    );
}

#[test]
fn meta_inputs_source_override_aliases_a_series() {
    let body = "src = input.source(close, \"Src\")\nplot(ta.sma(src, 10))\n";
    assert_eq!(run(body, Vec::new()), vec![ACCEPTED]);
    for ok in ["hlc3", "open", "ohlc4"] {
        // `hlc3` / `ohlc4` are prelude rows the lane's pine-std does not serve yet (unserved).
        let c = run(body, ov("src", InputValue::Str(ok.to_owned())));
        assert!(passes(&c), "{ok}: {c:?}");
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

/// §4.12.5 (I23): `unmatched_input_overrides` returns exactly the `opts.inputs` keys no compiled
/// `InputDesc::name` equals, in `opts.inputs` order; `compile` itself accepts them.
#[test]
fn meta_inputs_unmatched_overrides_are_reported() {
    let body = "len = input.int(14, \"Length\")\nsrc = input.source(close, \"Src\")\n\
                plot(ta.sma(src, len))\n";
    let mut o = default_opts(Dialect::V6, ExecMode::BarClose);
    o.inputs = vec![
        InputOverride {
            name: "nope".to_owned(),
            value: InputValue::Int(3),
        },
        InputOverride {
            name: "len".to_owned(),
            value: InputValue::Int(20),
        },
        InputOverride {
            name: "Length".to_owned(),
            value: InputValue::Int(20),
        },
        InputOverride {
            name: "LEN".to_owned(),
            value: InputValue::Int(20),
        },
    ];
    let src = format!("//@version=6\nindicator(\"t\")\n{body}");
    let p = match compile_src(&src, &o) {
        Ok(p) => p,
        Err(e) => panic!("{e:#?}"),
    };
    // The key is the variable (`len`), not the title; bytes compare, so `LEN` is unknown too.
    assert_eq!(
        pine_sema::unmatched_input_overrides(&p, &o),
        vec!["nope", "Length", "LEN"]
    );
    o.inputs.retain(|x| x.name == "len");
    let p = compile_src(&src, &o).expect("compiles");
    assert!(pine_sema::unmatched_input_overrides(&p, &o).is_empty());
    let none = default_opts(Dialect::V6, ExecMode::BarClose);
    let p = compile_src(&src, &none).expect("compiles");
    assert!(pine_sema::unmatched_input_overrides(&p, &none).is_empty());
}

/// Fuzz r21 (`input` admitted to the alphabet: 77+ compile panics in `nacap`/`refs`/`series`):
/// an `input(…)` whose `defval` does not fold — the `series` row `INPUT_F_SRC` fed a series —
/// is `3008` at the call (`pin:02-input-series-defval`), never an unfolded call whose missing value
/// slot a later pass indexes. A constant `defval`, nested or in a local block, still folds. Since
/// r22 a source-type built-in `defval` (`input(close)`) is a source input
/// (`pin:02-input-source-defval`, the next test); any other non-constant stays `3008`: an
/// expression, a history read, a user variable bound to a source, a non-float built-in series
/// (`bar_index`, `time`) and a `simple` built-in (`syminfo.mintick`).
#[test]
fn meta_inputs_unfoldable_defval_is_3008_not_a_panic() {
    for body in [
        "plot(input(close * 2))\n",
        "x = input(close[1])\nplot(x)\n",
        "x = input(bar_index)\nplot(x)\n",
        "x = input(time)\nplot(x)\n",
        "x = input(syminfo.mintick)\nplot(x)\n",
        "v = close\nplot(nz(input(v)))\n",
        "plot(input((false ? close : 96.79)))\n",
        "for i = 0 to 3\n    float v = input(i)\nplot(close)\n",
    ] {
        let r = std::panic::catch_unwind(|| run(body, Vec::new()));
        assert_eq!(r.ok(), Some(vec![3008]), "{body:?}");
    }
    for body in [
        "plot(input(3.0) + close)\n",
        "plot(input(true) ? 1.0 : 2.0)\n",
        "plot(ta.highest(input(4.59), 3))\n",
        "for i = 0 to 3\n    float v = input(3)\nplot(close)\n",
        "if close > 0\n    bool v3 = input(false)\nplot(close)\n",
    ] {
        assert_eq!(run(body, Vec::new()), vec![ACCEPTED], "{body:?}");
    }
}

/// r22 lead ruling, `pin:02-input-source-defval`: TradingView's `input()` reference gives `defval`
/// as "const int/float/bool/string/color or source-type built-ins" — "built-in series float
/// variables … `close`, `hlc3`" — and its example `i_src = input(close, "Source")` feeds
/// `ta.sma`. The `series` row `INPUT_F_SRC` over such a built-in is therefore a source input that
/// aliases the series exactly as `input.source` does: the same code and constants, the same
/// descriptor (kind `Source`, `String`, the series' name as its default), and a manifest override
/// re-aliases it (`5006` when the override names no built-in float series), in every position the
/// r21 sweep met (bound, nested, inside a block).
#[test]
fn meta_inputs_bare_input_over_a_source_builtin_aliases_like_input_source() {
    use pine_ir::{input_flag, InputKind, Sym, Ty};
    let compile = |body: &str, inputs: Vec<InputOverride>| {
        let mut o = default_opts(Dialect::V6, ExecMode::BarClose);
        o.inputs = inputs;
        compile_src(&format!("//@version=6\nindicator(\"t\")\n{body}"), &o)
    };
    for (bare, named) in [
        ("input(close, \"S\")", "input.source(close, \"S\")"),
        ("input(hlc3)", "input.source(hlc3)"),
        ("input(defval = hl2)", "input.source(defval = hl2)"),
        (
            "input(open, title = \"S\")",
            "input.source(open, title = \"S\")",
        ),
    ] {
        for (shape, ovr) in [
            ("s = {}\nplot(ta.sma(s, 10))\nplot(s[1])\n", None),
            ("s = {}\nplot(s)\n", Some("ohlc4")),
        ] {
            let ovs = || match ovr {
                Some(v) => ov("s", InputValue::Str(v.to_owned())),
                None => Vec::new(),
            };
            let a = compile(&shape.replace("{}", bare), ovs()).expect(bare);
            let b = compile(&shape.replace("{}", named), ovs()).expect(named);
            assert_eq!(a.code, b.code, "{bare}: the same code as {named}");
            // The constant pools agree; a constant-string `StrRef` (region 0) may sit at another
            // blob offset (`input.source` interns `source`), so those compare as text.
            assert_eq!(a.consts.len(), b.consts.len(), "{bare}");
            for (x, y) in a.consts.iter().zip(&b.consts) {
                if x != y {
                    let text = |p: &pine_ir::PineProgram, c: u64| {
                        assert_eq!(c >> 62, 0, "{bare}: a non-string constant differs");
                        p.syms
                            .str_at(((c >> 32) & 0x3FFF_FFFF) as u32, c as u32)
                            .to_vec()
                    };
                    assert_eq!(text(&a, *x), text(&b, *y), "{bare}");
                }
            }
            let (da, db) = (a.meta.inputs[0], b.meta.inputs[0]);
            assert_eq!((da.kind, da.ty), (InputKind::Source, Ty::String), "{bare}");
            assert_eq!(
                (da.kind, da.ty, da.flags),
                (db.kind, db.ty, db.flags),
                "{bare}"
            );
            let name =
                |p: &pine_ir::PineProgram, bits: u64| p.syms.resolve(Sym(bits as u32)).to_owned();
            assert_eq!(
                name(&a, da.default_bits),
                name(&b, db.default_bits),
                "{bare}"
            );
            if let Some(v) = ovr {
                assert_eq!(name(&a, da.default_bits), v, "{bare}: overridden");
                assert_ne!(da.flags & input_flag::OVERRIDDEN, 0, "{bare}");
            }
        }
    }
    // Every position the r21 sweep met compiles; an override naming no built-in float series is
    // the same `5006` as `input.source`'s.
    for body in [
        "x = input(close)\nplot(x)\n",
        "plot(nz(input(close)))\n",
        "plot(input(hlc3) + close)\n",
        "for i = 0 to 3\n    float v = input(low)\nplot(close)\n",
        "if close > 0\n    float v = input(high)\nplot(close)\n",
    ] {
        assert_eq!(run(body, Vec::new()), vec![ACCEPTED], "{body:?}");
    }
    let bad = ov("x", InputValue::Str("bar_index".to_owned()));
    assert_eq!(run("x = input(close)\nplot(x)\n", bad.clone()), vec![5006]);
    assert_eq!(run("x = input.source(close)\nplot(x)\n", bad), vec![5006]);
}
