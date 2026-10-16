// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Number rendering (impl-04 §4.13.3, T12): the exact decimal renderer and `str.tostring` of
//! `int`, `float` and `bool`, with and without a `format` (a number pattern or a `format.*`
//! keyword).
//!
//! A finite `f64` is `m × 2^e`; the renderer forms `round(|x| × 10^F)` **exactly** — `u128` while
//! it fits, a stack bignum (`[u32; 40]`, 1280 bits ≥ 2^1084) beyond — and rounds half-even on the
//! exact remainder (`pin:04-string-edges`). Never `x × 10^d` in floating point (double rounding).
//! No `format!`, no `ryu`, no heap: digits go into a 1 KiB stack buffer and out through
//! `Frame::str_concat` as `StrPiece::Bytes` (the one COPY is `pine-vm`'s, contract §3.7).

use pine_ir::{FaultKind, SiteId};
use pine_vm::{Frame, StrPiece};

/// Rendered-number buffer: sign + ≤ 309 integer digits + ≤ 308 group separators + `.` + ≤ 16
/// fraction digits < 1024 (a `,#` pattern groups every digit).
pub(crate) const NUM_BUF: usize = 1024;

/// How a number is laid out — the applied facts of a `NumPat` (or of the default `#.########`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shape {
    /// Integer digits at least (at least one is always printed: `0.5`, not `.5`,
    /// `pin:04-string-edges`).
    pub min_int: u8,
    /// Fraction digits kept even when zero.
    pub min_frac: u8,
    /// Fraction digits at most (the rounding position), ≤ 16.
    pub max_frac: u8,
    /// Grouping size of the integer part, 0 = none.
    pub group: u8,
    /// `%` in the pattern: the value × 100 (exact: folded into the decimal scale).
    pub percent: bool,
}

impl Shape {
    /// `#.###…` with `frac` fraction digits: the default rendering (`Sw::TostringDefault`).
    pub(crate) const fn upto(frac: u8) -> Shape {
        Shape {
            min_int: 0,
            min_frac: 0,
            max_frac: frac,
            group: 0,
            percent: false,
        }
    }
    /// `0.000…` with exactly `frac` fraction digits (trailing zeros kept).
    pub(crate) const fn fixed(frac: u8) -> Shape {
        Shape {
            min_int: 0,
            min_frac: frac,
            max_frac: frac,
            group: 0,
            percent: false,
        }
    }
}

/// Little-endian base-2^32 stack bignum.
struct Big {
    w: [u32; 40],
    n: usize,
}

impl Big {
    fn from_u64(v: u64) -> Big {
        let mut b = Big { w: [0; 40], n: 0 };
        b.w[0] = v as u32;
        b.w[1] = (v >> 32) as u32;
        b.n = 2;
        b.trim();
        b
    }
    fn trim(&mut self) {
        while self.n > 0 && self.w[self.n - 1] == 0 {
            self.n -= 1;
        }
    }
    fn shl(&mut self, s: u32) {
        let (q, r) = ((s / 32) as usize, s % 32);
        debug_assert!(self.n + q < 40);
        let mut res = [0u32; 40];
        let mut i = 0usize;
        while i < self.n {
            let v = (self.w[i] as u64) << r;
            res[i + q] |= v as u32;
            res[i + q + 1] |= (v >> 32) as u32;
            i += 1;
        }
        self.w = res;
        self.n += q + 1;
        self.trim();
    }
    fn mul_small(&mut self, k: u32) {
        let mut carry = 0u64;
        let mut i = 0usize;
        while i < self.n {
            let v = self.w[i] as u64 * k as u64 + carry;
            self.w[i] = v as u32;
            carry = v >> 32;
            i += 1;
        }
        if carry > 0 {
            debug_assert!(self.n < 40);
            self.w[self.n] = carry as u32;
            self.n += 1;
        }
    }
    /// Divide in place by `k`, returning the remainder.
    fn div_small(&mut self, k: u32) -> u32 {
        let mut rem = 0u64;
        let mut i = self.n;
        while i > 0 {
            i -= 1;
            let v = (rem << 32) | self.w[i] as u64;
            self.w[i] = (v / k as u64) as u32;
            rem = v % k as u64;
        }
        self.trim();
        rem as u32
    }
}

