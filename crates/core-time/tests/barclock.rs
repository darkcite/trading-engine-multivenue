// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `core_time::barclock` — the PVM bar-boundary convention (contract v4 §3.15.1): the thirteen
//! timeframes, left-closed/right-open floors toward −∞, the Monday week, the calendar month,
//! and agreement with `BarClock` on every sub-week grid.

use core_time::barclock::*;
use core_time::{BarClock, WallAnchor};

const DAY_MS: i64 = 86_400_000;

/// xorshift64 — deterministic, seeded, no dependency.
struct Xs(u64);

impl Xs {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn ms_from_ns(ns: i64) -> i64 {
    ns.div_euclid(1_000_000)
}

#[test]
fn barclock_allowed_set_is_exactly_the_thirteen() {
    assert_eq!(ALLOWED_TF_S.len(), 13);
    let mut i = 1;
    while i < ALLOWED_TF_S.len() {
        assert!(ALLOWED_TF_S[i - 1] < ALLOWED_TF_S[i], "ascending");
        i += 1;
    }
    for tf in ALLOWED_TF_S {
        assert!(is_allowed(tf), "{tf}");
    }
    for tf in [
        0u32,
        1,
        45,
        59,
        61,
        120,
        240,
        2 * 3600 + 1,
        172_800,
        259_200,
        1_209_600,
        2_629_745,
        2_629_747,
        u32::MAX,
    ] {
        assert!(!is_allowed(tf), "{tf}");
    }
    // Every timeframe below a day divides a day exactly.
    let mut i = 0;
    while ALLOWED_TF_S[i] < 86_400 {
        assert_eq!(86_400 % ALLOWED_TF_S[i], 0, "{}", ALLOWED_TF_S[i]);
        i += 1;
    }
    assert_eq!(i, 10);
}

#[test]
fn barclock_open_is_left_closed_right_open_and_idempotent() {
    let mut r = Xs(0x9E37_79B9_7F4A_7C15);
    for _ in 0..200_000 {
        let t = (r.next() as i64) >> 3; // |t| < 2^60
        for tf in ALLOWED_TF_S {
            let o = open_ms(t, tf);
            let c = close_ms(o, tf);
            assert!(o <= t && t < c, "t={t} tf={tf} o={o} c={c}");
            assert_eq!(open_ms(o, tf), o);
            assert_eq!(open_ms(c, tf), c, "close is the next open");
            assert_eq!(open_ms(c - 1, tf), o);
            assert_eq!(bars_between(o, c, tf), 1);
            assert_eq!(bars_between(o, o, tf), 0);
            assert_eq!(bars_between(c, o, tf), -1);
        }
    }
}

#[test]
fn barclock_floors_toward_negative_infinity() {
    assert_eq!(open_ms(-1, 60), -60_000);
    assert_eq!(open_ms(-60_000, 60), -60_000);
    assert_eq!(open_ms(-60_001, 60), -120_000);
    assert_eq!(open_ms(-1, 86_400), -DAY_MS);
    assert_eq!(open_ms(-1, TF_MONTH_S), -31 * DAY_MS); // 1969-12-01
}

#[test]
fn barclock_week_anchors_on_monday() {
    assert_eq!(WEEK_ANCHOR_MS, 345_600_000);
    // The ns value impl-08 has; impl-07 v1's ×1000-too-large constant was review finding S4.
    assert_eq!(WEEK_ANCHOR_MS * 1_000_000, 345_600_000_000_000);
    // 1970-01-05 is four days after the (Thursday) epoch.
    assert_eq!(WEEK_ANCHOR_MS, 4 * DAY_MS);
    let mut r = Xs(0x1234_5678_9ABC_DEF1);
    for _ in 0..100_000 {
        let t = (r.next() as i64) >> 3;
        let o = open_ms(t, 604_800);
        let days = o.div_euclid(DAY_MS);
        let dow_mon1 = (days + 3).rem_euclid(7) + 1; // 1970-01-01 → 4 (Thursday)
        assert_eq!(dow_mon1, 1, "o={o}");
        assert_eq!(o.rem_euclid(DAY_MS), 0);
    }
    // 2026-09-23 (a Wednesday) 12:00Z → the week opened Monday 2026-09-21 00:00Z.
    let t = 1_790_164_800_000i64;
    assert_eq!(open_ms(t, 604_800), 1_789_948_800_000);
}

#[test]
fn barclock_month_is_calendar_exact() {
    // 2024-02-15T10:00Z → 2024-02-01; the close is 2024-03-01 (a leap year: 29 days).
    let feb1_2024 = 1_706_745_600_000i64;
    let mar1_2024 = 1_709_251_200_000i64;
    assert_eq!(mar1_2024 - feb1_2024, 29 * DAY_MS);
    let t = feb1_2024 + 14 * DAY_MS + 10 * 3_600_000;
    assert_eq!(open_ms(t, TF_MONTH_S), feb1_2024);
    assert_eq!(close_ms(feb1_2024, TF_MONTH_S), mar1_2024);
    // December rolls the year: 2025-12-31T23:59:59.999Z.
    let jan1_2026 = 1_767_225_600_000i64;
    let dec1_2025 = 1_764_547_200_000i64;
    assert_eq!(open_ms(jan1_2026 - 1, TF_MONTH_S), dec1_2025);
    assert_eq!(close_ms(dec1_2025, TF_MONTH_S), jan1_2026);
    assert_eq!(bars_between(dec1_2025, jan1_2026, TF_MONTH_S), 1);
    assert_eq!(bars_between(feb1_2024, jan1_2026, TF_MONTH_S), 23);
    // Every month over 400 years opens at a day boundary and lasts 28..=31 days.
    let mut o = open_ms(-(400 * 366) * DAY_MS / 2, TF_MONTH_S);
    let mut k = 0;
    while k < 4_800 {
        let c = close_ms(o, TF_MONTH_S);
        let len = (c - o) / DAY_MS;
        assert!((28..=31).contains(&len), "len {len} at {o}");
        assert_eq!((c - o) % DAY_MS, 0);
        o = c;
        k += 1;
    }
    // The sort key is the mean Gregorian month: 365.2425 d × 86 400 s / 12, in integers.
    assert_eq!(TF_MONTH_S as i64, 3_652_425 * 86_400 / 10_000 / 12);
    assert_eq!(3_652_425i64 * 86_400 % 120_000, 0, "exact, no rounding");
}

#[test]
fn barclock_sub_week_agrees_with_bar_clock() {
    // BarClock with the identity anchor: wall = mono.
    let a = WallAnchor::new(0, 0);
    let mut r = Xs(0xDEAD_BEEF_CAFE_F00D);
    for tf in ALLOWED_TF_S {
        if tf > 86_400 {
            continue;
        }
        let c = BarClock::new(a, tf as u64 * 1_000_000_000, 0);
        for _ in 0..50_000 {
            let t_ns = (r.next() >> 2) as i64; // [0, 2^62)
            let id = c.bar_id(t_ns as u64);
            let open_ns = c.open_wall(id) as i64;
            assert_eq!(
                open_ms(ms_from_ns(t_ns), tf) * 1_000_000,
                open_ns,
                "tf={tf} t_ns={t_ns}"
            );
            assert_eq!(
                close_ms(open_ns / 1_000_000, tf) * 1_000_000,
                c.close_mono(id) as i64
            );
        }
    }
    // The week does NOT agree: BarClock's grid is epoch-aligned (a Thursday) — why barclock
    // exists.
    let w = BarClock::new(a, 604_800 * 1_000_000_000, 0);
    let t_ns: i64 = 1_790_164_800_000 * 1_000_000;
    assert_ne!(
        w.open_wall(w.bar_id(t_ns as u64)) as i64,
        open_ms(t_ns / 1_000_000, 604_800) * 1_000_000
    );
}

#[test]
fn barclock_bars_between_counts_the_hole() {
    // 2026-09-23T12:00:00Z, on the hour.
    let o = open_ms(1_790_164_800_000, 3600);
    assert_eq!(o, 1_790_164_800_000);
    // Three missing bars between two present ones: four steps.
    assert_eq!(bars_between(o, o + 4 * 3_600_000, 3600), 4);
}

#[test]
fn barclock_is_total_outside_the_domain_in_release() {
    if cfg!(debug_assertions) {
        return;
    }
    for tf in ALLOWED_TF_S {
        let _ = open_ms(i64::MIN, tf);
        let _ = open_ms(i64::MAX, tf);
        let _ = bars_between(i64::MIN, i64::MAX, tf);
    }
}
