// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.6 — `math.*` and the cast bodies, driven through `pine_std::testing` (§7.1).
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::ov;
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, CHART};
use pine_vm::testkit::bar;

#[derive(Clone, Copy)]
enum V {
    F(f64),
    I(Option<i64>),
}
/// Call `id` once on `inputs`, its output in the `int` lane when `out_i64`; return the output.
fn call(id: OverloadId, inputs: &[V], out_i64: bool) -> f64 {
    let ins: Vec<(Lane, Src)> = inputs
        .iter()
        .map(|v| match *v {
            V::F(x) => (Lane::F64, Src::Const(x.to_bits())),
            V::I(Some(x)) => (Lane::I64, Src::Const(x as u64)),
            V::I(None) => (Lane::I64, Src::Na),
        })
        .collect();
    let outs = [if out_i64 { Lane::I64 } else { Lane::F64 }];
    let calls = [Call {
        ov: id,
        inputs: &ins,
        outputs: &outs,
        site_bytes: 0,
        guard: None,
    }];
    let mode = ExecMode::BarClose;
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let o = h.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut TestCtx::default());
    assert_eq!(o, StepOutcome::Ok);
    h.out(0, 0)
}

fn same(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}

#[test]
fn math_max_propagates_na_unlike_f64_max() {
    let na = f64::NAN;
    assert!(call(ov::MATH_MAX_F, &[V::F(na), V::F(1.0)], false).is_nan());
    assert!(call(ov::MATH_MAX_F, &[V::F(1.0), V::F(na)], false).is_nan());
    assert!(call(ov::MATH_MIN_F, &[V::F(2.0), V::F(na), V::F(1.0)], false).is_nan());
    assert_eq!(
        call(ov::MATH_MAX_F, &[V::F(1.0), V::F(3.0), V::F(2.0)], false),
        3.0
    );
    assert_eq!(
        call(ov::MATH_MIN_F, &[V::F(1.0), V::F(-3.0), V::F(2.0)], false),
        -3.0
    );
    assert_eq!(
        call(ov::MATH_MAX_I, &[V::I(Some(4)), V::I(Some(9))], true),
        9.0
    );
    assert!(call(ov::MATH_MIN_I, &[V::I(Some(4)), V::I(None)], true).is_nan());
}

#[test]
fn cast_id_copies_every_lane() {
    assert_eq!(call(ov::FLOAT_F, &[V::F(2.5)], false), 2.5);
    assert!(call(ov::FLOAT_F, &[V::F(f64::NAN)], false).is_nan());
    assert_eq!(call(ov::INT_I, &[V::I(Some(-7))], true), -7.0);
    assert!(call(ov::INT_I, &[V::I(None)], true).is_nan());
    assert_eq!(
        call(ov::COLOR, &[V::I(Some(0x00FF_0000))], true),
        16_711_680.0
    );
    assert_eq!(call(ov::BOOL_B, &[V::I(Some(1))], true), 1.0);
}

#[test]
fn math_unary_rows_follow_f64_and_propagate_na() {
    let rows: [(OverloadId, fn(f64) -> f64, bool); 10] = [
        (ov::MATH_ABS_F, f64::abs, false),
        (ov::MATH_SQRT, f64::sqrt, false),
        (ov::MATH_EXP, f64::exp, false),
        (ov::MATH_LOG, f64::ln, false),
        (ov::MATH_LOG10, f64::log10, false),
        (ov::MATH_SIN, f64::sin, false),
        (ov::MATH_ATAN, f64::atan, false),
        (ov::MATH_TODEGREES, f64::to_degrees, false),
        (ov::MATH_CEIL, f64::ceil, true),
        (ov::MATH_FLOOR, f64::floor, true),
    ];
    for (id, f, int_out) in rows {
        for x in [0.3, 2.7, -1.5, 100.0] {
            let want = f(x);
            let want = if int_out { want.trunc() } else { want };
            assert!(same(call(id, &[V::F(x)], int_out), want), "{id:?} {x}");
        }
        assert!(call(id, &[V::F(f64::NAN)], int_out).is_nan(), "{id:?}");
    }
    assert_eq!(call(ov::MATH_SIGN, &[V::F(-0.2)], false), -1.0);
    assert_eq!(call(ov::MATH_SIGN, &[V::F(0.0)], false), 0.0);
    assert_eq!(call(ov::MATH_POW, &[V::F(2.0), V::F(10.0)], false), 1024.0);
    assert_eq!(
        call(ov::MATH_AVG, &[V::F(1.0), V::F(2.0), V::F(6.0)], false),
        3.0
    );
    assert!(call(ov::MATH_AVG, &[V::F(1.0), V::F(f64::NAN)], false).is_nan());
    assert_eq!(call(ov::MATH_ABS_I, &[V::I(Some(-5))], true), 5.0);
    // `i64::wrapping_abs` (§4.13.1): MIN stays MIN.
    assert_eq!(
        call(ov::MATH_ABS_I, &[V::I(Some(i64::MIN))], true),
        i64::MIN as f64
    );
}

