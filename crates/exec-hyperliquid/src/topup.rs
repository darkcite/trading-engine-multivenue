// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH5c-2 — slot 6's top-ups, stored off the engine thread.**
//!
//! The gateway arm (the engine thread) decides a request-weight purchase
//! at a budget read ([`crate::budget::TopupGovernor::take`]: the band rule
//! and the day's max — the day charged with the decision) and hands the
//! governor's state over through a [`TopupSlot`]; a [`TopupKeeper`] on the
//! gateway's thread stores it in the slot's state file (one a network,
//! `xmm-topup-<network>.state`, the blocking arm's line format,
//! [`crate::budget::TopupState`]) and marks it kept; the arm sends the
//! purchase only once it reads its hand-over kept. So a restart never
//! forgets a purchase that may have left: the day's max holds across
//! processes. After a purchase the arm hands its state over again without
//! waiting — a spend seen on the venue, or a stop — for the next boot.
//!
//! **One hand-over at a time:** the arm writes the state only once the
//! last one is stored, and the keeper reads it only after the version that
//! publishes it — no torn read, no lock. A store that fails marks the slot
//! failed: the arm buys nothing more (nothing is bought unrecorded), and
//! the keeper stores nothing more.
//!
//! **0 B after boot, on both threads, on the success path:** the hand-over
//! is six atomics in one 128 B granule; the keeper writes its line in place
//! ([`crate::budget::TopupState::write_line`]) through
//! [`core_io::write_atomic_at`] with its temp name taken at boot (shorter
//! than [`TMP_PATH_MAX`], so std builds its C path on the stack) — a temp
//! write, an fsync and a rename on the gateway's thread (its sockets wait
//! that long), at most twice a purchase: a hand-over equal to the state
//! last stored — a purchase refused unsent, decided again — is marked kept
//! with no write. A failed store formats its error once, and is the last.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use crate::budget::{TopupState, TOPUP_LINE_MAX};

/// Where a hand-over stands, as the arm reads it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Keep {
    /// Stored durably: the purchase may leave.
    Stored,
    /// Not stored yet — or, handing one over, the last one not stored yet
    /// (nothing written).
    Pending,
    /// A store failed: nothing more is kept.
    Failed,
}

/// **The hand-over** from slot 6's gateway arm (the engine thread) to its
/// [`TopupKeeper`] (the gateway's thread): the governor's day, its usage
/// and the session's spend, and the version that publishes them — the
/// arm's words; the version last stored and the failure flag — the
/// keeper's. A granule of its own: neither thread's hot lines share it,
/// and each side writes it only at a hand-over.
#[repr(C, align(128))]
#[derive(Debug, Default)]
pub struct TopupSlot {
    ver: AtomicU64,
    day: AtomicU64,
    used: AtomicU64,
    cost_1e6: AtomicI64,
    kept: AtomicU64,
    failed: AtomicBool,
}

// One granule: the words never share a line with another thread's writes.
const _: () =
    assert!(core::mem::size_of::<TopupSlot>() == 128 && core::mem::align_of::<TopupSlot>() == 128);

