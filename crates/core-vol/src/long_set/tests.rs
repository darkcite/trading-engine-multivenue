// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `LongVolSet` (HAR H3.3): the minute law, the stagger, and the proof
//! that the set's engines are exactly engines fed the same closes by hand.
//! Test-only code: allocation and `unwrap` are fine here.

use super::*;
use core_types::{make_symbol_id, Price, Qty, VenueId, TICK_FLAG_STALE};

use crate::long::DAY_NS;

/// 2026-01-01 00:00Z, ns — the wall anchor (mono 0 ↔ this wall instant).
const WALL0_NS: u64 = 1_767_225_600_000_000_000;
const WALL0_MS: u64 = WALL0_NS / 1_000_000;
const MIN_NS: u64 = 60_000_000_000;
const SEC_NS: u64 = 1_000_000_000;

fn anchor() -> WallAnchor {
    WallAnchor::new(0, WALL0_NS)
}

fn sym(i: u32) -> SymbolId {
    make_symbol_id(VenueId::Binance, 100 + i)
}

fn tick(ts_ns: u64, s: SymbolId, bid: i64, ask: i64) -> Tick {
    Tick::new(
        ts_ns,
        VenueId::Binance,
        s,
        0,
        Price::from_raw(bid),
        Qty::from_raw(1),
        Price::from_raw(ask),
        Qty::from_raw(1),
    )
}

const NAMES: [&[u8]; 12] = [
    b"SP500", b"SPCX", b"MU", b"NVDA", b"MSFT", b"META", b"AAPL", b"BABA", b"SNDK", b"BOT", b"BTC",
    b"ETH",
];

fn set_of(n: usize) -> Box<LongVolSet> {
    let mut set = Box::new(LongVolSet::new());
    let series: Vec<LongSeries<'_>> = (0..n)
        .map(|i| LongSeries {
            name: NAMES[i],
            feed: sym(i as u32),
        })
        .collect();
    set.configure(&series, anchor(), 0).unwrap();
    set
}

/// A deterministic mid path per series and minute.
fn mid(i: usize, m: u64) -> i64 {
    let mut s = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ m.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    s ^= s >> 31;
    100_000_000 + (i as i64 + 1) * 1_000_000 + (s % 40_000) as i64 - 20_000
}

fn fingerprint(e: &LongVolEngine) -> Vec<String> {
    let mut out: Vec<String> = (0..e.n_resident())
        .map(|i| format!("{:?}", e.day_at(i)))
        .collect();
    out.push(format!("{:?}", e.open_day()));
    out.push(format!(
        "{} {} {} {}",
        e.gaps(),
        e.refused(),
        e.last_min_ts_ms(),
        e.prev_px_1e6()
    ));
    for d in 1..=crate::LONG_TAU_DAYS_MAX as u64 {
        let t = d * DAY_NS;
        out.push(format!(
            "{d} {:?} {:?} {} {:?}",
            e.x_1e9(t),
            e.fit(t),
            e.n_pairs(t),
            e.qlike_counters(t)
        ));
    }
    out
}

#[test]
fn an_unconfigured_set_is_inert() {
    let mut set = LongVolSet::new();
    assert!(!set.is_configured() && set.is_empty());
    set.on_tick(&tick(5, sym(0), 100, 102));
    set.on_timer(10 * MIN_NS);
    set.drain_held();
    assert_eq!(set.counters(), LongSetCounters::default());
    assert_eq!(set.engine(0).map(|_| ()), None);
    assert_eq!(set.minute_ms(), 0);
}

#[test]
fn configure_refuses_bad_series_and_leaves_the_set_untouched() {
    let mut set = LongVolSet::new();
    let ok = |n: &'static [u8], f: u32| LongSeries { name: n, feed: sym(f) };
    assert_eq!(set.configure(&[], anchor(), 0), Err(LongSetErr::Empty));
    let thirteen: Vec<LongSeries<'_>> = (0..13).map(|i| ok(b"X", i)).collect();
    assert_eq!(set.configure(&thirteen, anchor(), 0), Err(LongSetErr::TooMany(13)));
    for bad in [&b""[..], b"btc", b"SP-500", b"ABCDEFGHIJKLM"] {
        assert_eq!(
            set.configure(&[ok(b"A", 0), LongSeries { name: bad, feed: sym(1) }], anchor(), 0),
            Err(LongSetErr::BadName(1))
        );
    }
    assert_eq!(
        set.configure(&[LongSeries { name: b"A", feed: SYMBOL_ID_NONE }], anchor(), 0),
        Err(LongSetErr::NoFeed(0))
    );
    assert_eq!(
        set.configure(&[ok(b"A", 0), ok(b"A", 1)], anchor(), 0),
        Err(LongSetErr::DuplicateName(1))
    );
    assert_eq!(
        set.configure(&[ok(b"A", 0), ok(b"B", 0)], anchor(), 0),
        Err(LongSetErr::DuplicateFeed(1))
    );
    assert!(!set.is_configured(), "every refusal leaves the set inert");
    set.configure(&[ok(b"ABCDEFGHIJ12", 0)], anchor(), 90 * SEC_NS).unwrap();
    assert_eq!(set.name(0), Some(&b"ABCDEFGHIJ12"[..]));
    assert_eq!(set.feed(0), Some(sym(0)));
    assert_eq!(set.minute_ms(), WALL0_MS + 60_000, "anchored on the minute `now` is in");
    assert_eq!(set.configure(&[ok(b"B", 1)], anchor(), 0), Err(LongSetErr::Configured));
    assert_eq!(LongSetErr::TooMany(13).to_string(), "13 series, at most 12");
}

