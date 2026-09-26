// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The long-tenor law against an independent re-derivation from the
//! engine's own day sums, the day and pair clocks, the restore round
//! trip, and the lifted fit against the engine it was lifted from.
//! Test-only code: allocation, iterators, floats and `unwrap` are fine.

use super::*;
use crate::{MIN_PAIRS, X_SPREAD_MIN_1E9};

/// 2026-01-01 00:00Z.
const DAY0: u64 = 1_767_225_600_000;

fn tau(days: u64) -> u64 {
    days * DAY_NS
}

/// A deterministic price walk whose per-minute step size follows a slow
/// vol REGIME (a 40-day triangle, plus a little day-to-day noise), so
/// the fold's regressor really varies AND predicts the vol that follows
/// — the fits have real slopes, and a slope-term bug cannot hide
/// behind a slope clamped to zero.
struct Walk {
    s: u64,
    px: i64,
}

impl Walk {
    fn new(seed: u64) -> Self {
        Self {
            s: seed,
            px: 79_000_000_000,
        }
    }

    fn next(&mut self, day: u64) -> i64 {
        self.s = self
            .s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let phase = day % 40;
        let level = if phase < 20 { phase } else { 40 - phase };
        let amp = 2_000_000 * (4 + level + (day * 7) % 3) as i64;
        let step = ((self.s >> 32) % (2 * amp as u64 + 1)) as i64 - amp;
        self.px = (self.px + step).max(1_000_000_000);
        self.px
    }
}

/// `days` full UTC days of minute closes from `DAY0 + first · DAY`.
fn feed(e: &mut LongVolEngine, w: &mut Walk, first: u64, days: u64) {
    for d in first..first + days {
        for m in 0..u64::from(DAY_MINUTES) {
            e.on_minute_close_at(w.next(d), DAY0 + d * DAY_MS + m * 60_000);
        }
    }
}

/// The first minute of day `d` (closes day `d − 1`).
fn tick_into(e: &mut LongVolEngine, w: &mut Walk, d: u64) {
    e.on_minute_close_at(w.next(d), DAY0 + d * DAY_MS);
}

fn boxed() -> Box<LongVolEngine> {
    Box::new(LongVolEngine::new())
}

/// The fold re-derived from the engine's resident day sums.
fn ref_x(e: &LongVolEngine, tau_days: u64) -> Option<i64> {
    let n = e.n_resident();
    if (n as u64) < LONG_WARM_DAYS {
        return None;
    }
    let back = |k: usize| e.day_at(n - 1 - k).unwrap().1;
    let mut mean_sq = 0i128;
    for w in LONG_WINDOWS_DAYS {
        let s: i128 = (0..w as usize).map(back).sum();
        let rv = i128::from(core_regime::math::isqrt_i128(s));
        mean_sq += rv * rv / (i128::from(w) * 1440);
    }
    let har = core_regime::math::isqrt_i128(mean_sq / 3 * i128::from(tau_days as u32) * 1440);
    if har <= 0 {
        return None;
    }
    let x = fx::ln_1e9(har as u64);
    (x != fx::LOG2_UNDEFINED).then_some(x)
}

/// `ln(isqrt(Σ))` over resident days `[from, from + len)`.
fn ref_y(e: &LongVolEngine, from: usize, len: usize) -> i64 {
    let s: i128 = (from..from + len).map(|i| e.day_at(i).unwrap().1).sum();
    fx::ln_1e9(core_regime::math::isqrt_i128(s) as u64)
}

fn resident_index(e: &LongVolEngine, day_ts: u64) -> Option<usize> {
    (0..e.n_resident()).find(|&i| e.day_at(i).unwrap().0 == day_ts)
}

// ---------------------------------------------------------------------
// The grid and its constants
// ---------------------------------------------------------------------

#[test]
fn the_annualisers_are_the_crate_law_on_every_grid_day() {
    // The plan's three, pinned (computed 2026-09-19, re-derived here).
    assert_eq!(ANNUALISE_LONG_1E9[0], 19_111_514_854, "1 d");
    assert_eq!(ANNUALISE_LONG_1E9[6], 7_223_473_640, "1 w");
    assert_eq!(ANNUALISE_LONG_1E9[29], 3_489_269_264, "1 M");
    // The same integer law reproduces VolEngine's three constants.
    assert_eq!(annualiser_1e9(15), crate::ANNUALISE_15M_1E9);
    assert_eq!(annualiser_1e9(240), crate::ANNUALISE_4H_1E9);
    assert_eq!(annualiser_1e9(480), crate::ANNUALISE_8H_1E9);
    // And the float reference agrees on the whole grid.
    for d in 1..=LONG_TAU_DAYS_MAX {
        let f = ((525_960.0 / (1440.0 * d as f64)).sqrt() * 1e9).round() as i64;
        assert_eq!(ANNUALISE_LONG_1E9[d - 1], f, "{d} d");
        let t = long_tenor_of(tau(d as u64)).unwrap();
        assert_eq!(t.annualise_1e9, f);
    }
}

