// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The held-quote IoC law — Hypercall's paper fills (HC11, ruling
//! O-HC21).
//!
//! **An IoC fills at the provider's indicative quote in force
//! [`HELD_DELAY_NS`] after submit, up to the quote's displayed size,
//! when that quote is at or better than the order's limit; otherwise it
//! gets nothing.**
//!
//! Why not the strict-cross IoC law every other venue uses (judge at the
//! first fresh tick after activation): Hypercall's book is a quote
//! provider's indicative stream, updated when the provider moves, often
//! seconds apart — "the first tick after activation" there can be the
//! quote many seconds later, and a law that waits for it would fill at a
//! price that did not exist when the venue's RPI answered. The venue
//! answers `best_execution` against the quote then in force; the law
//! samples the quote in force at `submit + 2 s` (sample-and-hold: the
//! last quote stamped at or before that instant) and judges once.
//!
//! The book keeps the last quote of each Hypercall option (indexed by
//! the symbol's ordinal) and the pending IoCs; a caller judges the due
//! ones at each record it sees — BEFORE it applies that record's own
//! quote, so a tick after the due instant never leaks into the verdict.
//! Quotes may be one-sided or crossed as the venue publishes them: a buy
//! needs an ask, a sell a bid, nothing else. A quote the ingress marked
//! STALE (the provider stopped refreshing it) is no quote: both sides
//! absent until a fresh one arrives.
//!
//! Zero allocation: fixed arrays; `const fn new`.

use core_types::{Side, SymbolId, Tick};

use crate::{judge_ioc, Touch, Verdict};

/// The sample instant after submit: 2 s (O-HC21).
pub const HELD_DELAY_NS: u64 = 2_000_000_000;

/// Hypercall option ordinals held: the venue's `HC_INSTRUMENTS_MAX`.
pub const HELD_SYMS: usize = 1024;

/// The first option ordinal (`OPT_ORDINAL_BASE + 1`).
pub const HELD_ORDINAL_FIRST: u32 = 513;

/// IoCs pending at once.
pub const HELD_PENDING: usize = 16;

/// A held quote: the touch as last published (0 = that side absent).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HeldQuote {
    /// Bid ×1e6.
    pub bid_1e6: i64,
    /// Displayed bid size ×1e6.
    pub bid_qty_1e6: i64,
    /// Ask ×1e6.
    pub ask_1e6: i64,
    /// Displayed ask size ×1e6.
    pub ask_qty_1e6: i64,
}

const EMPTY_QUOTE: HeldQuote = HeldQuote {
    bid_1e6: 0,
    bid_qty_1e6: 0,
    ask_1e6: 0,
    ask_qty_1e6: 0,
};

/// One pending IoC.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HeldOrder {
    /// When it is judged: submit + [`HELD_DELAY_NS`].
    pub due_ns: u64,
    /// Limit ×1e6.
    pub px_1e6: i64,
    /// Size ×1e6.
    pub qty_1e6: i64,
    /// The member's id.
    pub client_oid: u64,
    /// The instrument.
    pub sym: SymbolId,
    /// Bid (buy) or ask (sell).
    pub side: Side,
    /// The slot.
    pub strategy_id: u8,
    /// The venue byte the order addressed.
    pub venue: u8,
    _pad: u8,
}

const EMPTY_ORDER: HeldOrder = HeldOrder {
    due_ns: 0,
    px_1e6: 0,
    qty_1e6: 0,
    client_oid: 0,
    sym: core_types::SYMBOL_ID_NONE,
    side: Side::Bid,
    strategy_id: core_types::STRATEGY_ID_NONE,
    venue: 0,
    _pad: 0,
};

/// The quote index of `sym`, or `None` off the Hypercall option
/// ordinals — the venue byte included: another venue's ordinal 513 (a
/// Deribit option, a Binance USDⓈ-M perp) is not a Hypercall option.
#[inline]
#[must_use]
pub const fn held_index(sym: SymbolId) -> Option<usize> {
    if core_types::symbol_venue_byte(sym) != core_types::VenueId::Hypercall as u8 {
        return None;
    }
    let o = core_types::symbol_ordinal(sym);
    if o >= HELD_ORDINAL_FIRST && ((o - HELD_ORDINAL_FIRST) as usize) < HELD_SYMS {
        Some((o - HELD_ORDINAL_FIRST) as usize)
    } else {
        None
    }
}

/// The book (module doc).
pub struct HeldBook {
    quotes: [HeldQuote; HELD_SYMS],
    pending: [HeldOrder; HELD_PENDING],
    len: usize,
}

