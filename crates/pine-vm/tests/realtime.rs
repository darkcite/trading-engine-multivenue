// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

use pine_ir::*;
use pine_vm::*;
use pine_vm::testkit::stub::*;
use pine_vm::testkit::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The image a tick must leave behind after undo: [0, str_persist end) with the
/// reserved lane words masked (populate rewrites them), plus the dirty masks.
fn image(vm: &VmInstance<'_>, p: &PineProgram) -> Vec<u8> {
    let l = &p.layout;
    let a = arena(vm);
    let mut v = a[..(l.str_persist_off + l.str_persist_bytes) as usize].to_vec();
    let mut s = 0;
    while s < abi::RESERVED_F as usize {
        let o = l.f64_off as usize + 8 * s;
        v[o..o + 8].fill(0);
        s += 1;
    }
    s = 0;
    while s < abi::RESERVED_I as usize {
        let o = l.i64_off as usize + 8 * s;
        v[o..o + 8].fill(0);
        s += 1;
    }
    v.extend_from_slice(&a[l.dirty_off as usize..(l.dirty_off + 16 * l.dirty_words) as usize]);
    v
}

#[test]
fn prop_undo_restores_the_checkpoint_for_random_tick_write_sets() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut case = 0;
    while case < 200 {
        let (nf, ni, nfv, niv) = (12u32, 12u32, (case % 3) as u32, (case % 2) as u32 * 3);
        let mut t = Tb::new(ExecMode::Realtime, nf, ni, nfv, niv);
        t.cell_bytes = 16;
        let n_acc = 3u16;
        let mut sites = Vec::new();
        let mut k = 0;
        while k < n_acc {
            sites.push(t.state(StateKind::Builtin, 8));
            k += 1;
        }
        let obj = t.state(StateKind::Object, 32 + 8 * 3);
        let cell = t.cell();
        let kstr = t.kstr("abc");
        let kone = t.kf(1.0);
        let kseven = t.k(7);
        // On the first tick of a bar (isnew) write a random set; later ticks write nothing.
        let skip = t.jump(op::JZ, 0, abi::I_BS_ISNEW);
        let n_ops = 5 + rng.below(40) as usize;
        let mut used_acc = 0u16;
        let mut made_obj = false;
        let mut o = 0;
        while o < n_ops {
            let fs = if rng.below(4) == 0 && nfv > 0 { t.fv(rng.below(nfv as u64) as u32) } else { t.f(rng.below(nf as u64) as u32) };
            let is = if rng.below(4) == 0 && niv > 0 { t.iv(rng.below(niv as u64) as u32) } else { t.i(4 + rng.below(ni as u64 - 4) as u32) }; // i0..i3 reserved
            match rng.below(7) {
                0 => {
                    t.emit(op::F_ADD, 0, fs, fs, abi::F_CLOSE);
                }
                1 => {
                    t.emit(op::I_CONST, flg::VLD, is, kseven, 0);
                }
                2 => {
                    t.emit(op::NA_SET, flg::LANE_I, 0, is, 0);
                }
                3 if used_acc < n_acc => {
                    let w = t.win(&[argwin::f64(abi::F_CLOSE), argwin::f64(t.f(0))]);
                    t.emit(op::CALL_BUILTIN, 0, sites[used_acc as usize], ACC, w);
                    used_acc += 1;
                }
                4 => {
                    let h = t.i(1);
                    if !made_obj {
                        t.emit(op::O_NEW, 0, h, obj, 3); // one creating instruction per site (rule 15)
                        made_obj = true;
                    }
                    t.emit(op::O_SET_F, 0, h, 2, fs);
                }
                5 => {
                    let (a, s) = (t.i(2), t.i(3));
                    t.emit(op::I_CONST, flg::VLD, a, kstr, 0);
                    t.emit(op::S_PERSIST, 0, s, a, cell);
                }
                _ => {
                    t.emit(op::F_CONST, 0, fs, kone, 0);
                }
            }
            o += 1;
        }
        let end = t.code.len() as u32;
        t.patch(skip, end);
        t.emit(op::HALT, 0, 0, 0, 0);
        let p = t.build();
        let v = verify(&p, &TABLE).unwrap_or_else(|e| panic!("case {case}: {e:?}"));
        let mut ar = Arena::new(p.layout.total_bytes as usize);
        let mut vm = VmInstance::new(v, ar.slice());
        let mut ctx = Ctx { now: 0 };
        let mut b = 0u32;
        while b < 4 {
            assert_eq!(vm.step_bar(&bar(b, 1.0 + b as f64, barflag::CONFIRMED), &mut ctx), StepOutcome::Ok);
            let checkpoint = image(&vm, &p);
            assert_eq!(vm.step_tick(&bar(b + 1, 50.0, barflag::REALTIME), &mut ctx), StepOutcome::Ok); // writes
            assert_eq!(vm.step_tick(&bar(b + 1, 60.0, barflag::REALTIME), &mut ctx), StepOutcome::Ok); // undo, no writes
            let after = image(&vm, &p);
            // varip words may legitimately differ (they are never rolled back): mask them.
            let l = &p.layout;
            let mut diff = Vec::new();
            let mut x = 0usize;
            while x < checkpoint.len() {
                if checkpoint[x] != after[x] {
                    diff.push(x);
                }
                x += 1;
            }
            let mut y = 0usize;
            while y < diff.len() {
                let off = diff[y] as u32;
                let varip_f = off >= l.f64_off + 8 * l.f64_varip_from && off < l.f64_off + 8 * l.f64_len;
                let varip_i = off >= l.i64_off + 8 * l.i64_varip_from && off < l.i64_off + 8 * l.i64_len;
                let varip_v = l.i64_varip_from < l.i64_len
                    && off >= l.valid_off + 8 * ((l.f64_len + l.i64_varip_from) / 64)
                    && off < l.valid_off + 8 * l.valid_words;
                assert!(varip_f || varip_i || varip_v, "case {case} bar {b}: byte {off} not restored");
                y += 1;
            }
            b += 1;
        }
        case += 1;
    }
}

