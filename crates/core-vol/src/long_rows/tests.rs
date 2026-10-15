// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The rows grammar (HAR H3.4): the writer and reader round-trip an
//! engine exactly, the reader refuses what the writer could never write,
//! and the merge follows the §6 day-merge law case by case.
//! Test-only code: allocation and `unwrap` are fine here.

use super::*;
use crate::long::LONG_WARM_DAYS;

/// 2026-01-01 00:00Z.
const DAY0: u64 = 1_767_225_600_000;

fn walk(days: u64, seed: u64) -> Box<LongVolEngine> {
    let mut e = Box::new(LongVolEngine::new());
    let mut s = seed;
    let mut px: i64 = 79_000_000_000;
    for d in 0..days {
        let phase = d % 40;
        let amp = 2_000_000 * (4 + if phase < 20 { phase } else { 40 - phase }) as i64;
        for m in 0..1440u64 {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            px = (px + ((s >> 32) % (2 * amp as u64 + 1)) as i64 - amp).max(1_000_000_000);
            e.on_minute_close_at(px, DAY0 + d * DAY_MS + m * 60_000);
        }
    }
    e
}

fn text(e: &LongVolEngine) -> String {
    let mut t = String::new();
    e.write_rows(&mut t).unwrap();
    t
}

fn restored(rows: &LongRows) -> (Box<LongVolEngine>, ApplyStats) {
    let mut e = Box::new(LongVolEngine::new());
    let st = apply_rows(&mut e, rows);
    e.refresh();
    (e, st)
}

#[test]
fn the_writer_and_reader_round_trip_an_engine_exactly() {
    let e = walk(200, 11);
    let t = DAY_NS;
    assert!(e.fit(t).is_some() && e.qlike_counters(t).n == QLIKE_RING as u32);
    let src = text(&e);
    let rows = parse_rows(&src).unwrap();
    assert_eq!(rows.days.len(), DAY_RING);
    assert!(rows.open.is_some());
    assert_eq!(rows.arms.len(), (1..=40).sum::<usize>(), "only the pending arms");
    assert_eq!(rows, rows_of(&e));
    let (back, st) = restored(&rows);
    assert_eq!(st.refused, 0);
    assert_eq!(text(&back), src, "the restored engine writes the same bytes");
    for d in 1..=LONG_TAU_DAYS_MAX as u64 {
        let tau = d * DAY_NS;
        assert_eq!(back.fit(tau), e.fit(tau), "{d}");
        assert_eq!(back.x_1e9(tau), e.x_1e9(tau), "{d}");
        assert_eq!(back.qlike_counters(tau), e.qlike_counters(tau), "{d}");
    }
    assert_eq!(rows.last_min_ts_ms(), e.last_min_ts_ms());
    // A cold engine writes only `V`, and reads back to nothing.
    let cold = LongVolEngine::new();
    assert_eq!(text(&cold), "V\t1\n");
    assert_eq!(parse_rows("V\t1\n").unwrap(), LongRows::default());
}

