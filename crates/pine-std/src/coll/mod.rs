// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Collections in the pool (impl-04 §4.16, T32): block layouts and `coll_block_bytes` (§3.5),
//! creator init, views (`array.slice`), the private string-cell pool and its store / read, index
//! normalisation (W15) and the fault table of §4.16.5. The bodies are in `array` (T33/T34) and
//! `map` (T35).
//!
//! A collection is a `Collection` block of its creating site; `Frame::coll_init` / `coll` /
//! `coll_mut` hand out the 32-byte header and the storage words and journal the block (F3, F4).
//! Every element access goes through the **root owner** (`Owned`): a view contributes only its
//! offset into the owner's storage and its own length, so a write through a view journals and
//! mutates the owner (§4.16.3) and a string stored through a view takes a cell from the owner's
//! pool.
//!
//! The cell size. `Frame` lends no manifest field, so a body derives the words per string cell
//! from the block it was given: `s = (N − fixed) / (n_str · cap)`, where `N` is the block's word
//! count and `fixed` every non-cell word of §4.16.1 (`pin:04-coll-cell-from-block`). A block that
//! equals `coll_block_bytes` for the manifest's `str_cell_bytes` yields exactly that cell; a block
//! too small for one word per cell is `Internal` (sema and std disagree, I32).

pub(crate) mod array;
pub(crate) mod map;
pub(crate) mod matrix;
pub(crate) mod order;
pub(crate) mod part3;
pub(crate) mod split;
pub(crate) mod stats;

use pine_ir::{argwin, elem, CollHeader, CollKind, CollRef, FaultKind, SiteId, StrRef, Ty, NA_I64};
use pine_vm::Frame;

/// Bytes of an `array.slice` site's block: the header only. A slice is a view onto its owner's
/// storage (`Frame` redirects `coll` of a view to the owner, F3), never storage of its own (§4.16.3).
pub const VIEW_BLOCK_BYTES: u32 = 32;

const fn align8(x: u64) -> u64 {
    (x + 7) & !7
}

/// Entries of a map's open-addressing index: `next_pow2(max(2·cap, 2))` (§4.16.7).
const fn idx_entries(cap: u32) -> u64 {
    let m = 2 * cap as u64;
    let m = if m > 2 { m } else { 2 };
    m.next_power_of_two()
}

/// String components of a block: an `array<string>` has one; a map one per string key / value.
const fn n_str(kind: CollKind, elem: Ty, key: Ty) -> u64 {
    match kind {
        CollKind::Array => matches!(elem, Ty::String) as u64,
        CollKind::Map => matches!(elem, Ty::String) as u64 + matches!(key, Ty::String) as u64,
        _ => 0,
    }
}

/// Words of a block that are not string cells (§4.16.1): an array's elements; a map's keys,
/// values, index (`u32` entries, 2 per word) and one pool word per string component.
const fn fixed_words(kind: CollKind, elem: Ty, key: Ty, cap: u32) -> u64 {
    match kind {
        CollKind::Array => cap as u64,
        CollKind::Map => 2 * cap as u64 + idx_entries(cap) / 2 + n_str(kind, elem, key),
        _ => 0,
    }
}

/// Bytes of the `Collection` block of a creator site — contract §3.5: `align8(32 + stride × cap)`,
/// plus `cap × str_cell_bytes` of string storage per string component (CAR-03-1 (b)), plus the map
/// index. `cap` is the element count of an array, the pair count of a map; `str_cell_bytes` is the
/// manifest's (§3.14), rounded up to 8. Layout: §4.16.1. `Matrix` / `Object` are not pine-std's: 0.
/// A size past `u32::MAX` saturates there (`pin:04-coll-bytes-saturate`: sema refuses a block that
/// size by its frame bound; no wrap can make a small block look big enough).
#[must_use]
pub const fn coll_block_bytes(
    kind: CollKind,
    elem: Ty,
    key: Ty,
    cap: u32,
    str_cell_bytes: u32,
) -> u32 {
    if !matches!(kind, CollKind::Array | CollKind::Map) {
        return 0;
    }
    let cell = align8(str_cell_bytes as u64);
    let b = 32 + 8 * fixed_words(kind, elem, key, cap) + n_str(kind, elem, key) * cap as u64 * cell;
    let b = align8(b);
    if b > u32::MAX as u64 {
        u32::MAX
    } else {
        b as u32
    }
}

