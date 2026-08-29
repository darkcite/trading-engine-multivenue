// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! VM2 V2 property tests: the feature engine's rolling stats and
//! funding-APR windows vs naive reference implementations of the SAME
//! documented semantics (vm2-plan §4-V2). Offline test code — allocates
//! freely.

use core_types::FeatId;
use core_types::VenueId;
use proptest::prelude::*;
use strategy_vm::features::FeatureState;

const MONO0: u64 = 100_000_000_000_000_000;
const WALL0: u64 = 1_787_961_600_000; // UTC midnight, ms

fn mono_at(wall_ms: u64) -> u64 {
    MONO0 + (wall_ms - WALL0) * 1_000_000
}

fn teach_wall(f: &mut FeatureState) {
    let ev = core_types::ChannelEvent::new(
        MONO0,
        VenueId::Okx,
        core_types::ChannelId::Funding,
        core_types::make_symbol_id(VenueId::Okx, 999),
        0,
        WALL0,
        0,
        0,
    );
    f.on_venue_event(&ev, MONO0);
}

fn isqrt(v: i128) -> i64 {
    if v <= 0 {
        return 0;
    }
    let mut x = v;
    let mut y = (x + 1) >> 1;
    while y < x {
        x = y;
        y = (x + v / x) >> 1;
    }
    x as i64
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Random per-minute mid sequences (with gaps) over a random
    /// window: mean/min/max/std/EMA match the naive reference over
    /// the in-window last-of-minute samples.
    #[test]
    fn rolling_stats_match_naive(
        win in 2u16..48,
        // (minute offset, px ×1e6, present) — up to 120 minutes.
        seq in proptest::collection::vec(
            (0u64..120, 1_000_000i64..2_000_000_000, prop::bool::ANY),
            1..120,
        ),
        read_off in 0u64..10,
    ) {
        let mut f = FeatureState::new_boxed();
        teach_wall(&mut f);
        let sym = core_types::make_symbol_id(VenueId::Okx, 7);
        prop_assert!(f.bind_roll(sym, win));

        // Last-of-minute law: later entries for the same minute win.
        // Feed in minute order (the engine never sees time reversed;
        // same-minute overwrites are legal).
        let mut by_min: std::collections::BTreeMap<u64, i64> =
            std::collections::BTreeMap::new();
        for (m, px, present) in &seq {
            if *present {
                by_min.insert(*m, *px);
            }
        }
        for (m, px) in &by_min {
            let at = mono_at(WALL0 + m * 60_000 + 30_000);
            let t = core_types::Tick::new(
                at,
                VenueId::Okx,
                sym,
                (*m + 1) as u32,
                core_types::Price::from_raw(px - 1),
                core_types::Qty::from_raw(1),
                core_types::Price::from_raw(px + 1),
                core_types::Qty::from_raw(1),
            );
            f.on_tick(&t, at);
        }

        // Reads never precede the newest sample: the engine clock is
        // monotonic, so `read` minute ≥ newest fed minute always —
        // the proptest honors the same invariant.
        let max_fed = by_min.keys().max().copied().unwrap_or(0);
        let read_min = max_fed + read_off;
        let read_wall = WALL0 + read_min * 60_000 + 45_000;
        let now = mono_at(read_wall);
        let minute = (read_wall / 60_000) as i64;

        // Naive reference: in-window = (minute − win, minute], and a
        // sample participates only if it was actually recorded (ticks
        // never rewrite history ⇒ only minutes ≤ the newest fed
        // minute exist — all of ours are).
        let lo = minute - win as i64 + 1;
        let samples: Vec<i64> = by_min
            .iter()
            .filter(|(m, _)| {
                let mm = (WALL0 / 60_000 + **m) as i64;
                mm >= lo && mm <= minute
            })
            .map(|(_, px)| *px * 1_000)
            .collect();

        let got_mean = f.read(FeatId::RollMean, sym, win, now);
        let got_min = f.read(FeatId::RollMin, sym, win, now);
        let got_max = f.read(FeatId::RollMax, sym, win, now);
        let got_std = f.read(FeatId::RollStd, sym, win, now);
        let got_ema = f.read(FeatId::RollEma, sym, win, now);

        if samples.is_empty() {
            prop_assert_eq!(got_mean, None);
            prop_assert_eq!(got_min, None);
            prop_assert_eq!(got_std, None);
            prop_assert_eq!(got_ema, None);
        } else {
            let n = samples.len() as i64;
            let sum: i64 = samples.iter().sum();
            let mean = sum / n;
            prop_assert_eq!(got_mean, Some(mean));
            prop_assert_eq!(got_min, samples.iter().min().copied());
            prop_assert_eq!(got_max, samples.iter().max().copied());
            let ex2: i128 =
                samples.iter().map(|v| *v as i128 * *v as i128).sum::<i128>() / n as i128;
            let var = ex2 - (mean as i128 * mean as i128);
            prop_assert_eq!(got_std, Some(isqrt(var.max(0))));
            let alpha = 2_000_000_000i64 / (win as i64 + 1);
            let mut ema = samples[0];
            for v in &samples[1..] {
                ema += ((alpha as i128 * (*v - ema) as i128) / 1_000_000_000) as i64;
            }
            prop_assert_eq!(got_ema, Some(ema));
        }
    }

    /// Random funding prints: APRs match
    /// `claude_worker.carry_signal.apr_from_prints` transcribed —
    /// Σ(in-window)/divisor / days × 365, deribit ÷8, empty window
    /// ABSENT.
    #[test]
    fn funding_apr_matches_carry_signal_law(
        deribit in prop::bool::ANY,
        prints in proptest::collection::vec(
            // (hours back 0..96, rate ×1e9)
            (0i64..96, -500_000_000i64..500_000_000),
            0..80,
        ),
    ) {
        let venue = if deribit { VenueId::Deribit } else { VenueId::Bybit };
        let sym = core_types::make_symbol_id(venue, 5);
        let mut f = FeatureState::new_boxed();
        teach_wall(&mut f);

        // Distinct hour buckets only (the seed dedup law folds
        // near-duplicates — the reference must see what the engine
        // kept, so dedup at the fixture level).
        let mut by_hour: std::collections::BTreeMap<i64, i64> =
            std::collections::BTreeMap::new();
        for (h, r) in &prints {
            by_hour.entry(*h).or_insert(*r);
        }
        // Half-period dedup tolerance: bybit period 8 h ⇒ keep hours
        // ≥ 4 apart; deribit (period 0 ⇒ 30 min tol) keeps hourly.
        let mut kept: Vec<(i64, i64)> = Vec::new();
        for (h, r) in &by_hour {
            let tol_h = if deribit { 1 } else { 4 };
            if kept.iter().all(|(kh, _)| (kh - h).abs() >= tol_h) {
                kept.push((*h, *r));
            }
        }
        for (h, r) in &kept {
            let ts = WALL0 as i64 - h * 3_600_000;
            f.funding_seed(sym, ts, *r);
        }

        let now = mono_at(WALL0);
        let div: i128 = if deribit { 8 } else { 1 };
        // Window edges are INCLUSIVE (`ts >= wall − W` in the
        // engine): a print exactly 24 h old counts.
        let sum24: i128 = kept
            .iter()
            .filter(|(h, _)| *h <= 24)
            .map(|(_, r)| *r as i128)
            .sum();
        let n24 = kept.iter().filter(|(h, _)| *h <= 24).count();
        let sum72: i128 = kept
            .iter()
            .filter(|(h, _)| *h <= 72)
            .map(|(_, r)| *r as i128)
            .sum();
        let n72 = kept.iter().filter(|(h, _)| *h <= 72).count();

        let got24 = f.read(FeatId::Apr24, sym, 0, now);
        let got72 = f.read(FeatId::Apr72, sym, 0, now);
        if n24 == 0 {
            prop_assert_eq!(got24, None);
        } else {
            prop_assert_eq!(got24, Some(((sum24 * 365) / div) as i64));
        }
        if n72 == 0 {
            prop_assert_eq!(got72, None);
        } else {
            prop_assert_eq!(got72, Some(((sum72 * 365) / (div * 3)) as i64));
        }
    }
}
