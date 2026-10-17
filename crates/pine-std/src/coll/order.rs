// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `array.*` order statistics and the int moments (impl-04 §4.16.6; operator ruling 2026-10-16):
//! `median`, `mode`, `percentile_nearest_rank`, `percentile_linear_interpolation` of an int or
//! float array, and `avg` / `variance` / `stdev` of an int array; `percentrank` (int / float) and
//! `covariance` (any int / float pairing) (r27, R-P5).
//!
//! No scratch and no copy: a collection block has no region a body may permute (the owner's words
//! are the script's array; spare capacity past `len` is not guaranteed — a full array has none),
//! so the order statistics never sort. A selection is a **bisection over the order-preserving
//! 64-bit key** of the elements: the smallest key `K` with `#(key ≤ K) ≥ k + 1` is the `k`-th
//! smallest — at most 64 counting passes, O(64 · len), read-only. `mode` walks the distinct values
//! in ascending order (one pass each, stopping once the values left cannot beat the best run):
//! O(len · distinct values) worst case (`pin:04-array-mode-scan`).
//!
//! na elements are skipped ("statistics do not return na when some of the values … are na", the
//! arrays page); an empty array or one whose every value is na → na (the reference's remark).

use super::array::{declared, flags};
use super::{resolve, Owned};
use pine_ir::{argwin, CollKind, FaultKind, SiteId, Ty, NA_I64};
use pine_spec::ov;
use pine_vm::Frame;

const SIGN: u64 = 1 << 63;
const NA: u64 = NA_I64 as u64;

/// An int or float array reference whose every element is still the owner's (§4.16.3), and
/// whether it is float; the output lane must be the element's (`int` → i64, `float` → f64).
fn numeric(f: &Frame<'_>, e: u16, out: u16) -> Result<(Owned, bool), FaultKind> {
    let o = resolve(f, f.coll_arg(e))?;
    if !matches!(o.kind, CollKind::Array) {
        return Err(FaultKind::Internal);
    }
    let float = match o.elem {
        Ty::Float => true,
        Ty::Int => false,
        _ => return Err(FaultKind::Internal),
    };
    if argwin::is_i64(out) == float {
        return Err(FaultKind::Internal);
    }
    if o.base + o.len > o.owner_len {
        return Err(FaultKind::CollectionBounds);
    }
    Ok((o, float))
}

/// Element word `w` is na.
#[inline(always)]
fn is_na(w: u64, float: bool) -> bool {
    if float {
        f64::from_bits(w).is_nan()
    } else {
        w == NA
    }
}

/// The order-preserving key of a non-na element word: ascending keys ⇔ ascending values, `−0.0`
/// before `+0.0` (as `array.sort`'s `total_cmp`, `pin:04-collection-edges`).
#[inline(always)]
fn okey(w: u64, float: bool) -> u64 {
    if !float {
        w ^ SIGN
    } else if w & SIGN != 0 {
        !w
    } else {
        w | SIGN
    }
}

/// The element word of key `k` (the inverse of [`okey`]).
#[inline(always)]
fn unkey(k: u64, float: bool) -> u64 {
    if !float {
        k ^ SIGN
    } else if k & SIGN != 0 {
        k & !SIGN
    } else {
        !k
    }
}

/// The element's value as f64 (an int converts exactly below 2^53).
#[inline(always)]
fn val(w: u64, float: bool) -> f64 {
    if float {
        f64::from_bits(w)
    } else {
        w as i64 as f64
    }
}

/// `(non-na count, smallest key, largest key)` of `w`; keys meaningless when the count is 0.
fn scan(w: &[u64], float: bool) -> (u64, u64, u64) {
    let (mut n, mut lo, mut hi) = (0u64, u64::MAX, 0u64);
    let mut i = 0usize;
    while i < w.len() {
        let x = w[i];
        if !is_na(x, float) {
            let k = okey(x, float);
            n += 1;
            lo = lo.min(k);
            hi = hi.max(k);
        }
        i += 1;
    }
    (n, lo, hi)
}

