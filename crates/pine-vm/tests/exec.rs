// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

use pine_ir::*;
use pine_vm::*;
use pine_vm::testkit::stub::*;
use pine_vm::testkit::*;

/// Plot f64 slot `s` into plot index `idx` (via the stub plot builtin).
fn plot_f(t: &mut Tb, idx: u64, s: u16, iscratch: u16) {
    let k = t.k(idx);
    t.emit(op::I_CONST, flg::VLD, iscratch, k, 0);
    let w = t.win(&[argwin::i64(iscratch), argwin::f64(s)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PLOT, w);
}

fn run_bars<'p>(vm: &mut VmInstance<'p>, closes: &[f64]) -> Vec<Vec<f64>> {
    let mut ctx = Ctx { now: 0 };
    let mut out = Vec::new();
    for (i, c) in closes.iter().enumerate() {
        assert_eq!(vm.step_bar(&bar(i as u32, *c, barflag::CONFIRMED), &mut ctx), StepOutcome::Ok);
        out.push(vm.plots().to_vec());
    }
    out
}

#[test]
fn commit_scatter_history_reads_previous_bars_and_no_tick_bias() {
    let mut t = Tb::new(ExecMode::Realtime, 2, 1, 0, 0);
    t.n_plots = 2;
    let r = t.ring(abi::F_CLOSE, Lane::F64, 0, 4);
    let (f0, f1, i0) = (t.f(0), t.f(1), t.i(0));
    t.emit(op::HIST_F, 0, f0, r, 1); // close[1]
    t.emit(op::HIST_F, 0, f1, r, 2); // close[2]
    plot_f(&mut t, 0, f0, i0);
    plot_f(&mut t, 1, f1, i0);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let o = run_bars(&mut vm, &[10.0, 11.0, 12.0]);
    assert!(o[0][0].is_nan() && o[0][1].is_nan(), "bar 0 reads the initial NaN cells");
    assert_eq!(o[1][0], 10.0);
    assert!(o[1][1].is_nan());
    assert_eq!((o[2][0], o[2][1]), (11.0, 10.0));
    // A tick of the forming bar 3: close[1] is the last confirmed bar (12), no bias.
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_tick(&bar(3, 99.0, barflag::REALTIME), &mut ctx), StepOutcome::Ok);
    assert_eq!((vm.plots()[0], vm.plots()[1]), (12.0, 11.0));
    assert_eq!(vm.step_bar(&bar(3, 13.0, barflag::CONFIRMED | barflag::REALTIME), &mut ctx), StepOutcome::Ok);
    assert_eq!((vm.plots()[0], vm.plots()[1]), (12.0, 11.0));
}

#[test]
fn unassigned_slot_carries_forward_into_history() {
    // f0 is assigned only on bar 0 (isfirst); its ring still commits f0 every bar.
    let mut t = Tb::new(ExecMode::BarClose, 2, 1, 0, 0);
    t.n_plots = 1;
    let r = t.ring(t.f(0), Lane::F64, 0, 2);
    let (f0, f1, i0) = (t.f(0), t.f(1), t.i(0));
    let j = t.jump(op::JZ, 0, abi::I_BS_ISFIRST);
    t.emit(op::F_MOV, 0, f0, abi::F_CLOSE, 0);
    let after = t.code.len() as u32;
    t.patch(j, after);
    t.emit(op::HIST_F, 0, f1, r, 1);
    plot_f(&mut t, 0, f1, i0);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let o = run_bars(&mut vm, &[5.0, 6.0, 7.0, 8.0, 9.0]);
    assert!(o[0][0].is_nan());
    let mut k = 1;
    while k < 5 {
        assert_eq!(o[k][0], 5.0, "x[1] carries bar 0's value on bar {k} (across a ring wrap)");
        k += 1;
    }
}

