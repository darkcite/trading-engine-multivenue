// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Amendment A10 — the static per-evaluation charge of an acyclic program.
//!
//! Runs once inside `verify`, after every rule has passed; it never refuses a program. The
//! per-instruction charge mirrors `interp::run` exactly (impl-03 §4.5 item 5): `BUDGET` charges
//! `(b << 16) | a`, a forward `CALL` charges 1, `CALL_BUILTIN` charges its entry's `cost`, a
//! forward jump and every other opcode charge 0. A backward jump or `CALL` (`target <= here`)
//! and every `RET` (a dynamic target; a `CALL` / `RET` pair always closes a cycle) make the
//! code cyclic, and the answer is `None`. Otherwise every edge runs from `pc` to a larger pc,
//! so pc order is a topological order and one reverse sweep computes the longest path.
//!
//! COPY-DOCTRINE: load time only — one `u64` per instruction, dropped before `verify` returns.

use pine_ir::{op, PineProgram};

use crate::builtin::BuiltinTable;

/// The most budget units one evaluation of `p`'s own code can charge (`interp::run`'s
/// `Ok(charged)`): the maximum over every control-flow path from pc 0 of the sum of the
/// per-instruction charges. `None` when the code has a back-edge or a `RET`, or when the
/// maximum exceeds `u32::MAX`. Sub-programs are not included (each child is verified on its
/// own and carries its own bound — the A10 ruling). Requires a program `code::check` passed:
/// rule 8 bounds `CALL_BUILTIN`'s entry index, rule 9 every target, rule 10 the fall-through.
pub(crate) fn static_charge(p: &PineProgram, t: &BuiltinTable) -> Option<u32> {
    let n = p.code.len();
    // best[pc] = the largest charge from pc (inclusive) to a HALT / TRAP; u64 saturating, so
    // a sum past u32::MAX cannot wrap before the final conversion refuses it.
    let mut best: Vec<u64> = vec![0; n];
    let mut k = n;
    while k > 0 {
        k -= 1;
        let ins = p.code[k];
        let here = k as u32;
        // Rule 10: the last instruction is HALT / JMP / RET, none of which falls through.
        let nx: u64 = if k + 1 < n { best[k + 1] } else { 0 };
        let v: u64 = match ins.op {
            op::HALT | op::TRAP => 0,
            op::RET => return None,
            op::JMP => {
                let tg = ins.target();
                if tg <= here {
                    return None;
                }
                best[tg as usize]
            }
            op::JZ | op::JNZ | op::JNA_F | op::JNA_I => {
                let tg = ins.target();
                if tg <= here {
                    return None;
                }
                nx.max(best[tg as usize])
            }
            op::CALL => {
                let tg = ins.target();
                if tg <= here {
                    return None;
                }
                best[tg as usize].saturating_add(1)
            }
            op::BUDGET => {
                let c = ((ins.b as u32) << 16) | ins.a as u32;
                nx.saturating_add(c as u64)
            }
            op::CALL_BUILTIN => {
                let c = t.entries[ins.a as usize].cost;
                nx.saturating_add(c as u64)
            }
            _ => nx,
        };
        best[k] = v;
    }
    if n == 0 {
        return None;
    }
    u32::try_from(best[0]).ok()
}
