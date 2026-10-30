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
    // Searches: na → false. Comparisons: byte equality, an na operand reads as `""`
    // (`pin:04-na-reads-empty`, r33c: A053 `na == na` true, A054 `na != na` false).
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
        vec![1., 0., 1., 1., 0., 0., 1., 1., 1., 1.]
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

/// `$op.add_str`: one scratch string; an na operand reads as `""` (`pin:04-na-reads-empty`,
/// r33c, fixture A063: `na + "ab"` is `"ab"`) — the other operand comes back as itself.
#[test]
fn add_str_concatenates_and_reads_na_as_empty() {
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
        (ov::STR_LENGTH, vec![(Lane::I64, Src::Out(3, 0))]),
        (ov::OP_ADD_STR, vec![NA_S, NA_S]),
        (ov::STR_LENGTH, vec![(Lane::I64, Src::Out(7, 0))]),
    ];
    let g = &str_bars(&rows, &[0.0, 0.0], Dialect::V6).unwrap()[1];
    assert_eq!((g[1], g[2]), (1.0, 5.0));
    assert!(!g[3].is_nan());
    assert_eq!(g[5], 0.0);
    assert_eq!(g[6], 1.0);
    assert_eq!(g[8], 0.0);
}

// ---- T13: the copying str.* family (contract A7.2) -----------------------------------------

fn o(k: u16) -> (Lane, Src) {
    (Lane::I64, Src::Out(k, 0))
}

/// `str.upper` / `str.lower`: ASCII letters only (`pin:04-case-ascii`), other bytes kept; a source
/// with nothing to change comes back as itself (zero-copy); na → na.
#[test]
fn str_case_is_ascii_only_and_in_place() {
    let rows = vec![
        (ov::STR_UPPER, vec![st("aBc\u{e9}z1")]),
        (ov::OP_EQ_STR, vec![o(0), st("ABC\u{e9}Z1")]),
        (ov::STR_LOWER, vec![st("AbC\u{c9}")]),
        (ov::OP_EQ_STR, vec![o(2), st("abc\u{c9}")]),
        (ov::STR_UPPER, vec![st("ABC-1")]),
        (ov::STR_TOSTRING_S, vec![st("ABC-1")]),
        (ov::STR_UPPER, vec![NA_S]),
        (ov::STR_LOWER, vec![NA_S]),
    ];
    let g = str_rows(&rows);
    assert_eq!((g[1], g[3]), (1.0, 1.0));
    assert_eq!(
        konst(g[4]),
        konst(g[5]),
        "nothing to change: the source itself"
    );
    assert!(g[6].is_nan() && g[7].is_nan());
}

/// `str.replace`: the N-th non-overlapping match (0 = first, the reference's example); no such
/// occurrence, a negative one or an empty target → the source itself (`pin:04-replace-edges`);
/// any na → na.
#[test]
fn str_replace_nth_and_edges() {
    let src = "FTX:BTCUSD / FTX:BTCEUR";
    let rows = vec![
        (
            ov::STR_REPLACE,
            vec![st(src), st("FTX"), st("BINANCE"), it(0)],
        ),
        (ov::OP_EQ_STR, vec![o(0), st("BINANCE:BTCUSD / FTX:BTCEUR")]),
        (
            ov::STR_REPLACE,
            vec![st(src), st("FTX"), st("BINANCE"), it(1)],
        ),
        (ov::OP_EQ_STR, vec![o(2), st("FTX:BTCUSD / BINANCE:BTCEUR")]),
        (ov::STR_TOSTRING_S, vec![st(src)]),
        (ov::STR_REPLACE, vec![st(src), st("FTX"), st("X"), it(2)]),
        (ov::STR_REPLACE, vec![st(src), st("FTX"), st("X"), it(-1)]),
        (ov::STR_REPLACE, vec![st(src), st(""), st("X"), it(0)]),
        // "aaa": "aa" occurs once non-overlapping, so occurrence 1 does not exist.
        (ov::STR_REPLACE, vec![st("aaa"), st("aa"), st("b"), it(1)]),
        (ov::STR_TOSTRING_S, vec![st("aaa")]),
        (ov::STR_REPLACE, vec![st("abc"), st("b"), st(""), it(0)]),
        (ov::OP_EQ_STR, vec![o(10), st("ac")]),
        (ov::STR_REPLACE, vec![NA_S, st("a"), st("b"), it(0)]),
        (ov::STR_REPLACE, vec![st("a"), st("a"), NA_S, it(0)]),
        (ov::STR_REPLACE, vec![st("a"), st("a"), st("b"), NA_S]),
    ];
    let g = str_rows(&rows);
    assert_eq!((g[1], g[3], g[11]), (1.0, 1.0, 1.0));
    for k in [5, 6, 7] {
        assert_eq!(konst(g[k]), konst(g[4]), "row {k}: the source itself");
    }
    assert_eq!(konst(g[8]), konst(g[9]));
    assert!(g[12].is_nan() && g[13].is_nan() && g[14].is_nan());
}

