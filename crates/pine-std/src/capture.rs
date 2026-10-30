// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Capturing no-ops (impl-04 §4.10, T27, I30): `plot*`, `hline`, `bgcolor`, `barcolor`, `fill`,
//! `alert*`, `log.*`. Their arguments already ran (sema keeps them, `bf::CAPTURE`); a body only
//! decides what is kept: one `plot_set` per index it owns, every evaluation.
//!
//! Window (§3.4): the synthetic `__plot` (a constant `int`, the site's `PlotDesc::index`) leads,
//! then the Pine parameters, then the outputs (`plot` and `hline` return their handle: the
//! ordinal, i64 lane, amendment A4.2).

use pine_ir::{argwin, FaultKind, SiteId};
use pine_vm::Frame;

/// `__plot`: entry 0 of every plotting window. A na or negative index is a sema defect.
#[inline(always)]
fn plot_index(f: &Frame<'_>, args: &[u16], need: usize) -> Result<u32, FaultKind> {
    if args.len() < need {
        return Err(FaultKind::Internal);
    }
    match f.i64_arg(args[0]) {
        Some(i) if (0..=u32::MAX as i64).contains(&i) => Ok(i as u32),
        _ => Err(FaultKind::Internal),
    }
}

/// A `series: T` input as the captured f64. f64 lane: the value (NaN = na). i64 lane (`bool`,
/// `int`): read as a bool, 0.0 / 1.0, na (v5 only, W13) → NaN — `plotshape` / `plotchar` treat
/// their series as boolean (TradingView reference, `plotshape` `series`), and the window carries
/// no `Ty` to tell an `int` from a `bool` (`pin:04-plot-t-lane`).
#[inline(always)]
fn series_value(f: &Frame<'_>, e: u16) -> f64 {
    if argwin::is_i64(e) {
        match f.bool_arg(e) {
            Some(b) => b as u8 as f64,
            None => f64::NAN,
        }
    } else {
        plotted(f.f64_arg(e))
    }
}

/// A plotted float: ±inf plots as `na` (CAR-R33-6 R-na, r39 — TradingView F020 F024 F025 F121).
#[inline(always)]
fn plotted(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        f64::NAN
    }
}

/// `plot(series, …)` → `plot_set(__plot, series)`; the handle output is the ordinal.
pub fn bi_plot(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let p = plot_index(f, args, 3)?;
    let v = plotted(f.f64_arg(args[1]));
    f.plot_set(p, v);
    f.set_i64_arg(args[args.len() - 1], Some(p as i64));
    Ok(())
}

/// `hline(price, …)` → `plot_set(__plot, price)`; the handle output is the ordinal.
pub fn bi_hline(f: &mut Frame<'_>, s: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    bi_plot(f, s, args)
}

/// `plotarrow(series, …)`: `plot_set(__plot, series)`; no output.
pub fn bi_plot_series(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let p = plot_index(f, args, 2)?;
    let v = plotted(f.f64_arg(args[1]));
    f.plot_set(p, v);
    Ok(())
}

/// `plotshape(series, …)`, `plotchar(series, …)`: the float or the bool reading of `series`.
pub fn bi_plot_shape(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let p = plot_index(f, args, 2)?;
    let v = series_value(f, args[1]);
    f.plot_set(p, v);
    Ok(())
}

/// `plotcandle(o, h, l, c, …)`, `plotbar(o, h, l, c, …)`: four consecutive indices.
pub fn bi_plot_ohlc(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let p = plot_index(f, args, 5)?;
    let mut k = 0u32;
    while k < 4 {
        let v = plotted(f.f64_arg(args[1 + k as usize]));
        f.plot_set(p + k, v);
        k += 1;
    }
    Ok(())
}

/// `bgcolor(color, …)`, `barcolor(color, …)`: the packed colour as f64 (exact, `< 2^32`), NaN
/// when na.
pub fn bi_plot_color(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let p = plot_index(f, args, 2)?;
    let v = match f.i64_arg(args[1]) {
        Some(c) => c as f64,
        None => f64::NAN,
    };
    f.plot_set(p, v);
    Ok(())
}

/// `fill(…)`, `alert`, `alertcondition`, `log.*`: nothing — `fill`'s index is reserved for
/// accounting only (`PlotKind::Fill`), and the others' arguments already ran.
pub fn bi_capture_nothing(_: &mut Frame<'_>, _: SiteId, _: &[u16]) -> Result<(), FaultKind> {
    Ok(())
}
