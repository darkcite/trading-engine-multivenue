// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! # backtest::stale — the harness's staleness law (VT4)
//!
//! docs/venue-time-capture-plan.md §5: a stale tick neither fills nor
//! marks (the [`crate::backtest::fill`] engine skips it), and the
//! harness RE-JUDGES every v3 tick from its captured `venue_time_ms`
//! with the same estimator the ingress used (`core_time::FeedClock`,
//! one per (venue file, sym) — a sym never spans two connections on
//! any venue) so a threshold change is a REPLAY, never a recapture.
//! v2 files carry no stamp: their ticks stay never-stale (the v2 law)
//! and the venue is reported STALE-BLIND.
//!
//! Shared verbatim by `backtest` and `audit-pnl` (one law, never
//! twinned). DOCTRINE (audit_replay.rs): offline — allocates freely,
//! deterministic (BTree, integer math).

use std::collections::BTreeMap;

use core_time::FeedClock;
use core_types::{Tick, TICK_FLAG_STALE};

use super::VENUE_LABELS;

/// Per-venue stale accounting for one run, indexed like
/// [`VENUE_LABELS`] (the file/lane order, NOT the venue byte).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StaleStats {
    /// Ticks judged (every record of the venue file).
    pub ticks: u64,
    /// Ticks judged STALE.
    pub stale_ticks: u64,
    /// Engine-clock ns during which the venue's LATEST tick was stale
    /// (Σ gaps that follow a stale tick).
    pub stale_ns: u64,
    /// First tick ts of the file (engine clock).
    pub first_ts_ns: u64,
    /// Last tick ts of the file (engine clock).
    pub last_ts_ns: u64,
    /// True when the file is v2 (no stamp): every tick replays
    /// never-stale and the numbers above are UPPER BOUNDS of nothing.
    pub stale_blind: bool,
    last_stale: bool,
}

impl StaleStats {
    /// Stale time as basis points of the file's span (deterministic
    /// integer rendering; 0 for an empty or single-tick file).
    pub fn stale_time_bps(&self) -> u64 {
        let span = self.last_ts_ns.saturating_sub(self.first_ts_ns);
        if span == 0 {
            return 0;
        }
        ((self.stale_ns as u128 * 10_000) / span as u128) as u64
    }
}

/// One run's re-judge state: a `FeedClock` per (lane, sym) plus the
/// per-lane accounting.
pub struct StaleJudge {
    thresholds: [u32; 7],
    clocks: BTreeMap<(u8, u32), FeedClock>,
    /// Per-lane accounting, [`VENUE_LABELS`] order.
    pub stats: [StaleStats; VENUE_LABELS.len()],
}

impl StaleJudge {
    /// Thresholds indexed by the VENUE BYTE (`ModelParams::stale_after_ms`).
    pub fn new(thresholds: [u32; 7]) -> Self {
        Self {
            thresholds,
            clocks: BTreeMap::new(),
            stats: [StaleStats::default(); VENUE_LABELS.len()],
        }
    }

