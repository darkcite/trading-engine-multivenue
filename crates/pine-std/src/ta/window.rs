// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` windows (impl-04 §4.13.6–§4.13.7): `ta.variance`, `ta.stdev`, the extremes, `ta.bb`,
//! `ta.bbw`, `ta.linreg`, `ta.dev`, `ta.cci`, `ta.cog`, `ta.stoch`, `ta.wpr`.

use crate::args::out1;
use crate::naf::fdiv;
use crate::state::{
    mono_push, mono_query, win_at, win_push, win_repoint, Mono2Hdr, MonoCell, MonoHdr, WinHdr,
};
use crate::sw;
use pine_ir::{abi, argwin, FaultKind, SiteId};
use pine_spec::{swr, Sw};
use pine_vm::Frame;

const DIV0: [u8; 2] = sw::both(Sw::FloatDivZero);

/// `Sw::FloatDivZero`'s `na` reading for this call's dialect.
#[inline(always)]
fn na_div0(f: &Frame<'_>) -> bool {
    sw::pick(DIV0, f.dialect()) == swr::FLOAT_DIV_ZERO_NA
}

/// Window variance over `[src, len, biased, out]`: `sum2/n − mean²`, clamped at 0 before any
/// `sqrt` (§4.5 item 3); `biased == false` scales by `n/(n−1)`. `skip` na (W5/U18). An na
/// `biased` (v5) yields na (`pin:04-biased-na`).
#[inline(always)]
fn variance(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(f64, u16), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let len = f.i64_arg(args[1]);
    let biased = f.bool_arg(args[2]);
    let dst = out1(args)?;
    let (Some(len), Some(biased)) = (len, biased) else {
        return Ok((f64::NAN, dst));
    };
    let (h, ring) = f.state_split::<WinHdr, f64>(site);
    if len < 1 || len > ring.len() as i64 {
        return Err(FaultKind::ScriptRefused);
    }
    let len = len as u32;
    if h.len != len {
        win_repoint(h, ring, len);
    }
    if !src.is_nan() {
        win_push(h, ring, src);
    }
    if h.n < len {
        return Ok((f64::NAN, dst));
    }
    let n = len as f64;
    let mean = h.sum / n;
    let v = h.sum2 / n - mean * mean;
    let v = if v > 0.0 { v } else { 0.0 };
    let v = if biased {
        v
    } else if len > 1 {
        v * n / (n - 1.0)
    } else {
        f64::NAN
    };
    Ok((v, dst))
}

/// `ta.variance(source, length, biased)`.
pub fn bi_ta_variance(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let (v, dst) = variance(f, site, args)?;
    f.set_f64_arg(dst, v);
    Ok(())
}

/// `ta.stdev(source, length, biased)` = `sqrt(ta.variance(…))`.
pub fn bi_ta_stdev(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let (v, dst) = variance(f, site, args)?;
    f.set_f64_arg(dst, v.sqrt());
    Ok(())
}

/// `ta.highest`/`ta.lowest` (`BARS == false`) and `…bars` (`BARS == true`) over `[src?, len,
/// out]`; `SRC == false` reads `high` (MAX) or `low` from the reserved slots. `skip` na.
pub fn bi_extreme<const MAX: bool, const SRC: bool, const BARS: bool>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    let n_in = if SRC { 2 } else { 1 };
    if args.len() != n_in + 1 {
        return Err(FaultKind::Internal);
    }
    let src = if SRC {
        f.f64_arg(args[0])
    } else if MAX {
        f.f64_arg(argwin::f64(abi::F_HIGH))
    } else {
        f.f64_arg(argwin::f64(abi::F_LOW))
    };
    let len = f.i64_arg(args[n_in - 1]);
    let dst = out1(args)?;
    let Some(len) = len else {
        if BARS {
            f.set_i64_arg(dst, None);
        } else {
            f.set_f64_arg(dst, f64::NAN);
        }
        return Ok(());
    };
    let r = {
        let (h, q) = f.state_split::<MonoHdr, MonoCell>(site);
        if len < 1 || len > q.len() as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        if !src.is_nan() {
            mono_push::<MAX>(h, q, src);
        }
        mono_query(h, q, len as u32)
    };
    // `…bars` returns `series int` (pine-spec TA_*BARS*): its output entry is in the i64 lane.
    if BARS {
        f.set_i64_arg(dst, r.map(|(_, back)| -i64::from(back)));
    } else {
        f.set_f64_arg(dst, r.map_or(f64::NAN, |(v, _)| v));
    }
    Ok(())
}

