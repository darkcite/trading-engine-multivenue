// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `Mailbox` — one slot, handed between two threads by ownership
//!
//! The ring's single-slot sibling, for a message too large to queue and
//! too cold to copy twice: HAR H3.7's long-tenor state (~201 KiB a series),
//! filled on the engine thread and rendered and written by a state-writer
//! thread. One slot, two states:
//!
//! * **FREE** — the producer owns the slot. [`MailboxTx::try_fill`] lends
//!   it mutably through a [`Filling`] guard, and [`Filling::commit`] hands
//!   it over. A guard dropped without `commit` hands nothing: the slot stays
//!   FREE, holding whatever was written.
//! * **FULL** — the consumer owns the slot. [`MailboxRx::try_take`] lends
//!   it through a [`Taken`] guard, which frees it when dropped — or
//!   [`Taken::keep`] leaves it FULL, to be taken again (a write that failed
//!   and will be retried).
//!
//! Neither side ever waits: a `try_fill` while FULL and a `try_take` while
//! FREE are `None`. The mailbox copies nothing — the producer writes the
//! slot in place and the consumer reads it there — and allocates nothing
//! after [`Mailbox::new`], whose slot the caller boxed at boot (a message
//! this size has no business on a stack).
//!
//! ## Memory ordering
//!
//! Each side loads the state with **Acquire** before it touches the slot,
//! and stores the next state with **Release** once it is done with it: the
//! producer's writes happen-before the consumer's reads (commit → take),
//! and the consumer's reads happen-before the producer's next writes
//! (free → fill). In either state exactly one side may touch the slot, and
//! only that side moves the state on, so the two never alias. The slot is
//! reached through a raw pointer taken once from its box, so no reference
//! into it outlives a guard (sound under Stacked and Tree Borrows).
//!
//! ## SPSC by construction
//!
//! A mailbox splits exactly once ([`Mailbox::split`]); both handles are
//! `Send + !Sync`, every method that touches the slot takes `&mut self`,
//! and a guard borrows its handle exclusively.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

/// The producer owns the slot.
const FREE: u8 = 0;
/// The consumer owns the slot.
const FULL: u8 = 1;

/// One slot, handed between exactly two threads (module doc).
///
/// INVARIANT in `T`: no lifetime inside a `T` can be shortened through a
/// handle, so a `MailboxTx<&'static str>` can never be filled with a borrow
/// that dies before the consumer reads it (a covariant `T` allowed exactly
/// that — found in the H3.7 review):
///
/// ```compile_fail
/// fn shorten<'a>(tx: core_ring::MailboxTx<&'static str>) -> core_ring::MailboxTx<&'a str> {
///     tx
/// }
/// ```
///
/// while the identity compiles:
///
/// ```
/// fn same<'a>(tx: core_ring::MailboxTx<&'a str>) -> core_ring::MailboxTx<&'a str> {
///     tx
/// }
/// ```
pub struct Mailbox<T> {
    /// [`FREE`] or [`FULL`]; each state is moved on only by its owner.
    state: AtomicU8,
    /// Set by the one successful [`Mailbox::split`] (boot only).
    split: AtomicBool,
    /// The slot, from `Box::into_raw` — freed once, in `Drop`.
    slot: NonNull<T>,
    /// The mailbox owns a `T`, and is invariant in it (the type doc).
    _owns: PhantomData<UnsafeCell<T>>,
}

// SAFETY: a `Mailbox` moves between threads only inside the `Arc` its two
// handles share, and owns a `T: Send`; the state and split flags are atomics.
unsafe impl<T: Send> Send for Mailbox<T> {}
// SAFETY: `&Mailbox` alone touches nothing but atomics. Only the split handles
// reach the slot, there is exactly one of each (`split` succeeds once), each is
// `!Sync`, and each touches the slot only in the state it owns — observed with
// Acquire — and hands it on with Release (module doc, "Memory ordering").
unsafe impl<T: Send> Sync for Mailbox<T> {}

