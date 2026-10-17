// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.7 — T25's order statistics (`ta.percentrank`, `ta.median`, `ta.mode`) against a
//! brute force, and 0 B/op on the scratch-copy path (a counting global allocator in this test
//! binary: every step after the build allocates nothing).
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::{ov, OVERLOADS};
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, CHART};
use pine_vm::testkit::bar;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
thread_local! {
    /// Count only this thread's allocations inside a measured step (tests run in parallel).
    static ON: Cell<bool> = const { Cell::new(false) };
}
// SAFETY: forwards to `System`, counting calls only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if ON.with(Cell::get) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static A: Counting = Counting;

/// A deterministic tape of small integers (ties for `mode`) with na holes.
fn tape(n: usize, holes: bool) -> Vec<f64> {
    let mut s = 0x2545_f491_4f6c_dd1du64;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            if holes && s % 11 == 0 {
                f64::NAN
            } else {
                ((s >> 20) % 9) as f64 - 4.0
            }
        })
        .collect()
}

/// `id(feed[0], len)` over `xs`; `int` lanes when `int`. Returns the outputs and the number of
/// allocations this thread made inside the steps after the first `len` bars.
fn run(id: OverloadId, len: u32, xs: &[f64], int: bool) -> (Vec<f64>, u64) {
    run_p(id, len, xs, int, None)
}

/// As `run`; `p` appends a constant float third argument (`ta.percentile_*`'s `percentage`).
fn run_p(id: OverloadId, len: u32, xs: &[f64], int: bool, p: Option<f64>) -> (Vec<f64>, u64) {
    let lane = if int { Lane::I64 } else { Lane::F64 };
    let mut ins = vec![(lane, Src::Feed(0)), (Lane::I64, Src::Const(len as u64))];
    if let Some(p) = p {
        ins.push((Lane::F64, Src::Const(p.to_bits())));
    }
    let outs = [lane];
    let calls = [Call {
        ov: id,
        inputs: &ins,
        outputs: &outs,
        site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(len),
        guard: None,
    }];
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx {
        feed: vec![0.0],
        ..TestCtx::default()
    };
    let mut out = Vec::with_capacity(xs.len());
    let mut allocs = 0;
    for (k, &x) in xs.iter().enumerate() {
        ctx.feed[0] = x;
        let b = bar(k as u32, 100.0, barflag::CONFIRMED);
        let measured = k >= len as usize;
        let a0 = ALLOCS.load(Ordering::Relaxed);
        ON.with(|c| c.set(measured));
        let r = h.step_bar(&b, &mut ctx);
        ON.with(|c| c.set(false));
        if measured {
            allocs += ALLOCS.load(Ordering::Relaxed) - a0;
        }
        assert_eq!(r, StepOutcome::Ok);
        out.push(h.out(0, 0));
    }
    (out, allocs)
}

fn sorted(w: &[f64]) -> Vec<f64> {
    let mut v = w.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}

fn bf_median(w: &[f64]) -> f64 {
    let v = sorted(w);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn bf_mode(w: &[f64]) -> f64 {
    let v = sorted(w);
    let (mut best, mut br) = (v[0], 0);
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j < v.len() && v[j] == v[i] {
            j += 1;
        }
        if j - i > br {
            br = j - i;
            best = v[i];
        }
        i = j;
    }
    best
}

/// Skip rows: the statistic over the newest `len` non-na values, na during warm-up.
fn bf_skip(xs: &[f64], len: usize, stat: fn(&[f64]) -> f64) -> Vec<f64> {
    let mut acc = Vec::new();
    xs.iter()
        .map(|&x| {
            if !x.is_nan() {
                acc.push(x);
            }
            if acc.len() < len {
                f64::NAN
            } else {
                stat(&acc[acc.len() - len..])
            }
        })
        .collect()
}

fn same(a: &[f64], b: &[f64], what: &str) {
    assert_eq!(a.len(), b.len());
    for (k, (x, y)) in a.iter().zip(b).enumerate() {
        assert!(
            x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
            "{what} bar {k}: {x} vs {y}"
        );
    }
}