/// Digit scratch: the scaled integer's decimal digits, right-aligned at the end of the array.
const DIG: usize = 400;

/// Decimal digits of `round_half_even(m × 2^e × 10^f)` into `d`, right-aligned; returns the index
/// of the first digit (`DIG` − digit count; zero is the one digit `0`).
fn scaled_digits(m: u64, e: i32, f: u32, d: &mut [u8; DIG]) -> usize {
    debug_assert!(f <= 18);
    let p10 = 10u128.pow(f);
    let mut at = DIG;
    if e < 0 {
        // m × 10^f < 2^64 × 2^60: u128. A shift past its bit length rounds to 0 (below half).
        let n = m as u128 * p10;
        let s = (-e) as u32;
        let q = if s >= 127 {
            0
        } else {
            let q = n >> s;
            let rem = n & ((1u128 << s) - 1);
            let half = 1u128 << (s - 1);
            q + (rem > half || (rem == half && q & 1 == 1)) as u128
        };
        return u128_digits(q, d);
    }
    let bits = 64 - m.leading_zeros() + e as u32 + 60;
    if bits < 127 {
        return u128_digits(((m as u128) << e) * p10, d);
    }
    let mut b = Big::from_u64(m);
    b.shl(e as u32);
    let mut k = 0u32;
    while k < f {
        b.mul_small(10);
        k += 1;
    }
    // Nine digits per division; every group but the leading one is zero-padded.
    loop {
        let mut r = b.div_small(1_000_000_000);
        let last = b.n == 0;
        let mut j = 0;
        while j < 9 && (!last || r > 0 || j == 0) {
            at -= 1;
            d[at] = b'0' + (r % 10) as u8;
            r /= 10;
            j += 1;
        }
        if last {
            break;
        }
    }
    at
}

fn u128_digits(mut q: u128, d: &mut [u8; DIG]) -> usize {
    let mut at = DIG;
    loop {
        at -= 1;
        d[at] = b'0' + (q % 10) as u8;
        q /= 10;
        if q == 0 {
            return at;
        }
    }
}

/// Render `±(m × 2^e)` (`neg` = the sign) under `sh` into `out`; returns the length.
pub(crate) fn render_parts(neg: bool, m: u64, e: i32, sh: Shape, out: &mut [u8; NUM_BUF]) -> usize {
    debug_assert!(sh.max_frac <= 16 && sh.min_frac <= sh.max_frac);
    let mut d = [b'0'; DIG];
    let fr = sh.max_frac as usize;
    let scale = sh.max_frac as u32 + if sh.percent { 2 } else { 0 };
    let mut a = if m == 0 {
        DIG - 1
    } else {
        scaled_digits(m, e, scale, &mut d)
    };
    // At least fr + 1 digits: the zero padding is already in `d` (initialised to '0').
    if DIG - a < fr + 1 {
        a = DIG - fr - 1;
    }
    let int_end = DIG - fr;
    // Fraction: trim trailing zeros down to min_frac.
    let mut fe = DIG;
    while fe > int_end + sh.min_frac as usize && d[fe - 1] == b'0' {
        fe -= 1;
    }
    // Integer: strip leading zeros, keep max(min_int, 1) digits.
    let keep = (sh.min_int as usize).max(1);
    while int_end - a > keep && d[a] == b'0' {
        a += 1;
    }
    while int_end - a < keep {
        a -= 1; // `d` is '0'-filled, so this pads with zeros
    }
    let mut zero = true;
    let mut k = a;
    while k < fe {
        zero &= d[k] == b'0';
        k += 1;
    }
    let mut w = 0usize;
    // pin:04-num-neg-zero: a value that renders as all zeros prints no sign.
    if neg && !zero {
        out[w] = b'-';
        w += 1;
    }
    let n_int = int_end - a;
    k = 0;
    while k < n_int {
        if sh.group > 0 && k > 0 && (n_int - k) % sh.group as usize == 0 {
            out[w] = b',';
            w += 1;
        }
        out[w] = d[a + k];
        w += 1;
        k += 1;
    }
    if fe > int_end {
        out[w] = b'.';
        w += 1;
        k = int_end;
        while k < fe {
            out[w] = d[k];
            w += 1;
            k += 1;
        }
    }
    w
}

