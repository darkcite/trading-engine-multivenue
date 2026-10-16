// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The cumulative `ta.*` series (impl-04 §4.13.8): the `ta.vwap` function forms, the `ta.vwap`
//! variable, and the volume variables `ta.accdist obv pvt iii nvi pvi wad wvad`. The reference
//! (`ref-v6-vars.json`) names each in one line; seven of the eight carry a "the same on pine"
//! example, which is the definition served here, evaluated with this crate's `ta.cum` (an na
//! term contributes 0, §4.13.7) and `ta.change` (`close − close[1]`, na on the first bar).
//! `ta.accdist` has no example: TradingView's built-in "Accumulation/Distribution" study is used.
//! Operator ruling 2026-10-16: each carries `pin:04-<slug>-provisional`, to be verified against
//! TradingView in the capture month. `ta.wvad` and `ta.iii` are per-bar values (their examples
//! have no `ta.cum`); their 16-byte blocks only count executions. The `ta.vwap` variable's text
//! names its source ("It uses hlc3 as its source series").

use crate::naf::{fdiv, fmax, fmin};
use crate::state::{Cum, CumPrev, VwapHdr, VwapVar};
use crate::sw;
use pine_ir::{abi, argwin, FaultKind, SiteId};
use pine_spec::{swr, Sw};
use pine_vm::Frame;

const DIV0: [u8; 2] = sw::both(Sw::FloatDivZero);

/// One anchored step: reset the sums when `anchor`, accumulate `(src, vol)` unless either is na
/// (`skip`), and return `(vwap, sd)` — `Σ v·p / Σ v` and `√max(0, Σ v·p² / Σ v − vwap²)` — both
/// na before the first true anchor or while `Σ v == 0`.
#[inline(always)]
fn vwap_step(s: &mut VwapHdr, src: f64, vol: f64, anchor: bool) -> (f64, f64) {
    if anchor {
        s.sv = 0.0;
        s.spv = 0.0;
        s.sp2v = 0.0;
        s.started = 1;
    }
    if s.started == 0 {
        return (f64::NAN, f64::NAN);
    }
    if !src.is_nan() && !vol.is_nan() {
        s.sv += vol;
        s.spv += vol * src;
        s.sp2v += vol * src * src;
    }
    if s.sv == 0.0 {
        return (f64::NAN, f64::NAN);
    }
    let m = s.spv / s.sv;
    // The cancellation edge clamps at zero, as `ta.variance` does (pin:04-ta-reference).
    let var = s.sp2v / s.sv - m * m;
    (m, if var > 0.0 { var.sqrt() } else { 0.0 })
}

/// The window's `(source, anchor)` and the bar's volume; an na anchor (v5) reads as `false`
/// (`pin:04-handle-na`).
#[inline(always)]
fn inputs(f: &Frame<'_>, args: &[u16]) -> (f64, f64, bool) {
    (
        f.f64_arg(args[0]),
        f.f64_arg(argwin::f64(abi::F_VOLUME)),
        f.bool_arg(args[1]) == Some(true),
    )
}

/// `ta.vwap(source, anchor)` — window `[source, anchor, out]`, block `VwapHdr`.
pub fn bi_ta_vwap(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (src, vol, anchor) = inputs(f, args);
    let (m, _) = vwap_step(f.state::<VwapHdr>(site), src, vol, anchor);
    f.set_f64_arg(args[2], m);
    Ok(())
}

/// `ta.vwap(source, anchor, stdev_mult)` → `[vwap, upper, lower]` — window
/// `[source, anchor, stdev_mult, vwap, upper, lower]`, bands `vwap ± stdev_mult · sd`.
pub fn bi_ta_vwap_bands(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let (src, vol, anchor) = inputs(f, args);
    let mult = f.f64_arg(args[2]);
    let (m, sd) = vwap_step(f.state::<VwapHdr>(site), src, vol, anchor);
    f.set_f64_arg(args[3], m);
    f.set_f64_arg(args[4], mult.mul_add(sd, m));
    f.set_f64_arg(args[5], (-mult).mul_add(sd, m));
    Ok(())
}

const DAY_MS: i64 = 86_400_000;