#[test]
fn the_grid_is_every_whole_day_from_one_to_forty_and_nothing_else() {
    for d in 1..=LONG_TAU_DAYS_MAX as u64 {
        let t = long_tenor_of(tau(d)).unwrap();
        assert_eq!((t.tau_days, t.tau_min), (d as u32, d as i64 * 1440));
    }
    for ns in [
        0,
        tau(41),
        DAY_NS + 1,
        DAY_NS - 1,
        900_000_000_000,
        14_400_000_000_000,
        28_800_000_000_000,
        43_200_000_000_000,
        u64::MAX,
    ] {
        assert_eq!(long_tenor_of(ns), None, "{ns}");
    }
    // The short engine's tenors are not this engine's, and vice versa.
    assert!(crate::tenor_of(tau(1)).is_none());
}

// ---------------------------------------------------------------------
// The day clock
// ---------------------------------------------------------------------

#[test]
fn a_day_closes_when_the_first_minute_of_the_next_arrives() {
    let mut e = boxed();
    let mut w = Walk::new(7);
    let mut sq = 0i128;
    let mut prev = 0i64;
    for m in 0..1440u64 {
        let px = w.next(0);
        if prev > 0 {
            let r = i128::from(core_regime::math::ret_bps_1e9(prev, px));
            sq += r * r;
        }
        prev = px;
        e.on_minute_close_at(px, DAY0 + m * 60_000);
    }
    // The very first close only primes: 1 439 returns.
    assert_eq!(e.n_days, 0);
    assert_eq!(e.open_day(), Some((DAY0, sq, 1439)));
    // The 00:00 bar of the next day closes day 0; its return spans
    // midnight and belongs to the NEW day.
    let px = w.next(1);
    e.on_minute_close_at(px, DAY0 + DAY_MS);
    assert_eq!(e.n_days, 1);
    assert_eq!(e.day_at(0), Some((DAY0, sq, 1439)));
    let r = i128::from(core_regime::math::ret_bps_1e9(prev, px));
    assert_eq!(e.open_day(), Some((DAY0 + DAY_MS, r * r, 1)));
    // A complete day after that is 1 440 returns.
    for m in 1..1440u64 {
        e.on_minute_close_at(w.next(1), DAY0 + DAY_MS + m * 60_000);
    }
    tick_into(&mut e, &mut w, 2);
    assert_eq!(e.day_at(1).unwrap().2, 1440);
    assert_eq!((e.gaps(), e.refused()), (0, 0));
}

#[test]
fn refusals_are_counted_and_change_nothing() {
    let mut e = boxed();
    let mut w = Walk::new(11);
    feed(&mut e, &mut w, 0, 2);
    let open = e.open_day();
    let (last, prev) = (e.last_min_ts_ms(), e.prev_px_1e6());
    e.on_minute_close_at(0, last + 60_000); // not a price
    e.on_minute_close_at(-5, last + 60_000);
    e.on_minute_close_at(prev, last); // the same minute again
    e.on_minute_close_at(prev, last - 60_000); // an older one
    e.on_minute_close_at(prev, 0); // no clock
    assert_eq!(e.refused(), 5);
    assert_eq!(e.open_day(), open);
    assert_eq!(
        (e.last_min_ts_ms(), e.prev_px_1e6(), e.n_days),
        (last, prev, 1)
    );
    // A restored engine refuses a live minute from a day it already
    // holds CLOSED.
    let mut r = boxed();
    assert!(r.seed_day(DAY0, 5, 1440));
    r.on_minute_close_at(prev, DAY0 + 600_000);
    assert_eq!((r.refused(), r.open_day()), (1, None));
}

#[test]
fn a_hole_is_a_gap_and_the_next_return_spans_it() {
    let mut e = boxed();
    e.on_minute_close_at(100_000_000, DAY0);
    e.on_minute_close_at(101_000_000, DAY0 + 60_000);
    // Five minutes missing.
    e.on_minute_close_at(103_000_000, DAY0 + 7 * 60_000);
    assert_eq!(e.gaps(), 1);
    let r1 = i128::from(core_regime::math::ret_bps_1e9(100_000_000, 101_000_000));
    let r2 = i128::from(core_regime::math::ret_bps_1e9(101_000_000, 103_000_000));
    assert_eq!(e.open_day(), Some((DAY0, r1 * r1 + r2 * r2, 2)));
}

#[test]
fn silent_days_are_pushed_empty_through_the_close_law() {
    let mut e = boxed();
    let mut w = Walk::new(3);
    feed(&mut e, &mut w, 0, 1);
    // Nothing on days 1 and 2; the next minute is on day 3.
    tick_into(&mut e, &mut w, 3);
    assert_eq!(e.n_days, 3);
    assert_eq!(e.day_at(1), Some((DAY0 + DAY_MS, 0, 0)));
    assert_eq!(e.day_at(2), Some((DAY0 + 2 * DAY_MS, 0, 0)));
    // The empty-day law: no return ACROSS an unobserved day — the first
    // close after the silence only primes.
    assert_eq!(e.open_day(), Some((DAY0 + 3 * DAY_MS, 0, 0)));
    assert_eq!(e.gaps(), 1);
}