/// Render a finite `f64`. Non-finite: `NaN` → `"NaN"` (the reference: "When x is na, the
/// function returns \"NaN\""); ±∞ → `"Infinity"` / `"-Infinity"` (`pin:04-num-nonfinite`: the
/// reference names no infinity).
pub(crate) fn render_f64(x: f64, sh: Shape, out: &mut [u8; NUM_BUF]) -> usize {
    if x.is_nan() {
        return put(out, b"NaN");
    }
    if x.is_infinite() {
        return put(out, if x > 0.0 { b"Infinity" } else { b"-Infinity" });
    }
    let b = x.to_bits();
    let ex = ((b >> 52) & 0x7FF) as i32;
    let fr = b & ((1u64 << 52) - 1);
    let (m, e) = if ex == 0 {
        (fr, -1074)
    } else {
        (fr | (1u64 << 52), ex - 1075)
    };
    render_parts(b >> 63 == 1, m, e, sh, out)
}

/// Render an `i64` exactly.
pub(crate) fn render_i64(v: i64, sh: Shape, out: &mut [u8; NUM_BUF]) -> usize {
    render_parts(v < 0, v.unsigned_abs(), 0, sh, out)
}

pub(crate) fn put(out: &mut [u8; NUM_BUF], s: &[u8]) -> usize {
    // COPY: a fixed word ("NaN", "Infinity") into the render buffer, <= 9 B — the buffer is the
    // one output the caller hands to str_concat — a second StrPiece path for words rejected.
    out[..s.len()].copy_from_slice(s);
    s.len()
}

// ---------------------------------------------------------------------------------- bodies

const DEFAULT: [u8; 2] = crate::sw::both(pine_spec::Sw::TostringDefault);

/// The default rendering's fraction digits (`Sw::TostringDefault`, U08): 8 or 10.
#[inline(always)]
pub(crate) fn default_shape(f: &Frame<'_>) -> Shape {
    if crate::sw::pick(DEFAULT, f.dialect()) == pine_spec::swr::TOSTRING_DEFAULT_FRAC_UPTO_10 {
        Shape::upto(10)
    } else {
        Shape::upto(8)
    }
}

/// Emit `buf[..n]` as one scratch string into entry `out`.
#[inline]
fn emit(f: &mut Frame<'_>, out: u16, b: &[u8]) -> Result<(), FaultKind> {
    let r = f.str_concat(&[StrPiece::Bytes(b)])?;
    f.set_i64_arg(out, Some(r.0 as i64));
    Ok(())
}

/// `str.tostring(int)` — window `[value, out]`: decimal digits; na → `"NaN"` (the reference).
pub fn bi_tostring_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let mut buf = [0u8; NUM_BUF];
    let n = match f.i64_arg(args[0]) {
        Some(v) => render_i64(v, Shape::upto(0), &mut buf),
        None => put(&mut buf, b"NaN"),
    };
    emit(f, args[1], &buf[..n])
}

/// `str.tostring(float)` — window `[value, out]`: the default rendering (`Sw::TostringDefault`:
/// up to 8 — or 10 — fraction digits, trailing zeros trimmed, half-even); na → `"NaN"`.
pub fn bi_tostring_f(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let mut buf = [0u8; NUM_BUF];
    let n = render_f64(f.f64_arg(args[0]), default_shape(f), &mut buf);
    emit(f, args[1], &buf[..n])
}

