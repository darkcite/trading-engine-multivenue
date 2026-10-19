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
//! ## PMJ J8: the rolling legs fill as the venue matches a FAK
//!
//! The venue matches a FAK across every ask level up to its limit, not the
//! touch alone (the measurement: the vault's `docs/research/pmj/fill/`).
//! So a rolling leg's IoC takes the touch in force, then the levels held
//! behind it ([`HeldBook::on_depth`], the rolling ingress's top five per
//! side on every change) up to its limit, best first, up to its size, and
//! books ONE fill at the size-weighted price, rounded against the taker
//! ([`sweep`]). The touch stays the gate — no touch in force, or a stale
//! one, fills nothing whatever the levels say — and a level the touch's
//! price or better is never taken from the levels (the touch's own size
//! decides there). Five levels and no more: a size the five do not cover
//! cancels — fewer shares booked, on a losing take as on a winning one.
//! Each level's size is consumed, as the touch's is, so two orders due
//! together do not take one level twice. The levels must be as new as the
//! touch: the depth lane drains after the tick lanes, so when the touch
//! changed after the levels last did (a record of the frame that moved the
//! touch is still in its lane) the order sweeps the touch alone and is
//! counted ([`HeldBook::levels_lagged`]).
//! The Hypercall options keep the touch law: their quote has no depth.
//!
//! PMJ J8 (R-PMJ-S25): slot 11's IoCs are also judged in SHADOW at
//! [`PMJ_SHADOW_DELAYS_NS`] after submit — against the book in force then,
//! consuming nothing, booking nothing ([`HeldBook::submit_shadow`],
//! [`HeldBook::judge_shadows`]) — so the keeper's delay, still an
//! assumption until J0 measures it, can be priced after the fact.
//!
//! Zero allocation: fixed arrays; `const fn new`.

use core_types::pm_roll::{
    pm5_rolling_of, pm_rolling_of, PM5_OFF_MARK, PM5_ROLLING_MAX, PM_OFF_MARK, PM_ROLLING_MAX,
};
use core_types::{DepthLevel, DepthTopK, Side, SymbolId, Tick, DEPTH_FLAG_STALE};

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

/// PMJ J8: the levels held per side of a rolling leg (the depth lane's
/// top K).
pub const HELD_DEPTH_K: usize = core_types::DEPTH_K;

/// PMJ J8 (R-PMJ-S25): slot 11's shadow judgements, ns after submit — the
/// direct CLOB's activation plus hold (350 ms) and one second. (The real
/// verdict is the third, at `pmj.toml paper_delay_ms`.)
pub const PMJ_SHADOW_DELAYS_NS: [u64; 2] = [350_000_000, 1_000_000_000];

/// PMJ J8: shadow judgements pending at once — two per slot-11 IoC, one IoC
/// per 5-minute family.
pub const HELD_SHADOW: usize = PMJ_SHADOW_DELAYS_NS.len() * PM5_ROLLING_MAX;

/// PMJ J8: a rolling leg's levels, best first per side (an empty level is
/// `DepthLevel::EMPTY`, and every level after it is empty too).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HeldLevels {
    /// Bids, best (highest) first.
    pub bids: [DepthLevel; HELD_DEPTH_K],
    /// Asks, best (lowest) first.
    pub asks: [DepthLevel; HELD_DEPTH_K],
}

