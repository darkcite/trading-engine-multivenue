// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Linkage-pass tests: the A2 `barstate.*` table (§3.6.5), truncated `%` (CAR-R31-1),
//! and the `Tb` layout discipline impl-02 compares against (64-aligned big blocks, flags).

use pine_ir::*;
use pine_vm::testkit::stub::*;
use pine_vm::testkit::*;
use pine_vm::*;

const SLOTS: [u16; 8] = [
    abi::I_BS_ISFIRST,
    abi::I_BS_ISCONFIRMED,
    abi::I_BS_ISLAST,
    abi::I_BS_ISNEW,
    abi::I_BS_ISREALTIME,
    abi::I_BS_ISHISTORY,
    abi::I_BS_ISLASTCONFHIST,
    abi::I_BAR_INDEX,
];

/// A Realtime program plotting the seven barstate slots and bar_index (plots 0..8).
fn barstate_program() -> PineProgram {
    let mut t = Tb::new(ExecMode::Realtime, 8, 1, 0, 0);
    t.n_plots = 8;
    let iscratch = t.i(0);
    let mut k = 0usize;
    while k < SLOTS.len() {
        let f = t.f(k as u32);
        t.emit(op::F_I2F, 0, f, SLOTS[k], 0);
        let ki = t.k(k as u64);
        t.emit(op::I_CONST, flg::VLD, iscratch, ki, 0);
        let w = t.win(&[argwin::i64(iscratch), argwin::f64(f)]);
        t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PLOT, w);
        k += 1;
    }
    t.emit(op::HALT, 0, 0, 0, 0);
    t.build()
}

/// [isfirst, isconfirmed, islast, isnew, isrealtime, ishistory, islastconfhist, bar_index]
fn bs(vm: &VmInstance<'_>) -> [i64; 8] {
    let p = vm.plots();
    let mut o = [0i64; 8];
    let mut k = 0;
    while k < 8 {
        o[k] = p[k] as i64;
        k += 1;
    }
    o
}

#[test]
fn inst_barstate_bar_column_follows_the_flags() {
    let p = barstate_program();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    let (c, r, l, h) = (barflag::CONFIRMED, barflag::REALTIME, barflag::LAST, barflag::LAST_CONFIRMED_HISTORY);
    let mut m = 0u32;
    while m < 16 {
        let flags = (if m & 1 != 0 { c } else { 0 }) | (if m & 2 != 0 { r } else { 0 }) | (if m & 4 != 0 { l } else { 0 }) | (if m & 8 != 0 { h } else { 0 });
        assert_eq!(vm.step_bar(&bar(m, 1.0, flags), &mut ctx), StepOutcome::Ok);
        let rt = (flags & r != 0) as i64;
        let want = [
            (m == 0) as i64,                        // bar_index == 0
            (flags & c != 0) as i64,
            (flags & (l | r) != 0) as i64,
            1,                                      // no step_tick ran on any of these bars
            rt,
            1 - rt,
            (flags & h != 0) as i64,
            m as i64,
        ];
        assert_eq!(bs(&vm), want, "flags {flags:#b}");
        m += 1;
    }
}

