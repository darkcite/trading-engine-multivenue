// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` windows (impl-04 §4.13.6): `ta.variance`, `ta.stdev`.

use crate::args::out1;
use crate::state::{mono_push, mono_query, win_push, win_repoint, MonoCell, MonoHdr, WinHdr};
use pine_ir::{abi, argwin, FaultKind, SiteId};
use pine_vm::Frame;

/// Window variance over `[src, len, biased, out]`: `sum2/n − mean²`, clamped at 0 before any
/// `sqrt` (§4.5 item 3); `biased == false` scales by `n/(n−1)`. `skip` na (W5/U18). An na
/// `biased` (v5) yields na.
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
        f.set_f64_arg(dst, f64::NAN);
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
    let out = match r {
        None => f64::NAN,
        Some((v, back)) => {
            if BARS {
                -(back as f64)
            } else {
                v
            }
        }
    };
    f.set_f64_arg(dst, out);
    Ok(())
}

/// `ta.bb(series, length, mult) -> (middle, upper, lower)` — window `[src, len, mult, mid, up,
/// lo]`: `sma ± mult × stdev` (population) from one set of sums, variance clamped at 0; `skip` na.
pub fn bi_ta_bb(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 6 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let len = f.i64_arg(args[1]);
    let mult = f.f64_arg(args[2]);
    let (mid, sd) = match len {
        None => (f64::NAN, f64::NAN),
        Some(len) => {
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
                (f64::NAN, f64::NAN)
            } else {
                let n = len as f64;
                let m = h.sum / n;
                let v = h.sum2 / n - m * m;
                (m, if v > 0.0 { v.sqrt() } else { 0.0 })
            }
        }
    };
    f.set_f64_arg(args[3], mid);
    f.set_f64_arg(args[4], mult.mul_add(sd, mid));
    f.set_f64_arg(args[5], (-mult).mul_add(sd, mid));
    Ok(())
}
