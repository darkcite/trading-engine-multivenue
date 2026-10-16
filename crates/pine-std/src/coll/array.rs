// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `array.*` part 1 (impl-04 §4.16.6, T33 — the creators and the core accessors) and the `map.*`
//! creator (T32's creator init for maps). Every body checks everything before its first write
//! (§4.16.5, I14). Element words: a float element is its `f64` bits (NaN = na); every other
//! element an i64 (`NA_I64` = na); a string element a `CONST` ref's bits, `NA_I64`, or an
//! owned-cell word (§4.16.4).

use super::{
    create, elem_ty, layout_of, norm_index, read_str, release, resolve, store, str_fits, str_val,
    Layout, Owned, Pool, StrVal,
};
use crate::sw;
use pine_ir::{argwin, CollKind, CollRef, Dialect, FaultKind, SiteId, Ty, NA_I64};
use pine_spec::{ov, Sw, OVERLOADS};
use pine_vm::{bf, Frame};

const NEG: [u8; 2] = sw::both(Sw::NegIndex);
const BOOL_NA: [u8; 2] = sw::both(Sw::BoolNa);

/// The row's `STR_ALLOC` flag as pine-spec gives it (F10): without it a body cannot hand out a
/// runtime-built string element (every string-returning row carries it since spec 7d2ac894…).
const fn str_alloc(id: pine_ir::OverloadId) -> bool {
    OVERLOADS[id.0 as usize].bf & bf::STR_ALLOC != 0
}
const GET_STR: bool = str_alloc(ov::ARRAY_GET);
const POP_STR: bool = str_alloc(ov::ARRAY_POP);

/// pine-spec's flags of `id`.
pub(super) const fn flags(id: pine_ir::OverloadId) -> u8 {
    OVERLOADS[id.0 as usize].bf
}

/// The fault a row may return: pine-vm's `flag_ok` admits `StrArenaFull` only from a `STR_ALLOC`
/// row and `PoolFull` only from a `POOL` / `INTENT` row. Since spec 7d2ac894… every collection
/// row that stores or returns a string is `STR_ALLOC` and every one that may take a cell or grow
/// is `POOL` (§4.16.4 F10, §4.16.5), so on those rows this is the identity; it stays as the guard
/// for a row that does not declare the kind (`pin:04-coll-undeclared-fault`: `Internal`).
pub(super) const fn declared(fl: u8, k: FaultKind) -> FaultKind {
    match k {
        FaultKind::StrArenaFull if fl & bf::STR_ALLOC == 0 => FaultKind::Internal,
        FaultKind::PoolFull if fl & (bf::POOL | bf::INTENT) == 0 => FaultKind::Internal,
        _ => k,
    }
}

// Every `array.new*` row shares one body, instantiated per row (`bi_array_new::<ID>`) so each
// declares its faults by its own row's flags: pine-spec gives `STR_ALLOC` to `array.new<T>` and
// `array.new_string` only. The rows may differ by that flag alone.
const _: () = {
    let ids = [
        ov::ARRAY_NEW_FLOAT,
        ov::ARRAY_NEW_INT,
        ov::ARRAY_NEW_BOOL,
        ov::ARRAY_NEW_COLOR,
        ov::ARRAY_NEW_STRING,
    ];
    let mut k = 0;
    while k < ids.len() {
        assert!(flags(ids[k]) & !bf::STR_ALLOC == flags(ov::ARRAY_NEW) & !bf::STR_ALLOC);
        k += 1;
    }
    assert!(str_alloc(ov::ARRAY_NEW) && str_alloc(ov::ARRAY_NEW_STRING));
};

fn from_end(d: Dialect) -> bool {
    sw::pick(NEG, d) == 1
}

/// W13: under `Sw::BoolNa = never` (v6) a bool is never na.
pub(super) fn bool_never_na(d: Dialect) -> bool {
    sw::pick(BOOL_NA, d) == 1
}

