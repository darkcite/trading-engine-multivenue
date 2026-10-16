// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The cumulative `ta.*` series (impl-04 §4.13.8): the `ta.vwap` function forms and the `ta.vwap`
//! variable. The variables `ta.accdist obv pvt iii nvi pvi wad wvad` stay stubs: the reference
//! (`ref-v6-vars.json`) gives each only a name ("On Balance Volume." …) and no formula —
//! `pin:04-cum-series-formula-open`. The `ta.vwap` variable's text names its source ("It uses
//! hlc3 as its source series"), so it is served.

use crate::state::{VwapHdr, VwapVar};
use pine_ir::{abi, argwin, FaultKind, SiteId};
use pine_vm::Frame;

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
