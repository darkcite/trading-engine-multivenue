// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `array.*` part 3 (impl-04 §4.16.6, T34 rest): `fill` (in place, never grows), `variance` /
//! `stdev` of a float array (two-pass), and the element-copying rows `copy`, `from`, `concat`;
//! the string `sort` / `sort_indices` and (R-P5) the UDT field `sort` / `sort_indices` (`*_AT`,
//! keyed through `Frame::obj_field`) on one in-place heap sort. Every body checks everything before its first write (§4.16.5, I14); a creator's own checks
//! that need the new block's layout follow its init (`pin:04-coll-size-check-after-init`).

use super::array::{declared, flags, word_of};
use super::{
    as_owned, create, elem_ty, layout_of, owned_word, release, resolve, store, str_fits, str_val,
    Layout, Owned, Pool, StrVal,
};
use crate::simd;
use crate::strnum::{default_shape, Laid, Shape};
use core::cmp::Ordering;
use pine_ir::{argwin, CollKind, CollRef, FaultKind, SiteId, StrRef, Ty, NA_I64};
use pine_spec::ov;
use pine_vm::Frame;

/// An array reference whose every element is still the owner's (§4.16.3).
fn array(f: &Frame<'_>, e: u16) -> Result<Owned, FaultKind> {
    let o = resolve(f, f.coll_arg(e))?;
    if !matches!(o.kind, CollKind::Array) {
        return Err(FaultKind::Internal);
    }
    if o.base + o.len > o.owner_len {
        return Err(FaultKind::CollectionBounds);
    }
    Ok(o)
}

/// A value for an element of `elem`: a string (checked against the cell) or a word.
#[derive(Clone, Copy)]
enum Val {
    Str(StrVal),
    Word(u64),
}

fn val(f: &Frame<'_>, e: u16, elem: Ty, lay: &Layout) -> Result<Val, FaultKind> {
    if matches!(elem, Ty::String) {
        if !argwin::is_i64(e) {
            return Err(FaultKind::Internal);
        }
        let v = str_val(f, e);
        str_fits(v, lay)?;
        return Ok(Val::Str(v));
    }
    Ok(Val::Word(word_of(f, e, elem)?))
}

// ---- fill ------------------------------------------------------------------------------------

/// `array.fill(id, value, index_from, index_to)` — `[id, value, from, to]`: elements `[from, to)`
/// of the existing array set to `value`; `to` na = the size (the reference's default).
/// `pin:04-array-fill-no-grow` (lead ruling): the reference's text read literally — it sets
/// existing elements and never grows the array — so `from > to`, `to > size`, a na `from`, or a
/// negative bound (§4.16.5: a negative index outside `get set insert remove`) is
/// `CollectionBounds`, and the row's `POOL` is never raised. A view fills its window of the owner
/// (no size change). A runtime-built string takes one cell per element, each old cell released
/// first (so a full pool always has the cell the new value takes).
fn array_fill(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    let to = f.i64_arg(args[3]).unwrap_or(o.len as i64);
    let (from, to) = match f.i64_arg(args[2]) {
        Some(a) if 0 <= a && a <= to && to <= o.len as i64 => (a as u32, to as u32),
        _ => return Err(FaultKind::CollectionBounds),
    };
    let lay = layout_of(f, &o)?;
    let v = val(f, args[1], o.elem, &lay)?;
    let (h, words) = f.coll_mut(o.owner)?;
    let mut p = Pool::of_header(h);
    let mut i = from;
    while i < to {
        let k = (o.base + i) as usize;
        match v {
            Val::Word(w) => words[k] = w,
            Val::Str(s) => {
                let old = words[k];
                release(&mut p, words, lay.kcells, lay.cell_w, old);
                words[k] = store(&mut p, words, lay.kcells, &lay, s)?;
            }
        }
        i += 1;
    }
    p.to_header(h);
    Ok(())
}

// ---- variance / stdev ------------------------------------------------------------------------

/// The float elements a reference sees, as `f64`s.
fn floats<'a>(words: &'a [u64], o: &Owned) -> &'a [f64] {
    let w = &words[o.base as usize..(o.base + o.len) as usize];
    // SAFETY: `u64` and `f64` have the same size and alignment and every bit pattern is a valid
    // `f64`; a float element is its `f64` bits (§4.16.1).
    unsafe { core::slice::from_raw_parts(w.as_ptr().cast::<f64>(), w.len()) }
}

