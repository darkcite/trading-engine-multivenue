// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The venue clock (BX-6).** Gateway thread.
//!
//! Every signed request carries `timestamp` = local wall time + the
//! MEASURED venue offset, and `recvWindow` (from `exec.toml`, never the
//! venue's 5 000 ms default) is the stale-order guard. The offset comes
//! from the venue's time endpoint by the minimum-RTT rule: of a sync
//! round's samples, the one with the shortest round trip bounds the offset
//! tightest (`|error| ≤ RTT / 2`), and it alone is kept.
//!
//! Local wall time is the boot's [`core_time::WallAnchor`] advanced by the
//! monotonic clock — no `SystemTime` call on the hot path. The round runs
//! at boot and every 60 s; a `-1021` / `-5028` answer forces one early.
//! Until a round has succeeded the clock is NOT measured and the gateway
//! sends no signed request.

use core_time::WallAnchor;

/// Samples per sync round.
pub const SAMPLES_PER_ROUND: u8 = 4;

/// The venue clock.
#[derive(Copy, Clone, Debug)]
pub struct VenueClock {
    anchor: WallAnchor,
    offset_ms: i64,
    rtt_ns: u64,
    round_best_rtt_ns: u64,
    round_best_offset_ms: i64,
    round_samples: u8,
    measured: bool,
}

impl VenueClock {
    /// A clock anchored at boot; not measured yet.
    #[must_use]
    pub const fn new(anchor: WallAnchor) -> Self {
        Self {
            anchor,
            offset_ms: 0,
            rtt_ns: 0,
            round_best_rtt_ns: u64::MAX,
            round_best_offset_ms: 0,
            round_samples: 0,
            measured: false,
        }
    }

    /// Local wall time in ms at monotonic `now_ns`.
    #[inline(always)]
    #[must_use]
    pub const fn wall_ms(&self, now_ns: u64) -> u64 {
        self.anchor.wall_of(now_ns) / 1_000_000
    }

    /// The venue's time in ms at monotonic `now_ns`: the `timestamp` of a
    /// signed request.
    #[inline(always)]
    #[must_use]
    pub const fn venue_ms(&self, now_ns: u64) -> u64 {
        (self.wall_ms(now_ns) as i64 + self.offset_ms) as u64
    }

    /// One time-endpoint answer: sent at `sent_ns`, answered at `recv_ns`
    /// (monotonic), carrying the venue's `server_ms`. A round completes
    /// after [`SAMPLES_PER_ROUND`] samples; its minimum-RTT offset becomes
    /// the clock's. Returns `true` when a round completed.
    pub fn sample(&mut self, sent_ns: u64, recv_ns: u64, server_ms: u64) -> bool {
        if recv_ns < sent_ns || server_ms == 0 {
            return false;
        }
        let rtt = recv_ns - sent_ns;
        let mid_ns = sent_ns + rtt / 2;
        let offset = server_ms as i64 - self.wall_ms(mid_ns) as i64;
        if rtt < self.round_best_rtt_ns {
            self.round_best_rtt_ns = rtt;
            self.round_best_offset_ms = offset;
        }
        self.round_samples += 1;
        if self.round_samples < SAMPLES_PER_ROUND {
            return false;
        }
        self.offset_ms = self.round_best_offset_ms;
        self.rtt_ns = self.round_best_rtt_ns;
        self.measured = true;
        self.round_samples = 0;
        self.round_best_rtt_ns = u64::MAX;
        true
    }

    /// Forget the measurement (a `-1021` / `-5028`): nothing signed goes
    /// until the next round completes.
    pub fn invalidate(&mut self) {
        self.measured = false;
        self.round_samples = 0;
        self.round_best_rtt_ns = u64::MAX;
    }

    /// A round has succeeded since the last invalidation.
    #[inline(always)]
    #[must_use]
    pub const fn measured(&self) -> bool {
        self.measured
    }

    /// The venue minus local offset, ms (the gauge).
    #[must_use]
    pub const fn offset_ms(&self) -> i64 {
        self.offset_ms
    }

    /// The kept sample's round trip, ns (the gauge; `|error| ≤ rtt / 2`).
    #[must_use]
    pub const fn rtt_ns(&self) -> u64 {
        self.rtt_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn the_minimum_rtt_sample_wins_the_round() {
        // Wall = mono + 1 000 000 ms.
        let mut c = VenueClock::new(WallAnchor::new(0, 1_000_000 * MS));
        assert!(!c.measured());
        // Venue is 250 ms ahead. Sample 2 has the shortest RTT and its
        // midpoint reading is exact; the slow samples mislead.
        let venue = |mono_ms: u64| 1_000_000 + mono_ms + 250;
        assert!(!c.sample(0, 400 * MS, venue(100)));
        assert!(!c.sample(1_000 * MS, 1_010 * MS, venue(1_005)));
        assert!(!c.sample(2_000 * MS, 2_300 * MS, venue(2_000)));
        assert!(c.sample(3_000 * MS, 3_200 * MS, venue(3_190)));
        assert!(c.measured());
        assert_eq!(c.offset_ms(), 250);
        assert_eq!(c.rtt_ns(), 10 * MS);
        assert_eq!(c.venue_ms(5_000 * MS), 1_000_000 + 5_000 + 250);
        c.invalidate();
        assert!(!c.measured());
    }

    #[test]
    fn a_nonsense_sample_is_ignored() {
        let mut c = VenueClock::new(WallAnchor::new(0, 0));
        assert!(!c.sample(10, 5, 1));
        assert!(!c.sample(1, 5, 0));
        assert!(!c.measured());
    }
}
