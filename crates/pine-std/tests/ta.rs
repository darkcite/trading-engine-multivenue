// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.7 — `ta.sma`, `ta.ema`, `ta.rma` (T15): brute-force agreement, Wilder vs EMA, I4
//! (zero is cold) and I5 (ticks never advance builtin state), through `verify` + `VmInstance`.
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::{ov, swr, Sw, OVERLOADS};
use pine_std::BUILTINS;
use pine_vm::testkit::*;
use pine_vm::*;

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

/// `id(close, len)` plotted every bar; `ticks` forming-bar re-evaluations before each confirm.
fn run(id: OverloadId, len: i64, closes: &[f64], ticks: u32, reset_at: Option<usize>) -> Vec<f64> {
    let mode = if ticks > 0 {
        ExecMode::Realtime
    } else {
        ExecMode::BarClose
    };
    let mut t = Tb::new(mode, 2, 2, 0, 0);
    t.spec = pine_spec::SPEC_SHA256;
    t.n_plots = 1;
    let bytes = OVERLOADS[id.0 as usize].state.block_bytes(len as u32);
    let site = t.state(StateKind::Builtin, bytes);
    let (fo, il) = (t.f(0), t.i(0));
    let k = t.k(len as u64);
    t.emit(op::I_CONST, flg::VLD, il, k, 0);
    let w = t.win(&[argwin::f64(abi::F_CLOSE), argwin::i64(il), argwin::f64(fo)]);
    t.emit(op::CALL_BUILTIN, 0, site, id.0, w);
    let wp = t.win(&[argwin::f64(fo)]);
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
    let mut out = Vec::new();
    for (i, &c) in closes.iter().enumerate() {
        if reset_at == Some(i) {
            vm.reset();
            out.clear();
        }
        let mut j = 0;
        while j < ticks {
            let tick = bar(i as u32, c * (1.5 + j as f64), barflag::REALTIME);
            assert_eq!(vm.step_tick(&tick, &mut ctx), StepOutcome::Ok);
            j += 1;
        }
        assert_eq!(
            vm.step_bar(&bar(i as u32, c, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        out.push(vm.plots()[0]);
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
    let c = tape(3);
    let mut t = Tb::new(ExecMode::BarClose, 2, 2, 0, 0);
    t.spec = pine_spec::SPEC_SHA256;
    let site = t.state(StateKind::Builtin, 24);
    let (fo, il) = (t.f(0), t.i(0));
    let k = t.k(0);
    t.emit(op::I_CONST, flg::VLD, il, k, 0);
    let w = t.win(&[argwin::f64(abi::F_CLOSE), argwin::i64(il), argwin::f64(fo)]);
    t.emit(op::CALL_BUILTIN, 0, site, ov::TA_EMA.0, w);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, table()).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let o = vm.step_bar(&bar(0, c[0], barflag::CONFIRMED), &mut Ctx { now: 0 });
    assert_ne!(o, StepOutcome::Ok);
    assert_eq!(vm.last_fault().map(|f| f.0), Some(FaultKind::ScriptRefused));
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

/// As `run_g`; `out_i64` puts the output in the i64 lane (bool/int results, plotted 0/1/value).
fn run_o(
    id: OverloadId,
    ins: &[In],
    len: u32,
    bars: &[(f64, f64, f64)],
    out_i64: bool,
) -> Vec<f64> {
    let mut t = Tb::new(ExecMode::BarClose, 2, 4, 0, 0);
    t.spec = pine_spec::SPEC_SHA256;
    t.n_plots = 1;
    let bytes = OVERLOADS[id.0 as usize].state.block_bytes(len);
    let site = t.state(StateKind::Builtin, bytes);
    let fo = t.f(0);
    let mut win = Vec::new();
    let mut ni = 0u32;
    for x in ins {
        match *x {
            In::Close => win.push(argwin::f64(abi::F_CLOSE)),
            In::F(x) => {
                let s = t.f(1);
                let k = t.kf(x);
                t.emit(op::F_CONST, 0, s, k, 0);
                win.push(argwin::f64(s));
            }
            In::Int(v) => {
                let s = t.i(ni);
                ni += 1;
                let k = t.k(v as u64);
                t.emit(op::I_CONST, flg::VLD, s, k, 0);
                win.push(argwin::i64(s));
            }
            In::Bool(b) => {
                let s = t.i(ni);
                ni += 1;
                let k = t.k(b as u64);
                t.emit(op::I_CONST, flg::VLD, s, k, 0);
                win.push(argwin::i64(s));
            }
        }
    }
    let out = if out_i64 {
        argwin::i64(t.i(3))
    } else {
        argwin::f64(fo)
    };
    win.push(out);
    let w = t.win(&win);
    t.emit(op::CALL_BUILTIN, 0, site, id.0, w);
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
    let mut out = Vec::new();
    for (i, &(h, l, c)) in bars.iter().enumerate() {
        let mut b = bar(i as u32, c, barflag::CONFIRMED);
        b.high_1e6 = scale::to_1e6(h).unwrap();
        b.low_1e6 = scale::to_1e6(l).unwrap();
        assert_eq!(vm.step_bar(&b, &mut ctx), StepOutcome::Ok);
        out.push(vm.plots()[0]);
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

/// A 3-output row over `[close, ints…, floats…]` → `(o0, o1, o2)` per bar; outputs prefilled with
/// a sentinel so an unwritten output shows.
fn run3(id: OverloadId, ints: &[i64], fl: &[f64], len: u32, c: &[f64]) -> Vec<[f64; 3]> {
    let mut t = Tb::new(ExecMode::BarClose, 8, 4, 0, 0);
    t.spec = pine_spec::SPEC_SHA256;
    t.n_plots = 3;
    let bytes = OVERLOADS[id.0 as usize].state.block_bytes(len);
    let site = t.state(StateKind::Builtin, bytes);
    let mut win = vec![argwin::f64(abi::F_CLOSE)];
    for (k, &v) in ints.iter().enumerate() {
        let s = t.i(k as u32);
        let kk = t.k(v as u64);
        t.emit(op::I_CONST, flg::VLD, s, kk, 0);
        win.push(argwin::i64(s));
    }
    for (k, &v) in fl.iter().enumerate() {
        let s = t.f(k as u32);
        let kk = t.kf(v);
        t.emit(op::F_CONST, 0, s, kk, 0);
        win.push(argwin::f64(s));
    }
    let outs = [
        argwin::f64(t.f(5)),
        argwin::f64(t.f(6)),
        argwin::f64(t.f(7)),
    ];
    let ks = t.kf(-12345.0);
    for o in outs {
        t.emit(op::F_CONST, 0, o, ks, 0);
    }
    win.extend(outs);
    let w = t.win(&win);
    t.emit(op::CALL_BUILTIN, 0, site, id.0, w);
    let wp = t.win(&outs);
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
    let mut out = Vec::new();
    for (i, &x) in c.iter().enumerate() {
        assert_eq!(
            vm.step_bar(&bar(i as u32, x, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        let p = vm.plots();
        out.push([p[0], p[1], p[2]]);
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
