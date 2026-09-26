// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # subs — request/subscription bookkeeping for WS venues
//!
//! Lifted out of `ingress-rpc` in Phase 8a (§3.4) so OKX / Deribit /
//! Hyperliquid clone the machinery instead of re-implementing it:
//!
//! * [`PendingTable`] — fixed-capacity table of in-flight requests,
//!   indexed by `id & (N-1)` (monotonic id allocators make collisions
//!   impossible while in-flight count ≤ N).
//! * [`SubTable`] — fixed-capacity `(SubId, kind)` rows mapping a
//!   venue subscription id to what it streams.
//! * [`queue_masked_binary_frame`] / [`queue_masked_text_frame`] and
//!   their `_parts` forms — the "serialize into tx IoBuf with a fresh
//!   mask" pattern every client-side WS writer needs; a `_parts` payload
//!   is masked into tx part by part, never assembled first.
//!
//! Everything is preallocated, `Copy`-only rows, zero-alloc, no
//! `dyn`: per-venue request kinds are monomorphized through the
//! [`ReqKind`] trait.
//!
//! The **resubscribe-on-`Steady` pattern** these tables support: on
//! every (re)entry to the steady state the run loop clears both
//! tables and queues its full subscribe batch again — subscriptions
//! are connection-scoped state and must never survive a reconnect.

use std::io;

use crate::iobuf::IoBuf;
use crate::ws_frame::{
    ws_mask_from_counter, ws_write_binary_frame_parts, ws_write_text_frame_parts,
};

// ---------------------------------------------------------------
// Request kinds
// ---------------------------------------------------------------

/// Per-venue request-kind tag stored in a [`PendingTable`] slot.
/// Implementors are tiny `#[repr(u8)]` enums with a designated
/// free-slot sentinel.
pub trait ReqKind: Copy + Eq {
    /// The "slot free" sentinel value.
    const FREE: Self;
}

// ---------------------------------------------------------------
// PendingTable
// ---------------------------------------------------------------

/// One in-flight request. `Copy`, no heap.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct PendingReq<K: ReqKind> {
    /// Allocated request id (JSON-RPC id, OKX op id, ...). `0` is
    /// reserved: allocators must start at 1.
    pub id: u64,
    /// Monotonic ns when the request was queued.
    pub created_at_ns: u64,
    /// Request shape.
    pub kind: K,
}

impl<K: ReqKind> PendingReq<K> {
    /// Free-slot value.
    #[inline(always)]
    pub fn empty() -> Self {
        Self {
            id: 0,
            created_at_ns: 0,
            kind: K::FREE,
        }
    }

    /// Whether the slot holds a live request.
    #[inline(always)]
    pub fn is_used(&self) -> bool {
        self.kind != K::FREE
    }
}

/// Why a [`PendingTable`] operation failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PendingErr {
    /// The `id & (N-1)` slot still holds an older in-flight request —
    /// more than `N` requests in flight, a protocol-driver bug.
    SlotBusy,
    /// `id` was zero (reserved) — allocators must start at 1.
    ZeroId,
}

/// Fixed-capacity in-flight request table indexed by `id & (N-1)`.
/// `N` must be a power of two (compile-time enforced).
pub struct PendingTable<K: ReqKind, const N: usize> {
    slots: [PendingReq<K>; N],
}

impl<K: ReqKind, const N: usize> PendingTable<K, N> {
    /// Empty table. Boot-time.
    pub fn new() -> Self {
        const {
            assert!(
                N.is_power_of_two() && N >= 2,
                "PendingTable N must be a power of two >= 2"
            );
        }
        Self {
            slots: [PendingReq::empty(); N],
        }
    }

    /// Record a freshly-queued request.
    #[inline]
    pub fn record(&mut self, id: u64, kind: K, now_ns: u64) -> Result<(), PendingErr> {
        if id == 0 {
            return Err(PendingErr::ZeroId);
        }
        let slot = &mut self.slots[(id as usize) & (N - 1)];
        if slot.is_used() {
            return Err(PendingErr::SlotBusy);
        }
        *slot = PendingReq {
            id,
            created_at_ns: now_ns,
            kind,
        };
        Ok(())
    }

