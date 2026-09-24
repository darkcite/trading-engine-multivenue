// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-ring
//!
//! A single-producer / single-consumer, lock-free, fixed-capacity ring
//! of `Copy` PODs. One ring is shared between exactly two threads: an
//! ingress producer and the engine consumer.
//!
//! ## The data path
//!
//! The producer copies ONE element into its slot and publishes it —
//! [`Producer::try_push_ref`], the ring-slot publish, the zero-copy
//! law's designed copy. The consumer reads that slot IN PLACE through a
//! [`Popped`] guard — [`Consumer::try_pop_ref`] — and the slot is
//! released when the guard drops. No element crosses a function
//! boundary by value.
//!
//! ## Guarantees
//!
//! * **Zero-alloc after construction.** The `N` slots are embedded in
//!   the ring; `try_push_ref` / `try_pop_ref` touch no heap.
//! * **`Copy` elements only.** `Ring::new` — the only constructor — and
//!   every handle method are bounded on `T: Copy`, so a ring never runs
//!   a destructor: an unconsumed element is just bytes, and dropping the
//!   ring frees its slots without visiting them.
//! * **No bounds checks on the hot path.** Power-of-two capacity is
//!   enforced at compile time; `pos & (N - 1)` replaces `pos % N`, and a
//!   slot is reached through a raw element pointer.
//! * **No reference into the buffer's contents.** Every slot is its own
//!   `UnsafeCell`: a slot lookup borrows the cells, never their bytes, so
//!   the producer writing one slot and the consumer reading another never
//!   alias — sound under Stacked and Tree Borrows (Miri-checked).
//! * **No false sharing between the indices.** `head` and `tail` each
//!   own a 128 B granule: the Apple M-series report a 128 B cache line,
//!   and x86's adjacent-line prefetcher pulls 64 B lines in pairs.
//! * **SPSC by construction.** A ring splits exactly once
//!   ([`Ring::split`]); `Producer` and `Consumer` are `Send + !Sync`,
//!   every mutating method takes `&mut self`, and a [`Popped`] borrows
//!   its `Consumer` exclusively.
//!
//! ## Shape
//!
//! ```text
//!     ┌────────────────────── Ring<T, N> ──────────────────────┐
//!     │ head  (128 B granule; written by the Producer)          │
//!     │ tail  (128 B granule; written by the Consumer)          │
//!     │ buf[0..N] = UnsafeCell<MaybeUninit<T>>, one per slot    │
//!     │ split (set by the one successful split)                 │
//!     └─────────────────────────────────────────────────────────┘
//!                │                              │
//!            Producer                        Consumer ──► Popped (lends a slot)
//! ```
//!
//! ## Memory ordering
//!
//! * Producer: `tail` is loaded with **Acquire** — it pairs with the
//!   Release store in `Popped::drop`, so every read the consumer made of
//!   a slot happens-before the producer writes that slot again (the
//!   write-after-read hazard, real on aarch64). The copy into the slot
//!   is then published by the **Release** store of `head`.
//! * Consumer: `head` is loaded with **Acquire** — it pairs with that
//!   Release, so a slot's bytes are visible before [`Popped`] lends
//!   them. Dropping the guard stores `tail` with **Release**.
//! * Each side's own index is loaded Relaxed: it is its only writer.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::ptr::{addr_of_mut, NonNull};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------
// One index per coherence granule.
// ---------------------------------------------------------------

/// The coherence granule the indices are padded to. 128 B: the Apple
/// M4 Pro this engine runs on reports `hw.cachelinesize: 128`, and x86's
/// adjacent-line prefetcher pulls 64 B lines in pairs.
const GRANULE: usize = 128;

/// An `AtomicUsize` alone in its granule, so `head` and `tail` never
/// share a line.
#[repr(C, align(128))]
struct Padded {
    value: AtomicUsize,
    _pad: [u8; GRANULE - ::core::mem::size_of::<AtomicUsize>()],
}