/// `str.replace_all`: every non-overlapping match, left to right; none → the source itself.
#[test]
fn str_replace_all_sizes_then_fills() {
    let rows = vec![
        (ov::STR_REPLACE_ALL, vec![st("a.b.c."), st("."), st("--")]),
        (ov::OP_EQ_STR, vec![o(0), st("a--b--c--")]),
        (ov::STR_REPLACE_ALL, vec![st("aaaaa"), st("aa"), st("b")]),
        (ov::OP_EQ_STR, vec![o(2), st("bba")]),
        (ov::STR_REPLACE_ALL, vec![st("xyz"), st("q"), st("r")]),
        (ov::STR_TOSTRING_S, vec![st("xyz")]),
        (ov::STR_REPLACE_ALL, vec![st("xyz"), st(""), st("r")]),
        (ov::STR_REPLACE_ALL, vec![st("x"), NA_S, st("r")]),
        // A SCRATCH source: the fresh range lies past it (A7.2's disjointness, debug-asserted).
        (ov::STR_UPPER, vec![st("ab-ab")]),
        (ov::STR_REPLACE_ALL, vec![o(8), st("AB"), st("c")]),
        (ov::OP_EQ_STR, vec![o(9), st("c-c")]),
    ];
    let g = str_rows(&rows);
    assert_eq!((g[1], g[3], g[10]), (1.0, 1.0, 1.0));
    assert_eq!(konst(g[4]), konst(g[5]));
    assert_eq!(konst(g[6]), konst(g[5]));
    assert!(g[7].is_nan());
}

/// `str.repeat`: the reference's example; 0 → ""; 1 → the source itself; na source → na (the
/// remark); a negative count → `ScriptRefused` (`pin:04-repeat-negative`); an output larger than
/// the arena → `StrArenaFull`, nothing written.
#[test]
fn str_repeat_example_edges_and_arena_full() {
    let rows = vec![
        (ov::STR_REPEAT, vec![st("?"), it(3), st(",")]),
        (ov::OP_EQ_STR, vec![o(0), st("?,?,?")]),
        (ov::STR_REPEAT, vec![st("ab"), it(0), st(",")]),
        (ov::STR_LENGTH, vec![o(2)]),
        (ov::STR_REPEAT, vec![st("ab"), it(1), st(",")]),
        (ov::STR_TOSTRING_S, vec![st("ab")]),
        (ov::STR_REPEAT, vec![NA_S, it(2), st("")]),
        (ov::STR_REPEAT, vec![st("ab"), it(2), st("")]),
        (ov::OP_EQ_STR, vec![o(7), st("abab")]),
        (ov::STR_REPEAT, vec![st(""), it(1_000_000_000), st("")]),
        (ov::STR_LENGTH, vec![o(9)]),
    ];
    let g = str_rows(&rows);
    assert_eq!((g[1], g[3], g[8], g[10]), (1.0, 0.0, 1.0, 0.0));
    assert_eq!(konst(g[4]), konst(g[5]));
    assert!(g[6].is_nan());
    let neg = vec![(ov::STR_REPEAT, vec![st("ab"), it(-1), st("")])];
    assert_eq!(
        str_bars(&neg, &[0.0], Dialect::V6).unwrap_err(),
        FaultKind::ScriptRefused
    );
    let big = vec![(ov::STR_REPEAT, vec![st("ab"), it(1_000_000), st(",")])];
    assert_eq!(
        str_bars(&big, &[0.0], Dialect::V6).unwrap_err(),
        FaultKind::StrArenaFull
    );
}