/// `str.tostring(bool)` — window `[value, out]`: `"true"` / `"false"`; na (v5) → `"NaN"`.
pub fn bi_tostring_b(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let w: &[u8] = match f.bool_arg(args[0]) {
        Some(true) => b"true",
        Some(false) => b"false",
        None => b"NaN",
    };
    let r = f.str_concat(&[StrPiece::Bytes(w)])?;
    f.set_i64_arg(args[1], Some(r.0 as i64));
    Ok(())
}

/// Decimal places of the chart's `mintick` (`mintick_1e6`, so ≤ 6).
fn tick_decimals(t: i64) -> u8 {
    let mut d = 6u8;
    let mut t = t.unsigned_abs();
    while d > 0 && t % 10 == 0 && t != 0 {
        t /= 10;
        d -= 1;
    }
    d
}

/// The literal bytes of a pattern's prefix or suffix `t[a..b]`, unquoted (`''` → `'`, `'…'` →
/// its contents), appended to `out` at `w`; `None` when it does not fit.
fn unquote(t: &[u8], a: usize, b: usize, out: &mut [u8; NUM_BUF], mut w: usize) -> Option<usize> {
    let mut k = a;
    while k < b {
        let c = t[k];
        if c == b'\'' {
            if k + 1 < b && t[k + 1] == b'\'' {
                if w >= NUM_BUF {
                    return None;
                }
                out[w] = b'\'';
                w += 1;
                k += 2;
                continue;
            }
            k += 1;
            continue;
        }
        if w >= NUM_BUF {
            return None;
        }
        // COPY: one literal prefix/suffix byte into the render buffer, <= pattern length — the
        // affix surrounds rendered digits in one contiguous value — piecewise StrPieces rejected:
        // quoting splits an affix into an unbounded number of pieces.
        out[w] = c;
        w += 1;
        k += 1;
    }
    Some(w)
}

/// Render `x` (or the integer `iv`) under the `format` argument `fmt`: a `format.*` keyword or a
/// number pattern compiled now (allocation-free). Returns the length, or `None` for a pattern
/// outside the subset.
fn render_fmt(
    f: &Frame<'_>,
    x: Option<f64>,
    iv: Option<i64>,
    fmt: &[u8],
    out: &mut [u8; NUM_BUF],
) -> Option<usize> {
    let num = |sh: Shape, out: &mut [u8; NUM_BUF]| match (x, iv) {
        (Some(x), _) => render_f64(x, sh, out),
        (None, Some(v)) => render_i64(v, sh, out),
        (None, None) => put(out, b"NaN"),
    };
    match fmt {
        // pin:04-format-keywords: `format.inherit` / `format.volume` → the default rendering.
        b"inherit" | b"volume" => Some(num(default_shape(f), out)),
        // The reference: "rounded to the nearest number that can be divided by syminfo.mintick
        // without the remainder. The string is returned with trailing zeros." Snapped in f64
        // (`pin:04-format-mintick`), rendered exactly at the tick's decimals.
        b"mintick" | b"price" => {
            let t = f.meta().chart.mintick_1e6;
            let dec = tick_decimals(t);
            let v = match (x, iv) {
                (Some(x), _) => x,
                (None, Some(v)) => v as f64,
                (None, None) => f64::NAN,
            };
            let v = if fmt == &b"mintick"[..] && t > 0 && v.is_finite() {
                let tick = t as f64 / 1e6;
                (v / tick).round_ties_even() * tick
            } else {
                v
            };
            // pin:04-format-keywords: `format.price` → the tick's decimals, no snapping.
            Some(render_f64(v, Shape::fixed(dec), out))
        }
        // pin:04-format-keywords: `format.percent` → the default rendering + "%", no × 100.
        b"percent" => {
            let n = num(default_shape(f), out);
            out[n] = b'%';
            Some(n + 1)
        }
        _ => {
            let p = crate::load::num_pat_of(fmt).ok()?;
            match (x, iv) {
                (Some(x), _) => render_pat(Num::F(x), &p, fmt, out),
                (None, Some(v)) => render_pat(Num::I(v), &p, fmt, out),
                (None, None) => Some(put(out, b"NaN")),
            }
        }
    }
}

