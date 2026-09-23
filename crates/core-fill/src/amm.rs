// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # The AMM fill law (HYPARB H2, O-H9)
//!
//! A CLOB order is judged against a TICK the engine already holds. An AMM
//! swap is judged against POOL STATE, which nothing held — so the state
//! lives here, in [`AmmBook`], rebuilt from the same 40-byte pool-event
//! signals (`core_amm::payload`) the HyperEVM ingress writes to the ring
//! and to the capture tape. Both consumers — the engine's paper matcher
//! and the offline harness — feed their book the same signals and judge
//! with the same [`AmmBook::judge`], so a paper swap and its replayed fill
//! cannot disagree.
//!
//! ## The law
//!
//! * **Units.** An AMM order (`Order.kind` [`crate::ORDER_KIND_AMM_SWAP`])
//!   addresses a pool by its `sym`; `qty` is token0 in human units × 1e6,
//!   `px` the WORST average price accepted, token1 per token0 × 1e6.
//!   `Side::Ask` sells token0 into the pool (exact input), `Side::Bid`
//!   buys it (exact output). **The pool fee is IN the fill price** — on
//!   chain it is part of the execution price, and `Fill` has no fee field.
//! * **When.** An AMM order is judged ONCE, at the first `HEAD` at or after
//!   its activation (`emit + one block`), against the pool as it stands
//!   then: after every swap of the block the transaction raced. We fill as
//!   if LAST in that block — never ahead of flow we could not have seen.
//!   What does not fill cancels; a swap never rests (the IoC contract).
//! * **How much.** The ACTIVE RANGE only (`core_amm::fill_in_range`): no
//!   tick map, so a swap that would cross a tick is capped at the range
//!   boundary — LESS than the chain, never more.
//! * **Fee.** The worse of the fee in force (`SNAPSHOT`, Algebra `Fee`)
//!   and the last fee the tape showed a swap PAYING (`observed_fee_pips`,
//!   Algebra `SwapFee`): `fee()` does not report a dynamic-fee pool's
//!   charge (plan findings 15, 20), and the judge may not believe it does.
//! * **Our own impact** stays in the book until the chain's next `STATE`
//!   for that pool overwrites it, so a standing gap cannot be filled twice
//!   — the same rule the member carries (`ArbQuote::after`).
//! * **Staleness.** A pool is judgeable only between a completed snapshot
//!   (`SNAPSHOT` … `STATE(snapshot)`) and the next `GAP` touching it. A
//!   chain-wide `GAP` stales every pool AND cancels every open AMM order —
//!   a transaction in flight across a stream break has an unknowable fate.
//!
//! ## Doctrine
//!
//! Const-constructible, fixed arrays, no allocation, no panics, no floats.
//! The 256-bit arithmetic is `core_amm`'s; this module is the state
//! machine and the verdict.

use core_amm::payload::{
    decode, Payload, PoolEvent, FAMILY_ALGEBRA, FEE_SRC_ALGEBRA_V10, FEE_SRC_ALGEBRA_V12,
};
use core_amm::{
    fill_in_range, observed_fee_pips, PoolMeta, PoolState, AMM_KIND_ALGEBRA, AMM_KIND_V3,
    POOL_FLAG_EDGE, POOL_FLAG_STALE, RFILL_NONE, RFILL_NOT_LIVE,
};
use core_types::{Side, SymbolId, VenueId};

use crate::Verdict;

/// Pools one book holds — the HyperEVM ingress's own bound
/// (`ingress_hyperevm::HYPEREVM_MAX_POOLS`).
pub const AMM_MAX_POOLS: usize = 128;

/// Fees at or above this are not fees (100 %).
const PIPS: u32 = 1_000_000;