/// The `k`-th smallest (0-based, duplicates counted) non-na element word of `w`, whose non-na
/// keys lie in `[lo, hi]` and number more than `k`: the bisection of the module doc.
fn select(w: &[u64], float: bool, k: u64, mut lo: u64, mut hi: u64) -> u64 {
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let mut c = 0u64;
        let mut i = 0usize;
        while i < w.len() {
            let x = w[i];
            c += (!is_na(x, float) && okey(x, float) <= mid) as u64;
            i += 1;
        }
        if c > k {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    unkey(lo, float)
}

/// Write `v` to the output: an int lane takes `v` toward zero, as `int()` does
/// (`pin:04-array-int-stats-provisional`); na / non-finite → na.
#[inline(always)]
fn out(f: &mut Frame<'_>, e: u16, v: f64) {
    if argwin::is_i64(e) {
        let ok = v.is_finite() && v.abs() < SIGN as f64;
        f.set_i64_arg(e, ok.then(|| v.trunc() as i64));
    } else {
        f.set_f64_arg(e, v);
    }
}

/// The element words a reference sees.
fn words_of<'a>(words: &'a [u64], o: &Owned) -> &'a [u64] {
    &words[o.base as usize..(o.base + o.len) as usize]
}

/// `array.median(id)` — `[id, out]`. Float: the middle value, an even count the mean of the two
/// middles (as `ta.median`, `pin:04-median-even`). Int (`series int`): an even count takes the
/// **lower** middle — a member, never a rounded mean (`pin:04-median-int-provisional`).
fn median(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let (o, float) = numeric(f, args[0], args[1])?;
    let (_, words) = f.coll(o.owner)?;
    let w = words_of(words, &o);
    let (n, lo, hi) = scan(w, float);
    let v = if n == 0 {
        f64::NAN
    } else if !float {
        val(select(w, false, (n - 1) / 2, lo, hi), false)
    } else if n % 2 == 1 {
        val(select(w, true, n / 2, lo, hi), true)
    } else {
        let a = val(select(w, true, n / 2 - 1, lo, hi), true);
        let b = val(select(w, true, n / 2, lo, hi), true);
        (a + b) / 2.0
    };
    out(f, args[1], v);
    Ok(())
}

/// `array.mode(id)` — `[id, out]`: the most frequent non-na value; ties → the smallest (the
/// reference). Distinct values are visited ascending, so the first longest run wins.
fn mode(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let (o, float) = numeric(f, args[0], args[1])?;
    let (_, words) = f.coll(o.owner)?;
    let w = words_of(words, &o);
    let (n, _, _) = scan(w, float);
    let mut seen = 0u64;
    let (mut best, mut best_c) = (0u64, 0u64);
    let mut below: Option<u64> = None;
    // Every value left has at most `n − seen` occurrences: once that cannot beat the best run
    // (a tie keeps the smaller, earlier value), no later value can win.
    while n - seen > best_c {
        let (mut cur, mut c) = (u64::MAX, 0u64);
        let mut i = 0usize;
        while i < w.len() {
            let x = w[i];
            i += 1;
            if is_na(x, float) {
                continue;
            }
            let k = okey(x, float);
            if below.is_some_and(|b| k <= b) {
                continue;
            }
            if k < cur {
                cur = k;
                c = 1;
            } else if k == cur {
                c += 1;
            }
        }
        debug_assert!(c > 0);
        if c > best_c {
            best = cur;
            best_c = c;
        }
        seen += c;
        below = Some(cur);
    }
    let v = if n == 0 {
        f64::NAN
    } else {
        val(unkey(best, float), float)
    };
    out(f, args[1], v);
    Ok(())
}

