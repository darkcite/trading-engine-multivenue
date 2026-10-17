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
                // pin:06-dec-bound-exact (lead ruling, retires pin:06-dec-bound-f64) — the bound is
                // evaluated in exact decimal arithmetic over the printed text and the exact decimal
                // expansion of `actual`, so a fractional tie (0.13 vs 0.125) is inside.
                v != 0.0 && dec_bound_exact(text, actual, sig)
            }
        }
    };
    Cmp::Divergent { ulps, within }
}

/// A finite decimal text as `(neg, digits, exp)`, value `± int(digits) · 10^exp` (digits 0–9, MSB
/// first). `[+-]?d*[.d*]([eE][+-]?d+)?` with at least one digit; anything else is `None`.
fn dec_parts(text: &str) -> Option<(bool, Vec<u8>, i64)> {
    let (neg, t) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (mant, e) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], t[i + 1..].parse::<i64>().ok()?),
        None => (t, 0),
    };
    let (ip, fp) = mant.split_once('.').unwrap_or((mant, ""));
    if ip.is_empty() && fp.is_empty() {
        return None;
    }
    let mut digits = Vec::with_capacity(ip.len() + fp.len());
    for b in ip.bytes().chain(fp.bytes()) {
        if !b.is_ascii_digit() {
            return None;
        }
        digits.push(b - b'0');
    }
    Some((neg, digits, e - fp.len() as i64))
}

/// Leading zeros stripped.
fn trim(v: &[u8]) -> &[u8] {
    let z = v.iter().take_while(|d| **d == 0).count();
    &v[z..]
}

fn cmp_mag(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let (a, b) = (trim(a), trim(b));
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// `digits · 10^(exp − to)` (`to ≤ exp`).
fn rescale(d: &[u8], exp: i64, to: i64) -> Vec<u8> {
    // COPY: one decimal's digits (≤ ~1.5 K bytes) per `dec` bound check, compare time only — the
    // rescaled value needs its own buffer to grow — an in-place rescale rejected (input borrowed).
    let mut v = d.to_vec();
    v.resize(d.len() + (exp - to) as usize, 0);
    v
}

/// `a + b` (`add`) or `a − b` (`a ≥ b`), digit vectors MSB first.
fn add_sub(a: &[u8], b: &[u8], add: bool) -> Vec<u8> {
    let n = a.len().max(b.len()) + 1;
    let mut out = vec![0u8; n];
    let (mut carry, mut i) = (0i16, 0usize);
    while i < n {
        let da = if i < a.len() {
            i16::from(a[a.len() - 1 - i])
        } else {
            0
        };
        let db = if i < b.len() {
            i16::from(b[b.len() - 1 - i])
        } else {
            0
        };
        let mut s = if add {
            da + db + carry
        } else {
            da - db + carry
        };
        carry = 0;
        if s >= 10 {
            s -= 10;
            carry = 1;
        } else if s < 0 {
            s += 10;
            carry = -1;
        }
        out[n - 1 - i] = s as u8;
        i += 1;
    }
    debug_assert_eq!(carry, 0, "add_sub: a < b");
    out
}

/// §4.4.2's second `dec` arm, `|a − v| ≤ 0.5 · 10^(⌊log₁₀|v|⌋ − sig + 1)`, in exact decimal:
/// `v` is the printed text, `a` the exact decimal expansion of the `f64` (≤ 1074 fractional digits,
/// so `{:.1100}` prints it without rounding). `v ≠ 0`. A text that is not a finite decimal is
/// outside (fail closed).
fn dec_bound_exact(text: &str, actual: f64, sig: u8) -> bool {
    let Some((vn, vd, ve)) = dec_parts(text) else {
        return false;
    };
    let Some((an, ad, ae)) = dec_parts(&format!("{actual:.1100}")) else {
        return false;
    };
    let vt = trim(&vd);
    if vt.is_empty() {
        return false;
    }
    // 0.5 · 10^(L − sig + 1) = 5 · 10^(L − sig), L = ⌊log₁₀|v|⌋ = digits − 1 + exp.
    let l = vt.len() as i64 - 1 + ve;
    let be = l - i64::from(sig.max(1));
    let to = ve.min(ae).min(be);
    let (v, a, b) = (
        rescale(&vd, ve, to),
        rescale(&ad, ae, to),
        rescale(&[5], be, to),
    );
    let diff = if vn == an {
        if cmp_mag(&a, &v).is_ge() {
            add_sub(&a, &v, false)
        } else {
            add_sub(&v, &a, false)
        }
    } else {
        add_sub(&a, &v, true)
    };
    cmp_mag(&diff, &b).is_le()
}
