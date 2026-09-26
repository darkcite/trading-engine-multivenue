// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # The queue law (XMM XH2) — a post-only order in a price-time queue
//!
//! The crate root's maker law is STRICT CROSS: a resting order fills only
//! when the book trades through it, because resting AT the touch puts an
//! unknowable queue ahead of us. That is the right law for a member that
//! only ever wants a conservative bound. A market maker cannot be judged
//! by it — every one of its fills is AT the touch — so an order flagged
//! [`core_types::ORDER_FLAG_POST_ONLY`] on a price-time venue
//! (Hyperliquid perps) is judged here instead, by the queue it joined.
//!
//! One law, two consumers, exactly as the root law: the offline harness
//! (`cli::backtest::fill`) and the engine's paper matcher
//! (`clob_dispatcher::PaperMatcher`). Each calls [`QueueBook`] on its own
//! clock — the capture's in the harness, the engine's live — and supplies
//! its own `Δ`; the law only compares times it is given.
//!
//! ## The law
//!
//! * **Time.** An action emitted at `t` (a place, a cancel, a modify)
//!   takes effect at the FIRST record of its symbol at or after `t + Δ` —
//!   on Hyperliquid, the first block that can include it. The caller
//!   passes `t + Δ`; [`QueueBook`] waits for the record.
//! * **The book an arrival meets** is the book as it stood BEFORE that
//!   record's block — the last touch fed before it — because the venue
//!   processes a post-only action at the start of its block. So the
//!   landing itself is the same whichever of a book record and a print
//!   carrying one instant is fed first. (What the record's touch then
//!   resolves — an UNKNOWN queue reaching the touch — a same-instant
//!   print can fill only when the book came first; both consumers feed a
//!   record's book before its prints.) Any record of the symbol lands
//!   what is due — a stale or one-sided book too (its touch is not
//!   learnt) — so a cancel or an expiry is never held back by a quiet
//!   or degraded feed.
//! * **A block is the venue's (XMM XH3).** Hyperliquid processes a
//!   cancel in the next block whether or not that perp's book changes,
//!   so any record of the venue — any of its symbols, tracked or not —
//!   also lands what is due on every OTHER tracked symbol of that venue,
//!   against that symbol's last known book. A quiet book never holds a
//!   cancel back. The parity switch keeps the simulator's per-symbol
//!   clock (only the record's own symbol advances).
//! * **Order inside a block.** Placements, then cancels, then the block's
//!   prints: Hyperliquid sequences non-crossing ALO actions and cancels
//!   ahead of IOC/GTC in a block, so a cancel that lands in a block beats
//!   that block's takers, and a placement can be filled by them.
//! * **Activation (post-only).** An order that would cross — a bid at or
//!   above the best ask, an ask at or below the best bid — is REJECTED
//!   (`BAD_ALO_PX`) and never rests. Otherwise it rests, and its queue
//!   ahead is:
//!   * at the touch — the displayed size there;
//!   * improving the touch (a new level) — zero;
//!   * behind the touch, or with no touch known yet — UNKNOWN, until its
//!     price becomes the touch; then the size displayed at that moment.
//!     Conservative: that counts orders that joined after ours.
//! * **Prints.** A print of the opposite aggressor AT our price consumes
//!   the queue ahead, then fills us — partial fills allowed. A print
//!   THROUGH our price fills the whole remainder: our level was exhausted
//!   first. A print at our price while our queue is UNKNOWN fills nothing.
//!   Cancels ahead of us are ignored (conservative: slower fills).
//! * **Fill price** is always our limit — the maker is the one taken.
//! * **Modify** = a cancel of the old order and a new order at the new
//!   price, both landing in the same block: the new order goes to the
//!   BACK of its queue, and inherits the old one's expiry (LAW E-7).
//!   **Fail-closed:** a modify of an order the book no longer holds is
//!   refused, and a replacement whose predecessor left the book (filled
//!   or cancelled) before the modify landed is REJECTED on landing
//!   (`OTHER`) — the venue does not modify a filled or cancelled order.
//!   So one quote decision can never fill twice. (Whether Hyperliquid
//!   keeps priority on a size-only modify is XH5's measurement.)
//! * **Expiry** (the engine TTL, LAW E-8) is a cancel scheduled at the
//!   instant the caller computed; it lands like any other.
//! * **Own orders at one level.** Each of our orders counts the size
//!   displayed ahead of it when it joined, so two of ours at the same
//!   price count the others ahead of the first twice. Conservative, and
//!   rare: a member holds one order per side, two only while a modify is
//!   in flight, and a modify moves the price.
//! * **Research switch [`QueueBook::parity_sim`]** (the parity gate only):
//!   the XMM simulator's arrival rule — an arrival meets the touch OF its
//!   landing record (the consumer feeds a record's book before its
//!   prints), and an order whose price is not exactly the touch on its
//!   side there is dropped (reason `OTHER`).
//!
//! ## Doctrine
//!
//! No allocation, no floats, no panics in release: fixed arrays,
//! `while`-index loops, `debug_assert!` on the invariants. Every event
//! the book produces waits in a fixed ring for [`QueueBook::try_next_event`].

use core_types::{
    Side, ORDER_EVENT_CANCELED, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_BAD_ALO_PX,
    ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_EVENT_REASON_EXPIRED, ORDER_EVENT_REASON_NONE,
    ORDER_EVENT_REASON_OTHER, ORDER_EVENT_REASON_REPLACED, ORDER_EVENT_REJECTED,
    ORDER_EVENT_RESTING,
};

use crate::Touch;

/// Orders one [`QueueBook`] holds: eight perps × two sides × a modify in
/// flight on each.
pub const QUEUE_MAX_ORDERS: usize = 32;
/// Symbols one [`QueueBook`] tracks a touch for.
pub const QUEUE_MAX_SYMS: usize = 8;
/// Events waiting for [`QueueBook::try_next_event`]. One call emits at most
/// three per order (it lands, a fill, FILLED), so a consumer that drains
/// after every call can never overflow it.
pub const QUEUE_OUT_CAP: usize = 3 * QUEUE_MAX_ORDERS;
/// [`QueueOrder::ahead_1e6`] when the queue ahead is not known: behind
/// the touch, or no touch seen yet.
pub const QUEUE_AHEAD_UNKNOWN: i64 = -1;
/// "Never" for a scheduled cancel.
pub const QUEUE_NEVER: u64 = u64::MAX;

/// [`QueueEvent::kind`] of a fill. The other kinds are
/// `core_types::ORDER_EVENT_*` verbatim, so a consumer maps them one to
/// one onto an `OrderEvent`.
pub const QUEUE_EVENT_FILL: u8 = 0x10;

/// Emitted, not yet at the venue.
const ST_PENDING: u8 = 1;
/// At the venue, in its queue (or behind the touch).
const ST_RESTING: u8 = 2;

/// One post-only order the book is holding. One cache line.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct QueueOrder {
    /// The member's client id (unique within its slot).
    pub client_oid: u64,
    /// Limit price ×1e6.
    pub px_1e6: i64,
    /// Unfilled remainder ×1e6 (> 0 while held).
    pub remaining_1e6: i64,
    /// Displayed size ahead of us at our price ×1e6, or
    /// [`QUEUE_AHEAD_UNKNOWN`].
    pub ahead_1e6: i64,
    /// The instant the placement lands (`emit + Δ`): effective at the
    /// first record of `sym` at or after it.
    pub ready_ns: u64,
    /// The instant a scheduled cancel lands, or [`QUEUE_NEVER`].
    pub cancel_ns: u64,
    /// Instrument.
    pub sym: u32,
    /// Resting side.
    pub side: Side,
    /// The emitting strategy slot (the client id's namespace).
    pub slot: u8,
    /// `ST_PENDING` / `ST_RESTING`.
    state: u8,
    /// `ORDER_EVENT_REASON_*` the scheduled cancel will carry.
    cancel_reason: u8,
    /// The engine TTL's instant ([`QUEUE_NEVER`] = none) — what a modify
    /// inherits (LAW E-7: a reprice cannot extend a quote's life).
    pub expiry_ns: u64,
}

const _: () = assert!(::core::mem::size_of::<QueueOrder>() == 64);

const EMPTY_ORDER: QueueOrder = QueueOrder {
    client_oid: 0,
    px_1e6: 0,
    remaining_1e6: 0,
    ahead_1e6: 0,
    ready_ns: 0,
    cancel_ns: QUEUE_NEVER,
    sym: 0,
    side: Side::Bid,
    slot: 0,
    state: 0,
    cancel_reason: 0,
    expiry_ns: QUEUE_NEVER,
};

