// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `matrix.*` — tier 4 (CAR-R40-2, r40; ruling R-P34 "Matrix tier 4 now", overriding R-P5).
//!
//! **Block.** A matrix is a `Collection` block of kind [`CollKind::Matrix`] of its creating site,
//! sized like an array's (`coll_block_bytes(Array, …, collection_cap)`, the same words): the
//! header's `len` = `rows × columns`, `idx_off` = rows, `idx_mask` = columns
//! (`pin:04-mtx-header`: a matrix has no map index, so the two words carry its shape); elements
//! row-major in the storage words, encoded as an array's (a float its bits, NaN = na; int / bool /
//! color / enum an i64, `NA_I64` = na). `string` / UDT / drawing elements are not served this
//! round (pine-spec's `T` admits int, float, bool, color, enum).
//!
//! **Creators** (`new`, `copy`, `submatrix`, `transpose`, `row`, `col`, `remove_*`, `sum`, `diff`,
//! `mult`, `pow`, `kron`, `inv`, `pinv`, `eigen*`) re-initialise their own site's block each
//! evaluation, as `array.new*` (§4.16). Their sources are read through raw pointers: another
//! site's storage is disjoint (F6) and unmoved (F8); a source that is this site's previous block
//! (`m := matrix.mult(m, x)`) is read through the destination's own pointer below `off`, the
//! result is written from `off` and moved to the front at the end ([`Dst::finish`]).
//!
//! **Scratch.** The numeric creators and `det` / `rank` work in their own block past the result
//! (`pin:04-mtx-scratch`): capacity is the collection cap `C` words — `inv` needs `2n² + 2n`,
//! `pinv` / `rank` `mn + n² + n` (+ the result), `eigen*` `n² + 2n`, `pow` `2n²`, `det` `n² + n`;
//! more → `PoolFull`. Such a site is a creator site, so it is refused inside a loop body (`3046`,
//! `pin:04-mtx-numeric-site`).
//!
//! **Numerics** (no TradingView capture this round — every numeric pin is provisional): `det` and
//! `inv` by LU with partial pivoting; a zero pivot is singular: `det` 0, `inv` all na
//! (`pin:04-mtx-singular`); `pinv` and `rank` by a one-sided Jacobi SVD, tolerance
//! `max(m, n) · σ_max · 2⁻⁵²` (`pin:04-mtx-svd-tol`); `eigenvalues` / `eigenvectors` by the
//! symmetric Householder tridiagonalisation and the implicit QL algorithm (the reference: "The
//! Implicit QL Algorithm"), eigenvalues ascending with their vectors as columns
//! (`pin:04-mtx-eigen-symmetric`: the symmetric path for any input); an int matrix's results are
//! computed in f64 and taken toward zero into its i64 cells (`pin:04-mtx-int-numeric`); na
//! elements propagate as NaN.

use core::cmp::Ordering;

use super::array::{bool_never_na, put_word, word_of};
use super::{order, resolve, stats};
use pine_ir::{argwin, CollKind, CollRef, FaultKind, SiteId, Ty, NA_I64};
use pine_vm::Frame;

const NA: u64 = NA_I64 as u64;
/// 2⁻⁵², the binary64 epsilon.
const EPS: f64 = f64::EPSILON;
/// Sweeps of the Jacobi SVD before it gives up (na result).
const SVD_SWEEPS: u32 = 64;
/// QL iterations per eigenvalue before it gives up (na result).
const QL_ITERS: u32 = 64;
/// 2⁶³: an int result at or beyond it is na.
const I64_LIM: f64 = 9_223_372_036_854_775_808.0;
/// `pin:04-mtx-stochastic-tol` — a stochastic row sums to 1 within this (CAR-R33-7's 1e-10).
const STOCH_TOL: f64 = 1e-10;

// ---- element words ---------------------------------------------------------------------------

#[inline(always)]
fn val(w: u64, float: bool) -> f64 {
    if float {
        f64::from_bits(w)
    } else if w == NA {
        f64::NAN
    } else {
        w as i64 as f64
    }
}

/// An f64 result as an element word: a float as its bits; an int rounded to nearest, half away
/// from zero (an int determinant through LU is 8.999999999999998 for 9), non-finite or beyond
/// `i64` → na (`pin:04-mtx-int-numeric`).
#[inline(always)]
fn enc(v: f64, float: bool) -> u64 {
    if float {
        v.to_bits()
    } else if v.is_finite() && v.abs() < I64_LIM {
        v.round() as i64 as u64
    } else {
        NA
    }
}

/// Whether `elem` is float (`true`) or int (`false`); any other element is `Internal` (sema
/// resolves the numeric rows on `matrix<int>` / `matrix<float>` only).
fn num(elem: Ty) -> Result<bool, FaultKind> {
    match elem {
        Ty::Float => Ok(true),
        Ty::Int => Ok(false),
        _ => Err(FaultKind::Internal),
    }
}

/// Write `v` to a scalar output: the i64 lane toward zero (na when not finite), else f64.
fn put(f: &mut Frame<'_>, e: u16, v: f64) {
    if argwin::is_i64(e) {
        let ok = v.is_finite() && v.abs() < I64_LIM;
        f.set_i64_arg(e, ok.then(|| v.trunc() as i64));
    } else {
        f.set_f64_arg(e, v);
    }
}

fn put_bool(f: &mut Frame<'_>, e: u16, b: bool) {
    f.set_i64_arg(e, Some(i64::from(b)));
}

/// A non-na int index argument, or `CollectionBounds`.
fn index(f: &Frame<'_>, e: u16) -> Result<i64, FaultKind> {
    f.i64_arg(e).ok_or(FaultKind::CollectionBounds)
}

/// An optional bound: na → `dflt`.
fn bound_or(f: &Frame<'_>, e: u16, dflt: u32) -> i64 {
    f.i64_arg(e).unwrap_or(i64::from(dflt))
}

// ---- references --------------------------------------------------------------------------------

/// A matrix reference and its shape.
#[derive(Clone, Copy, Debug)]
struct Mx {
    r: CollRef,
    elem: Ty,
    rows: u32,
    cols: u32,
}

impl Mx {
    #[inline(always)]
    fn n(&self) -> usize {
        self.rows as usize * self.cols as usize
    }
}

/// The matrix of argument `e`: na or stale → `ScriptRefused` (contract §3.7); a block that is not
/// a consistent matrix → `Internal`.
fn mx(f: &Frame<'_>, e: u16) -> Result<Mx, FaultKind> {
    let r = f.coll_arg(e).ok_or(FaultKind::ScriptRefused)?;
    let (h, words) = f.coll(r)?;
    if h.kind != CollKind::Matrix as u8 || h.view != 0 {
        return Err(FaultKind::Internal);
    }
    let elem = Ty::from_u8(h.elem).ok_or(FaultKind::Internal)?;
    let n = u64::from(h.idx_off) * u64::from(h.idx_mask);
    if n != u64::from(h.len) || h.len > h.cap || h.len as usize > words.len() {
        return Err(FaultKind::Internal);
    }
    Ok(Mx {
        r,
        elem,
        rows: h.idx_off,
        cols: h.idx_mask,
    })
}

/// A source a body reads: its first element and length. `same` = this site's own previous block
/// (re-pointed into the destination by [`Dst::init`]).
#[derive(Clone, Copy)]
struct Src {
    p: *const u64,
    base: usize,
    len: usize,
    same: bool,
}

impl Src {
    #[inline(always)]
    fn at(&self, i: usize) -> u64 {
        debug_assert!(i < self.len);
        // SAFETY: `i < len`; `p` points at `len` readable words of the source's storage — another
        // block's (F6 disjoint from the destination, F8 unmoved during the call) or, `same`, the
        // destination's own words below `off` that the body never writes before `finish`.
        unsafe { *self.p.add(i) }
    }
}

/// The matrix `m` as a source.
fn src_mx(f: &Frame<'_>, m: &Mx, site: SiteId) -> Result<Src, FaultKind> {
    let (_, w) = f.coll(m.r)?;
    Ok(Src {
        p: w.as_ptr(),
        base: 0,
        len: m.n(),
        same: m.r.site() == site,
    })
}

/// The int / float array of argument `e` as a source (a view reads its window of the owner).
fn src_arr(f: &Frame<'_>, e: u16, site: SiteId) -> Result<(Src, Ty), FaultKind> {
    let o = resolve(f, f.coll_arg(e))?;
    if !matches!(o.kind, CollKind::Array) {
        return Err(FaultKind::Internal);
    }
    if o.base + o.len > o.owner_len {
        return Err(FaultKind::CollectionBounds);
    }
    let (_, w) = f.coll(o.owner)?;
    if (o.base + o.len) as usize > w.len() {
        return Err(FaultKind::Internal);
    }
    Ok((
        Src {
            // SAFETY: `base + len <= words` (checked above).
            p: unsafe { w.as_ptr().add(o.base as usize) },
            base: o.base as usize,
            len: o.len as usize,
            same: o.owner.site() == site,
        },
        o.elem,
    ))
}