#[test]
fn a_silence_of_a_whole_ring_clears_the_days_and_keeps_the_history() {
    let mut e = boxed();
    let mut w = Walk::new(5);
    feed(&mut e, &mut w, 0, 100);
    tick_into(&mut e, &mut w, 100);
    let pairs = e.n_pairs(tau(1));
    assert!(pairs >= MIN_PAIRS);
    // The next minute arrives DAY_RING days after the day after the open
    // one: day 100 closes normally (one more 1 d pair), then the silence
    // is a whole ring long.
    let back = 100 + 1 + DAY_RING as u64;
    let prev = e.prev_px_1e6();
    e.on_minute_close_at(prev, DAY0 + back * DAY_MS);
    assert_eq!(e.n_days, 0, "cleared");
    assert!(!e.is_warm());
    assert_eq!(e.n_resident(), 0);
    assert_eq!(e.x_1e9(tau(1)), None);
    // History survives; the prime does not (months are not a return).
    assert_eq!(e.n_pairs(tau(1)), pairs + 1);
    assert!(e.fit(tau(1)).is_some());
    assert_eq!(e.open_day(), Some((DAY0 + back * DAY_MS, 0, 0)));
    // One day short of a whole ring is filled instead — and, the silence
    // being unobserved days, the close after it only primes.
    let mut f = boxed();
    feed(&mut f, &mut w, 0, 2);
    let prev = f.prev_px_1e6();
    f.on_minute_close_at(prev, DAY0 + (1 + DAY_RING as u64) * DAY_MS);
    assert_eq!(f.n_days, 1 + DAY_RING as u64);
    assert_eq!(
        f.day_at(f.n_resident() - 1),
        Some((DAY0 + DAY_RING as u64 * DAY_MS, 0, 0))
    );
    assert_eq!(f.open_day().unwrap().2, 0);
}

#[test]
fn an_unobserved_day_holds_the_fold_and_every_target_through_it() {
    let mut e = boxed();
    let mut w = Walk::new(61);
    feed(&mut e, &mut w, 0, 100);
    tick_into(&mut e, &mut w, 100);
    break_even(&mut e, &mut w, 100);
    assert!(e.is_warm());
    let before: Vec<usize> = (1..=LONG_TAU_DAYS_MAX as u64).map(|d| e.n_pairs(tau(d))).collect();
    // Day 101 is never observed; the feed resumes on day 102.
    tick_into(&mut e, &mut w, 102);
    assert_eq!(e.day_at(e.n_resident() - 1), Some((DAY0 + 101 * DAY_MS, 0, 0)));
    assert!(!e.is_warm(), "the hole is inside the fold's window");
    for d in 1..=LONG_TAU_DAYS_MAX as u64 {
        assert_eq!(e.x_1e9(tau(d)), None, "{d} d: no arm across the hole");
    }
    // Closing day 100 settled every tenor normally (one more pair each);
    // closing the empty day 101 formed nothing (every target holds it).
    let after: Vec<usize> = (1..=LONG_TAU_DAYS_MAX as u64).map(|d| e.n_pairs(tau(d))).collect();
    for i in 0..LONG_TAU_DAYS_MAX {
        assert_eq!(after[i], (before[i] + 1).min(PAIR_RING_LONG), "{} d", i + 1);
    }
    // Twenty-nine observed days later the window still holds the hole...
    break_even(&mut e, &mut w, 102);
    feed(&mut e, &mut w, 103, 28);
    tick_into(&mut e, &mut w, 131);
    assert!(!e.is_warm());
    // ...and at the thirtieth close past it the fold is whole again.
    break_even(&mut e, &mut w, 131);
    tick_into(&mut e, &mut w, 132);
    assert!(e.is_warm());
    assert!(e.x_1e9(tau(1)).is_some() && e.x_1e9(tau(40)).is_some());
    // No pair ever formed across day 101: the 1 d tenor's targets skip it.
    let targets: Vec<u64> = (0..e.n_pairs(tau(1))).map(|i| e.pair_at(tau(1), i).unwrap().0).collect();
    assert!(!targets.contains(&(DAY0 + 101 * DAY_MS)));
}

#[test]
fn a_short_day_is_kept_and_flagged_by_its_count() {
    let mut e = boxed();
    let mut w = Walk::new(13);
    feed(&mut e, &mut w, 0, 1);
    // Day 1: the feed dies at 11:40Z.
    for m in 0..700u64 {
        e.on_minute_close_at(w.next(1), DAY0 + DAY_MS + m * 60_000);
    }
    tick_into(&mut e, &mut w, 2);
    let (ts, sq, n) = e.day_at(1).unwrap();
    assert_eq!((ts, n), (DAY0 + DAY_MS, 700));
    assert!(sq > 0);
    // The day-2 prime spans the hole: its return is in day 2.
    assert_eq!(e.open_day().unwrap().2, 1);
    assert_eq!(e.gaps(), 1);
}

