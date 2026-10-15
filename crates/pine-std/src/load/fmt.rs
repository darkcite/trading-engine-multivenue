// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `str.format` templates (impl-04 §4.13.3): `fmt_compile` and the number-pattern compiler.
//!
//! A template is literal text and placeholders
//! `{n[,number[,pattern|integer|currency|percent]]|,date[,pattern]|,time[,pattern]}` with
//! MessageFormat apostrophe quoting (`''` is one apostrophe; `'…'` quotes braces; an unmatched
//! apostrophe quotes the rest) — <https://www.tradingview.com/pine-script-docs/concepts/strings/>.
//! Segments and patterns are ranges of the template: nothing is copied.

use super::{code, err, LoadErr};

/// Segment capacity of a compiled template.
pub const FMT_MAX_SEGMENTS: usize = 32;
const MAX_PATS: usize = 8;
const MAX_ARG: u32 = 14;
const MAX_FRAC: u8 = 16;

/// Segment kinds.
mod seg {
    pub const LIT: u8 = 0;
    pub const ARG: u8 = 1;
    pub const NUMBER: u8 = 2;
    pub const INTEGER: u8 = 3;
    pub const CURRENCY: u8 = 4;
    pub const PERCENT: u8 = 5;
    pub const NUMPAT: u8 = 6;
    pub const DATE: u8 = 7;
    pub const TIME: u8 = 8;
}

/// One segment: a literal range (`off`, `len`) of the template, or a placeholder of `kind` on
/// argument `arg` with number pattern `pat` (`NUMPAT`) or a date pattern range (`DATE`/`TIME`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
struct FmtSeg {
    kind: u8,
    arg: u8,
    pat: u8,
    _pad: u8,
    off: u16,
    len: u16,
}
const _: () = assert!(core::mem::size_of::<FmtSeg>() == 8 && core::mem::align_of::<FmtSeg>() == 2);

/// A compiled number pattern (`#`, `0`, `.`, `,` grouping, `%`, literal prefix/suffix as
/// template ranges, apostrophe quoting).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
struct NumPat {
    min_int: u8,
    min_frac: u8,
    max_frac: u8,
    group: u8,
    percent: u8,
    _pad: [u8; 3],
    pre_off: u16,
    pre_len: u16,
    suf_off: u16,
    suf_len: u16,
}
const _: () = assert!(core::mem::size_of::<NumPat>() == 16 && core::mem::align_of::<NumPat>() == 2);

/// A pre-parsed `str.format` template: ≤ 32 segments, ≤ 8 number patterns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct FmtProg {
    segs: [FmtSeg; FMT_MAX_SEGMENTS],
    pats: [NumPat; MAX_PATS],
    n_segs: u8,
    n_pats: u8,
    _pad: [u8; 6],
}
const _: () = assert!(core::mem::size_of::<FmtProg>() == 392);
const _: () = assert!(core::mem::align_of::<FmtProg>() == 2);

impl FmtProg {
    /// Number of segments.
    #[must_use]
    pub fn segments(&self) -> usize {
        self.n_segs as usize
    }

    /// The highest argument index any placeholder names, `None` without placeholders (sema checks
    /// it against the call's argument count).
    #[must_use]
    pub fn max_arg(&self) -> Option<u8> {
        let mut m: Option<u8> = None;
        let mut i = 0usize;
        while i < self.n_segs as usize {
            let s = self.segs[i];
            if s.kind != seg::LIT && m.is_none_or(|x| s.arg > x) {
                m = Some(s.arg);
            }
            i += 1;
        }
        m
    }
}

const fn bad(at: usize, why: &'static str) -> LoadErr {
    err(code::BAD_FORMAT, at, why)
}

struct C<'t> {
    t: &'t [u8],
    p: FmtProg,
}

