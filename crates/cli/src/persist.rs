// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `persist` — a member's persisted state, written off the engine thread
//!
//! HAR H3.7's writer, made generic by HC11b: the long-tenor series
//! ([`crate::har_writer`]) and the slot-7 book (`strategy_hcv::state`) share
//! it. The engine thread's whole share of a write is one copy of the state
//! into a [`core_ring::Mailbox`] — made by the state's owner, only while the
//! mailbox is FREE — and this module's thread does the rest:
//!
//! * a pass every [`PASS`] takes each FULL slot, renders it with the owner's
//!   renderer (the one renderer — the shutdown's forced write uses it too)
//!   into a reused buffer and writes it with `state_file::write_atomic`; the
//!   guard's drop hands the slot back;
//! * a failed write KEEPS the slot FULL (`Taken::keep`) and is retried every
//!   [`RETRY_NS`]: the engine's offers are refused meanwhile (never waited
//!   on), and the newest state follows at its first offer after the retry
//!   succeeds — a file never goes backwards. One warning a minute (F18's
//!   `warn_state_write`);
//! * [`StateWriter::shutdown`] stops the thread and joins it. A slot still
//!   FULL then is dropped unwritten: the engine loop's forced synchronous
//!   write, which runs next from the live state, supersedes it. The join
//!   comes first so a write in flight and the forced write never share a
//!   path's temp file.
//!
//! No lock anywhere: the engine thread never waits on this one. Cold thread
//! — the render buffer grows to the largest file once; `write_atomic`
//! allocates its temp name per write, as it did on the engine thread.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use core_ring::MailboxRx;

/// Between two passes of the writer thread: a handed state waits at most
/// this long (the file is for restarts — nothing reads it sooner).
pub const PASS: Duration = Duration::from_millis(250);

/// A failed write is retried this long after (F18's state-write cadence).
pub const RETRY_NS: u64 = 5_000_000_000;

/// A running state writer (module doc). Dropping it stops and joins it like
/// [`StateWriter::shutdown`].
pub struct StateWriter {
    kind: &'static str,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StateWriter {
    /// Spawn the thread `name` over `rx`: mailbox `i`'s state is rendered by
    /// `render(i, state, buf)` — `false` = nothing to write, the slot is
    /// handed back — and written to `paths[i]`. `kind` names the writer in
    /// its warnings (`"har"`, `"hcv"`).
    ///
    /// # Errors
    ///
    /// The thread could not be spawned — the caller decides what the state
    /// does without it.
    pub fn spawn<T, R>(
        name: &str,
        kind: &'static str,
        rx: Vec<MailboxRx<T>>,
        paths: Vec<PathBuf>,
        render: R,
    ) -> std::io::Result<Self>
    where
        T: Send + 'static,
        R: FnMut(usize, &T, &mut String) -> bool + Send + 'static,
    {
        Self::spawn_with(name, kind, rx, paths, render, PASS, RETRY_NS)
    }

    /// [`Self::spawn`] with the pass and the retry chosen (tests).
    pub(crate) fn spawn_with<T, R>(
        name: &str,
        kind: &'static str,
        rx: Vec<MailboxRx<T>>,
        paths: Vec<PathBuf>,
        render: R,
        pass: Duration,
        retry_ns: u64,
    ) -> std::io::Result<Self>
    where
        T: Send + 'static,
        R: FnMut(usize, &T, &mut String) -> bool + Send + 'static,
    {
        debug_assert_eq!(rx.len(), paths.len(), "one path per mailbox");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || run(rx, &paths, kind, render, &flag, pass, retry_ns))?;
        Ok(Self {
            kind,
            stop,
            thread: Some(thread),
        })
    }

    /// Stop the thread and join it (module doc: what a FULL slot means then).
    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        let Some(t) = self.thread.take() else {
            return;
        };
        self.stop.store(true, Ordering::Release);
        t.thread().unpark();
        if t.join().is_err() {
            tracing::error!(kind = self.kind, "the state writer thread panicked");
        }
    }
}

impl Drop for StateWriter {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// The writer thread: a pass over every mailbox, then a park until the next
/// one (or the stop's unpark).
fn run<T, R>(
    mut rx: Vec<MailboxRx<T>>,
    paths: &[PathBuf],
    kind: &'static str,
    mut render: R,
    stop: &AtomicBool,
    pass: Duration,
    retry_ns: u64,
) where
    R: FnMut(usize, &T, &mut String) -> bool,
{
    let n = rx.len().min(paths.len());
    let mut buf = String::new();
    let mut retry_at = vec![0u64; n];
    let mut warn_ns = 0u64;
    while !stop.load(Ordering::Acquire) {
        let now = core_time::now_ns();
        let mut i = 0usize;
        while i < n {
            if now >= retry_at[i] {
                if let Some(slot) = rx[i].try_take() {
                    if render(i, &*slot, &mut buf) {
                        match crate::state_file::write_atomic(&paths[i], &buf) {
                            Ok(()) => retry_at[i] = 0,
                            Err(reason) => {
                                crate::paper::warn_state_write(kind, &reason, &mut warn_ns, now);
                                retry_at[i] = now.saturating_add(retry_ns);
                                slot.keep();
                            }
                        }
                    }
                }
            }
            i += 1;
        }
        std::thread::park_timeout(pass);
    }
}