/// The destination: this site's block, re-initialised; results are written from `off` (past the
/// words a same-site source still holds), scratch after the result.
struct Dst {
    r: CollRef,
    wp: *mut u64,
    nw: usize,
    off: usize,
    n: usize,
}

impl Dst {
    /// Re-initialise this site's block as a `kind` of `elem` holding `n` elements (a matrix:
    /// `rows × cols`) plus `extra` scratch words; same-site sources are re-pointed into it.
    /// `PoolFull` when it does not fit.
    #[allow(clippy::too_many_arguments)]
    fn init(
        f: &mut Frame<'_>,
        site: SiteId,
        kind: CollKind,
        elem: Ty,
        rows: u32,
        cols: u32,
        n: usize,
        extra: usize,
        srcs: &mut [Src],
    ) -> Result<Dst, FaultKind> {
        let mut off = 0usize;
        let mut k = 0usize;
        while k < srcs.len() {
            if srcs[k].same {
                off = off.max(srcs[k].base + srcs[k].len);
            }
            k += 1;
        }
        let r = f.coll_init(site, kind, elem, Ty::Void, 0)?;
        let (h, words) = f.coll_mut(r)?;
        let nw = words.len();
        let need = off
            .checked_add(n)
            .and_then(|x| x.checked_add(extra))
            .ok_or(FaultKind::PoolFull)?;
        if need > nw || n > h.cap as usize || n > u32::MAX as usize {
            return Err(FaultKind::PoolFull);
        }
        h.len = n as u32;
        if matches!(kind, CollKind::Matrix) {
            h.idx_off = rows;
            h.idx_mask = cols;
        }
        let wp = words.as_mut_ptr();
        let mut k = 0usize;
        while k < srcs.len() {
            if srcs[k].same {
                // SAFETY: `base + len <= off <= nw`: inside this block's words.
                srcs[k].p = unsafe { wp.add(srcs[k].base) };
            }
            k += 1;
        }
        Ok(Dst { r, wp, nw, off, n })
    }

    /// Result element `i`.
    #[inline(always)]
    fn set(&self, i: usize, w: u64) {
        debug_assert!(i < self.n && self.off + i < self.nw);
        // SAFETY: `off + i < off + n <= nw` (checked by `init`).
        unsafe { *self.wp.add(self.off + i) = w }
    }

    /// Scratch word `i` (past the result) as f64.
    #[inline(always)]
    fn s(&self, i: usize) -> f64 {
        let at = self.off + self.n + i;
        debug_assert!(at < self.nw);
        // SAFETY: `init` reserved the scratch words the body indexes (`extra`).
        f64::from_bits(unsafe { *self.wp.add(at) })
    }

    #[inline(always)]
    fn ss(&self, i: usize, v: f64) {
        let at = self.off + self.n + i;
        debug_assert!(at < self.nw);
        // SAFETY: as `s`.
        unsafe { *self.wp.add(at) = v.to_bits() }
    }

    /// Move the result to the front when a same-site source kept it from starting there.
    fn finish(&self) {
        if self.off > 0 && self.n > 0 {
            // COPY: 8 B × n inside the block, once — the result was written past the same-site
            // source it read (`m := matrix.mult(m, x)`) — computing in place over the source
            // rejected (the source is read until the last element).
            // SAFETY: `[off, off + n)` and `[0, n)` lie in the block's words; `copy` is a memmove.
            unsafe { core::ptr::copy(self.wp.add(self.off), self.wp, self.n) }
        }
    }
}

/// A square matrix's order, or `ScriptRefused`.
fn square(m: &Mx) -> Result<usize, FaultKind> {
    if m.rows == m.cols {
        Ok(m.rows as usize)
    } else {
        Err(FaultKind::ScriptRefused)
    }
}

// ---- creation and shape --------------------------------------------------------------------------

/// `matrix.new<T>(rows, columns, initial_value)` — `[__elem, __key, __cap, rows, columns, init,
/// out]`. A na or negative dimension is `ScriptRefused`; more elements than the block holds
/// `PoolFull`.
pub fn bi_matrix_new(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 7 {
        return Err(FaultKind::Internal);
    }
    let elem = f
        .i64_arg(args[0])
        .and_then(super::elem_ty)
        .ok_or(FaultKind::Internal)?;
    if matches!(elem, Ty::String | Ty::Udt) {
        return Err(FaultKind::Internal);
    }
    let (rows, cols) = match (f.i64_arg(args[3]), f.i64_arg(args[4])) {
        (Some(r), Some(c)) if r >= 0 && c >= 0 && r <= u32::MAX as i64 && c <= u32::MAX as i64 => {
            (r as u32, c as u32)
        }
        _ => return Err(FaultKind::ScriptRefused),
    };
    let n = (rows as u64)
        .checked_mul(cols as u64)
        .filter(|n| *n <= u32::MAX as u64)
        .ok_or(FaultKind::PoolFull)? as usize;
    let w = word_of(f, args[5], elem)?;
    let d = Dst::init(f, site, CollKind::Matrix, elem, rows, cols, n, 0, &mut [])?;
    let mut i = 0usize;
    while i < n {
        d.set(i, w);
        i += 1;
    }
    f.set_i64_arg(args[6], Some(d.r.0 as i64));
    Ok(())
}

/// `matrix.rows / columns / elements_count / is_square(id)` — `[id, out]`.
fn shape_of<const WHAT: u8>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    match WHAT {
        0 => f.set_i64_arg(args[1], Some(i64::from(m.rows))),
        1 => f.set_i64_arg(args[1], Some(i64::from(m.cols))),
        2 => f.set_i64_arg(args[1], Some(m.n() as i64)),
        _ => put_bool(f, args[1], m.rows == m.cols),
    }
    Ok(())
}

pub fn bi_matrix_rows(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    shape_of::<0>(f, args)
}
pub fn bi_matrix_columns(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    shape_of::<1>(f, args)
}
pub fn bi_matrix_elements_count(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    shape_of::<2>(f, args)
}
pub fn bi_matrix_is_square(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    shape_of::<3>(f, args)
}

/// The element index of `(row, col)`: `0 ≤ row < rows`, `0 ≤ col < cols`, else
/// `CollectionBounds` (no negative indexing for matrices, `pin:04-mtx-index`).
fn at(m: &Mx, r: i64, c: i64) -> Result<usize, FaultKind> {
    if r < 0 || c < 0 || r >= i64::from(m.rows) || c >= i64::from(m.cols) {
        return Err(FaultKind::CollectionBounds);
    }
    Ok(r as usize * m.cols as usize + c as usize)
}

/// `matrix.get(id, row, column)` — `[id, row, column, out]`.
pub fn bi_matrix_get(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let i = at(&m, index(f, args[1])?, index(f, args[2])?)?;
    let w = f.coll(m.r)?.1[i];
    put_word(f, args[3], m.elem, w)
}

/// `matrix.set(id, row, column, value)` — `[id, row, column, value]`.
pub fn bi_matrix_set(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let i = at(&m, index(f, args[1])?, index(f, args[2])?)?;
    let w = word_of(f, args[3], m.elem)?;
    f.coll_mut(m.r)?.1[i] = w;
    Ok(())
}

/// A `[from, to)` range of `0..=dim` (na `to` = `dim`), else `CollectionBounds`.
fn range(from: i64, to: i64, dim: u32) -> Result<(usize, usize), FaultKind> {
    if from < 0 || from > to || to > i64::from(dim) {
        return Err(FaultKind::CollectionBounds);
    }
    Ok((from as usize, to as usize))
}

/// `matrix.fill(id, value, from_row, to_row, from_column, to_column)` — `[id, value, fr, tr, fc,
/// tc]`; na bounds: `0` and the dimension.
pub fn bi_matrix_fill(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let w = word_of(f, args[1], m.elem)?;
    let (r0, r1) = range(
        bound_or(f, args[2], 0),
        bound_or(f, args[3], m.rows),
        m.rows,
    )?;
    let (c0, c1) = range(
        bound_or(f, args[4], 0),
        bound_or(f, args[5], m.cols),
        m.cols,
    )?;
    let cols = m.cols as usize;
    let (_, words) = f.coll_mut(m.r)?;
    let mut i = r0;
    while i < r1 {
        let mut j = c0;
        while j < c1 {
            words[i * cols + j] = w;
            j += 1;
        }
        i += 1;
    }
    Ok(())
}

/// `matrix.copy(id)` — `[__elem, __key, __cap, id, out]`.
pub fn bi_matrix_copy(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[3])?;
    let mut s = [src_mx(f, &m, site)?];
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        m.elem,
        m.rows,
        m.cols,
        m.n(),
        0,
        &mut s,
    )?;
    let mut i = 0usize;
    while i < m.n() {
        d.set(i, s[0].at(i));
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[4], Some(d.r.0 as i64));
    Ok(())
}