#[test]
fn ta_median_and_mode_equal_bruteforce_skipping_na() {
    let xs = tape(3000, true);
    for len in [1u32, 2, 5, 6, 31] {
        let (got, _) = run(ov::TA_MEDIAN_F, len, &xs, false);
        same(&got, &bf_skip(&xs, len as usize, bf_median), "median");
        let (got, _) = run(ov::TA_MODE_F, len, &xs, false);
        same(&got, &bf_skip(&xs, len as usize, bf_mode), "mode");
        let (got, _) = run(ov::TA_MODE_I, len, &xs, true);
        same(&got, &bf_skip(&xs, len as usize, bf_mode), "mode int");
    }
}

#[test]
fn ta_percentrank_equals_bruteforce_and_propagates_na() {
    for holes in [false, true] {
        let xs = tape(3000, holes);
        for len in [1u32, 4, 17] {
            let l = len as usize;
            let want: Vec<f64> = (0..xs.len())
                .map(|t| {
                    if t < l || xs[t - l..=t].iter().any(|v| v.is_nan()) {
                        return f64::NAN;
                    }
                    let c = xs[t - l..t].iter().filter(|&&p| p <= xs[t]).count();
                    100.0 * c as f64 / len as f64
                })
                .collect();
            let (got, _) = run(ov::TA_PERCENTRANK, len, &xs, false);
            same(&got, &want, "percentrank");
        }
    }
}

#[test]
fn scratch_copy_path_allocates_nothing() {
    // The counter is live: a measured allocation is seen.
    let a0 = ALLOCS.load(Ordering::Relaxed);
    ON.with(|c| c.set(true));
    let probe = std::hint::black_box(vec![1u8; 3]);
    ON.with(|c| c.set(false));
    drop(probe);
    assert!(ALLOCS.load(Ordering::Relaxed) > a0);
    let xs = tape(2000, true);
    for (id, int) in [
        (ov::TA_MEDIAN_F, false),
        (ov::TA_MODE_F, false),
        (ov::TA_MODE_I, true),
        (ov::TA_PERCENTRANK, false),
    ] {
        let (_, allocs) = run(id, 64, &xs, int);
        assert_eq!(allocs, 0, "{id:?}");
    }
    let (_, allocs) = run_p(ov::TA_PERCENTILE_NEAREST_RANK, 64, &xs, false, Some(37.5));
    assert_eq!(allocs, 0, "percentile_nearest_rank");
}

/// Brute-force Pearson r over the newest `len` pairs with both values non-na.
fn bf_corr(a: &[f64], b: &[f64], len: usize) -> Vec<f64> {
    let mut acc: Vec<(f64, f64)> = Vec::new();
    (0..a.len())
        .map(|t| {
            if !a[t].is_nan() && !b[t].is_nan() {
                acc.push((a[t], b[t]));
            }
            if acc.len() < len {
                return f64::NAN;
            }
            let w = &acc[acc.len() - len..];
            let n = len as f64;
            let ma = w.iter().map(|p| p.0).sum::<f64>() / n;
            let mb = w.iter().map(|p| p.1).sum::<f64>() / n;
            let (mut c, mut va, mut vb) = (0.0, 0.0, 0.0);
            for &(x, y) in w {
                c += (x - ma) * (y - mb);
                va += (x - ma) * (x - ma);
                vb += (y - mb) * (y - mb);
            }
            if va <= 1e-12 || vb <= 1e-12 {
                f64::NAN
            } else {
                c / (va * vb).sqrt()
            }
        })
        .collect()
}

fn run2(len: u32, a: &[f64], b: &[f64]) -> (Vec<f64>, u64) {
    let id = ov::TA_CORRELATION;
    let ins = [
        (Lane::F64, Src::Feed(0)),
        (Lane::F64, Src::Feed(1)),
        (Lane::I64, Src::Const(len as u64)),
    ];
    let outs = [Lane::F64];
    let calls = [Call {
        ov: id,
        inputs: &ins,
        outputs: &outs,
        site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(len),
        guard: None,
    }];
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx {
        feed: vec![0.0, 0.0],
        ..TestCtx::default()
    };
    let mut out = Vec::new();
    let mut allocs = 0;
    for t in 0..a.len() {
        ctx.feed[0] = a[t];
        ctx.feed[1] = b[t];
        let a0 = ALLOCS.load(Ordering::Relaxed);
        ON.with(|c| c.set(t >= len as usize));
        let r = h.step_bar(&bar(t as u32, 100.0, barflag::CONFIRMED), &mut ctx);
        ON.with(|c| c.set(false));
        allocs += ALLOCS.load(Ordering::Relaxed) - a0;
        assert_eq!(r, StepOutcome::Ok);
        out.push(h.out(0, 0));
    }
    (out, allocs)
}

