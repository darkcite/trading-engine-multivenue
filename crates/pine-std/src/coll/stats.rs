// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `array.*` part 2 (impl-04 §4.16.6, T34): `sort` of numeric arrays and `reverse` in place, the
//! float reductions `sum` / `avg` on the §4.9 kernel, the int `sum`, `min` / `max` (with `nth`), `range`,
//! `every` / `some`, `binary_search*`. Each works on the root owner's words seen by the reference
//! (a view sorts or reverses its window of the owner). No scratch, no allocation. R-P16 (CI-052,
//! F108): TradingView refuses a `binary_search*` field argument at compile, so the UDT field
//! search body is gone; pine-sema refuses the call (`pin:02-bsearch-field-refused`).

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

/// `array.reverse(id)` — `[id]`: the element words of the reference reversed in place (a view
/// reverses its window of the owner). Any element type: a string element's word names its cell,
/// so the words move and the cells stay (§4.16.1).
fn array_reverse(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    let (_, words) = f.coll_mut(o.owner)?;
    words[o.base as usize..(o.base + o.len) as usize].reverse();
    Ok(())
}

/// Element word `w` is na for an element of `elem`.
#[inline(always)]
fn is_na(w: u64, float: bool) -> bool {
    if float {
        f64::from_bits(w).is_nan()
    } else {
        w == NA
    }
}

/// The word at rank `nth` of the non-na elements of `w` under `cmp` (duplicates counted, as a
/// sorted copy's `[nth]`); `None` past the count. No scratch: each round finds the next distinct
/// value above the last one and its multiplicity — O(len × distinct values up to rank `nth`).
fn rank(w: &[u64], float: bool, cmp: fn(&u64, &u64) -> Ordering, nth: u64) -> Option<u64> {
    let mut below: Option<u64> = None;
    let mut left = nth;
    loop {
        let mut best: Option<u64> = None;
        let mut count = 0u64;
        let mut i = 0usize;
        while i < w.len() {
            let x = w[i];
            i += 1;
            if is_na(x, float) || below.is_some_and(|b| cmp(&x, &b) != Ordering::Greater) {
                continue;
            }
            match best.map(|b| cmp(&x, &b)) {
                None | Some(Ordering::Less) => {
                    best = Some(x);
                    count = 1;
                }
                Some(Ordering::Equal) => count += 1,
                Some(Ordering::Greater) => {}
            }
        }
        let v = best?;
        if left < count {
            return Some(v);
        }
        left -= count;
        below = Some(v);
    }
}

/// `array.min` / `array.max(id, nth)` — `[id, nth, out]` (float or int lane): the `nth`
/// smallest / greatest non-na element. `pin:04-array-nth`: rank `nth` of the non-na values in
/// ascending (`min`) / descending (`max`) order, duplicates counted (a sorted copy's `[nth]`, the
/// reference's `array.max(a, 1)` = "second highest"); na `nth`, a negative `nth`, or `nth` at or
/// past the non-na count → na; every value na (or none) → na (the arrays page).
fn min_max<const MAX: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    let float = matches!(o.elem, Ty::Float);
    if !matches!(o.elem, Ty::Float | Ty::Int) || pine_ir::argwin::is_i64(args[2]) == float {
        return Err(FaultKind::Internal);
    }
    let nth = f.i64_arg(args[1]);
    let cmp: fn(&u64, &u64) -> Ordering = match (float, MAX) {
        (true, false) => f_asc,
        (true, true) => f_desc,
        (false, false) => i_asc,
        (false, true) => i_desc,
    };
    let v = match nth {
        Some(n) if n >= 0 => {
            let (_, words) = f.coll(o.owner)?;
            rank(
                &words[o.base as usize..(o.base + o.len) as usize],
                float,
                cmp,
                n as u64,
            )
        }
        _ => None,
    };
    if float {
        f.set_f64_arg(args[2], v.map_or(f64::NAN, f64::from_bits));
    } else {
        f.set_i64_arg(args[2], v.map(|w| w as i64));
    }
    Ok(())
}

