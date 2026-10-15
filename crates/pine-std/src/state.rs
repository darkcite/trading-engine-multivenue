// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! State-block types and kernels (impl-04 §3.6, §4.3–§4.6). Every block is `#[repr(C)]`,
//! `Copy`, reference-free, a multiple of 8 bytes, and all-zero is its cold state.

use pine_vm::StateBlock;

/// Header of every sliding-window block; the tail is the site's own ring `[f64; cap]`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct WinHdr {
    /// Σ of the non-na values in the window.
    pub sum: f64,
    /// Σ of their squares (the variance family).
    pub sum2: f64,
    /// Next write position in the tail, `0..cap`.
    pub pos: u32,
    /// Values accepted so far (saturating).
    pub n: u32,
    /// The window the sums describe; 0 = cold.
    pub len: u32,
    /// na values inside the window (`propagate` rows only).
    pub na_in: u32,
    /// Accepted values until the exact recompute (§4.5).
    pub recompute: u32,
    /// Always zero.
    pub _pad: u32,
}
const _: () = assert!(core::mem::size_of::<WinHdr>() == 40 && core::mem::align_of::<WinHdr>() == 8);
// SAFETY: repr(C), Copy, no references, no padding read; all-zero is `len == 0`: never executed.
unsafe impl StateBlock for WinHdr {}

/// Accepted values between exact recomputes of a rolling sum (I27).
pub const WIN_RECOMPUTE: u32 = 1024;

#[inline(always)]
fn at(ring: &[f64], i: u32) -> f64 {
    debug_assert!((i as usize) < ring.len());
    // SAFETY: every caller passes an index produced by `back`, which is `< ring.len()`.
    unsafe { *ring.get_unchecked(i as usize) }
}

/// Ring index of the value accepted `k` values ago (`k = 0`: the newest); `k < min(n, cap)`.
#[inline(always)]
fn back(h: &WinHdr, cap: u32, k: u32) -> u32 {
    debug_assert!(k < cap && k < h.n);
    let p = if h.pos == 0 { cap - 1 } else { h.pos - 1 };
    if k <= p {
        p - k
    } else {
        cap - (k - p)
    }
}

/// Recompute `sum`, `sum2`, `na_in` exactly over the newest `len` accepted values and make `len`
/// the window (cold start or a `series` length change). `1 ≤ len ≤ ring.len()`.
pub fn win_repoint(h: &mut WinHdr, ring: &[f64], len: u32) {
    let cap = ring.len() as u32;
    debug_assert!(len >= 1 && len <= cap);
    h.len = len;
    let have = if h.n < len { h.n } else { len };
    let (mut s, mut s2, mut nan) = (0.0f64, 0.0f64, 0u32);
    let mut k = 0u32;
    while k < have {
        let v = at(ring, back(h, cap, k));
        if v.is_nan() {
            nan += 1;
        } else {
            s += v;
            s2 = v.mul_add(v, s2);
        }
        k += 1;
    }
    h.sum = s;
    h.sum2 = s2;
    h.na_in = nan;
    h.recompute = WIN_RECOMPUTE;
}

/// Accept one value into the window (a `skip` row never passes an na).
pub fn win_push(h: &mut WinHdr, ring: &mut [f64], v: f64) {
    let cap = ring.len() as u32;
    debug_assert!(h.len >= 1 && h.len <= cap);
    if h.n >= h.len {
        let old = at(ring, back(h, cap, h.len - 1));
        if old.is_nan() {
            h.na_in -= 1;
        } else {
            h.sum -= old;
            h.sum2 -= old * old;
        }
    }
    if v.is_nan() {
        h.na_in += 1;
    } else {
        h.sum += v;
        h.sum2 = v.mul_add(v, h.sum2);
    }
    debug_assert!(h.pos < cap);
    // SAFETY: `pos < cap` holds: it starts at 0 and wraps by compare below.
    unsafe { *ring.get_unchecked_mut(h.pos as usize) = v };
    h.pos = if h.pos + 1 == cap { 0 } else { h.pos + 1 };
    h.n = h.n.saturating_add(1);
    if h.recompute == 0 {
        let l = h.len;
        win_repoint(h, ring, l);
    } else {
        h.recompute -= 1;
    }
}

