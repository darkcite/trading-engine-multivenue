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

/// The value accepted `k` values ago (`k = 0`: the newest); `k < min(n, ring.len())`.
#[inline(always)]
pub fn win_at(h: &WinHdr, ring: &[f64], k: u32) -> f64 {
    at(ring, back(h, ring.len() as u32, k))
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
/// else `≥ v`, so the newest of equal values wins, `pin:04-extreme-tie`), append, evict cells older than the tail.
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

/// `math.random`'s generator state (splitmix64).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Rng64 {
    /// The splitmix64 state.
    pub s: u64,
    /// 0 = not yet seeded.
    pub seeded: u32,
    /// Always zero.
    pub _pad: u32,
}
const _: () = assert!(core::mem::size_of::<Rng64>() == 16 && core::mem::align_of::<Rng64>() == 8);
// SAFETY: repr(C) POD; all-zero is `seeded == 0`: never executed.
unsafe impl StateBlock for Rng64 {}

/// `ta.kc`/`ta.kcw`'s block: the true range's previous close, the basis and band smoothers.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct KcHdr {
    /// `tr`'s previous close.
    pub prev: Prev,
    /// `ema(src, len)`.
    pub mid: Ewma,
    /// `ema` or `rma` of the span (`Sw::KcBasis`).
    pub band: Ewma,
}
const _: () = assert!(core::mem::size_of::<KcHdr>() == 64 && core::mem::align_of::<KcHdr>() == 8);
// SAFETY: repr(C) of cold-at-zero blocks; all-zero is every part cold.
unsafe impl StateBlock for KcHdr {}

/// `ta.supertrend`'s block: the ATR, the final bands, the previous close and direction.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct StHdr {
    /// `ta.atr(atrPeriod)`.
    pub atr: AtrHdr,
    /// The previous final upper band (0 = `nz(na)`, the cold value).
    pub up: f64,
    /// The previous final lower band.
    pub dn: f64,
    /// The previous close.
    pub close1: f64,
    /// The previous direction (1 or −1; 0 before the first execution).
    pub dir: i32,
    /// 0 = never executed.
    pub primed: u32,
}
const _: () = assert!(core::mem::size_of::<StHdr>() == 72 && core::mem::align_of::<StHdr>() == 8);
// SAFETY: repr(C) POD; all-zero is `primed == 0` with zeroed (= `nz`) bands: never executed.
unsafe impl StateBlock for StHdr {}

/// `ta.sar`'s block (the reference manual's `pine_sar`, `pin:04-sar-literal`): its three `var`
/// floats, the site's previous two highs and lows, its execution count and `isBelow`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct SarHdr {
    /// `var float result`.
    pub result: f64,
    /// `var float maxMin`; between the first and second executions it holds the first close
    /// (`close[1]` for the `bar_index == 1` branch — `maxMin` is `na` there and reassigned before
    /// any read).
    pub max_min: f64,
    /// `var float acceleration`.
    pub accel: f64,
    /// `high[1]` (the site's previous execution).
    pub high1: f64,
    /// `high[2]`.
    pub high2: f64,
    /// `low[1]`.
    pub low1: f64,
    /// `low[2]`.
    pub low2: f64,
    /// Executions so far (saturating): the literal `bar_index` of the next execution.
    pub n: u32,
    /// `var bool isBelow` (0 = false).
    pub is_below: u32,
}
const _: () = assert!(core::mem::size_of::<SarHdr>() == 64 && core::mem::align_of::<SarHdr>() == 8);
// SAFETY: repr(C) POD; all-zero is `n == 0` (never executed) with `isBelow == false`; every other
// field is written by the first two executions before it is read.
unsafe impl StateBlock for SarHdr {}

/// `ta.hma`'s block: one `WinHdr` per ring — `wma(src, len/2)`, `wma(src, len)`, and the outer
/// `wma(diff, ⌊√len⌋)` — over the three thirds of the tail.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct HmaHdr {
    /// The three windows: half, full, outer.
    pub w: [WinHdr; 3],
}
const _: () =
    assert!(core::mem::size_of::<HmaHdr>() == 120 && core::mem::align_of::<HmaHdr>() == 8);