impl C<'_> {
    fn push(&mut self, s: FmtSeg, at: usize) -> Result<(), LoadErr> {
        if self.p.n_segs as usize == FMT_MAX_SEGMENTS {
            return Err(bad(at, "more than 32 template segments"));
        }
        self.p.segs[self.p.n_segs as usize] = s;
        self.p.n_segs += 1;
        Ok(())
    }

    fn lit(&mut self, from: usize, to: usize) -> Result<(), LoadErr> {
        if to > from {
            self.push(
                FmtSeg {
                    kind: seg::LIT,
                    off: from as u16,
                    len: (to - from) as u16,
                    ..FmtSeg::default()
                },
                from,
            )?;
        }
        Ok(())
    }

    /// The end of a style sub-pattern starting at `i`: the matching `}` outside quotes.
    fn style_end(&self, i: usize) -> Result<usize, LoadErr> {
        let mut k = i;
        let mut q = false;
        while k < self.t.len() {
            match self.t[k] {
                b'\'' => q = !q,
                b'{' if !q => return Err(bad(k, "nested braces in a placeholder style")),
                b'}' if !q => return Ok(k),
                _ => {}
            }
            k += 1;
        }
        Err(bad(i, "unbalanced '{'"))
    }

    fn placeholder(&mut self, open: usize) -> Result<usize, LoadErr> {
        let t = self.t;
        let mut i = open + 1;
        let st = i;
        let mut arg = 0u32;
        while i < t.len() && t[i].is_ascii_digit() {
            arg = arg.saturating_mul(10).saturating_add((t[i] - b'0') as u32);
            i += 1;
        }
        if i == st {
            return Err(bad(st, "expected an argument index"));
        }
        if arg > MAX_ARG {
            return Err(bad(st, "argument index above 14"));
        }
        let mut s = FmtSeg {
            kind: seg::ARG,
            arg: arg as u8,
            ..FmtSeg::default()
        };
        match t.get(i) {
            Some(b'}') => {
                self.push(s, open)?;
                return Ok(i + 1);
            }
            Some(b',') => i += 1,
            _ => return Err(bad(i, "expected ',' or '}' after the argument index")),
        }
        let ty0 = i;
        while i < t.len() && t[i] != b',' && t[i] != b'}' {
            i += 1;
        }
        let ty = trim(&t[ty0..i]);
        let kind = match ty {
            b"number" => seg::NUMBER,
            b"date" => seg::DATE,
            b"time" => seg::TIME,
            _ => return Err(bad(ty0, "placeholder type must be number, date or time")),
        };
        s.kind = kind;
        match t.get(i) {
            Some(b'}') => {
                self.push(s, open)?;
                return Ok(i + 1);
            }
            Some(b',') => i += 1,
            _ => return Err(bad(open, "unbalanced '{'")),
        }
        let end = self.style_end(i)?;
        let style = trim(&t[i..end]);
        if kind == seg::NUMBER {
            s.kind = match style {
                b"integer" => seg::INTEGER,
                b"currency" => seg::CURRENCY,
                b"percent" => seg::PERCENT,
                _ => {
                    if self.p.n_pats as usize == MAX_PATS {
                        return Err(bad(i, "more than 8 number patterns"));
                    }
                    self.p.pats[self.p.n_pats as usize] = num_pat(t, i, end)?;
                    s.pat = self.p.n_pats;
                    self.p.n_pats += 1;
                    seg::NUMPAT
                }
            };
        } else {
            date_pat(t, i, end)?;
            s.off = i as u16;
            s.len = (end - i) as u16;
        }
        self.push(s, open)?;
        Ok(end + 1)
    }
}

fn trim(b: &[u8]) -> &[u8] {
    let (mut lo, mut hi) = (0usize, b.len());
    while lo < hi && b[lo] == b' ' {
        lo += 1;
    }
    while hi > lo && b[hi - 1] == b' ' {
        hi -= 1;
    }
    &b[lo..hi]
}

/// Validate a date/time pattern `t[a..b]`: letters `y M d H h m s S a E Z`, quoting, punctuation.
fn date_pat(t: &[u8], a: usize, b: usize) -> Result<(), LoadErr> {
    let mut q = false;
    let mut k = a;
    while k < b {
        let c = t[k];
        if c == b'\'' {
            q = !q;
        } else if !q && c.is_ascii_alphabetic() && !b"yMdHhmsSaEZ".contains(&c) {
            return Err(bad(k, "unsupported date pattern letter"));
        }
        k += 1;
    }
    Ok(())
}

