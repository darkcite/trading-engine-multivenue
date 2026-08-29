// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # book-builder
//!
//! Maintains a fixed-depth book per market. Fed by the Polymarket
//! ingress tick stream; queried by strategies via a snapshot type.
//!
//! Phase 2 ships:
//! * `TopOfBook` — POD top-of-book for a single market (unchanged).
//! * `MultiBook<const N: usize>` — fixed-capacity table of N book
//!   slots indexed by `SymbolId`. Linear-scan lookup, zero-alloc
//!   apply, cache-aligned slots.
//!
//! Full L2 ladders are deferred to Phase 3 if the strategy needs
//! depth — current Polymarket markets have so little depth past
//! the top that mid-vs-mid is sufficient for the edge.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use core_types::{Price, Qty, SymbolId, Tick, SYMBOL_ID_NONE};

/// Outcome of [`TopOfBook::apply`]. D10 fix: out-of-order and gapped
/// events are counted and surfaced to the caller instead of silently
/// dropped — the caller owns the resync policy (per-venue, §6.2 of
/// the Phase-8 plan; e.g. Binance `bookTicker` gaps are legitimate
/// and must NOT trigger resync, OKX seq-chain breaks must).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ApplyOutcome {
    /// Applied; sequence advanced contiguously (or first update).
    Applied = 0,
    /// Applied, but `venue_seq` jumped by more than one — the venue
    /// skipped sequence numbers upstream of us.
    AppliedGap = 1,
    /// Dropped: `venue_seq` is not newer than the last applied.
    /// Out-of-order delivery or duplicate.
    Stale = 2,
    /// Dropped: tick is for a different symbol than this slot.
    WrongSymbol = 3,
}

/// Top-of-book for a single market.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct TopOfBook {
    /// Symbol.
    pub sym: SymbolId,
    /// Venue sequence of the last update applied.
    pub venue_seq: u32,
    /// Best bid.
    pub bid_px: Price,
    /// Size at best bid.
    pub bid_qty: Qty,
    /// Best ask.
    pub ask_px: Price,
    /// Size at best ask.
    pub ask_qty: Qty,
    /// Number of applies whose `venue_seq` advanced non-contiguously.
    gaps: u32,
    /// Number of dropped out-of-order/duplicate applies.
    stale_drops: u32,
    /// Reserved for cache-line padding.
    _pad: [u8; 16],
}

impl TopOfBook {
    /// Construct an empty book for `sym`.
    pub const fn empty(sym: SymbolId) -> Self {
        Self {
            sym,
            venue_seq: 0,
            bid_px: Price::from_raw(0),
            bid_qty: Qty::from_raw(0),
            ask_px: Price::from_raw(0),
            ask_qty: Qty::from_raw(0),
            gaps: 0,
            stale_drops: 0,
            _pad: [0; 16],
        }
    }

    /// Apply a tick. Never silently drops (D10): stale/out-of-order
    /// events are counted in [`Self::stale_drops`], non-contiguous
    /// advances in [`Self::gaps`], and the outcome is returned so the
    /// caller can trigger a venue-appropriate resync.
    #[inline]
    pub fn apply(&mut self, tick: &Tick) -> ApplyOutcome {
        if tick.sym != self.sym {
            return ApplyOutcome::WrongSymbol;
        }
        if tick.venue_seq <= self.venue_seq {
            self.stale_drops = self.stale_drops.wrapping_add(1);
            return ApplyOutcome::Stale;
        }
        // Gap iff this slot has seen at least one update
        // (venue_seq == 0 doubles as the "never seen" marker) and
        // the sequence advanced by more than one. Branchless count.
        let gapped = (self.venue_seq != 0) & (tick.venue_seq > self.venue_seq.wrapping_add(1));
        self.gaps = self.gaps.wrapping_add(gapped as u32);
        self.venue_seq = tick.venue_seq;
        self.bid_px = tick.bid_px;
        self.bid_qty = tick.bid_qty;
        self.ask_px = tick.ask_px;
        self.ask_qty = tick.ask_qty;
        if gapped {
            ApplyOutcome::AppliedGap
        } else {
            ApplyOutcome::Applied
        }
    }

    /// Non-contiguous sequence advances observed by this slot.
    #[inline]
    pub const fn gaps(&self) -> u32 {
        self.gaps
    }

    /// Out-of-order/duplicate events dropped by this slot.
    #[inline]
    pub const fn stale_drops(&self) -> u32 {
        self.stale_drops
    }

