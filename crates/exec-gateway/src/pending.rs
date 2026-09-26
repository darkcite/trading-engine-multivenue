// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The in-flight post table, indexed by `id & (N − 1)`.
//!
//! One slot per post the gateway has written and the venue has not yet
//! answered. Unlike `core_net::subs::PendingTable` — whose completion
//! returns the whole row by value — a slot here holds the post's record
//! IN PLACE (for a coalesced action: which command each reply item maps
//! to), filled through [`Pending::claim`] and read through
//! [`Pending::get`] with no copy.
//!
//! Ids come from a monotonic allocator starting at 1 that SKIPS a busy
//! slot ([`Pending::is_free`]): a post that never gets its reply keeps its
//! slot, and a plain allocator would collide with it `N` ids later however
//! few posts are in flight. With the skip, a free slot exists whenever
//! fewer than `N` posts are in flight (the venue caps them well below:
//! Hyperliquid 100 against 256). A refused claim
//! ([`PendingErr::SlotBusy`]) is a driver bug. The owner must also retire a
//! post whose reply is long overdue (its deadline, then a sweep at twice
//! it) — otherwise its slot leaks until the process restarts.

/// Why a [`Pending`] operation refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PendingErr {
    /// Id 0 is reserved (free-slot marker).
    ZeroId,
    /// The `id & (N − 1)` slot still holds an older post.
    SlotBusy,
}

/// In-flight posts: `N` slots (a power of two) of `T`, allocated once.
pub struct Pending<T: Copy, const N: usize> {
    ids: Box<[u64; N]>,
    sent_ns: Box<[u64; N]>,
    slots: Box<[T; N]>,
    live: usize,
}

impl<T: Copy, const N: usize> Pending<T, N> {
    /// Boot-time: `N` slots, each initialised to `blank`.
    ///
    /// # Panics
    /// Never at run time: `N` is checked at compile time.
    #[must_use]
    pub fn new(blank: T) -> Self {
        const {
            assert!(
                N.is_power_of_two() && N >= 2,
                "Pending N must be a power of two >= 2"
            );
        }
        Self {
            ids: crate::boxed_array(0u64),
            sent_ns: crate::boxed_array(0u64),
            slots: crate::boxed_array(blank),
            live: 0,
        }
    }

    #[inline(always)]
    const fn ix(id: u64) -> usize {
        (id as usize) & (N - 1)
    }

    /// Take the slot for post `id`, stamped `now_ns`, and lend it for the
    /// caller to fill in place. The slot still holds whatever its previous
    /// post left: the caller writes every field it will read back.
    ///
    /// # Errors
    /// [`PendingErr::ZeroId`], or [`PendingErr::SlotBusy`] when an older
    /// post still holds `id & (N − 1)`.
    #[inline]
    pub fn claim(&mut self, id: u64, now_ns: u64) -> Result<&mut T, PendingErr> {
        if id == 0 {
            return Err(PendingErr::ZeroId);
        }
        let i = Self::ix(id);
        if self.ids[i] != 0 {
            return Err(PendingErr::SlotBusy);
        }
        self.ids[i] = id;
        self.sent_ns[i] = now_ns;
        self.live += 1;
        Ok(&mut self.slots[i])
    }

    /// The record of in-flight post `id`, in place.
    #[inline]
    pub fn get(&self, id: u64) -> Option<&T> {
        let i = Self::ix(id);
        if id != 0 && self.ids[i] == id {
            Some(&self.slots[i])
        } else {
            None
        }
    }

    /// The record of in-flight post `id`, in place and mutable.
    #[inline]
    pub fn get_mut(&mut self, id: u64) -> Option<&mut T> {
        let i = Self::ix(id);
        if id != 0 && self.ids[i] == id {
            Some(&mut self.slots[i])
        } else {
            None
        }
    }

    /// When post `id` was written, if it is in flight.
    #[inline]
    pub fn sent_ns(&self, id: u64) -> Option<u64> {
        let i = Self::ix(id);
        if id != 0 && self.ids[i] == id {
            Some(self.sent_ns[i])
        } else {
            None
        }
    }

    /// Free post `id`'s slot. `false` when `id` is not in flight (a late or
    /// duplicate reply: the caller counts it, never crashes).
    #[inline]
    pub fn release(&mut self, id: u64) -> bool {
        let i = Self::ix(id);
        if id == 0 || self.ids[i] != id {
            return false;
        }
        self.ids[i] = 0;
        self.live -= 1;
        true
    }

