// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The per-instrument arrival sequencer behind [`crate::Tick::venue_seq`]
//! on the top-of-book streams that carry no sequence number — Polymarket
//! (HK31), Hyperliquid `bbo` and the outcome-leg `l2Book` touch, Deribit
//! `quote` (HK32) — and on those whose sequence id is a full `u64` that
//! the `u32` slot cannot hold — Binance `bookTicker` `u`, Bybit
//! `orderbook.1` `u`, MEXC book `version` (HK36).
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
//! HK36: the sequence-carrying venues truncated their full `u64` id to
//! its low 32 bits, which wrap per instrument (Binance USDⓈ-M ids are
//! already past 2^32), after which every tick of it would be `Stale`
//! until a restart. The sequencer takes the venue's FULL id as its key
//! instead of the ms stamp: the same law, a finer clock.
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
/// stream that carries none, or carries a `u64` id (module docs). The
/// KEY is the venue's ms stamp or its full sequence id, whichever the
/// stream orders by. A tick whose key is not older than the instrument's
/// newest (`key >= last_key`) takes the next ordinal — 1, 2, 3, … in
/// arrival order; a same-key duplicate takes one too (the same touch) —
/// and an OLDER one takes 0, which `book_builder::TopOfBook` drops as
/// `Stale` at every book (the current ordinal would not: a book that
/// missed the newest tick to a ring drop would apply the older touch).
/// The caller counts every 0 on `seq_regressions_total`: `last_key` only
/// rises within a connection, so one far-future key would otherwise drop
/// the instrument silently until [`ArrivalSeq::reset_clocks`] (or, at a
/// venue's own reset signal, [`ArrivalSeq::reset_clock`]) clears it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct ArrivalSeq {
    /// Key (venue ms stamp or full sequence id) of the newest tick
    /// sequenced on this connection; 0 = none (fresh, or zeroed by a
    /// reconnect).
    last_key: u64,
    /// The last ordinal handed out; 0 = none yet (the book's "never
    /// seen"). Never reset: monotone for the life of the process.
    seq: u32,
}

impl ArrivalSeq {
    /// Nothing sequenced yet.
    pub const ZERO: Self = Self {
        last_key: 0,
        seq: 0,
    };

    /// The `venue_seq` of a tick keyed `key` (its ms stamp or full venue
    /// id): the next ordinal (from 1) unless the key is older than the
    /// newest, then 0. Branch-free.
    #[inline(always)]
    pub fn next(&mut self, key: u64) -> u32 {
        // 2^32 ticks of one instrument take ~61 days of process uptime at
        // the busiest measured one-instrument rate (Binance USDⓈ-M
        // BTCUSDT `bookTicker`, 2 428 frames in 3 s on 2026-09-23), ~76 at
        // Polymarket's one-asset peak (655 ticks/s); Deribit's busiest
        // perp averages 34/s, Hyperliquid's 10/s. Unreachable in operation:
        // daily-restart boots a new process at 00:10/08:33/16:05/20:15/21:15Z.
        // Past it (release) the ordinal wraps to 0 and the instrument is
        // `Stale` until a restart.
        debug_assert!(self.seq != u32::MAX, "arrival ordinal exhausted");
        let fresh = key >= self.last_key;
        self.seq = self.seq.wrapping_add(fresh as u32);
        self.last_key = self.last_key.max(key);
        if fresh {
            self.seq
        } else {
            0
        }
    }

    /// A venue's own reset signal for ONE instrument (HK36: a Bybit
    /// `orderbook.1` snapshot, which follows a venue restart that restarts
    /// `u` at 1): zero its clock, keep its ordinal — the
    /// [`Self::reset_clocks`] law for one row. Safe only where the signal
    /// is the instrument's current state on its one ordered stream.
    #[inline(always)]
    pub fn reset_clock(&mut self) {
        self.last_key = 0;
    }

    /// A new connection: zero every instrument's clock, keep its ordinal.
    /// The ordinal stays monotone, so no book sees it fall; a clock a
    /// far-future key poisoned clears. Safe because the caller's old
    /// socket and rx are gone — no tick of the old connection can follow,
    /// and the first per instrument is the new subscription's, i.e. the
    /// current state. Cold: once per reconnect.
    pub fn reset_clocks(all: &mut [Self]) {
        for s in all {
            s.reset_clock();
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
    fn a_full_u64_id_across_the_low_32_bit_boundary_does_not_wrap() {
        // HK36: the sequence-carrying venues key on their full id.
        let mut s = ArrivalSeq::ZERO;
        let seqs = [0xFFFF_FFFF, 0x1_0000_0001, 0x1_0000_0001].map(|id| s.next(id));
        assert_eq!(
            seqs,
            [1, 2, 3],
            "a same-id duplicate takes the next ordinal"
        );
        assert_eq!(s.next(0xFFFF_FFFF), 0, "a backward id: 0");
    }

    #[test]
    fn reset_clock_clears_one_instrument_and_keeps_its_ordinal() {
        let mut all = [ArrivalSeq::ZERO; 2];
        assert_eq!(all[0].next(400), 1);
        assert_eq!(all[1].next(900), 1);
        all[0].reset_clock();
        assert_eq!(all[0].next(1), 2, "a restarted id after the reset");
        assert_eq!(all[1].next(1), 0, "the other instrument's clock stands");
    }

    #[test]
    fn the_low_32_bit_ms_boundary_does_not_wrap() {
        const WRAP: u64 = 418 << 32; // 2026-11-21 21:25:29.728Z
        let mut s = ArrivalSeq::ZERO;
        let seqs = [WRAP - 1, WRAP, WRAP + 1].map(|ms| s.next(ms));
        assert_eq!(seqs, [1, 2, 3]);
    }
}