#[test]
fn the_reader_refuses_what_the_writer_never_writes() {
    let err = |s: &str| parse_rows(s).unwrap_err().to_string();
    assert_eq!(err(""), "no `V` row: not a long-tenor rows file");
    assert_eq!(err("# only a comment\n"), "no `V` row: not a long-tenor rows file");
    assert_eq!(err("D\t1\t2\t3\n"), "line 1: the first row must be `V 1`");
    assert_eq!(err("V\t2\n"), "line 1: version 2, this reader reads 1");
    assert_eq!(err("V\t1\nV\t1\n"), "line 2: a second `V` row");
    assert_eq!(err("V\t1\nX\t1\n"), "line 2: unknown row `X`");
    assert_eq!(err("V\t1\nD\t1\t2\n"), "line 2: `D` rows carry 4 fields, got 3");
    assert_eq!(err("V\t1\nD\t1\t2.5\t3\n"), "line 2: `sum_sq` is not an integer: `2.5`");
    assert_eq!(err("V\t1\nA\t41\t1\t2\t-\n"), "line 2: tau_days 41 is off the 1..=40 grid");
    assert_eq!(err("V\t1\nQ\t0\t1\t2\n"), "line 2: tau_days 0 is off the 1..=40 grid");
    assert_eq!(err("V\t1\nC\t1\t2\t3\t4\t5\nD\t1\t2\t3\n"), "line 3: a `D` row after the open day or the arms");
    assert_eq!(
        err("V\t1\nC\t1\t2\t3\t4\t5\nC\t1\t2\t3\t4\t5\n"),
        "line 3: a `C` row must be the only one, before the arms"
    );
    assert_eq!(err("V\t1\nP\t1\t1\t2\t3\nC\t1\t2\t3\t4\t5\n"), "line 3: a `C` row must be the only one, before the arms");
    assert_eq!(err("V\t1\nA\t1\t1\t2\tx\n"), "line 2: `fit_1e9` is not an integer: `x`");
    // CRLF and comments are tolerated; `-` means "no fit".
    let rows = parse_rows("# seed\r\nV\t1\r\nA\t3\t86400000\t5\t-\r\n").unwrap();
    assert_eq!(rows.arms, vec![(3, 86_400_000, 5, i64::MIN)]);
}

fn day(ts_day: u64, sq: i128, n: u32) -> (u64, i128, u32) {
    (DAY0 + ts_day * DAY_MS, sq, n)
}

#[test]
fn merge_keeps_the_day_with_more_minutes_and_ties_to_the_state() {
    let seed = LongRows {
        days: vec![day(0, 10, 1440), day(1, 20, 1440), day(2, 30, 700)],
        ..LongRows::default()
    };
    let state = LongRows {
        days: vec![day(1, 21, 900), day(2, 31, 1440), day(3, 41, 1440)],
        ..LongRows::default()
    };
    let m = merge_rows(Some(&seed), Some(&state));
    assert_eq!(
        m.days,
        vec![day(0, 10, 1440), day(1, 20, 1440), day(2, 31, 1440), day(3, 41, 1440)]
    );
    let tie = LongRows {
        days: vec![day(0, 99, 1440)],
        ..LongRows::default()
    };
    assert_eq!(merge_rows(Some(&seed), Some(&tie)).days[0], day(0, 99, 1440), "a tie keeps the state's");
    assert_eq!(merge_rows(Some(&seed), None), seed);
    assert_eq!(merge_rows(None, Some(&state)), state);
    assert_eq!(merge_rows(None, None), LongRows::default());
}

#[test]
fn merge_fills_holes_empty_and_keeps_the_newest_ring() {
    let seed = LongRows {
        days: (0..10).map(|d| day(d, 1, 1440)).collect(),
        ..LongRows::default()
    };
    let state = LongRows {
        days: (70..80).map(|d| day(d, 2, 1440)).collect(),
        ..LongRows::default()
    };
    let m = merge_rows(Some(&seed), Some(&state));
    assert_eq!(m.days.len(), DAY_RING, "cut to the newest ring");
    assert_eq!(m.days[0].0, DAY0 + 16 * DAY_MS);
    assert!(m.days[..54].iter().all(|d| d.1 == 0 && d.2 == 0), "the hole is EMPTY");
    assert_eq!(m.days[63], day(79, 2, 1440));
    // Applied, the engine sees 64 contiguous days and is COLD (the empty
    // days sit in the fold's window): absent data holds.
    let (e, st) = restored(&m);
    assert_eq!((st.refused, e.n_resident()), (0, DAY_RING));
    assert!(!e.is_warm(), "{LONG_WARM_DAYS} observed days are needed");
}

