// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `map.*` (impl-04 §4.16.7, T35): keys `[0, cap)` and values `[cap, 2cap)` in insertion order,
//! an open-addressing index (linear probing, `u32` entries `slot + 1`, 0 = empty) hashed with
//! FNV-1a 64 over each key's canonical bytes, a cell pool per string component. Every body reads
//! (lookup through `coll` + `str_bytes`, shared borrows) before it writes (`coll_mut`), so all
//! checks precede every write (I14).

use super::array::{declared, flags, put_elem, word_of};
use super::{
    as_owned, cell_bytes_ref, create, elem_ty, release, resolve, store, str_fits, str_val, Layout,
    Owned, Pool, StrVal,
};
use pine_ir::{argwin, CollKind, FaultKind, SiteId, StrRef, Ty, NA_I64};
use pine_spec::ov;
use pine_vm::Frame;

/// The rows' `STR_ALLOC` flags as pine-spec gives them (F10, spec 7d2ac894…): a row with it hands
/// out a runtime-built string value through `str_alloc`.
const GET_STR: bool = flags(ov::MAP_GET) & pine_vm::bf::STR_ALLOC != 0;
const PUT_STR: bool = flags(ov::MAP_PUT) & pine_vm::bf::STR_ALLOC != 0;
const REMOVE_STR: bool = flags(ov::MAP_REMOVE) & pine_vm::bf::STR_ALLOC != 0;

const FNV_OFF: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

fn fnv(h: u64, b: &[u8]) -> u64 {
    let mut h = h;
    let mut i = 0usize;
    while i < b.len() {
        h = (h ^ b[i] as u64).wrapping_mul(FNV_PRIME);
        i += 1;
    }
    h
}

/// An incoming key: its storage word (canonical for floats: `−0.0 → +0.0`, every NaN one
/// pattern) and, for a non-na string, its bytes.
#[derive(Clone, Copy)]
struct KeyIn {
    word: u64,
    sv: StrVal,
}

fn key_in(f: &Frame<'_>, e: u16, kty: Ty) -> Result<KeyIn, FaultKind> {
    if matches!(kty, Ty::String) {
        if !argwin::is_i64(e) {
            return Err(FaultKind::Internal);
        }
        let sv = str_val(f, e);
        let word = match sv {
            StrVal::Const(b) => b,
            _ => NA_I64 as u64,
        };
        return Ok(KeyIn { word, sv });
    }
    let mut w = word_of(f, e, kty)?;
    if matches!(kty, Ty::Float) {
        let x = f64::from_bits(w);
        w = if x.is_nan() {
            f64::NAN.to_bits()
        } else if x == 0.0 {
            0
        } else {
            w
        };
    }
    Ok(KeyIn {
        word: w,
        sv: StrVal::Na,
    })
}

/// The bytes of an incoming string key (`None` = na).
fn key_bytes<'a>(f: &'a Frame<'_>, k: &KeyIn) -> Option<&'a [u8]> {
    match k.sv {
        StrVal::Na => None,
        StrVal::Const(b) => Some(f.str_bytes(StrRef(b))),
        // SAFETY: `p` names `n` live bytes of the SCRATCH / PERSIST region for the evaluation
        // (F8); only read here.
        StrVal::Bytes(p, n) => Some(unsafe { core::slice::from_raw_parts(p, n as usize) }),
    }
}

/// The bytes of stored string key word `w` (`None` = na): a `CONST` ref's, or its owned cell's.
fn stored_bytes<'a>(f: &'a Frame<'_>, words: &'a [u64], lay: &Layout, w: u64) -> Option<&'a [u8]> {
    if w == NA_I64 as u64 {
        return None;
    }
    match as_owned(w) {
        Some((c, n)) => {
            let b = cell_bytes_ref(words, lay.kcells, lay.cell_w, c);
            let n = if (n as usize) < b.len() {
                n as usize
            } else {
                b.len()
            };
            Some(&b[..n])
        }
        None => Some(f.str_bytes(StrRef(w))),
    }
}

fn hash_bytes(b: Option<&[u8]>, word: u64) -> u64 {
    match b {
        Some(b) => fnv(FNV_OFF, b),
        None => fnv(FNV_OFF, &word.to_le_bytes()),
    }
}