#[test]
fn tick_rolls_back_state_blocks_lanes_and_keeps_varip() {
    // acc(close) -> f0 (state block); f1 += 1 (lane, non-varip); fv0 += 1 (varip).
    let mut t = Tb::new(ExecMode::Realtime, 3, 1, 1, 0);
    t.n_plots = 3;
    let site = t.state(StateKind::Builtin, 8);
    let (f0, f1, k1, fv, i0) = (t.f(0), t.f(1), t.f(2), t.fv(0), t.i(0));
    let one = t.kf(1.0);
    let w = t.win(&[argwin::f64(abi::F_CLOSE), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, site, ACC, w);
    t.emit(op::F_CONST, 0, k1, one, 0);
    t.emit(op::F_ADD, 0, f1, f1, k1);
    t.emit(op::F_ADD, 0, fv, fv, k1);
    plot_f(&mut t, 0, f0, i0);
    plot_f(&mut t, 1, f1, i0);
    plot_f(&mut t, 2, fv, i0);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots(), &[1.0, 1.0, 1.0]);
    assert_eq!(vm.step_tick(&bar(1, 5.0, barflag::REALTIME), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots(), &[6.0, 2.0, 2.0]);
    assert_eq!(vm.step_tick(&bar(1, 7.0, barflag::REALTIME), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots(), &[8.0, 2.0, 3.0], "state and lane rolled back; varip kept");
    assert_eq!(vm.step_bar(&bar(1, 2.0, barflag::CONFIRMED | barflag::REALTIME), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots(), &[3.0, 2.0, 4.0], "the bar close undoes the last tick; varip survives");
}

#[test]
fn intents_are_per_evaluation_and_never_unemitted() {
    let mut t = Tb::new(ExecMode::Realtime, 1, 1, 0, 0);
    t.intent_cap = 2;
    let w = t.win(&[argwin::f64(abi::F_CLOSE)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, ENTRY, w);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    let mut handed: Vec<i64> = Vec::new(); // what a broker would have committed (W21)
    assert_eq!(vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.intents().len(), 1);
    handed.push(vm.intents()[0].qty_1e6);
    assert_eq!(vm.step_tick(&bar(1, 2.0, barflag::REALTIME), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.intents().len(), 1, "cleared at the start of the evaluation, one pushed");
    assert_eq!(vm.intents()[0].seq, 0, "the VM stamps emission order");
    handed.push(vm.intents()[0].qty_1e6);
    assert_eq!(vm.step_tick(&bar(1, 3.0, barflag::REALTIME), &mut ctx), StepOutcome::Ok);
    handed.push(vm.intents()[0].qty_1e6);
    assert_eq!(handed, vec![1_000_000, 2_000_000, 3_000_000], "a tick's intents are the host's once handed out");
}

#[test]
fn int_division_by_zero_is_na_counted_and_not_a_fault() {
    let mut t = Tb::new(ExecMode::BarClose, 1, 4, 0, 0);
    t.n_plots = 2;
    let (i0, i1, i2, i3, f0) = (t.i(0), t.i(1), t.i(2), t.i(3), t.f(0));
    let k7 = t.k(7);
    let k0 = t.k(0);
    t.emit(op::I_CONST, flg::VLD, i0, k7, 0);
    t.emit(op::I_CONST, flg::VLD, i1, k0, 0);
    t.emit(op::I_DIV, flg::VLD, i2, i0, i1); // 7 / 0 -> na
    t.emit(op::NA_CHECK, flg::LANE_I, i3, i2, 0);
    t.emit(op::F_I2F, 0, f0, i3, 0);
    plot_f(&mut t, 0, f0, i1);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let o = run_bars(&mut vm, &[1.0]);
    assert_eq!(o[0][0], 1.0, "na(7 / 0) is true");
    assert_eq!(vm.counters().na_arith, 1);
    assert_eq!(vm.counters().faults, 0);
}

#[test]
fn f_ne_with_nan_is_false_and_f_min_propagates() {
    let mut t = Tb::new(ExecMode::BarClose, 3, 2, 0, 0);
    t.n_plots = 2;
    let (f0, f1, f2, i0, i1) = (t.f(0), t.f(1), t.f(2), t.i(0), t.i(1));
    let nan = t.kf(f64::NAN);
    t.emit(op::F_CONST, 0, f0, nan, 0);
    t.emit(op::F_NE, 0, i0, f0, abi::F_CLOSE);
    t.emit(op::F_I2F, 0, f1, i0, 0);
    t.emit(op::F_MIN, 0, f2, f0, abi::F_CLOSE);
    plot_f(&mut t, 0, f1, i1);
    plot_f(&mut t, 1, f2, i1);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).unwrap();
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let o = run_bars(&mut vm, &[3.0]);
    assert_eq!(o[0][0], 0.0, "na != 3 is false");
    assert!(o[0][1].is_nan(), "min(na, 3) is na");
}

#[test]
fn back_edge_charges_and_a_runaway_loop_faults() {
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    t.budget = 1000;
    let top = t.code.len() as u32;
    t.emit(op::NOP, 0, 0, 0, 0);
    t.jump(op::JMP, top, 0);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).unwrap();
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_bar(&bar(0, 1.0, 1), &mut ctx), StepOutcome::Fault(FaultKind::InsnBudget));
    assert_eq!(vm.last_fault(), Some((FaultKind::InsnBudget, 1)));
    assert_eq!(vm.step_bar(&bar(1, 1.0, 1), &mut ctx), StepOutcome::Fault(FaultKind::InsnBudget), "latched");
    assert_eq!(vm.counters().faults, 1);
}

#[test]
fn a_ret_without_call_cannot_loop_uncharged() {
    // 0: CALL 3 (link L) ; 1: JMP 3 ; 2: HALT ; 3: NOP ; 4: RET L  -> 1 -> 3 -> 4 -> 1 ...
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    t.budget = 500;
    let l = t.i(0);
    t.jump(op::CALL, 3, l);
    t.jump(op::JMP, 3, 0);
    t.emit(op::HALT, 0, 0, 0, 0);
    t.emit(op::NOP, 0, 0, 0, 0);
    t.emit(op::RET, 0, 0, 0, l);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("rules 9-12 accept it");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_bar(&bar(0, 1.0, 1), &mut ctx), StepOutcome::Fault(FaultKind::InsnBudget));
}

#[test]
fn objects_new_get_set_and_stale_handle_refuses() {
    let mut t = Tb::new(ExecMode::Realtime, 1, 3, 0, 0);
    t.n_plots = 1;
    let site = t.state(StateKind::Object, 32 + 16);
    let (h, h_old, ix, f0) = (t.i(0), t.i(1), t.i(2), t.f(0));
    t.emit(op::O_NEW, 0, h, site, 2);
    t.emit(op::O_SET_F, 0, h, 1, abi::F_CLOSE);
    t.emit(op::O_GET_F, 0, f0, h, 1);
    plot_f(&mut t, 0, f0, ix);
    // on bar 1 (not first): read through the handle kept from the previous bar -> stale
    let j = t.jump(op::JNZ, 0, abi::I_BS_ISFIRST);
    t.emit(op::O_GET_F, 0, f0, h_old, 0);
    let end = t.code.len() as u32;
    t.patch(j, end);
    t.emit(op::I_MOV, flg::VLD, h_old, h, 0);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_bar(&bar(0, 4.0, 1), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots()[0], 4.0);
    assert_eq!(vm.step_bar(&bar(1, 5.0, 1), &mut ctx), StepOutcome::Fault(FaultKind::ScriptRefused));
}

#[test]
fn s_persist_copies_scratch_keeps_const_and_refuses_overlong() {
    let mut t = Tb::new(ExecMode::BarClose, 0, 6, 0, 0);
    t.cell_bytes = 8;
    t.scratch = 64;
    t.n_plots = 0;
    let cell = t.cell();
    let (s, a, b, cat, n) = (t.i(0), t.i(1), t.i(2), t.i(3), t.i(4));
    let ka = t.kstr("ab");
    let kb = t.kstr("cde");
    t.emit(op::I_CONST, flg::VLD, a, ka, 0);
    t.emit(op::I_CONST, flg::VLD, b, kb, 0);
    let w = t.win(&[argwin::i64(a), argwin::i64(b), argwin::i64(cat)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, w); // "abcde" in scratch
    // s := s + "cde" each bar: 5, 8, 11 bytes -> the third store overflows an 8-byte cell
    let j = t.jump(op::JNZ, 0, abi::I_BS_ISFIRST);
    let w2 = t.win(&[argwin::i64(s), argwin::i64(b), argwin::i64(cat)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, w2);
    let here = t.code.len() as u32;
    t.patch(j, here);
    t.emit(op::S_PERSIST, 0, s, cat, cell);
    let wl = t.win(&[argwin::i64(s), argwin::i64(n)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, SLEN, wl);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut a2 = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a2.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_bar(&bar(0, 1.0, 1), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.step_bar(&bar(1, 1.0, 0), &mut ctx), StepOutcome::Ok, "5 + 3 = 8 bytes fits");
    assert_eq!(vm.step_bar(&bar(2, 1.0, 0), &mut ctx), StepOutcome::Fault(FaultKind::StrArenaFull));
}

#[test]
fn a_scratch_string_read_after_its_evaluation_faults_internal() {
    // cat is NOT persisted; bar 1 reads it -> stale epoch -> Internal (never silent garbage).
    let mut t = Tb::new(ExecMode::BarClose, 0, 4, 0, 0);
    let (a, cat, n) = (t.i(0), t.i(1), t.i(2));
    let ka = t.kstr("xy");
    let j = t.jump(op::JZ, 0, abi::I_BS_ISFIRST);
    t.emit(op::I_CONST, flg::VLD, a, ka, 0);
    let w = t.win(&[argwin::i64(a), argwin::i64(a), argwin::i64(cat)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, w);
    let here = t.code.len() as u32;
    t.patch(j, here);
    let wl = t.win(&[argwin::i64(cat), argwin::i64(n)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, SLEN, wl);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).unwrap();
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_bar(&bar(0, 1.0, 1), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.step_bar(&bar(1, 1.0, 0), &mut ctx), StepOutcome::Fault(FaultKind::Internal));
}

#[test]
fn reset_restores_the_image_new_produced() {
    let mut t = Tb::new(ExecMode::Realtime, 2, 2, 1, 1);
    t.n_plots = 1;
    let site = t.state(StateKind::Builtin, 8);
    let r = t.ring(abi::F_CLOSE, Lane::F64, 0, 4);
    let (f0, f1, i0) = (t.f(0), t.f(1), t.i(0));
    let w = t.win(&[argwin::f64(abi::F_CLOSE), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, site, ACC, w);
    t.emit(op::HIST_F, 0, f1, r, 1);
    plot_f(&mut t, 0, f0, i0);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).unwrap();
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let fresh = arena(&vm).to_vec();
    let mut ctx = Ctx { now: 0 };
    let mut k = 0;
    while k < 20 {
        vm.step_bar(&bar(k, k as f64, 1), &mut ctx);
        vm.step_tick(&bar(k + 1, 1.5, barflag::REALTIME), &mut ctx);
        k += 1;
    }
    vm.reset();
    assert_eq!(arena(&vm), &fresh[..], "reset == new, byte for byte");
    assert_eq!((vm.bars_seen(), vm.counters(), vm.last_fault()), (0, VmCounters::default(), None));
}
