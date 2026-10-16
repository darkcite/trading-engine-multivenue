// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Number rendering (impl-04 §4.13.3, T12): the exact decimal renderer and `str.tostring` of
//! `int`, `float` and `bool`, with and without a `format` (a number pattern or a `format.*`
//! keyword).
//!
//! A finite `f64` is `m × 2^e`; the renderer forms `round(|x| × 10^F)` **exactly** — `u128` while
//! it fits, a stack bignum (`[u32; 36]`, 1152 bits ≥ 2^1084 = 2^1024 × 10^18, the spec's width)
//! beyond — and rounds half-even on the exact remainder (`pin:04-string-edges`). Never `x × 10^d`
//! in floating point (double rounding). No `format!`, no `ryu`, no heap. A number is first *laid
//! out* ([`Laid`]: its digits in a ≤ 512-byte stack scratch, impl-04 §4.13.3, plus the facts that
//! fix its length), then the body takes exactly that many bytes from the arena (`str_alloc`) and
//! [`Laid::write`] renders sign, digits, grouping and point straight into the lent range — no
//! intermediate output buffer, no second copy.

use pine_ir::{FaultKind, SiteId};
use pine_vm::{Frame, StrPiece};

use crate::load::NumPat;

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
    /// The shape a compiled number pattern applies.
    pub(crate) const fn of(p: &NumPat) -> Shape {
        Shape {
            min_int: p.min_int,
            min_frac: p.min_frac,
            max_frac: p.max_frac,
            group: p.group,
            percent: p.percent != 0,
        }
    }
}

/// Bignum width in 32-bit words: `m < 2^53` shifted by `e ≤ 971` then × 10^18 is < 2^1084, at
/// most 34 words; `shl` needs one word of headroom above the shifted top.
const BIG_W: usize = 36;

/// Little-endian base-2^32 stack bignum (144 B, built in place by the caller).
struct Big {
    w: [u32; BIG_W],
    n: usize,
}

impl Big {
    /// Load `v` into this (zeroed) bignum.
    fn set_u64(&mut self, v: u64) {
        debug_assert!(self.n == 0);
        self.w[0] = v as u32;
        self.w[1] = (v >> 32) as u32;
        self.n = 2;
        self.trim();
    }
    fn trim(&mut self) {
        while self.n > 0 && self.w[self.n - 1] == 0 {
            self.n -= 1;
        }
    }
    /// Shift left by `s` bits in place, top word first (word `i` lands on `i + q` and `i + q + 1`,
    /// both at or above every word still to be read).
    fn shl(&mut self, s: u32) {
        let (q, r) = ((s / 32) as usize, s % 32);
        let n = self.n;
        debug_assert!(n + q < BIG_W);
        self.w[n + q] = 0;
        let mut i = n;
        while i > 0 {
            i -= 1;
            let v = (self.w[i] as u64) << r;
            self.w[i + q + 1] |= (v >> 32) as u32;
            self.w[i + q] = v as u32;
        }
        let mut j = 0usize;
        while j < q {
            self.w[j] = 0;
            j += 1;
        }
        self.n = n + q + 1;
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
            debug_assert!(self.n < BIG_W);
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
/// Bound: a value < 2^1084 has ≤ 327 digits; the integer padding needs ≤ 255 + 16 + 1.
const DIG: usize = 336;
const _: () = assert!(DIG >= 327 && DIG > crate::load::NUM_MAX_INT as usize + 16 && DIG <= 512);

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
    let mut b = Big {
        w: [0; BIG_W],
        n: 0,
    };
    b.set_u64(m);
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

/// A number to render: the exact value of an `f64` or an `i64`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Num {
    /// A float (NaN renders `"NaN"`).
    F(f64),
    /// An int.
    I(i64),
}

/// A number laid out for output: its digits, computed once and exactly, and the facts that fix
/// its length. [`Laid::len`] sizes it, [`Laid::write`] renders it straight into the final range.
/// One per body call (a stack local the caller owns; every `set_*` redefines it whole).
pub(crate) struct Laid {
    d: [u8; DIG],
    /// A fixed word (`NaN`, `Infinity`) in place of digits; empty for a finite number.
    word: &'static [u8],
    /// A `-` goes first.
    neg: bool,
    /// Integer digits are `d[a..int_end]`, fraction digits `d[int_end..fe]`.
    a: usize,
    int_end: usize,
    fe: usize,
    group: usize,
}

impl Laid {
    /// The initial value (a caller's stack local, built in place).
    pub(crate) const EMPTY: Laid = Laid {
        d: [b'0'; DIG],
        word: b"",
        neg: false,
        a: DIG - 1,
        int_end: DIG,
        fe: DIG,
        group: 0,
    };

