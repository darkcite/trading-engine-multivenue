// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The law against the MEASURED law (the Python replicator that matched
//! the venue's posted settlements on 2026-09-25), its edge cases, the
//! grid's sample-and-hold semantics and the refusals.

use super::*;

/// The reference series: 1 800 grid points of a trending, spiky
/// underlying (an LCG so both languages regenerate it exactly), ×1e6.
fn lcg_series() -> Vec<i64> {
    let mut s: u64 = 20_260_925;
    let mut px: i64 = 224_000_000;
    let mut out = Vec::with_capacity(GRID_POINTS);
    let mut i = 0usize;
    while i < GRID_POINTS {
        s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        let step = ((s >> 33) % 95_001) as i64 - 40_000;
        px += step;
        out.push(px + if i % 211 == 7 { 3_000_000 } else { 0 });
        i += 1;
    }
    out
}

fn mom(v: &[i64], order: BucketOrder) -> Option<i64> {
    let mut scratch = vec![0i64; v.len().max(1)];
    median_of_means(v, order, &mut scratch)
}

#[test]
fn the_law_matches_the_measured_replicator_on_a_trending_spiky_series() {
    let v = lcg_series();
    assert_eq!(v[..3], [224_050_751, 224_035_445, 224_041_956]);
    assert_eq!(v[GRID_POINTS - 1], 237_348_491);
    // The 2026-09-25 replicator (float) says sorted 230730638.45, time
    // 230747519.175; the fixed-point law truncates each bucket mean, so
    // it sits within two units (2e-6 of a price unit) below — and it is
    // pinned EXACTLY, beside the Python mirror's identical pin
    // (claude-worker tests/test_hypercall_settle.py, the cross-language
    // table: change either side only with the other).
    let sorted = mom(&v, BucketOrder::Sorted).unwrap();
    let time = mom(&v, BucketOrder::Time).unwrap();
    assert_eq!((sorted, time), (230_730_637, 230_747_518));
    // The plain mean is a different estimator (the measured miss was up
    // to 6.5 bp on a trending underlying).
    let mean = v.iter().sum::<i64>() / v.len() as i64;
    assert_eq!(mean, 230_638_973);
    assert_ne!(mean, sorted);
}

#[test]
fn small_windows_are_exact() {
    const U: i64 = 1_000_000;
    for order in [BucketOrder::Sorted, BucketOrder::Time] {
        assert_eq!(mom(&[5 * U], order), Some(5 * U));
        assert_eq!(mom(&[3 * U, U, 2 * U], order), Some(2 * U));
        // k = 2 buckets: [10, 20] and [30, 40] -> the mean of 15 and 35.
        assert_eq!(mom(&[10 * U, 20 * U, 30 * U, 40 * U], order), Some(25 * U));
        // n = 20: trim 1 each side, n' = 18, k = 4 of 4 (the last takes 6):
        // 3.5, 7.5, 11.5, 16.5 -> 9.5.
        let ramp: Vec<i64> = (1..=20).map(|x| x * U).collect();
        assert_eq!(mom(&ramp, order), Some(9 * U + U / 2));
        assert_eq!(mom(&[7 * U; 50], order), Some(7 * U));
    }
}

#[test]
fn the_variants_differ_only_in_how_buckets_are_cut() {
    // Sorted buckets are order-invariant; time buckets follow the path.
    let v = lcg_series();
    let mut rev = v.clone();
    rev.reverse();
    assert_eq!(mom(&v, BucketOrder::Sorted), mom(&rev, BucketOrder::Sorted));
    assert_ne!(mom(&v, BucketOrder::Sorted), mom(&v, BucketOrder::Time));
}

#[test]
fn outliers_inside_the_trim_do_not_move_the_estimate_outside_the_clean_range() {
    let clean: Vec<i64> = (0..400).map(|i| 100_000_000 + (i % 7) * 10_000).collect();
    let mut dirty = clean.clone();
    for i in (0..400).step_by(40) {
        dirty[i] = 900_000_000; // 2.5 % far above
        dirty[i + 1] = 1; // 2.5 % far below
    }
    let (lo, hi) = (100_000_000, 100_060_000);
    for order in [BucketOrder::Sorted, BucketOrder::Time] {
        let x = mom(&dirty, order).unwrap();
        assert!((lo..=hi).contains(&x), "{order:?}: {x}");
    }
}

#[test]
fn isqrt_is_the_floor_root() {
    let mut n = 0usize;
    while n <= GRID_POINTS {
        let k = isqrt(n);
        assert!(k * k <= n && (k + 1) * (k + 1) > n, "{n}");
        n += 1;
    }
}