/// The word offsets of a block's parts (§4.16.1), derived from its kind, types, capacity and word
/// count `n` (the owner's storage words). `cell_w` = words per string cell (0 without a string
/// component or with `cap == 0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    /// Element (pair) capacity.
    pub cap: u32,
    /// Words per string cell.
    pub cell_w: u32,
    /// Map values; an array: 0.
    pub vals: u32,
    /// First key cell of a map with string keys, or the element cells of an `array<string>`.
    pub kcells: u32,
    /// First value cell of a map with string values.
    pub vcells: u32,
    /// Map index (word offset) and its entry count; an array: 0, 0.
    pub idx: u32,
    /// Index entries (a power of two).
    pub idx_n: u32,
    /// First map pool word; an array keeps its pool in the header.
    pub pool: u32,
}

impl Layout {
    /// The layout of a `kind` block of `cap` over `n` storage words; `Internal` when the block is
    /// smaller than `coll_block_bytes` with one-word cells (I32), or the kind is not pine-std's.
    pub(crate) const fn of(
        kind: CollKind,
        elem: Ty,
        key: Ty,
        cap: u32,
        n: u64,
    ) -> Result<Self, FaultKind> {
        if !matches!(kind, CollKind::Array | CollKind::Map) {
            return Err(FaultKind::Internal);
        }
        let fixed = fixed_words(kind, elem, key, cap);
        let ns = n_str(kind, elem, key);
        if n < fixed {
            return Err(FaultKind::Internal);
        }
        let cell_w = if ns == 0 || cap == 0 {
            0
        } else {
            let w = (n - fixed) / (ns * cap as u64);
            if w == 0 || w > u32::MAX as u64 {
                return Err(FaultKind::Internal);
            }
            w
        };
        // §3.5's check, term by term: the block is at least what sema sizes for this cell.
        let need = coll_block_bytes(kind, elem, key, cap, (8 * cell_w) as u32) as u64;
        if need > 32 + 8 * n {
            return Err(FaultKind::Internal);
        }
        let cap64 = cap as u64;
        let cw = cell_w * cap64;
        Ok(match kind {
            CollKind::Array => Self {
                cap,
                cell_w: cell_w as u32,
                vals: 0,
                kcells: cap,
                vcells: 0,
                idx: 0,
                idx_n: 0,
                pool: 0,
            },
            _ => {
                let kstr = matches!(key, Ty::String);
                let vstr = matches!(elem, Ty::String);
                let kcells = 2 * cap64;
                let vcells = kcells + if kstr { cw } else { 0 };
                let idx = vcells + if vstr { cw } else { 0 };
                let idx_n = idx_entries(cap);
                Self {
                    cap,
                    cell_w: cell_w as u32,
                    vals: cap,
                    kcells: kcells as u32,
                    vcells: vcells as u32,
                    idx: idx as u32,
                    idx_n: idx_n as u32,
                    pool: (idx + idx_n / 2) as u32,
                }
            }
        })
    }

    /// Bytes of one string cell.
    pub(crate) const fn cell_bytes(&self) -> u32 {
        8 * self.cell_w
    }
}

/// The `Ty` of an `elem` encoding (§3.4 `__elem` / `__key`, `pine_ir::elem`): a scalar's own
/// type, `Enum`, or `Udt`; `None` for an encoding outside `u16` or a scalar the collections do not
/// hold.
pub(crate) const fn elem_ty(enc: i64) -> Option<Ty> {
    if enc < 0 || enc > u16::MAX as i64 {
        return None;
    }
    let e = enc as u16;
    if e & elem::SCALAR != 0 {
        match Ty::from_u8(e as u8) {
            Some(t @ (Ty::Int | Ty::Float | Ty::Bool | Ty::Color | Ty::String)) => Some(t),
            _ => None,
        }
    } else if e & elem::ENUM != 0 {
        Some(Ty::Enum)
    } else {
        Some(Ty::Udt)
    }
}

