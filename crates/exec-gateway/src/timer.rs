// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A fixed-capacity deadline queue: post timeouts, the engine-sent TTL
//! cancels (Hyperliquid has no per-order TTL), keepalive pings, re-arms.
//!
//! A binary min-heap over `(deadline, arm order)` in one contiguous array,
//! with each timer's heap position kept per slot so a cancel is
//! O(log N) — the plan's "timer wheel" as a heap: at the gateway's sizes
//! (N ≤ 1 024, ≤ 10 levels) the heap orders deadlines exactly, with no
//! rotations or cascades to get wrong, and the comparisons touch only
//! the heap array. Ties fire in the order they were armed. A
//! [`TimerId`] carries its slot's 64-bit generation, so a handle kept
//! past its timer's firing never cancels the slot's next tenant. The sift
//! loops keep their bounds checks (the crate forbids `unsafe`): at most
//! ten levels, a cost accepted over an unchecked index.

/// Handle of an armed timer (16 B: a 64-bit generation never wraps).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimerId {
    slot: u32,
    gen: u64,
}

/// A timer that came due.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Expired {
    /// The deadline it was armed for (ns).
    pub deadline_ns: u64,
    /// The caller's key (a post id, a client oid, …).
    pub key: u64,
    /// The caller's kind tag.
    pub kind: u8,
}

/// Every slot holds an armed timer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimerFull;

#[derive(Copy, Clone, Debug)]
struct HeapEnt {
    deadline_ns: u64,
    order: u64,
    slot: u32,
}

const NO_POS: u32 = u32::MAX;

/// The queue: `N` timers at most, allocated once.
pub struct TimerQueue<const N: usize> {
    heap: Box<[HeapEnt; N]>,
    len: usize,
    pos: Box<[u32; N]>,
    gen: Box<[u64; N]>,
    key: Box<[u64; N]>,
    kind: Box<[u8; N]>,
    free: Box<[u32; N]>,
    free_len: usize,
    next_order: u64,
}

impl<const N: usize> Default for TimerQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> TimerQueue<N> {
    /// Boot-time: an empty queue of `N` timers.
    #[must_use]
    pub fn new() -> Self {
        const {
            assert!(
                N >= 1 && N < u32::MAX as usize,
                "TimerQueue N must be 1 ..= u32::MAX - 1"
            );
        }
        let blank = HeapEnt {
            deadline_ns: 0,
            order: 0,
            slot: 0,
        };
        let mut free: Box<[u32; N]> = crate::boxed_array(0u32);
        let mut i = 0usize;
        while i < N {
            // Slot 0 is handed out first.
            free[i] = (N - 1 - i) as u32;
            i += 1;
        }
        Self {
            heap: crate::boxed_array(blank),
            len: 0,
            pos: crate::boxed_array(NO_POS),
            gen: crate::boxed_array(0u64),
            key: crate::boxed_array(0u64),
            kind: crate::boxed_array(0u8),
            free,
            free_len: N,
            next_order: 0,
        }
    }

    /// Armed timers.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no timer is armed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The earliest deadline, if any timer is armed.
    #[inline]
    pub fn next_deadline(&self) -> Option<u64> {
        if self.len == 0 {
            None
        } else {
            Some(self.heap[0].deadline_ns)
        }
    }

    /// Arm a timer.
    ///
    /// # Errors
    /// [`TimerFull`] when all `N` are armed. Size `N` so that never
    /// happens: a full queue is a sizing bug that halts the arm. The
    /// caller refuses a GROWING command it cannot time out; a cancel goes
    /// out regardless (counted), reconciliation being its backstop.
    #[inline]
    pub fn arm(&mut self, deadline_ns: u64, key: u64, kind: u8) -> Result<TimerId, TimerFull> {
        if self.free_len == 0 {
            return Err(TimerFull);
        }
        self.free_len -= 1;
        let slot = self.free[self.free_len];
        let s = slot as usize;
        self.key[s] = key;
        self.kind[s] = kind;
        let at = self.len;
        self.heap[at] = HeapEnt {
            deadline_ns,
            order: self.next_order,
            slot,
        };
        self.next_order += 1;
        self.pos[s] = at as u32;
        self.len += 1;
        self.sift_up(at);
        Ok(TimerId {
            slot,
            gen: self.gen[s],
        })
    }

    /// Disarm `id`. `false` when it already fired or was cancelled.
    #[inline]
    pub fn cancel(&mut self, id: TimerId) -> bool {
        let s = id.slot as usize;
        if s >= N || self.gen[s] != id.gen || self.pos[s] == NO_POS {
            return false;
        }
        let at = self.pos[s] as usize;
        self.remove_at(at);
        true
    }

    /// Pop the earliest timer if it is due at `now_ns`, into `out`.
    #[inline]
    pub fn pop_due(&mut self, now_ns: u64, out: &mut Expired) -> bool {
        if self.len == 0 || self.heap[0].deadline_ns > now_ns {
            return false;
        }
        let top = self.heap[0];
        let s = top.slot as usize;
        out.deadline_ns = top.deadline_ns;
        out.key = self.key[s];
        out.kind = self.kind[s];
        self.remove_at(0);
        true
    }

    /// Disarm everything (a gateway restart). Handles taken before stay
    /// dead.
    pub fn clear(&mut self) {
        while self.len > 0 {
            self.remove_at(self.len - 1);
        }
    }

    fn remove_at(&mut self, at: usize) {
        debug_assert!(at < self.len);
        let s = self.heap[at].slot as usize;
        self.pos[s] = NO_POS;
        self.gen[s] = self.gen[s].wrapping_add(1);
        self.free[self.free_len] = s as u32;
        self.free_len += 1;
        self.len -= 1;
        if at == self.len {
            return;
        }
        self.heap[at] = self.heap[self.len];
        self.pos[self.heap[at].slot as usize] = at as u32;
        if at > 0 && less(&self.heap[at], &self.heap[(at - 1) / 2]) {
            self.sift_up(at);
        } else {
            self.sift_down(at);
        }
    }

