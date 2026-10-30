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

/// `pin:03-float-mod` / `pin:02-fold-sig15-ties` (CAR-R33-4 as amended by r39, TradingView F098
/// and F124): every constant-folded float `+ − * / %` is the VM's result rounded to **16 decimal
/// places** — `0.1 + 0.2` is `0.3`, `0.3 − 0.1` is `0.2`, `1.0 % 0.1` is `0.1`, `10.0 % 0.1` is
/// `0.0999999999999995`, while `1.0 / 3.0` stays `0.3333333333333333` and
/// `1234567890123.125 % 1e15` stays `1234567890123.125` (16 significant digits: r33's
/// "15 significant digits" fits F098 only because at 0.1 the two rules coincide). The same
/// operations on series keep the exact IEEE result (F098's series columns, F125). For `|x| ≥ 1` the
/// 1e-16 grid is finer than half an ulp, so the rounding returns `x` itself and is skipped.
/// Non-finite values pass through. A tie at the 17th decimal takes Rust's correctly rounded
/// `{:.16}` (half to even; the captures hold no such tie).
pub(crate) fn dec16(x: f64) -> f64 {
    if !x.is_finite() || x.abs() >= 1.0 {
        return x;
    }
    format!("{x:.16}").parse::<f64>().unwrap_or(x)
}

/// U15's rule on a float result.
fn non_finite(r: f64, rd: &Readings) -> f64 {
    if rd.float_div_zero == swr::FLOAT_DIV_ZERO_NA && !r.is_finite() {
        f64::NAN
    } else {
        r
    }
}

/// `2^63` as an `f64`: `F_INT`'s clamp, and the one binary64 `int` beyond `i64` a `K::Int` holds
/// (as `i64::MAX`, whose binary64 is `2^63`).
const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// CAR-R33-5 (r39, R-P28): an `int` is a binary64 — a `K::Int(v)` stands for `v as f64`. The
/// representative of the integral binary64 `x`: `x as i64` inside `(−2^63, 2^63)`, `i64::MAX` for
/// `2^63`, `i64::MIN` for `−2^63`; `None` beyond (and for NaN, ±inf, a fraction) — the fold is then
/// left to the VM, whose f64 lane holds any binary64 (TradingView F037: `3037000500 *
/// 3037000500` = 9223372037000249344).
pub(crate) fn int_rep(x: f64) -> Option<i64> {
    if !x.is_finite() || x.fract() != 0.0 || !(-TWO_63..=TWO_63).contains(&x) {
        return None;
    }
    Some(if x >= TWO_63 { i64::MAX } else { x as i64 })
}

/// `+` of two `int`s: binary64, round to nearest (F047 F120: `2^53 + 1` is `2^53`), no wrap
/// (F011: `9223372036854775807 + syni` is `2^63`). `None`: not foldable (beyond `±2^63`).
pub(crate) fn i_add(a: i64, b: i64) -> Option<i64> {
    int_rep(a as f64 + b as f64)
}

/// `−` of two `int`s (binary64).
pub(crate) fn i_sub(a: i64, b: i64) -> Option<i64> {
    int_rep(a as f64 - b as f64)
}

/// `*` of two `int`s (binary64).
pub(crate) fn i_mul(a: i64, b: i64) -> Option<i64> {
    int_rep(a as f64 * b as f64)
}

/// Unary `−` of an `int` (binary64: `−(−2^63)` is `2^63`).
pub(crate) fn i_neg(a: i64) -> Option<i64> {
    int_rep(-(a as f64))
}

/// The result of `const int / const int` (W1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum IntDiv {
    /// `const_trunc` (v5): the binary64 quotient truncated (`F_DIV` + `F_INT`); a zero divisor is
    /// `na`.
    Int(Option<i64>),
    /// `fraction` (v6): the float quotient, U15 applied.
    Float(f64),
}

/// `a / b` on two `const int`s under `Sw::IntDiv`.
pub(crate) fn i_div(a: i64, b: i64, rd: &Readings) -> IntDiv {
    if rd.int_div == swr::INT_DIV_CONST_TRUNC {
        IntDiv::Int(if b == 0 {
            None
        } else {
            int_rep((a as f64 / b as f64).trunc() + 0.0)
        })
    } else {
        IntDiv::Float(f_div(a as f64, b as f64, rd))
    }
}

/// Pine's truncated int `%` (CAR-R31-1; TradingView F004 / F115, `pin:06-mod-sign`) in binary64:
/// the exact `fmod` of the two values (`F_MOD`), the sign of the dividend; `b == 0` is `na`.
/// `i_mod(-1, 100) == Some(-1)`.
pub(crate) fn i_mod(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    int_rep((a as f64) % (b as f64) + 0.0)
}

/// `int(x)` of a float per `pin:02-int-cast` (`F_INT`, CAR-R33-5): `na` for non-finite, else
/// clamped to `±2^63` (TradingView F120: `int(1e30)` is 9223372036854775808).
pub(crate) fn int_of_float(x: f64, rd: &Readings) -> Option<i64> {
    let y = match rd.pins.int_cast {
        IntCast::Trunc => x.trunc(),
        IntCast::RoundHalfAway => x.round(),
    };
    if y.is_finite() {
        int_rep(y.clamp(-TWO_63, TWO_63))
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
    if y.is_finite() {
        int_rep(y.clamp(-TWO_63, TWO_63))
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