#[test]
fn ta_correlation_matches_bruteforce_and_skips_na_pairs() {
    let a = tape(5000, true);
    let mut b = tape(5000, true);
    b.rotate_left(7);
    for t in 0..b.len() {
        b[t] = 0.5 * b[t] + 0.25 * a[t]; // correlated, NaN where either tape has one
    }
    for len in [2u32, 5, 40] {
        let want = bf_corr(&a, &b, len as usize);
        let (got, allocs) = run2(len, &a, &b);
        assert_eq!(allocs, 0);
        for (t, (g, w)) in got.iter().zip(&want).enumerate() {
            if w.is_nan() {
                assert!(
                    g.is_nan(),
                    "len {len} bar {t}: {g} vs na (dyadic data: exact sums)"
                );
            } else {
                assert!((g - w).abs() < 1e-9, "len {len} bar {t}: {g} vs {w}");
            }
        }
    }
    // A flat source has zero variance: na.
    let flat = vec![3.0; 50];
    let (got, _) = run2(5, &flat, &a[..50]);
    assert!(got.iter().all(|v| v.is_nan()));
}

/// Nearest rank over the newest `len` non-na values: ordinal rank `⌈P·N/100⌉` of the sorted
/// window (rank 1 for `P = 0`, pin:04-percentile-p0), always a member, `P = 100` the largest;
/// `P` outside `0..=100` or na → na (pin:04-percentile-range).
#[test]
fn ta_percentile_nearest_rank_is_the_ordinal_rank() {
    let xs = tape(600, true);
    for len in [1u32, 2, 5, 10, 31] {
        for p in [0.0, 10.0, 25.0, 30.0, 33.3, 50.0, 99.9, 100.0] {
            let want = bf_skip(&xs, len as usize, |w| w[0]);
            let want: Vec<f64> = want
                .iter()
                .enumerate()
                .map(|(t, &x)| {
                    if x.is_nan() {
                        return f64::NAN;
                    }
                    // The newest `len` non-na values at bar `t`.
                    let acc: Vec<f64> = xs[..=t].iter().copied().filter(|v| !v.is_nan()).collect();
                    let v = sorted(&acc[acc.len() - len as usize..]);
                    let r = ((p * len as f64) / 100.0).ceil() as usize;
                    v[r.max(1) - 1]
                })
                .collect();
            let (got, _) = run_p(ov::TA_PERCENTILE_NEAREST_RANK, len, &xs, false, Some(p));
            same(&got, &want, "percentile_nearest_rank");
        }
    }
    // Exact integral ranks: 30 % of 10 is rank 3, not 4 (0.3 × 10 > 3 in binary).
    let xs: Vec<f64> = (1..=10).map(|v| v as f64).collect();
    let (got, _) = run_p(ov::TA_PERCENTILE_NEAREST_RANK, 10, &xs, false, Some(30.0));
    assert_eq!(got[9], 3.0);
    let (got, _) = run_p(ov::TA_PERCENTILE_NEAREST_RANK, 10, &xs, false, Some(100.0));
    assert_eq!(got[9], 10.0);
    for p in [-1.0, 100.5, f64::NAN] {
        let (got, _) = run_p(ov::TA_PERCENTILE_NEAREST_RANK, 10, &xs, false, Some(p));
        assert!(got[9].is_nan(), "{p}");
    }
}

/// Operator ruling 2026-10-16 (`pin:04-median-int-provisional`): the `int` overload of
/// `ta.median` returns `series int`, an even window giving the lower middle value — a member.
#[test]
fn ta_median_int_is_the_lower_middle_skipping_na() {
    let xs = tape(3000, true);
    let lower = |w: &[f64]| {
        let v = sorted(w);
        v[(v.len() - 1) / 2]
    };
    for len in [1u32, 2, 5, 6, 31, 64] {
        let (got, _) = run(ov::TA_MEDIAN_I, len, &xs, true);
        same(&got, &bf_skip(&xs, len as usize, lower), "median int");
    }
    let xs: Vec<f64> = [3.0, 7.0].to_vec();
    let (got, _) = run(ov::TA_MEDIAN_I, 2, &xs, true);
    assert_eq!(got[1], 3.0, "lower middle, not the mean 5");
}