/// `array.variance` / `array.stdev(id, biased)` of a float array — `[id, biased, out]`.
/// `pin:04-array-variance-two-pass` (lead ruling): the reference's definition, two passes — the
/// mean `Σx / n` on the §4.9 kernel, then `Σ(x − mean)²` — over the non-na elements (the arrays
/// page: na values do not make the result na); divided by `n` when `biased` is true, else by
/// `n − 1`; `stdev` is its square root. Empty / every value na → na (the reference's remark); an
/// unbiased estimate of one value is `0 / 0` → na (the literal division). A na `biased` (v5) is
/// not true (as `pin:04-array-every-some`): the unbiased estimate.
fn var_std<const SD: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if !matches!(o.elem, Ty::Float) || argwin::is_i64(args[2]) {
        return Err(FaultKind::Internal);
    }
    let biased = f.bool_arg(args[1]) == Some(true);
    let (_, words) = f.coll(o.owner)?;
    let xs = floats(words, &o);
    let (s, n) = simd::sum_f64_skipna(xs);
    let v = if n == 0 {
        f64::NAN
    } else {
        let m = s / n as f64;
        let mut ss = 0.0f64;
        let mut i = 0usize;
        while i < xs.len() {
            let x = xs[i];
            if !x.is_nan() {
                let d = x - m;
                ss += d * d;
            }
            i += 1;
        }
        let d = if biased { n } else { n - 1 };
        let var = ss / d as f64;
        if SD {
            var.sqrt()
        } else {
            var
        }
    };
    f.set_f64_arg(args[2], v);
    Ok(())
}

// ---- copying rows ----------------------------------------------------------------------------

/// Where the string cells of a source element word live.
#[derive(Clone, Copy)]
enum Src {
    /// Another block: its storage words (F8: unmoved for the evaluation), its cell area and width.
    Other(*const u64, u32, u32),
    /// The destination block itself (same layout).
    Same,
}

/// The destination word for source element word `w` (all checks done): a non-owned word as is; an
/// owned cell copied into a fresh cell of the destination.
fn copy_word(
    p: &mut Pool,
    words: &mut [u64],
    dl: &Layout,
    src: Src,
    w: u64,
) -> Result<u64, FaultKind> {
    let Some((c, n)) = as_owned(w) else {
        return Ok(w);
    };
    match src {
        Src::Other(sp, cells, cell_w) => {
            // SAFETY: `sp` is another block's storage (F6: disjoint from `words`), unmoved by the
            // destination's `coll_mut` (F8); the cell `[cells + c·cell_w, +cell_w)` lies in it.
            let b = unsafe { sp.add((cells + c * cell_w) as usize).cast::<u8>() };
            store(p, words, dl.kcells, dl, StrVal::Bytes(b, n))
        }
        Src::Same => {
            let nc = p.take(words, dl.kcells, dl.cell_w, dl.cap)?;
            let s = (dl.kcells + c * dl.cell_w) as usize;
            let d = (dl.kcells + nc * dl.cell_w) as usize;
            // COPY: one cell (≤ str_cell_bytes) inside the block — the new element owns its own
            // cell (§4.16.4) — sharing the source's cell rejected (cells are freed per element).
            words.copy_within(s..s + dl.cell_w as usize, d);
            Ok(owned_word(nc, n))
        }
    }
}

/// `StrArenaFull` when an owned string of `ws` is longer than a cell of `dl` (checked before any
/// element is written).
fn strings_fit(ws: &[u64], dl: &Layout) -> Result<(), FaultKind> {
    let mut k = 0usize;
    while k < ws.len() {
        if let Some((_, n)) = as_owned(ws[k]) {
            if n > dl.cell_bytes() {
                return Err(FaultKind::StrArenaFull);
            }
        }
        k += 1;
    }
    Ok(())
}

/// `array.copy(id)` — `[__elem, __key, __cap, id, out]`: a new array of the reference's elements
/// (a view copies its window); a runtime-built string is copied into the new block's own cell
/// (§4.16.4); later changes to either array do not reach the other (verified, arrays page). More
/// elements than `__cap` is `PoolFull`, a string longer than the new cell `StrArenaFull`.
fn array_copy(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[3])?;
    if f.i64_arg(args[0]).and_then(elem_ty) != Some(o.elem) {
        return Err(FaultKind::Internal);
    }
    let cap = f.i64_arg(args[2]);
    if o.owner.site() == site {
        return copy_self(f, site, &o, cap, args[4]);
    }
    let sl = layout_of(f, &o)?;
    let sp: *const u64 = f.coll(o.owner)?.1.as_ptr();
    let (r, dl) = create(f, site, CollKind::Array, o.elem, Ty::Void, cap, o.len)?;
    {
        // SAFETY: `sp` is the source block's storage (another site's block, F6), unmoved (F8);
        // `[base, base + len)` lies in it (`array` checked `base + len <= owner_len <= cap`).
        let ws = unsafe { core::slice::from_raw_parts(sp.add(o.base as usize), o.len as usize) };
        strings_fit(ws, &dl)?;
    }
    let (h, words) = f.coll_mut(r)?;
    let mut p = Pool::of_header(h);
    let src = Src::Other(sp, sl.kcells, sl.cell_w);
    let mut k = 0u32;
    while k < o.len {
        // SAFETY: as above.
        let w = unsafe { *sp.add((o.base + k) as usize) };
        words[k as usize] = copy_word(&mut p, words, &dl, src, w)?;
        k += 1;
    }
    p.to_header(h);
    f.set_i64_arg(args[4], Some(r.0 as i64));
    Ok(())
}

