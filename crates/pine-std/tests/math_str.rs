// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.6 — `math.*` and the cast bodies, driven through `pine_vm::verify` + `VmInstance`
//! with pine-vm's hand-assembly builder and a probe row appended to `BUILTINS` (§7.1).
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::ov;
use pine_std::BUILTINS;
use pine_vm::testkit::*;
use pine_vm::*;

/// Captures every window entry as plot `k` (i64 entries as f64, na as NaN).
fn probe(fr: &mut Frame<'_>, _: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    let mut k = 0usize;
    while k < a.len() {
        let v = if argwin::is_i64(a[k]) {
            fr.i64_arg(a[k]).map_or(f64::NAN, |x| x as f64)
        } else {
            fr.f64_arg(a[k])
        };
        fr.plot_set(k as u32, v);
        k += 1;
    }
    Ok(())
}

fn table() -> &'static BuiltinTable {
    let mut v: Vec<BuiltinEntry> = BUILTINS.entries.to_vec();
    v.push(BuiltinEntry {
        f: probe,
        cost: 1,
        n_out: 0,
        flags: bf::CAPTURE,
        state_bytes_min: 0,
    });
    let entries: &'static [BuiltinEntry] = Box::leak(v.into_boxed_slice());
    Box::leak(Box::new(BuiltinTable {
        spec_sha256: pine_spec::SPEC_SHA256,
        entries,
    }))
}

#[derive(Clone, Copy)]
enum V {
    F(f64),
    I(Option<i64>),
}

/// Call `id` once on `inputs`, its output in the `int` lane when `out_i64`; return the output.
fn call(id: OverloadId, inputs: &[V], out_i64: bool) -> f64 {
    let mut t = Tb::new(ExecMode::BarClose, 8, 8, 0, 0);
    t.spec = pine_spec::SPEC_SHA256;
    t.n_plots = 1;
    let (mut nf, mut ni) = (0u32, 0u32);
    let mut win: Vec<u16> = Vec::new();
    for v in inputs {
        match *v {
            V::F(x) => {
                let s = t.f(nf);
                nf += 1;
                let k = t.kf(x);
                t.emit(op::F_CONST, 0, s, k, 0);
                win.push(argwin::f64(s));
            }
            V::I(x) => {
                let s = t.i(ni);
                ni += 1;
                let k = t.k(x.unwrap_or(0) as u64);
                t.emit(op::I_CONST, if x.is_some() { flg::VLD } else { 0 }, s, k, 0);
                win.push(argwin::i64(s));
            }
        }
    }
    let out = if out_i64 {
        argwin::i64(t.i(ni))
    } else {
        argwin::f64(t.f(nf))
    };
    win.push(out);
    let w = t.win(&win);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, id.0, w);
    let wp = t.win(&[out]);
    t.emit(
        op::CALL_BUILTIN,
        0,
        SiteId::NONE.0,
        pine_spec::OVERLOAD_COUNT as u16,
        wp,
    );
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, table()).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut ctx = Ctx { now: 0 };
    let mut vm = VmInstance::new(v, ar.slice());
    assert_eq!(
        vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx),
        StepOutcome::Ok
    );
    vm.plots()[0]
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
    assert!(call(ov::MATH_ABS_I, &[V::I(Some(i64::MIN))], true).is_nan());
}
