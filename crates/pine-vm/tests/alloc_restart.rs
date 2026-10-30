// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! CAR-R33-9 (r33i): the `RESTART` prologue allocates nothing. Its own test binary: the counting
//! allocator counts every thread but libtest's runner, so it must not share a binary (and a
//! parallel window) with `alloc.rs`.

use core_alloc::{AllocGuard, CountingAllocator};
use pine_ir::*;
use pine_vm::testkit::stub::*;
use pine_vm::testkit::*;
use pine_vm::*;

#[global_allocator]
static G: CountingAllocator = CountingAllocator::new();

/// CAR-R33-9: the `RESTART` prologue (pre-image save / restore, stamp, the tick journal of the
/// whole block) allocates nothing — three in-loop executions per bar and per tick.
#[test]
fn restart_site_in_a_loop_allocates_nothing() {
    let mut t = Tb::new(ExecMode::Realtime, 3, 4, 0, 0);
    let site = t.state_with(
        StateKind::Builtin,
        24,
        state_flag::IN_LOOP | state_flag::RESTART,
    );
    let (x, out, sum) = (t.f(0), t.f(1), t.f(2));
    let (i, n, one, c) = (t.i(0), t.i(1), t.i(2), t.i(3));
    let (k0, k1, k3) = (t.k(0), t.k(1), t.k(3));
    t.emit(op::I_CONST, flg::VLD, i, k0, 0);
    t.emit(op::I_CONST, flg::VLD, n, k3, 0);
    t.emit(op::I_CONST, flg::VLD, one, k1, 0);
    let top = t.code.len() as u32;
    t.emit(op::I_LT, 0, c, i, n);
    let jend = t.jump(op::JZ, 0, c);
    t.emit(op::F_I2F, 0, x, i, 0);
    t.emit(op::F_ADD, 0, x, x, abi::F_CLOSE);
    let w = t.win(&[argwin::f64(x), argwin::f64(out)]);
    t.emit(op::CALL_BUILTIN, 0, site, ACC, w);
    t.emit(op::F_ADD, 0, sum, sum, out);
    t.emit(op::I_ADD, 0, i, i, one);
    t.jump(op::JMP, top, 0);
    let end = t.code.len() as u32;
    t.patch(jend, end);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let bars: Vec<Bar> = (0..200)
        .map(|i| bar(i, 1.0 + i as f64, barflag::CONFIRMED))
        .collect();
    let tick = bar(0, 3.0, barflag::REALTIME);
    let mut ctx = Ctx { now: 0 };
    let guard = AllocGuard::new();
    let mut vm = VmInstance::new(v, ar.slice());
    let mut k = 0;
    while k < bars.len() {
        assert_eq!(vm.step_bar(&bars[k], &mut ctx), StepOutcome::Ok);
        let mut j = 0;
        while j < 8 {
            assert_eq!(vm.step_tick(&tick, &mut ctx), StepOutcome::Ok);
            j += 1;
        }
        k += 1;
    }
    vm.reset();
    let (allocs, bytes, _) = guard.delta();
    assert_eq!(
        allocs, 0,
        "restart: new/step_bar/step_tick/reset allocated {allocs} times / {bytes} B"
    );
}
