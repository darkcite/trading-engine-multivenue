// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` over the site's own value sequence (impl-04 §4.13.6–§4.13.7): `ta.crossover`,
//! `ta.crossunder`, `ta.cross`, `ta.change`, `ta.mom`, `ta.roc`, `ta.cum`, `ta.barssince`,
//! `ta.valuewhen`, `ta.rising`, `ta.falling`, `ta.pivothigh`, `ta.pivotlow`.

use crate::args::out1;
use crate::state::{seq_bar_gap, seq_peek, seq_push_back, Cum, Prev, Seq};
use crate::sw;
use pine_ir::{abi, argwin, FaultKind, SiteId, NA_I64};
use pine_spec::{swr, Sw};
use pine_vm::{Frame, Pod};

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
/// `propagate` na (an na enters the ring). The ring is indexed by bar: the bars a conditional
/// call skipped carry the last value (`seq_bar_gap`, `pin:06-cond-call-state` — B041 bit-exact on
/// all 128 tail rows; a per-execution history misses all 128). `ta.mom` shares the body.
pub fn bi_change_f(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let src = f.f64_arg(args[0]);
    let dst = out1(args)?;
    let Some(k) = lag(f, args)? else {
        f.set_f64_arg(dst, f64::NAN);
        return Ok(());
    };
    let bar = f.bar_index();
    let old = {
        let (h, ring) = f.state_split::<Seq, f64>(site);
        if k as usize >= ring.len() {
            return Err(FaultKind::ScriptRefused);
        }
        seq_bar_gap(h, ring, bar);
        seq_push_back(h, ring, src, k)
    };
    f.set_f64_arg(dst, old.map_or(f64::NAN, |o| src - o));
    Ok(())
}

/// `ta.change(int, len)`: an `int` result; na while warming up or with an na on either side. The
/// ring is indexed by bar as `ta.change(float, len)`'s (`pin:06-cond-call-state`).
pub fn bi_change_i(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let src = f.i64_arg(args[0]);
    let dst = out1(args)?;
    let Some(k) = lag(f, args)? else {
        f.set_i64_arg(dst, None);
        return Ok(());
    };
    let bar = f.bar_index();
    let old = {
        let (h, ring) = f.state_split::<Seq, i64>(site);
        if k as usize >= ring.len() {
            return Err(FaultKind::ScriptRefused);
        }
        seq_bar_gap(h, ring, bar);
        seq_push_back(h, ring, src.unwrap_or(NA_I64), k)
    };
    let r = match (src, old) {
        (Some(a), Some(b)) if b != NA_I64 => a.checked_sub(b),
        _ => None,
    };
    f.set_i64_arg(dst, r);
    Ok(())
}

/// `ta.change(bool, len)`: `src != src[len]`; `false` while warming up or on an na side. The ring
/// is indexed by bar as `ta.change(float, len)`'s (`pin:06-cond-call-state`).
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
    let bar = f.bar_index();
    let old = {
        let (h, ring) = f.state_split::<Seq, i64>(site);
        if k as usize >= ring.len() {
            return Err(FaultKind::ScriptRefused);
        }
        seq_bar_gap(h, ring, bar);
        seq_push_back(h, ring, code, k)
    };
    let r = matches!(old, Some(o) if o != NA_I64 && code != NA_I64 && o != code);
    f.set_bool_arg(dst, r);
    Ok(())
}

const DIV0: [u8; 2] = sw::both(Sw::FloatDivZero);

/// `ta.roc(source, length)`: `100 × (src − src[len]) / src[len]` over the site's ring; a
/// non-finite quotient (÷0) is na under `Sw::FloatDivZero = na`, IEEE otherwise.
pub fn bi_roc(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let src = f.f64_arg(args[0]);
    let dst = out1(args)?;
    let Some(k) = lag(f, args)? else {
        f.set_f64_arg(dst, f64::NAN);
        return Ok(());
    };
    let na_nonfinite = sw::pick(DIV0, f.dialect()) == swr::FLOAT_DIV_ZERO_NA;
    let old = {
        let (h, ring) = f.state_split::<Seq, f64>(site);
        if k as usize >= ring.len() {
            return Err(FaultKind::ScriptRefused);
        }
        seq_push_back(h, ring, src, k)
    };
    let r = old.map_or(f64::NAN, |o| 100.0 * (src - o) / o);
    f.set_f64_arg(
        dst,
        if na_nonfinite && r.is_infinite() {
            f64::NAN
        } else {
            r
        },
    );
    Ok(())
}