impl Default for HeldBook {
    fn default() -> Self {
        Self::new()
    }
}

impl HeldBook {
    /// Nothing held.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            quotes: [EMPTY_QUOTE; HELD_SYMS],
            pending: [EMPTY_ORDER; HELD_PENDING],
            len: 0,
        }
    }

    /// IoCs pending.
    #[inline]
    #[must_use]
    pub const fn pending_len(&self) -> usize {
        self.len
    }

    /// Hold `tick` as its symbol's quote in force (a non-option symbol
    /// is ignored). A side the tick does not quote is absent; a stale tick
    /// quotes neither.
    #[inline]
    pub fn on_quote(&mut self, tick: &Tick) {
        let Some(i) = held_index(tick.sym) else {
            return;
        };
        if tick.is_stale() {
            self.quotes[i] = EMPTY_QUOTE;
            return;
        }
        let (b, bq, a, aq) = (
            tick.bid_px.raw(),
            tick.bid_qty.raw(),
            tick.ask_px.raw(),
            tick.ask_qty.raw(),
        );
        self.quotes[i] = HeldQuote {
            bid_1e6: if b > 0 && bq > 0 { b } else { 0 },
            bid_qty_1e6: if b > 0 && bq > 0 { bq } else { 0 },
            ask_1e6: if a > 0 && aq > 0 { a } else { 0 },
            ask_qty_1e6: if a > 0 && aq > 0 { aq } else { 0 },
        };
    }

    /// The quote held for `sym`.
    #[inline]
    #[must_use]
    pub fn quote(&self, sym: SymbolId) -> Option<HeldQuote> {
        held_index(sym).map(|i| self.quotes[i])
    }

    /// Take an IoC, judged at `now_ns + HELD_DELAY_NS`. `false` when the
    /// book is full or the symbol is not an option ordinal.
    pub fn submit(&mut self, o: HeldOrder) -> bool {
        if self.len >= HELD_PENDING || held_index(o.sym).is_none() || o.qty_1e6 <= 0 || o.px_1e6 <= 0 {
            return false;
        }
        self.pending[self.len] = o;
        self.len += 1;
        true
    }

    /// Judge every IoC due by `now_ns` against the quote in force, in
    /// submit order; `f(order, verdict)` per judgement. A fill consumes
    /// the displayed size it took, so two orders due at one instant do
    /// not both take the same size.
    pub fn judge_due<F: FnMut(&HeldOrder, Verdict)>(&mut self, now_ns: u64, mut f: F) {
        let mut i = 0usize;
        while i < self.len {
            let o = self.pending[i];
            if now_ns < o.due_ns {
                i += 1;
                continue;
            }
            let v = match held_index(o.sym) {
                Some(k) => {
                    let q = &mut self.quotes[k];
                    let t = Touch {
                        bid_1e6: q.bid_1e6,
                        ask_1e6: q.ask_1e6,
                        bid_qty_1e6: q.bid_qty_1e6,
                        ask_qty_1e6: q.ask_qty_1e6,
                    };
                    let (mut ab, mut bb) = (q.ask_qty_1e6, q.bid_qty_1e6);
                    // A side the quote lacks cannot fill (price 0 is no
                    // price): its budget is already 0.
                    let v = judge_ioc(o.side, o.px_1e6, o.qty_1e6, t, &mut ab, &mut bb);
                    q.ask_qty_1e6 = ab;
                    q.bid_qty_1e6 = bb;
                    v
                }
                None => Verdict::Cancel,
            };
            f(&o, v);
            // Remove, keeping submit order.
            let mut j = i;
            while j + 1 < self.len {
                self.pending[j] = self.pending[j + 1];
                j += 1;
            }
            self.len -= 1;
            self.pending[self.len] = EMPTY_ORDER;
        }
    }
}

