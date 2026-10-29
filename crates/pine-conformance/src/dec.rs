// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Exact printed decimals — prices, sizes, money (impl-06 §3.6, §4.4.3, D18).
//!
//! A printed decimal is parsed into an integer mantissa and a scale, never through `f64`. Load-time
//! and compare-time code: `render` allocates; `parse_dec`, `scaled_half_away` and `money_ok` do not.

/// A printed decimal, exactly: `value = (neg ? -1 : 1) · m / 10^d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dec {
    /// The sign as printed (`-0.00` keeps it).
    pub neg: bool,
    /// The digits, as an integer.
    pub m: u128,
    /// Printed fractional digits.
    pub d: u8,
}

/// Why a printed decimal was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecErr {
    /// No digit at all (`""`, `"-"`).
    Empty,
    /// A byte outside `[+-]?digits[.digits]`: a separator, an exponent, a stray `.`.
    BadChar(u8),
    /// More than [`MAX_DIGITS`] digits.
    TooLong,
}

/// The longest mantissa `parse_dec` accepts (§3.6: ≤ 36 digits; `10^36 < u128::MAX`).
pub const MAX_DIGITS: usize = 36;

const fn pow10(k: u32) -> u128 {
    let mut p: u128 = 1;
    let mut i = 0;
    while i < k {
        p *= 10;
        i += 1;
    }
    p
}

/// `[+-]?digits[.digits]`, ≤ 36 digits; separators and exponents are `BadChar` (the export format
/// table says how TradingView prints, §4.7.3).
///
/// `pin:06-dec-dot-edges` — a `.` with no digit before or after it (`.5`, `5.`) is `BadChar(b'.')`:
/// the grammar requires digits on both sides and is silent on which error names it.
pub fn parse_dec(s: &[u8]) -> Result<Dec, DecErr> {
    let mut i = 0usize;
    let mut neg = false;
    if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut m: u128 = 0;
    let mut digits = 0usize;
    let mut int_digits = 0usize;
    let mut d = 0usize;
    let mut seen_dot = false;
    while i < s.len() {
        let c = s[i];
        if c.is_ascii_digit() {
            digits += 1;
            if digits > MAX_DIGITS {
                return Err(DecErr::TooLong);
            }
            m = m * 10 + u128::from(c - b'0');
            if seen_dot {
                d += 1;
            } else {
                int_digits += 1;
            }
        } else if c == b'.' && !seen_dot && int_digits > 0 {
            seen_dot = true;
        } else {
            return Err(DecErr::BadChar(c));
        }
        i += 1;
    }
    if digits == 0 {
        return Err(DecErr::Empty);
    }
    if seen_dot && d == 0 {
        return Err(DecErr::BadChar(b'.'));
    }
    // `d ≤ 36` by MAX_DIGITS, so it fits a u8.
    Ok(Dec { neg, m, d: d as u8 })
}

impl Dec {
    /// The canonical printed text: sign, at least one integer digit, exactly `d` fractional digits.
    /// `render(parse_dec(t)) == t` iff `t` has no `+` and no redundant leading zero.
    pub fn render(&self) -> String {
        let digits = self.m.to_string();
        let d = usize::from(self.d);
        let mut out = String::with_capacity(digits.len() + d + 3);
        if self.neg {
            out.push('-');
        }
        let padded = if digits.len() <= d {
            let mut p = "0".repeat(d + 1 - digits.len());
            p.push_str(&digits);
            p
        } else {
            digits
        };
        let cut = padded.len() - d;
        out.push_str(&padded[..cut]);
        if d > 0 {
            out.push('.');
            out.push_str(&padded[cut..]);
        }
        out
    }
}

/// `round_half_away(decimal × 10^scale)` in exact integer arithmetic (prices and sizes: `scale = 6`).
///
/// # Panics
/// When the scaled magnitude exceeds `i128` (a 36-digit integer at `scale ≥ 3`): no export prints
/// such a price or size, and fail-fast (DOCTRINE law 7) beats a saturated compare.
pub fn scaled_half_away(x: Dec, scale: u8) -> i128 {
    let mag: u128 = if scale >= x.d {
        x.m.checked_mul(pow10(u32::from(scale - x.d)))
            .expect("scaled_half_away: decimal × 10^scale overflows u128")
    } else {
        let p = pow10(u32::from(x.d - scale));
        let q = x.m / p;
        let r = x.m % p;
        // Half away from zero: the magnitude rounds up at exactly one half.
        if r >= p - r {
            q + 1
        } else {
            q
        }
    };
    let mag = i128::try_from(mag).expect("scaled_half_away: magnitude exceeds i128");
    if x.neg {
        -mag
    } else {
        mag
    }
}

/// R-P34 (r40, 2026-10-29) — the 2-decimal fallback: one unit of TradingView's printed money cell,
/// 0.01, at ×1e12. TradingView prints money at two decimals and drops trailing zeros, so the unit is
/// fixed here, never read from the printed text's own digit count (a printed `0` is `0.00`).
pub const FALLBACK_UNIT_1E12: i128 = 10_000_000_000;

/// R-P34 (r40) — `|ours_1e12 − printed| ≤ 0.01` ([`FALLBACK_UNIT_1E12`]), inclusive: the 2-decimal
/// fallback a trade-list money cell may pass on when [`money_ok`] (half a printed unit) does not
/// hold. A harness tolerance, logged per cell by the report; the engine is unchanged.
/// `pin:06-money-overflow` holds: an overflow is NOT within.
pub fn money_near(ours_1e12: i128, printed: Dec) -> bool {
    let k = core::cmp::max(12u32, u32::from(printed.d));
    let Ok(m) = i128::try_from(printed.m) else {
        return false;
    };
    let Some(o) = ours_1e12.checked_mul(pow10(k - 12) as i128) else {
        return false;
    };
    let Some(p) = m.checked_mul(pow10(k - u32::from(printed.d)) as i128) else {
        return false;
    };
    let p = if printed.neg { -p } else { p };
    let Some(diff) = o.checked_sub(p) else {
        return false;
    };
    let Some(unit) = FALLBACK_UNIT_1E12.checked_mul(pow10(k - 12) as i128) else {
        return false;
    };
    diff.unsigned_abs() <= unit.unsigned_abs()
}

/// `|ours_1e12 − printed| ≤ ½ · 10^(−d)`, at ×1e12, inclusive (§3.16.5 money).
///
/// Compared exactly in units of `10^−K`, `K = max(12, d)`: `2·|O − P| ≤ 10^(K−d)`.
/// `pin:06-money-overflow` — a value whose exact comparison overflows `i128` (a ≥ 27-digit money
/// figure) is reported as NOT within (fail closed: a divergence the report shows, never a silent
/// pass).
pub fn money_ok(ours_1e12: i128, printed: Dec) -> bool {
    let k = core::cmp::max(12u32, u32::from(printed.d));
    let Ok(m) = i128::try_from(printed.m) else {
        return false;
    };
    let Some(o) = ours_1e12.checked_mul(pow10(k - 12) as i128) else {
        return false;
    };
    let Some(p) = m.checked_mul(pow10(k - u32::from(printed.d)) as i128) else {
        return false;
    };
    let p = if printed.neg { -p } else { p };
    let Some(diff) = o.checked_sub(p) else {
        return false;
    };
    let Some(twice) = diff.unsigned_abs().checked_mul(2) else {
        return false;
    };
    twice <= pow10(k - u32::from(printed.d))
}