/// The `ta.vwap` variable — window `[out]`, block `VwapVar`: the volume-weighted average of
/// `hlc3` (the reference: "It uses hlc3 as its source series") since the bar's UTC day began
/// (§4.13.8, `pin:04-exchange-tz`). A sema prelude call advances it on every bar (§1.4 item 6).
/// An na `hlc3` or volume adds nothing and the bar's value is na (`na: propagate`,
/// `pin:04-cum-na`); na while `Σ volume == 0`.
pub fn bi_ta_vwap_var(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let src = (f.f64_arg(argwin::f64(abi::F_HIGH))
        + f.f64_arg(argwin::f64(abi::F_LOW))
        + f.f64_arg(argwin::f64(abi::F_CLOSE)))
        / 3.0;
    let vol = f.f64_arg(argwin::f64(abi::F_VOLUME));
    let day = f
        .i64_arg(argwin::i64(abi::I_TIME))
        .unwrap_or(0)
        .div_euclid(DAY_MS);
    let s = f.state::<VwapVar>(site);
    if s.started == 0 || s.day != day {
        s.sv = 0.0;
        s.spv = 0.0;
        s.day = day;
        s.started = 1;
    }
    let v = if src.is_nan() || vol.is_nan() {
        f64::NAN
    } else {
        s.sv += vol;
        s.spv += vol * src;
        if s.sv == 0.0 {
            f64::NAN
        } else {
            s.spv / s.sv
        }
    };
    f.set_f64_arg(args[0], v);
    Ok(())
}

/// The bar's `[open, high, low, close, volume]` from the reserved slots (F7).
#[inline(always)]
fn ohlcv(f: &Frame<'_>) -> [f64; 5] {
    [
        f.f64_arg(argwin::f64(abi::F_OPEN)),
        f.f64_arg(argwin::f64(abi::F_HIGH)),
        f.f64_arg(argwin::f64(abi::F_LOW)),
        f.f64_arg(argwin::f64(abi::F_CLOSE)),
        f.f64_arg(argwin::f64(abi::F_VOLUME)),
    ]
}

/// `true` when a body's own `/` by data turns a non-finite quotient into na (`Sw::FloatDivZero`).
#[inline(always)]
fn div_na(f: &Frame<'_>) -> bool {
    sw::pick(DIV0, f.dialect()) == swr::FLOAT_DIV_ZERO_NA
}

/// `nz(x, 0.0)`.
#[inline(always)]
fn nz0(x: f64) -> f64 {
    if x.is_nan() {
        0.0
    } else {
        x
    }
}

/// The per-bar term of a [`CumPrev`] variable from the bar and the previous bar's `(close,
/// volume)` (both na on the first bar). `obv pvt wad` return the term `ta.cum` adds; `nvi pvi`
/// return the new index from `acc` (its previous value).
type PrevTerm = fn(&[f64; 5], f64, f64, f64, bool) -> f64;

/// One step of a [`CumPrev`] variable: `ACC` sums the term as `ta.cum` does (an na term adds 0),
/// otherwise the term replaces `acc` (the index forms). Window `[out]`; a sema prelude call
/// advances it on every bar (§1.4 item 6), so the site's previous execution is the previous bar.
#[inline(always)]
fn prev_step<const ACC: bool>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
    term: PrevTerm,
) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let b = ohlcv(f);
    let na0 = div_na(f);
    let v = {
        let s = f.state::<CumPrev>(site);
        let (pc, pv) = if s.n == 0 {
            (f64::NAN, f64::NAN)
        } else {
            (s.pc, s.pv)
        };
        let t = term(&b, pc, pv, s.acc, na0);
        if ACC {
            if !t.is_nan() {
                s.acc += t;
            }
        } else {
            s.acc = t;
        }
        s.pc = b[3];
        s.pv = b[4];
        s.n = s.n.saturating_add(1);
        s.acc
    };
    f.set_f64_arg(args[0], v);
    Ok(())
}

/// OBV: `math.sign(ta.change(close)) * volume` (the reference example, `pin:04-obv-provisional`).
fn obv_term(b: &[f64; 5], pc: f64, _: f64, _: f64, _: bool) -> f64 {
    let d = b[3] - pc;
    let sg = if d > 0.0 {
        1.0
    } else if d < 0.0 {
        -1.0
    } else if d == 0.0 {
        0.0
    } else {
        f64::NAN
    };
    sg * b[4]
}

/// PVT: `(ta.change(close) / close[1]) * volume` (the reference example,
/// `pin:04-pvt-provisional`); `/` per `Sw::FloatDivZero`.
fn pvt_term(b: &[f64; 5], pc: f64, _: f64, _: f64, na0: bool) -> f64 {
    fdiv(b[3] - pc, pc, na0) * b[4]
}

