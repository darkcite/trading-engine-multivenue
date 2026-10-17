// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! CAR-R33-9 (r33i, `pin:U13-ta-in-loop` reading `restart`): an in-loop site whose block carries
//! `state_flag::RESTART` re-runs every execution on a bar from the block committed at the previous
//! bar, and commits the bar's last execution. The block is `[state b | pre-image b | stamp u64]`;
//! the body sees `b`. Covers the bar path, the D16 rollback (a tick re-run, the checkpoint image
//! byte for byte) and the verifier's rules 6 and 23 on the new shape.

use pine_ir::*;
use pine_vm::testkit::stub::*;
use pine_vm::testkit::*;
use pine_vm::*;

/// The `acc` block (one f64) as a `RESTART` block: `2 · 8 + 8`.
const RESTART_ACC: u32 = 24;

/// `sum = 0; first = na; last = na; n = int(close) % 4 (or 3 when `fixed`); for i = 0 to n - 1:
/// out = acc(close + i); sum += out; first := i == 0 ? out : first; last := out`. Plots `sum`,
/// `first`, `last`. With `only_new` the loop runs only when `barstate.isnew` (a tick re-run
/// that writes nothing). Returns the program and the site.
fn loop_program(mode: ExecMode, flags: u8, fixed: bool, only_new: bool) -> (PineProgram, u16) {
    let mut t = Tb::new(mode, 6, 6, 0, 0);
    t.n_plots = 3;
    let bytes = if flags & state_flag::RESTART != 0 {
        RESTART_ACC
    } else {
        8
    };
    let site = t.state_with(StateKind::Builtin, bytes, flags);
    let (sum, first, last, x, out) = (t.f(0), t.f(1), t.f(2), t.f(3), t.f(4));
    let (n, i, one, c, four, ip) = (t.i(0), t.i(1), t.i(2), t.i(3), t.i(4), t.i(5));
    let (k0, kna, kz, k1, k3, k4) = (t.kf(0.0), t.kf(f64::NAN), t.k(0), t.k(1), t.k(3), t.k(4));
    let skip = if only_new {
        Some(t.jump(op::JZ, 0, abi::I_BS_ISNEW))
    } else {
        None
    };
    t.emit(op::F_CONST, 0, sum, k0, 0);
    t.emit(op::F_CONST, 0, first, kna, 0);
    t.emit(op::F_CONST, 0, last, kna, 0);
    if fixed {
        t.emit(op::I_CONST, flg::VLD, n, k3, 0);
    } else {
        t.emit(op::I_F2I, 0, n, abi::F_CLOSE, 0);
        t.emit(op::I_CONST, flg::VLD, four, k4, 0);
        t.emit(op::I_MOD, 0, n, n, four);
    }
    t.emit(op::F_CONST, 0, x, k0, 0);
    t.emit(op::I_CONST, flg::VLD, i, kz, 0);
    t.emit(op::I_CONST, flg::VLD, one, k1, 0);
    let top = t.code.len() as u32;
    t.emit(op::I_LT, 0, c, i, n);
    let jend = t.jump(op::JZ, 0, c);
    t.emit(op::F_I2F, 0, x, i, 0);
    t.emit(op::F_ADD, 0, x, x, abi::F_CLOSE);
    let w = t.win(&[argwin::f64(x), argwin::f64(out)]);
    t.emit(op::CALL_BUILTIN, 0, site, ACC, w);
    t.emit(op::F_ADD, 0, sum, sum, out);
    let jfirst = t.jump(op::JNZ, 0, i);
    t.emit(op::F_MOV, 0, first, out, 0);
    let here = t.code.len() as u32;
    t.patch(jfirst, here);
    t.emit(op::F_MOV, 0, last, out, 0);
    t.emit(op::I_ADD, 0, i, i, one);
    t.jump(op::JMP, top, 0);
    let end = t.code.len() as u32;
    t.patch(jend, end);
    if let Some(j) = skip {
        t.patch(j, end);
    }
    let mut p = 0u64;
    while p < 3 {
        let kp = t.k(p);
        t.emit(op::I_CONST, flg::VLD, ip, kp, 0);
        let wp = t.win(&[argwin::i64(ip), argwin::f64(t.f(p as u32))]);
        t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PLOT, wp);
        p += 1;
    }
    t.emit(op::HALT, 0, 0, 0, 0);
    (t.build(), site)
}