/// The field index and field type a UDT `*_AT` row's `sort_field` argument names (R-P5,
/// impl-04 §4.16.6). `pin:02-udt-field-selector` — pine-sema folds the `const int` index or
/// `const string` name to one `int` constant, `index | (field Ty as i64) << 32`: the index in
/// bits 0..32, the field's `Ty` byte in bits 32..40, bits 40..64 zero (pine-sema `overload.rs`
/// `udt_selector` writes it). Anything sema cannot have built — an f64 entry, na, negative,
/// stray high bits, an unassigned `Ty` — is `Internal` (sema and std disagree).
pub(crate) fn udt_selector(f: &Frame<'_>, e: u16) -> Result<(u32, Ty), FaultKind> {
    if !argwin::is_i64(e) {
        return Err(FaultKind::Internal);
    }
    let s = match f.i64_arg(e) {
        Some(s) if s >= 0 && s >> 40 == 0 => s,
        _ => return Err(FaultKind::Internal),
    };
    let ty = Ty::from_u8((s >> 32) as u8).ok_or(FaultKind::Internal)?;
    Ok((s as u32, ty))
}

/// Region tag of an owned-cell word (§4.16.1): the `PERSIST` tag, which no element ever holds as
/// a `StrRef` (contract §3.6.3 (b)), so the tag is unambiguous inside a collection.
const OWNED: u64 = pine_ir::str_region::PERSIST;

/// An owned-cell word: region tag 3, offset field = cell index, length = byte length.
pub(crate) const fn owned_word(cell: u32, len: u32) -> u64 {
    StrRef::new(OWNED, cell, len).0
}

/// `Some((cell, len))` when `w` is an owned-cell word.
pub(crate) const fn as_owned(w: u64) -> Option<(u32, u32)> {
    if w == NA_I64 as u64 {
        return None;
    }
    let r = StrRef(w);
    if r.region() == OWNED {
        Some((r.off(), r.len()))
    } else {
        None
    }
}

/// The root owner of a collection reference and the window a reference sees in it: `base` is
/// the first owner element the reference sees, `len` its element count (a view's fixed
/// `to − from`; an owner's live count), `view` whether size changes are refused.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Owned {
    pub owner: CollRef,
    pub base: u32,
    pub len: u32,
    pub view: bool,
    pub kind: CollKind,
    pub elem: Ty,
    pub key: Ty,
    /// The owner's live count (a view checks `base + i < owner_len`, §4.16.3).
    pub owner_len: u32,
}

/// Resolve `r` (na or stale → `ScriptRefused`, contract §3.7) to its root owner. A view whose
/// owner was re-created faults `ScriptRefused` (the generation recorded at slicing, I15).
pub(crate) fn resolve(f: &Frame<'_>, r: Option<CollRef>) -> Result<Owned, FaultKind> {
    let r = r.ok_or(FaultKind::ScriptRefused)?;
    let (h, _) = f.coll(r)?;
    let kind = kind_of(h)?;
    let elem = Ty::from_u8(h.elem).ok_or(FaultKind::Internal)?;
    let key = Ty::from_u8(h.key).ok_or(FaultKind::Internal)?;
    if h.view == 0 {
        return Ok(Owned {
            owner: r,
            base: 0,
            len: h.len,
            view: false,
            kind,
            elem,
            key,
            owner_len: h.len,
        });
    }
    let (len, data_off) = (h.len, h.data_off);
    let owner = CollRef::new(SiteId(h.view_of), h.idx_mask);
    let (oh, _) = f.coll(owner)?;
    if oh.view != 0 || data_off < oh.data_off || (data_off - oh.data_off) % 8 != 0 {
        return Err(FaultKind::Internal);
    }
    Ok(Owned {
        owner,
        base: (data_off - oh.data_off) / 8,
        len,
        view: true,
        kind,
        elem,
        key,
        owner_len: oh.len,
    })
}

fn kind_of(h: &CollHeader) -> Result<CollKind, FaultKind> {
    match h.kind {
        0 => Ok(CollKind::Array),
        1 => Ok(CollKind::Map),
        _ => Err(FaultKind::Internal),
    }
}

