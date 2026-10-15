// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The per-instrument arrival sequencer behind [`crate::Tick::venue_seq`]
//! on the top-of-book streams that carry no sequence number — Polymarket
//! (HK31), Hyperliquid `bbo` and the outcome-leg `l2Book` touch, Deribit
//! `quote` (HK32).
//!
//! Those streams carry only a millisecond venue stamp, and
//! `book_builder::TopOfBook::apply` drops `venue_seq <= last` as `Stale`.
//! With the stamp's low 32 bits as the seq, every tick after the first in
//! its instrument's millisecond was dropped, and the low 32 bits wrap
//! every ~49.7 days (next at 2026-11-21 21:25:29.728Z), after which every
//! tick would be `Stale` until a restart. The ingress numbers the ticks
//! instead: one [`ArrivalSeq`] per instrument in a fixed array indexed by
//! its symbol-table row, owned by the ingress thread, stamped where the
//! run loop holds a successfully parsed tick.
//!
//! Valid only on ONE ordered stream per instrument — the arrival order is
//! then the venue's send order. Each of the three venues has one
//! connection. Hyperliquid's two tick sources for an outcome leg (`bbo`
//! and the `l2Book` touch) are two VIEWS of one book on one block clock,
//! and share the leg's sequencer: two ordinals feeding one `TopOfBook`
//! would collide. The later arrival of a block wins it — the venue sends
//! the tighter `l2Book` view after the `bbo` (`ingress-hyperliquid` crate
//! docs).

/// One instrument's arrival sequencer — the `Tick.venue_seq` law of a
/// stream that carries none (module docs). A tick not older than the
/// instrument's newest (`venue_time_ms >= last_ms`) takes the next ordinal
/// — 1, 2, 3, … in arrival order — and an OLDER one takes 0, which
/// `book_builder::TopOfBook` drops as `Stale` at every book (the current
/// ordinal would not: a book that missed the newest tick to a ring drop
/// would apply the older touch). The caller counts every 0 on
/// `seq_regressions_total`: `last_ms` only rises within a connection, so
/// one far-future stamp would otherwise drop the instrument silently until
/// [`ArrivalSeq::reset_clocks`] clears it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct ArrivalSeq {
    /// Venue stamp (ms) of the newest tick sequenced on this connection;
    /// 0 = none (fresh, or zeroed by a reconnect).
    last_ms: u64,
    /// The last ordinal handed out; 0 = none yet (the book's "never
    /// seen"). Never reset: monotone for the life of the process.
    seq: u32,
}

impl ArrivalSeq {
    /// Nothing sequenced yet.
    pub const ZERO: Self = Self { last_ms: 0, seq: 0 };

    /// The `venue_seq` of a tick stamped `venue_time_ms`: the next ordinal
    /// (from 1) unless the stamp is older than the newest, then 0.
    /// Branch-free.
    #[inline(always)]
    pub fn next(&mut self, venue_time_ms: u64) -> u32 {
        // Unreachable within a process lifetime: 2^32 ticks of one
        // instrument take ~76 days even at the busiest measured one-asset
        // peak (Polymarket, 655 ticks/s; Deribit's busiest perp averages
        // 34/s, Hyperliquid's 10/s).
        debug_assert!(self.seq != u32::MAX, "arrival ordinal exhausted");
        let fresh = venue_time_ms >= self.last_ms;
        self.seq = self.seq.wrapping_add(fresh as u32);
        self.last_ms = self.last_ms.max(venue_time_ms);
        if fresh {
            self.seq
        } else {
            0
        }
    }

    /// A new connection: zero every instrument's clock, keep its ordinal.
    /// The ordinal stays monotone, so no book sees it fall; a clock a
    /// far-future stamp poisoned clears. Safe because the caller's old
    /// socket and rx are gone — no tick of the old connection can follow,
    /// and the first per instrument is the new subscription's, i.e. the
    /// current state. Cold: once per reconnect.
    pub fn reset_clocks(all: &mut [Self]) {
        for s in all {
            s.last_ms = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ArrivalSeq;

    #[test]
    fn same_ms_takes_the_next_ordinal_and_older_takes_zero() {
        let mut s = ArrivalSeq::ZERO;
        assert_eq!(s.next(1_000), 1);
        assert_eq!(s.next(1_000), 2, "same ms: the next ordinal");
        assert_eq!(s.next(999), 0, "older: 0");
        assert_eq!(s.next(1_000), 3, "an older tick takes no ordinal");
        assert_eq!(s.next(1_001), 4);
    }

    #[test]
    fn reset_clocks_clears_the_clock_and_keeps_the_ordinal() {
        let mut all = [ArrivalSeq::ZERO; 2];
        assert_eq!(all[0].next(u64::MAX / 2), 1);
        assert_eq!(all[0].next(1_000), 0, "a poisoned clock");
        assert_eq!(all[1].next(7), 1, "instruments are independent");
        ArrivalSeq::reset_clocks(&mut all);
        assert_eq!(all[0].next(1_000), 2);
        assert_eq!(all[1].next(5), 2, "an older stamp than before the reset");
    }

    #[test]
    fn the_low_32_bit_ms_boundary_does_not_wrap() {
        const WRAP: u64 = 418 << 32; // 2026-11-21 21:25:29.728Z
        let mut s = ArrivalSeq::ZERO;
        let seqs = [WRAP - 1, WRAP, WRAP + 1].map(|ms| s.next(ms));
        assert_eq!(seqs, [1, 2, 3]);
    }
}
