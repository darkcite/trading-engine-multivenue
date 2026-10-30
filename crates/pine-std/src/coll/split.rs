// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `str.split(string, separator)` (impl-04 §4.16.4, §4.13 row; T34): a new `array<string>` of the
//! pieces between non-overlapping occurrences of `separator`, left to right, every piece kept —
//! leading, trailing and adjacent separators give empty pieces (`pin:04-split-empties`: the
//! reference states no trimming). `separator == ""` splits into code points (`pin:04-string-edges`);
//! a `""` source with a non-empty separator is one empty piece. A `CONST` source stores `CONST`
//! views of its pieces and copies nothing; any other source's pieces are copied into the array's
//! own cells. A na source or separator gives a na array (`pin:04-split-na`: the str.* na rule).

use super::array::{declared, flags};
use super::{create, elem_ty, store, Pool, StrVal};
use pine_ir::{argwin, str_region, CollKind, FaultKind, SiteId, StrRef, Ty};
use pine_spec::ov;
use pine_vm::Frame;

/// The end of the piece starting at `at` and where the next piece starts (`None`: last piece).
/// `sep` empty: one code point per piece.
fn next(s: &[u8], sep: &[u8], at: usize) -> (usize, Option<usize>) {
    if sep.is_empty() {
        let mut e = at + 1;
        while e < s.len() && s[e] & 0xC0 == 0x80 {
            e += 1;
        }
        return (e, if e < s.len() { Some(e) } else { None });
    }
    let mut i = at;
    while i + sep.len() <= s.len() {
        if &s[i..i + sep.len()] == sep {
            return (i, Some(i + sep.len()));
        }
        i += 1;
    }
    (s.len(), None)
}

/// Pieces of `s` (0 for an empty source split into code points).
fn count(s: &[u8], sep: &[u8]) -> u64 {
    if sep.is_empty() && s.is_empty() {
        return 0;
    }
    let mut n = 0u64;
    let mut at = Some(0usize);
    while let Some(a) = at {
        at = next(s, sep, a).1;
        n += 1;
    }
    n
}

fn split(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 || !argwin::is_i64(args[3]) || !argwin::is_i64(args[4]) {
        return Err(FaultKind::Internal);
    }
    if f.i64_arg(args[0]).and_then(elem_ty) != Some(Ty::String) {
        return Err(FaultKind::Internal);
    }
    let src_bits = f.i64_arg(args[3]);
    let (Some(s), Some(sep)) = (f.str_arg(args[3]), f.str_arg(args[4])) else {
        f.set_i64_arg(args[5], None);
        return Ok(());
    };
    let (sp, sn, pp, pn) = (s.as_ptr(), s.len(), sep.as_ptr(), sep.len());
    // SAFETY: both name live bytes of the CONST / SCRATCH / PERSIST regions for the evaluation
    // (F8); re-borrowed so `coll_init` / `coll_mut` (the pool region, F6) can follow.
    let (s, sep) = unsafe {
        (
            core::slice::from_raw_parts(sp, sn),
            core::slice::from_raw_parts(pp, pn),
        )
    };
    let konst = src_bits
        .map(|b| StrRef(b as u64))
        .filter(|r| r.region() == str_region::CONST);
    let n = count(s, sep);
    let cap = f.i64_arg(args[2]);
    if cap.is_some_and(|c| n as i64 > c) || n > u32::MAX as u64 {
        return Err(FaultKind::PoolFull);
    }
    let (r, lay) = create(f, site, CollKind::Array, Ty::String, Ty::Void, cap, 0)?;
    if konst.is_none() {
        // Every piece must fit a cell before the first is stored.
        let mut at = if n == 0 { None } else { Some(0usize) };
        while let Some(a) = at {
            let (e, nx) = next(s, sep, a);
            if (e - a) as u64 > lay.cell_bytes() as u64 {
                return Err(FaultKind::StrArenaFull);
            }
            at = nx;
        }
    }
    let (h, words) = f.coll_mut(r)?;
    let mut p = Pool::of_header(h);
    let mut at = if n == 0 { None } else { Some(0usize) };
    let mut k = 0usize;
    while let Some(a) = at {
        let (e, nx) = next(s, sep, a);
        words[k] = match konst {
            Some(c) => StrRef::konst(c.off() + a as u32, (e - a) as u32).0,
            None => store(
                &mut p,
                words,
                lay.kcells,
                &lay,
                StrVal::Bytes(s[a..].as_ptr(), (e - a) as u32),
            )?,
        };
        k += 1;
        at = nx;
    }
    p.to_header(h);
    h.len = k as u32;
    f.set_i64_arg(args[5], Some(r.0 as i64));
    Ok(())
}

/// The `str.split` row.
pub fn bi_str_split(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    split(f, site, args).map_err(|k| declared(flags(ov::STR_SPLIT), k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_keep_empties_and_code_points() {
        assert_eq!(count(b"a,,b,", b","), 4);
        assert_eq!(count(b"", b","), 1);
        assert_eq!(count(b"", b""), 0);
        assert_eq!(count("a\u{e9}b".as_bytes(), b""), 3);
        assert_eq!(next("\u{e9}x".as_bytes(), b"", 0), (2, Some(2)));
        assert_eq!(next(b"ab::cd", b"::", 0), (2, Some(4)));
    }
}
