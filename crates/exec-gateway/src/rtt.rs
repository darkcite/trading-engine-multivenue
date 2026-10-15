// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The recent post round trips (XMM XH4f, D8): the last [`RTT_WINDOW`]
//! answered posts' round trips with the times they were answered, and the
//! worst of those no older than [`RTT_MAX_AGE_NS`] — of at most 64
//! samples the p99 is the maximum. A gateway records every post answered
//! on a socket — XMM XH5c-3: one sent by its REST fallback carries a fresh
//! dial and has its own series ([`crate::GatewayStats::rest_rtt`]) — and
//! publishes the worst with its heartbeat
//! ([`crate::GatewayStats::set_rtt_worst`]); a member pulls its quotes
//! while it exceeds its `rtt_pull_ms`. Monotonic time; no allocation.

/// Samples kept: the newest 64 answered posts.
pub const RTT_WINDOW: usize = 64;
/// A sample older than this no longer counts, ns (30 s): one slow answer
/// on a quiet arm does not hold its member off for ever.
pub const RTT_MAX_AGE_NS: u64 = 30_000_000_000;

/// The window, 1 088 B — boxed by its owner; not `Copy`.
#[repr(C, align(64))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RttWindow {
    /// When each sample's answer came (monotonic ns; 0 = an empty slot).
    at_ns: [u64; RTT_WINDOW],
    /// Its round trip, ns.
    rtt_ns: [u64; RTT_WINDOW],
    /// Samples recorded, ever: the next goes to `n % RTT_WINDOW`.
    n: u64,
}

const _: () = assert!(core::mem::size_of::<RttWindow>() == 1_088);

impl Default for RttWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl RttWindow {
    /// An empty window.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            at_ns: [0; RTT_WINDOW],
            rtt_ns: [0; RTT_WINDOW],
            n: 0,
        }
    }

    /// One post answered at `now_ns` after `rtt_ns`: the oldest sample
    /// makes room.
    #[inline]
    pub fn record(&mut self, now_ns: u64, rtt_ns: u64) {
        let i = (self.n % RTT_WINDOW as u64) as usize;
        self.at_ns[i] = now_ns;
        self.rtt_ns[i] = rtt_ns;
        self.n = self.n.wrapping_add(1);
    }

    /// The worst round trip answered within [`RTT_MAX_AGE_NS`] before
    /// `now_ns`, ns; 0 when none. Sixty-four compares — a heartbeat's
    /// work, never a post's.
    #[inline]
    #[must_use]
    pub fn worst(&self, now_ns: u64) -> u64 {
        let mut w = 0u64;
        let mut i = 0usize;
        while i < RTT_WINDOW {
            let at = self.at_ns[i];
            if at != 0 && now_ns.saturating_sub(at) <= RTT_MAX_AGE_NS && self.rtt_ns[i] > w {
                w = self.rtt_ns[i];
            }
            i += 1;
        }
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn the_worst_recent_answer_is_the_window_s_figure() {
        let mut w = RttWindow::new();
        assert_eq!(w.worst(1), 0, "nothing answered yet");
        let t = 1_000 * MS;
        w.record(t, 40 * MS);
        w.record(t + MS, 900 * MS);
        w.record(t + 2 * MS, 60 * MS);
        assert_eq!(w.worst(t + 2 * MS), 900 * MS);
        // The slow one ages out after 30 s; the others went with it.
        assert_eq!(w.worst(t + MS + RTT_MAX_AGE_NS), 900 * MS);
        assert_eq!(w.worst(t + 2 * MS + RTT_MAX_AGE_NS), 60 * MS);
        assert_eq!(w.worst(t + 3 * MS + RTT_MAX_AGE_NS), 0);
    }

    #[test]
    fn sixty_four_newer_answers_push_the_oldest_out() {
        let mut w = RttWindow::new();
        let t = 1_000 * MS;
        w.record(t, 2_000 * MS);
        let mut i = 1u64;
        while i < RTT_WINDOW as u64 {
            w.record(t + i, 10 * MS);
            i += 1;
        }
        assert_eq!(w.worst(t + i), 2_000 * MS, "64 samples: the first still in");
        w.record(t + i, 10 * MS);
        assert_eq!(w.worst(t + i), 10 * MS, "the 65th overwrote it");
    }
}