impl HeldOrder {
    /// A pending IoC.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        due_ns: u64,
        sym: SymbolId,
        side: Side,
        px_1e6: i64,
        qty_1e6: i64,
        client_oid: u64,
        strategy_id: u8,
        venue: u8,
    ) -> Self {
        Self {
            due_ns,
            px_1e6,
            qty_1e6,
            client_oid,
            sym,
            side,
            strategy_id,
            venue,
            _pad: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, Price, Qty, VenueId};

    fn sym(k: u32) -> SymbolId {
        make_symbol_id(VenueId::Hypercall, HELD_ORDINAL_FIRST + k)
    }

    fn tick(s: SymbolId, bid: i64, bq: i64, ask: i64, aq: i64) -> Tick {
        Tick::new_stamped(
            1,
            VenueId::Hypercall,
            s,
            0,
            Price::from_raw(bid),
            Qty::from_raw(bq),
            Price::from_raw(ask),
            Qty::from_raw(aq),
            0,
            0,
        )
    }

    fn buy(due: u64, s: SymbolId, px: i64, qty: i64) -> HeldOrder {
        HeldOrder::new(due, s, Side::Bid, px, qty, 9, 7, VenueId::Hypercall as u8)
    }

    #[test]
    fn the_quote_in_force_at_the_due_instant_decides() {
        let mut b = HeldBook::new();
        b.on_quote(&tick(sym(0), 900_000, 5_000_000, 1_000_000, 2_000_000));
        assert!(b.submit(buy(2_000, sym(0), 1_050_000, 3_000_000)));
        // Before due: nothing.
        let mut n = 0;
        b.judge_due(1_999, |_, _| n += 1);
        assert_eq!(n, 0);
        // Due: at the ask in force, capped by its displayed size.
        let mut got = None;
        b.judge_due(2_000, |o, v| got = Some((o.client_oid, v)));
        assert_eq!(got, Some((9, Verdict::Fill { px_1e6: 1_000_000, qty_1e6: 2_000_000 })));
        assert_eq!(b.pending_len(), 0);
    }

    #[test]
    fn a_quote_worse_than_the_limit_or_absent_gives_nothing() {
        let mut b = HeldBook::new();
        b.on_quote(&tick(sym(1), 900_000, 5_000_000, 1_100_000, 2_000_000));
        assert!(b.submit(buy(10, sym(1), 1_050_000, 1_000_000)));
        let mut v = None;
        b.judge_due(10, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Cancel));
        // One-sided: no ask → a buy cannot fill, whatever its limit.
        b.on_quote(&tick(sym(1), 900_000, 5_000_000, 0, 0));
        assert!(b.submit(buy(20, sym(1), 9_000_000, 1_000_000)));
        b.judge_due(20, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Cancel));
        // A price without a size is no quote either.
        b.on_quote(&tick(sym(1), 0, 0, 700_000, 0));
        assert!(b.submit(buy(30, sym(1), 9_000_000, 1_000_000)));
        b.judge_due(30, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Cancel));
        // A stale quote — the provider stopped refreshing it — is none.
        let mut stale = tick(sym(1), 900_000, 5_000_000, 1_000_000, 2_000_000);
        stale.flags = core_types::TICK_FLAG_STALE;
        b.on_quote(&stale);
        assert_eq!(b.quote(sym(1)), Some(HeldQuote::default()));
        assert!(b.submit(buy(40, sym(1), 9_000_000, 1_000_000)));
        b.judge_due(40, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Cancel));
    }

    #[test]
    fn two_orders_due_together_share_the_displayed_size() {
        let mut b = HeldBook::new();
        b.on_quote(&tick(sym(2), 1_000_000, 3_000_000, 1_200_000, 1_000_000));
        let sell = |due, qty| HeldOrder::new(due, sym(2), Side::Ask, 950_000, qty, 1, 7, 9);
        assert!(b.submit(sell(5, 2_000_000)));
        assert!(b.submit(sell(5, 2_000_000)));
        let mut fills = [0i64; 2];
        let mut k = 0usize;
        b.judge_due(5, |_, v| {
            if let Verdict::Fill { qty_1e6, px_1e6 } = v {
                assert_eq!(px_1e6, 1_000_000, "at the bid in force");
                fills[k] = qty_1e6;
            }
            k += 1;
        });
        assert_eq!(fills, [2_000_000, 1_000_000]);
    }

    #[test]
    fn off_ordinal_symbols_and_a_full_book_refuse() {
        let mut b = HeldBook::new();
        let idx = make_symbol_id(VenueId::Hypercall, 1);
        assert!(!b.submit(buy(1, idx, 1, 1)), "an index symbol is no option");
        assert!(!b.submit(buy(1, sym(HELD_SYMS as u32), 1, 1)));
        let mut i = 0;
        while i < HELD_PENDING {
            assert!(b.submit(buy(1, sym(3), 1, 1)));
            i += 1;
        }
        assert!(!b.submit(buy(1, sym(3), 1, 1)));
        assert_eq!(held_index(sym(5)), Some(5));
        // Another venue's ordinal 513 is not a Hypercall option.
        assert_eq!(held_index(make_symbol_id(VenueId::Deribit, HELD_ORDINAL_FIRST)), None);
        assert_eq!(held_index(core_types::SYMBOL_ID_NONE), None);
    }
}
