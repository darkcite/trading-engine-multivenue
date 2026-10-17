// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `math.*` and the lane-copy cast body (impl-04 §4.13.1). Stateless; `na` propagates (every row
//! here is `NaPolicy::Propagate` in pine-spec).

use crate::args::{out1, set_num_out};
use crate::naf::{fmax, fmin};
use crate::state::{win_push, win_repoint, Prev, Rng64, WinHdr};
use crate::sw;
use pine_ir::{argwin, color, scale, FaultKind, SiteId};
use pine_spec::{swr, Sw};
use pine_vm::Frame;

const DIV0: [u8; 2] = sw::both(Sw::FloatDivZero);
const TIES: [u8; 2] = sw::both(Sw::RoundTies);

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
    // A non-finite power is na under `Sw::FloatDivZero = na` (U15, `pin:06-nonfinite`).
    let na_nonfinite = sw::pick(DIV0, f.dialect()) == swr::FLOAT_DIV_ZERO_NA;
    let r = if na_nonfinite && !r.is_finite() {
        f64::NAN
    } else {
        r
    };
    f.set_f64_arg(args[2], r);
    Ok(())
}

/// `math.abs(int)`: `i64::wrapping_abs` (§4.13.1); `na` stays `na`.
pub fn bi_abs_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let r = f.i64_arg(args[0]).map(i64::wrapping_abs);
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

/// Round `x` to an integer-valued f64 with ties per `Sw::RoundTies` (`half_up`: toward +inf,
/// `half_away`: away from zero). Exact: the fraction is `x − floor(x)`, never `x + 0.5`.
#[inline]
pub fn round_ties(x: f64, half_up: bool) -> f64 {
    if !x.is_finite() || x.abs() >= 4_503_599_627_370_496.0 {
        return x;
    }
    let r = x.floor();
    let d = x - r;
    if d > 0.5 || (d == 0.5 && (half_up || x > 0.0)) {
        r + 1.0
    } else {
        r
    }
}

/// `math.round(x) -> int`: ties per `Sw::RoundTies` (`pin:U04-round-ties`); NaN, ±inf and out of
/// range → na.
pub fn bi_round(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let x = f.f64_arg(args[0]);
    let half_up = sw::pick(TIES, f.dialect()) == swr::ROUND_TIES_HALF_UP;
    set_num_out(f, args[1], round_ties(x, half_up));
    Ok(())
}

/// `math.round(number, precision) -> float` — window `[number, precision, out]`: `number`
/// rounded to `precision` decimal places as `round(number × 10^precision) / 10^precision`, ties
/// per `Sw::RoundTies` as `math.round(x)` (`pin:04-round-precision`, CI-013, fixture F016). A na
/// `number` or `precision` → na; a negative `precision` rounds to tens, hundreds, …; a scale
/// that overflows (`|precision| > 308`) yields na through the NaN of `inf / inf` or `0 / 0`.
pub fn bi_round_prec(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let x = f.f64_arg(args[0]);
    let half_up = sw::pick(TIES, f.dialect()) == swr::ROUND_TIES_HALF_UP;
    let v = match f.i64_arg(args[1]) {
        None => f64::NAN,
        Some(p) => {
            let p = p.clamp(-400, 400) as i32;
            let scale = 10f64.powi(p);
            round_ties(x * scale, half_up) / scale
        }
    };
    f.set_f64_arg(args[2], v);
    Ok(())
}

/// `math.round_to_mintick(x)`: `to_1e6(x)` rounded to a multiple of the chart's `mintick_1e6`
/// in integer arithmetic (ties per `Sw::RoundTies`), back through `from_1e6`.
pub fn bi_round_to_mintick(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let x = f.f64_arg(args[0]);
    let m = f.meta().chart.mintick_1e6;
    if m < 1 {
        return Err(FaultKind::Internal);
    }
    let half_up = sw::pick(TIES, f.dialect()) == swr::ROUND_TIES_HALF_UP;
    let r = match scale::to_1e6(x) {
        Some(v) if x.is_finite() => {
            let (q, rem) = (v.div_euclid(m), v.rem_euclid(m));
            let up =
                2 * rem as i128 > m as i128 || (2 * rem as i128 == m as i128 && (half_up || v > 0));
            let q = if up { q + 1 } else { q };
            match q.checked_mul(m) {
                Some(t) => scale::from_1e6(t),
                None => f64::NAN,
            }
        }
        _ => f64::NAN,
    };
    f.set_f64_arg(args[1], r);
    Ok(())
}

/// `math.sum(source, length)`: the rolling sum over the site's window (`W(len)`, `skip` na).
pub fn bi_sum(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let Some(len) = f.i64_arg(args[1]) else {
        f.set_f64_arg(args[2], f64::NAN);
        return Ok(());
    };
    let out = {
        let (h, ring) = f.state_split::<WinHdr, f64>(site);
        if len < 1 || len > ring.len() as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let len = len as u32;
        if h.len != len {
            win_repoint(h, ring, len);
        }
        if !src.is_nan() {
            win_push(h, ring, src);
        }
        if h.n < len {
            f64::NAN
        } else {
            h.sum
        }
    };
    f.set_f64_arg(args[2], out);
    Ok(())
}

