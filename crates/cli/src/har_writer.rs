// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `har_writer` — the long-tenor state, written off the engine thread
//!
//! HAR H3.7. Until now each long-tenor series' `state-<NAME>.tsv` (~350 KiB
//! for a fitted series) was rendered and written — create, write, fsync,
//! rename — on the engine thread inside its 5 s report block, at each of
//! the series' UTC day closes. Now the engine thread's whole share is one
//! copy of the series' engine into a [`core_ring::Mailbox`] — at the
//! close's own 1 s poll, only while the mailbox is FREE
//! (`StrategySet::offer_har_state`; the outbox is installed at boot by
//! `StrategySet::install_har_outbox`) — and this module's thread does the
//! rest:
//!
//! * one mailbox per series (`har.toml` order), each slot boxed once here
//!   at boot ([`outbox`]);
//! * a pass every [`PASS`] takes each FULL slot, renders it with
//!   `core_vol::render_state_file` (the one renderer — the shutdown write
//!   uses it too) into a reused buffer and writes it with
//!   `state_file::write_atomic`; the guard's drop hands the slot back;
//! * a failed write KEEPS the slot FULL (`Taken::keep`) and is retried
//!   every [`RETRY_NS`]: the engine's offers are refused meanwhile (never
//!   waited on), and the newest state follows at its first poll after the
//!   retry succeeds — a file never goes backwards. One warning a minute
//!   (F18's `warn_state_write`);
//! * [`HarWriter::shutdown`] stops the thread and joins it. A slot still
//!   FULL then is dropped unwritten: the engine loop's forced synchronous
//!   write of every series, which runs next from the live engines (the
//!   open day moves the state without moving the epoch), supersedes it.
//!   The join comes first so a write in flight and the forced write never
//!   share a path's temp file.
//!
//! No lock anywhere: the engine thread never waits on this one. Cold
//! thread — the render buffer grows to the largest file once;
//! `write_atomic` allocates its temp name per write, as it did on the
//! engine thread.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use core_ring::{Mailbox, MailboxRx, MailboxTx};
use core_vol::LongStateSnap;

/// Between two passes of the writer thread: a handed state waits at most
/// this long (the file is for restarts — nothing reads it sooner).
pub const PASS: Duration = Duration::from_millis(250);

/// A failed write is retried this long after (F18's state-write cadence).
pub const RETRY_NS: u64 = 5_000_000_000;

/// The outbox of `n` series: the engine's producers (for
/// `StrategySet::install_har_outbox`) and the writer's consumers, index
/// for index. Boot-only: boxes each ~201 KiB slot on the heap.
#[must_use]
pub fn outbox(n: usize) -> (Vec<MailboxTx<LongStateSnap>>, Vec<MailboxRx<LongStateSnap>>) {
    let mut tx = Vec::with_capacity(n);
    let mut rx = Vec::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        let (t, r) = Mailbox::new(Box::new(LongStateSnap::new())).split();
        tx.push(t);
        rx.push(r);
        i += 1;
    }
    (tx, rx)
}

/// The running state writer (module doc). Dropping it stops and joins it
/// like [`HarWriter::shutdown`].
pub struct HarWriter {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HarWriter {
    /// Spawn the writer thread (`har-state-writer`) over `rx`: series `i`
    /// is written to `paths[i]` under the header name `names[i]`. The three
    /// are in `har.toml` order and of one length.
    ///
    /// # Errors
    ///
    /// The thread could not be spawned — the caller keeps the pre-H3.7
    /// write on the engine thread.
    pub fn spawn(
        rx: Vec<MailboxRx<LongStateSnap>>,
        names: Vec<String>,
        paths: Vec<PathBuf>,
    ) -> std::io::Result<Self> {
        Self::spawn_with(rx, names, paths, PASS, RETRY_NS)
    }

