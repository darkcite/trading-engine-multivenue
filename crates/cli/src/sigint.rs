//! Hand-rolled SIGINT handler.
//!
//! Goal: when the user hits `Ctrl+C` (or `kill -INT $pid`), flip a
//! single static [`AtomicBool`] that every ingress run-loop polls at
//! the top of its mio cycle. No external crate (`signal-hook`,
//! `ctrlc`, etc.) — those would either pull a runtime or queue
//! callbacks on a helper thread we don't need.
//!
//! The handler itself is **async-signal-safe**: it does exactly one
//! atomic store. No allocation, no I/O, no locks.
//!
//! Two-stage shutdown: first SIGINT flips the flag; if the user hits
//! it *again* while the engine is still running, we re-raise the
//! default handler so the process dies immediately. This avoids the
//! "stuck on shutdown" papercut.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// Process-wide shutdown flag. Ingress run-loops poll this every
/// iteration; the SIGINT handler is the only writer (apart from
/// tests).
///
/// Use [`shutdown_requested`] to read; the handler does an
/// `Ordering::Release` store so a single `Ordering::Acquire` load
/// elsewhere is enough to observe the change.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Convenience read of [`SHUTDOWN`] with acquire ordering.
#[inline]
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::Acquire)
}

/// Install the SIGINT handler. Idempotent — calling twice in the
/// same process re-registers the same routine. The second SIGINT
/// raises the default handler (SIG_DFL) so the process exits.
///
/// Returns the previous handler config so tests can restore it.
pub fn install_sigint_handler() -> io::Result<()> {
    install_impl()
}

#[cfg(unix)]
fn install_impl() -> io::Result<()> {
    // SAFETY: `sigaction` mutates a kernel-side table for the
    // current process. `sa` is fully populated below. We do not
    // borrow any non-static state from inside the handler.
    unsafe {
        let mut sa: libc::sigaction = ::core::mem::zeroed();
        sa.sa_sigaction = handle_sigint as libc::sighandler_t;
        // SA_RESTART so blocked syscalls (read/write) resume rather
        // than fail with EINTR — keeps the ingress threads simple.
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        let rc = libc::sigaction(libc::SIGINT, &sa as *const _, ::core::ptr::null_mut());
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn install_impl() -> io::Result<()> {
    // Non-Unix targets aren't supported in v1; the engine is
    // POSIX-only. Return an error so the caller can fail-fast.
    Err(io::Error::other("SIGINT install: non-Unix unsupported"))
}

// `extern "C"` async-signal-safe handler. Cf. signal(7): only a tiny
// allowlist of POSIX functions is safe here; atomic operations are
// fine because they're library-level (no syscalls, no `errno`
// touches).
#[cfg(unix)]
extern "C" fn handle_sigint(_sig: libc::c_int) {
    // First Ctrl+C: ask everything to stop politely.
    if !SHUTDOWN.swap(true, Ordering::Release) {
        return;
    }
    // Second Ctrl+C: revert SIGINT to SIG_DFL and re-raise so the
    // process dies. This block is also async-signal-safe (sigaction
    // + raise are both on the allowlist).

    // SAFETY: zeroing the POD struct is fine, and we immediately
    // populate the one field we need (sa_sigaction = SIG_DFL).
    unsafe {
        let mut sa: libc::sigaction = ::core::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa as *const _, ::core::ptr::null_mut());
        libc::raise(libc::SIGINT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-threaded sanity check: install handler, raise SIGINT
    /// from our own thread, expect [`SHUTDOWN`] to be set on the
    /// second observable load.
    ///
    /// We can't safely raise the signal in unit tests without
    /// risking the second-press path that kills the test runner.
    /// Instead, we simulate the handler's effect directly.
    #[test]
    fn handler_flips_shutdown_flag() {
        // Reset baseline; SHUTDOWN is process-wide so don't assume
        // anything before us.
        SHUTDOWN.store(false, Ordering::Release);
        assert!(!shutdown_requested());

        // Simulate the handler body — first press.
        let prev = SHUTDOWN.swap(true, Ordering::Release);
        assert!(!prev, "first press must observe prior false");
        assert!(shutdown_requested());

        // Reset for downstream tests.
        SHUTDOWN.store(false, Ordering::Release);
    }

    #[test]
    fn install_succeeds_on_unix() {
        // Just verify the syscall doesn't fail; we don't actually
        // raise SIGINT during tests.
        if cfg!(unix) {
            install_sigint_handler().expect("sigaction must succeed on unix");
        }
    }
}