/// `matrix.submatrix(id, from_row, to_row, from_column, to_column)` — `[__elem, __key, __cap, id,
/// fr, tr, fc, tc, out]`; na bounds as `fill`.
pub fn bi_matrix_submatrix(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 9 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[3])?;
    let (r0, r1) = range(
        bound_or(f, args[4], 0),
        bound_or(f, args[5], m.rows),
        m.rows,
    )?;
    let (c0, c1) = range(
        bound_or(f, args[6], 0),
        bound_or(f, args[7], m.cols),
        m.cols,
    )?;
    let (rr, cc) = (r1 - r0, c1 - c0);
    let mut s = [src_mx(f, &m, site)?];
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        m.elem,
        rr as u32,
        cc as u32,
        rr * cc,
        0,
        &mut s,
    )?;
    let cols = m.cols as usize;
    let mut i = 0usize;
    while i < rr {
        let mut j = 0usize;
        while j < cc {
            d.set(i * cc + j, s[0].at((r0 + i) * cols + c0 + j));
            j += 1;
        }
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[8], Some(d.r.0 as i64));
    Ok(())
}

/// `matrix.transpose(id)` — `[__elem, __key, __cap, id, out]`.
pub fn bi_matrix_transpose(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[3])?;
    let mut s = [src_mx(f, &m, site)?];
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        m.elem,
        m.cols,
        m.rows,
        m.n(),
        0,
        &mut s,
    )?;
    let (rows, cols) = (m.rows as usize, m.cols as usize);
    let mut i = 0usize;
    while i < rows {
        let mut j = 0usize;
        while j < cols {
            d.set(j * rows + i, s[0].at(i * cols + j));
            j += 1;
        }
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[4], Some(d.r.0 as i64));
    Ok(())
}

/// `matrix.row(id, row)` / `matrix.col(id, column)` — `[__elem, __key, __cap, id, k, out]`: a new
/// array of the row's / column's elements.
fn line<const COL: bool>(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[3])?;
    let k = index(f, args[4])?;
    let (lim, len) = if COL {
        (m.cols, m.rows as usize)
    } else {
        (m.rows, m.cols as usize)
    };
    if k < 0 || k >= i64::from(lim) {
        return Err(FaultKind::CollectionBounds);
    }
    let k = k as usize;
    let cols = m.cols as usize;
    let mut s = [src_mx(f, &m, site)?];
    let d = Dst::init(f, site, CollKind::Array, m.elem, 0, 0, len, 0, &mut s)?;
    let mut i = 0usize;
    while i < len {
        let src = if COL { i * cols + k } else { k * cols + i };
        d.set(i, s[0].at(src));
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[5], Some(d.r.0 as i64));
    Ok(())
}

pub fn bi_matrix_row(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    line::<false>(f, site, args)
}
pub fn bi_matrix_col(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    line::<true>(f, site, args)
}

/// `matrix.reshape(id, rows, columns)` — `[id, rows, columns]`: the same elements, row-major, in a
/// new shape; `rows × columns` must equal the element count (`pin:04-mtx-reshape-count`), else
/// `ScriptRefused`.
pub fn bi_matrix_reshape(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let (r, c) = match (f.i64_arg(args[1]), f.i64_arg(args[2])) {
        (Some(r), Some(c)) if r >= 0 && c >= 0 && r <= u32::MAX as i64 && c <= u32::MAX as i64 => {
            (r as u32, c as u32)
        }
        _ => return Err(FaultKind::ScriptRefused),
    };
    if u64::from(r) * u64::from(c) != m.n() as u64 {
        return Err(FaultKind::ScriptRefused);
    }
    let (h, _) = f.coll_mut(m.r)?;
    h.idx_off = r;
    h.idx_mask = c;
    Ok(())
}

/// The element words of the `array_id` argument for a new row / column of `want` elements (`None`
/// = any length, the matrix being empty); na array → `None` (a line of na).
fn line_words(
    f: &Frame<'_>,
    e: u16,
    elem: Ty,
    want: Option<usize>,
) -> Result<Option<(Src, usize)>, FaultKind> {
    if f.coll_arg(e).is_none() {
        return Ok(None);
    }
    let (s, ae) = src_arr(f, e, SiteId(u16::MAX))?;
    if ae != elem {
        return Err(FaultKind::Internal);
    }
    if want.is_some_and(|w| w != s.len) {
        return Err(FaultKind::ScriptRefused);
    }
    Ok(Some((s, s.len)))
}

/// The na word of `elem` (a v6 bool: false).
fn na_word(f: &Frame<'_>, elem: Ty) -> u64 {
    match elem {
        Ty::Float => f64::NAN.to_bits(),
        Ty::Bool if bool_never_na(f.dialect()) => 0,
        _ => NA,
    }
}

/// `matrix.add_row(id, row, array_id)` — `[id, row, array]`: a row inserted at `row` (na = after
/// the last), from the array or na. An empty matrix takes any array length as its column count
/// (the reference); otherwise the length must equal the column count (`ScriptRefused`). Past the
/// block's capacity: `PoolFull`.
pub fn bi_matrix_add_row(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let empty = m.n() == 0;
    let src = line_words(f, args[2], m.elem, (!empty).then_some(m.cols as usize))?;
    let (rows0, cols) = match (empty, src) {
        (true, Some((_, k))) => (0u32, k),
        (true, None) => (if m.cols == 0 { m.rows } else { 0 }, m.cols as usize),
        (false, _) => (m.rows, m.cols as usize),
    };
    let r = match f.i64_arg(args[1]) {
        None => rows0 as usize,
        Some(r) if r >= 0 && r <= i64::from(rows0) => r as usize,
        Some(_) => return Err(FaultKind::CollectionBounds),
    };
    let naw = na_word(f, m.elem);
    let n0 = rows0 as usize * cols;
    let (h, words) = f.coll_mut(m.r)?;
    let n1 = n0 + cols;
    if n1 > h.cap as usize || n1 > words.len() || cols > u32::MAX as usize {
        return Err(FaultKind::PoolFull);
    }
    // COPY: 8 B × (rows − r) · cols inside the block — rows `r..` move down one row to open the
    // inserted row (row-major storage) — a row-indirection table rejected (every element read
    // would pay it, and the block layout is the array's).
    words.copy_within(r * cols..n0, (r + 1) * cols);
    let mut j = 0usize;
    while j < cols {
        words[r * cols + j] = match src {
            Some((s, _)) => s.at(j),
            None => naw,
        };
        j += 1;
    }
    h.len = n1 as u32;
    h.idx_off = rows0 + 1;
    h.idx_mask = cols as u32;
    Ok(())
}

/// `matrix.add_col(id, column, array_id)` — `[id, column, array]`: as `add_row`, a column.
pub fn bi_matrix_add_col(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let empty = m.n() == 0;
    let src = line_words(f, args[2], m.elem, (!empty).then_some(m.rows as usize))?;
    let (rows, cols0) = match (empty, src) {
        (true, Some((_, k))) => (k, 0u32),
        (true, None) => (m.rows as usize, if m.rows == 0 { m.cols } else { 0 }),
        (false, _) => (m.rows as usize, m.cols),
    };
    let c = match f.i64_arg(args[1]) {
        None => cols0 as usize,
        Some(c) if c >= 0 && c <= i64::from(cols0) => c as usize,
        Some(_) => return Err(FaultKind::CollectionBounds),
    };
    let naw = na_word(f, m.elem);
    let c0 = cols0 as usize;
    let c1 = c0 + 1;
    let (h, words) = f.coll_mut(m.r)?;
    let n1 = rows * c1;
    if n1 > h.cap as usize || n1 > words.len() || rows > u32::MAX as usize {
        return Err(FaultKind::PoolFull);
    }
    // From the last element down: every destination is at or past its source.
    let mut i = rows;
    while i > 0 {
        i -= 1;
        let mut j = c0;
        while j > 0 {
            j -= 1;
            let dst = i * c1 + j + usize::from(j >= c);
            words[dst] = words[i * c0 + j];
        }
        words[i * c1 + c] = match src {
            Some((s, _)) => s.at(i),
            None => naw,
        };
    }
    h.len = n1 as u32;
    h.idx_off = rows as u32;
    h.idx_mask = c1 as u32;
    Ok(())
}

