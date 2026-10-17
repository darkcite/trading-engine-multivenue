// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Constant folding (impl-02 §4.17): every fold equals what the VM computes **bit for bit**, or it
//! would move a script's behaviour. `None` is `na`. Transcendentals (`math.pow`, …) are never
//! folded: `f64::powf` is the platform libm and would bind the compile host's bits.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "called by pass 3, infer.rs (impl-02 T6)")
)]

use pine_ir::color;
use pine_spec::swr;

use crate::pins::{ColorAlpha, IntCast};
use crate::sw::Readings;

/// `F_DIV` per U15: `na` → a non-finite quotient is `na` (NaN); `ieee` → IEEE.
pub(crate) fn f_div(a: f64, b: f64, rd: &Readings) -> f64 {
    non_finite(a / b, rd)
}

/// Pine's truncated float `%` (CAR-R31-1: the sign of the dividend, the exact IEEE `fmod`, as the
/// VM's `F_MOD`; TradingView F039 / F116, `pin:06-mod-sign`), then U15's non-finite rule.
/// `f_mod(-1.0, 100.0) == -1.0`.
pub(crate) fn f_mod(a: f64, b: f64, rd: &Readings) -> f64 {
    non_finite(a % b, rd)
}

/// `pin:03-float-mod` (CAR-R33-4, TradingView F098): a folded float `%` is the VM's `F_MOD`
/// rounded to 15 significant decimal digits — `1.0 % 0.1` is `0.1`, `10.0 % 0.1` is
/// `0.0999999999999995`, `-0.3 % 0.1` is `-0.1` (3 of 3 folded columns), while the same `%` on a
/// series is the exact remainder (F098's series columns). Non-finite values pass through.
/// `pin:02-fold-sig15-ties`: an exact decimal tie at the 16th digit takes Rust's correctly
/// rounded `{:.14e}` (the capture is silent on ties).
pub(crate) fn sig15(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.14e}").parse::<f64>().unwrap_or(x)
}

/// U15's rule on a float result.
fn non_finite(r: f64, rd: &Readings) -> f64 {
    if rd.float_div_zero == swr::FLOAT_DIV_ZERO_NA && !r.is_finite() {
        f64::NAN
    } else {
        r
    }
}

/// `I_ADD` / `I_SUB` / `I_MUL`: wrapping.
pub(crate) const fn i_add(a: i64, b: i64) -> i64 {
    a.wrapping_add(b)
}

/// `I_SUB`.
pub(crate) const fn i_sub(a: i64, b: i64) -> i64 {
    a.wrapping_sub(b)
}

/// `I_MUL`.
pub(crate) const fn i_mul(a: i64, b: i64) -> i64 {
    a.wrapping_mul(b)
}

/// The result of `const int / const int` (W1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum IntDiv {
    /// `const_trunc` (v5): `I_DIV`, truncating; zero divisor or `MIN / −1` is `na`.
    Int(Option<i64>),
    /// `fraction` (v6): the float quotient, U15 applied.
    Float(f64),
}

/// `a / b` on two `const int`s under `Sw::IntDiv`.
pub(crate) fn i_div(a: i64, b: i64, rd: &Readings) -> IntDiv {
    if rd.int_div == swr::INT_DIV_CONST_TRUNC {
        IntDiv::Int(a.checked_div(b))
    } else {
        IntDiv::Float(f_div(a as f64, b as f64, rd))
    }
}

/// Pine's truncated int `%` (CAR-R31-1, as the VM's `I_MOD`; TradingView F004 / F115,
/// `pin:06-mod-sign`): `b == 0` is `na`; `MIN % −1` is 0; the sign of the dividend.
/// `i_mod(-1, 100) == Some(-1)`.
pub(crate) const fn i_mod(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    Some(match a.checked_rem(b) {
        Some(r) => r,
        None => 0,
    })
}

/// `2^63` as an `f64`: the first float `I_F2I` cannot represent.
const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// `int(x)` of a float per `pin:02-int-cast`; non-finite or out of range is `na` (`I_F2I`).
pub(crate) fn int_of_float(x: f64, rd: &Readings) -> Option<i64> {
    let y = match rd.pins.int_cast {
        IntCast::Trunc => x.trunc(),
        IntCast::RoundHalfAway => x.round(),
    };
    if y.is_finite() && (-TWO_63..TWO_63).contains(&y) {
        Some(y as i64)
    } else {
        None
    }
}

/// `math.round(x)` of a `const` float per U04 (`half_up` = `(x + 0.5).floor()`, `half_away` =
/// `x.round()`), then the `int` rule.
pub(crate) fn round(x: f64, rd: &Readings) -> Option<i64> {
    let y = if rd.round_ties == swr::ROUND_TIES_HALF_UP {
        (x + 0.5).floor()
    } else {
        x.round()
    };
    if y.is_finite() && (-TWO_63..TWO_63).contains(&y) {
        Some(y as i64)
    } else {
        None
    }
}

/// `#RRGGBBAA`'s alpha → transparency 0..=100 per `pin:02-color-alpha`; the endpoints are
/// documented (`00` → 100, `FF` → 0).
pub(crate) const fn alpha_to_transp(aa: u8, rd: &Readings) -> u8 {
    let num = (255 - aa as u32) * 100;
    match rd.pins.color_alpha {
        ColorAlpha::RoundHalfUp => ((num + 127) / 255) as u8,
        ColorAlpha::Truncate => (num / 255) as u8,
    }
}

/// A colour literal `#RRGGBB[AA]` (no `AA` is opaque).
pub(crate) const fn color_literal(r: u8, g: u8, b: u8, aa: Option<u8>, rd: &Readings) -> i64 {
    let t = match aa {
        Some(a) => alpha_to_transp(a, rd),
        None => 0,
    };
    color::pack(r, g, b, t)
}
