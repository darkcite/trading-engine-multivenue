// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `str.format` and `str.format_time` (impl-04 §4.13.3, T13). One sizing pass, one lend
//! (`Frame::str_alloc_with`, contract A7.2: the fresh range together with the template and the
//! string arguments), one fill pass over the same code — so the two passes cannot disagree. The
//! template re-parses at run time with `fmt_compile` (allocation-free, O(template)); a `series`
//! template that fails is `Err(ScriptRefused)` (the reference: imbalanced braces cause "a runtime
//! error").

use pine_ir::{FaultKind, SiteId, Ty};
use pine_vm::Frame;

use crate::load::{seg, FmtProg};
use crate::strnum::{render_f64, render_i64, render_pat, Num, Shape, NUM_BUF};

const DAY_MS: i64 = 86_400_000;
const MONTHS: [&[u8]; 12] = [
    b"January",
    b"February",
    b"March",
    b"April",
    b"May",
    b"June",
    b"July",
    b"August",
    b"September",
    b"October",
    b"November",
    b"December",
];
const DAYS: [&[u8]; 7] = [
    b"Sunday",
    b"Monday",
    b"Tuesday",
    b"Wednesday",
    b"Thursday",
    b"Friday",
    b"Saturday",
];

/// The output of one pass: counting (`out` empty) or writing into the lent range.
struct Sink<'o> {
    out: &'o mut [u8],
    n: usize,
    write: bool,
}

impl Sink<'_> {
    #[inline]
    fn bytes(&mut self, b: &[u8]) {
        if self.write {
            // COPY: one output piece (a literal run of the template, a string argument, rendered
            // digits) into the lent output range, <= the sized total — a Pine string is one
            // contiguous value built from pieces of other strings — a rope of views rejected
            // (contract §3.7).
            self.out[self.n..self.n + b.len()].copy_from_slice(b);
        }
        self.n += b.len();
    }
    #[inline]
    fn byte(&mut self, c: u8) {
        if self.write {
            self.out[self.n] = c;
        }
        self.n += 1;
    }
    /// `v` in decimal, zero-padded to `width` digits (a negative value: `-` then the digits).
    fn num(&mut self, v: i64, width: usize) {
        if v < 0 {
            self.byte(b'-');
        }
        let mut d = [0u8; 20];
        let mut u = v.unsigned_abs();
        let mut k = 20usize;
        loop {
            k -= 1;
            d[k] = b'0' + (u % 10) as u8;
            u /= 10;
            if u == 0 {
                break;
            }
        }
        let mut pad = width.saturating_sub(20 - k);
        while pad > 0 {
            self.byte(b'0');
            pad -= 1;
        }
        self.bytes(&d[k..]);
    }
}

/// Render UTC ms `t` shifted by `off_s` under the date pattern `pat` (letters `y M d H h m s S a
/// E Z`, apostrophe quoting, everything else literal). `pin:04-date-tokens`: `yy` = the last two
/// digits, any other run of `y` = the year padded to the run; `M`/`MM` numeric, `MMM` = `Jan`,
/// `MMMM` = `January`; `h` = 1..12 (the reference: the last hour of the day is 11); `S` = the
/// milliseconds padded to the run; `E…EEE` = `Sun`, `EEEE` = `Sunday`; `Z` = `+HHmm` (the
/// reference). Another letter is `Err(ScriptRefused)` (the load-time check's 5005 at run time).
fn date(s: &mut Sink<'_>, t: i64, off_s: i32, pat: &[u8]) -> Result<(), FaultKind> {
    let local = t.saturating_add(off_s as i64 * 1000);
    let days = local.div_euclid(DAY_MS);
    let ms = local.rem_euclid(DAY_MS);
    let (y, mo, d) = crate::time::civil_from_days(days);
    let hh = ms / 3_600_000;
    let dow = (days + 4).rem_euclid(7) as usize; // 1970-01-01 was a Thursday
    let (mut i, mut q) = (0usize, false);
    while i < pat.len() {
        let c = pat[i];
        if c == b'\'' {
            if i + 1 < pat.len() && pat[i + 1] == b'\'' {
                s.byte(b'\'');
                i += 2;
            } else {
                q = !q;
                i += 1;
            }
            continue;
        }
        if q || !c.is_ascii_alphabetic() {
            s.byte(c);
            i += 1;
            continue;
        }
        let mut j = i;
        while j < pat.len() && pat[j] == c {
            j += 1;
        }
        let n = j - i;
        match c {
            b'y' if n == 2 => s.num(y.rem_euclid(100), 2),
            b'y' => s.num(y, n),
            b'M' if n >= 4 => s.bytes(MONTHS[mo as usize - 1]),
            b'M' if n == 3 => s.bytes(&MONTHS[mo as usize - 1][..3]),
            b'M' => s.num(mo as i64, n),
            b'd' => s.num(d as i64, n),
            b'H' => s.num(hh, n),
            b'h' => s.num(if hh % 12 == 0 { 12 } else { hh % 12 }, n),
            b'm' => s.num(ms / 60_000 % 60, n),
            b's' => s.num(ms / 1000 % 60, n),
            b'S' => s.num(ms % 1000, n),
            b'a' => s.bytes(if hh < 12 { b"AM" } else { b"PM" }),
            b'E' if n >= 4 => s.bytes(DAYS[dow]),
            b'E' => s.bytes(&DAYS[dow][..3]),
            b'Z' => {
                s.byte(if off_s < 0 { b'-' } else { b'+' });
                let a = off_s.unsigned_abs() as i64 / 60;
                s.num(a / 60, 2);
                s.num(a % 60, 2);
            }
            _ => return Err(FaultKind::ScriptRefused),
        }
        i = j;
    }
    Ok(())
}