/// `array.percentile_nearest_rank(id, percentage)` — `[id, p, out]`: the value of ordinal rank
/// `⌈p · N / 100⌉` among the `N` non-na values, rank 1 for `p = 0` (as `ta.percentile_nearest_rank`,
/// `pin:04-percentile-p0`); na `p` or one outside `0..=100` → na (`pin:04-percentile-range`).
/// Always a member, so the int overload is exact.
fn nearest_rank(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (o, float) = numeric(f, args[0], args[2])?;
    let p = pct(f, args[1]);
    let (_, words) = f.coll(o.owner)?;
    let w = words_of(words, &o);
    let (n, lo, hi) = scan(w, float);
    let v = if n == 0 || !(0.0..=100.0).contains(&p) {
        f64::NAN
    } else {
        let r = ((p * n as f64) / 100.0).ceil() as u64;
        let r = r.clamp(1, n);
        val(select(w, float, r - 1, lo, hi), float)
    };
    out(f, args[2], v);
    Ok(())
}

/// `array.percentile_linear_interpolation(id, percentage)` — `[id, p, out]`: the C = 1 definition
/// (Excel `PERCENTILE.INC`, Hyndman–Fan type 7, as `ta.percentile_linear_interpolation`):
/// `x = p · (N − 1) / 100`, `k = ⌊x⌋`, `v₍ₖ₎ + (x − k) · (v₍ₖ₊₁₎ − v₍ₖ₎)` over the ascending non-na
/// values (`pin:04-percentile-linear-provisional`); na `p` or one outside `0..=100` → na. The int
/// overload (`series int`) takes the interpolated value toward zero
/// (`pin:04-array-int-stats-provisional`).
fn linear(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (o, float) = numeric(f, args[0], args[2])?;
    let p = pct(f, args[1]);
    let (_, words) = f.coll(o.owner)?;
    let w = words_of(words, &o);
    let (n, lo, hi) = scan(w, float);
    let v = if n == 0 || !(0.0..=100.0).contains(&p) {
        f64::NAN
    } else {
        let x = (p * (n - 1) as f64) / 100.0;
        let k = x.floor() as u64;
        debug_assert!(k < n);
        let a = val(select(w, float, k, lo, hi), float);
        let frac = x - k as f64;
        if frac == 0.0 || k + 1 >= n {
            a
        } else {
            let b = val(select(w, float, k + 1, lo, hi), float);
            a + frac * (b - a)
        }
    };
    out(f, args[2], v);
    Ok(())
}

/// The `percentage` argument in either lane (int exact below 2^53; na → NaN).
#[inline(always)]
fn pct(f: &Frame<'_>, e: u16) -> f64 {
    if argwin::is_i64(e) {
        f.i64_arg(e).map_or(f64::NAN, |v| v as f64)
    } else {
        f.f64_arg(e)
    }
}

/// `array.avg(id)` of an int array — `[id, out]` (`series int`): the exact mean of the non-na
/// values toward zero (`Σ` in i128, `/ n` truncating — `int()`'s rounding,
/// `pin:04-array-int-stats-provisional`); empty / every value na → na.
fn avg_i(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 2 {
        return Err(FaultKind::Internal);
    }
    let (o, float) = numeric(f, args[0], args[1])?;
    if float {
        return Err(FaultKind::Internal);
    }
    let (_, words) = f.coll(o.owner)?;
    let w = words_of(words, &o);
    let (mut s, mut n) = (0i128, 0i128);
    let mut i = 0usize;
    while i < w.len() {
        if w[i] != NA {
            s += w[i] as i64 as i128;
            n += 1;
        }
        i += 1;
    }
    // The mean lies between the extremes, so it fits i64.
    f.set_i64_arg(args[1], (n > 0).then(|| (s / n) as i64));
    Ok(())
}

