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
//! ## PM15-5c: the Polymarket rolling 15-minute legs
//!
//! Their book is a delta stream: the ingress emits a leg's touch when it
//! changes (and re-asserts it at each `PONG` of the market socket), so
//! silence means unchanged, and "the first tick after activation" on a
//! quiet leg is a LATER price — or none before the IoC's TTL, an unfilled
//! order the venue would have filled. The venue matches a FAK against the
//! book as it stands when its taker hold ends ([`PM_TAKER_HOLD_NS`], the
//! `itode` markets); the law samples the quote in force at submit +
//! [`PM_HELD_DELAY_NS`] (the activation Δ — the order's way there and the
//! feed's way back — plus the hold) and judges once. The ingress withdraws
//! every touch (a stale tick) when its session drops, so a dead socket
//! leaves no quote in force. Only the rolling LEGS are held: the legacy
//! Polymarket markets keep the strict-cross law, a rolling mark slot quotes
//! nothing.
//!
//! ## PMJ J4: the Polymarket rolling 5-minute legs (slot 11, through Jupiter)
//!
//! Held the same way, after the 15-minute legs, and judged at submit + the
//! keeper's delay — Jupiter's keeper takes the order to the book — which
//! the dispatcher takes from `pmj.toml paper_delay_ms` (default
//! [`PMJ_PAPER_DELAY_NS_DEFAULT`], 2 s; `clob_dispatcher`'s
//! `with_pm5_held_delay_ns`). Only slot 11 trades the 5-minute pool (ruling
//! R-PMJ-3), so a delay of the pool is a delay of the slot's orders.
//!
//! Zero allocation: fixed arrays; `const fn new`.

use core_types::pm_roll::{
    pm5_rolling_of, pm_rolling_of, PM5_OFF_MARK, PM5_ROLLING_MAX, PM_OFF_MARK, PM_ROLLING_MAX,
};
use core_types::{Side, SymbolId, Tick};

use crate::{judge_ioc, Touch, Verdict};

/// The sample instant after submit: 2 s (O-HC21).
pub const HELD_DELAY_NS: u64 = 2_000_000_000;

/// Hypercall option ordinals held: the venue's `HC_INSTRUMENTS_MAX`.
pub const HELD_SYMS: usize = 1024;

/// The first option ordinal (`OPT_ORDINAL_BASE + 1`).
pub const HELD_ORDINAL_FIRST: u32 = 513;

/// PM15-5c: the Polymarket rolling 15-minute legs, held after the
/// Hypercall options — four leg offsets per family.
pub const HELD_PM15_LEGS: usize = PM_ROLLING_MAX * PM_OFF_MARK as usize;

/// PMJ J4: the rolling 5-minute legs, held after the 15-minute ones — six
/// leg offsets per family.
pub const HELD_PM5_LEGS: usize = PM5_ROLLING_MAX * PM5_OFF_MARK as usize;

/// Every rolling leg held.
pub const HELD_PM_LEGS: usize = HELD_PM15_LEGS + HELD_PM5_LEGS;

/// PMJ J4: a 5-minute leg's IoC is judged this long after submit unless
/// the boot says otherwise (`pmj.toml paper_delay_ms`), ns.
pub const PMJ_PAPER_DELAY_NS_DEFAULT: u64 = 2_000_000_000;

/// Quotes the book holds: the Hypercall options, then the rolling legs.
pub const HELD_QUOTES: usize = HELD_SYMS + HELD_PM_LEGS;

/// PM15-5c: Polymarket's taker hold on its crypto markets (`itode: true`),
/// ns — 150 ms since 2026-09-04 14:00Z (the venue's changelog).
pub const PM_TAKER_HOLD_NS: u64 = 150_000_000;

/// PM15-5c: a rolling leg's IoC is judged at the quote in force this long
/// after submit: the venue's activation Δ plus its taker hold.
pub const PM_HELD_DELAY_NS: u64 =
    crate::ACTIVATION_NS_DEFAULT[core_types::VenueId::Polymarket as usize] + PM_TAKER_HOLD_NS;