/// One `str.format` argument, read before the lend.
#[derive(Clone, Copy)]
enum Val {
    Na,
    I(i64),
    F(f64),
    B(bool),
    /// A string: its bytes are lent with the output range.
    S,
}

/// `pin:04-fmt-keywords`: `{n,number}` / `integer` / `currency` / `percent` expand to
/// MessageFormat's US-locale patterns, the syntax the strings page follows.
const KW_NUMBER: &[u8] = b"#,##0.###";
const KW_INTEGER: &[u8] = b"#,##0";
const KW_CURRENCY: &[u8] = b"$#,##0.00";
const KW_PERCENT: &[u8] = b"#,##0%";
/// `pin:04-fmt-date-default`: `{n,date}` / `{n,time}` without a pattern — MessageFormat's
/// DEFAULT styles.
const DATE_DEFAULT: &[u8] = b"MMM d, yyyy";
const TIME_DEFAULT: &[u8] = b"h:mm:ss a";

fn put_num(s: &mut Sink<'_>, v: Num, sh: Shape) {
    let mut b = [0u8; NUM_BUF];
    let n = match v {
        Num::F(x) => render_f64(x, sh, &mut b),
        Num::I(i) => render_i64(i, sh, &mut b),
    };
    s.bytes(&b[..n]);
}

/// One pass over the compiled template `p` (whose ranges index `tpl`).
fn fill(
    s: &mut Sink<'_>,
    tpl: &[u8],
    p: &FmtProg,
    vals: &[Val; 15],
    strs: &[Option<&[u8]>; 16],
    nargs: usize,
    sh: Shape,
) -> Result<(), FaultKind> {
    let mut k = 0usize;
    while k < p.segments() {
        let g = p.segs[k];
        k += 1;
        if g.kind == seg::LIT {
            s.bytes(&tpl[g.off as usize..(g.off + g.len) as usize]);
            continue;
        }
        let a = g.arg as usize;
        if a >= nargs {
            // pin:04-fmt-missing-arg: MessageFormat leaves a placeholder without an argument as
            // the text `{n}`.
            s.byte(b'{');
            s.num(a as i64, 1);
            s.byte(b'}');
            continue;
        }
        let v = vals[a];
        let num = match v {
            Val::I(i) => Some(Num::I(i)),
            Val::F(x) => Some(Num::F(x)),
            _ => None,
        };
        match g.kind {
            seg::ARG => match v {
                // pin:04-fmt-na: an na argument renders "NaN", as `str.tostring` does.
                Val::Na => s.bytes(b"NaN"),
                Val::B(b) => s.bytes(if b { b"true" } else { b"false" }),
                Val::S => s.bytes(strs[1 + a].unwrap_or(b"NaN")),
                Val::I(i) => put_num(s, Num::I(i), Shape::upto(0)),
                Val::F(x) => put_num(s, Num::F(x), sh),
            },
            seg::DATE | seg::TIME => {
                let t = match v {
                    Val::I(i) => i,
                    Val::F(x) if x.is_finite() => x as i64,
                    Val::Na | Val::F(_) => {
                        s.bytes(b"NaN");
                        continue;
                    }
                    // pin:04-fmt-type: a date/time placeholder on a bool or string.
                    _ => return Err(FaultKind::ScriptRefused),
                };
                let pat = if g.len > 0 {
                    &tpl[g.off as usize..(g.off + g.len) as usize]
                } else if g.kind == seg::DATE {
                    DATE_DEFAULT
                } else {
                    TIME_DEFAULT
                };
                // UTC (verified, strings page).
                date(s, t, 0, pat)?;
            }
            _ => {
                let Some(x) = num else {
                    if matches!(v, Val::Na) {
                        s.bytes(b"NaN");
                        continue;
                    }
                    // pin:04-fmt-type: a number placeholder on a bool or string (MessageFormat
                    // throws).
                    return Err(FaultKind::ScriptRefused);
                };
                let (pt, pp) = match g.kind {
                    seg::NUMPAT => (tpl, p.pats[g.pat as usize]),
                    seg::INTEGER => (KW_INTEGER, kw(KW_INTEGER)?),
                    seg::CURRENCY => (KW_CURRENCY, kw(KW_CURRENCY)?),
                    seg::PERCENT => (KW_PERCENT, kw(KW_PERCENT)?),
                    _ => (KW_NUMBER, kw(KW_NUMBER)?),
                };
                let mut b = [0u8; NUM_BUF];
                let n = match x {
                    Num::F(f) if f.is_nan() => crate::strnum::put(&mut b, b"NaN"),
                    _ => render_pat(x, &pp, pt, &mut b).ok_or(FaultKind::ScriptRefused)?,
                };
                s.bytes(&b[..n]);
            }
        }
    }
    Ok(())
}