/// `array.variance` / `array.stdev(id, biased)` of an int array — `[id, biased, out]`
/// (`series int`): the float definition (`pin:04-array-variance-two-pass`: mean, then
/// `Σ(x − mean)²` over the non-na values, `/ n` when `biased`, else `/ (n − 1)`; `stdev` its
/// square root) over the values as f64, then toward zero (`pin:04-array-int-stats-provisional`;
/// `stdev` is the root of the unrounded variance). Empty / every value na → na; an unbiased
/// estimate of one value is `0 / 0` → na. A na `biased` (v5) is not true.
fn var_std_i<const SD: bool>(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (o, float) = numeric(f, args[0], args[2])?;
    if float {
        return Err(FaultKind::Internal);
    }
    let biased = f.bool_arg(args[1]) == Some(true);
    let (_, words) = f.coll(o.owner)?;
    let w = words_of(words, &o);
    let (mut s, mut n) = (0.0f64, 0u64);
    let mut i = 0usize;
    while i < w.len() {
        if w[i] != NA {
            s += w[i] as i64 as f64;
            n += 1;
        }
        i += 1;
    }
    let v = if n == 0 {
        f64::NAN
    } else {
        let m = s / n as f64;
        let mut ss = 0.0f64;
        let mut i = 0usize;
        while i < w.len() {
            if w[i] != NA {
                let d = w[i] as i64 as f64 - m;
                ss += d * d;
            }
            i += 1;
        }
        let d = if biased { n } else { n - 1 };
        let var = ss / d as f64;
        if SD {
            var.sqrt()
        } else {
            var
        }
    };
    out(f, args[2], v);
    Ok(())
}

// ---- covariance / percentrank (r27, R-P5) ------------------------------------------------------

/// An int or float array reference whose every element is still the owner's (§4.16.3), and
/// whether it is float — for a row whose output lane does not follow the element.
fn numeric_any(f: &Frame<'_>, e: u16) -> Result<(Owned, bool), FaultKind> {
    let o = resolve(f, f.coll_arg(e))?;
    if !matches!(o.kind, CollKind::Array) {
        return Err(FaultKind::Internal);
    }
    let float = match o.elem {
        Ty::Float => true,
        Ty::Int => false,
        _ => return Err(FaultKind::Internal),
    };
    if o.base + o.len > o.owner_len {
        return Err(FaultKind::CollectionBounds);
    }
    Ok((o, float))
}

/// `array.covariance(id1, id2, biased)` — `[id1, id2, biased, out]` (`series float`), any mix of
/// int and float arrays. `pin:04-array-covariance-pairs`: the reference gives neither a formula
/// nor an na rule; the literal reading, as `pin:04-array-variance-two-pass`: element `i` of `id1`
/// pairs with element `i` of `id2`, a pair with an na side is skipped (the arrays page: na values
/// do not make a statistic na), the means of the kept pairs, then `Σ(x − mx)(y − my)` divided by
/// `n` when `biased` is true, else by `n − 1`. `pin:04-array-covariance-sizes`: arrays of
/// different sizes have no pairing → `CollectionBounds`. No kept pair → na; an unbiased estimate of
/// one pair is `0 / 0` → na. A na `biased` (v5) is not true (as `array.variance`).
fn covariance(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 4 || argwin::is_i64(args[3]) {
        return Err(FaultKind::Internal);
    }
    let (a, af) = numeric_any(f, args[0])?;
    let (b, bf) = numeric_any(f, args[1])?;
    if a.len != b.len {
        return Err(FaultKind::CollectionBounds);
    }
    let biased = f.bool_arg(args[2]) == Some(true);
    let v = {
        let (_, wa) = f.coll(a.owner)?;
        let (_, wb) = f.coll(b.owner)?;
        let (xa, xb) = (words_of(wa, &a), words_of(wb, &b));
        let (mut sa, mut sb, mut n) = (0.0f64, 0.0f64, 0u64);
        let mut i = 0usize;
        while i < xa.len() {
            if !is_na(xa[i], af) && !is_na(xb[i], bf) {
                sa += val(xa[i], af);
                sb += val(xb[i], bf);
                n += 1;
            }
            i += 1;
        }
        if n == 0 {
            f64::NAN
        } else {
            let (ma, mb) = (sa / n as f64, sb / n as f64);
            let mut sp = 0.0f64;
            let mut i = 0usize;
            while i < xa.len() {
                if !is_na(xa[i], af) && !is_na(xb[i], bf) {
                    sp += (val(xa[i], af) - ma) * (val(xb[i], bf) - mb);
                }
                i += 1;
            }
            let d = if biased { n } else { n - 1 };
            sp / d as f64
        }
    };
    f.set_f64_arg(args[3], v);
    Ok(())
}

