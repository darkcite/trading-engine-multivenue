// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ladder — bounded in-ingress L2 depth (WS10-B)
//!
//! The maintenance half of the depth design
//! (docs/ws10-engine-plumbing-design.md §3): a fixed-capacity,
//! price-sorted ladder per side, delta-applied IN the ingress thread,
//! snapshotted into the [`DepthTopK`] carrier only when the top-K
//! changed. The honest constraint from the design doc: OKX `books`
//! and Deribit `book.100ms` are DELTA streams — real depth requires
//! book maintenance somewhere, and here it stays bounded
//! ([`DEPTH_LADDER_CAP`] levels/side, linear scans over one
//! structure-of-arrays cache-line group).
//!
//! Overflow beyond the cap tracks a `beyond_cap` count (the Deribit
//! `DEPTH_CAP` excess precedent) — conservative, never wrong about
//! the top: a full ladder still accepts a level that beats its worst
//! (evicting the worst), and only counts levels worse than everything
//! held.
//!
//! Zero-alloc: POD sides, in-place shifts. Book channels run
//! 10–20 Hz/instrument on our venues — the linear scan is measured
//! noise beside the parse.

use core_types::{DepthLevel, DepthTopK, NsTs, SymbolId, VenueId, DEPTH_K};

/// Maximum levels maintained per side (D-B2). Updates landing beyond
/// a full ladder's worst level are counted, not stored.
pub const DEPTH_LADDER_CAP: usize = 64;

/// One price-sorted side. Best-first: descending for bids, ascending
/// for asks — the ordering is a construction-time property.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct LadderSide {
    px_1e6: [i64; DEPTH_LADDER_CAP],
    qty_1e6: [i64; DEPTH_LADDER_CAP],
    len: u32,
    /// Set-updates dropped because the ladder was full and the price
    /// was worse than every held level (diagnostic; cleared with the
    /// ladder).
    beyond_cap: u32,
    /// True = bids (descending price is better-first).
    is_bid: bool,
    _pad: [u8; 7],
}

impl LadderSide {
    /// Empty side with the given ordering.
    #[inline]
    pub const fn new(is_bid: bool) -> Self {
        Self {
            px_1e6: [0; DEPTH_LADDER_CAP],
            qty_1e6: [0; DEPTH_LADDER_CAP],
            len: 0,
            beyond_cap: 0,
            is_bid,
            _pad: [0; 7],
        }
    }

    /// Held level count.
    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// True when no levels are held.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Set-updates dropped beyond a full ladder (see struct docs).
    #[inline]
    pub fn beyond_cap(&self) -> u32 {
        self.beyond_cap
    }

    /// Drop every level (resync path). `beyond_cap` clears too — a
    /// fresh snapshot restarts the book's story.
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
        self.beyond_cap = 0;
    }

    /// `a` is strictly better (closer to the touch) than `b`.
    #[inline(always)]
    fn better(&self, a: i64, b: i64) -> bool {
        if self.is_bid {
            a > b
        } else {
            a < b
        }
    }

    /// Apply one venue level update: `qty == 0` deletes the price
    /// level (absent = no-op, per both venues' delta semantics);
    /// otherwise inserts or replaces. Prices are ×1e6.
    pub fn set(&mut self, px_1e6: i64, qty_1e6: i64) {
        let n = self.len as usize;
        // Find the price (exact) or its insertion point (first held
        // price WORSE than the update). Linear — n ≤ 64, one SoA
        // array walked.
        let mut i = 0;
        while i < n {
            let held = self.px_1e6[i];
            if held == px_1e6 {
                if qty_1e6 == 0 {
                    // Delete: shift the tail left.
                    let mut j = i;
                    while j + 1 < n {
                        self.px_1e6[j] = self.px_1e6[j + 1];
                        self.qty_1e6[j] = self.qty_1e6[j + 1];
                        j += 1;
                    }
                    self.len -= 1;
                } else {
                    self.qty_1e6[i] = qty_1e6;
                }
                return;
            }
            if self.better(px_1e6, held) {
                break;
            }
            i += 1;
        }
        if qty_1e6 == 0 {
            // Delete of a level we never held — venue-legal no-op.
            return;
        }
        if n == DEPTH_LADDER_CAP {
            if i == n {
                // Worse than everything held on a full ladder.
                self.beyond_cap = self.beyond_cap.saturating_add(1);
                return;
            }
            // Beats a held level: evict the worst (last), insert at i.
            let mut j = DEPTH_LADDER_CAP - 1;
            while j > i {
                self.px_1e6[j] = self.px_1e6[j - 1];
                self.qty_1e6[j] = self.qty_1e6[j - 1];
                j -= 1;
            }
            self.px_1e6[i] = px_1e6;
            self.qty_1e6[i] = qty_1e6;
            return;
        }
        // Room available: shift right from the tail, insert at i.
        let mut j = n;
        while j > i {
            self.px_1e6[j] = self.px_1e6[j - 1];
            self.qty_1e6[j] = self.qty_1e6[j - 1];
            j -= 1;
        }
        self.px_1e6[i] = px_1e6;
        self.qty_1e6[i] = qty_1e6;
        self.len += 1;
    }

    /// Copy the best `DEPTH_K` levels into a carrier half; slots past
    /// the held depth are [`DepthLevel::EMPTY`].
    #[inline]
    pub fn top_k(&self) -> [DepthLevel; DEPTH_K] {
        let mut out = [DepthLevel::EMPTY; DEPTH_K];
        let n = (self.len as usize).min(DEPTH_K);
        let mut i = 0;
        while i < n {
            out[i] = DepthLevel {
                px_1e6: self.px_1e6[i],
                qty_1e6: self.qty_1e6[i],
            };
            i += 1;
        }
        out
    }
}