// ---- T12: str.tostring(int / float / bool[, format]) ----------------------------------------

fn ft(x: f64) -> (Lane, Src) {
    (Lane::F64, Src::Const(x.to_bits()))
}

/// impl-04 T12 / U08: the default rendering in both dialects follows `Sw::TostringDefault` (8 or
/// 10 digits — the renderer's two shapes are pinned in `strnum::tests`); trailing zeros trimmed;
/// `0.5`, not `.5`; na → `"NaN"` (the reference).
#[test]
fn tostring_default_switch() {
    let rows = vec![
        (ov::STR_TOSTRING_F, vec![ft(1.0 / 3.0)]),
        (ov::OP_EQ_STR, vec![o(0), st("0.33333333")]),
        (ov::OP_EQ_STR, vec![o(0), st("0.3333333333")]),
        (ov::STR_TOSTRING_F, vec![ft(0.5)]),
        (ov::OP_EQ_STR, vec![o(3), st("0.5")]),
        (ov::STR_TOSTRING_F, vec![ft(-12.0)]),
        (ov::OP_EQ_STR, vec![o(5), st("-12")]),
        (ov::STR_TOSTRING_F, vec![ft(f64::NAN)]),
        (ov::OP_EQ_STR, vec![o(7), st("NaN")]),
    ];
    for d in [Dialect::V5, Dialect::V6] {
        let ten = pine_spec::sw(pine_spec::Sw::TostringDefault, d)
            == pine_spec::swr::TOSTRING_DEFAULT_FRAC_UPTO_10;
        let g = &str_bars(&rows, &[0.0], d).unwrap()[0];
        assert_eq!((g[1], g[2]), (!ten as u8 as f64, ten as u8 as f64), "{d:?}");
        assert_eq!((g[4], g[6], g[8]), (1.0, 1.0, 1.0), "{d:?}");
    }
}