#[test]
fn a_minute_inside_a_day_touches_no_pair_and_no_arm() {
    let mut e = boxed();
    let mut w = Walk::new(17);
    feed(&mut e, &mut w, 0, 40);
    tick_into(&mut e, &mut w, 40);
    let snap: Vec<_> = (1..=LONG_TAU_DAYS_MAX as u64)
        .map(|d| (e.n_pairs(tau(d)), e.x_1e9(tau(d)), e.fit(tau(d))))
        .collect();
    for m in 1..1440u64 {
        e.on_minute_close_at(w.next(40), DAY0 + 40 * DAY_MS + m * 60_000);
    }
    let after: Vec<_> = (1..=LONG_TAU_DAYS_MAX as u64)
        .map(|d| (e.n_pairs(tau(d)), e.x_1e9(tau(d)), e.fit(tau(d))))
        .collect();
    assert_eq!(snap, after);
    assert_eq!(e.n_days, 40);
}

// ---------------------------------------------------------------------
// The fold and the pair clock
// ---------------------------------------------------------------------

#[test]
fn nothing_forecasts_before_thirty_days_and_everything_does_at_thirty() {
    let mut e = boxed();
    let mut w = Walk::new(19);
    feed(&mut e, &mut w, 0, 29);
    tick_into(&mut e, &mut w, 29);
    assert_eq!(e.n_days, 29);
    assert!(!e.is_warm());
    for d in 1..=LONG_TAU_DAYS_MAX as u64 {
        assert_eq!(e.x_1e9(tau(d)), None, "{d} d");
        assert_eq!(e.sigma_ann_1e9(tau(d), LongForecast::Raw), None);
    }
    break_even(&mut e, &mut w, 29);
    tick_into(&mut e, &mut w, 30);
    assert!(e.is_warm());
    for d in 1..=LONG_TAU_DAYS_MAX as u64 {
        assert!(e.x_1e9(tau(d)).is_some(), "{d} d");
        assert_eq!(e.n_pairs(tau(d)), 0, "no hold has run its course");
    }
}

#[test]
fn the_regressor_is_the_documented_fold_on_every_tenor() {
    let mut e = boxed();
    let mut w = Walk::new(23);
    feed(&mut e, &mut w, 0, 30);
    for d in 30..=70u64 {
        tick_into(&mut e, &mut w, d);
        for t in 1..=LONG_TAU_DAYS_MAX as u64 {
            let x = e.x_1e9(tau(t));
            assert!(x.is_some(), "day {d}, {t} d");
            assert_eq!(x, ref_x(&e, t), "day {d}, {t} d");
        }
        // Longer tenors forecast more vol: x rises with τ.
        assert!(e.x_1e9(tau(40)).unwrap() > e.x_1e9(tau(1)).unwrap());
        break_even(&mut e, &mut w, d);
    }
}

/// The rest of day `d` after its first minute.
fn break_even(e: &mut LongVolEngine, w: &mut Walk, d: u64) {
    for m in 1..u64::from(DAY_MINUTES) {
        e.on_minute_close_at(w.next(d), DAY0 + d * DAY_MS + m * 60_000);
    }
}

#[test]
fn every_tenor_forms_its_first_pair_tau_days_after_the_first_warm_close() {
    let mut e = boxed();
    let mut w = Walk::new(29);
    feed(&mut e, &mut w, 0, 75);
    tick_into(&mut e, &mut w, 75);
    // The first warm close is global day 29; an arm at g settles at
    // g + τ, so after n closes tenor τ holds n − 29 − τ pairs.
    let n = e.n_days;
    assert_eq!(n, 75);
    for d in 1..=LONG_TAU_DAYS_MAX as u64 {
        assert_eq!(
            e.n_pairs(tau(d)) as u64,
            (n - 29).saturating_sub(d),
            "{d} d"
        );
    }
}

#[test]
fn overlapping_holds_pair_each_arm_with_the_tau_days_after_it() {
    let mut e = boxed();
    let mut w = Walk::new(31);
    feed(&mut e, &mut w, 0, 80);
    tick_into(&mut e, &mut w, 80);
    for tau_days in [1usize, 7, 30, 40] {
        let t = tau(tau_days as u64);
        let np = e.n_pairs(t);
        assert!(np > 0, "{tau_days} d");
        let mut checked = 0;
        for i in 0..np {
            let (target, x, y) = e.pair_at(t, i).unwrap();
            // The arm was made at the close of the day before the target.
            let Some(j) = resident_index(&e, target - DAY_MS) else {
                continue;
            };
            assert_eq!(
                e.arm_at(j, t).unwrap().0,
                x,
                "{tau_days} d pair {i}: the armed x"
            );
            assert_eq!(
                y,
                ref_y(&e, j + 1, tau_days),
                "{tau_days} d pair {i}: the realised y"
            );
            checked += 1;
        }
        assert!(checked > 0);
        // Consecutive pairs step by exactly one day: overlapping holds.
        let (t0, _, _) = e.pair_at(t, 0).unwrap();
        let (t1, _, _) = e.pair_at(t, np - 1).unwrap();
        assert_eq!(t1 - t0, (np as u64 - 1) * DAY_MS);
    }
}