impl Owned {
    /// The owner element of reference element `i < len`: `CollectionBounds` when a view's owner
    /// shrank below it (§4.16.3).
    pub(crate) fn at(&self, i: u32) -> Result<u32, FaultKind> {
        debug_assert!(i < self.len);
        let k = self.base + i;
        if k >= self.owner_len {
            return Err(FaultKind::CollectionBounds);
        }
        Ok(k)
    }
}

/// W15 (§4.16.5): `index` of `get`/`set`/`remove` (`len` = size) or `insert` (`len` = size + 1)
/// normalised: a negative index under `Sw::NegIndex = from_end` adds `len`; anything outside
/// `[0, len)` after that — a negative index under `refused`, na included
/// (`pin:04-coll-na-index`: na is no index, the literal reading of "outside") — is
/// `CollectionBounds`.
pub(crate) fn norm_index(i: Option<i64>, len: u32, from_end: bool) -> Result<u32, FaultKind> {
    let Some(mut i) = i else {
        return Err(FaultKind::CollectionBounds);
    };
    if i < 0 && from_end {
        i += len as i64;
    }
    if i < 0 || i >= len as i64 {
        return Err(FaultKind::CollectionBounds);
    }
    Ok(i as u32)
}

// ---- the string-cell pool --------------------------------------------------------------------

/// A cell pool: `bump` cells handed out so far, `free` = free-list head + 1 (0 = empty); the
/// free list is threaded through each free cell's first word (next + 1).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pool {
    pub bump: u32,
    pub free: u32,
}

impl Pool {
    /// An array's pool, kept in the header's two map-only fields (§4.16.1).
    pub(crate) const fn of_header(h: &CollHeader) -> Self {
        Self {
            bump: h.idx_off,
            free: h.idx_mask,
        }
    }
    pub(crate) fn to_header(self, h: &mut CollHeader) {
        h.idx_off = self.bump;
        h.idx_mask = self.free;
    }
    /// A map's pool word: bump low 32, free-list head + 1 high 32.
    pub(crate) const fn of_word(w: u64) -> Self {
        Self {
            bump: w as u32,
            free: (w >> 32) as u32,
        }
    }
    pub(crate) const fn to_word(self) -> u64 {
        self.bump as u64 | (self.free as u64) << 32
    }

    /// Take a cell of the `cap` cells at word `at` (`cell_w` words each); `Internal` when none is
    /// left (each element owns at most one cell and there are `cap` of each: unreachable).
    pub(crate) fn take(
        &mut self,
        words: &[u64],
        at: u32,
        cell_w: u32,
        cap: u32,
    ) -> Result<u32, FaultKind> {
        if self.free != 0 {
            let c = self.free - 1;
            let w = (at + c * cell_w) as usize;
            if c >= cap || w >= words.len() {
                return Err(FaultKind::Internal);
            }
            self.free = words[w] as u32;
            return Ok(c);
        }
        if self.bump < cap {
            self.bump += 1;
            return Ok(self.bump - 1);
        }
        Err(FaultKind::Internal)
    }

    /// Return cell `c` to the free list.
    pub(crate) fn give(&mut self, words: &mut [u64], at: u32, cell_w: u32, c: u32) {
        let w = (at + c * cell_w) as usize;
        debug_assert!(w < words.len());
        words[w] = self.free as u64;
        self.free = c + 1;
    }
}

/// The bytes of a cell area as a byte slice: `cells[at .. at + n_words]` reinterpreted.
pub(crate) fn cell_bytes_mut(words: &mut [u64], at: u32, cell_w: u32, c: u32) -> &mut [u8] {
    let lo = (at + c * cell_w) as usize;
    let w = &mut words[lo..lo + cell_w as usize];
    // SAFETY: `w` is an initialised, exclusively borrowed `[u64]`; `u8` has alignment 1 and no
    // invalid bit patterns, so viewing its `8 × len` bytes is sound for the borrow's lifetime.
    unsafe { core::slice::from_raw_parts_mut(w.as_mut_ptr().cast::<u8>(), w.len() * 8) }
}

