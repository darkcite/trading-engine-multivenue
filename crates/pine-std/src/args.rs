// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Argument-window helpers (impl-04 §3.3 rule 2, contract §3.6.2): a window is the overload's
//! inputs in parameter order, then its outputs.

use pine_ir::{argwin, FaultKind};
use pine_vm::Frame;

/// The single output entry of an `n_out == 1` row (the window's last entry). An empty window is
/// a verifier/sema defect: `Internal`.
#[inline(always)]
pub fn out1(args: &[u16]) -> Result<u16, FaultKind> {
    match args.last() {
        Some(&e) => Ok(e),
        None => Err(FaultKind::Internal),
    }
}

/// Write the float result `v` (NaN = na) into the output entry `e` in that entry's lane: an
/// `int`-returning row (`math.ceil`, `math.floor`, `math.round`) has an i64-lane output, where a
/// NaN is `na` and a finite value truncates toward zero after the caller's rounding.
#[inline(always)]
pub fn set_num_out(f: &mut Frame<'_>, e: u16, v: f64) {
    if argwin::is_i64(e) {
        let r = if v.is_finite() && v.abs() < 9.223_372_036_854_775e18 {
            Some(v as i64)
        } else {
            None
        };
        f.set_i64_arg(e, r);
    } else {
        f.set_f64_arg(e, v);
    }
}
