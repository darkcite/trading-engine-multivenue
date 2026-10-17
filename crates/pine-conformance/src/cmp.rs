// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The one series tolerance (impl-06 §3.6, §4.4.2). Compare-time code, after the loop: `format_sig`
//! allocates, the rest does not.

use crate::fixture::{Cell, Tol};

/// The monotone key of §4.4.2: `if sign { !bits } else { bits | 1 << 63 }`.
#[inline]
fn key(x: f64) -> u64 {
    let b = x.to_bits();
    if b >> 63 == 1 {
        !b
    } else {
        b | 1 << 63
    }
}

/// Monotone key `k(x) = if sign { !bits } else { bits | 1 << 63 }`; distance of keys.
pub fn ulps_between(a: f64, b: f64) -> u64 {
    key(a).abs_diff(key(b))
}

/// `v` at `sig` significant digits, round-half-even, shortest form; never `NaN`.
///
/// `pin:06-format-sig-positional` — §3.6 says "shortest form" and is silent on notation: positional
/// digits (no exponent), trailing fractional zeros and a bare trailing `.` removed, `-` kept on a
/// negative non-zero result, `inf` / `-inf` for the infinities. `sig = 0` is read as 1.
pub fn format_sig(v: f64, sig: u8) -> String {
    debug_assert!(!v.is_nan(), "format_sig: na is compared before formatting");
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sig = usize::from(sig.max(1));
    // `{:.*e}` rounds the exact binary value half-to-even at `sig` digits.
    let e = format!("{:.*e}", sig - 1, v);
    let (mant, exp) = e.split_once('e').expect("LowerExp prints an exponent");
    let exp: i32 = exp.parse().expect("LowerExp exponent is an integer");
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    if digits.bytes().all(|b| b == b'0') {
        return "0".into();
    }
    let n = digits.len() as i32;
    let mut out = String::with_capacity(digits.len() + 8);
    if neg {
        out.push('-');
    }
    if exp < 0 {
        out.push_str("0.");
        let mut z = -exp - 1;
        while z > 0 {
            out.push('0');
            z -= 1;
        }
        out.push_str(&digits);
    } else if exp + 1 >= n {
        out.push_str(&digits);
        let mut z = exp + 1 - n;
        while z > 0 {
            out.push('0');
            z -= 1;
        }
    } else {
        let at = (exp + 1) as usize;
        out.push_str(&digits[..at]);
        out.push('.');
        out.push_str(&digits[at..]);
    }
    if out.contains('.') {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
    }
    out
}

/// The result of one cell comparison.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmp {
    /// The same value (bit-identical, or a printed zero against either signed zero).
    Equal,
    /// Different bits; `within` = inside the cell's tolerance.
    Divergent {
        /// `ulps_between(expected, actual)` (`u64::MAX` for an `na` / `inf` class mismatch).
        ulps: u64,
        /// Inside the tolerance.
        within: bool,
    },
}

impl Cmp {
    /// `Equal`, or a divergence inside the tolerance.
    pub const fn ok(self) -> bool {
        match self {
            Cmp::Equal => true,
            Cmp::Divergent { within, .. } => within,
        }
    }
}

const MISMATCH: Cmp = Cmp::Divergent {
    ulps: u64::MAX,
    within: false,
};

/// The only series comparison: `na` matches only `na`, ±inf only themselves; then `tol`. A printed
/// zero (text without a leading `-`) matches either signed zero (the export cannot carry the sign).
pub fn compare_cell(expected: &Cell, actual: f64, tol: Tol) -> Cmp {
    compare_cell_fmt(expected, actual, tol, false)
}

/// [`compare_cell`] with `capture/format.tsv`'s `p_negzero` (§4.4.2): when the export prints the
/// sign of zero, a printed `0` is `+0.0` only.
///
/// `pin:06-cmp-negzero-flag` — §3.6's `compare_cell` has no format argument while §4.4.2 makes the
/// zero rule depend on `format.tsv`; the flag is this second entry point, `false` until probe-00
/// measures it.
pub fn compare_cell_fmt(expected: &Cell, actual: f64, tol: Tol, prints_negzero: bool) -> Cmp {
    let (v, text) = match expected {
        Cell::Na => {
            return if actual.is_nan() {
                Cmp::Equal
            } else {
                MISMATCH
            };
        }
        Cell::Val { v, text } => (*v, text.as_str()),
    };
    if actual.is_nan() || v.is_nan() {
        return MISMATCH;
    }
    if v.is_infinite() || actual.is_infinite() {
        return if v.to_bits() == actual.to_bits() {
            Cmp::Equal
        } else {
            MISMATCH
        };
    }
    if v.to_bits() == actual.to_bits() {
        return Cmp::Equal;
    }
    if v == 0.0 && actual == 0.0 && !prints_negzero && !text.starts_with('-') {
        return Cmp::Equal;
    }
    let ulps = ulps_between(v, actual);
    let within = match tol {
        Tol::Exact => false,
        Tol::Ulp(n) => ulps <= u64::from(n),
        Tol::Dec(sig) => {
            format_sig(actual, sig) == text || {
                // pin:06-dec-zero-bound — `⌊log₁₀|v|⌋` is undefined at v = 0: the bound is 0 there
                // (only the text arm can match a printed zero).
                // pin:06-dec-bound-f64 — evaluated in f64 exactly as §4.4.2 writes it: a fractional
                // tie whose difference is inexact (0.13 vs 0.125) falls outside (lead: exact decimal?).
                v != 0.0 && {
                    let e = v.abs().log10().floor() as i32 - i32::from(sig) + 1;
                    (actual - v).abs() <= 0.5 * 10f64.powi(e)
                }
            }
        }
    };
    Cmp::Divergent { ulps, within }
}
