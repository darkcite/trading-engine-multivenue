// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The step outcome and the fault taxonomy (contract §3.11).

/// Why an evaluation stopped early. Numbering frozen since v2; a variant is
/// never renumbered or reused.
///
/// Arithmetic on `na`-producing inputs is **not** a fault in v4: `int / 0`,
/// `int(na)` and a non-finite float → int cast never stop the evaluation
/// (B8); the value each yields is a conformance switch (`pin:U05-int-of-na`,
/// `pin:06-mod-zero`, `pin:06-nonfinite`), so `DivideByZeroInt` and
/// `InvalidCast` are reserved and unused by the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FaultKind {
    /// The per-evaluation instruction budget ran out.
    InsnBudget = 1,
    /// The scratch arena, or a persistent string cell, is too small.
    StrArenaFull = 2,
    /// A collection is full, or the intent buffer is.
    PoolFull = 3,
    /// Unreachable for a verified program (I-8); kept as a defence.
    JournalFull = 4,
    /// An array index out of range (Pine raises a runtime error).
    CollectionBounds = 5,
    /// Reserved (v4: `int / 0` yields `na`).
    DivideByZeroInt = 6,
    /// Reserved (v4: a failed cast yields `na`).
    InvalidCast = 7,
    /// Our defect: a `TRAP`, a `RET` out of range, a table mismatch.
    Internal = 8,
    /// The script asked for something Pine itself rejects at runtime: a
    /// field of an `na` object, a stale collection reference, an `na` map
    /// key, an unresolved symbol with `ignore_invalid_symbol = false`.
    /// NOT our bug.
    ScriptRefused = 9,
}

/// The result of one `step_bar` / `step_tick`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepOutcome {
    /// The evaluation ran to `HALT`.
    Ok,
    /// It stopped. The frame is left in a defined state (a tick is undone
    /// to its checkpoint); `VmInstance::last_fault` has the `pc`. The host
    /// decides what happens next (D13): abort in backtest, disable and
    /// flatten the one script in paper/live.
    Fault(FaultKind),
}

const _: () = assert!(core::mem::size_of::<FaultKind>() == 1);

/// Per-instance counters a host mirrors into its metrics. 40 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct VmCounters {
    /// `step_bar` evaluations.
    pub bar_evals: u64,
    /// `step_tick` evaluations.
    pub tick_evals: u64,
    /// `na`-producing arithmetic events (`int / 0`, a failed cast) — not faults (B8).
    pub na_arith: u64,
    /// Faults returned.
    pub faults: u64,
    /// Most instructions any one evaluation charged (sizes `insn_budget`).
    pub max_insns: u32,
    /// Always zero.
    pub _pad: u32,
}

const _: () = assert!(core::mem::size_of::<VmCounters>() == 40);