/// The storage word of non-string value argument `e` for an element of `elem`; the entry's lane
/// must agree with the element type (`Internal` otherwise: a sema defect).
pub(super) fn word_of(f: &Frame<'_>, e: u16, elem: Ty) -> Result<u64, FaultKind> {
    let float = matches!(elem, Ty::Float);
    if argwin::is_i64(e) == float {
        return Err(FaultKind::Internal);
    }
    if float {
        return Ok(f.f64_arg(e).to_bits());
    }
    Ok(match f.i64_arg(e) {
        Some(v) => v as u64,
        None if matches!(elem, Ty::Bool) && bool_never_na(f.dialect()) => 0,
        None => NA_I64 as u64,
    })
}

/// Write element word `w` of a non-string element to output `e`.
pub(super) fn put_word(f: &mut Frame<'_>, e: u16, elem: Ty, w: u64) -> Result<(), FaultKind> {
    let float = matches!(elem, Ty::Float);
    if argwin::is_i64(e) == float {
        return Err(FaultKind::Internal);
    }
    if float {
        f.set_f64_arg(e, f64::from_bits(w));
    } else if w == NA_I64 as u64 {
        if matches!(elem, Ty::Bool) && bool_never_na(f.dialect()) {
            f.set_i64_arg(e, Some(0));
        } else {
            f.set_i64_arg(e, None);
        }
    } else {
        f.set_i64_arg(e, Some(w as i64));
    }
    Ok(())
}

/// Write element word `w` of `o` to output `e`: a string through `read_str` (`can` = the row's
/// `STR_ALLOC`), anything else as is.
pub(super) fn put_elem(
    f: &mut Frame<'_>,
    e: u16,
    o: &Owned,
    lay: &Layout,
    w: u64,
    can: bool,
) -> Result<(), FaultKind> {
    if matches!(o.elem, Ty::String) {
        if !argwin::is_i64(e) {
            return Err(FaultKind::Internal);
        }
        let v = read_str(f, o.owner, lay.kcells, lay.cell_w, w, can)?;
        f.set_i64_arg(e, v);
        return Ok(());
    }
    put_word(f, e, o.elem, w)
}

fn array(f: &Frame<'_>, e: u16) -> Result<Owned, FaultKind> {
    let o = resolve(f, f.coll_arg(e))?;
    if !matches!(o.kind, CollKind::Array) {
        return Err(FaultKind::Internal);
    }
    Ok(o)
}

/// A value to store into an element of `elem`: a string (checked against the cell) or a word.
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

/// Overwrite owner element `k` with `v` (all checks done): the old element's cell is released
/// first, so a full pool still has the cell the new value takes.
fn write(
    f: &mut Frame<'_>,
    owner: CollRef,
    lay: &Layout,
    k: u32,
    v: &Val,
) -> Result<(), FaultKind> {
    let (h, words) = f.coll_mut(owner)?;
    match *v {
        Val::Word(w) => words[k as usize] = w,
        Val::Str(s) => {
            let mut p = Pool::of_header(h);
            let old = words[k as usize];
            release(&mut p, words, lay.kcells, lay.cell_w, old);
            words[k as usize] = store(&mut p, words, lay.kcells, lay, s)?;
            p.to_header(h);
        }
    }
    Ok(())
}

// ---- creators --------------------------------------------------------------------------------

/// `array.new<T>` / `array.new_float/int/bool/color/string(size, initial_value)` — window
/// `[__elem, __key, __cap, size, initial_value, out]`: `size` elements of `initial_value` (na
/// when absent; a v6 `bool`: false). A na or negative size is `ScriptRefused`, a size past
/// `__cap` `PoolFull` (§4.16.5).
fn array_new(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let elem = f
        .i64_arg(args[0])
        .and_then(elem_ty)
        .ok_or(FaultKind::Internal)?;
    let size = match f.i64_arg(args[3]) {
        Some(s) if s >= 0 => s,
        _ => return Err(FaultKind::ScriptRefused),
    };
    let cap = f.i64_arg(args[2]);
    if cap.is_some_and(|c| size > c) {
        return Err(FaultKind::PoolFull);
    }
    let size = size as u32;
    // The initial value is read before `coll_init` (its source may be this site's old block).
    let sv = if matches!(elem, Ty::String) {
        if !argwin::is_i64(args[4]) {
            return Err(FaultKind::Internal);
        }
        Val::Str(str_val(f, args[4]))
    } else {
        Val::Word(word_of(f, args[4], elem)?)
    };
    let (r, lay) = create(f, site, CollKind::Array, elem, Ty::Void, cap, 0)?;
    if let Val::Str(s) = sv {
        str_fits(s, &lay)?;
    }
    let (h, words) = f.coll_mut(r)?;
    let mut p = Pool::of_header(h);
    let mut k = 0usize;
    while k < size as usize {
        words[k] = match sv {
            Val::Word(w) => w,
            Val::Str(s) => store(&mut p, words, lay.kcells, &lay, s)?,
        };
        k += 1;
    }
    p.to_header(h);
    h.len = size;
    f.set_i64_arg(args[5], Some(r.0 as i64));
    Ok(())
}