/// `array.range(id)` — `[id, out]`: greatest − smallest of the non-na elements; every value na
/// (or none) → na. An int range wraps (`pin:04-array-range-int`: `i64::wrapping_sub`, the
/// reference names no overflow; only a span past 2^63 wraps).
fn range(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    let float = matches!(o.elem, Ty::Float);
    if !matches!(o.elem, Ty::Float | Ty::Int) || pine_ir::argwin::is_i64(args[1]) == float {
        return Err(FaultKind::Internal);
    }
    let (_, words) = f.coll(o.owner)?;
    let w = &words[o.base as usize..(o.base + o.len) as usize];
    let (lo, hi) = if float {
        (rank(w, true, f_asc, 0), rank(w, true, f_desc, 0))
    } else {
        (rank(w, false, i_asc, 0), rank(w, false, i_desc, 0))
    };
    match (lo, hi) {
        (Some(lo), Some(hi)) if float => {
            f.set_f64_arg(args[1], f64::from_bits(hi) - f64::from_bits(lo));
        }
        (Some(lo), Some(hi)) => f.set_i64_arg(args[1], Some((hi as i64).wrapping_sub(lo as i64))),
        _ if float => f.set_f64_arg(args[1], f64::NAN),
        _ => f.set_i64_arg(args[1], None),
    }
    Ok(())
}

/// `array.every` / `array.some(id)` — `[id, out]` on a bool array: every element true / at
/// least one. `pin:04-array-every-some`: an na element (v5) is not true; an empty array is
/// `every` = **false** and `some` = false (TV F105: `every` 0 and `some` 0 of `array.new_bool(0)`
/// — not the vacuous truth; the reference shows no empty case).
fn every_some<const SOME: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if !matches!(o.elem, Ty::Bool) || !pine_ir::argwin::is_i64(args[1]) {
        return Err(FaultKind::Internal);
    }
    let (_, words) = f.coll(o.owner)?;
    let w = &words[o.base as usize..(o.base + o.len) as usize];
    let mut i = 0usize;
    let mut v = !SOME && !w.is_empty();
    while i < w.len() {
        let t = w[i] != 0 && w[i] != NA;
        if SOME && t {
            v = true;
            break;
        }
        if !SOME && !t {
            v = false;
            break;
        }
        i += 1;
    }
    f.set_bool_arg(args[1], v);
    Ok(())
}

/// `array.sum` of an int array — `[id, out]`: Σ of the non-na elements; every value na (or
/// none) → na (the arrays page). `pin:04-array-sum-int`: the sum wraps at 2^63
/// (`i64::wrapping_add`; the reference names no overflow).
fn sum_i(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if !matches!(o.elem, Ty::Int) || !pine_ir::argwin::is_i64(args[1]) {
        return Err(FaultKind::Internal);
    }
    let (_, words) = f.coll(o.owner)?;
    let w = &words[o.base as usize..(o.base + o.len) as usize];
    let (mut s, mut n) = (0i64, 0u32);
    let mut i = 0usize;
    while i < w.len() {
        if w[i] != NA {
            s = s.wrapping_add(w[i] as i64);
            n += 1;
        }
        i += 1;
    }
    f.set_i64_arg(args[1], (n > 0).then_some(s));
    Ok(())
}

const FEQ: [u8; 2] = crate::sw::both(pine_spec::Sw::FloatEqV6);

/// Element word `w` as the `f64` the searches compare (an int element converts exactly below
/// 2^53; na → NaN, which every `<` / `<=` puts above the value, as `sort` puts na last).
#[inline(always)]
fn key(w: u64, float: bool) -> f64 {
    if float {
        f64::from_bits(w)
    } else if w == NA {
        f64::NAN
    } else {
        w as i64 as f64
    }
}