/// `matrix.remove_row(id, row)` / `remove_col` — `[__elem, __key, __cap, id, k, out]`: the line
/// removed (na `k` = the last, `pin:04-mtx-remove-default`) returned as a new array.
fn remove<const COL: bool>(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[3])?;
    let (lim, len) = if COL {
        (m.cols, m.rows as usize)
    } else {
        (m.rows, m.cols as usize)
    };
    if lim == 0 {
        return Err(FaultKind::CollectionBounds);
    }
    let k = match f.i64_arg(args[4]) {
        None => (lim - 1) as usize,
        Some(k) if k >= 0 && k < i64::from(lim) => k as usize,
        Some(_) => return Err(FaultKind::CollectionBounds),
    };
    let (rows, cols) = (m.rows as usize, m.cols as usize);
    let mut s = [src_mx(f, &m, site)?];
    if s[0].same {
        return Err(FaultKind::Internal);
    }
    let d = Dst::init(f, site, CollKind::Array, m.elem, 0, 0, len, 0, &mut s)?;
    let mut i = 0usize;
    while i < len {
        let src = if COL { i * cols + k } else { k * cols + i };
        d.set(i, s[0].at(src));
        i += 1;
    }
    let out = d.r;
    let (h, words) = f.coll_mut(m.r)?;
    if COL {
        let c1 = cols - 1;
        let mut i = 0usize;
        while i < rows {
            let mut j = 0usize;
            while j < cols {
                if j != k {
                    words[i * c1 + j - usize::from(j > k)] = words[i * cols + j];
                }
                j += 1;
            }
            i += 1;
        }
        h.idx_mask = c1 as u32;
        h.len = (rows * c1) as u32;
    } else {
        // COPY: 8 B × (rows − k − 1) · cols inside the block — the rows after `k` close the gap
        // (row-major storage) — a row-indirection table rejected (as `add_row`).
        words.copy_within((k + 1) * cols..rows * cols, k * cols);
        h.idx_off = (rows - 1) as u32;
        h.len = ((rows - 1) * cols) as u32;
    }
    f.set_i64_arg(args[5], Some(out.0 as i64));
    Ok(())
}

pub fn bi_matrix_remove_row(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    remove::<false>(f, site, args)
}
pub fn bi_matrix_remove_col(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    remove::<true>(f, site, args)
}

/// `matrix.swap_rows(id, a, b)` / `swap_columns` — `[id, a, b]`.
fn swap<const COL: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let lim = if COL { m.cols } else { m.rows };
    let (a, b) = (index(f, args[1])?, index(f, args[2])?);
    if a < 0 || b < 0 || a >= i64::from(lim) || b >= i64::from(lim) {
        return Err(FaultKind::CollectionBounds);
    }
    let (a, b) = (a as usize, b as usize);
    let (rows, cols) = (m.rows as usize, m.cols as usize);
    let (_, w) = f.coll_mut(m.r)?;
    if COL {
        let mut i = 0usize;
        while i < rows {
            w.swap(i * cols + a, i * cols + b);
            i += 1;
        }
    } else {
        let mut j = 0usize;
        while j < cols {
            w.swap(a * cols + j, b * cols + j);
            j += 1;
        }
    }
    Ok(())
}

pub fn bi_matrix_swap_rows(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    swap::<false>(f, args)
}
pub fn bi_matrix_swap_columns(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    swap::<true>(f, args)
}

/// `matrix.reverse(id)` — `[id]`: rows and columns reversed (the row-major storage reversed).
pub fn bi_matrix_reverse(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let n = m.n();
    f.coll_mut(m.r)?.1[..n].reverse();
    Ok(())
}

/// `matrix.concat(id1, id2)` — `[id1, id2, out]`: `id2`'s rows appended to `id1`, which is returned.
/// The column counts must agree unless `id1` is empty (`ScriptRefused`); past `id1`'s capacity
/// `PoolFull`.
pub fn bi_matrix_concat(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[0])?;
    let b = mx(f, args[1])?;
    if a.elem != b.elem {
        return Err(FaultKind::Internal);
    }
    let (n0, n2) = (a.n(), b.n());
    let cols = if n0 == 0 { b.cols } else { a.cols };
    if n0 != 0 && n2 != 0 && a.cols != b.cols {
        return Err(FaultKind::ScriptRefused);
    }
    let rows = if n0 == 0 {
        b.rows
    } else {
        a.rows + if n2 == 0 { 0 } else { b.rows }
    };
    if a.r == b.r {
        let (h, w) = f.coll_mut(a.r)?;
        if 2 * n0 > h.cap as usize || 2 * n0 > w.len() {
            return Err(FaultKind::PoolFull);
        }
        // COPY: 8 B × n inside the block — `concat(m, m)` appends the matrix to itself; the
        // elements are the result — sharing the storage rejected (one block, one owner).
        w.copy_within(0..n0, n0);
        h.len = (2 * n0) as u32;
        h.idx_off = 2 * a.rows;
    } else {
        let bp: *const u64 = f.coll(b.r)?.1.as_ptr();
        let (h, w) = f.coll_mut(a.r)?;
        if n0 + n2 > h.cap as usize || n0 + n2 > w.len() {
            return Err(FaultKind::PoolFull);
        }
        let mut i = 0usize;
        while i < n2 {
            // SAFETY: `bp` is `id2`'s storage, another block (F6), unmoved (F8); `i < n2`.
            w[n0 + i] = unsafe { *bp.add(i) };
            i += 1;
        }
        h.len = (n0 + n2) as u32;
        h.idx_off = rows;
        h.idx_mask = cols;
    }
    f.set_i64_arg(args[2], Some(a.r.0 as i64));
    Ok(())
}

/// `matrix.sort(id, column, order)` — `[id, column, order]`: the rows reordered by the column's
/// values, stably (`pin:04-mtx-sort-stable`: insertion by row swaps, no scratch), na last
/// ascending / first descending (as `array.sort`).
pub fn bi_matrix_sort(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let float = num(m.elem)?;
    let c = index(f, args[1])?;
    if c < 0 || (m.cols > 0 && c >= i64::from(m.cols)) {
        return Err(FaultKind::CollectionBounds);
    }
    let desc = f.i64_arg(args[2]) == Some(1);
    let cmp: fn(&u64, &u64) -> Ordering = match (float, desc) {
        (true, false) => stats::f_asc,
        (true, true) => stats::f_desc,
        (false, false) => stats::i_asc,
        (false, true) => stats::i_desc,
    };
    if m.cols == 0 {
        return Ok(());
    }
    let (rows, cols, c) = (m.rows as usize, m.cols as usize, c as usize);
    let (_, w) = f.coll_mut(m.r)?;
    let mut i = 1usize;
    while i < rows {
        let mut j = i;
        while j > 0 && cmp(&w[(j - 1) * cols + c], &w[j * cols + c]) == Ordering::Greater {
            let mut k = 0usize;
            while k < cols {
                w.swap((j - 1) * cols + k, j * cols + k);
                k += 1;
            }
            j -= 1;
        }
        i += 1;
    }
    Ok(())
}

// ---- reductions -----------------------------------------------------------------------------------

/// `matrix.min / max(id)` — `[id, out]`: na skipped; none → na.
fn min_max<const MAX: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let float = num(m.elem)?;
    let (_, w) = f.coll(m.r)?;
    let mut best = f64::NAN;
    let mut i = 0usize;
    while i < m.n() {
        let v = val(w[i], float);
        if !v.is_nan() && (best.is_nan() || if MAX { v > best } else { v < best }) {
            best = v;
        }
        i += 1;
    }
    put(f, args[1], best);
    Ok(())
}

pub fn bi_matrix_min(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    min_max::<false>(f, args)
}
pub fn bi_matrix_max(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    min_max::<true>(f, args)
}

/// `matrix.avg(id)` — `[id, out]`: the mean of the non-na elements (as `array.avg`); none → na.
pub fn bi_matrix_avg(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let float = num(m.elem)?;
    let (_, w) = f.coll(m.r)?;
    let (mut s, mut n) = (0.0f64, 0u64);
    let mut i = 0usize;
    while i < m.n() {
        let v = val(w[i], float);
        if !v.is_nan() {
            s += v;
            n += 1;
        }
        i += 1;
    }
    put(f, args[1], if n == 0 { f64::NAN } else { s / n as f64 });
    Ok(())
}

/// `matrix.median / mode(id)` — `[id, out]`: `array.median` / `array.mode` over the elements
/// (na skipped; the selection without scratch of `order.rs`).
fn order_stat<const MODE: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let float = num(m.elem)?;
    let (_, w) = f.coll(m.r)?;
    let w = &w[..m.n()];
    let v = if MODE {
        order::mode_words(w, float)
    } else {
        order::median_words(w, float, argwin::is_i64(args[1]))
    };
    put(f, args[1], v);
    Ok(())
}

pub fn bi_matrix_median(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    order_stat::<false>(f, args)
}
pub fn bi_matrix_mode(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    order_stat::<true>(f, args)
}