impl Padded {
    const fn new(v: usize) -> Self {
        Self {
            value: AtomicUsize::new(v),
            _pad: [0; GRANULE - ::core::mem::size_of::<AtomicUsize>()],
        }
    }
}

// ---------------------------------------------------------------
// The ring itself.
// ---------------------------------------------------------------

/// SPSC lock-free ring. `N` must be a power of two, at least 2.
#[repr(C)] // pins head | tail | buf | split: `buf` starts at 256
pub struct Ring<T, const N: usize> {
    /// Next position the producer writes. Written by the `Producer`
    /// only; published with Release.
    head: Padded,
    /// Next position the consumer reads. Written by the `Consumer` only
    /// (a [`Popped`] drop); released with Release.
    tail: Padded,
    /// The slots — one `UnsafeCell` each: a lookup borrows the cells,
    /// never their contents, so the two threads never alias.
    buf: [UnsafeCell<MaybeUninit<T>>; N],
    /// Set by the one successful [`Ring::split`] (boot only).
    split: AtomicBool,
}

// SAFETY: a `Ring` moves between threads only inside the `Arc` its two
// handles share; its elements are `T: Send`, and the indices are atomics.
unsafe impl<T: Send, const N: usize> Send for Ring<T, N> {}
// SAFETY: `&Ring` alone mutates nothing. Only the split handles touch the
// slots, and there is exactly one of each — `split` succeeds once — each
// `!Sync` by its `PhantomData<UnsafeCell<()>>` marker: the producer writes
// only slots the consumer has released (observed through `tail` with
// Acquire), the consumer reads only slots the producer has published
// (observed through `head` with Acquire).
unsafe impl<T: Send, const N: usize> Sync for Ring<T, N> {}

impl<T: Copy, const N: usize> Ring<T, N> {
    /// Create a new empty ring.
    ///
    /// # Panics (at compile time)
    ///
    /// Fails to compile when `N` is not a power of two or `N < 2`.
    pub fn new() -> Arc<Self> {
        const {
            assert!(
                N >= 2 && N.is_power_of_two(),
                "core-ring: N must be a power of two, at least 2"
            )
        };
        // Built in place inside its own `Arc` allocation: a tick ring is
        // 1 MiB of slots, so a stack temporary would blow a test thread's
        // stack, and a `Box` → `Arc` conversion would copy the megabyte
        // into a second allocation.
        let mut arc: Arc<MaybeUninit<Self>> = Arc::new_uninit();
        let Some(slot) = Arc::get_mut(&mut arc) else {
            unreachable!("a fresh Arc is unique");
        };
        let p = slot.as_mut_ptr();
        // SAFETY: `p` points at the uninitialised `Ring` inside the fresh,
        // unique `Arc`. `head`, `tail` and `split` are written once, through
        // raw field pointers; `buf` is left as the allocator gave it — any
        // bytes are a valid `[UnsafeCell<MaybeUninit<T>>; N]`.
        unsafe {
            addr_of_mut!((*p).head).write(Padded::new(0));
            addr_of_mut!((*p).tail).write(Padded::new(0));
            addr_of_mut!((*p).split).write(AtomicBool::new(false));
        }
        // SAFETY: every field with a validity invariant was written above.
        unsafe { arc.assume_init() }
    }

    /// Capacity of the ring (equal to the generic `N`).
    #[inline(always)]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Split this ring into its one producer and its one consumer.
    /// Each is `Send` but neither is `Sync`.
    ///
    /// # Panics
    ///
    /// If this ring was split before: a second pair of handles would be
    /// a second producer — a data race. Boot-time, fail-fast (release
    /// aborts).
    pub fn split(self: Arc<Self>) -> (Producer<T, N>, Consumer<T, N>) {
        let first = !self.split.swap(true, Ordering::AcqRel);
        assert!(first, "core-ring: a ring is split exactly once (SPSC)");
        let prod = Producer {
            ring: Arc::clone(&self),
            _not_sync: PhantomData,
        };
        let cons = Consumer {
            ring: self,
            _not_sync: PhantomData,
        };
        (prod, cons)
    }

