// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.2 — the builder rows of `tests/abi.rs`: `builder_programs_verify` (both modes),
//! `feed_probe_roundtrip`, and a guarded site keeping its block (I6).
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::{ov, OVERLOADS};
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, CHART};
use pine_vm::testkit::bar;

fn script<'a>(calls: &'a [Call<'a>], mode: ExecMode) -> Script<'a> {
    Script {
        calls,
        mode,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    }
}

#[test]
fn feed_probe_roundtrip_and_builder_programs_verify() {
    let f_in = [(Lane::F64, Src::Feed(0))];
    let i_in = [(Lane::I64, Src::Feed(1))];
    let calls = [
        Call {
            ov: ov::FLOAT_F,
            inputs: &f_in,
            outputs: &[Lane::F64],
            site_bytes: 0,
            guard: None,
        },
        Call {
            ov: ov::INT_I,
            inputs: &i_in,
            outputs: &[Lane::I64],
            site_bytes: 0,
            guard: None,
        },
    ];
    for mode in [ExecMode::BarClose, ExecMode::Realtime] {
        let mut h = Harness::build(&script(&calls, mode));
        for (i, (a, b)) in [(1.5, 7.0), (f64::NAN, f64::NAN), (-2.25, -3.0)]
            .into_iter()
            .enumerate()
        {
            let mut ctx = TestCtx {
                feed: vec![a, b],
                ..TestCtx::default()
            };
            assert_eq!(
                h.step_bar(&bar(i as u32, 1.0, barflag::CONFIRMED), &mut ctx),
                StepOutcome::Ok
            );
            let (x, y) = (h.out(0, 0), h.out(1, 0));
            assert!((x.is_nan() && a.is_nan()) || x == a, "{mode:?} {i}");
            assert!((y.is_nan() && b.is_nan()) || y == b, "{mode:?} {i}");
        }
    }
}

#[test]
fn guarded_site_keeps_its_block() {
    let ins = [(Lane::F64, Src::Feed(0)), (Lane::I64, Src::Const(3))];
    let bytes = OVERLOADS[ov::TA_SMA.0 as usize].state.block_bytes(3);
    let calls = [Call {
        ov: ov::TA_SMA,
        inputs: &ins,
        outputs: &[Lane::F64],
        site_bytes: bytes,
        guard: Some(1),
    }];
    let mut h = Harness::build(&script(&calls, ExecMode::BarClose));
    // (value, executed?) — the sma sees only the executed values 1, 2, 3, 4.
    let tape = [
        (1.0, 1.0),
        (50.0, 0.0),
        (2.0, 1.0),
        (60.0, 0.0),
        (3.0, 1.0),
        (4.0, 1.0),
    ];
    let mut got = Vec::new();
    for (i, (v, g)) in tape.into_iter().enumerate() {
        let mut ctx = TestCtx {
            feed: vec![v, g],
            ..TestCtx::default()
        };
        assert_eq!(
            h.step_bar(&bar(i as u32, 1.0, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        got.push(h.out(0, 0));
    }
    assert_eq!(got[4], 2.0);
    assert_eq!(got[5], 3.0);
}