/// A recursive smoother (`ta.ema`, `ta.rma`, and inside the EMA/RMA-based `ta.*`).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Ewma {
    /// The smoothed value once seeded.
    pub value: f64,
    /// Σ of the first `len` accepted values (the SMA seed).
    pub acc: f64,
    /// Accepted values (saturating).
    pub n: u32,
    /// 0 = cold.
    pub seeded: u32,
}
const _: () = assert!(core::mem::size_of::<Ewma>() == 24 && core::mem::align_of::<Ewma>() == 8);
// SAFETY: repr(C) POD; all-zero is `seeded == 0`: never seeded.
unsafe impl StateBlock for Ewma {}

/// One step of `s` on a non-na `v` with weight `alpha ∈ (0, 1]`; `sma_seed` seeds with the mean of
/// the first `len` values, else with the first value. Returns the output (na until seeded).
#[inline]
pub fn ewma_step(s: &mut Ewma, v: f64, alpha: f64, len: u32, sma_seed: bool) -> f64 {
    debug_assert!(!v.is_nan());
    s.n = s.n.saturating_add(1);
    if s.seeded != 0 {
        s.value = alpha.mul_add(v - s.value, s.value);
        return s.value;
    }
    if !sma_seed {
        s.value = v;
        s.seeded = 1;
        return v;
    }
    s.acc += v;
    if s.n >= len {
        s.value = s.acc / len as f64;
        s.seeded = 1;
        s.value
    } else {
        f64::NAN
    }
}

/// The output of a cold-or-seeded smoother on an na execution (`skip`: hold the value).
#[inline(always)]
pub fn ewma_hold(s: &Ewma) -> f64 {
    if s.seeded != 0 {
        s.value
    } else {
        f64::NAN
    }
}

/// The site's previous value (`ta.tr`'s previous close, `ta.cross*`, `fixnan`, …).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Prev {
    /// The value at the site's previous execution.
    pub v: f64,
    /// 0 = never executed.
    pub primed: u32,
    /// Always zero.
    pub _pad: u32,
}
const _: () = assert!(core::mem::size_of::<Prev>() == 16 && core::mem::align_of::<Prev>() == 8);
// SAFETY: repr(C) POD; all-zero is `primed == 0`: never executed.
unsafe impl StateBlock for Prev {}

/// `ta.atr`'s block: the previous close for `tr(true)` and Wilder's smoother.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct AtrHdr {
    /// The true range's previous close.
    pub prev: Prev,
    /// `rma(tr(true), len)`.
    pub ew: Ewma,
}
const _: () = assert!(core::mem::size_of::<AtrHdr>() == 40 && core::mem::align_of::<AtrHdr>() == 8);
// SAFETY: repr(C) of two cold-at-zero blocks; all-zero is both parts cold.
unsafe impl StateBlock for AtrHdr {}

/// Header of a site's own value sequence (`ta.change`, `ta.mom`, …); the tail is a ring of
/// `L + 1` eight-byte values.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Seq {
    /// Values accepted so far (saturating).
    pub n: u32,
    /// Next write position in the tail.
    pub pos: u32,
    /// Body-specific word.
    pub aux: u32,
    /// Always zero.
    pub _pad: u32,
}
const _: () = assert!(core::mem::size_of::<Seq>() == 16 && core::mem::align_of::<Seq>() == 4);
// SAFETY: repr(C) POD; all-zero is `n == 0`: never executed.
unsafe impl StateBlock for Seq {}

/// Push `v` into the sequence ring and return the value accepted `k` executions before it, or
/// `None` while fewer than `k + 1` values were accepted. `k < ring.len()` (checked by the caller).
#[inline]
pub fn seq_push_back<T: Copy>(h: &mut Seq, ring: &mut [T], v: T, k: u32) -> Option<T> {
    let cap = ring.len() as u32;
    debug_assert!(k < cap && h.pos < cap);
    // SAFETY: `pos < cap`: it starts at 0 and wraps by compare.
    unsafe { *ring.get_unchecked_mut(h.pos as usize) = v };
    let at = if h.pos >= k {
        h.pos - k
    } else {
        cap - (k - h.pos)
    };
    h.pos = if h.pos + 1 == cap { 0 } else { h.pos + 1 };
    h.n = h.n.saturating_add(1);
    if h.n > k {
        debug_assert!(at < cap);
        // SAFETY: `at < cap` by construction from `pos < cap` and `k < cap`.
        Some(unsafe { *ring.get_unchecked(at as usize) })
    } else {
        None
    }
}