fn ent(words: &[u64], lay: &Layout, j: u32) -> u32 {
    let w = words[(lay.idx + j / 2) as usize];
    if j & 1 == 0 {
        w as u32
    } else {
        (w >> 32) as u32
    }
}

fn set_ent(words: &mut [u64], lay: &Layout, j: u32, v: u32) {
    let w = &mut words[(lay.idx + j / 2) as usize];
    *w = if j & 1 == 0 {
        (*w & !0xFFFF_FFFF) | v as u64
    } else {
        (*w & 0xFFFF_FFFF) | (v as u64) << 32
    };
}

/// Look `k` up: `(Some(slot), _)` when present, else `(None, j)` with `j` the empty index entry
/// an insert takes. The index holds at most `cap` entries of `≥ 2·cap`, so a probe ends.
fn find(
    f: &Frame<'_>,
    o: &Owned,
    lay: &Layout,
    k: &KeyIn,
) -> Result<(Option<u32>, u32), FaultKind> {
    let (_, words) = f.coll(o.owner)?;
    let s = matches!(o.key, Ty::String);
    let kb = if s { key_bytes(f, k) } else { None };
    let mask = lay.idx_n - 1;
    let mut j = (hash_bytes(kb, k.word) as u32) & mask;
    let mut n = 0u32;
    while n < lay.idx_n {
        let e = ent(words, lay, j);
        if e == 0 {
            return Ok((None, j));
        }
        let w = words[(e - 1) as usize];
        let hit = if s {
            match (kb, stored_bytes(f, words, lay, w)) {
                (None, None) => true,
                (Some(a), Some(b)) => a == b,
                _ => false,
            }
        } else {
            w == k.word
        };
        if hit {
            return Ok((Some(e - 1), j));
        }
        j = (j + 1) & mask;
        n += 1;
    }
    Err(FaultKind::Internal)
}

fn map(f: &Frame<'_>, e: u16) -> Result<(Owned, Layout), FaultKind> {
    let o = resolve(f, f.coll_arg(e))?;
    if !matches!(o.kind, CollKind::Map) || o.view {
        return Err(FaultKind::Internal);
    }
    let (h, words) = f.coll(o.owner)?;
    let lay = Layout::of(o.kind, o.elem, o.key, h.cap, words.len() as u64)?;
    Ok((o, lay))
}

/// The value layout as `put_elem` reads it: string values' cells are `vcells`.
fn vlay(lay: &Layout) -> Layout {
    Layout {
        kcells: lay.vcells,
        ..*lay
    }
}

/// The na storage word of an element of `t` (float: NaN; anything else: `NA_I64`).
fn na_word(t: Ty) -> u64 {
    if matches!(t, Ty::Float) {
        f64::NAN.to_bits()
    } else {
        NA_I64 as u64
    }
}

fn kpool_at(lay: &Layout) -> usize {
    lay.pool as usize
}

fn vpool_at(o: &Owned, lay: &Layout) -> usize {
    lay.pool as usize + matches!(o.key, Ty::String) as usize
}

/// `map.get(id, key)` — `[id, key, out]`: the value, or na for an absent key (a v6 `bool`:
/// false) — never a fault.
fn map_get(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (o, lay) = map(f, args[0])?;
    let k = key_in(f, args[1], o.key)?;
    let w = match find(f, &o, &lay, &k)?.0 {
        Some(s) => f.coll(o.owner)?.1[(lay.vals + s) as usize],
        None => na_word(o.elem),
    };
    put_elem(f, args[2], &o, &vlay(&lay), w, GET_STR)
}

/// `map.contains(id, key)` — `[id, key, out]`.
fn map_contains(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (o, lay) = map(f, args[0])?;
    let k = key_in(f, args[1], o.key)?;
    let hit = find(f, &o, &lay, &k)?.0.is_some();
    f.set_i64_arg(args[2], Some(hit as i64));
    Ok(())
}