impl<T: Send> Mailbox<T> {
    /// A FREE mailbox around `slot`, boxed by the caller (at boot).
    pub fn new(slot: Box<T>) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(FREE),
            split: AtomicBool::new(false),
            slot: NonNull::from(Box::leak(slot)),
            _owns: PhantomData,
        })
    }

    /// Split this mailbox into its one producer and its one consumer. Each
    /// is `Send` but neither is `Sync`.
    ///
    /// # Panics
    ///
    /// If this mailbox was split before: a second pair of handles would be a
    /// second producer — a data race. Boot-time, fail-fast (release aborts).
    pub fn split(self: Arc<Self>) -> (MailboxTx<T>, MailboxRx<T>) {
        let first = !self.split.swap(true, Ordering::AcqRel);
        assert!(first, "core-ring: a mailbox is split exactly once (SPSC)");
        let tx = MailboxTx {
            mb: Arc::clone(&self),
            _not_sync: PhantomData,
        };
        let rx = MailboxRx {
            mb: self,
            _not_sync: PhantomData,
        };
        (tx, rx)
    }
}

impl<T> Drop for Mailbox<T> {
    fn drop(&mut self) {
        // SAFETY: `slot` came from `Box::leak` in `new`, is freed only here,
        // and the last `Arc` is going: no handle, no guard, no reference.
        drop(unsafe { Box::from_raw(self.slot.as_ptr()) });
    }
}

// ---------------------------------------------------------------
// The producer
// ---------------------------------------------------------------

/// The one producer handle. `!Sync` by construction.
pub struct MailboxTx<T> {
    mb: Arc<Mailbox<T>>,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

impl<T> MailboxTx<T> {
    /// The slot, lent mutably while FREE; `None` while the consumer holds
    /// it (FULL). Never waits, never allocates.
    #[inline]
    #[must_use = "a `Filling` hands nothing until it is committed"]
    pub fn try_fill(&mut self) -> Option<Filling<'_, T>> {
        // Pairs with the consumer's Release in `Taken::drop`: its reads of
        // the slot happen-before the writes this guard allows.
        if self.mb.state.load(Ordering::Acquire) != FREE {
            return None;
        }
        Some(Filling { tx: self })
    }

    /// True while the consumer holds the slot (a snapshot, may race).
    #[inline]
    pub fn is_full(&self) -> bool {
        self.mb.state.load(Ordering::Relaxed) == FULL
    }
}

/// The FREE slot, lent mutably by [`MailboxTx::try_fill`]. Hands the slot
/// to the consumer on [`Filling::commit`]; dropped without it, hands
/// nothing.
pub struct Filling<'a, T> {
    /// Exclusive: no second guard while this one lives.
    tx: &'a mut MailboxTx<T>,
}

impl<T> Filling<'_, T> {
    /// Hand the slot to the consumer (FULL). Its contents are the message.
    #[inline]
    pub fn commit(self) {
        // Publishes every write made through this guard.
        self.tx.mb.state.store(FULL, Ordering::Release);
    }
}

impl<T> Deref for Filling<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: the state is FREE (observed with Acquire in `try_fill`) and
        // only this handle — borrowed exclusively by the guard — moves it on,
        // so the consumer does not touch the slot while the guard lives; the
        // pointer is the live box `Mailbox` owns, and the `&T` cannot outlive
        // `&self`.
        unsafe { &*self.tx.mb.slot.as_ptr() }
    }
}

impl<T> DerefMut for Filling<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as `deref`; `&mut self` makes the `&mut T` the only
        // reference into the slot for its lifetime.
        unsafe { &mut *self.tx.mb.slot.as_ptr() }
    }
}

// ---------------------------------------------------------------
// The consumer
// ---------------------------------------------------------------

/// The one consumer handle. `!Sync` by construction.
pub struct MailboxRx<T> {
    mb: Arc<Mailbox<T>>,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

impl<T> MailboxRx<T> {
    /// The slot, lent while FULL; `None` while the producer holds it
    /// (FREE). Never waits, never allocates.
    #[inline]
    #[must_use = "dropping the guard hands the slot back to the producer"]
    pub fn try_take(&mut self) -> Option<Taken<'_, T>> {
        // Pairs with `Filling::commit`'s Release: the producer's writes are
        // visible before the slot is lent.
        if self.mb.state.load(Ordering::Acquire) != FULL {
            return None;
        }
        Some(Taken { rx: self })
    }

    /// True while the consumer holds the slot (a snapshot, may race).
    #[inline]
    pub fn is_full(&self) -> bool {
        self.mb.state.load(Ordering::Relaxed) == FULL
    }
}

/// The FULL slot, lent by [`MailboxRx::try_take`]. Hands the slot back to
/// the producer when dropped, unless [`Taken::keep`] holds it.
pub struct Taken<'a, T> {
    /// Exclusive: no second guard while this one lives.
    rx: &'a mut MailboxRx<T>,
}