    /// Whether this slot has ever observed a tick.
    #[inline]
    pub const fn has_quotes(&self) -> bool {
        self.venue_seq != 0
    }

    /// Integer mid (rounds toward zero).
    #[inline]
    pub const fn mid(&self) -> Price {
        Price::from_raw((self.bid_px.raw() + self.ask_px.raw()) / 2)
    }
}

// ---------------------------------------------------------------
// MultiBook — N-slot fixed-capacity table
// ---------------------------------------------------------------

/// Reason an insert into [`MultiBook`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BookErr {
    /// The slot table is at capacity. The caller is misconfigured —
    /// raise `N` at boot or trim the tracked symbol set.
    Full,
    /// The caller asked to track a sentinel `SYMBOL_ID_NONE`. That
    /// id is reserved for cross-market signals and must never be
    /// a book key.
    ReservedSymbol,
    /// The symbol is already tracked. Second `track` is a no-op
    /// from the caller's standpoint, but we surface it so misuse is
    /// visible in tests.
    AlreadyTracked,
}

/// Fixed-capacity collection of [`TopOfBook`]s. `N` is set at boot;
/// linear-scan lookup is fine at the sizes we care about (≤ 64).
///
/// Storage layout: `[TopOfBook; N]` so every slot owns its own
/// cache line. The `count` is a separate `u32` — lookup walks the
/// first `count` entries.
///
/// Single-writer; the engine thread mutates, strategy threads only
/// read.
#[derive(Debug)]
#[repr(C, align(64))]
pub struct MultiBook<const N: usize> {
    entries: [TopOfBook; N],
    count: u32,
    _pad: [u8; 60],
}

impl<const N: usize> MultiBook<N> {
    /// Construct an empty book table. All slots are
    /// `TopOfBook::empty(SYMBOL_ID_NONE)` — the sentinel sym means
    /// "slot is free".
    pub const fn empty() -> Self {
        Self {
            entries: [TopOfBook::empty(SYMBOL_ID_NONE); N],
            count: 0,
            _pad: [0; 60],
        }
    }

    /// How many slots are populated.
    #[inline]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the table is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Slot capacity.
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Register a Polymarket symbol. Inserts at boot only — never on
    /// the hot path. Returns the index assigned to the slot.
    pub fn track(&mut self, sym: SymbolId) -> Result<u32, BookErr> {
        if sym == SYMBOL_ID_NONE {
            return Err(BookErr::ReservedSymbol);
        }
        // Already tracked?
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            if self.entries[i].sym == sym {
                return Err(BookErr::AlreadyTracked);
            }
            i += 1;
        }
        if n >= N {
            return Err(BookErr::Full);
        }
        self.entries[n] = TopOfBook::empty(sym);
        let idx = self.count;
        self.count = self.count.wrapping_add(1);
        Ok(idx)
    }

    /// Apply a tick. Walks `0..count` looking for the matching
    /// `sym`; ticks for untracked symbols return
    /// [`ApplyOutcome::WrongSymbol`] (the same stream may feed
    /// multiple consumers — an untracked symbol is not an error).
    ///
    /// Hot path — zero alloc, no panic.
    #[inline]
    pub fn apply(&mut self, tick: &Tick) -> ApplyOutcome {
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            if self.entries[i].sym == tick.sym {
                return self.entries[i].apply(tick);
            }
            i += 1;
        }
        ApplyOutcome::WrongSymbol
    }

    /// Apply at a pre-computed slot index. Caller has obtained
    /// `idx` from a prior [`index_of`] and cached it — skips the
    /// O(N) scan when the same symbol fires repeatedly (the
    /// common case for cross-arb + latency-arb hot paths).
    /// Returns `None` if `idx` is out of range or the slot's
    /// symbol no longer matches the tick; the caller should fall
    /// back to `apply` in that case.
    #[inline(always)]
    pub fn apply_at(&mut self, idx: u32, tick: &Tick) -> Option<ApplyOutcome> {
        let n = self.count as usize;
        let i = idx as usize;
        if i >= n {
            return None;
        }
        if self.entries[i].sym != tick.sym {
            return None;
        }
        Some(self.entries[i].apply(tick))
    }

    /// Snapshot for `sym`, if tracked. `None` for unknown symbols.
    #[inline]
    pub fn snapshot(&self, sym: SymbolId) -> Option<TopOfBook> {
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            if self.entries[i].sym == sym {
                return Some(self.entries[i]);
            }
            i += 1;
        }
        None
    }

    /// Find the table index for `sym`, if tracked. Strategies that
    /// keep parallel per-symbol arrays can cache this once and skip
    /// the linear scan on subsequent ticks.
    #[inline]
    pub fn index_of(&self, sym: SymbolId) -> Option<u32> {
        let n = self.count as usize;
        let mut i = 0;
        while i < n {
            if self.entries[i].sym == sym {
                return Some(i as u32);
            }
            i += 1;
        }
        None
    }

    /// Borrow the populated slots as a slice.
    #[inline]
    pub fn slots(&self) -> &[TopOfBook] {
        &self.entries[..self.count as usize]
    }
}

