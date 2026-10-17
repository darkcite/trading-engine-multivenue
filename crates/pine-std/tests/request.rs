// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.10 — `request.security` (T30) through `pine_std::testing`: gaps (I20), nothing
//! published, output lanes, two keys, `request.currency_rate`. The kernel's table (I18) is
//! `request::tests` in the crate.
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::{ov, OVERLOADS};
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, TestSeries, CHART};
use pine_vm::testkit::bar;

const KEY: u64 = 0xA11CE;
const M: i64 = 60_000;

fn it(x: i64) -> (Lane, Src) {
    (Lane::I64, Src::Const(x as u64))
}

/// A `request.security` call: `[__key, __out, __ty, gaps, lookahead]`, output in `lane`.
fn sec(key: u64, ty: Ty, gaps: bool, look: bool) -> Vec<(Lane, Src)> {
    vec![
        it(key as i64),
        it(0),
        it(elem::scalar(ty) as i64),
        it(gaps as i64),
        it(look as i64),
    ]
}

/// Run `calls` (each `(inputs, lane)`) over `n` one-minute chart bars; `publish(i)` is the series
/// the host publishes before bar `i`. Returns each call's output per bar.
fn run(
    calls: &[(Vec<(Lane, Src)>, Lane)],
    d: Dialect,
    n: usize,
    publish: impl Fn(usize) -> Vec<TestSeries>,
) -> Vec<Vec<f64>> {
    let outs: Vec<[Lane; 1]> = calls.iter().map(|c| [c.1]).collect();
    let cs: Vec<Call> = calls
        .iter()
        .zip(&outs)
        .map(|(c, o)| Call {
            ov: ov::REQUEST_SECURITY,
            inputs: &c.0,
            outputs: o,
            site_bytes: OVERLOADS[ov::REQUEST_SECURITY.0 as usize].state_bytes_min,
            guard: None,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &cs,
        mode: ExecMode::BarClose,
        dialect: d,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx::default();
    let mut out = Vec::new();
    for i in 0..n {
        ctx.series = publish(i);
        let mut b = bar(i as u32, 1.0, barflag::CONFIRMED);
        b.open_ms = i as i64 * M;
        b.close_ms = (i as i64 + 1) * M;
        assert_eq!(h.step_bar(&b, &mut ctx), StepOutcome::Ok, "bar {i}");
        out.push((0..calls.len()).map(|k| h.out(k, 0)).collect());
    }
    out
}

/// A 4-minute series: the HTF bars closed by chart bar `i`'s close (§4.14.3), value = 10 + index.
fn htf(i: usize) -> Vec<TestSeries> {
    let closed = (i + 1) / 4;
    let mut t = TestSeries {
        key: KEY,
        out: 0,
        last_confirmed: true,
        ..TestSeries::default()
    };
    for k in 0..closed {
        t.values.push(10.0 + k as f64);
        t.open_ms.push(k as i64 * 4 * M);
        t.close_ms.push((k as i64 + 1) * 4 * M);
    }
    vec![t]
}

/// I20: `gaps_on` emits exactly on the chart bars where a new confirmed HTF element is first
/// selected (na elsewhere); `gaps_off` on every bar once an element exists.
#[test]
fn gaps_on_emits_only_on_new_confirmed_data() {
    let calls = [
        (sec(KEY, Ty::Float, true, false), Lane::F64),
        (sec(KEY, Ty::Float, false, false), Lane::F64),
    ];
    let r = run(&calls, Dialect::V6, 17, htf);
    for (i, row) in r.iter().enumerate() {
        let closed = (i + 1) / 4;
        let new = (i + 1) % 4 == 0;
        if new {
            assert_eq!(row[0], 10.0 + (closed - 1) as f64, "bar {i}: emits");
        } else {
            assert!(row[0].is_nan(), "bar {i}: gaps_on na");
        }
        if closed == 0 {
            assert!(row[1].is_nan(), "bar {i}: nothing yet");
        } else {
            assert_eq!(row[1], 10.0 + (closed - 1) as f64, "bar {i}: gaps_off");
        }
    }
}

/// `pin:U12-gaps-on-realtime` (lead ruling CI-056, D054 sheet-rt-02 `k` column): realtime 1-minute
/// bars of 3 ticks + a confirm under a 5-minute series (the host re-publishes the developing
/// element per update, confirms it at the period's last bar). `gaps_on`, lookahead off, reads na
/// on every update of the other bars (k = upd) and a value on every update of the period-closing
/// bar (k = 0): its ticks the developing value, its confirm the confirmed one — the ticks'
/// journaled emissions never consume the confirm's.
#[test]
fn gaps_on_emits_on_every_update_of_the_period_closing_bar_d054() {
    const H: i64 = 5 * M;
    let ins = sec(KEY, Ty::Float, true, false);
    let calls = [Call {
        ov: ov::REQUEST_SECURITY,
        inputs: &ins,
        outputs: &[Lane::F64],
        site_bytes: OVERLOADS[ov::REQUEST_SECURITY.0 as usize].state_bytes_min,
        guard: None,
    }];
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::Realtime,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    // Chart bar `i` after update `j` (0..3 ticks, 3 = the confirm): the closed periods, plus the
    // developing one unless bar `i` closes it at its confirm (value 100 + g, + (j + 1) / 10 while
    // developing).
    let series = |i: usize, j: usize| {
        let g = i / 5;
        let closes = j == 3 && i % 5 == 4;
        let mut t = TestSeries {
            key: KEY,
            out: 0,
            last_confirmed: closes,
            ..TestSeries::default()
        };
        for k in 0..=g {
            let dev = k == g && !closes;
            let extra = if dev { (j + 1) as f64 / 10.0 } else { 0.0 };
            t.values.push(100.0 + k as f64 + extra);
            t.open_ms.push(k as i64 * H);
            t.close_ms.push((k as i64 + 1) * H);
        }
        vec![t]
    };
    let mut ctx = TestCtx::default();
    for i in 0..15usize {
        let mut seen = Vec::new();
        for j in 0..4usize {
            ctx.series = series(i, j);
            let fl = if j == 3 {
                barflag::REALTIME | barflag::CONFIRMED
            } else {
                barflag::REALTIME
            };
            let mut b = bar(i as u32, 1.0, fl);
            b.open_ms = i as i64 * M;
            b.close_ms = (i as i64 + 1) * M;
            let out = if j == 3 {
                h.step_bar(&b, &mut ctx)
            } else {
                h.step_tick(&b, &mut ctx)
            };
            assert_eq!(out, StepOutcome::Ok, "bar {i} update {j}");
            seen.push(h.out(0, 0));
        }
        let g = (i / 5) as f64;
        if i % 5 == 4 {
            let want = [100.1 + g, 100.2 + g, 100.3 + g, 100.0 + g];
            assert_eq!(seen, want, "bar {i}: k = 0 (every update emits)");
        } else {
            assert!(seen.iter().all(|v| v.is_nan()), "bar {i}: k = upd {seen:?}");
        }
    }
}

/// Nothing published → na; a key the host never published → na, the other key unaffected.
#[test]
fn request_nothing_published_is_na_and_keys_do_not_collide() {
    let calls = [
        (sec(KEY, Ty::Float, false, false), Lane::F64),
        (sec(KEY + 1, Ty::Float, false, false), Lane::F64),
    ];
    let r = run(&calls, Dialect::V6, 8, htf);
    assert!(r[0][0].is_nan() && r[2][0].is_nan());
    assert_eq!(r[7][0], 11.0);
    assert!(r.iter().all(|row| row[1].is_nan()));
}

/// Output lanes follow `__ty`: int, bool (v6 na → false, v5 na), color.
#[test]
fn request_output_lanes_follow_ty() {
    let calls = [
        (sec(KEY, Ty::Int, false, false), Lane::I64),
        (sec(KEY, Ty::Bool, false, false), Lane::I64),
        (sec(KEY, Ty::Color, false, false), Lane::I64),
    ];
    for d in [Dialect::V5, Dialect::V6] {
        let r = run(&calls, d, 8, htf);
        assert_eq!((r[7][0], r[7][1], r[7][2]), (11.0, 1.0, 11.0));
        assert!(r[0][0].is_nan() && r[0][2].is_nan());
        match d {
            Dialect::V6 => assert_eq!(r[0][1], 0.0, "v6 bool never na"),
            Dialect::V5 => assert!(r[0][1].is_nan(), "v5 bool na"),
        }
    }
}

/// `request.currency_rate`: 1 for byte-equal currencies, else na.
#[test]
fn request_currency_rate_is_one_or_na() {
    for (a, b, want) in [("USD", "USD", 1.0), ("USD", "EUR", f64::NAN)] {
        let ins = vec![(Lane::I64, Src::Str(a)), (Lane::I64, Src::Str(b)), it(0)];
        let calls = [Call {
            ov: ov::REQUEST_CURRENCY_RATE,
            inputs: &ins,
            outputs: &[Lane::F64],
            site_bytes: 0,
            guard: None,
        }];
        let mut h = Harness::build(&Script {
            calls: &calls,
            mode: ExecMode::BarClose,
            dialect: Dialect::V6,
            intent_cap: 0,
            chart: CHART,
        });
        let r = h.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut TestCtx::default());
        assert_eq!(r, StepOutcome::Ok);
        let g = h.out(0, 0);
        assert!(g.to_bits() == want.to_bits() || (g.is_nan() && want.is_nan()));
    }
}
