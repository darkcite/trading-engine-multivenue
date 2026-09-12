// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `core-fill` — the one modelled fill law
//!
//! Two consumers judge a paper order: the offline harness
//! (`cli::backtest::fill`) and the engine's own `PaperDispatcher`. Until
//! X1 there was only one, and the members inferred their positions from
//! SUBMITS instead — which is how the VRP member came to believe it
//! held a hedged option for two live campaigns in a row while the
//! harness, replaying the same capture through the same law, held a
//! naked perp (`orders=2 fills=1 ioc_canceled=1`).
//!
//! So the law lives here, once, and both consumers call it. A paper
//! position and its replayed fill cannot disagree about a price, a
//! size or a cancel, because there is no second implementation to
//! disagree with.
//!
//! ## The law
//!
//! * **Evidence.** Only a FRESH TWO-SIDED tick fills anything. A stale
//!   tick ([`core_types::Tick::is_stale`], VT4) or a one-sided book is
//!   not evidence — the 8e `preopen` lesson. Fewer fills is the
//!   conservative direction, and conservative is the direction a paper
//!   model is allowed to be wrong in.
//! * **IoC (I1).** Judged exactly ONCE, at the first fresh two-sided
//!   tick at or after activation. Marketable ⇒ fill at the TOUCH
//!   (`<=` / `>=` — a taker takes the touch), capped by the displayed
//!   opposite size; the remainder, or the whole order, CANCELS. An IoC
//!   never rests, so [`judge_ioc`] never returns [`Verdict::Wait`].
//! * **Maker (§4.2).** STRICT cross only — `<` / `>`, never the touch,
//!   because resting AT the touch means an unknowable queue ahead of
//!   us. Partial fills allowed, at the order's own LIMIT price (not the
//!   touch: a maker is the one being crossed). Unfilled ⇒ it keeps
//!   resting.
//! * **Budgets.** Our BIDs consume the printed ask size and our ASKs
//!   the printed bid size, shared across every order of that sym on
//!   that tick, FIFO in emit order — makers and IoCs alike. One tick's
//!   displayed size cannot fill two orders twice.
//! * **TTL (I1).** An order of ANY kind still open at the first record
//!   of its sym at or after `emit + ttl` is canceled before that
//!   record's fill evidence is read: the bar has closed, and a fill now
//!   would belong to the next one. `ttl_ns == 0` never expires.
//!
//! ## Doctrine
//!
//! * **No allocation, no floats, no panics.** Every function here is a
//!   handful of integer compares on `Copy` arguments.
//! * **The activation table is a MEASUREMENT** ([`ACTIVATION_NS_DEFAULT`]),
//!   not an assumption, and it is per deployment and per location.
//! * This crate knows nothing about fees, marks, P&L or settlement.
//!   Those are the harness's ledger and stay there.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core_types::{Side, Tick};

/// `Order.kind` of a post-only maker (the VM's only primitive).
pub const ORDER_KIND_MAKER: u8 = 0;
/// `Order.kind` of an immediate-or-cancel taker (I1; ICDP's, XSD's and
/// the VRP member's primitive).
pub const ORDER_KIND_IOC: u8 = 1;

/// Open orders one sym may hold at once.
pub const MAX_OPEN_PER_SYM: usize = 8;
/// Open orders the whole table may hold at once.
pub const MAX_OPEN_TOTAL: usize = 64;

/// Nanoseconds in a millisecond.
const MS: u64 = 1_000_000;

/// Activation delay per model venue byte (`core_types::VenueId as u8`),
/// in ns: `Δ_venue` = feed one-way p50 + REST request RTT p50 / 2,
/// rounded up to 10 ms.
///
/// **MEASURED**, not assumed: 2026-09-03 17:07–17:32Z by
/// `claude_worker.latency_probe` on the MacBook Pro M4 / operator's home
/// network (`docs/venue-latency.md` §2) —
/// bn 71 + 107/2 → 130 ms · okx 67 + 120/2 → 130 ms ·
/// deribit 108 + 208/2 → 220 ms · hl 272 + 124/2 → 340 ms ·
/// bybit 29 + 44/2 → 60 ms. Polymarket's CLOB feed is UNMEASURED (the
/// socket needs an asset id), so the §4.4 assumption of 200 ms stands
/// there. Index 5 is `Ai`, a dead slot.
///
/// **RE-MEASURE ON EVERY DEPLOYMENT AND LOCATION.** One table, both
/// consumers — the harness's `ModelParams::default()` reads it from
/// here so a re-measurement cannot land in one and not the other.
pub const ACTIVATION_NS_DEFAULT: [u64; 7] = [
    200 * MS, // pm
    130 * MS, // bn
    130 * MS, // okx
    220 * MS, // deribit
    340 * MS, // hl
    0,        // ai (dead)
    60 * MS,  // bybit
];

