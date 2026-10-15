// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A thin wrapper over `pine_spec::sw` (impl-00 §3.2; impl-04 §4.11.1) and its only caller. It
//! names no switch: a body folds `sw::both(Sw::X)` in a `const` and `pick`s once per call.

use pine_ir::Dialect;
use pine_spec::Sw;

/// Both dialects' readings of `s`, `[v5, v6]`, for a `const` fold.
#[must_use]
pub const fn both(s: Sw) -> [u8; 2] {
    [pine_spec::sw(s, Dialect::V5), pine_spec::sw(s, Dialect::V6)]
}

/// The reading for `d` from a folded pair: a two-way select, once per call.
#[inline(always)]
#[must_use]
pub const fn pick(r: [u8; 2], d: Dialect) -> u8 {
    match d {
        Dialect::V5 => r[0],
        Dialect::V6 => r[1],
    }
}
