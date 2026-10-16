// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` moving averages (impl-04 §4.13.5): `ta.sma`, `ta.ema`, `ta.rma`, `ta.wma`, `ta.swma`,
//! `ta.hma`, `ta.vwma`, `ta.alma`.

use crate::args::out1;
use crate::state::{
    ewma_hold, ewma_step, win2_push, win2_repoint, win_at, win_push, win_repoint, AlmaHdr, Ewma,
    HmaHdr, Win2Hdr, WinHdr,
};
use crate::sw;
use pine_ir::{abi, argwin, FaultKind, SiteId};
use pine_spec::{swr, NaPolicy, Sw};
use pine_vm::Frame;

const EMA_SEED: [u8; 2] = sw::both(Sw::EmaSeed);
const RMA_SEED: [u8; 2] = sw::both(Sw::RmaSeed);

/// `ta.sma(source, length)` — window `[src, len, out]`. W5: under `skip` an na source is not
/// accepted and the output is the unchanged window value. `NA` = the row's `Overload::na`, bound
/// in the table's `const` block (§3.2).
pub fn bi_ta_sma<const NA: u8>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let len = f.i64_arg(args[1]);
    let dst = out1(args)?;
    // An na length yields na without touching the block (§4.6, pin:04-ta-reference).
    let Some(len) = len else {
        f.set_f64_arg(dst, f64::NAN);
        return Ok(());
    };
    let out = {
        let (h, ring) = f.state_split::<WinHdr, f64>(site);
        if len < 1 || len > ring.len() as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let len = len as u32;
        if h.len != len {
            win_repoint(h, ring, len);
        }
        if !(NA == NaPolicy::Skip as u8 && src.is_nan()) {
            win_push(h, ring, src);
        }
        if h.n < len || h.na_in != 0 {
            f64::NAN
        } else {
            h.sum / len as f64
        }
    };
    f.set_f64_arg(dst, out);
    Ok(())
}

/// The shared EMA/RMA body: `alpha(len)`, the seed reading, `skip` na (hold).
#[inline(always)]
fn smooth(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
    alpha: fn(f64) -> f64,
    sma_seed: bool,
) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let len = f.i64_arg(args[1]);
    let dst = out1(args)?;
    let Some(len) = len else {
        f.set_f64_arg(dst, f64::NAN);
        return Ok(());
    };
    if len < 1 || len > u32::MAX as i64 {
        return Err(FaultKind::ScriptRefused);
    }
    let out = if src.is_nan() {
        ewma_hold(f.state_ro::<Ewma>(site))
    } else {
        ewma_step(
            f.state::<Ewma>(site),
            src,
            alpha(len as f64),
            len as u32,
            sma_seed,
        )
    };
    f.set_f64_arg(dst, out);
    Ok(())
}

/// `ta.ema(source, length)`: `α = 2/(len+1)`, seed per `Sw::EmaSeed` (`pin:U01-ema-seed`).
pub fn bi_ta_ema(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let sma = sw::pick(EMA_SEED, f.dialect()) == swr::EMA_SEED_SMA;
    smooth(f, site, args, |l| 2.0 / (l + 1.0), sma)
}

/// `ta.rma(source, length)`: Wilder's `α = 1/len` — not `ema` renamed; seed per `Sw::RmaSeed`
/// (`pin:00-rma-seed`).
pub fn bi_ta_rma(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let sma = sw::pick(RMA_SEED, f.dialect()) == swr::RMA_SEED_SMA;
    smooth(f, site, args, |l| 1.0 / l, sma)
}