fn dialect_reads(sw: pine_spec::Sw) -> u8 {
    pine_spec::sw(sw, Dialect::V6)
}

#[test]
fn round_ties_switch() {
    let half_up = dialect_reads(pine_spec::Sw::RoundTies) == pine_spec::swr::ROUND_TIES_HALF_UP;
    let r = |x: f64| call(ov::MATH_ROUND_I, &[V::F(x)], true);
    assert_eq!(r(2.5), 3.0);
    assert_eq!(r(-2.5), if half_up { -2.0 } else { -3.0 });
    assert_eq!(
        r(0.499_999_999_999_999_94),
        0.0,
        "no x + 0.5 double rounding"
    );
    assert_eq!(r(-0.4), 0.0);
    assert_eq!(r(1e15 + 0.5), 1e15 + 1.0);
    assert!(r(f64::NAN).is_nan() && r(f64::INFINITY).is_nan() && r(1e19).is_nan());
}

#[test]
fn round_to_mintick_is_integer_exact() {
    // CHART: mintick 0.01.
    let m = |x: f64| call(ov::MATH_ROUND_TO_MINTICK, &[V::F(x)], false);
    assert_eq!(m(1.234), 1.23);
    assert_eq!(m(1.236), 1.24);
    assert_eq!(m(100.005), 100.01);
    assert_eq!(m(-0.004), 0.0);
    assert!(m(f64::NAN).is_nan() && m(f64::INFINITY).is_nan());
}

#[test]
fn int_strict_faults_invalid_cast() {
    assert_eq!(call(ov::OP_INT_STRICT, &[V::F(-3.9)], true), -3.0);
    for x in [f64::NAN, f64::INFINITY, 9.3e18] {
        let ins = [(Lane::F64, Src::Const(x.to_bits()))];
        let calls = [Call {
            ov: ov::OP_INT_STRICT,
            inputs: &ins,
            outputs: &[Lane::I64],
            site_bytes: 0,
            guard: None,
        }];
        let mode = ExecMode::BarClose;
        let mut h = Harness::build(&Script {
            calls: &calls,
            mode,
            dialect: Dialect::V6,
            intent_cap: 0,
            chart: CHART,
        });
        let o = h.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut TestCtx::default());
        assert_ne!(o, StepOutcome::Ok);
        assert_eq!(
            h.last_fault().map(|f| f.0),
            Some(FaultKind::InvalidCast),
            "{x}"
        );
    }
}

#[test]
fn pow_nonfinite_follows_float_div_zero() {
    let na = dialect_reads(pine_spec::Sw::FloatDivZero) == pine_spec::swr::FLOAT_DIV_ZERO_NA;
    let r = call(ov::MATH_POW, &[V::F(0.0), V::F(-1.0)], false);
    assert_eq!(r.is_nan(), na);
    if !na {
        assert_eq!(r, f64::INFINITY);
    }
}

#[test]
fn color_rows_pack_and_unpack() {
    let c = pine_ir::color::pack(10, 20, 30, 40);
    assert_eq!(call(ov::COLOR_R, &[V::I(Some(c))], false), 10.0);
    assert_eq!(call(ov::COLOR_G, &[V::I(Some(c))], false), 20.0);
    assert_eq!(call(ov::COLOR_B, &[V::I(Some(c))], false), 30.0);
    assert_eq!(call(ov::COLOR_T, &[V::I(Some(c))], false), 40.0);
    assert!(call(ov::COLOR_R, &[V::I(None)], false).is_nan());
    let n = call(ov::COLOR_NEW, &[V::I(Some(c)), V::F(70.4)], true) as i64;
    assert_eq!(n, pine_ir::color::pack(10, 20, 30, 70));
    let rgb = call(
        ov::COLOR_RGB,
        &[V::F(300.0), V::F(-5.0), V::F(127.5), V::F(0.0)],
        true,
    ) as i64;
    assert_eq!(rgb, pine_ir::color::pack(255, 0, 128, 0));
    let (lo, hi) = (
        pine_ir::color::pack(0, 0, 0, 0),
        pine_ir::color::pack(200, 100, 50, 100),
    );
    let g = |v: f64| {
        call(
            ov::COLOR_FROM_GRADIENT,
            &[
                V::F(v),
                V::F(0.0),
                V::F(10.0),
                V::I(Some(lo)),
                V::I(Some(hi)),
            ],
            true,
        ) as i64
    };
    assert_eq!(g(5.0), pine_ir::color::pack(100, 50, 25, 50));
    assert_eq!(g(-3.0), lo);
    assert_eq!(g(30.0), hi);
}

