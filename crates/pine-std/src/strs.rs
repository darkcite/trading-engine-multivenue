// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The zero-copy `str.*` family (impl-04 §4.7, §4.13.2, T11): `str.length contains startswith
//! endswith pos substring trim`, `str.tostring(string)`, `$op.eq_str` / `$op.ne_str`. Nothing here
//! copies a byte: lengths and searches borrow the bytes, `substring` / `trim` return a view
//! (`StrRef::new(r.region(), r.off() + b0, b1 − b0)`) over the same region — a SCRATCH view keeps
//! the source's epoch bits, since `r.off() + b0` stays below the region's epoch shift.

use pine_ir::{FaultKind, SiteId, StrRef};
use pine_vm::Frame;

/// The raw `StrRef` of string entry `e`, and its bytes; na → `None`.
#[inline(always)]
fn str_ref(f: &Frame<'_>, e: u16) -> Option<StrRef> {
    f.i64_arg(e).map(|v| StrRef(v as u64))
}

/// Characters (UTF-8 scalar values) of `b`: the bytes that are not continuation bytes.
#[inline]
fn chars(b: &[u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        n += (b[i] & 0xC0 != 0x80) as usize;
        i += 1;
    }
    n
}

/// The byte index of character `c` of `b` (`c ≤ chars(b)`; `c == chars(b)` → `b.len()`).
#[inline]
fn byte_of(b: &[u8], c: usize) -> usize {
    let mut seen = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] & 0xC0 != 0x80 {
            if seen == c {
                return i;
            }
            seen += 1;
        }
        i += 1;
    }
    b.len()
}

