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
        ov::TA_HMA,
        ov::TA_VWMA,
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
        ov::TA_HMA,
        ov::TA_VWMA,
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
    run_od(id, ins, len, bars, out_i64, Dialect::V6)
}
/// As `run_o`, in a `d` program (a v5-only overload runs in v5: rule 22, amendment A8).
fn run_od(
    id: OverloadId,
    ins: &[In],
    len: u32,
    bars: &[(f64, f64, f64)],
    out_i64: bool,
    d: Dialect,
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
        dialect: d,
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
        // ta.highest(length) (implicit `high`) is v5-only in pine-spec: it runs in a v5 program.
        let hh = run_od(ov::TA_HIGHEST, &[ii], cap, &b, false, Dialect::V5);
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

/// One bar of the general runner: `(high, low, close, volume, feed column 0)`.
type Bx = (f64, f64, f64, f64, f64);

/// One call of `id` over full bars with explicit inputs (`Src::Feed(0)` reads the bar's feed
/// value), every output per bar; `len` sizes a window block; `ticks` forming-bar re-evaluations
/// (scaled prices, the same feed) before each confirm.
fn run_x(
    id: OverloadId,
    ins: &[(Lane, Src)],
    outs: &[Lane],
    len: u32,
    bars: &[Bx],
    ticks: u32,
) -> Vec<Vec<f64>> {
    let mode = if ticks > 0 {
        ExecMode::Realtime
    } else {
        ExecMode::BarClose
    };
    let bytes = OVERLOADS[id.0 as usize].state.block_bytes(len);
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
    for (i, &(hi, lo, c, v, fd)) in bars.iter().enumerate() {
        ctx.feed = vec![fd];
        let mk = |x: f64, fl: u32| {
            let mut b = bar(i as u32, c * x, fl);
            b.high_1e6 = scale::to_1e6(hi * x).unwrap();
            b.low_1e6 = scale::to_1e6(lo * x).unwrap();
            b.volume_1e6 = scale::to_1e6(v * x).unwrap();
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

/// Full bars from the HLC tape (1e-6 exact), volume cycling 1..7, the feed = the close.
fn bx_tape(n: usize) -> Vec<Bx> {
    fx(&hlc_tape(n))
        .iter()
        .enumerate()
        .map(|(i, &(h, l, c))| (h, l, c, 1.0 + (i % 7) as f64, c))
        .collect()
}

fn col(r: &[Vec<f64>], j: usize) -> Vec<f64> {
    r.iter().map(|x| x[j]).collect()
}

fn fconst(v: f64) -> (Lane, Src) {
    (Lane::F64, Src::Const(v.to_bits()))
}

fn iconst(v: i64) -> (Lane, Src) {
    (Lane::I64, Src::Const(v as u64))
}

const FEED: (Lane, Src) = (Lane::F64, Src::Feed(0));

/// The reference manual's `pine_sar`, transcribed line by line over the chart history (every
/// bar executes, so the site's history is the chart's).
fn pine_sar(b: &[Bx], start: f64, inc: f64, max: f64) -> Vec<f64> {
    let mx = |a: f64, b: f64| {
        if a.is_nan() || b.is_nan() {
            f64::NAN
        } else if a >= b {
            a
        } else {
            b
        }
    };
    let mn = |a: f64, b: f64| {
        if a.is_nan() || b.is_nan() {
            f64::NAN
        } else if a <= b {
            a
        } else {
            b
        }
    };
    let hist = |i: usize, k: usize, f: fn(&Bx) -> f64| if i >= k { f(&b[i - k]) } else { f64::NAN };
    let (mut result, mut max_min, mut acceleration) = (f64::NAN, f64::NAN, f64::NAN);
    let mut is_below = false;
    let mut out = Vec::new();
    for (bar_index, x) in b.iter().enumerate() {
        let (high, low, close) = (x.0, x.1, x.2);
        let mut is_first_trend_bar = false;
        if bar_index == 1 {
            if close > hist(bar_index, 1, |x| x.2) {
                is_below = true;
                max_min = high;
                result = hist(bar_index, 1, |x| x.1);
            } else {
                is_below = false;
                max_min = low;
                result = hist(bar_index, 1, |x| x.0);
            }
            is_first_trend_bar = true;
            acceleration = start;
        }
        result += acceleration * (max_min - result);
        if is_below {
            if result > low {
                is_first_trend_bar = true;
                is_below = false;
                result = mx(high, max_min);
                max_min = low;
                acceleration = start;
            }
        } else if result < high {
            is_first_trend_bar = true;
            is_below = true;
            result = mn(low, max_min);
            max_min = high;
            acceleration = start;
        }
        if !is_first_trend_bar {
            if is_below {
                if high > max_min {
                    max_min = high;
                    acceleration = mn(acceleration + inc, max);
                }
            } else if low < max_min {
                max_min = low;
                acceleration = mn(acceleration + inc, max);
            }
        }
        if is_below {
            result = mn(result, hist(bar_index, 1, |x| x.1));
            if bar_index > 1 {
                result = mn(result, hist(bar_index, 2, |x| x.1));
            }
        } else {
            result = mx(result, hist(bar_index, 1, |x| x.0));
            if bar_index > 1 {
                result = mx(result, hist(bar_index, 2, |x| x.0));
            }
        }
        out.push(result);
    }
    out
}

#[test]
fn ta_sar_is_the_manuals_pine_sar() {
    // pin:04-sar-literal: bit-equal to the transcribed reference, and I5 under ticks.
    let b = bx_tape(600);
    let ins = [fconst(0.02), fconst(0.02), fconst(0.2)];
    let got = col(&run_x(ov::TA_SAR, &ins, &[Lane::F64], 0, &b, 0), 0);
    let tk = col(&run_x(ov::TA_SAR, &ins, &[Lane::F64], 0, &b, 2), 0);
    let want = pine_sar(&b, 0.02, 0.02, 0.2);
    assert!(bits_eq(&got, &want), "sar vs pine_sar");
    assert!(bits_eq(&got, &tk), "I5 sar");
    assert!(got[0].is_nan() && !got[1].is_nan());
    // Both sides of the price occur (the recurrence flips).
    assert!((2..b.len()).any(|i| got[i] < b[i].1) && (2..b.len()).any(|i| got[i] > b[i].0));
}

fn wma_ref(x: &[f64], i: usize, len: usize) -> f64 {
    let den = (len * (len + 1) / 2) as f64;
    (0..len).map(|k| x[i - k] * (len - k) as f64).sum::<f64>() / den
}

#[test]
fn ta_hma_matches_bruteforce() {
    let b = bx_tape(700);
    let c: Vec<f64> = b.iter().map(|x| x.2).collect();
    for len in [2usize, 9, 16] {
        let got = col(
            &run_x(
                ov::TA_HMA,
                &[FEED, iconst(len as i64)],
                &[Lane::F64],
                len as u32,
                &b,
                0,
            ),
            0,
        );
        let half = len / 2;
        let sq = (len as f64).sqrt().floor() as usize;
        let diff: Vec<f64> = (0..c.len())
            .map(|i| {
                if i + 1 < len {
                    f64::NAN
                } else {
                    2.0 * wma_ref(&c, i, half) - wma_ref(&c, i, len)
                }
            })
            .collect();
        for (i, &g) in got.iter().enumerate() {
            if i + 2 < len + sq {
                assert!(g.is_nan(), "hma {len} warm-up {i}");
            } else {
                let want = wma_ref(&diff, i, sq);
                assert!((g - want).abs() < 1e-9, "hma {len} {i}: {g} vs {want}");
            }
        }
    }
    // len 1 has a half of 0: refused as ta.wma(src, 0) is (pin:04-hma-half).
    let calls = [Call {
        ov: ov::TA_HMA,
        inputs: &[CLOSE, (Lane::I64, Src::Const(1))],
        outputs: &[Lane::F64],
        site_bytes: OVERLOADS[ov::TA_HMA.0 as usize].state.block_bytes(1),
        guard: None,
    }];
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let o = h.step_bar(&bar(0, 100.0, barflag::CONFIRMED), &mut TestCtx::default());
    assert_ne!(o, StepOutcome::Ok);
    assert_eq!(h.last_fault().map(|f| f.0), Some(FaultKind::ScriptRefused));
}

#[test]
fn ta_vwma_is_the_pair_window() {
    let mut b = bx_tape(400);
    // An na source on a few bars (skip: the pair is not accepted), a zero-volume stretch.
    for i in [50usize, 51, 120] {
        b[i].4 = f64::NAN;
    }
    for x in b.iter_mut().skip(300).take(12) {
        x.3 = 0.0;
    }
    let len = 10usize;
    let got = col(
        &run_x(ov::TA_VWMA, &[FEED, iconst(10)], &[Lane::F64], 10, &b, 0),
        0,
    );
    let tk = col(
        &run_x(ov::TA_VWMA, &[FEED, iconst(10)], &[Lane::F64], 10, &b, 2),
        0,
    );
    assert!(bits_eq(&got, &tk), "I5 vwma");
    let acc: Vec<(f64, f64)> = b
        .iter()
        .filter(|x| !x.4.is_nan())
        .map(|x| (x.4, x.3))
        .collect();
    let mut k = 0usize;
    for i in 0..b.len() {
        if !b[i].4.is_nan() {
            k += 1;
        }
        if k < len {
            assert!(got[i].is_nan(), "warm-up {i}");
            continue;
        }
        let w = &acc[k - len..k];
        let sv: f64 = w.iter().map(|p| p.1).sum();
        if sv == 0.0 {
            assert!(got[i].is_nan(), "zero-volume window {i}");
            continue;
        }
        let want = w.iter().map(|p| p.0 * p.1).sum::<f64>() / sv;
        assert!(
            (got[i] - want).abs() < 1e-9,
            "vwma {i}: {} vs {want}",
            got[i]
        );
    }
    assert!(got[311].is_nan());
}

/// The manual's `pine_alma` at bar `i` (na while fewer than `len` values exist or any is na).
fn pine_alma(x: &[f64], i: usize, len: usize, offset: f64, sigma: f64, floor: bool) -> f64 {
    let m = offset * (len - 1) as f64;
    let m = if floor { m.floor() } else { m };
    let s = len as f64 / sigma;
    let (mut norm, mut sum) = (0.0f64, 0.0f64);
    for k in 0..len {
        let d = k as f64 - m;
        let weight = (-(d * d) / (2.0 * (s * s))).exp();
        norm += weight;
        let back = len - k - 1;
        let v = if i >= back { x[i - back] } else { f64::NAN };
        sum += v * weight;
    }
    sum / norm
}

/// I26: the fixed-weight windows recomputed from the values equal the emitted value bit for bit.
#[test]
fn fixed_weight_windows_are_recompute_exact() {
    let b = bx_tape(1500);
    let c: Vec<f64> = b.iter().map(|x| x.2).collect();
    // ta.alma: three parameter sets, the floor flag both ways, and an na hole (propagate).
    let mut bn = b.clone();
    bn[700].4 = f64::NAN;
    let cn: Vec<f64> = bn.iter().map(|x| x.4).collect();
    for (len, off, sig, fl) in [
        (9usize, 0.85, 6.0, false),
        (9, 0.85, 6.0, true),
        (20, 0.5, 3.0, false),
    ] {
        let ins = [
            FEED,
            iconst(len as i64),
            fconst(off),
            fconst(sig),
            (Lane::I64, Src::Const(fl as u64)),
        ];
        let got = col(
            &run_x(ov::TA_ALMA, &ins, &[Lane::F64], len as u32, &bn, 0),
            0,
        );
        let tk = col(
            &run_x(ov::TA_ALMA, &ins, &[Lane::F64], len as u32, &bn, 2),
            0,
        );
        assert!(bits_eq(&got, &tk), "I5 alma");
        for (i, &g) in got.iter().enumerate() {
            let want = pine_alma(&cn, i, len, off, sig, fl);
            assert!(
                (g.is_nan() && want.is_nan()) || g.to_bits() == want.to_bits(),
                "alma {len} {fl} {i}: {g} vs {want}"
            );
        }
        assert!(got[700].is_nan() && got[700 + len - 1].is_nan() && !got[700 + len].is_nan());
    }
    // ta.wma: newest-first fused dot with weights len − k.
    for len in [1usize, 7, 30] {
        let got = col(
            &run_x(
                ov::TA_WMA,
                &[FEED, iconst(len as i64)],
                &[Lane::F64],
                len as u32,
                &b,
                0,
            ),
            0,
        );
        for i in len - 1..c.len() {
            let mut acc = 0.0f64;
            for k in 0..len {
                acc = c[i - k].mul_add((len - k) as f64, acc);
            }
            let want = acc / (len as f64 * (len as f64 + 1.0) / 2.0);
            assert_eq!(got[i].to_bits(), want.to_bits(), "wma {len} {i}");
        }
    }
    // ta.cog: `−num / sum`, both from one newest-first pass (pine_cog, unfused).
    for len in [1usize, 10] {
        let got = col(
            &run_x(
                ov::TA_COG,
                &[FEED, iconst(len as i64)],
                &[Lane::F64],
                len as u32,
                &b,
                0,
            ),
            0,
        );
        for i in len - 1..c.len() {
            let (mut num, mut sum) = (0.0f64, 0.0f64);
            for k in 0..len {
                num += c[i - k] * (k + 1) as f64;
                sum += c[i - k];
            }
            assert_eq!(got[i].to_bits(), (-num / sum).to_bits(), "cog {len} {i}");
        }
    }
    // ta.swma.
    let got = col(&run_x(ov::TA_SWMA, &[FEED], &[Lane::F64], 0, &b, 0), 0);
    for i in 3..c.len() {
        let want = (c[i] + c[i - 3]) / 6.0 + (c[i - 1] + c[i - 2]) * (2.0 / 6.0);
        assert_eq!(got[i].to_bits(), want.to_bits(), "swma {i}");
    }
    // ta.linreg: Σxy fused newest first against the cached x column, the closed forms.
    for (len, off) in [(10usize, 0i64), (25, 3)] {
        let ins = [FEED, iconst(len as i64), iconst(off)];
        let got = col(
            &run_x(ov::TA_LINREG, &ins, &[Lane::F64], len as u32, &b, 0),
            0,
        );
        for i in 2 * len..c.len() {
            let n = len as f64;
            let sx = n * (n - 1.0) / 2.0;
            let sxx = (n - 1.0) * n * (2.0 * n - 1.0) / 6.0;
            let mut sxy = 0.0f64;
            for k in 0..len {
                sxy = c[i - k].mul_add((len - 1 - k) as f64, sxy);
            }
            // The rolling Σy is the body's own (I27 bounds it); recompute it exactly and require
            // agreement to a few ulps, the dot product bit for bit through `sxy`.
            let sy: f64 = (0..len).map(|k| c[i - k]).sum();
            let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
            let icept = (sy - slope * sx) / n;
            let want = slope.mul_add(n - 1.0 - off as f64, icept);
            assert!(
                (got[i] - want).abs() <= want.abs() * 1e-12,
                "linreg {len} {i}"
            );
        }
    }
}

fn rel_eq(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || (a - b).abs() <= 1e-9 * (1.0 + b.abs())
}

/// The output of `id` over `bars`, and the same run with ticks (I5) — asserted bit-equal.
fn run_i5(id: OverloadId, ins: &[(Lane, Src)], outs: &[Lane], len: u32, b: &[Bx]) -> Vec<Vec<f64>> {
    let a = run_x(id, ins, outs, len, b, 0);
    let t = run_x(id, ins, outs, len, b, 2);
    for i in 0..a.len() {
        assert!(bits_eq(&a[i], &t[i]), "I5 {id:?} bar {i}");
    }
    a
}

const HIGH: (Lane, Src) = (Lane::F64, Src::Reserved(Lane::F64, abi::F_HIGH));
const LOW: (Lane, Src) = (Lane::F64, Src::Reserved(Lane::F64, abi::F_LOW));

/// T23 — the windowed oscillators against brute force over the chart history.
#[test]
fn oscillator_windows_match_reference() {
    let b = bx_tape(600);
    let c: Vec<f64> = b.iter().map(|x| x.2).collect();
    let len = 14usize;
    let l = len as u32;
    let dev = col(
        &run_i5(ov::TA_DEV, &[FEED, iconst(14)], &[Lane::F64], l, &b),
        0,
    );
    let cci = col(
        &run_i5(ov::TA_CCI, &[FEED, iconst(14)], &[Lane::F64], l, &b),
        0,
    );
    let stoch = col(
        &run_i5(
            ov::TA_STOCH,
            &[FEED, HIGH, LOW, iconst(14)],
            &[Lane::F64],
            l,
            &b,
        ),
        0,
    );
    let wpr = col(&run_i5(ov::TA_WPR, &[iconst(14)], &[Lane::F64], l, &b), 0);
    let cmo = col(
        &run_i5(ov::TA_CMO, &[FEED, iconst(14)], &[Lane::F64], l, &b),
        0,
    );
    let mfi = col(
        &run_i5(ov::TA_MFI, &[FEED, iconst(14)], &[Lane::F64], l, &b),
        0,
    );
    for i in 0..c.len() {
        if i + 1 < len {
            assert!(dev[i].is_nan() && cci[i].is_nan() && stoch[i].is_nan() && wpr[i].is_nan());
        } else {
            let w = &c[i + 1 - len..=i];
            let mean = w.iter().sum::<f64>() / len as f64;
            let d = w.iter().map(|x| (x - mean).abs()).sum::<f64>() / len as f64;
            assert!(rel_eq(dev[i], d), "dev {i}");
            assert!(rel_eq(cci[i], (c[i] - mean) / (0.015 * d)), "cci {i}");
            let bw = &b[i + 1 - len..=i];
            let hh = bw
                .iter()
                .map(|x| x.0)
                .fold(f64::MIN, |a, x| if x > a { x } else { a });
            let ll = bw
                .iter()
                .map(|x| x.1)
                .fold(f64::MAX, |a, x| if x < a { x } else { a });
            assert!(
                rel_eq(stoch[i], 100.0 * (c[i] - ll) / (hh - ll)),
                "stoch {i}"
            );
            assert!(rel_eq(wpr[i], -100.0 * (hh - c[i]) / (hh - ll)), "wpr {i}");
        }
        // W(len + 1): the first execution only primes `ta.change` (pin:04-first-change).
        if i < len {
            assert!(cmo[i].is_nan() && mfi[i].is_nan(), "warm-up {i}");
            continue;
        }
        let (mut s1, mut s2, mut up, mut lo) = (0.0, 0.0, 0.0, 0.0);
        for j in i + 1 - len..=i {
            let m = c[j] - c[j - 1];
            if m >= 0.0 {
                s1 += m;
            } else {
                s2 -= m;
            }
            up += b[j].3 * if m <= 0.0 { 0.0 } else { c[j] };
            lo += b[j].3 * if m >= 0.0 { 0.0 } else { c[j] };
        }
        assert!(rel_eq(cmo[i], 100.0 * (s1 - s2) / (s1 + s2)), "cmo {i}");
        let want = 100.0 - 100.0 / (1.0 + up / lo);
        assert!(
            rel_eq(mfi[i], want) || (lo == 0.0 && mfi[i].is_nan()),
            "mfi {i}"
        );
    }
    // A flat window divides by zero per Sw::FloatDivZero (`na` in both dialects today).
    let flat: Vec<Bx> = (0..30).map(|_| (100.0, 100.0, 100.0, 1.0, 100.0)).collect();
    let na0 = pine_spec::sw(Sw::FloatDivZero, Dialect::V6) == swr::FLOAT_DIV_ZERO_NA;
    let st = col(
        &run_x(
            ov::TA_STOCH,
            &[FEED, HIGH, LOW, iconst(5)],
            &[Lane::F64],
            5,
            &flat,
            0,
        ),
        0,
    );
    let cc = col(
        &run_x(ov::TA_CCI, &[FEED, iconst(5)], &[Lane::F64], 5, &flat, 0),
        0,
    );
    assert!(st[10].is_nan() && cc[10].is_nan(), "0/0 is na either way");
    // A rising tape: `lower` is 0, so `upper / lower` is +inf — na under `na`, 100 under `ieee`.
    let rise: Vec<Bx> = (0..30)
        .map(|i| {
            let x = 100.0 + i as f64;
            (x, x, x, 1.0, x)
        })
        .collect();
    let mf = col(
        &run_x(ov::TA_MFI, &[FEED, iconst(5)], &[Lane::F64], 5, &rise, 0),
        0,
    );
    assert!(
        if na0 {
            mf[10].is_nan()
        } else {
            mf[10] == 100.0
        },
        "mfi ÷0 per U15"
    );
}

/// A recursive smoother over a series with na holes (skip: hold), seeded per `sma_seed`.
fn sm_ref(xs: &[f64], len: usize, alpha: f64, sma_seed: bool) -> Vec<f64> {
    let (mut v, mut acc, mut n) = (f64::NAN, 0.0f64, 0usize);
    let mut out = Vec::new();
    for &x in xs {
        if !x.is_nan() {
            n += 1;
            if !v.is_nan() {
                v = alpha.mul_add(x - v, v);
            } else if !sma_seed {
                v = x;
            } else {
                acc += x;
                if n >= len {
                    v = acc / len as f64;
                }
            }
        }
        out.push(v);
    }
    out
}

/// T23 — the recursive oscillators (`ta.tsi`, `ta.dmi`) against compositions of the smoother.
#[test]
fn oscillator_smoothers_match_reference() {
    let b = bx_tape(700);
    let c: Vec<f64> = b.iter().map(|x| x.2).collect();
    let d = Dialect::V6;
    let ema_sma = pine_spec::sw(Sw::EmaSeed, d) == swr::EMA_SEED_SMA;
    let rma_sma = pine_spec::sw(Sw::RmaSeed, d) == swr::RMA_SEED_SMA;
    // tsi(13, 25): ema(ema(m, long), short) / ema(ema(|m|, long), short) (pin:04-tsi-order).
    let tsi = col(
        &run_i5(
            ov::TA_TSI,
            &[FEED, iconst(13), iconst(25)],
            &[Lane::F64],
            0,
            &b,
        ),
        0,
    );
    let m: Vec<f64> = (0..c.len())
        .map(|i| if i == 0 { f64::NAN } else { c[i] - c[i - 1] })
        .collect();
    let am: Vec<f64> = m.iter().map(|x| x.abs()).collect();
    let (al, as_) = (2.0 / 26.0, 2.0 / 14.0);
    let num = sm_ref(&sm_ref(&m, 25, al, ema_sma), 13, as_, ema_sma);
    let den = sm_ref(&sm_ref(&am, 25, al, ema_sma), 13, as_, ema_sma);
    for i in 0..c.len() {
        let want = num[i] / den[i];
        assert!(
            (tsi[i].is_nan() && want.is_nan()) || tsi[i].to_bits() == want.to_bits(),
            "tsi {i}"
        );
        assert!(tsi[i].is_nan() || tsi[i].abs() <= 1.0, "tsi range {i}");
    }
    assert!(!tsi[100].is_nan());
    // dmi(14, 10): three outputs, every one written.
    let r = run_i5(
        ov::TA_DMI,
        &[iconst(14), iconst(10)],
        &[Lane::F64; 3],
        0,
        &b,
    );
    let n = b.len();
    let mut tr = vec![f64::NAN; n];
    let mut pdm = vec![f64::NAN; n];
    let mut mdm = vec![f64::NAN; n];
    for i in 1..n {
        let (h, l, _, _, _) = b[i];
        let (h1, l1, c1, _, _) = b[i - 1];
        let (up, down) = (h - h1, l1 - l);
        pdm[i] = if up > down && up > 0.0 { up } else { 0.0 };
        mdm[i] = if down > up && down > 0.0 { down } else { 0.0 };
        let mx = |a: f64, b: f64| if a >= b { a } else { b };
        tr[i] = mx(mx(h - l, (h - c1).abs()), (l - c1).abs());
    }
    let t = sm_ref(&tr, 14, 1.0 / 14.0, rma_sma);
    let p = sm_ref(&pdm, 14, 1.0 / 14.0, rma_sma);
    let q = sm_ref(&mdm, 14, 1.0 / 14.0, rma_sma);
    let pdi: Vec<f64> = (0..n).map(|i| 100.0 * p[i] / t[i]).collect();
    let mdi: Vec<f64> = (0..n).map(|i| 100.0 * q[i] / t[i]).collect();
    let dx: Vec<f64> = (0..n)
        .map(|i| 100.0 * (pdi[i] - mdi[i]).abs() / (pdi[i] + mdi[i]))
        .collect();
    let adx = sm_ref(&dx, 10, 0.1, rma_sma);
    for i in 0..n {
        assert!(
            bits_eq(&r[i], &[pdi[i], mdi[i], adx[i]]),
            "dmi {i}: {:?}",
            r[i]
        );
    }
    assert!(!r[100][2].is_nan() && r[100][0] > 0.0 && r[100][1] > 0.0);
}

/// One call of `id` over full bars with per-bar feed columns, optionally guarded by feed column
/// `guard` (a conditional site, I6); every output per bar.
fn run_feed(
    id: OverloadId,
    ins: &[(Lane, Src)],
    outs: &[Lane],
    len: u32,
    bars: &[Bx],
    feed: &[Vec<f64>],
    guard: Option<u16>,
) -> Vec<Vec<f64>> {
    let calls = [Call {
        ov: id,
        inputs: ins,
        outputs: outs,
        site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(len),
        guard,
    }];
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx::default();
    let mut out = Vec::new();
    for (i, &(hi, lo, c, v, _)) in bars.iter().enumerate() {
        ctx.feed = feed[i].clone();
        let mut b = bar(i as u32, c, barflag::CONFIRMED);
        b.high_1e6 = scale::to_1e6(hi).unwrap();
        b.low_1e6 = scale::to_1e6(lo).unwrap();
        b.volume_1e6 = scale::to_1e6(v).unwrap();
        assert_eq!(h.step_bar(&b, &mut ctx), StepOutcome::Ok);
        out.push((0..outs.len()).map(|j| h.out(0, j)).collect());
    }
    out
}

fn fcol(c: u16, lane: Lane) -> (Lane, Src) {
    (lane, Src::Feed(c))
}

/// T24 — `ta.cum`, `ta.barssince`, `ta.valuewhen` (all four source types) against references.
#[test]
fn ta_cum_barssince_valuewhen_match_reference() {
    let b = bx_tape(300);
    // Feed: [source (na every 7th), condition (false for the first 5 bars), int source].
    let feed: Vec<Vec<f64>> = (0..b.len())
        .map(|i| {
            let src = if i % 7 == 3 { f64::NAN } else { b[i].2 };
            let cond = if i >= 5 && (i * 7919) % 5 < 2 {
                1.0
            } else {
                0.0
            };
            vec![src, cond, (i as f64 * 3.0).floor()]
        })
        .collect();
    let cum = col(
        &run_feed(
            ov::TA_CUM,
            &[fcol(0, Lane::F64)],
            &[Lane::F64],
            0,
            &b,
            &feed,
            None,
        ),
        0,
    );
    let bs = col(
        &run_feed(
            ov::TA_BARSSINCE,
            &[fcol(1, Lane::I64)],
            &[Lane::I64],
            0,
            &b,
            &feed,
            None,
        ),
        0,
    );
    let occ = 1i64;
    let vw = |id: OverloadId, src: (Lane, Src), out: Lane| {
        col(
            &run_feed(
                id,
                &[fcol(1, Lane::I64), src, iconst(occ)],
                &[out],
                occ as u32,
                &b,
                &feed,
                None,
            ),
            0,
        )
    };
    let vf = vw(ov::TA_VALUEWHEN_F, fcol(0, Lane::F64), Lane::F64);
    let vi = vw(ov::TA_VALUEWHEN_I, fcol(2, Lane::I64), Lane::I64);
    let vc = vw(ov::TA_VALUEWHEN_C, fcol(2, Lane::I64), Lane::I64);
    let vb = vw(ov::TA_VALUEWHEN_B, fcol(1, Lane::I64), Lane::I64);
    let (mut acc, mut since, mut trues) = (0.0f64, None::<i64>, Vec::new());
    for i in 0..b.len() {
        let (src, cond, isrc) = (feed[i][0], feed[i][1] != 0.0, feed[i][2]);
        if !src.is_nan() {
            acc += src;
        }
        assert_eq!(cum[i].to_bits(), acc.to_bits(), "cum {i}");
        since = if cond { Some(0) } else { since.map(|s| s + 1) };
        assert!(
            since.map_or(bs[i].is_nan(), |s| bs[i] == s as f64),
            "barssince {i}: {} vs {since:?}",
            bs[i]
        );
        if cond {
            trues.push((src, isrc));
        }
        let want = trues.len().checked_sub(1 + occ as usize).map(|k| trues[k]);
        match want {
            None => assert!(vf[i].is_nan() && vi[i].is_nan() && vc[i].is_nan() && vb[i] == 0.0),
            Some((s, n)) => {
                assert!(
                    (s.is_nan() && vf[i].is_nan()) || vf[i] == s,
                    "valuewhen f {i}"
                );
                assert!(vi[i] == n && vc[i] == n, "valuewhen i/c {i}");
                assert_eq!(vb[i], 1.0, "valuewhen b {i}: the condition was true there");
            }
        }
    }
    assert!(bs[3].is_nan() && trues.len() > 20);
}

/// U03 — the dialect's `Sw::RisingFalling` reading against brute force, on a sequence that tells
/// the two readings apart (`[1, 3, 2, 4]`, len 3: `gt_all_prior` true, `monotone` false).
#[test]
fn rising_falling_switch() {
    let d = Dialect::V6;
    let mono = pine_spec::sw(Sw::RisingFalling, d) == swr::RISING_FALLING_MONOTONE;
    let b = bx_tape(400);
    let c: Vec<f64> = b.iter().map(|x| x.2).collect();
    let len = 3usize;
    let feed: Vec<Vec<f64>> = c.iter().map(|&x| vec![x]).collect();
    let ins = [fcol(0, Lane::F64), iconst(len as i64)];
    let up = col(
        &run_feed(
            ov::TA_RISING,
            &ins,
            &[Lane::I64],
            len as u32,
            &b,
            &feed,
            None,
        ),
        0,
    );
    let dn = col(
        &run_feed(
            ov::TA_FALLING,
            &ins,
            &[Lane::I64],
            len as u32,
            &b,
            &feed,
            None,
        ),
        0,
    );
    for i in 0..c.len() {
        let (mut r, mut f) = (i >= len, i >= len);
        for k in 1..=len.min(i) {
            let (a, p) = if mono {
                (c[i - k + 1], c[i - k])
            } else {
                (c[i], c[i - k])
            };
            r &= a > p;
            f &= a < p;
        }
        assert_eq!(up[i] == 1.0, r, "rising {i}");
        assert_eq!(dn[i] == 1.0, f, "falling {i}");
    }
    assert!(up.contains(&1.0) && dn.contains(&1.0));
    let seq = [1.0, 3.0, 2.0, 4.0];
    let b4: Vec<Bx> = seq.iter().map(|&x| (x, x, x, 1.0, x)).collect();
    let f4: Vec<Vec<f64>> = seq.iter().map(|&x| vec![x]).collect();
    let r4 = col(
        &run_feed(ov::TA_RISING, &ins, &[Lane::I64], 3, &b4, &f4, None),
        0,
    );
    assert_eq!(r4[3] == 1.0, !mono, "the readings differ on [1, 3, 2, 4]");
}

/// `ta.pivothigh`/`ta.pivotlow`: the block holds `left + right + 1` values (one fewer refuses);
/// a pivot is reported `right` executions late, strictly beyond every neighbour.
#[test]
fn pivot_window_is_left_plus_right_plus_one() {
    let b = bx_tape(500);
    let (l, r) = (3usize, 2usize);
    let w = l + r + 1;
    let spec = &OVERLOADS[ov::TA_PIVOTHIGH.0 as usize].state;
    assert_eq!(spec.block_bytes((l + r) as u32), 16 + 8 * w as u32);
    let feed: Vec<Vec<f64>> = b.iter().map(|x| vec![x.2]).collect();
    let lr = [iconst(l as i64), iconst(r as i64)];
    let src_lr = [fcol(0, Lane::F64), iconst(l as i64), iconst(r as i64)];
    let n = (l + r) as u32;
    let ph = col(
        &run_feed(ov::TA_PIVOTHIGH, &lr, &[Lane::F64], n, &b, &feed, None),
        0,
    );
    let pl = col(
        &run_feed(ov::TA_PIVOTLOW, &lr, &[Lane::F64], n, &b, &feed, None),
        0,
    );
    let phs = col(
        &run_feed(
            ov::TA_PIVOTHIGH_SRC,
            &src_lr,
            &[Lane::F64],
            n,
            &b,
            &feed,
            None,
        ),
        0,
    );
    let pls = col(
        &run_feed(
            ov::TA_PIVOTLOW_SRC,
            &src_lr,
            &[Lane::F64],
            n,
            &b,
            &feed,
            None,
        ),
        0,
    );
    let piv = |xs: &[f64], i: usize, hi: bool| -> f64 {
        if i + 1 < w {
            return f64::NAN;
        }
        let ctr = xs[i - r];
        let ok =
            (i + 1 - w..=i)
                .filter(|&j| j != i - r)
                .all(|j| if hi { ctr > xs[j] } else { ctr < xs[j] });
        if ok {
            ctr
        } else {
            f64::NAN
        }
    };
    let (hs, ls, cs): (Vec<f64>, Vec<f64>, Vec<f64>) = (
        b.iter().map(|x| x.0).collect(),
        b.iter().map(|x| x.1).collect(),
        b.iter().map(|x| x.2).collect(),
    );
    let mut found = 0;
    for i in 0..b.len() {
        assert!(bits_eq(&[ph[i]], &[piv(&hs, i, true)]), "pivothigh {i}");
        assert!(bits_eq(&[pl[i]], &[piv(&ls, i, false)]), "pivotlow {i}");
        assert!(
            bits_eq(&[phs[i]], &[piv(&cs, i, true)]),
            "pivothigh(src) {i}"
        );
        assert!(
            bits_eq(&[pls[i]], &[piv(&cs, i, false)]),
            "pivotlow(src) {i}"
        );
        found += !ph[i].is_nan() as usize;
    }
    assert!(found > 5, "the tape has pivots");
    // One value short of left + right + 1: refused.
    let calls = [Call {
        ov: ov::TA_PIVOTHIGH,
        inputs: &lr,
        outputs: &[Lane::F64],
        site_bytes: spec.block_bytes(n - 1),
        guard: None,
    }];
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let o = h.step_bar(&bar(0, 100.0, barflag::CONFIRMED), &mut TestCtx::default());
    assert_ne!(o, StepOutcome::Ok);
    assert_eq!(h.last_fault().map(|f| f.0), Some(FaultKind::ScriptRefused));
}

/// I6 — a stateful site skipped by a false branch produces, on its next execution, exactly what
/// it would have without the skipped bars.
#[test]
fn conditional_site_does_not_advance() {
    let b = bx_tape(400);
    let run_on = |i: usize| (i * 2654435761usize) % 3 != 0;
    let feed: Vec<Vec<f64>> = b
        .iter()
        .enumerate()
        .map(|(i, x)| vec![x.2, if run_on(i) { 1.0 } else { 0.0 }])
        .collect();
    let kept: Vec<usize> = (0..b.len()).filter(|&i| run_on(i)).collect();
    let bk: Vec<Bx> = kept.iter().map(|&i| b[i]).collect();
    let fk: Vec<Vec<f64>> = kept.iter().map(|&i| feed[i].clone()).collect();
    let src = fcol(0, Lane::F64);
    let rows: [(OverloadId, Vec<(Lane, Src)>, Lane, u32); 12] = [
        (ov::TA_SMA, vec![src, iconst(9)], Lane::F64, 9),
        (ov::TA_EMA, vec![src, iconst(9)], Lane::F64, 0),
        (ov::TA_HMA, vec![src, iconst(9)], Lane::F64, 9),
        (ov::TA_VWMA, vec![src, iconst(9)], Lane::F64, 9),
        (
            ov::TA_ALMA,
            vec![src, iconst(9), fconst(0.85), fconst(6.0), iconst(0)],
            Lane::F64,
            9,
        ),
        (ov::TA_CMO, vec![src, iconst(9)], Lane::F64, 9),
        (ov::TA_TSI, vec![src, iconst(5), iconst(9)], Lane::F64, 0),
        (
            ov::TA_SAR,
            vec![fconst(0.02), fconst(0.02), fconst(0.2)],
            Lane::F64,
            0,
        ),
        (ov::TA_WPR, vec![iconst(9)], Lane::F64, 9),
        (ov::TA_RISING, vec![src, iconst(3)], Lane::I64, 3),
        (ov::TA_PIVOTHIGH, vec![iconst(2), iconst(2)], Lane::F64, 4),
        (ov::TA_CUM, vec![src], Lane::F64, 0),
    ];
    for (id, ins, lane, len) in rows.iter() {
        let all = col(&run_feed(*id, ins, &[*lane], *len, &b, &feed, Some(1)), 0);
        let sub = col(&run_feed(*id, ins, &[*lane], *len, &bk, &fk, None), 0);
        for (j, &i) in kept.iter().enumerate() {
            assert!(bits_eq(&[all[i]], &[sub[j]]), "{id:?} bar {i}");
        }
    }
}

/// `ta.vwap(source, anchor[, stdev_mult])` (§4.13.8): `(src, anchor)` per bar from feed columns
/// 0 and 1, volume from the bar; every output per bar; `ticks` forming-bar re-evaluations.
fn run_vwap(bands: bool, b: &[(f64, f64, bool)], ticks: u32) -> Vec<Vec<f64>> {
    let id = if bands {
        ov::TA_VWAP_FFF
    } else {
        ov::TA_VWAP_F
    };
    let mut ins = vec![(Lane::F64, Src::Feed(0)), (Lane::I64, Src::Feed(1))];
    if bands {
        ins.push(fconst(2.0));
    }
    let outs: &[Lane] = if bands { &[Lane::F64; 3] } else { &[Lane::F64] };
    let calls = [Call {
        ov: id,
        inputs: &ins,
        outputs: outs,
        site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(0),
        guard: None,
    }];
    let mode = if ticks > 0 {
        ExecMode::Realtime
    } else {
        ExecMode::BarClose
    };
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx::default();
    let mut out = Vec::new();
    for (i, &(src, vol, anchor)) in b.iter().enumerate() {
        ctx.feed = vec![src, if anchor { 1.0 } else { 0.0 }];
        let mk = |x: f64, fl: u32| {
            let mut bb = bar(i as u32, 100.0, fl);
            bb.volume_1e6 = scale::to_1e6(vol * x).unwrap();
            bb
        };
        for j in 0..ticks {
            let t = mk(1.5 + j as f64, barflag::REALTIME);
            assert_eq!(h.step_tick(&t, &mut ctx), StepOutcome::Ok);
        }
        let c = mk(1.0, barflag::CONFIRMED);
        assert_eq!(h.step_bar(&c, &mut ctx), StepOutcome::Ok);
        out.push((0..outs.len()).map(|j| h.out(0, j)).collect());
    }
    out
}

/// `ta.vwap` resets on the anchor, is na before the first true anchor (the reference's remark),
/// skips an na source, is na while `Σ volume == 0`, and its bands are `vwap ± mult · sd` with
/// `sd² = Σ v·p² / Σ v − vwap²`; forming ticks never advance it (I5).
#[test]
fn ta_vwap_function_resets_on_the_anchor() {
    let b: Vec<(f64, f64, bool)> = (0..300)
        .map(|i| {
            let src = if i == 40 || i == 41 {
                f64::NAN
            } else {
                100.0 + ((i * 37) % 23) as f64 * 0.25
            };
            let vol = if (200..205).contains(&i) {
                0.0
            } else {
                1.0 + (i % 5) as f64
            };
            (src, vol, i == 7 || i % 50 == 0 && i > 0 || i == 200)
        })
        .collect();
    let one = col(&run_vwap(false, &b, 0), 0);
    let three = run_vwap(true, &b, 0);
    let tk = run_vwap(true, &b, 2);
    let (mut sv, mut spv, mut sp2v, mut started) = (0.0f64, 0.0f64, 0.0f64, false);
    for (i, &(p, v, a)) in b.iter().enumerate() {
        if a {
            (sv, spv, sp2v, started) = (0.0, 0.0, 0.0, true);
        }
        if started && !p.is_nan() {
            sv += v;
            spv += v * p;
            sp2v += v * p * p;
        }
        let (m, sd) = if !started || sv == 0.0 {
            (f64::NAN, f64::NAN)
        } else {
            let m = spv / sv;
            let var = sp2v / sv - m * m;
            (m, if var > 0.0 { var.sqrt() } else { 0.0 })
        };
        assert!(close_eq(one[i], m), "vwap {i}: {} vs {m}", one[i]);
        assert!(close_eq(three[i][0], m), "vwap3 {i}");
        assert!(close_eq(three[i][1], m + 2.0 * sd), "upper {i}");
        assert!(close_eq(three[i][2], m - 2.0 * sd), "lower {i}");
        assert!(
            three[i]
                .iter()
                .zip(&tk[i])
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "I5 vwap {i}"
        );
    }
    assert!(
        one[..7].iter().all(|x| x.is_nan()),
        "na before the first anchor"
    );
    assert!(one[200..205].iter().all(|x| x.is_nan()), "Σ volume 0 is na");
    assert!(!one[205].is_nan());
}

/// The bars of the cumulative-variable tests: 6-hour bars (a UTC day every 4), varied
/// high/low/close and volume.
fn cum_bars(n: u32) -> Vec<Bar> {
    (0..n)
        .map(|i| {
            let c = 100.0 + ((i * 37) % 23) as f64 - 11.0;
            let mut b = bar(i, c, barflag::CONFIRMED);
            b.high_1e6 = scale::to_1e6(c + 1.0 + (i % 3) as f64).unwrap();
            b.low_1e6 = scale::to_1e6(c - 2.0 + (i % 2) as f64).unwrap();
            b.volume_1e6 = scale::to_1e6(1.0 + ((i * 13) % 7) as f64).unwrap();
            b.open_ms = i as i64 * 21_600_000;
            b.close_ms = b.open_ms + 21_600_000;
            b
        })
        .collect()
}

/// The `ta.vwap` variable's reference: Σ v·hlc3 / Σ v over the bar's UTC day so far.
fn vwap_var_reference(bars: &[Bar]) -> Vec<f64> {
    let (mut sv, mut spv, mut day) = (0.0f64, 0.0f64, i64::MIN);
    bars.iter()
        .map(|b| {
            let d = b.open_ms.div_euclid(86_400_000);
            if d != day {
                (sv, spv, day) = (0.0, 0.0, d);
            }
            let p = |x: i64| x as f64 / 1e6;
            let src = (p(b.high_1e6) + p(b.low_1e6) + p(b.close_1e6)) / 3.0;
            sv += p(b.volume_1e6);
            spv += p(b.volume_1e6) * src;
            spv / sv
        })
        .collect()
}

/// impl-04 T26 (§4.13.8, §1.4 item 6): a builtin series variable is advanced by an unguarded
/// prelude call, so a read under a branch that was false on earlier bars still sees every bar —
/// here the `ta.vwap` variable (hlc3, reset at the UTC day), read through a guarded
/// `math.abs`. The same call placed inside the branch (the per-site rule) misses the false bars:
/// that is why sema hoists the variables.
#[test]
fn builtin_series_advance_under_a_false_branch() {
    let bars = cum_bars(40);
    let want = vwap_var_reference(&bars);
    let branch: Vec<f64> = (0..bars.len())
        .map(|i| ((i % 5) == 4) as u8 as f64)
        .collect();
    let run = |hoisted: bool| -> Vec<f64> {
        let vw = &OVERLOADS[ov::TA_VWAP_VAR.0 as usize];
        let read = [(Lane::F64, Src::Out(0, 0))];
        let calls = [
            Call {
                ov: ov::TA_VWAP_VAR,
                inputs: &[],
                outputs: &[Lane::F64],
                site_bytes: vw.state.block_bytes(0),
                guard: if hoisted { None } else { Some(0) },
            },
            Call {
                ov: ov::MATH_ABS_F,
                inputs: &read,
                outputs: &[Lane::F64],
                site_bytes: 0,
                guard: Some(0),
            },
        ];
        let mut h = Harness::build(&Script {
            calls: &calls,
            mode: ExecMode::BarClose,
            dialect: Dialect::V6,
            intent_cap: 0,
            chart: CHART,
        });
        let mut ctx = TestCtx::default();
        let mut out = Vec::new();
        for (i, b) in bars.iter().enumerate() {
            ctx.feed = vec![branch[i]];
            assert_eq!(h.step_bar(b, &mut ctx), StepOutcome::Ok);
            out.push(h.out(1, 0));
        }
        out
    };
    let hoisted = run(true);
    let inside = run(false);
    let mut differs = false;
    for i in 0..bars.len() {
        if branch[i] == 1.0 {
            assert!(
                close_eq(hoisted[i], want[i]),
                "bar {i}: {} vs {}",
                hoisted[i],
                want[i]
            );
            differs |= !close_eq(inside[i], want[i]);
        }
    }
    assert!(differs, "a call inside the branch skips the false bars");
}

/// The `ta.vwap` variable on every bar: the day reset, a forming tick never advances it (I5).
#[test]
fn ta_vwap_variable_resets_at_the_utc_day() {
    let bars = cum_bars(24);
    let want = vwap_var_reference(&bars);
    let vw = &OVERLOADS[ov::TA_VWAP_VAR.0 as usize];
    let calls = [Call {
        ov: ov::TA_VWAP_VAR,
        inputs: &[],
        outputs: &[Lane::F64],
        site_bytes: vw.state.block_bytes(0),
        guard: None,
    }];
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::Realtime,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx::default();
    for (i, b) in bars.iter().enumerate() {
        let mut t = *b;
        t.flags = barflag::REALTIME;
        t.volume_1e6 *= 3;
        assert_eq!(h.step_tick(&t, &mut ctx), StepOutcome::Ok);
        assert_eq!(h.step_bar(b, &mut ctx), StepOutcome::Ok);
        assert!(
            close_eq(h.out(0, 0), want[i]),
            "bar {i}: {} vs {}",
            h.out(0, 0),
            want[i]
        );
        if i % 4 == 0 {
            let p = |x: i64| x as f64 / 1e6;
            let hlc3 = (p(b.high_1e6) + p(b.low_1e6) + p(b.close_1e6)) / 3.0;
            assert!(
                close_eq(h.out(0, 0), hlc3),
                "bar {i}: a new UTC day starts from its bar"
            );
        }
    }
}

/// The bar's prelude series: `hl2 hlc3 ohlc4 hlcc4` are the reference's literal definitions over
/// the bar's prices (left-to-right sums), bit for bit.
#[test]
fn bar_prelude_series_match_reference() {
    let p = |x: i64| x as f64 / 1e6;
    for id in [ov::HL2_VAR, ov::HLC3_VAR, ov::OHLC4_VAR, ov::HLCC4_VAR] {
        assert_eq!(pine_std::served(id), pine_std::Served::Body, "{id:?}");
        let outs = [Lane::F64];
        let calls = [Call {
            ov: id,
            inputs: &[],
            outputs: &outs,
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
        let mut ctx = TestCtx::default();
        for (i, &(o, hi, lo, c)) in [
            (10.0, 12.5, 9.25, 11.0),
            (101.1, 103.7, 99.9, 100.3),
            (0.3, 0.7, 0.1, 0.2),
        ]
        .iter()
        .enumerate()
        {
            let mut b = bar(i as u32, c, barflag::CONFIRMED);
            b.open_1e6 = scale::to_1e6(o).unwrap();
            b.high_1e6 = scale::to_1e6(hi).unwrap();
            b.low_1e6 = scale::to_1e6(lo).unwrap();
            assert_eq!(h.step_bar(&b, &mut ctx), StepOutcome::Ok);
            let (o, hi, lo, c) = (p(b.open_1e6), p(b.high_1e6), p(b.low_1e6), p(b.close_1e6));
            let want = if id == ov::HL2_VAR {
                (hi + lo) / 2.0
            } else if id == ov::HLC3_VAR {
                (hi + lo + c) / 3.0
            } else if id == ov::OHLC4_VAR {
                (o + hi + lo + c) / 4.0
            } else {
                (hi + lo + c + c) / 4.0
            };
            assert_eq!(h.out(0, 0).to_bits(), want.to_bits(), "{id:?} bar {i}");
        }
    }
}
