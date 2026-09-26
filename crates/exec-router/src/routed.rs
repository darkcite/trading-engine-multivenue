// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The compositing dispatcher: one `OrderDispatch` over two arms,
//! choosing between them on `Order.strategy_id`.
//!
//! ## The hot path
//!
//! One masked byte load from the route table, one three-way branch,
//! one delegated call. No `dyn`, no allocation, no lock, no syscall.
//! Both arms are concrete type parameters, so the whole thing
//! monomorphises and the branch predicts perfectly in the steady state
//! (a given slot's mode never changes within a boot).
//!
//! ## The two laws this type exists to enforce
//!
//! **LAW E-1 — a live slot never falls back to paper.** A `Live` slot
//! whose order names a venue it has no route to is *refused and
//! counted*, never quietly matched on paper. A modelled fill carries
//! `FILL_ORIGIN_PAPER`, but every downstream reader of a live slot's
//! tape is entitled to assume its fills happened; silently satisfying
//! a live order on paper would put a trade that never occurred into
//! the P&L with live semantics.
//!
//! **LAW E-2 — the matcher's counters describe the PAPER arm only.**
//! `matcher_counters()` and `open_paper_orders()` forward to the paper
//! arm and nothing else, so `/metrics` can never suggest the matcher
//! is modelling something the venue is really doing.
//!
//! ## Where live fills come from (plan §0.1-1)
//!
//! **Not from here.** `QueuedDispatcher::try_next_fill` returns `None`
//! by contract ("fills flow through a separate path (engine fill
//! ring)"), and the engine already owns per-venue fill lanes —
//! `engine::fill_lane_of(VenueId::Hyperliquid) == Some(3)`, whose
//! producer the live worker thread takes over in E4. The engine drains
//! fill lanes *before* the dispatcher fill pump, so "a real fill never
//! queues behind a modelled one" is already a structural property of
//! the loop and costs no code here. `try_next_fill` therefore forwards
//! the **paper arm only**.

use crate::counters::{RiskRefusal, RouteCounters};
use crate::halt::{trigger_for, CancelPhase, HaltReason, HaltState};
use crate::ledger::Ledger;
use crate::mode::ExecMode;
use crate::route::{ExecRoute, EXEC_SLOTS};
use clob_dispatcher::{
    CancelAllState, DispatchError, DispatchStats, ExecCounters, MatcherCounters, OrderDispatch,
};
use core_types::{CancelReq, Fill, ModifyReq, NsTs, Order, Side, SymbolId, Tick};

/// How often `exec.HALT` is checked on the idle path. One second: an
/// operator reaching for a kill switch waits a second, and the engine
/// thread does one `stat` per second rather than five hundred.
const HALT_POLL_EVERY_NS: u64 = 1_000_000_000;

/// What [`RoutedDispatcher::read_halt_file`] found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum HaltFileRead {
    /// No file. The normal case.
    Absent,
    /// A file exists and could not be read as a halt file. Present
    /// and inert — never silent.
    Refused,
    /// `n` bytes read into the caller's buffer.
    Read(usize),
}

/// Routes each order to the paper matcher or the live arm according to
/// the boot-fixed [`ExecRoute`].
///
/// With an all-paper table this is observationally identical to the
/// bare paper arm — that equivalence is the E1 acceptance property and
/// is asserted in `tests::an_all_paper_table_is_indistinguishable_…`.
#[derive(Debug)]
pub struct RoutedDispatcher<P: OrderDispatch, L: OrderDispatch> {
    route: ExecRoute,
    paper: P,
    live: L,
    counters: RouteCounters,
    /// **E6 commit 3** — the per-slot sticky halt.
    halt: HaltState,
    /// **E6 commit 3** — where to write `exec.HALT` so an auto-halt
    /// survives a restart. `None` = do not persist (tests, and a boot
    /// with no exec artifact).
    ///
    /// The scheduled daily restart would otherwise clear an auto-halt
    /// at 00:10Z and resume trading into whatever tripped it,
    /// unattended.
    halt_path: Option<std::path::PathBuf>,
    /// The mtime of `exec.HALT` as this router last saw it. The
    /// runtime poll reads the file only when this moves, so the
    /// steady-state cost is one `stat` a second rather than a read.
    ///
    /// Refreshed after the router's OWN writes too, so a halt edge
    /// does not make the next poll re-read what it just wrote.
    halt_file_seen: Option<std::time::SystemTime>,
    /// `core_time::now_ns()` at the last poll. The cadence is in
    /// here rather than a counter because `on_idle`'s rate is a
    /// property of how busy the engine is, and "once a second" should
    /// not mean "more often when quiet".
    halt_poll_last_ns: u64,
    /// Times the cadence let a poll through to a `stat`. This is the
    /// number the cadence exists to hold down — a syscall on the
    /// engine thread — and it is counted separately from the reads
    /// because removing the cadence would leave the READ count at 1
    /// while the syscall rate went to five hundred a second.
    ///
    /// Internal instrumentation, not an operator surface: it exists
    /// so the cadence and the mtime check can each be pinned by a
    /// test and by alloc gate 62. What an operator reads is the boot
    /// tell and `/state`'s `exec` object.
    halt_file_polls: u64,
    /// Times `exec.HALT` was actually READ, as opposed to `stat`ed
    /// and found unchanged. Internal, as above.
    halt_file_reads: u64,
    /// Has a readable `exec.HALT` been seen at all? Distinguishes
    /// "no file" from "a file that halted nothing", which is the
    /// difference between a normal boot and an operator's kill switch
    /// silently doing nothing.
    halt_file_present: bool,
    /// Reads that parsed to no new halt. **The typo counter.**
    halt_file_inert: u64,
    /// Slots halted by reading the file rather than by a trigger.
    /// Reported at boot so an operator is told, loudly, that this
    /// engine started already stopped.
    halt_file_adopted: u32,
    /// **E6** — what the venue has actually done, as the router sees
    /// it. Boxed because it is ~14 KiB of tables and `RoutedDispatcher`
    /// is moved by value into the engine at boot; the engine's own
    /// struct is not a place to grow by cache lines nobody on the tick
    /// path reads. One pointer hop, on the refusal path only, past an
    /// HTTP round trip.
    ledger: Box<Ledger>,
}

impl<P: OrderDispatch, L: OrderDispatch> RoutedDispatcher<P, L> {
    /// Compose the two arms under `route`. Boot-only.
    ///
    /// `anchor` is the monotonic→wall conversion the day cap's 00:00Z
    /// epoch needs; take it with `core_time::WallAnchor::now()` at
    /// boot. See `ledger::Ledger::anchor` for why an engine timestamp
    /// cannot be fed to a wall-clock epoch directly.
    #[inline]
    pub fn new(route: ExecRoute, paper: P, live: L, anchor: core_time::WallAnchor) -> Self {
        Self {
            route,
            paper,
            live,
            counters: RouteCounters::new(),
            halt: HaltState::new(),
            halt_path: None,
            halt_file_seen: None,
            halt_poll_last_ns: 0,
            halt_file_polls: 0,
            halt_file_reads: 0,
            halt_file_present: false,
            halt_file_inert: 0,
            halt_file_adopted: 0,
            ledger: Box::new(Ledger::new(anchor)),
        }
    }

    /// **E6 commit 3** — the per-slot halt state. Cold; `/state`,
    /// `/metrics` and tests.
    #[inline]
    #[must_use]
    pub const fn halt(&self) -> &HaltState {
        &self.halt
    }

    /// **E6 commit 3** — persist an auto-halt to this path.
    ///
    /// Boot-only. Without it a halt lives only in this process, and
    /// the scheduled daily restart clears it at 00:10Z and resumes
    /// trading into whatever tripped it, unattended.
    #[inline]
    pub fn set_halt_path(&mut self, path: std::path::PathBuf) {
        self.halt_path = Some(path);
    }