/// The pending IoCs' shares, by class: the Hypercall options (16, the
/// book's whole capacity before the rolling legs joined it), the 15-minute
/// legs (slot 9) and the 5-minute legs (slot 11), one per rolling family —
/// a member holds at most one IoC per family (one pending per market, and
/// one market per family is in its late window). A class never takes
/// another's share, so a full 5-minute burst cannot refuse slot 9's take,
/// nor the options either's (PMJ J4, risk review round 1).
pub const HELD_PENDING_SHARES: [usize; 3] = [16, PM_ROLLING_MAX, PM5_ROLLING_MAX];

/// IoCs pending at once: the shares' sum.
pub const HELD_PENDING: usize = HELD_PENDING_SHARES[0] + HELD_PENDING_SHARES[1] + HELD_PENDING_SHARES[2];

/// The share class of quote index `k` ([`HELD_PENDING_SHARES`]): 0 the
/// options, 1 the 15-minute legs, 2 the 5-minute legs.
#[inline(always)]
#[must_use]
pub const fn share_class(k: usize) -> usize {
    if k < HELD_SYMS {
        0
    } else if k < HELD_SYMS + HELD_PM15_LEGS {
        1
    } else {
        2
    }
}

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

/// PM15-5c: whether `sym` is a Polymarket rolling leg (not its mark slot,
/// not a legacy Polymarket market) — judged by this law.
#[inline]
#[must_use]
pub const fn held_pm_leg(sym: SymbolId) -> bool {
    matches!(pm_rolling_of(sym), Some((_, off)) if off < PM_OFF_MARK) || held_pm5_leg(sym)
}

/// PMJ J4: whether `sym` is a rolling 5-minute leg (slot 11's).
#[inline]
#[must_use]
pub const fn held_pm5_leg(sym: SymbolId) -> bool {
    matches!(pm5_rolling_of(sym), Some((_, off)) if off < PM5_OFF_MARK)
}

/// The book's quote index of `sym`: a Hypercall option ([`held_index`]),
/// then a Polymarket rolling leg ([`held_pm_leg`]); `None` for every other
/// symbol.
#[inline]
#[must_use]
pub const fn quote_index(sym: SymbolId) -> Option<usize> {
    if let Some(i) = held_index(sym) {
        return Some(i);
    }
    match pm_rolling_of(sym) {
        Some((f, off)) if off < PM_OFF_MARK => {
            return Some(HELD_SYMS + f * PM_OFF_MARK as usize + off as usize);
        }
        _ => {}
    }
    match pm5_rolling_of(sym) {
        Some((g, off)) if off < PM5_OFF_MARK => {
            Some(HELD_SYMS + HELD_PM15_LEGS + g * PM5_OFF_MARK as usize + off as usize)
        }
        _ => None,
    }
}

/// When an IoC on `sym` is judged, ns after its submit: a rolling leg at
/// [`PM_HELD_DELAY_NS`], a Hypercall option at [`HELD_DELAY_NS`].
#[inline]
#[must_use]
pub const fn held_delay_ns(sym: SymbolId) -> u64 {
    if held_pm5_leg(sym) {
        PMJ_PAPER_DELAY_NS_DEFAULT
    } else if held_pm_leg(sym) {
        PM_HELD_DELAY_NS
    } else {
        HELD_DELAY_NS
    }
}