    /// The slot at position `pos` (any position; masked to `pos & (N - 1)`).
    #[inline(always)]
    fn slot(&self, pos: usize) -> *mut T {
        // SAFETY: `pos & (N - 1) < N` — N is a power of two ≥ 2 (asserted
        // at compile time in `new`, the only constructor).
        let cell = unsafe { self.buf.get_unchecked(pos & (N - 1)) };
        cell.get().cast::<T>()
    }
}

// ---------------------------------------------------------------
// Producer
// ---------------------------------------------------------------

/// Single producer handle. `!Sync` by construction.
pub struct Producer<T, const N: usize> {
    ring: Arc<Ring<T, N>>,
    // The producer's unique-writer status is a runtime invariant
    // enforced by the type — make it !Sync so the compiler stops
    // you from sharing it across threads.
    _not_sync: PhantomData<UnsafeCell<()>>,
}

// SAFETY: Producer can be moved between threads; it is NOT `Sync`.
unsafe impl<T: Send, const N: usize> Send for Producer<T, N> {}

impl<T: Copy, const N: usize> Producer<T, N> {
    /// Copy `*src` into the next free slot and publish it. `false` = the
    /// ring was full and nothing was written.
    ///
    /// Zero-alloc on all paths.
    #[inline(always)]
    #[must_use = "`false` means the ring was full and nothing was published"]
    pub fn try_push_ref(&mut self, src: &T) -> bool {
        // The producer is `head`'s only writer.
        let head = self.ring.head.value.load(Ordering::Relaxed);
        // Pairs with `Popped::drop`'s Release: the consumer's reads of the
        // slot we are about to reuse happen-before our write.
        let tail = self.ring.tail.value.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= N {
            return false;
        }
        let dst = self.ring.slot(head);
        // SAFETY: (1) `dst` is the slot at `head`; `head − tail < N`
        // observed with Acquire means the consumer released it. (2) No
        // `&T` lives in it: a live `Popped` lends a position `pos` with
        // `tail ≤ pos < head` (the `tail` loaded above may lag the
        // consumer, never lead it), so `0 < head − pos ≤ head − tail < N`
        // and `head ≢ pos (mod N)`. (3) `src` cannot overlap `dst`: a `&T`
        // into this ring comes only from that `Popped`, a different slot by
        // (2); a `&T` into anything else is another allocation. (4)
        // `T: Copy`, so duplicating its bytes is a copy.
        unsafe {
            // COPY: one `T` from the caller's value into its ring slot — ≤ 64 B
            // on every lane but depth (192 B `DepthTopK`) and the ruleset table
            // (32 832 B, operator cadence) — the designed ring-slot publish: the
            // slot IS the message the consumer reads in place — rejected: a
            // claimed slot built in place (the ingress lanes capture `T` before
            // the push, the depth gate keeps its row; the other producers are
            // ≤ 64 B warm or operator cadence — not worth a second push path).
            ::core::ptr::copy_nonoverlapping(src, dst, 1);
        }
        // Publishes the bytes written above.
        self.ring
            .head
            .value
            .store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Number of slots currently full (snapshot, may race).
    #[inline]
    pub fn len(&self) -> usize {
        let h = self.ring.head.value.load(Ordering::Relaxed);
        let t = self.ring.tail.value.load(Ordering::Relaxed);
        h.wrapping_sub(t)
    }

    /// True iff no slots are currently full (snapshot, may race).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------
// Consumer
// ---------------------------------------------------------------

/// Single consumer handle. `!Sync` by construction.
pub struct Consumer<T, const N: usize> {
    ring: Arc<Ring<T, N>>,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

// SAFETY: Consumer can be moved between threads; it is NOT `Sync`.
unsafe impl<T: Send, const N: usize> Send for Consumer<T, N> {}

impl<T: Copy, const N: usize> Consumer<T, N> {
    /// The oldest element, lent IN PLACE: the returned guard derefs to
    /// the slot itself (no byte is copied) and releases it when dropped.
    /// `None` if the ring is empty.
    ///
    /// While the guard lives its slot still counts as occupied, and the
    /// `Consumer` stays borrowed — no second guard, no `len()`. A guard
    /// that is `mem::forget`ten consumes nothing: the next call lends the
    /// same element again (safe, but a liveness bug — don't).
    #[inline(always)]
    #[must_use = "dropping the guard consumes the element"]
    pub fn try_pop_ref(&mut self) -> Option<Popped<'_, T, N>> {
        // The consumer is `tail`'s only writer (through its guards).
        let pos = self.ring.tail.value.load(Ordering::Relaxed);
        // Pairs with `try_push_ref`'s Release: the slot's bytes are
        // visible before they are lent.
        let head = self.ring.head.value.load(Ordering::Acquire);
        if head == pos {
            return None;
        }
        // SAFETY: a slot pointer points into the `Arc`'d ring — never null.
        let slot = unsafe { NonNull::new_unchecked(self.ring.slot(pos)) };
        Some(Popped {
            cons: self,
            slot,
            pos,
        })
    }

    /// Snapshot length (may race).
    #[inline]
    pub fn len(&self) -> usize {
        let h = self.ring.head.value.load(Ordering::Relaxed);
        let t = self.ring.tail.value.load(Ordering::Relaxed);
        h.wrapping_sub(t)
    }

    /// True iff the ring is empty (snapshot, may race).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------
// Popped: one element, lent in place
// ---------------------------------------------------------------

/// The oldest element of a ring, lent in place by
/// [`Consumer::try_pop_ref`]. Derefs to the slot itself; the slot is
/// released — the element consumed — when the guard drops.
pub struct Popped<'a, T, const N: usize> {
    /// Exclusive: no second guard while this one lives.
    cons: &'a mut Consumer<T, N>,
    slot: NonNull<T>,
    pos: usize,
}

impl<T, const N: usize> Deref for Popped<'_, T, N> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        // SAFETY: position `pos` was published (`head > pos` observed with
        // Acquire in `try_pop_ref`), and the producer cannot write it again
        // until `drop` stores `tail = pos + 1`. The ring outlives the guard
        // (the borrowed `Consumer` holds its `Arc`), and the returned `&T`
        // cannot outlive `&self`.
        unsafe { self.slot.as_ref() }
    }
}