/// `matrix.trace(id)` — `[id, out]`: the sum of `a[i][i]` for `i < min(rows, columns)`
/// (`pin:04-mtx-trace-rect`); na propagates.
pub fn bi_matrix_trace(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let float = num(m.elem)?;
    let (_, w) = f.coll(m.r)?;
    let k = m.rows.min(m.cols) as usize;
    let cols = m.cols as usize;
    let mut s = 0.0f64;
    let mut i = 0usize;
    while i < k {
        s += val(w[i * cols + i], float);
        i += 1;
    }
    put(f, args[1], s);
    Ok(())
}

// ---- predicates -----------------------------------------------------------------------------------

/// `matrix.is_*(id)` — `[id, out]`. Exact comparisons (na is never 0 or 1); every predicate but
/// `is_zero` and `is_binary` is false for a non-square matrix (the reference).
fn pred<const P: u8>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let m = mx(f, args[0])?;
    let float = num(m.elem)?;
    let (_, w) = f.coll(m.r)?;
    let (rows, cols) = (m.rows as usize, m.cols as usize);
    let a = |i: usize, j: usize| val(w[i * cols + j], float);
    let sq = rows == cols;
    let n = rows;
    let mut ok = true;
    match P {
        // is_zero
        0 => {
            let mut i = 0usize;
            while ok && i < m.n() {
                ok = val(w[i], float) == 0.0;
                i += 1;
            }
        }
        // is_binary
        1 => {
            let mut i = 0usize;
            while ok && i < m.n() {
                let v = val(w[i], float);
                ok = v == 0.0 || v == 1.0;
                i += 1;
            }
        }
        // is_identity, is_diagonal, is_antidiagonal, is_symmetric, is_antisymmetric
        2..=6 => {
            ok = sq;
            let mut i = 0usize;
            while ok && i < n {
                let mut j = 0usize;
                while ok && j < n {
                    let v = a(i, j);
                    ok = match P {
                        2 => v == if i == j { 1.0 } else { 0.0 },
                        3 => i == j || v == 0.0,
                        4 => i + j == n - 1 || v == 0.0,
                        5 => v == a(j, i),
                        _ => v == -a(j, i),
                    };
                    j += 1;
                }
                i += 1;
            }
        }
        // is_triangular: all zero above, or all zero below, the main diagonal
        7 => {
            let (mut up, mut lo) = (sq, sq);
            let mut i = 0usize;
            while (up || lo) && i < n {
                let mut j = 0usize;
                while j < n {
                    let z = a(i, j) == 0.0;
                    if j > i && !z {
                        up = false;
                    }
                    if j < i && !z {
                        lo = false;
                    }
                    j += 1;
                }
                i += 1;
            }
            ok = up || lo;
        }
        // is_stochastic: square, every element ≥ 0, every row sums to 1 (`pin:04-mtx-stochastic-tol`)
        _ => {
            ok = sq && n > 0;
            let mut i = 0usize;
            while ok && i < n {
                let mut s = 0.0f64;
                let mut j = 0usize;
                while ok && j < n {
                    let v = a(i, j);
                    ok = v >= 0.0;
                    s += v;
                    j += 1;
                }
                ok = ok && (s - 1.0).abs() <= STOCH_TOL;
                i += 1;
            }
        }
    }
    put_bool(f, args[1], ok);
    Ok(())
}

macro_rules! pred_fn {
    ($name:ident, $p:expr) => {
        pub fn $name(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
            pred::<$p>(f, args)
        }
    };
}
pred_fn!(bi_matrix_is_zero, 0);
pred_fn!(bi_matrix_is_binary, 1);
pred_fn!(bi_matrix_is_identity, 2);
pred_fn!(bi_matrix_is_diagonal, 3);
pred_fn!(bi_matrix_is_antidiagonal, 4);
pred_fn!(bi_matrix_is_symmetric, 5);
pred_fn!(bi_matrix_is_antisymmetric, 6);
pred_fn!(bi_matrix_is_triangular, 7);
pred_fn!(bi_matrix_is_stochastic, 8);

// ---- element-wise and products ------------------------------------------------------------------

/// `matrix.sum / diff(id1, id2)` — `[__elem, __key, __cap, id1, id2, out]`: element-wise with a
/// matrix of the same shape (`ScriptRefused` otherwise) or a scalar.
fn add_sub<const SUB: bool>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[3])?;
    let float = num(a.elem)?;
    let b = mx(f, args[4])?;
    if b.rows != a.rows || b.cols != a.cols || b.elem != a.elem {
        return Err(FaultKind::ScriptRefused);
    }
    let mut s = [src_mx(f, &a, site)?, src_mx(f, &b, site)?];
    let n = a.n();
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        a.elem,
        a.rows,
        a.cols,
        n,
        0,
        &mut s,
    )?;
    let mut i = 0usize;
    while i < n {
        let (x, y) = (val(s[0].at(i), float), val(s[1].at(i), float));
        d.set(i, enc(if SUB { x - y } else { x + y }, float));
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[5], Some(d.r.0 as i64));
    Ok(())
}

/// The scalar forms: `matrix.sum / diff / mult(id1, x)` with `x` an int or float.
fn scalar_op<const OP: u8>(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[3])?;
    let float = num(a.elem)?;
    let x = if argwin::is_i64(args[4]) {
        f.i64_arg(args[4]).map_or(f64::NAN, |v| v as f64)
    } else {
        f.f64_arg(args[4])
    };
    let mut s = [src_mx(f, &a, site)?];
    let n = a.n();
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        a.elem,
        a.rows,
        a.cols,
        n,
        0,
        &mut s,
    )?;
    let mut i = 0usize;
    while i < n {
        let v = val(s[0].at(i), float);
        let r = match OP {
            0 => v + x,
            1 => v - x,
            _ => v * x,
        };
        d.set(i, enc(r, float));
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[5], Some(d.r.0 as i64));
    Ok(())
}

pub fn bi_matrix_sum_m(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    add_sub::<false>(f, site, args)
}
pub fn bi_matrix_diff_m(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    add_sub::<true>(f, site, args)
}
pub fn bi_matrix_sum_s(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    scalar_op::<0>(f, site, args)
}
pub fn bi_matrix_diff_s(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    scalar_op::<1>(f, site, args)
}
pub fn bi_matrix_mult_s(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    scalar_op::<2>(f, site, args)
}

/// `matrix.mult(id1, id2)` of two matrices — `[__elem, __key, __cap, id1, id2, out]`: the
/// `rows1 × cols2` product, `Σ_t a[i][t]·b[t][j]` in `t` order; `cols1 ≠ rows2` → `ScriptRefused`.
pub fn bi_matrix_mult_m(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[3])?;
    let b = mx(f, args[4])?;
    let float = num(a.elem)?;
    if a.cols != b.rows || a.elem != b.elem {
        return Err(FaultKind::ScriptRefused);
    }
    let (m, k, p) = (a.rows as usize, a.cols as usize, b.cols as usize);
    let mut s = [src_mx(f, &a, site)?, src_mx(f, &b, site)?];
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        a.elem,
        a.rows,
        b.cols,
        m * p,
        0,
        &mut s,
    )?;
    let mut i = 0usize;
    while i < m {
        let mut j = 0usize;
        while j < p {
            let mut acc = 0.0f64;
            let mut t = 0usize;
            while t < k {
                acc += val(s[0].at(i * k + t), float) * val(s[1].at(t * p + j), float);
                t += 1;
            }
            d.set(i * p + j, enc(acc, float));
            j += 1;
        }
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[5], Some(d.r.0 as i64));
    Ok(())
}

/// `matrix.mult(id1, array)` — `[__elem, __key, __cap, id1, array, out]`: the `rows1`-element
/// product vector; `cols1 ≠ size` → `ScriptRefused`.
pub fn bi_matrix_mult_a(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[3])?;
    let float = num(a.elem)?;
    let (v, ve) = src_arr(f, args[4], site)?;
    if ve != a.elem {
        return Err(FaultKind::Internal);
    }
    if v.len != a.cols as usize {
        return Err(FaultKind::ScriptRefused);
    }
    let (m, k) = (a.rows as usize, a.cols as usize);
    let mut s = [src_mx(f, &a, site)?, v];
    let d = Dst::init(f, site, CollKind::Array, a.elem, 0, 0, m, 0, &mut s)?;
    let mut i = 0usize;
    while i < m {
        let mut acc = 0.0f64;
        let mut t = 0usize;
        while t < k {
            acc += val(s[0].at(i * k + t), float) * val(s[1].at(t), float);
            t += 1;
        }
        d.set(i, enc(acc, float));
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[5], Some(d.r.0 as i64));
    Ok(())
}