    /// Whether `id` can be recorded now: non-zero and its
    /// `id & (N-1)` slot free. An allocator facing an endpoint that
    /// answers out of order skips a busy slot's id instead of colliding.
    #[inline]
    pub fn is_free(&self, id: u64) -> bool {
        id != 0 && !self.slots[(id as usize) & (N - 1)].is_used()
    }

    /// Take the request matching `id`, freeing its slot. `None` when
    /// the id is unknown (late/duplicate response — count, don't
    /// crash: venues do redeliver).
    #[inline]
    pub fn complete(&mut self, id: u64) -> Option<PendingReq<K>> {
        if id == 0 {
            return None;
        }
        let slot = &mut self.slots[(id as usize) & (N - 1)];
        if !slot.is_used() || slot.id != id {
            return None;
        }
        let out = *slot;
        *slot = PendingReq::empty();
        Some(out)
    }

    /// Live request count (O(N), N tiny — metrics/tests only).
    pub fn count(&self) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < N {
            if self.slots[i].is_used() {
                n += 1;
            }
            i += 1;
        }
        n
    }

    /// Free every slot (reconnect reset).
    pub fn clear(&mut self) {
        let mut i = 0;
        while i < N {
            self.slots[i] = PendingReq::empty();
            i += 1;
        }
    }

    /// Take the first request (by slot) that has waited `max_age_ns` or
    /// longer at `now_ns`, freeing its slot — an answer that never came.
    /// O(N); a timer's work, not an event's. `max_age_ns == 0` takes
    /// every request, one per call: the drain of a dead connection,
    /// whose requests are all in doubt.
    pub fn take_expired(&mut self, now_ns: u64, max_age_ns: u64) -> Option<PendingReq<K>> {
        let mut i = 0;
        while i < N {
            let s = self.slots[i];
            if s.is_used() && now_ns.saturating_sub(s.created_at_ns) >= max_age_ns {
                self.slots[i] = PendingReq::empty();
                return Some(s);
            }
            i += 1;
        }
        None
    }
}

impl<K: ReqKind, const N: usize> Default for PendingTable<K, N> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// ReqIds
// ---------------------------------------------------------------

/// Request ids for a request/response session over one connection (a
/// WebSocket API): monotonic from 1, each recorded in a
/// [`PendingTable`] until its answer. An id whose slot still holds an
/// older unanswered request is SKIPPED — never reused, never colliding
/// — so an endpoint that answers out of order costs gaps, not
/// confusion.
pub struct ReqIds<K: ReqKind, const N: usize> {
    next: u64,
    table: PendingTable<K, N>,
}

impl<K: ReqKind, const N: usize> ReqIds<K, N> {
    /// No request in flight; the first id is 1. Boot-time.
    pub fn new() -> Self {
        Self {
            next: 1,
            table: PendingTable::new(),
        }
    }

    /// The next id, recorded as `kind` at `now_ns`. `None` when all `N`
    /// slots are waiting for answers: the session's back-pressure.
    #[inline]
    pub fn issue(&mut self, kind: K, now_ns: u64) -> Option<u64> {
        let mut tries = 0;
        while tries < N {
            let id = self.next;
            // Never 0 (reserved), even after a wrap no session lives to see.
            self.next = self.next.wrapping_add(1).max(1);
            if self.table.is_free(id) {
                let r = self.table.record(id, kind, now_ns);
                debug_assert!(r.is_ok(), "a free slot refused its id");
                return Some(id);
            }
            tries += 1;
        }
        None
    }

    /// The answer to `id` arrived: its request, freed. `None` for an id
    /// not in flight (late, duplicate or foreign — count, don't crash).
    #[inline]
    pub fn answer(&mut self, id: u64) -> Option<PendingReq<K>> {
        self.table.complete(id)
    }

    /// [`PendingTable::take_expired`].
    #[inline]
    pub fn take_expired(&mut self, now_ns: u64, max_age_ns: u64) -> Option<PendingReq<K>> {
        self.table.take_expired(now_ns, max_age_ns)
    }

    /// Requests waiting for answers (O(N) — metrics/tests).
    pub fn in_flight(&self) -> usize {
        self.table.count()
    }
}

