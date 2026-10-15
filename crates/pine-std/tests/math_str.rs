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
