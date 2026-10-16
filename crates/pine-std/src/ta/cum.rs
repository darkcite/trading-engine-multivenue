// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The cumulative `ta.*` series (impl-04 §4.13.8): the `ta.vwap` function forms. The variables
//! (`ta.accdist obv pvt iii nvi pvi wad wvad`, the `ta.vwap` variable) stay stubs: the captured
//! reference states no formula (nor the variable's source) for them —
//! `pin:04-cum-series-formula-open`.

use crate::state::VwapHdr;
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
