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
//! * [`queue_masked_binary_frame`] / [`queue_masked_text_frame`] —
//!   the "serialize into tx IoBuf with a fresh mask" pattern every
//!   client-side WS writer needs.
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
use crate::ws_frame::{ws_mask_from_counter, ws_write_binary_frame, ws_write_text_frame};

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
            assert!(N.is_power_of_two() && N >= 2, "PendingTable N must be a power of two >= 2");
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
}

impl<K: ReqKind, const N: usize> Default for PendingTable<K, N> {
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
    let mask = ws_mask_from_counter(*mask_counter);
    *mask_counter = mask_counter.wrapping_add(1);
    let dst = tx.free_mut();
    let n = ws_write_binary_frame(dst, payload, mask)
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
    let mask = ws_mask_from_counter(*mask_counter);
    *mask_counter = mask_counter.wrapping_add(1);
    let dst = tx.free_mut();
    let n = ws_write_text_frame(dst, payload, mask)
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
        assert_eq!(
            t.record(0, TestKind::Poll, 0),
            Err(PendingErr::ZeroId)
        );
        t.record(3, TestKind::Poll, 0).unwrap();
        // id 7 maps to the same slot (7 & 3 == 3): busy.
        assert_eq!(
            t.record(7, TestKind::Poll, 0),
            Err(PendingErr::SlotBusy)
        );
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
        assert_eq!(s.insert(SubId::NONE, TestKind::Poll), Err(SubErr::ReservedId));
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
}