/// C = 1 linear interpolation (Excel `PERCENTILE.INC`, Hyndman–Fan 7) over the sorted window.
fn bf_linear(w: &[f64], p: f64) -> f64 {
    if !(0.0..=100.0).contains(&p) {
        return f64::NAN;
    }
    let v = sorted(w);
    // C = 1/2 (`pin:04-percentile-linear-c-half`, CI-015b): rank `p·N/100 − 1/2`, clamped.
    let x = (p * v.len() as f64) / 100.0 - 0.5;
    let last = (v.len() - 1) as f64;
    let x = if x < 0.0 {
        0.0
    } else if x > last {
        last
    } else {
        x
    };
    let k = x.floor() as usize;
    let frac = x - k as f64;
    if frac == 0.0 || k + 1 == v.len() {
        v[k]
    } else {
        v[k] + frac * (v[k + 1] - v[k])
    }
}

/// CI-015b (B096; `pin:04-percentile-linear-c-half` retires `pin:04-percentile-linear-provisional`,
/// `pin:04-percentile-linear-na` kept): `ta.percentile_linear_interpolation` is the C = 1/2
/// interpolation over the newest `length` executions; a na among them gives na (the reference:
/// "`na` values ... are included in calculations and will produce an `na` result"); it
/// allocates nothing.
#[test]
fn ta_percentile_linear_interpolation_is_c_half_and_propagates_na() {
    for holes in [false, true] {
        let base = tape(1500, holes);
        let xs: Vec<f64> = base
            .iter()
            .enumerate()
            .map(|(k, v)| v * 1.37 + (k % 5) as f64 * 0.01)
            .collect();
        for len in [1u32, 2, 4, 10, 33] {
            let l = len as usize;
            for p in [
                0.0,
                10.0,
                25.0,
                33.3,
                50.0,
                87.5,
                100.0,
                -1.0,
                100.5,
                f64::NAN,
            ] {
                let want: Vec<f64> = (0..xs.len())
                    .map(|t| {
                        if t + 1 < l || xs[t + 1 - l..=t].iter().any(|v| v.is_nan()) {
                            return f64::NAN;
                        }
                        bf_linear(&xs[t + 1 - l..=t], p)
                    })
                    .collect();
                let (got, allocs) = run_p(
                    ov::TA_PERCENTILE_LINEAR_INTERPOLATION,
                    len,
                    &xs,
                    false,
                    Some(p),
                );
                same(&got, &want, "percentile_linear_interpolation");
                assert_eq!(allocs, 0);
            }
        }
    }
    // 1..=10 at 25 %: x = 2.5 − 0.5 = 2.0 → 3; at 35 %: x = 3.0 → 4; at 30 %: x = 2.5 → 3.5.
    let xs: Vec<f64> = (1..=10).map(|v| v as f64).collect();
    for (p, want) in [
        (25.0, 3.0),
        (35.0, 4.0),
        (30.0, 3.5),
        (2.0, 1.0),
        (99.0, 10.0),
    ] {
        let (got, _) = run_p(
            ov::TA_PERCENTILE_LINEAR_INTERPOLATION,
            10,
            &xs,
            false,
            Some(p),
        );
        assert_eq!(got[9], want, "p = {p}");
    }
    // B096 row 13: `ta.percentile_linear_interpolation(syn, 14, 2.0)` over SYN-1's first 14
    // values is their minimum, 1089.3 (C = 1 gave 1095.3892).
    let syn = syn1(14);
    let (got, _) = run_p(
        ov::TA_PERCENTILE_LINEAR_INTERPOLATION,
        14,
        &syn,
        false,
        Some(2.0),
    );
    assert_eq!(got[13], 1089.3);
}

/// SYN-1's first `n` `syn` values (impl-06 §4.1.1, the prelude's exact binary64 order).
fn syn1(n: usize) -> Vec<f64> {
    let (mut s, mut out) = (1.0f64, Vec::with_capacity(n));
    while out.len() < n {
        let hi = (s / 127773.0).floor();
        let lo = s - hi * 127773.0;
        let t = 16807.0 * lo - 2836.0 * hi;
        s = if t > 0.0 { t } else { t + 2147483647.0 };
        out.push(1000.0 + (s - (s / 100000.0).floor() * 100000.0) / 100.0);
    }
    out
}
