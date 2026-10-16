// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Amendment A7 (contract §11.10): `Frame::state_and_str` and `Frame::str_alloc_with`.

use pine_ir::*;
use pine_vm::testkit::*;
use pine_vm::*;

/// SACC — STATEFUL `[s, out_f]`: the block (f64) += len(s), or += 1000 when s is na; out = block.
fn sacc(fr: &mut Frame<'_>, site: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    let (st, s) = fr.state_and_str::<f64>(site, a[0]);
    *st += match s {
        Some(b) => b.len() as f64,
        None => 1000.0,
    };
    let v = *st;
    fr.set_f64_arg(a[1], v);
    Ok(())
}

/// UPCAT — STR_ALLOC `[x, y, len, out]`: ASCII-upper(x) ++ ASCII-upper(y) written straight into a
/// fresh `len`-byte range (an na source writes `~`), the rest padded with `.`.
fn upcat(fr: &mut Frame<'_>, _: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    let n = fr.i64_arg(a[2]).unwrap_or(0) as u32;
    let (r, dst, [x, y]) = fr.str_alloc_with(n, [a[0], a[1]])?;
    let mut w = 0usize;
    for src in [x, y] {
        match src {
            Some(b) => {
                for &c in b {
                    dst[w] = c.to_ascii_uppercase();
                    w += 1;
                }
            }
            None => {
                dst[w] = b'~';
                w += 1;
            }
        }
    }
    while w < dst.len() {
        dst[w] = b'.';
        w += 1;
    }
    fr.set_i64_arg(a[3], Some(r.0 as i64));
    Ok(())
}

/// PROBE — STR_ALLOC `[x, big, fit, out_f]`: a `big` lend must be `StrArenaFull`, and a `fit` lend
/// right after it must still succeed from the unmoved cursor; out = 1.
fn probe(fr: &mut Frame<'_>, _: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    let big = fr.i64_arg(a[1]).unwrap_or(0) as u32;
    let fit = fr.i64_arg(a[2]).unwrap_or(0) as u32;
    match fr.str_alloc_with(big, [a[0]]) {
        Err(FaultKind::StrArenaFull) => {}
        _ => return Err(FaultKind::Internal),
    }
    let (_, dst, [x]) = fr.str_alloc_with(fit, [a[0]])?;
    if dst.len() != fit as usize || x != Some(&b"ab"[..]) {
        return Err(FaultKind::Internal);
    }
    fr.set_f64_arg(a[3], 1.0);
    Ok(())
}

/// EQ — `[x, y, out_f]`: 1 when the bytes match (na == na), else 0.
fn eq(fr: &mut Frame<'_>, _: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    let v = (fr.str_arg(a[0]) == fr.str_arg(a[1])) as u8 as f64;
    fr.set_f64_arg(a[2], v);
    Ok(())
}

/// FAIL — `[out_f]`: `Err(ScriptRefused)` on a tick (after an earlier call of the same evaluation
/// mutated its block), else out = 1.
fn fail(fr: &mut Frame<'_>, _: SiteId, a: &[u16]) -> Result<(), FaultKind> {
    if fr.is_tick() {
        return Err(FaultKind::ScriptRefused);
    }
    fr.set_f64_arg(a[0], 1.0);
    Ok(())
}

static ENTRIES: [BuiltinEntry; 7] = [
    BuiltinEntry {
        f: stub::plot,
        cost: 1,
        n_out: 0,
        flags: bf::CAPTURE,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: stub::concat,
        cost: 1,
        n_out: 1,
        flags: bf::STR_ALLOC,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: sacc,
        cost: 1,
        n_out: 1,
        flags: bf::STATEFUL,
        state_bytes_min: 8,
    },
    BuiltinEntry {
        f: upcat,
        cost: 1,
        n_out: 1,
        flags: bf::STR_ALLOC,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: probe,
        cost: 1,
        n_out: 1,
        flags: bf::STR_ALLOC,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: eq,
        cost: 1,
        n_out: 1,
        flags: 0,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: fail,
        cost: 1,
        n_out: 1,
        flags: 0,
        state_bytes_min: 0,
    },
];
static TABLE: BuiltinTable = BuiltinTable {
    spec_sha256: [7; 32],
    entries: &ENTRIES,
};
const PLOT: u16 = 0;
const CONCAT: u16 = 1;
const SACC: u16 = 2;
const UPCAT: u16 = 3;
const PROBE: u16 = 4;
const EQ: u16 = 5;
const FAIL: u16 = 6;