/// `a := array.copy(a)` — the source is the block this site re-creates (`pin:04-array-copy-self`:
/// the copy is made in place): the reference's window moves to the front, the cells of the
/// elements outside it are freed, and the block is re-initialised under a new generation (the
/// old reference goes stale, as every creator's previous result).
fn copy_self(
    f: &mut Frame<'_>,
    site: SiteId,
    o: &Owned,
    cap: Option<i64>,
    out: u16,
) -> Result<(), FaultKind> {
    let lay = layout_of(f, o)?;
    if cap != Some(lay.cap as i64) {
        return Err(FaultKind::Internal);
    }
    let (h, words) = f.coll_mut(o.owner)?;
    let mut p = Pool::of_header(h);
    let (lo, hi) = (o.base as usize, (o.base + o.len) as usize);
    let mut k = 0usize;
    while k < o.owner_len as usize {
        if k < lo || k >= hi {
            let w = words[k];
            release(&mut p, words, lay.kcells, lay.cell_w, w);
        }
        k += 1;
    }
    // COPY: ≤ cap × 8 B of element words inside the block — the copied window starts at element
    // 0 — a second block rejected (a site owns one).
    words.copy_within(lo..hi, 0);
    let r = f.coll_init(site, CollKind::Array, o.elem, Ty::Void, 0)?;
    let (h, _) = f.coll_mut(r)?;
    h.cap = lay.cap;
    h.len = o.len;
    p.to_header(h);
    f.set_i64_arg(out, Some(r.0 as i64));
    Ok(())
}

/// `array.from(arg0, arg1, …)` — `[__elem, __key, __cap, arg0 …, out]`: a new array of the
/// arguments in order (a v6 `bool` na is `false`; a runtime-built string copied into a cell).
/// More arguments than `__cap` is `PoolFull`; a string longer than a cell `StrArenaFull`.
fn array_from(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() < 4 {
        return Err(FaultKind::Internal);
    }
    let elem = f
        .i64_arg(args[0])
        .and_then(elem_ty)
        .ok_or(FaultKind::Internal)?;
    let vals = &args[3..args.len() - 1];
    let n = vals.len() as u32;
    let (r, lay) = create(
        f,
        site,
        CollKind::Array,
        elem,
        Ty::Void,
        f.i64_arg(args[2]),
        n,
    )?;
    let mut k = 0usize;
    while k < vals.len() {
        val(f, vals[k], elem, &lay)?;
        k += 1;
    }
    let mut k = 0usize;
    while k < vals.len() {
        let v = val(f, vals[k], elem, &lay)?;
        let (h, words) = f.coll_mut(r)?;
        words[k] = match v {
            Val::Word(w) => w,
            Val::Str(s) => {
                let mut p = Pool::of_header(h);
                let w = store(&mut p, words, lay.kcells, &lay, s)?;
                p.to_header(h);
                w
            }
        };
        k += 1;
    }
    f.set_i64_arg(args[args.len() - 1], Some(r.0 as i64));
    Ok(())
}

/// `array.concat(id1, id2)` — `[id1, id2, out]`: `id2`'s elements pushed onto `id1`, `id1`
/// returned (verified, arrays page). A view `id1` is `CollectionBounds` (a size change); past
/// `id1`'s `cap` `PoolFull`; a string longer than `id1`'s cell `StrArenaFull` — all before any
/// write. `concat(a, a)` (or `id2` a view of `id1`) reads the window it appends from the one block
/// (§4.16.4: `copy_within`); that window ends before the appended tail.
fn array_concat(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let a = array(f, args[0])?;
    let b = array(f, args[1])?;
    if a.elem != b.elem {
        return Err(FaultKind::Internal);
    }
    if a.view {
        return Err(FaultKind::CollectionBounds);
    }
    let al = layout_of(f, &a)?;
    let bl = layout_of(f, &b)?;
    if a.len as u64 + b.len as u64 > al.cap as u64 {
        return Err(FaultKind::PoolFull);
    }
    let sp: *const u64 = f.coll(b.owner)?.1.as_ptr();
    {
        let (_, bw) = f.coll(b.owner)?;
        strings_fit(&bw[b.base as usize..(b.base + b.len) as usize], &al)?;
    }
    let same = a.owner == b.owner;
    let src = if same {
        Src::Same
    } else {
        Src::Other(sp, bl.kcells, bl.cell_w)
    };
    let (h, words) = f.coll_mut(a.owner)?;
    let mut p = Pool::of_header(h);
    let mut k = 0u32;
    while k < b.len {
        let w = if same {
            words[(b.base + k) as usize]
        } else {
            // SAFETY: `sp` is `id2`'s block (another block than `id1`'s, F6), unmoved by
            // `coll_mut` (F8); `base + k < owner_len <= cap`.
            unsafe { *sp.add((b.base + k) as usize) }
        };
        words[(a.len + k) as usize] = copy_word(&mut p, words, &al, src, w)?;
        k += 1;
    }
    p.to_header(h);
    h.len = a.len + b.len;
    let id1: Option<CollRef> = f.coll_arg(args[0]);
    f.set_i64_arg(args[2], id1.map(|r| r.0 as i64));
    Ok(())
}

