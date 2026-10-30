// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.correlation(source1, source2, length)` (impl-04 §4.13.6, T25): rolling `Σa Σb Σa² Σb² Σab`
//! over the newest `length` accepted pairs, with the exact recompute every [`WIN_RECOMPUTE`]
//! accepted pairs (§4.5, I27). A pair is accepted only when both values are non-na (`skip`).

use crate::args::out1;
use crate::state::WIN_RECOMPUTE;
use pine_ir::{FaultKind, SiteId};
use pine_vm::{Frame, StateBlock};

/// Header of a `ta.correlation` block; the tail is the site's ring of `[a, b]` pairs.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct CorrHdr {
    /// Σa over the window.
    pub sa: f64,
    /// Σb.
    pub sb: f64,
    /// Σa².
    pub saa: f64,
    /// Σb².
    pub sbb: f64,
    /// Σab.
    pub sab: f64,
    /// Next write position in the tail, `0..cap`.
    pub pos: u32,
    /// Pairs accepted so far (saturating).
    pub n: u32,
    /// The window the sums describe; 0 = cold.
    pub len: u32,
    /// Accepted pairs until the exact recompute.
    pub recompute: u32,
    /// Always zero.
    pub _pad: u64,
}
const _: () =
    assert!(core::mem::size_of::<CorrHdr>() == 64 && core::mem::align_of::<CorrHdr>() == 8);
// SAFETY: repr(C), Copy, no references, no padding read; all-zero is `len == 0`: never executed.
unsafe impl StateBlock for CorrHdr {}

/// Ring index of the pair accepted `k` pairs ago (`k = 0`: the newest); `k < min(n, cap)`.
#[inline(always)]
fn back(h: &CorrHdr, cap: u32, k: u32) -> u32 {
    debug_assert!(k < cap && k < h.n);
    let p = if h.pos == 0 { cap - 1 } else { h.pos - 1 };
    if k <= p {
        p - k
    } else {
        cap - (k - p)
    }
}

#[inline(always)]
fn at(ring: &[[f64; 2]], i: u32) -> [f64; 2] {
    debug_assert!((i as usize) < ring.len());
    // SAFETY: every caller passes an index produced by `back`, which is `< ring.len()`.
    unsafe { *ring.get_unchecked(i as usize) }
}

/// Recompute the sums exactly over the newest `len` accepted pairs and make `len` the window.
fn repoint(h: &mut CorrHdr, ring: &[[f64; 2]], len: u32) {
    let cap = ring.len() as u32;
    debug_assert!(len >= 1 && len <= cap);
    h.len = len;
    let have = if h.n < len { h.n } else { len };
    let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    let mut k = 0u32;
    while k < have {
        let [a, b] = at(ring, back(h, cap, k));
        sa += a;
        sb += b;
        saa = a.mul_add(a, saa);
        sbb = b.mul_add(b, sbb);
        sab = a.mul_add(b, sab);
        k += 1;
    }
    (h.sa, h.sb, h.saa, h.sbb, h.sab) = (sa, sb, saa, sbb, sab);
    h.recompute = WIN_RECOMPUTE;
}

/// Accept one pair (both non-na).
fn push(h: &mut CorrHdr, ring: &mut [[f64; 2]], a: f64, b: f64) {
    let cap = ring.len() as u32;
    debug_assert!(h.len >= 1 && h.len <= cap);
    debug_assert!(!a.is_nan() && !b.is_nan());
    if h.n >= h.len {
        let [oa, ob] = at(ring, back(h, cap, h.len - 1));
        h.sa -= oa;
        h.sb -= ob;
        h.saa -= oa * oa;
        h.sbb -= ob * ob;
        h.sab -= oa * ob;
    }
    h.sa += a;
    h.sb += b;
    h.saa = a.mul_add(a, h.saa);
    h.sbb = b.mul_add(b, h.sbb);
    h.sab = a.mul_add(b, h.sab);
    debug_assert!(h.pos < cap);
    // SAFETY: `pos < cap` holds: it starts at 0 and wraps by compare below.
    unsafe { *ring.get_unchecked_mut(h.pos as usize) = [a, b] };
    h.pos = if h.pos + 1 == cap { 0 } else { h.pos + 1 };
    h.n = h.n.saturating_add(1);
    if h.recompute == 0 {
        let l = h.len;
        repoint(h, ring, l);
    } else {
        h.recompute -= 1;
    }
}

/// Pearson's r from the window sums: `cov / √(var_a · var_b)` with population moments
/// (`cov = Σab/n − ā·b̄`, `var = Σx²/n − x̄²`; the `n` vs `n − 1` choice cancels). A variance at or
/// below 0 (a flat window, or the cancellation edge) → na. Evaluation order and the absence of a
/// clamp to `[−1, 1]` are `pin:04-correlation-form`.
fn value(h: &CorrHdr) -> f64 {
    let n = h.len as f64;
    let (ma, mb) = (h.sa / n, h.sb / n);
    let va = h.saa / n - ma * ma;
    let vb = h.sbb / n - mb * mb;
    if va <= 0.0 || vb <= 0.0 {
        return f64::NAN;
    }
    (h.sab / n - ma * mb) / (va * vb).sqrt()
}

/// `ta.correlation(source1, source2, length)` over `[a, b, len, out]`.
pub fn bi_ta_correlation(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let (a, b) = (f.f64_arg(args[0]), f.f64_arg(args[1]));
    let out = out1(args)?;
    let Some(len) = f.i64_arg(args[2]) else {
        f.set_f64_arg(out, f64::NAN);
        return Ok(());
    };
    let v = {
        let (h, ring) = f.state_split::<CorrHdr, [f64; 2]>(site);
        let cap = ring.len();
        if len < 1 || len > cap as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let len = len as u32;
        if h.len != len {
            repoint(h, ring, len);
        }
        if !a.is_nan() && !b.is_nan() {
            push(h, ring, a, b);
        }
        if h.n < len {
            f64::NAN
        } else {
            value(h)
        }
    };
    f.set_f64_arg(out, v);
    Ok(())
}