/// Both sides of one instrument's ladder.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct DepthLadder {
    /// Bid side (descending price).
    pub bids: LadderSide,
    /// Ask side (ascending price).
    pub asks: LadderSide,
}

impl DepthLadder {
    /// Empty ladder.
    #[inline]
    pub const fn new() -> Self {
        Self {
            bids: LadderSide::new(true),
            asks: LadderSide::new(false),
        }
    }

    /// Drop both sides (resync path).
    #[inline]
    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }

    /// Snapshot the current top-K into the carrier.
    #[inline]
    pub fn snapshot(&self, ts_ns: NsTs, venue: VenueId, sym: SymbolId, flags: u8) -> DepthTopK {
        DepthTopK::new(
            ts_ns,
            venue,
            sym,
            flags,
            self.bids.top_k(),
            self.asks.top_k(),
        )
    }
}

impl Default for DepthLadder {
    fn default() -> Self {
        Self::new()
    }
}

/// True when two carriers describe the same book state — levels and
/// flags, NOT timestamps. The change-gated emission compare.
#[inline]
pub fn levels_equal(a: &DepthTopK, b: &DepthTopK) -> bool {
    a.bids == b.bids && a.asks == b.asks && a.flags == b.flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bid_side_sorts_descending_and_top_k_fills() {
        let mut s = LadderSide::new(true);
        s.set(100, 1);
        s.set(300, 3);
        s.set(200, 2);
        assert_eq!(s.len(), 3);
        let k = s.top_k();
        assert_eq!(
            k[0],
            DepthLevel {
                px_1e6: 300,
                qty_1e6: 3
            }
        );
        assert_eq!(
            k[1],
            DepthLevel {
                px_1e6: 200,
                qty_1e6: 2
            }
        );
        assert_eq!(
            k[2],
            DepthLevel {
                px_1e6: 100,
                qty_1e6: 1
            }
        );
        assert_eq!(k[3], DepthLevel::EMPTY);
    }

    #[test]
    fn ask_side_sorts_ascending() {
        let mut s = LadderSide::new(false);
        s.set(300, 3);
        s.set(100, 1);
        s.set(200, 2);
        let k = s.top_k();
        assert_eq!(k[0].px_1e6, 100);
        assert_eq!(k[1].px_1e6, 200);
        assert_eq!(k[2].px_1e6, 300);
    }

    #[test]
    fn replace_and_delete_semantics() {
        let mut s = LadderSide::new(true);
        s.set(100, 1);
        s.set(100, 9);
        assert_eq!(s.len(), 1);
        assert_eq!(s.top_k()[0].qty_1e6, 9);
        s.set(100, 0);
        assert_eq!(s.len(), 0);
        // Delete of a never-held level is a venue-legal no-op.
        s.set(555, 0);
        assert_eq!(s.len(), 0);
        assert_eq!(s.beyond_cap(), 0);
    }

    #[test]
    fn full_ladder_evicts_worst_or_counts_beyond_cap() {
        let mut s = LadderSide::new(true);
        // Fill with 64 descending prices 1000, 999, ... 937.
        for i in 0..DEPTH_LADDER_CAP as i64 {
            s.set(1_000 - i, 1);
        }
        assert_eq!(s.len(), DEPTH_LADDER_CAP);
        // Worse than everything → counted, not stored.
        s.set(1, 1);
        assert_eq!(s.len(), DEPTH_LADDER_CAP);
        assert_eq!(s.beyond_cap(), 1);
        // Better than the best → evicts the worst (937).
        s.set(2_000, 5);
        assert_eq!(s.len(), DEPTH_LADDER_CAP);
        assert_eq!(s.top_k()[0].px_1e6, 2_000);
        let worst = s.px_1e6[DEPTH_LADDER_CAP - 1];
        assert_eq!(worst, 938, "old worst evicted");
    }

    #[test]
    fn clear_resets_everything() {
        let mut s = LadderSide::new(false);
        s.set(10, 1);
        for i in 0..(DEPTH_LADDER_CAP as i64 + 5) {
            s.set(100 + i, 1);
        }
        assert!(s.beyond_cap() > 0 || s.len() == DEPTH_LADDER_CAP);
        s.clear();
        assert!(s.is_empty());
        assert_eq!(s.beyond_cap(), 0);
    }

    #[test]
    fn ladder_snapshot_and_levels_equal_gate() {
        let mut l = DepthLadder::new();
        l.bids.set(100, 1);
        l.asks.set(110, 2);
        let a = l.snapshot(1, VenueId::Okx, 7, 0);
        // Same book, later clock → equal by the emission gate.
        let b = l.snapshot(2, VenueId::Okx, 7, 0);
        assert!(levels_equal(&a, &b));
        // Any level change breaks equality.
        l.bids.set(100, 3);
        let c = l.snapshot(3, VenueId::Okx, 7, 0);
        assert!(!levels_equal(&a, &c));
        // Flag change breaks equality too (STALE must always emit).
        let d = l.snapshot(3, VenueId::Okx, 7, core_types::DEPTH_FLAG_STALE);
        assert!(!levels_equal(&c, &d));
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    /// Reference model: BTreeMap px → qty, truncated to the best
    /// DEPTH_LADDER_CAP levels after every op (the ladder can never
    /// resurrect an evicted level, so the model drops them too).
    fn model_apply(m: &mut BTreeMap<i64, i64>, px: i64, qty: i64, is_bid: bool) {
        if qty == 0 {
            m.remove(&px);
            return;
        }
        if m.len() == DEPTH_LADDER_CAP && !m.contains_key(&px) {
            // Full: only accept if better than the model's worst.
            let worst = if is_bid {
                *m.keys().next().unwrap()
            } else {
                *m.keys().next_back().unwrap()
            };
            let better = if is_bid { px > worst } else { px < worst };
            if !better {
                return;
            }
            m.remove(&worst);
        }
        m.insert(px, qty);
    }

    fn model_top_k(m: &BTreeMap<i64, i64>, is_bid: bool) -> Vec<(i64, i64)> {
        let it: Vec<(i64, i64)> = if is_bid {
            m.iter()
                .rev()
                .take(DEPTH_K)
                .map(|(p, q)| (*p, *q))
                .collect()
        } else {
            m.iter().take(DEPTH_K).map(|(p, q)| (*p, *q)).collect()
        };
        it
    }

    proptest! {
        /// Random delta sequences: the ladder's top-K equals the
        /// BTreeMap reference model's, both orderings.
        #[test]
        fn ladder_matches_btreemap_reference(
            is_bid in proptest::bool::ANY,
            ops in proptest::collection::vec((1i64..200, 0i64..5), 0..512),
        ) {
            let mut s = LadderSide::new(is_bid);
            let mut m: BTreeMap<i64, i64> = BTreeMap::new();
            for (px, qty) in ops {
                s.set(px, qty);
                model_apply(&mut m, px, qty, is_bid);
                prop_assert_eq!(s.len(), m.len());
            }
            let got = s.top_k();
            let want = model_top_k(&m, is_bid);
            for (i, (px, qty)) in want.iter().enumerate() {
                prop_assert_eq!(got[i].px_1e6, *px);
                prop_assert_eq!(got[i].qty_1e6, *qty);
            }
            for slot in got.iter().skip(want.len()) {
                prop_assert_eq!(*slot, DepthLevel::EMPTY);
            }
        }

        /// The ladder never panics and never exceeds its cap on
        /// arbitrary inputs (the §21.3 never-panic bar).
        #[test]
        fn ladder_never_panics_or_overflows(
            ops in proptest::collection::vec((any::<i64>(), any::<i64>()), 0..256),
        ) {
            let mut l = DepthLadder::new();
            for (px, qty) in ops {
                l.bids.set(px, qty);
                l.asks.set(px, qty);
                prop_assert!(l.bids.len() <= DEPTH_LADDER_CAP);
                prop_assert!(l.asks.len() <= DEPTH_LADDER_CAP);
            }
            let snap = l.snapshot(1, VenueId::Deribit, 9, 0);
            prop_assert_eq!(snap.k as usize, DEPTH_K);
        }
    }
}
