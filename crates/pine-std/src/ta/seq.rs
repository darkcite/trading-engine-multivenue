// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` over the site's own value sequence (impl-04 §4.13.6–§4.13.7): `ta.crossover`,
//! `ta.crossunder`, `ta.cross`, `ta.change`, `ta.mom`.

use crate::args::out1;
use crate::state::{seq_push_back, Prev, Seq};
use pine_ir::{FaultKind, SiteId, NA_I64};
use pine_vm::Frame;

const OVER: u8 = 1;
const UNDER: u8 = 2;

/// The cross family over `[a, b, out]` on `d = a − b`: over `prev ≤ 0 && d > 0`, under
/// `prev ≥ 0 && d < 0`; `false` on the first execution and whenever either side is na.
pub fn bi_cross<const DIR: u8>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let d = f.f64_arg(args[0]) - f.f64_arg(args[1]);
    let hit = {
        let p = f.state::<Prev>(site);
        let (prev, primed) = (p.v, p.primed != 0);
        p.v = d;
        p.primed = 1;
        // NaN compares false on either side, so an na input yields `false`.
        primed
            && ((DIR & OVER != 0 && prev <= 0.0 && d > 0.0)
                || (DIR & UNDER != 0 && prev >= 0.0 && d < 0.0))
    };
    f.set_bool_arg(out1(args)?, hit);
    Ok(())
}

/// The ring and the lag of a `[src, len, out]` sequence call; `None` for an na length.
#[inline(always)]
fn lag(f: &Frame<'_>, args: &[u16]) -> Result<Option<u32>, FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    Ok(match f.i64_arg(args[1]) {
        None => None,
        Some(l) if (0..u32::MAX as i64).contains(&l) => Some(l as u32),
        Some(_) => return Err(FaultKind::ScriptRefused),
    })
}

/// `ta.change(float, len)` / `ta.mom(src, len)`: `src − src[len]` over the site's ring;
/// `propagate` na (an na enters the ring).
pub fn bi_change_f(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let src = f.f64_arg(args[0]);
    let dst = out1(args)?;
    let Some(k) = lag(f, args)? else {
        f.set_f64_arg(dst, f64::NAN);
        return Ok(());
    };
    let old = {
        let (h, ring) = f.state_split::<Seq, f64>(site);
        if k as usize >= ring.len() {
            return Err(FaultKind::ScriptRefused);
        }
        seq_push_back(h, ring, src, k)
    };
    f.set_f64_arg(dst, old.map_or(f64::NAN, |o| src - o));
    Ok(())
}

/// `ta.change(int, len)`: an `int` result; na while warming up or with an na on either side.
pub fn bi_change_i(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let src = f.i64_arg(args[0]);
    let dst = out1(args)?;
    let Some(k) = lag(f, args)? else {
        f.set_i64_arg(dst, None);
        return Ok(());
    };
    let old = {
        let (h, ring) = f.state_split::<Seq, i64>(site);
        if k as usize >= ring.len() {
            return Err(FaultKind::ScriptRefused);
        }
        seq_push_back(h, ring, src.unwrap_or(NA_I64), k)
    };
    let r = match (src, old) {
        (Some(a), Some(b)) if b != NA_I64 => a.checked_sub(b),
        _ => None,
    };
    f.set_i64_arg(dst, r);
    Ok(())
}

/// `ta.change(bool, len)`: `src != src[len]`; `false` while warming up or on an na side.
pub fn bi_change_b(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let src = f.bool_arg(args[0]);
    let dst = out1(args)?;
    let Some(k) = lag(f, args)? else {
        f.set_bool_arg(dst, false);
        return Ok(());
    };
    let code = match src {
        Some(b) => b as i64,
        None => NA_I64,
    };
    let old = {
        let (h, ring) = f.state_split::<Seq, i64>(site);
        if k as usize >= ring.len() {
            return Err(FaultKind::ScriptRefused);
        }
        seq_push_back(h, ring, code, k)
    };
    let r = matches!(old, Some(o) if o != NA_I64 && code != NA_I64 && o != code);
    f.set_bool_arg(dst, r);
    Ok(())
}