#[test]
fn varip_string_cell_survives_a_rollback_and_a_plain_one_does_not() {
    let mut t = Tb::new(ExecMode::Realtime, 0, 4, 0, 1);
    t.cell_bytes = 16;
    t.scratch = 64;
    let c_plain = t.cell();
    let c_varip_rel = t.varip_cell();
    let c_varip = t.varip_cell_index(c_varip_rel);
    let (a, b, cat, plain, vs, n) = (t.i(0), t.i(1), t.i(2), t.i(3), t.iv(0), t.i(3));
    let _ = n;
    let kx = t.kstr("x");
    t.emit(op::I_CONST, flg::VLD, a, kx, 0);
    // plain := plain + "x"; vs := vs + "x"  (first bar: both start at "x")
    let j = t.jump(op::JNZ, 0, abi::I_BS_ISFIRST);
    let w1 = t.win(&[argwin::i64(plain), argwin::i64(a), argwin::i64(cat)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, w1);
    t.emit(op::S_PERSIST, 0, plain, cat, c_plain);
    let w2 = t.win(&[argwin::i64(vs), argwin::i64(a), argwin::i64(b)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, w2);
    t.emit(op::S_PERSIST, 0, vs, b, c_varip);
    let jend = t.jump(op::JMP, 0, 0);
    let first = t.code.len() as u32;
    t.patch(j, first);
    t.emit(op::S_PERSIST, 0, plain, a, c_plain);
    t.emit(op::S_PERSIST, 0, vs, a, c_varip);
    let end = t.code.len() as u32;
    t.patch(jend, end);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    let read = |vm: &VmInstance<'_>, slot: u16| -> usize {
        let l = &p.layout;
        let bits = i64::from_le_bytes(arena(vm)[(l.i64_off + 8 * slot as u32) as usize..][..8].try_into().unwrap());
        StrRef(bits as u64).len() as usize
    };
    assert_eq!(vm.step_bar(&bar(0, 1.0, 1), &mut ctx), StepOutcome::Ok);
    assert_eq!((read(&vm, plain), read(&vm, vs)), (1, 1));
    vm.step_tick(&bar(1, 1.0, barflag::REALTIME), &mut ctx);
    vm.step_tick(&bar(1, 1.0, barflag::REALTIME), &mut ctx);
    vm.step_tick(&bar(1, 1.0, barflag::REALTIME), &mut ctx);
    assert_eq!((read(&vm, plain), read(&vm, vs)), (2, 4), "plain rolled back per tick; varip grew");
    assert_eq!(vm.step_bar(&bar(1, 1.0, 1), &mut ctx), StepOutcome::Ok);
    assert_eq!((read(&vm, plain), read(&vm, vs)), (2, 5));
}

#[test]
fn collection_blocks_roll_back_per_tick() {
    let mut t = Tb::new(ExecMode::Realtime, 1, 4, 0, 0);
    t.n_plots = 1;
    let site = t.state(StateKind::Collection, 32 + 8 * 8);
    let (arr, zero, last, ix) = (t.i(0), t.i(1), t.i(2), t.i(3));
    let (f0,) = (t.f(0),);
    let kz = t.k(0);
    t.emit(op::I_CONST, flg::VLD, zero, kz, 0);
    let j = t.jump(op::JZ, 0, abi::I_BS_ISFIRST);
    let wn = t.win(&[argwin::i64(zero), argwin::i64(arr)]);
    t.emit(op::CALL_BUILTIN, 0, site, ARR_NEW, wn);
    let here = t.code.len() as u32;
    t.patch(j, here);
    let wp = t.win(&[argwin::i64(arr), argwin::f64(abi::F_CLOSE)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, ARR_PUSH, wp);
    // last = bar_index  (the element index just pushed: one push per confirmed bar + one per tick)
    t.emit(op::I_MOV, flg::VLD, last, abi::I_BAR_INDEX, 0);
    let wg = t.win(&[argwin::i64(arr), argwin::i64(last), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, ARR_GET, wg);
    let kp = t.k(0);
    t.emit(op::I_CONST, flg::VLD, ix, kp, 0);
    let wpl = t.win(&[argwin::i64(ix), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PLOT, wpl);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_bar(&bar(0, 10.0, 1), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots()[0], 10.0);
    // In a tick bar_index = 1: the tick's push lands at index 1; the rollback removes it.
    assert_eq!(vm.step_tick(&bar(1, 20.0, barflag::REALTIME), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots()[0], 20.0);
    assert_eq!(vm.step_tick(&bar(1, 30.0, barflag::REALTIME), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots()[0], 30.0, "the previous tick's push was rolled back (else index 1 = 20)");
    assert_eq!(vm.step_bar(&bar(1, 40.0, 1), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots()[0], 40.0);
}

fn barclose_vm_tick() -> (StepOutcome, Option<(FaultKind, u32)>, StepOutcome) {
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).unwrap();
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    let o = vm.step_tick(&bar(0, 1.0, 0), &mut ctx);
    let lf = vm.last_fault();
    let next = vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx);
    (o, lf, next)
}

#[cfg(not(debug_assertions))]
#[test]
fn step_tick_on_a_barclose_program_is_internal_and_latches() {
    let (o, lf, next) = barclose_vm_tick();
    assert_eq!(o, StepOutcome::Fault(FaultKind::Internal));
    assert_eq!(lf, Some((FaultKind::Internal, u32::MAX)));
    assert_eq!(next, StepOutcome::Fault(FaultKind::Internal), "latched");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "step_tick on a BarClose program")]
fn step_tick_on_a_barclose_program_is_internal_and_latches() {
    let _ = barclose_vm_tick();
}