// SAFETY: repr(C) array of cold-at-zero `WinHdr`s; all-zero is every `len == 0`: never executed.
unsafe impl StateBlock for HmaHdr {}

/// A window over `(src, vol)` pairs (operator ruling A5, `ta.vwma`): `win.sum = Σ src·vol`,
/// `sum_v = Σ vol`, `win.sum2` stays 0; `pos/n/len/na_in/recompute` keep `WinHdr`'s meaning.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Win2Hdr {
    /// The window bookkeeping; `sum` is `Σ src·vol`.
    pub win: WinHdr,
    /// `Σ vol`.
    pub sum_v: f64,
}
const _: () =
    assert!(core::mem::size_of::<Win2Hdr>() == 48 && core::mem::align_of::<Win2Hdr>() == 8);
// SAFETY: repr(C) POD; all-zero is `win.len == 0`: never executed.
unsafe impl StateBlock for Win2Hdr {}

/// `ta.alma`'s block (operator ruling A5): the window, and the weight-cache key `m`, `s` (the
/// manual's own, with `win.len`) and `norm = Σ weights`; the tail is `[ring | weights]`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct AlmaHdr {
    /// The window bookkeeping.
    pub win: WinHdr,
    /// `m = offset × (length − 1)` (floored when `floor`).
    pub m: f64,
    /// `s = length / sigma`.
    pub s: f64,
    /// `Σ weights`.
    pub norm: f64,
}
const _: () =
    assert!(core::mem::size_of::<AlmaHdr>() == 64 && core::mem::align_of::<AlmaHdr>() == 8);
// SAFETY: repr(C) POD; all-zero is `win.len == 0`: never executed (the weights rebuild first).
unsafe impl StateBlock for AlmaHdr {}

#[inline(always)]
fn at2(ring: &[[f64; 2]], i: u32) -> [f64; 2] {
    debug_assert!((i as usize) < ring.len());
    // SAFETY: every caller passes an index produced by `back`, which is `< ring.len()`.
    unsafe { *ring.get_unchecked(i as usize) }
}

/// `win_repoint` over pairs: recompute `Σ src·vol`, `Σ vol` exactly over the newest `len` pairs
/// and make `len` the window. `1 ≤ len ≤ ring.len()`; pairs are never na (`skip`).
pub fn win2_repoint(h: &mut Win2Hdr, ring: &[[f64; 2]], len: u32) {
    let cap = ring.len() as u32;
    debug_assert!(len >= 1 && len <= cap);
    h.win.len = len;
    let have = if h.win.n < len { h.win.n } else { len };
    let (mut s, mut sv) = (0.0f64, 0.0f64);
    let mut k = 0u32;
    while k < have {
        let p = at2(ring, back(&h.win, cap, k));
        s = p[0].mul_add(p[1], s);
        sv += p[1];
        k += 1;
    }
    h.win.sum = s;
    h.win.sum2 = 0.0;
    h.sum_v = sv;
    h.win.na_in = 0;
    h.win.recompute = WIN_RECOMPUTE;
}

/// `win_push` over pairs: accept the non-na pair `(src, vol)`.
pub fn win2_push(h: &mut Win2Hdr, ring: &mut [[f64; 2]], src: f64, vol: f64) {
    let cap = ring.len() as u32;
    debug_assert!(h.win.len >= 1 && h.win.len <= cap);
    debug_assert!(!src.is_nan() && !vol.is_nan());
    if h.win.n >= h.win.len {
        let old = at2(ring, back(&h.win, cap, h.win.len - 1));
        h.win.sum -= old[0] * old[1];
        h.sum_v -= old[1];
    }
    h.win.sum = src.mul_add(vol, h.win.sum);
    h.sum_v += vol;
    debug_assert!(h.win.pos < cap);
    // SAFETY: `pos < cap` holds: it starts at 0 and wraps by compare below.
    unsafe { *ring.get_unchecked_mut(h.win.pos as usize) = [src, vol] };
    h.win.pos = if h.win.pos + 1 == cap {
        0
    } else {
        h.win.pos + 1
    };
    h.win.n = h.win.n.saturating_add(1);
    if h.win.recompute == 0 {
        let l = h.win.len;
        win2_repoint(h, ring, l);
    } else {
        h.win.recompute -= 1;
    }
}

