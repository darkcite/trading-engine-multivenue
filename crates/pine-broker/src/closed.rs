// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ClosedRing` — the 9000-closed-trade retention ring (impl-05 §4.6.6). The crate's one heap
//! allocation, made in `new`; a push past `cap::CLOSED_RETAIN` overwrites the oldest.
//!
//! REQ-r33g-1 (r33h): beside each record the ring keeps the cumulative net profit at its close
//! (TradingView's List of Trades `cum_pnl`), in the same allocation — blocks of [`LANES`] records
//! followed by their [`LANES`] values — so capacity and eviction are the records' by
//! construction and `new` still allocates once (I3).

use crate::cap;
use pine_ir::TradeRec;

/// `cap::CLOSED_RETAIN` as the ring's `u32` index domain.
const RETAIN: u32 = 9000;
const _: () = assert!(RETAIN as usize == cap::CLOSED_RETAIN);

/// Records per block; a power of two dividing `RETAIN`.
const LANES: usize = 8;
const _: () = assert!(LANES.is_power_of_two() && cap::CLOSED_RETAIN % LANES == 0);
/// Blocks in the ring.
const BLOCKS: usize = cap::CLOSED_RETAIN / LANES;

/// [`LANES`] closed records and the cumulative net profit ×1e12 at each one's close.
#[derive(Clone, Copy, Default)]
#[repr(C, align(64))]
struct Block {
    recs: [TradeRec; LANES],
    cum_net_1e12: [i128; LANES],
}

const _: () = assert!(core::mem::size_of::<Block>() == LANES * 128 + LANES * 16);
const _: () = assert!(core::mem::align_of::<Block>() == 64);

/// The retained closed trades, oldest first.
pub(crate) struct ClosedRing {
    /// `BLOCKS` blocks (`cap::CLOSED_RETAIN` records), allocated once.
    blocks: Box<[Block]>,
    /// Next write position.
    head: u32,
    /// Records retained, `≤ RETAIN`.
    len: u32,
}

impl ClosedRing {
    /// The crate's one allocation: 1 296 000 B built on the heap (no ring-sized stack temporary).
    pub(crate) fn new() -> Self {
        Self {
            blocks: vec![Block::default(); BLOCKS].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    /// Records retained: `min(closedtrades, 9000)`.
    pub(crate) fn len(&self) -> u32 {
        self.len
    }

    /// Forget every record, in place: no allocation, no free (I3/I4).
    pub(crate) fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// The slot the next closed trade and its cumulative net profit are written into, IN PLACE
    /// (§4.17 #1: one copy, lot → ring); claims it now, overwriting the oldest when full.
    pub(crate) fn push_slot(&mut self) -> (&mut TradeRec, &mut i128) {
        let h = self.head as usize;
        self.head = if self.head + 1 == RETAIN {
            0
        } else {
            self.head + 1
        };
        if self.len < RETAIN {
            self.len += 1;
        }
        let b = &mut self.blocks[h / LANES];
        (&mut b.recs[h % LANES], &mut b.cum_net_1e12[h % LANES])
    }

    /// Ring position of record `i` (0 = oldest retained); `None` past `len`.
    fn pos(&self, i: u32) -> Option<usize> {
        if i >= self.len {
            return None;
        }
        // head + RETAIN − len + i < 2·RETAIN: no overflow, one conditional wrap.
        let mut k = self.head + RETAIN - self.len + i;
        if k >= RETAIN {
            k -= RETAIN;
        }
        Some(k as usize)
    }

    /// Closed trade `i`, 0 = oldest retained; `None` past `len`.
    pub(crate) fn get(&self, i: u32) -> Option<&TradeRec> {
        let k = self.pos(i)?;
        let b = self.blocks.get(k / LANES)?;
        b.recs.get(k % LANES)
    }

    /// The cumulative net profit ×1e12 recorded at closed trade `i`'s close; `None` past `len`.
    pub(crate) fn cum_net(&self, i: u32) -> Option<i128> {
        let k = self.pos(i)?;
        let b = self.blocks.get(k / LANES)?;
        b.cum_net_1e12.get(k % LANES).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::{ClosedRing, RETAIN};
    use pine_ir::TradeRec;

    fn push(r: &mut ClosedRing, t: &TradeRec, cum: i128) {
        let (rec, c) = r.push_slot();
        *rec = *t;
        *c = cum;
    }

    fn rec(k: i64) -> TradeRec {
        TradeRec {
            qty_1e6: k,
            ..TradeRec::default()
        }
    }

    #[test]
    fn indexes_oldest_first_and_trims_past_9000() {
        let mut r = ClosedRing::new();
        assert!(r.get(0).is_none());
        assert!(r.cum_net(0).is_none());
        let mut k = 0;
        while k < 9001 {
            push(&mut r, &rec(k), -i128::from(k));
            k += 1;
        }
        assert_eq!(r.len(), RETAIN);
        // The 9001st closed trade made the oldest (0) unreadable, its cumulative value with it.
        assert_eq!(r.get(0).map(|t| t.qty_1e6), Some(1));
        assert_eq!(r.cum_net(0), Some(-1));
        assert_eq!(r.get(RETAIN - 1).map(|t| t.qty_1e6), Some(9000));
        assert_eq!(r.cum_net(RETAIN - 1), Some(-9000));
        assert!(r.get(RETAIN).is_none());
        assert!(r.cum_net(RETAIN).is_none());
        // Every retained record and its value stay paired across the wrap.
        let mut i = 0;
        while i < RETAIN {
            let q = r.get(i).map(|t| t.qty_1e6);
            assert_eq!(r.cum_net(i), q.map(|q| -i128::from(q)));
            i += 1;
        }
    }

    #[test]
    fn clear_empties_in_place() {
        let mut r = ClosedRing::new();
        push(&mut r, &rec(7), 70);
        push(&mut r, &rec(8), 80);
        let p = r.blocks.as_ptr();
        r.clear();
        assert_eq!(r.len(), 0);
        assert!(r.get(0).is_none());
        assert!(r.cum_net(0).is_none());
        assert_eq!(r.blocks.as_ptr(), p, "clear must not reallocate");
        push(&mut r, &rec(9), 90);
        assert_eq!(r.get(0).map(|t| t.qty_1e6), Some(9));
        assert_eq!(r.cum_net(0), Some(90));
        assert_eq!(r.len(), 1);
    }
}