    /// Lay out `±(m × 2^e)` (`neg` = the sign) under `sh`.
    fn set_parts(&mut self, neg: bool, m: u64, e: i32, sh: Shape) {
        debug_assert!(sh.max_frac <= 16 && sh.min_frac <= sh.max_frac);
        let fr = sh.max_frac as usize;
        let scale = sh.max_frac as u32 + if sh.percent { 2 } else { 0 };
        let mut a = if m == 0 {
            self.d[DIG - 1] = b'0';
            DIG - 1
        } else {
            scaled_digits(m, e, scale, &mut self.d)
        };
        // At least fr + 1 digits, zero-padded on the left.
        while DIG - a < fr + 1 {
            a -= 1;
            self.d[a] = b'0';
        }
        let int_end = DIG - fr;
        // Fraction: trim trailing zeros down to min_frac.
        let mut fe = DIG;
        while fe > int_end + sh.min_frac as usize && self.d[fe - 1] == b'0' {
            fe -= 1;
        }
        // Integer: strip leading zeros, keep max(min_int, 1) digits (min_int <= 255 < DIG - 17).
        let keep = (sh.min_int as usize).max(1);
        while int_end - a > keep && self.d[a] == b'0' {
            a += 1;
        }
        while int_end - a < keep {
            a -= 1;
            self.d[a] = b'0';
        }
        let mut zero = true;
        let mut k = a;
        while k < fe {
            zero &= self.d[k] == b'0';
            k += 1;
        }
        self.word = b"";
        // pin:04-num-neg-zero: a value that renders as all zeros prints no sign.
        self.neg = neg && !zero;
        self.a = a;
        self.int_end = int_end;
        self.fe = fe;
        self.group = sh.group as usize;
    }

    /// Lay out an `f64`. Non-finite: `NaN` → `"NaN"` (the reference: "When x is na, the function
    /// returns \"NaN\""); ±∞ → `"Infinity"` / `"-Infinity"` (`pin:04-num-nonfinite`: the
    /// reference names no infinity).
    pub(crate) fn set_f64(&mut self, x: f64, sh: Shape) {
        if x.is_nan() {
            self.word = b"NaN";
            self.neg = false;
            return;
        }
        if x.is_infinite() {
            self.word = b"Infinity";
            self.neg = x < 0.0;
            return;
        }
        let b = x.to_bits();
        let ex = ((b >> 52) & 0x7FF) as i32;
        let fr = b & ((1u64 << 52) - 1);
        let (m, e) = if ex == 0 {
            (fr, -1074)
        } else {
            (fr | (1u64 << 52), ex - 1075)
        };
        self.set_parts(b >> 63 == 1, m, e, sh);
    }

    /// Lay out an `i64` exactly.
    pub(crate) fn set_i64(&mut self, v: i64, sh: Shape) {
        self.set_parts(v < 0, v.unsigned_abs(), 0, sh);
    }

    /// Lay out a [`Num`].
    pub(crate) fn set_num(&mut self, v: Num, sh: Shape) {
        match v {
            Num::F(x) => self.set_f64(x, sh),
            Num::I(i) => self.set_i64(i, sh),
        }
    }