/// `str.tostring` of int / bool, and with a `format`: the reference's `'#'` and `'#.000'`
/// examples, grouping, a quoted affix, `format.mintick` (snapped, trailing zeros); na → `"NaN"`;
/// a run-time pattern outside the subset → `ScriptRefused` (`pin:04-tostring-bad-pattern`).
#[test]
fn tostring_overloads_and_formats() {
    let rows = vec![
        (ov::STR_TOSTRING_I, vec![it(-9_007_199_254_740_993)]),
        (ov::OP_EQ_STR, vec![o(0), st("-9007199254740993")]),
        (ov::STR_TOSTRING_B, vec![it(1)]),
        (ov::OP_EQ_STR, vec![o(2), st("true")]),
        (ov::STR_TOSTRING_FS, vec![ft(3.99), st("#")]),
        (ov::OP_EQ_STR, vec![o(4), st("4")]),
        (ov::STR_TOSTRING_FS, vec![ft(1.5), st("#.000")]),
        (ov::OP_EQ_STR, vec![o(6), st("1.500")]),
        (
            ov::STR_TOSTRING_FS,
            vec![ft(1234567.891), st("'$'#,##0.00' ok'")],
        ),
        (ov::OP_EQ_STR, vec![o(8), st("$1,234,567.89 ok")]),
        (ov::STR_TOSTRING_FS, vec![ft(1.23456), st("mintick")]),
        (ov::OP_EQ_STR, vec![o(10), st("1.23")]),
        (ov::STR_TOSTRING_FS, vec![ft(2.0), st("mintick")]),
        (ov::OP_EQ_STR, vec![o(12), st("2.00")]),
        (ov::STR_TOSTRING_IS, vec![it(1234), st("#,###")]),
        (ov::OP_EQ_STR, vec![o(14), st("1,234")]),
        (ov::STR_TOSTRING_FS, vec![ft(0.256), st("#.#%")]),
        (ov::OP_EQ_STR, vec![o(16), st("25.6%")]),
        (ov::STR_TOSTRING_I, vec![NA_S]),
        (ov::OP_EQ_STR, vec![o(18), st("NaN")]),
        (ov::STR_TOSTRING_FS, vec![ft(f64::NAN), st("#.00")]),
        (ov::OP_EQ_STR, vec![o(20), st("NaN")]),
        (ov::STR_TOSTRING_FS, vec![ft(1.0), NA_S]),
    ];
    let g = &str_bars(&rows, &[0.0], Dialect::V6).unwrap()[0];
    for k in (1..22).step_by(2) {
        assert_eq!(g[k], 1.0, "row {k}");
    }
    assert!(g[22].is_nan());
    let bad = vec![(ov::STR_TOSTRING_FS, vec![ft(1.0), st("#E0")])];
    assert_eq!(
        str_bars(&bad, &[0.0], Dialect::V6).unwrap_err(),
        FaultKind::ScriptRefused
    );
}

// ---- T13: str.format / str.format_time -------------------------------------------------------

/// `__types` for argument types `ts` (4 bits each, argument 0 lowest; impl-04 §3.4).
fn types(ts: &[Ty]) -> (Lane, Src) {
    let mut v = 0u64;
    for (k, t) in ts.iter().enumerate() {
        v |= (*t as u64) << (4 * k);
    }
    it(v as i64)
}

/// `str.format`: plain, number-pattern, keyword (`pin:04-fmt-keywords`), quoted and date
/// placeholders; a missing argument stays `{n}` (`pin:04-fmt-missing-arg`); a SCRATCH string
/// argument is lent with the range (A7.2); na template → na; an unbalanced `series` template →
/// `ScriptRefused` (the reference: a runtime error).
#[test]
fn str_format_placeholders() {
    use Ty::{Float as F, Int as I, String as S};
    let rows = vec![
        (
            ov::STR_FORMAT_F,
            vec![types(&[F, I]), st("{0} and {1}"), ft(1.5), it(7)],
        ),
        (ov::OP_EQ_STR, vec![o(0), st("1.5 and 7")]),
        (
            ov::STR_FORMAT_F,
            vec![types(&[F]), st("x={0,number,#.##}"), ft(4.56789)],
        ),
        (ov::OP_EQ_STR, vec![o(2), st("x=4.57")]),
        (
            ov::STR_FORMAT_F,
            vec![
                types(&[F, F, F]),
                st("{0,number}|{1,number,integer}|{2,number,percent}"),
                ft(1234.5),
                ft(2.5),
                ft(0.256),
            ],
        ),
        (ov::OP_EQ_STR, vec![o(4), st("1,234.5|2|26%")]),
        (
            ov::STR_FORMAT_F,
            vec![types(&[F]), st("{0,number,currency}"), ft(-1.5)],
        ),
        (ov::OP_EQ_STR, vec![o(6), st("-$1.50")]),
        (ov::STR_UPPER, vec![st("x")]),
        (
            ov::STR_FORMAT_S,
            vec![types(&[S]), st("'{'{0}'}' it''s"), o(8)],
        ),
        (ov::OP_EQ_STR, vec![o(9), st("{X} it's")]),
        (
            ov::STR_FORMAT_I,
            vec![
                types(&[I]),
                st("{0,date,yyyy-MM-dd HH:mm:ss} {1}"),
                it(1_704_585_600_000),
            ],
        ),
        (ov::OP_EQ_STR, vec![o(11), st("2024-01-07 00:00:00 {1}")]),
        (ov::STR_FORMAT_I, vec![types(&[I]), NA_S, it(1)]),
        (
            ov::STR_FORMAT_F,
            vec![types(&[F]), st("<{0}>"), ft(f64::NAN)],
        ),
        (ov::OP_EQ_STR, vec![o(14), st("<NaN>")]),
    ];
    let g = &str_bars(&rows, &[0.0], Dialect::V6).unwrap()[0];
    for k in [1, 3, 5, 7, 10, 12, 15] {
        assert_eq!(g[k], 1.0, "row {k}");
    }
    assert!(g[13].is_nan());
    let bad = vec![(ov::STR_FORMAT_I, vec![types(&[I]), st("ab {0"), it(1)])];
    assert_eq!(
        str_bars(&bad, &[0.0], Dialect::V6).unwrap_err(),
        FaultKind::ScriptRefused
    );
}

