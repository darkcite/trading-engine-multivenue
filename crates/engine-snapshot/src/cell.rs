// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Single-writer seqlock cell (Phase 8a `tui::SnapshotCell`,
//! generalized over its POD for RG6).
//!
//! Protocol: `seq` even = slot stable, odd = writer mid-copy. The
//! writer brackets its copy with an Acquire RMW (odd) and a Release
//! store (even); readers copy the slot and revalidate the version,
//! discarding any copy that overlapped a write.
//!
//! Chosen over `std::sync::Mutex` in Phase 8a: on Darwin, std's
//! `Mutex` falls back to the pthread implementation, which lazily
//! heap-allocates its 64-byte `pthread_mutex_t` on first lock — a
//! hidden allocation on the engine thread that broke the zero-alloc
//! gate on macOS. The seqlock has no OS object, no poisoning, and
//! identical behaviour on every platform.

use std::cell::UnsafeCell;
use std::sync::atomic::{fence, AtomicU64, Ordering};

/// Cross-thread snapshot pump. The engine calls `publish`; readers
/// call `read` / `read_into`. Single-writer seqlock — zero allocation,
/// no OS lock object, on every platform.
///
/// **Single-writer contract:** exactly one thread (the engine
/// thread) may call `publish`. Enforced by `debug_assert!` only —
/// a second publisher is a design error upstream.
///
/// `T` must be a plain `Copy` POD (no pointers, no invalid bit
/// patterns): a torn copy is materialized transiently by a reader
/// that raced the writer and is discarded by the version re-check.
#[repr(C, align(64))]
pub struct SnapshotCell<T: Copy> {
    /// Version counter. Even = stable; odd = write in flight. Sits on
    /// its own cache line ahead of `data` so reader version-polling
    /// does not false-share with the payload copy.
    seq: AtomicU64,
    _pad: [u8; 56],
    data: UnsafeCell<T>,
}

// SAFETY: all cross-thread access to `data` is mediated by the
// seqlock protocol on `seq`. The single writer (contract above)
// brackets its non-atomic copy with an odd version (Acquire RMW —
// the copy cannot be reordered before the odd version is visible)
// and an even version (Release store — the copy is visible before
// the new version). Readers copy `data` and then revalidate `seq`
// behind an Acquire fence, so any copy that overlapped a write is
// discarded and retried; a torn copy of the `Copy` POD `T` (no
// invalid bit patterns, no pointers — the type contract) is
// materialized at most transiently and never returned.
unsafe impl<T: Copy + Send> Sync for SnapshotCell<T> {}

impl<T: Copy> SnapshotCell<T> {
    /// Build a cell holding `init`.
    pub const fn new(init: T) -> Self {
        Self {
            seq: AtomicU64::new(0),
            _pad: [0; 56],
            data: UnsafeCell::new(init),
        }
    }

    /// Publish the latest snapshot. Wait-free for the (single)
    /// writer: one Acquire RMW, one POD copy, one Release store.
    /// Never allocates, never blocks, cannot poison. Takes the value
    /// by reference so a large POD is copied exactly once (into the
    /// slot), not twice through the call.
    pub fn publish(&self, s: &T) {
        let seq0 = self.seq.load(Ordering::Relaxed);
        debug_assert_eq!(
            seq0 & 1,
            0,
            "SnapshotCell: odd version on publish entry — second concurrent publisher"
        );
        // Enter the write section. The Acquire RMW forbids the
        // payload copy below from being reordered before the odd
        // version becomes visible (pairs with the readers'
        // Acquire fence + revalidation).
        let _prev = self.seq.swap(seq0.wrapping_add(1), Ordering::Acquire);
        debug_assert_eq!(_prev, seq0, "SnapshotCell: version moved under the writer");
        // SAFETY: single-writer contract — no concurrent writes to
        // `data` exist. Concurrent readers may race this non-atomic
        // copy, but the version bracket makes them discard any copy
        // that overlapped it (see `Sync` impl note). The pointer is
        // valid, aligned, and owned by `self`.
        unsafe { *self.data.get() = *s };
        // Exit the write section: publish the copy to any reader
        // that observes the new even version.
        self.seq.store(seq0.wrapping_add(2), Ordering::Release);
    }