/// The Bollinger core over `[src, len, …]`: `(mid, population stdev)` from one set of sums,
/// variance clamped at 0; `skip` na.
fn bb_core(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(f64, f64), FaultKind> {
    let src = f.f64_arg(args[0]);
    let Some(len) = f.i64_arg(args[1]) else {
        return Ok((f64::NAN, f64::NAN));
    };
    let (h, ring) = f.state_split::<WinHdr, f64>(site);
    if len < 1 || len > ring.len() as i64 {
        return Err(FaultKind::ScriptRefused);
    }
    let len = len as u32;
    if h.len != len {
        win_repoint(h, ring, len);
    }
    if !src.is_nan() {
        win_push(h, ring, src);
    }
    if h.n < len {
        return Ok((f64::NAN, f64::NAN));
    }
    let n = len as f64;
    let m = h.sum / n;
    let v = h.sum2 / n - m * m;
    Ok((m, if v > 0.0 { v.sqrt() } else { 0.0 }))
}

/// `ta.bb(series, length, mult) -> (middle, upper, lower)` — window `[src, len, mult, mid, up,
/// lo]`: `sma ± mult × stdev`.
pub fn bi_ta_bb(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let mult = f.f64_arg(args[2]);
    let (mid, sd) = bb_core(f, site, args)?;
    f.set_f64_arg(args[3], mid);
    f.set_f64_arg(args[4], mult.mul_add(sd, mid));
    f.set_f64_arg(args[5], (-mult).mul_add(sd, mid));
    Ok(())
}

/// `ta.bbw(series, length, mult)` = `(upper − lower) / middle`.
pub fn bi_ta_bbw(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let mult = f.f64_arg(args[2]);
    let (mid, sd) = bb_core(f, site, args)?;
    let (up, lo) = (mult.mul_add(sd, mid), (-mult).mul_add(sd, mid));
    f.set_f64_arg(args[3], (up - lo) / mid);
    Ok(())
}

/// `ta.linreg(source, length, offset)` — block `WinHdr` + `[ring | x column]`: least squares over
/// `x = len − 1 − k` for the value `k` back, `intercept + slope × (len − 1 − offset)`;
/// `propagate` na (an na inside the window makes the output na). `len == 1` is the value itself.
pub fn bi_ta_linreg(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let (len, off) = (f.i64_arg(args[1]), f.i64_arg(args[2]));
    let (Some(len), Some(off)) = (len, off) else {
        f.set_f64_arg(args[3], f64::NAN);
        return Ok(());
    };
    let out = {
        let (h, tail) = f.state_split::<WinHdr, f64>(site);
        let cap = tail.len() / 2;
        if len < 1 || len > cap as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let (ring, xs) = tail.split_at_mut(cap);
        let len = len as u32;
        if h.len != len {
            win_repoint(h, ring, len);
            let mut k = 0u32;
            while k < len {
                xs[k as usize] = (len - 1 - k) as f64;
                k += 1;
            }
        }
        win_push(h, ring, src);
        if h.n < len || h.na_in != 0 {
            f64::NAN
        } else if len == 1 {
            src
        } else {
            let n = len as f64;
            let sx = n * (n - 1.0) / 2.0;
            let sxx = (n - 1.0) * n * (2.0 * n - 1.0) / 6.0;
            let mut sxy = 0.0f64;
            let mut k = 0u32;
            while k < len {
                sxy = win_at(h, ring, k).mul_add(xs[k as usize], sxy);
                k += 1;
            }
            let slope = (n * sxy - sx * h.sum) / (n * sxx - sx * sx);
            let icept = (h.sum - slope * sx) / n;
            slope.mul_add(n - 1.0 - off as f64, icept)
        }
    };
    f.set_f64_arg(args[3], out);
    Ok(())
}

/// The mean-deviation core of `ta.dev`/`ta.cci` over `[src, len, out]`: `(newest accepted value,
/// mean, dev)` per the manual's `pine_dev` — `mean = ta.sma(source, length)`, then
/// `Σ |source[i] − mean|` for `i = 0..length` (newest first) `/ length`; `skip` na (an na source
/// is not accepted and the values are the window's current ones). `None` while warming up.
fn dev_core(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<Option<(f64, f64, f64)>, FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let Some(len) = f.i64_arg(args[1]) else {
        return Ok(None);
    };
    let (h, ring) = f.state_split::<WinHdr, f64>(site);
    if len < 1 || len > ring.len() as i64 {
        return Err(FaultKind::ScriptRefused);
    }
    let len = len as u32;
    if h.len != len {
        win_repoint(h, ring, len);
    }
    if !src.is_nan() {
        win_push(h, ring, src);
    }
    if h.n < len {
        return Ok(None);
    }
    let mean = h.sum / len as f64;
    let mut sum = 0.0f64;
    let mut i = 0u32;
    while i < len {
        sum += (win_at(h, ring, i) - mean).abs();
        i += 1;
    }
    Ok(Some((win_at(h, ring, 0), mean, sum / len as f64)))
}

/// `ta.dev(source, length)`: mean absolute deviation from the window mean (`pine_dev`).
pub fn bi_ta_dev(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let r = dev_core(f, site, args)?;
    f.set_f64_arg(out1(args)?, r.map_or(f64::NAN, |x| x.2));
    Ok(())
}

/// `ta.cci(source, length)` = `(src − sma) / (0.015 × dev)`; a flat window divides by 0 per
/// `Sw::FloatDivZero`. Under `skip` the source is the newest accepted value.
pub fn bi_ta_cci(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let na0 = na_div0(f);
    let r = dev_core(f, site, args)?;
    let v = r.map_or(f64::NAN, |(x, m, d)| fdiv(x - m, 0.015 * d, na0));
    f.set_f64_arg(out1(args)?, v);
    Ok(())
}

/// `ta.cog(source, length)` — block `WinHdr` + `[ring | (i + 1) column]`: the manual's
/// `pine_cog`, `−num / sum` with `num = Σ source[i]·(i + 1)` (newest first, unfused as written)
/// and `sum` = `math.sum(source, length)` taken by the same pass (exact, no rolling drift); ÷0 per
/// `Sw::FloatDivZero`. `skip` na.
pub fn bi_ta_cog(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let na0 = na_div0(f);
    let Some(len) = f.i64_arg(args[1]) else {
        f.set_f64_arg(args[2], f64::NAN);
        return Ok(());
    };
    let out = {
        let (h, tail) = f.state_split::<WinHdr, f64>(site);
        let cap = tail.len() / 2;
        if len < 1 || len > cap as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let (ring, ws) = tail.split_at_mut(cap);
        let len = len as u32;
        if h.len != len {
            win_repoint(h, ring, len);
            let mut i = 0u32;
            while i < len {
                ws[i as usize] = (i + 1) as f64;
                i += 1;
            }
        }
        if !src.is_nan() {
            win_push(h, ring, src);
        }
        if h.n < len {
            f64::NAN
        } else {
            let (mut num, mut sum) = (0.0f64, 0.0f64);
            let mut i = 0u32;
            while i < len {
                let price = win_at(h, ring, i);
                num += price * ws[i as usize];
                sum += price;
                i += 1;
            }
            fdiv(-num, sum, na0)
        }
    };
    f.set_f64_arg(args[2], out);
    Ok(())
}

/// The `(highest(high, len), lowest(low, len))` pair of `ta.stoch`/`ta.wpr` over the halves of a
/// `Mono2Hdr` tail; each deque skips its own na, as `ta.highest`/`ta.lowest` do
/// (`pin:04-ta-composed`). `None` while either is warming up.
fn hilo(
    f: &mut Frame<'_>,
    site: SiteId,
    hi: f64,
    lo: f64,
    len: i64,
) -> Result<Option<(f64, f64)>, FaultKind> {
    let (h, q) = f.state_split::<Mono2Hdr, MonoCell>(site);
    let cap = q.len() / 2;
    if len < 1 || len > cap as i64 {
        return Err(FaultKind::ScriptRefused);
    }
    let (qh, ql) = q.split_at_mut(cap);
    if !hi.is_nan() {
        mono_push::<true>(&mut h.hi, qh, hi);
    }
    if !lo.is_nan() {
        mono_push::<false>(&mut h.lo, ql, lo);
    }
    let len = len as u32;
    Ok(
        match (mono_query(&h.hi, qh, len), mono_query(&h.lo, ql, len)) {
            (Some((hh, _)), Some((ll, _))) => Some((hh, ll)),
            _ => None,
        },
    )
}

/// `ta.stoch(source, high, low, length)` = `100 × (src − lowest(low)) / (highest(high) −
/// lowest(low))`, the manual's formula composed literally: the two windows skip their own na and
/// the current `source` enters as is (an na source is an na result, `pin:04-ta-composed`); a
/// flat window divides by 0 per `Sw::FloatDivZero`.
pub fn bi_ta_stoch(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let (src, hi, lo) = (f.f64_arg(args[0]), f.f64_arg(args[1]), f.f64_arg(args[2]));
    let na0 = na_div0(f);
    let Some(len) = f.i64_arg(args[3]) else {
        f.set_f64_arg(args[4], f64::NAN);
        return Ok(());
    };
    let v = match hilo(f, site, hi, lo, len)? {
        Some((hh, ll)) => fdiv(100.0 * (src - ll), hh - ll, na0),
        None => f64::NAN,
    };
    f.set_f64_arg(args[4], v);
    Ok(())
}

/// `ta.wpr(length)` = `−100 × (highest(high) − close) / (highest − lowest)` over the bar's
/// reserved `high`/`low`/`close`, composed as `ta.stoch` (`pin:04-ta-composed`).
pub fn bi_ta_wpr(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let hi = f.f64_arg(argwin::f64(abi::F_HIGH));
    let lo = f.f64_arg(argwin::f64(abi::F_LOW));
    let c = f.f64_arg(argwin::f64(abi::F_CLOSE));
    let na0 = na_div0(f);
    let Some(len) = f.i64_arg(args[0]) else {
        f.set_f64_arg(args[1], f64::NAN);
        return Ok(());
    };
    let v = match hilo(f, site, hi, lo, len)? {
        Some((hh, ll)) => fdiv(-100.0 * (hh - c), hh - ll, na0),
        None => f64::NAN,
    };
    f.set_f64_arg(args[1], v);
    Ok(())
}