impl TopupSlot {
    /// Nothing handed over yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ver: AtomicU64::new(0),
            day: AtomicU64::new(0),
            used: AtomicU64::new(0),
            cost_1e6: AtomicI64::new(0),
            kept: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        }
    }

    /// The arm: hand `s`'s day, usage and spend over (the anchor is the
    /// keeper's) — the version to wait for.
    ///
    /// # Errors
    /// [`Keep::Pending`]: the last hand-over is not stored yet — nothing
    /// written; [`Keep::Failed`]: nothing more is kept.
    pub fn publish(&self, s: &TopupState) -> Result<u64, Keep> {
        if self.failed.load(Ordering::Acquire) {
            return Err(Keep::Failed);
        }
        // The arm alone writes `ver`; the keeper's Release of `kept` orders
        // its reads of the last hand-over before the writes below.
        let v = self.ver.load(Ordering::Relaxed);
        if self.kept.load(Ordering::Acquire) != v {
            return Err(Keep::Pending);
        }
        self.day.store(s.day, Ordering::Relaxed);
        self.used.store(s.used, Ordering::Relaxed);
        self.cost_1e6.store(s.cost_1e6, Ordering::Relaxed);
        self.ver.store(v + 1, Ordering::Release);
        Ok(v + 1)
    }

    /// The arm: where hand-over `v` stands.
    #[must_use]
    pub fn kept(&self, v: u64) -> Keep {
        if self.failed.load(Ordering::Acquire) {
            Keep::Failed
        } else if self.kept.load(Ordering::Acquire) >= v {
            Keep::Stored
        } else {
            Keep::Pending
        }
    }

    /// The keeper: the hand-over waiting and its version — its anchor left
    /// 0 (the keeper's to write). Two loads when none waits (the version
    /// and the last kept); five with one.
    #[must_use]
    pub fn pending(&self) -> Option<(u64, TopupState)> {
        let v = self.ver.load(Ordering::Acquire);
        // The keeper alone writes `kept`.
        if v == self.kept.load(Ordering::Relaxed) {
            return None;
        }
        Some((
            v,
            TopupState {
                day: self.day.load(Ordering::Relaxed),
                used: self.used.load(Ordering::Relaxed),
                anchor_set_s: 0,
                cost_1e6: self.cost_1e6.load(Ordering::Relaxed),
            },
        ))
    }

    /// The keeper: hand-over `v` is stored durably.
    pub fn mark_kept(&self, v: u64) {
        self.kept.store(v, Ordering::Release);
    }

    /// The keeper: a store failed — nothing more is kept.
    pub fn mark_failed(&self) {
        self.failed.store(true, Ordering::Release);
    }
}

/// What one [`TopupKeeper`] pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Topped {
    /// Nothing handed over, or the keeper is done.
    Idle,
    /// The hand-over is stored durably (the line's figures), the arm told.
    Stored(TopupState),
    /// The hand-over is the state already stored (a purchase refused
    /// unsent, decided again): marked kept, no write.
    Unchanged,
    /// The store failed (why): the slot is failed — nothing more is kept,
    /// nothing more bought.
    Failed(String),
}

/// The longest temp path a keeper takes, bytes (exclusive): std builds a
/// path's C string on the stack under 384 B — its private bound — and on
/// the heap past it; the keeper's store must stay 0 B after init, so a
/// longer path refuses the boot (gate 98 is the tripwire for a toolchain
/// that moves std's bound).
pub const TMP_PATH_MAX: usize = 256;

/// **XMM XH5c-2 — slot 6's top-up keeper**, on its gateway's thread beside
/// the anchor's (`crate::anchor::AnchorKeeper`). It stores each state the
/// arm hands over in the slot's file — before the purchase it was handed
/// over for leaves — tied to the session's anchor (its `set_unix_s`: the
/// restored one's, or the fresh one's once its keeper stored it,
/// [`Self::anchored_at`]), and marks it kept. A store that fails marks the
/// slot failed, once: the arm buys nothing more.
pub struct TopupKeeper {
    slot: Arc<TopupSlot>,
    path: std::path::PathBuf,
    tmp: std::path::PathBuf,
    master: [u8; 20],
    anchor_set_s: u64,
    /// The state last stored (valid once `stored`): a hand-over equal to it
    /// needs no write.
    last: TopupState,
    stored: bool,
    done: bool,
}

// 120 B: moved once, at boot, into the gateway's thread (noted there).
const _: () = assert!(core::mem::size_of::<TopupKeeper>() == 120);