/// A number to render: the exact value of an `f64` or an `i64`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Num {
    /// A float (NaN renders `"NaN"`).
    F(f64),
    /// An int.
    I(i64),
}

/// Render `v` under the compiled pattern `p`, whose affix ranges index `t`. The sign goes before
/// the prefix (DecimalFormat's negative form, `-` + prefix + digits + suffix). `None` past 1 KiB.
pub(crate) fn render_pat(
    v: Num,
    p: &crate::load::NumPat,
    t: &[u8],
    out: &mut [u8; NUM_BUF],
) -> Option<usize> {
    let sh = Shape {
        min_int: p.min_int,
        min_frac: p.min_frac,
        max_frac: p.max_frac,
        group: p.group,
        percent: p.percent != 0,
    };
    let mut body = [0u8; NUM_BUF];
    let nb = match v {
        Num::F(x) => render_f64(x, sh, &mut body),
        Num::I(i) => render_i64(i, sh, &mut body),
    };
    let neg = nb > 0 && body[0] == b'-';
    let mut w = 0usize;
    if neg {
        out[0] = b'-';
        w = 1;
    }
    w = unquote(
        t,
        p.pre_off as usize,
        (p.pre_off + p.pre_len) as usize,
        out,
        w,
    )?;
    let b0 = neg as usize;
    if w + nb - b0 > NUM_BUF {
        return None;
    }
    // COPY: the rendered digits after the sign and prefix, <= 1 KiB — the prefix length is known
    // only after unquoting — rendering into an offset buffer rejected: the same bytes, one more
    // code path.
    out[w..w + nb - b0].copy_from_slice(&body[b0..nb]);
    w += nb - b0;
    unquote(
        t,
        p.suf_off as usize,
        (p.suf_off + p.suf_len) as usize,
        out,
        w,
    )
}

