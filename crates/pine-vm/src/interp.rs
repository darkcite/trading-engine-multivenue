// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The dispatch loop. `J` = journaling (`step_tick`); `run::<false>` is the
//! bar path and contains no journal instruction. No closures, no iterators,
//! no `?`, no allocation; every unchecked access names its verifier rule.

use pine_ir::{flg, op, ring_flag, FaultKind, Insn, RingDesc, SiteId, StateDesc, NA_I64};

use crate::builtin::{bf, BuiltinEntry};
use crate::frame::{Core, Frame};
use crate::hist::{cell_ptr, hist_k};
use crate::object::{o_field, o_new};
use crate::strings::s_persist;

/// Raw views of the verified program the loop reads.
pub(crate) struct Exec {
    pub code: *const Insn,
    pub code_len: u32,
    pub consts: *const u64,
    pub argwin: *const u16,
    pub entries: *const BuiltinEntry,
    pub rings: *const RingDesc,
    pub states: *const StateDesc,
    pub head: u64,
    pub budget: u32,
    // lengths for the debug mirrors of rules 8 / 22 / 23 (release never reads them)
    pub consts_len: u32,
    pub argwin_len: u32,
    pub entries_len: u32,
    pub rings_len: u32,
    pub states_len: u32,
}

const TWO63: f64 = 9_223_372_036_854_775_808.0;

