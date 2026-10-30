// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The bar's prelude series `hl2`, `hlc3`, `ohlc4`, `hlcc4` (`ValueSrc::Call` values, impl-00
//! §3.11; sema emits each referenced one once per bar in the prelude). Each is the reference's
//! literal definition over the reserved price slots (F7), summed left to right as written:
//! `(high + low)/2`, `(high + low + close)/3`, `(open + high + low + close)/4`,
//! `(high + low + close + close)/4`. A na slot propagates (IEEE: NaN in, NaN out).

use pine_ir::{abi, argwin, FaultKind, SiteId};
use pine_vm::Frame;

/// The window's only entry, the output (a longer window is a sema defect: `Internal`).
#[inline(always)]
fn out(args: &[u16]) -> Result<u16, FaultKind> {
    match *args {
        [e] => Ok(e),
        _ => Err(FaultKind::Internal),
    }
}

#[inline(always)]
fn px(f: &Frame<'_>, slot: u16) -> f64 {
    f.f64_arg(argwin::f64(slot))
}

/// `hl2` — window `[out]`.
pub fn bi_hl2(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let dst = out(args)?;
    let v = (px(f, abi::F_HIGH) + px(f, abi::F_LOW)) / 2.0;
    f.set_f64_arg(dst, v);
    Ok(())
}

/// `hlc3` — window `[out]`.
pub fn bi_hlc3(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let dst = out(args)?;
    let v = (px(f, abi::F_HIGH) + px(f, abi::F_LOW) + px(f, abi::F_CLOSE)) / 3.0;
    f.set_f64_arg(dst, v);
    Ok(())
}

/// `ohlc4` — window `[out]`.
pub fn bi_ohlc4(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let dst = out(args)?;
    let v =
        (px(f, abi::F_OPEN) + px(f, abi::F_HIGH) + px(f, abi::F_LOW) + px(f, abi::F_CLOSE)) / 4.0;
    f.set_f64_arg(dst, v);
    Ok(())
}

/// `hlcc4` — window `[out]`.
pub fn bi_hlcc4(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let dst = out(args)?;
    let c = px(f, abi::F_CLOSE);
    let v = (px(f, abi::F_HIGH) + px(f, abi::F_LOW) + c + c) / 4.0;
    f.set_f64_arg(dst, v);
    Ok(())
}