#[test]
fn nothing_to_settle_or_too_little_scratch_is_none() {
    let mut scratch = [0i64; 4];
    assert_eq!(median_of_means(&[], BucketOrder::Sorted, &mut scratch), None);
    assert_eq!(median_of_means(&[1; 5], BucketOrder::Time, &mut scratch), None);
}

#[test]
fn the_grid_holds_each_price_until_the_next_and_carries_one_in() {
    const T: u64 = 1_790_366_400_000; // 2026-09-25 20:00Z
    let t0 = T - SETTLE_WINDOW_MS;
    let mut w = SettleWindow::new(T);
    // A print before the window carries in; a print exactly on a grid
    // instant counts at that instant (stamped at or before it).
    w.push(t0 - 5_000, 100).unwrap();
    w.push(t0 + 2_000, 200).unwrap();
    w.push(t0 + 2_500, 300).unwrap();
    w.push(T - 1_000, 400).unwrap();
    let g = w.finish();
    assert_eq!(g.len(), GRID_POINTS);
    assert_eq!(g[0], 100, "t0 + 1 s: the carried price");
    assert_eq!(g[1], 200, "t0 + 2 s: the print at exactly 2 s");
    assert_eq!(g[2], 300, "t0 + 3 s: the latest print at or before it");
    assert_eq!(g[GRID_POINTS - 3], 300);
    assert_eq!(g[GRID_POINTS - 2], 400, "T - 1 s");
    assert_eq!(g[GRID_POINTS - 1], 400, "T: held");
    assert_eq!(w.finish().len(), GRID_POINTS, "idempotent");
    assert_eq!(w.points(), GRID_POINTS);
}

#[test]
fn a_feed_that_starts_late_covers_only_the_rest_of_the_window() {
    const T: u64 = 1_790_366_400_000;
    let mut w = SettleWindow::new(T);
    w.push(T - 60_000, 5).unwrap(); // one minute before expiry
    assert_eq!(w.finish().len(), 61, "T - 60 s .. T: stamped at or before each");
    let mut scratch = [0i64; GRID_POINTS];
    assert_eq!(w.settle_1e6(BucketOrder::Sorted, &mut scratch), Some(5));
    // Nothing ever arrived: no estimate.
    let mut empty = SettleWindow::new(T);
    assert_eq!(empty.settle_1e6(BucketOrder::Time, &mut scratch), None);
}

#[test]
fn refusals_leave_the_window_unchanged() {
    const T: u64 = 1_790_366_400_000;
    let mut w = SettleWindow::new(T);
    w.push(T - 10_000, 7).unwrap();
    assert_eq!(w.push(T - 9_000, 0), Err(PushRefused::NonPositive));
    assert_eq!(w.push(T - 11_000, 8), Err(PushRefused::OutOfOrder));
    assert_eq!(w.push(T + 1, 9), Err(PushRefused::AfterExpiry));
    w.push(T, 9).unwrap(); // exactly at expiry is inside
    let g = w.finish();
    assert_eq!(g.len(), 11, "T - 10 s .. T");
    assert!(g[..10].iter().all(|&x| x == 7));
    assert_eq!(g[10], 9);
    // Reuse for the next expiry.
    w.reset(T + 86_400_000);
    assert_eq!(w.points(), 0);
    assert_eq!(w.t_end_ms(), T + 86_400_000);
    assert!(w.finish().is_empty());
}

proptest::proptest! {
    #[test]
    fn the_estimate_lies_within_the_samples_and_sorted_ignores_order(
        mut v in proptest::collection::vec(1i64..1_000_000_000_000, 1..GRID_POINTS),
        seed in 0u64..u64::MAX,
    ) {
        let (min, max) = (*v.iter().min().unwrap(), *v.iter().max().unwrap());
        let a = mom(&v, BucketOrder::Sorted).unwrap();
        let b = mom(&v, BucketOrder::Time).unwrap();
        proptest::prop_assert!(min <= a && a <= max);
        proptest::prop_assert!(min <= b && b <= max);
        // Fisher-Yates with a fixed LCG: any reordering, same Sorted answer.
        let mut s = seed | 1;
        let mut i = v.len();
        while i > 1 {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let j = (s >> 33) as usize % i;
            i -= 1;
            v.swap(i, j);
        }
        proptest::prop_assert_eq!(mom(&v, BucketOrder::Sorted).unwrap(), a);
    }

    #[test]
    fn a_constant_window_settles_at_its_constant(x in 1i64..i64::MAX / 4, n in 1usize..GRID_POINTS) {
        let v = vec![x; n];
        proptest::prop_assert_eq!(mom(&v, BucketOrder::Sorted), Some(x));
        proptest::prop_assert_eq!(mom(&v, BucketOrder::Time), Some(x));
    }
}