    /// Read the most recent snapshot by value. Lock-free: retries
    /// only while a write is in flight (the writer publishes once per
    /// second and copies ≤ 24 KB — retries are vanishingly rare).
    /// Never allocates. For a large `T` prefer [`Self::read_into`].
    pub fn read(&self) -> T {
        loop {
            let seq1 = self.seq.load(Ordering::Acquire);
            if seq1 & 1 != 0 {
                // Writer mid-copy — spin briefly.
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: this volatile copy may race the writer's
            // non-atomic copy; the revalidation below discards any
            // result that overlapped a write, and a transient torn
            // copy of the `Copy` POD `T` is harmless (type contract:
            // no invalid bit patterns, no pointers). Volatile forces
            // the copy to complete before the version re-check.
            // Pointer valid + aligned by construction.
            let val = unsafe { core::ptr::read_volatile(self.data.get()) };
            // Pairs with the writer's bracket: an Acquire fence keeps
            // the loads above from being reordered after the version
            // re-load; if the copy overlapped a write, the version
            // has moved (odd or advanced) and we retry.
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == seq1 {
                return val;
            }
        }
    }

    /// Read the most recent snapshot into `out` (same protocol as
    /// [`Self::read`]; the caller owns the destination — the shape
    /// for the ≈ 24 KB engine snapshot on the server thread).
    pub fn read_into(&self, out: &mut T) {
        loop {
            let seq1 = self.seq.load(Ordering::Acquire);
            if seq1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: see `read` — same racing volatile copy, same
            // discard-on-revalidation law; `out` is a valid, aligned,
            // exclusively borrowed destination.
            unsafe { *out = core::ptr::read_volatile(self.data.get()) };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == seq1 {
                return;
            }
        }
    }

    /// Publishes so far (the even version halved). Cold; tests and
    /// the boot log.
    #[inline]
    pub fn publishes(&self) -> u64 {
        self.seq.load(Ordering::Acquire) >> 1
    }
}

impl<T: Copy + Default> Default for SnapshotCell<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
    #[repr(C, align(64))]
    struct Quad {
        a: u64,
        b: u64,
        c: u64,
        d: u64,
        pad: [u64; 12],
    }

    #[test]
    fn cell_is_cache_aligned_with_payload_on_its_own_line() {
        assert_eq!(core::mem::align_of::<SnapshotCell<Quad>>(), 64);
        assert_eq!(core::mem::offset_of!(SnapshotCell<Quad>, data), 64);
    }

    #[test]
    fn round_trips_and_overwrites() {
        let cell = SnapshotCell::new(Quad::default());
        let mut q = Quad {
            a: 7,
            ..Default::default()
        };
        cell.publish(&q);
        assert_eq!(cell.read().a, 7);
        q.a = 9;
        cell.publish(&q);
        let mut out = Quad::default();
        cell.read_into(&mut out);
        assert_eq!(out.a, 9);
        assert_eq!(cell.publishes(), 2);
    }

    /// Failure-mode coverage for the seqlock: hammer `publish` from
    /// one thread while another reads continuously; every observed
    /// snapshot must be internally consistent (all mirrored fields
    /// equal), i.e. torn reads are never returned.
    #[test]
    fn concurrent_reads_never_tear() {
        const PUBLISHES: u64 = 100_000;
        let cell = SnapshotCell::new(Quad::default());
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let mut q = Quad::default();
                for i in 0..PUBLISHES {
                    q.a = i;
                    q.b = i;
                    q.c = i;
                    q.d = i;
                    q.pad = [i; 12];
                    cell.publish(&q);
                }
            });
            let mut last = 0u64;
            let mut out = Quad::default();
            while !writer.is_finished() {
                cell.read_into(&mut out);
                assert_eq!(out.a, out.b, "torn read");
                assert_eq!(out.a, out.c, "torn read");
                assert_eq!(out.a, out.d, "torn read");
                assert_eq!(out.pad, [out.a; 12], "torn read");
                assert!(out.a >= last, "snapshot went backwards");
                last = out.a;
            }
            writer.join().expect("writer thread panicked");
        });
        assert_eq!(cell.read().a, PUBLISHES - 1);
    }
}