/// `map.put(id, key, value)` — `[id, key, value, out]`: replaces a present key's value in place
/// (its position kept) and returns the previous value; a new key is appended (insertion order)
/// and returns na (`pin:04-collection-edges`). A new key past `cap` is `PoolFull`.
fn map_put(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let (o, lay) = map(f, args[0])?;
    let k = key_in(f, args[1], o.key)?;
    str_fits(k.sv, &lay)?;
    let (vw, vs) = if matches!(o.elem, Ty::String) {
        let v = str_val(f, args[2]);
        str_fits(v, &lay)?;
        (0, Some(v))
    } else {
        (word_of(f, args[2], o.elem)?, None)
    };
    let (slot, j) = find(f, &o, &lay, &k)?;
    let vl = vlay(&lay);
    let s = match slot {
        Some(s) => {
            let old = f.coll(o.owner)?.1[(lay.vals + s) as usize];
            // The previous value is handed out before anything changes.
            put_elem(f, args[3], &o, &vl, old, PUT_STR)?;
            s
        }
        None => {
            if o.len >= lay.cap {
                return Err(FaultKind::PoolFull);
            }
            put_elem(f, args[3], &o, &vl, na_word(o.elem), PUT_STR)?;
            let s = o.len;
            let (h, words) = f.coll_mut(o.owner)?;
            let kw = if matches!(o.key, Ty::String) {
                let at = kpool_at(&lay);
                let mut p = Pool::of_word(words[at]);
                let w = store(&mut p, words, lay.kcells, &lay, k.sv)?;
                words[at] = p.to_word();
                w
            } else {
                k.word
            };
            words[s as usize] = kw;
            words[(lay.vals + s) as usize] = NA_I64 as u64;
            set_ent(words, &lay, j, s + 1);
            h.len = s + 1;
            s
        }
    };
    let (_, words) = f.coll_mut(o.owner)?;
    let at = (lay.vals + s) as usize;
    match vs {
        None => words[at] = vw,
        Some(v) => {
            let pa = vpool_at(&o, &lay);
            let mut p = Pool::of_word(words[pa]);
            let old = words[at];
            release(&mut p, words, lay.vcells, lay.cell_w, old);
            words[at] = store(&mut p, words, lay.vcells, &vl, v)?;
            words[pa] = p.to_word();
        }
    }
    Ok(())
}

/// `map.remove(id, key)` — `[id, key, out]`: the removed value or na; later pairs shift down
/// (insertion order kept) and the index is rebuilt.
fn map_remove(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (o, lay) = map(f, args[0])?;
    let k = key_in(f, args[1], o.key)?;
    let vl = vlay(&lay);
    let Some(s) = find(f, &o, &lay, &k)?.0 else {
        return put_elem(f, args[2], &o, &vl, na_word(o.elem), REMOVE_STR);
    };
    let old = f.coll(o.owner)?.1[(lay.vals + s) as usize];
    put_elem(f, args[2], &o, &vl, old, REMOVE_STR)?;
    let (h, words) = f.coll_mut(o.owner)?;
    let (kw, vw) = (words[s as usize], words[(lay.vals + s) as usize]);
    if matches!(o.key, Ty::String) {
        let at = kpool_at(&lay);
        let mut p = Pool::of_word(words[at]);
        release(&mut p, words, lay.kcells, lay.cell_w, kw);
        words[at] = p.to_word();
    }
    if matches!(o.elem, Ty::String) {
        let at = vpool_at(&o, &lay);
        let mut p = Pool::of_word(words[at]);
        release(&mut p, words, lay.vcells, lay.cell_w, vw);
        words[at] = p.to_word();
    }
    let n = h.len;
    // COPY: ≤ 2 × cap × 8 B of key / value words inside the block — removal keeps insertion order
    // (verified, maps page) — a tombstone layout rejected (keys()/values() would skip holes).
    words.copy_within((s + 1) as usize..n as usize, s as usize);
    let v = lay.vals as usize;
    words.copy_within(v + (s + 1) as usize..v + n as usize, v + s as usize);
    h.len = n - 1;
    rebuild(f, &o, &lay, n - 1)
}