    fn spawn_with(
        rx: Vec<MailboxRx<LongStateSnap>>,
        names: Vec<String>,
        paths: Vec<PathBuf>,
        pass: Duration,
        retry_ns: u64,
    ) -> std::io::Result<Self> {
        debug_assert!(rx.len() == names.len() && rx.len() == paths.len());
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("har-state-writer".into())
            .spawn(move || run(rx, &names, &paths, &flag, pass, retry_ns))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Stop the thread and join it (module doc: what a FULL slot means
    /// then).
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
            tracing::error!("har: the state writer thread panicked");
        }
    }
}

impl Drop for HarWriter {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// The writer thread: a pass over every mailbox, then a park until the
/// next one (or the stop's unpark).
fn run(
    mut rx: Vec<MailboxRx<LongStateSnap>>,
    names: &[String],
    paths: &[PathBuf],
    stop: &AtomicBool,
    pass: Duration,
    retry_ns: u64,
) {
    let n = rx.len().min(names.len()).min(paths.len());
    let mut buf = String::new();
    let mut retry_at = vec![0u64; n];
    let mut warn_ns = 0u64;
    while !stop.load(Ordering::Acquire) {
        let now = core_time::now_ns();
        let mut i = 0usize;
        while i < n {
            if now >= retry_at[i] {
                if let Some(slot) = rx[i].try_take() {
                    core_vol::render_state_file(&names[i], &slot.engine, &mut buf);
                    match crate::state_file::write_atomic(&paths[i], &buf) {
                        Ok(()) => retry_at[i] = 0,
                        Err(reason) => {
                            crate::paper::warn_state_write("har", &reason, &mut warn_ns, now);
                            retry_at[i] = now.saturating_add(retry_ns);
                            slot.keep();
                        }
                    }
                }
            }
            i += 1;
        }
        std::thread::park_timeout(pass);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_vol::LongVolEngine;

    const WALL0_MS: u64 = 1_767_225_600_000; // 2026-01-01T00:00Z

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("har-writer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Two closed days and an open one: an engine with rows to write.
    fn engine(seed: i64) -> Box<LongVolEngine> {
        let mut e = Box::new(LongVolEngine::new());
        let mut m = 0u64;
        while m < 2 * 1_440 + 30 {
            let px = 100_000_000 + seed + ((m * 7_919) % 4_001) as i64;
            e.on_minute_close_at(px, WALL0_MS + m * 60_000);
            m += 1;
        }
        e
    }

    fn hand(tx: &mut MailboxTx<LongStateSnap>, e: &LongVolEngine, epoch: u64) {
        let mut slot = tx.try_fill().expect("the slot is FREE");
        e.copy_to(&mut slot.engine);
        slot.epoch = epoch;
        slot.commit();
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let t0 = std::time::Instant::now();
        while !done() {
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn rendered(name: &str, e: &LongVolEngine) -> String {
        let mut s = String::new();
        core_vol::render_state_file(name, e, &mut s);
        s
    }

    /// A handed state lands in its own series' file, rendered exactly as
    /// the engine thread would have, and the slot is FREE again for the
    /// next day close; a series nothing was handed for is not written.
    #[test]
    fn a_handed_state_lands_in_its_file_and_frees_the_slot() {
        let d = tmp("lands");
        let paths = vec![d.join("state-BTC.tsv"), d.join("state-ETH.tsv")];
        let (mut tx, rx) = outbox(2);
        let w = HarWriter::spawn_with(
            rx,
            vec!["BTC".into(), "ETH".into()],
            paths.clone(),
            Duration::from_millis(5),
            50_000_000,
        )
        .unwrap();
        let e = engine(0);
        hand(&mut tx[0], &e, 1);
        wait_for("the first write", || !tx[0].is_full() && paths[0].exists());
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), rendered("BTC", &e));
        assert!(!paths[1].exists(), "nothing handed: nothing written");

        // The next close: the same slot, the newer state.
        let e2 = engine(9);
        hand(&mut tx[0], &e2, 2);
        wait_for("the second write", || {
            !tx[0].is_full() && std::fs::read_to_string(&paths[0]).unwrap() == rendered("BTC", &e2)
        });
        w.shutdown();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A write that fails keeps the state: the slot stays FULL — the
    /// engine's next offer is refused, never waited on — and the write is
    /// retried until it lands; then the slot is handed back.
    #[test]
    fn a_failed_write_keeps_the_slot_and_retries() {
        let d = tmp("retry");
        let sub = d.join("not-yet");
        let path = sub.join("state-MU.tsv");
        let (mut tx, rx) = outbox(1);
        let w = HarWriter::spawn_with(
            rx,
            vec!["MU".into()],
            vec![path.clone()],
            Duration::from_millis(5),
            30_000_000,
        )
        .unwrap();
        let e = engine(3);
        hand(&mut tx[0], &e, 7);
        // Several passes and retries fail (no such directory).
        std::thread::sleep(Duration::from_millis(120));
        assert!(tx[0].is_full(), "the failed state is kept");
        assert!(tx[0].try_fill().is_none(), "the next offer is refused");
        assert!(!path.exists());
        std::fs::create_dir_all(&sub).unwrap();
        wait_for("the retried write", || !tx[0].is_full());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), rendered("MU", &e));
        w.shutdown();
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Shutdown stops and joins at once (the park is cut short), a FULL
    /// slot left to the engine loop's forced write; dropping a writer does
    /// the same.
    #[test]
    fn shutdown_joins_promptly_and_leaves_a_full_slot_to_the_forced_write() {
        let d = tmp("stop");
        let path = d.join("missing").join("state-ETH.tsv");
        let (mut tx, rx) = outbox(1);
        let w = HarWriter::spawn_with(
            rx,
            vec!["ETH".into()],
            vec![path.clone()],
            Duration::from_secs(3_600),
            u64::MAX,
        )
        .unwrap();
        let e = engine(5);
        hand(&mut tx[0], &e, 1);
        let t0 = std::time::Instant::now();
        w.shutdown();
        assert!(t0.elapsed() < Duration::from_secs(5), "the hour-long park was cut short");
        assert!(!path.exists());
        let (_tx, rx) = outbox(1);
        drop(HarWriter::spawn_with(rx, vec!["X".into()], vec![d.join("x")], PASS, RETRY_NS).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }
}