/// The two sides of a book at one instant, ×1e6.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Touch {
    /// Best bid price ×1e6.
    pub bid_1e6: i64,
    /// Best ask price ×1e6.
    pub ask_1e6: i64,
    /// Displayed bid size ×1e6.
    pub bid_qty_1e6: i64,
    /// Displayed ask size ×1e6.
    pub ask_qty_1e6: i64,
}

impl Touch {
    /// The touch a tick prints. Negative sizes clamp to zero — a
    /// negative displayed size is not a smaller one.
    #[inline]
    #[must_use]
    pub fn of(tick: &Tick) -> Self {
        Self {
            bid_1e6: tick.bid_px.raw(),
            ask_1e6: tick.ask_px.raw(),
            bid_qty_1e6: if tick.bid_qty.raw() > 0 { tick.bid_qty.raw() } else { 0 },
            ask_qty_1e6: if tick.ask_qty.raw() > 0 { tick.ask_qty.raw() } else { 0 },
        }
    }

    /// Whether both sides are quoted. A one-sided book is not fill
    /// evidence.
    #[inline]
    #[must_use]
    pub const fn two_sided(&self) -> bool {
        self.bid_1e6 > 0 && self.ask_1e6 > 0
    }
}

/// What one order does at one tick.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing happened; the order keeps resting. Makers only.
    Wait,
    /// Filled `qty_1e6` at `px_1e6`. A maker may have a remainder left
    /// resting; an IoC's remainder cancels with the fill.
    Fill {
        /// Fill price ×1e6.
        px_1e6: i64,
        /// Filled quantity ×1e6.
        qty_1e6: i64,
    },
    /// The order is gone with no fill.
    Cancel,
}

/// Whether a tick may fill anything: not stale, and quoted on both
/// sides.
#[inline]
#[must_use]
pub fn is_fill_evidence(tick: &Tick) -> bool {
    !tick.is_stale() && tick.bid_px.raw() > 0 && tick.ask_px.raw() > 0
}

/// I1: the instant an order emitted at `emit_ns` with `ttl_ns` expires;
/// `0` = never. Precomputed once at submit so the per-tick sweep is one
/// compare against a stored field, which is how both consumers hold it.
#[inline]
#[must_use]
pub const fn expiry_at(emit_ns: u64, ttl_ns: u64) -> u64 {
    if ttl_ns == 0 {
        0
    } else {
        emit_ns.saturating_add(ttl_ns)
    }
}

/// I1: has an order whose precomputed expiry is `expiry_ns` expired by
/// `now_ns`? `expiry_ns == 0` never expires.
#[inline]
#[must_use]
pub const fn expired_at(now_ns: u64, expiry_ns: u64) -> bool {
    expiry_ns != 0 && now_ns >= expiry_ns
}

/// I1: has an order emitted at `emit_ns` with `ttl_ns` expired by
/// `now_ns`? `ttl_ns == 0` never expires. The same predicate as
/// [`expired_at`], spelled from the fields an order carries.
#[inline]
#[must_use]
pub const fn ttl_expired(now_ns: u64, emit_ns: u64, ttl_ns: u64) -> bool {
    expired_at(now_ns, expiry_at(emit_ns, ttl_ns))
}