/// Clear the index and re-insert slots `[0, len)` (O(index) + O(len) probes).
fn rebuild(f: &mut Frame<'_>, o: &Owned, lay: &Layout, len: u32) -> Result<(), FaultKind> {
    {
        let (_, words) = f.coll_mut(o.owner)?;
        let mut k = 0u32;
        while k < lay.idx_n / 2 {
            words[(lay.idx + k) as usize] = 0;
            k += 1;
        }
    }
    let s_key = matches!(o.key, Ty::String);
    let mask = lay.idx_n - 1;
    let mut s = 0u32;
    while s < len {
        let j = {
            let (_, words) = f.coll(o.owner)?;
            let w = words[s as usize];
            let b = if s_key {
                stored_bytes(f, words, lay, w)
            } else {
                None
            };
            let mut j = (hash_bytes(b, w) as u32) & mask;
            while ent(words, lay, j) != 0 {
                j = (j + 1) & mask;
            }
            j
        };
        let (_, words) = f.coll_mut(o.owner)?;
        set_ent(words, lay, j, s + 1);
        s += 1;
    }
    Ok(())
}

/// `map.clear(id)` — `[id]`: no pairs, an empty index, both pools reset.
fn map_clear(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let (o, lay) = map(f, args[0])?;
    let (h, words) = f.coll_mut(o.owner)?;
    h.len = 0;
    let hi = (lay.pool + super::n_str(o.kind, o.elem, o.key) as u32) as usize;
    let mut k = lay.idx as usize;
    while k < hi {
        words[k] = 0;
        k += 1;
    }
    Ok(())
}

/// `map.keys(id)` / `map.values(id)` — `[__elem, __key, __cap, id, out]`: a new array of copies of
/// the keys (values) in insertion order; a runtime-built string is copied into the new array's
/// own cell. More pairs than the new array's `__cap` is `PoolFull`.
fn keys_or_values<const VALUES: bool>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let (o, lay) = map(f, args[3])?;
    let ty = if VALUES { o.elem } else { o.key };
    let enc_ty = f.i64_arg(args[0]).and_then(elem_ty);
    if enc_ty != Some(ty) {
        return Err(FaultKind::Internal);
    }
    let n = o.len;
    let (r, al) = create(
        f,
        site,
        CollKind::Array,
        ty,
        Ty::Void,
        f.i64_arg(args[2]),
        n,
    )?;
    let (base, cells) = if VALUES {
        (lay.vals, lay.vcells)
    } else {
        (0, lay.kcells)
    };
    let src: *const u64 = f.coll(o.owner)?.1.as_ptr();
    let (h, words) = f.coll_mut(r)?;
    let mut p = Pool::of_header(h);
    let mut k = 0u32;
    while k < n {
        // SAFETY: `src` is the map's storage (another block than `r`'s — a creator site never
        // creates the map it reads), unmoved by `coll_mut` (F8); `base + k < 2·cap` words.
        let w = unsafe { *src.add((base + k) as usize) };
        words[k as usize] = match as_owned(w) {
            Some((c, len)) => {
                if len > al.cell_bytes() {
                    return Err(FaultKind::StrArenaFull);
                }
                // SAFETY: as above; the cell `[cells + c·cell_w, +cell_w)` lies in the map block.
                let sp = unsafe { src.add((cells + c * lay.cell_w) as usize).cast::<u8>() };
                let sv = StrVal::Bytes(sp, len);
                let w2 = store(&mut p, words, al.kcells, &al, sv)?;
                debug_assert!(as_owned(w2).is_some());
                w2
            }
            None => w,
        };
        k += 1;
    }
    p.to_header(h);
    f.set_i64_arg(args[4], Some(r.0 as i64));
    Ok(())
}

// ---- copy / put_all ---------------------------------------------------------------------------

/// The store form of stored string word `w` of a block whose storage starts at `sp` and whose
/// cells (of `cell_w` words) start at word `cells`: na, a `CONST` ref, or the cell's bytes.
fn stored_val(sp: *const u64, cells: u32, cell_w: u32, w: u64) -> StrVal {
    if w == NA_I64 as u64 {
        return StrVal::Na;
    }
    match as_owned(w) {
        // SAFETY: the cell `[cells + c·cell_w, +cell_w)` lies in the block at `sp` (F8: unmoved
        // for the evaluation); `n` ≤ its bytes (stored so).
        Some((c, n)) => StrVal::Bytes(
            unsafe { sp.add((cells + c * cell_w) as usize).cast::<u8>() },
            n,
        ),
        None => StrVal::Const(w),
    }
}

