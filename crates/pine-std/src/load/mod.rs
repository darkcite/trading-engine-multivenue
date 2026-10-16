// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Load-time services (contract §3.8.4, impl-04 §3.7). Allocation-free, clock-free and pure; each
//! is also callable at run time. Refusals are `DiagCode`s 5001–5007 only (contract §3.3).

use pine_ir::DiagCode;

mod fmt;
mod input;
mod regex;
mod session;
mod tf;
mod tz;
mod tz_table;

pub use fmt::{fmt_compile, FmtProg, FMT_MAX_SEGMENTS};
pub use input::{resolve_input, InputResolved};
pub use regex::{regex_compile, regex_compile_into, RegexProg, REGEX_MAX_STATES};
pub use session::{session_parse, SessionRange, SessionSpec};
pub use tf::{tf_on_bar_clock, tf_parts, tf_seconds, tf_string, TfParts, TfUnit, MONTH_SORT_S};
pub use tz::{tz_lookup, tz_offset_s, TzId, TZ_UTC};

/// A refusal from a load-time service. The caller (pine-sema, a host) wraps it in a
/// `pine_ir::Diag` with its own span, `Severity::Error`, and renders `why` into `Diag::msg`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadErr {
    /// One of `code::*` (5001..=5007).
    pub code: DiagCode,
    /// Byte offset in the input text where parsing failed; 0 when not positional.
    pub at: u32,
    /// Fixed English reason.
    pub why: &'static str,
}

/// The seven refusal codes of the load-time services (contract §3.3).
pub mod code {
    use pine_ir::DiagCode;
    /// A timeframe string outside the documented grammar.
    pub const BAD_TIMEFRAME: DiagCode = DiagCode(5001);
    /// A session string outside the documented grammar.
    pub const BAD_SESSION: DiagCode = DiagCode(5002);
    /// A timezone name the table does not hold.
    pub const UNKNOWN_TZ: DiagCode = DiagCode(5003);
    /// A regex outside the supported subset, or over `REGEX_MAX_STATES`.
    pub const REGEX_UNSUPPORTED: DiagCode = DiagCode(5004);
    /// A `str.format` template outside the supported grammar.
    pub const BAD_FORMAT: DiagCode = DiagCode(5005);
    /// An input override outside `minval`/`maxval`/`options`/the colour range.
    pub const INPUT_RANGE: DiagCode = DiagCode(5006);
    /// An input override of the wrong `InputValue` variant.
    pub const INPUT_TYPE: DiagCode = DiagCode(5007);
}

/// Build a `LoadErr` (`at` is a byte offset into the input text).
#[inline]
pub(crate) const fn err(code: DiagCode, at: usize, why: &'static str) -> LoadErr {
    LoadErr {
        code,
        at: at as u32,
        why,
    }
}