impl HeldLevels {
    /// No level on either side.
    pub const EMPTY: Self = Self {
        bids: [DepthLevel::EMPTY; HELD_DEPTH_K],
        asks: [DepthLevel::EMPTY; HELD_DEPTH_K],
    };
}

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
    /// PMJ J8: the levels behind each rolling leg's touch (index: quote
    /// index − [`HELD_SYMS`]).
    levels: [HeldLevels; HELD_PM_LEGS],
    /// PMJ J8: shadow judgements pending, and their delay class.
    shadow: [HeldOrder; HELD_SHADOW],
    shadow_class: [u8; HELD_SHADOW],
    shadow_len: usize,
    /// PMJ J8: rolling IoCs that took a level behind the touch.
    swept_deeper: u64,
    /// PMJ J8: when each rolling leg's touch last CHANGED, and when its
    /// levels last arrived (the records' own stamps; index as `levels`).
    touch_chg_ns: [u64; HELD_PM_LEGS],
    levels_ns: [u64; HELD_PM_LEGS],
    /// PMJ J8: each rolling leg's touch as last published (unconsumed — the
    /// change test's reference; `quotes` holds what is left to take).
    touch_last: [HeldQuote; HELD_PM_LEGS],
    /// PMJ J8: rolling IoCs judged while the levels were older than the
    /// touch (swept the touch alone).
    levels_lagged: u64,
    /// PMJ J8: shadow judgements the table could not take.
    shadow_refused: u64,
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
            levels: [HeldLevels::EMPTY; HELD_PM_LEGS],
            shadow: [EMPTY_ORDER; HELD_SHADOW],
            shadow_class: [0; HELD_SHADOW],
            shadow_len: 0,
            swept_deeper: 0,
            touch_chg_ns: [0; HELD_PM_LEGS],
            levels_ns: [0; HELD_PM_LEGS],
            touch_last: [EMPTY_QUOTE; HELD_PM_LEGS],
            levels_lagged: 0,
            shadow_refused: 0,
        }
    }

    /// PMJ J8: rolling IoCs judged while the levels lagged the touch.
    #[inline]
    #[must_use]
    pub const fn levels_lagged(&self) -> u64 {
        self.levels_lagged
    }

    /// PMJ J8: shadow judgements refused (a full table).
    #[inline]
    #[must_use]
    pub const fn shadow_refused(&self) -> u64 {
        self.shadow_refused
    }

    /// PMJ J8: rolling IoCs whose fill took a level behind the touch.
    #[inline]
    #[must_use]
    pub const fn swept_deeper(&self) -> u64 {
        self.swept_deeper
    }

    /// PMJ J8: shadow judgements pending.
    #[inline]
    #[must_use]
    pub const fn shadow_len(&self) -> usize {
        self.shadow_len
    }

    /// PMJ J8: the levels held behind `sym`'s touch (a rolling leg only).
    #[inline]
    #[must_use]
    pub fn levels(&self, sym: SymbolId) -> Option<HeldLevels> {
        match quote_index(sym) {
            Some(k) if k >= HELD_SYMS => Some(self.levels[(k - HELD_SYMS) % HELD_PM_LEGS]),
            _ => None,
        }
    }

    /// PMJ J8: hold a rolling leg's top K per side (any other symbol is
    /// ignored). A record flagged stale clears the leg's levels.
    #[inline]
    pub fn on_depth(&mut self, d: &DepthTopK) {
        let Some(k) = quote_index(d.sym) else {
            return;
        };
        if k < HELD_SYMS {
            return;
        }
        let j = (k - HELD_SYMS) % HELD_PM_LEGS;
        self.levels_ns[j] = d.ts_ns;
        let l = &mut self.levels[j];
        if d.flags & DEPTH_FLAG_STALE != 0 {
            *l = HeldLevels::EMPTY;
            return;
        }
        // COPY: the leg's 2 × 5 levels (160 B) out of the lane's slot into
        // the book — the slot is the producer's again after this call, and
        // the book must hold the levels until the next change — rejected:
        // keeping a reference into the ring (it would be overwritten).
        l.bids = d.bids;
        l.asks = d.asks;
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
            // PMJ J8: a withdrawn touch takes its levels with it.
            if i >= HELD_SYMS {
                let j = (i - HELD_SYMS) % HELD_PM_LEGS;
                self.levels[j] = HeldLevels::EMPTY;
                self.touch_last[j] = EMPTY_QUOTE;
                self.touch_chg_ns[j] = tick.ts_ns;
            }
            return;
        }
        let (b, bq, a, aq) = (
            tick.bid_px.raw(),
            tick.bid_qty.raw(),
            tick.ask_px.raw(),
            tick.ask_qty.raw(),
        );
        let q = HeldQuote {
            bid_1e6: if b > 0 && bq > 0 { b } else { 0 },
            bid_qty_1e6: if b > 0 && bq > 0 { bq } else { 0 },
            ask_1e6: if a > 0 && aq > 0 { a } else { 0 },
            ask_qty_1e6: if a > 0 && aq > 0 { aq } else { 0 },
        };
        // PMJ J8: a rolling leg's touch CHANGE is stamped — against the touch
        // as last published, not what fills left of it (a PONG's
        // re-assertion of the same touch is not a change).
        if i >= HELD_SYMS {
            let j = (i - HELD_SYMS) % HELD_PM_LEGS;
            if q != self.touch_last[j] {
                self.touch_chg_ns[j] = tick.ts_ns;
                self.touch_last[j] = q;
            }
        }
        self.quotes[i] = q;
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
                // PMJ J8: a rolling leg sweeps the touch and the levels.
                Some(k) if k >= HELD_SYMS => {
                    let j = (k - HELD_SYMS) % HELD_PM_LEGS;
                    if self.levels_ns[j] < self.touch_chg_ns[j] {
                        // The levels predate the touch: the touch alone.
                        self.levels_lagged = self.levels_lagged.wrapping_add(1);
                        let mut none = HeldLevels::EMPTY;
                        sweep(o.side, o.px_1e6, o.qty_1e6, &mut self.quotes[k], &mut none).0
                    } else {
                        let (v, deeper) =
                            sweep(o.side, o.px_1e6, o.qty_1e6, &mut self.quotes[k], &mut self.levels[j]);
                        self.swept_deeper = self.swept_deeper.wrapping_add(u64::from(deeper));
                        v
                    }
                }
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

