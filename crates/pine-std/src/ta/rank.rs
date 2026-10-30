// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ta.*` order statistics over a window (impl-04 §4.13.6, T25): `ta.percentrank`, `ta.median`,
//! `ta.mode`, `ta.percentile_nearest_rank`, `ta.percentile_linear_interpolation`. Selection and sorting run on a **scratch copy** of the window inside the site's own
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

/// The shared window step of `[src, len, out]` (or `[src, len, p, out]`, `p` handed to `stat`)
/// over `WinHdr` + `[ring | scratch]`. `SKIP`: push a non-na `src` only, and once `len` values
/// are accepted copy the newest `len` into the scratch half and hand `scratch[..len]` to `stat`.
/// `!SKIP` (na included): push every execution's `src`; a na among the newest `len` → na.
/// Warm-up or a na `len` → na. `len ∉ [1, cap]` → `ScriptRefused` (the site was sized for `cap`).
#[inline(always)]
fn scratch_stat<const SKIP: bool>(
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
        if !SKIP || !src.is_nan() {
            win_push(h, ring, src);
        }
        if h.n < len || (!SKIP && h.na_in != 0) {
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

/// The median of an `int` window (non-empty, no NaN; the values are exact integers): the
/// reference returns `series int` for the int overload, so an even count takes the **lower**
/// middle value — always a window member, never a rounded mean (`pin:04-median-int-provisional`,
/// operator ruling 2026-10-16; to be verified against TradingView in the capture month).
fn median_lower(w: &mut [f64], _: f64) -> f64 {
    let n = w.len();
    let (_, v, _) = w.select_nth_unstable_by((n - 1) / 2, cmp_na_last);
    *v
}

/// The linearly interpolated `p`-th percentile of `w` (non-empty, no NaN): the reference's
/// "linear interpolation between the two nearest ranks" is the Wikipedia method of that name in
/// its first variant, C = 1/2 (Hyndman–Fan type 5): the 0-based rank `x = p · N / 100 − 1/2`,
/// clamped to `[0, N − 1]` (below the first rank the smallest value, above the last the largest),
/// `k = ⌊x⌋`, `v = w₍ₖ₎ + (x − k) · (w₍ₖ₊₁₎ − w₍ₖ₎)` over the ascending order statistics
/// (`pin:04-percentile-linear-c-half`, CI-015b, fixture B096). Evidence: B096 (`p = 2`, `N = 14`)
/// is the window minimum on all 128 captured rows (row 13: 1089.3); C = 1 interpolates there
/// (1095.3892) and is refuted; C = 0 also clamps at `p = 2` and is not separated by the capture
/// (a fixture at `p = 25` would separate them: C = 1/2 rank 3.0, C = 0 rank 2.75). A na `p` or
/// one outside `0..=100` → na (`pin:04-percentile-range`).
fn linear(w: &mut [f64], p: f64) -> f64 {
    if !(0.0..=100.0).contains(&p) {
        return f64::NAN;
    }
    let n = w.len();
    let x = (p * n as f64) / 100.0 - 0.5;
    let last = (n - 1) as f64;
    let x = if x < 0.0 {
        0.0
    } else if x > last {
        last
    } else {
        x
    };
    let k = x.floor() as usize;
    debug_assert!(k < n);
    let (_, lo, hi) = w.select_nth_unstable_by(k, cmp_na_last);
    let lo = *lo;
    let frac = x - k as f64;
    if frac == 0.0 || hi.is_empty() {
        return lo;
    }
    // The next order statistic is the smallest of the values ordered after `lo`.
    let mut next = f64::INFINITY;
    let mut i = 0usize;
    while i < hi.len() {
        debug_assert!(i < hi.len());
        // SAFETY: `i < hi.len()` (loop bound).
        let y = unsafe { *hi.get_unchecked(i) };
        debug_assert!(!y.is_nan());
        if y < next {
            next = y;
        }
        i += 1;
    }
    lo + frac * (next - lo)
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
    scratch_stat::<true>(f, site, args, nearest_rank)
}

/// `ta.median(source, length)` — `skip` na.
pub fn bi_ta_median(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    scratch_stat::<true>(f, site, args, median)
}

/// `ta.median(source, length)`, the `int` overload — `skip` na; an even window gives the lower
/// middle value (`pin:04-median-int-provisional`).
pub fn bi_ta_median_i(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    scratch_stat::<true>(f, site, args, median_lower)
}

/// `ta.percentile_linear_interpolation(source, length, percentage)` — C = 1/2 interpolation
/// ([`linear`], `pin:04-percentile-linear-c-half`; TV F104: rows 13, 14 → 1275.44, row 15 →
/// 1225.03, and B096) over the newest `length` **executions**: the
/// reference's remark "`na` values in the `source` series are included in calculations and will
/// produce an `na` result" is followed — pine-spec's row is `na: propagate`
/// (`pin:04-percentile-linear-na`).
pub fn bi_ta_percentile_linear_interpolation(
    f: &mut Frame<'_>,
    site: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    if args.len() != 4 {
        return Err(FaultKind::Internal);
    }
    scratch_stat::<false>(f, site, args, linear)
}

/// `ta.mode(source, length)` (float and int overloads) — `skip` na.
pub fn bi_ta_mode(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    scratch_stat::<true>(f, site, args, mode)
}

/// `ta.percentrank(source, length)` = `100 × #(previous length values ≤ current) / length`
/// (§4.13.6). The ring (`cap ≥ length`) holds the previous values when the current one is ranked,
/// then takes it.
///
/// `pin:U18-ta-na-propagation` (r33a verdict, CI-014, TV right): the window is the previous
/// `length` bars, na included, and an na takes part in the count as Pine compares — `false` on
/// either side: an na among the previous values is never counted, an na current value counts 0
/// (the output is 0, not na). The output is na only while fewer than `length` previous bars
/// exist. B075 bit-exact on all 128 rows; `propagate` (an na in the window → na) misses 85.
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
        let v = if h.n < len {
            f64::NAN
        } else {
            let mut c = 0u32;
            let mut k = 0u32;
            while k < len {
                // NaN on either side compares false: an na is never counted.
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

/// Spearman's ρ of the newest `len` window values against time, ×100 (`ta.rci`'s value): each
/// value's ascending rank (ties take the average rank), the oldest value's time rank 1, the
/// newest's `len`; both rank means are `(len + 1) / 2`. With half-integer ranks every deviation,
/// product and sum below is exact, so only `cov`, the two `sd`s and the final expression round.
#[inline(always)]
fn rci_of(h: &WinHdr, ring: &[f64], len: u32) -> f64 {
    let n = len as f64;
    let mean = (n + 1.0) / 2.0;
    let (mut sxy, mut sxx, mut stt) = (0.0f64, 0.0f64, 0.0f64);
    let mut k = 0u32;
    while k < len {
        let x = win_at(h, ring, k);
        let (mut lt, mut eq) = (0u32, 0u32);
        let mut j = 0u32;
        while j < len {
            let y = win_at(h, ring, j);
            lt += (y < x) as u32;
            eq += (y == x) as u32;
            j += 1;
        }
        let dx = lt as f64 + (eq as f64 + 1.0) / 2.0 - mean;
        let dt = (len - k) as f64 - mean;
        sxy += dx * dt;
        sxx += dx * dx;
        stt += dt * dt;
        k += 1;
    }
    let cov = sxy / n;
    let (sdx, sdt) = ((sxx / n).sqrt(), (stt / n).sqrt());
    100.0 * cov / (sdx * sdt)
}

/// `ta.rci(source, length)` — window `[src, len, out]`, block `WinHdr` + a ring of `length`
/// values; `skip` na (an na source is not accepted; the window's value is printed again).
///
/// `pin:04-rci-tv-arith` (r33c, fitted to TradingView, fixture B099): the reference's
/// "Spearman's rank correlation coefficient … between `source` and the bar index over `length`
/// bars … scaled to −100…100" as Pearson's r of the two rank series with population moments —
/// `100 · cov / (sd_x · sd_t)`, `cov = Σ dx·dt / n`, `sd = √(Σ d² / n)` — bit-exact on all 114
/// valued rows. The textbook `(1 − 6·Σd² / (n(n² − 1))) · 100` (in thirteen evaluation orders)
/// misses 100–114 of them by 1–3 ULP. Ties are not captured (SYN-1 has none in B099's window);
/// the average rank is Spearman's standard. A flat window gives `0 / 0` → na (not captured).
///
/// `pin:04-rci-warmup` (r33c, TV right, B099): the first value comes once `length + 1` values
/// have been accepted — rows 0…13 are na for `length = 14`, row 14 is the first value, and it is
/// the rank correlation of the newest 14 values (the window ending one bar earlier misses every
/// row).
pub fn bi_ta_rci(f: &mut Frame<'_>, site: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let src = src_of(f, args[0]);
    let Some(len) = f.i64_arg(args[1]) else {
        f.set_f64_arg(args[2], f64::NAN);
        return Ok(());
    };
    let v = {
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
        if h.n <= len {
            f64::NAN
        } else {
            rci_of(h, ring, len)
        }
    };
    f.set_f64_arg(args[2], v);
    Ok(())
}