impl<K: ReqKind, const N: usize> Default for ReqIds<K, N> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// SubTable
// ---------------------------------------------------------------

/// Venue-assigned subscription id, normalized to a `u64` for O(1)
/// compare (Polygon: the 16-hex-digit id; venues with string channel
/// keys hash/index them at subscribe time). `0` = unused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct SubId(pub u64);

impl SubId {
    /// Sentinel "unused".
    pub const NONE: SubId = SubId(0);
}

/// Fixed-capacity subscription registry: rows of `(SubId, kind)`.
/// Linear scan — `N` is single-digits-to-tens everywhere we use it.
pub struct SubTable<K: ReqKind, const N: usize> {
    rows: [(SubId, K); N],
}

/// Why a [`SubTable::insert`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SubErr {
    /// All `N` rows in use — the configured channel set exceeds the
    /// table capacity (boot-time misconfiguration; fail fast).
    Full,
    /// `SubId::NONE` is reserved.
    ReservedId,
}

impl<K: ReqKind, const N: usize> SubTable<K, N> {
    /// Empty table.
    pub fn new() -> Self {
        Self {
            rows: [(SubId::NONE, K::FREE); N],
        }
    }

    /// Register `id → kind`.
    pub fn insert(&mut self, id: SubId, kind: K) -> Result<(), SubErr> {
        if id == SubId::NONE {
            return Err(SubErr::ReservedId);
        }
        let mut i = 0;
        while i < N {
            if self.rows[i].0 == SubId::NONE {
                self.rows[i] = (id, kind);
                return Ok(());
            }
            i += 1;
        }
        Err(SubErr::Full)
    }

    /// What `id` streams, if registered.
    #[inline]
    pub fn kind_of(&self, id: SubId) -> Option<K> {
        let mut i = 0;
        while i < N {
            if self.rows[i].0 == id {
                return Some(self.rows[i].1);
            }
            i += 1;
        }
        None
    }

    /// Live row count.
    pub fn count(&self) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < N {
            if self.rows[i].0 != SubId::NONE {
                n += 1;
            }
            i += 1;
        }
        n
    }

    /// Free every row (reconnect reset — subscriptions are
    /// connection-scoped).
    pub fn clear(&mut self) {
        let mut i = 0;
        while i < N {
            self.rows[i] = (SubId::NONE, K::FREE);
            i += 1;
        }
    }
}

impl<K: ReqKind, const N: usize> Default for SubTable<K, N> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Masked-frame queueing
// ---------------------------------------------------------------

/// Serialize `payload` as a masked client→server **binary** frame
/// into `tx`, advancing `mask_counter`. Zero-copy into the tx window;
/// zero-alloc.
#[inline]
pub fn queue_masked_binary_frame(
    tx: &mut IoBuf,
    mask_counter: &mut u64,
    payload: &[u8],
) -> io::Result<()> {
    queue_masked_binary_frame_parts(tx, mask_counter, &[payload])
}

/// [`queue_masked_binary_frame`] whose payload is the concatenation of
/// `parts`, each masked straight into the tx window — a fixed-shape
/// request is never assembled anywhere first (the RPC venues'
/// `eth_blockNumber` poll and `newHeads` subscribe).
#[inline]
pub fn queue_masked_binary_frame_parts(
    tx: &mut IoBuf,
    mask_counter: &mut u64,
    parts: &[&[u8]],
) -> io::Result<()> {
    let mask = ws_mask_from_counter(*mask_counter);
    *mask_counter = mask_counter.wrapping_add(1);
    let n = ws_write_binary_frame_parts(tx.free_mut(), parts, mask)
        .map_err(|_| io::Error::other("ws binary frame: tx buffer too small"))?;
    tx.advance(n);
    Ok(())
}

/// Text-frame counterpart of [`queue_masked_binary_frame`] (OKX and
/// Hyperliquid speak JSON text frames).
#[inline]
pub fn queue_masked_text_frame(
    tx: &mut IoBuf,
    mask_counter: &mut u64,
    payload: &[u8],
) -> io::Result<()> {
    queue_masked_text_frame_parts(tx, mask_counter, &[payload])
}