// ---- join -----------------------------------------------------------------------------------

/// Lay out element word `w` of a numeric array (`float`: the default `str.tostring` shape, na →
/// `"NaN"`; `int`: decimal digits, na → `"NaN"`) into `l`.
fn lay_num(l: &mut Laid, w: u64, float: bool, sh: Shape) {
    if float {
        l.set_f64(f64::from_bits(w), sh);
    } else if w == NA_I64 as u64 {
        l.set_f64(f64::NAN, sh);
    } else {
        l.set_i64(w as i64, Shape::upto(0));
    }
}

/// `array.join(id, separator)` — `[id, separator, out]`: one new string, the elements separated
/// by `separator` (the reference). Sized first, then one `str_alloc_with` filled in place
/// (§4.16.4); a number is laid out twice (size, then write — no scratch holds its digits).
/// `pin:04-array-join-render`: the reference names no rendering — an element is rendered as
/// `str.tostring` renders it (`float`: the default shape, `int`: decimal), an na element of any
/// type as `"NaN"` (`str.tostring`'s na); a na `separator` makes the result na (na propagates).
/// Past the arena `StrArenaFull` (all-or-nothing). An empty array is `""`.
fn array_join(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 || !argwin::is_i64(args[2]) {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    let (string, float) = match o.elem {
        Ty::String => (true, false),
        Ty::Float => (false, true),
        Ty::Int => (false, false),
        _ => return Err(FaultKind::Internal),
    };
    let Some(sep_n) = f.str_arg(args[1]).map(<[u8]>::len) else {
        f.set_i64_arg(args[2], None);
        return Ok(());
    };
    let lay = layout_of(f, &o)?;
    let sh = default_shape(f);
    let wp: *const u64 = f.coll(o.owner)?.1.as_ptr();
    let mut l = Laid::EMPTY;
    let mut total = 0u64;
    let mut k = 0u32;
    while k < o.len {
        // SAFETY: the owner's storage (F8), `base + k < owner_len <= cap` (`array`).
        let w = unsafe { *wp.add((o.base + k) as usize) };
        total += if !string {
            lay_num(&mut l, w, float, sh);
            l.len() as u64
        } else if w == NA_I64 as u64 {
            3
        } else {
            match as_owned(w) {
                Some((_, n)) => n as u64,
                None => StrRef(w).len() as u64,
            }
        };
        if k > 0 {
            total += sep_n as u64;
        }
        k += 1;
    }
    if total > u32::MAX as u64 {
        return Err(FaultKind::StrArenaFull);
    }
    let (r, dst, [sep]) = f.str_alloc_with(total as u32, [args[1]])?;
    let (dp, dn) = (dst.as_mut_ptr(), dst.len());
    let (sp, sn) = sep.map_or((core::ptr::null(), 0), |s| (s.as_ptr(), s.len()));
    let mut at = 0usize;
    let mut k = 0u32;
    while k < o.len {
        if k > 0 {
            // COPY: the separator (its length) into the result — the result is one contiguous
            // string — a StrPiece concat rejected: one sized range filled in place (§4.16.4).
            // SAFETY: `sp` / `dp` are the separator and the fresh range `str_alloc_with` lent
            // (disjoint, A7.2); `at + sn <= dn` (sized above).
            unsafe { core::ptr::copy_nonoverlapping(sp, dp.add(at), sn) };
            at += sn;
        }
        // SAFETY: as in the sizing pass.
        let w = unsafe { *wp.add((o.base + k) as usize) };
        if !string {
            lay_num(&mut l, w, float, sh);
            // SAFETY: the fresh range, `at + l.len() <= dn` (sized above).
            let out = unsafe { core::slice::from_raw_parts_mut(dp.add(at), dn - at) };
            at += l.write(out);
        } else {
            let (p, n) = if w == NA_I64 as u64 {
                (b"NaN".as_ptr(), 3usize)
            } else if let Some((c, n)) = as_owned(w) {
                // SAFETY: the cell `[kcells + c·cell_w, +cell_w)` of the owner block (F8).
                let p = unsafe { wp.add((lay.kcells + c * lay.cell_w) as usize).cast::<u8>() };
                (p, n as usize)
            } else {
                let b = f.str_bytes(StrRef(w));
                (b.as_ptr(), b.len())
            };
            // COPY: one element's bytes (≤ its length) into the result — the result is one
            // contiguous string — a StrPiece concat rejected: one sized range filled in place.
            // SAFETY: `p` names `n` bytes of a CONST string, a pool cell or a static word —
            // regions disjoint from the fresh SCRATCH range (F6); `at + n <= dn` (sized above).
            unsafe { core::ptr::copy_nonoverlapping(p, dp.add(at), n) };
            at += n;
        }
        k += 1;
    }
    debug_assert!(at == dn);
    f.set_i64_arg(args[2], Some(r.0 as i64));
    Ok(())
}

// ---- string sort, sort_indices ---------------------------------------------------------------

/// What the sort comparator reads: the frame (a `CONST` string's bytes), the owner block's
/// storage (an owned cell's bytes; for `sort_indices` the values the indices name, from `base`),
/// the element type and the order.
struct Cmp<'a, 'f> {
    f: &'a Frame<'f>,
    src: *const u64,
    base: u32,
    kcells: u32,
    cell_w: u32,
    elem: Ty,
    desc: bool,
}

/// The bytes of string element word `w` (`None` = na).
fn str_of<'a>(c: &'a Cmp<'_, '_>, w: u64) -> Option<&'a [u8]> {
    if w == NA_I64 as u64 {
        return None;
    }
    match as_owned(w) {
        // SAFETY: the cell `[kcells + c·cell_w, +cell_w)` of the owner block (F8: unmoved), `n`
        // ≤ its bytes (stored so).
        Some((k, n)) => Some(unsafe {
            core::slice::from_raw_parts(
                c.src.add((c.kcells + k * c.cell_w) as usize).cast::<u8>(),
                n as usize,
            )
        }),
        None => Some(c.f.str_bytes(StrRef(w))),
    }
}