    /// True when the output starts with `-`.
    #[inline]
    pub(crate) fn neg(&self) -> bool {
        self.neg
    }

    /// Bytes after the sign: the word, or the digits with grouping and point.
    pub(crate) fn body_len(&self) -> usize {
        if !self.word.is_empty() {
            return self.word.len();
        }
        let n_int = self.int_end - self.a;
        let seps = if self.group > 0 {
            (n_int - 1) / self.group
        } else {
            0
        };
        let frac = if self.fe > self.int_end {
            1 + self.fe - self.int_end
        } else {
            0
        };
        n_int + seps + frac
    }

    /// The whole output length (sign included).
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.neg as usize + self.body_len()
    }

    /// Render the body (no sign) at the start of `out` (≥ [`Laid::body_len`] bytes); returns its
    /// length.
    pub(crate) fn write_body(&self, out: &mut [u8]) -> usize {
        if !self.word.is_empty() {
            let n = self.word.len();
            // COPY: a fixed word (NaN, Infinity) into the output range, <= 8 B — it is part of the
            // one contiguous result, between a sign / prefix and a suffix in the same range — a
            // StrPiece::Bytes concat rejected: the affixes are pattern bytes rendered in place.
            out[..n].copy_from_slice(self.word);
            return n;
        }
        let (a, int_end, fe, g) = (self.a, self.int_end, self.fe, self.group);
        let n_int = int_end - a;
        let mut w = 0usize;
        let mut k = 0usize;
        // COPY: the digits from the digit scratch into the output range, sign / grouping / point
        // interleaved, <= 327 digits + separators — the exact conversion emits them least
        // significant first and the trimmed length is known only once all are out — converting
        // twice (once to size, once straight into the range) rejected: doubles the bignum work.
        while k < n_int {
            if g > 0 && k > 0 && (n_int - k) % g == 0 {
                out[w] = b',';
                w += 1;
            }
            out[w] = self.d[a + k];
            w += 1;
            k += 1;
        }
        if fe > int_end {
            out[w] = b'.';
            w += 1;
            k = int_end;
            // COPY: the fraction digits from the digit scratch into the output range, <= 16 B —
            // the trimmed fraction is known only once the exact conversion is out — converting
            // twice rejected: doubles the bignum work.
            while k < fe {
                out[w] = self.d[k];
                w += 1;
                k += 1;
            }
        }
        w
    }

    /// Render the whole number (sign included) at the start of `out` (≥ [`Laid::len`] bytes);
    /// returns its length.
    pub(crate) fn write(&self, out: &mut [u8]) -> usize {
        let s = self.neg as usize;
        if self.neg {
            out[0] = b'-';
        }
        s + self.write_body(&mut out[s..])
    }
}

/// The literal bytes of a pattern's prefix or suffix `t[a..b]`, unquoted (`''` → `'`, `'…'` →
/// its contents), written at the start of `out` when `write` (else only counted); returns the
/// length.
fn unquote(t: &[u8], a: usize, b: usize, out: &mut [u8], write: bool) -> usize {
    let mut w = 0usize;
    let mut k = a;
    while k < b {
        let c = t[k];
        if c == b'\'' {
            if k + 1 < b && t[k + 1] == b'\'' {
                if write {
                    out[w] = b'\'';
                }
                w += 1;
                k += 2;
                continue;
            }
            k += 1;
            continue;
        }
        if write {
            // COPY: one literal prefix/suffix byte into the output range, <= pattern length — the
            // affix surrounds rendered digits in one contiguous value — piecewise StrPieces
            // rejected: quoting splits an affix into an unbounded number of pieces.
            out[w] = c;
        }
        w += 1;
        k += 1;
    }
    w
}

