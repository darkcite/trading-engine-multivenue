// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `array.*` part 2 (impl-04 §4.16.6, T34), first cut: `sort` of numeric arrays in place and the
//! float reductions `sum` / `avg` on the §4.9 kernel. Each works on the root owner's words seen
//! by the reference (a view sorts its window of the owner).

use super::array::{declared, flags};
use super::{resolve, Owned};
use crate::simd;
use core::cmp::Ordering;
use pine_ir::{CollKind, FaultKind, SiteId, Ty, NA_I64};
use pine_spec::ov;
use pine_vm::Frame;

fn array(f: &Frame<'_>, e: u16) -> Result<Owned, FaultKind> {
    let o = resolve(f, f.coll_arg(e))?;
    if !matches!(o.kind, CollKind::Array) {
        return Err(FaultKind::Internal);
    }
    // Every element the reference sees must still be the owner's (§4.16.3).
    if o.base + o.len > o.owner_len {
        return Err(FaultKind::CollectionBounds);
    }
    Ok(o)
}

/// Floats ascending: na last; `−0.0` before `+0.0` (`total_cmp`, `pin:04-collection-edges`).
fn f_asc(a: &u64, b: &u64) -> Ordering {
    let (x, y) = (f64::from_bits(*a), f64::from_bits(*b));
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => x.total_cmp(&y),
    }
}

/// Floats descending: na first.
fn f_desc(a: &u64, b: &u64) -> Ordering {
    let (x, y) = (f64::from_bits(*a), f64::from_bits(*b));
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => y.total_cmp(&x),
    }
}

const NA: u64 = NA_I64 as u64;

/// Ints ascending: na last.
fn i_asc(a: &u64, b: &u64) -> Ordering {
    match (*a == NA, *b == NA) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => (*a as i64).cmp(&(*b as i64)),
    }
}

/// Ints descending: na first.
fn i_desc(a: &u64, b: &u64) -> Ordering {
    match (*a == NA, *b == NA) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => (*b as i64).cmp(&(*a as i64)),
    }
}

/// `array.sort(id, order)` — `[id, order]`, `order.ascending` / `descending` = 0 / 1 (§3.4): in
/// place, `sort_unstable_by` with a named comparator — no closure, no allocation. `int`, `float`
/// (and `bool` / `color` / enum as ints); a string array is not served by this body yet.
fn array_sort(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    let desc = match f.i64_arg(args[1]) {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(FaultKind::Internal),
    };
    let cmp: fn(&u64, &u64) -> Ordering = match (o.elem, desc) {
        (Ty::Float, false) => f_asc,
        (Ty::Float, true) => f_desc,
        (Ty::String, _) => return Err(FaultKind::Internal),
        (_, false) => i_asc,
        (_, true) => i_desc,
    };
    let (_, words) = f.coll_mut(o.owner)?;
    words[o.base as usize..(o.base + o.len) as usize].sort_unstable_by(cmp);
    Ok(())
}

/// The float elements a reference sees, as `f64`s.
fn floats<'a>(words: &'a [u64], o: &Owned) -> &'a [f64] {
    let w = &words[o.base as usize..(o.base + o.len) as usize];
    // SAFETY: `u64` and `f64` have the same size and alignment and every bit pattern is a valid
    // `f64`; a float element is its `f64` bits (§4.16.1).
    unsafe { core::slice::from_raw_parts(w.as_ptr().cast::<f64>(), w.len()) }
}

/// `array.sum` / `array.avg` of a float array — `[id, out]`: the §4.9 kernel's `Σ` over the
/// non-na values (`avg` = `Σ / count`); every value na (or none) → na (arrays page: the
/// statistics "do not return na when some of the values … have na values").
fn sum_avg<const AVG: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if !matches!(o.elem, Ty::Float) || pine_ir::argwin::is_i64(args[1]) {
        return Err(FaultKind::Internal);
    }
    let (_, words) = f.coll(o.owner)?;
    let (s, n) = simd::sum_f64_skipna(floats(words, &o));
    let v = if n == 0 {
        f64::NAN
    } else if AVG {
        s / n as f64
    } else {
        s
    };
    f.set_f64_arg(args[1], v);
    Ok(())
}

// ---- the rows ------------------------------------------------------------------------------

/// The `array.sort` rows (float / int).
pub fn bi_array_sort(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_sort(f, site, args).map_err(|k| declared(flags(ov::ARRAY_SORT_AF), k))
}
/// The `array.sum` row of a float array.
pub fn bi_array_sum_f(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    sum_avg::<false>(f, args).map_err(|k| declared(flags(ov::ARRAY_SUM_F), k))
}
/// The `array.avg` row of a float array.
pub fn bi_array_avg_f(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    sum_avg::<true>(f, args).map_err(|k| declared(flags(ov::ARRAY_AVG_F), k))
}