    /// **Read `exec.HALT` into a fixed buffer.** `None` when there is
    /// nothing to read, or nothing we are willing to read.
    ///
    /// `exec.HALT` is an OPERATOR-WRITABLE INPUT consumed from the
    /// engine thread, so it gets treated as one. `read_to_string`,
    /// which this replaces, was three hazards at once:
    ///
    /// * **it allocates** — on the 2 ms thread, every time the file
    ///   changes;
    /// * **it is unbounded** — a multi-gigabyte file is a
    ///   multi-gigabyte `String`;
    /// * **it blocks** — `open` on a FIFO waits for a writer, for
    ///   ever, with no timeout and no log. That is a silent denial of
    ///   service on the one engine that might be needed to flatten a
    ///   position, and it is the exact opposite of the "a corrupt
    ///   file must not stop the boot" reading this file is meant to
    ///   have.
    ///
    /// So: `stat` first and refuse anything that is not a REGULAR
    /// FILE (a directory, a device, a socket, a FIFO — and a symlink
    /// to any of them, because `metadata` follows); refuse anything
    /// larger than a file we would have written; and open with
    /// `O_NONBLOCK` so that even a FIFO swapped in between the `stat`
    /// and the `open` returns instead of hanging.
    ///
    /// Zero allocation: the path goes to `stat`/`open` through std's
    /// stack fast path for short paths, and the bytes land in the
    /// caller's buffer.
    ///
    /// **Three answers, not two.** `Absent` is the normal case. `Refused`
    /// is a file that EXISTS and could not be read as a halt file — not
    /// regular, over `HALT_FILE_MAX`, unopenable, unreadable, or (the
    /// caller's check) not UTF-8. The first cut folded the second into
    /// the first, so an operator who wrote a halt file with the wrong
    /// permissions, or appended notes past 512 B, got a clean boot log
    /// and a trading engine: the kill switch silently disarmed, which
    /// commit 4's own prose called "strictly worse" than a loud
    /// refusal (E7 review, 2026-09-19). A refused file now counts as
    /// PRESENT and INERT, so the boot and the runtime poll both say so.
    fn read_halt_file(
        path: &std::path::Path,
        buf: &mut [u8; crate::halt::HALT_FILE_MAX],
    ) -> HaltFileRead {
        use std::io::Read as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let Ok(md) = std::fs::metadata(path) else {
            return HaltFileRead::Absent;
        };
        if !md.is_file() {
            return HaltFileRead::Refused;
        }
        if md.len() > crate::halt::HALT_FILE_MAX as u64 {
            return HaltFileRead::Refused;
        }
        let Ok(mut f) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        else {
            return HaltFileRead::Refused;
        };
        let mut n = 0usize;
        while n < buf.len() {
            // COPY: kernel → user read of exec.HALT, ≤ 512 B, at most
            // once a second behind the mtime gate — the read(2)
            // boundary, straight into the caller's fixed buffer —
            // rejected: mmap, which brings back the blocking and
            // unbounded hazards O_NONBLOCK + the stat guard remove.
            match f.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                // `O_NONBLOCK` can answer EAGAIN on an exotic file we
                // should not have reached anyway. Give up rather than
                // spin; the caller counts it and the next poll retries.
                Err(_) => return HaltFileRead::Refused,
            }
        }
        HaltFileRead::Read(n)
    }

    /// **E6 commit 4 — read `exec.HALT` back.**
    ///
    /// Called once at boot, after [`Self::set_halt_path`]. Every slot
    /// the file names is latched with the reason it names, exactly as
    /// though the trigger had fired in this process.
    ///
    /// Returns how many slots it halted, so the boot can say so
    /// loudly. **An engine that starts already stopped must be
    /// impossible to miss**: the failure this guards against is an
    /// operator seeing a clean boot log, assuming the halt cleared,
    /// and waiting for quotes that are never coming.
    ///
    /// A missing file is the normal case and halts nothing. An
    /// unreadable one halts nothing either and is NOT an error — the
    /// alternative, refusing the boot, would make a corrupt file a
    /// denial of service on an engine that might be needed to flatten
    /// a position.
    pub fn adopt_halt_file(&mut self) -> u32 {
        // Borrowed, not cloned: a `PathBuf` clone is a heap allocation
        // for nothing, and the runtime twin already borrows.
        let Some(path) = self.halt_path.take() else {
            return 0;
        };
        // The baseline for the runtime poll is taken whether or not
        // there is anything to adopt, so a file that appears later is
        // seen as a change.
        self.halt_file_seen = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut buf = [0u8; crate::halt::HALT_FILE_MAX];
        let adopted = match Self::read_halt_file(&path, &mut buf) {
            HaltFileRead::Absent => 0,
            HaltFileRead::Refused => {
                self.halt_file_present = true;
                self.halt_file_inert = self.halt_file_inert.saturating_add(1);
                0
            }
            HaltFileRead::Read(n) => {
                self.halt_file_present = true;
                match core::str::from_utf8(&buf[..n]) {
                    Ok(text) => self.latch_all(&crate::halt::parse_halt_file(text)),
                    Err(_) => {
                        self.halt_file_inert = self.halt_file_inert.saturating_add(1);
                        0
                    }
                }
            }
        };
        self.halt_path = Some(path);
        self.halt_file_adopted = adopted;
        adopted
    }

    /// How many slots the boot's `exec.HALT` halted. Cold; the boot
    /// tell and `/state`.
    #[inline]
    #[must_use]
    pub const fn halt_file_adopted(&self) -> u32 {
        self.halt_file_adopted
    }

    /// Times the halt file was read rather than merely `stat`ed.
    #[inline]
    #[must_use]
    pub const fn halt_file_reads(&self) -> u64 {
        self.halt_file_reads
    }

    /// Times the cadence let a poll through to a `stat`.
    #[inline]
    #[must_use]
    pub const fn halt_file_polls(&self) -> u64 {
        self.halt_file_polls
    }

    /// Was ANYTHING seen at the halt path? `true` with
    /// [`Self::halt_file_adopted`] at zero means an operator wrote a
    /// halt file that halted NOTHING — a typo, a slot that is not
    /// live, or a file the engine could not read at all (a FIFO, a
    /// directory, a non-UTF-8 body: `halt_file_inert` counts those) —
    /// which the boot must say out loud either way (E7 review).
    #[inline]
    #[must_use]
    pub const fn halt_file_present(&self) -> bool {
        self.halt_file_present
    }

    /// Runtime reads that parsed to no new halt.
    #[inline]
    #[must_use]
    pub const fn halt_file_inert(&self) -> u64 {
        self.halt_file_inert
    }

    /// Make the next `on_idle` poll the halt file regardless of when
    /// the last one ran. **Tests only** — production paces itself off
    /// the monotonic clock, which a test cannot advance.
    #[cfg(test)]
    #[inline]
    pub fn force_halt_poll(&mut self) {
        self.halt_poll_last_ns = 0;
    }

    /// Latch every halted slot in `reasons`, returning how many were
    /// NEW. Shared by the boot read-back and the runtime poll.
    ///
    /// **LIVE slots only**, exactly like the trigger loop. A paper or
    /// off slot cannot reach a venue, so halting one changes nothing
    /// it does — but `on_halt_edge` fires a VENUE-WIDE cancel, so a
    /// mistyped slot number in a hand-written halt file would pull
    /// the live arm's real quotes off the book. The skip is counted
    /// by the caller as an inert read.
    fn latch_all(&mut self, reasons: &[HaltReason; EXEC_SLOTS]) -> u32 {
        let mut n = 0u32;
        let mut slot = 0usize;
        while slot < EXEC_SLOTS {
            let here = slot;
            slot += 1;
            if !reasons[here].is_halted() {
                continue;
            }
            if !matches!(self.route.mode_at(here), Some(ExecMode::Live)) {
                continue;
            }
            if self.halt.latch(here, reasons[here]) {
                n += 1;
            }
        }
        if n > 0 {
            self.on_halt_edge();
        }
        n
    }

    /// **E6 commit 4 — the operator's runtime kill switch.**
    ///
    /// Polled from `on_idle`, at most once a second and with a `stat`
    /// rather than a read unless the file has actually changed. An
    /// operator writes `echo 3 > exec.HALT` and slot 3 stops inside a
    /// second, without a restart and without a control socket.
    ///
    /// **One direction only.** A halt found in the file is adopted; a
    /// halt *absent* from the file is NOT cleared, because a halt is
    /// sticky and clearing one is a restart-level decision. Deleting
    /// the file un-halts nothing in the running process — it only
    /// stops the NEXT boot from adopting it.
    fn poll_halt_file(&mut self) {
        let Some(path) = self.halt_path.as_ref() else {
            return;
        };
        let now = core_time::now_ns();
        // `now_ns` is monotonic, so this only fails to fire early.
        if now.saturating_sub(self.halt_poll_last_ns) < HALT_POLL_EVERY_NS {
            return;
        }
        self.halt_poll_last_ns = now;
        self.halt_file_polls = self.halt_file_polls.saturating_add(1);
        let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) else {
            // Gone, or unreadable. Nothing to adopt, and nothing is
            // un-halted by a missing file.
            return;
        };
        if self.halt_file_seen == Some(mtime) {
            return;
        }
        self.halt_file_reads = self.halt_file_reads.saturating_add(1);
        let mut buf = [0u8; crate::halt::HALT_FILE_MAX];
        // The baseline moves only on a SUCCESSFUL read. A refused file
        // stays "unseen", so the next second retries it — the first cut
        // advanced the baseline before reading and never came back.
        let n = match Self::read_halt_file(path, &mut buf) {
            HaltFileRead::Absent => return,
            HaltFileRead::Refused => {
                self.halt_file_present = true;
                self.halt_file_inert = self.halt_file_inert.saturating_add(1);
                return;
            }
            HaltFileRead::Read(n) => n,
        };
        self.halt_file_seen = Some(mtime);
        self.halt_file_present = true;
        let Ok(text) = core::str::from_utf8(&buf[..n]) else {
            self.halt_file_inert = self.halt_file_inert.saturating_add(1);
            return;
        };
        let reasons = crate::halt::parse_halt_file(text);
        if self.latch_all(&reasons) == 0 {
            // Read, and nothing came of it. Counted, because a halt
            // file an operator wrote that halts nothing is the
            // failure the loud boot tell exists to prevent — arriving
            // by the other door.
            self.halt_file_inert = self.halt_file_inert.saturating_add(1);
        }
    }

    /// **E6 commit 3** — halt a slot on the operator's say-so.
    ///
    /// The same latch every trigger uses, so an operator halt is
    /// exactly as sticky and cancels exactly as hard.
    ///
    /// **LIVE slots only**, the same filter `latch_all` carries and
    /// for the same reason: the edge fires a VENUE-WIDE cancel, so
    /// `--halt-slot <paper slot>` would have pulled the live arm's
    /// real quotes for nothing, written an `exec.HALT` the next boot
    /// refuses to adopt, and halted nothing (a paper submit never
    /// consults the latch). Returns whether the slot was halted, so
    /// the cli can say which slots it actually stopped.
    pub fn halt_slot(&mut self, slot: usize, why: HaltReason) -> bool {
        if !matches!(self.route.mode_at(slot), Some(ExecMode::Live)) {
            return false;
        }
        if self.halt.latch(slot, why) {
            self.on_halt_edge();
            // An operator halt does not wait for the next idle poll.
            // It is usually a human reacting to something, and this
            // path is also reachable at boot — from a halt file the
            // last run left behind — where the idle driver has not
            // started and might never, depending on the boot mode.
            self.try_cancel_all();
            return true;
        }
        false
    }

    /// A slot has just halted: write the file that makes the halt
    /// survive a restart.
    ///
    /// Takes no slot, deliberately. Everything it does is a function
    /// of the WHOLE halt state — the file names every halted slot,
    /// and the cancel below is venue-wide — so a per-slot argument
    /// here would be an invitation to write per-slot behaviour that
    /// is wrong by construction.
    fn on_halt_edge(&mut self) {
        // **The venue-wide cancel is not fired here.** `latch` marked
        // it pending and the caller performs it through
        // `try_cancel_all` — the single place that ever calls the
        // arm's `cancel_all` — so the edge is not delayed by a tick,
        // but it also cannot be attempted twice back to back with
        // nothing changing at the venue in between.
        //
        // The same holds when two slots halt on one poll: cancel-all
        // is venue-wide, so one call answers both edges.
        self.write_halt_file();
    }

    /// **The only place that calls the live arm's `cancel_all`.**
    ///
    /// One step per poll, and which step depends on where the cycle
    /// is. `cancel_all` REQUESTS; `cancel_all_state` CONFIRMS. They
    /// are separate because on a real venue they are separated by
    /// minutes — the arm turns one cancel-all into a queued sweep per
    /// live leg, and each sweep asks the venue what is resting and
    /// cancels by oid over many idle moments.
    ///
    /// **Reading the request's `Ok(())` as the confirmation is a
    /// fail-OPEN bug, and this lane shipped it for a day.** It made
    /// the router stop retrying the moment the sweep was *queued*,
    /// and — worse — zero the ledger's resting count for every slot
    /// while those orders were still working, handing the HEALTHY
    /// slots a permissive `max_open_orders` at the exact moment a
    /// sibling slot had halted.
    fn try_cancel_all(&mut self) {
        match self.halt.cancel_phase() {
            CancelPhase::None => return,
            // Latched but never asked. Ask, below.
            CancelPhase::Wanted => self.halt.cancel_requested(),
            // A request did not land. Ask again, below — unless the
            // arm is still draining what the failed request DID
            // queue, in which case there is no room for the rest yet
            // and asking would fail again, 500 times a second.
            CancelPhase::Retry => {
                if matches!(self.live.cancel_all_state(), CancelAllState::Working) {
                    return;
                }
                self.halt.cancel_requested();
            }
            CancelPhase::Asked => match self.live.cancel_all_state() {
                // The venue said it holds nothing of ours, so every
                // slot's resting count is now TRUTHFULLY zero —
                // including the healthy slots, which were cancelled
                // too and will simply re-quote.
                CancelAllState::Clear => {
                    self.halt.cancel_cleared();
                    self.ledger.clear_resting();
                    return;
                }
                // A sweep is draining. It IS the retry; asking again
                // every 2 ms would queue nothing and count everything.
                CancelAllState::Working => return,
                // The arm gave up on a leg, or never queued one. Ask
                // again, below — a fresh request gives it fresh
                // retries, and a halted slot with orders still
                // working at the venue is the state LAW E-8 exists
                // to avoid.
                CancelAllState::Stranded => self.halt.cancel_stranded(),
            },
        }

        // One request per poll, reached two ways. The re-request
        // lives HERE rather than inside the `Stranded` arm so that
        // confirming can never loop back into asking.
        if self.live.cancel_all().is_err() {
            self.halt.cancel_failed();
            return;
        }
        // Confirm in the SAME poll. An arm that cancels synchronously
        // is already clear; one that sweeps answers `Working`, so
        // asking now costs nothing and keeps the halt edge crisp.
        if matches!(self.live.cancel_all_state(), CancelAllState::Clear) {
            self.halt.cancel_cleared();
            self.ledger.clear_resting();
        }
    }

    /// Write `exec.HALT`, once per halt edge. Best effort: a halt
    /// that could not be persisted is still a halt, and refusing to
    /// halt because a file write failed would be the wrong direction.
    ///
    /// **The WHOLE state, every time.** An earlier version wrote only
    /// the slot that had just tripped, so with two slots halted the
    /// file named one — and the boot read-back would have resumed the
    /// other, which is the exact failure the file exists to prevent.
    fn write_halt_file(&mut self) {
        let Some(path) = self.halt_path.as_ref() else {
            return;
        };
        // Fixed buffer, no `format!`: this runs on the engine thread.
        // `write_atomic` takes `&str`, and every byte rendered is
        // ASCII by construction, so the conversion cannot fail and is
        // refused rather than unwrapped if it ever could.
        let mut buf = [0u8; crate::halt::HALT_FILE_MAX];
        let n = crate::halt::render_halt_file(&mut buf, self.halt.reasons());
        if let Ok(text) = core::str::from_utf8(&buf[..n]) {
            let _ = core_io::write_atomic(path, text);
        }
        // Our own write is not an operator edit. Without this the next
        // poll would re-read the file the edge just produced, every
        // time.
        self.halt_file_seen = self
            .halt_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    }

    /// **E6** — the venue-fill ledger. Cold; `/state`, `/metrics`
    /// and tests.
    #[inline]
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// **E6 — the ledger has been reconciled against the venue.**
    ///
    /// Until this is called the risk gate refuses every live PLACE,
    /// because an unreconciled ledger reads zero exposure, zero
    /// turnover and zero resting orders — which after a restart is not
    /// the truth, and would fail all three clamps OPEN against
    /// whatever the previous boot left working.
    ///
    /// **Production seeds through `on_idle`** (E6 commit 3: the first
    /// `halt_signal().reconciled` the arm reports). This is the door
    /// the tests and the alloc gates use to start from a seeded
    /// ledger without a venue — it is the same `Ledger::mark_seeded`
    /// call, so there is one interlock and two callers of it.
    #[inline]
    pub fn mark_ledger_seeded(&mut self) {
        self.ledger.mark_seeded();
    }

    /// What the router did. Cold; tests and the alloc gates — the
    /// production surface is `exec_counters()` through the trait. By
    /// reference: four cache lines.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> &RouteCounters {
        &self.counters
    }

    /// The paper arm. Tests and the alloc gates; production reaches it
    /// only through the trait.
    #[inline]
    pub fn paper(&self) -> &P {
        &self.paper
    }

    /// The live arm, same.
    #[inline]
    pub fn live(&self) -> &L {
        &self.live
    }

    /// The live arm, mutably. **Tests only.** Production never reaches
    /// the arm this way — it goes through the `OrderDispatch` trait,
    /// which is what the router is written against. A test uses it to
    /// set the spy's `halt_signal()` payload before an idle poll.
    #[cfg(test)]
    #[inline]
    pub fn live_mut(&mut self) -> &mut L {
        &mut self.live
    }
}

impl<P: OrderDispatch, L: OrderDispatch> RoutedDispatcher<P, L> {
    /// **E6 — the risk gate.** Four clamps, live arm only.
    ///
    /// | clamp | asks | from |
    /// |---|---|---|
    /// | `max_order_usd` | is this ONE order too big | the request |
    /// | `cap_instance_usd` | would it RAISE net exposure past the cap | the ledger |
    /// | `cap_day_usd` | would it take today's BUY TURNOVER too far | the ledger |
    /// | `max_open_orders` | are too many already working | the ledger |
    ///
    /// Ahead of all four: **has this ledger ever been reconciled?**
    /// If not, its three numbers describe an empty world rather than
    /// the venue's, and every verb that can reach the venue is
    /// refused.
    ///
    /// ## Why here and not in the member
    ///
    /// bin15 has its own `cap_instance`/`cap_day` ledger and sizes
    /// every order against it. This is a SECOND OPINION, not a copy:
    /// it is the operator's number, it is checked on the dispatch path
    /// rather than the sizing path, and — for the two money caps — it
    /// is computed from **what the venue actually filled**, while the
    /// member's is computed from what it INTENDED to spend (notional
    /// reserved at submit, credited back on the unfilled part).
    ///
    /// **Those are different quantities, deliberately.** The member's
    /// `cap_instance` counts cumulative entry notional; this one
    /// counts `|yes − no|`, money actually at stake. So a gap between
    /// the two is NOT automatically an alarm the way a
    /// `max_order_usd` refusal is — a member can sit well inside its
    /// own turnover cap while holding a one-sided position this clamp
    /// refuses to add to, and that is the clamp working, not a
    /// disagreement. `refused_max_order` is the field that means "the
    /// two ledgers disagreed"; the other three mean "the operator's
    /// ceiling was reached".
    ///
    /// ## Order of the tests
    ///
    /// Cheapest and most local first: the request's own notional, then
    /// the resting count (one array load), then the two ledger walks.
    /// Only the FIRST clamp to fire is counted — the order stops at
    /// it, so no second clamp is ever breached, and counting a
    /// "would also have failed" would inflate the number an operator
    /// reads as a rate.
    ///
    /// ## Notional
    ///
    /// `px` and `qty` are both ×1e6, so their product over 1e6 is USD
    /// ×1e6, the same scale the caps are stated in. Done in `i128`
    /// because the raw product overflows `i64` at ~9.2e18 — about
    /// $9.2 M of notional (a $4 M price and three contracts), which is
    /// inside the range a fat-fingered `exec.toml` could ask for — and
    /// a saturating product would clamp to a POSITIVE `i64::MAX` and
    /// sail past a cap rather than into it. The narrow back to `i64`
    /// SATURATES too: an `as i64` cast would wrap the same product
    /// negative and re-open the hole the `i128` closed.
    ///
    /// ## A cap of 0
    ///
    /// `0` means UNSET, never "unlimited" — `core_config::exec` says
    /// so in those words and REFUSES A LIVE SLOT that leaves any of
    /// the four at zero, so a boot can never reach this with an unset
    /// clamp. A default `ExecRoute` leaves all four zero, but a
    /// default route is all-paper and never gets here.
    ///
    /// Three of the four then refuse everything. **`cap_instance` does
    /// not**, and saying it did would be a checkably wrong claim: its
    /// second test lets any non-increasing order through at any cap,
    /// zero included. `max_open_orders = 0` refuses every PLACE first,
    /// so nothing reaches the venue either way — but the reason is the
    /// open-order clamp, not this one.
    ///
    /// ## What these clamps do NOT bound
    ///
    /// `cap_instance` is computed from FILLS. A slot's own resting
    /// orders are not in it, so N orders in flight are each judged
    /// against the same unchanged position and all N can pass. The
    /// worst case with every quote working is
    /// `cap_instance_usd + max_open_orders × max_order_usd`, not
    /// `cap_instance_usd`. The three numbers MULTIPLY, and an
    /// operator setting them needs to know that. Projecting resting
    /// notional too would mean assuming both sides of a two-sided
    /// quote fill, which cannot happen and would strangle the maker —
    /// so this is a stated bound, not an oversight.
    fn risk_check(&mut self, order: &Order, verb: RiskVerb) -> Result<(), DispatchError> {
        let slot = order.strategy_id as usize;
        let Some(caps) = self.route.caps_at(slot) else {
            // No such slot. The caller's own `mode()` lookup masks the
            // id into range, so this is unreachable — and it refuses
            // rather than passing, because a clamp that cannot find
            // its number must not wave the order through. Counted as
            // UNSEEDED, "the gate has nothing to judge this against",
            // never as `MaxOrder`, which is the one field an alert is
            // wired to and means "the member and the operator
            // disagreed".
            self.counters
                .on_refused_risk(order.strategy_id, RiskRefusal::Unseeded);
            return Err(DispatchError::RiskRefused);
        };

        // ---- 0a. is this slot halted? -------------------------------
        //
        // Before every clamp, because a halted slot's numbers are the
        // least trustworthy thing in the process — a reconciliation
        // drift halt means the ledger and the venue disagree, which
        // is exactly what the clamps below are computed from.
        //
        // A CANCEL never reaches here (`cancel` does not call
        // `risk_check`), which is the whole escape hatch: a halted
        // slot can always get flat.
        if self.halt.is_halted(slot) {
            // Counted ONCE, in `RouteCounters`, alongside every other
            // refusal reason. `HaltState` used to keep a second field
            // of the same name and the same value that nothing ever
            // read — two counters for one fact is how they drift.
            return self.refuse(order.strategy_id, RiskRefusal::Halted);
        }

        // ---- 0b. does this ledger know what the venue holds? --------
        //
        // A ledger that has not been reconciled reads zero exposure,
        // zero turnover and zero resting orders — which after a
        // RESTART is not the truth, and all three clamps below would
        // fail OPEN against whatever the previous boot left at the
        // venue.
        //
        // **BOTH VERBS.** An earlier cut exempted a modify, on the
        // reasoning that refusing one would strand a quote at the
        // venue with no way to move or shrink it. That reasoning was
        // simply wrong: `cancel` is never risk-checked, so a member
        // always has a way to take a quote back, and waiting for
        // seeding is not being stranded. What the exemption actually
        // bought was the one thing this interlock exists to stop — a
        // modify RAISES price and size, the venue holds pre-boot
        // orders across our restarts, and the exempted verb would
        // have been judged against a ledger reading zero. Commit 1's
        // own note names that hole: "a clamp on `submit` alone leaves
        // the cap reachable by repricing upward."
        //
        // Per slot since HYPARB L4: seeded by the reconciler of the arm
        // that trades THIS slot.
        if !self.ledger.is_slot_seeded(slot) {
            return self.refuse(order.strategy_id, RiskRefusal::Unseeded);
        }

        let px = order.px.raw();
        let qty = order.qty.raw();
        let buy = order.side == Side::Bid;
        let notional_1e6 =
            i64::try_from((px as i128).saturating_mul(qty as i128) / 1_000_000)
                .unwrap_or(i64::MAX);
        // **L1 — an EXIT is never capped by clamps 1 and 2** ("a cap
        // never blocks an exit", the policy's own rule). A SELL no
        // larger than what the slot holds on that very leg returns
        // premium and adds no turnover. Clamps 1 and 2 read the order's
        // notional and the resting count, which are blind to direction,
        // so without this a close larger than one entry (an Arm A close
        // sells the whole free holding) was refused on every reprice and
        // trapped the member in the position. Clamps 3 and 4 still judge
        // it on their own terms — clamp 3 matters when the slot holds
        // BOTH legs of an outcome, where selling one leg raises the net
        // exposure. A halted or unseeded slot still refuses above: a
        // halt trades nothing, and an unseeded ledger knows no holding.
        //
        // `buy || qty > held` is "not an exit", asked only when clamp 1
        // or 2 would otherwise refuse: the holding is a walk of up to
        // `LEDGER_ROWS` rows, and the clamps' own tests are the cheaper
        // ones.

        // ---- 1. this one order ---------------------------------------
        if notional_1e6 > caps.max_order_usd_1e6
            && (buy || qty > self.ledger.held_on_sym_1e6(slot, order.sym))
        {
            return self.refuse(order.strategy_id, RiskRefusal::MaxOrder);
        }

        // ---- 2. how many are already working -------------------------
        // A REPLACE is exempt by LAW E-7: a modify swaps one resting
        // order for another in place, so the count it is judged
        // against is the count it will leave behind. Testing it would
        // refuse the requote of a slot sitting exactly at its cap —
        // which is the slot that most needs to be able to move its
        // quotes.
        if matches!(verb, RiskVerb::Place)
            && self.ledger.slot_resting(slot) >= caps.max_open_orders
            && (buy || qty > self.ledger.held_on_sym_1e6(slot, order.sym))
        {
            return self.refuse(order.strategy_id, RiskRefusal::OpenOrders);
        }

        // ---- 3. what it would leave at stake -------------------------
        //
        // **Two conditions, and the second is not redundant.** Over
        // the cap is not enough: the order must also INCREASE
        // exposure. A slot can be over its cap without having asked
        // to be — the operator lowered the number, or fills landed
        // past what any projection could have known — and the only
        // way out of a position is to send an order. A clamp that
        // tested `projected > cap` alone would refuse exactly that
        // order and TRAP the member inside the exposure the cap
        // exists to bound, with no path back except an operator
        // cancelling by hand.
        //
        // So: an order that does not raise exposure is never refused
        // here. Below the cap, increases up to it pass. At or over
        // it, every increase is refused and every reduction passes.
        // There is no way to nibble upward — any increase at all
        // fails the second test once the first is failing.
        // MONOTONIC — `Order::ts_ns` comes from the engine's
        // `core_time::now_ns`, which is `CLOCK_MONOTONIC_RAW`. The
        // ledger converts through its boot anchor; handing it straight
        // to a `wall_ns / DAY_NS` epoch would put two clocks in one
        // field and wipe the day's turnover on every alternation with
        // a (wall-stamped) venue fill.
        self.ledger.observe_mono_clock(order.ts_ns);
        let current = self.ledger.slot_exposure_1e6(slot);
        let projected = self
            .ledger
            .projected_exposure_1e6(slot, order.sym, qty, buy);
        if projected > caps.cap_instance_usd_1e6 && projected > current {
            return self.refuse(order.strategy_id, RiskRefusal::CapInstance);
        }

        // ---- 4. what it would have bought today ----------------------
        // BUYS ONLY. A sell adds no turnover, and testing one would
        // refuse the order that closes a position on a day whose cap
        // is already spent — trapping a member inside exactly the
        // exposure the caps exist to bound.
        if buy {
            let day = self
                .ledger
                .slot_day_turnover_1e6(slot)
                .saturating_add(notional_1e6);
            if day > caps.cap_day_usd_1e6 {
                return self.refuse(order.strategy_id, RiskRefusal::CapDay);
            }
        }

        Ok(())
    }