/// The first index of `w` whose key is not `< v` (`LE` false) / not `<= v` (`LE` true), by
/// bisection over an ascending array.
fn bisect<const LE: bool>(w: &[u64], float: bool, v: f64) -> usize {
    let (mut lo, mut hi) = (0usize, w.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let x = key(w[mid], float);
        if if LE { x <= v } else { x < v } {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// `array.binary_search` (`MODE` 0) / `_leftmost` (1) / `_rightmost` (2) — `[id, val, out]`
/// over an ascending float or int array (`val` is a float; `==` per `Sw::FloatEqV6`). The
/// reference: the index of `val`, else −1 (plain); else the index of the next smaller element
/// to the left (leftmost) / of the element to the right of where `val` would lie (rightmost).
/// `pin:04-binary-search`: a found value is its first occurrence (plain, leftmost) / its last
/// (rightmost). The miss law (TV F106, float and int rows alike, over `[10, 20, 30, 40]`): with
/// the insertion point `ip`, leftmost = `max(0, ip − 1)` and rightmost = `ip` — leftmost(25) 1,
/// rightmost(25) 2, leftmost(5) **0** (not −1), rightmost(5) 0, leftmost(45) 3, rightmost(45) 4,
/// plain(5) −1. An empty array is the law's literal reading (leftmost 0, rightmost 0; not
/// captured); an na `val` is −1 for all three; an unsorted array gives the bisection's answer.
fn binary_search<const MODE: u8>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    use pine_ir::argwin::is_i64;
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    let float = matches!(o.elem, Ty::Float);
    if !matches!(o.elem, Ty::Float | Ty::Int) || is_i64(args[1]) || !is_i64(args[2]) {
        return Err(FaultKind::Internal);
    }
    let tol = match crate::sw::pick(FEQ, f.dialect()) {
        pine_spec::swr::FLOAT_EQ_V6_EXACT => false,
        pine_spec::swr::FLOAT_EQ_V6_ABS_1E10 => float,
        // `round9` needs `$op.eq_round9`'s decimal kernel, not landed here (as `indexof`).
        _ => return Err(FaultKind::Internal),
    };
    let v = f.f64_arg(args[1]);
    let r = if v.is_nan() {
        -1
    } else {
        let (_, words) = f.coll(o.owner)?;
        let w = &words[o.base as usize..(o.base + o.len) as usize];
        let lo = bisect::<false>(w, float, v);
        // `abs_1e10`: the element the bisection lands on is "found" within the tolerance (the
        // bisection itself orders exactly; a run of near-equal elements is uncaptured).
        let hit = |x: f64| if tol { super::array::feq_tol(x, v) } else { x == v };
        let found = lo < w.len() && hit(key(w[lo], float));
        match (MODE, found) {
            (0, true) | (1, true) => lo as i64,
            (0, false) => -1,
            (1, false) => (lo as i64 - 1).max(0),
            (_, true) => bisect::<true>(w, float, v) as i64 - 1,
            (_, false) => lo as i64,
        }
    };
    f.set_i64_arg(args[2], Some(r));
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
/// The `array.reverse` row (any element type).
pub fn bi_array_reverse(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_reverse(f, args).map_err(|k| declared(flags(ov::ARRAY_REVERSE), k))
}
/// The `array.min` rows (float / int).
pub fn bi_array_min(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    min_max::<false>(f, args).map_err(|k| declared(flags(ov::ARRAY_MIN_F), k))
}
/// The `array.max` rows (float / int).
pub fn bi_array_max(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    min_max::<true>(f, args).map_err(|k| declared(flags(ov::ARRAY_MAX_F), k))
}
/// The `array.range` rows (float / int).
pub fn bi_array_range(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    range(f, args).map_err(|k| declared(flags(ov::ARRAY_RANGE_F), k))
}
/// The `array.every` row.
pub fn bi_array_every(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    every_some::<false>(f, args).map_err(|k| declared(flags(ov::ARRAY_EVERY), k))
}
/// The `array.some` row.
pub fn bi_array_some(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    every_some::<true>(f, args).map_err(|k| declared(flags(ov::ARRAY_SOME), k))
}
/// The `array.binary_search` rows (float / int).
pub fn bi_array_binary_search(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    binary_search::<0>(f, args).map_err(|k| declared(flags(ov::ARRAY_BINARY_SEARCH_AF), k))
}
/// The `array.binary_search_leftmost` rows (float / int).
pub fn bi_array_binary_search_leftmost(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    binary_search::<1>(f, args).map_err(|k| declared(flags(ov::ARRAY_BINARY_SEARCH_LEFTMOST_AF), k))
}
/// The `array.binary_search_rightmost` rows (float / int).
pub fn bi_array_binary_search_rightmost(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    binary_search::<2>(f, args)
        .map_err(|k| declared(flags(ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AF), k))
}
/// The `array.sum` row of an int array.
pub fn bi_array_sum_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    sum_i(f, args).map_err(|k| declared(flags(ov::ARRAY_SUM_I), k))
}