/// Run to `HALT`. `Ok(charged units)` or `Err((kind, pc))`.
#[inline(never)]
pub(crate) fn run<const J: bool>(fr: &mut Frame<'_>, x: &Exec) -> Result<u32, (FaultKind, u32)> {
    let mut budget: u32 = x.budget;
    let mut pc: u32 = 0;

    macro_rules! charge {
        ($here:expr, $n:expr) => {{
            let n: u32 = $n;
            if n >= budget {
                return Err((FaultKind::InsnBudget, $here));
            }
            budget -= n;
        }};
    }
    macro_rules! jump {
        ($here:expr, $t:expr) => {{
            let t: u32 = $t;
            debug_assert!(t < x.code_len, "rule 9");
            if t <= $here {
                charge!($here, $here - t + 1);
            }
            pc = t;
        }};
    }

    loop {
        debug_assert!(pc < x.code_len, "rules 9/10: pc {pc} out of range");
        // SAFETY: rules 9 + 10 — every target < code.len() and the last insn never falls through.
        let ins = unsafe { *x.code.add(pc as usize) };
        let here = pc;
        pc += 1;
        let c: &mut Core = &mut *fr.core;
        // SAFETY (every arm): rule 8 bounds each operand for its role — lane slots below
        // their lane length (written slots at or above the reserved ABI), const indices below
        // consts.len(), ring indices below rings.len(), validity indices in the i64 range —
        // and rule 7 makes the default arm unreachable.
        unsafe {
            match ins.op {
                op::NOP => {}
                op::HALT => return Ok(x.budget - budget),
                op::TRAP => return Err((FaultKind::Internal, here)),
                op::BUDGET => charge!(here, ((ins.b as u32) << 16) | ins.a as u32),

                // ---- family 1a: f64 ----------------------------------------------------
                op::F_MOV => {
                    let v = c.rf(ins.a);
                    c.wf::<J>(ins.dst, v)
                }
                op::F_CONST => {
                    debug_assert!((ins.a as u32) < x.consts_len, "rule 8");
                    let v = f64::from_bits(*x.consts.add(ins.a as usize));
                    c.wf::<J>(ins.dst, v)
                }
                op::F_ADD => {
                    let v = c.rf(ins.a) + c.rf(ins.b);
                    c.wf::<J>(ins.dst, v)
                }
                op::F_SUB => {
                    let v = c.rf(ins.a) - c.rf(ins.b);
                    c.wf::<J>(ins.dst, v)
                }
                op::F_MUL => {
                    let v = c.rf(ins.a) * c.rf(ins.b);
                    c.wf::<J>(ins.dst, v)
                }
                op::F_DIV | op::F_MOD | op::F_POW => {
                    let (a, b) = (c.rf(ins.a), c.rf(ins.b));
                    let r = match ins.op {
                        op::F_DIV => a / b,
                        // Floored, the divisor's sign (CAR-06-1): 00-INDEX §3.6.1's expression
                        // evaluated as written — never fmod (`a % b`) plus an adjustment.
                        op::F_MOD => a - b * (a / b).floor(),
                        _ => a.powf(b),
                    };
                    let v = if ins.flags & flg::NA_ON_NONFINITE != 0 && !r.is_finite() { f64::NAN } else { r };
                    c.wf::<J>(ins.dst, v)
                }
                op::F_NEG => {
                    let v = -c.rf(ins.a);
                    c.wf::<J>(ins.dst, v)
                }
                op::F_MIN | op::F_MAX => {
                    let (a, b) = (c.rf(ins.a), c.rf(ins.b));
                    // NaN-propagating, and deterministic on ±0 (never f64::min / f64::max).
                    let v = if a.is_nan() || b.is_nan() {
                        f64::NAN
                    } else if ins.op == op::F_MIN {
                        if b < a { b } else { a }
                    } else if b > a {
                        b
                    } else {
                        a
                    };
                    c.wf::<J>(ins.dst, v)
                }
                op::F_ABS => {
                    let v = c.rf(ins.a).abs();
                    c.wf::<J>(ins.dst, v)
                }
                op::F_SQRT => {
                    let v = c.rf(ins.a).sqrt();
                    c.wf::<J>(ins.dst, v)
                }
                op::F_I2F => {
                    let v = if ins.flags & flg::VLD != 0 && !c.ivalid(ins.a) { f64::NAN } else { c.ri(ins.a) as f64 };
                    c.wf::<J>(ins.dst, v)
                }
                op::F_SEL => {
                    let v = if c.ri(ins.b) != 0 { c.rf(ins.a) } else { c.rf(ins.dst) };
                    c.wf::<J>(ins.dst, v)
                }
                op::F_FINITE => {
                    let a = c.rf(ins.a);
                    c.wf::<J>(ins.dst, if a.is_finite() { a } else { f64::NAN })
                }

                // ---- family 1b: i64 / bool -----------------------------------------------
                op::I_MOV => {
                    let v = c.ri(ins.a);
                    c.wi::<J>(ins.dst, v);
                    if ins.flags & flg::VLD != 0 {
                        let ok = c.ivalid(ins.a);
                        c.vput::<J>(c.vbit(ins.dst), ok);
                    }
                }
                op::I_CONST => {
                    debug_assert!((ins.a as u32) < x.consts_len, "rule 8");
                    let v = *x.consts.add(ins.a as usize) as i64;
                    c.wi::<J>(ins.dst, v);
                    if ins.flags & flg::VLD != 0 {
                        c.vput::<J>(c.vbit(ins.dst), true);
                    }
                }
                op::I_ADD | op::I_SUB | op::I_MUL | op::I_MIN | op::I_MAX | op::B_AND | op::B_OR => {
                    let (a, b) = (c.ri(ins.a), c.ri(ins.b));
                    let v = match ins.op {
                        op::I_ADD => a.wrapping_add(b),
                        op::I_SUB => a.wrapping_sub(b),
                        op::I_MUL => a.wrapping_mul(b),
                        op::I_MIN => a.min(b),
                        op::I_MAX => a.max(b),
                        op::B_AND => ((a != 0) & (b != 0)) as i64,
                        _ => ((a != 0) | (b != 0)) as i64,
                    };
                    c.wi::<J>(ins.dst, v);
                    if ins.flags & flg::VLD != 0 {
                        let ok = c.ivalid(ins.a) & c.ivalid(ins.b);
                        c.vput::<J>(c.vbit(ins.dst), ok);
                    }
                }
                op::I_DIV | op::I_MOD => {
                    let (a, b) = (c.ri(ins.a), c.ri(ins.b));
                    let ops = ins.flags & flg::VLD == 0 || (c.ivalid(ins.a) & c.ivalid(ins.b));
                    // checked_*: no panic path in release (b == 0, MIN / -1).
                    let (v, bad) = if ins.op == op::I_DIV {
                        match a.checked_div(b) {
                            Some(q) => (q, false),
                            None => (0, true), // b == 0 or MIN / -1 -> na (B8)
                        }
                    } else {
                        // Floored (CAR-06-1): Rust's remainder has the dividend's sign; move it to
                        // the divisor's. None <=> b == 0 (na) or MIN % -1 (the true remainder, 0).
                        let r = a.checked_rem(b).unwrap_or(0);
                        let v = if (r != 0) & ((r ^ b) < 0) { r.wrapping_add(b) } else { r };
                        (v, b == 0)
                    };
                    if bad & ops {
                        c.na_arith += 1; // B8: an na-producing event, not a fault
                    }
                    c.wiv::<J>(ins.dst, v, ops & !bad);
                }
                op::I_NEG | op::I_ABS | op::B_NOT => {
                    let a = c.ri(ins.a);
                    let v = match ins.op {
                        op::I_NEG => a.wrapping_neg(),
                        op::I_ABS => a.wrapping_abs(),
                        _ => (a == 0) as i64,
                    };
                    c.wi::<J>(ins.dst, v);
                    if ins.flags & flg::VLD != 0 {
                        let ok = c.ivalid(ins.a);
                        c.vput::<J>(c.vbit(ins.dst), ok);
                    }
                }
                op::I_F2I => {
                    let a = c.rf(ins.a);
                    let r = if ins.flags & flg::ROUND != 0 { a.round() } else { a.trunc() };
                    let ok = r.is_finite() && r.abs() < TWO63;
                    if !ok && !a.is_nan() {
                        c.na_arith += 1; // a failed cast (±inf, out of range)
                    }
                    c.wiv::<J>(ins.dst, if ok { r as i64 } else { 0 }, ok);
                }
                op::I_SEL => {
                    let take = c.ri(ins.b) != 0;
                    let v = if take { c.ri(ins.a) } else { c.ri(ins.dst) };
                    c.wi::<J>(ins.dst, v);
                    if ins.flags & flg::VLD != 0 {
                        let ok = if take { c.ivalid(ins.a) } else { c.ivalid(ins.dst) };
                        c.vput::<J>(c.vbit(ins.dst), ok);
                    }
                }

                // ---- family 2: compare ---------------------------------------------------
                op::F_LT | op::F_LE | op::F_GT | op::F_GE | op::F_EQ | op::F_NE => {
                    let (a, b) = (c.rf(ins.a), c.rf(ins.b));
                    let v = match ins.op {
                        op::F_LT => a < b,
                        op::F_LE => a <= b,
                        op::F_GT => a > b,
                        op::F_GE => a >= b,
                        op::F_EQ => a == b,
                        _ => !a.is_nan() & !b.is_nan() & (a != b), // Pine: false when either is na
                    };
                    c.wiv::<J>(ins.dst, v as i64, true);
                }
                op::I_LT | op::I_LE | op::I_GT | op::I_GE | op::I_EQ | op::I_NE => {
                    let (a, b) = (c.ri(ins.a), c.ri(ins.b));
                    let both = ins.flags & flg::NA_AWARE == 0 || (c.ivalid(ins.a) & c.ivalid(ins.b));
                    let v = match ins.op {
                        op::I_LT => a < b,
                        op::I_LE => a <= b,
                        op::I_GT => a > b,
                        op::I_GE => a >= b,
                        op::I_EQ => a == b,
                        _ => a != b,
                    };
                    c.wiv::<J>(ins.dst, (v & both) as i64, true);
                }

                // ---- family 3: na ---------------------------------------------------------
                op::NA_CHECK => {
                    let v = if ins.flags & flg::LANE_I != 0 { !c.ivalid(ins.a) } else { c.rf(ins.a).is_nan() };
                    c.wiv::<J>(ins.dst, v as i64, true);
                }
                op::NA_SET => {
                    if ins.flags & flg::LANE_I != 0 {
                        c.vput::<J>(c.vbit(ins.a), false);
                    } else {
                        c.wf::<J>(ins.a, f64::NAN);
                    }
                }
                op::NA_CLEAR => c.vput::<J>(c.vbit(ins.a), true),
                op::NZ_F => {
                    let a = c.rf(ins.a);
                    let v = if a.is_nan() { c.rf(ins.b) } else { a };
                    c.wf::<J>(ins.dst, v)
                }
                op::NZ_I => {
                    let v = if c.ivalid(ins.a) { c.ri(ins.a) } else { c.ri(ins.b) };
                    c.wiv::<J>(ins.dst, v, true);
                }
                op::VLD_AND => {
                    let ok = c.vget(ins.a as u32) & c.vget(ins.b as u32);
                    c.vput::<J>(ins.dst as u32, ok)
                }
                op::VLD_MOV => {
                    let ok = c.vget(ins.a as u32);
                    c.vput::<J>(ins.dst as u32, ok)
                }
                op::VLD_FROM_F => {
                    let ok = !c.rf(ins.a).is_nan();
                    c.vput::<J>(ins.dst as u32, ok)
                }

                // ---- family 4: history ------------------------------------------------------
                op::HIST_F => {
                    debug_assert!((ins.a as u32) < x.rings_len, "rule 8");
                    let r = *x.rings.add(ins.a as usize);
                    let (k, inr) = hist_k(c, ins, x.consts, x.consts_len, r.mask);
                    let v = if !inr {
                        f64::NAN
                    } else if k == 0 {
                        c.rf(r.slot)
                    } else {
                        // SAFETY: rule 8 — `a` < rings.len(), so `r` is a rule-4 ring.
                        f64::from_bits(*cell_ptr(c, &r, x.head, k))
                    };
                    c.wf::<J>(ins.dst, v)
                }
                op::HIST_I => {
                    debug_assert!((ins.a as u32) < x.rings_len, "rule 8");
                    let r = *x.rings.add(ins.a as usize);
                    let (k, inr) = hist_k(c, ins, x.consts, x.consts_len, r.mask);
                    let (v, ok) = if !inr {
                        if r.flags & ring_flag::INIT_FALSE != 0 { (0, true) } else { (NA_I64, false) }
                    } else if k == 0 {
                        (c.ri(r.slot), c.ivalid(r.slot))
                    } else {
                        // SAFETY: rule 8 — `a` < rings.len(), so `r` is a rule-4 ring.
                        let w = *cell_ptr(c, &r, x.head, k) as i64;
                        (w, !(ins.flags & flg::NA_TRACKED != 0 && w == NA_I64))
                    };
                    c.wiv::<J>(ins.dst, v, ok);
                }

                // ---- family 5: control flow ------------------------------------------------
                op::JMP => jump!(here, ins.target()),
                op::JZ => {
                    if c.ri(ins.b) == 0 {
                        jump!(here, ins.target())
                    }
                }
                op::JNZ => {
                    if c.ri(ins.b) != 0 {
                        jump!(here, ins.target())
                    }
                }
                op::JNA_F => {
                    if c.rf(ins.b).is_nan() {
                        jump!(here, ins.target())
                    }
                }
                op::JNA_I => {
                    if !c.vget(ins.b as u32) {
                        jump!(here, ins.target())
                    }
                }
                op::CALL => {
                    c.wi::<J>(ins.b, here as i64 + 1);
                    // A backward CALL is a back-edge like any jump (termination: the budget must
                    // bound every loop, including one closed by CALL).
                    let t = ins.target();
                    if t <= here {
                        jump!(here, t)
                    } else {
                        charge!(here, 1);
                        pc = t;
                    }
                }
                op::RET => {
                    let t = c.ri(ins.b);
                    if t < 0 || t >= x.code_len as i64 {
                        return Err((FaultKind::Internal, here));
                    }
                    jump!(here, t as u32) // a backward return is a back-edge (termination)
                }

                // ---- family 6: builtin call ---------------------------------------------------
                op::CALL_BUILTIN => {
                    // SAFETY: rule 8 — a < entries.len(); rule 22 — the window is inside argwin;
                    // rule 23 — dst is NONE or the entry's verified block.
                    debug_assert!((ins.a as u32) < x.entries_len, "rule 8");
                    let e = *x.entries.add(ins.a as usize);
                    charge!(here, e.cost as u32);
                    let w = ins.b as usize;
                    debug_assert!((w as u32) < x.argwin_len, "rule 22");
                    let n = *x.argwin.add(w) as usize;
                    debug_assert!(((w + n) as u32) < x.argwin_len, "rule 22");
                    let args = core::slice::from_raw_parts(x.argwin.add(w + 1), n);
                    if ins.dst != SiteId::NONE.0 {
                        debug_assert!((ins.dst as u32) < x.states_len, "rule 23");
                        let d = *x.states.add(ins.dst as usize);
                        c.cur_off = d.off;
                        c.cur_bytes = d.bytes;
                    } else {
                        c.cur_off = 0;
                        c.cur_bytes = 0;
                    }
                    c.cur_site = ins.dst;
                    c.cur_flags = e.flags;
                    let r = (e.f)(fr, SiteId(ins.dst), args);
                    if fr.core.defect.get() {
                        return Err((FaultKind::Internal, here));
                    }
                    if let Err(k) = r {
                        debug_assert!(flag_ok(e.flags, k), "builtin returned {k:?} its flags do not declare");
                        return Err((k, here));
                    }
                }

                // ---- family 7: UDT objects --------------------------------------------------
                op::O_NEW => {
                    if let Err(k) = o_new::<J>(c, ins.dst, ins.a, ins.b) {
                        return Err((k, here));
                    }
                }
                op::O_GET_F => match o_field(c, ins.a, ins.b) {
                    Ok((_, off)) => {
                        // SAFETY: rule 15 — o_field returned an offset inside the Object block.
                        let v = f64::from_bits(*(c.base.add(off as usize) as *const u64));
                        c.wf::<J>(ins.dst, v)
                    }
                    Err(k) => return Err((k, here)),
                },
                op::O_GET_I => match o_field(c, ins.a, ins.b) {
                    Ok((_, off)) => {
                        // SAFETY: rule 15 — o_field returned an offset inside the Object block.
                        let w = *(c.base.add(off as usize) as *const i64);
                        let ok = !(ins.flags & flg::NA_TRACKED != 0 && w == NA_I64);
                        c.wiv::<J>(ins.dst, w, ok);
                    }
                    Err(k) => return Err((k, here)),
                },
                op::O_SET_F | op::O_SET_I => match o_field(c, ins.dst, ins.a) {
                    Ok((d, off)) => {
                        let w = if ins.op == op::O_SET_F {
                            c.rf(ins.b).to_bits() as i64
                        } else if ins.flags & flg::NA_TRACKED != 0 && !c.ivalid(ins.b) {
                            NA_I64
                        } else {
                            c.ri(ins.b)
                        };
                        if J {
                            c.touch_block(d.off, d.bytes);
                        }
                        // SAFETY: rule 15 — o_field returned an offset inside the Object block.
                        *(c.base.add(off as usize) as *mut i64) = w;
                    }
                    Err(k) => return Err((k, here)),
                },

                // ---- family 8: strings --------------------------------------------------------
                op::S_PERSIST => {
                    if let Err(k) = s_persist::<J>(c, ins.dst, ins.a, ins.b) {
                        return Err((k, here));
                    }
                }

                _ => {
                    debug_assert!(false, "rule 7: undefined opcode {:#04x} at {here}", ins.op);
                    // SAFETY: rule 7 rejected every undefined opcode at load.
                    core::hint::unreachable_unchecked()
                }
            }
        }
    }
}

/// Debug: a builtin's error is one its flags declare.
fn flag_ok(flags: u8, k: FaultKind) -> bool {
    match k {
        FaultKind::StrArenaFull => flags & bf::STR_ALLOC != 0,
        FaultKind::PoolFull => flags & (bf::POOL | bf::INTENT) != 0,
        _ => true,
    }
}