    /// Whether `id` could be claimed now.
    #[inline]
    pub fn is_free(&self, id: u64) -> bool {
        id != 0 && self.ids[Self::ix(id)] == 0
    }

    /// Posts in flight. O(1).
    #[inline]
    pub fn live(&self) -> usize {
        self.live
    }

    /// The id held by slot `slot` (`0..N`), for the cold walk that fails
    /// every in-flight post when its connection dies; `None` when free.
    #[inline]
    pub fn id_at(&self, slot: usize) -> Option<u64> {
        let id = self.ids[slot & (N - 1)];
        if id == 0 {
            None
        } else {
            Some(id)
        }
    }

    /// Slots in the table (`N`).
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    struct Rec {
        items: u8,
        oids: [u64; 4],
    }

    const BLANK: Rec = Rec {
        items: 0,
        oids: [0; 4],
    };

    #[test]
    fn a_claimed_slot_is_filled_in_place_and_released_once() {
        let mut p: Pending<Rec, 8> = Pending::new(BLANK);
        {
            let r = p.claim(3, 100).expect("claim");
            r.items = 2;
            r.oids[0] = 11;
            r.oids[1] = 12;
        }
        assert_eq!(p.live(), 1);
        assert_eq!(p.sent_ns(3), Some(100));
        assert_eq!(p.get(3).map(|r| r.oids[1]), Some(12));
        p.get_mut(3).expect("live").items = 3;
        assert_eq!(p.get(3).map(|r| r.items), Some(3));
        assert!(p.release(3));
        assert!(!p.release(3), "a duplicate reply is not a second release");
        assert_eq!(p.live(), 0);
        assert!(p.get(3).is_none());
    }

    #[test]
    fn zero_and_a_busy_slot_refuse() {
        let mut p: Pending<Rec, 4> = Pending::new(BLANK);
        assert_eq!(p.claim(0, 1).err(), Some(PendingErr::ZeroId));
        assert!(p.claim(1, 1).is_ok());
        assert_eq!(
            p.claim(5, 1).err(),
            Some(PendingErr::SlotBusy),
            "5 & 3 == 1"
        );
        assert!(!p.is_free(5));
        assert!(p.get(5).is_none(), "the older id owns the slot");
        assert!(!p.release(5));
        assert!(p.release(1));
        assert!(
            p.claim(5, 2).is_ok(),
            "free again: the id wraps onto the slot"
        );
    }

    #[test]
    fn id_at_walks_the_live_posts() {
        let mut p: Pending<Rec, 8> = Pending::new(BLANK);
        let mut id = 1u64;
        while id <= 5 {
            p.claim(id, id).expect("claim");
            id += 1;
        }
        assert!(p.release(2));
        let mut seen = 0u64;
        let mut s = 0usize;
        while s < p.capacity() {
            if let Some(id) = p.id_at(s) {
                seen += id;
            }
            s += 1;
        }
        assert_eq!(seen, 1 + 3 + 4 + 5);
    }

    proptest! {
        /// A monotonic allocator that skips busy slots never collides while
        /// fewer than N posts are in flight, whatever order replies come
        /// back in; every in-flight id reads back its own record.
        #[test]
        fn a_skipping_allocator_never_collides(ops in proptest::collection::vec((any::<bool>(), 0usize..64), 1..500)) {
            let mut p: Pending<Rec, 16> = Pending::new(BLANK);
            let mut next = 1u64;
            let mut model: Vec<u64> = Vec::new();
            for (claim, pick) in ops {
                if claim && model.len() < 15 {
                    let mut hops = 0;
                    while !p.is_free(next) {
                        next += 1;
                        hops += 1;
                        prop_assert!(hops <= 16, "a free slot exists while fewer than N are in flight");
                    }
                    let r = p.claim(next, next).expect("claim after skipping");
                    r.oids[0] = next;
                    model.push(next);
                    next += 1;
                } else if !model.is_empty() {
                    let id = model.swap_remove(pick % model.len());
                    prop_assert!(p.release(id));
                }
                prop_assert_eq!(p.live(), model.len());
                for id in &model {
                    prop_assert_eq!(p.get(*id).map(|r| r.oids[0]), Some(*id));
                }
            }
        }
    }
}