/// `map.new<K, V>()` — window `[__elem, __key, __cap, out]`: an empty map, its index cleared.
fn map_new(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let v = f
        .i64_arg(args[0])
        .and_then(elem_ty)
        .ok_or(FaultKind::Internal)?;
    let k = f
        .i64_arg(args[1])
        .and_then(elem_ty)
        .ok_or(FaultKind::Internal)?;
    let cap = f.i64_arg(args[2]);
    let (r, _) = create(f, site, CollKind::Map, v, k, cap, 0)?;
    f.set_i64_arg(args[3], Some(r.0 as i64));
    Ok(())
}

/// `array.slice(id, index_from, index_to)` — window `[__elem, __key, __cap, id, from, to, out]`:
/// a view (§4.16.3) onto the root owner's elements `[from, to)` of `id`'s window; `from > to`,
/// `to > size`, a na or a negative bound is `CollectionBounds` (no W15 normalisation outside
/// `get set insert remove`).
fn array_slice(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 7 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[3])?;
    let (from, to) = match (f.i64_arg(args[4]), f.i64_arg(args[5])) {
        (Some(a), Some(b)) if 0 <= a && a <= b && b <= o.len as i64 => (a as u32, b as u32),
        _ => return Err(FaultKind::CollectionBounds),
    };
    let owner_off = f.coll(o.owner)?.0.data_off;
    let r = f.coll_init(site, CollKind::Array, o.elem, Ty::Void, 0)?;
    let (h, _) = f.coll_mut(r)?;
    h.view = 1;
    h.view_of = o.owner.site().0;
    h.data_off = owner_off + 8 * (o.base + from);
    h.len = to - from;
    h.cap = to - from;
    h.idx_mask = o.owner.gen();
    h.idx_off = 0;
    f.set_i64_arg(args[6], Some(r.0 as i64));
    Ok(())
}

// ---- accessors -------------------------------------------------------------------------------

/// `array.size(id)` — `[id, out]`.
fn array_size(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    f.set_i64_arg(args[1], Some(o.len as i64));
    Ok(())
}

/// `map.size(id)` — `[id, out]`.
fn map_size(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = resolve(f, f.coll_arg(args[0]))?;
    if !matches!(o.kind, CollKind::Map) {
        return Err(FaultKind::Internal);
    }
    f.set_i64_arg(args[1], Some(o.len as i64));
    Ok(())
}

/// `array.get(id, index)` — `[id, index, out]` (W15 normalisation).
fn array_get(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    let i = norm_index(f.i64_arg(args[1]), o.len, from_end(f.dialect()))?;
    let k = o.at(i)?;
    let lay = layout_of(f, &o)?;
    let w = f.coll(o.owner)?.1[k as usize];
    put_elem(f, args[2], &o, &lay, w, GET_STR)
}

/// `array.set(id, index, value)` — `[id, index, value]` (W15 normalisation).
fn array_set(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    let i = norm_index(f.i64_arg(args[1]), o.len, from_end(f.dialect()))?;
    let k = o.at(i)?;
    let lay = layout_of(f, &o)?;
    let v = val(f, args[2], o.elem, &lay)?;
    write(f, o.owner, &lay, k, &v)
}