/// The book slot of a pool symbol: HyperEVM venue byte, ordinal
/// `1..=AMM_MAX_POOLS` → `ordinal − 1`. `None` for anything else.
#[inline]
#[must_use]
pub const fn amm_pool_index(sym: SymbolId) -> Option<usize> {
    if core_types::symbol_venue_byte(sym) != VenueId::HyperEvm as u8 {
        return None;
    }
    let ord = core_types::symbol_ordinal(sym) as usize;
    if ord == 0 || ord > AMM_MAX_POOLS {
        None
    } else {
        Some(ord - 1)
    }
}

/// Snapshot phase of one pool.
const PHASE_EMPTY: u8 = 0;
const PHASE_SNAPSHOT: u8 = 1;
const PHASE_LIVE: u8 = 2;

/// One pool's judgeable state. Two cache lines.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
struct AmmPool {
    /// Price, tick, liquidity, `POOL_FLAG_*`.
    state: PoolState,
    /// The last `SWAP` awaiting its `STATE` (pool's view, + = in).
    pending0: i128,
    pending1: i128,
    /// Fee in force (`SNAPSHOT`, Algebra v1.0 `Fee`), pips.
    fee_in_force: u32,
    /// The last fee a swap was seen to PAY, pips; 0 = none yet.
    fee_observed: u32,
    tick_spacing: i32,
    has_pending: bool,
    phase: u8,
    kind: u8,
    dec0: u8,
    dec1: u8,
    _pad: [u8; 15],
}
const _: () = assert!(core::mem::size_of::<AmmPool>() == 128);

const EMPTY_POOL: AmmPool = AmmPool {
    state: PoolState::ZERO,
    pending0: 0,
    pending1: 0,
    fee_in_force: 0,
    fee_observed: 0,
    tick_spacing: 1,
    has_pending: false,
    phase: PHASE_EMPTY,
    kind: AMM_KIND_V3,
    dec0: 0,
    dec1: 0,
    _pad: [0; 15],
};

impl AmmPool {
    /// The static facts the walk reads, charging `fee_pips`.
    #[inline]
    fn meta(&self, fee_pips: u32) -> PoolMeta {
        let mut m = PoolMeta::ZERO;
        m.fee_pips = fee_pips;
        m.tick_spacing = self.tick_spacing;
        m.dec0 = self.dec0;
        m.dec1 = self.dec1;
        m.kind = self.kind;
        m
    }

    /// The fee the judge charges: the worse of in-force and observed.
    #[inline]
    const fn judged_fee(&self) -> u32 {
        if self.fee_observed > self.fee_in_force {
            self.fee_observed
        } else {
            self.fee_in_force
        }
    }

    #[inline]
    fn stale(&mut self) {
        self.state.flags |= POOL_FLAG_STALE;
        self.has_pending = false;
    }
}

/// What one pool-event signal did to the book.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AmmObs {
    /// A new head: the clock AMM orders are judged on.
    Head {
        /// Block number.
        block: u64,
    },
    /// A chain-wide stream break: every pool is stale.
    Gap,
    /// Pool `index`'s state changed (or went stale).
    Pool {
        /// Book slot.
        index: usize,
    },
    /// Accepted, nothing judgeable changed (a `TICK`, a `SWAP` awaiting
    /// its `STATE`, an event for a pool not yet snapshotted).
    Quiet,
    /// Refused: a payload the codec does not accept, or a symbol that is
    /// not a pool slot. Counted, never half-applied.
    Refused,
}

/// Counters of the book (plain `u64`s; the owner mirrors them).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct AmmBookCounters {
    /// Signals accepted.
    pub events: u64,
    /// Signals refused ([`AmmObs::Refused`]).
    pub refused: u64,
    /// Snapshots completed (a pool went live).
    pub snapshots: u64,
    /// Swaps whose effective fee the tape revealed.
    pub fees_observed: u64,
    /// Pools marked stale (a `GAP`, a position the state refused).
    pub stale_marks: u64,
}

/// What [`AmmBook::judge`] decided, and why.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AmmVerdict {
    /// [`Verdict::Fill`] or [`Verdict::Cancel`] — never `Wait`.
    pub verdict: Verdict,
    /// `core_amm::RFILL_*` bits of the underlying walk.
    pub flags: u8,
}