    /// Count one refusal and name it. Never inlined into four copies
    /// of the same two lines.
    #[inline]
    fn refuse(&mut self, strategy_id: u8, why: RiskRefusal) -> Result<(), DispatchError> {
        self.counters.on_refused_risk(strategy_id, why);
        Err(DispatchError::RiskRefused)
    }
}

/// Which lifecycle verb the risk gate is judging.
///
/// Only [`RiskVerb::Place`] adds to the slot's resting count, so only
/// it can be the order that takes the count over `max_open_orders`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RiskVerb {
    /// A fresh order.
    Place,
    /// A modify — LAW E-7, one resting order swapped for another.
    Replace,
}

impl<P: OrderDispatch, L: OrderDispatch> OrderDispatch for RoutedDispatcher<P, L> {
    /// Route one order. **Hot path.**
    ///
    /// E6: the risk clamp runs on the LIVE arm only. A paper slot is
    /// modelling, and refusing its orders would make the model
    /// disagree with the harness — which replays the same intents
    /// through no such gate — for a reason that has nothing to do
    /// with the strategy. The clamp exists to stop real money
    /// leaving.
    #[inline]
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        match self.route.mode(order.strategy_id) {
            ExecMode::Paper => {
                self.counters.on_paper_submit();
                self.paper.submit(order)
            }
            ExecMode::Off => {
                self.counters.on_refused_off(order.strategy_id);
                Err(DispatchError::SlotDisabled)
            }
            ExecMode::Live => {
                if !self.route.venue_allowed(order.strategy_id, order.venue) {
                    // LAW E-1. NOT `self.paper.submit(order)`.
                    self.counters.on_refused_no_route(order.strategy_id);
                    return Err(DispatchError::NoLiveRoute);
                }
                self.risk_check(order, RiskVerb::Place)?;
                self.counters.on_live_submit(order.strategy_id);
                let r = self.live.submit(order);
                if r.is_ok() {
                    // **Only on acceptance.** An order the arm refused
                    // is not working at the venue, and counting it
                    // would leak the resting count upward until
                    // `max_open_orders` refused a slot that had
                    // nothing resting at all.
                    self.ledger.on_submit(
                        order.client_oid,
                        order.strategy_id as usize,
                        order.sym,
                        order.qty.raw(),
                    );
                }
                r
            }
        }
    }

    /// Route one cancel. **LAW E-1 applies to a cancel too, and
    /// harder.**
    ///
    /// A live slot's cancel satisfied by the paper matcher would take
    /// a modelled order out of a modelled book and report success,
    /// while the real quote stays resting at the venue — the strategy
    /// then believes it has no exposure and the venue disagrees. A
    /// mis-routed submit invents a fill; a mis-routed cancel invents
    /// the ABSENCE of one, which nothing downstream can detect.
    ///
    /// Same three-way branch as `submit`, on the cancel's own
    /// `strategy_id`/`venue` — which `StampCtx` stamps exactly as it
    /// stamps an order's.
    #[inline]
    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        match self.route.mode(req.strategy_id) {
            ExecMode::Paper => self.paper.cancel(req),
            // **A cancel passes on an `Off` slot** (E7 review ruling,
            // 2026-09-19). `off` stops PLACING. A cancel can only
            // reduce risk, the halt machine's own law is "a cancel is
            // the only way to get flat and must never be blocked",
            // and refusing it here left a slot flipped live → off
            // across a restart with orders resting at the venue and
            // no in-engine path to take them back — the trigger loop
            // and `latch_all` both skip non-live slots, so not even
            // the venue-wide sweep could reach them. Routed by the
            // slot's venue mask exactly as a live cancel is; an `off`
            // slot whose artifact names no venue has nowhere to send
            // it and is refused as before.
            ExecMode::Off => {
                if !self.route.venue_allowed(req.strategy_id, req.venue) {
                    self.counters.on_refused_off(req.strategy_id);
                    return Err(DispatchError::SlotDisabled);
                }
                self.counters.on_cancel_on_off();
                let r = self.live.cancel(req);
                if r.is_ok() {
                    self.ledger
                        .on_cancel(req.client_oid, req.strategy_id as usize);
                }
                r
            }
            ExecMode::Live => {
                if !self.route.venue_allowed(req.strategy_id, req.venue) {
                    self.counters.on_refused_no_route(req.strategy_id);
                    return Err(DispatchError::NoLiveRoute);
                }
                let r = self.live.cancel(req);
                if r.is_ok() {
                    self.ledger
                        .on_cancel(req.client_oid, req.strategy_id as usize);
                }
                r
            }
        }
    }

    /// Route one modify — LAW E-1 and LAW E-7 together. Identical
    /// branch to `cancel`, on the REPLACEMENT's routing fields: a
    /// modify carries a whole `Order`, and the slot/venue that own
    /// the resting order are the slot/venue that own its replacement
    /// (a modify may not change either — [`core_types::OrderIdentity`]).
    #[inline]
    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        match self.route.mode(req.order().strategy_id) {
            ExecMode::Paper => self.paper.modify(req),
            ExecMode::Off => {
                self.counters.on_refused_off(req.order().strategy_id);
                Err(DispatchError::SlotDisabled)
            }
            ExecMode::Live => {
                if !self.route.venue_allowed(req.order().strategy_id, req.order().venue) {
                    self.counters.on_refused_no_route(req.order().strategy_id);
                    return Err(DispatchError::NoLiveRoute);
                }
                // **A modify can RAISE size**, so a clamp on `submit`
                // alone leaves the cap reachable by repricing upward
                // — the hole the E5 commit-4b review named. The
                // replacement is measured exactly as a fresh order is.
                self.risk_check(req.order(), RiskVerb::Replace)?;
                let r = self.live.modify(req);
                if r.is_ok() {
                    self.ledger.on_modify(
                        req.prev_client_oid(),
                        req.order().client_oid,
                        req.order().strategy_id as usize,
                        req.order().sym,
                        req.order().qty.raw(),
                    );
                }
                r
            }
        }
    }

    /// Paper fills first, then the live arm's. The Hyperliquid arm's
    /// venue fills ride the engine's own fill lane 3 and its
    /// `try_next_fill` is always `None`; HYPARB L3's arm hands its
    /// swap receipts and its account's hedge fills out here instead —
    /// the engine books both kinds through the same `on_fill_booked`
    /// hook either way.
    #[inline]
    fn try_next_fill(&mut self) -> Option<Fill> {
        match self.paper.try_next_fill() {
            Some(f) => Some(f),
            None => self.live.try_next_fill(),
        }
    }

    /// Both arms, summed (plan §0.1-3). Cold: the 5 s tick only.
    fn stats(&self) -> DispatchStats {
        self.paper.stats().merged(self.live.stats())
    }

    /// Every tick reaches the paper matcher, **always and ungated**.
    ///
    /// Even with slot 3 live, the other seven slots' paper orders
    /// still need judging against the book. Gating this on "is any
    /// slot live" would silently freeze the paper matcher for every
    /// member that is still modelling.
    ///
    /// The live arm sees every tick too (HYPARB L3 marks its account at
    /// the hedge books' mids). The Hyperliquid arm's hook is a no-op.
    #[inline]
    fn observe_tick(&mut self, tick: &Tick, now_ns: NsTs) {
        self.paper.observe_tick(tick, now_ns);
        self.live.observe_tick(tick, now_ns);
    }

    /// HYPARB H2: every pool event reaches the paper matcher, ungated,
    /// for the same reason every tick does.
    #[inline]
    fn observe_amm(&mut self, sym: SymbolId, payload: &[u8; 40], now_ns: NsTs) {
        self.paper.observe_amm(sym, payload, now_ns);
    }

    /// XMM XH2: every trade print reaches the paper matcher, ungated,
    /// for the same reason every tick does — the queue law's fills come
    /// from prints. The live arm learns its fills from the venue.
    #[inline]
    fn observe_trade(&mut self, print: &core_types::TradePrint, now_ns: NsTs) {
        self.paper.observe_trade(print, now_ns);
    }

    /// XMM XH2: the paper arm's order events first, then the live arm's
    /// (the XH4 gateway's) — the `try_next_fill` order.
    #[inline]
    fn try_next_order_event(&mut self, out: &mut core_types::OrderEvent) -> bool {
        self.paper.try_next_order_event(out) || self.live.try_next_order_event(out)
    }

    /// XMM XH2: the queue law's instruments are the paper arm's to track.
    #[inline]
    fn track_queue_sym(&mut self, sym: SymbolId) {
        self.paper.track_queue_sym(sym);
    }

    /// LAW E-2 — the paper arm's numbers, never the live arm's.
    // COPY: 176 B by value, the metrics cadence only (the trait's note).
    #[inline]
    fn matcher_counters(&self) -> MatcherCounters {
        self.paper.matcher_counters()
    }

    /// LAW E-2 — likewise.
    #[inline]
    fn open_paper_orders(&self) -> usize {
        self.paper.open_paper_orders()
    }

    /// E1: hand the router's counters and the live route map across the
    /// trait boundary, so the engine loop can mirror them to `/metrics`
    /// without knowing this type. **Cold** — the 5 s tick only.
    /// E4: both arms get the idle moment.
    ///
    /// `|` and NOT `||`: short-circuiting would starve the second arm
    /// every time the first reported work, and the second arm is the
    /// one that owns a venue socket. A hook that runs only when the
    /// other arm is quiet is a hook that stops running exactly when
    /// the engine is busiest.
    ///
    /// This forwarding is why the hook exists at all — `RoutedDispatcher`
    /// is what the `--exec` path wires, so a default `false` here would
    /// leave a live arm's user-event socket unpumped and its budget
    /// state file unwritten, which is the "valid, empty and silent"
    /// failure the user-event module names as the worst one.
    #[inline]
    fn on_idle(&mut self) -> bool {
        let a = self.paper.on_idle();
        let b = self.live.on_idle();

        // HYPARB L5: orders the arm accepted that ended without a
        // (further) fill leave the resting count — a reverted swap is
        // not working anywhere, and counting it would stall the slot
        // at `max_open_orders`. Bounded by what the arm queued.
        while let Some((oid, slot)) = self.live.try_next_retired() {
            self.ledger.on_cancel(oid, slot as usize);
        }

        // **E6 commit 3 — the halt machine runs HERE, not on the
        // dispatch path.**
        //
        // A dead venue is exactly the condition under which a member
        // stops submitting, so a halt evaluated only on submit would
        // fire last or never. Commit 3a gave this hook a thread on
        // the `--exec` path, so the triggers are polled every 2 ms
        // whether or not anything is trading.
        //
        // HYPARB L4: each live slot is judged against the signal of the
        // arm that trades it (`OrderDispatch::halt_signal_for`), and is
        // seeded by that arm's reconciler. With one arm the signal is
        // the same for every slot, exactly as before.
        //
        // **S7-L1 (gap A)** — the venue's own count of each live slot's
        // day spend, adopted BEFORE the per-slot seeding below. The arm reports
        // `reconciled` only once it has read it, so the first live
        // place a boot allows is judged against the day the venue says
        // was spent, never against the zero a fresh ledger reads.
        let mut s = 0usize;
        while s < EXEC_SLOTS {
            let here = s;
            s += 1;
            if !matches!(self.route.mode_at(here), Some(ExecMode::Live)) {
                continue;
            }
            if let Some((day, bought_1e6)) = self.live.venue_day_bought(here) {
                self.ledger.adopt_venue_day_turnover(here, day, bought_1e6);
            }
        }

        // ONE edge per poll, however many slots latch in it — the file
        // names every halted slot and the cancel is venue-wide, so N
        // `write_atomic` calls (N allocations, N fsyncs on the engine
        // thread) for one incident would be N−1 too many. `latch_all`
        // already did it this way; the trigger loop caught up (E7
        // review, 2026-09-19).
        let mut latched = 0u32;
        let mut slot = 0usize;
        while slot < EXEC_SLOTS {
            let here = slot;
            slot += 1;
            if !matches!(self.route.mode_at(here), Some(ExecMode::Live)) {
                continue;
            }
            let Some(lim) = self.route.halts_at(here) else {
                continue;
            };
            let sig = self.live.halt_signal_for(here as u8);
            // The reconciler has compared this slot's arm against the
            // venue, so the ledger's numbers mean something. Until this,
            // every live PLACE for the slot is refused — see
            // `Ledger::mark_seeded`.
            if sig.reconciled != 0 && !self.ledger.is_slot_seeded(here) {
                self.ledger.mark_slot_seeded(here);
            }
            let why = trigger_for(&sig, &lim);
            if self.halt.latch(here, why) {
                latched += 1;
            }
        }
        if latched > 0 {
            self.on_halt_edge();
        }

        // The operator's file, after the triggers: a slot the
        // operator halted and a slot a trigger halted are the same
        // state, and a trigger firing in the same poll should keep
        // its more specific reason.
        self.poll_halt_file();

        // The venue was not cleared on the edge. Retried here rather
        // than once, because a halted slot with live orders is the
        // state LAW E-8's machinery exists to avoid, and one failed
        // attempt during a blip would leave it there for ever. Runs
        // AFTER the file poll, so a halt adopted from the file gets
        // its cancel in the same poll rather than the next.

        self.try_cancel_all();
        a | b
    }

    /// Both arms, unconditionally. No short-circuit subtlety here —
    /// this returns nothing, so there is no `|` vs `||` trap the way
    /// there is in `on_idle`; the only requirement is that the LIVE
    /// arm is never skipped, because it is the one that binds.
    #[inline]
    /// **E6** — the arms first, then the ledger.
    ///
    /// The arms keep the ordering they have always had (a roll must
    /// reach the live arm before the member that will quote into the
    /// new instance). The ledger goes last because nothing it does is
    /// visible to either arm, and putting it first would make a
    /// refusal in the binding table look like it came from the venue
    /// path.
    fn on_venue_event(&mut self, event: &core_types::ChannelEvent) {
        self.paper.on_venue_event(event);
        self.live.on_venue_event(event);

        if event.channel != core_types::ChannelId::InstrumentRoll as u8 {
            return;
        }
        // LAW E-4: bound by the roll, never derived. `core_types` owns
        // the one copy of this layout — the codec the ingress writes
        // with, the live arm reads with and the harness replays with.
        //
        // **The STRICT reading of the kind byte**, not
        // `unpack_roll_seq`'s low-bit mask. This is new code with no
        // prior behaviour to preserve, so it has no reason to inherit
        // the permissive reading the older call sites are stuck with.
        // On a byte no packer of ours writes, the masking reading
        // calls `0x03` "settled" — the ledger would zero the row and
        // drop its resting orders — while `strategy_bin15` reads the
        // whole byte and calls the same frame "created" and goes on
        // quoting. Split brain with the risk ledger on the blind side,
        // and every subsequent fill landing as `fills_unbound`.
        // Refused instead.
        let (outcome, _twap_s, family, _settled) =
            core_types::unpack_roll_seq(event.venue_seq);
        match core_types::roll_kind(event.venue_seq) {
            core_types::ROLL_KIND_CREATED => {
                self.ledger.bind(event.venue, family, outcome, event.sym)
            }
            core_types::ROLL_KIND_SETTLED => self.ledger.settle(event.venue, family, outcome),
            _ => self.ledger.refuse_roll(),
        }
    }

    /// **E6** — book the fill into the ledger the clamps read.
    ///
    /// Neither arm is forwarded to, and that is deliberate rather than
    /// an omission. The fill came OUT of one of the two arms, so
    /// handing it back is an echo; handing it ACROSS is LAW E-2's
    /// exact prohibition — the paper matcher must never be shown a
    /// venue fill, or its counters start describing something the
    /// venue did.
    #[inline]
    fn on_fill_booked(&mut self, fill: &Fill) {
        self.ledger.book_fill(fill);
    }

    fn exec_counters(&self) -> ExecCounters {
        let c = &self.counters;
        let mut modes = [0u8; clob_dispatcher::EXEC_COUNTER_SLOTS];
        for (slot, m) in modes.iter_mut().enumerate() {
            *m = self
                .route
                .mode_at(slot)
                .unwrap_or(ExecMode::Paper)
                .as_u8();
        }
        let mut halted = [0u8; clob_dispatcher::EXEC_COUNTER_SLOTS];
        for (slot, h) in halted.iter_mut().enumerate() {
            *h = self.halt.reason(slot) as u8;
        }
        let l = self.ledger.counters();
        // COPY: ExecCounters (568 B by repr(C) layout, const-asserted —
        // the 272 B LiveArmCounters ride inside) returned by value across
        // the OrderDispatch boundary, 1/s for /state + 1/5 s for /metrics
        // (+ once at the drain) —
        // it is COMPOSED here from the router, the halt state, the
        // ledger and the live arm, so there is nothing to borrow —
        // rejected: an out-param, which trades the copy for a second
        // borrow of the same `&mut self` the publish path holds.
        ExecCounters {
            configured: 1,
            modes,
            live_submits: c.live_submits,
            paper_submits: c.paper_submits,
            refused_off: c.refused_off,
            refused_no_route: c.refused_no_route,
            refused_risk: c.refused_risk,
            cancel_on_off: c.cancel_on_off,
            refused_halted: c.refused_halted,
            refused_unseeded: c.refused_unseeded,
            halts: self.halt.halts,
            cancel_all_failures: self.halt.cancel_all_failures,
            cancel_all_stranded: self.halt.cancel_all_stranded,
            halt_file_adopted: u8::try_from(self.halt_file_adopted).unwrap_or(u8::MAX),
            halt_file_present: u8::from(self.halt_file_present),
            seeded: u8::from(self.ledger.is_seeded()),
            halted,
            live_submits_by_slot: c.live_submits_by_slot,
            refused_by_slot: c.refused_by_slot,
            ledger_fills_unbound: l.fills_unbound,
            ledger_sells_below_zero: l.sells_below_zero,
            ledger_binds_refused: l.binds_refused,
            ledger_resting_full: l.resting_full,
            ledger_resting_ambiguous: l.resting_ambiguous,
            ledger_settles_unmatched: l.settles_unmatched,
            arm: self.live.arm_counters(),
        }
    }

    #[inline]
    fn arm_counters(&self) -> clob_dispatcher::LiveArmCounters {
        self.live.arm_counters()
    }

    /// **S7-L1** — both arms; only the live one has anything resting
    /// outside its own memory.
    fn on_shutdown(&mut self) {
        self.paper.on_shutdown();
        self.live.on_shutdown();
    }

    /// **S7-L1** — the live arm's answer; the paper arm has no venue
    /// history to report.
    #[inline]
    fn venue_day_bought(&self, slot: usize) -> Option<(u64, i64)> {
        self.live.venue_day_bought(slot)
    }
}