impl<const N: usize> Default for MultiBook<N> {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(sym: SymbolId, seq: u32, bid: i64, ask: i64) -> Tick {
        Tick::new(
            0,
            core_types::VenueId::Polymarket,
            sym,
            seq,
            Price::from_raw(bid),
            Qty::from_raw(10),
            Price::from_raw(ask),
            Qty::from_raw(20),
        )
    }

    #[test]
    fn apply_updates_state() {
        let mut b = TopOfBook::empty(1);
        b.apply(&mk(1, 1, 100, 200));
        assert_eq!(b.venue_seq, 1);
        assert_eq!(b.bid_px.raw(), 100);
    }

    #[test]
    fn apply_ignores_stale_seq() {
        let mut b = TopOfBook::empty(1);
        b.apply(&mk(1, 5, 100, 200));
        b.apply(&mk(1, 3, 999, 999));
        assert_eq!(b.venue_seq, 5);
        assert_eq!(b.bid_px.raw(), 100);
    }

    #[test]
    fn apply_ignores_wrong_symbol() {
        let mut b = TopOfBook::empty(1);
        b.apply(&mk(2, 1, 100, 200));
        assert_eq!(b.venue_seq, 0);
    }

    #[test]
    fn top_of_book_is_cache_aligned() {
        assert_eq!(::core::mem::align_of::<TopOfBook>(), 64);
    }

    #[test]
    fn top_of_book_mid_rounds_toward_zero() {
        let mut b = TopOfBook::empty(1);
        b.apply(&mk(1, 1, 100, 201));
        assert_eq!(b.mid().raw(), 150); // (100+201)/2 = 150 (truncates)
    }

    #[test]
    fn top_of_book_has_quotes_flips_after_first_apply() {
        let mut b = TopOfBook::empty(1);
        assert!(!b.has_quotes());
        b.apply(&mk(1, 1, 100, 200));
        assert!(b.has_quotes());
    }

    // ---- MultiBook ----

    #[test]
    fn multi_book_track_returns_sequential_indices() {
        let mut mb: MultiBook<4> = MultiBook::empty();
        assert_eq!(mb.track(10).unwrap(), 0);
        assert_eq!(mb.track(20).unwrap(), 1);
        assert_eq!(mb.track(30).unwrap(), 2);
        assert_eq!(mb.len(), 3);
    }

    #[test]
    fn multi_book_track_rejects_sentinel() {
        let mut mb: MultiBook<4> = MultiBook::empty();
        assert_eq!(mb.track(SYMBOL_ID_NONE), Err(BookErr::ReservedSymbol));
    }

    #[test]
    fn multi_book_track_rejects_duplicates() {
        let mut mb: MultiBook<4> = MultiBook::empty();
        mb.track(10).unwrap();
        assert_eq!(mb.track(10), Err(BookErr::AlreadyTracked));
    }

    #[test]
    fn multi_book_track_returns_full_on_overflow() {
        let mut mb: MultiBook<2> = MultiBook::empty();
        mb.track(10).unwrap();
        mb.track(20).unwrap();
        assert_eq!(mb.track(30), Err(BookErr::Full));
    }

    #[test]
    fn multi_book_apply_routes_to_correct_slot() {
        let mut mb: MultiBook<4> = MultiBook::empty();
        mb.track(10).unwrap();
        mb.track(20).unwrap();
        mb.apply(&mk(10, 1, 500_000, 510_000));
        mb.apply(&mk(20, 1, 100_000, 110_000));
        let s10 = mb.snapshot(10).unwrap();
        let s20 = mb.snapshot(20).unwrap();
        assert_eq!(s10.bid_px.raw(), 500_000);
        assert_eq!(s20.bid_px.raw(), 100_000);
    }

    #[test]
    fn multi_book_apply_drops_untracked_symbol() {
        let mut mb: MultiBook<4> = MultiBook::empty();
        mb.track(10).unwrap();
        mb.apply(&mk(99, 1, 1, 2)); // not tracked
        let s = mb.snapshot(10).unwrap();
        assert!(!s.has_quotes());
    }