// ---- T11: the zero-copy str.* family ------------------------------------------------------

fn st(x: &'static str) -> (Lane, Src) {
    (Lane::I64, Src::Str(x))
}

fn it(x: i64) -> (Lane, Src) {
    (Lane::I64, Src::Const(x as u64))
}

const NA_S: (Lane, Src) = (Lane::I64, Src::Na);

/// One evaluation of `rows` (each one i64-lane output; `Src::Out` chains them); every output.
fn str_rows(rows: &[(OverloadId, Vec<(Lane, Src)>)]) -> Vec<f64> {
    let outs = [Lane::I64];
    let calls: Vec<Call> = rows
        .iter()
        .map(|(id, ins)| Call {
            ov: *id,
            inputs: ins,
            outputs: &outs,
            site_bytes: 0,
            guard: None,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let o = h.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut TestCtx::default());
    assert_eq!(o, StepOutcome::Ok);
    (0..rows.len()).map(|k| h.out(k, 0)).collect()
}

/// A CONST `StrRef` read back through the f64 plot lane (exact: region 0, offset < 2^21):
/// `(offset, length)`.
fn konst(v: f64) -> (u64, u64) {
    let r = v as u64;
    assert!(
        (0.0..9.0e15).contains(&v) && r >> 62 == 0,
        "not a CONST ref: {v}"
    );
    (r >> 32, r & 0xFFFF_FFFF)
}

#[test]
fn str_substring_indexes_characters() {
    let s = "h\u{e9}llo w\u{f6}rld"; // 11 characters, 13 bytes
    let rows = vec![
        (ov::STR_SUBSTRING, vec![st(s), it(1), it(4)]), // 0: "éll"
        (
            ov::OP_EQ_STR,
            vec![(Lane::I64, Src::Out(0, 0)), st("\u{e9}ll")],
        ),
        (ov::STR_LENGTH, vec![(Lane::I64, Src::Out(0, 0))]),
        (ov::STR_SUBSTRING, vec![st(s), it(-3), it(100)]), // 3: clamped → s
        (ov::OP_EQ_STR, vec![(Lane::I64, Src::Out(3, 0)), st(s)]),
        (ov::STR_SUBSTRING, vec![st(s), it(5), it(2)]), // 5: begin > end → ""
        (ov::STR_LENGTH, vec![(Lane::I64, Src::Out(5, 0))]),
        (ov::STR_POS, vec![st(s), st("w\u{f6}")]),
        (ov::STR_POS, vec![st(s), st("zz")]),
        (ov::STR_SUBSTRING, vec![st(s), (Lane::I64, Src::Na), it(2)]),
        (ov::STR_LENGTH, vec![st(s)]),
        (ov::STR_LENGTH, vec![NA_S]),
        (ov::STR_POS, vec![st(s), st("")]),
    ];
    let g = str_rows(&rows);
    assert_eq!((g[1], g[2]), (1.0, 3.0));
    assert_eq!(g[4], 1.0);
    assert_eq!(g[6], 0.0);
    assert_eq!(g[7], 6.0, "a character index, not the byte index 7");
    assert!(g[8].is_nan() && g[9].is_nan());
    assert_eq!(g[10], 11.0);
    assert!(g[11].is_nan());
    assert_eq!(g[12], 0.0);
}

#[test]
fn str_views_share_the_source_region() {
    let s = "h\u{e9}llo w\u{f6}rld";
    let rows = vec![
        (ov::STR_TOSTRING_S, vec![st(s)]),
        (ov::STR_SUBSTRING, vec![st(s), it(1), it(4)]),
        (ov::STR_TOSTRING_S, vec![st("  ab \t")]),
        (ov::STR_TRIM, vec![st("  ab \t")]),
        (ov::STR_TRIM, vec![st("   ")]),
        (ov::STR_TRIM, vec![NA_S]),
    ];
    let g = str_rows(&rows);
    let (o0, l0) = konst(g[0]);
    assert_eq!(l0, 13);
    assert_eq!(
        konst(g[1]),
        (o0 + 1, 4),
        "substring is a view at the char's byte offset"
    );
    let (o2, l2) = konst(g[2]);
    assert_eq!(l2, 6);
    assert_eq!(konst(g[3]), (o2 + 2, 2), "trim is a view");
    assert_eq!(konst(g[4]).1, 0);
    assert!(g[5].is_nan());
    // Searches and comparisons: na → false; byte equality.
    let rows = vec![
        (ov::STR_CONTAINS, vec![st(s), st("o w")]),
        (ov::STR_CONTAINS, vec![st(s), st("ow")]),
        (ov::STR_STARTSWITH, vec![st(s), st("h\u{e9}")]),
        (ov::STR_ENDSWITH, vec![st(s), st("rld")]),
        (ov::STR_ENDSWITH, vec![st(s), st("rl")]),
        (ov::STR_CONTAINS, vec![NA_S, st("")]),
        (ov::OP_EQ_STR, vec![NA_S, NA_S]),
        (ov::OP_NE_STR, vec![NA_S, st("a")]),
        (ov::OP_NE_STR, vec![st("a"), st("b")]),
        (ov::OP_EQ_STR, vec![st("ab"), st("ab")]),
    ];
    assert_eq!(
        str_rows(&rows),
        vec![1., 0., 1., 1., 0., 0., 0., 0., 1., 1.]
    );
}

/// `str.tonumber`: the §4.13.2 grammar, else na.
#[test]
fn str_tonumber_grammar() {
    let ok: [(&'static str, f64); 8] = [
        ("12", 12.0),
        ("  -3.25 ", -3.25),
        ("+.5", 0.5),
        ("7.", 7.0),
        ("1e3", 1000.0),
        ("2.5E-2", 0.025),
        ("0.1", 0.1),
        ("-0", -0.0),
    ];
    let bad = [
        "", " ", "abc", "1.2.3", "inf", "NaN", "0x10", "1e", "--1", ".", "1 2",
    ];
    let mut rows: Vec<(OverloadId, Vec<(Lane, Src)>)> = Vec::new();
    for (t, _) in ok {
        rows.push((ov::STR_TONUMBER, vec![st(t)]));
    }
    for t in bad {
        rows.push((ov::STR_TONUMBER, vec![st(t)]));
    }
    rows.push((ov::STR_TONUMBER, vec![NA_S]));
    // The output lane is f64: rebuild with an f64 output.
    let outs = [Lane::F64];
    let calls: Vec<Call> = rows
        .iter()
        .map(|(id, ins)| Call {
            ov: *id,
            inputs: ins,
            outputs: &outs,
            site_bytes: 0,
            guard: None,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let o = h.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut TestCtx::default());
    assert_eq!(o, StepOutcome::Ok);
    for (k, (t, w)) in ok.iter().enumerate() {
        assert_eq!(h.out(k, 0).to_bits(), w.to_bits(), "{t:?}");
    }
    for k in ok.len()..rows.len() {
        assert!(h.out(k, 0).is_nan(), "row {k}");
    }
}

/// `rows` over bars whose feed column 0 is `feed[i]`; every row's i64 output per bar. A fault
/// stops the run and is returned.
fn str_bars(
    rows: &[(OverloadId, Vec<(Lane, Src)>)],
    feed: &[f64],
    d: Dialect,
) -> Result<Vec<Vec<f64>>, FaultKind> {
    let outs = [Lane::I64];
    let calls: Vec<Call> = rows
        .iter()
        .map(|(id, ins)| Call {
            ov: *id,
            inputs: ins,
            outputs: &outs,
            site_bytes: pine_spec::OVERLOADS[id.0 as usize].state.block_bytes(0),
            guard: None,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: d,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx::default();
    let mut out = Vec::new();
    for (i, &x) in feed.iter().enumerate() {
        ctx.feed = vec![x];
        if h.step_bar(&bar(i as u32, 1.0, barflag::CONFIRMED), &mut ctx) != StepOutcome::Ok {
            return Err(h.last_fault().unwrap().0);
        }
        out.push((0..rows.len()).map(|k| h.out(k, 0)).collect());
    }
    Ok(out)
}

/// One site's `RegexCache` follows a `series` pattern: a changed pattern recompiles in place, a
/// repeated one reuses the cache; the match is a view of the source; a run-time pattern outside
/// the subset is `ScriptRefused`.
#[test]
fn str_match_caches_and_recompiles() {
    let feed_i = (Lane::I64, Src::Feed(0));
    let rows = vec![
        // 0: the pattern, "x[0-9]+" (feed 0) or "[0-9]+" (feed 1).
        (ov::STR_SUBSTRING, vec![st("x[0-9]+"), feed_i, it(7)]),
        // 1: the match in "ax123b".
        (
            ov::STR_MATCH,
            vec![st("ax123b"), (Lane::I64, Src::Out(0, 0))],
        ),
        (ov::STR_LENGTH, vec![(Lane::I64, Src::Out(1, 0))]),
        (ov::OP_EQ_STR, vec![(Lane::I64, Src::Out(1, 0)), st("x123")]),
        (ov::STR_TOSTRING_S, vec![st("ax123b")]),
    ];
    let feed = [0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0];
    let g = str_bars(&rows, &feed, Dialect::V6).unwrap();
    for (i, &x) in feed.iter().enumerate() {
        let x = x as i64;
        assert_eq!(g[i][2], (4 - x) as f64, "bar {i}: match length");
        assert_eq!(g[i][3], (x == 0) as u8 as f64, "bar {i}: match text");
        let (src, m) = (konst(g[i][4]), konst(g[i][1]));
        assert_eq!(m, (src.0 + 1 + x as u64, (4 - x) as u64), "bar {i}: a view");
    }
    // A run-time pattern outside the subset ("x[0-9]" cut to "x[0-") is refused.
    let rows = vec![
        (ov::STR_SUBSTRING, vec![st("x[0-9]+"), it(0), feed_i]),
        (ov::STR_MATCH, vec![st("ax1"), (Lane::I64, Src::Out(0, 0))]),
    ];
    assert_eq!(
        str_bars(&rows, &[7.0, 4.0], Dialect::V6).unwrap_err(),
        FaultKind::ScriptRefused
    );
}

/// No match → `""` in both dialects (U07's default reading, `pin:U07-str-match-none`); na
/// source or pattern → na.
#[test]
fn str_match_none_switch() {
    let rows = vec![
        (ov::STR_MATCH, vec![st("abc"), st("\\d+")]),
        (ov::STR_LENGTH, vec![(Lane::I64, Src::Out(0, 0))]),
        (ov::STR_MATCH, vec![NA_S, st("a")]),
        (ov::STR_MATCH, vec![st("abc"), NA_S]),
        (ov::STR_MATCH, vec![st("abc"), st("^b")]),
        (ov::STR_LENGTH, vec![(Lane::I64, Src::Out(4, 0))]),
    ];
    for d in [Dialect::V5, Dialect::V6] {
        let empty =
            pine_spec::sw(pine_spec::Sw::StrMatchNone, d) == pine_spec::swr::STR_MATCH_NONE_EMPTY;
        let g = &str_bars(&rows, &[0.0], d).unwrap()[0];
        if empty {
            assert_eq!((g[1], g[5]), (0.0, 0.0), "{d:?}: \"\"");
        } else {
            assert!(g[0].is_nan() && g[4].is_nan(), "{d:?}: na");
        }
        assert!(g[2].is_nan() && g[3].is_nan());
    }
}

/// `$op.add_str`: one scratch string; na operand → na.
#[test]
fn add_str_concatenates_and_propagates_na() {
    let rows = vec![
        (ov::OP_ADD_STR, vec![st("h\u{e9}l"), st("lo")]),
        (
            ov::OP_EQ_STR,
            vec![(Lane::I64, Src::Out(0, 0)), st("h\u{e9}llo")],
        ),
        (ov::STR_LENGTH, vec![(Lane::I64, Src::Out(0, 0))]),
        (ov::OP_ADD_STR, vec![NA_S, st("x")]),
        (ov::OP_ADD_STR, vec![st(""), st("")]),
        (ov::STR_LENGTH, vec![(Lane::I64, Src::Out(4, 0))]),
    ];
    let g = &str_bars(&rows, &[0.0, 0.0], Dialect::V6).unwrap()[1];
    assert_eq!((g[1], g[2]), (1.0, 5.0));
    assert!(g[3].is_nan());
    assert_eq!(g[5], 0.0);
}