#[cfg(test)]
mod tests {
    use crate::route::{HaltLimits, SlotCaps};
    use super::*;

    /// The two crates each name their own slot count (the dependency
    /// runs one way, so neither can import the other's). If they ever
    /// disagree, the per-slot arrays crossing the trait boundary would
    /// silently truncate.
    #[test]
    fn the_slot_counts_of_the_two_crates_agree() {
        assert_eq!(crate::route::EXEC_SLOTS, clob_dispatcher::EXEC_COUNTER_SLOTS);
    }
    use crate::null::NullLiveDispatcher;
    use clob_dispatcher::PaperDispatcher;
    use core_types::{Price, Qty, Side, VenueId, STRATEGY_ID_NONE, STRATEGY_SLOT_BIN15};

    /// A live arm that ACCEPTS, so the tests can tell "routed to live"
    /// apart from "refused". Records what it saw.
    #[derive(Debug, Default)]
    struct SpyLive {
        seen: Vec<u64>,
        cancelled: Vec<u64>,
        modified: Vec<(u64, u64)>,
    }

    impl OrderDispatch for SpyLive {
        fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
            self.seen.push(order.client_oid);
            Ok(())
        }
        fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
            self.cancelled.push(req.client_oid);
            Ok(())
        }
        fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
            self.modified.push((req.prev_client_oid(), req.order().client_oid));
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> DispatchStats {
            DispatchStats {
                accepted: self.seen.len() as u64,
                ..DispatchStats::default()
            }
        }
    }

    /// The anchor the tests build a router with: the identity, so a
    /// fixture's monotonic order stamp and its wall fill stamp are the
    /// same number and land in one day epoch.
    ///
    /// **Production cannot take that shortcut** — the two clocks are
    /// decades apart — which is exactly why an identity anchor must
    /// not be the only one any test uses.
    /// `ledger::tests::the_two_clocks_do_not_thrash_the_day_epoch`
    /// builds a realistic one and is what actually holds the
    /// conversion.
    fn test_anchor() -> core_time::WallAnchor {
        core_time::WallAnchor::new(T0, T0)
    }

    fn order(slot: u8, venue: VenueId, oid: u64) -> Order {
        let mut o = Order::new(
            T0,
            venue,
            42,
            Side::Bid,
            0,
            Price::from_raw(500_000),
            Qty::from_raw(1_000_000),
            oid,
        );
        o.strategy_id = slot;
        o
    }

    /// The same, at an explicit price and size — the risk gate's
    /// whole input.
    fn order_px_qty(slot: u8, oid: u64, px: i64, qty: i64) -> Order {
        let mut o = Order::new(
            T0,
            VenueId::Hyperliquid,
            42,
            Side::Bid,
            0,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        );
        o.strategy_id = slot;
        o
    }

    fn bin15_live_table() -> ExecRoute {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            // The shipped `exec.toml` template's own numbers. A
            // fixture with zero caps would be refused by E6's clamps
            // before it reached whatever the test is about — and `0`
            // means UNSET, which `core_config::exec` refuses at boot.
            SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64),
            HaltLimits::none(),
        )
        .unwrap();
        r
    }

    #[test]
    fn a_live_slot_reaches_the_live_arm_and_no_other_slot_does() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok());
        for slot in [0u8, 1, 2, 4, 5, 6, 7] {
            assert!(d.submit(&order(slot, VenueId::Hyperliquid, 100 + slot as u64)).is_ok());
        }
        assert_eq!(d.live().seen, vec![1], "only slot 3 went live");
        assert_eq!(d.counters().live_submits, 1);
        assert_eq!(d.counters().paper_submits, 7);
        assert_eq!(d.counters().live_submits_at(3), Some(1));
    }

    /// LAW E-1, the load-bearing test.
    #[test]
    fn a_live_slot_on_a_wrong_venue_is_refused_and_never_falls_back_to_paper() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        let before = d.paper().open_paper_orders();
        // Slot 3 is live for Hyperliquid only; Binance has no route.
        let r = d.submit(&order(3, VenueId::Binance, 9));
        assert_eq!(r, Err(DispatchError::NoLiveRoute));
        assert!(d.live().seen.is_empty(), "never reached the live arm");
        assert_eq!(
            d.paper().open_paper_orders(),
            before,
            "LAW E-1: the paper matcher must not have taken it either"
        );
        assert_eq!(d.counters().refused_no_route, 1);
        assert_eq!(d.counters().paper_submits, 0);
    }

    #[test]
    fn an_off_slot_is_refused_by_both_arms() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(6, ExecMode::Off, &[], SlotCaps::none(), HaltLimits::none()).unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default(), test_anchor());
        d.mark_ledger_seeded();
        assert_eq!(
            d.submit(&order(6, VenueId::Hyperliquid, 5)),
            Err(DispatchError::SlotDisabled)
        );
        assert!(d.live().seen.is_empty());
        assert_eq!(d.paper().open_paper_orders(), 0);
        assert_eq!(d.counters().refused_off, 1);
        assert_eq!(d.counters().refused_at(6), Some(1));
    }

    #[test]
    fn an_unstamped_order_goes_to_paper_even_under_a_live_table() {
        // STRATEGY_ID_NONE == 0xFF; 0xFF & 7 == 7. Arm slot 7 live to
        // make the aliasing maximally tempting.
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            7,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            SlotCaps::none(),
            HaltLimits::none(),
        )
            .unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default(), test_anchor());
        d.mark_ledger_seeded();
        assert!(d
            .submit(&order(STRATEGY_ID_NONE, VenueId::Hyperliquid, 11))
            .is_ok());
        assert!(d.live().seen.is_empty(), "un-stamped must never go live");
        assert_eq!(d.counters().paper_submits, 1);
    }

    /// The E1 acceptance property: with an all-paper table the router
    /// is indistinguishable from the bare paper dispatcher on every
    /// observable the trait exposes.
    #[test]
    fn an_all_paper_table_is_indistinguishable_from_a_bare_paper_dispatcher() {
        let mut bare = PaperDispatcher::new();
        let mut routed = RoutedDispatcher::new(
            ExecRoute::all_paper(),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
            test_anchor(),
        );
        routed.mark_ledger_seeded();

        // A scripted stream over every slot, both venues, both sides.
        let mut oid = 0u64;
        for round in 0..64u64 {
            for slot in [0u8, 1, 2, 3, 4, 5, 6, 7, STRATEGY_ID_NONE] {
                oid += 1;
                let venue = if round % 2 == 0 {
                    VenueId::Hyperliquid
                } else {
                    VenueId::Polymarket
                };
                let o = order(slot, venue, oid);
                assert_eq!(
                    bare.submit(&o),
                    routed.submit(&o),
                    "submit disagreed at oid {oid}"
                );
            }
            let t = Tick::new(
                1_000 + round,
                VenueId::Hyperliquid,
                42,
                round as u32,
                Price::from_raw(499_000),
                Qty::from_raw(1_000_000),
                Price::from_raw(501_000),
                Qty::from_raw(1_000_000),
            );
            bare.observe_tick(&t, 1_000 + round);
            routed.observe_tick(&t, 1_000 + round);

            assert_eq!(
                bare.try_next_fill().map(|f| (f.order_id, f.origin)),
                routed.try_next_fill().map(|f| (f.order_id, f.origin)),
                "fill stream diverged at round {round}"
            );
            assert_eq!(
                bare.matcher_counters(),
                routed.matcher_counters(),
                "matcher counters diverged at round {round}"
            );
            assert_eq!(
                bare.open_paper_orders(),
                routed.open_paper_orders(),
                "open orders diverged at round {round}"
            );
        }

        // Stats: the null live arm contributes nothing at all, so the
        // merged total is the bare total field for field.
        let b = bare.stats();
        let r = routed.stats();
        assert_eq!(b.accepted, r.accepted);
        assert_eq!(b.rejected, r.rejected);
        assert_eq!(b.rejected_queue_full, r.rejected_queue_full);
        assert_eq!(b.rejected_routing, r.rejected_routing);
        assert_eq!(b.fills_seen, r.fills_seen);
        assert_eq!(routed.counters().live_submits, 0);
        assert_eq!(routed.counters().refused_off, 0);
        assert_eq!(routed.counters().refused_no_route, 0);
    }

    // ---------------- E5: routing the lifecycle verbs ----------------

    fn cancel_of(o: &Order) -> CancelReq {
        CancelReq::of(o, 2_000)
    }

    /// **LAW E-1 for a cancel, the load-bearing test.**
    ///
    /// A mis-routed submit invents a fill. A mis-routed cancel
    /// invents the ABSENCE of one: the paper matcher would remove a
    /// modelled order and report success while the real quote stays
    /// resting at the venue, and nothing downstream can detect the
    /// difference.
    #[test]
    fn a_live_slots_cancel_on_a_wrong_venue_never_reaches_the_paper_matcher() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Slot 3 is live for Hyperliquid only. Park a paper order on
        // the matcher so there is a book for a fall-through to reach.
        let decoy = order(0, VenueId::Polymarket, 9);
        assert!(d.submit(&decoy).is_ok());
        assert_eq!(d.paper().open_paper_orders(), 1);

        let mut c = cancel_of(&order(3, VenueId::Binance, 9));
        c.strategy_id = 3;
        assert_eq!(d.cancel(&c), Err(DispatchError::NoLiveRoute));
        assert!(d.live().cancelled.is_empty(), "never reached the live arm");
        assert_eq!(
            d.paper().open_paper_orders(),
            1,
            "LAW E-1: the paper matcher must not have taken the cancel either"
        );
        // The open count alone would NOT prove that: a fall-through
        // would have been refused by the matcher's own lookup and
        // left the count at 1 anyway. These are what prove the
        // matcher was never asked — every path through
        // `PaperMatcher::cancel` moves exactly one of them.
        let mc = d.paper().matcher_counters();
        assert_eq!(mc.cancels, 0, "the matcher performed no cancel");
        assert_eq!(mc.no_such_order, 0, "the matcher was never even asked");
        assert_eq!(mc.identity_mismatch, 0);
        assert_eq!(mc.ambiguous_order, 0);
        assert_eq!(d.counters().refused_no_route, 1);
    }

    #[test]
    fn a_live_slots_cancel_and_modify_reach_the_live_arm_and_no_other_slots_do() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        let live = order(3, VenueId::Hyperliquid, 1);
        let mut c = cancel_of(&live);
        c.strategy_id = 3;
        assert_eq!(d.cancel(&c), Ok(()));
        assert_eq!(d.live().cancelled, vec![1]);

        let mut repl = order(3, VenueId::Hyperliquid, 2);
        repl.strategy_id = 3;
        assert_eq!(d.modify(&ModifyReq::new(1, repl)), Ok(()));
        assert_eq!(d.live().modified, vec![(1, 2)]);

        // A paper slot's verbs must not touch the live arm.
        let paper = order(0, VenueId::Polymarket, 5);
        assert!(d.submit(&paper).is_ok());
        assert_eq!(d.cancel(&cancel_of(&paper)), Ok(()));
        assert_eq!(d.live().cancelled, vec![1], "still only the live one");
        assert_eq!(d.paper().matcher_counters().cancels, 1);
    }

    /// `off` stops PLACING. A modify is a place; a cancel is not, and
    /// it is the one path that can take back an order a previous boot
    /// left at the venue — so it reaches the live arm on the slot's
    /// venue mask (E7 review ruling, 2026-09-19).
    #[test]
    fn an_off_slot_refuses_a_modify_and_lets_a_cancel_through_to_the_arm() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            6,
            ExecMode::Off,
            &[VenueId::Hyperliquid.to_u8()],
            SlotCaps::none(),
            HaltLimits::none(),
        )
        .unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default(), test_anchor());
        d.mark_ledger_seeded();
        let o = order(6, VenueId::Hyperliquid, 5);
        assert_eq!(
            d.modify(&ModifyReq::new(4, o)),
            Err(DispatchError::SlotDisabled)
        );
        assert!(d.live().modified.is_empty());
        assert_eq!(d.cancel(&cancel_of(&o)), Ok(()));
        assert_eq!(d.live().cancelled, vec![5], "the cancel reached the arm");
        assert_eq!(d.paper().matcher_counters().cancels, 0, "never the paper matcher");
        assert_eq!(d.counters().refused_off, 1, "the modify");
        assert_eq!(d.counters().cancel_on_off, 1, "the cancel, counted not refused");
        assert_eq!(d.exec_counters().cancel_on_off, 1);
    }

    /// An `off` slot whose artifact names NO venue has nowhere to send
    /// a cancel. Refused, as before — never routed to paper.
    #[test]
    fn an_off_slot_with_no_venue_still_refuses_a_cancel() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(6, ExecMode::Off, &[], SlotCaps::none(), HaltLimits::none()).unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default(), test_anchor());
        d.mark_ledger_seeded();
        let o = order(6, VenueId::Hyperliquid, 5);
        assert_eq!(d.cancel(&cancel_of(&o)), Err(DispatchError::SlotDisabled));
        assert!(d.live().cancelled.is_empty());
        assert_eq!(d.paper().matcher_counters().cancels, 0);
        assert_eq!(d.counters().refused_off, 1);
        assert_eq!(d.counters().cancel_on_off, 0);
    }

    /// The stub live arm must refuse a lifecycle verb exactly as it
    /// refuses a submit. `Ok` here would be a quote the strategy
    /// stops tracking and the venue never had.
    #[test]
    fn the_null_live_arm_refuses_a_lifecycle_verb_rather_than_swallowing_it() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        let o = order(3, VenueId::Hyperliquid, 1);
        let mut c = cancel_of(&o);
        c.strategy_id = 3;
        assert_eq!(d.cancel(&c), Err(DispatchError::NoLiveRoute));
        assert_eq!(d.modify(&ModifyReq::new(1, o)), Err(DispatchError::NoLiveRoute));
        assert_eq!(d.live().refused(), 2);
        assert_eq!(d.paper().open_paper_orders(), 0);
    }

    // ---------------- E6: the risk gate's per-order clamp ----------

    /// **The clamp, and the thing it is for.** `bin15_live_table` sets
    /// `max_order_usd = $100`; an order for more is refused BEFORE the
    /// live arm sees it, so nothing reaches the venue.
    ///
    /// bin15 sizes against its own caps, so this should never fire —
    /// which is exactly why it is counted. A non-zero `refused_risk`
    /// means the member's ledger and the operator's number disagreed.
    #[test]
    fn an_order_over_the_slots_cap_never_reaches_the_live_arm() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // $100.50: 201 contracts at 0.50.
        let over = order_px_qty(3, 1, 500_000, 201_000_000);
        assert_eq!(d.submit(&over), Err(DispatchError::RiskRefused));
        assert!(d.live().seen.is_empty(), "the venue must never see it");
        assert_eq!(
            d.paper().open_paper_orders(),
            0,
            "and LAW E-1 still holds — a refused live order is not modelled"
        );
        assert_eq!(d.counters().refused_risk, 1);
        assert_eq!(d.counters().refused_at(3), Some(1));
        assert_eq!(d.counters().live_submits, 0);
    }

    /// The boundary is `>`, not `>=`: an order exactly AT the cap is
    /// what an operator who wrote that number asked for.
    #[test]
    fn an_order_exactly_at_the_cap_is_allowed() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Exactly $100.00.
        assert_eq!(d.submit(&order_px_qty(3, 7, 500_000, 200_000_000)), Ok(()));
        assert_eq!(d.live().seen, vec![7]);
        assert_eq!(d.counters().refused_risk, 0);
    }

    /// **A modify can RAISE size**, so a clamp on `submit` alone
    /// leaves the cap reachable by repricing upward — the hole the E5
    /// commit-4b review named. The replacement is measured exactly as
    /// a fresh order is.
    #[test]
    fn a_modify_that_raises_the_order_past_the_cap_is_refused() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // A legal order first, so the modify is the only thing on
        // trial.
        assert_eq!(d.submit(&order_px_qty(3, 1, 500_000, 100_000_000)), Ok(()));
        let bigger = order_px_qty(3, 2, 500_000, 400_000_000); // $200
        assert_eq!(
            d.modify(&ModifyReq::new(1, bigger)),
            Err(DispatchError::RiskRefused)
        );
        assert!(
            d.live().modified.is_empty(),
            "the venue must never be asked to grow it past the cap"
        );
        assert_eq!(d.counters().refused_risk, 1);
    }

    /// A PAPER slot is modelling, and the offline harness replays the
    /// same intents through no such gate. Refusing them here would
    /// make the two disagree for a reason that has nothing to do with
    /// the strategy.
    #[test]
    fn the_clamp_does_not_touch_a_paper_slot() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Slot 0 is paper; the same size that slot 3 was refused for.
        assert_eq!(d.submit(&order_px_qty(0, 1, 500_000, 201_000_000)), Ok(()));
        assert_eq!(d.counters().refused_risk, 0);
        assert_eq!(d.counters().paper_submits, 1);
    }

    /// **The overflow the `i128` is for.** `px × qty` leaves `i64`
    /// at about 9.2e18 — a $4 m price and three contracts reaches it
    /// — and a wrapped product is NEGATIVE, which sails straight past
    /// a `>` test. Saturating to `i64::MAX` would be no better: it is
    /// positive, but it is a number nobody computed.
    #[test]
    fn a_notional_that_would_overflow_i64_is_refused_not_wrapped() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // 4e12 x 3e6: the i64 product wraps negative.
        let absurd = order_px_qty(3, 1, 4_000_000_000_000, 3_000_000);
        assert!(
            (4_000_000_000_000i64).checked_mul(3_000_000).is_none(),
            "the premise: this product does not fit i64"
        );
        assert_eq!(d.submit(&absurd), Err(DispatchError::RiskRefused));
        assert!(d.live().seen.is_empty());
    }

    #[test]
    fn law_e2_matcher_numbers_are_the_paper_arms_even_with_a_live_slot() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Three live orders; the matcher must stay at zero intake.
        for i in 0..3 {
            assert!(d.submit(&order(3, VenueId::Hyperliquid, i)).is_ok());
        }
        assert_eq!(d.matcher_counters().intake, 0, "LAW E-2");
        assert_eq!(d.open_paper_orders(), 0, "LAW E-2");
        assert_eq!(d.live().seen.len(), 3);
    }

    #[test]
    fn exec_counters_cross_the_trait_boundary_intact() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok());
        assert!(d.submit(&order(0, VenueId::Polymarket, 2)).is_ok());
        assert_eq!(
            d.submit(&order(3, VenueId::Binance, 3)),
            Err(DispatchError::NoLiveRoute)
        );
        let e = d.exec_counters();
        assert_eq!(e.configured, 1);
        assert_eq!(e.live_submits, 1);
        assert_eq!(e.paper_submits, 1);
        assert_eq!(e.refused_no_route, 1);
        assert_eq!(e.refused_off, 0);
        assert_eq!(e.live_submits_by_slot[3], 1);
        assert_eq!(e.refused_by_slot[3], 1);
        assert_eq!(e.modes[3], ExecMode::Live.as_u8());
        assert_eq!(e.modes[0], ExecMode::Paper.as_u8());
    }

    /// A plain paper dispatcher reports NO router — this is what the
    /// cli reads at boot to decide whether `/metrics` grows the
    /// `engine_exec_*` family at all.
    #[test]
    fn a_bare_paper_dispatcher_reports_no_router() {
        let d = PaperDispatcher::new();
        assert_eq!(d.exec_counters().configured, 0);
        assert_eq!(d.exec_counters(), Default::default());
    }

    #[test]
    fn stats_sum_both_arms() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok()); // live
        assert!(d.submit(&order(0, VenueId::Polymarket, 2)).is_ok()); // paper
        let s = d.stats();
        assert_eq!(s.accepted, d.paper().stats().accepted + 1);
    }

    #[test]
    fn the_null_live_arm_makes_a_stray_live_order_a_refusal_not_a_paper_fill() {
        // Belt and braces: even if config validation were bypassed and
        // a Live slot pointed at the stub, nothing gets modelled.
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        assert_eq!(
            d.submit(&order(3, VenueId::Hyperliquid, 1)),
            Err(DispatchError::NoLiveRoute)
        );
        assert_eq!(d.paper().open_paper_orders(), 0);
        assert_eq!(d.live().refused(), 1);
    }

    // -----------------------------------------------------------------
    // E6 commit 2 — the three ledger-fed clamps
    // -----------------------------------------------------------------

    const OUTCOME: u32 = 20_182;
    /// Hyperliquid namespace, ordinal 900 — the Yes leg; 901 is No.
    const SYM_YES: core_types::SymbolId = (4u32 << 24) | 900;
    const SYM_NO: core_types::SymbolId = (4u32 << 24) | 901;
    /// 2026-09-19T00:00:01Z.
    const T0: u64 = 1_789_776_001_000_000_000;

    /// A live slot 3 with the caps the test names, so each clamp can
    /// be driven to its edge without the other three firing first.
    fn table_with(caps: SlotCaps) -> ExecRoute {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            caps,
            HaltLimits::none(),
        )
        .unwrap();
        r
    }

    fn leg_order(oid: u64, sym: core_types::SymbolId, buy: bool, px: i64, qty: i64) -> Order {
        let mut o = Order::new(
            T0,
            VenueId::Hyperliquid,
            42,
            if buy { Side::Bid } else { Side::Ask },
            0,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        );
        o.strategy_id = STRATEGY_SLOT_BIN15;
        o.sym = sym;
        o
    }

    fn roll(outcome: u32, sym_yes: core_types::SymbolId, settled: bool) -> core_types::ChannelEvent {
        core_types::ChannelEvent::new(
            T0,
            VenueId::Hyperliquid,
            core_types::ChannelId::InstrumentRoll,
            sym_yes,
            core_types::pack_roll_seq(outcome, 60, 0, settled),
            0,
            0,
            0,
        )
    }

    fn venue_fill(sym: core_types::SymbolId, buy: bool, px: i64, qty: i64, oid: u64) -> Fill {
        Fill::new(
            T0,
            sym,
            if buy { Side::Bid } else { Side::Ask },
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        )
        .with_attribution(STRATEGY_SLOT_BIN15, core_types::FILL_ORIGIN_VENUE)
    }

    fn armed(caps: SlotCaps) -> RoutedDispatcher<PaperDispatcher, SpyLive> {
        let mut d = RoutedDispatcher::new(
            table_with(caps),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        // What E6 commit 3's reconciler will do at boot. Without it
        // every live PLACE is refused — see `mark_ledger_seeded`.
        d.mark_ledger_seeded();
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));
        d
    }

    /// **An unreconciled ledger refuses, it does not guess.**
    ///
    /// A fresh `Ledger` reads zero exposure, zero turnover and zero
    /// resting orders. After a RESTART that is not the truth — the
    /// venue still holds whatever the last boot left — and all three
    /// ledger-fed clamps would fail OPEN: a whole `cap_instance`
    /// addable on top of an existing position, a fresh `cap_day` on
    /// top of the day's real spend, and `max_open_orders` more orders
    /// on top of the ones already working.
    #[test]
    fn a_live_place_is_refused_until_the_ledger_has_been_reconciled() {
        let mut d = RoutedDispatcher::new(
            table_with(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64)),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));
        assert!(!d.ledger().is_seeded());
        assert_eq!(
            d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert!(d.live().seen.is_empty(), "it never reached the venue");
        assert_eq!(d.counters().refused_unseeded, 1);
        assert_eq!(
            d.counters().refused_cap_instance,
            0,
            "an unseeded refusal must not read as a cap_instance breach"
        );

        // **A MODIFY IS REFUSED TOO.** An earlier cut exempted it,
        // reasoning that a quote would otherwise be stranded. That
        // was false — a modify RAISES price and size, which is the
        // one thing this interlock exists to stop, and it would have
        // been judged against a ledger reading zero.
        let m = core_types::ModifyReq::new(1, leg_order(2, SYM_YES, true, 510_000, 1_000_000));
        assert_eq!(d.modify(&m), Err(DispatchError::RiskRefused));

        // A CANCEL is the escape hatch, and it always was: it is
        // never risk-checked at all, so nothing is ever stranded.
        let c = core_types::CancelReq::of(&leg_order(1, SYM_YES, true, 500_000, 1_000_000), T0);
        assert!(d.cancel(&c).is_ok());

        d.mark_ledger_seeded();
        assert!(d.submit(&leg_order(3, SYM_YES, true, 500_000, 1_000_000)).is_ok());
    }

    #[test]
    fn a_malformed_roll_kind_byte_is_refused_rather_than_read_as_settled() {
        // New code, so it takes the STRICT reading. Under
        // `unpack_roll_seq`'s low-bit mask a `0x03` byte reads as
        // "settled" — the ledger would zero the row and drop its
        // resting orders — while `strategy_bin15` reads the whole byte
        // and calls the same frame "created" and goes on quoting.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 4_000_000);

        let mut ev = roll(OUTCOME, SYM_YES, false);
        ev.venue_seq |= 0x03u64 << 56;
        let refused_before = d.ledger().counters().binds_refused;
        d.on_venue_event(&ev);
        assert_eq!(d.ledger().counters().binds_refused, refused_before + 1);
        assert_eq!(
            d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize),
            4_000_000,
            "a malformed frame must not clear the position"
        );
    }

    #[test]
    fn a_roll_reaches_the_ledger_through_the_event_the_arms_already_get() {
        // LAW E-4 — the binding comes from the roll, and the router
        // gets the same event the live arm binds from. No cross-arm
        // reach, and one codec (`core_types`) for both readings.
        let d = armed(SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64));
        assert_eq!(d.ledger().counters().binds, 1);
        assert_eq!(d.ledger().position_1e6(STRATEGY_SLOT_BIN15 as usize, OUTCOME), Some((0, 0)));
    }

    #[test]
    fn a_settle_and_then_its_successor_clear_the_instance_through_the_same_path() {
        let mut d = armed(SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 4_000_000);

        // The settle keeps the position — the contracts are held
        // until the settlement pays out — and drops what was resting.
        d.on_venue_event(&roll(OUTCOME, SYM_YES, true));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 4_000_000);

        // The successor's CREATED roll is what retires the instance,
        // and it carries a new outcome id on the same family.
        d.on_venue_event(&roll(OUTCOME + 1, SYM_YES + 2, false));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 0);
    }

    #[test]
    fn the_instance_cap_refuses_the_order_that_would_breach_it() {
        // cap_instance $6. Four contracts of Yes are at stake ($4);
        // three more would be $7.
        let mut d = armed(SlotCaps::new(100_000_000, 6_000_000, 30_000_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        assert_eq!(
            d.submit(&leg_order(2, SYM_YES, true, 500_000, 3_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_instance, 1);
        assert_eq!(d.counters().refused_risk, 1);
        assert!(d.live().seen.is_empty(), "it never reached the venue");
        // Two more WOULD fit, exactly at the cap: the boundary is `>`.
        assert!(d.submit(&leg_order(3, SYM_YES, true, 500_000, 2_000_000)).is_ok());
        assert_eq!(d.counters().refused_cap_instance, 1);
    }

    #[test]
    fn the_instance_cap_never_refuses_an_order_that_does_not_raise_exposure() {
        // The property that keeps a cap from trapping a member inside
        // the exposure it is trying to leave, asserted from the WORST
        // position: already over the cap, where the only way out is an
        // order. A clamp testing `projected > cap` alone refuses both
        // of these and leaves no path back but an operator cancelling
        // by hand. (It did, until this test said so.)
        let mut d = armed(SlotCaps::new(100_000_000, 1_000_000, 30_000_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 9_000_000, 1));
        assert!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize) > 1_000_000);
        // Selling the long leg.
        assert!(d.submit(&leg_order(2, SYM_YES, false, 500_000, 5_000_000)).is_ok());
        // Buying the SHORT leg — also risk-reducing.
        assert!(d.submit(&leg_order(3, SYM_NO, true, 500_000, 5_000_000)).is_ok());
        assert_eq!(d.counters().refused_cap_instance, 0);
        // And the trap door stays shut: from over the cap, an order
        // that RAISES exposure is still refused.
        assert_eq!(
            d.submit(&leg_order(4, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_instance, 1);
    }

    #[test]
    fn the_instance_cap_sees_a_slots_first_order() {
        // Without the projection a flat slot passes any exposure test,
        // so the cap would start biting only on the SECOND order —
        // one order too late, and `max_order_usd` is the only thing
        // that would have stopped the first.
        let mut d = armed(SlotCaps::new(i64::MAX, 1_000_000, 30_000_000_000, 64));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 0);
        assert_eq!(
            d.submit(&leg_order(1, SYM_YES, true, 500_000, 9_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_instance, 1);
    }

    #[test]
    fn the_day_cap_counts_what_was_bought_and_refuses_the_next_buy() {
        // cap_day $3. One fill of 4 contracts at $0.50 is $2.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, 3_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        assert_eq!(d.ledger().slot_day_turnover_1e6(STRATEGY_SLOT_BIN15 as usize), 2_000_000);
        // $1.50 more would be $3.50.
        assert_eq!(
            d.submit(&leg_order(2, SYM_YES, true, 500_000, 3_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_day, 1);
        // Exactly $1 more lands on the cap, and `>` lets it through.
        assert!(d.submit(&leg_order(3, SYM_YES, true, 500_000, 2_000_000)).is_ok());
    }

    #[test]
    fn the_day_cap_never_refuses_a_sell() {
        // A sell adds no turnover. Testing one would refuse the order
        // that closes a position on a day whose cap is already spent.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, 1_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 900_000, 9_000_000, 1));
        assert!(d.ledger().slot_day_turnover_1e6(STRATEGY_SLOT_BIN15 as usize) > 1_000_000);
        assert!(d.submit(&leg_order(2, SYM_YES, false, 900_000, 9_000_000)).is_ok());
        assert_eq!(d.counters().refused_cap_day, 0);
    }

    /// **L1 — an exit is never refused by the order caps.** A sell no
    /// larger than the slot's holding on that leg passes `max_order`
    /// and `max_open_orders` however large it is; one share more than
    /// the holding is an ordinary order and is clamped again.
    #[test]
    fn an_exit_is_never_refused_by_the_order_caps() {
        // max_order $2, one open order at most.
        let mut d = armed(SlotCaps::new(2_000_000, i64::MAX, i64::MAX, 1));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 200_000, 10_000_000, 1));
        assert_eq!(d.ledger().held_on_sym_1e6(STRATEGY_SLOT_BIN15 as usize, SYM_YES), 10_000_000);
        // A resting buy fills the one open slot.
        assert!(d.submit(&leg_order(2, SYM_YES, true, 100_000, 1_000_000)).is_ok());
        // Selling all 10 at $0.60 is $6 — three times `max_order`, with
        // the open-order cap full — and passes: it is the exit.
        assert!(d.submit(&leg_order(3, SYM_YES, false, 600_000, 10_000_000)).is_ok());
        // Eleven is more than is held: an ordinary order, clamped.
        assert_eq!(
            d.submit(&leg_order(4, SYM_YES, false, 600_000, 11_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_max_order, 1);
        // A sell on a leg the slot does not hold is not an exit either.
        assert_eq!(d.ledger().held_on_sym_1e6(STRATEGY_SLOT_BIN15 as usize, SYM_NO), 0);
    }

    #[test]
    fn the_open_order_cap_counts_what_the_arm_accepted_and_nothing_else() {
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 2));
        assert!(d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000)).is_ok());
        assert!(d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)).is_ok());
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 2);
        assert_eq!(
            d.submit(&leg_order(3, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_open_orders, 1);
        // A cancel frees a place.
        // `CancelReq::of` copies the identity fields off the order,
        // so the test cannot get them wrong in a way the production
        // path could not.
        let c = core_types::CancelReq::of(&leg_order(1, SYM_YES, true, 500_000, 1_000_000), T0);
        assert!(d.cancel(&c).is_ok());
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 1);
        assert!(d.submit(&leg_order(4, SYM_YES, true, 500_000, 1_000_000)).is_ok());
    }

    #[test]
    fn a_modify_is_exempt_from_the_open_order_cap() {
        // LAW E-7: a requote replaces in place, so a modify cannot be
        // the order that takes the count over. Testing it would refuse
        // the requote of a slot sitting exactly at its cap — the slot
        // that most needs to move its quotes.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 1));
        assert!(d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000)).is_ok());
        let m = core_types::ModifyReq::new(1, leg_order(2, SYM_YES, true, 510_000, 1_000_000));
        assert!(d.modify(&m).is_ok());
        assert_eq!(d.counters().refused_open_orders, 0);
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 1);
        // And the replacement's id is what a fill now matches.
        d.on_fill_booked(&venue_fill(SYM_YES, true, 510_000, 1_000_000, 2));
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 0);
    }

    #[test]
    fn a_refused_submit_does_not_count_against_the_open_order_cap() {
        // Only acceptance counts. An order the arm refused is not
        // working at the venue, and counting it would leak the count
        // upward until the clamp refused a slot holding nothing.
        let mut d = RoutedDispatcher::new(
            table_with(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 4)),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));
        for oid in 1..=6u64 {
            assert!(d.submit(&leg_order(oid, SYM_YES, true, 500_000, 1_000_000)).is_err());
        }
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 0);
        assert_eq!(d.counters().refused_open_orders, 0, "no clamp ever fired");
    }

    #[test]
    fn only_the_first_clamp_to_fire_is_counted() {
        // All four would refuse this order. The order stops at the
        // first, so no second clamp is ever breached, and counting a
        // "would also have failed" would inflate the rate an operator
        // reads.
        let mut d = armed(SlotCaps::new(1, 1, 1, 0));
        assert_eq!(
            d.submit(&leg_order(1, SYM_YES, true, 900_000, 9_000_000)),
            Err(DispatchError::RiskRefused)
        );
        let c = d.counters();
        assert_eq!(c.refused_risk, 1);
        assert_eq!(c.refused_max_order, 1);
        assert_eq!(c.refused_cap_instance, 0);
        assert_eq!(c.refused_cap_day, 0);
        assert_eq!(c.refused_open_orders, 0);
        assert_eq!(
            c.refused_max_order + c.refused_cap_instance + c.refused_cap_day + c.refused_open_orders,
            c.refused_risk,
            "the breakdown sums to the aggregate E6 commit 1 shipped"
        );
    }

    #[test]
    fn the_clamps_are_live_arm_only() {
        // A paper slot is modelling. Refusing its orders would make
        // the model disagree with the harness — which replays the same
        // intents through no such gate — for a reason that has nothing
        // to do with the strategy.
        let mut d = RoutedDispatcher::new(
            ExecRoute::all_paper(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Every cap is 0 on an all-paper table, which on the live arm
        // refuses everything.
        let mut o = leg_order(1, SYM_YES, true, 900_000, 9_000_000);
        o.strategy_id = 0;
        assert!(d.submit(&o).is_ok());
        assert_eq!(d.counters().refused_risk, 0);
    }

    #[test]
    fn a_paper_fill_does_not_move_a_live_slots_ledger() {
        // LAW E-2's shape, applied to the ledger: the paper matcher's
        // modelled trades put no money at risk and must not consume a
        // live slot's caps.
        let mut d = armed(SlotCaps::new(100_000_000, 1_000_000, 30_000_000_000, 64));
        let f = venue_fill(SYM_YES, true, 900_000, 9_000_000, 1)
            .with_attribution(STRATEGY_SLOT_BIN15, core_types::FILL_ORIGIN_PAPER);
        d.on_fill_booked(&f);
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 0);
        assert!(d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)).is_ok());
    }

    #[test]
    fn a_venue_fill_reaching_the_router_is_what_makes_the_next_order_refusable() {
        // The whole commit in one test. Before the hook existed the
        // router had no way to learn a live fill had happened —
        // `try_next_fill` carries the paper arm only and
        // `on_venue_event` carries market data — so this second order
        // would have passed.
        let mut d = armed(SlotCaps::new(100_000_000, 5_000_000, 30_000_000_000, 64));
        assert!(d.submit(&leg_order(1, SYM_YES, true, 500_000, 5_000_000)).is_ok());
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 5_000_000, 1));
        assert_eq!(
            d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_instance, 1);
    }

    #[test]
    fn the_day_cap_rolls_on_the_orders_own_clock() {
        // A boot that fills nothing after midnight must not judge the
        // new day's first order against yesterday's turnover.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, 2_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        let mut over = leg_order(2, SYM_YES, true, 500_000, 1_000_000);
        assert_eq!(d.submit(&over), Err(DispatchError::RiskRefused));
        over.ts_ns = T0 + 24 * 3_600_000_000_000;
        over.client_oid = 3;
        assert!(d.submit(&over).is_ok(), "a new day, and no fill rolled it");
        assert_eq!(d.ledger().counters().day_rollovers, 1);
    }

    // -----------------------------------------------------------------
    // E6 commit 3 — the sticky halt, end to end
    // -----------------------------------------------------------------

    /// A live arm whose halt signal the test drives, and which
    /// records every `cancel_all`.
    #[derive(Debug, Default)]
    struct SpyHalt {
        seen: Vec<u64>,
        cancelled: Vec<u64>,
        modified: Vec<(u64, u64)>,
        sig: core_types_halt::HaltSignal,
        cancel_all_calls: usize,
        /// The next N requests are refused.
        cancel_all_fails: usize,
        /// **How many idle moments a sweep takes before the venue
        /// confirms.** `0` models a synchronous arm; anything higher
        /// models the real one, which queues a sweep and drains it.
        sweep_polls: usize,
        /// Counts down in `on_idle`, exactly as the real arm's sweep
        /// table drains there.
        sweeping: usize,
        /// The last request did not land, so the arm has stopped and
        /// the venue was never confirmed clear.
        stranded: bool,
        /// S7-L1: what the venue says the bin15 slot bought today.
        day_bought: Option<(u64, i64)>,
        /// Orders the arm says ended without a fill (HYPARB L5).
        retired: Vec<(u64, u8)>,
    }

    /// `clob_dispatcher::HaltSignal` under a short name, so the test
    /// fixtures read as what they are.
    mod core_types_halt {
        pub use clob_dispatcher::HaltSignal;
    }

    impl OrderDispatch for SpyHalt {
        fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
            self.seen.push(order.client_oid);
            Ok(())
        }
        fn try_next_retired(&mut self) -> Option<(u64, u8)> {
            self.retired.pop()
        }
        fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
            self.cancelled.push(req.client_oid);
            Ok(())
        }
        fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
            self.modified
                .push((req.prev_client_oid(), req.order().client_oid));
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> DispatchStats {
            DispatchStats::default()
        }
        fn halt_signal(&self) -> clob_dispatcher::HaltSignal {
            self.sig
        }
        fn venue_day_bought(&self, slot: usize) -> Option<(u64, i64)> {
            if slot == STRATEGY_SLOT_BIN15 as usize {
                self.day_bought
            } else {
                None
            }
        }
        fn cancel_all(&mut self) -> Result<(), DispatchError> {
            self.cancel_all_calls += 1;
            if self.cancel_all_fails > 0 {
                self.cancel_all_fails -= 1;
                // A refused request leaves the arm stopped, not
                // clear. Answering `Clear` here is exactly the
                // fail-open the split exists to prevent.
                self.stranded = true;
                return Err(DispatchError::Disconnected);
            }
            self.stranded = false;
            self.sweeping = self.sweep_polls;
            Ok(())
        }
        fn cancel_all_state(&self) -> clob_dispatcher::CancelAllState {
            if self.sweeping > 0 {
                clob_dispatcher::CancelAllState::Working
            } else if self.stranded {
                clob_dispatcher::CancelAllState::Stranded
            } else {
                clob_dispatcher::CancelAllState::Clear
            }
        }
        fn on_idle(&mut self) -> bool {
            // The real arm drains its sweep table here too.
            if self.sweeping > 0 {
                self.sweeping -= 1;
            }
            false
        }
    }

    /// The thresholds every halt test uses.
    fn halt_limits() -> crate::route::HaltLimits {
        crate::route::HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000)
    }

    /// A live slot 3 with real caps AND real halt thresholds, plus a
    /// spy arm whose signal the test drives.
    fn haltable() -> RoutedDispatcher<PaperDispatcher, SpyHalt> {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64),
            halt_limits(),
        )
        .unwrap();
        let mut d = RoutedDispatcher::new(
            r,
            PaperDispatcher::new(),
            SpyHalt::default(),
            test_anchor(),
        );
        // A healthy, reconciled arm to start from.
        d.live_mut().sig = clob_dispatcher::HaltSignal::new(1_000_000, 0, 0, 0, false, true, 1_000_000);
        d.on_idle();
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));
        d
    }

    /// Drive one trigger and assert the whole contract: it halts, it
    /// cancels the venue, it refuses a submit AND a modify, and it
    /// still lets a cancel through.
    fn assert_trigger(
        set: impl FnOnce(&mut clob_dispatcher::HaltSignal),
        expect: crate::halt::HaltReason,
    ) {
        let mut d = haltable();
        let slot = STRATEGY_SLOT_BIN15 as usize;
        assert!(!d.halt().is_halted(slot), "healthy to start");
        assert!(d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000)).is_ok());
        let cancels_before = d.live().cancel_all_calls;

        set(&mut d.live_mut().sig);
        d.on_idle();

        assert!(d.halt().is_halted(slot), "{expect:?} did not halt");
        assert_eq!(d.halt().reason(slot), expect);
        assert_eq!(
            d.live().cancel_all_calls,
            cancels_before + 1,
            "{expect:?} halted without clearing the venue"
        );
        // The ledger's resting count went with it — those orders were
        // cancelled, and leaving them counted would have
        // `max_open_orders` refuse a slot holding nothing.
        assert_eq!(d.ledger().slot_resting(slot), 0);

        // Refused: a submit and a modify.
        assert_eq!(
            d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        let m = core_types::ModifyReq::new(1, leg_order(3, SYM_YES, true, 510_000, 1_000_000));
        assert_eq!(d.modify(&m), Err(DispatchError::RiskRefused));
        assert!(d.counters().refused_halted >= 2);

        // Allowed: a cancel. The only way to get flat, and it never
        // goes through the risk gate at all.
        let c = core_types::CancelReq::of(&leg_order(1, SYM_YES, true, 500_000, 1_000_000), T0);
        assert!(d.cancel(&c).is_ok(), "a halted slot must be able to flatten");
    }

    #[test]
    fn a_reject_streak_halts_cancels_and_refuses() {
        assert_trigger(|s| s.reject_streak = 5, crate::halt::HaltReason::RejectStreak);
    }

    #[test]
    fn a_breached_budget_floor_halts_cancels_and_refuses() {
        assert_trigger(
            |s| s.budget_floor_breached = 1,
            crate::halt::HaltReason::BudgetFloor,
        );
    }

    #[test]
    fn reconciliation_drift_halts_cancels_and_refuses() {
        assert_trigger(
            |s| s.recon_drift_usd_1e6 = 5_000_000,
            crate::halt::HaltReason::ReconDrift,
        );
    }

    #[test]
    fn a_user_stream_gap_halts_cancels_and_refuses() {
        assert_trigger(
            |s| s.ws_gap_ns = 30_000_000_000,
            crate::halt::HaltReason::WsGap,
        );
    }

    #[test]
    fn an_asset_refusal_streak_halts_cancels_and_refuses() {
        assert_trigger(
            |s| s.asset_refusal_streak = 3,
            crate::halt::HaltReason::AssetRefusals,
        );
    }

    /// **The day cap is NOT a halt trigger**, and the plan listed it
    /// as one. Reaching it is the clamp working, and it clears itself
    /// at 00:00Z — halting would stop the engine for good every day
    /// it traded to its cap.
    #[test]
    fn spending_the_day_cap_refuses_but_does_not_halt() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            SlotCaps::new(100_000_000, i64::MAX, 1_000_000, 64),
            halt_limits(),
        )
        .unwrap();
        let mut d = RoutedDispatcher::new(
            r,
            PaperDispatcher::new(),
            SpyHalt::default(),
            test_anchor(),
        );
        d.live_mut().sig = clob_dispatcher::HaltSignal::new(1_000_000, 0, 0, 0, false, true, 1_000_000);
        d.on_idle();
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));

        let slot = STRATEGY_SLOT_BIN15 as usize;
        d.on_fill_booked(&venue_fill(SYM_YES, true, 900_000, 9_000_000, 1));
        assert!(d.ledger().slot_day_turnover_1e6(slot) > 1_000_000);
        assert_eq!(
            d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_day, 1);
        d.on_idle();
        assert!(!d.halt().is_halted(slot), "a spent day cap is not a halt");
    }

    /// The halt machine runs on the IDLE path, so a dead venue halts
    /// even when the member has stopped submitting — which is exactly
    /// what a member does when the venue is dead.
    #[test]
    fn a_dead_venue_halts_with_no_order_flow_at_all() {
        let mut d = haltable();
        let slot = STRATEGY_SLOT_BIN15 as usize;
        d.live_mut().sig.ws_gap_ns = 30_000_000_000;
        d.on_idle();
        assert!(d.halt().is_halted(slot));
        assert!(d.live().seen.is_empty(), "nothing was ever submitted");
    }

    #[test]
    fn a_failed_cancel_all_halts_anyway_and_is_retried() {
        // Waiting for a successful cancel before refusing would keep
        // submitting into the condition that tripped the halt.
        let mut d = haltable();
        let slot = STRATEGY_SLOT_BIN15 as usize;
        d.live_mut().cancel_all_fails = 2;
        d.live_mut().sig.reject_streak = 5;

        d.on_idle();
        assert!(d.halt().is_halted(slot), "halted despite the failed cancel");
        assert!(d.halt().cancel_outstanding(), "the venue still holds them");
        assert_eq!(d.halt().cancel_all_failures, 1);

        // Retried from the idle path until it lands.
        d.on_idle();
        assert!(d.halt().cancel_outstanding());
        assert_eq!(d.halt().cancel_all_failures, 2);
        d.on_idle();
        assert!(!d.halt().cancel_outstanding(), "and it landed");
        assert_eq!(d.live().cancel_all_calls, 3);
    }

    #[test]
    fn a_halt_edge_fires_cancel_all_once_not_once_per_poll() {
        let mut d = haltable();
        d.live_mut().sig.reject_streak = 5;
        d.on_idle();
        let after_edge = d.live().cancel_all_calls;
        for _ in 0..50 {
            d.on_idle();
        }
        assert_eq!(
            d.live().cancel_all_calls,
            after_edge,
            "the venue was cleared, so nothing should be retried"
        );
        assert_eq!(d.halt().halts, 1, "one incident, one edge");
    }

    #[test]
    fn a_paper_slot_is_untouched_by_a_live_slots_halt() {
        let mut d = haltable();
        d.live_mut().sig.reject_streak = 5;
        d.on_idle();
        assert!(d.halt().is_halted(STRATEGY_SLOT_BIN15 as usize));
        // Slot 0 is paper and keeps trading.
        assert!(!d.halt().is_halted(0));
        assert!(d.submit(&order(0, VenueId::Polymarket, 99)).is_ok());
    }

    /// **S7-L1 (gap A) — a restart's day cap is the venue's from the
    /// first live place.** The arm reports the day's spend on the poll
    /// that seeds the ledger, and the router adopts it first: a boot
    /// that finds $2.50 already bought today refuses the buy that would
    /// cross a $3 cap — the buy a fresh ledger reading zero would have
    /// let through.
    #[test]
    fn a_restart_adopts_the_venues_day_spend_before_its_first_live_place() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            SlotCaps::new(100_000_000, i64::MAX, 3_000_000, 64),
            halt_limits(),
        )
        .unwrap();
        let mut d = RoutedDispatcher::new(
            r,
            PaperDispatcher::new(),
            SpyHalt::default(),
            test_anchor(),
        );
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));
        let day = T0 / 86_400_000_000_000;
        d.live_mut().sig = clob_dispatcher::HaltSignal::new(1_000_000, 0, 0, 0, false, true, 0);
        d.live_mut().day_bought = Some((day, 2_500_000));
        d.on_idle();
        assert!(d.ledger().is_seeded());
        assert_eq!(d.ledger().slot_day_turnover_1e6(STRATEGY_SLOT_BIN15 as usize), 2_500_000);
        // $1 more would be $3.50 against the $3 cap.
        assert_eq!(
            d.submit(&leg_order(1, SYM_YES, true, 500_000, 2_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_day, 1);
        // $0.50 lands on it.
        assert!(d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)).is_ok());
    }

    #[test]
    fn the_reconciler_seeds_the_ledger_through_the_same_poll() {
        // The deferred half of commit 2: nothing called
        // `mark_ledger_seeded`, so a live slot refused every order for
        // ever. The arm reports having reconciled on the same signal
        // the halt triggers ride.
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64),
            halt_limits(),
        )
        .unwrap();
        let mut d = RoutedDispatcher::new(
            r,
            PaperDispatcher::new(),
            SpyHalt::default(),
            test_anchor(),
        );
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));

        // An arm that has not reconciled: every live place refused.
        d.live_mut().sig = clob_dispatcher::HaltSignal::new(1_000_000, 0, 0, 0, false, false, 0);
        d.on_idle();
        assert!(!d.ledger().is_seeded());
        assert_eq!(
            d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_unseeded, 1);

        // It reconciles, and the gate opens.
        d.live_mut().sig.reconciled = 1;
        d.on_idle();
        assert!(d.ledger().is_seeded());
        assert!(d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)).is_ok());
    }

    #[test]
    fn an_operator_halt_is_exactly_as_sticky_as_a_triggered_one() {
        let mut d = haltable();
        let slot = STRATEGY_SLOT_BIN15 as usize;
        d.halt_slot(slot, crate::halt::HaltReason::Operator);
        assert!(d.halt().is_halted(slot));
        assert_eq!(d.halt().reason(slot), crate::halt::HaltReason::Operator);
        assert_eq!(d.live().cancel_all_calls, 1, "it cancels just as hard");
        for _ in 0..100 {
            d.on_idle();
        }
        assert!(d.halt().is_halted(slot), "and nothing clears it");
    }

    /// A directory of this test's own. Counted, not clocked — see
    /// `cli::exec_boot::tmp` for the flaky gate that taught this.
    fn halt_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "e6-halt-{}-{}",
            std::process::id(),
            core_types::fnv1a_64(tag.as_bytes())
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("tmp dir");
        d
    }

    /// **The restart hole, closed.** A halt the last run wrote is a
    /// halt this run starts with — otherwise the 00:10Z restart
    /// resumes trading into whatever tripped it, unattended.
    #[test]
    fn a_halt_file_left_by_the_last_run_halts_the_slot_at_boot() {
        let dir = halt_dir("adopt_at_boot");
        let path = dir.join("exec.HALT");
        std::fs::write(&path, "slot=3 reason=recon-drift\n").unwrap();

        let mut d = haltable();
        d.set_halt_path(path.clone());
        assert_eq!(d.adopt_halt_file(), 1, "one slot adopted");
        assert_eq!(d.halt_file_adopted(), 1);

        let slot = STRATEGY_SLOT_BIN15 as usize;
        assert!(d.halt().is_halted(slot));
        assert_eq!(
            d.halt().reason(slot),
            crate::halt::HaltReason::ReconDrift,
            "and with the reason the LAST run recorded"
        );
        assert_eq!(
            d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused),
            "exactly as refusing as a triggered halt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_halt_file_adopts_nothing_and_is_the_normal_boot() {
        let dir = halt_dir("no_file");
        let mut d = haltable();
        d.set_halt_path(dir.join("exec.HALT"));
        assert_eq!(d.adopt_halt_file(), 0);
        assert!(!d.halt().any_halted());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt file must not be a denial of service on an engine
    /// that might be needed to flatten a position.
    #[test]
    fn a_corrupt_halt_file_is_not_a_boot_refusal() {
        let dir = halt_dir("corrupt");
        let path = dir.join("exec.HALT");
        // Not UTF-8: `read_to_string` itself fails.
        std::fs::write(&path, [0xffu8, 0xfe, 0xfd, 0x00]).unwrap();
        let mut d = haltable();
        d.set_halt_path(path);
        assert_eq!(d.adopt_halt_file(), 0, "nothing adopted, nothing thrown");
        assert!(!d.halt().any_halted());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A FIFO must not hang the engine thread.**
    ///
    /// `read_to_string`, which this replaces, blocks in `open` on a
    /// FIFO with no writer — for ever, with no timeout and no log, on
    /// the thread that pumps the live arm. A halt file we will not
    /// read halts nothing; a halt file we cannot stop reading stops
    /// everything.
    #[test]
    fn a_fifo_in_place_of_the_halt_file_does_not_hang_the_boot() {
        let dir = halt_dir("fifo");
        let path = dir.join("exec.HALT");
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: a path in this test's own fresh temp directory.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

        let mut d = haltable();
        d.set_halt_path(path);
        // If this ever blocks, the test hangs rather than fails —
        // which is exactly what the production bug did.
        assert_eq!(d.adopt_halt_file(), 0, "a FIFO halts nothing");
        assert!(!d.halt().any_halted());
        // But it is NOT silent (E7 review): something sits at the halt
        // path that the engine could not read, and a boot that said
        // nothing about it would be the silent-halt-file failure with
        // the sign flipped. Present + inert is what the boot tell
        // prints for it.
        assert!(d.halt_file_present(), "something is at the halt path, and the boot must say so");
        assert_eq!(d.halt_file_inert(), 1, "counted as an unreadable/inert read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory, a device, and a file too large to be one we wrote
    /// are all refused — and none of them halts anything.
    #[test]
    fn only_a_regular_file_of_a_sane_size_is_read() {
        let dir = halt_dir("not_a_file");

        // A directory where the file should be.
        let as_dir = dir.join("exec.HALT");
        std::fs::create_dir(&as_dir).unwrap();
        let mut d = haltable();
        d.set_halt_path(as_dir);
        assert_eq!(d.adopt_halt_file(), 0, "a directory is not a halt file");

        // A file larger than anything we would have written.
        let dir2 = halt_dir("too_big");
        let big = dir2.join("exec.HALT");
        let mut body = String::from("slot=3 reason=ws-gap\n");
        while body.len() <= crate::halt::HALT_FILE_MAX {
            body.push_str("# padding to push this past the cap\n");
        }
        std::fs::write(&big, &body).unwrap();
        let mut d2 = haltable();
        d2.set_halt_path(big);
        assert_eq!(
            d2.adopt_halt_file(),
            0,
            "a halt file bigger than one we could have written is refused, \
             not read into an unbounded String"
        );
        assert!(!d2.halt().any_halted());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// **A halt file that halts nothing must be visible.** A mistyped
    /// slot number, or a slot that is not live, is an operator who
    /// asked for a halt and did not get one.
    #[test]
    fn a_halt_file_that_halts_nothing_is_still_reported_as_present() {
        let dir = halt_dir("inert");
        let path = dir.join("exec.HALT");
        // Slot 1 is PAPER in `haltable()` — it cannot reach a venue,
        // so halting it would mean nothing except a venue-wide cancel.
        std::fs::write(&path, "slot=1 reason=operator\n").unwrap();

        let mut d = haltable();
        d.set_halt_path(path);
        assert_eq!(d.adopt_halt_file(), 0, "a paper slot is not halted");
        assert!(
            d.halt_file_present(),
            "but the boot must be able to say a file was there"
        );
        assert!(!d.halt().any_halted());
        assert_eq!(d.live().cancel_all_calls, 0, "and nothing was cancelled");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same at runtime, counted rather than logged.
    #[test]
    fn an_inert_runtime_read_is_counted() {
        let dir = halt_dir("inert_runtime");
        let path = dir.join("exec.HALT");
        let mut d = haltable();
        d.set_halt_path(path.clone());
        std::fs::write(&path, "slot=1 reason=operator\n").unwrap();
        d.force_halt_poll();
        d.on_idle();
        assert_eq!(d.halt_file_reads(), 1);
        assert_eq!(d.halt_file_inert(), 1, "read, and nothing came of it");
        assert!(!d.halt().any_halted());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The operator's runtime kill switch.** `echo 3 > exec.HALT`
    /// and the slot stops, with no restart and no control socket.
    #[test]
    fn an_operator_writing_the_file_halts_the_slot_from_the_idle_path() {
        let dir = halt_dir("runtime_poll");
        let path = dir.join("exec.HALT");
        let slot = STRATEGY_SLOT_BIN15 as usize;

        let mut d = haltable();
        d.set_halt_path(path.clone());
        d.on_idle();
        assert!(!d.halt().is_halted(slot), "healthy to start");

        // The shortest thing an operator would type.
        std::fs::write(&path, "3\n").unwrap();
        d.force_halt_poll();
        d.on_idle();

        assert!(d.halt().is_halted(slot), "halted from the file alone");
        assert_eq!(d.halt().reason(slot), crate::halt::HaltReason::Operator);
        assert!(d.live().cancel_all_calls > 0, "and it cancelled");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The cadence is what keeps this off the hot path.** Without
    /// it the engine thread would `stat` and read a file five hundred
    /// times a second to answer a question that changes when a human
    /// types.
    #[test]
    fn the_halt_file_is_polled_once_a_second_not_once_a_tick() {
        let dir = halt_dir("poll_cadence");
        let path = dir.join("exec.HALT");
        std::fs::write(&path, "# nothing halted\n").unwrap();

        let mut d = haltable();
        d.set_halt_path(path);
        for _ in 0..500 {
            d.on_idle();
        }
        assert_eq!(
            d.halt_file_polls(),
            1,
            "500 idle moments inside one second is ONE syscall — \
             without the cadence this is 500, and the READ count \
             below would not notice"
        );
        assert_eq!(
            d.halt_file_reads(),
            1,
            "and the one poll that ran did read, since nothing had \
             seen this file before"
        );

        // And the next second reads again, so a change is not missed.
        d.force_halt_poll();
        d.on_idle();
        assert_eq!(d.halt_file_polls(), 2, "the next second stats again");
        assert_eq!(d.halt_file_reads(), 1, "unchanged: stat only, no read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deleting the file un-halts nothing in the running process. A
    /// halt is sticky, and clearing one is a restart-level decision.
    #[test]
    fn deleting_the_halt_file_does_not_un_halt_a_running_engine() {
        let dir = halt_dir("delete_does_not_clear");
        let path = dir.join("exec.HALT");
        let slot = STRATEGY_SLOT_BIN15 as usize;

        let mut d = haltable();
        d.set_halt_path(path.clone());
        std::fs::write(&path, "slot=3 reason=ws-gap\n").unwrap();
        d.force_halt_poll();
        d.on_idle();
        assert!(d.halt().is_halted(slot));

        std::fs::remove_file(&path).unwrap();
        for _ in 0..50 {
            d.force_halt_poll();
            d.on_idle();
        }
        assert!(d.halt().is_halted(slot), "still halted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_halt_writes_the_file_that_survives_a_restart() {
        let dir = halt_dir("a_halt_writes_the_file");
        let path = dir.join("exec.HALT");

        let mut d = haltable();
        d.set_halt_path(path.clone());
        d.live_mut().sig.ws_gap_ns = 30_000_000_000;
        d.on_idle();

        let text = std::fs::read_to_string(&path).expect("exec.HALT must exist");
        assert!(text.contains("reason=ws-gap"), "{text}");
        assert!(
            text.contains(&format!("slot={STRATEGY_SLOT_BIN15}")),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_halt_path_means_no_file_and_no_refusal_to_halt() {
        // Best effort: a halt that could not be persisted is still a
        // halt. Refusing to halt because a file write failed would be
        // the wrong direction entirely.
        let mut d = haltable();
        d.live_mut().sig.ws_gap_ns = 30_000_000_000;
        d.on_idle();
        assert!(d.halt().is_halted(STRATEGY_SLOT_BIN15 as usize));
    }

    /// `cancel_all` is VENUE-WIDE. Two slots tripping on one poll is
    /// one incident at one venue, so it is one call — not one per
    /// halted slot, and not one per slot plus a retry.
    #[test]
    fn two_slots_halting_on_one_poll_share_one_cancel_all() {
        let mut r = ExecRoute::all_paper();
        let caps = SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64);
        let a = STRATEGY_SLOT_BIN15 as usize;
        let b = a + 1;
        r.set_slot(
            a,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            caps,
            halt_limits(),
        )
        .unwrap();
        r.set_slot(
            b,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            caps,
            halt_limits(),
        )
        .unwrap();
        let mut d = RoutedDispatcher::new(
            r,
            PaperDispatcher::new(),
            SpyHalt::default(),
            test_anchor(),
        );

        // One signal, read by both slots on the same poll.
        d.live_mut().sig = clob_dispatcher::HaltSignal::new(30_000_000_000, 0, 0, 0, false, true, 1_000_000);
        d.on_idle();

        assert!(d.halt().is_halted(a));
        assert!(d.halt().is_halted(b));
        assert_eq!(d.halt().halts, 2, "two slots, two halt edges");
        assert_eq!(
            d.live().cancel_all_calls,
            1,
            "but one venue, so one cancel-all"
        );
        assert!(!d.halt().cancel_outstanding());
    }

    /// **A queued sweep is not a cleared venue.**
    ///
    /// This is the defect the request/confirm split exists to
    /// prevent. `cancel_all` returning `Ok` means the arm accepted
    /// the request; the orders are still working until the venue says
    /// otherwise, and the router must not act as though they are gone.
    #[test]
    fn a_queued_sweep_is_not_a_cleared_venue() {
        let mut d = haltable();
        let slot = STRATEGY_SLOT_BIN15 as usize;
        d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000))
            .unwrap();
        assert_eq!(d.ledger().slot_resting(slot), 1);

        // An arm whose sweep takes three idle moments to drain.
        d.live_mut().sweep_polls = 3;
        d.live_mut().sig.reject_streak = 5;

        d.on_idle();
        assert!(d.halt().is_halted(slot), "halted on the edge");
        assert_eq!(d.live().cancel_all_calls, 1, "and asked, once");
        assert!(d.halt().cancel_outstanding());
        assert_eq!(
            d.ledger().slot_resting(slot),
            1,
            "the sweep is only QUEUED — the orders are still working"
        );

        // Draining. Still not clear, and not re-asked either: the
        // sweep already running is the retry.
        d.on_idle();
        assert!(d.halt().cancel_outstanding());
        assert_eq!(d.ledger().slot_resting(slot), 1);
        assert_eq!(d.live().cancel_all_calls, 1, "not asked again");

        d.on_idle();
        assert!(d.halt().cancel_outstanding());

        // The venue confirms.
        d.on_idle();
        assert!(!d.halt().cancel_outstanding(), "now it is clear");
        assert_eq!(d.ledger().slot_resting(slot), 0);
        assert_eq!(d.halt().cancel_all_failures, 0, "nothing ever failed");
        assert_eq!(d.halt().cancel_all_stranded, 0);
    }

    /// An abandoned sweep is asked again. Left alone it would be a
    /// stranded quote on a halted slot — the one state LAW E-8 exists
    /// to prevent.
    #[test]
    fn an_abandoned_sweep_is_asked_again_rather_than_read_as_clear() {
        let mut d = haltable();
        let slot = STRATEGY_SLOT_BIN15 as usize;
        d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000))
            .unwrap();
        // The first request lands and the arm starts sweeping.
        d.live_mut().sweep_polls = 4;
        d.live_mut().sig.reject_streak = 5;
        d.on_idle();
        assert_eq!(d.live().cancel_all_calls, 1);
        assert!(d.halt().cancel_outstanding(), "still sweeping");

        // The sweep then burns its retries and is dropped with orders
        // it never managed to cancel — `sweep_left` moves, the table
        // empties, and the arm reports `Stranded` rather than reading
        // its own empty table as a clear venue.
        d.live_mut().sweeping = 0;
        d.live_mut().stranded = true;
        // The retry, modelled as landing cleanly.
        d.live_mut().sweep_polls = 0;

        d.on_idle();
        assert_eq!(d.halt().cancel_all_stranded, 1, "counted as stranded");
        assert_eq!(
            d.halt().cancel_all_failures,
            0,
            "the request landed — that is a different number"
        );
        assert_eq!(d.live().cancel_all_calls, 2, "and asked again");
        assert!(!d.halt().cancel_outstanding(), "the fresh ask confirmed");
        assert_eq!(d.ledger().slot_resting(slot), 0);
    }

    /// A slot that halts while ANOTHER slot's sweep is draining gets
    /// its own request — its legs were not in that sweep.
    #[test]
    fn a_slot_halting_during_anothers_sweep_gets_its_own_request() {
        let mut r = ExecRoute::all_paper();
        let caps = SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64);
        let a = STRATEGY_SLOT_BIN15 as usize;
        let b = a + 1;
        for slot in [a, b] {
            r.set_slot(
                slot,
                ExecMode::Live,
                &[VenueId::Hyperliquid.to_u8()],
                caps,
                halt_limits(),
            )
            .unwrap();
        }
        let mut d = RoutedDispatcher::new(
            r,
            PaperDispatcher::new(),
            SpyHalt::default(),
            test_anchor(),
        );
        d.live_mut().sweep_polls = 8;

        d.halt_slot(a, crate::halt::HaltReason::Operator);
        assert_eq!(d.live().cancel_all_calls, 1);
        d.on_idle();
        assert_eq!(d.live().cancel_all_calls, 1, "a's sweep is draining");

        d.halt_slot(b, crate::halt::HaltReason::Operator);
        assert_eq!(
            d.live().cancel_all_calls,
            2,
            "b's legs were not in a's sweep"
        );
    }

    /// The resting count is only zeroed when the venue CONFIRMED it
    /// holds nothing. Zeroing it on a failed attempt would tell
    /// `max_open_orders` that a book still working at the venue is
    /// gone, and let a healthy slot stack a second one on top of it.
    #[test]
    fn a_failed_cancel_all_leaves_the_resting_count_alone() {
        let mut d = haltable();
        let slot = STRATEGY_SLOT_BIN15 as usize;
        d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000))
            .unwrap();
        assert_eq!(d.ledger().slot_resting(slot), 1);

        d.live_mut().cancel_all_fails = 1;
        d.live_mut().sig.reject_streak = 5;
        d.on_idle();

        assert!(d.halt().is_halted(slot));
        assert_eq!(d.halt().cancel_all_failures, 1);
        assert_eq!(
            d.ledger().slot_resting(slot),
            1,
            "the venue never said it let them go"
        );

        d.on_idle();
        assert!(!d.halt().cancel_outstanding());
        assert_eq!(d.ledger().slot_resting(slot), 0, "now it did");
    }

    // -----------------------------------------------------------------
    // HYPARB L4 — two live arms: slot 0 on its own, slot 3 on the other
    // -----------------------------------------------------------------

    /// Slot 3 live on Hyperliquid through arm `a`; slot 0 live on
    /// HyperEVM + Hyperliquid through arm `b`.
    fn two_arms() -> RoutedDispatcher<PaperDispatcher, crate::SlotSplit<SpyHalt, SpyHalt>> {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64),
            halt_limits(),
        )
        .unwrap();
        r.set_slot(
            0,
            ExecMode::Live,
            &[VenueId::HyperEvm.to_u8(), VenueId::Hyperliquid.to_u8()],
            SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64),
            halt_limits().with_pnl_bound(50_000_000, 20_000_000),
        )
        .unwrap();
        RoutedDispatcher::new(
            r,
            PaperDispatcher::new(),
            crate::SlotSplit::new(0, SpyHalt::default(), SpyHalt::default()),
            test_anchor(),
        )
    }

    fn healthy() -> clob_dispatcher::HaltSignal {
        clob_dispatcher::HaltSignal::new(1_000_000, 0, 0, 0, false, true, 1_000_000)
    }

    #[test]
    fn each_slot_is_seeded_by_its_own_arm() {
        let mut d = two_arms();
        d.live_mut().a_mut().sig = healthy();
        d.on_idle();
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok());
        assert_eq!(
            d.submit(&order(0, VenueId::HyperEvm, 2)),
            Err(DispatchError::RiskRefused),
            "slot 0's arm has not reconciled"
        );
        d.live_mut().b_mut().sig = healthy();
        d.on_idle();
        assert!(d.submit(&order(0, VenueId::HyperEvm, 3)).is_ok());
        assert_eq!(d.live().a().seen, [1]);
        assert_eq!(d.live().b().seen, [3]);
    }

    #[test]
    fn slot_0s_pnl_bound_halts_slot_0_and_never_slot_3() {
        let mut d = two_arms();
        d.live_mut().a_mut().sig = healthy();
        d.live_mut().b_mut().sig = healthy();
        d.on_idle();
        // Slot 0's arm reports a flat account $20.000001 below its
        // anchor; slot 3's arm is healthy.
        d.live_mut().b_mut().sig = healthy().with_pnl(true, -20_000_001);
        d.on_idle();
        assert!(d.halt().is_halted(0), "slot 0 tripped its loss bound");
        assert!(!d.halt().is_halted(STRATEGY_SLOT_BIN15 as usize));
        assert_eq!(
            d.submit(&order(0, VenueId::Hyperliquid, 4)),
            Err(DispatchError::RiskRefused)
        );
        assert!(
            d.submit(&order(3, VenueId::Hyperliquid, 5)).is_ok(),
            "slot 3 keeps trading"
        );
        // The venue-wide cancel reached both arms.
        assert_eq!(
            (d.live().a().cancel_all_calls, d.live().b().cancel_all_calls),
            (1, 1)
        );
        // Slot 3's arm reports a flat account too: slot 3 has no bound
        // configured, so it never trips on it.
        d.live_mut().a_mut().sig = healthy().with_pnl(true, -99_000_000);
        d.on_idle();
        assert!(!d.halt().is_halted(STRATEGY_SLOT_BIN15 as usize));
    }

    #[test]
    fn slot_0s_venues_are_its_own_and_slot_3_cannot_reach_hyperevm() {
        let mut d = two_arms();
        d.live_mut().a_mut().sig = healthy();
        d.live_mut().b_mut().sig = healthy();
        d.on_idle();
        assert_eq!(
            d.submit(&order(3, VenueId::HyperEvm, 6)),
            Err(DispatchError::NoLiveRoute)
        );
        assert!(d.submit(&order(0, VenueId::Hyperliquid, 7)).is_ok());
        assert_eq!(d.live().b().seen, [7]);
    }

    /// A reverted swap is not working anywhere: the arm retires it and
    /// the slot's resting count comes back down — without it, eight
    /// reverts would stall slot 0 at `max_open_orders` for the session.
    #[test]
    fn an_order_the_arm_retires_leaves_the_resting_count() {
        let mut d = two_arms();
        d.live_mut().a_mut().sig = healthy();
        d.live_mut().b_mut().sig = healthy();
        d.on_idle();
        assert!(d.submit(&order(0, VenueId::HyperEvm, 8)).is_ok());
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 9)).is_ok());
        assert_eq!((d.ledger().slot_resting(0), d.ledger().slot_resting(3)), (1, 1));
        d.live_mut().b_mut().retired.push((8, 0));
        d.on_idle();
        assert_eq!((d.ledger().slot_resting(0), d.ledger().slot_resting(3)), (0, 1));
        // An unknown or already-filled key is silent.
        d.live_mut().b_mut().retired.push((8, 0));
        d.on_idle();
        assert_eq!(d.ledger().slot_resting(3), 1);
    }
}