/// WAD: `trueHigh = math.max(high, close[1])`, `trueLow = math.min(low, close[1])`,
/// `mom = ta.change(close)`, `mom > 0 ? close − trueLow : mom < 0 ? close − trueHigh : 0`
/// (the reference example, `pin:04-wad-provisional`). An na `mom` compares false → 0.
fn wad_term(b: &[f64; 5], pc: f64, _: f64, _: f64, _: bool) -> f64 {
    let mom = b[3] - pc;
    if mom > 0.0 {
        b[3] - fmin(b[2], pc)
    } else if mom < 0.0 {
        b[3] - fmax(b[1], pc)
    } else {
        0.0
    }
}

/// NVI / PVI, the reference example verbatim (`pin:04-nvi-provisional`,
/// `pin:04-pvi-provisional`): `prev = nz(idx[1]) == 0 ? 1.0 : idx[1]` (so the index starts at
/// 1.0); unchanged when `nz(close) == 0` or `nz(close[1]) == 0`; else, when `volume < nz(volume[1])`
/// (NVI) / `volume > nz(volume[1])` (PVI), `prev + (close − close[1]) / close[1] · prev`.
/// An na `volume` compares false.
#[inline(always)]
fn vi_term<const POS: bool>(b: &[f64; 5], pc: f64, pv: f64, acc: f64, _: bool) -> f64 {
    let prev = if nz0(acc) == 0.0 { 1.0 } else { acc };
    if nz0(b[3]) == 0.0 || nz0(pc) == 0.0 {
        return prev;
    }
    let pv0 = nz0(pv);
    let moved = if POS { b[4] > pv0 } else { b[4] < pv0 };
    if moved {
        prev + (b[3] - pc) / pc * prev
    } else {
        prev
    }
}

/// The `ta.obv` variable — window `[out]`, block `CumPrev`.
pub fn bi_ta_obv(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    prev_step::<true>(f, site, args, obv_term)
}

/// The `ta.pvt` variable — window `[out]`, block `CumPrev`.
pub fn bi_ta_pvt(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    prev_step::<true>(f, site, args, pvt_term)
}

/// The `ta.wad` variable — window `[out]`, block `CumPrev`.
pub fn bi_ta_wad(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    prev_step::<true>(f, site, args, wad_term)
}

/// The `ta.nvi` variable — window `[out]`, block `CumPrev`.
pub fn bi_ta_nvi(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    prev_step::<false>(f, site, args, vi_term::<false>)
}

/// The `ta.pvi` variable — window `[out]`, block `CumPrev`.
pub fn bi_ta_pvi(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    prev_step::<false>(f, site, args, vi_term::<true>)
}

/// The `ta.accdist` variable — window `[out]`, block `Cum`. No reference example; TradingView's
/// built-in Accumulation/Distribution study (Chaikin's A/D line):
/// `ta.cum(close == high and close == low or high == low ? 0 :
/// ((2·close − low − high) / (high − low)) · volume)` (`pin:04-accdist-provisional`). An na term
/// adds 0, as `ta.cum`.
pub fn bi_ta_accdist(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let [_, h, l, c, v] = ohlcv(f);
    let t = if (c == h && c == l) || h == l {
        0.0
    } else {
        (2.0 * c - l - h) / (h - l) * v
    };
    let acc = {
        let s = f.state::<Cum>(site);
        if !t.is_nan() {
            s.acc += t;
        }
        s.n = s.n.saturating_add(1);
        s.acc
    };
    f.set_f64_arg(args[0], acc);
    Ok(())
}

/// The `ta.wvad` variable — window `[out]`, block `Cum` (executions only): the per-bar
/// `(close − open) / (high − low) · volume` (the reference example, `pin:04-wvad-provisional`);
/// `/` per `Sw::FloatDivZero`; na inputs give na.
pub fn bi_ta_wvad(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let [o, h, l, c, v] = ohlcv(f);
    let na0 = div_na(f);
    let s = f.state::<Cum>(site);
    s.n = s.n.saturating_add(1);
    f.set_f64_arg(args[0], fdiv(c - o, h - l, na0) * v);
    Ok(())
}

/// The `ta.iii` variable — window `[out]`, block `Cum` (executions only): the per-bar
/// `((2·close − high − low) / (high − low)) · volume` (the reference example,
/// `pin:04-iii-provisional`; it multiplies by volume, Bollinger's Intraday Intensity, not the
/// `/ volume` normalised form); `/` per `Sw::FloatDivZero`; na inputs give na.
pub fn bi_ta_iii(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    let [_, h, l, c, v] = ohlcv(f);
    let na0 = div_na(f);
    let s = f.state::<Cum>(site);
    s.n = s.n.saturating_add(1);
    f.set_f64_arg(args[0], fdiv(2.0 * c - h - l, h - l, na0) * v);
    Ok(())
}