#[test]
fn a_minute_closes_on_its_last_mid_stamped_with_its_open() {
    let mut set = set_of(2);
    // Minute 0: two quotes of series 0, one of series 1; a stale quote,
    // a one-sided quote and a stranger change nothing.
    set.on_tick(&tick(10 * SEC_NS, sym(0), 100_000_000, 100_000_002));
    set.on_tick(&tick(20 * SEC_NS, sym(1), 50_000_000, 50_000_004));
    set.on_tick(&tick(50 * SEC_NS, sym(0), 100_000_010, 100_000_014));
    let mut stale = tick(55 * SEC_NS, sym(0), 1, 3);
    stale.flags = TICK_FLAG_STALE;
    set.on_tick(&stale);
    set.on_tick(&tick(56 * SEC_NS, sym(0), 0, 100_000_014));
    set.on_tick(&tick(57 * SEC_NS, sym(7), 1, 3));
    // A quote past the boundary, before the timer rolls: the NEXT minute's.
    set.on_tick(&tick(MIN_NS + SEC_NS, sym(0), 100_000_100, 100_000_100));
    set.on_timer(MIN_NS - 1);
    assert_eq!(set.counters().minutes_rolled, 0, "nothing before the boundary");
    set.on_timer(MIN_NS + 2 * SEC_NS);
    assert_eq!(set.counters().minutes_rolled, 1);
    assert_eq!(set.counters().closes, 2);
    let e0 = set.engine(0).unwrap();
    assert_eq!(e0.last_min_ts_ms(), WALL0_MS);
    assert_eq!(e0.prev_px_1e6(), 100_000_012, "the minute's LAST fresh mid, floored");
    assert_eq!(set.engine(1).unwrap().prev_px_1e6(), 50_000_002);
    // Minute 1: series 0 carries the quote that arrived early; series 1
    // is silent and delivers nothing.
    set.on_timer(2 * MIN_NS);
    assert_eq!(set.counters().closes, 3);
    assert_eq!(set.engine(0).unwrap().last_min_ts_ms(), WALL0_MS + 60_000);
    assert_eq!(set.engine(0).unwrap().prev_px_1e6(), 100_000_100);
    assert_eq!(set.engine(1).unwrap().last_min_ts_ms(), WALL0_MS);
    // Minute 3 after a silent minute 2: one close, across the hole (a gap).
    set.on_tick(&tick(3 * MIN_NS + SEC_NS, sym(1), 50_000_010, 50_000_010));
    set.on_timer(4 * MIN_NS + 30 * SEC_NS);
    assert_eq!(set.counters().minutes_rolled, 4, "a late poll rolls every completed minute");
    let e1 = set.engine(1).unwrap();
    assert_eq!((e1.last_min_ts_ms(), e1.gaps()), (WALL0_MS + 3 * 60_000, 1));
}

/// Feed `days` UTC days of one quote per minute per series (at second
/// 30), polling every second from second 0 of each minute for the first
/// `polls` seconds, and return the closes a by-hand engine should see.
fn run_days(set: &mut LongVolSet, n: usize, days: u64, polls: u64) -> Vec<Vec<(i64, u64)>> {
    let mut want: Vec<Vec<(i64, u64)>> = vec![Vec::new(); n];
    let minutes = days * 1440;
    for m in 0..minutes {
        let t0 = m * MIN_NS;
        for s in 0..polls {
            set.on_timer(t0 + s * SEC_NS);
        }
        for (i, w) in want.iter_mut().enumerate() {
            set.on_tick(&tick(t0 + 30 * SEC_NS, sym(i as u32), mid(i, m), mid(i, m)));
            w.push((mid(i, m), WALL0_MS + m * 60_000));
        }
    }
    set.on_timer(minutes * MIN_NS);
    for s in 1..=LONG_SET_MAX as u64 {
        set.on_timer(minutes * MIN_NS + s * SEC_NS);
    }
    want
}