/// `ta.cum(source)` — window `[src, out]`: the running total from the site's first execution;
/// an na contributes 0 (§4.13.7, `pin:04-ta-reference`).
pub fn bi_cum(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let acc = {
        let s = f.state::<Cum>(site);
        if !src.is_nan() {
            s.acc += src;
        }
        s.n = s.n.saturating_add(1);
        s.acc
    };
    f.set_f64_arg(args[1], acc);
    Ok(())
}

/// `ta.barssince(condition)` — window `[cond, out]`, block `Seq` (`n` = trues seen, `aux` = the
/// count): 0 when `cond`, else the previous count + 1; na until the first true. An na condition
/// (v5) reads as `false` (`pin:04-handle-na`).
pub fn bi_barssince(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let cond = f.bool_arg(args[0]) == Some(true);
    let r = {
        let s = f.state::<Seq>(site);
        if cond {
            s.aux = 0;
            s.n = s.n.saturating_add(1);
        } else if s.n > 0 {
            s.aux = s.aux.saturating_add(1);
        }
        if s.n == 0 {
            None
        } else {
            Some(s.aux as i64)
        }
    };
    f.set_i64_arg(args[1], r);
    Ok(())
}

/// The `ta.valuewhen` core over `[cond, src, occ, out]`: the ring is written only when `cond`;
/// the result is the value `occ` trues back (`None` until `occ + 1` trues, or for an na `occ`,
/// which leaves the block untouched). `occ < 0` or beyond the tail is `ScriptRefused`. An na
/// condition (v5) reads as `false` (`pin:04-handle-na`).
#[inline(always)]
fn valuewhen<T: Pod>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
    v: T,
) -> Result<Option<T>, FaultKind> {
    let cond = f.bool_arg(args[0]) == Some(true);
    let Some(occ) = f.i64_arg(args[2]) else {
        return Ok(None);
    };
    let (h, ring) = f.state_split::<Seq, T>(site);
    if occ < 0 || occ >= ring.len() as i64 {
        return Err(FaultKind::ScriptRefused);
    }
    let k = occ as u32;
    Ok(if cond {
        seq_push_back(h, ring, v, k)
    } else {
        seq_peek(h, ring, k)
    })
}

/// `ta.valuewhen(condition, float, occurrence)`.
pub fn bi_valuewhen_f(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let v = f.f64_arg(args[1]);
    let r = valuewhen(f, site, args, v)?;
    f.set_f64_arg(args[3], r.unwrap_or(f64::NAN));
    Ok(())
}

/// `ta.valuewhen(condition, int|color, occurrence)`: the i64 lane, na kept as `NA_I64`.
pub fn bi_valuewhen_i(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let v = f.i64_arg(args[1]).unwrap_or(NA_I64);
    let r = valuewhen(f, site, args, v)?;
    f.set_i64_arg(args[3], r.filter(|&x| x != NA_I64));
    Ok(())
}

/// `ta.valuewhen(condition, bool, occurrence)`: an na result is `false` (§4.2).
pub fn bi_valuewhen_b(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    let v = match f.bool_arg(args[1]) {
        Some(b) => b as i64,
        None => NA_I64,
    };
    let r = valuewhen(f, site, args, v)?;
    f.set_bool_arg(args[3], r == Some(1));
    Ok(())
}

const RF: [u8; 2] = sw::both(Sw::RisingFalling);

/// The `ta.rising` (`RISE`) / `ta.falling` test over the newest `len + 1` accepted values per
/// `Sw::RisingFalling` (`pin:U03-rising-falling`): `gt_all_prior` — `x₀` beyond each of
/// `x₁..=x_len`; `monotone` — `xᵢ` beyond `xᵢ₊₁` for every `i` in `0..len`. `false` while
/// fewer than `len + 1` values were accepted. `len + 1 ≤ ring.len()`.
pub fn rising_test<const RISE: bool>(h: &Seq, ring: &[f64], len: u32, reading: u8) -> bool {
    if h.n <= len {
        return false;
    }
    let mono = reading == swr::RISING_FALLING_MONOTONE;
    let pk = |k: u32| seq_peek(h, ring, k).unwrap_or(f64::NAN);
    let mut i = 1u32;
    while i <= len {
        let (a, b) = (if mono { pk(i - 1) } else { pk(0) }, pk(i));
        if !(if RISE { a > b } else { a < b }) {
            return false;
        }
        i += 1;
    }
    true
}