/// `map.copy(id)` — `[__elem, __key, __cap, id, out]`: a new map of the same pairs in the same
/// insertion order (shallow, verified, maps page); runtime-built string keys / values copied into
/// the new block's own cells, the index rebuilt. More pairs than `__cap` is `PoolFull`, a string
/// longer than the new cell `StrArenaFull`. `pin:04-map-copy-self`: `m := map.copy(m)` (the source
/// is the block this site re-creates) keeps the pairs in place under a new generation.
fn map_copy(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let (o, sl) = map(f, args[3])?;
    if f.i64_arg(args[0]).and_then(elem_ty) != Some(o.elem)
        || f.i64_arg(args[1]).and_then(elem_ty) != Some(o.key)
    {
        return Err(FaultKind::Internal);
    }
    let cap = f.i64_arg(args[2]);
    if o.owner.site() == site {
        if cap != Some(sl.cap as i64) {
            return Err(FaultKind::Internal);
        }
        let h0 = *f.coll(o.owner)?.0;
        let r = f.coll_init(site, CollKind::Map, o.elem, o.key, 0)?;
        let (h, _) = f.coll_mut(r)?;
        h.cap = h0.cap;
        h.len = h0.len;
        h.idx_off = h0.idx_off;
        h.idx_mask = h0.idx_mask;
        f.set_i64_arg(args[4], Some(r.0 as i64));
        return Ok(());
    }
    let n = o.len;
    let sp: *const u64 = f.coll(o.owner)?.1.as_ptr();
    let (r, dl) = create(f, site, CollKind::Map, o.elem, o.key, cap, n)?;
    let (ks, vs) = (matches!(o.key, Ty::String), matches!(o.elem, Ty::String));
    let mut k = 0u32;
    while k < n {
        // SAFETY: the source map's key / value words `k < len <= cap` (F8).
        let (kw, vw) = unsafe { (*sp.add(k as usize), *sp.add((sl.vals + k) as usize)) };
        let kv = if ks {
            stored_val(sp, sl.kcells, sl.cell_w, kw)
        } else {
            StrVal::Na
        };
        let vv = if vs {
            stored_val(sp, sl.vcells, sl.cell_w, vw)
        } else {
            StrVal::Na
        };
        str_fits(kv, &dl)?;
        str_fits(vv, &dl)?;
        k += 1;
    }
    let vl = vlay(&dl);
    let (_, words) = f.coll_mut(r)?;
    let (kp, vp) = (kpool_at(&dl), dl.pool as usize + ks as usize);
    let mut k = 0u32;
    while k < n {
        // SAFETY: as above.
        let (kw, vw) = unsafe { (*sp.add(k as usize), *sp.add((sl.vals + k) as usize)) };
        words[k as usize] = if ks {
            let mut p = Pool::of_word(words[kp]);
            let w = store(
                &mut p,
                words,
                dl.kcells,
                &dl,
                stored_val(sp, sl.kcells, sl.cell_w, kw),
            )?;
            words[kp] = p.to_word();
            w
        } else {
            kw
        };
        words[(dl.vals + k) as usize] = if vs {
            let mut p = Pool::of_word(words[vp]);
            let w = store(
                &mut p,
                words,
                dl.vcells,
                &vl,
                stored_val(sp, sl.vcells, sl.cell_w, vw),
            )?;
            words[vp] = p.to_word();
            w
        } else {
            vw
        };
        k += 1;
    }
    let no = resolve(f, Some(r))?;
    rebuild(f, &no, &dl, n)?;
    f.set_i64_arg(args[4], Some(r.0 as i64));
    Ok(())
}