/// Bytes of `l` under the compiled pattern `p` (affix ranges index `t`): sign, unquoted prefix,
/// body, unquoted suffix (DecimalFormat's negative form, `-` + prefix + digits + suffix).
pub(crate) fn pat_len(l: &Laid, p: &NumPat, t: &[u8]) -> usize {
    let (pa, pb) = (p.pre_off as usize, (p.pre_off + p.pre_len) as usize);
    let (sa, sb) = (p.suf_off as usize, (p.suf_off + p.suf_len) as usize);
    l.neg() as usize
        + unquote(t, pa, pb, &mut [], false)
        + l.body_len()
        + unquote(t, sa, sb, &mut [], false)
}

/// Render `l` under `p` straight into `out` (≥ [`pat_len`] bytes); returns the length.
pub(crate) fn write_pat(l: &Laid, p: &NumPat, t: &[u8], out: &mut [u8]) -> usize {
    let (pa, pb) = (p.pre_off as usize, (p.pre_off + p.pre_len) as usize);
    let (sa, sb) = (p.suf_off as usize, (p.suf_off + p.suf_len) as usize);
    let mut w = 0usize;
    if l.neg() {
        out[0] = b'-';
        w = 1;
    }
    w += unquote(t, pa, pb, &mut out[w..], true);
    w += l.write_body(&mut out[w..]);
    w + unquote(t, sa, sb, &mut out[w..], true)
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

/// Take exactly `l.len()` scratch bytes, render `l` into them, and store the ref into entry `out`.
#[inline]
fn emit(f: &mut Frame<'_>, out: u16, l: &Laid) -> Result<(), FaultKind> {
    let (r, dst) = f.str_alloc(l.len() as u32)?;
    let w = l.write(dst);
    debug_assert!(w == dst.len());
    f.set_i64_arg(out, Some(r.0 as i64));
    Ok(())
}

/// Store a fixed word as one scratch string into entry `out` (`pine-vm`'s one copy).
#[inline]
fn emit_word(f: &mut Frame<'_>, out: u16, w: &[u8]) -> Result<(), FaultKind> {
    let r = f.str_concat(&[StrPiece::Bytes(w)])?;
    f.set_i64_arg(out, Some(r.0 as i64));
    Ok(())
}

/// `str.tostring(int)` — window `[value, out]`: decimal digits; na → `"NaN"` (the reference).
pub fn bi_tostring_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let Some(v) = f.i64_arg(args[0]) else {
        return emit_word(f, args[1], b"NaN");
    };
    let mut l = Laid::EMPTY;
    l.set_i64(v, Shape::upto(0));
    emit(f, args[1], &l)
}