/// `matrix.kron(id1, id2)` — `[__elem, __key, __cap, id1, id2, out]`: the Kronecker product.
pub fn bi_matrix_kron(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[3])?;
    let b = mx(f, args[4])?;
    let float = num(a.elem)?;
    if a.elem != b.elem {
        return Err(FaultKind::Internal);
    }
    let (r1, c1, r2, c2) = (a.rows as u64, a.cols as u64, b.rows as u64, b.cols as u64);
    let (rr, cc) = (r1 * r2, c1 * c2);
    if rr > u32::MAX as u64 || cc > u32::MAX as u64 || rr * cc > u32::MAX as u64 {
        return Err(FaultKind::PoolFull);
    }
    let (r1, c1, r2, c2) = (r1 as usize, c1 as usize, r2 as usize, c2 as usize);
    let cc = cc as usize;
    let mut s = [src_mx(f, &a, site)?, src_mx(f, &b, site)?];
    let n = r1 * r2 * cc;
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        a.elem,
        rr as u32,
        cc as u32,
        n,
        0,
        &mut s,
    )?;
    let mut i1 = 0usize;
    while i1 < r1 {
        let mut j1 = 0usize;
        while j1 < c1 {
            let x = val(s[0].at(i1 * c1 + j1), float);
            let mut i2 = 0usize;
            while i2 < r2 {
                let mut j2 = 0usize;
                while j2 < c2 {
                    let y = val(s[1].at(i2 * c2 + j2), float);
                    d.set((i1 * r2 + i2) * cc + j1 * c2 + j2, enc(x * y, float));
                    j2 += 1;
                }
                i2 += 1;
            }
            j1 += 1;
        }
        i1 += 1;
    }
    d.finish();
    f.set_i64_arg(args[5], Some(d.r.0 as i64));
    Ok(())
}

/// `matrix.pow(id, power)` — `[__elem, __key, __cap, id, power, out]`: square only
/// (`ScriptRefused`); `power` na or negative → `ScriptRefused`; 0 → the identity; else the
/// product `id · id · … · id` left to right (`pin:04-mtx-pow-naive`). Scratch `2n²`.
pub fn bi_matrix_pow(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[3])?;
    let float = num(a.elem)?;
    let n = square(&a)?;
    let p = match f.i64_arg(args[4]) {
        Some(p) if p >= 0 => p as u64,
        _ => return Err(FaultKind::ScriptRefused),
    };
    let nn = n * n;
    let mut s = [src_mx(f, &a, site)?];
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        a.elem,
        a.rows,
        a.cols,
        nn,
        2 * nn,
        &mut s,
    )?;
    // cur = scratch[0..nn), next = scratch[nn..2nn)
    let mut i = 0usize;
    while i < nn {
        let v = if p == 0 {
            f64::from(u8::from(i / n == i % n))
        } else {
            val(s[0].at(i), float)
        };
        d.ss(i, v);
        i += 1;
    }
    let mut t = 1u64;
    while t < p {
        let mut r = 0usize;
        while r < n {
            let mut c = 0usize;
            while c < n {
                let mut acc = 0.0f64;
                let mut k = 0usize;
                while k < n {
                    acc += d.s(r * n + k) * val(s[0].at(k * n + c), float);
                    k += 1;
                }
                d.ss(nn + r * n + c, acc);
                c += 1;
            }
            r += 1;
        }
        let mut k = 0usize;
        while k < nn {
            d.ss(k, d.s(nn + k));
            k += 1;
        }
        t += 1;
    }
    let mut k = 0usize;
    while k < nn {
        d.set(k, enc(d.s(k), float));
        k += 1;
    }
    d.finish();
    f.set_i64_arg(args[5], Some(d.r.0 as i64));
    Ok(())
}

// ---- LU: det, inv -------------------------------------------------------------------------------

/// LU with partial pivoting in place over the `n × n` scratch at `a0` (row-major), the pivot rows
/// recorded at `piv` (as f64 indices). Returns the permutation's sign, 0.0 when a pivot is
/// exactly zero (singular) or NaN (`pin:04-mtx-singular`).
fn lu(d: &Dst, a0: usize, piv: usize, n: usize) -> f64 {
    let mut sign = 1.0f64;
    let mut i = 0usize;
    while i < n {
        d.ss(piv + i, i as f64);
        i += 1;
    }
    let mut k = 0usize;
    while k < n {
        // The largest |a[i][k]|, i ≥ k.
        let mut p = k;
        let mut best = d.s(a0 + k * n + k).abs();
        let mut i = k + 1;
        while i < n {
            let v = d.s(a0 + i * n + k).abs();
            if v > best {
                best = v;
                p = i;
            }
            i += 1;
        }
        if best.is_nan() || best == 0.0 {
            return 0.0;
        }
        if p != k {
            let mut j = 0usize;
            while j < n {
                let t = d.s(a0 + k * n + j);
                d.ss(a0 + k * n + j, d.s(a0 + p * n + j));
                d.ss(a0 + p * n + j, t);
                j += 1;
            }
            let t = d.s(piv + k);
            d.ss(piv + k, d.s(piv + p));
            d.ss(piv + p, t);
            sign = -sign;
        }
        let pk = d.s(a0 + k * n + k);
        let mut i = k + 1;
        while i < n {
            let l = d.s(a0 + i * n + k) / pk;
            d.ss(a0 + i * n + k, l);
            let mut j = k + 1;
            while j < n {
                d.ss(
                    a0 + i * n + j,
                    d.s(a0 + i * n + j) - l * d.s(a0 + k * n + j),
                );
                j += 1;
            }
            i += 1;
        }
        k += 1;
    }
    sign
}

/// Load matrix source `s` (`n` elements) into scratch at `a0` as f64.
fn load(d: &Dst, s: &Src, a0: usize, n: usize, float: bool) {
    let mut i = 0usize;
    while i < n {
        d.ss(a0 + i, val(s.at(i), float));
        i += 1;
    }
}

/// `matrix.det(id)` — `[id, out]` (its own block is the scratch): square only (`ScriptRefused`).
pub fn bi_matrix_det(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[0])?;
    let float = num(a.elem)?;
    let n = square(&a)?;
    let mut s = [src_mx(f, &a, site)?];
    if s[0].same {
        return Err(FaultKind::Internal);
    }
    let d = Dst::init(
        f,
        site,
        CollKind::Array,
        Ty::Float,
        0,
        0,
        0,
        n * n + n,
        &mut s,
    )?;
    load(&d, &s[0], 0, n * n, float);
    let sign = lu(&d, 0, n * n, n);
    let mut det = sign;
    let mut i = 0usize;
    while sign != 0.0 && i < n {
        det *= d.s(i * n + i);
        i += 1;
    }
    if n == 0 {
        det = 1.0;
    }
    // An int matrix's determinant rounds as its other numeric results (`pin:04-mtx-int-numeric`).
    let det = if float { det } else { det.round() };
    put(f, args[1], det);
    Ok(())
}

/// `matrix.inv(id)` — `[__elem, __key, __cap, id, out]`: square only (`ScriptRefused`); singular →
/// every element na (`pin:04-mtx-singular`). Scratch `n² + 2n`.
pub fn bi_matrix_inv(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[3])?;
    let float = num(a.elem)?;
    let n = square(&a)?;
    let nn = n * n;
    let mut s = [src_mx(f, &a, site)?];
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        a.elem,
        a.rows,
        a.cols,
        nn,
        nn + 2 * n,
        &mut s,
    )?;
    load(&d, &s[0], 0, nn, float);
    let (piv, y) = (nn, nn + n);
    if lu(&d, 0, piv, n) == 0.0 {
        let mut k = 0usize;
        while k < nn {
            d.set(k, enc(f64::NAN, float));
            k += 1;
        }
    } else {
        // Column j of the inverse: solve L·U·x = P·e_j.
        let mut j = 0usize;
        while j < n {
            // Forward: L unit lower.
            let mut i = 0usize;
            while i < n {
                let mut v = if d.s(piv + i) as usize == j { 1.0 } else { 0.0 };
                let mut k = 0usize;
                while k < i {
                    v -= d.s(i * n + k) * d.s(y + k);
                    k += 1;
                }
                d.ss(y + i, v);
                i += 1;
            }
            // Backward: U.
            let mut i = n;
            while i > 0 {
                i -= 1;
                let mut v = d.s(y + i);
                let mut k = i + 1;
                while k < n {
                    v -= d.s(i * n + k) * d.s(y + k);
                    k += 1;
                }
                d.ss(y + i, v / d.s(i * n + i));
            }
            let mut i = 0usize;
            while i < n {
                d.set(i * n + j, enc(d.s(y + i), float));
                i += 1;
            }
            j += 1;
        }
    }
    d.finish();
    f.set_i64_arg(args[4], Some(d.r.0 as i64));
    Ok(())
}

// ---- SVD: pinv, rank ----------------------------------------------------------------------------