impl HeldBook {
    /// PMJ J8 (R-PMJ-S25): hold a shadow judgement of `o` (its `due_ns` the
    /// shadow instant) in delay class `class` ([`PMJ_SHADOW_DELAYS_NS`]).
    /// `false` when the shadow table is full or `o` is not a rolling leg's.
    pub fn submit_shadow(&mut self, o: HeldOrder, class: u8) -> bool {
        match quote_index(o.sym) {
            Some(k) if k >= HELD_SYMS => {}
            _ => return false,
        }
        if o.qty_1e6 <= 0 || o.px_1e6 <= 0 {
            return false;
        }
        if self.shadow_len >= HELD_SHADOW {
            self.shadow_refused = self.shadow_refused.wrapping_add(1);
            return false;
        }
        self.shadow[self.shadow_len] = o;
        self.shadow_class[self.shadow_len] = class;
        self.shadow_len += 1;
        true
    }

    /// PMJ J8: judge every shadow due by `now_ns` against the book in
    /// force — on copies: a shadow consumes no size and books nothing;
    /// `f(order, class, verdict)` per judgement, in submit order. Call it
    /// before the record at `now_ns` is applied, as [`Self::judge_due`].
    pub fn judge_shadows<F: FnMut(&HeldOrder, u8, Verdict)>(&mut self, now_ns: u64, mut f: F) {
        let mut i = 0usize;
        while i < self.shadow_len {
            let o = self.shadow[i];
            if now_ns < o.due_ns {
                i += 1;
                continue;
            }
            let v = match quote_index(o.sym) {
                Some(k) if k >= HELD_SYMS => {
                    let j = (k - HELD_SYMS) % HELD_PM_LEGS;
                    // COPY: the touch (32 B) and the levels (160 B) onto the
                    // stack — the shadow sweeps copies so it consumes nothing;
                    // cold: twice per slot-11 IoC — rejected: a non-consuming
                    // second sweep (two laws to keep equal).
                    let mut q = self.quotes[k];
                    let mut l = if self.levels_ns[j] < self.touch_chg_ns[j] {
                        HeldLevels::EMPTY
                    } else {
                        self.levels[j]
                    };
                    sweep(o.side, o.px_1e6, o.qty_1e6, &mut q, &mut l).0
                }
                _ => Verdict::Cancel,
            };
            f(&o, self.shadow_class[i], v);
            let mut j = i;
            while j + 1 < self.shadow_len {
                self.shadow[j] = self.shadow[j + 1];
                self.shadow_class[j] = self.shadow_class[j + 1];
                j += 1;
            }
            self.shadow_len -= 1;
            self.shadow[self.shadow_len] = EMPTY_ORDER;
        }
    }
}

