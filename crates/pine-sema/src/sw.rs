// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! `Readings` (impl-02 §4.2.3): the **only** caller of `pine_spec::sw`, folding every switch sema
//! owns for one dialect, plus the `pins.rs` flags. Built at the top of `compile` and passed by
//! reference to every pass that reads one. A switch joins `Readings` in the change that lands the
//! pass acting on it, so every field has a reader.

use pine_ir::{Diag, Dialect, Span};
use pine_spec::{sw, swr, Sw};

use crate::code::{diag, ReasonCode};

/// Every reading of one compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Readings {
    /// `Sw::IntDiv` (W1) — `swr::INT_DIV_*`; §4.17 folding.
    pub(crate) int_div: u8,
    /// `Sw::AndOr` (W3) — `swr::AND_OR_*`; §4.14.3 `and` / `or`.
    pub(crate) and_or: u8,
    /// `Sw::BoolNa` (W13) — `swr::BOOL_NA_*`; §4.4.3 conversions, §4.14.4 structure values.
    pub(crate) bool_na: u8,
    /// `Sw::ForBound` (MG-23) — `swr::FOR_BOUND_*`; §4.14.4 `for`.
    pub(crate) for_bound: u8,
    /// `Sw::RoundTies` (U04) — `swr::ROUND_TIES_*`; §4.17 folding.
    pub(crate) round_ties: u8,
    /// `Sw::FloatDivZero` (U15) — `swr::FLOAT_DIV_ZERO_*`; §4.17 folding, §4.14.2 `/` and `%`.
    pub(crate) float_div_zero: u8,
    /// `Sw::IntOfNa` (U05) — `swr::INT_OF_NA_*`; §4.14.6 `int(x)`.
    pub(crate) int_of_na: u8,
    /// `Sw::FloatEqV6` (U06) — `swr::FLOAT_EQ_V6_*`; §4.14.2 float `==` / `!=` (v6 only).
    pub(crate) float_eq_v6: u8,
    /// `Sw::TaInLoop` (U13) — `swr::TA_IN_LOOP_*`; §4.7.4 `hoist.rs`.
    pub(crate) ta_in_loop: u8,
    /// `Sw::TernaryArms` (U14) — `swr::TERNARY_ARMS_*`; §4.14.3 the ternary.
    pub(crate) ternary_arms: u8,
    /// `Sw::BlockLocal` (U16) — `swr::BLOCK_LOCAL_*`; §4.14.4 block-locals.
    pub(crate) block_local: u8,
    /// The sema-local pins.
    pub(crate) pins: crate::pins::Pins,
}

impl Readings {
    /// The default readings of dialect `d`.
    pub(crate) const fn of(d: Dialect) -> Readings {
        Readings {
            int_div: sw(Sw::IntDiv, d),
            and_or: sw(Sw::AndOr, d),
            bool_na: sw(Sw::BoolNa, d),
            for_bound: sw(Sw::ForBound, d),
            round_ties: sw(Sw::RoundTies, d),
            float_div_zero: sw(Sw::FloatDivZero, d),
            int_of_na: sw(Sw::IntOfNa, d),
            float_eq_v6: sw(Sw::FloatEqV6, d),
            ta_in_loop: sw(Sw::TaInLoop, d),
            ternary_arms: sw(Sw::TernaryArms, d),
            block_local: sw(Sw::BlockLocal, d),
            pins: crate::pins::DEFAULT,
        }
    }

    /// Fail fast on a switch code sema has no reading for (a pine-spec row sema cannot lower is
    /// `3255`, §3.2).
    pub(crate) fn check(&self) -> Result<(), Diag> {
        let known = [
            (
                "int_div",
                self.int_div,
                [swr::INT_DIV_CONST_TRUNC, swr::INT_DIV_FRACTION],
            ),
            (
                "and_or",
                self.and_or,
                [swr::AND_OR_STRICT, swr::AND_OR_LAZY],
            ),
            (
                "for_bound",
                self.for_bound,
                [swr::FOR_BOUND_FIXED_AT_START, swr::FOR_BOUND_EACH_ITERATION],
            ),
            (
                "ta_in_loop",
                self.ta_in_loop,
                [swr::TA_IN_LOOP_PER_CALL, swr::TA_IN_LOOP_PER_ITERATION],
            ),
            (
                "ternary_arms",
                self.ternary_arms,
                [swr::TERNARY_ARMS_SELECTED_ONLY, swr::TERNARY_ARMS_BOTH],
            ),
            (
                "block_local",
                self.block_local,
                [swr::BLOCK_LOCAL_CARRY, swr::BLOCK_LOCAL_NA],
            ),
            (
                "bool_na",
                self.bool_na,
                [swr::BOOL_NA_NULLABLE, swr::BOOL_NA_NEVER],
            ),
            (
                "round_ties",
                self.round_ties,
                [swr::ROUND_TIES_HALF_UP, swr::ROUND_TIES_HALF_AWAY],
            ),
            (
                "float_div_zero",
                self.float_div_zero,
                [swr::FLOAT_DIV_ZERO_NA, swr::FLOAT_DIV_ZERO_IEEE],
            ),
            (
                "int_of_na",
                self.int_of_na,
                [swr::INT_OF_NA_NA, swr::INT_OF_NA_FAULT],
            ),
            (
                "float_eq_v6",
                self.float_eq_v6,
                [swr::FLOAT_EQ_V6_EXACT, swr::FLOAT_EQ_V6_ROUND9],
            ),
        ];
        for (name, v, codes) in known {
            if !codes.contains(&v) {
                let msg = format!("pine-spec switch {name} reading {v} has no reading in sema");
                return Err(diag(ReasonCode::Internal, Span::default(), msg));
            }
        }
        Ok(())
    }
}
