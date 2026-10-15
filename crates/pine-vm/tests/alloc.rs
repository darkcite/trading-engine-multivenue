// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

use core_alloc::{AllocGuard, CountingAllocator};
use pine_ir::*;
use pine_vm::*;
use pine_vm::testkit::stub::*;
use pine_vm::testkit::*;

// The workspace's counting allocator: it does not count libtest's runner
// (the main thread), whose bookkeeping right after spawning a test thread
// would otherwise land inside the window under CPU load (HK14, A-S7-101).
#[global_allocator]
static G: CountingAllocator = CountingAllocator::new();

#[test]
fn new_step_bar_and_step_tick_allocate_nothing() {
    let mut t = Tb::new(ExecMode::Realtime, 3, 6, 1, 0);
    t.n_plots = 1;
    t.intent_cap = 1;
    let rf = t.ring(abi::F_CLOSE, Lane::F64, 0, 4);
    let acc = t.state(StateKind::Builtin, 8);
    let obj = t.state(StateKind::Object, 48);
    let cell = t.cell();
    let (f0, f1, i1, i2, i3, s) = (t.f(0), t.f(1), t.i(1), t.i(2), t.i(3), t.i(4));
    let ka = t.kstr("hi");
    let w = t.win(&[argwin::f64(abi::F_CLOSE), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, acc, ACC, w);
    t.emit(op::HIST_F, 0, f1, rf, 1);
    t.emit(op::O_NEW, 0, i1, obj, 2);
    t.emit(op::O_SET_F, 0, i1, 0, f1);
    t.emit(op::I_CONST, flg::VLD, i2, ka, 0);
    let wc = t.win(&[argwin::i64(i2), argwin::i64(i2), argwin::i64(i3)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, wc);
    t.emit(op::S_PERSIST, 0, s, i3, cell);
    let we = t.win(&[argwin::f64(abi::F_CLOSE)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, ENTRY, we);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let bars: Vec<Bar> = (0..200).map(|i| bar(i, 1.0 + i as f64, barflag::CONFIRMED)).collect();
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
    assert_eq!(allocs, 0, "new/step_bar/step_tick/reset allocated {allocs} times / {bytes} B");
}