    #[test]
    fn multi_book_snapshot_returns_none_for_untracked() {
        let mb: MultiBook<4> = MultiBook::empty();
        assert!(mb.snapshot(42).is_none());
    }

    #[test]
    fn multi_book_index_of_matches_track_index() {
        let mut mb: MultiBook<4> = MultiBook::empty();
        let i = mb.track(42).unwrap();
        assert_eq!(mb.index_of(42), Some(i));
        assert_eq!(mb.index_of(43), None);
    }

    #[test]
    fn multi_book_is_cache_aligned() {
        assert_eq!(::core::mem::align_of::<MultiBook<4>>(), 64);
    }

    #[test]
    fn multi_book_slots_returns_populated_only() {
        let mut mb: MultiBook<4> = MultiBook::empty();
        mb.track(10).unwrap();
        mb.track(20).unwrap();
        assert_eq!(mb.slots().len(), 2);
        assert_eq!(mb.slots()[0].sym, 10);
        assert_eq!(mb.slots()[1].sym, 20);
    }

    // ---- D10: apply outcomes + gap/stale accounting ----

    #[test]
    fn apply_reports_contiguous_and_first_as_applied() {
        let mut b = TopOfBook::empty(1);
        // First update never counts as a gap, whatever its seq.
        assert_eq!(b.apply(&mk(1, 5, 100, 200)), ApplyOutcome::Applied);
        assert_eq!(b.apply(&mk(1, 6, 101, 201)), ApplyOutcome::Applied);
        assert_eq!(b.gaps(), 0);
        assert_eq!(b.stale_drops(), 0);
    }

    #[test]
    fn apply_counts_gap_and_still_applies() {
        let mut b = TopOfBook::empty(1);
        b.apply(&mk(1, 1, 100, 200));
        assert_eq!(b.apply(&mk(1, 5, 110, 210)), ApplyOutcome::AppliedGap);
        assert_eq!(b.gaps(), 1);
        // The gapped update must still land — newest data wins.
        assert_eq!(b.venue_seq, 5);
        assert_eq!(b.bid_px.raw(), 110);
    }

    #[test]
    fn apply_counts_stale_and_preserves_state() {
        let mut b = TopOfBook::empty(1);
        b.apply(&mk(1, 5, 100, 200));
        assert_eq!(b.apply(&mk(1, 3, 999, 999)), ApplyOutcome::Stale);
        assert_eq!(b.apply(&mk(1, 5, 888, 888)), ApplyOutcome::Stale);
        assert_eq!(b.stale_drops(), 2);
        assert_eq!(b.venue_seq, 5);
        assert_eq!(b.bid_px.raw(), 100);
    }

    #[test]
    fn apply_wrong_symbol_touches_no_counter() {
        let mut b = TopOfBook::empty(1);
        assert_eq!(b.apply(&mk(2, 1, 100, 200)), ApplyOutcome::WrongSymbol);
        assert_eq!(b.gaps(), 0);
        assert_eq!(b.stale_drops(), 0);
        assert!(!b.has_quotes());
    }

    #[test]
    fn multi_book_apply_reports_untracked_as_wrong_symbol() {
        let mut mb: MultiBook<4> = MultiBook::empty();
        mb.track(10).unwrap();
        assert_eq!(mb.apply(&mk(99, 1, 1, 2)), ApplyOutcome::WrongSymbol);
        assert_eq!(mb.apply(&mk(10, 1, 1, 2)), ApplyOutcome::Applied);
    }

    #[test]
    fn multi_book_apply_at_propagates_outcome_or_none() {
        let mut mb: MultiBook<4> = MultiBook::empty();
        let idx = mb.track(10).unwrap();
        assert_eq!(
            mb.apply_at(idx, &mk(10, 1, 1, 2)),
            Some(ApplyOutcome::Applied)
        );
        assert_eq!(
            mb.apply_at(idx, &mk(10, 1, 1, 2)),
            Some(ApplyOutcome::Stale)
        );
        // Wrong slot symbol → None (caller falls back to apply()).
        assert_eq!(mb.apply_at(idx, &mk(11, 2, 1, 2)), None);
        // Out-of-range index → None.
        assert_eq!(mb.apply_at(7, &mk(10, 2, 1, 2)), None);
    }

    #[test]
    fn top_of_book_is_still_one_cache_line_with_counters() {
        assert_eq!(::core::mem::size_of::<TopOfBook>(), 64);
        assert_eq!(::core::mem::align_of::<TopOfBook>(), 64);
    }
}