#[test]
fn the_staggered_set_equals_engines_fed_by_hand() {
    let mut set = set_of(12);
    let want = run_days(&mut set, 12, 35, 20);
    let c = set.counters();
    // 35 days × 12 series: every series crosses into days 1..=34 (day 0
    // opens a fresh engine: nothing to close), one per poll, the rest held.
    assert_eq!(c.day_closes, 34 * 12, "every series crossed every day");
    assert_eq!(c.held, 34 * 11, "one crossing per poll, the rest held");
    assert_eq!(c.forced, 0, "a poll a second releases everything before the next roll");
    assert_eq!(c.epoch, c.day_closes);
    assert!((0..12).all(|i| set.series_epoch(i) == 34), "one bump per day close");
    for (i, closes) in want.iter().enumerate() {
        let mut e = Box::new(LongVolEngine::new());
        for &(px, ms) in closes {
            e.on_minute_close_at(px, ms);
        }
        assert_eq!(fingerprint(set.engine(i).unwrap()), fingerprint(&e), "series {i}");
        assert!(e.is_warm(), "35 days make a warm engine");
        assert_eq!(set.held(i), None);
        let (p, cnt) = set.profile(i).unwrap();
        assert_eq!((*p, *cnt), e.weekday_profile_1e6(), "series {i}");
    }
    assert!(c.day_close_ns_max >= c.day_close_ns_last && c.day_close_ns_max > 0);
}

#[test]
fn a_poll_pays_for_one_day_close_and_holds_the_rest_in_order() {
    let mut set = set_of(3);
    let quote = |set: &mut LongVolSet, m: u64, px: i64| {
        for i in 0..3u32 {
            set.on_tick(&tick(m * MIN_NS + SEC_NS, sym(i), px, px));
        }
    };
    // Day 0's last minute opens a day in each (fresh) engine: no close.
    quote(&mut set, 1439, 10_000_000);
    set.on_timer(1440 * MIN_NS);
    assert_eq!(set.counters().day_closes, 0);
    // Day 1's first minute crosses in all three: one pays, two hold.
    quote(&mut set, 1440, 11_000_000);
    set.on_timer(1441 * MIN_NS);
    let c = set.counters();
    assert_eq!((c.day_closes, c.held, c.forced), (1, 2, 0));
    assert_eq!(set.held(0), None);
    assert_eq!(set.held(1), Some((11_000_000, WALL0_MS + 1440 * 60_000)));
    assert_eq!(set.held(2), Some((11_000_000, WALL0_MS + 1440 * 60_000)));
    // The next poll comes only at the next roll: series 1 is released by
    // the budget, series 2 is FORCED out by its own next minute — the held
    // minute first, then the new one. Order is kept either way.
    quote(&mut set, 1441, 11_000_100);
    set.on_timer(1442 * MIN_NS);
    let c = set.counters();
    assert_eq!((c.day_closes, c.held, c.forced), (3, 2, 1));
    for i in 0..3 {
        assert_eq!(set.held(i), None);
        let e = set.engine(i).unwrap();
        assert_eq!(e.last_min_ts_ms(), WALL0_MS + 1441 * 60_000, "series {i}");
        assert_eq!((e.refused(), e.n_resident()), (0, 1), "series {i}: in order");
        assert_eq!(e.open_day().unwrap().0, WALL0_MS + DAY_MS);
    }
}

#[test]
fn drain_held_delivers_what_is_still_held() {
    let mut set = set_of(2);
    for d in 0..2u64 {
        for m in 0..1440u64 {
            let t = (d * 1440 + m) * MIN_NS;
            for i in 0..2 {
                set.on_tick(&tick(t + SEC_NS, sym(i as u32), 20_000_000 + m as i64, 20_000_000));
            }
            set.on_timer(t + MIN_NS);
            set.on_timer(t + MIN_NS + SEC_NS);
        }
    }
    let t = 2 * 1440 * MIN_NS;
    for i in 0..2 {
        set.on_tick(&tick(t + SEC_NS, sym(i as u32), 30_000_000, 30_000_000));
    }
    set.on_timer(t + MIN_NS);
    assert!(set.held(1).is_some());
    let before = set.counters().day_closes;
    set.drain_held();
    assert_eq!(set.held(1), None);
    assert_eq!(set.counters().day_closes, before + 1);
    assert_eq!(set.engine(1).unwrap().last_min_ts_ms(), WALL0_MS + 2 * 1440 * 60_000);
}