/// Text-frame counterpart of [`queue_masked_binary_frame_parts`]
/// (Hyperliquid's subscriptions, Deribit's `public/test` answer).
#[inline]
pub fn queue_masked_text_frame_parts(
    tx: &mut IoBuf,
    mask_counter: &mut u64,
    parts: &[&[u8]],
) -> io::Result<()> {
    let mask = ws_mask_from_counter(*mask_counter);
    *mask_counter = mask_counter.wrapping_add(1);
    let n = ws_write_text_frame_parts(tx.free_mut(), parts, mask)
        .map_err(|_| io::Error::other("ws text frame: tx buffer too small"))?;
    tx.advance(n);
    Ok(())
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    #[repr(u8)]
    enum TestKind {
        Subscribe = 0,
        Poll = 1,
        None = 255,
    }
    impl ReqKind for TestKind {
        const FREE: Self = Self::None;
    }

    #[test]
    fn pending_record_complete_roundtrip() {
        let mut t: PendingTable<TestKind, 8> = PendingTable::new();
        t.record(1, TestKind::Subscribe, 100).unwrap();
        t.record(2, TestKind::Poll, 200).unwrap();
        assert_eq!(t.count(), 2);
        let r = t.complete(1).unwrap();
        assert_eq!(r.kind, TestKind::Subscribe);
        assert_eq!(r.created_at_ns, 100);
        assert_eq!(t.count(), 1);
        // Double-complete → None (venue redelivery tolerated).
        assert!(t.complete(1).is_none());
    }

    #[test]
    fn pending_rejects_zero_id_and_busy_slot() {
        let mut t: PendingTable<TestKind, 4> = PendingTable::new();
        assert_eq!(t.record(0, TestKind::Poll, 0), Err(PendingErr::ZeroId));
        t.record(3, TestKind::Poll, 0).unwrap();
        // id 7 maps to the same slot (7 & 3 == 3): busy.
        assert_eq!(t.record(7, TestKind::Poll, 0), Err(PendingErr::SlotBusy));
        // Unknown id whose slot is used by a different id → None.
        assert!(t.complete(7).is_none());
    }

    #[test]
    fn pending_clear_frees_everything() {
        let mut t: PendingTable<TestKind, 4> = PendingTable::new();
        t.record(1, TestKind::Poll, 0).unwrap();
        t.clear();
        assert_eq!(t.count(), 0);
        assert!(t.complete(1).is_none());
    }

    #[test]
    fn expired_requests_are_taken_oldest_age_first_by_slot() {
        let mut t: PendingTable<TestKind, 8> = PendingTable::new();
        t.record(1, TestKind::Poll, 100).unwrap();
        t.record(2, TestKind::Subscribe, 300).unwrap();
        assert!(t.take_expired(350, 300).is_none(), "neither has waited 300");
        let r = t.take_expired(400, 300).expect("id 1 waited 300");
        assert_eq!((r.id, r.kind), (1, TestKind::Poll));
        assert!(t.take_expired(400, 300).is_none());
        // max_age 0 drains everything, one per call.
        t.record(9, TestKind::Poll, 400).unwrap();
        let mut n = 0;
        while t.take_expired(400, 0).is_some() {
            n += 1;
        }
        assert_eq!((n, t.count()), (2, 0));
    }

    #[test]
    fn req_ids_skip_busy_slots_and_never_issue_zero() {
        let mut ids: ReqIds<TestKind, 4> = ReqIds::new();
        let mut i = 0;
        while i < 4 {
            assert_eq!(ids.issue(TestKind::Poll, 0), Some(i + 1));
            i += 1;
        }
        assert_eq!(ids.issue(TestKind::Poll, 0), None, "all four in flight");
        // Answer 2 out of order: its slot (2 & 3) is the only free one.
        assert_eq!(ids.answer(2).map(|r| r.id), Some(2));
        // The refused call spent ids 5–8; 9 → slot 1 busy; 10 → slot 2 free.
        assert_eq!(ids.issue(TestKind::Subscribe, 5), Some(10));
        assert!(ids.answer(2).is_none(), "an answered id is gone");
        assert!(ids.answer(99).is_none(), "a foreign id is ignored");
        assert_eq!(ids.in_flight(), 4);
        let mut drained = 0;
        while ids.take_expired(u64::MAX, 0).is_some() {
            drained += 1;
        }
        assert_eq!((drained, ids.in_flight()), (4, 0));
        ids.next = u64::MAX;
        assert_eq!(ids.issue(TestKind::Poll, 0), Some(u64::MAX));
        assert_eq!(ids.issue(TestKind::Poll, 0), Some(1), "the wrap skips 0");
    }

    #[test]
    fn sub_table_insert_lookup_clear() {
        let mut s: SubTable<TestKind, 4> = SubTable::new();
        s.insert(SubId(0xAB), TestKind::Subscribe).unwrap();
        assert_eq!(s.kind_of(SubId(0xAB)), Some(TestKind::Subscribe));
        assert_eq!(s.kind_of(SubId(0xCD)), None);
        assert_eq!(s.count(), 1);
        s.clear();
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn sub_table_rejects_reserved_and_overflow() {
        let mut s: SubTable<TestKind, 2> = SubTable::new();
        assert_eq!(
            s.insert(SubId::NONE, TestKind::Poll),
            Err(SubErr::ReservedId)
        );
        s.insert(SubId(1), TestKind::Poll).unwrap();
        s.insert(SubId(2), TestKind::Poll).unwrap();
        assert_eq!(s.insert(SubId(3), TestKind::Poll), Err(SubErr::Full));
    }

    #[test]
    fn queue_frames_write_into_tx() {
        let mut tx = IoBuf::with_capacity(256);
        let mut ctr = 0u64;
        queue_masked_binary_frame(&mut tx, &mut ctr, b"\x01\x02").unwrap();
        queue_masked_text_frame(&mut tx, &mut ctr, b"{\"op\":\"subscribe\"}").unwrap();
        assert_eq!(ctr, 2);
        // Two client frames: FIN+opcode, MASK bit set on both.
        let bytes = tx.filled();
        assert!(bytes.len() > 4);
        assert_eq!(bytes[0] & 0x0F, 0x02, "first frame is binary");
        assert!(bytes[1] & 0x80 != 0, "client frames are masked");
    }

    #[test]
    fn queue_frame_fails_on_tiny_tx() {
        let mut tx = IoBuf::with_capacity(4);
        let mut ctr = 0u64;
        let big = [0u8; 64];
        assert!(queue_masked_binary_frame(&mut tx, &mut ctr, &big).is_err());
    }

    #[test]
    fn a_parts_frame_is_the_frame_of_their_concatenation() {
        // The mask index runs across part boundaries: split or whole,
        // the same bytes under the same mask counter give the same wire
        // frame — binary and text, short and 16-bit length forms.
        let long = [b'x'; 200];
        let parts: [&[u8]; 3] = [b"{\"id\":", &long, b"}"];
        let whole = parts.concat();
        let (mut split, mut one) = (IoBuf::with_capacity(1024), IoBuf::with_capacity(1024));
        let (mut cs, mut co) = (7u64, 7u64);
        queue_masked_binary_frame_parts(&mut split, &mut cs, &parts).unwrap();
        queue_masked_binary_frame(&mut one, &mut co, &whole).unwrap();
        queue_masked_text_frame_parts(&mut split, &mut cs, &[b"{\"id\":", b"42}"]).unwrap();
        queue_masked_text_frame(&mut one, &mut co, b"{\"id\":42}").unwrap();
        assert_eq!(split.filled(), one.filled());
        assert_eq!((cs, co), (9, 9));
    }

    #[test]
    fn a_parts_frame_fails_on_tiny_tx() {
        let mut tx = IoBuf::with_capacity(8);
        let mut ctr = 0u64;
        assert!(queue_masked_binary_frame_parts(&mut tx, &mut ctr, &[b"abc", b"defgh"]).is_err());
        assert!(queue_masked_text_frame_parts(&mut tx, &mut ctr, &[b"abc", b"defgh"]).is_err());
        assert!(tx.is_empty(), "a refused frame writes nothing");
    }
}
