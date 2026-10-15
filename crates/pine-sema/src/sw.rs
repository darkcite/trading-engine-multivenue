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
    /// `Sw::BoolNa` (W13) — `swr::BOOL_NA_*`; §4.4.3 conversions.
    pub(crate) bool_na: u8,
    /// `Sw::RoundTies` (U04) — `swr::ROUND_TIES_*`; §4.17 folding.
    pub(crate) round_ties: u8,
    /// `Sw::FloatDivZero` (U15) — `swr::FLOAT_DIV_ZERO_*`; §4.17 folding.
    pub(crate) float_div_zero: u8,
    /// The sema-local pins.
    pub(crate) pins: crate::pins::Pins,
}

impl Readings {
    /// The default readings of dialect `d`.
    pub(crate) const fn of(d: Dialect) -> Readings {
        Readings {
            int_div: sw(Sw::IntDiv, d),
            bool_na: sw(Sw::BoolNa, d),
            round_ties: sw(Sw::RoundTies, d),
            float_div_zero: sw(Sw::FloatDivZero, d),
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