/// Ascending order of two element words: na last; floats by `total_cmp` (`−0.0` before `+0.0`),
/// ints signed, strings bytewise (§4.16.6, `pin:04-collection-edges`).
fn asc(c: &Cmp<'_, '_>, a: u64, b: u64) -> Ordering {
    match c.elem {
        Ty::String => match (str_of(c, a), str_of(c, b)) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => x.cmp(y),
        },
        ty => num_asc(ty, a, b),
    }
}

/// [`asc`] of two non-string words: floats by `total_cmp` (na last, `−0.0` before `+0.0`), any
/// other type as a signed int (na last).
fn num_asc(ty: Ty, a: u64, b: u64) -> Ordering {
    if matches!(ty, Ty::Float) {
        let (x, y) = (f64::from_bits(a), f64::from_bits(b));
        return match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => x.total_cmp(&y),
        };
    }
    let na = NA_I64 as u64;
    match (a == na, b == na) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => (a as i64).cmp(&(b as i64)),
    }
}

/// `a` goes before `b`: element words (`sort`; descending = the ascending order reversed, so na
/// first).
fn before_val(c: &Cmp<'_, '_>, a: u64, b: u64) -> bool {
    let o = asc(c, a, b);
    if c.desc {
        o == Ordering::Greater
    } else {
        o == Ordering::Less
    }
}

/// `a` goes before `b`: indices into the source (`sort_indices`) — by value in the order, ties by
/// index ascending, so the `(value, index)` order equals a stable sort (§4.16.6).
fn before_idx(c: &Cmp<'_, '_>, a: u64, b: u64) -> bool {
    // SAFETY: `a`, `b` < the source window's length; `base + len <= owner_len <= cap` words.
    let (va, vb) = unsafe {
        (
            *c.src.add((c.base as u64 + a) as usize),
            *c.src.add((c.base as u64 + b) as usize),
        )
    };
    let o = asc(c, va, vb);
    let o = if c.desc { o.reverse() } else { o };
    o.then(a.cmp(&b)) == Ordering::Less
}

/// Sift `root` down the max-heap `p[..end]` under `before` (the root goes last).
///
/// # Safety
/// As [`heap_sort`]; `root < end <= n`.
unsafe fn sift<C>(p: *mut u64, root: usize, end: usize, cx: &C, before: fn(&C, u64, u64) -> bool) {
    let mut r = root;
    loop {
        let mut ch = 2 * r + 1;
        if ch >= end {
            break;
        }
        // SAFETY: `ch`, `ch + 1`, `r` < `end` <= `n` (the caller's contract).
        unsafe {
            if ch + 1 < end && before(cx, *p.add(ch), *p.add(ch + 1)) {
                ch += 1;
            }
            if !before(cx, *p.add(r), *p.add(ch)) {
                break;
            }
            core::ptr::swap(p.add(r), p.add(ch));
        }
        r = ch;
    }
}

