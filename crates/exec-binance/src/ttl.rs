// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The TTL wheel (BX-13).** Gateway thread.
//!
//! Binance has no short good-till-date, so the gateway enforces a maker's
//! TTL itself: at the deadline it sends the cancel (the order retires as
//! `CANCELED_TTL`). One hashed wheel of [`BUCKETS`] buckets of
//! [`GRAN_NS`] each; an order is an intrusive doubly linked node indexed by
//! its open-order-table slot, so insert and remove are O(1) and nothing
//! allocates. A deadline past the horizon stays in its bucket and is
//! skipped until its lap comes round.

use crate::oot::OOT_CAP;

/// Bucket width: 4 ms.
pub const GRAN_NS: u64 = 4_000_000;
/// Buckets (a 2.048 s lap).
pub const BUCKETS: usize = 512;
const NIL: u16 = u16::MAX;

/// The wheel.
pub struct TtlWheel {
    head: Box<[u16; BUCKETS]>,
    next: Box<[u16; OOT_CAP]>,
    prev: Box<[u16; OOT_CAP]>,
    deadline: Box<[u64; OOT_CAP]>,
    bucket_of: Box<[u16; OOT_CAP]>,
    /// The tick (`now / GRAN_NS`) up to which every bucket was visited.
    cursor: u64,
    len: usize,
}

impl TtlWheel {
    /// An empty wheel starting at `now_ns`.
    #[must_use]
    pub fn new(now_ns: u64) -> Self {
        Self {
            head: Box::new([NIL; BUCKETS]),
            next: Box::new([NIL; OOT_CAP]),
            prev: Box::new([NIL; OOT_CAP]),
            deadline: Box::new([0; OOT_CAP]),
            bucket_of: Box::new([0; OOT_CAP]),
            cursor: now_ns / GRAN_NS,
            len: 0,
        }
    }

    /// Arm `ix` (an open-order slot not on the wheel) for `deadline_ns`. A
    /// deadline already behind the cursor goes into the cursor's bucket, so
    /// the next pass fires it.
    #[inline]
    pub fn insert(&mut self, ix: u16, deadline_ns: u64) {
        let i = ix as usize;
        debug_assert!(self.prev[i] == NIL && self.next[i] == NIL, "armed twice");
        let tick = (deadline_ns / GRAN_NS).max(self.cursor);
        let b = (tick as usize) & (BUCKETS - 1);
        let h = self.head[b];
        self.deadline[i] = deadline_ns;
        self.bucket_of[i] = b as u16;
        self.next[i] = h;
        self.prev[i] = NIL;
        if h != NIL {
            self.prev[h as usize] = ix;
        }
        self.head[b] = ix;
        self.len += 1;
    }

    /// Disarm `ix` (it must be on the wheel).
    #[inline]
    pub fn remove(&mut self, ix: u16) {
        let i = ix as usize;
        let (p, n) = (self.prev[i], self.next[i]);
        if p != NIL {
            self.next[p as usize] = n;
        } else {
            let b = self.bucket_of[i] as usize;
            debug_assert!(self.head[b] == ix, "not on the wheel");
            self.head[b] = n;
        }
        if n != NIL {
            self.prev[n as usize] = p;
        }
        self.prev[i] = NIL;
        self.next[i] = NIL;
        self.len -= 1;
    }

    /// The next order whose deadline has passed at `now_ns`, taken off the
    /// wheel; `None` when there is none. Call until `None` each timer pass.
    pub fn pop_expired(&mut self, now_ns: u64) -> Option<u16> {
        if self.len == 0 {
            self.cursor = now_ns / GRAN_NS;
            return None;
        }
        let now_tick = now_ns / GRAN_NS;
        // A stall longer than a lap visits each bucket once.
        if now_tick.saturating_sub(self.cursor) >= BUCKETS as u64 {
            self.cursor = now_tick - (BUCKETS as u64 - 1);
        }
        loop {
            let b = (self.cursor as usize) & (BUCKETS - 1);
            let mut ix = self.head[b];
            while ix != NIL {
                if self.deadline[ix as usize] <= now_ns {
                    self.remove(ix);
                    return Some(ix);
                }
                ix = self.next[ix as usize];
            }
            if self.cursor >= now_tick {
                return None;
            }
            self.cursor += 1;
        }
    }

    /// The deadline `ix` is armed for (it must be on the wheel).
    #[inline]
    #[must_use]
    pub fn deadline_of(&self, ix: u16) -> u64 {
        self.deadline[ix as usize]
    }

    /// Orders on the wheel.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Nothing armed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn fires_at_the_deadline_not_before() {
        let mut w = TtlWheel::new(0);
        w.insert(3, 5_000 * MS);
        w.insert(4, 10 * MS);
        assert_eq!(w.pop_expired(9 * MS), None);
        assert_eq!(w.pop_expired(10 * MS), Some(4));
        assert_eq!(w.pop_expired(4_999 * MS), None, "a later lap waits");
        assert_eq!(w.pop_expired(5_000 * MS), Some(3));
        assert!(w.is_empty());
    }

    #[test]
    fn remove_disarms() {
        let mut w = TtlWheel::new(0);
        w.insert(1, 8 * MS);
        w.insert(2, 8 * MS);
        w.insert(7, 8 * MS);
        w.remove(2);
        let mut got = [w.pop_expired(9 * MS), w.pop_expired(9 * MS), w.pop_expired(9 * MS)];
        got.sort();
        assert_eq!(got, [None, Some(1), Some(7)]);
        // Re-armable after firing.
        w.insert(1, 20 * MS);
        assert_eq!(w.pop_expired(20 * MS), Some(1));
    }

    #[test]
    fn a_deadline_already_past_fires_on_the_next_pass() {
        let mut w = TtlWheel::new(0);
        w.insert(9, 100_000 * MS);
        assert_eq!(w.pop_expired(50_000 * MS), None);
        w.insert(5, 10 * MS); // long past: behind the cursor
        assert_eq!(w.pop_expired(50_000 * MS), Some(5));
    }

    #[test]
    fn a_long_stall_fires_everything_due() {
        let mut w = TtlWheel::new(0);
        for i in 0..100u16 {
            w.insert(i, (i as u64 + 1) * 37 * MS);
        }
        let mut n = 0;
        while w.pop_expired(60_000 * MS).is_some() {
            n += 1;
        }
        assert_eq!(n, 100);
    }

    proptest! {
        /// Against a model: every popped order is due, and after a full
        /// drain at `now` no due order remains armed.
        #[test]
        fn matches_a_model(deadlines in proptest::collection::vec(0u64..20_000, 1..200),
                           steps in proptest::collection::vec(1u64..3_000, 1..40)) {
            let mut w = TtlWheel::new(0);
            let mut armed = std::collections::BTreeMap::new();
            for (i, d) in deadlines.iter().enumerate() {
                w.insert(i as u16, d * MS);
                armed.insert(i as u16, d * MS);
            }
            let mut now = 0u64;
            for s in steps {
                now += s * MS;
                while let Some(ix) = w.pop_expired(now) {
                    let d = armed.remove(&ix).expect("popped an order not armed");
                    prop_assert!(d <= now);
                }
                prop_assert!(armed.values().all(|&d| d > now));
                prop_assert_eq!(w.len(), armed.len());
            }
        }
    }
}