    fn sift_up(&mut self, mut at: usize) {
        while at > 0 {
            let parent = (at - 1) / 2;
            if !less(&self.heap[at], &self.heap[parent]) {
                break;
            }
            self.swap(at, parent);
            at = parent;
        }
    }

    fn sift_down(&mut self, mut at: usize) {
        loop {
            let l = 2 * at + 1;
            if l >= self.len {
                break;
            }
            let r = l + 1;
            let c = if r < self.len && less(&self.heap[r], &self.heap[l]) {
                r
            } else {
                l
            };
            if !less(&self.heap[c], &self.heap[at]) {
                break;
            }
            self.swap(at, c);
            at = c;
        }
    }

    #[inline]
    fn swap(&mut self, a: usize, b: usize) {
        self.heap.swap(a, b);
        self.pos[self.heap[a].slot as usize] = a as u32;
        self.pos[self.heap[b].slot as usize] = b as u32;
    }
}

#[inline(always)]
fn less(a: &HeapEnt, b: &HeapEnt) -> bool {
    (a.deadline_ns, a.order) < (b.deadline_ns, b.order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn drain<const N: usize>(q: &mut TimerQueue<N>, now: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut e = Expired {
            deadline_ns: 0,
            key: 0,
            kind: 0,
        };
        while q.pop_due(now, &mut e) {
            out.push((e.deadline_ns, e.key));
        }
        out
    }

    #[test]
    fn timers_fire_in_deadline_order_and_ties_in_arm_order() {
        let mut q: TimerQueue<8> = TimerQueue::new();
        q.arm(30, 1, 0).expect("arm");
        q.arm(10, 2, 0).expect("arm");
        q.arm(20, 3, 0).expect("arm");
        q.arm(10, 4, 0).expect("arm");
        assert_eq!(q.next_deadline(), Some(10));
        assert_eq!(drain(&mut q, 15), vec![(10, 2), (10, 4)]);
        assert_eq!(drain(&mut q, 100), vec![(20, 3), (30, 1)]);
        assert!(q.is_empty());
        assert_eq!(q.next_deadline(), None);
    }

    #[test]
    fn a_cancelled_timer_never_fires_and_a_stale_handle_is_inert() {
        let mut q: TimerQueue<4> = TimerQueue::new();
        let a = q.arm(10, 1, 7).expect("arm");
        let b = q.arm(20, 2, 7).expect("arm");
        assert!(q.cancel(a));
        assert!(!q.cancel(a), "cancel twice");
        let mut e = Expired {
            deadline_ns: 0,
            key: 0,
            kind: 0,
        };
        assert!(q.pop_due(25, &mut e));
        assert_eq!((e.key, e.kind), (2, 7));
        // b fired; its slot's next tenant must survive b's stale handle.
        let c = q.arm(40, 3, 1).expect("arm");
        assert!(!q.cancel(b));
        assert_eq!(q.len(), 1);
        assert!(q.cancel(c));
    }

    #[test]
    fn a_full_queue_refuses_and_frees_on_pop() {
        let mut q: TimerQueue<2> = TimerQueue::new();
        q.arm(1, 1, 0).expect("arm");
        q.arm(2, 2, 0).expect("arm");
        assert_eq!(q.arm(3, 3, 0), Err(TimerFull));
        let mut e = Expired {
            deadline_ns: 0,
            key: 0,
            kind: 0,
        };
        assert!(q.pop_due(1, &mut e));
        assert!(q.arm(3, 3, 0).is_ok());
        q.clear();
        assert!(q.is_empty());
        assert!(!q.pop_due(u64::MAX, &mut e));
    }

    proptest! {
        /// Against a sorted model: any interleaving of arms, cancels and
        /// pops fires exactly the armed-and-not-cancelled timers, in
        /// (deadline, arm order).
        #[test]
        fn the_queue_matches_a_sorted_model(ops in proptest::collection::vec((0u8..3, 0u64..50, 0usize..64), 1..300)) {
            let mut q: TimerQueue<32> = TimerQueue::new();
            let mut model: Vec<(u64, u64, u64)> = Vec::new(); // (deadline, order, key)
            let mut ids: Vec<(TimerId, u64)> = Vec::new();
            let mut order = 0u64;
            let mut now = 0u64;
            for (op, x, pick) in ops {
                match op {
                    0 => {
                        let d = now + x;
                        match q.arm(d, order, 0) {
                            Ok(id) => { model.push((d, order, order)); ids.push((id, order)); }
                            Err(TimerFull) => prop_assert_eq!(model.len(), 32),
                        }
                        order += 1;
                    }
                    1 if !ids.is_empty() => {
                        let (id, key) = ids.swap_remove(pick % ids.len());
                        let live = model.iter().any(|m| m.2 == key);
                        prop_assert_eq!(q.cancel(id), live);
                        model.retain(|m| m.2 != key);
                    }
                    _ => {
                        now += x;
                        model.sort();
                        let mut e = Expired { deadline_ns: 0, key: 0, kind: 0 };
                        while q.pop_due(now, &mut e) {
                            let m = model.remove(0);
                            prop_assert_eq!((e.deadline_ns, e.key), (m.0, m.2));
                            prop_assert!(m.0 <= now);
                        }
                        prop_assert!(model.first().is_none_or(|m| m.0 > now));
                    }
                }
                prop_assert_eq!(q.len(), model.len());
            }
        }
    }
}