/// Header of a monotonic deque (`ta.highest`, `ta.lowest`, `…bars`); the tail is `MonoCell`s.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct MonoHdr {
    /// Values accepted so far (the next value's index).
    pub n: u32,
    /// Ring position of the deque's front.
    pub front: u32,
    /// Cells in the deque.
    pub count: u32,
    /// Always zero.
    pub _pad: u32,
}
const _: () =
    assert!(core::mem::size_of::<MonoHdr>() == 16 && core::mem::align_of::<MonoHdr>() == 4);
// SAFETY: repr(C) POD; all-zero is `n == 0`: never executed.
unsafe impl StateBlock for MonoHdr {}

/// One deque cell: a value and the index it was accepted at.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct MonoCell {
    /// The value.
    pub v: f64,
    /// Its acceptance index.
    pub ix: u32,
    /// Always zero.
    pub _pad: u32,
}
const _: () =
    assert!(core::mem::size_of::<MonoCell>() == 16 && core::mem::align_of::<MonoCell>() == 8);
// SAFETY: every bit pattern of an f64 and two u32 is a valid value.
unsafe impl pine_vm::Pod for MonoCell {}

/// Accept the non-na `v` into the deque: drop dominated cells from the back (`MAX`: `≤ v`;
/// else `≥ v`, so the newest of equal values wins), append, evict cells older than the tail.
pub fn mono_push<const MAX: bool>(h: &mut MonoHdr, q: &mut [MonoCell], v: f64) {
    let cap = q.len() as u32;
    debug_assert!(cap > 0 && h.count <= cap && h.front < cap);
    while h.count > 0 {
        let b = (h.front + h.count - 1) % cap;
        let bv = q[b as usize].v;
        if (MAX && bv <= v) || (!MAX && bv >= v) {
            h.count -= 1;
        } else {
            break;
        }
    }
    if h.count == cap {
        h.front = if h.front + 1 == cap { 0 } else { h.front + 1 };
        h.count -= 1;
    }
    let at = (h.front + h.count) % cap;
    q[at as usize] = MonoCell {
        v,
        ix: h.n,
        _pad: 0,
    };
    h.count += 1;
    h.n = h.n.wrapping_add(1);
    while h.count > 0 && h.n.wrapping_sub(q[h.front as usize].ix) > cap {
        h.front = if h.front + 1 == cap { 0 } else { h.front + 1 };
        h.count -= 1;
    }
}

/// The extreme of the newest `len` accepted values: `(value, bars back)`, by binary search for
/// the first cell whose index lies in the window. `None` while fewer than `len` were accepted.
pub fn mono_query(h: &MonoHdr, q: &[MonoCell], len: u32) -> Option<(f64, u32)> {
    let cap = q.len() as u32;
    if h.n < len || h.count == 0 {
        return None;
    }
    let (mut lo, mut hi) = (0u32, h.count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let c = q[((h.front + mid) % cap) as usize];
        if h.n - c.ix > len {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    debug_assert!(lo < h.count, "the newest cell is always inside the window");
    let c = q[((h.front + lo) % cap) as usize];
    Some((c.v, h.n - 1 - c.ix))
}

/// `ta.rsi`'s block: the previous accepted source and the up/down smoothers.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct RsiHdr {
    /// The previous accepted source value.
    pub prev: Prev,
    /// `rma(max(Δ, 0))`.
    pub up: Ewma,
    /// `rma(max(−Δ, 0))`.
    pub dn: Ewma,
}
const _: () = assert!(core::mem::size_of::<RsiHdr>() == 64 && core::mem::align_of::<RsiHdr>() == 8);
// SAFETY: repr(C) of cold-at-zero blocks; all-zero is every part cold.
unsafe impl StateBlock for RsiHdr {}

/// `ta.macd`'s block: fast, slow and signal EMAs.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct MacdHdr {
    /// `ema(src, fast)`.
    pub fast: Ewma,
    /// `ema(src, slow)`.
    pub slow: Ewma,
    /// `ema(macd, sig)`.
    pub sig: Ewma,
}
const _: () =
    assert!(core::mem::size_of::<MacdHdr>() == 72 && core::mem::align_of::<MacdHdr>() == 8);
// SAFETY: repr(C) of cold-at-zero blocks; all-zero is every part cold.
unsafe impl StateBlock for MacdHdr {}
