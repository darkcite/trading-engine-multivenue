// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `har_writer` — the long-tenor state, written off the engine thread
//!
//! HAR H3.7. Until then each long-tenor series' `state-<NAME>.tsv` (~350
//! KiB for a fitted series) was rendered and written — create, write,
//! fsync, rename — on the engine thread inside its 5 s report block, at each
//! of the series' UTC day closes. Now the engine thread's whole share is one
//! copy of the series' engine into a [`core_ring::Mailbox`] — at the close's
//! own 1 s poll, only while the mailbox is FREE
//! (`StrategySet::offer_har_state`; the outbox is installed at boot by
//! `StrategySet::install_har_outbox`) — and the writer thread does the rest.
//!
//! The thread is [`crate::persist::StateWriter`] (its laws: the pass,
//! the kept slot and its retry, the shutdown's join before the forced
//! write). This module is the HAR side of it: one mailbox per series
//! (`har.toml` order), each slot boxed once here at boot ([`outbox`]), and
//! the renderer — `core_vol::render_state_file` under each series' header
//! name, the one renderer the shutdown's forced write uses too ([`spawn`]).

use std::path::PathBuf;
#[cfg(test)]
use std::time::Duration;

use core_ring::{Mailbox, MailboxRx, MailboxTx};
use core_vol::LongStateSnap;

use crate::persist::StateWriter;

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

/// Spawn the writer thread (`har-state-writer`) over `rx`: series `i` is
/// written to `paths[i]` under the header name `names[i]`. The three are in
/// `har.toml` order and of one length.
///
/// # Errors
///
/// The thread could not be spawned — the caller keeps the pre-H3.7 write on
/// the engine thread.
pub fn spawn(rx: Vec<MailboxRx<LongStateSnap>>, names: Vec<String>, paths: Vec<PathBuf>) -> std::io::Result<StateWriter> {
    debug_assert!(rx.len() == names.len() && rx.len() == paths.len());
    StateWriter::spawn("har-state-writer", "har", rx, paths, render(names))
}

/// The series' renderer: `false` for an index no name was given for.
fn render(names: Vec<String>) -> impl FnMut(usize, &LongStateSnap, &mut String) -> bool + Send + 'static {
    move |i, slot, buf| match names.get(i) {
        Some(name) => {
            core_vol::render_state_file(name, &slot.engine, buf);
            true
        }
        None => false,
    }
}

/// [`spawn`] with the pass and the retry chosen.
#[cfg(test)]
fn spawn_with(
    rx: Vec<MailboxRx<LongStateSnap>>,
    names: Vec<String>,
    paths: Vec<PathBuf>,
    pass: Duration,
    retry_ns: u64,
) -> std::io::Result<StateWriter> {
    StateWriter::spawn_with("har-state-writer", "har", rx, paths, render(names), pass, retry_ns)
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
        let w = spawn_with(
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
        let w = spawn_with(
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
        let w = spawn_with(
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
        drop(spawn_with(rx, vec!["X".into()], vec![d.join("x")], crate::persist::PASS, crate::persist::RETRY_NS).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }
}
