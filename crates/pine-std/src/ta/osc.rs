// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` oscillators, bands, trend (impl-04 §4.13.7): `ta.tr`, `ta.atr`, `ta.rsi`, `ta.macd`,
//! `ta.kc`, `ta.kcw`, `ta.supertrend`, `ta.sar`, `ta.cmo`, `ta.mfi`, `ta.tsi`, `ta.dmi`.

use crate::args::out1;
use crate::naf::{fdiv, fmax, fmin};
use crate::state::{
    ewma_hold, ewma_step, winp_push, winp_repoint, AtrHdr, CmoHdr, DmiHdr, Ewma, MfiHdr, Prev,
    TsiHdr,
};
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
/// documented non-handling behaviour (`pin:04-handle-na`).
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

const KC_BASIS: [u8; 2] = sw::both(Sw::KcBasis);

/// The Keltner step shared by `ta.kc` and `ta.kcw`: `(mid, up, lo)` over
/// `[src, len, mult, useTrueRange, …]`; `skip` na holds the value.
fn kc(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(f64, f64, f64), FaultKind> {
    let src = f.f64_arg(args[0]);
    let mult = f.f64_arg(args[2]);
    let use_tr = f.bool_arg(args[3]) == Some(true);
    let Some(len) = length(f, args[1])? else {
        return Ok((f64::NAN, f64::NAN, f64::NAN));
    };
    let d = f.dialect();
    let ema_sma = sw::pick(EMA_SEED, d) == swr::EMA_SEED_SMA;
    let rma_band = sw::pick(KC_BASIS, d) == swr::KC_BASIS_RMA_RANGE;
    let band_sma = if rma_band {
        sw::pick(RMA_SEED, d) == swr::RMA_SEED_SMA
    } else {
        ema_sma
    };
    let (h, l, c) = hlc(f);
    let (mid, band) = if src.is_nan() {
        let s = f.state_ro::<crate::state::KcHdr>(site);
        (ewma_hold(&s.mid), ewma_hold(&s.band))
    } else {
        let s = f.state::<crate::state::KcHdr>(site);
        let a = 2.0 / (len as f64 + 1.0);
        let mid = ewma_step(&mut s.mid, src, a, len, ema_sma);
        let span = if use_tr {
            tr_step(&mut s.prev, h, l, c, false)
        } else {
            h - l
        };
        let ab = if rma_band { 1.0 / len as f64 } else { a };
        let band = if span.is_nan() {
            ewma_hold(&s.band)
        } else {
            ewma_step(&mut s.band, span, ab, len, band_sma)
        };
        (mid, band)
    };
    Ok((mid, mult.mul_add(band, mid), (-mult).mul_add(band, mid)))
}

/// `ta.kc(series, length, mult, useTrueRange) -> (middle, upper, lower)`; the band per
/// `Sw::KcBasis` (`pin:U02-kc-basis`).
pub fn bi_ta_kc(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 7 {
        return Err(FaultKind::Internal);
    }
    let (m, u, l) = kc(f, site, args)?;
    f.set_f64_arg(args[4], m);
    f.set_f64_arg(args[5], u);
    f.set_f64_arg(args[6], l);
    Ok(())
}

/// `ta.kcw(series, length, mult, useTrueRange)` = `(upper − lower) / middle`.
pub fn bi_ta_kcw(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let (m, u, l) = kc(f, site, args)?;
    f.set_f64_arg(args[4], (u - l) / m);
    Ok(())
}

/// `ta.supertrend(factor, atrPeriod) -> (supertrend, direction)` — the reference manual's
/// recurrence: bands `hl2 ± factor × atr` clamped against the previous final bands (`nz`) and
/// the previous close; direction 1 while `na(atr[1])`, then flips on a close through the held
/// band; −1 selects the lower band (`pin:04-ta-reference`).
pub fn bi_ta_supertrend(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let factor = f.f64_arg(args[0]);
    let Some(len) = length(f, args[1])? else {
        f.set_f64_arg(args[2], f64::NAN);
        crate::args::set_num_out(f, args[3], f64::NAN);
        return Ok(());
    };
    let sma = sw::pick(RMA_SEED, f.dialect()) == swr::RMA_SEED_SMA;
    let (h, l, c) = hlc(f);
    let (st, dir) = {
        let s = f.state::<crate::state::StHdr>(site);
        let atr_was_na = s.atr.ew.seeded == 0;
        let tr = tr_step(&mut s.atr.prev, h, l, c, true);
        let atr = if tr.is_nan() {
            ewma_hold(&s.atr.ew)
        } else {
            ewma_step(&mut s.atr.ew, tr, 1.0 / len as f64, len, sma)
        };
        let src = (h + l) / 2.0;
        let (mut up, mut dn) = (factor.mul_add(atr, src), (-factor).mul_add(atr, src));
        let (pu, pd) = (s.up, s.dn); // nz(): a stored na is kept as 0
        let c1 = if s.primed != 0 { s.close1 } else { f64::NAN };
        if !(dn > pd || c1 < pd) {
            dn = pd;
        }
        if !(up < pu || c1 > pu) {
            up = pu;
        }
        let dir = if atr_was_na {
            1
        } else if s.dir == 1 {
            if c > up {
                -1
            } else {
                1
            }
        } else if c < dn {
            1
        } else {
            -1
        };
        s.up = if up.is_nan() { 0.0 } else { up };
        s.dn = if dn.is_nan() { 0.0 } else { dn };
        s.close1 = c;
        s.dir = dir;
        s.primed = 1;
        (if dir == -1 { dn } else { up }, dir)
    };
    f.set_f64_arg(args[2], st);
    crate::args::set_num_out(f, args[3], dir as f64);
    Ok(())
}

/// `ta.sar(start, inc, max)` — window `[start, inc, max, out]`: the reference manual's
/// `pine_sar`, ported literally (`pin:04-sar-literal`). `bar_index` reads as the site's own
/// execution index and `high[k]`/`low[k]`/`close[1]` as the values at the site's `k`-th previous
/// execution (the implicit-history reading of `ta.tr`, `pin:04-ta-reference`). Pine's `na`
/// comparisons are `false` (NaN compares false) and `math.max`/`math.min` propagate `na`
/// (`naf`); the arithmetic is unfused, as written.
pub fn bi_ta_sar(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let (start, inc, max) = (f.f64_arg(args[0]), f.f64_arg(args[1]), f.f64_arg(args[2]));
    let (h, l, c) = hlc(f);
    let out = {
        let s = f.state::<crate::state::SarHdr>(site);
        let bar_index = s.n;
        if bar_index == 0 {
            // Bar 0 of `pine_sar`: every `var` is na, so `result` stays na and no branch fires.
            s.result = f64::NAN;
            s.accel = f64::NAN;
            s.max_min = c; // `close[1]` for the next execution; `maxMin` is na here.
            s.is_below = 0;
            s.high1 = h;
            s.low1 = l;
            s.n = 1;
            f64::NAN
        } else {
            let (mut result, mut max_min, mut accel) = (s.result, s.max_min, s.accel);
            let mut is_below = s.is_below != 0;
            let mut first = false;
            if bar_index == 1 {
                let close1 = s.max_min;
                if c > close1 {
                    is_below = true;
                    max_min = h;
                    result = s.low1;
                } else {
                    is_below = false;
                    max_min = l;
                    result = s.high1;
                }
                first = true;
                accel = start;
            }
            result += accel * (max_min - result);
            if is_below {
                if result > l {
                    first = true;
                    is_below = false;
                    result = fmax(h, max_min);
                    max_min = l;
                    accel = start;
                }
            } else if result < h {
                first = true;
                is_below = true;
                result = fmin(l, max_min);
                max_min = h;
                accel = start;
            }
            if !first {
                if is_below {
                    if h > max_min {
                        max_min = h;
                        accel = fmin(accel + inc, max);
                    }
                } else if l < max_min {
                    max_min = l;
                    accel = fmin(accel + inc, max);
                }
            }
            if is_below {
                result = fmin(result, s.low1);
                if bar_index > 1 {
                    result = fmin(result, s.low2);
                }
            } else {
                result = fmax(result, s.high1);
                if bar_index > 1 {
                    result = fmax(result, s.high2);
                }
            }
            s.result = result;
            s.max_min = max_min;
            s.accel = accel;
            s.is_below = is_below as u32;
            s.high2 = s.high1;
            s.high1 = h;
            s.low2 = s.low1;
            s.low1 = l;
            s.n = bar_index.saturating_add(1);
            result
        }
    };
    f.set_f64_arg(args[3], out);
    Ok(())
}

const DIV0: [u8; 2] = sw::both(Sw::FloatDivZero);

/// One step of a `MfiHdr` window over `ta.change(src)` with `skip` na: `accept` is false for an
/// execution the row does not accept (the window is unchanged); otherwise `pair(src, change)`
/// gives the `(a, b)` it enters. The first accepted execution only sets `prev` — its change is na
/// and the spec's `W(len + 1)` excludes it (`pin:04-first-change`). Returns `(Σa, Σb)`, `None`
/// while fewer than `len` pairs were accepted.
#[inline(always)]
fn change_pairs(
    h: &mut MfiHdr,
    ring: &mut [[f64; 2]],
    len: u32,
    src: f64,
    accept: bool,
    pair: impl Fn(f64, f64) -> (f64, f64),
) -> Option<(f64, f64)> {
    let primed = h.win.len != 0;
    if !primed {
        if !accept {
            return None;
        }
        h.prev = src;
        winp_repoint(&mut h.win, ring, len);
        return None;
    }
    if h.win.len != len {
        winp_repoint(&mut h.win, ring, len);
    }
    if accept {
        let (a, b) = pair(src, src - h.prev);
        h.prev = src;
        // pin:04-change-pair-nan — a non-na source can still give an na pair (`+inf − +inf` for
        // the change, `0 × inf` for `ta.mfi`'s volume weight): the row's `skip` rule (§4.6) then
        // applies to the pair — not accepted, the window unchanged — while the accepted source
        // still becomes `prev` (fuzz r21 seed 692: `ta.cmo(math.exp(syni), 12)`).
        if !a.is_nan() && !b.is_nan() {
            winp_push(&mut h.win, ring, a, b);
        }
    }
    if h.win.n < len {
        None
    } else {
        Some((h.win.sum, h.win.sum2))
    }
}

/// The `[src, len, out]` frame of `ta.cmo`/`ta.mfi`: the window length, or `None` (na length).
#[inline(always)]
fn pair_len(f: &Frame<'_>, args: &[u16]) -> Result<Option<i64>, FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    Ok(f.i64_arg(args[1]))
}

/// `ta.cmo(series, length)` — the manual's `f_cmo`: `mom = ta.change(src)`,
/// `sm1 = math.sum(mom >= 0 ? mom : 0, length)`, `sm2 = math.sum(mom >= 0 ? 0 : −mom, length)`,
/// `100 × (sm1 − sm2) / (sm1 + sm2)` (÷0 per `Sw::FloatDivZero`). `skip` na.
pub fn bi_ta_cmo(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let Some(len) = pair_len(f, args)? else {
        f.set_f64_arg(args[2], f64::NAN);
        return Ok(());
    };
    let src = f.f64_arg(args[0]);
    let na0 = sw::pick(DIV0, f.dialect()) == swr::FLOAT_DIV_ZERO_NA;
    let r = {
        let (h, ring) = f.state_split::<CmoHdr, [f64; 2]>(site);
        if len < 1 || len > ring.len() as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        change_pairs(h, ring, len as u32, src, !src.is_nan(), |_, m| {
            if m >= 0.0 {
                (m, 0.0)
            } else {
                (0.0, -m)
            }
        })
    };
    let v = r.map_or(f64::NAN, |(s1, s2)| fdiv(100.0 * (s1 - s2), s1 + s2, na0));
    f.set_f64_arg(args[2], v);
    Ok(())
}

/// `ta.mfi(series, length)` — the manual's `pine_mfi`: `upper = Σ volume × (change ≤ 0 ? 0 :
/// src)`, `lower = Σ volume × (change ≥ 0 ? 0 : src)`, `100 − 100 / (1 + upper / lower)`, each `/`
/// per `Sw::FloatDivZero`. `skip` na over the `(source, volume)` pair: an execution is accepted
/// only when both are non-na (`pin:04-volume-pair-skip`).
pub fn bi_ta_mfi(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let Some(len) = pair_len(f, args)? else {
        f.set_f64_arg(args[2], f64::NAN);
        return Ok(());
    };
    let src = f.f64_arg(args[0]);
    let vol = f.f64_arg(argwin::f64(abi::F_VOLUME));
    let na0 = sw::pick(DIV0, f.dialect()) == swr::FLOAT_DIV_ZERO_NA;
    let r = {
        let (h, ring) = f.state_split::<MfiHdr, [f64; 2]>(site);
        if len < 1 || len > ring.len() as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let ok = !src.is_nan() && !vol.is_nan();
        change_pairs(h, ring, len as u32, src, ok, |x, ch| {
            (
                vol * if ch <= 0.0 { 0.0 } else { x },
                vol * if ch >= 0.0 { 0.0 } else { x },
            )
        })
    };
    let v = r.map_or(f64::NAN, |(up, lo)| {
        100.0 - fdiv(100.0, 1.0 + fdiv(up, lo, na0), na0)
    });
    f.set_f64_arg(args[2], v);
    Ok(())
}

/// `ta.tsi(source, short_length, long_length)` — window `[src, short, long, out]`: with
/// `m = ta.change(src)`, `ema(ema(m, long), short) / ema(ema(|m|, long), short)` — the long
/// smoothing first, as TradingView's TSI's `double_smooth`, and no `× 100` (the manual's range is
/// [−1, 1]) (`pin:04-tsi-order`); seeds per `Sw::EmaSeed`; ÷0 per `Sw::FloatDivZero`. `skip` na:
/// every smoother holds; the first accepted execution has no change.
pub fn bi_ta_tsi(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let (sl, ll) = (length(f, args[1])?, length(f, args[2])?);
    let (Some(sl), Some(ll)) = (sl, ll) else {
        f.set_f64_arg(args[3], f64::NAN);
        return Ok(());
    };
    let d = f.dialect();
    let sma = sw::pick(EMA_SEED, d) == swr::EMA_SEED_SMA;
    let na0 = sw::pick(DIV0, d) == swr::FLOAT_DIV_ZERO_NA;
    let (num, den) = {
        let s = f.state::<TsiHdr>(site);
        if !src.is_nan() && s.prev.primed != 0 {
            let m = src - s.prev.v;
            let (al, as_) = (2.0 / (ll as f64 + 1.0), 2.0 / (sl as f64 + 1.0));
            let m1 = ewma_step(&mut s.m1, m, al, ll, sma);
            if !m1.is_nan() {
                ewma_step(&mut s.m2, m1, as_, sl, sma);
            }
            let a1 = ewma_step(&mut s.a1, m.abs(), al, ll, sma);
            if !a1.is_nan() {
                ewma_step(&mut s.a2, a1, as_, sl, sma);
            }
        }
        if !src.is_nan() {
            s.prev.v = src;
            s.prev.primed = 1;
        }
        (ewma_hold(&s.m2), ewma_hold(&s.a2))
    };
    f.set_f64_arg(args[3], fdiv(num, den, na0));
    Ok(())
}

/// Feed a non-na `v` to a Wilder smoother (`rma` is `skip`: an na is not fed, the value holds).
#[inline(always)]
fn rma_feed(e: &mut Ewma, v: f64, len: u32, sma: bool) -> f64 {
    if v.is_nan() {
        ewma_hold(e)
    } else {
        ewma_step(e, v, 1.0 / len as f64, len, sma)
    }
}

/// `ta.dmi(diLength, adxSmoothing) -> (+DI, −DI, ADX)` — window `[diLen, adxLen, +di, −di,
/// adx]`, the §4.13.7 row literally: `up = high − high[1]`, `down = low[1] − low`;
/// `+DM = up > down && up > 0 ? up : 0`, `−DM = down > up && down > 0 ? down : 0`; `TR` = the
/// `ta.tr` variable; each smoothed by `rma(·, diLen)` (seed per `Sw::RmaSeed`);
/// `±DI = 100 × rma(±DM) / rma(TR)`; `DX = 100 × |+DI − −DI| / (+DI + −DI)`;
/// `ADX = rma(DX, adxLen)`; each `/` per `Sw::FloatDivZero`. History is the site's previous
/// execution; an na term is not fed to its `rma` (the inner rows' own rule, `pin:04-ta-composed`).
pub fn bi_ta_dmi(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 5 {
        return Err(FaultKind::Internal);
    }
    let (dl, al) = (length(f, args[0])?, length(f, args[1])?);
    let (Some(dl), Some(al)) = (dl, al) else {
        let mut k = 2usize;
        while k < 5 {
            f.set_f64_arg(args[k], f64::NAN);
            k += 1;
        }
        return Ok(());
    };
    let d = f.dialect();
    let sma = sw::pick(RMA_SEED, d) == swr::RMA_SEED_SMA;
    let na0 = sw::pick(DIV0, d) == swr::FLOAT_DIV_ZERO_NA;
    let (h, l, c) = hlc(f);
    let (pdi, mdi, adx) = {
        let s = f.state::<DmiHdr>(site);
        let (tr, pdm, mdm) = if s.primed != 0 {
            let up = h - s.h1;
            let down = s.l1 - l;
            let pdm = if up.is_nan() || down.is_nan() {
                f64::NAN
            } else if up > down && up > 0.0 {
                up
            } else {
                0.0
            };
            let mdm = if up.is_nan() || down.is_nan() {
                f64::NAN
            } else if down > up && down > 0.0 {
                down
            } else {
                0.0
            };
            let tr = fmax(fmax(h - l, (h - s.c1).abs()), (l - s.c1).abs());
            (tr, pdm, mdm)
        } else {
            (f64::NAN, f64::NAN, f64::NAN)
        };
        s.h1 = h;
        s.l1 = l;
        s.c1 = c;
        s.primed = 1;
        let t = rma_feed(&mut s.tr, tr, dl, sma);
        let p = rma_feed(&mut s.p, pdm, dl, sma);
        let m = rma_feed(&mut s.m, mdm, dl, sma);
        let pdi = fdiv(100.0 * p, t, na0);
        let mdi = fdiv(100.0 * m, t, na0);
        let dx = fdiv(100.0 * (pdi - mdi).abs(), pdi + mdi, na0);
        let adx = rma_feed(&mut s.adx, dx, al, sma);
        (pdi, mdi, adx)
    };
    f.set_f64_arg(args[2], pdi);
    f.set_f64_arg(args[3], mdi);
    f.set_f64_arg(args[4], adx);
    Ok(())
}
