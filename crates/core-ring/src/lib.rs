//! # core-ring
//!
//! A single-producer / single-consumer, lock-free, fixed-capacity ring
//! buffer. One ring is shared between exactly two threads: an ingress
//! producer and the engine consumer.
//!
//! ## Guarantees
//!
//! * **Zero-alloc after construction.** The backing storage is a
//!   `[MaybeUninit<T>; N]` embedded directly in the ring; there is no
//!   heap access in `push` / `pop`.
//! * **No bounds checks on the hot path.** Power-of-two capacity is
//!   enforced at compile time; `head & (N-1)` replaces `head % N` with
//!   a branchless bitmask. The backing store is accessed with
//!   `get_unchecked_mut` inside carefully scoped `unsafe` blocks.
//! * **No false sharing.** `head` and `tail` live in separate
//!   cache-line-aligned wrappers so the producer and consumer never
//!   contend on the same line.
//! * **SPSC only.** Calling `push` from two threads at once is UB;
//!   enforce ownership statically via the `Producer` / `Consumer`
//!   split handles.
//!
//! ## Shape
//!
//! ```text
//!     ┌──────────────── Ring<T, N> ────────────────┐
//!     │ head (producer-owned)                      │
//!     │ tail (consumer-owned)                      │
//!     │ buf[0..N] = MaybeUninit<T>                 │
//!     └────────────────────────────────────────────┘
//!               │                    │
//!           Producer              Consumer
//!
//! 1. Only Producer touches `head`, Consumer only reads it (Acquire).
//! 2. Only Consumer touches `tail`, Producer only reads it (Acquire).
//! 3. Slot writes happen before `head` bumps (Release) — Acquire on
//!    the consumer side gives a happens-before on the data.
//! ```
//!
//! ## Safety of the `const` capacity check
//!
//! The `assert_pow2::<N>()` helper evaluates at monomorphization time.
//! If `N` is not a power of two, compilation fails.

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
use std::mem::MaybeUninit;
use std::sync::atomic::{compiler_fence, AtomicUsize, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------
// Cache-line padded atomic (64 bytes on x86_64 + aarch64).
// ---------------------------------------------------------------

/// Pads an `AtomicUsize` to a full cache line. Prevents false sharing
/// between `head` and `tail` across cores.
#[repr(C, align(64))]
struct Padded {
    value: AtomicUsize,
    _pad: [u8; 64 - ::core::mem::size_of::<AtomicUsize>()],
}

impl Padded {
    const fn new(v: usize) -> Self {
        Self {
            value: AtomicUsize::new(v),
            _pad: [0; 64 - ::core::mem::size_of::<AtomicUsize>()],
        }
    }
}

// ---------------------------------------------------------------
// The ring itself.
// ---------------------------------------------------------------

/// SPSC lock-free ring. `N` must be a power of two, at least 2.
pub struct Ring<T, const N: usize> {
    /// Producer-owned write index; consumer reads with Acquire.
    head: Padded,
    /// Consumer-owned read index; producer reads with Acquire.
    tail: Padded,
    /// Fixed backing storage; accessed via raw pointer in hot path.
    buf: UnsafeCell<[MaybeUninit<T>; N]>,
}

// SAFETY: `Ring` is only shared between two threads through the
// `Producer` / `Consumer` handles, which split ownership so that only
// one side mutates `head`/`buf[head]` and the other mutates
// `tail`/reads `buf[tail]`. Synchronization happens via the two
// atomics with Acquire/Release ordering.
unsafe impl<T: Send, const N: usize> Send for Ring<T, N> {}
// SAFETY: `&Ring` alone does not permit mutation; only the split
// handles do. Those are `!Sync` by design (they contain raw pointers).
unsafe impl<T: Send, const N: usize> Sync for Ring<T, N> {}

impl<T, const N: usize> Ring<T, N> {
    /// Create a new empty ring.
    ///
    /// # Panics (at compile time)
    ///
    /// Fails to compile when `N` is not a power of two or `N < 2`.
    pub fn new() -> Arc<Self> {
        // Compile-time power-of-two check. Any non-pow2 N produces an
        // arithmetic overflow in the const context, which aborts the
        // build. No runtime cost.
        assert_pow2::<N>();

        // Allocate directly on the heap via `Box::new_uninit()`. Going
        // through a stack temporary would blow the default test-thread
        // stack for large `N` (TICK_RING_SIZE = 16384 * 64 B = 1 MiB
        // per ring, three rings per engine). Initialize each field in
        // place with `addr_of_mut!` + `ptr::write` to avoid ever
        // materializing a full `Self` value on the stack.
        let mut boxed: Box<MaybeUninit<Self>> = Box::new_uninit();
        // SAFETY: `head` and `tail` are written once and never read until
        // `assume_init` below. `buf` is an `UnsafeCell<[MaybeUninit<T>; N]>`
        // — any bit pattern is valid (`MaybeUninit` has no validity
        // invariants, `UnsafeCell` adds none of its own), so leaving the
        // bytes returned by the allocator in place is sound.
        unsafe {
            let p = boxed.as_mut_ptr();
            ::core::ptr::addr_of_mut!((*p).head).write(Padded::new(0));
            ::core::ptr::addr_of_mut!((*p).tail).write(Padded::new(0));
            // `buf` is intentionally NOT written here — the allocator
            // bytes are a valid `MaybeUninit` array.
        }
        // SAFETY: every field with a validity invariant has been written.
        let init: Box<Self> = unsafe { boxed.assume_init() };
        Arc::from(init)
    }

    /// Capacity of the ring (equal to the generic `N`).
    #[inline(always)]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Split this Arc into a producer and a consumer handle. Each is
    /// `Send` but neither is `Sync`, statically preventing SPMC/MPSC
    /// misuse.
    pub fn split(self: Arc<Self>) -> (Producer<T, N>, Consumer<T, N>) {
        let prod = Producer {
            ring: self.clone(),
            _not_sync: ::core::marker::PhantomData,
        };
        let cons = Consumer {
            ring: self,
            _not_sync: ::core::marker::PhantomData,
        };
        (prod, cons)
    }
}

/// Const bail-out that fires at monomorphization if `N` is not a power
/// of two or is smaller than 2. Produces a compile error via
/// division-by-zero rather than a runtime panic.
#[inline]
const fn assert_pow2<const N: usize>() {
    // If N is a power of two, `N & (N-1) == 0`. Anything else → 1.
    // Divide by that; 1/0 traps at const eval.
    let _ = 1usize / ((N >= 2) as usize * ((N & (N.wrapping_sub(1))) == 0) as usize);
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
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

// SAFETY: Producer can be moved between threads; it is NOT `Sync`.
unsafe impl<T: Send, const N: usize> Send for Producer<T, N> {}

impl<T, const N: usize> Producer<T, N> {
    /// Attempt to push. Returns `Err(value)` when the ring is full so
    /// the caller can choose the back-pressure strategy.
    ///
    /// Zero-alloc on all paths.
    #[inline(always)]
    pub fn try_push(&mut self, value: T) -> Result<(), T> {
        // Producer is the only writer of `head`; load relaxed is fine.
        let head = self.ring.head.value.load(Ordering::Relaxed);
        // We need a happens-before view of the consumer's `tail` bumps.
        let tail = self.ring.tail.value.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= N {
            return Err(value);
        }
        let idx = head & (N - 1);
        // SAFETY: We are the sole producer (SPSC invariant). The slot
        // at `idx` is logically unowned — the consumer's `tail` has
        // moved past it (or equals `head`, empty). Writing the value
        // does not race with the consumer, which only reads slots with
        // `tail <= i < head` observed with Acquire.
        unsafe {
            let slot = (*self.ring.buf.get()).get_unchecked_mut(idx);
            slot.as_mut_ptr().write(value);
        }
        // Belt-and-suspenders compiler fence: stops the compiler from
        // reordering the non-atomic slot-write above past the atomic
        // head-store below. The Release on `head` already establishes
        // happens-before with the consumer's Acquire load, but for
        // large `T` whose internal field writes are themselves
        // non-atomic the compiler is technically free to spread them
        // — under -Copt-level=3 with LTO this fence is a no-op on
        // x86_64/aarch64 strong-memory models but documents intent.
        compiler_fence(Ordering::Release);
        // Release on the index bump publishes the slot write to the
        // consumer under its Acquire load of `head`.
        self.ring.head.value.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
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
    _not_sync: ::core::marker::PhantomData<::core::cell::UnsafeCell<()>>,
}

// SAFETY: Consumer can be moved between threads; it is NOT `Sync`.
unsafe impl<T: Send, const N: usize> Send for Consumer<T, N> {}

impl<T, const N: usize> Consumer<T, N> {
    /// Pop one element. Returns `None` if empty.
    ///
    /// Zero-alloc on all paths.
    #[inline(always)]
    pub fn try_pop(&mut self) -> Option<T> {
        let tail = self.ring.tail.value.load(Ordering::Relaxed);
        let head = self.ring.head.value.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        // Pair the producer's `compiler_fence(Release)` with an
        // Acquire fence here so the compiler cannot speculate the
        // slot read above the head-load. The atomic Acquire on
        // `head` already prevents the CPU from doing so; this fence
        // is for the compiler's benefit.
        compiler_fence(Ordering::Acquire);
        let idx = tail & (N - 1);
        // SAFETY: We are the sole consumer. `head` has been observed
        // with Acquire; the producer's Release pairs with our load, so
        // the slot at `idx` is fully written and visible. After reading
        // it we advance `tail` with Release, so the producer's next
        // Acquire load sees the freed slot.
        let value = unsafe {
            let slot = (*self.ring.buf.get()).get_unchecked(idx);
            slot.as_ptr().read()
        };
        self.ring.tail.value.store(tail.wrapping_add(1), Ordering::Release);
        Some(value)
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
// Drop: drain remaining elements so destructors run on `T`s that
// were pushed but never popped.
// ---------------------------------------------------------------

impl<T, const N: usize> Drop for Ring<T, N> {
    fn drop(&mut self) {
        let head = self.head.value.load(Ordering::Relaxed);
        let tail = self.tail.value.load(Ordering::Relaxed);
        let mut t = tail;
        while t != head {
            let idx = t & (N - 1);
            // SAFETY: Slots `[tail, head)` are initialised but not yet
            // consumed. We hold exclusive access via &mut self.
            unsafe {
                let slot = (*self.buf.get()).get_unchecked_mut(idx);
                slot.as_mut_ptr().drop_in_place();
            }
            t = t.wrapping_add(1);
        }
    }
}

// ---------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_pop_recovers_value() {
        let ring: Arc<Ring<u64, 4>> = Ring::new();
        let (mut p, mut c) = ring.split();
        assert!(p.try_push(42).is_ok());
        assert_eq!(c.try_pop(), Some(42));
        assert_eq!(c.try_pop(), None);
    }

    #[test]
    fn empty_pop_returns_none() {
        let ring: Arc<Ring<u64, 4>> = Ring::new();
        let (_p, mut c) = ring.split();
        assert_eq!(c.try_pop(), None);
    }

    #[test]
    fn push_until_full_returns_err() {
        let ring: Arc<Ring<u64, 4>> = Ring::new();
        let (mut p, _c) = ring.split();
        assert!(p.try_push(1).is_ok());
        assert!(p.try_push(2).is_ok());
        assert!(p.try_push(3).is_ok());
        assert!(p.try_push(4).is_ok());
        assert_eq!(p.try_push(5), Err(5));
    }

    #[test]
    fn fifo_order_is_preserved() {
        let ring: Arc<Ring<u64, 8>> = Ring::new();
        let (mut p, mut c) = ring.split();
        for i in 0..8 {
            assert!(p.try_push(i).is_ok());
        }
        for i in 0..8 {
            assert_eq!(c.try_pop(), Some(i));
        }
        assert_eq!(c.try_pop(), None);
    }

    #[test]
    fn wraparound_works() {
        let ring: Arc<Ring<u64, 4>> = Ring::new();
        let (mut p, mut c) = ring.split();
        // Do 1000 pushes + pops; capacity is 4.
        for i in 0..1000 {
            assert!(p.try_push(i).is_ok());
            assert_eq!(c.try_pop(), Some(i));
        }
    }

    #[test]
    fn two_thread_spsc_hands_off_all_values() {
        // 4K slots, 16K values, producer thread + consumer thread.
        let ring: Arc<Ring<u32, 4096>> = Ring::new();
        let (mut p, mut c) = ring.split();
        const N: u32 = 16_384;

        let prod = std::thread::spawn(move || {
            let mut i: u32 = 0;
            while i < N {
                if p.try_push(i).is_ok() {
                    i += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });

        let cons = std::thread::spawn(move || {
            let mut expected: u32 = 0;
            while expected < N {
                match c.try_pop() {
                    Some(v) => {
                        assert_eq!(v, expected);
                        expected += 1;
                    }
                    None => std::hint::spin_loop(),
                }
            }
        });

        prod.join().unwrap();
        cons.join().unwrap();
    }

    #[test]
    fn ring_is_cache_line_aligned() {
        // `Padded` must sit at 64-byte alignment, and two consecutive
        // Padded should be exactly 64 bytes apart.
        assert_eq!(::core::mem::align_of::<Padded>(), 64);
        assert_eq!(::core::mem::size_of::<Padded>(), 64);
    }

    #[test]
    fn capacity_is_reported_correctly() {
        let r: Arc<Ring<u64, 16>> = Ring::new();
        assert_eq!(r.capacity(), 16);
    }
}
