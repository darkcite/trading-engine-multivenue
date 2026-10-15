// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `ClosedRing` — the 9000-closed-trade retention ring (impl-05 §4.6.6). The crate's one heap
//! allocation, made in `new`; a push past `cap::CLOSED_RETAIN` overwrites the oldest.

use crate::cap;
use pine_ir::TradeRec;

/// `cap::CLOSED_RETAIN` as the ring's `u32` index domain.
const RETAIN: u32 = 9000;
const _: () = assert!(RETAIN as usize == cap::CLOSED_RETAIN);

/// The retained closed trades, oldest first.
pub(crate) struct ClosedRing {
    /// `cap::CLOSED_RETAIN` records, allocated once.
    recs: Box<[TradeRec]>,
    /// Next write position.
    head: u32,
    /// Records retained, `≤ RETAIN`.
    len: u32,
}

impl ClosedRing {
    /// The crate's one allocation: 1 152 000 B built on the heap (no stack temporary).
    pub(crate) fn new() -> Self {
        Self {
            recs: vec![TradeRec::default(); cap::CLOSED_RETAIN].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    /// Records retained: `min(closedtrades, 9000)`.
    pub(crate) fn len(&self) -> u32 {
        self.len
    }

    /// Append a closed trade, overwriting the oldest when full.
    pub(crate) fn push(&mut self, rec: &TradeRec) {
        let h = self.head as usize;
        // COPY: §4.17 #1 — the ring owns an immutable 128-B record while the lot slot is reused.
        self.recs[h] = *rec;
        self.head = if self.head + 1 == RETAIN {
            0
        } else {
            self.head + 1
        };
        if self.len < RETAIN {
            self.len += 1;
        }
    }

    /// Closed trade `i`, 0 = oldest retained; `None` past `len`.
    pub(crate) fn get(&self, i: u32) -> Option<&TradeRec> {
        if i >= self.len {
            return None;
        }
        // head + RETAIN − len + i < 2·RETAIN: no overflow, one conditional wrap.
        let mut k = self.head + RETAIN - self.len + i;
        if k >= RETAIN {
            k -= RETAIN;
        }
        self.recs.get(k as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::{ClosedRing, RETAIN};
    use pine_ir::TradeRec;

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
        let mut k = 0;
        while k < 9001 {
            r.push(&rec(k));
            k += 1;
        }
        assert_eq!(r.len(), RETAIN);
        // The 9001st closed trade made the oldest (0) unreadable.
        assert_eq!(r.get(0).map(|t| t.qty_1e6), Some(1));
        assert_eq!(r.get(RETAIN - 1).map(|t| t.qty_1e6), Some(9000));
        assert!(r.get(RETAIN).is_none());
    }
}