pub(crate) fn cell_bytes_ref(words: &[u64], at: u32, cell_w: u32, c: u32) -> &[u8] {
    let lo = (at + c * cell_w) as usize;
    let w = &words[lo..lo + cell_w as usize];
    // SAFETY: as `cell_bytes_mut`, shared.
    unsafe { core::slice::from_raw_parts(w.as_ptr().cast::<u8>(), w.len() * 8) }
}

/// A string value about to be stored: na, a `CONST` ref (stored as its bits), or the raw bytes of
/// a `SCRATCH` / `PERSIST` string (copied into a cell). The raw pointer outlives the source
/// borrow: every frame region stays where it is for the evaluation (F8), and the source lies in
/// another region than any pool block (F6).
#[derive(Clone, Copy)]
pub(crate) enum StrVal {
    Na,
    Const(u64),
    Bytes(*const u8, u32),
}

/// Read string argument `e` for a store (§4.16.4): its bits say `CONST` or not; its bytes come
/// from `str_arg` (any region; a stale ref is na with the defect flag latched).
pub(crate) fn str_val(f: &Frame<'_>, e: u16) -> StrVal {
    let Some(bits) = f.i64_arg(e) else {
        return StrVal::Na;
    };
    if StrRef(bits as u64).region() == pine_ir::str_region::CONST {
        return StrVal::Const(bits as u64);
    }
    match f.str_arg(e) {
        Some(b) => StrVal::Bytes(b.as_ptr(), b.len() as u32),
        None => StrVal::Na,
    }
}

/// Check-only half of a store: `StrArenaFull` when the bytes do not fit a cell (never truncated).
pub(crate) const fn str_fits(v: StrVal, lay: &Layout) -> Result<(), FaultKind> {
    match v {
        StrVal::Bytes(_, n) if n > lay.cell_bytes() => Err(FaultKind::StrArenaFull),
        _ => Ok(()),
    }
}

/// Free the cell an element word owns, if any.
pub(crate) fn release(pool: &mut Pool, words: &mut [u64], at: u32, cell_w: u32, w: u64) {
    if let Some((c, _)) = as_owned(w) {
        pool.give(words, at, cell_w, c);
    }
}

/// The element word for `v`, taking a cell from `pool` for bytes. Call after `str_fits` and after
/// releasing the cell of the word it replaces (all checks precede every write, I14).
pub(crate) fn store(
    pool: &mut Pool,
    words: &mut [u64],
    at: u32,
    lay: &Layout,
    v: StrVal,
) -> Result<u64, FaultKind> {
    match v {
        StrVal::Na => Ok(NA_I64 as u64),
        StrVal::Const(b) => Ok(b),
        StrVal::Bytes(p, n) => {
            debug_assert!(n <= lay.cell_bytes());
            let c = pool.take(words, at, lay.cell_w, lay.cap)?;
            let dst = cell_bytes_mut(words, at, lay.cell_w, c);
            // COPY: ≤ str_cell_bytes of string bytes into the element's own cell — a collection
            // element must outlive the scratch arena and later stores to its source (contract
            // §3.6.3 (b)) — aliasing the source rejected (scratch resets per evaluation;
            // persistent cells are reused).
            // SAFETY: `p` names `n` live bytes of the CONST / SCRATCH / PERSIST region (F8: frame
            // regions do not move within an evaluation); `dst` is this pool block's cell (F6:
            // disjoint regions) and `n <= dst.len()` (checked by `str_fits`).
            unsafe { core::ptr::copy_nonoverlapping(p, dst.as_mut_ptr(), n as usize) };
            Ok(owned_word(c, n))
        }
    }
}