/// Two monotonic deques over the two halves of the tail (`ta.stoch`, `ta.wpr`): `highest(high)`
/// and `lowest(low)`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Mono2Hdr {
    /// The max deque (first half).
    pub hi: MonoHdr,
    /// The min deque (second half).
    pub lo: MonoHdr,
}
const _: () =
    assert!(core::mem::size_of::<Mono2Hdr>() == 32 && core::mem::align_of::<Mono2Hdr>() == 4);
// SAFETY: repr(C) of two cold-at-zero `MonoHdr`s; all-zero is both `n == 0`.
unsafe impl StateBlock for Mono2Hdr {}

/// `ta.mfi`'s and `ta.cmo`'s block: a window of `(a, b)` pairs over `ta.change(src)` —
/// `win.sum = Σa`, `win.sum2 = Σb` (not a sum of squares here) — and the previous accepted
/// source. Cold is `win.len == 0`; the execution that leaves it sets `prev`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct MfiHdr {
    /// The pair window.
    pub win: WinHdr,
    /// The previous accepted source (`src[1]` of `ta.change`).
    pub prev: f64,
}
const _: () = assert!(core::mem::size_of::<MfiHdr>() == 48 && core::mem::align_of::<MfiHdr>() == 8);
// SAFETY: repr(C) POD; all-zero is `win.len == 0`: never executed (`prev` is set before a read).
unsafe impl StateBlock for MfiHdr {}

/// `ta.cmo`'s block: the same layout as [`MfiHdr`] (Σ gains, Σ losses, previous source).
pub type CmoHdr = MfiHdr;

/// `win_repoint` over `(a, b)` pairs into `sum = Σa`, `sum2 = Σb`; pairs are never na.
pub fn winp_repoint(h: &mut WinHdr, ring: &[[f64; 2]], len: u32) {
    let cap = ring.len() as u32;
    debug_assert!(len >= 1 && len <= cap);
    h.len = len;
    let have = if h.n < len { h.n } else { len };
    let (mut a, mut b) = (0.0f64, 0.0f64);
    let mut k = 0u32;
    while k < have {
        let p = at2(ring, back(h, cap, k));
        a += p[0];
        b += p[1];
        k += 1;
    }
    h.sum = a;
    h.sum2 = b;
    h.na_in = 0;
    h.recompute = WIN_RECOMPUTE;
}

/// `win_push` over `(a, b)` pairs into `sum = Σa`, `sum2 = Σb`; accept the non-na pair.
pub fn winp_push(h: &mut WinHdr, ring: &mut [[f64; 2]], a: f64, b: f64) {
    let cap = ring.len() as u32;
    debug_assert!(h.len >= 1 && h.len <= cap);
    debug_assert!(!a.is_nan() && !b.is_nan());
    if h.n >= h.len {
        let old = at2(ring, back(h, cap, h.len - 1));
        h.sum -= old[0];
        h.sum2 -= old[1];
    }
    h.sum += a;
    h.sum2 += b;
    debug_assert!(h.pos < cap);
    // SAFETY: `pos < cap` holds: it starts at 0 and wraps by compare below.
    unsafe { *ring.get_unchecked_mut(h.pos as usize) = [a, b] };
    h.pos = if h.pos + 1 == cap { 0 } else { h.pos + 1 };
    h.n = h.n.saturating_add(1);
    if h.recompute == 0 {
        let l = h.len;
        winp_repoint(h, ring, l);
    } else {
        h.recompute -= 1;
    }
}

/// `ta.tsi`'s block: the previous source and the four EMAs — `ema(m, long)`, `ema(·, short)` of
/// it, and the same pair over `|m|`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct TsiHdr {
    /// `ta.change`'s previous source.
    pub prev: Prev,
    /// `ema(m, long)`.
    pub m1: Ewma,
    /// `ema(ema(m, long), short)`.
    pub m2: Ewma,
    /// `ema(|m|, long)`.
    pub a1: Ewma,
    /// `ema(ema(|m|, long), short)`.
    pub a2: Ewma,
}
const _: () =
    assert!(core::mem::size_of::<TsiHdr>() == 112 && core::mem::align_of::<TsiHdr>() == 8);
