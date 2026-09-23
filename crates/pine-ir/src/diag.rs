// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Diagnostics and the frozen reason-code ranges (contract §3.3).
//!
//! COPY-DOCTRINE: compile/load time only. `Diag` owns a `String` message —
//! the only owned string the lane permits, and never on a hot path.

use crate::ids::Span;

/// How bad a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Severity {
    /// Refuses the script.
    Error = 0,
    /// Loads, with a recorded caveat.
    Warning = 1,
    /// Context attached to another diagnostic.
    Note = 2,
}

/// A stable refusal / diagnostic identifier. A code is never reused and
/// never renumbered once shipped — PVM-9's bucket histogram and PVM-10's
/// quarantine buckets compare counts across builds.
///
/// Ranges, frozen (one owner each):
///
/// | range | owner |
/// |---|---|
/// | 1000–1999 | `pine-lang` lexer |
/// | 2000–2999 | `pine-lang` parser |
/// | 3000–3999 | `pine-sema`: `3000 + n` for impl-02's `ReasonCode` `n` |
/// | 4000–4999 | `pine-vm` verifier: `4000 + rule` |
/// | 5000–5099 | `pine-std` load-time services (timeframe, session, timezone, regex, format, input) |
/// | 5100–5199 | `pine-broker` declaration validation |
/// | 6000–6999 | hosts: manifest, boot and arming refusals (`strategy-pine`, `core-config`) |
/// | 7000–7999 | offline harnesses: `pine-screen`, `pine-conformance`, `cli backtest --strategy pine` |
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DiagCode(pub u16);

/// Which crate owns a [`DiagCode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DiagRange {
    /// 1000–1999.
    Lexer = 0,
    /// 2000–2999.
    Parser = 1,
    /// 3000–3999.
    Sema = 2,
    /// 4000–4999.
    Verifier = 3,
    /// 5000–5099.
    Std = 4,
    /// 5100–5199.
    Broker = 5,
    /// 6000–6999.
    Host = 6,
    /// 7000–7999.
    Harness = 7,
    /// Outside every range: a defect.
    Unassigned = 255,
}

impl DiagCode {
    /// The sema code for impl-02 `ReasonCode` number `n` (`1..=999`).
    #[inline]
    #[must_use]
    pub const fn sema(n: u16) -> Self {
        debug_assert!(n >= 1 && n <= 999);
        DiagCode(3000 + n)
    }

    /// The verifier code for rule number `rule` (`1..=999`).
    #[inline]
    #[must_use]
    pub const fn verify(rule: u16) -> Self {
        debug_assert!(rule >= 1 && rule <= 999);
        DiagCode(4000 + rule)
    }

    /// The owning range.
    #[inline]
    #[must_use]
    pub const fn range(self) -> DiagRange {
        match self.0 {
            1000..=1999 => DiagRange::Lexer,
            2000..=2999 => DiagRange::Parser,
            3000..=3999 => DiagRange::Sema,
            4000..=4999 => DiagRange::Verifier,
            5000..=5099 => DiagRange::Std,
            5100..=5199 => DiagRange::Broker,
            6000..=6999 => DiagRange::Host,
            7000..=7999 => DiagRange::Harness,
            _ => DiagRange::Unassigned,
        }
    }
}

/// One diagnostic. Compile/load time only.
///
/// The refusal reason recorded in an operator log or a bucket file is the
/// [`DiagCode`], never the prose.
#[derive(Debug, Clone)]
pub struct Diag {
    /// The stable code.
    pub code: DiagCode,
    /// Error, warning or note.
    pub sev: Severity,
    /// Where.
    pub span: Span,
    /// Human-readable, rendered at load.
    pub msg: String,
    /// Extra locations: the other overload candidate, the previous
    /// declaration, a cycle's path.
    pub notes: Vec<(Span, String)>,
}

const _: () = assert!(core::mem::size_of::<DiagCode>() == 2);
const _: () = assert!(core::mem::size_of::<Severity>() == 1);