/// Heap sort of the `n` words at `p` under `before` (`a` goes before `b`) — O(n log n), in place,
/// an index loop, no allocation and no closure (`cx` carries what the named comparator reads: a
/// [`Cmp`] for the scalar rows, an [`ObjCmp`] for the UDT field rows).
///
/// # Safety
/// `p` names `n` initialised, exclusively owned words for the call (no `&` / `&mut` to them).
unsafe fn heap_sort<C>(p: *mut u64, n: usize, cx: &C, before: fn(&C, u64, u64) -> bool) {
    let mut s = n / 2;
    while s > 0 {
        s -= 1;
        // SAFETY: `s < n` (the caller's contract).
        unsafe { sift(p, s, n, cx, before) };
    }
    let mut end = n;
    while end > 1 {
        end -= 1;
        // SAFETY: `0`, `end` < `n`.
        unsafe {
            core::ptr::swap(p, p.add(end));
            sift(p, 0, end, cx, before);
        }
    }
}

fn order(f: &Frame<'_>, e: u16) -> Result<bool, FaultKind> {
    match f.i64_arg(e) {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(FaultKind::Internal),
    }
}

/// `array.sort(id, order)` of a string array — `[id, order]`: in place, bytewise, na last
/// ascending / first descending; the words move, the cells stay (§4.16.1). A view sorts its
/// window of the owner.
fn sort_str(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if !matches!(o.elem, Ty::String) {
        return Err(FaultKind::Internal);
    }
    let desc = order(f, args[1])?;
    let lay = layout_of(f, &o)?;
    let wp: *mut u64 = f.coll_mut(o.owner)?.1.as_mut_ptr();
    let cx = Cmp {
        f,
        src: wp,
        base: 0,
        kcells: lay.kcells,
        cell_w: lay.cell_w,
        elem: o.elem,
        desc,
    };
    // SAFETY: the owner's window `[base, base + len)` (`array` checked it), journaled by
    // `coll_mut` above, unmoved (F8); no reference to it is live (the comparator reads only the
    // cells — other words of the block — and CONST bytes).
    unsafe { heap_sort(wp.add(o.base as usize), o.len as usize, &cx, before_val) };
    Ok(())
}

/// `array.sort_indices(id, order)` — `[__elem, __key, __cap, id, order, out]`: a new `array<int>`
/// of the reference's indices in sorted order (`(value, index)` pairs: deterministic, = a stable
/// sort); the source is not modified. More elements than `__cap` is `PoolFull`.
/// `pin:04-array-sort-indices-self`: a source that is this site's own previous result
/// (`idx := array.sort_indices(idx)`) would be overwritten while read and no scratch holds the
/// values — `ScriptRefused` (a stated gap, not a semantics).
fn sort_indices(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[3])?;
    if f.i64_arg(args[0]).and_then(elem_ty) != Some(Ty::Int)
        || !matches!(o.elem, Ty::Float | Ty::Int | Ty::String)
    {
        return Err(FaultKind::Internal);
    }
    if o.owner.site() == site {
        return Err(FaultKind::ScriptRefused);
    }
    let desc = order(f, args[4])?;
    let sl = layout_of(f, &o)?;
    let sp: *const u64 = f.coll(o.owner)?.1.as_ptr();
    let (r, _) = create(
        f,
        site,
        CollKind::Array,
        Ty::Int,
        Ty::Void,
        f.i64_arg(args[2]),
        o.len,
    )?;
    let dp: *mut u64 = {
        let (_, words) = f.coll_mut(r)?;
        let mut k = 0usize;
        while k < o.len as usize {
            words[k] = k as u64;
            k += 1;
        }
        words.as_mut_ptr()
    };
    let cx = Cmp {
        f,
        src: sp,
        base: o.base,
        kcells: sl.kcells,
        cell_w: sl.cell_w,
        elem: o.elem,
        desc,
    };
    // SAFETY: the new block's first `len` words (`create` checked `len <= cap`), journaled by
    // `coll_mut` above, unmoved (F8), no reference to them live; the comparator reads the source
    // block (another site's, F6) and CONST bytes.
    unsafe { heap_sort(dp, o.len as usize, &cx, before_idx) };
    f.set_i64_arg(args[5], Some(r.0 as i64));
    Ok(())
}

// ---- UDT field sort, sort_indices (R-P5, CAR-R27-1) -------------------------------------------

/// What a UDT field comparator reads: the frame (each object's field through
/// `Frame::obj_field`, a string key's bytes), the source words (`sort_indices`: the elements the
/// indices name, from `base`), the field, its type and the order.
struct ObjCmp<'a, 'f> {
    f: &'a Frame<'f>,
    src: *const u64,
    base: u32,
    field: u32,
    key: Ty,
    desc: bool,
}