impl<T> Taken<'_, T> {
    /// Keep the slot FULL: the next [`MailboxRx::try_take`] lends the same
    /// message again, and the producer stays refused until then.
    #[inline]
    pub fn keep(self) {
        ::core::mem::forget(self);
    }
}

impl<T> Deref for Taken<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: the state is FULL (observed with Acquire in `try_take`) and
        // only this handle — borrowed exclusively by the guard — moves it on,
        // so the producer does not touch the slot while the guard lives; the
        // pointer is the live box `Mailbox` owns, and the `&T` cannot outlive
        // `&self`.
        unsafe { &*self.rx.mb.slot.as_ptr() }
    }
}

impl<T> Drop for Taken<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // Every read made through this guard happens-before the producer's
        // Acquire load that observes FREE.
        self.rx.mb.state.store(FREE, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_committed_fill_is_taken_once_then_the_slot_is_free_again() {
        let (mut tx, mut rx) = Mailbox::new(Box::new([0u64; 8])).split();
        assert!(rx.try_take().is_none(), "FREE: nothing to take");
        {
            let mut f = tx.try_fill().expect("FREE: the producer's");
            f[0] = 7;
            f[7] = 9;
            f.commit();
        }
        assert!(tx.is_full() && rx.is_full());
        assert!(tx.try_fill().is_none(), "FULL: the consumer's");
        {
            let t = rx.try_take().expect("FULL");
            assert_eq!((t[0], t[7]), (7, 9));
        }
        assert!(!tx.is_full());
        assert!(rx.try_take().is_none(), "taken once");
        assert_eq!(tx.try_fill().expect("FREE again")[0], 7, "the slot keeps its bytes");
    }

    #[test]
    fn a_fill_dropped_without_commit_hands_nothing() {
        let (mut tx, mut rx) = Mailbox::new(Box::new(0u32)).split();
        *tx.try_fill().expect("FREE") = 5;
        assert!(rx.try_take().is_none(), "no commit, no message");
        assert!(!tx.is_full());
        let mut f = tx.try_fill().expect("still FREE");
        assert_eq!(*f, 5);
        *f = 6;
        f.commit();
        assert_eq!(*rx.try_take().expect("committed"), 6);
    }

    #[test]
    fn a_kept_take_is_lent_again_and_the_producer_waits() {
        let (mut tx, mut rx) = Mailbox::new(Box::new(1u8)).split();
        let mut f = tx.try_fill().expect("FREE");
        *f = 2;
        f.commit();
        rx.try_take().expect("FULL").keep();
        assert!(tx.try_fill().is_none(), "kept: still the consumer's");
        assert_eq!(*rx.try_take().expect("lent again"), 2);
        assert!(tx.try_fill().is_some(), "dropped: the producer's");
    }

    #[test]
    #[should_panic(expected = "split exactly once")]
    fn a_second_split_is_refused() {
        let mb = Mailbox::new(Box::new(0u8));
        let again = Arc::clone(&mb);
        let _pair = mb.split();
        let _second = again.split();
    }

    /// Two threads hand one slot back and forth: every message the consumer
    /// takes is whole (all words equal) and newer than the last, and every
    /// one the producer committed arrives.
    #[test]
    fn two_threads_never_see_a_torn_slot() {
        const MESSAGES: u64 = if cfg!(miri) { 200 } else { 50_000 };
        let (mut tx, mut rx) = Mailbox::new(Box::new([0u64; 32])).split();
        let producer = std::thread::spawn(move || {
            let mut k = 1u64;
            while k <= MESSAGES {
                if let Some(mut f) = tx.try_fill() {
                    let mut i = 0usize;
                    while i < f.len() {
                        f[i] = k;
                        i += 1;
                    }
                    f.commit();
                    k += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        let mut last = 0u64;
        while last < MESSAGES {
            if let Some(t) = rx.try_take() {
                let k = t[0];
                let mut i = 1usize;
                while i < t.len() {
                    assert_eq!(t[i], k, "a torn slot");
                    i += 1;
                }
                assert_eq!(k, last + 1, "every committed message arrives, in order");
                last = k;
            } else {
                std::hint::spin_loop();
            }
        }
        producer.join().expect("the producer panicked");
    }
}