#[test]
fn a_restore_refits_profiles_and_bumps_the_epoch() {
    let mut src = Box::new(LongVolEngine::new());
    for m in 0..(40 * 1440u64) {
        src.on_minute_close_at(mid(0, m), WALL0_MS + m * 60_000);
    }
    let mut set = set_of(1);
    assert!(set.engine_mut(1).is_none());
    let e = set.engine_mut(0).unwrap();
    for i in 0..src.n_resident() {
        let (ts, sq, n) = src.day_at(i).unwrap();
        assert!(e.seed_day(ts, sq, n));
    }
    let epoch = set.counters().epoch;
    set.restored();
    assert_eq!(set.counters().epoch, epoch + 1);
    assert_eq!(set.series_epoch(0), 1);
    assert_eq!(set.series_epoch(1), 0, "past the configured series");
    let (p, cnt) = set.profile(0).unwrap();
    assert_eq!((*p, *cnt), src.weekday_profile_1e6());
    assert_eq!(cnt.iter().sum::<u32>(), 39, "39 closed days (day 39 was open), by weekday");
    // The restore ended on a CLOSED day (no open one): the next minute
    // crosses — day 39 is pushed EMPTY and day 40 opens.
    set.on_tick(&tick(40 * 1440 * MIN_NS + SEC_NS, sym(0), 99_000_000, 99_000_000));
    set.on_timer(40 * 1440 * MIN_NS + MIN_NS);
    assert_eq!(set.counters().day_closes, 1);
    let e = set.engine(0).unwrap();
    assert_eq!(e.n_resident(), 40);
    assert_eq!(e.day_at(39).unwrap(), (WALL0_MS + 39 * DAY_MS, 0, 0), "the empty day");
    assert_eq!(e.open_day().unwrap().0, WALL0_MS + 40 * DAY_MS);
}

#[test]
fn the_profile_law_is_the_ratio_of_weekday_means() {
    // Days alternate quiet and loud by weekday: Saturday/Sunday flat.
    let mut e = Box::new(LongVolEngine::new());
    let mut px = 100_000_000i64;
    for d in 0..28u64 {
        let wd = crate::weekday_of(WALL0_MS + d * DAY_MS);
        for m in 0..1440u64 {
            if wd < 5 {
                px += if m % 2 == 0 { 1_000 } else { -1_000 };
            }
            e.on_minute_close_at(px, WALL0_MS + d * DAY_MS + m * 60_000);
        }
    }
    e.on_minute_close_at(px, WALL0_MS + 28 * DAY_MS);
    let (p, cnt) = e.weekday_profile_1e6();
    assert_eq!(cnt, [4, 4, 4, 4, 4, 4, 4]);
    assert_eq!(&p[5..], &[0, 0], "flat weekends");
    // Five loud weekdays carry all the variance: each ≈ 7/5 of the mean.
    for (w, ratio) in p.iter().enumerate().take(5) {
        assert!((1_390_000..=1_410_000).contains(ratio), "{w}: {ratio}");
    }
    assert_eq!(crate::weekday_of(WALL0_MS), 3, "2026-01-01 was a Thursday");
    let empty = LongVolEngine::new();
    assert_eq!(empty.weekday_profile_1e6(), ([0; 7], [0; 7]));
}

/// HAR H3.7: a series' snapshot is its engine copied WHOLE — the copy
/// renders the same state file byte for byte, reads the same everywhere,
/// and keeps nothing of what `dst` held — and carries the epoch it was
/// taken at; a series past the configured ones leaves `dst` untouched.
#[test]
fn a_snapshot_is_the_engine_copied_whole_with_its_epoch() {
    let mut set = set_of(2);
    run_days(&mut set, 2, 33, 3);
    let e = set.engine(1).unwrap();
    assert!(e.is_warm(), "the copy must be tested on a warm engine");

    let mut snap = Box::new(LongStateSnap::new());
    // Poison the destination: nothing of it may survive the copy.
    for m in 0..(3 * 1440u64) {
        snap.engine.on_minute_close_at(mid(7, m), WALL0_MS + (m + 900 * 1440) * 60_000);
    }
    snap.epoch = u64::MAX;
    assert!(set.snapshot_series(1, &mut snap));
    assert_eq!(snap.epoch, set.series_epoch(1));
    assert!(snap.epoch > 0, "day closes bumped it");
    assert_eq!(fingerprint(&snap.engine), fingerprint(e));

    let (mut a, mut b) = (String::new(), String::new());
    crate::render_state_file("MU", e, &mut a);
    crate::render_state_file("MU", &snap.engine, &mut b);
    assert_eq!(a, b, "the copy renders the same state file");
    assert!(a.starts_with("# har-state.tsv v"), "the header leads");
    assert!(a.contains("\nV\t"), "then the rows");

    let before = snap.epoch;
    assert!(!set.snapshot_series(2, &mut snap), "past the configured series");
    assert_eq!(snap.epoch, before, "untouched");
}