/// `str.format_time`: the reference's ISO default pattern with `Z`, 12-hour `h` + `a`, names
/// (`pin:04-date-tokens`), a zone offset; na → na; an unknown zone → `ScriptRefused`.
#[test]
fn str_format_time_tokens_and_zone() {
    let t = 1_704_585_600_000 + 13 * 3_600_000 + 30 * 60_000 + 42_007; // Sun 2024-01-07 13:30:42.007Z
    let rows = vec![
        (
            ov::STR_FORMAT_TIME,
            vec![it(t), st("yyyy-MM-dd'T'HH:mm:ssZ"), st("Asia/Tokyo")],
        ),
        (ov::OP_EQ_STR, vec![o(0), st("2024-01-07T22:30:42+0900")]),
        (
            ov::STR_FORMAT_TIME,
            vec![it(t), st("EEEE, MMMM d yy h:m a S"), st("Asia/Tokyo")],
        ),
        (
            ov::OP_EQ_STR,
            vec![o(2), st("Sunday, January 7 24 10:30 PM 7")],
        ),
        (
            ov::STR_FORMAT_TIME,
            vec![it(t), st("EEE MMM dd HH:mm:ss.SSS"), st("Asia/Tokyo")],
        ),
        (ov::OP_EQ_STR, vec![o(4), st("Sun Jan 07 22:30:42.007")]),
        (
            ov::STR_FORMAT_TIME,
            vec![NA_S, st("yyyy"), st("Asia/Tokyo")],
        ),
        (ov::STR_FORMAT_TIME, vec![it(t), st("yyyy"), NA_S]),
    ];
    let g = &str_bars(&rows, &[0.0], Dialect::V6).unwrap()[0];
    assert_eq!((g[1], g[3], g[5]), (1.0, 1.0, 1.0));
    assert!(g[6].is_nan() && g[7].is_nan());
    let bad = vec![(
        ov::STR_FORMAT_TIME,
        vec![it(t), st("yyyy"), st("Mars/Base")],
    )];
    assert_eq!(
        str_bars(&bad, &[0.0], Dialect::V6).unwrap_err(),
        FaultKind::ScriptRefused
    );
    let badpat = vec![(
        ov::STR_FORMAT_TIME,
        vec![it(t), st("yyyy Q"), st("Asia/Tokyo")],
    )];
    assert_eq!(
        str_bars(&badpat, &[0.0], Dialect::V6).unwrap_err(),
        FaultKind::ScriptRefused
    );
}