/// `ta.wma(source, length)` — block `WinHdr` + `[ring | weights]` (two halves of the tail).
/// Weights `len..1` newest first ÷ `len(len+1)/2`, rebuilt on a length change; an exact weighted
/// sum per bar, never a recurrence (I26). `skip` na.
pub fn bi_ta_wma(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let dst = out1(args)?;
    let Some(len) = f.i64_arg(args[1]) else {
        f.set_f64_arg(dst, f64::NAN);
        return Ok(());
    };
    let out = {
        let (h, tail) = f.state_split::<WinHdr, f64>(site);
        let cap = tail.len() / 2;
        if len < 1 || len > cap as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let (ring, wts) = tail.split_at_mut(cap);
        let len = len as u32;
        if h.len != len {
            win_repoint(h, ring, len);
            let mut k = 0u32;
            while k < len {
                wts[k as usize] = (len - k) as f64;
                k += 1;
            }
        }
        if !src.is_nan() {
            win_push(h, ring, src);
        }
        if h.n < len {
            f64::NAN
        } else {
            let mut acc = 0.0f64;
            let mut k = 0u32;
            while k < len {
                acc = win_at(h, ring, k).mul_add(wts[k as usize], acc);
                k += 1;
            }
            acc / (len as f64 * (len as f64 + 1.0) / 2.0)
        }
    };
    f.set_f64_arg(dst, out);
    Ok(())
}

/// `ta.swma(source)`: weights `1/6, 2/6, 2/6, 1/6` over the last four values; `propagate` na
/// (an na inside the window makes the output na).
pub fn bi_ta_swma(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let out = {
        let (h, ring) = f.state_split::<WinHdr, f64>(site);
        if ring.len() != 4 {
            return Err(FaultKind::Internal);
        }
        if h.len != 4 {
            win_repoint(h, ring, 4);
        }
        win_push(h, ring, src);
        if h.n < 4 || h.na_in != 0 {
            f64::NAN
        } else {
            let (x0, x1, x2, x3) = (
                win_at(h, ring, 0),
                win_at(h, ring, 1),
                win_at(h, ring, 2),
                win_at(h, ring, 3),
            );
            (x0 + x3) / 6.0 + (x1 + x2) * (2.0 / 6.0)
        }
    };
    f.set_f64_arg(args[1], out);
    Ok(())
}

/// `ta.wma`'s arithmetic over the newest `len` values of a warm window, the weight `len − k`
/// computed in place: `Σ xₖ·(len − k) ÷ len(len+1)/2`, newest first, fused as `ta.wma`.
#[inline(always)]
fn wma_of(h: &WinHdr, ring: &[f64], len: u32) -> f64 {
    let mut acc = 0.0f64;
    let mut k = 0u32;
    while k < len {
        acc = win_at(h, ring, k).mul_add((len - k) as f64, acc);
        k += 1;
    }
    acc / (len as f64 * (len as f64 + 1.0) / 2.0)
}

/// `ta.hma(source, length)` — block `HmaHdr` + three rings in the thirds of the tail:
/// `wma(2·wma(src, len/2) − wma(src, len), ⌊√len⌋)`. `len/2` is integer division
/// (`pin:04-hma-half`): `length == 1` gives a half of 0, refused as `ta.wma(src, 0)` is. `skip`
/// na: an na source is not accepted and the output is the unchanged outer window's value; the
/// outer `wma` (itself `skip`) accepts a difference only once both inner windows are warm.
pub fn bi_ta_hma(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let dst = out1(args)?;
    let Some(len) = f.i64_arg(args[1]) else {
        f.set_f64_arg(dst, f64::NAN);
        return Ok(());
    };
    let out = {
        let (h, tail) = f.state_split::<HmaHdr, f64>(site);
        let cap = tail.len() / 3;
        if len < 2 || len > cap as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let len = len as u32;
        let half = len / 2;
        // `math.floor(math.sqrt(length))`: exact for every u32 (sqrt is correctly rounded).
        let sq = (len as f64).sqrt() as u32;
        let (r0, rest) = tail.split_at_mut(cap);
        let (r1, r2) = rest.split_at_mut(cap);
        let [w0, w1, w2] = &mut h.w;
        if w0.len != half {
            win_repoint(w0, r0, half);
        }
        if w1.len != len {
            win_repoint(w1, r1, len);
        }
        if w2.len != sq {
            win_repoint(w2, r2, sq);
        }
        if !src.is_nan() {
            win_push(w0, r0, src);
            win_push(w1, r1, src);
            if w1.n >= len {
                let d = 2.0 * wma_of(w0, r0, half) - wma_of(w1, r1, len);
                if !d.is_nan() {
                    win_push(w2, r2, d);
                }
            }
        }
        if w2.n < sq {
            f64::NAN
        } else {
            wma_of(w2, r2, sq)
        }
    };
    f.set_f64_arg(dst, out);
    Ok(())
}