/// `array.percentrank(id, index)` — `[id, index, out]` (float row `series float`, int row
/// `series int`). The reference: "the percentage of how many elements in the array are less than
/// or equal to the reference value" (the element at `index`). `pin:04-array-percentrank`: read
/// literally — `100 × #(x ≤ ref) / n` over the non-na elements (the reference element counts
/// itself; an na element is neither counted nor in `n`, as every array statistic), IEEE `≤`
/// (`−0.0 = +0.0`); an na reference element → na; the int row truncates toward zero
/// (`pin:04-array-int-stats-provisional`). An `index` outside `[0, size)` — negative (§4.16.5:
/// only `get set insert remove` normalise one) or na included — is `CollectionBounds`.
fn percentrank(f: &mut Frame<'_>, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 3 {
        return Err(FaultKind::Internal);
    }
    let (o, float) = numeric(f, args[0], args[2])?;
    let k = match f.i64_arg(args[1]) {
        Some(k) if 0 <= k && k < o.len as i64 => k as usize,
        _ => return Err(FaultKind::CollectionBounds),
    };
    let v = {
        let (_, words) = f.coll(o.owner)?;
        let w = words_of(words, &o);
        let r = w[k];
        if is_na(r, float) {
            f64::NAN
        } else {
            let (mut le, mut n) = (0u64, 0u64);
            let mut i = 0usize;
            while i < w.len() {
                if !is_na(w[i], float) {
                    n += 1;
                    let below = if float {
                        f64::from_bits(w[i]) <= f64::from_bits(r)
                    } else {
                        (w[i] as i64) <= (r as i64)
                    };
                    le += below as u64;
                }
                i += 1;
            }
            100.0 * le as f64 / n as f64
        }
    };
    out(f, args[2], v);
    Ok(())
}

// ---- the rows (pine-vm `flag_ok`) --------------------------------------------------------------

/// The `array.median` rows (float / int).
pub fn bi_array_median(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    median(f, args).map_err(|k| declared(flags(ov::ARRAY_MEDIAN_F), k))
}
/// The `array.mode` rows (float / int).
pub fn bi_array_mode(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    mode(f, args).map_err(|k| declared(flags(ov::ARRAY_MODE_F), k))
}
/// The `array.percentile_nearest_rank` rows (float / int).
pub fn bi_array_percentile_nearest_rank(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    nearest_rank(f, args).map_err(|k| declared(flags(ov::ARRAY_PERCENTILE_NEAREST_RANK_F), k))
}
/// The `array.percentile_linear_interpolation` rows (float / int).
pub fn bi_array_percentile_linear_interpolation(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    linear(f, args).map_err(|k| declared(flags(ov::ARRAY_PERCENTILE_LINEAR_INTERPOLATION_F), k))
}
/// The `array.avg` row of an int array.
pub fn bi_array_avg_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    avg_i(f, args).map_err(|k| declared(flags(ov::ARRAY_AVG_I), k))
}
/// The `array.variance` row of an int array.
pub fn bi_array_variance_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    var_std_i::<false>(f, args).map_err(|k| declared(flags(ov::ARRAY_VARIANCE_I), k))
}
/// The `array.stdev` row of an int array.
pub fn bi_array_stdev_i(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    var_std_i::<true>(f, args).map_err(|k| declared(flags(ov::ARRAY_STDEV_I), k))
}
/// The `array.covariance` rows (every int / float pairing).
pub fn bi_array_covariance(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    covariance(f, args).map_err(|k| declared(flags(ov::ARRAY_COVARIANCE_AFAF), k))
}
/// The `array.percentrank` rows (float / int).
pub fn bi_array_percentrank(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    percentrank(f, args).map_err(|k| declared(flags(ov::ARRAY_PERCENTRANK_F), k))
}