/// I1: judge an IoC, ONCE.
///
/// Marketable ⇒ fill at the TOUCH, capped by the displayed opposite
/// size, decrementing the shared budget. Anything else cancels. Never
/// returns [`Verdict::Wait`] — that is the whole point of an IoC, and a
/// caller that re-judges one has already broken the law.
#[inline]
#[must_use]
pub fn judge_ioc(
    side: Side,
    px_1e6: i64,
    remaining_1e6: i64,
    t: Touch,
    ask_budget: &mut i64,
    bid_budget: &mut i64,
) -> Verdict {
    match side {
        // A taker BID lifts the ask: marketable iff the ask is at or
        // under our limit, and we pay the ask.
        Side::Bid if t.ask_1e6 <= px_1e6 && *ask_budget > 0 => {
            let q = if remaining_1e6 < *ask_budget { remaining_1e6 } else { *ask_budget };
            if q <= 0 {
                return Verdict::Cancel;
            }
            *ask_budget -= q;
            Verdict::Fill { px_1e6: t.ask_1e6, qty_1e6: q }
        }
        Side::Ask if t.bid_1e6 >= px_1e6 && *bid_budget > 0 => {
            let q = if remaining_1e6 < *bid_budget { remaining_1e6 } else { *bid_budget };
            if q <= 0 {
                return Verdict::Cancel;
            }
            *bid_budget -= q;
            Verdict::Fill { px_1e6: t.bid_1e6, qty_1e6: q }
        }
        _ => Verdict::Cancel,
    }
}

