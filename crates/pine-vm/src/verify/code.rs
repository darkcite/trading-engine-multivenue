// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Pass C — per instruction in pc order: 7, 8, 9, 13, 15 (O_NEW), 22, 23, 25.
//! Pass D — whole program: 10, 11, 12, 15 (fields), 17 (intents).

use pine_ir::{abi, argwin, flg, op, ring_flag, Insn, Lane, PineProgram, SiteId, StateKind};

use super::{e, rule, VerifyError, NO_PC};
use crate::builtin::{bf, BuiltinTable};
use crate::journal::in_string_ring;

/// Operand roles.
#[derive(Clone, Copy, PartialEq, Eq)]
enum R {
    /// Must be 0.
    Z,
    /// f64 slot read / written (written: >= RESERVED_F).
    Fr,
    Fw,
    /// i64 slot read / value-written (written: >= RESERVED_I).
    Ir,
    Iw,
    /// i64 slot whose validity only is written (>= RESERVED_I).
    Iv,
    /// Constant-pool index.
    C,
    /// Ring index (lane-checked against the opcode).
    Ring,
    /// History depth, per K_CONST / K_DYNAMIC.
    K,
    /// Validity index in the i64 range: read / written (written: >= f64_len + RESERVED_I).
    Vr,
    Vw,
    /// Jump target halves (rule 9 checks the target).
    Th,
    Tl,
    /// `NA_CHECK` / `NA_SET` operand: lane by LANE_I; read / written.
    Lr,
    Lw,
    /// Checked by their own rules.
    Site,
    Ovl,
    Aw,
    OSite,
    Nf,
    Fld,
    Cell,
    Any,
}

/// `(dst, a, b)` roles and the permitted flag bits (V8f).
fn roles(o: u8) -> ([R; 3], u8) {
    use R::*;
    let vld = flg::VLD;
    match o {
        op::NOP | op::HALT | op::TRAP => ([Z, Z, Z], 0),
        op::BUDGET => ([Z, Any, Any], 0),
        op::F_MOV => ([Fw, Fr, Z], 0),
        op::F_CONST => ([Fw, C, Z], 0),
        op::F_ADD | op::F_SUB | op::F_MUL | op::F_MIN | op::F_MAX => ([Fw, Fr, Fr], 0),
        op::F_DIV | op::F_MOD | op::F_POW => ([Fw, Fr, Fr], flg::NA_ON_NONFINITE),
        op::F_NEG | op::F_ABS | op::F_SQRT | op::F_FINITE => ([Fw, Fr, Z], 0),
        op::F_I2F => ([Fw, Ir, Z], vld),
        op::F_SEL => ([Fw, Fr, Ir], 0),
        op::I_MOV => ([Iw, Ir, Z], vld),
        op::I_CONST => ([Iw, C, Z], vld),
        op::I_ADD | op::I_SUB | op::I_MUL | op::I_DIV | op::I_MOD | op::I_MIN | op::I_MAX | op::B_AND | op::B_OR => {
            ([Iw, Ir, Ir], vld)
        }
        op::I_NEG | op::I_ABS | op::B_NOT => ([Iw, Ir, Z], vld),
        op::I_F2I => ([Iw, Fr, Z], flg::ROUND | vld),
        op::I_SEL => ([Iw, Ir, Ir], vld),
        op::F_LT..=op::F_NE => ([Iw, Fr, Fr], 0),
        op::I_LT..=op::I_NE => ([Iw, Ir, Ir], flg::NA_AWARE),
        op::NA_CHECK => ([Iw, Lr, Z], flg::LANE_I),
        op::NA_SET => ([Z, Lw, Z], flg::LANE_I),
        op::NA_CLEAR => ([Z, Iv, Z], 0),
        op::NZ_F => ([Fw, Fr, Fr], 0),
        op::NZ_I => ([Iw, Ir, Ir], 0),
        op::VLD_AND => ([Vw, Vr, Vr], 0),
        op::VLD_MOV => ([Vw, Vr, Z], 0),
        op::VLD_FROM_F => ([Vw, Fr, Z], 0),
        op::HIST_F => ([Fw, Ring, K], flg::K_CONST | flg::K_DYNAMIC),
        op::HIST_I => ([Iw, Ring, K], flg::K_CONST | flg::K_DYNAMIC | flg::NA_TRACKED),
        op::JMP => ([Th, Tl, Z], 0),
        op::JZ | op::JNZ => ([Th, Tl, Ir], 0),
        op::JNA_F => ([Th, Tl, Fr], 0),
        op::JNA_I => ([Th, Tl, Vr], 0),
        op::CALL => ([Th, Tl, Iw], 0),
        op::RET => ([Z, Z, Ir], 0),
        op::CALL_BUILTIN => ([Site, Ovl, Aw], 0),
        op::O_NEW => ([Iw, OSite, Nf], 0),
        op::O_GET_F => ([Fw, Ir, Fld], 0),
        op::O_GET_I => ([Iw, Ir, Fld], flg::NA_TRACKED),
        op::O_SET_F => ([Ir, Fld, Fr], 0),
        op::O_SET_I => ([Ir, Fld, Ir], flg::NA_TRACKED),
        op::S_PERSIST => ([Iw, Ir, Cell], 0),
        _ => ([Z, Z, Z], 0), // unreachable: rule 7 runs first
    }
}