impl TopupKeeper {
    /// Boot: keep what `slot` hands over in `path` (its temp name taken
    /// now), for `master`, under the anchor set at `anchor_set_s` (`0`: none
    /// yet).
    ///
    /// # Errors
    /// The temp path is [`TMP_PATH_MAX`] B or longer: every store would
    /// allocate after init (the path and its length named).
    pub fn new(
        slot: Arc<TopupSlot>,
        path: std::path::PathBuf,
        master: [u8; 20],
        anchor_set_s: u64,
    ) -> Result<Self, String> {
        // COPY: boot, once — the temp name built beside the path (one
        // `PathBuf`: the path's length + 4 B), so a store after init takes no
        // heap. Rejected: building it at each store.
        let tmp = core_io::tmp_path(&path);
        let len = tmp.as_os_str().len();
        if len >= TMP_PATH_MAX {
            return Err(format!(
                "{}: a temp path of {len} B — slot 6's top-up store would allocate after init \
                 (under {TMP_PATH_MAX} B; XMM XH5c-2): keep the exec artifact in a shorter \
                 directory",
                tmp.display()
            ));
        }
        Ok(Self {
            slot,
            path,
            tmp,
            master,
            anchor_set_s,
            last: TopupState::default(),
            stored: false,
            done: false,
        })
    }

    /// The session's fresh anchor was stored at `set_unix_s` (its keeper's
    /// stamp): the spend from here on belongs to it.
    pub fn anchored_at(&mut self, set_unix_s: u64) {
        self.anchor_set_s = set_unix_s;
    }

    /// One pass, once a gateway turn: two loads until the arm hands a state
    /// over ([`TopupSlot::pending`]); then the state already stored is marked
    /// kept with no write, any other stored — the line in place, the temp
    /// file, the fsync, the rename: a few ms. 0 B on the success path; a
    /// failed store formats its error, once — the keeper is done.
    pub fn turn(&mut self) -> Topped {
        if self.done {
            return Topped::Idle;
        }
        let Some((v, mut s)) = self.slot.pending() else {
            return Topped::Idle;
        };
        s.anchor_set_s = self.anchor_set_s;
        if self.stored && s == self.last {
            self.slot.mark_kept(v);
            return Topped::Unchanged;
        }
        let mut line = [0u8; TOPUP_LINE_MAX];
        let n = s.write_line(&self.master, &mut line);
        match core_io::write_atomic_at(&self.path, &self.tmp, &line[..n]) {
            Ok(()) => {
                self.last = s;
                self.stored = true;
                self.slot.mark_kept(v);
                Topped::Stored(s)
            }
            Err(e) => {
                self.done = true;
                self.slot.mark_failed();
                Topped::Failed(e)
            }
        }
    }

    /// Where it keeps the state.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: [u8; 20] = [0xAB; 20];

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mv-topup-keep-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(used: u64) -> TopupState {
        TopupState {
            day: 20_730,
            used,
            anchor_set_s: 0,
            cost_1e6: 2_500_000,
        }
    }