/// The book (module doc).
pub struct HeldBook {
    quotes: [HeldQuote; HELD_QUOTES],
    pending: [HeldOrder; HELD_PENDING],
    len: usize,
    class_len: [usize; 3],
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
            quotes: [EMPTY_QUOTE; HELD_QUOTES],
            pending: [EMPTY_ORDER; HELD_PENDING],
            len: 0,
            class_len: [0; 3],
        }
    }

    /// IoCs pending.
    #[inline]
    #[must_use]
    pub const fn pending_len(&self) -> usize {
        self.len
    }

    /// IoCs pending in share class `class` ([`share_class`]).
    #[inline]
    #[must_use]
    pub const fn pending_in(&self, class: usize) -> usize {
        self.class_len[class]
    }

    /// Hold `tick` as its symbol's quote in force (a symbol the book does
    /// not hold is ignored). A side the tick does not quote is absent; a
    /// stale tick quotes neither.
    #[inline]
    pub fn on_quote(&mut self, tick: &Tick) {
        let Some(i) = quote_index(tick.sym) else {
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
        quote_index(sym).map(|i| self.quotes[i])
    }

    /// Take an IoC, judged at its `due_ns` (submit + [`held_delay_ns`]).
    /// `false` when its class's share is full or the symbol is not one it
    /// holds.
    pub fn submit(&mut self, o: HeldOrder) -> bool {
        let Some(k) = quote_index(o.sym) else {
            return false;
        };
        let c = share_class(k);
        if self.len >= HELD_PENDING || self.class_len[c] >= HELD_PENDING_SHARES[c] || o.qty_1e6 <= 0 || o.px_1e6 <= 0 {
            return false;
        }
        self.pending[self.len] = o;
        self.len += 1;
        self.class_len[c] += 1;
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
            let v = match quote_index(o.sym) {
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
            // Every pending order's symbol is held (`submit`).
            if let Some(k) = quote_index(o.sym) {
                let c = share_class(k);
                debug_assert!(self.class_len[c] > 0);
                self.class_len[c] -= 1;
            }
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
        while i < HELD_PENDING_SHARES[0] {
            assert!(b.submit(buy(1, sym(3), 1, 1)));
            i += 1;
        }
        assert!(!b.submit(buy(1, sym(3), 1, 1)), "the options' share is full");
        assert_eq!(held_index(sym(5)), Some(5));
        // Another venue's ordinal 513 is not a Hypercall option.
        assert_eq!(held_index(make_symbol_id(VenueId::Deribit, HELD_ORDINAL_FIRST)), None);
        assert_eq!(held_index(core_types::SYMBOL_ID_NONE), None);
    }

    /// PM15-5c: a rolling leg is held after the options; a quiet leg fills
    /// at the quote in force at submit + 350 ms; the mark slot and the
    /// legacy markets are not held; a withdrawn touch fills nothing.
    #[test]
    fn a_rolling_leg_is_judged_at_the_quote_in_force_after_the_hold() {
        use core_types::pm_roll::{pm_mark_sym, pm_rolling_sym};
        let leg = pm_rolling_sym(1, 1, 1);
        assert_eq!(quote_index(leg), Some(HELD_SYMS + 4 + 3));
        assert_eq!(
            quote_index(pm_rolling_sym(PM_ROLLING_MAX - 1, 1, 1)),
            Some(HELD_SYMS + HELD_PM15_LEGS - 1)
        );
        assert!(held_pm_leg(leg) && !held_pm_leg(pm_mark_sym(1)));
        assert_eq!(quote_index(pm_mark_sym(1)), None);
        assert_eq!(
            quote_index(make_symbol_id(VenueId::Polymarket, 7)),
            None,
            "a legacy market keeps strict cross"
        );
        assert_eq!(held_index(leg), None, "hcv's index stays the options'");
        assert_eq!(held_delay_ns(leg), 350_000_000);
        assert_eq!(held_delay_ns(sym(0)), HELD_DELAY_NS);
        let q = |bid: i64, ask: i64| {
            Tick::new_stamped(
                1,
                VenueId::Polymarket,
                leg,
                1,
                Price::from_raw(bid),
                Qty::from_raw(9_000_000),
                Price::from_raw(ask),
                Qty::from_raw(4_000_000),
                0,
                0,
            )
        };
        let mut b = HeldBook::new();
        b.on_quote(&q(950_000, 960_000));
        let o = HeldOrder::new(
            held_delay_ns(leg),
            leg,
            Side::Bid,
            970_000,
            5_000_000,
            3,
            9,
            VenueId::Polymarket as u8,
        );
        assert!(b.submit(o));
        let mut got = None;
        b.judge_due(349_999_999, |_, v| got = Some(v));
        assert_eq!(got, None, "inside the hold");
        // Quiet: nothing changed, and the quote in force fills.
        b.judge_due(350_000_000, |_, v| got = Some(v));
        assert_eq!(
            got,
            Some(Verdict::Fill {
                px_1e6: 960_000,
                qty_1e6: 4_000_000
            })
        );
        // The session drops: the withdrawn touch fills nothing.
        let mut down = q(0, 0);
        down.flags = core_types::TICK_FLAG_STALE;
        b.on_quote(&down);
        assert!(b.submit(o));
        b.judge_due(u64::MAX, |_, v| got = Some(v));
        assert_eq!(got, Some(Verdict::Cancel));
    }

    /// PMJ J4: a 5-minute leg is held after the 15-minute legs, its mark is
    /// not, and it is judged at the keeper's delay (the 15-minute legs keep
    /// theirs).
    #[test]
    fn a_five_minute_leg_is_held_after_the_fifteen_minute_legs_at_the_keepers_delay() {
        use core_types::pm_roll::{pm5_mark_sym, pm5_rolling_sym, pm_rolling_sym, PM5_PAIRS};
        let first = pm5_rolling_sym(0, 0, 0);
        assert_eq!(quote_index(first), Some(HELD_SYMS + HELD_PM15_LEGS));
        let last = pm5_rolling_sym(PM5_ROLLING_MAX - 1, PM5_PAIRS - 1, 1);
        assert_eq!(quote_index(last), Some(HELD_QUOTES - 1));
        assert!(held_pm_leg(first) && held_pm5_leg(first));
        assert!(!held_pm5_leg(pm_rolling_sym(0, 0, 0)), "a 15 m leg is not a 5 m leg");
        assert_eq!(quote_index(pm5_mark_sym(0)), None);
        assert!(!held_pm_leg(pm5_mark_sym(0)));
        assert_eq!(held_delay_ns(first), PMJ_PAPER_DELAY_NS_DEFAULT);
        assert_eq!(held_delay_ns(pm_rolling_sym(0, 0, 0)), PM_HELD_DELAY_NS);
        let mut b = HeldBook::new();
        let t = |at: u64, ask: i64| {
            Tick::new(
                at,
                VenueId::Polymarket,
                first,
                1,
                core_types::Price::from_raw(ask - 10_000),
                core_types::Qty::from_raw(100_000_000),
                core_types::Price::from_raw(ask),
                core_types::Qty::from_raw(100_000_000),
            )
        };
        b.on_quote(&t(0, 930_000));
        assert!(b.submit(HeldOrder::new(2_000, first, Side::Bid, 940_000, 5_000_000, 7, 11, VenueId::Jupiter as u8)));
        let mut early = 0u32;
        b.judge_due(1_999, |_, _| early += 1);
        assert_eq!(early, 0, "not due before the delay");
        let mut got = None;
        b.judge_due(2_000, |o, v| got = Some((o.venue, v)));
        assert!(
            matches!(got, Some((10, Verdict::Fill { px_1e6: 930_000, qty_1e6: 5_000_000 }))),
            "the quote in force, the order's Jupiter venue byte kept: {got:?}"
        );
    }

    /// PMJ J4 (risk review round 1): each class has its own share — a full
    /// burst of options and 5-minute IoCs leaves slot 9's 15-minute share
    /// whole, and a judged order frees its own class's place.
    #[test]
    fn a_full_burst_of_one_class_never_refuses_another() {
        use core_types::pm_roll::{pm5_rolling_sym, pm_rolling_sym};
        let mut b = HeldBook::new();
        let opt = |k: u32| buy(1, sym(k), 1, 1);
        let pm5 = |f: usize| HeldOrder::new(1, pm5_rolling_sym(f, 0, 0), Side::Bid, 1, 1, 5, 11, VenueId::Jupiter as u8);
        let pm15 = |f: usize| HeldOrder::new(1, pm_rolling_sym(f, 0, 0), Side::Bid, 1, 1, 6, 9, VenueId::Polymarket as u8);
        let mut i = 0usize;
        while i < HELD_PENDING_SHARES[0] {
            assert!(b.submit(opt(i as u32)));
            i += 1;
        }
        let mut f = 0usize;
        while f < PM5_ROLLING_MAX {
            assert!(b.submit(pm5(f)));
            f += 1;
        }
        assert!(!b.submit(opt(0)) && !b.submit(pm5(0)), "both shares full");
        let mut f = 0usize;
        while f < PM_ROLLING_MAX {
            assert!(b.submit(pm15(f)), "slot 9's take is never refused for slot 11's or slot 7's");
            f += 1;
        }
        assert!(!b.submit(pm15(0)));
        assert_eq!((b.pending_len(), b.pending_in(0), b.pending_in(1), b.pending_in(2)), (HELD_PENDING, 16, PM_ROLLING_MAX, PM5_ROLLING_MAX));
        let mut n = 0usize;
        b.judge_due(1, |_, _| n += 1);
        assert_eq!((n, b.pending_len(), b.pending_in(0), b.pending_in(1), b.pending_in(2)), (HELD_PENDING, 0, 0, 0, 0));
        assert!(b.submit(pm15(0)) && b.submit(pm5(0)) && b.submit(opt(0)), "judged orders free their places");
    }
}