/// `map.put_all(id, id2)` — `[id, id2]`: `id2`'s pairs put into `id` in `id2`'s order (verified,
/// maps page): a present key's value replaced in place, a new key appended. All checks first:
/// the new keys past `id`'s `cap` is `PoolFull`, a string longer than `id`'s cell `StrArenaFull`.
/// `put_all(m, m)` puts every pair onto itself — nothing changes (the literal reading).
fn map_put_all(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let (o, lay) = map(f, args[0])?;
    let (o2, l2) = map(f, args[1])?;
    if o.elem != o2.elem || o.key != o2.key {
        return Err(FaultKind::Internal);
    }
    if o.owner == o2.owner {
        return Ok(());
    }
    let (ks, vs) = (matches!(o.key, Ty::String), matches!(o.elem, Ty::String));
    let sp: *const u64 = f.coll(o2.owner)?.1.as_ptr();
    let key_at = |k: u32| -> KeyIn {
        // SAFETY: `id2`'s key word `k < len <= cap` (F8: unmoved).
        let w = unsafe { *sp.add(k as usize) };
        if ks {
            let sv = stored_val(sp, l2.kcells, l2.cell_w, w);
            let word = match sv {
                StrVal::Const(b) => b,
                _ => NA_I64 as u64,
            };
            KeyIn { word, sv }
        } else {
            KeyIn {
                word: w,
                sv: StrVal::Na,
            }
        }
    };
    let mut fresh = 0u32;
    let mut k = 0u32;
    while k < o2.len {
        let ki = key_at(k);
        str_fits(ki.sv, &lay)?;
        if vs {
            // SAFETY: `id2`'s value word `k < len` (F8).
            let vw = unsafe { *sp.add((l2.vals + k) as usize) };
            str_fits(stored_val(sp, l2.vcells, l2.cell_w, vw), &lay)?;
        }
        fresh += find(f, &o, &lay, &ki)?.0.is_none() as u32;
        k += 1;
    }
    if o.len as u64 + fresh as u64 > lay.cap as u64 {
        return Err(FaultKind::PoolFull);
    }
    let vl = vlay(&lay);
    let mut k = 0u32;
    while k < o2.len {
        let ki = key_at(k);
        // SAFETY: as above.
        let vw = unsafe { *sp.add((l2.vals + k) as usize) };
        let (slot, j) = find(f, &o, &lay, &ki)?;
        let (h, words) = f.coll_mut(o.owner)?;
        let s = match slot {
            Some(s) => s,
            None => {
                let s = h.len;
                words[s as usize] = if ks {
                    let at = kpool_at(&lay);
                    let mut p = Pool::of_word(words[at]);
                    let w = store(&mut p, words, lay.kcells, &lay, ki.sv)?;
                    words[at] = p.to_word();
                    w
                } else {
                    ki.word
                };
                words[(lay.vals + s) as usize] = NA_I64 as u64;
                set_ent(words, &lay, j, s + 1);
                h.len = s + 1;
                s
            }
        };
        let at = (lay.vals + s) as usize;
        if vs {
            let pa = vpool_at(&o, &lay);
            let mut p = Pool::of_word(words[pa]);
            let old = words[at];
            release(&mut p, words, lay.vcells, lay.cell_w, old);
            words[at] = store(
                &mut p,
                words,
                lay.vcells,
                &vl,
                stored_val(sp, l2.vcells, l2.cell_w, vw),
            )?;
            words[pa] = p.to_word();
        } else {
            words[at] = vw;
        }
        k += 1;
    }
    Ok(())
}

fn map_keys(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    keys_or_values::<false>(f, site, args)
}

fn map_values(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    keys_or_values::<true>(f, site, args)
}

// ---- the rows ------------------------------------------------------------------------------

/// The `map.get` row.
pub fn bi_map_get(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_get(f, site, args).map_err(|k| declared(flags(ov::MAP_GET), k))
}
/// The `map.contains` row.
pub fn bi_map_contains(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_contains(f, site, args).map_err(|k| declared(flags(ov::MAP_CONTAINS), k))
}
/// The `map.put` row.
pub fn bi_map_put(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_put(f, site, args).map_err(|k| declared(flags(ov::MAP_PUT), k))
}
/// The `map.remove` row.
pub fn bi_map_remove(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_remove(f, site, args).map_err(|k| declared(flags(ov::MAP_REMOVE), k))
}
/// The `map.clear` row.
pub fn bi_map_clear(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_clear(f, site, args).map_err(|k| declared(flags(ov::MAP_CLEAR), k))
}
/// The `map.keys` row.
pub fn bi_map_keys(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_keys(f, site, args).map_err(|k| declared(flags(ov::MAP_KEYS), k))
}
/// The `map.copy` row.
pub fn bi_map_copy(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_copy(f, site, args).map_err(|k| declared(flags(ov::MAP_COPY), k))
}
/// The `map.put_all` row.
pub fn bi_map_put_all(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_put_all(f, args).map_err(|k| declared(flags(ov::MAP_PUT_ALL), k))
}
/// The `map.values` row.
pub fn bi_map_values(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    map_values(f, site, args).map_err(|k| declared(flags(ov::MAP_VALUES), k))
}