    /// Nothing kept until the arm hands a state over; then one store, tied
    /// to the anchor, read back by the restore; then nothing.
    #[test]
    fn the_keeper_stores_what_the_arm_hands_over_under_its_anchor() {
        let dir = scratch("keep");
        let p = dir.join("xmm-topup-testnet.state");
        let slot = Arc::new(TopupSlot::new());
        let mut k = TopupKeeper::new(Arc::clone(&slot), p.clone(), ADDR, 0).expect("keeper");
        assert_eq!(k.turn(), Topped::Idle, "nothing handed over");
        k.anchored_at(1_790_000_000);
        let v = slot.publish(&state(10_000)).expect("handed over");
        assert_eq!(slot.kept(v), Keep::Pending, "not before the store");
        let stored = TopupState {
            anchor_set_s: 1_790_000_000,
            ..state(10_000)
        };
        assert_eq!(k.turn(), Topped::Stored(stored));
        assert_eq!(slot.kept(v), Keep::Stored);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), stored.to_line(&ADDR));
        assert_eq!(
            crate::budget::restore_topup(&p, ADDR, 20_730, 1_790_000_000),
            (20_730, 10_000, 2_500_000)
        );
        assert!(!core_io::tmp_path(&p).exists(), "no temp file left");
        assert_eq!(k.turn(), Topped::Idle, "stored once");
        assert_eq!(k.path(), p.as_path());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One hand-over at a time: a second one before the first is stored
    /// writes nothing; the next after it supersedes it in the file.
    #[test]
    fn the_arm_hands_over_one_state_at_a_time() {
        let dir = scratch("one");
        let p = dir.join("xmm-topup-mainnet.state");
        let slot = Arc::new(TopupSlot::new());
        let mut k = TopupKeeper::new(Arc::clone(&slot), p.clone(), ADDR, 7).expect("keeper");
        let v1 = slot.publish(&state(10_000)).expect("the first");
        assert_eq!(
            slot.publish(&state(20_000)),
            Err(Keep::Pending),
            "the first is not stored yet"
        );
        assert!(
            matches!(k.turn(), Topped::Stored(s) if s.used == 10_000),
            "the first, untouched"
        );
        let v2 = slot.publish(&state(20_000)).expect("the next");
        assert!(v2 > v1);
        assert_eq!(slot.kept(v2), Keep::Pending);
        assert!(matches!(k.turn(), Topped::Stored(s) if s.used == 20_000));
        assert_eq!(slot.kept(v2), Keep::Stored);
        assert_eq!(crate::budget::restore_topup(&p, ADDR, 20_730, 7).1, 20_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A store that fails marks the slot failed, once: the arm reads it
    /// failed and hands nothing more over; the keeper stores nothing more.
    #[test]
    fn a_failed_store_keeps_nothing_more() {
        let dir = scratch("fail");
        let p = dir.join("no-such-dir").join("xmm-topup-testnet.state");
        let slot = Arc::new(TopupSlot::new());
        let mut k = TopupKeeper::new(Arc::clone(&slot), p.clone(), ADDR, 0).expect("keeper");
        let v = slot.publish(&state(10_000)).expect("handed over");
        assert!(matches!(k.turn(), Topped::Failed(_)));
        assert_eq!(slot.kept(v), Keep::Failed);
        assert_eq!(slot.publish(&state(0)), Err(Keep::Failed));
        assert_eq!(k.turn(), Topped::Idle, "one try");
        assert!(!p.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A hand-over equal to the state last stored — a purchase refused
    /// unsent, decided again — is marked kept with no write; any other is
    /// written; a new anchor makes the same figures another line.
    #[test]
    fn the_state_already_stored_is_kept_without_a_write() {
        let dir = scratch("same");
        let p = dir.join("xmm-topup-testnet.state");
        let slot = Arc::new(TopupSlot::new());
        let mut k = TopupKeeper::new(Arc::clone(&slot), p.clone(), ADDR, 7).expect("keeper");
        let _ = slot.publish(&state(10_000)).expect("handed over");
        assert!(matches!(k.turn(), Topped::Stored(_)));
        std::fs::remove_file(&p).expect("the file, removed to see a write");
        let v = slot.publish(&state(10_000)).expect("the same again");
        assert_eq!(k.turn(), Topped::Unchanged);
        assert_eq!(slot.kept(v), Keep::Stored);
        assert!(!p.exists(), "no write");
        let _ = slot.publish(&state(20_000)).expect("another");
        assert!(matches!(k.turn(), Topped::Stored(s) if s.used == 20_000));
        assert!(p.exists());
        k.anchored_at(9);
        let _ = slot.publish(&state(20_000)).expect("the same figures");
        assert!(
            matches!(k.turn(), Topped::Stored(s) if s.anchor_set_s == 9),
            "another anchor: another line"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A temp path the stack cannot hold refuses the keeper at boot.
    #[test]
    fn a_temp_path_too_long_for_the_stack_refuses_the_keeper() {
        let slot = Arc::new(TopupSlot::new());
        let long =
            std::path::PathBuf::from(format!("/{}/xmm-topup-testnet.state", "d".repeat(240)));
        let e = TopupKeeper::new(Arc::clone(&slot), long, ADDR, 0)
            .err()
            .expect("refused");
        assert!(e.contains("would allocate after init"), "{e}");
        let short = std::path::PathBuf::from("/x/xmm-topup-testnet.state");
        assert!(TopupKeeper::new(slot, short, ADDR, 0).is_ok());
    }
}