// SAFETY: repr(C) of cold-at-zero blocks; all-zero is every part cold.
unsafe impl StateBlock for TsiHdr {}

/// `ta.dmi`'s block: the previous high/low/close and Wilder's four smoothers.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct DmiHdr {
    /// `high` at the site's previous execution.
    pub h1: f64,
    /// `low` at the previous execution.
    pub l1: f64,
    /// `close` at the previous execution.
    pub c1: f64,
    /// 0 = never executed.
    pub primed: u32,
    /// Always zero.
    pub _pad: u32,
    /// `rma(tr, diLen)`.
    pub tr: Ewma,
    /// `rma(+DM, diLen)`.
    pub p: Ewma,
    /// `rma(−DM, diLen)`.
    pub m: Ewma,
    /// `rma(DX, adxLen)`.
    pub adx: Ewma,
}
const _: () =
    assert!(core::mem::size_of::<DmiHdr>() == 128 && core::mem::align_of::<DmiHdr>() == 8);
// SAFETY: repr(C) POD of cold-at-zero parts; all-zero is `primed == 0` and every smoother cold.
unsafe impl StateBlock for DmiHdr {}

/// `ta.cum`'s running total.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Cum {
    /// The total.
    pub acc: f64,
    /// Executions so far (saturating); 0 = never executed.
    pub n: u32,
    /// Always zero.
    pub _pad: u32,
}
const _: () = assert!(core::mem::size_of::<Cum>() == 16 && core::mem::align_of::<Cum>() == 8);
// SAFETY: repr(C) POD; all-zero is `n == 0` with a zero total: never executed.
unsafe impl StateBlock for Cum {}

/// The value pushed `k` pushes before the newest, without pushing; `None` while fewer than
/// `k + 1` values were pushed. `k < ring.len()` (checked by the caller).
#[inline]
pub fn seq_peek<T: Copy>(h: &Seq, ring: &[T], k: u32) -> Option<T> {
    let cap = ring.len() as u32;
    debug_assert!(k < cap && h.pos < cap);
    if h.n <= k {
        return None;
    }
    let newest = if h.pos == 0 { cap - 1 } else { h.pos - 1 };
    let at = if newest >= k {
        newest - k
    } else {
        cap - (k - newest)
    };
    debug_assert!(at < cap);
    // SAFETY: `at < cap` by construction from `pos < cap` and `k < cap`.
    Some(unsafe { *ring.get_unchecked(at as usize) })
}

/// `ta.vwap(source, anchor[, stdev_mult])`'s block (§4.13.8): the anchored sums.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct VwapHdr {
    /// Σ volume since the last anchor.
    pub sv: f64,
    /// Σ volume · source.
    pub spv: f64,
    /// Σ volume · source² (the bands).
    pub sp2v: f64,
    /// 0 = the anchor was never true (the output is na until it is).
    pub started: u32,
    /// Always zero.
    pub _pad: u32,
    /// Always zero.
    pub _rsv: [u64; 4],
}
const _: () =
    assert!(core::mem::size_of::<VwapHdr>() == 64 && core::mem::align_of::<VwapHdr>() == 8);
// SAFETY: repr(C) POD; all-zero is `started == 0` with zero sums: the anchor never seen.
unsafe impl StateBlock for VwapHdr {}

/// `str.match`'s block (§3.6, §4.13.4): the compiled pattern and the FNV-1a of its bytes.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RegexCache {
    /// FNV-1a 64 of the pattern bytes (0 is remapped to 1); 0 = nothing compiled.
    pub key: u64,
    /// Always zero.
    pub _pad: u64,
    /// The compiled program.
    pub prog: crate::load::RegexProg,
}
const _: () =
    assert!(core::mem::size_of::<RegexCache>() == 2584 && core::mem::align_of::<RegexCache>() == 8);
// SAFETY: repr(C) POD (`RegexProg` is repr(C) + Copy of integers); all-zero is `key == 0`,
// nothing compiled, and the zero program is never run.
unsafe impl StateBlock for RegexCache {}
