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

static ENTRIES: [BuiltinEntry; 6] = [
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