/// The key of element `w`: field `c.field` of the object it names. Every element passed
/// [`objects_ok`] before the sort began and a sort writes only the array's words, never an
/// object's block, so the read cannot fault; the na key in the arm that cannot run keeps the
/// comparator total.
fn obj_key(c: &ObjCmp<'_, '_>, w: u64) -> u64 {
    let r = c.f.obj_field(CollRef(w), c.field);
    debug_assert!(r.is_ok(), "an element objects_ok passed faults mid-sort");
    match r {
        Ok(k) => k,
        Err(_) => NA_I64 as u64,
    }
}

/// Ascending order of two field keys of type `ty`, the scalar rows' order ([`asc`]): na last;
/// floats by `total_cmp`, ints signed, strings bytewise. A string field holds a `CONST` or
/// `PERSIST` `StrRef` (impl-02 §4.14.7) — never an owned-cell word, whose tag is also `PERSIST`
/// (§4.16.1) — so its bytes come from `Frame::str_bytes`, not [`str_of`].
fn key_asc(f: &Frame<'_>, ty: Ty, a: u64, b: u64) -> Ordering {
    if !matches!(ty, Ty::String) {
        return num_asc(ty, a, b);
    }
    let na = NA_I64 as u64;
    match (a == na, b == na) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => f.str_bytes(StrRef(a)).cmp(f.str_bytes(StrRef(b))),
    }
}

/// `a` goes before `b`: element words of an `array<UDT>` by their field keys (`sort`, as
/// [`before_val`]: descending = the ascending order reversed, so an na field first).
fn before_obj(c: &ObjCmp<'_, '_>, a: u64, b: u64) -> bool {
    let o = key_asc(c.f, c.key, obj_key(c, a), obj_key(c, b));
    if c.desc {
        o == Ordering::Greater
    } else {
        o == Ordering::Less
    }
}

/// `a` goes before `b`: indices into an `array<UDT>` (`sort_indices`, as [`before_idx`]) — by
/// field key in the order, ties by index ascending.
fn before_obj_idx(c: &ObjCmp<'_, '_>, a: u64, b: u64) -> bool {
    // SAFETY: `a`, `b` < the source window's length; `base + len <= owner_len <= cap` words.
    let (wa, wb) = unsafe {
        (
            *c.src.add((c.base as u64 + a) as usize),
            *c.src.add((c.base as u64 + b) as usize),
        )
    };
    let o = key_asc(c.f, c.key, obj_key(c, wa), obj_key(c, wb));
    let o = if c.desc { o.reverse() } else { o };
    o.then(a.cmp(&b)) == Ordering::Less
}

/// Every element of `o`'s window names a live object with field `field`, checked before any
/// write (§4.16.5, I14): `pin:04-udt-sort-na-object` — an na element is `ScriptRefused` (each
/// element is a key, and reading a field of an na object faults as `O_GET_*` does); a stale or
/// non-object element is `ScriptRefused`, a field past the object's `Internal`
/// (`Frame::obj_field`).
fn objects_ok(f: &Frame<'_>, o: &Owned, field: u32) -> Result<(), FaultKind> {
    let (_, words) = f.coll(o.owner)?;
    let end = (o.base + o.len) as usize;
    let mut i = o.base as usize;
    while i < end {
        if words[i] == NA_I64 as u64 {
            return Err(FaultKind::ScriptRefused);
        }
        f.obj_field(CollRef(words[i]), field)?;
        i += 1;
    }
    Ok(())
}

/// The field and key type of a UDT sort's selector (`pin:02-udt-field-selector`): an int,
/// float or string field; any other type is `Internal` (sema refuses it, 3007).
fn sort_field(f: &Frame<'_>, e: u16) -> Result<(u32, Ty), FaultKind> {
    match super::udt_selector(f, e)? {
        s @ (_, Ty::Int | Ty::Float | Ty::String) => Ok(s),
        _ => Err(FaultKind::Internal),
    }
}

/// `array.sort(id, order, sort_field)` of an `array<UDT>` (v6) — `[id, order, sel]`: the element
/// words sorted in place by the field `sel` names, with the scalar rows' sort and order — the
/// heap sort of [`sort_str`], na field last ascending / first descending. A view sorts its
/// window of the owner. `pin:04-udt-sort-ties` — the reference is silent on stability; the
/// scalar rows' in-place sort is not stable, which no scalar array can observe (equal keys are
/// equal values), but two objects with equal keys are distinct: their order after the sort is
/// the heap sort's (deterministic, a function of the input order alone), not necessarily their
/// input order. `sort_indices` is stable (`(key, index)`).
fn sort_at(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if !matches!(o.elem, Ty::Udt) {
        return Err(FaultKind::Internal);
    }
    let desc = order(f, args[1])?;
    let (field, key) = sort_field(f, args[2])?;
    objects_ok(f, &o, field)?;
    let wp: *mut u64 = f.coll_mut(o.owner)?.1.as_mut_ptr();
    let cx = ObjCmp {
        f,
        src: wp,
        base: 0,
        field,
        key,
        desc,
    };
    // SAFETY: the owner's window `[base, base + len)` (`array` checked it), journaled by
    // `coll_mut` above, unmoved (F8); no reference to it is live (the comparator reads only the
    // objects' blocks — other sites' — and CONST / PERSIST string bytes, F6).
    unsafe { heap_sort(wp.add(o.base as usize), o.len as usize, &cx, before_obj) };
    Ok(())
}