/// `ta.rising(source, length)` (`RISE`) / `ta.falling` — window `[src, len, out]`, block `Seq` +
/// a ring of `len + 1`: `skip` na (an na source is not accepted and the result is the current
/// window's).
pub fn bi_rising<const RISE: bool>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let reading = sw::pick(RF, f.dialect());
    let Some(len) = f.i64_arg(args[1]) else {
        f.set_bool_arg(args[2], false);
        return Ok(());
    };
    let r = {
        let (h, ring) = f.state_split::<Seq, f64>(site);
        if len < 1 || len >= ring.len() as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        if !src.is_nan() {
            let _ = seq_push_back(h, ring, src, 0);
        }
        rising_test::<RISE>(h, ring, len as u32, reading)
    };
    f.set_bool_arg(args[2], r);
    Ok(())
}

/// `ta.pivothigh` (`HIGH`) / `ta.pivotlow` over `[src?, left, right, out]` (`SRC == false` reads
/// `high`/`low`), block `Seq` + a ring of `left + right + 1` (`param2`): at each execution the
/// value `right` back is the pivot iff strictly beyond each of the `left` older and `right` newer
/// values; else na — reported `right` executions late. Strengths are `≥ 0`; a negative one or a
/// window beyond the tail is `ScriptRefused` (`pin:04-ta-reference`); an na one yields na
/// untouched. `skip` na: an na source is not accepted and the result is the unchanged window's —
/// a pivot reported on the previous execution is reported again (`pin:04-pivot-skip`).
pub fn bi_pivot<const HIGH: bool, const SRC: bool>(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    let n_in = if SRC { 3 } else { 2 };
    if args.len() != n_in + 1 {
        return Err(FaultKind::Internal);
    }
    let src = if SRC {
        f.f64_arg(args[0])
    } else if HIGH {
        f.f64_arg(argwin::f64(abi::F_HIGH))
    } else {
        f.f64_arg(argwin::f64(abi::F_LOW))
    };
    let (left, right) = (f.i64_arg(args[n_in - 2]), f.i64_arg(args[n_in - 1]));
    let dst = args[n_in];
    let (Some(left), Some(right)) = (left, right) else {
        f.set_f64_arg(dst, f64::NAN);
        return Ok(());
    };
    let out = {
        let (h, ring) = f.state_split::<Seq, f64>(site);
        if left < 0 || right < 0 || left + right >= ring.len() as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let (r, w) = (right as u32, (left + right) as u32 + 1);
        if !src.is_nan() {
            let _ = seq_push_back(h, ring, src, 0);
        }
        match seq_peek(h, ring, w - 1) {
            None => f64::NAN,
            Some(_) => {
                let c = seq_peek(h, ring, r).unwrap_or(f64::NAN);
                let mut pivot = true;
                let mut k = 0u32;
                while k < w {
                    if k != r {
                        let v = seq_peek(h, ring, k).unwrap_or(f64::NAN);
                        if !(if HIGH { c > v } else { c < v }) {
                            pivot = false;
                            break;
                        }
                    }
                    k += 1;
                }
                if pivot {
                    c
                } else {
                    f64::NAN
                }
            }
        }
    };
    f.set_f64_arg(dst, out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U03 both readings on `[1, 3, 2, 4]` (len 3) and `[1, 2, 3, 4]`.
    #[test]
    fn rising_falling_switch_both_readings() {
        let run = |xs: &[f64], reading: u8| {
            let mut h = Seq {
                n: 0,
                pos: 0,
                aux: 0,
                bar: 0,
            };
            let mut ring = [0.0f64; 4];
            for &x in xs {
                let _ = seq_push_back(&mut h, &mut ring, x, 0);
            }
            (
                rising_test::<true>(&h, &ring, 3, reading),
                rising_test::<false>(&h, &ring, 3, reading),
            )
        };
        let (gt, mono) = (
            swr::RISING_FALLING_GT_ALL_PRIOR,
            swr::RISING_FALLING_MONOTONE,
        );
        assert_eq!(run(&[1.0, 3.0, 2.0, 4.0], gt), (true, false));
        assert_eq!(run(&[1.0, 3.0, 2.0, 4.0], mono), (false, false));
        assert_eq!(run(&[1.0, 2.0, 3.0, 4.0], mono), (true, false));
        assert_eq!(run(&[4.0, 3.0, 2.0, 1.0], mono), (false, true));
        assert_eq!(run(&[4.0, 3.0, 2.0], gt), (false, false), "warm-up");
    }
}
