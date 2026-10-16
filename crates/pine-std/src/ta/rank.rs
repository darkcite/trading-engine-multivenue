// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` order statistics over a window (impl-04 §4.13.6, T25): `ta.percentrank`, `ta.median`,
//! `ta.mode`, `ta.percentile_nearest_rank`. Selection and sorting run on a **scratch copy** of the window inside the site's own
//! block (`WinHdr` + `[ring | scratch]`), so the ring stays chronological; `select_nth_unstable_by`
//! and `sort_unstable_by` work in place — no allocation (0 B/op).

use crate::args::out1;
use crate::state::{win_at, win_push, win_repoint, WinHdr};
use core::cmp::Ordering;
use pine_ir::{argwin, FaultKind, SiteId};
use pine_vm::Frame;

/// The na-last total order (§4.13.6): numeric order, NaN after every number, `-0.0 == 0.0`.
/// A named `fn`, not a closure.
fn cmp_na_last(a: &f64, b: &f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (false, false) => {
            if a < b {
                Ordering::Less
            } else if a > b {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        (true, true) => Ordering::Equal,
    }
}

/// A `source` input in either lane as f64 (`int`: exact below 2^53; na → NaN).
#[inline(always)]
fn src_of(f: &Frame<'_>, e: u16) -> f64 {
    if argwin::is_i64(e) {
        f.i64_arg(e).map_or(f64::NAN, |v| v as f64)
    } else {
        f.f64_arg(e)
    }
}

/// Write `v` (NaN = na) to `e` in its lane; an `int` output holds an exact window member.
#[inline(always)]
fn out_num(f: &mut Frame<'_>, e: u16, v: f64) {
    if argwin::is_i64(e) {
        f.set_i64_arg(e, if v.is_nan() { None } else { Some(v as i64) });
    } else {
        f.set_f64_arg(e, v);
    }
}

/// The shared `skip` window step of `[src, len, out]` (or `[src, len, p, out]`, `p` handed to
/// `stat`) over `WinHdr` + `[ring | scratch]`: push a non-na `src`, and once `len` values are
/// accepted copy the newest `len` into the scratch half and hand `scratch[..len]` to `stat`.
/// Warm-up or a na `len` → na. `len ∉ [1, cap]` → `ScriptRefused` (the site was sized for `cap`).
#[inline(always)]
fn scratch_stat(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
    stat: fn(&mut [f64], f64) -> f64,
) -> Result<(), FaultKind> {
    let p = match args.len() {
        3 => 0.0,
        4 => f.f64_arg(args[2]),
        _ => return Err(FaultKind::Internal),
    };
    let src = src_of(f, args[0]);
    let out = out1(args)?;
    let Some(len) = f.i64_arg(args[1]) else {
        out_num(f, out, f64::NAN);
        return Ok(());
    };
    let v = {
        let (h, tail) = f.state_split::<WinHdr, f64>(site);
        let cap = tail.len() / 2;
        if len < 1 || len > cap as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let (ring, ws) = tail.split_at_mut(cap);
        let len = len as u32;
        if h.len != len {
            win_repoint(h, ring, len);
        }
        if !src.is_nan() {
            win_push(h, ring, src);
        }
        if h.n < len {
            f64::NAN
        } else {
            // COPY: the newest `len` window values into the block's scratch half, ≤ cap f64 per
            // call — selection / sorting permute their input and the ring must stay chronological
            // (§4.13.6) — selecting in the ring rejected: it would reorder the history every
            // later bar reads.
            let mut k = 0u32;
            while k < len {
                ws[k as usize] = win_at(h, ring, k);
                k += 1;
            }
            stat(&mut ws[..len as usize], p)
        }
    };
    out_num(f, out, v);
    Ok(())
}

/// The median of `w` (non-empty, no NaN): the middle value; an even count averages the two
/// middle values (the textbook median — the reference says only "the median of the series";
/// `pin:04-median-even`).
fn median(w: &mut [f64], _: f64) -> f64 {
    let n = w.len();
    let m = n / 2;
    let (lo, mid, _) = w.select_nth_unstable_by(m, cmp_na_last);
    let hi = *mid;
    if n % 2 == 1 {
        return hi;
    }
    // The lower middle is the largest of the `m` values ordered before `mid`.
    let mut best = f64::NEG_INFINITY;
    let mut i = 0usize;
    while i < lo.len() {
        debug_assert!(i < lo.len());
        // SAFETY: `i < lo.len()` (loop bound).
        let x = unsafe { *lo.get_unchecked(i) };
        debug_assert!(!x.is_nan());
        if x > best {
            best = x;
        }
        i += 1;
    }
    (best + hi) / 2.0
}

/// The mode of `w` (non-empty, no NaN): sort in place, the longest run of equal values; ties →
/// the smallest value (TradingView reference, `ta.mode`).
fn mode(w: &mut [f64], _: f64) -> f64 {
    w.sort_unstable_by(cmp_na_last);
    let (mut best, mut best_run) = (w[0], 0usize);
    let (mut cur, mut run) = (w[0], 0usize);
    let mut i = 0usize;
    while i < w.len() {
        debug_assert!(i < w.len());
        // SAFETY: `i < w.len()` (loop bound).
        let x = unsafe { *w.get_unchecked(i) };
        if x == cur {
            run += 1;
        } else {
            cur = x;
            run = 1;
        }
        if run > best_run {
            best_run = run;
            best = cur;
        }
        i += 1;
    }
    best
}

/// The nearest-rank `p`-th percentile of `w` (non-empty, no NaN; the reference's "Nearest Rank
/// method", its Wikipedia link): the value of ordinal rank `n = ⌈p · N / 100⌉` (computed as
/// `(p · N) / 100`, so an integral rank is exact), always a window member, `p = 100` the
/// largest. `p = 0` gives rank 0, which names no member: rank 1, the smallest
/// (`pin:04-percentile-p0` — "always a member of the input data set"). A na `p` or one outside
/// `0..=100` (the reference's range) → na (`pin:04-percentile-range`).
fn nearest_rank(w: &mut [f64], p: f64) -> f64 {
    if !(0.0..=100.0).contains(&p) {
        return f64::NAN;
    }
    let n = w.len();
    let r = ((p * n as f64) / 100.0).ceil() as usize;
    let r = if r == 0 { 1 } else { r };
    debug_assert!(r >= 1 && r <= n);
    let (_, v, _) = w.select_nth_unstable_by(r - 1, cmp_na_last);
    *v
}

/// `ta.percentile_nearest_rank(source, length, percentage)` — `skip` na ("`na` values in the
/// `source` series are ignored"): over the newest `length` non-na values (pin:04-percentile-nearest-rank).
pub fn bi_ta_percentile_nearest_rank(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    scratch_stat(f, site, args, nearest_rank)
}

/// `ta.median(source, length)` — `skip` na.
pub fn bi_ta_median(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    scratch_stat(f, site, args, median)
}

/// `ta.mode(source, length)` (float and int overloads) — `skip` na.
pub fn bi_ta_mode(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    scratch_stat(f, site, args, mode)
}

/// `ta.percentrank(source, length)` = `100 × #(previous length values ≤ current) / length`
/// (§4.13.6; warm-up `W(len + 1)`). `propagate`: a na current value or a na among the previous
/// `length` values yields na. The ring (`cap ≥ length`) holds the previous values when the current
/// one is ranked, then takes it.
pub fn bi_ta_percentrank(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = f.f64_arg(args[0]);
    let Some(len) = f.i64_arg(args[1]) else {
        f.set_f64_arg(args[2], f64::NAN);
        return Ok(());
    };
    let v = {
        let (h, ring) = f.state_split::<WinHdr, f64>(site);
        let cap = ring.len();
        if len < 1 || len > cap as i64 {
            return Err(FaultKind::ScriptRefused);
        }
        let len = len as u32;
        if h.len != len {
            win_repoint(h, ring, len);
        }
        let v = if src.is_nan() || h.n < len || h.na_in != 0 {
            f64::NAN
        } else {
            let mut c = 0u32;
            let mut k = 0u32;
            while k < len {
                c += (win_at(h, ring, k) <= src) as u32;
                k += 1;
            }
            100.0 * c as f64 / len as f64
        };
        win_push(h, ring, src);
        v
    };
    f.set_f64_arg(args[2], v);
    Ok(())
}