/// `str.tostring(float)` — window `[value, out]`: the default rendering (`Sw::TostringDefault`:
/// up to 8 — or 10 — fraction digits, trailing zeros trimmed, half-even); na → `"NaN"`.
pub fn bi_tostring_f(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let x = f.f64_arg(args[0]);
    if x.is_nan() {
        return emit_word(f, args[1], b"NaN");
    }
    let mut l = Laid::EMPTY;
    l.set_f64(x, default_shape(f));
    emit(f, args[1], &l)
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
    emit_word(f, args[1], w)
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

/// What follows the digits of a `str.tostring(x, format)` result.
#[derive(Clone, Copy)]
enum Affix {
    /// Nothing (`format.inherit` / `volume` / `mintick` / `price`).
    None,
    /// A `%` (`format.percent`).
    Pct,
    /// A compiled number pattern; its affix ranges index the `format` string.
    Pat(NumPat),
}

/// Lay out `v` under the `format` argument `fmt`: a `format.*` keyword or a number pattern
/// compiled now (allocation-free). `None` for a pattern outside the subset.
fn lay_fmt(f: &Frame<'_>, v: Num, fmt: &[u8], l: &mut Laid) -> Option<Affix> {
    match fmt {
        // pin:04-format-keywords: `format.inherit` / `format.volume` → the default rendering.
        b"inherit" | b"volume" => {
            l.set_num(v, default_shape(f));
            Some(Affix::None)
        }
        // The reference: "rounded to the nearest number that can be divided by syminfo.mintick
        // without the remainder. The string is returned with trailing zeros." Snapped in f64
        // (`pin:04-format-mintick`), rendered exactly at the tick's decimals.
        b"mintick" | b"price" => {
            let t = f.meta().chart.mintick_1e6;
            let dec = tick_decimals(t);
            let v = match v {
                Num::F(x) => x,
                Num::I(i) => i as f64,
            };
            let v = if fmt == &b"mintick"[..] && t > 0 && v.is_finite() {
                let tick = t as f64 / 1e6;
                (v / tick).round_ties_even() * tick
            } else {
                v
            };
            // pin:04-format-keywords: `format.price` → the tick's decimals, no snapping.
            l.set_f64(v, Shape::fixed(dec));
            Some(Affix::None)
        }
        // pin:04-format-keywords: `format.percent` → the default rendering + "%", no × 100.
        b"percent" => {
            l.set_num(v, default_shape(f));
            Some(Affix::Pct)
        }
        _ => {
            let p = crate::load::num_pat_of(fmt).ok()?;
            l.set_num(v, Shape::of(&p));
            Some(Affix::Pat(p))
        }
    }
}

/// `str.tostring(float, format)` (`INT` false) / `str.tostring(int, format)` — window `[value,
/// format, out]`. na value → `"NaN"`; na format → na; a run-time pattern outside the subset is
/// `Err(ScriptRefused)` (`pin:04-tostring-bad-pattern`). One sizing, one `str_alloc` of the exact
/// length (lent with the `format` string for a pattern's affixes), one render in place; a result
/// past the arena is `StrArenaFull` (all-or-nothing).
pub fn bi_tostring_fmt<const INT: bool>(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let mut l = Laid::EMPTY;
    let (n, affix) = {
        let v = if INT {
            f.i64_arg(args[0]).map(Num::I)
        } else {
            let x = f.f64_arg(args[0]);
            (!x.is_nan()).then_some(Num::F(x))
        };
        let Some(fmt) = f.str_arg(args[1]) else {
            // pin:04-tostring-na-format: an na `format` → na (the reference names no result).
            f.set_i64_arg(args[2], None);
            return Ok(());
        };
        let Some(v) = v else {
            // The reference: "When x is na, the function returns \"NaN\"" — no affixes.
            return emit_word(f, args[2], b"NaN");
        };
        let Some(affix) = lay_fmt(f, v, fmt, &mut l) else {
            return Err(FaultKind::ScriptRefused);
        };
        let n = match affix {
            Affix::None => l.len(),
            Affix::Pct => l.len() + 1,
            Affix::Pat(p) => pat_len(&l, &p, fmt),
        };
        (n, affix)
    };
    if n > u32::MAX as usize {
        return Err(FaultKind::StrArenaFull);
    }
    let r = match affix {
        Affix::Pat(p) => {
            let (r, dst, [fmt]) = f.str_alloc_with(n as u32, [args[1]])?;
            let Some(fmt) = fmt else {
                return Err(FaultKind::Internal);
            };
            let w = write_pat(&l, &p, fmt, dst);
            debug_assert!(w == n);
            r
        }
        Affix::None | Affix::Pct => {
            let (r, dst) = f.str_alloc(n as u32)?;
            let w = l.write(dst);
            if matches!(affix, Affix::Pct) {
                dst[w] = b'%';
            }
            debug_assert!(w + matches!(affix, Affix::Pct) as usize == n);
            r
        }
    };
    f.set_i64_arg(args[2], Some(r.0 as i64));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, sh: Shape) -> String {
        let mut l = Laid::EMPTY;
        l.set_f64(x, sh);
        let mut b = vec![0xAAu8; l.len() + 8];
        let n = l.write(&mut b);
        assert_eq!(n, l.len(), "len and write agree for {x:e}");
        assert!(b[n..].iter().all(|&c| c == 0xAA), "no write past len");
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
        let mut l = Laid::EMPTY;
        l.set_i64(i64::MIN, Shape::upto(8));
        let mut b = [0u8; 32];
        let n = l.write(&mut b);
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