pub(crate) fn check(p: &PineProgram, t: &'static BuiltinTable) -> Result<(), VerifyError> {
    let l = &p.layout;
    let (fl, il) = (l.f64_len, l.i64_len);
    let (rf, ri) = (abi::RESERVED_F as u32, abi::RESERVED_I as u32);
    let code_len = p.code.len() as u64;
    let n_states = l.states.len();
    let cb = l.str_cell_bytes as u64;
    let persist_end = l.str_persist_off as u64 + l.str_persist_bytes as u64;

    let mut site_used = vec![false; n_states];
    let mut cell_owner: Vec<u32> = vec![u32::MAX; if cb == 0 { 0 } else { (l.str_persist_bytes as u64 / cb) as usize }];
    let mut links = vec![false; il as usize];
    let mut max_fields: u32 = 0;
    let mut any_o_new = false;
    let mut any_intent = false;

    // ---------------------------------------------------------------- pass C
    let mut pc = 0usize;
    while pc < p.code.len() {
        let ins = p.code[pc];
        let at = pc as u32;
        // 7 Opcode
        if !op::defined(ins.op) {
            return Err(e(rule::OPCODE, at, ins.op as u32));
        }
        // 8 Operand
        let (rs, allowed) = roles(ins.op);
        if ins.flags & !allowed != 0 || (ins.flags & flg::K_CONST != 0 && ins.flags & flg::K_DYNAMIC != 0) {
            return Err(e(rule::OPERAND, at, 3));
        }
        let ops = [ins.dst, ins.a, ins.b];
        let mut k = 0usize;
        while k < 3 {
            let v = ops[k] as u32;
            let lane_i = ins.flags & flg::LANE_I != 0;
            let ok = match rs[k] {
                R::Z => v == 0,
                R::Fr => v < fl,
                R::Fw => v >= rf && v < fl,
                R::Ir => v < il,
                R::Iw | R::Iv => v >= ri && v < il,
                R::C => (v as usize) < p.consts.len(),
                R::Ring => ring_ok(p, ins, v),
                R::K => {
                    if ins.flags & flg::K_CONST != 0 {
                        (v as usize) < p.consts.len()
                    } else if ins.flags & flg::K_DYNAMIC != 0 {
                        v < il
                    } else {
                        true
                    }
                }
                R::Vr => v >= fl && v < fl + il,
                R::Vw => v >= fl + ri && v < fl + il,
                R::Lr => if lane_i { v < il } else { v < fl },
                R::Lw => if lane_i { v >= ri && v < il } else { v >= rf && v < fl },
                R::Ovl => (v as usize) < t.entries.len(),
                R::Th | R::Tl | R::Site | R::Aw | R::OSite | R::Nf | R::Fld | R::Cell | R::Any => true,
            };
            if !ok {
                return Err(e(rule::OPERAND, at, k as u32));
            }
            k += 1;
        }
        // 9 JumpTarget
        if matches!(ins.op, op::JMP | op::JZ | op::JNZ | op::JNA_F | op::JNA_I | op::CALL)
            && ins.target() as u64 >= code_len
        {
            return Err(e(rule::JUMP_TARGET, at, ins.target()));
        }
        // 13 ConstHistDepth
        if ins.op == op::HIST_F || ins.op == op::HIST_I {
            let mask = l.rings[ins.a as usize].mask as i64;
            let kk: Option<i64> = if ins.flags & flg::K_CONST != 0 {
                Some(p.consts[ins.b as usize] as i64)
            } else if ins.flags & flg::K_DYNAMIC != 0 {
                None
            } else {
                Some(ins.b as i64)
            };
            if let Some(kv) = kk {
                if kv < 0 || kv > mask {
                    return Err(e(rule::CONST_HIST_DEPTH, at, kv as u32));
                }
            }
        }
        // 15 ObjectOps (creation)
        if ins.op == op::O_NEW {
            let s = ins.a as usize;
            let ok = s < n_states
                && l.states[s].kind == StateKind::Object
                && 32 + 8 * ins.b as u64 <= l.states[s].bytes as u64
                && !site_used[s];
            if !ok {
                return Err(e(rule::OBJECT_OPS, at, ins.a as u32));
            }
            site_used[s] = true;
            any_o_new = true;
            max_fields = max_fields.max(ins.b as u32);
        }
        if ins.op == op::CALL_BUILTIN {
            let ent = t.entries[ins.a as usize];
            // 22 ArgWin
            let w = ins.b as usize;
            if w >= p.argwin.len() {
                return Err(e(rule::ARG_WIN, at, 0));
            }
            let n = p.argwin[w] as usize;
            if w + n >= p.argwin.len() {
                return Err(e(rule::ARG_WIN, at, 1));
            }
            if n < ent.n_out as usize {
                return Err(e(rule::ARG_WIN, at, 2));
            }
            let mut j = 0usize;
            while j < n {
                let en = p.argwin[w + 1 + j];
                let (len, res) = if argwin::is_i64(en) { (il, ri) } else { (fl, rf) };
                let s = argwin::slot(en) as u32;
                if s >= len {
                    return Err(e(rule::ARG_WIN, at, 3));
                }
                if j >= n - ent.n_out as usize && s < res {
                    return Err(e(rule::ARG_WIN, at, 4));
                }
                j += 1;
            }
            // 23 SiteKind
            let st = ent.flags & bf::STATEFUL != 0;
            let co = ent.flags & bf::COLLECTION != 0;
            let s = ins.dst as usize;
            let ok = if st && co {
                false
            } else if st {
                s < n_states && l.states[s].kind == StateKind::Builtin && l.states[s].bytes >= ent.state_bytes_min
            } else if co {
                s < n_states && l.states[s].kind == StateKind::Collection
            } else {
                ins.dst == SiteId::NONE.0
            };
            if !ok {
                return Err(e(rule::SITE_KIND, at, ins.dst as u32));
            }
            if st || co {
                if site_used[s] {
                    return Err(e(rule::SITE_KIND, at, ins.dst as u32));
                }
                site_used[s] = true;
            }
            any_intent |= ent.flags & bf::INTENT != 0;
        }
        // 25 StrCells
        if ins.op == op::S_PERSIST {
            let c = ins.b as u64;
            let off = l.str_persist_off as u64 + c * cb;
            let ok = cb != 0
                && off + cb <= persist_end
                && !in_string_ring(l, off)
                && ((ins.dst as u32 >= l.i64_varip_from) == (off >= l.str_persist_varip_off as u64))
                && (cell_owner[c as usize] == u32::MAX || cell_owner[c as usize] == ins.dst as u32);
            if !ok {
                return Err(e(rule::STR_CELLS, at, ins.b as u32));
            }
            cell_owner[c as usize] = ins.dst as u32;
        }
        if ins.op == op::CALL {
            links[ins.b as usize] = true;
        }
        pc += 1;
    }

    // ---------------------------------------------------------------- pass D
    // 10 Termination
    let last = match p.code.last() {
        Some(i) => i.op,
        None => return Err(e(rule::TERMINATION, NO_PC, 0)),
    };
    let mut any_halt = false;
    let mut k = 0usize;
    while k < p.code.len() {
        any_halt |= p.code[k].op == op::HALT;
        k += 1;
    }
    if !matches!(last, op::HALT | op::JMP | op::RET) || !any_halt {
        return Err(e(rule::TERMINATION, NO_PC, 1));
    }
    // 11 CodeBudget
    if code_len > p.meta.insn_budget_per_eval as u64 {
        return Err(e(rule::CODE_BUDGET, NO_PC, p.code.len() as u32));
    }
    // 12 LinkSlot and 15 ObjectOps (fields)
    k = 0;
    while k < p.code.len() {
        let ins: Insn = p.code[k];
        let at = k as u32;
        if ins.op == op::RET && !links[ins.b as usize] {
            return Err(e(rule::LINK_SLOT, at, ins.b as u32));
        }
        if ins.op != op::CALL {
            let (rs, _) = roles(ins.op);
            let ops = [ins.dst, ins.a, ins.b];
            let mut j = 0usize;
            while j < 3 {
                if rs[j] == R::Iw && links[ops[j] as usize] {
                    return Err(e(rule::LINK_SLOT, at, ops[j] as u32));
                }
                j += 1;
            }
        }
        if ins.op == op::CALL_BUILTIN {
            let ent = t.entries[ins.a as usize];
            let w = ins.b as usize;
            let n = p.argwin[w] as usize;
            let mut j = n - ent.n_out as usize;
            while j < n {
                let en = p.argwin[w + 1 + j];
                if argwin::is_i64(en) && links[argwin::slot(en) as usize] {
                    return Err(e(rule::LINK_SLOT, at, argwin::slot(en) as u32));
                }
                j += 1;
            }
        }
        let field = match ins.op {
            op::O_GET_F | op::O_GET_I => Some(ins.b),
            op::O_SET_F | op::O_SET_I => Some(ins.a),
            _ => None,
        };
        if let Some(fv) = field {
            if !any_o_new || fv as u32 >= max_fields {
                return Err(e(rule::OBJECT_OPS, at, fv as u32));
            }
        }
        k += 1;
    }
    // 17 OutRegions (intents)
    if (l.intent_cap >= 1) != any_intent {
        return Err(e(rule::OUT_REGIONS, NO_PC, 2));
    }
    Ok(())
}

/// Rule 8 for a ring operand: index, lane and NA_TRACKED agreement.
fn ring_ok(p: &PineProgram, ins: Insn, v: u32) -> bool {
    let rings = &p.layout.rings;
    if v as usize >= rings.len() {
        return false;
    }
    let d = rings[v as usize];
    if ins.op == op::HIST_F {
        d.lane == Lane::F64
    } else {
        d.lane == Lane::I64 && ((ins.flags & flg::NA_TRACKED != 0) == (d.flags & ring_flag::NA_TRACKED != 0))
    }
}