/// The value of element word `w` as a string output: na, a `CONST` ref's bits (zero-copy), or an
/// owned cell copied into scratch through `str_alloc` (§4.16.4). `can_alloc` is the calling row's
/// `STR_ALLOC` flag: a row without it cannot hand out a runtime-built element and faults
/// `Internal` (since spec 7d2ac894… every string-returning row declares it, F10).
pub(crate) fn read_str(
    f: &mut Frame<'_>,
    owner: CollRef,
    at: u32,
    cell_w: u32,
    w: u64,
    can_alloc: bool,
) -> Result<Option<i64>, FaultKind> {
    if w == NA_I64 as u64 {
        return Ok(None);
    }
    let Some((c, n)) = as_owned(w) else {
        return Ok(Some(w as i64));
    };
    if !can_alloc {
        return Err(FaultKind::Internal);
    }
    let (_, words) = f.coll(owner)?;
    let src = cell_bytes_ref(words, at, cell_w, c);
    if n as usize > src.len() {
        return Err(FaultKind::Internal);
    }
    let p = src.as_ptr();
    let (r, dst) = f.str_alloc(n)?;
    // COPY: ≤ str_cell_bytes of a cell's bytes into scratch — a Pine string is one contiguous
    // region-relative value and no StrRef can name a pool cell (F6) — handing out the owned-cell
    // word rejected: it is no StrRef, and every consumer would fault Internal.
    // SAFETY: `p` names `n` bytes of the pool block (F8: unmoved by `str_alloc`); `dst` is a fresh
    // scratch range of exactly `n` bytes — disjoint regions (F6).
    unsafe { core::ptr::copy_nonoverlapping(p, dst.as_mut_ptr(), n as usize) };
    Ok(Some(r.0 as i64))
}

/// Creator init (§4.16.1): (re)initialise the calling site's block as `kind` of `cap` with `len`
/// live elements, derive its layout and write pine-std's header fields; a map's index and pool
/// words are cleared (O(index)), nothing else. `__cap` na or negative, or a block smaller than
/// the layout, is `Internal` (sema and std disagree, I32). The generation is bumped before the
/// size check can fail (`pin:04-coll-size-check-after-init`: only `coll_init` reveals the block's
/// size to a body; the fault aborts the step either way).
pub(crate) fn create(
    f: &mut Frame<'_>,
    site: SiteId,
    kind: CollKind,
    elem: Ty,
    key: Ty,
    cap: Option<i64>,
    len: u32,
) -> Result<(CollRef, Layout), FaultKind> {
    let cap = match cap {
        Some(c) if c >= 0 && c <= u32::MAX as i64 => c as u32,
        _ => return Err(FaultKind::Internal),
    };
    if len > cap {
        return Err(FaultKind::PoolFull);
    }
    let r = f.coll_init(site, kind, elem, key, 0)?;
    let (h, words) = f.coll_mut(r)?;
    let lay = Layout::of(kind, elem, key, cap, words.len() as u64)?;
    h.cap = cap;
    h.len = len;
    h.idx_off = 0;
    h.idx_mask = 0;
    if matches!(kind, CollKind::Map) {
        h.idx_off = h.data_off + 8 * lay.idx;
        h.idx_mask = lay.idx_n - 1;
        let lo = lay.idx as usize;
        let hi = lo + (lay.idx_n / 2) as usize + n_str(kind, elem, key) as usize;
        let mut k = lo;
        while k < hi {
            words[k] = 0;
            k += 1;
        }
    }
    Ok((r, lay))
}