#[test]
fn the_one_day_tenor_settles_each_close_against_the_next_day() {
    let mut e = boxed();
    let mut w = Walk::new(37);
    feed(&mut e, &mut w, 0, 35);
    tick_into(&mut e, &mut w, 35);
    let np = e.n_pairs(tau(1));
    assert_eq!(np, 35 - 29 - 1);
    let (target, _, y) = e.pair_at(tau(1), np - 1).unwrap();
    let j = resident_index(&e, target).unwrap();
    let rv = core_regime::math::isqrt_i128(e.day_at(j).unwrap().1);
    assert_eq!(y, fx::ln_1e9(rv as u64));
}

// ---------------------------------------------------------------------
// The fit
// ---------------------------------------------------------------------

/// `n` LCG pairs on a noisy line `y ≈ 0.7·x + c`, log-vol scale.
fn lcg_pairs(n: usize, seed: u64) -> Vec<(i64, i64)> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let x = 23_000_000_000 + ((s >> 33) % 900_000_000) as i64;
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let noise = ((s >> 33) % 400_000_000) as i64 - 200_000_000;
            (x, x * 7 / 10 + 6_000_000_000 + noise)
        })
        .collect()
}

#[test]
fn long_refit_equals_vol_refit_on_the_same_ring() {
    for (n, seed) in [
        (60usize, 1u64),
        (61, 2),
        (97, 3),
        (128, 4),
        (129, 5),
        (300, 6),
    ] {
        let pairs = lcg_pairs(n, seed);
        let mut v = crate::VolEngine::new();
        let mut l = boxed();
        for &(x, y) in &pairs {
            v.seed_pair(x, y);
            assert!(l.seed_pair(tau(1), DAY0, x, y));
            assert!(l.seed_pair(tau(40), DAY0, x, y));
        }
        l.refresh();
        let want = v.fit();
        assert!(want.is_some(), "n={n}");
        assert_eq!(l.fit(tau(1)), want, "n={n}: 1 d");
        assert_eq!(l.fit(tau(40)), want, "n={n}: 40 d");
        // And the lifted body directly, on the ring as VolEngine holds it
        // (the newest PAIR_RING pairs, in any order).
        let tail = &pairs[n.saturating_sub(crate::PAIR_RING)..];
        let xs: Vec<i64> = tail.iter().map(|p| p.0).collect();
        let ys: Vec<i64> = tail.iter().map(|p| p.1).collect();
        assert_eq!(ols_fit_1e9(&xs, &ys), want, "n={n}: ols_fit_1e9");
    }
}

#[test]
fn the_lifted_fit_keeps_every_refusal_and_the_clamp() {
    let xs: Vec<i64> = (0..60).map(|i| 20_000_000_000 + i * 100_000_000).collect();
    let line = |b: i64| -> Vec<i64> { xs.iter().map(|&x| x / 1_000 * b / 1_000_000).collect() };
    // Too few pairs.
    assert_eq!(ols_fit_1e9(&xs[..59], &line(700_000_000)[..59]), None);
    // A regressor with no spread, then one with too little.
    assert_eq!(ols_fit_1e9(&[xs[0]; 60], &line(700_000_000)), None);
    let tight: Vec<i64> = (0..60)
        .map(|i| 20_000_000_000 + (i % 2) * (X_SPREAD_MIN_1E9 / 2))
        .collect();
    assert_eq!(ols_fit_1e9(&tight, &line(700_000_000)), None);
    // A healthy line is recovered exactly.
    let (a, b) = ols_fit_1e9(&xs, &line(700_000_000)).unwrap();
    assert_eq!((a, b), (0, 700_000_000));
    // A slope of 3 is clamped to 2 and the intercept follows the clamp;
    // a negative slope is clamped to 0 and the intercept is ȳ.
    let steep = line(3_000_000_000);
    let (a, b) = ols_fit_1e9(&xs, &steep).unwrap();
    let mean = |v: &[i64]| {
        v.iter()
            .map(|&x| i128::from(x))
            .sum::<i128>()
            .div_euclid(60) as i64
    };
    assert_eq!(b, crate::B_MAX_1E9);
    assert_eq!(a, mean(&steep) - 2 * mean(&xs));
    let falling: Vec<i64> = xs.iter().map(|&x| 50_000_000_000 - x).collect();
    let (a, b) = ols_fit_1e9(&xs, &falling).unwrap();
    assert_eq!((a, b), (mean(&falling), crate::B_MIN_1E9));
}