/// `str.tostring(float, format)` (`INT` false) / `str.tostring(int, format)` — window `[value,
/// format, out]`. na value → `"NaN"`; na format → na; a run-time pattern outside the subset (or
/// rendering past 1 KiB) is `Err(ScriptRefused)` (`pin:04-tostring-bad-pattern`).
pub fn bi_tostring_fmt<const INT: bool>(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let mut buf = [0u8; NUM_BUF];
    let n = {
        let (x, iv) = if INT {
            (None, f.i64_arg(args[0]))
        } else {
            let x = f.f64_arg(args[0]);
            (Some(x), None)
        };
        let Some(fmt) = f.str_arg(args[1]) else {
            // pin:04-tostring-na-format: an na `format` → na (the reference names no result).
            f.set_i64_arg(args[2], None);
            return Ok(());
        };
        if x.is_some_and(|v| v.is_nan()) || (INT && iv.is_none()) {
            // The reference: "When x is na, the function returns \"NaN\"" — no affixes.
            let r = f.str_concat(&[StrPiece::Bytes(b"NaN")])?;
            f.set_i64_arg(args[2], Some(r.0 as i64));
            return Ok(());
        }
        match render_fmt(f, x, iv, fmt, &mut buf) {
            Some(n) => n,
            None => return Err(FaultKind::ScriptRefused),
        }
    };
    emit(f, args[2], &buf[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, sh: Shape) -> String {
        let mut b = [0u8; NUM_BUF];
        let n = render_f64(x, sh, &mut b);
        String::from_utf8(b[..n].to_vec()).unwrap()
    }

    /// The reference: Rust's exact (`{:.N}`, Dragon-style, ties-to-even) fixed rendering,
    /// trailing zeros trimmed, at least one integer digit, no sign on an all-zero result.
    fn reference(x: f64, frac: usize) -> String {
        let s = format!("{x:.frac$}");
        let mut s = if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        };
        if s == "-0" {
            s = "0".to_string();
        }
        s
    }

    /// xorshift64*, deterministic.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// impl-04 T12: 10⁵ doubles across the whole exponent range against the exact reference, at
    /// 8 fraction digits and at a random 0..=16, plus hand-checked ties and extremes.
    #[test]
    fn number_renderer_is_exact() {
        let mut g = Rng(0x9E37_79B9_7F4A_7C15);
        let mut k = 0;
        while k < 100_000 {
            let x = match k % 4 {
                0 => f64::from_bits(g.next()),
                1 => (g.next() as i64 as f64) / (1u64 << (g.next() % 60)) as f64,
                2 => (g.next() % 2_000_000) as f64 / 1000.0 - 1000.0,
                _ => {
                    (g.next() % 1_000_000) as f64
                        * 0.125
                        * if g.next() & 1 == 0 { -1.0 } else { 1.0 }
                }
            };
            if !x.is_finite() {
                k += 1;
                continue;
            }
            let fr = (g.next() % 17) as u8;
            assert_eq!(r(x, Shape::upto(8)), reference(x, 8), "x = {x:e} @8");
            assert_eq!(
                r(x, Shape::upto(fr)),
                reference(x, fr as usize),
                "x = {x:e} @{fr}"
            );
            k += 1;
        }
        // Exact ties round half-even; double-rounding traps; extremes.
        assert_eq!(r(0.125, Shape::upto(2)), "0.12");
        assert_eq!(r(0.375, Shape::upto(2)), "0.38");
        assert_eq!(r(2.5, Shape::upto(0)), "2");
        assert_eq!(r(3.5, Shape::upto(0)), "4");
        assert_eq!(
            r(1.005, Shape::upto(2)),
            "1",
            "1.005 is 1.00499999999999989…"
        );
        assert_eq!(r(0.5, Shape::upto(8)), "0.5");
        assert_eq!(r(-0.000_000_001, Shape::upto(8)), "0");
        assert_eq!(r(f64::MAX, Shape::upto(8)), reference(f64::MAX, 0));
        assert_eq!(r(f64::MIN_POSITIVE * 1e-10, Shape::upto(16)), "0");
        assert_eq!(r(5e-324, Shape::upto(16)), "0");
        assert_eq!(r(1e300, Shape::upto(3)).len(), 301);
        let mut b = [0u8; NUM_BUF];
        let n = render_i64(i64::MIN, Shape::upto(8), &mut b);
        assert_eq!(&b[..n], b"-9223372036854775808");
    }

    /// U08 both readings: 8 and 10 digits, trailing zeros trimmed, "0.5".
    #[test]
    fn tostring_default_switch_shapes() {
        let x = 1.0 / 3.0;
        assert_eq!(r(x, Shape::upto(8)), "0.33333333");
        assert_eq!(r(x, Shape::upto(10)), "0.3333333333");
        assert_eq!(r(2.50, Shape::upto(8)), "2.5");
        assert_eq!(r(2.0, Shape::upto(10)), "2");
        assert_eq!(r(0.5, Shape::upto(10)), "0.5");
    }

    #[test]
    fn patterns_group_pad_and_percent() {
        let sh = |mi, mn, mx, g, p| Shape {
            min_int: mi,
            min_frac: mn,
            max_frac: mx,
            group: g,
            percent: p,
        };
        assert_eq!(
            r(3.99, sh(0, 0, 0, 0, false)),
            "4",
            "the reference: str.tostring(3.99, '#') is 4"
        );
        assert_eq!(
            r(1.5, sh(0, 3, 3, 0, false)),
            "1.500",
            "the reference: '#.000'"
        );
        assert_eq!(r(1234567.891, sh(1, 2, 2, 3, false)), "1,234,567.89");
        assert_eq!(r(-0.256, sh(1, 1, 1, 0, true)), "-25.6");
        assert_eq!(r(7.0, sh(3, 0, 0, 0, false)), "007");
        assert_eq!(tick_decimals(10_000), 2);
        assert_eq!(tick_decimals(250_000), 2);
        assert_eq!(tick_decimals(1_000_000), 0);
        assert_eq!(tick_decimals(1), 6);
    }
}