/// One-sided Jacobi SVD of the `m × n` (m ≥ n) matrix in scratch `u0` (row-major, overwritten
/// by `U·Σ`, columns then normalised), `V` (`n × n`) at `v0`, the singular values at `s0`.
/// `false` when it did not converge or met NaN (the caller writes na).
fn svd(d: &Dst, u0: usize, v0: usize, s0: usize, m: usize, n: usize) -> bool {
    let mut i = 0usize;
    while i < n * n {
        d.ss(v0 + i, f64::from(u8::from(i / n == i % n)));
        i += 1;
    }
    // A column whose squared norm is below `(ε·‖A‖_F)²` is numerically zero: never rotated (a
    // rank-deficient matrix converges instead of rotating round-off forever).
    let mut fro2 = 0.0f64;
    let mut i = 0usize;
    while i < m * n {
        let x = d.s(u0 + i);
        fro2 += x * x;
        i += 1;
    }
    if fro2.is_nan() {
        return false;
    }
    let floor = EPS * EPS * fro2;
    let mut sweep = 0u32;
    loop {
        let mut rotated = false;
        let mut p = 0usize;
        while p + 1 < n {
            let mut q = p + 1;
            while q < n {
                let (mut al, mut be, mut ga) = (0.0f64, 0.0f64, 0.0f64);
                let mut k = 0usize;
                while k < m {
                    let (x, y) = (d.s(u0 + k * n + p), d.s(u0 + k * n + q));
                    al += x * x;
                    be += y * y;
                    ga += x * y;
                    k += 1;
                }
                if al.is_nan() || be.is_nan() || ga.is_nan() {
                    return false;
                }
                if al > floor && be > floor && ga.abs() > EPS * (al * be).sqrt() {
                    rotated = true;
                    let zeta = (be - al) / (2.0 * ga);
                    let t = zeta.signum() / (zeta.abs() + (1.0 + zeta * zeta).sqrt());
                    let c = 1.0 / (1.0 + t * t).sqrt();
                    let sn = c * t;
                    let mut k = 0usize;
                    while k < m {
                        let (x, y) = (d.s(u0 + k * n + p), d.s(u0 + k * n + q));
                        d.ss(u0 + k * n + p, c * x - sn * y);
                        d.ss(u0 + k * n + q, sn * x + c * y);
                        k += 1;
                    }
                    let mut k = 0usize;
                    while k < n {
                        let (x, y) = (d.s(v0 + k * n + p), d.s(v0 + k * n + q));
                        d.ss(v0 + k * n + p, c * x - sn * y);
                        d.ss(v0 + k * n + q, sn * x + c * y);
                        k += 1;
                    }
                }
                q += 1;
            }
            p += 1;
        }
        sweep += 1;
        if !rotated {
            break;
        }
        if sweep >= SVD_SWEEPS {
            return false;
        }
    }
    let mut j = 0usize;
    while j < n {
        let mut s = 0.0f64;
        let mut k = 0usize;
        while k < m {
            let x = d.s(u0 + k * n + j);
            s += x * x;
            k += 1;
        }
        let s = s.sqrt();
        d.ss(s0 + j, s);
        if s != 0.0 {
            let mut k = 0usize;
            while k < m {
                d.ss(u0 + k * n + j, d.s(u0 + k * n + j) / s);
                k += 1;
            }
        }
        j += 1;
    }
    true
}

/// The SVD tolerance and the rank (`pin:04-mtx-svd-tol`).
fn svd_rank(d: &Dst, s0: usize, m: usize, n: usize) -> (f64, usize) {
    let mut top = 0.0f64;
    let mut j = 0usize;
    while j < n {
        let x = d.s(s0 + j);
        if x > top {
            top = x;
        }
        j += 1;
    }
    let tol = (m.max(n) as f64) * top * EPS;
    let mut r = 0usize;
    let mut j = 0usize;
    while j < n {
        r += usize::from(d.s(s0 + j) > tol);
        j += 1;
    }
    (tol, r)
}

/// Load `a` (`rows × cols`) into scratch `u0` as the `m × n`, `m ≥ n` working matrix: itself, or
/// its transpose when it is wide. Returns `(m, n, transposed)`.
fn load_tall(
    d: &Dst,
    s: &Src,
    u0: usize,
    rows: usize,
    cols: usize,
    float: bool,
) -> (usize, usize, bool) {
    let t = rows < cols;
    let (m, n) = if t { (cols, rows) } else { (rows, cols) };
    let mut i = 0usize;
    while i < rows {
        let mut j = 0usize;
        while j < cols {
            let v = val(s.at(i * cols + j), float);
            if t {
                d.ss(u0 + j * n + i, v);
            } else {
                d.ss(u0 + i * n + j, v);
            }
            j += 1;
        }
        i += 1;
    }
    (m, n, t)
}

/// `matrix.pinv(id)` — `[__elem, __key, __cap, id, out]`: the `cols × rows` Moore–Penrose inverse
/// `V·Σ⁺·Uᵀ` (of the transpose, transposed back, for a wide matrix). Scratch `mn + n² + n`.
pub fn bi_matrix_pinv(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[3])?;
    let float = num(a.elem)?;
    let (rows, cols) = (a.rows as usize, a.cols as usize);
    let (m, n) = (rows.max(cols), rows.min(cols));
    let mut s = [src_mx(f, &a, site)?];
    let d = Dst::init(
        f,
        site,
        CollKind::Matrix,
        a.elem,
        a.cols,
        a.rows,
        rows * cols,
        m * n + n * n + n,
        &mut s,
    )?;
    let (u0, v0, s0) = (0usize, m * n, m * n + n * n);
    let (m, n, t) = load_tall(&d, &s[0], u0, rows, cols, float);
    let ok = svd(&d, u0, v0, s0, m, n);
    let (tol, _) = svd_rank(&d, s0, m, n);
    // pinv(W) for the working W (m × n) is n × m: P[i][k] = Σ_j V[i][j] / σ_j · U[k][j].
    // The result is `cols × rows`: P itself, or Pᵀ when W = Aᵀ.
    let mut i = 0usize;
    while i < n {
        let mut k = 0usize;
        while k < m {
            let mut acc = 0.0f64;
            let mut j = 0usize;
            while j < n {
                let sg = d.s(s0 + j);
                if sg > tol {
                    acc += d.s(v0 + i * n + j) / sg * d.s(u0 + k * n + j);
                }
                j += 1;
            }
            let v = if ok { acc } else { f64::NAN };
            // A tall / square A: result[i][k] (cols = n, rows = m). A wide A: result = Pᵀ,
            // result[k][i] with `rows` = n columns.
            let at = if t { k * rows + i } else { i * rows + k };
            d.set(at, enc(v, float));
            k += 1;
        }
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[4], Some(d.r.0 as i64));
    Ok(())
}

/// `matrix.rank(id)` — `[id, out]` (its own block is the scratch): the count of singular values
/// above the tolerance; na when the SVD fails.
pub fn bi_matrix_rank(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[0])?;
    let float = num(a.elem)?;
    let (rows, cols) = (a.rows as usize, a.cols as usize);
    let (m, n) = (rows.max(cols), rows.min(cols));
    let mut s = [src_mx(f, &a, site)?];
    if s[0].same {
        return Err(FaultKind::Internal);
    }
    let d = Dst::init(
        f,
        site,
        CollKind::Array,
        Ty::Float,
        0,
        0,
        0,
        m * n + n * n + n,
        &mut s,
    )?;
    let (u0, v0, s0) = (0usize, m * n, m * n + n * n);
    let (m, n, _) = load_tall(&d, &s[0], u0, rows, cols, float);
    let ok = svd(&d, u0, v0, s0, m, n);
    let (_, r) = svd_rank(&d, s0, m, n);
    f.set_i64_arg(args[1], ok.then_some(r as i64));
    Ok(())
}

// ---- symmetric eigen: tred2 + tql2 --------------------------------------------------------------