/// PMJ J8: a rolling leg's IoC matched as the venue matches a FAK (module
/// docs): the touch in force (`q`), then the levels behind it (`l`) up to
/// `limit`, best first, up to `qty`; ONE fill at the size-weighted price
/// rounded against the taker (up for a buy, down for a sell). Consumes the
/// sizes it takes. The second value: whether a level behind the touch was
/// taken. No touch on the side, or a touch past the limit, fills nothing.
#[must_use]
pub fn sweep(side: Side, limit: i64, qty: i64, q: &mut HeldQuote, l: &mut HeldLevels) -> (Verdict, bool) {
    let (touch, touch_qty, levels) = match side {
        Side::Bid => (q.ask_1e6, &mut q.ask_qty_1e6, &mut l.asks),
        Side::Ask => (q.bid_1e6, &mut q.bid_qty_1e6, &mut l.bids),
    };
    let marketable = |px: i64| match side {
        Side::Bid => px <= limit,
        Side::Ask => px >= limit,
    };
    if touch <= 0 || *touch_qty <= 0 || qty <= 0 || !marketable(touch) {
        return (Verdict::Cancel, false);
    }
    let mut got = if qty < *touch_qty { qty } else { *touch_qty };
    *touch_qty -= got;
    let mut cost = i128::from(got) * i128::from(touch);
    let mut deeper = false;
    let mut k = 0usize;
    while k < HELD_DEPTH_K && got < qty {
        let lv = &mut levels[k];
        k += 1;
        if lv.px_1e6 <= 0 {
            break;
        }
        // A level an earlier order emptied: the next may still be there.
        if lv.qty_1e6 <= 0 {
            continue;
        }
        // The touch's price or better: the touch's own size decided there.
        let behind = match side {
            Side::Bid => lv.px_1e6 > touch,
            Side::Ask => lv.px_1e6 < touch,
        };
        if !behind {
            continue;
        }
        if !marketable(lv.px_1e6) {
            break;
        }
        let want = qty - got;
        let take = if want < lv.qty_1e6 { want } else { lv.qty_1e6 };
        lv.qty_1e6 -= take;
        got += take;
        cost += i128::from(take) * i128::from(lv.px_1e6);
        deeper = true;
    }
    let g = i128::from(got);
    let px = match side {
        Side::Bid => (cost + g - 1) / g,
        Side::Ask => cost / g,
    } as i64;
    (Verdict::Fill { px_1e6: px, qty_1e6: got }, deeper)
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

    fn pm_leg_tick(leg: SymbolId, bid: i64, bq: i64, ask: i64, aq: i64) -> Tick {
        Tick::new_stamped(
            1,
            VenueId::Polymarket,
            leg,
            1,
            Price::from_raw(bid),
            Qty::from_raw(bq),
            Price::from_raw(ask),
            Qty::from_raw(aq),
            0,
            0,
        )
    }

    fn depth(leg: SymbolId, bids: &[(i64, i64)], asks: &[(i64, i64)], flags: u8) -> DepthTopK {
        let mut b = [DepthLevel::EMPTY; HELD_DEPTH_K];
        let mut a = [DepthLevel::EMPTY; HELD_DEPTH_K];
        let mut i = 0usize;
        while i < bids.len() {
            b[i] = DepthLevel { px_1e6: bids[i].0, qty_1e6: bids[i].1 };
            i += 1;
        }
        let mut i = 0usize;
        while i < asks.len() {
            a[i] = DepthLevel { px_1e6: asks[i].0, qty_1e6: asks[i].1 };
            i += 1;
        }
        DepthTopK::new(1, VenueId::Polymarket, leg, flags, b, a)
    }

    /// PMJ J8: a rolling leg's IoC takes the touch, then the levels behind
    /// it up to its limit, at the size-weighted price rounded against it;
    /// the sizes it took are gone for the next order due at that instant.
    #[test]
    fn a_rolling_leg_sweeps_the_levels_behind_the_touch_up_to_its_limit() {
        use core_types::pm_roll::pm_rolling_sym;
        let leg = pm_rolling_sym(2, 1, 0);
        let mut b = HeldBook::new();
        b.on_quote(&pm_leg_tick(leg, 900_000, 50_000_000, 940_000, 10_000_000));
        b.on_depth(&depth(
            leg,
            &[(900_000, 50_000_000)],
            &[(940_000, 10_000_000), (950_000, 20_000_000), (960_000, 30_000_000), (980_000, 100_000_000)],
            0,
        ));
        let buy = |qty| HeldOrder::new(5, leg, Side::Bid, 960_000, qty, 1, 9, VenueId::Polymarket as u8);
        assert!(b.submit(buy(100_000_000)));
        assert!(b.submit(buy(100_000_000)));
        let mut got = [None, None];
        let mut n = 0usize;
        b.judge_due(5, |_, v| {
            got[n] = Some(v);
            n += 1;
        });
        // 10 @ 0.94 + 20 @ 0.95 + 30 @ 0.96 = 57.2 for 60: 0.953333… → 0.953334.
        assert_eq!(got[0], Some(Verdict::Fill { px_1e6: 953_334, qty_1e6: 60_000_000 }));
        assert_eq!(got[1], Some(Verdict::Cancel), "the touch and the levels it reached are spent");
        assert_eq!(b.swept_deeper(), 1);
        // A sell mirrors it on the bids, rounded down.
        b.on_quote(&pm_leg_tick(leg, 900_000, 10_000_000, 940_000, 10_000_000));
        b.on_depth(&depth(leg, &[(900_000, 10_000_000), (890_000, 5_000_000), (870_000, 9_000_000)], &[], 0));
        assert!(b.submit(HeldOrder::new(6, leg, Side::Ask, 880_000, 30_000_000, 2, 9, VenueId::Polymarket as u8)));
        let mut v = None;
        b.judge_due(6, |_, x| v = Some(x));
        // 10 @ 0.90 + 5 @ 0.89 = 13.45 for 15: 0.896666… → 0.896666.
        assert_eq!(v, Some(Verdict::Fill { px_1e6: 896_666, qty_1e6: 15_000_000 }));
    }

    /// PMJ J8: a captured book (the vault's `docs/research/pmj/fill/`) — a
    /// 239.76-share BUY at 0.97 against 0.76 × 10 then 0.79 × 10, 0.82 × 7,
    /// 0.83 × 242.3, 0.84 × 7: the FAK takes it all, at the size-weighted
    /// 0.825121.
    #[test]
    fn the_captured_sol_take_fills_whole_across_the_levels() {
        use core_types::pm_roll::pm_rolling_sym;
        let leg = pm_rolling_sym(2, 0, 1);
        let mut b = HeldBook::new();
        b.on_quote(&pm_leg_tick(leg, 580_000, 10_000_000, 760_000, 10_000_000));
        b.on_depth(&depth(
            leg,
            &[(580_000, 10_000_000)],
            &[
                (760_000, 10_000_000),
                (790_000, 10_000_000),
                (820_000, 7_000_000),
                (830_000, 242_300_000),
                (840_000, 7_000_000),
            ],
            0,
        ));
        assert!(b.submit(HeldOrder::new(1, leg, Side::Bid, 970_000, 239_760_000, 1, 9, VenueId::Polymarket as u8)));
        let mut v = None;
        b.judge_due(1, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Fill { px_1e6: 825_121, qty_1e6: 239_760_000 }));
    }

    /// PMJ J8: the touch is the gate — none in force, a stale one, or one
    /// past the limit fills nothing whatever the levels say; a level at the
    /// touch's price or better is never taken from the levels; a stale
    /// depth record, or a withdrawn touch, clears the levels.
    #[test]
    fn the_touch_gates_the_sweep_and_a_stale_record_clears_the_levels() {
        use core_types::pm_roll::pm5_rolling_sym;
        let leg = pm5_rolling_sym(1, 2, 1);
        let mut b = HeldBook::new();
        let lv = depth(leg, &[], &[(930_000, 500_000_000), (950_000, 500_000_000)], 0);
        b.on_depth(&lv);
        let buy = |due, px| HeldOrder::new(due, leg, Side::Bid, px, 100_000_000, 3, 11, VenueId::Jupiter as u8);
        assert!(b.submit(buy(1, 990_000)));
        let mut v = None;
        b.judge_due(1, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Cancel), "no touch in force");
        // The touch past the limit: nothing, though a level is under it.
        b.on_quote(&pm_leg_tick(leg, 0, 0, 960_000, 1_000_000));
        assert!(b.submit(buy(2, 955_000)));
        b.judge_due(2, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Cancel));
        // A level better than the touch is not taken from the levels.
        assert!(b.submit(buy(3, 960_000)));
        b.judge_due(3, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Fill { px_1e6: 960_000, qty_1e6: 1_000_000 }));
        // A stale depth record clears the levels; so does a withdrawn touch.
        b.on_depth(&lv);
        assert_eq!(b.levels(leg).map(|l| l.asks[0].px_1e6), Some(930_000));
        b.on_depth(&depth(leg, &[], &[], DEPTH_FLAG_STALE));
        assert_eq!(b.levels(leg), Some(HeldLevels::EMPTY));
        b.on_depth(&lv);
        let mut down = pm_leg_tick(leg, 0, 0, 0, 0);
        down.flags = core_types::TICK_FLAG_STALE;
        b.on_quote(&down);
        assert_eq!(b.levels(leg), Some(HeldLevels::EMPTY));
        // Not a rolling leg: no levels.
        b.on_depth(&depth(sym(3), &[], &[(1, 1)], 0));
        assert_eq!(b.levels(sym(3)), None);
    }

    /// PMJ J8 (R-PMJ-S25): a shadow is judged at its own instant against
    /// the book then, takes nothing and books nothing — the real order due
    /// later, with no record in between, still sweeps the whole size.
    #[test]
    fn a_shadow_judgement_consumes_nothing() {
        use core_types::pm_roll::pm5_rolling_sym;
        let leg = pm5_rolling_sym(0, 0, 0);
        let mut b = HeldBook::new();
        b.on_quote(&pm_leg_tick(leg, 0, 0, 940_000, 50_000_000));
        b.on_depth(&depth(leg, &[], &[(940_000, 50_000_000), (950_000, 30_000_000)], 0));
        let (q0, l0) = (b.quote(leg), b.levels(leg));
        let o = |due| HeldOrder::new(due, leg, Side::Bid, 960_000, 70_000_000, 4, 11, VenueId::Jupiter as u8);
        assert!(b.submit(o(2_000)));
        assert!(b.submit_shadow(o(350), 0) && b.submit_shadow(o(1_000), 1));
        assert!(!b.submit_shadow(HeldOrder::new(1, sym(0), Side::Bid, 1, 1, 4, 11, 9), 0), "an option has no shadow");
        // 50 @ 0.94 + 20 @ 0.95 = 66 for 70: 0.942857… → 0.942858.
        let want = Verdict::Fill { px_1e6: 942_858, qty_1e6: 70_000_000 };
        let mut seen = [None, None];
        b.judge_shadows(999, |_, c, v| seen[c as usize] = Some(v));
        assert_eq!(seen, [Some(want), None]);
        assert_eq!((b.quote(leg), b.levels(leg)), (q0, l0), "the shadow took nothing");
        b.judge_shadows(1_000, |_, c, v| seen[c as usize] = Some(v));
        assert_eq!(seen[1], Some(want));
        assert_eq!((b.quote(leg), b.levels(leg)), (q0, l0));
        assert_eq!(b.shadow_len(), 0);
        let mut real = None;
        b.judge_due(2_000, |_, v| real = Some(v));
        assert_eq!(real, Some(want), "the real order sweeps the whole size");
        // A shadow reads the book at its own instant.
        b.on_quote(&pm_leg_tick(leg, 0, 0, 990_000, 50_000_000));
        assert!(b.submit_shadow(o(3_000), 1));
        b.judge_shadows(3_000, |_, c, v| seen[c as usize] = Some(v));
        assert_eq!(seen[1], Some(Verdict::Cancel), "the ask past the limit");
        // A full table refuses, counted.
        let mut i = 0usize;
        while i < HELD_SHADOW {
            assert!(b.submit_shadow(o(9), 0));
            i += 1;
        }
        assert!(!b.submit_shadow(o(9), 0));
        assert_eq!(b.shadow_refused(), 1);
    }

    /// PMJ J8: levels older than the touch (the depth lane drains after the
    /// tick lanes) are not swept — the touch alone, counted; the depth
    /// record of the same frame restores the sweep.
    #[test]
    fn levels_older_than_the_touch_sweep_the_touch_alone() {
        use core_types::pm_roll::pm_rolling_sym;
        let leg = pm_rolling_sym(0, 0, 0);
        let mut b = HeldBook::new();
        let mut t = pm_leg_tick(leg, 0, 0, 940_000, 10_000_000);
        t.ts_ns = 100;
        b.on_quote(&t);
        let mut d = depth(leg, &[], &[(940_000, 10_000_000), (950_000, 90_000_000)], 0);
        d.ts_ns = 100;
        b.on_depth(&d);
        // The touch moves at 200; its depth record is still in the lane.
        t.ts_ns = 200;
        t.ask_qty = Qty::from_raw(5_000_000);
        b.on_quote(&t);
        let buy = HeldOrder::new(250, leg, Side::Bid, 960_000, 50_000_000, 1, 9, VenueId::Polymarket as u8);
        assert!(b.submit(buy));
        let mut v = None;
        b.judge_due(250, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Fill { px_1e6: 940_000, qty_1e6: 5_000_000 }));
        assert_eq!((b.levels_lagged(), b.swept_deeper()), (1, 0));
        // A PONG re-asserting the same touch later is no change.
        t.ts_ns = 300;
        b.on_quote(&t);
        d.ts_ns = 200;
        d.asks[0].qty_1e6 = 5_000_000;
        b.on_depth(&d);
        assert!(b.submit(HeldOrder::new(400, leg, Side::Bid, 960_000, 50_000_000, 2, 9, VenueId::Polymarket as u8)));
        b.judge_due(400, |_, x| v = Some(x));
        assert_eq!(v, Some(Verdict::Fill { px_1e6: 949_000, qty_1e6: 50_000_000 }));
        assert_eq!((b.levels_lagged(), b.swept_deeper()), (1, 1));
    }
}
