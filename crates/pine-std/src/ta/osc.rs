// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` oscillators, bands, trend (impl-04 §4.13.7): `ta.tr`, `ta.atr`.

use crate::args::out1;
use crate::naf::fmax;
use crate::state::{ewma_hold, ewma_step, AtrHdr, Prev};
use crate::sw;
use pine_ir::{abi, argwin, FaultKind, SiteId};
use pine_spec::{swr, Sw};
use pine_vm::Frame;

const RMA_SEED: [u8; 2] = sw::both(Sw::RmaSeed);

/// The bar's `(high, low, close)` from the reserved slots (F7).
#[inline(always)]
fn hlc(f: &Frame<'_>) -> (f64, f64, f64) {
    (
        f.f64_arg(argwin::f64(abi::F_HIGH)),
        f.f64_arg(argwin::f64(abi::F_LOW)),
        f.f64_arg(argwin::f64(abi::F_CLOSE)),
    )
}

/// One true-range step on `p`: `max(h − l, |h − pc|, |l − pc|)`, `pc` the close at the site's
/// previous execution; on the first execution `h − l` when `handle_na`, else na.
#[inline(always)]
fn tr_step(p: &mut Prev, h: f64, l: f64, c: f64, handle_na: bool) -> f64 {
    let r = if p.primed != 0 {
        fmax(fmax(h - l, (h - p.v).abs()), (l - p.v).abs())
    } else if handle_na {
        h - l
    } else {
        f64::NAN
    };
    p.v = c;
    p.primed = 1;
    r
}

/// `ta.tr(handle_na)` — window `[handle_na, out]`; an na `handle_na` (v5) reads as `false`, the
/// documented non-handling behaviour.
pub fn bi_ta_tr(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let handle_na = f.bool_arg(args[0]) == Some(true);
    let (h, l, c) = hlc(f);
    let r = tr_step(f.state::<Prev>(site), h, l, c, handle_na);
    f.set_f64_arg(args[1], r);
    Ok(())
}

/// The `ta.tr` variable: `ta.tr(false)` — window `[out]`.
pub fn bi_ta_tr_var(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let dst = out1(args)?;
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let (h, l, c) = hlc(f);
    let r = tr_step(f.state::<Prev>(site), h, l, c, false);
    f.set_f64_arg(dst, r);
    Ok(())
}

/// `ta.atr(length)` = `rma(tr(true), length)` — window `[len, out]`; seed per `Sw::RmaSeed`.
pub fn bi_ta_atr(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let Some(len) = f.i64_arg(args[0]) else {
        f.set_f64_arg(args[1], f64::NAN);
        return Ok(());
    };
    if len < 1 || len > u32::MAX as i64 {
        return Err(FaultKind::ScriptRefused);
    }
    let sma = sw::pick(RMA_SEED, f.dialect()) == swr::RMA_SEED_SMA;
    let (h, l, c) = hlc(f);
    let out = {
        let s = f.state::<AtrHdr>(site);
        let tr = tr_step(&mut s.prev, h, l, c, true);
        if tr.is_nan() {
            ewma_hold(&s.ew)
        } else {
            ewma_step(&mut s.ew, tr, 1.0 / len as f64, len as u32, sma)
        }
    };
    f.set_f64_arg(args[1], out);
    Ok(())
}

const EMA_SEED: [u8; 2] = sw::both(Sw::EmaSeed);

/// A length argument as `u32`: `None` for na; `ScriptRefused` below 1.
#[inline(always)]
fn length(f: &Frame<'_>, e: u16) -> Result<Option<u32>, FaultKind> {
    match f.i64_arg(e) {
        None => Ok(None),
        Some(l) if (1..=u32::MAX as i64).contains(&l) => Ok(Some(l as u32)),
        Some(_) => Err(FaultKind::ScriptRefused),
    }
}

/// `100 − 100/(1 + up/dn)`; `dn == 0` → 100, `up == 0` → 0; na while either is unseeded.
#[inline(always)]
fn rsi_of(up: f64, dn: f64) -> f64 {
    if up.is_nan() || dn.is_nan() {
        f64::NAN
    } else if dn == 0.0 {
        100.0
    } else if up == 0.0 {
        0.0
    } else {
        100.0 - 100.0 / (1.0 + up / dn)
    }
}

/// `ta.rsi(source, length)` — window `[src, len, out]`: Wilder smoothing of the gains and losses
/// (seed per `Sw::RmaSeed`); `skip` na holds the value.
pub fn bi_ta_rsi(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let Some(len) = length(f, args[1])? else {
        f.set_f64_arg(args[2], f64::NAN);
        return Ok(());
    };
    let sma = sw::pick(RMA_SEED, f.dialect()) == swr::RMA_SEED_SMA;
    let out = if src.is_nan() {
        let s = f.state_ro::<crate::state::RsiHdr>(site);
        rsi_of(ewma_hold(&s.up), ewma_hold(&s.dn))
    } else {
        let s = f.state::<crate::state::RsiHdr>(site);
        let primed = s.prev.primed != 0;
        let d = src - s.prev.v;
        s.prev.v = src;
        s.prev.primed = 1;
        if primed {
            let a = 1.0 / len as f64;
            let u = ewma_step(&mut s.up, if d > 0.0 { d } else { 0.0 }, a, len, sma);
            let w = ewma_step(&mut s.dn, if d < 0.0 { -d } else { 0.0 }, a, len, sma);
            rsi_of(u, w)
        } else {
            f64::NAN
        }
    };
    f.set_f64_arg(args[2], out);
    Ok(())
}

/// `ta.macd(source, fastlen, slowlen, siglen) -> (macd, signal, hist)` — window
/// `[src, fast, slow, sig, macd, signal, hist]`; the signal is the EMA of the macd line; seeds per
/// `Sw::EmaSeed`; `skip` na holds every output.
pub fn bi_ta_macd(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 7 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let (fl, sl, gl) = (
        length(f, args[1])?,
        length(f, args[2])?,
        length(f, args[3])?,
    );
    let (Some(fl), Some(sl), Some(gl)) = (fl, sl, gl) else {
        let mut k = 4usize;
        while k < 7 {
            f.set_f64_arg(args[k], f64::NAN);
            k += 1;
        }
        return Ok(());
    };
    let sma = sw::pick(EMA_SEED, f.dialect()) == swr::EMA_SEED_SMA;
    let (m, g) = if src.is_nan() {
        let s = f.state_ro::<crate::state::MacdHdr>(site);
        (ewma_hold(&s.fast) - ewma_hold(&s.slow), ewma_hold(&s.sig))
    } else {
        let s = f.state::<crate::state::MacdHdr>(site);
        let a = ewma_step(&mut s.fast, src, 2.0 / (fl as f64 + 1.0), fl, sma);
        let b = ewma_step(&mut s.slow, src, 2.0 / (sl as f64 + 1.0), sl, sma);
        let m = a - b;
        let g = if m.is_nan() {
            ewma_hold(&s.sig)
        } else {
            ewma_step(&mut s.sig, m, 2.0 / (gl as f64 + 1.0), gl, sma)
        };
        (m, g)
    };
    f.set_f64_arg(args[4], m);
    f.set_f64_arg(args[5], g);
    f.set_f64_arg(args[6], m - g);
    Ok(())
}