#[test]
fn a_tenor_fits_from_min_pairs_and_follows_its_rolling_ring() {
    let mut e = boxed();
    let mut w = Walk::new(41);
    // 29 + 1 + 59 closes: the 1 d tenor holds 59 pairs — no fit yet.
    feed(&mut e, &mut w, 0, 89);
    tick_into(&mut e, &mut w, 89);
    assert_eq!(e.n_pairs(tau(1)), MIN_PAIRS - 1);
    assert_eq!(e.fit(tau(1)), None);
    assert_eq!(e.ln_sigma_fit_1e9(tau(1)), None);
    assert_eq!(e.sigma_ann_1e9(tau(1), LongForecast::Fit), None);
    break_even(&mut e, &mut w, 89);
    tick_into(&mut e, &mut w, 90);
    assert_eq!(e.n_pairs(tau(1)), MIN_PAIRS);
    let pairs: Vec<(u64, i64, i64)> = (0..MIN_PAIRS)
        .map(|i| e.pair_at(tau(1), i).unwrap())
        .collect();
    let xs: Vec<i64> = pairs.iter().map(|p| p.1).collect();
    let ys: Vec<i64> = pairs.iter().map(|p| p.2).collect();
    let fit = ols_fit_1e9(&xs, &ys);
    assert!(fit.is_some(), "the walk's vol regime varies enough to fit");
    assert_eq!(e.fit(tau(1)), fit);
    // The fitted forecast made at this close is the one armed with it.
    let (a, b) = fit.unwrap();
    assert!(
        b > 0 && b < crate::B_MAX_1E9,
        "a real, unclamped slope: {b}"
    );
    let x = e.x_1e9(tau(1)).unwrap();
    let want =
        a + core_regime::math::floor_div(i128::from(b) * i128::from(x), 1_000_000_000) as i64;
    assert_eq!(e.ln_sigma_fit_1e9(tau(1)), Some(want));
    assert_eq!(e.arm_at(e.n_resident() - 1, tau(1)), Some((x, want)));
}

#[test]
fn qlike_scores_the_raw_fold_against_the_fit_made_at_arming() {
    let mut e = boxed();
    let mut w = Walk::new(43);
    feed(&mut e, &mut w, 0, 140);
    tick_into(&mut e, &mut w, 140);
    for d in [1u64, 7, 20] {
        let t = tau(d);
        // The fit first existed at the close that formed pair 60; only
        // arms made from then on carry a fit, and only their
        // settlements score.
        let q = e.qlike_counters(t);
        let np = e.n_pairs(t);
        assert!(q.n > 0 && (q.n as usize) < np, "{d} d: {q:?} of {np}");
        // Each resident row is its pair's two forecasts — the raw fold
        // and the fit AS ARMED, τ closes before the pair formed (for
        // τ > 1 the line has moved since) — against its y.
        let mut checked = 0;
        for k in 0..q.n as usize {
            let (target, x, y) = e.pair_at(t, np - q.n as usize + k).unwrap();
            let Some(j) = resident_index(&e, target - DAY_MS) else {
                continue;
            };
            let (ax, afit) = e.arm_at(j, t).unwrap();
            assert_eq!(ax, x);
            assert_ne!(afit, NONE);
            assert_eq!(
                e.qlike_at(t, k),
                Some((qlike_1e9(y, x), qlike_1e9(y, afit))),
                "{d} d row {k}"
            );
            checked += 1;
        }
        assert!(checked > 0, "{d} d");
        // The means are the floored means of the rows.
        let rows: Vec<(i64, i64)> = (0..q.n as usize)
            .map(|i| e.qlike_at(t, i).unwrap())
            .collect();
        let mean = |f: fn(&(i64, i64)) -> i64| {
            rows.iter()
                .map(|r| i128::from(f(r)))
                .sum::<i128>()
                .div_euclid(rows.len() as i128) as i64
        };
        assert_eq!(q.raw_mean_1e9, mean(|r| r.0));
        assert_eq!(q.fit_mean_1e9, mean(|r| r.1));
        assert_eq!(
            q.fit_beats_raw,
            q.n as usize == QLIKE_RING && q.fit_mean_1e9 < q.raw_mean_1e9
        );
    }
}

#[test]
fn the_tell_arms_only_on_a_full_window() {
    let mut e = boxed();
    let t = tau(7);
    for _ in 0..QLIKE_RING - 1 {
        assert!(e.seed_qlike(t, 300_000_000, 200_000_000));
    }
    let q = e.qlike_counters(t);
    assert_eq!((q.n as usize, q.fit_beats_raw), (QLIKE_RING - 1, false));
    assert!(e.seed_qlike(t, 300_000_000, 200_000_000));
    let q = e.qlike_counters(t);
    assert_eq!(
        (q.n as usize, q.raw_mean_1e9, q.fit_mean_1e9),
        (QLIKE_RING, 300_000_000, 200_000_000)
    );
    assert!(q.fit_beats_raw);
    assert_eq!(e.qlike_counters(tau(8)), LongQlike::default());
    assert_eq!(e.qlike_counters(DAY_NS / 2), LongQlike::default());
}

