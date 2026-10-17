// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` windows (impl-04 §4.13.6–§4.13.7): `ta.variance`, `ta.stdev`, the extremes, `ta.bb`,
//! `ta.bbw`, `ta.linreg`, `ta.dev`, `ta.cci`, `ta.cog`, `ta.stoch`, `ta.wpr`.

use crate::args::out1;
use crate::naf::fdiv;
use crate::state::{
    mono_push, mono_query, win_at, win_push, win_repoint, Mono2Hdr, MonoCell, MonoHdr, Prev, WinHdr,
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

/// Neumaier's compensated running sum: `(sum, compensation)`; the value is `sum + comp`.
#[inline(always)]
fn neumaier(acc: (f64, f64), x: f64) -> (f64, f64) {
    let (s, c) = acc;
    let t = s + x;
    let c = if s.abs() >= x.abs() {
        c + ((s - t) + x)
    } else {
        c + ((x - t) + s)
    };
    (t, c)
}

/// TradingView's moments of the newest `len` values (`h.n >= len`): a fresh pass each bar, oldest
/// value first, each sum Neumaier-compensated, squares unfused — `mean = (Σ x) / n`,
/// `var = (Σ x·x) / n − mean²` (`pin:04-var-fresh-sum`, CI-015 / CI-015a: bit-exact on every
/// captured row of B025 B030 (both columns) B077 (3 columns) B078 B103 — 8 columns, 1024 rows;
/// the header's rolling sums and a plain fresh sum both drift in the last digits from row 15;
/// an exactly rounded sum (`fsum`) also matches and is not separated by the capture). The
/// header's rolling sums still track the window; only the reported moments come from this pass.
#[inline(always)]
fn fresh_moments(h: &WinHdr, ring: &[f64], len: u32) -> (f64, f64) {
    let (mut s, mut s2) = ((0.0f64, 0.0f64), (0.0f64, 0.0f64));
    let mut k = len;
    while k > 0 {
        k -= 1;
        let x = win_at(h, ring, k);
        s = neumaier(s, x);
        s2 = neumaier(s2, x * x);
    }
    let n = len as f64;
    let mean = (s.0 + s.1) / n;
    (mean, (s2.0 + s2.1) / n - mean * mean)
}

/// Window variance over `[src, len, biased, out]`: `sum2/n − mean²` (`fresh_moments`), clamped at 0 before any
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
    let (_, v) = fresh_moments(h, ring, len);
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
    let (m, v) = fresh_moments(h, ring, len);
    Ok((m, if v > 0.0 { v.sqrt() } else { 0.0 }))
}

/// `ta.bb(series, length, mult) -> (middle, upper, lower)` — window `[src, len, mult, mid, up,
/// lo]`: `sma ± mult × stdev`, unfused as the reference writes it (`dev = mult × stdev`,
/// `basis ± dev`; `pin:04-var-fresh-sum`, CI-015, fixture B077).
pub fn bi_ta_bb(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let mult = f.f64_arg(args[2]);
    let (mid, sd) = bb_core(f, site, args)?;
    let dev = mult * sd;
    f.set_f64_arg(args[3], mid);
    f.set_f64_arg(args[4], mid + dev);
    f.set_f64_arg(args[5], mid - dev);
    Ok(())
}

/// `ta.bbw(series, length, mult)` = the reference's own Pine twin, unfused as written:
/// `dev = mult × stdev`, `(((basis + dev) − (basis − dev)) / basis) × 100` — a percentage
/// (`pin:04-bbw-percent`, CI-015a, fixture B078: TradingView 64.43888212862731 at row 13 where
/// the plain ratio gave 0.644…).
pub fn bi_ta_bbw(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let mult = f.f64_arg(args[2]);
    let (mid, sd) = bb_core(f, site, args)?;
    let dev = mult * sd;
    f.set_f64_arg(args[3], ((mid + dev) - (mid - dev)) / mid * 100.0);
    Ok(())
}

/// `ta.max(source)` (`MAX`) / `ta.min(source)` — block `Prev` (`v` = the extreme so far,
/// `primed` = a non-na value was seen): the all-time extreme of the non-na values from the first
/// bar; na until the first one (`skip`, warm-up 1). CI-013, fixtures B092 B094.
pub fn bi_ta_alltime<const MAX: bool>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let v = {
        let s = f.state::<Prev>(site);
        if !src.is_nan() && (s.primed == 0 || (MAX && src > s.v) || (!MAX && src < s.v)) {
            s.v = src;
            s.primed = 1;
        }
        if s.primed == 0 {
            f64::NAN
        } else {
            s.v
        }
    };
    f.set_f64_arg(args[1], v);
    Ok(())
}

/// `ta.range(source, length)` = `highest − lowest` of the newest `length` non-na values over a
/// `Mono2Hdr` block (one deque per extreme, as `ta.stoch`); na until `length` are accepted
/// (`skip`). The `int` overload (`source` and the result in the i64 lane) runs the same deques
/// on the exact f64 image of each value (|v| < 2^53) and writes the exact difference back.
/// CI-013, fixture B098.
pub fn bi_ta_range(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let int = argwin::is_i64(args[0]);
    let src = if int {
        f.i64_arg(args[0]).map_or(f64::NAN, |v| v as f64)
    } else {
        f.f64_arg(args[0])
    };
    let Some(len) = f.i64_arg(args[1]) else {
        put_num(f, args[2], f64::NAN);
        return Ok(());
    };
    let v = match hilo(f, site, src, src, len)? {
        Some((hh, ll)) => hh - ll,
        None => f64::NAN,
    };
    put_num(f, args[2], v);
    Ok(())
}

/// Write `v` (NaN = na) to output `e` in its lane (an `int` output holds an exact difference).
#[inline(always)]
fn put_num(f: &mut Frame<'_>, e: u16, v: f64) {
    if argwin::is_i64(e) {
        f.set_i64_arg(e, if v.is_nan() { None } else { Some(v as i64) });
    } else {
        f.set_f64_arg(e, v);
    }
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