/// §4.2: judge a resting maker.
///
/// STRICT cross only — `<` / `>`, never the touch: resting AT the touch
/// puts an unknowable queue ahead of us, and crediting a fill for it is
/// the single most flattering lie a paper model can tell. The fill is
/// at OUR limit, because we are the one being crossed. A partial leaves
/// the remainder resting.
#[inline]
#[must_use]
pub fn judge_maker(
    side: Side,
    px_1e6: i64,
    remaining_1e6: i64,
    t: Touch,
    ask_budget: &mut i64,
    bid_budget: &mut i64,
) -> Verdict {
    match side {
        Side::Bid if t.ask_1e6 < px_1e6 && *ask_budget > 0 => {
            let q = if remaining_1e6 < *ask_budget { remaining_1e6 } else { *ask_budget };
            if q <= 0 {
                return Verdict::Wait;
            }
            *ask_budget -= q;
            Verdict::Fill { px_1e6, qty_1e6: q }
        }
        Side::Ask if t.bid_1e6 > px_1e6 && *bid_budget > 0 => {
            let q = if remaining_1e6 < *bid_budget { remaining_1e6 } else { *bid_budget };
            if q <= 0 {
                return Verdict::Wait;
            }
            *bid_budget -= q;
            Verdict::Fill { px_1e6, qty_1e6: q }
        }
        _ => Verdict::Wait,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty, SymbolId, VenueId};

    const SYM: SymbolId = 7;

    fn touch(bid: i64, bid_q: i64, ask: i64, ask_q: i64) -> Touch {
        Touch { bid_1e6: bid, ask_1e6: ask, bid_qty_1e6: bid_q, ask_qty_1e6: ask_q }
    }

    fn tick(bid: i64, ask: i64, stale: bool) -> Tick {
        let mut t = Tick::new(
            1_000,
            VenueId::Deribit,
            SYM,
            0,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        );
        if stale {
            t.flags |= core_types::TICK_FLAG_STALE;
        }
        t
    }

    // ---------------- IoC: one arm per test ----------------

    /// A marketable taker BID lifts the ask and pays it — never its own
    /// limit. The spread that widened during Δ is paid in full.
    #[test]
    fn a_marketable_ioc_bid_fills_at_the_ask() {
        let mut ab = 5_000_000i64;
        let mut bb = 5_000_000i64;
        let t = touch(99_000_000, 5_000_000, 100_000_000, 5_000_000);
        let v = judge_ioc(Side::Bid, 101_000_000, 1_000_000, t, &mut ab, &mut bb);
        assert_eq!(v, Verdict::Fill { px_1e6: 100_000_000, qty_1e6: 1_000_000 });
        assert_eq!(ab, 4_000_000, "the displayed ask size was consumed");
        assert_eq!(bb, 5_000_000, "the bid side is untouched");
        // AT the touch is still marketable for a taker (`<=`).
        let mut ab = 5_000_000i64;
        let v = judge_ioc(Side::Bid, 100_000_000, 1_000_000, t, &mut ab, &mut bb);
        assert_eq!(v, Verdict::Fill { px_1e6: 100_000_000, qty_1e6: 1_000_000 });
    }

    #[test]
    fn a_marketable_ioc_ask_fills_at_the_bid() {
        let mut ab = 5_000_000i64;
        let mut bb = 5_000_000i64;
        let t = touch(99_000_000, 5_000_000, 100_000_000, 5_000_000);
        let v = judge_ioc(Side::Ask, 98_000_000, 1_000_000, t, &mut ab, &mut bb);
        assert_eq!(v, Verdict::Fill { px_1e6: 99_000_000, qty_1e6: 1_000_000 });
        assert_eq!(bb, 4_000_000);
        assert_eq!(ab, 5_000_000);
    }

    /// THE F7 CASE. A mid-priced IoC on a positive spread is not
    /// marketable on either side, so it cancels — which is exactly what
    /// the VRP member's option entry did, live, twice, while it
    /// believed it held the position.
    #[test]
    fn a_mid_priced_ioc_on_a_real_spread_cancels_on_both_sides() {
        let t = touch(99_000_000, 5_000_000, 101_000_000, 5_000_000);
        let mid = 100_000_000;
        let mut ab = 5_000_000i64;
        let mut bb = 5_000_000i64;
        assert_eq!(
            judge_ioc(Side::Bid, mid, 1_000_000, t, &mut ab, &mut bb),
            Verdict::Cancel
        );
        assert_eq!(
            judge_ioc(Side::Ask, mid, 1_000_000, t, &mut ab, &mut bb),
            Verdict::Cancel
        );
        assert_eq!((ab, bb), (5_000_000, 5_000_000), "a cancel spends nothing");
    }

    /// An IoC caps at the displayed size and the remainder CANCELS —
    /// it never rests, so this can never be `Wait`.
    #[test]
    fn an_ioc_is_capped_by_the_displayed_size_and_never_waits() {
        let mut ab = 400_000i64;
        let mut bb = 5_000_000i64;
        let t = touch(99_000_000, 5_000_000, 100_000_000, 400_000);
        let v = judge_ioc(Side::Bid, 101_000_000, 1_000_000, t, &mut ab, &mut bb);
        assert_eq!(v, Verdict::Fill { px_1e6: 100_000_000, qty_1e6: 400_000 });
        assert_eq!(ab, 0);
        // With the budget spent, the next one cancels rather than waits.
        let v = judge_ioc(Side::Bid, 101_000_000, 1_000_000, t, &mut ab, &mut bb);
        assert_eq!(v, Verdict::Cancel);
    }

    // ---------------- maker: one arm per test ----------------

    /// STRICT cross: at the touch a maker WAITS, because the queue
    /// ahead of it is unknowable. One tick either side of that line.
    #[test]
    fn a_maker_needs_a_strict_cross_and_fills_at_its_own_limit() {
        let mut ab = 5_000_000i64;
        let mut bb = 5_000_000i64;
        // Ask exactly AT our bid limit: no fill.
        let t = touch(99_000_000, 5_000_000, 100_000_000, 5_000_000);
        assert_eq!(
            judge_maker(Side::Bid, 100_000_000, 1_000_000, t, &mut ab, &mut bb),
            Verdict::Wait,
            "resting AT the touch is a queue, not a fill"
        );
        // One unit through it: filled, at OUR limit and not the ask.
        let t = touch(99_000_000, 5_000_000, 99_999_999, 5_000_000);
        assert_eq!(
            judge_maker(Side::Bid, 100_000_000, 1_000_000, t, &mut ab, &mut bb),
            Verdict::Fill { px_1e6: 100_000_000, qty_1e6: 1_000_000 }
        );
        assert_eq!(ab, 4_000_000);
        // The mirror on the ask side.
        let t = touch(100_000_001, 5_000_000, 101_000_000, 5_000_000);
        assert_eq!(
            judge_maker(Side::Ask, 100_000_000, 1_000_000, t, &mut ab, &mut bb),
            Verdict::Fill { px_1e6: 100_000_000, qty_1e6: 1_000_000 }
        );
        assert_eq!(bb, 4_000_000);
        let t = touch(100_000_000, 5_000_000, 101_000_000, 5_000_000);
        assert_eq!(
            judge_maker(Side::Ask, 100_000_000, 1_000_000, t, &mut ab, &mut bb),
            Verdict::Wait
        );
    }

    /// A partial leaves the remainder resting: `Wait` is the verdict
    /// for what is left, and the caller keeps the order.
    #[test]
    fn a_maker_partial_fills_what_is_displayed() {
        let mut ab = 300_000i64;
        let mut bb = 5_000_000i64;
        let t = touch(99_000_000, 5_000_000, 99_000_000, 300_000);
        let v = judge_maker(Side::Bid, 100_000_000, 1_000_000, t, &mut ab, &mut bb);
        assert_eq!(v, Verdict::Fill { px_1e6: 100_000_000, qty_1e6: 300_000 });
        assert_eq!(ab, 0);
        assert_eq!(
            judge_maker(Side::Bid, 100_000_000, 700_000, t, &mut ab, &mut bb),
            Verdict::Wait,
            "the budget is spent; the remainder keeps resting"
        );
    }

    /// ONE tick's displayed size cannot fill two orders twice. FIFO in
    /// emit order, makers and IoCs sharing the same budget.
    #[test]
    fn two_orders_on_one_tick_share_one_displayed_size() {
        let mut ab = 1_000_000i64;
        let mut bb = 1_000_000i64;
        let t = touch(99_000_000, 1_000_000, 100_000_000, 1_000_000);
        // First in emit order takes 600k of the 1M displayed.
        let a = judge_ioc(Side::Bid, 101_000_000, 600_000, t, &mut ab, &mut bb);
        assert_eq!(a, Verdict::Fill { px_1e6: 100_000_000, qty_1e6: 600_000 });
        // The second — a MAKER this time — sees only what is left.
        let t2 = touch(99_000_000, 1_000_000, 99_000_000, 1_000_000);
        let b = judge_maker(Side::Bid, 100_000_000, 900_000, t2, &mut ab, &mut bb);
        assert_eq!(b, Verdict::Fill { px_1e6: 100_000_000, qty_1e6: 400_000 });
        assert_eq!(ab, 0, "one size, spent once");
    }

    // ---------------- evidence + TTL ----------------

    #[test]
    fn only_a_fresh_two_sided_tick_is_evidence() {
        assert!(is_fill_evidence(&tick(99_000_000, 100_000_000, false)));
        assert!(
            !is_fill_evidence(&tick(99_000_000, 100_000_000, true)),
            "VT4: a stale book is not evidence"
        );
        assert!(!is_fill_evidence(&tick(0, 100_000_000, false)), "one-sided");
        assert!(!is_fill_evidence(&tick(99_000_000, 0, false)), "one-sided");
        assert!(Touch::of(&tick(99_000_000, 100_000_000, false)).two_sided());
        assert!(!Touch::of(&tick(0, 100_000_000, false)).two_sided());
    }

    #[test]
    fn the_ttl_expires_at_the_boundary_and_zero_never_does() {
        assert!(!ttl_expired(1_999, 1_000, 1_000));
        assert!(ttl_expired(2_000, 1_000, 1_000), "at/after, not after");
        assert!(ttl_expired(9_999, 1_000, 1_000));
        assert!(!ttl_expired(u64::MAX, 1_000, 0), "ttl 0 never expires");
        // No overflow panic on a saturating emit+ttl.
        assert!(!ttl_expired(1, u64::MAX, u64::MAX));
        // The two spellings are ONE predicate: what the harness stores
        // precomputed and what an order carries have to agree.
        assert_eq!(expiry_at(1_000, 1_000), 2_000);
        assert_eq!(expiry_at(1_000, 0), 0, "0 = never, not `emit`");
        assert_eq!(expiry_at(u64::MAX, 5), u64::MAX, "saturating");
        let mut emit = 0u64;
        while emit < 4 {
            let mut ttl = 0u64;
            while ttl < 4 {
                let mut now = 0u64;
                while now < 8 {
                    assert_eq!(
                        ttl_expired(now, emit, ttl),
                        expired_at(now, expiry_at(emit, ttl)),
                        "emit {emit} ttl {ttl} now {now}"
                    );
                    now += 1;
                }
                ttl += 1;
            }
            emit += 1;
        }
    }

    #[test]
    fn a_negative_displayed_size_is_zero_not_a_smaller_one() {
        let mut t = tick(99_000_000, 100_000_000, false);
        t.bid_qty = Qty::from_raw(-5);
        assert_eq!(Touch::of(&t).bid_qty_1e6, 0);
    }

    #[test]
    fn the_activation_table_is_the_measured_one() {
        // Pinned so a re-measurement has to be deliberate, and so the
        // harness's `ModelParams::default()` and the paper dispatcher
        // can never drift apart.
        assert_eq!(
            ACTIVATION_NS_DEFAULT,
            [200 * MS, 130 * MS, 130 * MS, 220 * MS, 340 * MS, 0, 60 * MS]
        );
        assert_eq!(ACTIVATION_NS_DEFAULT[VenueId::Ai as usize], 0, "a dead slot");
    }
}
