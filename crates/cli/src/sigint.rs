// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Hand-rolled SIGINT / SIGTERM handler.
//!
//! Goal: when the user hits `Ctrl+C` (or `kill -INT $pid`), or the
//! restart lane sends `SIGTERM` (`scripts/daily-restart.sh`, launchd's
//! `bootout`), flip a single static [`AtomicBool`] that every ingress
//! run-loop polls at the top of its mio cycle. No external crate
//! (`signal-hook`, `ctrlc`, etc.) — those would either pull a runtime
//! or queue callbacks on a helper thread we don't need.
//!
//! **S7-L1 — SIGTERM drains too.** Only SIGINT used to be caught, so
//! the SIGTERM the restart lane sends five times a UTC day took the
//! default action and killed the process where it stood: no member
//! state flushed (the F18 drain law's `flush_member_state!` never ran on
//! a scheduled restart), no `Engine::stop`, and — once a slot trades
//! live — no cancel of the quotes resting on the venue. Both signals now
//! take the same path.
//!
//! The handler itself is **async-signal-safe**: one atomic swap and an
//! `alarm(2)`. No allocation, no I/O, no locks.
//!
//! Two-stage shutdown: the first signal flips the flag and arms a
//! [`DRAIN_DEADLINE_S`] alarm, whose default action kills a drain that
//! hangs — so a restart can be delayed by the drain, never prevented by
//! it. A second signal while the engine is still running re-raises the
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

/// **S7-L1** — seconds a drain may take after the first shutdown
/// signal before `SIGALRM`'s default action ends the process anyway.
///
/// The drain flushes member state, stops the engine (the live arm's
/// account-wide cancel runs there, itself bounded well inside this) and
/// joins the ingress threads. Thirty seconds is far above a healthy
/// drain and far below the capture catalog's 300 s gap tolerance; a
/// drain that hangs costs the restart at most this, where the old
/// SIGTERM path cost nothing and flushed nothing.
pub const DRAIN_DEADLINE_S: u32 = 30;

/// Install the shutdown handler for SIGINT and SIGTERM. Idempotent —
/// calling twice in the same process re-registers the same routine. A
/// second signal raises the default handler (SIG_DFL) so the process
/// exits.
pub fn install_sigint_handler() -> io::Result<()> {
    install_impl()
}

#[cfg(unix)]
fn install_impl() -> io::Result<()> {
    install_one(libc::SIGINT)?;
    install_one(libc::SIGTERM)
}

#[cfg(unix)]
fn install_one(sig: libc::c_int) -> io::Result<()> {
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
        let rc = libc::sigaction(sig, &sa as *const _, ::core::ptr::null_mut());
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
extern "C" fn handle_sigint(sig: libc::c_int) {
    // First signal: ask everything to stop politely — against a
    // deadline. `alarm` is on the async-signal-safe allowlist, and
    // SIGALRM's default action terminates the process, so a drain
    // that hangs is ended rather than left holding the restart.
    if !SHUTDOWN.swap(true, Ordering::Release) {
        // SAFETY: `alarm` takes a plain integer and touches no state
        // of ours; it is async-signal-safe per POSIX.
        unsafe {
            libc::alarm(DRAIN_DEADLINE_S);
        }
        return;
    }
    // Second signal: revert THAT signal to SIG_DFL and re-raise so the
    // process dies. This block is also async-signal-safe (sigaction
    // + raise are both on the allowlist).

    // SAFETY: zeroing the POD struct is fine, and we immediately
    // populate the one field we need (sa_sigaction = SIG_DFL).
    unsafe {
        let mut sa: libc::sigaction = ::core::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa as *const _, ::core::ptr::null_mut());
        libc::raise(sig);
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