#[inline]
fn kw(t: &[u8]) -> Result<crate::load::NumPat, FaultKind> {
    crate::load::num_pat_of(t).map_err(|_| FaultKind::Internal)
}

/// `str.format(formatString, arg0, …)` — window `[__types, formatString, arg0 … argN-1, out]`;
/// `__types` holds each argument's 4-bit `Ty` (impl-04 §3.4). An na template → na. Arrays are
/// not served here (their overloads have no body; sema refuses them).
pub fn bi_str_format(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() < 3 || args.len() > 3 + 15 {
        return Err(FaultKind::Internal);
    }
    let out = args[args.len() - 1];
    let nargs = args.len() - 3;
    let types = f.i64_arg(args[0]).unwrap_or(0) as u64;
    let mut vals = [Val::Na; 15];
    let mut srcs = [args[1]; 16];
    let mut k = 0usize;
    while k < nargs {
        let e = args[2 + k];
        let ty = ((types >> (4 * k)) & 15) as u8;
        vals[k] = if ty == Ty::Int as u8 {
            f.i64_arg(e).map_or(Val::Na, Val::I)
        } else if ty == Ty::Float as u8 {
            let x = f.f64_arg(e);
            if x.is_nan() {
                Val::Na
            } else {
                Val::F(x)
            }
        } else if ty == Ty::Bool as u8 {
            f.bool_arg(e).map_or(Val::Na, Val::B)
        } else if ty == Ty::String as u8 {
            srcs[1 + k] = e;
            if f.str_arg(e).is_some() {
                Val::S
            } else {
                Val::Na
            }
        } else {
            return Err(FaultKind::Internal);
        };
        k += 1;
    }
    let sh = crate::strnum::default_shape(f);
    // Pass 1: size (borrows the template and the string arguments).
    let (prog, n) = {
        let Some(tpl) = f.str_arg(args[1]) else {
            f.set_i64_arg(out, None);
            return Ok(());
        };
        let prog = crate::load::fmt_compile(tpl).map_err(|_| FaultKind::ScriptRefused)?;
        let mut strs: [Option<&[u8]>; 16] = [None; 16];
        k = 0;
        while k < nargs {
            if matches!(vals[k], Val::S) {
                strs[1 + k] = f.str_arg(args[2 + k]);
            }
            k += 1;
        }
        let mut s = Sink {
            out: &mut [],
            n: 0,
            write: false,
        };
        fill(&mut s, tpl, &prog, &vals, &strs, nargs, sh)?;
        (prog, s.n)
    };
    if n > u32::MAX as usize {
        return Err(FaultKind::StrArenaFull);
    }
    // Pass 2: fill the lent range from the lent template and strings.
    let (r, dst, lent) = f.str_alloc_with(n as u32, srcs)?;
    let Some(tpl) = lent[0] else {
        return Err(FaultKind::Internal);
    };
    let mut s = Sink {
        out: dst,
        n: 0,
        write: true,
    };
    fill(&mut s, tpl, &prog, &vals, &lent, nargs, sh)?;
    debug_assert!(s.n == n);
    f.set_i64_arg(out, Some(r.0 as i64));
    Ok(())
}

/// `str.format_time(time, format, timezone)` — window `[time, format, timezone, out]`: the date
/// pattern of [`date`] in `timezone` (an unknown zone is `Err(ScriptRefused)`,
/// `pin:04-time-edges`); any na argument → na.
pub fn bi_format_time(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let Some(t) = f.i64_arg(args[0]) else {
        f.set_i64_arg(args[3], None);
        return Ok(());
    };
    let off = match f.str_arg(args[2]) {
        None => {
            f.set_i64_arg(args[3], None);
            return Ok(());
        }
        Some(z) => match crate::load::tz_lookup(z) {
            Some(z) => crate::load::tz_offset_s(z, t),
            None => return Err(FaultKind::ScriptRefused),
        },
    };
    let n = {
        let Some(pat) = f.str_arg(args[1]) else {
            f.set_i64_arg(args[3], None);
            return Ok(());
        };
        let mut s = Sink {
            out: &mut [],
            n: 0,
            write: false,
        };
        date(&mut s, t, off, pat)?;
        s.n
    };
    if n > u32::MAX as usize {
        return Err(FaultKind::StrArenaFull);
    }
    let (r, dst, [pat]) = f.str_alloc_with(n as u32, [args[1]])?;
    let Some(pat) = pat else {
        return Err(FaultKind::Internal);
    };
    let mut s = Sink {
        out: dst,
        n: 0,
        write: true,
    };
    date(&mut s, t, off, pat)?;
    debug_assert!(s.n == n);
    f.set_i64_arg(args[3], Some(r.0 as i64));
    Ok(())
}