#[test]
fn sigma_ann_is_the_vol_engine_law_with_the_tenor_annualiser() {
    let mut e = boxed();
    let mut w = Walk::new(47);
    feed(&mut e, &mut w, 0, 100);
    tick_into(&mut e, &mut w, 100);
    for d in [1u64, 7, 30] {
        let t = tau(d);
        let ann = i128::from(ANNUALISE_LONG_1E9[d as usize - 1]);
        let via = |ln: i64| {
            let rv = i128::from(fx::exp_1e9(ln));
            (rv * ann / crate::BPS_1E9_PER_UNIT) as i64
        };
        let raw = e.sigma_ann_1e9(t, LongForecast::Raw).unwrap();
        assert_eq!(raw, via(e.x_1e9(t).unwrap()), "{d} d raw");
        match e.ln_sigma_fit_1e9(t) {
            Some(ln) => assert_eq!(e.sigma_ann_1e9(t, LongForecast::Fit), Some(via(ln))),
            None => assert_eq!(e.sigma_ann_1e9(t, LongForecast::Fit), None),
        }
        // An annualised vol of a $79k walk stepping a few dollars a
        // minute: tens of percent, not thousands.
        assert!((50_000_000..5_000_000_000).contains(&raw), "{d} d: {raw}");
    }
    assert_eq!(e.sigma_ann_1e9(tau(41), LongForecast::Raw), None);
}

// ---------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------

/// Every writer accessor, flattened — two engines with equal views are
/// the same engine as far as any reader can tell.
fn view(e: &LongVolEngine) -> Vec<String> {
    let mut out = vec![format!(
        "{:?} {} {} {} {}",
        e.open_day(),
        e.last_min_ts_ms(),
        e.prev_px_1e6(),
        e.is_warm(),
        e.n_resident()
    )];
    for i in 0..e.n_resident() {
        out.push(format!("D {:?}", e.day_at(i)));
    }
    for d in 1..=LONG_TAU_DAYS_MAX as u64 {
        let t = tau(d);
        out.push(format!(
            "T {d} {:?} {:?} {:?} {:?} {:?} {:?} {}",
            e.x_1e9(t),
            e.fit(t),
            e.ln_sigma_fit_1e9(t),
            e.sigma_ann_1e9(t, LongForecast::Raw),
            e.sigma_ann_1e9(t, LongForecast::Fit),
            e.qlike_counters(t),
            e.n_pairs(t)
        ));
        for i in 0..e.n_resident() {
            out.push(format!("A {d} {i} {:?}", e.arm_at(i, t)));
        }
        for i in 0..e.n_pairs(t) {
            out.push(format!("P {d} {:?}", e.pair_at(t, i)));
        }
        for i in 0..QLIKE_RING {
            out.push(format!("Q {d} {:?}", e.qlike_at(t, i)));
        }
    }
    out
}

/// Write `e` out through its accessors and read it into a fresh engine
/// through the seeds — what the H3 state file will do, minus the file.
fn restore(e: &LongVolEngine) -> Box<LongVolEngine> {
    let mut r = boxed();
    for i in 0..e.n_resident() {
        let (ts, sq, n) = e.day_at(i).unwrap();
        assert!(r.seed_day(ts, sq, n));
    }
    if let Some((ts, sq, n)) = e.open_day() {
        assert!(r.seed_open(ts, sq, n, e.last_min_ts_ms(), e.prev_px_1e6()));
    }
    for d in 1..=LONG_TAU_DAYS_MAX as u64 {
        let t = tau(d);
        for i in 0..e.n_resident() {
            if let Some((x, fit)) = e.arm_at(i, t) {
                assert!(r.seed_arm(t, e.day_at(i).unwrap().0, x, fit));
            }
        }
        for i in 0..e.n_pairs(t) {
            let (ts, x, y) = e.pair_at(t, i).unwrap();
            assert!(r.seed_pair(t, ts, x, y));
        }
        for i in 0..e.qlike_counters(t).n as usize {
            let (raw, fit) = e.qlike_at(t, i).unwrap();
            assert!(r.seed_qlike(t, raw, fit));
        }
    }
    r
}

#[test]
fn the_writer_and_the_reader_round_trip_exactly_and_continue_identically() {
    let mut a = boxed();
    let mut w = Walk::new(53);
    // 170 days: every 1 d–40 d ring holds pairs, the short tenors have
    // wrapped PAIR_RING_LONG, the QLIKE rings are full; then half a day.
    feed(&mut a, &mut w, 0, 170);
    for m in 0..720u64 {
        a.on_minute_close_at(w.next(170), DAY0 + 170 * DAY_MS + m * 60_000);
    }
    assert_eq!(a.n_pairs(tau(1)), PAIR_RING_LONG);
    assert_eq!(a.qlike_counters(tau(1)).n as usize, QLIKE_RING);
    let mut b = restore(&a);
    // Before refresh the restored fits are withheld, never stale.
    assert_eq!(b.fit(tau(1)), None);
    b.refresh();
    assert_eq!(view(&a), view(&b));
    // The next minute after the restart continues both identically — the
    // restored prime forms the return across the (zero-length) seam.
    let mut w2 = Walk::new(59);
    let mut day = 170u64;
    let mut m = 720u64;
    for _ in 0..(5 * 1440) {
        let px = w2.next(day);
        let ts = DAY0 + day * DAY_MS + m * 60_000;
        a.on_minute_close_at(px, ts);
        b.on_minute_close_at(px, ts);
        m += 1;
        if m == 1440 {
            m = 0;
            day += 1;
        }
    }
    assert_eq!(view(&a), view(&b));
    assert_eq!((a.gaps(), b.gaps()), (0, 0));
}

