// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The copying `str.*` family (impl-04 §4.7, §4.13.2, T13): `str.upper lower replace replace_all
//! repeat`. Each body sizes its output from borrowed bytes first, then takes ONE fresh scratch
//! range lent together with its sources (`Frame::str_alloc_with`, contract A7.2) and writes the
//! output straight into it: no stack buffer, no second pass over a copy. A request that does not
//! fit is `StrArenaFull` with nothing written (the lend is all-or-nothing). An output equal to its
//! source (nothing to change, no match, one repeat) is the source's own ref: no copy at all.
//!
//! na: every argument propagates (`na` policy `propagate` in pine-spec) — an na argument → na.

use pine_ir::{FaultKind, SiteId, StrRef};
use pine_vm::Frame;

/// The first byte index `>= from` of the non-empty `needle` in `hay`.
#[inline]
fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let (n, m) = (hay.len(), needle.len());
    debug_assert!(m > 0);
    let mut i = from;
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

/// The raw bits of string entry `e` (the source ref handed back unchanged); na → `None`.
#[inline(always)]
fn ref_bits(f: &Frame<'_>, e: u16) -> Option<i64> {
    f.i64_arg(e)
}

/// A lent range's length: the scratch arena is far below `u32::MAX`, so a larger request is
/// simply one that does not fit.
#[inline(always)]
fn fit(n: u64) -> Result<u32, FaultKind> {
    if n > u32::MAX as u64 {
        Err(FaultKind::StrArenaFull)
    } else {
        Ok(n as u32)
    }
}

/// `str.upper` (`UP`) / `str.lower` — window `[source, out]`. ASCII letters only
/// (`pin:04-case-ascii`: impl-04 §4.13.2 cites the strings page, "can only change the cases of
/// ASCII letter characters"; the reference's "all letters" does not say how non-ASCII letters
/// map), so every other byte — UTF-8 sequences included — passes through unchanged and the
/// byte length is kept. A source with no letter to change is returned as itself.
pub fn bi_case<const UP: bool>(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let Some(src) = ref_bits(f, args[0]) else {
        f.set_i64_arg(args[1], None);
        return Ok(());
    };
    let (n, change) = match f.str_arg(args[0]) {
        Some(b) => {
            let mut c = false;
            let mut i = 0usize;
            while i < b.len() {
                c |= if UP {
                    b[i].is_ascii_lowercase()
                } else {
                    b[i].is_ascii_uppercase()
                };
                i += 1;
            }
            (b.len() as u32, c)
        }
        None => {
            f.set_i64_arg(args[1], None);
            return Ok(());
        }
    };
    if !change {
        f.set_i64_arg(args[1], Some(src));
        return Ok(());
    }
    let (r, dst, [s]) = f.str_alloc_with(n, [args[0]])?;
    let Some(s) = s else {
        return Err(FaultKind::Internal);
    };
    debug_assert!(s.len() == dst.len());
    let mut i = 0usize;
    while i < dst.len() {
        // COPY: each source byte, case-mapped, into the output range, <= len(source) — the result
        // is a new string value and the source may be CONST or another variable's bytes — a view
        // rejected: a case change is not a sub-range of the source.
        dst[i] = if UP {
            s[i].to_ascii_uppercase()
        } else {
            s[i].to_ascii_lowercase()
        };
        i += 1;
    }
    f.set_i64_arg(args[1], Some(r.0 as i64));
    Ok(())
}

/// `str.replace(source, target, replacement, occurrence)` — window `[source, target,
/// replacement, occurrence, out]`: the `occurrence`-th (0 = first) non-overlapping match of
/// `target`, scanning left to right, replaced by `replacement`. No such occurrence, a negative
/// `occurrence`, or an empty `target` → the source unchanged, as itself
/// (`pin:04-replace-edges`: the reference names no result for these).
pub fn bi_replace(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let (Some(src), Some(occ)) = (ref_bits(f, args[0]), f.i64_arg(args[3])) else {
        f.set_i64_arg(args[4], None);
        return Ok(());
    };
    let (Some(s), Some(t), Some(rp)) = (f.str_arg(args[0]), f.str_arg(args[1]), f.str_arg(args[2]))
    else {
        f.set_i64_arg(args[4], None);
        return Ok(());
    };
    let (ns, nt, nr) = (s.len(), t.len(), rp.len());
    let mut at = None;
    if nt > 0 && occ >= 0 {
        let mut k = 0i64;
        let mut from = 0usize;
        while let Some(i) = find_from(s, t, from) {
            if k == occ {
                at = Some(i);
                break;
            }
            k += 1;
            from = i + nt;
        }
    }
    let Some(at) = at else {
        f.set_i64_arg(args[4], Some(src));
        return Ok(());
    };
    let n = fit(ns as u64 - nt as u64 + nr as u64)?;
    let (r, dst, [s, rp]) = f.str_alloc_with(n, [args[0], args[2]])?;
    let (Some(s), Some(rp)) = (s, rp) else {
        return Err(FaultKind::Internal);
    };
    // COPY: the source's bytes around the match and the replacement into the output range,
    // <= len(source) - len(target) + len(replacement) — the result is one contiguous new string
    // and its pieces live in other strings — a rope of views rejected (contract §3.7: a string
    // is one (off, len) value).
    dst[..at].copy_from_slice(&s[..at]);
    // COPY: the replacement into the output range after the head, <= len(replacement) — the
    // result is one contiguous new string — a rope of views rejected (contract §3.7).
    dst[at..at + nr].copy_from_slice(rp);
    // COPY: the source's tail after the match into the output range, <= len(source) - at -
    // len(target) — the result is one contiguous new string — a rope of views rejected
    // (contract §3.7).
    dst[at + nr..].copy_from_slice(&s[at + nt..]);
    f.set_i64_arg(args[4], Some(r.0 as i64));
    Ok(())
}