/// `array.push(id, value)` — `[id, value]`: a view is `CollectionBounds`, a full array
/// `PoolFull`.
fn array_push(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if o.view {
        return Err(FaultKind::CollectionBounds);
    }
    let lay = layout_of(f, &o)?;
    let v = val(f, args[1], o.elem, &lay)?;
    if o.len >= lay.cap {
        return Err(FaultKind::PoolFull);
    }
    let k = o.len;
    // The new slot holds no cell: mark it na so `write` releases nothing.
    f.coll_mut(o.owner)?.1[k as usize] = NA_I64 as u64;
    write(f, o.owner, &lay, k, &v)?;
    f.coll_mut(o.owner)?.0.len = k + 1;
    Ok(())
}

/// `array.pop(id)` — `[id, out]`: the last element, removed; empty or a view is
/// `CollectionBounds`. The output is made before anything changes (a `StrArenaFull` copy
/// leaves the array as it was).
fn array_pop(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if o.view || o.len == 0 {
        return Err(FaultKind::CollectionBounds);
    }
    let lay = layout_of(f, &o)?;
    let k = o.len - 1;
    let w = f.coll(o.owner)?.1[k as usize];
    put_elem(f, args[1], &o, &lay, w, POP_STR)?;
    let (h, words) = f.coll_mut(o.owner)?;
    if matches!(o.elem, Ty::String) {
        let mut p = Pool::of_header(h);
        release(&mut p, words, lay.kcells, lay.cell_w, w);
        p.to_header(h);
    }
    h.len = k;
    Ok(())
}

/// `array.clear(id)` — `[id]`: every element removed and the cell pool reset; a view is
/// `CollectionBounds`.
fn array_clear(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if o.view {
        return Err(FaultKind::CollectionBounds);
    }
    let h = f.coll_mut(o.owner)?.0;
    h.len = 0;
    h.idx_off = 0;
    h.idx_mask = 0;
    Ok(())
}

// ---- the rows (pine-vm `flag_ok`: a fault the row's flags do not declare is `Internal`) ----

/// The `array.new*` rows (`ID` = the row's `OverloadId`): `array_new` with its faults declared
/// by that row's own flags.
pub fn bi_array_new<const ID: u16>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    array_new(f, site, args).map_err(|k| declared(flags(pine_ir::OverloadId(ID)), k))
}

/// The `map.new` row: `map_new` with its faults declared.
pub fn bi_map_new(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_new(f, site, args).map_err(|k| declared(flags(ov::MAP_NEW), k))
}

/// The `array.slice` row: `array_slice` with its faults declared.
pub fn bi_array_slice(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_slice(f, site, args).map_err(|k| declared(flags(ov::ARRAY_SLICE), k))
}

/// The `array.size` row: `array_size` with its faults declared.
pub fn bi_array_size(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_size(f, site, args).map_err(|k| declared(flags(ov::ARRAY_SIZE), k))
}

/// The `map.size` row: `map_size` with its faults declared.
pub fn bi_map_size(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_size(f, site, args).map_err(|k| declared(flags(ov::MAP_SIZE), k))
}

/// The `array.get` row: `array_get` with its faults declared.
pub fn bi_array_get(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_get(f, site, args).map_err(|k| declared(flags(ov::ARRAY_GET), k))
}

/// The `array.set` row: `array_set` with its faults declared.
pub fn bi_array_set(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_set(f, site, args).map_err(|k| declared(flags(ov::ARRAY_SET), k))
}

/// The `array.push` row: `array_push` with its faults declared.
pub fn bi_array_push(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_push(f, site, args).map_err(|k| declared(flags(ov::ARRAY_PUSH), k))
}

/// The `array.pop` row: `array_pop` with its faults declared.
pub fn bi_array_pop(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_pop(f, site, args).map_err(|k| declared(flags(ov::ARRAY_POP), k))
}

/// The `array.clear` row: `array_clear` with its faults declared.
pub fn bi_array_clear(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    array_clear(f, site, args).map_err(|k| declared(flags(ov::ARRAY_CLEAR), k))
}

// ---- part 1, the rest: first last shift unshift insert remove includes indexof lastindexof ----

