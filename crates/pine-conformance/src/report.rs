// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! What `Runner::check` concludes about one fixture (impl-06 §3.9). This edition carries the
//! verdict model T15 needs; the §4.6.4 `render` lands with T17 (`triage`, `sig`).

/// One fixture's verdict.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Every assertion held.
    Pass,
    /// `Expect::Pending`: the run completed (smoke); compared to nothing.
    Pending,
    /// Not run (counted, I20).
    Skipped(crate::run::SkipReason),
    /// An assertion failed.
    Fail(Divergence),
    /// Our own v5 and v6 runs of a `56` fixture differ (I9) — before any expectation is read.
    DialectDivergence {
        /// The first differing row key ([`NO_ROW`] when the runs differ in outcome, not in a row).
        first_row: i64,
        /// The first differing plot (`usize::MAX` when no plot is involved).
        plot: usize,
    },
}

/// `pin:06-divergence-no-row` — §3.9's `first_row` is an `i64` with no "none": a failure that is
/// not about a row (a wrong refusal code, an unexpected fault, a run that should have been refused)
/// carries `i64::MIN`.
pub const NO_ROW: i64 = i64::MIN;

/// Where a fixture's expectation and our run part.
#[derive(Debug, Clone)]
pub struct Divergence {
    /// The first divergent plot (`Fixture::plots` index).
    pub plot: Option<usize>,
    /// The first divergent row key (inside or outside the tolerance), or [`NO_ROW`].
    pub first_row: i64,
    /// The first row key outside the tolerance.
    pub first_out_of_bound: Option<i64>,
    /// The trade-list or summary divergence.
    pub trade: Option<crate::trades::TradeDiv>,
    /// A run error or an expectation the run could not meet, as text.
    pub error: Option<String>,
}

impl Divergence {
    /// A divergence that is only an error text.
    pub fn error(what: String) -> Self {
        Self {
            plot: None,
            first_row: NO_ROW,
            first_out_of_bound: None,
            trade: None,
            error: Some(what),
        }
    }
}
