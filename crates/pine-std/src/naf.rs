// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `na`-aware arithmetic (impl-04 §4.2): NaN-propagating `fmax`/`fmin` — `f64::max` drops a NaN
//! operand, which would turn `math.max(na, 1)` into `1` (clippy's `disallowed-methods` bans it).

/// `max(a, b)`, `na` (NaN) if either is.
#[inline(always)]
#[must_use]
pub fn fmax(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a >= b {
        a
    } else {
        b
    }
}

/// `min(a, b)`, `na` (NaN) if either is.
#[inline(always)]
#[must_use]
pub fn fmin(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a <= b {
        a
    } else {
        b
    }
}