/// `array.sort_indices(id, order, sort_field)` of an `array<UDT>` (v6) —
/// `[__elem, __key, __cap, id, order, sel, out]`: a new `array<int>` of the reference's indices
/// sorted by the field `sel` names, `(key, index)` pairs as the scalar row (deterministic, = a
/// stable sort); the source is not modified. More elements than `__cap` is `PoolFull`.
fn sort_indices_at(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 7 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[3])?;
    if f.i64_arg(args[0]).and_then(elem_ty) != Some(Ty::Int) || !matches!(o.elem, Ty::Udt) {
        return Err(FaultKind::Internal);
    }
    // An `array<UDT>` source is never this site's own `array<int>` result.
    debug_assert!(o.owner.site() != site);
    let desc = order(f, args[4])?;
    let (field, key) = sort_field(f, args[5])?;
    objects_ok(f, &o, field)?;
    let sp: *const u64 = f.coll(o.owner)?.1.as_ptr();
    let (r, _) = create(
        f,
        site,
        CollKind::Array,
        Ty::Int,
        Ty::Void,
        f.i64_arg(args[2]),
        o.len,
    )?;
    let dp: *mut u64 = {
        let (_, words) = f.coll_mut(r)?;
        let mut k = 0usize;
        while k < o.len as usize {
            words[k] = k as u64;
            k += 1;
        }
        words.as_mut_ptr()
    };
    let cx = ObjCmp {
        f,
        src: sp,
        base: o.base,
        field,
        key,
        desc,
    };
    // SAFETY: the new block's first `len` words (`create` checked `len <= cap`), journaled by
    // `coll_mut` above, unmoved (F8), no reference to them live; the comparator reads the source
    // block and the objects' blocks (other sites', F6) and CONST / PERSIST string bytes.
    unsafe { heap_sort(dp, o.len as usize, &cx, before_obj_idx) };
    f.set_i64_arg(args[6], Some(r.0 as i64));
    Ok(())
}

// ---- the rows (pine-vm `flag_ok`: a fault the row's flags do not declare is `Internal`) ----

/// The `array.fill` row.
pub fn bi_array_fill(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_fill(f, args).map_err(|k| declared(flags(ov::ARRAY_FILL), k))
}
/// The `array.variance` row (float).
pub fn bi_array_variance_f(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    var_std::<false>(f, args).map_err(|k| declared(flags(ov::ARRAY_VARIANCE_F), k))
}
/// The `array.stdev` row (float).
pub fn bi_array_stdev_f(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    var_std::<true>(f, args).map_err(|k| declared(flags(ov::ARRAY_STDEV_F), k))
}
/// The `array.copy` row.
pub fn bi_array_copy(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_copy(f, site, args).map_err(|k| declared(flags(ov::ARRAY_COPY), k))
}
/// The `array.from` rows (`ID` = the row's `OverloadId`: `STR_ALLOC` on `from<T>` and the string
/// row only).
pub fn bi_array_from<const ID: u16>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    array_from(f, site, args).map_err(|k| declared(flags(pine_ir::OverloadId(ID)), k))
}
/// The `array.join` rows (float, int, string).
pub fn bi_array_join(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_join(f, args).map_err(|k| declared(flags(ov::ARRAY_JOIN_AS), k))
}
/// The `array.sort` row of a string array.
pub fn bi_array_sort_str(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    sort_str(f, args).map_err(|k| declared(flags(ov::ARRAY_SORT_AS), k))
}
/// The `array.sort_indices` rows (float, int, string).
pub fn bi_array_sort_indices(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    sort_indices(f, site, args).map_err(|k| declared(flags(ov::ARRAY_SORT_INDICES_AF), k))
}
/// The `array.sort` row of an `array<UDT>` by field (v6).
pub fn bi_array_sort_at(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    sort_at(f, args).map_err(|k| declared(flags(ov::ARRAY_SORT_AT), k))
}
/// The `array.sort_indices` row of an `array<UDT>` by field (v6).
pub fn bi_array_sort_indices_at(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    sort_indices_at(f, site, args).map_err(|k| declared(flags(ov::ARRAY_SORT_INDICES_AT), k))
}
/// The `array.concat` row.
pub fn bi_array_concat(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_concat(f, args).map_err(|k| declared(flags(ov::ARRAY_CONCAT), k))
}