/// Compile the number pattern `t[a..b]` (DecimalFormat subset).
fn num_pat(t: &[u8], a: usize, b: usize) -> Result<NumPat, LoadErr> {
    let mut p = NumPat::default();
    let mut k = a;
    let mut q = false;
    // Prefix: up to the first unquoted `#`, `0`, `,` or `.`.
    while k < b && (q || !matches!(t[k], b'#' | b'0' | b',' | b'.')) {
        special(t[k], q, k, &mut p)?;
        if t[k] == b'\'' {
            q = !q;
        }
        k += 1;
    }
    p.pre_off = a as u16;
    p.pre_len = (k - a) as u16;
    let num0 = k;
    let (mut digits, mut since_comma, mut comma, mut seen_zero) = (0u32, 0u32, false, false);
    while k < b && matches!(t[k], b'#' | b'0' | b',') {
        match t[k] {
            b',' => {
                comma = true;
                since_comma = 0;
            }
            b'0' => {
                seen_zero = true;
                p.min_int += 1;
                digits += 1;
                since_comma += 1;
            }
            _ => {
                if seen_zero {
                    return Err(bad(k, "'#' after '0' in the integer part"));
                }
                digits += 1;
                since_comma += 1;
            }
        }
        k += 1;
    }
    if k < b && t[k] == b'.' {
        k += 1;
        let mut hash = false;
        while k < b && matches!(t[k], b'#' | b'0') {
            if t[k] == b'0' {
                if hash {
                    return Err(bad(k, "'0' after '#' in the fraction"));
                }
                p.min_frac += 1;
            } else {
                hash = true;
            }
            p.max_frac += 1;
            if p.max_frac > MAX_FRAC {
                return Err(bad(k, "more than 16 fraction digits"));
            }
            k += 1;
        }
    }
    if digits == 0 && p.max_frac == 0 {
        return Err(bad(num0, "a number pattern needs a '#' or '0'"));
    }
    if comma {
        if since_comma == 0 {
            return Err(bad(num0, "grouping separator without digits after it"));
        }
        p.group = since_comma as u8;
    }
    let s0 = k;
    while k < b {
        if !q && matches!(t[k], b'#' | b'0' | b',' | b'.') {
            return Err(bad(k, "digit characters after the suffix began"));
        }
        special(t[k], q, k, &mut p)?;
        if t[k] == b'\'' {
            q = !q;
        }
        k += 1;
    }
    p.suf_off = s0 as u16;
    p.suf_len = (b - s0) as u16;
    Ok(p)
}

/// Prefix/suffix characters: `%` sets percent; `;`, `E`, `¤` (0xC2 0xA4), `‰` are unsupported.
fn special(c: u8, quoted: bool, k: usize, p: &mut NumPat) -> Result<(), LoadErr> {
    if quoted {
        return Ok(());
    }
    match c {
        b'%' => {
            p.percent = 1;
            Ok(())
        }
        b';' => Err(bad(k, "negative sub-patterns are unsupported")),
        b'E' => Err(bad(k, "exponent patterns are unsupported")),
        0x80..=0xFF => Err(bad(k, "non-ASCII pattern characters must be quoted")),
        _ => Ok(()),
    }
}

/// Compile a `str.format` template: 5005 for unbalanced braces, an argument index > 14, an
/// unsupported pattern letter, more than 32 segments.
pub fn fmt_compile(template: &[u8]) -> Result<FmtProg, LoadErr> {
    if template.len() > u16::MAX as usize {
        return Err(bad(0, "template longer than 65535 bytes"));
    }
    let mut c = C {
        t: template,
        p: FmtProg {
            segs: [FmtSeg::default(); FMT_MAX_SEGMENTS],
            pats: [NumPat::default(); MAX_PATS],
            n_segs: 0,
            n_pats: 0,
            _pad: [0; 6],
        },
    };
    let t = template;
    let (mut i, mut from, mut q) = (0usize, 0usize, false);
    while i < t.len() {
        match t[i] {
            b'\'' if t.get(i + 1) == Some(&b'\'') => {
                // `''` is one apostrophe: keep the first, drop the second.
                c.lit(from, i + 1)?;
                i += 2;
                from = i;
            }
            b'\'' => {
                c.lit(from, i)?;
                q = !q;
                i += 1;
                from = i;
            }
            b'{' if !q => {
                c.lit(from, i)?;
                i = c.placeholder(i)?;
                from = i;
            }
            b'}' if !q => return Err(bad(i, "unbalanced '}'")), // pin:04-fmt-brace
            _ => i += 1,
        }
    }
    c.lit(from, t.len())?;
    Ok(c.p)
}