fn plot_f(t: &mut Tb, idx: u64, s: u16, iscratch: u16) {
    let k = t.k(idx);
    t.emit(op::I_CONST, flg::VLD, iscratch, k, 0);
    let w = t.win(&[argwin::i64(iscratch), argwin::f64(s)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PLOT, w);
}

fn konst_str(t: &mut Tb, slot: u16, s: &str) {
    let k = t.kstr(s);
    t.emit(op::I_CONST, flg::VLD, slot, k, 0);
}

fn konst_int(t: &mut Tb, slot: u16, v: i64) {
    let k = t.k(v as u64);
    t.emit(op::I_CONST, flg::VLD, slot, k, 0);
}

#[test]
fn state_and_str_lends_block_and_scratch_string_and_rolls_back_like_state() {
    // s = "ab" + "c" (SCRATCH); f0 = sacc(s) (block += 3); f1 = upcat(s, "x", 4) == "ABCX".
    let mut t = Tb::new(ExecMode::Realtime, 2, 8, 0, 0);
    t.n_plots = 2;
    let site = t.state(StateKind::Builtin, 8);
    let (f0, f1) = (t.f(0), t.f(1));
    let (a, b, s, x, n, up, want, ip) = (
        t.i(0),
        t.i(1),
        t.i(2),
        t.i(3),
        t.i(4),
        t.i(5),
        t.i(6),
        t.i(7),
    );
    konst_str(&mut t, a, "ab");
    konst_str(&mut t, b, "c");
    let w = t.win(&[argwin::i64(a), argwin::i64(b), argwin::i64(s)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, w);
    let w = t.win(&[argwin::i64(s), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, site, SACC, w);
    konst_str(&mut t, x, "x");
    konst_int(&mut t, n, 4);
    let w = t.win(&[
        argwin::i64(s),
        argwin::i64(x),
        argwin::i64(n),
        argwin::i64(up),
    ]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, UPCAT, w);
    konst_str(&mut t, want, "ABCX");
    let w = t.win(&[argwin::i64(up), argwin::i64(want), argwin::f64(f1)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, EQ, w);
    plot_f(&mut t, 0, f0, ip);
    plot_f(&mut t, 1, f1, ip);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(
        vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx),
        StepOutcome::Ok
    );
    assert_eq!(vm.plots(), &[3.0, 1.0]);
    assert_eq!(
        vm.step_tick(&bar(1, 1.0, barflag::REALTIME), &mut ctx),
        StepOutcome::Ok
    );
    assert_eq!(vm.plots(), &[6.0, 1.0]);
    assert_eq!(
        vm.step_tick(&bar(1, 1.0, barflag::REALTIME), &mut ctx),
        StepOutcome::Ok
    );
    assert_eq!(
        vm.plots(),
        &[6.0, 1.0],
        "the block was journaled by state_and_str and rolled back"
    );
    assert_eq!(
        vm.step_bar(
            &bar(1, 1.0, barflag::CONFIRMED | barflag::REALTIME),
            &mut ctx
        ),
        StepOutcome::Ok
    );
    assert_eq!(
        vm.plots(),
        &[6.0, 1.0],
        "the bar close undoes the last tick"
    );
    assert_eq!(
        vm.step_bar(&bar(2, 1.0, barflag::CONFIRMED), &mut ctx),
        StepOutcome::Ok
    );
    assert_eq!(vm.plots(), &[9.0, 1.0]);
}

#[test]
fn state_and_str_na_string_is_none_and_the_block_is_still_lent() {
    let mut t = Tb::new(ExecMode::BarClose, 1, 2, 0, 0);
    t.n_plots = 1;
    let site = t.state(StateKind::Builtin, 8);
    let (f0, na, ip) = (t.f(0), t.i(0), t.i(1));
    let w = t.win(&[argwin::i64(na), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, site, SACC, w);
    plot_f(&mut t, 0, f0, ip);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(
        vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx),
        StepOutcome::Ok
    );
    assert_eq!(vm.plots(), &[1000.0]);
    assert_eq!(
        vm.step_bar(&bar(1, 1.0, barflag::CONFIRMED), &mut ctx),
        StepOutcome::Ok
    );
    assert_eq!(vm.plots(), &[2000.0]);
}

#[test]
fn str_alloc_with_na_source_is_none_and_output_is_written_in_place() {
    // upcat(na, "xy", 4) == "~XY."
    let mut t = Tb::new(ExecMode::BarClose, 1, 6, 0, 0);
    t.n_plots = 1;
    let (f0, na, y, n, up, want, ip) = (t.f(0), t.i(0), t.i(1), t.i(2), t.i(3), t.i(4), t.i(5));
    konst_str(&mut t, y, "xy");
    konst_int(&mut t, n, 4);
    let w = t.win(&[
        argwin::i64(na),
        argwin::i64(y),
        argwin::i64(n),
        argwin::i64(up),
    ]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, UPCAT, w);
    konst_str(&mut t, want, "~XY.");
    let w = t.win(&[argwin::i64(up), argwin::i64(want), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, EQ, w);
    plot_f(&mut t, 0, f0, ip);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(
        vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx),
        StepOutcome::Ok
    );
    assert_eq!(vm.plots(), &[1.0]);
}

#[test]
fn str_alloc_with_full_arena_is_strarenafull_and_leaves_the_cursor() {
    // scratch 64: s = "a" + "b" takes 2; a 63-byte lend fails, the 62 that remain still fit.
    let mut t = Tb::new(ExecMode::BarClose, 1, 6, 0, 0);
    t.scratch = 64;
    t.n_plots = 1;
    let (f0, a, b, s, big, fit) = (t.f(0), t.i(0), t.i(1), t.i(2), t.i(3), t.i(4));
    let ip = t.i(5);
    konst_str(&mut t, a, "a");
    konst_str(&mut t, b, "b");
    let w = t.win(&[argwin::i64(a), argwin::i64(b), argwin::i64(s)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, w);
    konst_int(&mut t, big, 63);
    konst_int(&mut t, fit, 62);
    let w = t.win(&[
        argwin::i64(s),
        argwin::i64(big),
        argwin::i64(fit),
        argwin::f64(f0),
    ]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PROBE, w);
    plot_f(&mut t, 0, f0, ip);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(
        vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx),
        StepOutcome::Ok
    );
    assert_eq!(
        vm.plots(),
        &[1.0],
        "the failed lend left the cursor where it was"
    );
}

#[test]
fn str_alloc_with_overlong_faults_the_step_strarenafull() {
    let mut t = Tb::new(ExecMode::BarClose, 0, 4, 0, 0);
    t.scratch = 64;
    let (x, y, n, up) = (t.i(0), t.i(1), t.i(2), t.i(3));
    konst_str(&mut t, x, "p");
    konst_str(&mut t, y, "q");
    konst_int(&mut t, n, 65);
    let w = t.win(&[
        argwin::i64(x),
        argwin::i64(y),
        argwin::i64(n),
        argwin::i64(up),
    ]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, UPCAT, w);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(
        vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx),
        StepOutcome::Fault(FaultKind::StrArenaFull)
    );
}

/// The f64 a Builtin block of `site` holds, read from the arena image.
fn block_f64(vm: &VmInstance<'_>, p: &PineProgram, site: u16) -> f64 {
    let off = p.layout.states[site as usize].off as usize;
    f64::from_le_bytes(arena(vm)[off..off + 8].try_into().unwrap())
}

/// A7.1 / A7.2 with a PERSIST source: `s = "ab" + "c"` is persisted on the first bar only, so on
/// bar 1 the slot names PERSIST bytes from an earlier evaluation — still valid (unlike SCRATCH).
/// `state_and_str` lends the block with them, and `str_alloc_with` lends them with the fresh
/// range (upcat(p, p, 6) == "ABCABC").
#[test]
fn a7_lends_a_persist_source_across_evaluations() {
    let mut t = Tb::new(ExecMode::BarClose, 2, 10, 0, 0);
    t.n_plots = 2;
    t.cell_bytes = 8;
    let site = t.state(StateKind::Builtin, 8);
    let cell = t.cell();
    let (f0, f1) = (t.f(0), t.f(1));
    let (a, b, cat, ps, n, up, want, ip) = (
        t.i(0),
        t.i(1),
        t.i(2),
        t.i(3),
        t.i(4),
        t.i(5),
        t.i(6),
        t.i(7),
    );
    let j = t.jump(op::JZ, 0, abi::I_BS_ISFIRST);
    konst_str(&mut t, a, "ab");
    konst_str(&mut t, b, "c");
    let w = t.win(&[argwin::i64(a), argwin::i64(b), argwin::i64(cat)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, w);
    t.emit(op::S_PERSIST, 0, ps, cat, cell);
    let here = t.code.len() as u32;
    t.patch(j, here);
    let w = t.win(&[argwin::i64(ps), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, site, SACC, w);
    konst_int(&mut t, n, 6);
    let w = t.win(&[
        argwin::i64(ps),
        argwin::i64(ps),
        argwin::i64(n),
        argwin::i64(up),
    ]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, UPCAT, w);
    konst_str(&mut t, want, "ABCABC");
    let w = t.win(&[argwin::i64(up), argwin::i64(want), argwin::f64(f1)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, EQ, w);
    plot_f(&mut t, 0, f0, ip);
    plot_f(&mut t, 1, f1, ip);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(vm.step_bar(&bar(0, 1.0, 1), &mut ctx), StepOutcome::Ok);
    assert_eq!(vm.plots(), &[3.0, 1.0]);
    assert_eq!(vm.step_bar(&bar(1, 1.0, 0), &mut ctx), StepOutcome::Ok);
    assert_eq!(
        vm.plots(),
        &[6.0, 1.0],
        "bar 1 reads the PERSIST bytes stored on bar 0"
    );
}

/// A stale SCRATCH source (a ref made on bar 0, read on bar 1 without being persisted) is never
/// lent: `state_and_str` and `str_alloc_with` resolve it as `None`, latch the defect flag, and
/// the call faults `Internal` when it returns (never silent garbage).
#[test]
fn a7_stale_scratch_source_latches_the_defect_and_faults_internal() {
    for via_state in [true, false] {
        let mut t = Tb::new(ExecMode::BarClose, 1, 6, 0, 0);
        let site = t.state(StateKind::Builtin, 8);
        let (f0, a, cat, n, up) = (t.f(0), t.i(0), t.i(1), t.i(2), t.i(3));
        let j = t.jump(op::JZ, 0, abi::I_BS_ISFIRST);
        konst_str(&mut t, a, "xy");
        let w = t.win(&[argwin::i64(a), argwin::i64(a), argwin::i64(cat)]);
        t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, CONCAT, w);
        let here = t.code.len() as u32;
        t.patch(j, here);
        if via_state {
            let w = t.win(&[argwin::i64(cat), argwin::f64(f0)]);
            t.emit(op::CALL_BUILTIN, 0, site, SACC, w);
        } else {
            konst_int(&mut t, n, 8); // bar 0: "XYXY" ++ "XYXY"
            let w = t.win(&[
                argwin::i64(cat),
                argwin::i64(cat),
                argwin::i64(n),
                argwin::i64(up),
            ]);
            t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, UPCAT, w);
        }
        t.emit(op::HALT, 0, 0, 0, 0);
        let p = t.build();
        let v = verify(&p, &TABLE).expect("verifies");
        let mut ar = Arena::new(p.layout.total_bytes as usize);
        let mut vm = VmInstance::new(v, ar.slice());
        let mut ctx = Ctx { now: 0 };
        assert_eq!(
            vm.step_bar(&bar(0, 1.0, 1), &mut ctx),
            StepOutcome::Ok,
            "bar 0: fresh SCRATCH (state={via_state})"
        );
        assert_eq!(
            vm.step_bar(&bar(1, 1.0, 0), &mut ctx),
            StepOutcome::Fault(FaultKind::Internal),
            "bar 1: stale SCRATCH (state={via_state})"
        );
        assert_eq!(
            vm.last_fault().map(|f| f.0),
            Some(FaultKind::Internal),
            "latched (state={via_state})"
        );
    }
}

/// A Realtime tick that faults AFTER `state_and_str` mutated the block: the step undoes to the
/// checkpoint, so the block holds the confirmed bar's value again (state_and_str journaled it on
/// first touch exactly as `state`).
#[test]
fn a7_tick_fault_after_state_and_str_restores_the_block() {
    let mut t = Tb::new(ExecMode::Realtime, 2, 2, 0, 0);
    t.n_plots = 1;
    let site = t.state(StateKind::Builtin, 8);
    let (f0, f1, s, ip) = (t.f(0), t.f(1), t.i(0), t.i(1));
    konst_str(&mut t, s, "abc");
    let w = t.win(&[argwin::i64(s), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, site, SACC, w);
    let w = t.win(&[argwin::f64(f1)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, FAIL, w);
    plot_f(&mut t, 0, f0, ip);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).expect("verifies");
    let mut ar = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, ar.slice());
    let mut ctx = Ctx { now: 0 };
    assert_eq!(
        vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut ctx),
        StepOutcome::Ok
    );
    assert_eq!(vm.plots(), &[3.0]);
    assert_eq!(block_f64(&vm, &p, site), 3.0);
    assert_eq!(
        vm.step_tick(&bar(1, 1.0, barflag::REALTIME), &mut ctx),
        StepOutcome::Fault(FaultKind::ScriptRefused),
        "the tick faults after sacc added 3"
    );
    assert_eq!(
        block_f64(&vm, &p, site),
        3.0,
        "the journal restored the block state_and_str lent"
    );
}