#[test]
fn inst_barstate_tick_column_reads_the_flags_and_isnew_first_tick_only() {
    let p = barstate_program();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    let (c, r) = (barflag::CONFIRMED, barflag::REALTIME);
    // Cold start: a tick before any bar has bar_index 0, so isfirst is true in the tick column.
    assert_eq!(vm.step_tick(&bar(0, 1.0, r), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [1, 0, 1, 1, 1, 0, 0, 0]);
    assert_eq!(vm.step_tick(&bar(0, 1.0, r), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [1, 0, 1, 0, 1, 0, 0, 0], "second tick of a realtime bar: isnew false");
    // Its confirm: a step_tick already ran on this REALTIME bar, so isnew is false.
    assert_eq!(vm.step_bar(&bar(0, 1.0, c | r), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [1, 1, 1, 0, 1, 0, 0, 0]);
    // A realtime bar confirmed with no tick: isnew true.
    assert_eq!(vm.step_bar(&bar(1, 1.0, c | r), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 1, 1, 1, 1, 0, 0, 1]);
    // History re-executions through step_tick (TV-mode calc_on_order_fills, impl-05 §3.6.1):
    // the flag rows read the bar; isconfirmed / islastconfhist stay false; isnew stays true.
    let hist = c | barflag::LAST_CONFIRMED_HISTORY;
    assert_eq!(vm.step_tick(&bar(2, 1.0, hist), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 0, 0, 1, 0, 1, 0, 2]);
    assert_eq!(vm.step_tick(&bar(2, 1.0, hist), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 0, 0, 1, 0, 1, 0, 2], "not REALTIME: isnew true on every tick");
    assert_eq!(vm.step_bar(&bar(2, 1.0, hist), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 1, 0, 1, 0, 1, 1, 2], "not REALTIME: isnew true although ticks ran");
    // A live forming bar: the tick column equals v4's original constants.
    assert_eq!(vm.step_tick(&bar(3, 1.0, r), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 0, 1, 1, 1, 0, 0, 3]);
    assert_eq!(vm.step_tick(&bar(3, 1.0, r), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 0, 1, 0, 1, 0, 0, 3]);
}

/// CAR-D1 (R-P6): a partial history bar (`CONFIRMED` and `REALTIME` clear) through `step_tick` —
/// `isnew` on its first history tick only, a `calc_on_order_fills` re-run on the full bar stays
/// new, `isconfirmed` false throughout; the bar-close column is unchanged
/// (`pin:U17-hist-close-isnew`) and the counter restarts at `step_bar`.
#[test]
fn inst_barstate_history_ticks_isnew_first_only_and_cof_reruns_unchanged() {
    let p = barstate_program();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    let c = barflag::CONFIRMED;
    assert_eq!(vm.step_bar(&bar(0, 1.0, c), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.step_tick(&bar(1, 1.0, 0), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 0, 0, 1, 0, 1, 0, 1]);
    assert_eq!(vm.step_tick(&bar(1, 1.0, c), &mut ctx), StepOutcome::Ok);
    assert_eq!(
        bs(&vm),
        [0, 0, 0, 1, 0, 1, 0, 1],
        "cof re-run on the full bar: isnew true"
    );
    assert_eq!(vm.step_tick(&bar(1, 1.0, 0), &mut ctx), StepOutcome::Ok);
    assert_eq!(
        bs(&vm),
        [0, 0, 0, 0, 0, 1, 0, 1],
        "second history tick: isnew false"
    );
    assert_eq!(vm.step_tick(&bar(1, 1.0, 0), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 0, 0, 0, 0, 1, 0, 1]);
    assert_eq!(vm.step_bar(&bar(1, 1.0, c), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 1, 0, 1, 0, 1, 0, 1]);
    assert_eq!(vm.step_tick(&bar(2, 1.0, 0), &mut ctx), StepOutcome::Ok);
    assert_eq!(bs(&vm), [0, 0, 0, 1, 0, 1, 0, 2]);
}

/// Plot `a OP b` for each pair: I_MOD (i64, via F_I2F|VLD so na plots NaN) or F_MOD.
fn mod_program(int: bool, pairs: &[(u64, u64)], flags: u8) -> PineProgram {
    let mut t = Tb::new(ExecMode::BarClose, 3, 4, 0, 0);
    t.n_plots = pairs.len() as u32;
    let (fa, fb, fr) = (t.f(0), t.f(1), t.f(2));
    let (ia, ib, ir, is) = (t.i(0), t.i(1), t.i(2), t.i(3));
    let mut k = 0usize;
    while k < pairs.len() {
        let (ka, kb) = (t.k(pairs[k].0), t.k(pairs[k].1));
        if int {
            t.emit(op::I_CONST, flg::VLD, ia, ka, 0);
            t.emit(op::I_CONST, flg::VLD, ib, kb, 0);
            t.emit(op::I_MOD, flg::VLD, ir, ia, ib);
            t.emit(op::F_I2F, flg::VLD, fr, ir, 0);
        } else {
            t.emit(op::F_CONST, 0, fa, ka, 0);
            t.emit(op::F_CONST, 0, fb, kb, 0);
            t.emit(op::F_MOD, flags, fr, fa, fb);
        }
        let ki = t.k(k as u64);
        t.emit(op::I_CONST, flg::VLD, is, ki, 0);
        let w = t.win(&[argwin::i64(is), argwin::f64(fr)]);
        t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PLOT, w);
        k += 1;
    }
    t.emit(op::HALT, 0, 0, 0, 0);
    t.build()
}

fn run_once(p: &PineProgram) -> (Vec<f64>, VmCounters) {
    let v = verify(p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx), StepOutcome::Ok);
    (vm.plots().to_vec(), vm.counters())
}

#[test]
fn exec_i_mod_is_truncated_the_sign_of_the_dividend() {
    // CAR-R31-1 (TradingView F004 / F115: `807 % -7 == 2`, `-807 % 7 == -2`).
    let cases: [(i64, i64, Option<i64>); 11] = [
        (-1, 100, Some(-1)),
        (1, -100, Some(1)),
        (-7, 3, Some(-1)),
        (7, -3, Some(1)),
        (7, 3, Some(1)),
        (-7, -3, Some(-1)),
        (-7, 7, Some(0)),
        (i64::MIN, -1, Some(0)),  // the true remainder, valid, not counted
        (i64::MIN, 3, Some(-2)),
        (i64::MAX, -2, Some(1)),
        (7, 0, None),             // b == 0: na, counted
    ];
    let pairs: Vec<(u64, u64)> = cases.iter().map(|c| (c.0 as u64, c.1 as u64)).collect();
    let (o, n) = run_once(&mod_program(true, &pairs, 0));
    let mut k = 0;
    while k < cases.len() {
        match cases[k].2 {
            Some(v) => assert_eq!(o[k], v as f64, "{} % {}", cases[k].0, cases[k].1),
            None => assert!(o[k].is_nan(), "{} % 0 is na", cases[k].0),
        }
        k += 1;
    }
    assert_eq!((n.na_arith, n.faults), (1, 0));
}

#[test]
fn exec_f_mod_is_the_exact_truncated_remainder() {
    // CAR-R31-1: IEEE `fmod` (the dividend's sign, exact) — TradingView F098 series rows
    // (`(1.0 + (syn - syn)) % 0.1` = 0.09999999999999995) and F039 / F116 (every row).
    let cases: [(f64, f64); 12] = [
        (-1.0, 100.0), (1.0, -100.0), (5.5, -2.0), (-5.5, 2.0), (7.0, 3.0), (-0.0, 5.0),
        (1.0, 0.1), (10.0, 0.1), (0.7, 0.1), (-0.3, 0.1), (1e17, 7.0), (7.0, 0.0),
    ];
    let pairs: Vec<(u64, u64)> = cases.iter().map(|c| (c.0.to_bits(), c.1.to_bits())).collect();
    let mut flags = 0u8;
    while flags < 2 {
        let (o, _) = run_once(&mod_program(false, &pairs, flags));
        let mut k = 0;
        while k < cases.len() {
            let (a, b) = cases[k];
            let want = a % b;
            if want.is_nan() {
                assert!(o[k].is_nan(), "{a} % {b} (flags {flags})"); // the flag canonicalises NaN
            } else {
                assert_eq!(o[k].to_bits(), want.to_bits(), "{a} % {b} (flags {flags})");
            }
            k += 1;
        }
        assert_eq!((o[0], o[1], o[2], o[3]), (-1.0, 1.0, 1.5, -1.5), "the dividend's sign");
        assert_eq!(o[6], 0.09999999999999995, "1.0 % 0.1 is the exact remainder (TradingView F098)");
        assert!(o[11].is_nan(), "x % 0 is NaN under either U15 reading (0 * inf)");
        flags += 1;
    }
}

#[test]
fn tb_blocks_of_256_bytes_or_more_are_64_aligned_and_carry_flags() {
    let mut t = Tb::new(ExecMode::Realtime, 1, 1, 0, 0);
    let s0 = t.state(StateKind::Builtin, 8);
    let s1 = t.state_with(StateKind::Builtin, 256, state_flag::IN_LOOP);
    let s2 = t.state(StateKind::Collection, 40);
    let s3 = t.state(StateKind::Collection, 512);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let l = &p.layout;
    assert_eq!(l.states[s0 as usize].off, l.states_off);
    assert_eq!(l.states[s1 as usize].off, l.states_off + 64, "a >= 256 B block starts on a line");
    assert_eq!(l.states[s1 as usize].flags, state_flag::IN_LOOP);
    assert_eq!(l.states[s2 as usize].off, l.pool_off);
    assert_eq!(l.states[s3 as usize].off, l.pool_off + 64);
    assert_eq!(l.total_bytes % 64, 0);
    verify(&p, &TABLE).expect("the padded layout verifies");
}