/// `str.replace_all(source, target, replacement)` — window `[source, target, replacement, out]`:
/// every non-overlapping match, left to right, in one sizing scan and one filling scan. No match
/// or an empty `target` → the source unchanged, as itself (`pin:04-replace-edges`).
pub fn bi_replace_all(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let Some(src) = ref_bits(f, args[0]) else {
        f.set_i64_arg(args[3], None);
        return Ok(());
    };
    let (Some(s), Some(t), Some(rp)) = (f.str_arg(args[0]), f.str_arg(args[1]), f.str_arg(args[2]))
    else {
        f.set_i64_arg(args[3], None);
        return Ok(());
    };
    let (ns, nt, nr) = (s.len(), t.len(), rp.len());
    let mut hits = 0u64;
    if nt > 0 {
        let mut from = 0usize;
        while let Some(i) = find_from(s, t, from) {
            hits += 1;
            from = i + nt;
        }
    }
    if hits == 0 {
        f.set_i64_arg(args[3], Some(src));
        return Ok(());
    }
    let n = fit(ns as u64 - hits * nt as u64 + hits * nr as u64)?;
    let (r, dst, [s, t, rp]) = f.str_alloc_with(n, [args[0], args[1], args[2]])?;
    let (Some(s), Some(t), Some(rp)) = (s, t, rp) else {
        return Err(FaultKind::Internal);
    };
    let (mut w, mut from) = (0usize, 0usize);
    while let Some(i) = find_from(s, t, from) {
        // COPY: the unmatched run before each match, then the replacement, into the output
        // range, <= the sized total — one contiguous new string from pieces of other strings —
        // a rope of views rejected (contract §3.7: a string is one (off, len) value).
        dst[w..w + (i - from)].copy_from_slice(&s[from..i]);
        w += i - from;
        // COPY: the replacement after each unmatched run into the output range, <= len(replacement)
        // per match, <= the sized total in all — one contiguous new string — a rope of views
        // rejected (contract §3.7).
        dst[w..w + nr].copy_from_slice(rp);
        w += nr;
        from = i + nt;
    }
    // COPY: the source's tail after the last match into the output range, <= len(source) —
    // one contiguous new string — a rope of views rejected (contract §3.7).
    dst[w..].copy_from_slice(&s[from..]);
    debug_assert!(w + (ns - from) == dst.len());
    f.set_i64_arg(args[3], Some(r.0 as i64));
    Ok(())
}

/// `str.repeat(source, repeat, separator)` — window `[source, repeat, separator, out]`:
/// `repeat × len + (repeat − 1) × len(sep)` bytes, checked against the arena before any write.
/// `repeat == 0` → `""` (an empty view of the source); `repeat == 1` → the source itself; a
/// negative `repeat` is `Err(ScriptRefused)` (`pin:04-repeat-negative`: the reference's "must be
/// greater than or equal to 0" — a script error, not a value).
pub fn bi_repeat(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let (Some(src), Some(k)) = (ref_bits(f, args[0]), f.i64_arg(args[1])) else {
        f.set_i64_arg(args[3], None);
        return Ok(());
    };
    let (Some(s), Some(sep)) = (f.str_arg(args[0]), f.str_arg(args[2])) else {
        f.set_i64_arg(args[3], None);
        return Ok(());
    };
    if k < 0 {
        return Err(FaultKind::ScriptRefused);
    }
    let (ls, lp) = (s.len() as u128, sep.len() as u128);
    let total = k as u128 * ls + (k as u128).saturating_sub(1) * lp;
    if k == 1 {
        f.set_i64_arg(args[3], Some(src));
        return Ok(());
    }
    if total == 0 {
        let r = StrRef(src as u64);
        f.set_i64_arg(args[3], Some(StrRef::new(r.region(), r.off(), 0).0 as i64));
        return Ok(());
    }
    let n = fit(total.min(u64::MAX as u128) as u64)?;
    let (r, dst, [s, sep]) = f.str_alloc_with(n, [args[0], args[2]])?;
    let (Some(s), Some(sep)) = (s, sep) else {
        return Err(FaultKind::Internal);
    };
    // total > 0 and total fits the arena, so k <= total + 1: the loop is arena-bounded.
    let (mut w, mut i) = (0usize, 0i64);
    while i < k {
        if i > 0 {
            // COPY: the separator between two repeats into the output range, <= the sized total
            // — one contiguous new string — a rope of views rejected (contract §3.7).
            dst[w..w + sep.len()].copy_from_slice(sep);
            w += sep.len();
        }
        // COPY: one repeat of the source into the output range, <= len(source) per repeat, <= the
        // sized total in all — one contiguous new string — a rope of views rejected (contract §3.7).
        dst[w..w + s.len()].copy_from_slice(s);
        w += s.len();
        i += 1;
    }
    debug_assert!(w == dst.len());
    f.set_i64_arg(args[3], Some(r.0 as i64));
    Ok(())
}
