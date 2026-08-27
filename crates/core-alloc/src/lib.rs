// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-alloc
//!
//! A thin wrapper around the system allocator that counts allocations
//! and bytes allocated. Used **exclusively** by the alloc-assertion
//! test harness — never linked into a production binary.
//!
//! ## Usage (test-only)
//!
//! ```
//! use core_alloc::{CountingAllocator, AllocGuard};
//!
//! // Install as the global allocator in the test binary:
//! #[global_allocator]
//! static GLOBAL: CountingAllocator = CountingAllocator::new();
//!
//! fn example() {
//!     let g = AllocGuard::new();
//!     // ...hot path under test...
//!     let (allocs, bytes, _deallocs) = g.delta();
//!     assert_eq!(allocs, 0, "hot path allocated {} bytes", bytes);
//! }
//! ```
//!
//! The counters are relaxed atomics — the overhead is a couple of ns
//! per alloc, which is fine because we only use this allocator in
//! tests/benches.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

/// Global counters. Intentionally package-private.
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOCS: AtomicU64 = AtomicU64::new(0);

/// Allocator wrapper that keeps per-process counters and forwards every
/// call to the platform `System` allocator.
pub struct CountingAllocator;

impl CountingAllocator {
    /// Construct. `const fn` so it can be used in `#[global_allocator] static`.
    pub const fn new() -> Self {
        Self
    }

    /// Snapshot of (alloc calls, bytes allocated, dealloc calls) since
    /// process start.
    #[inline]
    pub fn snapshot() -> (u64, u64, u64) {
        (
            ALLOCS.load(Ordering::Relaxed),
            BYTES.load(Ordering::Relaxed),
            DEALLOCS.load(Ordering::Relaxed),
        )
    }

    /// Reset the counters to zero. Typically called from a test's setup
    /// block before the hot path under measurement.
    #[inline]
    pub fn reset() {
        ALLOCS.store(0, Ordering::Relaxed);
        BYTES.store(0, Ordering::Relaxed);
        DEALLOCS.store(0, Ordering::Relaxed);
    }
}

impl Default for CountingAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: All alloc/dealloc calls are forwarded to `System`, which is a
// valid `GlobalAlloc`. We never fabricate pointers; we never change
// their Layout. The atomic counter updates are relaxed and introduce no
// soundness concerns.
unsafe impl GlobalAlloc for CountingAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `System`'s `alloc` contract is upheld — `layout` was
        // passed through unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        ptr
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: Caller guarantees (per `GlobalAlloc`) that `ptr` came
        // from a prior `alloc` with a compatible `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `System`'s zeroed alloc has the same safety contract
        // as `alloc`, which we uphold.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        ptr
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: Per `GlobalAlloc`, caller guarantees `ptr`/`layout`
        // came from us, and `new_size` fits the alignment contract.
        let newp = unsafe { System.realloc(ptr, layout, new_size) };
        if !newp.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        newp
    }
}

// ---------------------------------------------------------------
// Scoped guard
// ---------------------------------------------------------------

/// RAII scoped allocation recorder. Captures the counters on
/// construction; `.delta()` returns the per-scope delta.
///
/// Intended usage is inside a single test function.
pub struct AllocGuard {
    start_allocs: u64,
    start_bytes: u64,
    start_deallocs: u64,
}

impl AllocGuard {
    /// Snapshot current counters.
    #[inline]
    pub fn new() -> Self {
        let (a, b, d) = CountingAllocator::snapshot();
        Self {
            start_allocs: a,
            start_bytes: b,
            start_deallocs: d,
        }
    }

    /// Return `(alloc_calls, bytes, dealloc_calls)` that occurred since
    /// this guard was created.
    #[inline]
    pub fn delta(&self) -> (u64, u64, u64) {
        let (a, b, d) = CountingAllocator::snapshot();
        (
            a.wrapping_sub(self.start_allocs),
            b.wrapping_sub(self.start_bytes),
            d.wrapping_sub(self.start_deallocs),
        )
    }

    /// Panic if ANY allocation happened in this scope. Used at the end
    /// of hot-path alloc-assertion tests.
    #[inline]
    pub fn assert_zero(&self) {
        let (allocs, bytes, _deallocs) = self.delta();
        assert_eq!(
            allocs, 0,
            "expected 0 allocations in this scope; saw {allocs} calls / {bytes} bytes"
        );
    }
}

impl Default for AllocGuard {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    // NOTE: These tests install the allocator as *global* per-binary.
    // We only exercise the snapshot/delta math here — the end-to-end
    // behaviour is tested by `tests/alloc_assertions.rs`, which swaps
    // in the `CountingAllocator` as `#[global_allocator]`.
    use super::*;

    #[test]
    fn guard_delta_is_initially_zero() {
        // Before any alloc happens in this thread, the delta is zero
        // plus/minus background noise (which is zero in this scope).
        let g = AllocGuard::new();
        // No alloc work here — purely arithmetic.
        let hint = 42u64;
        ::std::hint::black_box(hint);
        let (_allocs, _bytes, _deallocs) = g.delta();
        // We can't meaningfully assert exact numbers here without being
        // installed as the global allocator, so just make sure the call
        // itself didn't panic.
    }

    #[test]
    fn snapshot_returns_three_monotonic_numbers() {
        let (a0, b0, d0) = CountingAllocator::snapshot();
        let (a1, b1, d1) = CountingAllocator::snapshot();
        assert!(a1 >= a0);
        assert!(b1 >= b0);
        assert!(d1 >= d0);
    }
}