/// impl-04 I12: every string builder is all-or-nothing — on `StrArenaFull` the output slot keeps
/// its previous value (bits and validity). Driven on a 64-byte scratch arena through pine-vm's
/// program builder with pine-std's own `BUILTINS`.
#[test]
fn string_builders_are_all_or_nothing() {
    use pine_vm::testkit::{arena, Arena, Ctx, Tb};
    let long: &'static str = Box::leak("a".repeat(100).into_boxed_str());
    // (overload, inputs as CONST strings or ints, output entry is appended)
    enum In {
        S(&'static str),
        I(i64),
        F(f64),
    }
    let cases: Vec<(OverloadId, Vec<In>)> = vec![
        (ov::STR_UPPER, vec![In::S(long)]),
        (
            ov::STR_LOWER,
            vec![In::S(
                "ABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZ",
            )],
        ),
        (
            ov::STR_REPLACE,
            vec![In::S(long), In::S("a"), In::S("bb"), In::I(0)],
        ),
        (
            ov::STR_REPLACE_ALL,
            vec![In::S(long), In::S("a"), In::S("b")],
        ),
        (ov::STR_REPEAT, vec![In::S("ab"), In::I(50), In::S("")]),
        (
            ov::STR_FORMAT_S,
            vec![In::I(Ty::String as i64), In::S("{0}{0}"), In::S(long)],
        ),
        (
            ov::STR_FORMAT_TIME,
            vec![In::I(0), In::S(&long[..0]), In::S("Asia/Tokyo")],
        ),
        (
            ov::STR_TOSTRING_FS,
            vec![
                In::F(1.0),
                In::S("0000000000000000000000000000000000000000000000000000000000000000000000"),
            ],
        ),
    ];
    for (k, (id, ins)) in cases.into_iter().enumerate() {
        let mut t = Tb::new(ExecMode::BarClose, 1, 6, 0, 0);
        t.scratch = 64;
        t.spec = pine_spec::SPEC_SHA256;
        let out = t.i(5);
        let keep = t.kstr("keep");
        t.emit(op::I_CONST, flg::VLD, out, keep, 0);
        let mut win = Vec::new();
        for (j, x) in ins.iter().enumerate() {
            match x {
                In::S(s) => {
                    // A format_time pattern of 100 `y`s renders a 100-digit year.
                    let s: &str = if id == ov::STR_FORMAT_TIME && j == 1 {
                        Box::leak("y".repeat(100).into_boxed_str())
                    } else {
                        s
                    };
                    let c = t.kstr(s);
                    t.emit(op::I_CONST, flg::VLD, t.i(j as u32), c, 0);
                    win.push(argwin::i64(t.i(j as u32)));
                }
                In::I(v) => {
                    let c = t.k(*v as u64);
                    t.emit(op::I_CONST, flg::VLD, t.i(j as u32), c, 0);
                    win.push(argwin::i64(t.i(j as u32)));
                }
                In::F(v) => {
                    let c = t.kf(*v);
                    t.emit(op::F_CONST, 0, t.f(0), c, 0);
                    win.push(argwin::f64(t.f(0)));
                }
            }
        }
        win.push(argwin::i64(out));
        let w = t.win(&win);
        t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, id.0, w);
        t.emit(op::HALT, 0, 0, 0, 0);
        let p = t.build();
        let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
        let mut a = Arena::new(p.layout.total_bytes as usize);
        let mut vm = pine_vm::VmInstance::new(v, a.slice());
        let o = vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut Ctx { now: 0 });
        assert_eq!(o, StepOutcome::Fault(FaultKind::StrArenaFull), "case {k}");
        let img = arena(&vm);
        let at = p.layout.i64_off as usize + 8 * out as usize;
        let bits = u64::from_le_bytes(img[at..at + 8].try_into().unwrap());
        assert_eq!(
            bits, t.consts[keep as usize],
            "case {k}: the slot keeps its value"
        );
        let vb = p.layout.f64_len as usize + out as usize;
        let word = u64::from_le_bytes(
            img[p.layout.valid_off as usize + 8 * (vb / 64)..][..8]
                .try_into()
                .unwrap(),
        );
        assert!(word >> (vb % 64) & 1 == 1, "case {k}: and its validity");
    }
}
