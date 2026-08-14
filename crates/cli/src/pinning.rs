//! CPU-affinity pinning. Single entry point: pin the *current* thread
//! to a single CPU core, identified by zero-based logical index.
//!
//! Platform matrix:
//! * **Linux** — `sched_setaffinity(0, sizeof(cpu_set_t), &set)` via
//!   `libc`. Hard guarantee from the kernel scheduler.
//! * **macOS** — `thread_policy_set` with `THREAD_AFFINITY_POLICY` is
//!   a *hint*, not a guarantee. We don't ship that pathway in 1c —
//!   the production target is Linux. macOS returns a no-op + warning
//!   so that `cargo run` doesn't fail on the dev laptop.
//! * **Other** — best-effort no-op.
//!
//! No external crate. `libc` is already a transitive workspace dep
//! through `core-io`.

use std::io;

/// Why pinning failed. Distinct from `io::Error` so callers can
/// downgrade soft failures (unsupported OS) without paying for a
/// boxed error.
#[derive(Debug)]
pub enum PinError {
    /// The OS rejected the affinity call (EPERM, EINVAL, ...).
    Syscall(io::Error),
    /// The platform has no supported pinning primitive — caller may
    /// treat this as a warning instead of an error.
    Unsupported,
}

impl ::core::fmt::Display for PinError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        match self {
            PinError::Syscall(e) => write!(f, "sched_setaffinity: {e}"),
            PinError::Unsupported => write!(f, "thread pinning unsupported on this platform"),
        }
    }
}

impl std::error::Error for PinError {}

/// Pin the *current* thread to logical CPU `core_id`. `core_id` is
/// zero-based and must be `< CPU_SETSIZE` (1024 on glibc, more than
/// enough for any current hardware).
///
/// On Linux this is a kernel-enforced binding. On other platforms it
/// returns [`PinError::Unsupported`] so the caller can log + continue.
#[inline]
pub fn pin_current_thread_to_core(core_id: usize) -> Result<(), PinError> {
    pin_impl(core_id)
}

// -----------------------------------------------------------------
// Linux: real implementation via sched_setaffinity
// -----------------------------------------------------------------

#[cfg(target_os = "linux")]
fn pin_impl(core_id: usize) -> Result<(), PinError> {
    // SAFETY: zeroing a POD `cpu_set_t` is always safe; the struct is
    // declared in libc as `#[repr(C)]` with a fixed-size bitmask
    // field. `mem::zeroed` produces a valid all-CPUs-cleared mask.
    let mut set: libc::cpu_set_t = unsafe { ::core::mem::zeroed() };
    // SAFETY: `CPU_SET` is documented to operate on a properly
    // zero-initialised `cpu_set_t`. `core_id` is checked against
    // `CPU_SETSIZE` inside libc's macro emulation; we just forward.
    unsafe {
        libc::CPU_SET(core_id, &mut set);
    }
    // SAFETY: passing `0` for `pid` targets the *current* thread on
    // Linux (the manpage explicitly allows this). The `set` and its
    // size are valid for the call.
    let rc = unsafe {
        libc::sched_setaffinity(
            0,
            ::core::mem::size_of::<libc::cpu_set_t>(),
            &set as *const _,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(PinError::Syscall(io::Error::last_os_error()))
    }
}

// -----------------------------------------------------------------
// macOS / other: no-op fallback
// -----------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
fn pin_impl(_core_id: usize) -> Result<(), PinError> {
    Err(PinError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn pin_current_thread_to_core_0_succeeds() {
        // Core 0 always exists on any machine that can run this test.
        match pin_current_thread_to_core(0) {
            Ok(()) => {}
            Err(PinError::Syscall(e)) => panic!("unexpected pin failure: {e}"),
            Err(PinError::Unsupported) => panic!("Linux must support pinning"),
        }
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn pin_returns_unsupported_off_linux() {
        match pin_current_thread_to_core(0) {
            Err(PinError::Unsupported) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    // NB: we don't unit-test out-of-range core ids — libc's
    // `CPU_SET` macro emulation asserts internally and aborts under
    // `panic = "abort"`. The kernel-level rejection (EINVAL on an
    // empty mask) is covered by integration tests against a real
    // pinned thread.
}