#[test]
fn a_restore_without_refresh_refits_at_the_first_live_minute() {
    let mut e = boxed();
    for (x, y) in lcg_pairs(MIN_PAIRS, 9) {
        assert!(e.seed_pair(tau(3), DAY0, x, y));
    }
    assert_eq!(e.fit(tau(3)), None);
    e.on_minute_close_at(79_000_000_000, DAY0);
    assert!(e.fit(tau(3)).is_some());
}

#[test]
fn the_seeds_refuse_what_the_writer_could_not_have_written() {
    let mut e = boxed();
    assert!(!e.seed_day(0, 1, 1), "no day zero");
    assert!(!e.seed_day(DAY0 + 1, 1, 1), "not a UTC midnight");
    assert!(!e.seed_day(DAY0, -1, 1), "a negative sum");
    assert!(e.seed_day(DAY0, 1, 1440));
    assert!(!e.seed_day(DAY0 + 2 * DAY_MS, 1, 1440), "not contiguous");
    assert!(!e.seed_day(DAY0, 1, 1440), "not after the newest");
    assert!(e.seed_day(DAY0 + DAY_MS, 1, 1440));
    // The open day: contiguous, its last minute inside it, a real prime.
    assert!(!e.seed_open(DAY0 + 3 * DAY_MS, 0, 0, DAY0 + 3 * DAY_MS, 5));
    assert!(!e.seed_open(DAY0 + 2 * DAY_MS, 0, 0, DAY0 + 3 * DAY_MS, 5));
    assert!(!e.seed_open(DAY0 + 2 * DAY_MS, 0, 0, DAY0 + DAY_MS, 5));
    assert!(!e.seed_open(DAY0 + 2 * DAY_MS, 0, 0, DAY0 + 2 * DAY_MS, 0));
    assert!(e.seed_open(DAY0 + 2 * DAY_MS, 0, 0, DAY0 + 2 * DAY_MS, 5));
    assert!(
        !e.seed_open(DAY0 + 2 * DAY_MS, 0, 0, DAY0 + 2 * DAY_MS, 5),
        "one open day"
    );
    assert!(
        !e.seed_day(DAY0 + 2 * DAY_MS, 1, 1440),
        "no closed day after the open one"
    );
    // Arms: on the grid, on a resident day, and a real x.
    assert!(e.seed_arm(tau(2), DAY0 + DAY_MS, 1, NONE));
    assert!(!e.seed_arm(tau(41), DAY0, 1, 1));
    assert!(!e.seed_arm(tau(2), DAY0 + 5 * DAY_MS, 1, 1));
    assert!(!e.seed_arm(tau(2), DAY0, NONE, 1));
    assert_eq!(e.arm_at(1, tau(2)), Some((1, NONE)));
    assert_eq!(e.arm_at(0, tau(2)), None);
    assert!(!e.seed_pair(DAY_NS / 2, DAY0, 1, 1));
    assert!(!e.seed_qlike(0, 1, 1));
    // Values the writer can never produce are refused, never repaired: a
    // "none" or out-of-range log-vol, an impossible day sum.
    assert!(!e.seed_pair(tau(1), DAY0, NONE, 1));
    assert!(!e.seed_pair(tau(1), DAY0, 1, -1));
    assert!(!e.seed_pair(tau(1), DAY0, LN_ABS_MAX_1E9 + 1, 1));
    assert!(e.seed_pair(tau(1), DAY0, LN_ABS_MAX_1E9, 0));
    assert!(!e.seed_arm(tau(2), DAY0 + DAY_MS, -1, NONE));
    assert!(!e.seed_arm(tau(2), DAY0 + DAY_MS, 1, LN_ABS_MAX_1E9 + 1));
    assert!(e.seed_arm(tau(2), DAY0 + DAY_MS, 1, -LN_ABS_MAX_1E9));
    assert!(!e.seed_qlike(tau(1), NONE, 1));
    assert!(!e.seed_qlike(tau(1), 1, NONE));
    let mut f = boxed();
    assert!(!f.seed_day(DAY0, SUM_SQ_MAX + 1, 1440));
    assert!(f.seed_day(DAY0, SUM_SQ_MAX, 1440));
    assert!(!f.seed_open(DAY0 + DAY_MS, SUM_SQ_MAX + 1, 1, DAY0 + DAY_MS, 5));
}