/// The layout of a resolved owner (its header's types and `cap` over its storage words).
pub(crate) fn layout_of(f: &Frame<'_>, o: &Owned) -> Result<Layout, FaultKind> {
    let (h, words) = f.coll(o.owner)?;
    Layout::of(o.kind, o.elem, o.key, h.cap, words.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYS: [Ty; 7] = [
        Ty::Int,
        Ty::Float,
        Ty::Bool,
        Ty::Color,
        Ty::String,
        Ty::Enum,
        Ty::Udt,
    ];

    /// I32 (the in-crate half of `coll_block_bytes_matches_layout`): for every kind / elem / key,
    /// `cap` 0..=4096 and cell sizes 8..=512, the layout a body derives from a block of exactly
    /// `coll_block_bytes` words has the manifest's cell and tiles the block exactly; a block one
    /// word short of its non-cell part, or without one word per cell, is `Internal`.
    #[test]
    fn layout_tiles_exactly_the_block_coll_block_bytes_sizes() {
        let mut cells = [0u32; 16];
        let mut k = 0;
        while k < 16 {
            cells[k] = 8 + 32 * k as u32 - (k as u32 % 3); // 8, 39, 70, … (unaligned too)
            k += 1;
        }
        cells[15] = 512;
        for kind in [CollKind::Array, CollKind::Map] {
            for elem in TYS {
                for key in if kind == CollKind::Map {
                    &TYS[..]
                } else {
                    &[Ty::Void][..]
                } {
                    let ns = n_str(kind, elem, *key) as u32;
                    for cap in 0..=4096u32 {
                        for &cb in &cells {
                            let bytes = coll_block_bytes(kind, elem, *key, cap, cb);
                            assert_eq!(bytes % 8, 0);
                            let n = ((bytes - 32) / 8) as u64;
                            let l = Layout::of(kind, elem, *key, cap, n).unwrap();
                            let s = if ns == 0 || cap == 0 {
                                0
                            } else {
                                cb.div_ceil(8)
                            };
                            assert_eq!(l.cell_w, s, "{kind:?} {elem:?} {key:?} {cap} {cb}");
                            let end = match kind {
                                CollKind::Array => l.kcells + ns * cap * s,
                                _ => l.pool + ns,
                            };
                            assert_eq!(end as u64, n, "{kind:?} {elem:?} {key:?} {cap} {cb}");
                            let fixed = fixed_words(kind, elem, *key, cap);
                            if fixed > 0 {
                                assert_eq!(
                                    Layout::of(kind, elem, *key, cap, fixed - 1),
                                    Err(FaultKind::Internal)
                                );
                            }
                            if ns > 0 && cap > 0 {
                                let short = fixed + (ns * cap) as u64 - 1;
                                assert_eq!(
                                    Layout::of(kind, elem, *key, cap, short),
                                    Err(FaultKind::Internal)
                                );
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(
            coll_block_bytes(CollKind::Matrix, Ty::Int, Ty::Void, 4, 8),
            0
        );
        assert_eq!(
            coll_block_bytes(CollKind::Object, Ty::Int, Ty::Void, 4, 8),
            0
        );
        assert_eq!(
            coll_block_bytes(CollKind::Array, Ty::Float, Ty::Void, 0, 128),
            32
        );
        assert_eq!(
            coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, u32::MAX, 512),
            u32::MAX
        );
    }

    #[test]
    fn view_block_is_the_header() {
        assert_eq!(
            VIEW_BLOCK_BYTES as usize,
            core::mem::size_of::<CollHeader>()
        );
    }

    #[test]
    fn owned_words_round_trip_and_never_alias_na_or_const() {
        let w = owned_word(17, 99);
        assert_eq!(as_owned(w), Some((17, 99)));
        assert_eq!(as_owned(NA_I64 as u64), None);
        assert_eq!(as_owned(StrRef::konst(17, 99).0), None);
    }

    #[test]
    fn index_normalisation_follows_w15() {
        assert_eq!(norm_index(Some(-1), 3, true), Ok(2));
        assert_eq!(norm_index(Some(-3), 3, true), Ok(0));
        assert_eq!(
            norm_index(Some(-4), 3, true),
            Err(FaultKind::CollectionBounds)
        );
        assert_eq!(
            norm_index(Some(-1), 3, false),
            Err(FaultKind::CollectionBounds)
        );
        assert_eq!(
            norm_index(Some(3), 3, true),
            Err(FaultKind::CollectionBounds)
        );
        assert_eq!(norm_index(None, 3, true), Err(FaultKind::CollectionBounds));
    }

    #[test]
    fn pool_bumps_then_reuses_freed_cells() {
        let mut words = [0u64; 12];
        let mut p = Pool { bump: 0, free: 0 };
        assert_eq!(p.take(&words, 0, 4, 3), Ok(0));
        assert_eq!(p.take(&words, 0, 4, 3), Ok(1));
        p.give(&mut words, 0, 4, 0);
        assert_eq!(p.take(&words, 0, 4, 3), Ok(0));
        assert_eq!(p.take(&words, 0, 4, 3), Ok(2));
        assert_eq!(p.take(&words, 0, 4, 3), Err(FaultKind::Internal));
        assert_eq!(p.bump, 3);
    }
}