#[test]
fn merge_takes_the_later_open_day_and_drops_a_stale_one() {
    let seed = LongRows {
        days: vec![day(0, 1, 1440), day(1, 2, 1440)],
        open: Some((DAY0 + 2 * DAY_MS, 5, 600, DAY0 + 2 * DAY_MS + 599 * 60_000, 77)),
        ..LongRows::default()
    };
    // The state stopped a day earlier: its open day is day 1, which the
    // seed has CLOSED — stale however late its minute.
    let state = LongRows {
        days: vec![day(0, 1, 1440)],
        open: Some((DAY0 + DAY_MS, 3, 1000, DAY0 + DAY_MS + 999 * 60_000, 66)),
        ..LongRows::default()
    };
    let m = merge_rows(Some(&seed), Some(&state));
    assert_eq!(m.open, seed.open);
    // A newer state wins the open day; the days before it fill EMPTY.
    let newer = LongRows {
        days: vec![day(0, 1, 1440)],
        open: Some((DAY0 + 4 * DAY_MS, 9, 10, DAY0 + 4 * DAY_MS + 9 * 60_000, 88)),
        ..LongRows::default()
    };
    let m = merge_rows(Some(&seed), Some(&newer));
    assert_eq!(m.open, newer.open);
    assert_eq!(m.days, vec![day(0, 1, 1440), day(1, 2, 1440), day(2, 0, 0), day(3, 0, 0)]);
    let (e, st) = restored(&m);
    assert_eq!(st.refused, 0);
    assert_eq!(e.last_min_ts_ms(), DAY0 + 4 * DAY_MS + 9 * 60_000);
    assert_eq!(e.prev_px_1e6(), 88);
}

#[test]
fn merge_unions_arms_and_pairs_state_winning_and_takes_one_qlike_chronology() {
    let days: Vec<(u64, i128, u32)> = (0..3).map(|d| day(d, 1, 1440)).collect();
    let seed = LongRows {
        days: days.clone(),
        arms: vec![(1, DAY0, 100, NONE), (2, DAY0 + DAY_MS, 200, 201), (3, DAY0 - DAY_MS, 1, 1)],
        pairs: vec![(1, DAY0, 10, 11), (1, DAY0 + DAY_MS, 12, 13), (2, DAY0, 20, 21)],
        qlike: vec![(1, 1, 2), (1, 3, 4), (2, 5, 6)],
        ..LongRows::default()
    };
    let state = LongRows {
        days,
        arms: vec![(1, DAY0, 101, 102)],
        pairs: vec![(1, DAY0 + DAY_MS, 99, 98), (1, DAY0 + 2 * DAY_MS, 14, 15)],
        qlike: vec![(1, 7, 8)],
        ..LongRows::default()
    };
    let m = merge_rows(Some(&seed), Some(&state));
    assert_eq!(m.arms, vec![(1, DAY0, 101, 102), (2, DAY0 + DAY_MS, 200, 201)], "resident days only");
    assert_eq!(
        m.pairs,
        vec![(1, DAY0, 10, 11), (1, DAY0 + DAY_MS, 99, 98), (1, DAY0 + 2 * DAY_MS, 14, 15), (2, DAY0, 20, 21)]
    );
    assert_eq!(m.qlike, vec![(1, 7, 8), (2, 5, 6)], "tenor 1 from the state, tenor 2 from the seed");
    // The newest 128 pairs per tenor.
    let many = LongRows {
        pairs: (0..200u64).map(|k| (1, DAY0 + k * DAY_MS, 1, 1)).collect(),
        ..LongRows::default()
    };
    let m = merge_rows(Some(&many), None);
    assert_eq!(m.pairs.len(), PAIR_RING_LONG);
    assert_eq!(m.pairs[0].1, DAY0 + 72 * DAY_MS);
}

#[test]
fn apply_counts_what_the_engine_refuses() {
    let rows = LongRows {
        days: vec![day(0, 1, 1440), day(2, 1, 1440)],
        pairs: vec![(1, DAY0, -5, 1)],
        qlike: vec![(1, NONE, 1)],
        ..LongRows::default()
    };
    let (_, st) = restored(&rows);
    assert_eq!(st, ApplyStats { applied: 1, refused: 3 }, "a gap, a negative x, a `none` score");
}