/// A placement request.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct QueuePlace {
    /// The member's client id.
    pub client_oid: u64,
    /// Instrument.
    pub sym: u32,
    /// Side.
    pub side: Side,
    /// Emitting slot.
    pub slot: u8,
    /// Limit price ×1e6 (> 0).
    pub px_1e6: i64,
    /// Size ×1e6 (> 0).
    pub qty_1e6: i64,
    /// When the placement lands: `emit + Δ`.
    pub ready_ns: u64,
    /// When the engine TTL's cancel lands, or [`QUEUE_NEVER`]. Not read
    /// by [`QueueBook::modify`], which inherits the old order's.
    pub expiry_ns: u64,
}

/// Why the book did not take an action.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum QueueRefusal {
    /// A non-positive price or size.
    Invalid,
    /// [`QUEUE_MAX_ORDERS`] are held already.
    Full,
    /// A new symbol and [`QUEUE_MAX_SYMS`] are tracked already.
    SymsFull,
    /// No held order of that slot carries that client id — already
    /// filled, rejected or cancelled: a race, not a fault.
    NoSuchOrder,
    /// More than one does, so the request names no single order.
    Ambiguous,
}

/// One thing that happened to one order.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct QueueEvent {
    /// The record instant it happened at (the caller's clock).
    pub t_ns: u64,
    /// The order's client id.
    pub client_oid: u64,
    /// Our limit price ×1e6 (a fill's price).
    pub px_1e6: i64,
    /// Filled size ×1e6 for [`QUEUE_EVENT_FILL`]; 0 otherwise.
    pub qty_1e6: i64,
    /// Instrument.
    pub sym: u32,
    /// The order's side.
    pub side: Side,
    /// The order's slot.
    pub slot: u8,
    /// [`QUEUE_EVENT_FILL`] or a `core_types::ORDER_EVENT_*` kind.
    pub kind: u8,
    /// `core_types::ORDER_EVENT_REASON_*`.
    pub reason: u8,
}

const EMPTY_EVENT: QueueEvent = QueueEvent {
    t_ns: 0,
    client_oid: 0,
    px_1e6: 0,
    qty_1e6: 0,
    sym: 0,
    side: Side::Bid,
    slot: 0,
    kind: 0,
    reason: 0,
};

/// What the book did, cumulatively.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueCounters {
    /// Placements taken.
    pub placed: u64,
    /// Placements refused (invalid, full, too many symbols).
    pub refused: u64,
    /// Orders that landed and rest.
    pub rested: u64,
    /// Orders rejected at activation for crossing (`BAD_ALO_PX`).
    pub rejected_alo: u64,
    /// Orders dropped by [`QueueBook::parity_sim`] (research only).
    pub dropped_touch: u64,
    /// Orders cancelled (requested, replaced or expired).
    pub canceled: u64,
    /// Fill events.
    pub fills: u64,
    /// Orders filled in full.
    pub filled: u64,
    /// Events dropped because the ring was full. Must stay 0.
    pub out_overflow: u64,
    /// Replacements rejected on landing because their predecessor had
    /// already left the book (the fail-closed modify).
    pub stale_modify: u64,
}

/// The last touch seen for one symbol.
#[derive(Copy, Clone, Debug)]
struct SymTouch {
    sym: u32,
    known: bool,
    touch: Touch,
}

const EMPTY_SYM: SymTouch = SymTouch {
    sym: 0,
    known: false,
    touch: Touch {
        bid_1e6: 0,
        ask_1e6: 0,
        bid_qty_1e6: 0,
        ask_qty_1e6: 0,
    },
};

/// The queue law's state: the held orders in emit order (FIFO priority
/// among our own orders), the last touch per symbol, and the event ring.
/// Boot-constructed; nothing allocates afterwards.
#[repr(C, align(64))]
pub struct QueueBook {
    orders: [QueueOrder; QUEUE_MAX_ORDERS],
    len: usize,
    syms: [SymTouch; QUEUE_MAX_SYMS],
    n_syms: usize,
    out: [QueueEvent; QUEUE_OUT_CAP],
    out_head: usize,
    out_len: usize,
    /// Research switch (parity gate only): the XMM simulator's arrival
    /// rule — see the module doc.
    pub parity_sim: bool,
    /// What the book did.
    pub counters: QueueCounters,
    /// Per held order (same index as `orders`): the client id a modify
    /// replaces with it, 0 = a plain placement. Checked on landing.
    links: [u64; QUEUE_MAX_ORDERS],
}

const _: () = assert!(::core::mem::size_of::<QueueEvent>() == 40);
const _: () = assert!(::core::mem::size_of::<QueueBook>() <= 8 * 1024);