const FIRST_STR: bool = str_alloc(ov::ARRAY_FIRST);
const LAST_STR: bool = str_alloc(ov::ARRAY_LAST);
const SHIFT_STR: bool = str_alloc(ov::ARRAY_SHIFT);
const REMOVE_STR: bool = str_alloc(ov::ARRAY_REMOVE);
const FEQ: [u8; 2] = sw::both(Sw::FloatEqV6);

/// `array.first(id)` / `array.last(id)` — `[id, out]`: empty is `CollectionBounds`.
fn first_last<const LAST: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if o.len == 0 {
        return Err(FaultKind::CollectionBounds);
    }
    let k = o.at(if LAST { o.len - 1 } else { 0 })?;
    let lay = layout_of(f, &o)?;
    let w = f.coll(o.owner)?.1[k as usize];
    put_elem(
        f,
        args[1],
        &o,
        &lay,
        w,
        if LAST { LAST_STR } else { FIRST_STR },
    )
}

/// Remove owner element `k` of an owner of `len` live elements: the tail moves down one word and
/// the removed element's cell (if any) is released. All checks are done.
fn remove_at(f: &mut Frame<'_>, o: &Owned, lay: &Layout, k: u32) -> Result<(), FaultKind> {
    let (h, words) = f.coll_mut(o.owner)?;
    let n = h.len as usize;
    let w = words[k as usize];
    // COPY: ≤ cap × 8 B of element refs inside the block — Pine's insert/remove semantics shift
    // the tail — a linked layout rejected (no allocation, cache).
    words.copy_within(k as usize + 1..n, k as usize);
    if matches!(o.elem, Ty::String) {
        let mut p = Pool::of_header(h);
        release(&mut p, words, lay.kcells, lay.cell_w, w);
        p.to_header(h);
    }
    h.len = (n - 1) as u32;
    Ok(())
}

/// Open a hole at owner element `k` (the tail moves up one word; the hole holds na). The caller
/// checked `len < cap`.
fn open_at(f: &mut Frame<'_>, o: &Owned, k: u32) -> Result<(), FaultKind> {
    let (h, words) = f.coll_mut(o.owner)?;
    let n = h.len as usize;
    // COPY: ≤ cap × 8 B of element refs inside the block — Pine's insert/unshift semantics shift
    // the tail — a linked layout rejected (no allocation, cache).
    words.copy_within(k as usize..n, k as usize + 1);
    words[k as usize] = NA_I64 as u64;
    h.len = (n + 1) as u32;
    Ok(())
}

/// `array.shift(id)` — `[id, out]` / `array.remove(id, index)` — `[id, index, out]` (W15): the
/// element, removed; the output is made before anything changes.
fn shift_remove<const REMOVE: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 + REMOVE as usize {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if o.view || o.len == 0 {
        return Err(FaultKind::CollectionBounds);
    }
    let i = if REMOVE {
        norm_index(f.i64_arg(args[1]), o.len, from_end(f.dialect()))?
    } else {
        0
    };
    let lay = layout_of(f, &o)?;
    let w = f.coll(o.owner)?.1[i as usize];
    let out = args[args.len() - 1];
    put_elem(
        f,
        out,
        &o,
        &lay,
        w,
        if REMOVE { REMOVE_STR } else { SHIFT_STR },
    )?;
    remove_at(f, &o, &lay, i)
}

/// `array.unshift(id, value)` — `[id, value]` / `array.insert(id, index, value)` —
/// `[id, index, value]`: `index` in `[0, size]` after W15 (`index + size` under `from_end`;
/// `index == size` appends, `pin:04-collection-edges`); a view is `CollectionBounds`, a full
/// array `PoolFull`.
fn unshift_insert<const INSERT: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 + INSERT as usize {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if o.view {
        return Err(FaultKind::CollectionBounds);
    }
    let i = if INSERT {
        let mut i = f.i64_arg(args[1]).ok_or(FaultKind::CollectionBounds)?;
        if i < 0 && from_end(f.dialect()) {
            i += o.len as i64;
        }
        if i < 0 || i > o.len as i64 {
            return Err(FaultKind::CollectionBounds);
        }
        i as u32
    } else {
        0
    };
    let lay = layout_of(f, &o)?;
    let v = val(f, args[args.len() - 1], o.elem, &lay)?;
    if o.len >= lay.cap {
        return Err(FaultKind::PoolFull);
    }
    open_at(f, &o, i)?;
    write(f, o.owner, &lay, i, &v)
}

