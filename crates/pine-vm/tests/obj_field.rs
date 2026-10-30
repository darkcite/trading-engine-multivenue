// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! CAR-R27-1 (R-P5): `Frame::obj_field` — one field word of a UDT `Object` block, read-only,
//! with `object::o_field`'s checks: a valid read; a stale generation and a `Collection` reference
//! refuse (`ScriptRefused`, as `O_GET_*`); `field == len` is `Internal` (sema bounds the selector).
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_vm::testkit::stub::{arr_new, plot};
use pine_vm::testkit::*;
use pine_vm::*;

/// `ofield(handle, field) -> f64`: the field word read through `Frame::obj_field`, as f64 bits.
fn ofield(fr: &mut Frame<'_>, _site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    let r = CollRef(fr.i64_arg(a[0]).unwrap_or(0) as u64);
    let k = u32::try_from(fr.i64_arg(a[1]).unwrap_or(-1)).unwrap_or(u32::MAX);
    let w = fr.obj_field(r, k)?;
    fr.set_f64_arg(a[2], f64::from_bits(w));
    Ok(())
}

const PLOT: u16 = 0;
const OFIELD: u16 = 1;
const ARR_NEW: u16 = 2;

static ENTRIES: [BuiltinEntry; 3] = [
    BuiltinEntry {
        f: plot,
        cost: 1,
        n_out: 0,
        flags: bf::CAPTURE,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: ofield,
        cost: 1,
        n_out: 1,
        flags: 0,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: arr_new,
        cost: 1,
        n_out: 1,
        flags: bf::COLLECTION | bf::POOL,
        state_bytes_min: 0,
    },
];
static ARITY: [[u8; 2]; 3] = [[2, 2], [2, 2], [1, 1]];
static TABLE: BuiltinTable = BuiltinTable {
    spec_sha256: [7; 32],
    entries: &ENTRIES,
    arity: &ARITY,
};

/// Bar 0 creates a two-field object (field 1 = close) and reads `field` of it — or, with
/// `stale`, of the handle kept from the previous bar (bar 1 onwards) — or, with `coll`, of a
/// float array's reference; plot 0 is the word read.
fn program(field: u64, stale: bool, coll: bool) -> PineProgram {
    let mut t = Tb::new(ExecMode::BarClose, 1, 5, 0, 0);
    t.n_plots = 1;
    let obj = t.state(StateKind::Object, 32 + 16);
    let arr = t.state(StateKind::Collection, 32 + 8 * 4);
    let (h, h_old, ix, kf, a) = (t.i(0), t.i(1), t.i(2), t.i(3), t.i(4));
    let f0 = t.f(0);
    t.emit(op::O_NEW, 0, h, obj, 2);
    t.emit(op::O_SET_F, 0, h, 1, abi::F_CLOSE);
    let kk = t.k(field);
    t.emit(op::I_CONST, flg::VLD, kf, kk, 0);
    let src = if coll {
        let k0 = t.k(0);
        t.emit(op::I_CONST, flg::VLD, ix, k0, 0);
        let wn = t.win(&[argwin::i64(ix), argwin::i64(a)]);
        t.emit(op::CALL_BUILTIN, 0, arr, ARR_NEW, wn);
        a
    } else if stale {
        h_old
    } else {
        h
    };
    // `stale` reads only from bar 1 (bar 0 has no previous handle).
    let j = if stale {
        Some(t.jump(op::JNZ, 0, abi::I_BS_ISFIRST))
    } else {
        None
    };
    let w = t.win(&[argwin::i64(src), argwin::i64(kf), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, OFIELD, w);
    if let Some(j) = j {
        let here = t.code.len() as u32;
        t.patch(j, here);
    }
    let kp = t.k(0);
    t.emit(op::I_CONST, flg::VLD, ix, kp, 0);
    let wp = t.win(&[argwin::i64(ix), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PLOT, wp);
    t.emit(op::I_MOV, flg::VLD, h_old, h, 0);
    t.emit(op::HALT, 0, 0, 0, 0);
    t.build()
}

fn run(p: &PineProgram, closes: &[f64]) -> (Vec<StepOutcome>, f64) {
    let v = verify(p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    let mut out = Vec::new();
    for (i, c) in closes.iter().enumerate() {
        out.push(vm.step_bar(&bar(i as u32, *c, barflag::CONFIRMED), &mut ctx));
    }
    let last = vm.plots()[0];
    (out, last)
}

#[test]
fn obj_field_reads_a_field_word() {
    let (o, v) = run(&program(1, false, false), &[7.0, 9.5]);
    assert_eq!(o, [StepOutcome::Ok, StepOutcome::Ok]);
    assert_eq!(v, 9.5, "field 1 holds this bar's close");
}

#[test]
fn obj_field_refuses_a_stale_generation() {
    let (o, _) = run(&program(1, true, false), &[7.0, 9.5]);
    assert_eq!(
        o,
        [
            StepOutcome::Ok,
            StepOutcome::Fault(FaultKind::ScriptRefused)
        ]
    );
}

#[test]
fn obj_field_at_len_is_internal() {
    let (o, _) = run(&program(2, false, false), &[7.0]);
    assert_eq!(o, [StepOutcome::Fault(FaultKind::Internal)]);
}

#[test]
fn obj_field_refuses_a_collection_reference() {
    let (o, _) = run(&program(0, false, true), &[7.0]);
    assert_eq!(o, [StepOutcome::Fault(FaultKind::ScriptRefused)]);
}