impl Default for QueueBook {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueBook {
    /// An empty book.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            orders: [EMPTY_ORDER; QUEUE_MAX_ORDERS],
            len: 0,
            syms: [EMPTY_SYM; QUEUE_MAX_SYMS],
            n_syms: 0,
            out: [EMPTY_EVENT; QUEUE_OUT_CAP],
            out_head: 0,
            out_len: 0,
            parity_sim: false,
            counters: QueueCounters {
                placed: 0,
                refused: 0,
                rested: 0,
                rejected_alo: 0,
                dropped_touch: 0,
                canceled: 0,
                fills: 0,
                filled: 0,
                out_overflow: 0,
                stale_modify: 0,
            },
            links: [0; QUEUE_MAX_ORDERS],
        }
    }

    /// Orders held (pending or resting).
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is held.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The held order carrying `client_oid` in `slot`, if exactly one does.
    #[must_use]
    pub fn get(&self, client_oid: u64, slot: u8) -> Option<QueueOrder> {
        match self.find(client_oid, slot) {
            Ok(i) => Some(self.orders[i]),
            Err(_) => None,
        }
    }

    /// The held order carrying `client_oid` in `slot`, or why there is
    /// no single one ([`QueueRefusal::NoSuchOrder`] /
    /// [`QueueRefusal::Ambiguous`]).
    pub fn lookup(&self, client_oid: u64, slot: u8) -> Result<QueueOrder, QueueRefusal> {
        match self.find(client_oid, slot) {
            Ok(i) => Ok(self.orders[i]),
            Err(e) => Err(e),
        }
    }

    /// Whether the held order `client_oid` of `slot` has landed and rests.
    #[must_use]
    pub fn is_resting(&self, client_oid: u64, slot: u8) -> bool {
        match self.find(client_oid, slot) {
            Ok(i) => self.orders[i].state == ST_RESTING,
            Err(_) => false,
        }
    }

    /// Start tracking the touch of `sym`, so the first order placed on it
    /// meets a known book. A consumer tracks, at boot, every instrument a
    /// queue member may trade: book records of an untracked symbol are
    /// ignored, so an order whose symbol [`Self::place`] had to track
    /// itself meets no book on arrival (queue UNKNOWN, no post-only
    /// check). Idempotent.
    pub fn track(&mut self, sym: u32) -> Result<(), QueueRefusal> {
        if self.sym_index(sym).is_some() {
            return Ok(());
        }
        if self.n_syms >= QUEUE_MAX_SYMS {
            return Err(QueueRefusal::SymsFull);
        }
        self.syms[self.n_syms] = SymTouch {
            sym,
            known: false,
            touch: EMPTY_SYM.touch,
        };
        self.n_syms += 1;
        Ok(())
    }

    /// Take a placement. It lands at the first record of its symbol at or
    /// after `p.ready_ns`.
    pub fn place(&mut self, p: &QueuePlace) -> Result<(), QueueRefusal> {
        if p.px_1e6 <= 0 || p.qty_1e6 <= 0 {
            self.counters.refused = self.counters.refused.wrapping_add(1);
            return Err(QueueRefusal::Invalid);
        }
        if self.len >= QUEUE_MAX_ORDERS {
            self.counters.refused = self.counters.refused.wrapping_add(1);
            return Err(QueueRefusal::Full);
        }
        if let Err(e) = self.track(p.sym) {
            self.counters.refused = self.counters.refused.wrapping_add(1);
            return Err(e);
        }
        self.orders[self.len] = QueueOrder {
            client_oid: p.client_oid,
            px_1e6: p.px_1e6,
            remaining_1e6: p.qty_1e6,
            ahead_1e6: QUEUE_AHEAD_UNKNOWN,
            ready_ns: p.ready_ns,
            cancel_ns: p.expiry_ns,
            sym: p.sym,
            side: p.side,
            slot: p.slot,
            state: ST_PENDING,
            cancel_reason: ORDER_EVENT_REASON_EXPIRED,
            expiry_ns: p.expiry_ns,
        };
        self.links[self.len] = 0;
        self.len += 1;
        self.counters.placed = self.counters.placed.wrapping_add(1);
        Ok(())
    }

    /// Schedule a cancel of `client_oid` of `slot`, landing at the first
    /// record at or after `effective_ns`. An earlier scheduled cancel (an
    /// expiry that lands first) is kept, with its own reason.
    pub fn cancel(
        &mut self,
        client_oid: u64,
        slot: u8,
        effective_ns: u64,
    ) -> Result<(), QueueRefusal> {
        self.schedule_cancel(
            client_oid,
            slot,
            effective_ns,
            ORDER_EVENT_REASON_CANCEL_REQUESTED,
        )
    }

    /// Replace `prev_oid` of `new.slot` with `new`: a cancel of the old
    /// order and the new placement, both landing at `new.ready_ns` — the
    /// new order at the BACK of its queue. The new order INHERITS the old
    /// one's expiry (`new.expiry_ns` is not read): LAW E-7, a reprice
    /// cannot extend a quote's life. Refused whole: when the new order
    /// cannot be held, the old one is left exactly as it was.
    pub fn modify(&mut self, prev_oid: u64, new: &QueuePlace) -> Result<(), QueueRefusal> {
        if new.px_1e6 <= 0 || new.qty_1e6 <= 0 {
            self.counters.refused = self.counters.refused.wrapping_add(1);
            return Err(QueueRefusal::Invalid);
        }
        let i = self.find(prev_oid, new.slot)?;
        if self.len >= QUEUE_MAX_ORDERS {
            self.counters.refused = self.counters.refused.wrapping_add(1);
            return Err(QueueRefusal::Full);
        }
        let o = &mut self.orders[i];
        if new.ready_ns < o.cancel_ns {
            o.cancel_ns = new.ready_ns;
            o.cancel_reason = ORDER_EVENT_REASON_REPLACED;
        }
        let inherited = QueuePlace {
            expiry_ns: o.expiry_ns,
            ..*new
        };
        // The symbol is the old order's, so it is tracked already.
        self.place(&inherited)?;
        self.links[self.len - 1] = prev_oid;
        Ok(())
    }

    /// A book update for `sym` at `t_ns`: land what is due in this
    /// record's block against the book before it, remember the touch
    /// (two-sided only — a one-sided book says nothing about crossing),
    /// and let an order behind the touch join its queue when the touch
    /// comes to its price. Under [`Self::parity_sim`] the touch is taken
    /// first, so an arrival meets this record's own touch.
    pub fn on_book(&mut self, sym: u32, touch: Touch, t_ns: u64) {
        self.advance_venue(sym, t_ns);
        let Some(k) = self.sym_index(sym) else {
            return;
        };
        let two_sided = touch.two_sided();
        if self.parity_sim && two_sided {
            self.syms[k].touch = touch;
            self.syms[k].known = true;
        }
        self.advance(sym, t_ns);
        if !self.parity_sim && two_sided {
            self.syms[k].touch = touch;
            self.syms[k].known = true;
        }
        if !self.syms[k].known {
            return;
        }
        let tb = self.syms[k].touch;
        let mut i = 0usize;
        while i < self.len {
            let o = &mut self.orders[i];
            if o.sym == sym && o.state == ST_RESTING && o.ahead_1e6 == QUEUE_AHEAD_UNKNOWN {
                match o.side {
                    Side::Bid if o.px_1e6 == tb.bid_1e6 => o.ahead_1e6 = tb.bid_qty_1e6,
                    Side::Ask if o.px_1e6 == tb.ask_1e6 => o.ahead_1e6 = tb.ask_qty_1e6,
                    _ => {}
                }
            }
            i += 1;
        }
    }

    /// A print on `sym` at `t_ns`: land what is due in this record's block
    /// first (placements before the block's takers, cancels too), then
    /// apply it. `sell_aggressor` = the taker sold (hit the bids).
    pub fn on_print(
        &mut self,
        sym: u32,
        px_1e6: i64,
        qty_1e6: i64,
        sell_aggressor: bool,
        t_ns: u64,
    ) {
        self.advance_venue(sym, t_ns);
        if self.sym_index(sym).is_none() {
            return;
        }
        self.advance(sym, t_ns);
        if px_1e6 <= 0 || qty_1e6 <= 0 {
            return;
        }
        let hit = if sell_aggressor { Side::Bid } else { Side::Ask };
        let mut left = qty_1e6;
        let mut i = 0usize;
        while i < self.len {
            let o = self.orders[i];
            if o.sym != sym || o.state != ST_RESTING || o.side != hit {
                i += 1;
                continue;
            }
            let through = match hit {
                Side::Bid => px_1e6 < o.px_1e6,
                Side::Ask => px_1e6 > o.px_1e6,
            };
            let fill = if through {
                o.remaining_1e6
            } else if px_1e6 == o.px_1e6 && o.ahead_1e6 != QUEUE_AHEAD_UNKNOWN {
                let take = if o.ahead_1e6 < left {
                    o.ahead_1e6
                } else {
                    left
                };
                self.orders[i].ahead_1e6 = o.ahead_1e6 - take;
                left -= take;
                let f = if o.remaining_1e6 < left {
                    o.remaining_1e6
                } else {
                    left
                };
                left -= f;
                f
            } else {
                0
            };
            if fill <= 0 {
                i += 1;
                continue;
            }
            debug_assert!(
                fill <= o.remaining_1e6,
                "a fill never exceeds the remainder"
            );
            self.emit(t_ns, &o, QUEUE_EVENT_FILL, ORDER_EVENT_REASON_NONE, fill);
            self.counters.fills = self.counters.fills.wrapping_add(1);
            let rem = o.remaining_1e6 - fill;
            if rem > 0 {
                self.orders[i].remaining_1e6 = rem;
                i += 1;
            } else {
                self.emit(t_ns, &o, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_NONE, 0);
                self.counters.filled = self.counters.filled.wrapping_add(1);
                self.remove(i);
            }
        }
    }

    /// Pop the next event, FIFO.
    pub fn try_next_event(&mut self) -> Option<QueueEvent> {
        if self.out_len == 0 {
            return None;
        }
        let e = self.out[self.out_head];
        self.out_head = (self.out_head + 1) % QUEUE_OUT_CAP;
        self.out_len -= 1;
        Some(e)
    }

    /// XMM XH3: a record of `record_sym` at `t_ns` is a block of its whole
    /// venue — land what is due on every OTHER tracked symbol of that
    /// venue (against its last known book). Off under the parity switch
    /// (the simulator's per-symbol clock). At most 8 symbols × 32 orders.
    #[inline]
    fn advance_venue(&mut self, record_sym: u32, t_ns: u64) {
        if self.parity_sim || self.len == 0 {
            return;
        }
        let venue = core_types::symbol_venue_byte(record_sym);
        let mut k = 0usize;
        while k < self.n_syms {
            let sym = self.syms[k].sym;
            if sym != record_sym && core_types::symbol_venue_byte(sym) == venue {
                self.advance(sym, t_ns);
            }
            k += 1;
        }
    }

    /// Land everything of `sym` due at or before `t_ns`: placements
    /// first, then cancels — the block's own order.
    fn advance(&mut self, sym: u32, t_ns: u64) {
        let Some(k) = self.sym_index(sym) else {
            return;
        };
        let known = self.syms[k].known;
        let tb = self.syms[k].touch;
        let mut i = 0usize;
        while i < self.len {
            let o = self.orders[i];
            if o.sym != sym || o.state != ST_PENDING || o.ready_ns > t_ns {
                i += 1;
                continue;
            }
            // Fail-closed modify: its predecessor must still be held (it
            // leaves in this same block, cancels after placements).
            let link = self.links[i];
            if link != 0 && self.find(link, o.slot).is_err() {
                self.emit(t_ns, &o, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_OTHER, 0);
                self.counters.stale_modify = self.counters.stale_modify.wrapping_add(1);
                self.remove(i);
                continue;
            }
            let crosses = known
                && match o.side {
                    Side::Bid => o.px_1e6 >= tb.ask_1e6,
                    Side::Ask => o.px_1e6 <= tb.bid_1e6,
                };
            if crosses {
                self.emit(
                    t_ns,
                    &o,
                    ORDER_EVENT_REJECTED,
                    ORDER_EVENT_REASON_BAD_ALO_PX,
                    0,
                );
                self.counters.rejected_alo = self.counters.rejected_alo.wrapping_add(1);
                self.remove(i);
                continue;
            }
            let (touch_px, touch_qty) = match o.side {
                Side::Bid => (tb.bid_1e6, tb.bid_qty_1e6),
                Side::Ask => (tb.ask_1e6, tb.ask_qty_1e6),
            };
            if self.parity_sim && (!known || o.px_1e6 != touch_px) {
                self.emit(t_ns, &o, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_OTHER, 0);
                self.counters.dropped_touch = self.counters.dropped_touch.wrapping_add(1);
                self.remove(i);
                continue;
            }
            let improves = match o.side {
                Side::Bid => o.px_1e6 > touch_px,
                Side::Ask => o.px_1e6 < touch_px,
            };
            self.orders[i].ahead_1e6 = if !known {
                QUEUE_AHEAD_UNKNOWN
            } else if o.px_1e6 == touch_px {
                touch_qty
            } else if improves {
                0
            } else {
                QUEUE_AHEAD_UNKNOWN
            };
            self.orders[i].state = ST_RESTING;
            self.emit(t_ns, &o, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0);
            self.counters.rested = self.counters.rested.wrapping_add(1);
            i += 1;
        }
        let mut i = 0usize;
        while i < self.len {
            let o = self.orders[i];
            if o.sym == sym && o.state == ST_RESTING && o.cancel_ns <= t_ns {
                self.emit(t_ns, &o, ORDER_EVENT_CANCELED, o.cancel_reason, 0);
                self.counters.canceled = self.counters.canceled.wrapping_add(1);
                self.remove(i);
                continue;
            }
            i += 1;
        }
    }

    fn schedule_cancel(
        &mut self,
        client_oid: u64,
        slot: u8,
        effective_ns: u64,
        reason: u8,
    ) -> Result<(), QueueRefusal> {
        let i = self.find(client_oid, slot)?;
        let o = &mut self.orders[i];
        if effective_ns < o.cancel_ns {
            o.cancel_ns = effective_ns;
            o.cancel_reason = reason;
        }
        Ok(())
    }

    /// The index of the one held order of `slot` carrying `client_oid`.
    fn find(&self, client_oid: u64, slot: u8) -> Result<usize, QueueRefusal> {
        let mut found: Option<usize> = None;
        let mut i = 0usize;
        while i < self.len {
            let o = &self.orders[i];
            if o.client_oid == client_oid && o.slot == slot {
                if found.is_some() {
                    return Err(QueueRefusal::Ambiguous);
                }
                found = Some(i);
            }
            i += 1;
        }
        found.ok_or(QueueRefusal::NoSuchOrder)
    }

    #[inline]
    fn sym_index(&self, sym: u32) -> Option<usize> {
        let mut k = 0usize;
        while k < self.n_syms {
            if self.syms[k].sym == sym {
                return Some(k);
            }
            k += 1;
        }
        None
    }

    /// Append one event to the out ring; the consumer drains it after
    /// every call.
    #[inline]
    fn emit(&mut self, t_ns: u64, o: &QueueOrder, kind: u8, reason: u8, qty_1e6: i64) {
        if self.out_len >= QUEUE_OUT_CAP {
            debug_assert!(
                false,
                "queue event ring overflowed — the consumer drains after every call"
            );
            self.counters.out_overflow = self.counters.out_overflow.wrapping_add(1);
            return;
        }
        let slot = (self.out_head + self.out_len) % QUEUE_OUT_CAP;
        // COPY: one 40 B `QueueEvent` per order state change (a landing, a
        // fill, a cancel, a reject — never per tick), ≤ 96 per call
        // (`QUEUE_OUT_CAP`), read back once by the consumer. The law emits
        // ONE vocabulary that both consumers and the property tests read; a
        // monomorphized sink writing each consumer's own record was
        // rejected: two consumer types threaded through every entry point.
        self.out[slot] = QueueEvent {
            t_ns,
            client_oid: o.client_oid,
            px_1e6: o.px_1e6,
            qty_1e6,
            sym: o.sym,
            side: o.side,
            slot: o.slot,
            kind,
            reason,
        };
        self.out_len += 1;
    }

    /// Remove held order `i`, shifting the tail left so emit order — our
    /// own FIFO priority — is preserved.
    #[inline]
    fn remove(&mut self, i: usize) {
        let mut k = i;
        // COPY: the tail shifts one slot left — at most 31 × 72 B = 2,232 B
        // (a 64 B order and its 8 B link per slot), once per order that
        // ends (a fill, a cancel, a reject), never per tick. The table's
        // order IS our FIFO priority; an index permutation was rejected:
        // every per-tick scan (`on_book`, `on_print`) would chase it.
        while k + 1 < self.len {
            self.orders[k] = self.orders[k + 1];
            self.links[k] = self.links[k + 1];
            k += 1;
        }
        self.len -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYM: u32 = 7;
    const SLOT: u8 = 6;
    /// One unit ×1e6 (a price of 100 is `100 * U`).
    const U: i64 = 1_000_000;

    fn touch(bid: i64, bid_q: i64, ask: i64, ask_q: i64) -> Touch {
        Touch {
            bid_1e6: bid * U,
            ask_1e6: ask * U,
            bid_qty_1e6: bid_q * U,
            ask_qty_1e6: ask_q * U,
        }
    }

    fn order(oid: u64, side: Side, px: i64, qty: i64, ready_ns: u64) -> QueuePlace {
        QueuePlace {
            client_oid: oid,
            sym: SYM,
            side,
            slot: SLOT,
            px_1e6: px * U,
            qty_1e6: qty * U,
            ready_ns,
            expiry_ns: QUEUE_NEVER,
        }
    }

    /// A book tracking `SYM` whose last touch is `t` (fed at instant 0).
    fn book_with(t: Touch) -> QueueBook {
        let mut b = QueueBook::new();
        assert_eq!(b.track(SYM), Ok(()));
        b.on_book(SYM, t, 0);
        b
    }

    fn drain(b: &mut QueueBook) -> Vec<QueueEvent> {
        let mut v = Vec::new();
        while let Some(e) = b.try_next_event() {
            v.push(e);
        }
        v
    }

    fn kinds(v: &[QueueEvent]) -> Vec<(u64, u8, u8, i64)> {
        v.iter()
            .map(|e| (e.client_oid, e.kind, e.reason, e.qty_1e6))
            .collect()
    }

    #[test]
    fn a_new_book_is_empty_and_default_is_new() {
        let b = QueueBook::new();
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
        assert_eq!(b.counters, QueueCounters::default());
        assert!(!b.parity_sim);
        // Failure mode: nothing is held, so nothing is found or resting.
        assert!(b.get(1, SLOT).is_none());
        assert!(!b.is_resting(1, SLOT));
        let d = QueueBook::default();
        assert!(d.is_empty() && d.counters == QueueCounters::default() && !d.parity_sim);
    }

    #[test]
    fn track_is_idempotent_and_refuses_one_symbol_too_many() {
        let mut b = QueueBook::new();
        let mut s = 0u32;
        while (s as usize) < QUEUE_MAX_SYMS {
            assert_eq!(b.track(s), Ok(()));
            assert_eq!(b.track(s), Ok(()), "tracking twice is a no-op");
            s += 1;
        }
        assert_eq!(b.track(QUEUE_MAX_SYMS as u32), Err(QueueRefusal::SymsFull));
        // A book of an untracked symbol is ignored, and so is a print.
        b.on_book(99, touch(100, 1, 101, 1), 5);
        b.on_print(99, 100 * U, U, true, 5);
        assert!(b.try_next_event().is_none());
    }

    #[test]
    fn a_placement_waits_for_the_first_record_at_or_after_ready() {
        let mut b = book_with(touch(100, 7, 101, 3));
        assert_eq!(b.place(&order(1, Side::Bid, 100, 2, 50)), Ok(()));
        assert_eq!(b.len(), 1);
        let o = b.get(1, SLOT).expect("held");
        assert_eq!(
            (o.px_1e6, o.remaining_1e6, o.ahead_1e6, o.ready_ns),
            (100 * U, 2 * U, QUEUE_AHEAD_UNKNOWN, 50)
        );
        // A record before `ready` lands nothing.
        b.on_book(SYM, touch(100, 7, 101, 3), 49);
        assert!(!b.is_resting(1, SLOT));
        assert!(b.try_next_event().is_none());
        // The first record at `ready` lands it, against the book BEFORE it.
        b.on_book(SYM, touch(100, 9, 101, 3), 50);
        assert!(b.is_resting(1, SLOT));
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [(1, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0)]
        );
        assert_eq!(
            (v[0].t_ns, v[0].slot, v[0].sym, v[0].side, v[0].px_1e6),
            (50, SLOT, SYM, Side::Bid, 100 * U)
        );
        assert_eq!(b.get(1, SLOT).expect("held").ahead_1e6, 7 * U);
        assert_eq!((b.counters.placed, b.counters.rested), (1, 1));
    }

    #[test]
    fn a_placement_is_refused_when_invalid_or_the_book_is_full() {
        let mut b = QueueBook::new();
        assert_eq!(
            b.place(&order(1, Side::Bid, 0, 1, 0)),
            Err(QueueRefusal::Invalid)
        );
        assert_eq!(
            b.place(&order(1, Side::Bid, -1, 1, 0)),
            Err(QueueRefusal::Invalid)
        );
        assert_eq!(
            b.place(&order(1, Side::Bid, 100, 0, 0)),
            Err(QueueRefusal::Invalid)
        );
        let mut s = 0u32;
        while (s as usize) < QUEUE_MAX_SYMS {
            let mut p = order(100 + u64::from(s), Side::Ask, 100, 1, 0);
            p.sym = s;
            assert_eq!(b.place(&p), Ok(()), "placing tracks the symbol");
            s += 1;
        }
        let mut p = order(200, Side::Ask, 100, 1, 0);
        p.sym = QUEUE_MAX_SYMS as u32;
        assert_eq!(b.place(&p), Err(QueueRefusal::SymsFull));
        let mut oid = 300u64;
        while b.len() < QUEUE_MAX_ORDERS {
            let mut p = order(oid, Side::Ask, 100, 1, 0);
            p.sym = 0;
            assert_eq!(b.place(&p), Ok(()));
            oid += 1;
        }
        let mut p = order(oid, Side::Ask, 100, 1, 0);
        p.sym = 0;
        assert_eq!(b.place(&p), Err(QueueRefusal::Full));
        assert_eq!(b.counters.refused, 5);
        assert_eq!(b.counters.placed, QUEUE_MAX_ORDERS as u64);
    }

    #[test]
    fn the_queue_ahead_is_the_displayed_size_zero_for_a_new_level_unknown_behind() {
        let mut b = book_with(touch(100, 5, 103, 4));
        b.place(&order(1, Side::Bid, 100, 1, 10)).expect("placed"); // joins the touch
        b.place(&order(2, Side::Ask, 102, 1, 10)).expect("placed"); // a new level inside 103
        b.place(&order(3, Side::Bid, 99, 1, 10)).expect("placed"); // behind the 100 bid
        b.on_book(SYM, touch(100, 6, 103, 4), 10);
        assert_eq!(
            b.get(1, SLOT).expect("held").ahead_1e6,
            5 * U,
            "the size before the block"
        );
        assert_eq!(b.get(2, SLOT).expect("held").ahead_1e6, 0);
        assert_eq!(b.get(3, SLOT).expect("held").ahead_1e6, QUEUE_AHEAD_UNKNOWN);
        // The touch comes down to 99: the order behind joins with the size shown then …
        b.on_book(SYM, touch(99, 8, 103, 4), 20);
        assert_eq!(b.get(3, SLOT).expect("held").ahead_1e6, 8 * U);
        // … and a touch elsewhere afterwards never changes a known queue.
        b.on_book(SYM, touch(98, 1, 103, 4), 30);
        assert_eq!(b.get(3, SLOT).expect("held").ahead_1e6, 8 * U);
        assert_eq!(b.get(1, SLOT).expect("held").ahead_1e6, 5 * U);
        assert_eq!(b.counters.rested, 3);
    }

    #[test]
    fn with_no_book_known_an_arrival_rests_unknown_and_is_not_checked() {
        let mut b = QueueBook::new();
        // Untracked until placed, so the landing record is the first book seen.
        b.place(&order(1, Side::Bid, 105, 1, 0)).expect("placed");
        b.on_book(SYM, touch(100, 5, 104, 5), 0);
        assert!(b.is_resting(1, SLOT), "no book was known when it arrived");
        assert_eq!(b.get(1, SLOT).expect("held").ahead_1e6, QUEUE_AHEAD_UNKNOWN);
        // A one-sided book is not a touch: the next arrival is unchecked too.
        let mut c = QueueBook::new();
        c.track(SYM).expect("tracked");
        c.on_book(
            SYM,
            Touch {
                bid_1e6: 0,
                ask_1e6: 101 * U,
                bid_qty_1e6: 0,
                ask_qty_1e6: U,
            },
            0,
        );
        c.place(&order(2, Side::Ask, 50, 1, 1)).expect("placed");
        c.on_book(SYM, touch(100, 1, 101, 1), 1);
        assert!(c.is_resting(2, SLOT));
        assert_eq!(c.counters.rejected_alo, 0);
    }

    #[test]
    fn a_crossing_post_only_is_rejected_bad_alo_px_against_the_book_it_meets() {
        let mut b = book_with(touch(100, 5, 101, 5));
        b.place(&order(1, Side::Bid, 101, 1, 10)).expect("placed"); // bid at the ask
        b.place(&order(2, Side::Ask, 100, 1, 10)).expect("placed"); // ask at the bid
        b.place(&order(3, Side::Bid, 100, 1, 10)).expect("placed"); // joins: fine
                                                                    // The landing record's own book would let order 1 rest (ask 102),
                                                                    // but it arrived in the block that PRODUCED that book: the book it
                                                                    // met is the one before.
        b.on_book(SYM, touch(100, 5, 102, 5), 10);
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [
                (1, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX, 0),
                (2, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX, 0),
                (3, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0),
            ]
        );
        assert_eq!(b.len(), 1);
        assert!(b.get(1, SLOT).is_none() && b.get(2, SLOT).is_none());
        assert_eq!((b.counters.rejected_alo, b.counters.rested), (2, 1));
        // Failure mode: one tick inside the touch never crosses.
        b.place(&order(4, Side::Bid, 101, 1, 20)).expect("placed");
        b.on_print(SYM, 102 * U, U, false, 20); // the ask is 102 now
        assert!(b.is_resting(4, SLOT));
    }

    #[test]
    fn prints_consume_the_queue_ahead_then_fill_partially_at_our_limit() {
        let mut b = book_with(touch(100, 3, 101, 5));
        b.place(&order(1, Side::Bid, 100, 4, 10)).expect("placed");
        b.on_book(SYM, touch(100, 3, 101, 5), 10);
        drain(&mut b);
        // A seller takes 2 at 100: all of it from the 3 ahead.
        b.on_print(SYM, 100 * U, 2 * U, true, 20);
        assert!(b.try_next_event().is_none());
        assert_eq!(b.get(1, SLOT).expect("held").ahead_1e6, U);
        // A seller takes 3: 1 ahead, then 2 of ours — at OUR price.
        b.on_print(SYM, 100 * U, 3 * U, true, 30);
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [(1, QUEUE_EVENT_FILL, ORDER_EVENT_REASON_NONE, 2 * U)]
        );
        assert_eq!((v[0].px_1e6, v[0].t_ns), (100 * U, 30));
        let o = b.get(1, SLOT).expect("held");
        assert_eq!((o.ahead_1e6, o.remaining_1e6), (0, 2 * U));
        // Failure modes: a BUYER at our price, another price, another symbol.
        b.on_print(SYM, 100 * U, 9 * U, false, 40);
        b.on_print(SYM, 101 * U, 9 * U, true, 40);
        b.on_print(SYM + 1, 100 * U, 9 * U, true, 40);
        b.on_print(SYM, 100 * U, 0, true, 40);
        assert!(b.try_next_event().is_none());
        // The rest fills, and the order is FILLED and gone.
        b.on_print(SYM, 100 * U, 5 * U, true, 50);
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [
                (1, QUEUE_EVENT_FILL, ORDER_EVENT_REASON_NONE, 2 * U),
                (1, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_NONE, 0)
            ]
        );
        assert!(b.is_empty());
        assert_eq!((b.counters.fills, b.counters.filled), (2, 1));
    }

    #[test]
    fn a_print_through_our_price_fills_the_whole_remainder() {
        let mut b = book_with(touch(100, 50, 101, 50));
        b.place(&order(1, Side::Ask, 101, 3, 10)).expect("placed");
        b.on_book(SYM, touch(100, 50, 101, 50), 10);
        drain(&mut b);
        // Failure mode first: a print at our price with 50 ahead fills nothing.
        b.on_print(SYM, 101 * U, 10 * U, false, 20);
        assert!(b.try_next_event().is_none());
        // A buyer lifting 102 went through our level: we filled first.
        b.on_print(SYM, 102 * U, U / 100, false, 30);
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [
                (1, QUEUE_EVENT_FILL, ORDER_EVENT_REASON_NONE, 3 * U),
                (1, ORDER_EVENT_FILLED, ORDER_EVENT_REASON_NONE, 0)
            ]
        );
        assert_eq!(v[0].px_1e6, 101 * U, "at our limit, not the print");
        assert!(b.is_empty());
    }

    #[test]
    fn an_unknown_queue_fills_only_through_its_price() {
        let mut b = book_with(touch(100, 5, 101, 5));
        b.place(&order(1, Side::Bid, 99, 2, 10)).expect("placed"); // behind the touch
        b.on_book(SYM, touch(100, 5, 101, 5), 10);
        drain(&mut b);
        b.on_print(SYM, 99 * U, 50 * U, true, 20);
        assert!(
            b.try_next_event().is_none(),
            "no queue known: a print at our price fills nothing"
        );
        b.on_print(SYM, 98 * U, U, true, 30);
        let v = drain(&mut b);
        assert_eq!(v[0].kind, QUEUE_EVENT_FILL);
        assert_eq!(v[0].qty_1e6, 2 * U);
    }

    #[test]
    fn a_cancel_lands_at_its_record_ahead_of_that_blocks_prints() {
        let mut b = book_with(touch(100, 0, 101, 5));
        b.place(&order(1, Side::Bid, 100, 2, 10)).expect("placed");
        b.on_book(SYM, touch(100, 0, 101, 5), 10);
        drain(&mut b);
        assert_eq!(b.cancel(1, SLOT, 30), Ok(()));
        // Before its record the order still fills …
        b.on_print(SYM, 100 * U, U, true, 29);
        assert_eq!(drain(&mut b)[0].qty_1e6, U);
        // … and the print of the cancel's own block does not.
        b.on_print(SYM, 100 * U, U, true, 30);
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [(
                1,
                ORDER_EVENT_CANCELED,
                ORDER_EVENT_REASON_CANCEL_REQUESTED,
                0
            )]
        );
        assert_eq!(v[0].t_ns, 30);
        assert!(b.is_empty());
        assert_eq!(b.counters.canceled, 1);
    }

    #[test]
    fn a_cancel_of_nothing_or_of_an_ambiguous_id_is_refused() {
        let mut b = book_with(touch(100, 1, 101, 1));
        assert_eq!(b.cancel(1, SLOT, 5), Err(QueueRefusal::NoSuchOrder));
        b.place(&order(1, Side::Bid, 100, 1, 10)).expect("placed");
        assert_eq!(
            b.cancel(1, SLOT + 1, 5),
            Err(QueueRefusal::NoSuchOrder),
            "the id is per slot"
        );
        assert_eq!(b.lookup(1, SLOT).map(|o| o.side), Ok(Side::Bid));
        assert_eq!(b.lookup(1, SLOT + 1), Err(QueueRefusal::NoSuchOrder));
        b.place(&order(1, Side::Ask, 101, 1, 10)).expect("placed");
        assert_eq!(b.cancel(1, SLOT, 5), Err(QueueRefusal::Ambiguous));
        assert_eq!(b.lookup(1, SLOT), Err(QueueRefusal::Ambiguous));
        assert!(
            b.get(1, SLOT).is_none(),
            "two orders carry it, so get names neither"
        );
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn an_expiry_is_a_scheduled_cancel_and_the_earlier_cancel_wins() {
        let mut b = book_with(touch(100, 1, 101, 1));
        let mut p = order(1, Side::Bid, 100, 1, 10);
        p.expiry_ns = 40;
        b.place(&p).expect("placed");
        // A later requested cancel does not move the expiry …
        assert_eq!(b.cancel(1, SLOT, 60), Ok(()));
        b.on_book(SYM, touch(100, 1, 101, 1), 10);
        b.on_book(SYM, touch(100, 1, 101, 1), 45);
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [
                (1, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0),
                (1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_EXPIRED, 0)
            ]
        );
        // … and an earlier one replaces it, with its own reason.
        let mut p = order(2, Side::Bid, 100, 1, 50);
        p.expiry_ns = 90;
        b.place(&p).expect("placed");
        assert_eq!(b.cancel(2, SLOT, 70), Ok(()));
        b.on_book(SYM, touch(100, 1, 101, 1), 70);
        let v = drain(&mut b);
        assert_eq!(v[1].reason, ORDER_EVENT_REASON_CANCEL_REQUESTED);
        // A cancel that lands before the placement: it lands, then goes, in one block.
        b.place(&order(3, Side::Bid, 100, 1, 80)).expect("placed");
        b.cancel(3, SLOT, 75).expect("scheduled");
        b.on_print(SYM, 100 * U, 9 * U, true, 80);
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [
                (3, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0),
                (
                    3,
                    ORDER_EVENT_CANCELED,
                    ORDER_EVENT_REASON_CANCEL_REQUESTED,
                    0
                )
            ]
        );
        assert!(b.is_empty());
    }

    #[test]
    fn a_modify_cancels_the_old_order_and_the_new_one_joins_the_back() {
        let mut b = book_with(touch(100, 2, 102, 2));
        let mut first = order(1, Side::Bid, 100, 1, 10);
        first.expiry_ns = 90;
        b.place(&first).expect("placed");
        b.on_book(SYM, touch(100, 2, 102, 2), 10);
        drain(&mut b);
        // The touch rises to 101 (4 shown); requote there.
        b.on_book(SYM, touch(101, 4, 102, 2), 20);
        let mut requote = order(2, Side::Bid, 101, 1, 30);
        requote.expiry_ns = 500; // not read: a reprice cannot extend a quote's life
        assert_eq!(b.modify(1, &requote), Ok(()));
        assert_eq!(b.len(), 2);
        b.on_book(SYM, touch(101, 6, 102, 2), 30);
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [
                (2, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0),
                (1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_REPLACED, 0)
            ]
        );
        assert_eq!(b.get(2, SLOT).expect("held").expiry_ns, 90, "inherited (LAW E-7)");
        assert_eq!(
            b.get(2, SLOT).expect("held").ahead_1e6,
            4 * U,
            "the back of the queue it met"
        );
        assert!(b.get(1, SLOT).is_none());
    }

    #[test]
    fn a_replacement_whose_predecessor_filled_in_flight_is_rejected_on_landing() {
        let mut b = book_with(touch(100, 0, 102, 2));
        b.place(&order(1, Side::Bid, 100, 1, 10)).expect("placed");
        b.on_book(SYM, touch(100, 0, 102, 2), 10);
        drain(&mut b);
        assert_eq!(b.modify(1, &order(2, Side::Bid, 101, 1, 50)), Ok(()));
        // The old order fills while the modify is in flight …
        b.on_print(SYM, 100 * U, U, true, 30);
        let v = drain(&mut b);
        assert_eq!(v[1].kind, ORDER_EVENT_FILLED);
        // … so the replacement is refused where it lands: one decision
        // never fills twice.
        b.on_book(SYM, touch(100, 0, 102, 2), 50);
        assert_eq!(
            kinds(&drain(&mut b)),
            [(2, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_OTHER, 0)]
        );
        assert_eq!(b.counters.stale_modify, 1);
        assert!(b.is_empty());
        // Failure mode: a predecessor still held lets it land.
        b.place(&order(3, Side::Bid, 100, 1, 60)).expect("placed");
        b.modify(3, &order(4, Side::Bid, 101, 1, 70)).expect("modified");
        b.on_book(SYM, touch(100, 0, 102, 2), 70);
        assert!(b.is_resting(4, SLOT));
    }

    #[test]
    fn a_modify_is_refused_whole() {
        let mut b = book_with(touch(100, 2, 102, 2));
        assert_eq!(
            b.modify(9, &order(2, Side::Bid, 101, 1, 30)),
            Err(QueueRefusal::NoSuchOrder)
        );
        b.place(&order(1, Side::Bid, 100, 1, 10)).expect("placed");
        assert_eq!(
            b.modify(1, &order(2, Side::Bid, 0, 1, 30)),
            Err(QueueRefusal::Invalid)
        );
        let mut oid = 10u64;
        while b.len() < QUEUE_MAX_ORDERS {
            b.place(&order(oid, Side::Ask, 102, 1, 10)).expect("placed");
            oid += 1;
        }
        assert_eq!(
            b.modify(1, &order(2, Side::Bid, 101, 1, 30)),
            Err(QueueRefusal::Full)
        );
        // The old order was left exactly as it was: no cancel scheduled.
        let o = b.get(1, SLOT).expect("held");
        assert_eq!(o.cancel_ns, QUEUE_NEVER);
        // Invalid and Full are refusals; a missing order is a race the
        // consumer answers (it places the new order, as the venue does).
        assert_eq!(b.counters.refused, 2);
    }

    #[test]
    fn events_come_out_in_order_and_an_empty_ring_says_none() {
        let mut b = book_with(touch(100, 0, 101, 0));
        assert!(b.try_next_event().is_none());
        b.place(&order(1, Side::Bid, 100, 1, 10)).expect("placed");
        b.place(&order(2, Side::Ask, 101, 1, 10)).expect("placed");
        b.on_book(SYM, touch(100, 0, 101, 0), 10);
        assert_eq!(b.try_next_event().map(|e| e.client_oid), Some(1));
        assert_eq!(b.try_next_event().map(|e| e.client_oid), Some(2));
        assert!(b.try_next_event().is_none());
    }

    #[test]
    fn parity_sim_meets_the_landing_records_touch_and_drops_an_order_off_it() {
        let mut b = book_with(touch(100, 5, 101, 5));
        b.parity_sim = true;
        b.place(&order(1, Side::Bid, 100, 1, 10)).expect("placed"); // still the touch at landing
        b.place(&order(2, Side::Ask, 101, 1, 10)).expect("placed"); // the ask moved away
        b.place(&order(3, Side::Bid, 102, 1, 10)).expect("placed"); // crosses the NEW ask
        b.on_book(SYM, touch(100, 7, 102, 5), 10);
        let v = drain(&mut b);
        assert_eq!(
            kinds(&v),
            [
                (1, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0),
                (2, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_OTHER, 0),
                (3, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX, 0),
            ]
        );
        assert_eq!(
            b.get(1, SLOT).expect("held").ahead_1e6,
            7 * U,
            "the landing record's own size"
        );
        assert_eq!((b.counters.dropped_touch, b.counters.rejected_alo), (1, 1));
        // Failure mode: without a known touch nothing can be at it.
        let mut c = QueueBook::new();
        c.parity_sim = true;
        c.place(&order(4, Side::Bid, 100, 1, 0)).expect("placed");
        c.on_print(SYM, 100 * U, U, true, 0);
        assert_eq!(
            kinds(&drain(&mut c)),
            [(4, ORDER_EVENT_REJECTED, ORDER_EVENT_REASON_OTHER, 0)]
        );
    }

    /// Fill every slot, then emit one event more than the ring holds
    /// without draining: the consumer broke its contract.
    fn overflow_the_ring() -> QueueBook {
        let mut b = book_with(touch(100, 0, 101, 0));
        let mut oid = 1u64;
        while b.len() < QUEUE_MAX_ORDERS {
            b.place(&order(oid, Side::Bid, 100, 1, 10)).expect("placed");
            oid += 1;
        }
        b.on_print(SYM, 99 * U, U, true, 10); // lands + fill + FILLED for each: 96
        b.place(&order(oid, Side::Bid, 100, 1, 20)).expect("placed");
        b.on_book(SYM, touch(100, 0, 101, 0), 20); // the 97th
        b
    }

    /// XMM XH3: a quiet perp's cancel lands at the next record of ANY
    /// symbol of its venue (the block is the venue's) — tracked or not —
    /// but never at another venue's record; its landing meets its own
    /// last known book.
    #[test]
    fn off_parity_a_quiet_symbols_due_actions_land_at_any_record_of_its_venue() {
        let other: u32 = SYM + 1; // same venue byte (0)
        let untracked: u32 = SYM + 2;
        let foreign: u32 = (7 << 24) | SYM; // another venue
        let mut b = book_with(touch(100, 5, 101, 5));
        assert_eq!(b.track(other), Ok(()));
        b.on_book(other, touch(50, 1, 51, 1), 0);
        assert_eq!(b.place(&order(1, Side::Bid, 100, 1, 10)), Ok(()));
        // SYM is quiet: the placement lands at the other symbol's record,
        // against SYM's own book (a bid AT its touch: 5 ahead).
        b.on_book(other, touch(50, 1, 51, 1), 10);
        let ev = drain(&mut b);
        assert_eq!(kinds(&ev), vec![(1, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0)]);
        assert_eq!(b.get(1, SLOT).map(|o| o.ahead_1e6), Some(5 * U));
        // A cancel due at 20: another venue's record lands nothing …
        assert_eq!(b.cancel(1, SLOT, 20), Ok(()));
        b.on_book(foreign, touch(1, 1, 2, 1), 25);
        assert!(drain(&mut b).is_empty());
        // … an untracked symbol of the same venue lands it.
        b.on_book(untracked, touch(1, 1, 2, 1), 25);
        let ev = drain(&mut b);
        assert_eq!(kinds(&ev), vec![(1, ORDER_EVENT_CANCELED, ORDER_EVENT_REASON_CANCEL_REQUESTED, 0)]);
        assert!(b.is_empty());
        // A print of another symbol is a record of the block too.
        assert_eq!(b.place(&order(2, Side::Ask, 101, 1, 30)), Ok(()));
        b.on_print(other, 50 * U, U, true, 30);
        assert_eq!(kinds(&drain(&mut b)), vec![(2, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0)]);
    }

    /// Under the parity switch the simulator's per-symbol clock holds: a
    /// quiet symbol's due actions wait for its OWN record.
    #[test]
    fn under_parity_a_quiet_symbols_due_actions_wait_for_its_own_record() {
        let other: u32 = SYM + 1;
        let mut b = book_with(touch(100, 5, 101, 5));
        b.parity_sim = true;
        assert_eq!(b.track(other), Ok(()));
        assert_eq!(b.place(&order(1, Side::Bid, 100, 1, 10)), Ok(()));
        b.on_book(other, touch(50, 1, 51, 1), 10);
        b.on_print(other, 50 * U, U, true, 11);
        assert!(drain(&mut b).is_empty(), "no record of SYM yet");
        b.on_book(SYM, touch(100, 5, 101, 5), 12);
        assert_eq!(kinds(&drain(&mut b)), vec![(1, ORDER_EVENT_RESTING, ORDER_EVENT_REASON_NONE, 0)]);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "queue event ring overflowed")]
    fn an_undrained_ring_overflow_is_a_bug_in_debug() {
        let _ = overflow_the_ring();
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn an_undrained_ring_overflow_is_counted_in_release() {
        let mut b = overflow_the_ring();
        assert_eq!(b.counters.out_overflow, 1);
        assert_eq!(drain(&mut b).len(), QUEUE_OUT_CAP);
    }

    // ------------------------------------------------------------ properties (plan §7.3)

    mod props {
        use super::*;
        use proptest::prelude::*;
        use std::collections::HashMap;

        #[derive(Clone, Debug)]
        enum Op {
            Place {
                sym: u32,
                bid: bool,
                off: i64,
                qty: i64,
                delay: u64,
                ttl: Option<u64>,
            },
            Cancel {
                pick: usize,
                delay: u64,
            },
            Modify {
                pick: usize,
                off: i64,
                qty: i64,
                delay: u64,
            },
            Book {
                sym: u32,
                bid: i64,
                spread: i64,
                bid_q: i64,
                ask_q: i64,
            },
            Print {
                sym: u32,
                off: i64,
                qty: i64,
                sell: bool,
            },
        }

        fn op() -> impl Strategy<Value = (u64, Op)> {
            let place = (
                0u32..2,
                any::<bool>(),
                -4i64..=4,
                1i64..=5,
                0u64..4,
                prop::option::of(1u64..30),
            )
                .prop_map(|(sym, bid, off, qty, delay, ttl)| Op::Place {
                    sym,
                    bid,
                    off,
                    qty,
                    delay,
                    ttl,
                });
            let cancel =
                (any::<usize>(), 0u64..4).prop_map(|(pick, delay)| Op::Cancel { pick, delay });
            let modify = (any::<usize>(), -4i64..=4, 1i64..=5, 0u64..4).prop_map(
                |(pick, off, qty, delay)| Op::Modify {
                    pick,
                    off,
                    qty,
                    delay,
                },
            );
            let book = (0u32..2, 97i64..=103, 1i64..=3, 0i64..=6, 0i64..=6).prop_map(
                |(sym, bid, spread, bid_q, ask_q)| Op::Book {
                    sym,
                    bid,
                    spread,
                    bid_q,
                    ask_q,
                },
            );
            let print =
                (0u32..2, -5i64..=5, 1i64..=8, any::<bool>()).prop_map(|(sym, off, qty, sell)| {
                    Op::Print {
                        sym,
                        off,
                        qty,
                        sell,
                    }
                });
            (
                0u64..3,
                prop_oneof![3 => place, 1 => cancel, 1 => modify, 3 => book, 4 => print],
            )
        }

        /// What the test knows about one accepted placement.
        struct Placed {
            side: Side,
            px: i64,
            qty: i64,
            filled: i64,
            /// The earliest effective cancel asked for (expiry included).
            cancel_at: u64,
            /// The TTL instant (what a modify inherits).
            expiry: u64,
            last_ahead: i64,
            last_rem: i64,
            landed: bool,
            /// The order a modify replaced with this one (0 = a placement).
            pred: u64,
        }

        fn crosses(t: Option<Touch>, side: Side, px: i64) -> bool {
            match t {
                Some(t) => match side {
                    Side::Bid => px >= t.ask_1e6,
                    Side::Ask => px <= t.bid_1e6,
                },
                None => false,
            }
        }

        fn run(ops: &[(u64, Op)], parity: bool) -> Result<(), TestCaseError> {
            let mut b = QueueBook::new();
            b.parity_sim = parity;
            b.track(0).expect("tracked");
            b.track(1).expect("tracked");
            // The law's known touch per symbol, mirrored.
            let mut known: [Option<Touch>; 2] = [None, None];
            let mut placed: HashMap<u64, Placed> = HashMap::new();
            let mut order_ids: Vec<u64> = Vec::new();
            let mut now = 0u64;
            let mut next_oid = 1u64;
            let mut k = 0usize;
            while k < ops.len() {
                let (dt, op) = &ops[k];
                k += 1;
                now += *dt;
                // The touch an arrival in this call meets.
                let mut meets: [Option<Touch>; 2] = known;
                match *op {
                    Op::Place {
                        sym,
                        bid,
                        off,
                        qty,
                        delay,
                        ttl,
                    } => {
                        let side = if bid { Side::Bid } else { Side::Ask };
                        let p = QueuePlace {
                            client_oid: next_oid,
                            sym,
                            side,
                            slot: SLOT,
                            px_1e6: (100 + off) * U,
                            qty_1e6: qty * U,
                            ready_ns: now + delay,
                            expiry_ns: ttl.map_or(QUEUE_NEVER, |t| now + t),
                        };
                        if b.place(&p).is_ok() {
                            placed.insert(
                                next_oid,
                                Placed {
                                    side,
                                    px: p.px_1e6,
                                    qty: p.qty_1e6,
                                    filled: 0,
                                    cancel_at: p.expiry_ns,
                                    expiry: p.expiry_ns,
                                    last_ahead: QUEUE_AHEAD_UNKNOWN,
                                    last_rem: p.qty_1e6,
                                    landed: false,
                                    pred: 0,
                                },
                            );
                            order_ids.push(next_oid);
                        }
                        next_oid += 1;
                    }
                    Op::Cancel { pick, delay } => {
                        if !order_ids.is_empty() {
                            let oid = order_ids[pick % order_ids.len()];
                            if b.cancel(oid, SLOT, now + delay).is_ok() {
                                let e = placed.get_mut(&oid).expect("known");
                                e.cancel_at = e.cancel_at.min(now + delay);
                            }
                        }
                    }
                    Op::Modify {
                        pick,
                        off,
                        qty,
                        delay,
                    } => {
                        if !order_ids.is_empty() {
                            let prev = order_ids[pick % order_ids.len()];
                            let side = placed[&prev].side;
                            let p = QueuePlace {
                                client_oid: next_oid,
                                sym: b.get(prev, SLOT).map_or(0, |o| o.sym),
                                side,
                                slot: SLOT,
                                px_1e6: (100 + off) * U,
                                qty_1e6: qty * U,
                                ready_ns: now + delay,
                                expiry_ns: QUEUE_NEVER,
                            };
                            if b.modify(prev, &p).is_ok() {
                                let e = placed.get_mut(&prev).expect("known");
                                e.cancel_at = e.cancel_at.min(p.ready_ns);
                                // LAW E-7: the new order inherits the TTL.
                                let inherited = e.expiry;
                                placed.insert(
                                    next_oid,
                                    Placed {
                                        side,
                                        px: p.px_1e6,
                                        qty: p.qty_1e6,
                                        filled: 0,
                                        cancel_at: inherited,
                                        expiry: inherited,
                                        last_ahead: QUEUE_AHEAD_UNKNOWN,
                                        last_rem: p.qty_1e6,
                                        landed: false,
                                        pred: prev,
                                    },
                                );
                                order_ids.push(next_oid);
                            }
                            next_oid += 1;
                        }
                    }
                    Op::Book {
                        sym,
                        bid,
                        spread,
                        bid_q,
                        ask_q,
                    } => {
                        let t = touch(bid, bid_q, bid + spread, ask_q);
                        if parity {
                            meets[sym as usize] = Some(t);
                        }
                        known[sym as usize] = Some(t);
                        b.on_book(sym, t, now);
                    }
                    Op::Print {
                        sym,
                        off,
                        qty,
                        sell,
                    } => {
                        b.on_print(sym, (100 + off) * U, qty * U, sell, now);
                    }
                }
                while let Some(e) = b.try_next_event() {
                    let o = placed
                        .get_mut(&e.client_oid)
                        .expect("an event names a placed order");
                    prop_assert_eq!(e.px_1e6, o.px, "every event carries our limit");
                    match e.kind {
                        QUEUE_EVENT_FILL => {
                            prop_assert!(o.landed, "a fill before landing");
                            prop_assert!(e.qty_1e6 > 0);
                            o.filled += e.qty_1e6;
                            prop_assert!(o.filled <= o.qty, "overfilled: {} > {}", o.filled, o.qty);
                            prop_assert!(
                                e.t_ns < o.cancel_at,
                                "a fill at {} after the cancel at {}",
                                e.t_ns,
                                o.cancel_at
                            );
                        }
                        ORDER_EVENT_FILLED => prop_assert_eq!(o.filled, o.qty),
                        ORDER_EVENT_RESTING | ORDER_EVENT_REJECTED => {
                            prop_assert!(!o.landed, "landed twice");
                            o.landed = true;
                            let sym = placed_sym(&e);
                            let crossed = crosses(meets[sym], o.side, o.px);
                            let bad_alo = e.kind == ORDER_EVENT_REJECTED
                                && e.reason == ORDER_EVENT_REASON_BAD_ALO_PX;
                            // A replacement whose predecessor had left the
                            // book is refused before any other check.
                            let stale_replacement = e.kind == ORDER_EVENT_REJECTED
                                && e.reason == ORDER_EVENT_REASON_OTHER
                                && o.pred != 0
                                && b.get(o.pred, SLOT).is_none();
                            if !stale_replacement {
                                prop_assert_eq!(
                                    bad_alo,
                                    crossed,
                                    "reject iff it crosses the book it met"
                                );
                                if e.kind == ORDER_EVENT_REJECTED && !bad_alo {
                                    prop_assert!(
                                        parity,
                                        "only the parity switch drops an order off the touch"
                                    );
                                }
                            }
                        }
                        ORDER_EVENT_CANCELED => {
                            prop_assert!(e.t_ns >= o.cancel_at, "a cancel before it was due");
                        }
                        other => prop_assert!(false, "unexpected kind {}", other),
                    }
                }
                prop_assert_eq!(b.counters.out_overflow, 0);
                // Monotone: remainder and a KNOWN queue ahead never grow back.
                let mut i = 0usize;
                while i < order_ids.len() {
                    let oid = order_ids[i];
                    i += 1;
                    if let Some(q) = b.get(oid, SLOT) {
                        let o = placed.get_mut(&oid).expect("known");
                        prop_assert!(q.remaining_1e6 > 0 && q.remaining_1e6 <= o.last_rem);
                        prop_assert_eq!(q.remaining_1e6, o.qty - o.filled);
                        if o.last_ahead != QUEUE_AHEAD_UNKNOWN {
                            prop_assert!(
                                q.ahead_1e6 != QUEUE_AHEAD_UNKNOWN && q.ahead_1e6 <= o.last_ahead
                            );
                        }
                        prop_assert!(q.ahead_1e6 == QUEUE_AHEAD_UNKNOWN || q.ahead_1e6 >= 0);
                        o.last_rem = q.remaining_1e6;
                        o.last_ahead = q.ahead_1e6;
                    }
                }
            }
            Ok(())
        }

        fn placed_sym(e: &QueueEvent) -> usize {
            e.sym as usize
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4_000))]

            #[test]
            fn the_queue_law_holds_its_invariants(ops in prop::collection::vec(op(), 1..160)) {
                run(&ops, false)?;
            }

            #[test]
            fn the_parity_switch_holds_them_too(ops in prop::collection::vec(op(), 1..160)) {
                run(&ops, true)?;
            }
        }
    }
}
