// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `verify` (contract §3.8.3): the licence for every unchecked access in the
//! dispatch loop. Load-time only; may allocate (`//! COPY-DOCTRINE:` below).
//!
//! COPY-DOCTRINE: load time only — `verify` sorts span lists and builds small
//! bitsets once per program; nothing here runs after `VmInstance::new`.

mod code;
mod layout;

use pine_ir::{DiagCode, ExecMode, PineProgram};

use crate::builtin::BuiltinTable;

/// Rule numbers, frozen by contract §3.8.3 (`DiagCode` = 4000 + n). 14 is retired.
pub mod rule {
    /// I-3, I-5; `total_bytes <= u32::MAX / 2`.
    pub const REGIONS: u16 = 1;
    /// I-1.
    pub const VALID_WORDS: u16 = 2;
    /// `f64_len >= 5`, `i64_len >= 10`.
    pub const LANE_MINIMA: u16 = 3;
    /// Ring capacity, span, slot, flags, string cells.
    pub const RING_SHAPE: u16 = 4;
    /// Ring spans pairwise disjoint.
    pub const RING_OVERLAP: u16 = 5;
    /// I-4; block placement, alignment, disjointness.
    pub const STATE_DENSE: u16 = 6;
    /// `op::defined`.
    pub const OPCODE: u16 = 7;
    /// Every operand in range for its role; flags (V8f).
    pub const OPERAND: u16 = 8;
    /// Every jump target < code.len().
    pub const JUMP_TARGET: u16 = 9;
    /// No fall-through past the end; a HALT exists.
    pub const TERMINATION: u16 = 10;
    /// `code.len() <= insn_budget_per_eval`.
    pub const CODE_BUDGET: u16 = 11;
    /// RET link slots are written only by CALL.
    pub const LINK_SLOT: u16 = 12;
    /// A literal / constant `k` is `<= mask`.
    pub const CONST_HIST_DEPTH: u16 = 13;
    /// `O_*` sites and field bounds.
    pub const OBJECT_OPS: u16 = 15;
    /// I-6; the varip tails.
    pub const VARIP: u16 = 16;
    /// Plots and intent capacity.
    pub const OUT_REGIONS: u16 = 17;
    /// I-7, I-8 (Realtime); both zero (BarClose).
    pub const JOURNAL: u16 = 18;
    /// Recursive verify of sub-programs.
    pub const SUBPROGRAM: u16 = 19;
    /// Ring capacity bounded by `max_bars_back` and 2^20.
    pub const RING_CAP: u16 = 20;
    /// Lane and table widths.
    pub const OPERAND_WIDTH: u16 = 21;
    /// Argument windows.
    pub const ARG_WIN: u16 = 22;
    /// Builtin site kinds.
    pub const SITE_KIND: u16 = 23;
    /// `meta.spec_sha256 == table.spec_sha256`.
    pub const SPEC_HASH: u16 = 24;
    /// Persistent string cells.
    pub const STR_CELLS: u16 = 25;
}

/// The first rule a program breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyError {
    /// The rule number (`rule::*`).
    pub rule: u16,
    /// Instruction index, or `u32::MAX` for a layout / program-level rule.
    pub pc: u32,
    /// The offending index or value (per rule, see impl-03 §4.12).
    pub detail: u32,
}

impl VerifyError {
    /// `DiagCode::verify(rule)` = 4000 + rule.
    pub fn code(&self) -> DiagCode {
        DiagCode::verify(self.rule)
    }
}

/// Proof that a program passed `verify` against a table. The only way to build an instance.
#[derive(Clone, Copy)]
pub struct Verified<'p> {
    pub(crate) prog: &'p PineProgram,
    pub(crate) table: &'static BuiltinTable,
}

impl core::fmt::Debug for Verified<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Verified")
            .field("code_len", &self.prog.code.len())
            .field("mode", &self.prog.mode)
            .field("subprograms", &self.prog.subprograms.len())
            .finish()
    }
}

pub(crate) const NO_PC: u32 = u32::MAX;

#[inline]
pub(crate) fn e(rule: u16, pc: u32, detail: u32) -> VerifyError {
    VerifyError { rule, pc, detail }
}

/// Verify `p` (and, recursively, its sub-programs) against `t`.
pub fn verify<'p>(p: &'p PineProgram, t: &'static BuiltinTable) -> Result<Verified<'p>, VerifyError> {
    verify_at(p, t, 0)?;
    Ok(Verified { prog: p, table: t })
}

fn verify_at(p: &PineProgram, t: &'static BuiltinTable, depth: u32) -> Result<(), VerifyError> {
    layout::check(p)?;
    if p.meta.spec_sha256 != t.spec_sha256 {
        return Err(e(rule::SPEC_HASH, NO_PC, 0));
    }
    code::check(p, t)?;
    let mut k = 0usize;
    while k < p.subprograms.len() {
        let sp = &p.subprograms[k];
        let bad = depth + 1 > 3
            || sp.n_out < 1
            || sp.n_out as u32 > sp.program.layout.n_plots
            || sp.history < 2
            || (p.mode == ExecMode::Realtime && sp.program.mode != ExecMode::Realtime);
        if bad {
            return Err(e(rule::SUBPROGRAM, NO_PC, (k as u32) << 16));
        }
        if let Err(c) = verify_at(&sp.program, t, depth + 1) {
            return Err(e(rule::SUBPROGRAM, c.pc, ((k as u32) << 16) | c.rule as u32));
        }
        k += 1;
    }
    Ok(())
}