/// Whether element word `w` equals the value of argument `e` (`==` of the element type: floats
/// per `Sw::FloatEqV6` = `exact`, IEEE `==`; ints and strings exactly; na equals nothing — the
/// na-false comparison).
fn equals(
    f: &Frame<'_>,
    o: &Owned,
    lay: &Layout,
    words: &[u64],
    w: u64,
    e: u16,
) -> Result<bool, FaultKind> {
    Ok(match o.elem {
        Ty::Float => {
            if sw::pick(FEQ, f.dialect()) != 0 {
                // `round9` needs `$op.eq_round9`'s decimal kernel, not landed here.
                return Err(FaultKind::Internal);
            }
            f64::from_bits(w) == f.f64_arg(e)
        }
        Ty::String => {
            let Some(b) = f.str_arg(e) else {
                return Ok(false);
            };
            if w == NA_I64 as u64 {
                return Ok(false);
            }
            match super::as_owned(w) {
                Some((c, n)) => {
                    let cell = super::cell_bytes_ref(words, lay.kcells, lay.cell_w, c);
                    (n as usize) <= cell.len() && &cell[..n as usize] == b
                }
                None => f.str_bytes(pine_ir::StrRef(w)) == b,
            }
        }
        _ => match f.i64_arg(e) {
            Some(v) => w != NA_I64 as u64 && w == v as u64,
            None => false,
        },
    })
}

/// `array.includes` (`MODE` 0) / `indexof` (1) / `lastindexof` (2) — `[id, value, out]`:
/// compared in place, nothing allocated; −1 when absent.
fn search<const MODE: u8>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let o = array(f, args[0])?;
    if o.base + o.len > o.owner_len {
        return Err(FaultKind::CollectionBounds);
    }
    let lay = layout_of(f, &o)?;
    let (_, words) = f.coll(o.owner)?;
    let mut hit: i64 = -1;
    let mut i = 0u32;
    while i < o.len {
        let k = if MODE == 2 { o.len - 1 - i } else { i };
        if equals(f, &o, &lay, words, words[(o.base + k) as usize], args[1])? {
            hit = k as i64;
            break;
        }
        i += 1;
    }
    if MODE == 0 {
        f.set_i64_arg(args[2], Some((hit >= 0) as i64));
    } else {
        f.set_i64_arg(args[2], Some(hit));
    }
    Ok(())
}

/// The `array.first` row.
pub fn bi_array_first(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    first_last::<false>(f, args).map_err(|k| declared(flags(ov::ARRAY_FIRST), k))
}
/// The `array.last` row.
pub fn bi_array_last(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    first_last::<true>(f, args).map_err(|k| declared(flags(ov::ARRAY_LAST), k))
}
/// The `array.shift` row.
pub fn bi_array_shift(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    shift_remove::<false>(f, args).map_err(|k| declared(flags(ov::ARRAY_SHIFT), k))
}
/// The `array.remove` row.
pub fn bi_array_remove(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    shift_remove::<true>(f, args).map_err(|k| declared(flags(ov::ARRAY_REMOVE), k))
}
/// The `array.unshift` row.
pub fn bi_array_unshift(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    unshift_insert::<false>(f, args).map_err(|k| declared(flags(ov::ARRAY_UNSHIFT), k))
}
/// The `array.insert` row.
pub fn bi_array_insert(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    unshift_insert::<true>(f, args).map_err(|k| declared(flags(ov::ARRAY_INSERT), k))
}
/// The `array.includes` row.
pub fn bi_array_includes(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    search::<0>(f, args).map_err(|k| declared(flags(ov::ARRAY_INCLUDES), k))
}
/// The `array.indexof` row.
pub fn bi_array_indexof(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    search::<1>(f, args).map_err(|k| declared(flags(ov::ARRAY_INDEXOF), k))
}
/// The `array.lastindexof` row.
pub fn bi_array_lastindexof(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    search::<2>(f, args).map_err(|k| declared(flags(ov::ARRAY_LASTINDEXOF), k))
}