/// One evaluation of [`loop_program`] under `restart` from committed state `s`: the plots and
/// the state the evaluation leaves (the last execution's).
fn restart_model(s: f64, close: f64, fixed: bool) -> ([f64; 3], f64) {
    let n = if fixed { 3 } else { (close as i64) % 4 };
    let (mut sum, mut first, mut last, mut left) = (0.0, f64::NAN, f64::NAN, s);
    let mut i = 0;
    while i < n {
        let out = s + (i as f64 + close); // every execution starts from `s`
        sum += out;
        if i == 0 {
            first = out;
        }
        last = out;
        left = out;
        i += 1;
    }
    ([sum, first, last], left)
}

fn same(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

/// The block bytes `[off, off + bytes)` of `site`.
fn block<'a>(vm: &'a VmInstance<'_>, p: &PineProgram, site: u16) -> &'a [u8] {
    let d = p.layout.states[site as usize];
    &arena(vm)[d.off as usize..(d.off + d.bytes) as usize]
}

const CLOSES: [f64; 9] = [5.0, 6.0, 7.0, 4.0, 9.0, 11.0, 3.0, 10.0, 8.0];

#[test]
fn exec_restart_site_reruns_from_the_committed_block() {
    for mode in [ExecMode::BarClose, ExecMode::Realtime] {
        let (p, site) = loop_program(mode, state_flag::IN_LOOP | state_flag::RESTART, true, false);
        let v = verify(&p, &TABLE).expect("verifies");
        let mut ar = Arena::new(p.layout.total_bytes as usize);
        let mut vm = VmInstance::new(v, ar.slice());
        let mut ctx = Ctx { now: 0 };
        let mut s = 0.0;
        for (k, &c) in CLOSES.iter().enumerate() {
            assert_eq!(
                vm.step_bar(&bar(k as u32, c, barflag::CONFIRMED), &mut ctx),
                StepOutcome::Ok
            );
            let (want, left) = restart_model(s, c, true);
            assert!(
                same(vm.plots(), &want),
                "{mode:?} bar {k}: {:?} vs {want:?}",
                vm.plots()
            );
            s = left;
            // The block: the state is the last execution's, the pre-image the committed state,
            // the stamp bar_index + 1.
            let b = block(&vm, &p, site);
            assert_eq!(
                f64::from_le_bytes(b[0..8].try_into().unwrap()),
                s,
                "the last execution is kept"
            );
            assert_eq!(
                u64::from_le_bytes(b[16..24].try_into().unwrap()),
                k as u64 + 1,
                "stamp"
            );
        }
        // The state half is what the body sees (cur_bytes = 8): a 3-call bar commits
        // s_prev + close + 2, never s_prev + 3·close + 3 (per_call).
        assert_eq!(s, CLOSES.iter().map(|c| c + 2.0).sum::<f64>());
    }
}

#[test]
fn exec_in_loop_without_restart_still_advances_per_call() {
    // `IN_LOOP` alone is the `per_call` reading, unchanged: every execution advances the block.
    let (p, _) = loop_program(ExecMode::BarClose, state_flag::IN_LOOP, true, false);
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    let mut s = 0.0;
    for (k, &c) in CLOSES.iter().enumerate() {
        assert_eq!(
            vm.step_bar(&bar(k as u32, c, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        let (mut sum, mut i) = (0.0, 0);
        let first = s + c;
        while i < 3 {
            s += i as f64 + c;
            sum += s;
            i += 1;
        }
        assert!(same(vm.plots(), &[sum, first, s]), "bar {k}");
    }
}

#[test]
fn exec_restart_with_a_varying_trip_count_keeps_the_last_execution() {
    // n = int(close) % 4: bars with 0..3 executions; a bar with none leaves the block untouched.
    let (p, _) = loop_program(
        ExecMode::BarClose,
        state_flag::IN_LOOP | state_flag::RESTART,
        false,
        false,
    );
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    let mut s = 0.0;
    for (k, &c) in CLOSES.iter().enumerate() {
        assert_eq!(
            vm.step_bar(&bar(k as u32, c, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        let (want, left) = restart_model(s, c, false);
        assert!(
            same(vm.plots(), &want),
            "bar {k} close {c}: {:?} vs {want:?}",
            vm.plots()
        );
        s = left;
    }
}

/// D16: a tick that runs the loop writes the state, the pre-image and the stamp; the next tick
/// (not new, so it writes nothing) undoes all three — the frame is the checkpoint byte for byte.
#[test]
fn realtime_restart_tick_rerun_restores_the_checkpoint_image() {
    let (p, site) = loop_program(
        ExecMode::Realtime,
        state_flag::IN_LOOP | state_flag::RESTART,
        true,
        true,
    );
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    let mut s = 0.0;
    let l = &p.layout;
    let end = (l.str_persist_off + l.str_persist_bytes) as usize;
    let mut b = 0u32;
    while b < 4 {
        let c = 3.0 + b as f64;
        assert_eq!(
            vm.step_bar(&bar(b, c, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        s = restart_model(s, c, true).1;
        let checkpoint = block(&vm, &p, site).to_vec();
        let states = arena(&vm)[l.states_off as usize..end].to_vec();
        // The first tick of bar b + 1 runs the loop from `s` (isnew).
        assert_eq!(
            vm.step_tick(&bar(b + 1, 50.0, barflag::REALTIME), &mut ctx),
            StepOutcome::Ok
        );
        let (want, left) = restart_model(s, 50.0, true);
        assert!(same(vm.plots(), &want), "bar {b} tick 1");
        let ticked = block(&vm, &p, site).to_vec();
        assert_eq!(f64::from_le_bytes(ticked[0..8].try_into().unwrap()), left);
        assert_eq!(
            &ticked[8..16],
            &checkpoint[0..8],
            "the pre-image holds the committed state"
        );
        assert_eq!(
            u64::from_le_bytes(ticked[16..24].try_into().unwrap()),
            b as u64 + 2,
            "stamp"
        );
        // The second tick is not new: it only undoes. State, pre-image and stamp are back.
        assert_eq!(
            vm.step_tick(&bar(b + 1, 60.0, barflag::REALTIME), &mut ctx),
            StepOutcome::Ok
        );
        assert_eq!(
            block(&vm, &p, site),
            &checkpoint[..],
            "bar {b}: the block is the checkpoint"
        );
        assert_eq!(
            &arena(&vm)[l.states_off as usize..end],
            &states[..],
            "bar {b}: states and pool"
        );
        b += 1;
    }
}

/// Ticks between bars change nothing a bar commits: a ticked instance and a bar-only one hold
/// the same block (state, pre-image, stamp) after every bar, and every tick re-runs from the
/// committed state.
#[test]
fn realtime_restart_ticks_then_bar_equal_bar_only() {
    let (p, site) = loop_program(
        ExecMode::Realtime,
        state_flag::IN_LOOP | state_flag::RESTART,
        false,
        false,
    );
    let v1 = verify(&p, &TABLE).expect("verifies");
    let v2 = verify(&p, &TABLE).expect("verifies");
    let (mut a1, mut a2) = (
        Arena::new(p.layout.total_bytes as usize),
        Arena::new(p.layout.total_bytes as usize),
    );
    let mut ticked = VmInstance::new(v1, a1.slice());
    let mut plain = VmInstance::new(v2, a2.slice());
    let mut ctx = Ctx { now: 0 };
    let ticks = [7.0, 2.0, 13.0, 4.0, 15.0];
    let mut s = 0.0;
    for (k, &c) in CLOSES.iter().enumerate() {
        let mut j = 0usize;
        while j < (k % 3) + 2 {
            let tc = ticks[(k + j) % ticks.len()];
            assert_eq!(
                ticked.step_tick(&bar(k as u32, tc, barflag::REALTIME), &mut ctx),
                StepOutcome::Ok
            );
            let (want, _) = restart_model(s, tc, false);
            assert!(
                same(ticked.plots(), &want),
                "bar {k} tick {j}: {:?} vs {want:?}",
                ticked.plots()
            );
            j += 1;
        }
        assert_eq!(
            ticked.step_bar(&bar(k as u32, c, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        assert_eq!(
            plain.step_bar(&bar(k as u32, c, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        let (want, left) = restart_model(s, c, false);
        assert!(
            same(ticked.plots(), &want) && same(plain.plots(), &want),
            "bar {k}"
        );
        assert_eq!(
            block(&ticked, &p, site),
            block(&plain, &p, site),
            "bar {k}: same block"
        );
        s = left;
    }
}

// ---- verifier --------------------------------------------------------------------------------

fn rule_of(p: &PineProgram, t: &'static BuiltinTable) -> u16 {
    verify(p, t).err().map(|e| e.rule).unwrap_or(0)
}

/// One `acc` call on a block of `bytes` with `flags` and `kind`.
fn one_call(kind: StateKind, bytes: u32, flags: u8) -> PineProgram {
    let mut t = Tb::new(ExecMode::Realtime, 1, 1, 0, 0);
    let site = t.state_with(kind, bytes, flags);
    let w = t.win(&[argwin::f64(abi::F_CLOSE), argwin::f64(t.f(0))]);
    if kind == StateKind::Builtin {
        t.emit(op::CALL_BUILTIN, 0, site, ACC, w);
    }
    t.emit(op::HALT, 0, 0, 0, 0);
    t.build()
}

#[test]
fn verify_rule_6_restart_needs_an_in_loop_builtin_block_of_2b_plus_8() {
    let (il, rs) = (state_flag::IN_LOOP, state_flag::RESTART);
    assert_eq!(
        rule_of(&one_call(StateKind::Builtin, 24, il | rs), &TABLE),
        0
    );
    assert_eq!(
        rule_of(&one_call(StateKind::Builtin, 136, il | rs), &TABLE),
        0
    );
    assert_eq!(
        rule_of(&one_call(StateKind::Builtin, 24, rs), &TABLE),
        rule::STATE_DENSE,
        "no IN_LOOP"
    );
    assert_eq!(
        rule_of(&one_call(StateKind::Builtin, 16, il | rs), &TABLE),
        rule::STATE_DENSE,
        "b < 8"
    );
    assert_eq!(
        rule_of(&one_call(StateKind::Builtin, 32, il | rs), &TABLE),
        rule::STATE_DENSE,
        "bytes != 2b + 8"
    );
    assert_eq!(
        rule_of(&one_call(StateKind::Collection, 40, il | rs), &TABLE),
        rule::STATE_DENSE,
        "a pool block"
    );
    assert_eq!(
        rule_of(&one_call(StateKind::Builtin, 24, 1 << 2), &TABLE),
        rule::STATE_DENSE,
        "an undefined bit"
    );
}

static WIDE: [BuiltinEntry; 1] = [BuiltinEntry {
    f: acc,
    cost: 1,
    n_out: 1,
    flags: bf::STATEFUL,
    state_bytes_min: 16,
}];
static WIDE_ARITY: [[u8; 2]; 1] = [[1, 1]];
static WIDE_TABLE: BuiltinTable = BuiltinTable {
    spec_sha256: [7; 32],
    entries: &WIDE,
    arity: &WIDE_ARITY,
};

#[test]
fn verify_rule_23_measures_the_restart_view_not_the_block() {
    let f = state_flag::IN_LOOP | state_flag::RESTART;
    // 24 B holds a 16 B minimum as a plain block, but its view is 8 B.
    assert_eq!(
        rule_of(&one_call(StateKind::Builtin, 24, 0), &WIDE_TABLE),
        0
    );
    assert_eq!(
        rule_of(&one_call(StateKind::Builtin, 24, f), &WIDE_TABLE),
        rule::SITE_KIND
    );
    assert_eq!(
        rule_of(&one_call(StateKind::Builtin, 40, f), &WIDE_TABLE),
        0,
        "view 16"
    );
}
