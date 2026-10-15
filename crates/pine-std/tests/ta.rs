// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.7 — `ta.sma`, `ta.ema`, `ta.rma` (T15): brute-force agreement, Wilder vs EMA, I4
//! (zero is cold) and I5 (ticks never advance builtin state), through `pine_std::testing`.
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::{ov, swr, Sw, OVERLOADS};
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, CHART, CLOSE};
use pine_vm::testkit::bar;

/// `id(close, len)` captured every bar; `ticks` forming-bar re-evaluations before each confirm.
fn run(id: OverloadId, len: i64, closes: &[f64], ticks: u32, reset_at: Option<usize>) -> Vec<f64> {
    let mode = if ticks > 0 {
        ExecMode::Realtime
    } else {
        ExecMode::BarClose
    };
    let bytes = OVERLOADS[id.0 as usize].state.block_bytes(len as u32);
    let ins = [CLOSE, (Lane::I64, Src::Const(len as u64))];
    let calls = [Call {
        ov: id,
        inputs: &ins,
        outputs: &[Lane::F64],
        site_bytes: bytes,
        guard: None,
    }];
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx::default();
    let mut out = Vec::new();
    for (i, &c) in closes.iter().enumerate() {
        if reset_at == Some(i) {
            h.reset();
            out.clear();
        }
        let mut j = 0;
        while j < ticks {
            let tick = bar(i as u32, c * (1.5 + j as f64), barflag::REALTIME);
            assert_eq!(h.step_tick(&tick, &mut ctx), StepOutcome::Ok);
            j += 1;
        }
        assert_eq!(
            h.step_bar(&bar(i as u32, c, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        out.push(h.out(0, 0));
    }
    out
}

fn tape(n: usize) -> Vec<f64> {
    let mut x = 100.0f64;
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            x += ((s >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2.0;
            (x * 100.0).round() / 100.0
        })
        .collect()
}

fn bits_eq(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits())
}

#[test]
fn ta_sma_matches_bruteforce() {
    let c = tape(3000);
    for len in [1i64, 5, 20] {
        let got = run(ov::TA_SMA, len, &c, 0, None);
        for i in 0..c.len() {
            if (i as i64) < len - 1 {
                assert!(got[i].is_nan(), "warm-up {i}");
            } else {
                let w = &c[i + 1 - len as usize..=i];
                let want = w.iter().sum::<f64>() / len as f64;
                assert!(
                    (got[i] - want).abs() < 1e-9,
                    "len {len} bar {i}: {} vs {want}",
                    got[i]
                );
            }
        }
    }
}

fn smooth_ref(c: &[f64], len: usize, alpha: f64, sma_seed: bool) -> Vec<f64> {
    let mut out = Vec::new();
    let mut v = f64::NAN;
    for (i, &x) in c.iter().enumerate() {
        if v.is_nan() {
            if !sma_seed {
                v = x;
            } else if i + 1 >= len {
                v = c[i + 1 - len..=i].iter().sum::<f64>() / len as f64;
            }
        } else {
            v = alpha.mul_add(x - v, v);
        }
        out.push(v);
    }
    out
}

#[test]
fn ta_rma_is_wilder_not_ema() {
    let c = tape(500);
    let d = Dialect::V6;
    let rma_sma = pine_spec::sw(Sw::RmaSeed, d) == swr::RMA_SEED_SMA;
    let ema_sma = pine_spec::sw(Sw::EmaSeed, d) == swr::EMA_SEED_SMA;
    let rma = run(ov::TA_RMA, 14, &c, 0, None);
    let ema = run(ov::TA_EMA, 14, &c, 0, None);
    let r = smooth_ref(&c, 14, 1.0 / 14.0, rma_sma);
    let e = smooth_ref(&c, 14, 2.0 / 15.0, ema_sma);
    for i in 0..c.len() {
        assert!(
            (r[i].is_nan() && rma[i].is_nan()) || (r[i] - rma[i]).abs() < 1e-9,
            "rma {i}"
        );
        assert!(
            (e[i].is_nan() && ema[i].is_nan()) || (e[i] - ema[i]).abs() < 1e-9,
            "ema {i}"
        );
    }
    assert!((rma[400] - ema[400]).abs() > 1e-6);
}

#[test]
fn state_zero_is_cold_for_the_moving_averages() {
    let c = tape(300);
    for id in [
        ov::TA_SMA,
        ov::TA_EMA,
        ov::TA_RMA,
        ov::TA_CHANGE_F,
        ov::TA_MOM,
    ] {
        // N bars after `reset` equal N bars on a fresh instance, bit for bit.
        let fresh = run(id, 10, &c[150..], 0, None);
        let after = run(id, 10, &c, 0, Some(150));
        assert!(bits_eq(&fresh, &after), "{id:?}");
    }
}

#[test]
fn ticks_never_advance_builtin_state() {
    let c = tape(400);
    for id in [
        ov::TA_SMA,
        ov::TA_EMA,
        ov::TA_RMA,
        ov::TA_CHANGE_F,
        ov::TA_MOM,
    ] {
        let closes_only = run(id, 7, &c, 0, None);
        let with_ticks = run(id, 7, &c, 3, None);
        assert!(bits_eq(&closes_only, &with_ticks), "{id:?}");
    }
}

#[test]
fn ta_length_out_of_range_refuses() {
    // Length 0 faults ScriptRefused (pin:04-ta-reference) instead of a silent na.
    let ins = [CLOSE, (Lane::I64, Src::Const(0))];
    let calls = [Call {
        ov: ov::TA_EMA,
        inputs: &ins,
        outputs: &[Lane::F64],
        site_bytes: 24,
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
    let o = h.step_bar(&bar(0, 100.0, barflag::CONFIRMED), &mut TestCtx::default());
    assert_ne!(o, StepOutcome::Ok);
    assert_eq!(h.last_fault().map(|f| f.0), Some(FaultKind::ScriptRefused));
}

#[derive(Clone, Copy)]
enum In {
    Close,
    F(f64),
    Int(i64),
    Bool(bool),
}

/// `id(ins…)` plotted every bar over OHLC bars `(o, h, l, c)`; `len` sizes a window block.
fn run_g(id: OverloadId, ins: &[In], len: u32, bars: &[(f64, f64, f64)]) -> Vec<f64> {
    run_o(id, ins, len, bars, false)
}
/// As `run_g`; `out_i64` puts the output in the i64 lane (bool/int results).
fn run_o(
    id: OverloadId,
    ins: &[In],
    len: u32,
    bars: &[(f64, f64, f64)],
    out_i64: bool,
) -> Vec<f64> {
    let inputs: Vec<(Lane, Src)> = ins
        .iter()
        .map(|x| match *x {
            In::Close => CLOSE,
            In::F(v) => (Lane::F64, Src::Const(v.to_bits())),
            In::Int(v) => (Lane::I64, Src::Const(v as u64)),
            In::Bool(b) => (Lane::I64, Src::Const(b as u64)),
        })
        .collect();
    let lane = if out_i64 { Lane::I64 } else { Lane::F64 };
    let bytes = OVERLOADS[id.0 as usize].state.block_bytes(len);
    let outs = [lane];
    let calls = [Call {
        ov: id,
        inputs: &inputs,
        outputs: &outs,
        site_bytes: bytes,
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
    let mut ctx = TestCtx::default();
    let mut out = Vec::new();
    for (i, &(hi, lo, c)) in bars.iter().enumerate() {
        let mut b = bar(i as u32, c, barflag::CONFIRMED);
        b.high_1e6 = scale::to_1e6(hi).unwrap();
        b.low_1e6 = scale::to_1e6(lo).unwrap();
        assert_eq!(h.step_bar(&b, &mut ctx), StepOutcome::Ok);
        out.push(h.out(0, 0));
    }
    out
}

fn hlc_tape(n: usize) -> Vec<(f64, f64, f64)> {
    tape(n)
        .iter()
        .enumerate()
        .map(|(i, &c)| (c + 0.5 + (i % 3) as f64, c - 0.25, c))
        .collect()
}

#[test]
fn ta_tr_and_atr_match_reference() {
    let b = hlc_tape(400);
    let tr_t = run_g(ov::TA_TR, &[In::Bool(true)], 0, &b);
    let tr_f = run_g(ov::TA_TR, &[In::Bool(false)], 0, &b);
    let tr_v = run_g(ov::TA_TR_VAR, &[], 0, &b);
    let mut want = Vec::new();
    for i in 0..b.len() {
        let (h, l, _) = b[i];
        let w = if i == 0 {
            h - l
        } else {
            let pc = b[i - 1].2;
            let m = |a: f64, b: f64| if a >= b { a } else { b };
            m(m(h - l, (h - pc).abs()), (l - pc).abs())
        };
        want.push(w);
    }
    assert_eq!(tr_t[0], want[0]);
    assert!(tr_f[0].is_nan() && tr_v[0].is_nan());
    for i in 1..b.len() {
        assert_eq!(tr_t[i], want[i]);
        assert_eq!(tr_f[i], want[i]);
        assert_eq!(tr_v[i], want[i]);
    }
    let rma_sma = pine_spec::sw(Sw::RmaSeed, Dialect::V6) == swr::RMA_SEED_SMA;
    let atr = run_g(ov::TA_ATR, &[In::Int(14)], 0, &b);
    let r = smooth_ref(&want, 14, 1.0 / 14.0, rma_sma);
    for i in 0..b.len() {
        assert!(
            (r[i].is_nan() && atr[i].is_nan()) || (r[i] - atr[i]).abs() < 1e-9,
            "atr {i}"
        );
    }
}

#[test]
fn ta_stdev_and_variance_match_bruteforce() {
    let c = tape(600);
    let b: Vec<(f64, f64, f64)> = c.iter().map(|&x| (x, x, x)).collect();
    for biased in [true, false] {
        let var = run_g(
            ov::TA_VARIANCE,
            &[In::Close, In::Int(20), In::Bool(biased)],
            20,
            &b,
        );
        let sd = run_g(
            ov::TA_STDEV,
            &[In::Close, In::Int(20), In::Bool(biased)],
            20,
            &b,
        );
        for i in 19..c.len() {
            let w = &c[i - 19..=i];
            let m = w.iter().sum::<f64>() / 20.0;
            let ss = w.iter().map(|x| (x - m) * (x - m)).sum::<f64>();
            let want = if biased { ss / 20.0 } else { ss / 19.0 };
            assert!((var[i] - want).abs() < 1e-6, "{i} {} {want}", var[i]);
            assert!((sd[i] - want.sqrt()).abs() < 1e-6);
        }
        assert!(var[18].is_nan());
    }
}

#[test]
fn ta_variance_is_clamped_at_the_cancellation_edge() {
    // A constant large series: Σx²/n − mean² cancels to a tiny negative in floating point.
    let b: Vec<(f64, f64, f64)> = (0..2100)
        .map(|_| (98_765.43, 98_765.43, 98_765.43))
        .collect();
    let var = run_g(
        ov::TA_VARIANCE,
        &[In::Close, In::Int(7), In::Bool(true)],
        7,
        &b,
    );
    let sd = run_g(
        ov::TA_STDEV,
        &[In::Close, In::Int(7), In::Bool(true)],
        7,
        &b,
    );
    for i in 6..b.len() {
        assert!(var[i] >= 0.0, "{i}: {}", var[i]);
        assert!(!sd[i].is_nan(), "{i}");
    }
}

#[test]
fn ta_crossover_with_na_is_false() {
    let c = tape(800);
    let b: Vec<(f64, f64, f64)> = c.iter().map(|&x| (x, x, x)).collect();
    let lvl = c[400];
    let over = run_o(ov::TA_CROSSOVER, &[In::Close, In::F(lvl)], 0, &b, true);
    let under = run_o(ov::TA_CROSSUNDER, &[In::Close, In::F(lvl)], 0, &b, true);
    let cross = run_o(ov::TA_CROSS, &[In::Close, In::F(lvl)], 0, &b, true);
    let (mut n_over, mut n_under) = (0, 0);
    for i in 0..c.len() {
        let wo = i > 0 && c[i] > lvl && c[i - 1] <= lvl;
        let wu = i > 0 && c[i] < lvl && c[i - 1] >= lvl;
        assert_eq!(over[i] == 1.0, wo, "over {i}");
        assert_eq!(under[i] == 1.0, wu, "under {i}");
        assert_eq!(cross[i] == 1.0, wo || wu, "cross {i}");
        n_over += wo as u32;
        n_under += wu as u32;
    }
    assert!(n_over > 0 && n_under > 0);
    for id in [ov::TA_CROSSOVER, ov::TA_CROSSUNDER, ov::TA_CROSS] {
        let na = run_o(id, &[In::Close, In::F(f64::NAN)], 0, &b, true);
        assert!(na.iter().all(|&x| x == 0.0), "{id:?}");
    }
}

#[test]
fn ta_change_and_mom_use_the_site_ring() {
    let c = tape(300);
    let b: Vec<(f64, f64, f64)> = c.iter().map(|&x| (x, x, x)).collect();
    for id in [ov::TA_CHANGE_F, ov::TA_MOM] {
        for k in [1u32, 9] {
            let got = run_g(id, &[In::Close, In::Int(k as i64)], k, &b);
            for i in 0..c.len() {
                if i < k as usize {
                    assert!(got[i].is_nan(), "{id:?} {k} {i}");
                } else {
                    assert_eq!(got[i], c[i] - c[i - k as usize], "{id:?} {k} {i}");
                }
            }
        }
    }
}

#[test]
fn ta_highest_lowest_equal_bruteforce() {
    let b = hlc_tape(3000);
    let c: Vec<f64> = b.iter().map(|x| x.2).collect();
    let hi: Vec<f64> = b
        .iter()
        .map(|x| scale::from_1e6(scale::to_1e6(x.0).unwrap()))
        .collect();
    for (len, cap) in [(1u32, 1u32), (7, 7), (7, 50), (20, 64)] {
        let ii = In::Int(len as i64);
        let hs = run_g(ov::TA_HIGHEST_SRC, &[In::Close, ii], cap, &b);
        let ls = run_g(ov::TA_LOWEST_SRC, &[In::Close, ii], cap, &b);
        let hb = run_g(ov::TA_HIGHESTBARS_SRC, &[In::Close, ii], cap, &b);
        let lb = run_g(ov::TA_LOWESTBARS_SRC, &[In::Close, ii], cap, &b);
        let hh = run_g(ov::TA_HIGHEST, &[ii], cap, &b);
        for i in 0..c.len() {
            if i + 1 < len as usize {
                assert!(hs[i].is_nan() && ls[i].is_nan() && hb[i].is_nan() && hh[i].is_nan());
                continue;
            }
            let w = &c[i + 1 - len as usize..=i];
            let (mut mx, mut mxk, mut mn, mut mnk) = (w[0], 0usize, w[0], 0usize);
            for (k, &v) in w.iter().enumerate() {
                if v >= mx {
                    (mx, mxk) = (v, k);
                }
                if v <= mn {
                    (mn, mnk) = (v, k);
                }
            }
            assert_eq!(hs[i], mx, "highest len {len} cap {cap} bar {i}");
            assert_eq!(ls[i], mn, "lowest len {len} cap {cap} bar {i}");
            assert_eq!(hb[i], -((len as usize - 1 - mxk) as f64), "highestbars {i}");
            assert_eq!(lb[i], -((len as usize - 1 - mnk) as f64), "lowestbars {i}");
            let wh = &hi[i + 1 - len as usize..=i];
            let m = wh
                .iter()
                .fold(f64::NEG_INFINITY, |a, &v| if v > a { v } else { a });
            assert_eq!(hh[i], m, "highest(len) {i}");
        }
    }
}
/// A 3-output row over `[close, ints…, floats…]` → `(o0, o1, o2)` per bar.
fn run3(id: OverloadId, ints: &[i64], fl: &[f64], len: u32, c: &[f64]) -> Vec<[f64; 3]> {
    let mut inputs = vec![CLOSE];
    inputs.extend(ints.iter().map(|&v| (Lane::I64, Src::Const(v as u64))));
    inputs.extend(fl.iter().map(|&v| (Lane::F64, Src::Const(v.to_bits()))));
    let bytes = OVERLOADS[id.0 as usize].state.block_bytes(len);
    let outs = [Lane::F64; 3];
    let calls = [Call {
        ov: id,
        inputs: &inputs,
        outputs: &outs,
        site_bytes: bytes,
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
    let mut ctx = TestCtx::default();
    let mut out = Vec::new();
    for (i, &x) in c.iter().enumerate() {
        assert_eq!(
            h.step_bar(&bar(i as u32, x, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        out.push([h.out(0, 0), h.out(0, 1), h.out(0, 2)]);
    }
    out
}

fn close_eq(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || (a - b).abs() < 1e-9
}

#[test]
fn ta_multi_output_rows_write_every_output() {
    let c = tape(500);
    let ema_sma = pine_spec::sw(Sw::EmaSeed, Dialect::V6) == swr::EMA_SEED_SMA;
    let m = run3(ov::TA_MACD, &[12, 26, 9], &[], 0, &c);
    let f = smooth_ref(&c, 12, 2.0 / 13.0, ema_sma);
    let s = smooth_ref(&c, 26, 2.0 / 27.0, ema_sma);
    let line: Vec<f64> = f.iter().zip(&s).map(|(a, b)| a - b).collect();
    let first = line.iter().position(|x| !x.is_nan()).unwrap();
    let sig_tail = smooth_ref(&line[first..], 9, 0.2, ema_sma);
    for i in 0..c.len() {
        assert!(
            m[i].iter().all(|&x| x != -12345.0),
            "macd output unwritten at {i}"
        );
        assert!(close_eq(m[i][0], line[i]), "macd {i}");
        let g = if i < first {
            f64::NAN
        } else {
            sig_tail[i - first]
        };
        assert!(close_eq(m[i][1], g), "signal {i}");
        assert!(close_eq(m[i][2], line[i] - g), "hist {i}");
    }
    let b = run3(ov::TA_BB, &[20], &[2.0], 20, &c);
    for i in 0..c.len() {
        assert!(
            b[i].iter().all(|&x| x != -12345.0),
            "bb output unwritten at {i}"
        );
        if i < 19 {
            assert!(b[i].iter().all(|x| x.is_nan()));
            continue;
        }
        let w = &c[i - 19..=i];
        let mid = w.iter().sum::<f64>() / 20.0;
        let sd = (w.iter().map(|x| (x - mid) * (x - mid)).sum::<f64>() / 20.0).sqrt();
        assert!((b[i][0] - mid).abs() < 1e-6 && (b[i][1] - (mid + 2.0 * sd)).abs() < 1e-6);
        assert!((b[i][2] - (mid - 2.0 * sd)).abs() < 1e-6, "bb {i}");
    }
}

#[test]
fn ta_rsi_matches_reference() {
    let c = tape(600);
    let rma_sma = pine_spec::sw(Sw::RmaSeed, Dialect::V6) == swr::RMA_SEED_SMA;
    let got = run(ov::TA_RSI, 14, &c, 0, None);
    let up: Vec<f64> = (1..c.len())
        .map(|i| {
            if c[i] > c[i - 1] {
                c[i] - c[i - 1]
            } else {
                0.0
            }
        })
        .collect();
    let dn: Vec<f64> = (1..c.len())
        .map(|i| {
            if c[i] < c[i - 1] {
                c[i - 1] - c[i]
            } else {
                0.0
            }
        })
        .collect();
    let (u, d) = (
        smooth_ref(&up, 14, 1.0 / 14.0, rma_sma),
        smooth_ref(&dn, 14, 1.0 / 14.0, rma_sma),
    );
    assert!(got[0].is_nan());
    for i in 1..c.len() {
        let (a, b) = (u[i - 1], d[i - 1]);
        let want = if a.is_nan() || b.is_nan() {
            f64::NAN
        } else if b == 0.0 {
            100.0
        } else if a == 0.0 {
            0.0
        } else {
            100.0 - 100.0 / (1.0 + a / b)
        };
        assert!(close_eq(got[i], want), "rsi {i}: {} vs {want}", got[i]);
    }
    let ticks = run(ov::TA_RSI, 14, &c, 2, None);
    assert!(bits_eq(&got, &ticks));
}

#[test]
fn math_sum_fixnan_random_and_roc() {
    let c = tape(200);
    let b: Vec<(f64, f64, f64)> = c.iter().map(|&x| (x, x, x)).collect();
    let sum = run_g(ov::MATH_SUM, &[In::Close, In::Int(5)], 5, &b);
    for i in 4..c.len() {
        assert!(
            (sum[i] - c[i - 4..=i].iter().sum::<f64>()).abs() < 1e-9,
            "sum {i}"
        );
    }
    assert!(sum[3].is_nan());
    let fx = run_g(ov::FIXNAN_F, &[In::Close], 0, &b);
    assert_eq!(fx, c);
    let fna = run_g(ov::FIXNAN_F, &[In::F(f64::NAN)], 0, &b);
    assert!(fna.iter().all(|x| x.is_nan()));
    let r1 = run_g(
        ov::MATH_RANDOM,
        &[In::F(-1.0), In::F(1.0), In::Int(42)],
        0,
        &b,
    );
    let r2 = run_g(
        ov::MATH_RANDOM,
        &[In::F(-1.0), In::F(1.0), In::Int(42)],
        0,
        &b,
    );
    assert!(bits_eq(&r1, &r2), "replayable");
    assert!(r1.iter().all(|&x| (-1.0..1.0).contains(&x)));
    assert!(r1.windows(2).any(|w| w[0] != w[1]));
    let roc = run_g(ov::TA_ROC, &[In::Close, In::Int(3)], 3, &b);
    for i in 3..c.len() {
        let want = 100.0 * (c[i] - c[i - 3]) / c[i - 3];
        assert!((roc[i] - want).abs() < 1e-9, "roc {i}");
    }
    assert!(roc[2].is_nan());
}

/// One call of `id` over HLC bars with explicit inputs, every output per bar; `ticks` forming-bar
/// re-evaluations before each confirm.
fn run_n(
    id: OverloadId,
    ins: &[(Lane, Src)],
    outs: &[Lane],
    bars: &[(f64, f64, f64)],
    ticks: u32,
) -> Vec<Vec<f64>> {
    let mode = if ticks > 0 {
        ExecMode::Realtime
    } else {
        ExecMode::BarClose
    };
    let bytes = OVERLOADS[id.0 as usize].state.block_bytes(0);
    let calls = [Call {
        ov: id,
        inputs: ins,
        outputs: outs,
        site_bytes: bytes,
        guard: None,
    }];
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx::default();
    let mut out = Vec::new();
    for (i, &(hi, lo, c)) in bars.iter().enumerate() {
        let mk = |x: f64, fl: u32| {
            let mut b = bar(i as u32, c * x, fl);
            b.high_1e6 = scale::to_1e6(hi * x).unwrap();
            b.low_1e6 = scale::to_1e6(lo * x).unwrap();
            b
        };
        for j in 0..ticks {
            assert_eq!(
                h.step_tick(&mk(1.1 + j as f64 * 0.1, barflag::REALTIME), &mut ctx),
                StepOutcome::Ok
            );
        }
        assert_eq!(
            h.step_bar(&mk(1.0, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        out.push((0..outs.len()).map(|j| h.out(0, j)).collect());
    }
    out
}

fn fx(b: &[(f64, f64, f64)]) -> Vec<(f64, f64, f64)> {
    let r = |x: f64| scale::from_1e6(scale::to_1e6(x).unwrap());
    b.iter().map(|&(h, l, c)| (r(h), r(l), r(c))).collect()
}

#[test]
fn kc_basis_switch_and_kcw() {
    let b = fx(&hlc_tape(300));
    let c: Vec<f64> = b.iter().map(|x| x.2).collect();
    let d = Dialect::V6;
    let ema_sma = pine_spec::sw(Sw::EmaSeed, d) == swr::EMA_SEED_SMA;
    let rma = pine_spec::sw(Sw::KcBasis, d) == swr::KC_BASIS_RMA_RANGE;
    let rma_sma = pine_spec::sw(Sw::RmaSeed, d) == swr::RMA_SEED_SMA;
    let mid = smooth_ref(&c, 10, 2.0 / 11.0, ema_sma);
    let span: Vec<f64> = b.iter().map(|x| x.0 - x.1).collect();
    let band = if rma {
        smooth_ref(&span, 10, 0.1, rma_sma)
    } else {
        smooth_ref(&span, 10, 2.0 / 11.0, ema_sma)
    };
    let ins = [
        CLOSE,
        (Lane::I64, Src::Const(10)),
        (Lane::F64, Src::Const(1.5f64.to_bits())),
        (Lane::I64, Src::Const(0)),
    ];
    let kc = run_n(ov::TA_KC, &ins, &[Lane::F64; 3], &b, 0);
    let kcw = run_n(ov::TA_KCW, &ins, &[Lane::F64], &b, 0);
    let ticks = run_n(ov::TA_KC, &ins, &[Lane::F64; 3], &b, 2);
    for i in 0..b.len() {
        let (m, u, l) = (
            mid[i],
            1.5f64.mul_add(band[i], mid[i]),
            (-1.5f64).mul_add(band[i], mid[i]),
        );
        assert!(
            close_eq(kc[i][0], m) && close_eq(kc[i][1], u) && close_eq(kc[i][2], l),
            "kc {i}"
        );
        assert!(close_eq(kcw[i][0], (u - l) / m), "kcw {i}");
        assert!(bits_eq(&kc[i], &ticks[i]), "I5 {i}");
    }
}

#[test]
fn ta_supertrend_is_stable_under_replay() {
    let b = fx(&hlc_tape(400));
    let ins = [
        (Lane::F64, Src::Const(3.0f64.to_bits())),
        (Lane::I64, Src::Const(10)),
    ];
    let a = run_n(ov::TA_SUPERTREND, &ins, &[Lane::F64, Lane::F64], &b, 0);
    let t = run_n(ov::TA_SUPERTREND, &ins, &[Lane::F64, Lane::F64], &b, 3);
    for i in 0..b.len() {
        assert!(bits_eq(&a[i], &t[i]), "replay {i}");
        assert!(a[i][1] == 1.0 || a[i][1] == -1.0, "dir {i}");
    }
    // A held direction keeps the close on its side of the line: −1 (lower band) needs
    // `close >= line`, 1 (upper band) needs `close <= line`.
    for i in 20..b.len() {
        if a[i - 1][1] == a[i][1] {
            if a[i][1] == -1.0 {
                assert!(b[i].2 >= a[i][0], "{i}");
            } else {
                assert!(b[i].2 <= a[i][0], "{i}");
            }
        }
    }
    assert!(a.iter().any(|x| x[1] == -1.0) && a.iter().any(|x| x[1] == 1.0));
}

#[test]
fn wma_and_swma_are_exact_weighted_sums() {
    let c = tape(2500);
    for len in [1usize, 4, 15] {
        let got = run(ov::TA_WMA, len as i64, &c, 0, None);
        let ticks = run(ov::TA_WMA, len as i64, &c, 2, None);
        assert!(bits_eq(&got, &ticks), "I5 wma {len}");
        let den = (len * (len + 1) / 2) as f64;
        for i in 0..c.len() {
            if i + 1 < len {
                assert!(got[i].is_nan());
                continue;
            }
            let want: f64 = (0..len).map(|k| c[i - k] * (len - k) as f64).sum::<f64>() / den;
            assert!((got[i] - want).abs() < 1e-9, "wma {len} {i}");
        }
    }
    let b: Vec<(f64, f64, f64)> = c.iter().map(|&x| (x, x, x)).collect();
    let sw = run_g(ov::TA_SWMA, &[In::Close], 0, &b);
    for i in 0..c.len() {
        if i < 3 {
            assert!(sw[i].is_nan());
            continue;
        }
        let want = (c[i] + c[i - 3]) / 6.0 + (c[i - 1] + c[i - 2]) * (2.0 / 6.0);
        assert!((sw[i] - want).abs() < 1e-9, "swma {i}");
    }
    let na = run_g(ov::TA_SWMA, &[In::F(f64::NAN)], 0, &b);
    assert!(na.iter().all(|x| x.is_nan()));
}

#[test]
fn linreg_and_bbw_match_bruteforce() {
    let c = tape(800);
    let b: Vec<(f64, f64, f64)> = c.iter().map(|&x| (x, x, x)).collect();
    for (len, off) in [(1usize, 0i64), (10, 0), (10, 2)] {
        let got = run_g(
            ov::TA_LINREG,
            &[In::Close, In::Int(len as i64), In::Int(off)],
            len as u32,
            &b,
        );
        for i in 0..c.len() {
            if i + 1 < len {
                assert!(got[i].is_nan());
                continue;
            }
            let want = if len == 1 {
                c[i]
            } else {
                let n = len as f64;
                let pts: Vec<(f64, f64)> =
                    (0..len).map(|k| ((len - 1 - k) as f64, c[i - k])).collect();
                let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
                let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
                let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
                let sxx: f64 = pts.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
                let slope = sxy / sxx;
                my - slope * mx + slope * (n - 1.0 - off as f64)
            };
            assert!(
                (got[i] - want).abs() < 1e-6,
                "linreg {len} {off} {i}: {} vs {want}",
                got[i]
            );
        }
    }
    let bb = run3(ov::TA_BB, &[20], &[2.0], 20, &c);
    let bbw = run_g(ov::TA_BBW, &[In::Close, In::Int(20), In::F(2.0)], 20, &b);
    for i in 0..c.len() {
        let want = (bb[i][1] - bb[i][2]) / bb[i][0];
        assert!(close_eq(bbw[i], want), "bbw {i}");
    }
}