/// Householder reduction of the symmetric `n × n` matrix in scratch `v` to tridiagonal form:
/// diagonal at `dd`, sub-diagonal at `ee`, the orthogonal transform accumulated in `v`.
fn tred2(x: &Dst, v: usize, dd: usize, ee: usize, n: usize) {
    let vij = |i: usize, j: usize| v + i * n + j;
    let mut j = 0usize;
    while j < n {
        x.ss(dd + j, x.s(vij(n - 1, j)));
        j += 1;
    }
    let mut i = n - 1;
    while i > 0 {
        let mut scale = 0.0f64;
        let mut h = 0.0f64;
        let mut k = 0usize;
        while k < i {
            scale += x.s(dd + k).abs();
            k += 1;
        }
        if scale == 0.0 {
            x.ss(ee + i, x.s(dd + i - 1));
            let mut j = 0usize;
            while j < i {
                x.ss(dd + j, x.s(vij(i - 1, j)));
                x.ss(vij(i, j), 0.0);
                x.ss(vij(j, i), 0.0);
                j += 1;
            }
        } else {
            let mut k = 0usize;
            while k < i {
                x.ss(dd + k, x.s(dd + k) / scale);
                h += x.s(dd + k) * x.s(dd + k);
                k += 1;
            }
            let mut f = x.s(dd + i - 1);
            let mut g = h.sqrt();
            if f > 0.0 {
                g = -g;
            }
            x.ss(ee + i, scale * g);
            h -= f * g;
            x.ss(dd + i - 1, f - g);
            let mut j = 0usize;
            while j < i {
                x.ss(ee + j, 0.0);
                j += 1;
            }
            let mut j = 0usize;
            while j < i {
                f = x.s(dd + j);
                x.ss(vij(j, i), f);
                g = x.s(ee + j) + x.s(vij(j, j)) * f;
                let mut k = j + 1;
                while k < i {
                    g += x.s(vij(k, j)) * x.s(dd + k);
                    x.ss(ee + k, x.s(ee + k) + x.s(vij(k, j)) * f);
                    k += 1;
                }
                x.ss(ee + j, g);
                j += 1;
            }
            f = 0.0;
            let mut j = 0usize;
            while j < i {
                x.ss(ee + j, x.s(ee + j) / h);
                f += x.s(ee + j) * x.s(dd + j);
                j += 1;
            }
            let hh = f / (h + h);
            let mut j = 0usize;
            while j < i {
                x.ss(ee + j, x.s(ee + j) - hh * x.s(dd + j));
                j += 1;
            }
            let mut j = 0usize;
            while j < i {
                f = x.s(dd + j);
                g = x.s(ee + j);
                let mut k = j;
                while k < i {
                    let t = x.s(vij(k, j)) - (f * x.s(ee + k) + g * x.s(dd + k));
                    x.ss(vij(k, j), t);
                    k += 1;
                }
                x.ss(dd + j, x.s(vij(i - 1, j)));
                x.ss(vij(i, j), 0.0);
                j += 1;
            }
        }
        x.ss(dd + i, h);
        i -= 1;
    }
    // Accumulate the transformations.
    let mut i = 0usize;
    while i + 1 < n {
        x.ss(vij(n - 1, i), x.s(vij(i, i)));
        x.ss(vij(i, i), 1.0);
        let h = x.s(dd + i + 1);
        if h != 0.0 {
            let mut k = 0usize;
            while k <= i {
                x.ss(dd + k, x.s(vij(k, i + 1)) / h);
                k += 1;
            }
            let mut j = 0usize;
            while j <= i {
                let mut g = 0.0f64;
                let mut k = 0usize;
                while k <= i {
                    g += x.s(vij(k, i + 1)) * x.s(vij(k, j));
                    k += 1;
                }
                let mut k = 0usize;
                while k <= i {
                    x.ss(vij(k, j), x.s(vij(k, j)) - g * x.s(dd + k));
                    k += 1;
                }
                j += 1;
            }
        }
        let mut k = 0usize;
        while k <= i {
            x.ss(vij(k, i + 1), 0.0);
            k += 1;
        }
        i += 1;
    }
    let mut j = 0usize;
    while j < n {
        x.ss(dd + j, x.s(vij(n - 1, j)));
        x.ss(vij(n - 1, j), 0.0);
        j += 1;
    }
    x.ss(vij(n - 1, n - 1), 1.0);
    x.ss(ee, 0.0);
}

/// The implicit QL algorithm on the tridiagonal form of [`tred2`], eigenvalues sorted ascending
/// with their vectors (columns of `v`). `false` when an eigenvalue does not converge.
fn tql2(x: &Dst, v: usize, dd: usize, ee: usize, n: usize) -> bool {
    let vij = |i: usize, j: usize| v + i * n + j;
    let mut i = 1usize;
    while i < n {
        x.ss(ee + i - 1, x.s(ee + i));
        i += 1;
    }
    x.ss(ee + n - 1, 0.0);
    let mut f = 0.0f64;
    let mut tst1 = 0.0f64;
    let mut l = 0usize;
    while l < n {
        let t = x.s(dd + l).abs() + x.s(ee + l).abs();
        if t > tst1 {
            tst1 = t;
        }
        let mut m = l;
        while m < n {
            if x.s(ee + m).abs() <= EPS * tst1 {
                break;
            }
            m += 1;
        }
        if m >= n {
            return false;
        }
        if m > l {
            let mut iter = 0u32;
            loop {
                iter += 1;
                if iter > QL_ITERS {
                    return false;
                }
                let mut g = x.s(dd + l);
                let mut p = (x.s(dd + l + 1) - g) / (2.0 * x.s(ee + l));
                let mut r = p.hypot(1.0);
                if p < 0.0 {
                    r = -r;
                }
                x.ss(dd + l, x.s(ee + l) / (p + r));
                x.ss(dd + l + 1, x.s(ee + l) * (p + r));
                let dl1 = x.s(dd + l + 1);
                let mut h = g - x.s(dd + l);
                let mut i = l + 2;
                while i < n {
                    x.ss(dd + i, x.s(dd + i) - h);
                    i += 1;
                }
                f += h;
                p = x.s(dd + m);
                let (mut c, mut c2, mut c3) = (1.0f64, 1.0f64, 1.0f64);
                let el1 = x.s(ee + l + 1);
                let (mut s, mut s2) = (0.0f64, 0.0f64);
                let mut i = m;
                while i > l {
                    i -= 1;
                    c3 = c2;
                    c2 = c;
                    s2 = s;
                    g = c * x.s(ee + i);
                    h = c * p;
                    r = p.hypot(x.s(ee + i));
                    x.ss(ee + i + 1, s * r);
                    s = x.s(ee + i) / r;
                    c = p / r;
                    p = c * x.s(dd + i) - s * g;
                    x.ss(dd + i + 1, h + s * (c * g + s * x.s(dd + i)));
                    let mut k = 0usize;
                    while k < n {
                        let hk = x.s(vij(k, i + 1));
                        x.ss(vij(k, i + 1), s * x.s(vij(k, i)) + c * hk);
                        x.ss(vij(k, i), c * x.s(vij(k, i)) - s * hk);
                        k += 1;
                    }
                }
                p = -s * s2 * c3 * el1 * x.s(ee + l) / dl1;
                x.ss(ee + l, s * p);
                x.ss(dd + l, c * p);
                if x.s(ee + l).abs() <= EPS * tst1 {
                    break;
                }
                if x.s(ee + l).is_nan() {
                    return false;
                }
            }
        }
        x.ss(dd + l, x.s(dd + l) + f);
        x.ss(ee + l, 0.0);
        l += 1;
    }
    // Ascending, vectors along.
    let mut i = 0usize;
    while i + 1 < n {
        let mut k = i;
        let mut p = x.s(dd + i);
        let mut j = i + 1;
        while j < n {
            if x.s(dd + j) < p {
                k = j;
                p = x.s(dd + j);
            }
            j += 1;
        }
        if k != i {
            x.ss(dd + k, x.s(dd + i));
            x.ss(dd + i, p);
            let mut r = 0usize;
            while r < n {
                let t = x.s(vij(r, i));
                x.ss(vij(r, i), x.s(vij(r, k)));
                x.ss(vij(r, k), t);
                r += 1;
            }
        }
        i += 1;
    }
    true
}

/// `matrix.eigenvalues(id)` (`VECTORS = false`, an `n`-element array) / `matrix.eigenvectors(id)`
/// (an `n × n` matrix, column `j` the vector of eigenvalue `j`) — `[__elem, __key, __cap, id,
/// out]`: square only (`ScriptRefused`); NaN anywhere or no convergence → na
/// (`pin:04-mtx-eigen-symmetric`). Scratch `n² + 2n`.
fn eigen<const VECTORS: bool>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let a = mx(f, args[3])?;
    let float = num(a.elem)?;
    let n = square(&a)?;
    let nn = n * n;
    let mut s = [src_mx(f, &a, site)?];
    let (kind, rows, cols, out_n) = if VECTORS {
        (CollKind::Matrix, a.rows, a.cols, nn)
    } else {
        (CollKind::Array, 0, 0, n)
    };
    let d = Dst::init(f, site, kind, a.elem, rows, cols, out_n, nn + 2 * n, &mut s)?;
    let (v, dd, ee) = (0usize, nn, nn + n);
    load(&d, &s[0], v, nn, float);
    let mut finite = true;
    let mut i = 0usize;
    while i < nn {
        finite &= d.s(v + i).is_finite();
        i += 1;
    }
    let ok = n == 0
        || (finite && {
            tred2(&d, v, dd, ee, n);
            tql2(&d, v, dd, ee, n)
        });
    let mut i = 0usize;
    while i < out_n {
        let x = if !ok {
            f64::NAN
        } else if VECTORS {
            d.s(v + i)
        } else {
            d.s(dd + i)
        };
        d.set(i, enc(x, float));
        i += 1;
    }
    d.finish();
    f.set_i64_arg(args[4], Some(d.r.0 as i64));
    Ok(())
}

pub fn bi_matrix_eigenvalues(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    eigen::<false>(f, site, args)
}
pub fn bi_matrix_eigenvectors(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    eigen::<true>(f, site, args)
}
