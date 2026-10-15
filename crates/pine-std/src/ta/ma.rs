// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` moving averages (impl-04 §4.13.5): `ta.sma`, `ta.ema`, `ta.rma`, `ta.wma`, `ta.swma`.

use crate::args::out1;
use crate::state::{ewma_hold, ewma_step, win_at, win_push, win_repoint, Ewma, WinHdr};
use crate::sw;
use pine_ir::{FaultKind, SiteId};
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
