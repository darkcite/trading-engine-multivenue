// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-time
//!
//! Monotonic nanosecond clock. A thin wrapper over the platform syscall
//! that returns a `u64` — the type every POD struct uses as a timestamp.
//!
//! ## Why not `std::time::Instant`?
//!
//! `Instant` is correct but NOT `#[repr(C)]` and NOT a plain `u64`. Its
//! internal representation is platform-private. Since our ring slots
//! (`Tick`, `Signal`, `Fill`, `Order`) live in `#[repr(C, align(64))]`
//! structs, we want a POD timestamp type we fully control.
//!
//! ## Rules
//!
//! * `now_ns()` is inlined aggressively and allocates nothing.
//! * On Unix we call `clock_gettime(CLOCK_MONOTONIC_RAW)` directly to
//!   avoid NTP adjustment. `CLOCK_MONOTONIC_RAW` is available on both
//!   macOS (since 10.12) and Linux.
//! * Fallback (non-unix, tests) uses `std::time::Instant`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

/// Monotonic nanoseconds since some unspecified starting point. Values
/// are comparable across calls on the same process/run.
pub type NsTs = u64;

// ---------------------------------------------------------------
// Unix fast path.
// ---------------------------------------------------------------

/// Read the monotonic nanosecond clock. Zero allocations, inline-always.
///
/// On Linux and macOS this calls `clock_gettime(CLOCK_MONOTONIC_RAW)`
/// directly. On other unixes it falls back to `CLOCK_MONOTONIC`. On
/// non-unix targets (test-only scaffolding) it wraps `Instant`.
#[cfg(unix)]
#[inline(always)]
pub fn now_ns() -> NsTs {
    let mut ts = ::core::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `libc::clock_gettime` with a valid `timespec` pointer is
    // safe. `ts` is stack-allocated and fully owned here. We ignore the
    // return code and trust the monotonic clock to always be available
    // on macOS / Linux — if it ever isn't, the whole process is wedged.
    // `assume_init` is valid because a successful `clock_gettime` fully
    // writes both `tv_sec` and `tv_nsec`.
    unsafe {
        libc::clock_gettime(CLOCK_ID, ts.as_mut_ptr());
        let ts = ts.assume_init();
        (ts.tv_sec as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(ts.tv_nsec as u64)
    }
}

#[cfg(all(unix, target_os = "linux"))]
const CLOCK_ID: libc::clockid_t = libc::CLOCK_MONOTONIC_RAW;

#[cfg(all(unix, target_os = "macos"))]
const CLOCK_ID: libc::clockid_t = libc::CLOCK_MONOTONIC_RAW;

// BSDs / other unixes: plain CLOCK_MONOTONIC.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
const CLOCK_ID: libc::clockid_t = libc::CLOCK_MONOTONIC;

// ---------------------------------------------------------------
// Non-unix fallback (tests only).
// ---------------------------------------------------------------

/// Non-unix fallback. Tests and exotic targets only.
#[cfg(not(unix))]
pub fn now_ns() -> NsTs {
    // This branch is tested-only scaffolding; the production target is
    // unix. Using `Instant` here allocates nothing (it's a value type).
    use std::sync::OnceLock;
    use std::time::Instant;
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = BASE.get_or_init(Instant::now);
    base.elapsed().as_nanos() as u64
}

// ---------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------

/// Difference between two timestamps in nanoseconds. Saturates at zero
/// when `later < earlier` (time went backwards — should not happen with
/// `CLOCK_MONOTONIC_RAW`, but we guard against bugs rather than trust).
#[inline(always)]
pub const fn ns_since(earlier: NsTs, later: NsTs) -> u64 {
    later.saturating_sub(earlier)
}

// ---------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ns_is_monotonic_across_two_calls() {
        let a = now_ns();
        let b = now_ns();
        assert!(b >= a, "a={a} b={b}");
    }

    #[test]
    fn ns_since_saturates_on_reverse() {
        assert_eq!(ns_since(100, 50), 0);
    }

    #[test]
    fn ns_since_computes_simple_delta() {
        assert_eq!(ns_since(1_000, 1_500), 500);
    }

    #[test]
    fn now_ns_advances_over_a_busy_loop() {
        // `CLOCK_MONOTONIC` has nanosecond resolution on Linux, but
        // actual tick rate is platform-dependent — on some ARM cores
        // (e.g. the Graviton-family used in CI) the backing counter
        // advances in ~40 ns steps, so a fixed 1024-iter spin can
        // race the counter. Spin until the clock actually advances,
        // bounded so we never wedge the suite.
        let a = now_ns();
        let mut spin = 0u64;
        let mut b = a;
        for i in 0..1_000_000u64 {
            spin = spin.wrapping_add(i);
            b = now_ns();
            if b > a {
                break;
            }
        }
        // Black-box the spin result so LLVM doesn't elide it.
        ::std::hint::black_box(spin);
        assert!(b > a, "a={a} b={b}");
    }
}