    /// Judge one tick of lane `lord` in FILE ORDER. On a v3 file
    /// (`has_venue_time`) the `TICK_FLAG_STALE` bit is REWRITTEN from
    /// the stamp (the sentinel bit is preserved); on a v2 file the
    /// flags stay as captured (0) and the lane is marked stale-blind.
    pub fn judge(&mut self, lord: usize, tick: &mut Tick, has_venue_time: bool) {
        let st = &mut self.stats[lord];
        if st.ticks == 0 {
            st.first_ts_ns = tick.ts_ns;
        } else if st.last_stale {
            st.stale_ns += tick.ts_ns.saturating_sub(st.last_ts_ns);
        }
        st.ticks += 1;
        st.last_ts_ns = tick.ts_ns;
        if !has_venue_time {
            st.stale_blind = true;
            st.last_stale = false;
            return;
        }
        let venue = tick.venue as usize;
        let threshold = if venue < self.thresholds.len() {
            self.thresholds[venue]
        } else {
            0
        };
        let clock = self
            .clocks
            .entry((tick.venue, tick.sym))
            .or_insert_with(|| FeedClock::new(threshold));
        let judged = clock.judge(tick.venue_time_ms, tick.ts_ns);
        tick.flags = (tick.flags & !TICK_FLAG_STALE) | ((judged.stale as u8) * TICK_FLAG_STALE);
        st.last_stale = judged.stale;
        if judged.stale {
            st.stale_ticks += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty, VenueId, TICK_FLAG_VENUE_TIME_SENTINEL};

    fn t(ts_ms: u64, venue_ms: u64, flags: u8) -> Tick {
        Tick::new_stamped(
            ts_ms * 1_000_000,
            VenueId::Okx,
            (2 << 24) | 1,
            1,
            Price::from_raw(1),
            Qty::from_raw(1),
            Price::from_raw(2),
            Qty::from_raw(1),
            venue_ms,
            flags,
        )
    }

    #[test]
    fn v3_ticks_are_rejudged_from_the_stamp_and_time_is_accounted() {
        // okx threshold 400: the first tick sets the offset (fresh); a
        // tick whose stamp is 5 s behind is stale; the captured flag is
        // OVERWRITTEN either way (a threshold change is a replay).
        let mut j = StaleJudge::new(VenueId::stale_after_ms_defaults());
        let lord = 2; // okx lane
        let mut a = t(1_000, 1_000_062, TICK_FLAG_STALE); // captured stale, re-judged fresh
        j.judge(lord, &mut a, true);
        assert_eq!(a.flags, 0);
        let mut b = t(1_300, 1_000_072, 0); // 290 ms behind: fresh
        j.judge(lord, &mut b, true);
        assert_eq!(b.flags, 0);
        let mut c = t(6_100, 1_000_100, TICK_FLAG_VENUE_TIME_SENTINEL); // 5 s behind: stale, bit1 kept
        j.judge(lord, &mut c, true);
        assert_eq!(c.flags, TICK_FLAG_STALE | TICK_FLAG_VENUE_TIME_SENTINEL);
        let mut d = t(6_600, 1_006_170, 0); // faster than ever: fresh
        j.judge(lord, &mut d, true);
        assert_eq!(d.flags, 0);
        let s = j.stats[lord];
        assert_eq!(s.ticks, 4);
        assert_eq!(s.stale_ticks, 1);
        // stale time = the gap after the stale tick (6_100 → 6_600 ms)
        assert_eq!(s.stale_ns, 500 * 1_000_000);
        assert_eq!(s.first_ts_ns, 1_000 * 1_000_000);
        assert_eq!(s.last_ts_ns, 6_600 * 1_000_000);
        // 500 ms of a 5_600 ms span = 892 bps
        assert_eq!(s.stale_time_bps(), 892);
        assert!(!s.stale_blind);
    }

    #[test]
    fn v2_files_are_stale_blind_and_never_flagged() {
        let mut j = StaleJudge::new(VenueId::stale_after_ms_defaults());
        let mut a = t(1_000, 0, 0);
        let mut b = t(9_000, 0, 0);
        j.judge(2, &mut a, false);
        j.judge(2, &mut b, false);
        assert_eq!(a.flags, 0);
        assert_eq!(b.flags, 0);
        let s = j.stats[2];
        assert!(s.stale_blind);
        assert_eq!(s.stale_ticks, 0);
        assert_eq!(s.stale_time_bps(), 0);
        assert_eq!(s.ticks, 2);
    }

    #[test]
    fn zero_threshold_measures_but_never_flags_and_unknown_stamp_is_fresh() {
        let mut thr = VenueId::stale_after_ms_defaults();
        thr[VenueId::Okx as usize] = 0;
        let mut j = StaleJudge::new(thr);
        let mut a = t(1_000, 1_000_000, 0);
        let mut b = t(61_000, 1_000_000, 0); // a minute behind, threshold 0
        let mut c = t(62_000, 0, 0); // unknown stamp
        j.judge(2, &mut a, true);
        j.judge(2, &mut b, true);
        j.judge(2, &mut c, true);
        assert_eq!(b.flags, 0);
        assert_eq!(c.flags, 0);
        assert_eq!(j.stats[2].stale_ticks, 0);
    }

    #[test]
    fn stale_time_bps_is_zero_for_empty_and_single_tick_lanes() {
        let mut j = StaleJudge::new([400; 7]);
        assert_eq!(j.stats[0].stale_time_bps(), 0);
        let mut a = t(1_000, 1_000_000, 0);
        j.judge(0, &mut a, true);
        assert_eq!(j.stats[0].stale_time_bps(), 0);
    }
}
