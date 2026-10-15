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
//! // Measure inside a `#[test]` (a thread libtest spawns), never on the
//! // main thread: the main thread is the runner's and is not counted, so
//! // `AllocGuard::new` refuses it (A-S7-101).
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
//!
//! ## The test runner's thread is not counted (A-S7-101)
//!
//! The counters are process-wide, so every thread of the test process
//! lands in them — including the libtest runner, which is the process's
//! MAIN thread and never runs code under test (libtest spawns a thread
//! for every test, `--test-threads=1` and nextest's one-test-per-process
//! included). Right after spawning a test's thread the runner does its
//! own bookkeeping on the heap: the running-test map (140 B), the timeout
//! queue (608 B), then the lazily-built state of its first blocking
//! `recv_timeout` — its own `Thread` handle (56 B), the waiter context
//! (48 B), the channel waker's boxed pthread mutex (64 B) and waiter
//! list (96 B): 6 allocations / 1 012 B on aarch64-apple-darwin, Rust
//! 1.88 (5 / 872 B when the window opens after the map insert).
//! Unloaded, that is done long before the test opens its window; under
//! CPU load the runner is preempted right after `pthread_create`, the
//! test thread runs first, and the runner's allocations land inside the
//! window: a red on code that allocates nothing (A-S7-14, A-S7-40,
//! A-S7-101).
//!
//! So the allocator does not count the main thread. Every other thread
//! — the test's own, and any thread the code under test spawns — is
//! counted exactly as before. [`AllocGuard::new`] refuses to open a
//! window on the main thread, where it would see nothing.

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
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// Global counters. Intentionally package-private.
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOCS: AtomicU64 = AtomicU64::new(0);

#[cfg(any(target_vendor = "apple", target_os = "linux"))]
thread_local! {
    /// What this thread is, classified on its first allocation:
    /// 0 = not yet asked, 1 = the main thread (the test runner),
    /// 2 = any other thread. `const`-initialised and drop-free: on these
    /// targets (native thread-locals) the access never allocates through
    /// this allocator and never registers a destructor — safe inside it.
    static ROLE: Cell<u8> = const { Cell::new(0) };
}

/// True on the process's main thread — under libtest (and nextest, which
/// runs libtest binaries) the test RUNNER, which never runs a test's
/// body. Its allocations are the harness's, not the code under test's.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
#[inline]
fn on_runner_thread() -> bool {
    ROLE.with(|role| match role.get() {
        0 => classify(role),
        r => r == 1,
    })
}

/// The once-per-thread classification, kept out of the four inlined
/// allocator entry points.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
#[cold]
#[inline(never)]
fn classify(role: &Cell<u8>) -> bool {
    let main = is_main_thread();
    role.set(if main { 1 } else { 2 });
    main
}

/// Elsewhere the runner cannot be told apart, and a thread-local may not be
/// native (std's fallback allocates on first access — re-entering this
/// allocator): no thread-local at all, every thread counted (the strict,
/// pre-A-S7-101 behaviour — it can flake red, never pass wrongly).
#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
#[inline]
fn on_runner_thread() -> bool {
    false
}

#[cfg(target_vendor = "apple")]
#[inline]
fn is_main_thread() -> bool {
    unsafe extern "C" {
        /// libSystem: 1 on the process's main thread, 0 on any other.
        fn pthread_main_np() -> std::ffi::c_int;
    }
    // SAFETY: `pthread_main_np` takes no arguments, has no
    // preconditions, does not allocate and touches no Rust memory.
    unsafe { pthread_main_np() == 1 }
}

#[cfg(target_os = "linux")]
#[inline]
fn is_main_thread() -> bool {
    unsafe extern "C" {
        /// glibc >= 2.30 / musl >= 1.2.2: the calling thread's kernel id.
        fn gettid() -> i32;
        /// The process id — equal to the main thread's kernel id.
        fn getpid() -> i32;
    }
    // SAFETY: neither call takes arguments or has preconditions; neither
    // allocates nor touches Rust memory.
    unsafe { gettid() == getpid() }
}

/// Allocator wrapper that keeps per-process counters and forwards every
/// call to the platform `System` allocator.
pub struct CountingAllocator;

impl CountingAllocator {
    /// Construct. `const fn` so it can be used in `#[global_allocator] static`.
    pub const fn new() -> Self {
        Self
    }

    /// Snapshot of (alloc calls, bytes allocated, dealloc calls) since
    /// process start, over every thread but the main (test-runner) one.
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
        if !ptr.is_null() && !on_runner_thread() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        ptr
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if !on_runner_thread() {
            DEALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: Caller guarantees (per `GlobalAlloc`) that `ptr` came
        // from a prior `alloc` with a compatible `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `System`'s zeroed alloc has the same safety contract
        // as `alloc`, which we uphold.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() && !on_runner_thread() {
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
        if !newp.is_null() && !on_runner_thread() {
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
    ///
    /// # Panics
    /// On the process's main thread: its allocations are not counted
    /// (it is the test runner's), so a window opened there would pass
    /// whatever it measured. libtest never runs a test there.
    #[inline]
    pub fn new() -> Self {
        assert!(
            !on_runner_thread(),
            "AllocGuard opened on the main thread, which the counting allocator \
             does not count (it is the test runner's) — measure on a test thread"
        );
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

    /// The exclusion is sound only while libtest runs every test off the
    /// main thread. Pin it, and that a thread the test spawns counts too.
    #[test]
    fn tests_and_their_threads_are_not_the_runner_thread() {
        assert!(!on_runner_thread(), "libtest ran a test on the main thread");
        let spawned = std::thread::spawn(on_runner_thread)
            .join()
            .expect("probe thread");
        assert!(!spawned, "a test-spawned thread classified as the runner");
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