impl AmmVerdict {
    /// The cancel of a swap whose pool is not judgeable.
    pub const NOT_LIVE: Self = Self {
        verdict: Verdict::Cancel,
        flags: RFILL_NONE | RFILL_NOT_LIVE,
    };

    /// Whether the pool, not the price, refused the swap.
    #[inline]
    #[must_use]
    pub const fn pool_not_live(&self) -> bool {
        self.flags & RFILL_NOT_LIVE != 0
    }
}

/// Every pool's judgeable state, rebuilt from the pool-event tape.
#[derive(Clone, Debug)]
#[repr(C, align(64))]
pub struct AmmBook {
    pools: [AmmPool; AMM_MAX_POOLS],
    head_block: u64,
    /// What the book did.
    pub counters: AmmBookCounters,
}

impl Default for AmmBook {
    fn default() -> Self {
        Self::new()
    }
}

impl AmmBook {
    /// An empty book: every pool unsnapshotted (not judgeable).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pools: [EMPTY_POOL; AMM_MAX_POOLS],
            head_block: 0,
            counters: AmmBookCounters {
                events: 0,
                refused: 0,
                snapshots: 0,
                fees_observed: 0,
                stale_marks: 0,
            },
        }
    }

    /// The last head seen.
    #[inline]
    #[must_use]
    pub const fn head_block(&self) -> u64 {
        self.head_block
    }

    /// Whether pool `index` may be judged right now.
    #[inline]
    #[must_use]
    pub fn is_live(&self, index: usize) -> bool {
        index < AMM_MAX_POOLS
            && self.pools[index].phase == PHASE_LIVE
            && self.pools[index].state.is_live()
    }

    /// The pool's current state (price, tick, liquidity, flags), for
    /// tests and the member's cross-checks.
    #[inline]
    #[must_use]
    pub fn state(&self, index: usize) -> Option<PoolState> {
        if index < AMM_MAX_POOLS {
            Some(self.pools[index].state)
        } else {
            None
        }
    }

    /// The live pool's mid price, token1 per token0 × 1e6 (floored) —
    /// the harness marks AMM inventory at it. `None` unless judgeable.
    #[inline]
    #[must_use]
    pub fn mid_1e6(&self, index: usize) -> Option<i64> {
        if !self.is_live(index) {
            return None;
        }
        let p = &self.pools[index];
        let px = core_amm::price_1e18_from_sqrt(
            p.state.sqrt_price_lo,
            p.state.sqrt_price_hi,
            p.dec0,
            p.dec1,
        ) / 1_000_000_000_000;
        if px == 0 || px > i64::MAX as u128 {
            None
        } else {
            Some(px as i64)
        }
    }

    /// The fee the judge would charge pool `index`, pips.
    #[inline]
    #[must_use]
    pub fn judged_fee(&self, index: usize) -> Option<u32> {
        if index < AMM_MAX_POOLS {
            Some(self.pools[index].judged_fee())
        } else {
            None
        }
    }

    /// Apply one pool-event signal (`sym` = the pool, or
    /// `SYMBOL_ID_NONE` for a chain-wide `HEAD` / `GAP`).
    pub fn observe(&mut self, sym: SymbolId, payload: &Payload) -> AmmObs {
        let Some(ev) = decode(payload) else {
            self.counters.refused = self.counters.refused.wrapping_add(1);
            return AmmObs::Refused;
        };
        let obs = match ev {
            PoolEvent::Head { block, .. } => {
                if block > self.head_block {
                    self.head_block = block;
                }
                AmmObs::Head { block }
            }
            PoolEvent::Gap { .. } => match amm_pool_index(sym) {
                Some(i) => {
                    self.pools[i].stale();
                    self.counters.stale_marks = self.counters.stale_marks.wrapping_add(1);
                    AmmObs::Pool { index: i }
                }
                None => {
                    let mut i = 0usize;
                    while i < AMM_MAX_POOLS {
                        if self.pools[i].phase != PHASE_EMPTY {
                            self.pools[i].stale();
                        }
                        i += 1;
                    }
                    self.counters.stale_marks = self.counters.stale_marks.wrapping_add(1);
                    AmmObs::Gap
                }
            },
            _ => match amm_pool_index(sym) {
                Some(i) => self.pool_event(i, ev),
                None => AmmObs::Refused,
            },
        };
        if obs == AmmObs::Refused {
            self.counters.refused = self.counters.refused.wrapping_add(1);
        } else {
            self.counters.events = self.counters.events.wrapping_add(1);
        }
        obs
    }

    fn pool_event(&mut self, i: usize, ev: PoolEvent) -> AmmObs {
        let p = &mut self.pools[i];
        match ev {
            PoolEvent::Snapshot {
                family,
                fee,
                spacing,
                dec0,
                dec1,
                ..
            } => {
                p.phase = PHASE_SNAPSHOT;
                p.fee_in_force = fee;
                p.fee_observed = 0;
                p.tick_spacing = spacing;
                p.kind = if family == FAMILY_ALGEBRA {
                    AMM_KIND_ALGEBRA
                } else {
                    AMM_KIND_V3
                };
                p.dec0 = dec0;
                p.dec1 = dec1;
                p.stale();
                AmmObs::Pool { index: i }
            }
            PoolEvent::Tick { .. } => AmmObs::Quiet,
            PoolEvent::State {
                tick,
                sqrt_price_lo,
                sqrt_price_hi,
                liquidity,
                snapshot,
            } => {
                if snapshot {
                    if p.phase == PHASE_EMPTY {
                        return AmmObs::Quiet;
                    }
                    p.state = PoolState::new(sqrt_price_lo, sqrt_price_hi, tick, liquidity);
                    p.state.pool = i as u16;
                    p.phase = PHASE_LIVE;
                    p.has_pending = false;
                    self.counters.snapshots = self.counters.snapshots.wrapping_add(1);
                    return AmmObs::Pool { index: i };
                }
                if p.phase != PHASE_LIVE {
                    return AmmObs::Quiet;
                }
                if p.has_pending {
                    let meta = p.meta(p.fee_in_force);
                    if let Some(f) = observed_fee_pips(
                        &p.state,
                        &meta,
                        p.pending0,
                        p.pending1,
                        sqrt_price_lo,
                        sqrt_price_hi,
                    ) {
                        p.fee_observed = f;
                        self.counters.fees_observed = self.counters.fees_observed.wrapping_add(1);
                    }
                    p.has_pending = false;
                }
                // The chain's state overwrites ours (and our own paper
                // impact with it); staleness survives until a snapshot.
                let keep = p.state.flags & POOL_FLAG_STALE;
                p.state.sqrt_price_lo = sqrt_price_lo;
                p.state.sqrt_price_hi = sqrt_price_hi;
                p.state.tick = tick;
                p.state.liquidity = liquidity;
                p.state.flags = keep;
                AmmObs::Pool { index: i }
            }
            PoolEvent::Swap {
                block,
                amount0,
                amount1,
            } => {
                if p.phase == PHASE_LIVE {
                    p.pending0 = amount0;
                    p.pending1 = amount1;
                    p.has_pending = true;
                    p.state.block = block;
                }
                AmmObs::Quiet
            }
            PoolEvent::Fee { source, a, b, .. } => {
                if source == FEE_SRC_ALGEBRA_V10 {
                    if a < PIPS {
                        p.fee_in_force = a;
                    }
                } else if source == FEE_SRC_ALGEBRA_V12 {
                    let base = if a != 0 { a } else { p.fee_in_force };
                    let paid = base.saturating_add(b);
                    if paid < PIPS {
                        p.fee_observed = paid;
                        self.counters.fees_observed = self.counters.fees_observed.wrapping_add(1);
                    }
                }
                AmmObs::Pool { index: i }
            }
            PoolEvent::Liquidity {
                burn,
                tick_lower,
                tick_upper,
                amount,
                ..
            } => {
                if p.phase != PHASE_LIVE {
                    return AmmObs::Quiet;
                }
                let delta = if amount > i128::MAX as u128 {
                    None
                } else if burn {
                    Some(-(amount as i128))
                } else {
                    Some(amount as i128)
                };
                let ok = match delta {
                    Some(d) => p.state.apply_position(tick_lower, tick_upper, d).is_ok(),
                    None => false,
                };
                if !ok {
                    p.stale();
                    self.counters.stale_marks = self.counters.stale_marks.wrapping_add(1);
                } else {
                    // A position change is chain truth for the range:
                    // our own edge-ended paper state no longer binds.
                    p.state.flags &= !POOL_FLAG_EDGE;
                }
                AmmObs::Pool { index: i }
            }
            // Handled by the caller.
            PoolEvent::Head { .. } | PoolEvent::Gap { .. } => AmmObs::Quiet,
        }
    }

    /// Judge one AMM order against pool `index` NOW, and carry its impact.
    ///
    /// `side` Ask sells `remaining_1e6` token0 into the pool, Bid buys it;
    /// `px_limit_1e6` is the worst average price accepted. A fill is at
    /// the swap's average price, pool fee included, for at most
    /// `remaining_1e6`; anything unfilled is gone (a swap never rests).
    /// A pool that is not live cancels the order.
    pub fn judge(
        &mut self,
        index: usize,
        side: Side,
        px_limit_1e6: i64,
        remaining_1e6: i64,
    ) -> AmmVerdict {
        if !self.is_live(index) {
            return AmmVerdict::NOT_LIVE;
        }
        let p = &mut self.pools[index];
        let meta = p.meta(p.judged_fee());
        let r = fill_in_range(
            &p.state,
            &meta,
            matches!(side, Side::Ask),
            remaining_1e6,
            px_limit_1e6,
        );
        if r.flags & RFILL_NONE != 0 {
            return AmmVerdict {
                verdict: Verdict::Cancel,
                flags: r.flags,
            };
        }
        p.state = r.after;
        AmmVerdict {
            verdict: Verdict::Fill {
                px_1e6: r.px_1e6,
                qty_1e6: r.qty_1e6,
            },
            flags: r.flags,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_amm::payload::{
        encode_fee, encode_gap, encode_head, encode_liquidity, encode_snapshot, encode_state,
        encode_swap, encode_tick, FAMILY_V3,
    };
    use core_amm::{price_1e18_from_sqrt, sqrt_at_tick, RFILL_PARTIAL};
    use core_types::{make_symbol_id, SYMBOL_ID_NONE};

    const POOL: SymbolId = make_symbol_id(VenueId::HyperEvm, 3);
    const TICK: i32 = -230_543;
    const L: u128 = 50_000_000_000_000_000_000;

    fn live_book() -> AmmBook {
        let mut b = AmmBook::new();
        let (lo, hi) = sqrt_at_tick(TICK);
        assert_eq!(
            b.observe(
                POOL,
                &encode_snapshot(7, FAMILY_V3, -240_000, -220_000, 1, 500, 10, 18, 6).unwrap()
            ),
            AmmObs::Pool { index: 2 }
        );
        assert_eq!(
            b.observe(POOL, &encode_tick(-230_550, 5, 5).unwrap()),
            AmmObs::Quiet
        );
        assert!(!b.is_live(2), "not live mid-snapshot");
        assert_eq!(
            b.observe(POOL, &encode_state(TICK, lo, hi, L, true).unwrap()),
            AmmObs::Pool { index: 2 }
        );
        assert!(b.is_live(2));
        b
    }

    fn mid(b: &AmmBook) -> i64 {
        let s = b.state(2).unwrap();
        (price_1e18_from_sqrt(s.sqrt_price_lo, s.sqrt_price_hi, 18, 6) / 1_000_000_000_000) as i64
    }

    #[test]
    fn pool_index_is_the_hyperevm_ordinal() {
        assert_eq!(amm_pool_index(POOL), Some(2));
        assert_eq!(amm_pool_index(make_symbol_id(VenueId::HyperEvm, 0)), None);
        assert_eq!(amm_pool_index(make_symbol_id(VenueId::HyperEvm, 129)), None);
        assert_eq!(
            amm_pool_index(make_symbol_id(VenueId::Hyperliquid, 3)),
            None
        );
        assert_eq!(amm_pool_index(SYMBOL_ID_NONE), None);
    }

    #[test]
    fn a_snapshot_makes_the_pool_judgeable_and_a_sell_fills_below_mid() {
        let mut b = live_book();
        let m = mid(&b);
        let before = b.state(2).unwrap();
        let v = b.judge(2, Side::Ask, m * 99 / 100, 1_000_000);
        let Verdict::Fill { px_1e6, qty_1e6 } = v.verdict else {
            panic!("{v:?}");
        };
        assert_eq!(qty_1e6, 1_000_000);
        assert!(px_1e6 < m && px_1e6 >= m * 99 / 100);
        assert!(
            b.state(2).unwrap().sqrt_price_lo < before.sqrt_price_lo,
            "our impact is carried"
        );
    }

    #[test]
    fn a_limit_the_pool_cannot_meet_cancels() {
        let mut b = live_book();
        let m = mid(&b);
        assert_eq!(b.judge(2, Side::Bid, m, 1_000_000).verdict, Verdict::Cancel);
        assert_eq!(b.judge(2, Side::Ask, m, 1_000_000).verdict, Verdict::Cancel);
    }

    #[test]
    fn a_swap_too_big_for_the_range_fills_partially_then_the_pool_is_edge_bound() {
        let mut b = live_book();
        let m = mid(&b);
        let v = b.judge(2, Side::Ask, m / 2, 10_000_000_000_000);
        assert!(v.flags & RFILL_PARTIAL != 0, "{v:?}");
        assert!(matches!(v.verdict, Verdict::Fill { .. }));
        assert!(!b.is_live(2), "edge-bound until the chain speaks");
        assert_eq!(
            b.judge(2, Side::Ask, m / 2, 1_000_000).verdict,
            Verdict::Cancel
        );
        // The chain's next STATE is truth again.
        let (lo, hi) = sqrt_at_tick(TICK);
        b.observe(POOL, &encode_state(TICK, lo, hi, L, false).unwrap());
        assert!(b.is_live(2));
    }

    #[test]
    fn a_gap_stales_every_pool_until_a_fresh_snapshot() {
        let mut b = live_book();
        assert_eq!(
            b.observe(SYMBOL_ID_NONE, &encode_gap(9).unwrap()),
            AmmObs::Gap
        );
        assert!(!b.is_live(2));
        let (lo, hi) = sqrt_at_tick(TICK);
        b.observe(POOL, &encode_state(TICK, lo, hi, L, false).unwrap());
        assert!(!b.is_live(2), "a swap STATE does not end a gap");
        assert_eq!(b.judge(2, Side::Ask, 1, 1).verdict, Verdict::Cancel);
        b.observe(
            POOL,
            &encode_snapshot(10, FAMILY_V3, -240_000, -220_000, 0, 500, 10, 18, 6).unwrap(),
        );
        b.observe(POOL, &encode_state(TICK, lo, hi, L, true).unwrap());
        assert!(b.is_live(2));
    }

    #[test]
    fn a_pool_gap_stales_only_that_pool() {
        let mut b = live_book();
        assert_eq!(
            b.observe(POOL, &encode_gap(9).unwrap()),
            AmmObs::Pool { index: 2 }
        );
        assert!(!b.is_live(2));
    }

    #[test]
    fn the_judge_charges_the_worse_of_the_fee_in_force_and_the_fee_observed() {
        let mut b = live_book();
        assert_eq!(b.judged_fee(2), Some(500));
        // A swap that paid 2,995 pips while fee() says 500.
        let st = b.state(2).unwrap();
        let mut meta = PoolMeta::ZERO;
        meta.fee_pips = 2_995;
        meta.tick_spacing = 10;
        let spec = core_amm::SwapSpec {
            amount: 3_000_000_000_000_000_000,
            limit_lo: core_amm::MIN_SQRT_LO + 1,
            limit_hi: 0,
            fee_pips: 2_995,
            zero_for_one: true,
            exact_in: true,
        };
        let r = core_amm::swap_exact_in_range(&st, &meta, &spec);
        b.observe(
            POOL,
            &encode_swap(11, r.amount_in as i128, -(r.amount_out as i128)).unwrap(),
        );
        b.observe(
            POOL,
            &encode_state(
                r.after.tick,
                r.after.sqrt_price_lo,
                r.after.sqrt_price_hi,
                r.after.liquidity,
                false,
            )
            .unwrap(),
        );
        let f = b.judged_fee(2).unwrap();
        assert!((2_995..=2_996).contains(&f), "judged {f}");
        assert_eq!(b.counters.fees_observed, 1);
        // Algebra v1.2: override 0 ⇒ lastFee + pluginFee.
        b.observe(
            POOL,
            &encode_fee(12, FEE_SRC_ALGEBRA_V12, 0, 4_000).unwrap(),
        );
        assert_eq!(b.judged_fee(2), Some(4_500));
        // v1.0: the fee in force moves; observed still binds if worse.
        b.observe(POOL, &encode_fee(13, FEE_SRC_ALGEBRA_V10, 100, 0).unwrap());
        assert_eq!(b.judged_fee(2), Some(4_500));
    }

    #[test]
    fn an_in_range_position_moves_liquidity_and_a_bad_one_stales_the_pool() {
        let mut b = live_book();
        let l0 = b.state(2).unwrap().liquidity;
        b.observe(
            POOL,
            &encode_liquidity(12, false, -230_600, -230_500, 7).unwrap(),
        );
        assert_eq!(b.state(2).unwrap().liquidity, l0 + 7);
        b.observe(
            POOL,
            &encode_liquidity(12, true, -250_000, -249_000, 7).unwrap(),
        );
        assert_eq!(
            b.state(2).unwrap().liquidity,
            l0 + 7,
            "out of range: map-only"
        );
        b.observe(
            POOL,
            &encode_liquidity(13, true, -230_600, -230_500, l0 + 8).unwrap(),
        );
        assert!(
            !b.is_live(2),
            "a burn below zero is a book that disagrees with the chain"
        );
    }

    #[test]
    fn heads_advance_and_foreign_symbols_or_bytes_are_refused() {
        let mut b = live_book();
        assert_eq!(
            b.observe(SYMBOL_ID_NONE, &encode_head(42, 1, 1).unwrap()),
            AmmObs::Head { block: 42 }
        );
        assert_eq!(b.head_block(), 42);
        assert_eq!(b.observe(SYMBOL_ID_NONE, &[0u8; 40]), AmmObs::Refused);
        let (lo, hi) = sqrt_at_tick(TICK);
        assert_eq!(
            b.observe(
                make_symbol_id(VenueId::Binance, 1),
                &encode_state(TICK, lo, hi, L, true).unwrap()
            ),
            AmmObs::Refused
        );
        assert_eq!(b.counters.refused, 2);
    }

    #[test]
    fn a_pool_never_snapshotted_is_not_judgeable() {
        let mut b = AmmBook::new();
        let (lo, hi) = sqrt_at_tick(TICK);
        assert_eq!(
            b.observe(POOL, &encode_state(TICK, lo, hi, L, true).unwrap()),
            AmmObs::Quiet
        );
        assert!(!b.is_live(2));
        assert_eq!(b.judge(2, Side::Bid, 1, 1).verdict, Verdict::Cancel);
        assert_eq!(
            b.judge(AMM_MAX_POOLS, Side::Bid, 1, 1).verdict,
            Verdict::Cancel
        );
    }
}