const DIV0: [u8; 2] = sw::both(Sw::FloatDivZero);

/// `ta.vwma(source, length)` — block `Win2Hdr` + one ring of `(src, volume)` pairs (operator
/// ruling A5): `Σ(src·vol) / Σ vol`; a zero-volume window divides by 0 per `Sw::FloatDivZero`.
/// `skip` na applies to the pair: one is accepted only when `source` and `volume` are both
/// non-na (`pin:04-volume-pair-skip` — the manual's `sma(x·volume)/sma(volume)` as one ring).
pub fn bi_ta_vwma(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let vol = f.f64_arg(argwin::f64(abi::F_VOLUME));
    let dst = out1(args)?;
    let Some(len) = f.i64_arg(args[1]) else {
        f.set_f64_arg(dst, f64::NAN);
        return Ok(());
    };
    let na_nonfinite = sw::pick(DIV0, f.dialect()) == swr::FLOAT_DIV_ZERO_NA;
    let out = {
        let (h, ring) = f.state_split::<Win2Hdr, [f64; 2]>(site);
        if len < 1 || len > ring.len() as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let len = len as u32;
        if h.win.len != len {
            win2_repoint(h, ring, len);
        }
        if !src.is_nan() && !vol.is_nan() {
            win2_push(h, ring, src, vol);
        }
        if h.win.n < len {
            f64::NAN
        } else {
            h.win.sum / h.sum_v
        }
    };
    f.set_f64_arg(
        dst,
        if na_nonfinite && out.is_infinite() {
            f64::NAN
        } else {
            out
        },
    );
    Ok(())
}

/// `ta.alma(series, length, offset, sigma, floor)` — block `AlmaHdr` + `[ring | weights]`
/// (operator ruling A5): the manual's `pine_alma` literally — `m = offset × (length − 1)`
/// (floored when `floor`), `s = length / sigma`, weight `i` = `exp(−(i − m)² / (2·s²))`
/// (`math.pow(x, 2)` as `x·x`: one rounding either way), `norm = Σ weights` and
/// `sum = Σ series[length − i − 1]·weightᵢ` accumulated in `i` order, unfused; the value is
/// `sum / norm`. Weights rebuild only when `(win.len, m, s)` change bitwise. `propagate` na: an
/// na in the window makes `sum` na. An na `floor` (v5) reads as `false` (`pin:04-handle-na`).
pub fn bi_ta_alma(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let (offset, sigma) = (f.f64_arg(args[2]), f.f64_arg(args[3]));
    let floor = f.bool_arg(args[4]) == Some(true);
    let Some(len) = f.i64_arg(args[1]) else {
        f.set_f64_arg(args[5], f64::NAN);
        return Ok(());
    };
    let out = {
        let (h, tail) = f.state_split::<AlmaHdr, f64>(site);
        let cap = tail.len() / 2;
        if len < 1 || len > cap as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let (ring, wts) = tail.split_at_mut(cap);
        let len = len as u32;
        let m = offset * (len - 1) as f64;
        let m = if floor { m.floor() } else { m };
        let s = len as f64 / sigma;
        let rebuild =
            h.win.len != len || h.m.to_bits() != m.to_bits() || h.s.to_bits() != s.to_bits();
        if h.win.len != len {
            win_repoint(&mut h.win, ring, len);
        }
        if rebuild {
            let den = 2.0 * (s * s);
            let mut norm = 0.0f64;
            let mut i = 0u32;
            while i < len {
                let d = i as f64 - m;
                let w = (-(d * d) / den).exp();
                wts[i as usize] = w;
                norm += w;
                i += 1;
            }
            h.m = m;
            h.s = s;
            h.norm = norm;
        }
        win_push(&mut h.win, ring, src);
        if h.win.n < len {
            f64::NAN
        } else {
            let mut sum = 0.0f64;
            let mut i = 0u32;
            while i < len {
                sum += win_at(&h.win, ring, len - 1 - i) * wts[i as usize];
                i += 1;
            }
            sum / h.norm
        }
    };
    f.set_f64_arg(args[5], out);
    Ok(())
}