/// `math.random(min, max, seed)`: splitmix64 seeded from `seed` on the site's first execution;
/// `min + (max − min) × (u >> 11) × 2⁻⁵³` — replayable, **not** TradingView's stream
/// (`pin:04-math-random`).
pub fn bi_random(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let (lo, hi) = (f.f64_arg(args[0]), f.f64_arg(args[1]));
    let Some(seed) = f.i64_arg(args[2]) else {
        return Err(FaultKind::ScriptRefused);
    };
    let u = {
        let r = f.state::<Rng64>(site);
        if r.seeded == 0 {
            r.s = seed as u64;
            r.seeded = 1;
        }
        r.s = r.s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = r.s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let t = (u >> 11) as f64 * (1.0 / (1u64 << 53) as f64);
    f.set_f64_arg(args[3], (hi - lo).mul_add(t, lo));
    Ok(())
}

/// `fixnan(x)` for every lane: the last non-na value, na before any (`Prev`; an i64 value is
/// kept as its bits).
pub fn bi_fixnan(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let (a, o) = (args[0], args[1]);
    if argwin::is_i64(a) {
        let x = f.i64_arg(a);
        let r = {
            let p = f.state::<Prev>(site);
            if let Some(v) = x {
                p.v = f64::from_bits(v as u64);
                p.primed = 1;
            }
            if p.primed != 0 {
                Some(p.v.to_bits() as i64)
            } else {
                None
            }
        };
        f.set_i64_arg(o, r);
    } else {
        let x = f.f64_arg(a);
        let r = {
            let p = f.state::<Prev>(site);
            if !x.is_nan() {
                p.v = x;
                p.primed = 1;
            }
            if p.primed != 0 {
                p.v
            } else {
                f64::NAN
            }
        };
        f.set_f64_arg(o, r);
    }
    Ok(())
}

/// `$op.int_strict(x) -> int`: truncate toward zero; NaN, ±inf, `|x| ≥ 2^63` fault
/// `InvalidCast` (U05's `fault` reading).
pub fn bi_int_strict(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let x = f.f64_arg(args[0]);
    if !x.is_finite() || x.abs() >= 9_223_372_036_854_775_808.0 {
        return Err(FaultKind::InvalidCast);
    }
    f.set_i64_arg(args[1], Some(x as i64));
    Ok(())
}

/// A channel `0..=255` from a float: rounded half away from zero, clamped (`pin:04-color-edges`).
#[inline(always)]
fn chan(x: f64) -> u8 {
    let r = x.round();
    if r <= 0.0 {
        0
    } else if r >= 255.0 {
        255
    } else {
        r as u8
    }
}

/// A transparency `0..=100` from a float, rounded and clamped (`pin:04-color-edges`).
#[inline(always)]
fn transp(x: f64) -> u8 {
    let r = x.round();
    if r <= 0.0 {
        0
    } else if r >= 100.0 {
        100
    } else {
        r as u8
    }
}

/// `color.new(color, transp)`.
pub fn bi_color_new(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (c, t) = (f.i64_arg(args[0]), f.f64_arg(args[1]));
    let r = match c {
        Some(c) if !t.is_nan() => Some(color::pack(
            color::r(c),
            color::g(c),
            color::b(c),
            transp(t),
        )),
        _ => None,
    };
    f.set_i64_arg(args[2], r);
    Ok(())
}

/// `color.rgb(red, green, blue[, transp])` — window `[r, g, b, t, out]` (sema fills the default).
pub fn bi_color_rgb(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let v = [
        f.f64_arg(args[0]),
        f.f64_arg(args[1]),
        f.f64_arg(args[2]),
        f.f64_arg(args[3]),
    ];
    let any_na = v[0].is_nan() || v[1].is_nan() || v[2].is_nan() || v[3].is_nan();
    let r = if any_na {
        None
    } else {
        Some(color::pack(
            chan(v[0]),
            chan(v[1]),
            chan(v[2]),
            transp(v[3]),
        ))
    };
    f.set_i64_arg(args[4], r);
    Ok(())
}

/// `color.r/g/b/t(color)` as a float; `CH` 0..=3 selects the channel.
pub fn bi_color_chan<const CH: u8>(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let r = match f.i64_arg(args[0]) {
        Some(c) => {
            let v = match CH {
                0 => color::r(c),
                1 => color::g(c),
                2 => color::b(c),
                _ => color::t(c),
            };
            v as f64
        }
        None => f64::NAN,
    };
    f.set_f64_arg(args[1], r);
    Ok(())
}

/// `color.from_gradient(value, bottom_value, top_value, bottom_color, top_color)`: the position
/// clamped to [0, 1], per-channel linear interpolation, rounded (`pin:04-color-edges`).
pub fn bi_color_from_gradient(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let (v, lo, hi) = (f.f64_arg(args[0]), f.f64_arg(args[1]), f.f64_arg(args[2]));
    let (c1, c2) = (f.i64_arg(args[3]), f.i64_arg(args[4]));
    let r = match (c1, c2) {
        (Some(a), Some(b)) if !(v.is_nan() || lo.is_nan() || hi.is_nan()) => {
            let p = if hi == lo { 0.0 } else { (v - lo) / (hi - lo) };
            let p = if p.is_nan() || p <= 0.0 {
                0.0
            } else if p >= 1.0 {
                1.0
            } else {
                p
            };
            let mix = |x: u8, y: u8| (y as f64 - x as f64).mul_add(p, x as f64);
            Some(color::pack(
                chan(mix(color::r(a), color::r(b))),
                chan(mix(color::g(a), color::g(b))),
                chan(mix(color::b(a), color::b(b))),
                transp(mix(color::t(a), color::t(b))),
            ))
        }
        _ => None,
    };
    f.set_i64_arg(args[5], r);
    Ok(())
}