impl<T, const N: usize> Drop for Popped<'_, T, N> {
    #[inline(always)]
    fn drop(&mut self) {
        // Every read made through this guard happens-before the producer's
        // Acquire load that observes the release.
        self.cons
            .ring
            .tail
            .value
            .store(self.pos.wrapping_add(1), Ordering::Release);
    }
}

// ---------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn push_ref_then_pop_ref_recovers_value() {
        let (mut p, mut c) = Ring::<u64, 4>::new().split();
        assert!(p.try_push_ref(&42));
        assert_eq!(c.try_pop_ref().as_deref().copied(), Some(42));
        assert!(c.try_pop_ref().is_none());
    }

    #[test]
    fn empty_pop_ref_returns_none() {
        let (_p, mut c) = Ring::<u64, 4>::new().split();
        assert!(c.try_pop_ref().is_none());
    }

    #[test]
    fn push_ref_is_refused_at_exactly_n() {
        let (mut p, _c) = Ring::<u64, 4>::new().split();
        for v in 1..=4u64 {
            assert!(p.try_push_ref(&v), "slot {v} of 4");
        }
        assert!(!p.try_push_ref(&5), "a full ring writes nothing");
    }

    #[test]
    fn pop_ref_keeps_fifo_order() {
        let (mut p, mut c) = Ring::<u64, 8>::new().split();
        for v in 0..8u64 {
            assert!(p.try_push_ref(&v));
        }
        for v in 0..8u64 {
            assert_eq!(c.try_pop_ref().as_deref().copied(), Some(v));
        }
        assert!(c.try_pop_ref().is_none());
    }

    #[test]
    fn push_ref_pop_ref_wrap_around() {
        let (mut p, mut c) = Ring::<u64, 4>::new().split();
        for v in 0..1000u64 {
            assert!(p.try_push_ref(&v));
            assert_eq!(c.try_pop_ref().as_deref().copied(), Some(v));
        }
    }

    #[test]
    fn a_held_guard_keeps_its_slot() {
        let (mut p, mut c) = Ring::<u64, 4>::new().split();
        for v in 0..4u64 {
            assert!(p.try_push_ref(&v));
        }
        let g = c.try_pop_ref().expect("full ring");
        assert_eq!(*g, 0);
        assert!(
            !p.try_push_ref(&9),
            "the lent slot still counts as occupied"
        );
        drop(g);
        assert!(p.try_push_ref(&9), "dropping the guard released the slot");
        for want in [1u64, 2, 3, 9] {
            assert_eq!(c.try_pop_ref().as_deref().copied(), Some(want));
        }
    }

    #[test]
    fn a_forgotten_guard_consumes_nothing() {
        let (mut p, mut c) = Ring::<u64, 4>::new().split();
        assert!(p.try_push_ref(&7));
        ::core::mem::forget(c.try_pop_ref().expect("one element"));
        assert_eq!(c.try_pop_ref().as_deref().copied(), Some(7), "lent again");
        assert!(c.try_pop_ref().is_none());
    }

    #[test]
    fn the_guard_lends_the_slot_itself() {
        let ring = Ring::<u64, 4>::new();
        let base = Arc::as_ptr(&ring) as usize;
        let end = base + ::core::mem::size_of::<Ring<u64, 4>>();
        let (mut p, mut c) = ring.split();
        assert!(p.try_push_ref(&11));
        let first = {
            let g = c.try_pop_ref().expect("one element");
            let at = &*g as *const u64 as usize;
            ::core::mem::forget(g);
            at
        };
        let g = c.try_pop_ref().expect("still there");
        let again = &*g as *const u64 as usize;
        assert!(
            (base..end).contains(&again),
            "the reference points into the ring"
        );
        assert_eq!(first, again, "no copy: both lends are the same slot");
        assert_eq!(*g, 11);
    }

    #[test]
    fn a_popped_reference_pushes_into_another_ring() {
        let (mut pa, mut ca) = Ring::<[u64; 3], 4>::new().split();
        let (mut pb, mut cb) = Ring::<[u64; 3], 4>::new().split();
        assert!(pa.try_push_ref(&[1, 2, 3]));
        {
            let g = ca.try_pop_ref().expect("one element");
            assert!(pb.try_push_ref(&g));
        }
        assert!(ca.try_pop_ref().is_none());
        assert_eq!(cb.try_pop_ref().as_deref().copied(), Some([1, 2, 3]));
    }

    #[test]
    #[should_panic(expected = "a ring is split exactly once")]
    fn split_twice_panics() {
        let ring = Ring::<u64, 4>::new();
        let again = Arc::clone(&ring);
        let _first = ring.split();
        let _second = again.split();
    }

    #[test]
    fn ring_layout_keeps_the_indices_a_granule_apart() {
        assert_eq!(::core::mem::align_of::<Padded>(), 128);
        assert_eq!(::core::mem::size_of::<Padded>(), 128);
        let head = ::core::mem::offset_of!(Ring<u64, 4>, head);
        let tail = ::core::mem::offset_of!(Ring<u64, 4>, tail);
        let buf = ::core::mem::offset_of!(Ring<u64, 4>, buf);
        assert_eq!(head, 0);
        assert_eq!(tail - head, 128);
        assert_eq!(buf, 256);
    }

    #[test]
    fn capacity_is_reported_correctly() {
        let r: Arc<Ring<u64, 16>> = Ring::new();
        assert_eq!(r.capacity(), 16);
    }

    // ---- two threads (the M4 is weakly ordered: these are real tests)

    /// Values handed through a ring by two threads; scaled down under
    /// Miri, which interprets every step.
    #[cfg(not(miri))]
    const HANDOFF: u32 = 16_384;
    #[cfg(miri)]
    const HANDOFF: u32 = 64;

    #[test]
    fn two_thread_in_place_handoff_delivers_every_value_once() {
        let (mut p, mut c) = Ring::<u32, 8>::new().split();
        let prod = std::thread::spawn(move || {
            let mut i: u32 = 0;
            while i < HANDOFF {
                if p.try_push_ref(&i) {
                    i += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        let cons = std::thread::spawn(move || {
            let mut expected: u32 = 0;
            while expected < HANDOFF {
                match c.try_pop_ref() {
                    Some(g) => {
                        assert_eq!(*g, expected);
                        expected += 1;
                    }
                    None => std::hint::spin_loop(),
                }
            }
        });
        prod.join().unwrap();
        cons.join().unwrap();
    }

    /// A 192 B payload (the `DepthTopK` size) whose every word carries
    /// the same value: a torn read shows as two different words.
    #[derive(Copy, Clone)]
    #[repr(C, align(64))]
    struct Wide([u64; 24]);

    #[cfg(not(miri))]
    const WIDE: u64 = 200_000;
    #[cfg(miri)]
    const WIDE: u64 = 64;

    #[test]
    fn two_thread_192_byte_payload_is_never_torn() {
        let (mut p, mut c) = Ring::<Wide, 8>::new().split();
        let prod = std::thread::spawn(move || {
            let mut i: u64 = 0;
            while i < WIDE {
                if p.try_push_ref(&Wide([i; 24])) {
                    i += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        let cons = std::thread::spawn(move || {
            let mut expected: u64 = 0;
            while expected < WIDE {
                match c.try_pop_ref() {
                    Some(g) => {
                        let mut w = 0;
                        while w < 24 {
                            assert_eq!(g.0[w], expected, "word {w} of element {expected}");
                            w += 1;
                        }
                        expected += 1;
                    }
                    None => std::hint::spin_loop(),
                }
            }
        });
        prod.join().unwrap();
        cons.join().unwrap();
    }

    // ---- the model: a bounded FIFO -----------------------------------

    #[derive(Clone, Debug)]
    enum Op {
        Push(u32),
        Pop,
        /// Hold the oldest element's guard while pushing: the held
        /// element still occupies its slot.
        HoldAndPush(u32),
        /// Forget a guard: consumes nothing.
        ForgetOne,
    }

    fn op() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::*;
        prop_oneof![
            any::<u32>().prop_map(Op::Push),
            Just(Op::Pop),
            any::<u32>().prop_map(Op::HoldAndPush),
            Just(Op::ForgetOne),
        ]
    }

    proptest::proptest! {
        #[test]
        #[cfg_attr(miri, ignore)]
        fn the_ring_is_a_bounded_fifo(ops in proptest::collection::vec(op(), 0..256)) {
            let (mut p, mut c) = Ring::<u32, 8>::new().split();
            let mut model: VecDeque<u32> = VecDeque::new();
            for op in ops {
                match op {
                    Op::Push(v) => {
                        let ok = p.try_push_ref(&v);
                        proptest::prop_assert_eq!(ok, model.len() < 8);
                        if ok {
                            model.push_back(v);
                        }
                    }
                    Op::Pop => {
                        let got = c.try_pop_ref().as_deref().copied();
                        proptest::prop_assert_eq!(got, model.pop_front());
                    }
                    Op::HoldAndPush(v) => match c.try_pop_ref() {
                        Some(g) => {
                            proptest::prop_assert_eq!(Some(*g), model.front().copied());
                            let ok = p.try_push_ref(&v);
                            proptest::prop_assert_eq!(ok, model.len() < 8);
                            if ok {
                                model.push_back(v);
                            }
                            drop(g);
                            model.pop_front();
                        }
                        None => proptest::prop_assert!(model.is_empty()),
                    },
                    Op::ForgetOne => {
                        if let Some(g) = c.try_pop_ref() {
                            proptest::prop_assert_eq!(Some(*g), model.front().copied());
                            ::core::mem::forget(g);
                        }
                    }
                }
            }
        }
    }
}
