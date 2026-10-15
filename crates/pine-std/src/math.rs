// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `math.*` and the lane-copy cast body (impl-04 §4.13.1). Stateless; `na` propagates (every row
//! here is `NaPolicy::Propagate` in pine-spec).

use crate::args::{out1, set_num_out};
use crate::naf::{fmax, fmin};
use pine_ir::{argwin, FaultKind, SiteId};
use pine_vm::Frame;

/// A unary f64 → number body over `op`.
#[inline(always)]
fn unary(f: &mut Frame<'_>, args: &[u16], op: fn(f64) -> f64) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let x = f.f64_arg(args[0]);
    set_num_out(f, args[1], if x.is_nan() { f64::NAN } else { op(x) });
    Ok(())
}

macro_rules! unary_body {
    ($(#[$m:meta] $name:ident => $op:expr;)*) => {$(
        #[$m]
        pub fn $name(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
            unary(f, args, $op)
        }
    )*};
}

unary_body! {
    /// `math.abs(float)`.
    bi_abs_f => f64::abs;
    /// `math.acos`.
    bi_acos => f64::acos;
    /// `math.asin`.
    bi_asin => f64::asin;
    /// `math.atan`.
    bi_atan => f64::atan;
    /// `math.ceil` (an `int` result).
    bi_ceil => f64::ceil;
    /// `math.cos`.
    bi_cos => f64::cos;
    /// `math.exp`.
    bi_exp => f64::exp;
    /// `math.floor` (an `int` result).
    bi_floor => f64::floor;
    /// `math.log` (natural).
    bi_log => f64::ln;
    /// `math.log10`.
    bi_log10 => f64::log10;
    /// `math.sin`.
    bi_sin => f64::sin;
    /// `math.sqrt` — bit-identical to `F_SQRT` (contract §3.6.1).
    bi_sqrt => f64::sqrt;
    /// `math.tan`.
    bi_tan => f64::tan;
    /// `math.todegrees`.
    bi_todegrees => f64::to_degrees;
    /// `math.toradians`.
    bi_toradians => f64::to_radians;
    /// `math.sign`: -1, 0 or 1.
    bi_sign => sign;
}

#[inline(always)]
fn sign(x: f64) -> f64 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// `math.pow(base, exponent)` — bit-identical to `F_POW` (contract §3.6.1).
pub fn bi_pow(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (b, x) = (f.f64_arg(args[0]), f.f64_arg(args[1]));
    let r = if b.is_nan() || x.is_nan() {
        f64::NAN
    } else {
        b.powf(x)
    };
    f.set_f64_arg(args[2], r);
    Ok(())
}

/// `math.abs(int)`: `na` stays `na`; `i64::MIN` has no absolute value and is `na`.
pub fn bi_abs_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let r = match f.i64_arg(args[0]) {
        Some(x) => x.checked_abs(),
        None => None,
    };
    f.set_i64_arg(args[1], r);
    Ok(())
}

/// A variadic float fold (`math.max`, `math.min`) over every input entry; `na` propagates.
#[inline(always)]
fn fold_f(f: &mut Frame<'_>, args: &[u16], op: fn(f64, f64) -> f64) -> Result<(), FaultKind> {
    let out = out1(args)?;
    let n = args.len() - 1;
    if n == 0 {
        return Err(FaultKind::Internal);
    }
    let mut acc = f.f64_arg(args[0]);
    let mut i = 1usize;
    while i < n {
        acc = op(acc, f.f64_arg(args[i]));
        i += 1;
    }
    f.set_f64_arg(out, acc);
    Ok(())
}

/// A variadic int fold (`math.max`, `math.min` on ints); `na` propagates.
#[inline(always)]
fn fold_i(f: &mut Frame<'_>, args: &[u16], op: fn(i64, i64) -> i64) -> Result<(), FaultKind> {
    let out = out1(args)?;
    let n = args.len() - 1;
    if n == 0 {
        return Err(FaultKind::Internal);
    }
    let mut acc = f.i64_arg(args[0]);
    let mut i = 1usize;
    while i < n {
        acc = match (acc, f.i64_arg(args[i])) {
            (Some(a), Some(b)) => Some(op(a, b)),
            _ => None,
        };
        i += 1;
    }
    f.set_i64_arg(out, acc);
    Ok(())
}

/// `math.max(float…)` — NaN-propagating, unlike `f64::max`.
pub fn bi_max_f(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    fold_f(f, args, fmax)
}

/// `math.min(float…)` — NaN-propagating, unlike `f64::min`.
pub fn bi_min_f(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    fold_f(f, args, fmin)
}

/// `math.max(int…)`.
pub fn bi_max_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    fold_i(f, args, i64::max)
}

/// `math.min(int…)`.
pub fn bi_min_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    fold_i(f, args, i64::min)
}

/// `math.avg(float…)`: the sum over the count, `na` if any input is.
pub fn bi_avg(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let out = out1(args)?;
    let n = args.len() - 1;
    if n == 0 {
        return Err(FaultKind::Internal);
    }
    let mut s = 0.0f64;
    let mut i = 0usize;
    while i < n {
        s += f.f64_arg(args[i]);
        i += 1;
    }
    f.set_f64_arg(out, s / n as f64);
    Ok(())
}

/// The identity casts (`int(int)`, `float(float)`, `bool(bool)`, `color(color)`,
/// `string(string)`): copy the input entry to the output in the entry's lane, validity included
/// (impl-04 §3.2).
pub fn bi_cast_id(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let (a, o) = (args[0], args[1]);
    if argwin::is_i64(a) != argwin::is_i64(o) {
        return Err(FaultKind::Internal);
    }
    if argwin::is_i64(a) {
        let v = f.i64_arg(a);
        f.set_i64_arg(o, v);
    } else {
        let v = f.f64_arg(a);
        f.set_f64_arg(o, v);
    }
    Ok(())
}