/// The first byte index of `needle` in `hay` (a naive scan; `""` is at 0).
#[inline]
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let (n, m) = (hay.len(), needle.len());
    if m > n {
        return None;
    }
    let mut i = 0usize;
    while i + m <= n {
        let mut k = 0usize;
        while k < m && hay[i + k] == needle[k] {
            k += 1;
        }
        if k == m {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `str.length(string)` — window `[s, out]`: characters; na → na.
pub fn bi_length(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let v = f.str_arg(args[0]).map(|b| chars(b) as i64);
    f.set_i64_arg(args[1], v);
    Ok(())
}

/// `str.contains` (`K` 0), `str.startswith` (1), `str.endswith` (2) — window `[source, str,
/// out]`; an na input → `false`.
pub fn bi_match<const K: u8>(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let r = match (f.str_arg(args[0]), f.str_arg(args[1])) {
        (Some(s), Some(t)) => match K {
            0 => find(s, t).is_some(),
            1 => s.starts_with(t),
            _ => s.ends_with(t),
        },
        _ => false,
    };
    f.set_bool_arg(args[2], r);
    Ok(())
}

/// `str.pos(source, str)` — window `[source, str, out]`: the character index of the first match;
/// none → na; an na input → na; `""` is found at 0 (`pin:04-string-edges`).
pub fn bi_pos(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let v = match (f.str_arg(args[0]), f.str_arg(args[1])) {
        (Some(s), Some(t)) => find(s, t).map(|i| chars(&s[..i]) as i64),
        _ => None,
    };
    f.set_i64_arg(args[2], v);
    Ok(())
}

/// `str.substring(source, begin_pos[, end_pos])` — window `[source, begin, end, out]` (sema
/// resolves `end_pos`'s derived default, the source's length): a view of characters
/// `[begin, end)`, both clamped to `[0, length]`, `begin > end` → `""` (`pin:04-string-edges`);
/// `propagate` na.
pub fn bi_substring(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let (b, e) = (f.i64_arg(args[1]), f.i64_arg(args[2]));
    let v = match (str_ref(f, args[0]), f.str_arg(args[0]), b, e) {
        (Some(r), Some(s), Some(b), Some(e)) => {
            let n = chars(s) as i64;
            let (cb, ce) = (b.clamp(0, n) as usize, e.clamp(0, n) as usize);
            let (b0, b1) = if cb > ce {
                (0, 0)
            } else {
                (byte_of(s, cb), byte_of(s, ce))
            };
            Some(StrRef::new(r.region(), r.off() + b0 as u32, (b1 - b0) as u32).0 as i64)
        }
        _ => None,
    };
    f.set_i64_arg(args[3], v);
    Ok(())
}

/// `str.trim(source)` — window `[s, out]`: a view without the ASCII whitespace (bytes ≤ `0x20`)
/// at both ends (`pin:04-string-edges`); na → na.
pub fn bi_trim(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let v = match (str_ref(f, args[0]), f.str_arg(args[0])) {
        (Some(r), Some(s)) => {
            let (mut b0, mut b1) = (0usize, s.len());
            while b0 < b1 && s[b0] <= 0x20 {
                b0 += 1;
            }
            while b1 > b0 && s[b1 - 1] <= 0x20 {
                b1 -= 1;
            }
            Some(StrRef::new(r.region(), r.off() + b0 as u32, (b1 - b0) as u32).0 as i64)
        }
        _ => None,
    };
    f.set_i64_arg(args[1], v);
    Ok(())
}

/// `str.tostring(string)` — window `[s, out]`: its input, na included (§4.7: no copy).
pub fn bi_tostring_s(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let v = f.i64_arg(args[0]);
    f.set_i64_arg(args[1], v);
    Ok(())
}

/// `$op.eq_str` (`NE` false) / `$op.ne_str` (`NE` true) — window `[a, b, out]`: byte equality;
/// an na operand → `false` for both (`pin:04-string-edges`, `pin:06-na-compare`).
pub fn bi_eq_str<const NE: bool>(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let r = match (f.str_arg(args[0]), f.str_arg(args[1])) {
        (Some(a), Some(b)) => (a == b) != NE,
        _ => false,
    };
    f.set_bool_arg(args[2], r);
    Ok(())
}

/// The `str.tonumber` grammar (§4.13.2): surrounding ASCII spaces (`0x20`), an optional sign,
/// digits with an optional `.` fraction (at least one digit in all), an optional exponent
/// `[eE][+-]?digits` (`pin:04-string-edges`); no `inf`/`nan`/`0x`. Returns the trimmed body.
#[inline]
fn number_body(s: &[u8]) -> Option<&[u8]> {
    let (mut a, mut z) = (0usize, s.len());
    while a < z && s[a] == b' ' {
        a += 1;
    }
    while z > a && s[z - 1] == b' ' {
        z -= 1;
    }
    let b = &s[a..z];
    let mut i = 0usize;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut digits = 0usize;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let e0 = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e0 {
            return None;
        }
    }
    if i == b.len() {
        Some(b)
    } else {
        None
    }
}

/// `str.tonumber(string)` — window `[s, out]`: the float of a well-formed number (correctly
/// rounded, `core`'s parser over the borrowed bytes), else na ("not a properly formed integer or
/// floating point value"); na → na.
pub fn bi_tonumber(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let v = f
        .str_arg(args[0])
        .and_then(number_body)
        .and_then(|b| core::str::from_utf8(b).ok())
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(f64::NAN);
    f.set_f64_arg(args[1], v);
    Ok(())
}

const MATCH_NONE: [u8; 2] = crate::sw::both(pine_spec::Sw::StrMatchNone);

/// FNV-1a 64 of `b`; 0 is remapped to 1 so a cache key is never the cold `0`.
#[inline]
fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0usize;
    while i < b.len() {
        h ^= b[i] as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    if h == 0 {
        1
    } else {
        h
    }
}

/// `str.match(source, regex)` — window `[source, regex, out]`, block `RegexCache` (§4.13.4): the
/// leftmost-first match as a view of `source` (zero-copy). The pattern compiles once per distinct
/// FNV-1a key (a `series` pattern that changed recompiles); one outside the subset is
/// `Err(ScriptRefused)`. No match → `""` or na per `Sw::StrMatchNone` (`pin:U07-str-match-none`);
/// an na source or pattern → na (`pin:04-string-edges`).
pub fn bi_str_match(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let Some(pat) = f.str_arg(args[1]) else {
        f.set_i64_arg(args[2], None);
        return Ok(());
    };
    let key = fnv1a(pat);
    if f.state_ro::<crate::state::RegexCache>(site).key != key {
        let mut local = crate::load::RegexProg::EMPTY;
        crate::load::regex_compile_into(pat, &mut local).map_err(|_| FaultKind::ScriptRefused)?;
        let c = f.state::<crate::state::RegexCache>(site);
        // COPY: the freshly compiled RegexProg 2 568 B into the site's cache, once per pattern
        // change — `Frame` lends the pattern bytes and the state block only one at a time, so the
        // compile cannot write the block in place (CAR: a `Frame` accessor lending both) —
        // compiling on every call rejected: the work is O(pattern), the copy O(1) per change.
        c.prog = local;
        c.key = key;
    }
    let none_empty =
        crate::sw::pick(MATCH_NONE, f.dialect()) == pine_spec::swr::STR_MATCH_NONE_EMPTY;
    let v = match (str_ref(f, args[0]), f.str_arg(args[0])) {
        (Some(r), Some(s)) => match f.state_ro::<crate::state::RegexCache>(site).prog.find(s) {
            Some((a, z)) => {
                Some(StrRef::new(r.region(), r.off() + a as u32, (z - a) as u32).0 as i64)
            }
            None if none_empty => Some(StrRef::new(r.region(), r.off(), 0).0 as i64),
            None => None,
        },
        _ => None,
    };
    f.set_i64_arg(args[2], v);
    Ok(())
}

/// `s1 + s2` (`$op.add_str`) — window `[a, b, out]`: `str_concat` of the two refs (the one copy
/// is pine-vm's, §4.7); an na operand → na (`pin:04-string-edges`); a full arena is
/// `StrArenaFull`, nothing written.
pub fn bi_add_str(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let v = match (str_ref(f, args[0]), str_ref(f, args[1])) {
        (Some(a), Some(b)) => Some(
            f.str_concat(&[pine_vm::StrPiece::Ref(a), pine_vm::StrPiece::Ref(b)])?
                .0 as i64,
        ),
        _ => None,
    };
    f.set_i64_arg(args[2], v);
    Ok(())
}
