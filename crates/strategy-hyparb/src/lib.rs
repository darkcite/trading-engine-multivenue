// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-hyparb — the slot-0 member (HYPARB, O-H1/O-H2)
//!
//! HyperEVM concentrated-liquidity pools against Hyperliquid spot and perp
//! books: the single-block CEX-DEX arb, paper-first (plan §8).
//!
//! ## What it does
//!
//! 1. **Pools from the tape.** Every HyperEVM pool-event signal feeds a
//!    `core_fill::AmmBook` — the SAME state machine the engine's paper
//!    matcher and the harness judge swaps with, so the member sizes against
//!    exactly the pool the judge will fill against (price, liquidity, the
//!    worse of the fee in force and the fee observed, staleness). The
//!    member additionally keeps each pool's TICK MAP (rebuilt from the
//!    snapshot signals, maintained by `Mint`/`Burn`) because sizing walks
//!    the real map (`core_amm::solve_arb`) and stops at its edge.
//! 2. **Hedge books.** Hyperliquid BBO ticks of each coin's perp and spot
//!    (O-H10); funding from the perp's `activeAssetCtx`.
//! 3. **The decision** (on a pool event, or a hedge tick of one of its
//!    coins): the profit-maximising single-block swap against hedge
//!    bounds with the hedge taker fees folded in, the pool's own basis
//!    de-meaned, the size capped by the hedge's live top-of-book, the gas
//!    of the attempt charged — submitted as one AMM swap
//!    (`ORDER_KIND_AMM_SWAP`) limited at the quote's last-unit price (the
//!    marginal bound the judge and the chain enforce). Our own impact is
//!    carried into the book until the chain overwrites it.
//! 4. **The hedge** is sent when the AMM leg FILLS — an IoC per non-USD
//!    coin, on the venue the selector chose, at the price the decision
//!    assumed. It lives `lag_ns`: a book that moved inside the latency is
//!    a miss, counted, and the residue is unhedged inventory.
//! 5. **Unhedged inventory is first-class:** tracked per coin, flattened by
//!    the 1 s timer at the touch, capped — a breach halts new arbs until
//!    it is back under half the cap.
//!
//! ## The four corrections (plan §8.4) — each a config knob
//!
//! Depth cap (`depth_cap_enabled`), latency (`lag_ns`, and the one-block
//! Δ the judge applies), basis (`basis_enabled`, `basis_window_ns`), gas
//! (`gas_p50_usd_1e6` charged per ATTEMPT — reverted swaps pay too).
//!
//! ## Doctrine
//!
//! Boot allocates the tick maps (one box, ~4 MiB) and the snapshot
//! staging buffer; no callback allocates. No floats. No `unsafe`.
//! Every callback opens with the configured check (the bin15 convention).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod hedge;
mod pools;

use core_amm::{ArbParams, ArbQuote, ArbSide, TickMap, TickNode, ARB_FLAG_SIZE_CAPPED};
use core_types::{
    ChannelEvent, ChannelId, Fill, NsTs, Order, Price, Qty, Side, Signal, SignalSource, SymbolId,
    Tick, VenueId, SYMBOL_ID_NONE,
};
use strategy_core::{
    CooldownGate, Ctx, HyparbCoinView, HyparbCounters, HyparbDecision, HyparbPoolView, Strategy,
    StrategyCounters, StrategyError, SubmitErr, HYPARB_DECISION_LOG,
};

pub use hedge::{choose_venue, CoinTouch, HedgeMode, HedgeVenue, VenueCost};

/// Pools the member tracks — the AMM book's bound.
pub const HYPARB_MAX_POOLS: usize = core_fill::AMM_MAX_POOLS;
/// Hedge coins the member tracks.
pub const HYPARB_MAX_COINS: usize = 8;
/// Initialised ticks one pool's map holds (the snapshot's cap).
pub const MAP_NODES: usize = 1024;
/// A pool side that needs no hedge: the token IS the USD numéraire.
pub const COIN_USD: u8 = u8::MAX;

/// One unit ×1e6.
const E6: i64 = 1_000_000;
/// 100 % in bps × 1e6.
const ONE_BPS_1E6: i64 = 10_000 * E6;
/// A timer tick, ns.
const TIMER_NS: u64 = 1_000_000_000;
/// How long an AMM swap may stay in flight before the member stops
/// waiting for its fill (the judge decides at the first head after one
/// block; three blocks is generous).
const INFLIGHT_NS: u64 = 3_000_000_000;
/// Grace after a hedge IoC's lifetime before an unfilled one is a miss.
const HEDGE_GRACE_NS: u64 = 1_000_000_000;
/// ns per UTC day.
const DAY_NS: u64 = 86_400_000_000_000;

/// One hedge coin: its Hyperliquid books and its lot law.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CoinParams {
    /// The perp's symbol (`SYMBOL_ID_NONE` = no perp hedge).
    pub perp_sym: SymbolId,
    /// The spot pair's symbol (`SYMBOL_ID_NONE` = no spot hedge).
    pub spot_sym: SymbolId,
    /// Order-size step, coin units × 1e6 (the venue's `szDecimals`).
    pub lot_1e6: i64,
    /// The venue's minimum order notional, USD × 1e6.
    pub min_notional_usd_1e6: i64,
}

impl CoinParams {
    /// An unconfigured coin.
    pub const NONE: Self = Self {
        perp_sym: SYMBOL_ID_NONE,
        spot_sym: SYMBOL_ID_NONE,
        lot_1e6: 0,
        min_notional_usd_1e6: 0,
    };
}

/// One pool: which coins hedge its tokens, and whether it trades.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PoolParams {
    /// The pool's symbol (`make_symbol_id(HyperEvm, i + 1)`).
    pub sym: SymbolId,
    /// token0's hedge coin, or [`COIN_USD`].
    pub coin0: u8,
    /// token1's hedge coin, or [`COIN_USD`].
    pub coin1: u8,
    /// `false` = observe only (the pool is tracked, never traded).
    pub trade: bool,
    /// Per-arb notional cap for this pool, USD × 1e6.
    pub max_notional_usd_1e6: i64,
}

impl PoolParams {
    /// An unconfigured pool.
    pub const NONE: Self = Self {
        sym: SYMBOL_ID_NONE,
        coin0: COIN_USD,
        coin1: COIN_USD,
        trade: false,
        max_notional_usd_1e6: 0,
    };
}

/// Everything the member is configured with — parsed and bound-checked
/// by the cli from `hyparb.toml` (the member reads no TOML: one grammar,
/// one artifact hash). [`HyparbParams::validate`] re-checks the shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HyparbParams {
    /// Hedge coins, `[..n_coins]` used.
    pub coins: [CoinParams; HYPARB_MAX_COINS],
    /// Configured coins.
    pub n_coins: usize,
    /// Pools, `[..n_pools]` used.
    pub pools: [PoolParams; HYPARB_MAX_POOLS],
    /// Configured pools.
    pub n_pools: usize,
    /// Latency: a hedge IoC lives this long after the AMM fill; a book
    /// that moved inside it is a miss.
    pub lag_ns: u64,
    /// Basis EMA horizon.
    pub basis_window_ns: u64,
    /// De-mean each pool against its own pool-vs-hedge basis.
    pub basis_enabled: bool,
    /// Cap each arb at the hedge books' live top-of-book.
    pub depth_cap_enabled: bool,
    /// Gas per attempt, USD × 1e6 (charged on every swap submitted).
    pub gas_p50_usd_1e6: i64,
    /// Gas p99, USD × 1e6 (the G2 bid cap — carried for the exec arm).
    pub gas_p99_usd_1e6: i64,
    /// Per-arb notional cap, USD × 1e6.
    pub max_order_usd_1e6: i64,
    /// Daily AMM notional cap, USD × 1e6 (UTC day).
    pub cap_day_usd_1e6: i64,
    /// Least net edge worth an attempt, bps × 1e6 (after fees and gas).
    pub min_net_bps_1e6: i64,
    /// Unhedged-inventory cap, USD × 1e6; a breach halts new arbs.
    pub inventory_cap_usd_1e6: i64,
    /// Which hedge venue (auto / forced perp / forced spot).
    pub hedge_mode: HedgeMode,
    /// The selector's anti-flap margin, bps × 1e6.
    pub hedge_switch_hysteresis_bps_1e6: i64,
    /// Perp taker fee, bps × 1e6.
    pub perp_taker_bps_1e6: i64,
    /// Spot taker fee, bps × 1e6.
    pub spot_taker_bps_1e6: i64,
    /// Expected hedge hold for the funding term, ns.
    pub funding_window_ns: u64,
    /// Least time between two arbs on one pool, ns.
    pub cooldown_ns: u64,
    /// The coin gas is paid in (HYPE on HyperEVM), or [`COIN_USD`] when
    /// no configured coin is it: each decision records its USD mid so a
    /// gas bid in USD can be priced in wei (H8).
    pub gas_coin: u8,
}

impl HyparbParams {
    /// No coins, no pools, every knob zero — `validate` refuses it.
    pub const EMPTY: Self = Self {
        coins: [CoinParams::NONE; HYPARB_MAX_COINS],
        n_coins: 0,
        pools: [PoolParams::NONE; HYPARB_MAX_POOLS],
        n_pools: 0,
        lag_ns: 0,
        basis_window_ns: 0,
        basis_enabled: false,
        depth_cap_enabled: false,
        gas_p50_usd_1e6: 0,
        gas_p99_usd_1e6: 0,
        max_order_usd_1e6: 0,
        cap_day_usd_1e6: 0,
        min_net_bps_1e6: 0,
        inventory_cap_usd_1e6: 0,
        hedge_mode: HedgeMode::Auto,
        hedge_switch_hysteresis_bps_1e6: 0,
        perp_taker_bps_1e6: 0,
        spot_taker_bps_1e6: 0,
        funding_window_ns: 0,
        cooldown_ns: 0,
        gas_coin: COIN_USD,
    };

    /// The shape law: refuses what the member could not run honestly.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.n_pools == 0 || self.n_pools > HYPARB_MAX_POOLS {
            return Err("hyparb: 1..=128 pools");
        }
        if self.n_coins > HYPARB_MAX_COINS {
            return Err("hyparb: at most 8 coins");
        }
        if self.gas_coin != COIN_USD && self.gas_coin as usize >= self.n_coins {
            return Err("hyparb: the gas coin is not a configured coin");
        }
        let mut c = 0usize;
        while c < self.n_coins {
            let k = self.coins[c];
            let perp_ok = k.perp_sym == SYMBOL_ID_NONE
                || core_types::symbol_venue_byte(k.perp_sym) == VenueId::Hyperliquid as u8;
            let spot_ok = k.spot_sym == SYMBOL_ID_NONE
                || core_types::symbol_venue_byte(k.spot_sym) == VenueId::Hyperliquid as u8;
            if !perp_ok || !spot_ok {
                return Err("hyparb: a hedge book must be a Hyperliquid symbol");
            }
            if k.perp_sym == SYMBOL_ID_NONE && k.spot_sym == SYMBOL_ID_NONE {
                return Err("hyparb: a coin needs a perp or a spot book");
            }
            if k.lot_1e6 <= 0 || k.min_notional_usd_1e6 < 0 {
                return Err("hyparb: a coin needs a positive lot");
            }
            c += 1;
        }
        let mut p = 0usize;
        while p < self.n_pools {
            let q = self.pools[p];
            if core_fill::amm_pool_index(q.sym).is_none() {
                return Err("hyparb: a pool must be a HyperEVM pool symbol");
            }
            let coin_ok = |x: u8| x == COIN_USD || (x as usize) < self.n_coins;
            if !coin_ok(q.coin0) || !coin_ok(q.coin1) {
                return Err("hyparb: a pool names a coin that is not configured");
            }
            if q.coin0 == COIN_USD && q.coin1 == COIN_USD {
                return Err("hyparb: a pool of two USD tokens has nothing to hedge");
            }
            if q.max_notional_usd_1e6 <= 0 {
                return Err("hyparb: a pool needs a positive notional cap");
            }
            p += 1;
        }
        if self.max_order_usd_1e6 <= 0
            || self.cap_day_usd_1e6 <= 0
            || self.inventory_cap_usd_1e6 <= 0
            || self.gas_p50_usd_1e6 < 0
            || self.gas_p99_usd_1e6 < self.gas_p50_usd_1e6
            || self.basis_window_ns == 0
            || self.lag_ns == 0
            || self.perp_taker_bps_1e6 < 0
            || self.spot_taker_bps_1e6 < 0
            || self.perp_taker_bps_1e6 >= ONE_BPS_1E6
            || self.spot_taker_bps_1e6 >= ONE_BPS_1E6
            || self.hedge_switch_hysteresis_bps_1e6 < 0
        {
            return Err("hyparb: a cap, fee, gas or window is out of range");
        }
        Ok(())
    }
}

/// Per-pool member state beside the book.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct PoolRun {
    /// Book slot of this pool (`amm_pool_index(sym)`).
    book: u16,
    /// The map is loaded and every position since applied cleanly.
    map_ok: bool,
    /// The spacing the MAP walks with (Algebra: 1 — its list, not a grid).
    map_spacing: i32,
    /// An AMM swap of ours is in flight until this instant (0 = none).
    inflight_until: u64,
    /// Basis EMA, bps × 1e6, when it was last moved, and whether it
    /// has its first sample.
    basis_bps_1e6: i64,
    basis_ns: u64,
    basis_live: bool,
    /// The hedge prices the last decision assumed, per coin side
    /// (token0 / token1), ×1e6 USD, and the venue it chose.
    hedge_px_1e6: [i64; 2],
    hedge_venue: [HedgeVenue; 2],
    /// Arbs submitted.
    arbs: u64,
    /// The solver's predicted net P&L over this pool's arbs, USD × 1e6.
    pnl_predicted_usd_1e6: i64,
}

const POOL_RUN_NONE: PoolRun = PoolRun {
    book: u16::MAX,
    map_ok: false,
    map_spacing: 1,
    inflight_until: 0,
    basis_bps_1e6: 0,
    basis_ns: 0,
    basis_live: false,
    hedge_px_1e6: [0; 2],
    hedge_venue: [HedgeVenue::None; 2],
    arbs: 0,
    pnl_predicted_usd_1e6: 0,
};

/// Per-coin member state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct CoinRun {
    perp: CoinTouch,
    spot: CoinTouch,
    /// HL hourly funding rate × 1e9 (the perp's `activeAssetCtx`).
    funding_1e9: i64,
    /// Unhedged inventory, coin units × 1e6, signed.
    inventory_1e6: i64,
    /// Hedge quantity sent and not yet filled, signed like inventory's
    /// correction (a sell is negative), and when it stops counting.
    pending_1e6: i64,
    pending_until: u64,
    /// The selector's last choice per side (0 = we sell, 1 = we buy).
    last_venue: [HedgeVenue; 2],
    /// The selector's last total cost per venue (perp, spot), bps × 1e6.
    last_cost_bps_1e6: [i64; 2],
    /// Net perp position the hedge fills built, coin × 1e6, signed.
    perp_pos_1e6: i64,
}

const COIN_RUN_NONE: CoinRun = CoinRun {
    perp: CoinTouch::EMPTY,
    spot: CoinTouch::EMPTY,
    funding_1e9: 0,
    inventory_1e6: 0,
    pending_1e6: 0,
    pending_until: 0,
    last_venue: [HedgeVenue::None; 2],
    last_cost_bps_1e6: [0; 2],
    perp_pos_1e6: 0,
};

/// The snapshot being staged (one pool at a time — the ingress emits each
/// pool's `SNAPSHOT · TICK… · STATE` contiguously).
struct Staging {
    nodes: Box<[TickNode; MAP_NODES]>,
    n: usize,
    expect: usize,
    /// Member pool index, `usize::MAX` = none.
    pool: usize,
    lo: i32,
    hi: i32,
    spacing: i32,
    broken: bool,
}

/// The slot-0 member.
pub struct HyparbStrategy {
    params: HyparbParams,
    configured: bool,
    book: core_fill::AmmBook,
    maps: Box<[TickMap<MAP_NODES>]>,
    staging: Staging,
    pools: [PoolRun; HYPARB_MAX_POOLS],
    coins: [CoinRun; HYPARB_MAX_COINS],
    /// Book slot → member pool index (`u8::MAX` = not configured).
    by_book: [u8; HYPARB_MAX_POOLS],
    cooldown: CooldownGate<HYPARB_MAX_POOLS>,
    anchor: core_time::WallAnchor,
    day: u64,
    day_notional_usd_1e6: i64,
    halted: bool,
    oid_seq: u64,
    counters: HyparbCounters,
    orders_emitted: u64,
    /// When funding was last accrued (0 = never).
    last_funding_ns: u64,
    /// The last [`HYPARB_DECISION_LOG`] AMM decisions, a ring indexed by
    /// `seq % HYPARB_DECISION_LOG`.
    decisions: [HyparbDecision; HYPARB_DECISION_LOG],
    /// The last decision's `seq` (0 = none yet).
    decision_seq: u64,
}

impl Default for HyparbStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for HyparbStrategy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HyparbStrategy")
            .field("configured", &self.configured)
            .field("n_pools", &self.params.n_pools)
            .field("n_coins", &self.params.n_coins)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl HyparbStrategy {
    /// An unconfigured member. Boot only: allocates the tick maps (one
    /// box) and the staging buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            params: HyparbParams::EMPTY,
            configured: false,
            book: core_fill::AmmBook::new(),
            maps: vec![TickMap::<MAP_NODES>::EMPTY; HYPARB_MAX_POOLS].into_boxed_slice(),
            staging: Staging {
                nodes: vec![TickNode::ZERO; MAP_NODES]
                    .into_boxed_slice()
                    .try_into()
                    .expect("MAP_NODES nodes"),
                n: 0,
                expect: 0,
                pool: usize::MAX,
                lo: 0,
                hi: 0,
                spacing: 1,
                broken: false,
            },
            pools: [POOL_RUN_NONE; HYPARB_MAX_POOLS],
            coins: [COIN_RUN_NONE; HYPARB_MAX_COINS],
            by_book: [u8::MAX; HYPARB_MAX_POOLS],
            cooldown: CooldownGate::new(0),
            anchor: core_time::WallAnchor::new(0, 0),
            day: 0,
            day_notional_usd_1e6: 0,
            halted: false,
            oid_seq: 0,
            counters: HyparbCounters {
                pool_events: 0,
                pool_refused: 0,
                maps_loaded: 0,
                maps_refused: 0,
                evaluations: 0,
                arbs_submitted: 0,
                skipped_below_min: 0,
                skipped_not_live: 0,
                skipped_no_hedge: 0,
                skipped_inflight: 0,
                skipped_cooldown: 0,
                skipped_halted: 0,
                size_capped: 0,
                amm_fills: 0,
                hedges_submitted: 0,
                hedges_perp: 0,
                hedges_spot: 0,
                hedge_fills: 0,
                hedges_missed: 0,
                flattens_submitted: 0,
                inventory_breaches: 0,
                orders_dropped: 0,
                gas_charged_usd_1e6: 0,
                pnl_predicted_usd_1e6: 0,
                amm_notional_usd_1e6: 0,
                arbs_buy: 0,
                arbs_sell: 0,
                funding_earned_usd_1e6: 0,
                halted: 0,
            },
            orders_emitted: 0,
            last_funding_ns: 0,
            decisions: [HyparbDecision {
                seq: 0,
                ts_ns: 0,
                edge_usd_1e6: 0,
                notional_usd_1e6: 0,
                gas_px_usd_1e6: 0,
                pool_sym: 0,
                buy: 0,
                _pad: [0; 3],
            }; HYPARB_DECISION_LOG],
            decision_seq: 0,
        }
    }

    /// Configure (boot only). `anchor` maps the engine's monotonic clock
    /// to wall time for the UTC-day cap.
    pub fn configure(
        &mut self,
        params: HyparbParams,
        anchor: core_time::WallAnchor,
    ) -> Result<(), StrategyError> {
        params.validate().map_err(StrategyError::Config)?;
        let mut p = 0usize;
        while p < params.n_pools {
            let Some(b) = core_fill::amm_pool_index(params.pools[p].sym) else {
                return Err(StrategyError::Config("hyparb: pool symbol"));
            };
            if self.by_book[b] != u8::MAX && self.by_book[b] as usize != p {
                return Err(StrategyError::Config("hyparb: a pool is configured twice"));
            }
            self.by_book[b] = p as u8;
            self.pools[p] = POOL_RUN_NONE;
            self.pools[p].book = b as u16;
            p += 1;
        }
        self.cooldown.set_cooldown_ns(params.cooldown_ns);
        self.anchor = anchor;
        self.params = params;
        self.configured = true;
        Ok(())
    }

    /// The member's counters.
    #[must_use]
    pub const fn counters(&self) -> HyparbCounters {
        self.counters
    }

    /// Unhedged inventory of coin `c`, coin units × 1e6.
    #[must_use]
    pub fn inventory_1e6(&self, c: usize) -> Option<i64> {
        if c < self.params.n_coins {
            Some(self.coins[c].inventory_1e6)
        } else {
            None
        }
    }

    /// Whether new arbs are halted (the inventory cap).
    #[must_use]
    pub const fn is_halted(&self) -> bool {
        self.halted
    }

    /// The AMM book the member sizes against (tests, cross-checks).
    #[must_use]
    pub const fn book(&self) -> &core_fill::AmmBook {
        &self.book
    }

    #[inline]
    fn next_oid(&mut self) -> u64 {
        self.oid_seq = self.oid_seq.wrapping_add(1);
        // Bits 0..32 are the instance convention (`OID_INSTANCE_MASK`);
        // perps and spot pairs have none, so the sequence rides above it.
        self.oid_seq << 32
    }

    /// Submit one order; `true` when the context took it.
    fn submit<C: Ctx>(&mut self, order: Order, ctx: &mut C) -> bool {
        match ctx.submit(order) {
            Ok(()) => {
                self.orders_emitted = self.orders_emitted.wrapping_add(1);
                true
            }
            Err(
                SubmitErr::RingFull
                | SubmitErr::Unsupported
                | SubmitErr::NoSuchOrder
                | SubmitErr::Refused,
            ) => {
                self.counters.orders_dropped = self.counters.orders_dropped.wrapping_add(1);
                false
            }
        }
    }

    /// The member pool index of a pool symbol.
    #[inline]
    fn pool_of(&self, sym: SymbolId) -> Option<usize> {
        let b = core_fill::amm_pool_index(sym)?;
        let p = self.by_book[b];
        if p == u8::MAX {
            None
        } else {
            Some(p as usize)
        }
    }

    /// The coin a hedge-book symbol belongs to, and whether it is the spot.
    #[inline]
    fn coin_of(&self, sym: SymbolId) -> Option<(usize, bool)> {
        let mut c = 0usize;
        while c < self.params.n_coins {
            let k = self.params.coins[c];
            if k.perp_sym == sym {
                return Some((c, false));
            }
            if k.spot_sym == sym {
                return Some((c, true));
            }
            c += 1;
        }
        None
    }

    /// USD mid of a pool side's coin, ×1e6 (the USD side is exactly 1).
    #[inline]
    fn usd_mid_1e6(&self, coin: u8) -> Option<i64> {
        if coin == COIN_USD {
            return Some(E6);
        }
        let r = &self.coins[coin as usize];
        match r.perp.mid_1e6() {
            Some(m) => Some(m),
            None => r.spot.mid_1e6(),
        }
    }

    /// The least notional a hedge of pool `pp` can carry: the larger
    /// venue minimum of its non-USD coins (at least one micro-dollar).
    #[inline]
    fn min_hedge_usd_1e6(&self, pp: &PoolParams) -> i64 {
        let mut m = 1i64;
        if pp.coin0 != COIN_USD {
            m = m.max(self.params.coins[pp.coin0 as usize].min_notional_usd_1e6);
        }
        if pp.coin1 != COIN_USD {
            m = m.max(self.params.coins[pp.coin1 as usize].min_notional_usd_1e6);
        }
        m
    }

    /// Roll the UTC day; the day cap restarts.
    fn roll_day(&mut self, now: NsTs) {
        let d = self.anchor.wall_of(now) / DAY_NS;
        if d != self.day {
            self.day = d;
            self.day_notional_usd_1e6 = 0;
        }
    }
}

// ---------------------------------------------------------------
// The decision
// ---------------------------------------------------------------

/// `a × b / d` for positive i64 operands, rounded down, saturating.
#[inline]
fn mul_div_i64(a: i64, b: i64, d: i64) -> i64 {
    if d <= 0 {
        return 0;
    }
    let v = (a as i128) * (b as i128) / (d as i128);
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// `num / den × 1e18`, `num` first shifted by `basis_bps_1e6` (the pool's
/// persistent premium over the hedge moves the hedge bound with it).
/// `None` for a non-positive result.
#[inline]
fn ratio_1e18(num_1e6: i64, den_1e6: i64, basis_bps_1e6: i64) -> Option<u128> {
    if num_1e6 <= 0 || den_1e6 <= 0 {
        return None;
    }
    let shifted =
        (num_1e6 as i128) * ((ONE_BPS_1E6 + basis_bps_1e6) as i128) / (ONE_BPS_1E6 as i128);
    if shifted <= 0 {
        return None;
    }
    // shifted ≤ 2 · i64::MAX, so × 1e18 < 2^128.
    let r = (shifted as u128) * 1_000_000_000_000_000_000 / (den_1e6 as u128);
    if r > 0 {
        Some(r)
    } else {
        None
    }
}

impl HyparbStrategy {
    /// Evaluate pool `p` now and, when the edge clears every gate, submit
    /// its AMM swap.
    fn evaluate<C: Ctx>(&mut self, p: usize, now: NsTs, ctx: &mut C) {
        let pp = self.params.pools[p];
        if !pp.trade {
            return;
        }
        let b = self.pools[p].book as usize;
        // A size the hedge venue would refuse is not an arb: a day budget
        // below the pool's hedge minimum is spent.
        let min_hedge = self.min_hedge_usd_1e6(&pp);
        if self.halted || self.params.cap_day_usd_1e6 - self.day_notional_usd_1e6 < min_hedge {
            self.counters.skipped_halted = self.counters.skipped_halted.wrapping_add(1);
            return;
        }
        if self.pools[p].inflight_until > now {
            self.counters.skipped_inflight = self.counters.skipped_inflight.wrapping_add(1);
            return;
        }
        if !self.cooldown.allow(p, now) {
            self.counters.skipped_cooldown = self.counters.skipped_cooldown.wrapping_add(1);
            return;
        }
        let (Some(state), Some(mut meta)) = (self.book.state(b), self.book.pool_meta(b)) else {
            self.counters.skipped_not_live = self.counters.skipped_not_live.wrapping_add(1);
            return;
        };
        if !self.pools[p].map_ok {
            self.counters.skipped_not_live = self.counters.skipped_not_live.wrapping_add(1);
            return;
        }
        meta.tick_spacing = self.pools[p].map_spacing;

        // ---- hedge bounds, per direction ----
        let cap_pool = pp.max_notional_usd_1e6.min(self.params.max_order_usd_1e6);
        // BuyToken0 SELLS coin0 and BUYS coin1 on the hedge books;
        // SellToken0 BUYS coin0 and SELLS coin1.
        let (Some(s0), Some(b1), Some(b0), Some(s1)) = (
            self.hedge_side(pp.coin0, true, cap_pool, now),
            self.hedge_side(pp.coin1, false, cap_pool, now),
            self.hedge_side(pp.coin0, false, cap_pool, now),
            self.hedge_side(pp.coin1, true, cap_pool, now),
        ) else {
            self.counters.skipped_no_hedge = self.counters.skipped_no_hedge.wrapping_add(1);
            return;
        };
        // Basis: the pool's persistent deviation is not edge.
        let basis = if self.params.basis_enabled {
            self.pools[p].basis_bps_1e6
        } else {
            0
        };
        // token1 per token0 × 1e18, hedge fees folded: one token0 sold on
        // the hedge buys `eff_bid` token1 back; `eff_ask` token1 buys one.
        let (Some(eff_bid_1e18), Some(eff_ask_1e18)) = (
            ratio_1e18(s0.eff_px_1e6, b1.eff_px_1e6, basis),
            ratio_1e18(b0.eff_px_1e6, s1.eff_px_1e6, basis),
        ) else {
            self.counters.skipped_no_hedge = self.counters.skipped_no_hedge.wrapping_add(1);
            return;
        };

        // ---- size ----
        let mut cap = cap_pool.min(self.params.cap_day_usd_1e6 - self.day_notional_usd_1e6);
        if self.params.depth_cap_enabled {
            // Only one direction can clear: a pool above the hedge
            // midpoint can only be sold into, one below only bought from.
            let pool_1e18 = core_amm::price_1e18_from_sqrt(
                state.sqrt_price_lo,
                state.sqrt_price_hi,
                meta.dec0,
                meta.dec1,
            );
            let rich = pool_1e18 >= eff_bid_1e18 / 2 + eff_ask_1e18 / 2;
            let depth = if rich {
                b0.depth_usd_1e6.min(s1.depth_usd_1e6)
            } else {
                s0.depth_usd_1e6.min(b1.depth_usd_1e6)
            };
            cap = cap.min(depth);
        }
        let Some(px0_usd_1e6) = self.usd_mid_1e6(pp.coin0) else {
            self.counters.skipped_no_hedge = self.counters.skipped_no_hedge.wrapping_add(1);
            return;
        };
        if cap < min_hedge {
            self.counters.skipped_below_min = self.counters.skipped_below_min.wrapping_add(1);
            return;
        }
        let q = core_amm::solve_arb(
            &state,
            &meta,
            &self.maps[p],
            &ArbParams {
                eff_bid_1e18,
                eff_ask_1e18,
                px0_usd_1e6,
                max_notional_usd_1e6: cap,
                gas_usd_1e6: self.params.gas_p50_usd_1e6,
            },
        );
        self.counters.evaluations = self.counters.evaluations.wrapping_add(1);
        if q.side == ArbSide::None || q.notional_usd_1e6 <= 0 {
            self.counters.skipped_below_min = self.counters.skipped_below_min.wrapping_add(1);
            return;
        }
        let net_bps_1e6 = mul_div_i64(q.pnl_usd_1e6, ONE_BPS_1E6, q.notional_usd_1e6);
        if net_bps_1e6 < self.params.min_net_bps_1e6 {
            self.counters.skipped_below_min = self.counters.skipped_below_min.wrapping_add(1);
            return;
        }
        let buy = q.side == ArbSide::BuyToken0;
        let (h0, h1) = if buy { (s0, b1) } else { (b0, s1) };
        self.submit_arb(p, &q, buy, [h0, h1], &meta, now, ctx);
    }

    /// The AMM leg of a decision: the quote's token0 quantity, limited at
    /// its LAST unit's price (`core_amm::limit_px_1e6` at the quote's
    /// `after`, the fee folded) — the marginal bound the judge and the
    /// chain's `sqrtPriceLimitX96` both enforce, so the quote completes
    /// on an unchanged pool and a pool that moved against it fills less.
    #[allow(clippy::too_many_arguments)]
    fn submit_arb<C: Ctx>(
        &mut self,
        p: usize,
        q: &ArbQuote,
        buy: bool,
        hedges: [HedgeLeg; 2],
        meta: &core_amm::PoolMeta,
        now: NsTs,
        ctx: &mut C,
    ) {
        let (Some(qty_1e6), Some(px_1e6)) = (
            core_amm::qty_1e6_from_raw(q.token0_raw, meta.dec0),
            core_amm::limit_px_1e6(&q.after, meta, buy),
        ) else {
            self.counters.skipped_below_min = self.counters.skipped_below_min.wrapping_add(1);
            return;
        };
        if qty_1e6 <= 0 {
            self.counters.skipped_below_min = self.counters.skipped_below_min.wrapping_add(1);
            return;
        }
        let sym = self.params.pools[p].sym;
        let mut o = Order::new(
            now,
            VenueId::HyperEvm,
            sym,
            if buy { Side::Bid } else { Side::Ask },
            core_fill::ORDER_KIND_AMM_SWAP,
            Price::from_raw(px_1e6),
            Qty::from_raw(qty_1e6),
            self.next_oid(),
        );
        o.ttl_ns = INFLIGHT_NS;
        if !self.submit(o, ctx) {
            return;
        }
        let b = self.pools[p].book as usize;
        self.book.carry(b, q.after);
        self.cooldown.record_emit(p, now);
        let run = &mut self.pools[p];
        run.inflight_until = now.saturating_add(INFLIGHT_NS);
        run.hedge_px_1e6 = [hedges[0].px_1e6, hedges[1].px_1e6];
        run.hedge_venue = [hedges[0].venue, hedges[1].venue];
        run.arbs = run.arbs.wrapping_add(1);
        run.pnl_predicted_usd_1e6 = run.pnl_predicted_usd_1e6.saturating_add(q.pnl_usd_1e6);
        let c = &mut self.counters;
        c.arbs_submitted = c.arbs_submitted.wrapping_add(1);
        if buy {
            c.arbs_buy = c.arbs_buy.wrapping_add(1);
        } else {
            c.arbs_sell = c.arbs_sell.wrapping_add(1);
        }
        c.gas_charged_usd_1e6 = c
            .gas_charged_usd_1e6
            .saturating_add(self.params.gas_p50_usd_1e6);
        c.pnl_predicted_usd_1e6 = c.pnl_predicted_usd_1e6.saturating_add(q.pnl_usd_1e6);
        if q.flags & ARB_FLAG_SIZE_CAPPED != 0 {
            c.size_capped = c.size_capped.wrapping_add(1);
        }
        self.day_notional_usd_1e6 = self.day_notional_usd_1e6.saturating_add(q.notional_usd_1e6);
        self.record_decision(now, sym, buy, q);
    }

    /// Log one submitted AMM decision for the write path's shadow (H8).
    #[inline]
    fn record_decision(&mut self, now: NsTs, sym: SymbolId, buy: bool, q: &ArbQuote) {
        self.decision_seq += 1;
        let gas_px_usd_1e6 = if self.params.gas_coin == COIN_USD {
            0
        } else {
            self.usd_mid_1e6(self.params.gas_coin).unwrap_or(0)
        };
        self.decisions[(self.decision_seq % HYPARB_DECISION_LOG as u64) as usize] =
            HyparbDecision {
                seq: self.decision_seq,
                ts_ns: now,
                edge_usd_1e6: q.pnl_usd_1e6,
                notional_usd_1e6: q.notional_usd_1e6,
                gas_px_usd_1e6,
                pool_sym: sym,
                buy: u8::from(buy),
                _pad: [0; 3],
            };
    }
}

/// One side of the hedge a decision assumes: the fee-folded price, the raw
/// touch price the IoC will be limited to, the venue, and the book's depth
/// on that side in USD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct HedgeLeg {
    /// Fee-folded USD price ×1e6 (a sell's is lowered, a buy's raised).
    pub(crate) eff_px_1e6: i64,
    /// The touch price the IoC is limited to, USD ×1e6 (0 for USD).
    pub(crate) px_1e6: i64,
    /// Venue chosen (`None` for the USD side).
    pub(crate) venue: HedgeVenue,
    /// Displayed size × price on that side, USD ×1e6.
    pub(crate) depth_usd_1e6: i64,
}

// ---------------------------------------------------------------
// Fills, hedges, inventory
// ---------------------------------------------------------------

impl HyparbStrategy {
    /// An AMM-leg fill: book the inventory it created and hedge it.
    fn on_amm_fill<C: Ctx>(&mut self, p: usize, fill: &Fill, now: NsTs, ctx: &mut C) {
        let pp = self.params.pools[p];
        self.pools[p].inflight_until = 0;
        self.counters.amm_fills = self.counters.amm_fills.wrapping_add(1);
        let qty0 = fill.qty.raw();
        let px = fill.px.raw();
        let qty1 = mul_div_i64(qty0, px, E6);
        let bought = fill.side == Side::Bid;
        if let Some(usd0) = self.usd_mid_1e6(pp.coin0) {
            let n = mul_div_i64(qty0, usd0, E6);
            self.counters.amm_notional_usd_1e6 =
                self.counters.amm_notional_usd_1e6.saturating_add(n);
        }
        // token0 in (bought) or out; token1 the other way.
        let d0 = if bought { qty0 } else { -qty0 };
        let d1 = if bought { -qty1 } else { qty1 };
        let legs = [(pp.coin0, d0, 0usize), (pp.coin1, d1, 1usize)];
        let mut k = 0usize;
        while k < 2 {
            let (coin, delta, side_idx) = legs[k];
            if coin != COIN_USD {
                let c = coin as usize;
                self.coins[c].inventory_1e6 = self.coins[c].inventory_1e6.saturating_add(delta);
                let venue = self.pools[p].hedge_venue[side_idx];
                let px = self.pools[p].hedge_px_1e6[side_idx];
                self.send_hedge(c, -delta, venue, px, now, false, ctx);
            }
            k += 1;
        }
        self.check_inventory();
    }

    /// Send one IoC that moves coin `c`'s inventory by `want_1e6` (a sale
    /// is negative) on `venue` at limit `px_1e6`, rounded DOWN to the lot;
    /// below the venue's minimum notional nothing is sent (the residue
    /// stays inventory for the timer).
    #[allow(clippy::too_many_arguments)]
    fn send_hedge<C: Ctx>(
        &mut self,
        c: usize,
        want_1e6: i64,
        venue: HedgeVenue,
        px_1e6: i64,
        now: NsTs,
        flatten: bool,
        ctx: &mut C,
    ) {
        let k = self.params.coins[c];
        let sym = match venue {
            HedgeVenue::Perp => k.perp_sym,
            HedgeVenue::Spot => k.spot_sym,
            HedgeVenue::None => return,
        };
        if sym == SYMBOL_ID_NONE || px_1e6 <= 0 || want_1e6 == 0 {
            return;
        }
        let abs = want_1e6.unsigned_abs() as i64;
        let qty = abs - abs % k.lot_1e6;
        if qty <= 0 || mul_div_i64(qty, px_1e6, E6) < k.min_notional_usd_1e6 {
            return;
        }
        let side = if want_1e6 > 0 { Side::Bid } else { Side::Ask };
        let mut o = Order::new(
            now,
            VenueId::Hyperliquid,
            sym,
            side,
            core_fill::ORDER_KIND_IOC,
            Price::from_raw(px_1e6),
            Qty::from_raw(qty),
            self.next_oid(),
        );
        o.ttl_ns = self.params.lag_ns;
        if !self.submit(o, ctx) {
            return;
        }
        let signed = if want_1e6 > 0 { qty } else { -qty };
        let run = &mut self.coins[c];
        run.pending_1e6 = run.pending_1e6.saturating_add(signed);
        run.pending_until = now
            .saturating_add(self.params.lag_ns)
            .saturating_add(HEDGE_GRACE_NS);
        let cn = &mut self.counters;
        if flatten {
            cn.flattens_submitted = cn.flattens_submitted.wrapping_add(1);
        } else {
            cn.hedges_submitted = cn.hedges_submitted.wrapping_add(1);
        }
        match venue {
            HedgeVenue::Perp => cn.hedges_perp = cn.hedges_perp.wrapping_add(1),
            HedgeVenue::Spot => cn.hedges_spot = cn.hedges_spot.wrapping_add(1),
            HedgeVenue::None => {}
        }
    }

    /// A hedge-book fill: the inventory it closes (and, on the perp, the
    /// position that earns or pays funding).
    fn on_hedge_fill(&mut self, c: usize, spot: bool, fill: &Fill) {
        let q = fill.qty.raw();
        let signed = if fill.side == Side::Bid { q } else { -q };
        let run = &mut self.coins[c];
        run.inventory_1e6 = run.inventory_1e6.saturating_add(signed);
        if !spot {
            run.perp_pos_1e6 = run.perp_pos_1e6.saturating_add(signed);
        }
        // The pending hedge shrinks toward zero by what filled.
        if (run.pending_1e6 > 0 && signed > 0) || (run.pending_1e6 < 0 && signed < 0) {
            let left = run.pending_1e6 - signed;
            run.pending_1e6 = if (left > 0) == (run.pending_1e6 > 0) {
                left
            } else {
                0
            };
        }
        self.counters.hedge_fills = self.counters.hedge_fills.wrapping_add(1);
        self.check_inventory();
    }

    /// Unhedged notional across coins vs the cap; a breach halts new arbs
    /// until it is back under half the cap.
    fn check_inventory(&mut self) {
        let mut total = 0i64;
        let mut c = 0usize;
        while c < self.params.n_coins {
            let inv = self.coins[c].inventory_1e6;
            if inv != 0 {
                let px = self.usd_mid_1e6(c as u8).unwrap_or(0);
                total = total.saturating_add(mul_div_i64(inv.unsigned_abs() as i64, px, E6));
            }
            c += 1;
        }
        let cap = self.params.inventory_cap_usd_1e6;
        if !self.halted && total > cap {
            self.halted = true;
            self.counters.inventory_breaches = self.counters.inventory_breaches.wrapping_add(1);
        } else if self.halted && total <= cap / 2 {
            self.halted = false;
        }
        self.counters.halted = u64::from(self.halted);
    }

    /// The 1 s pass: day roll, funding, in-flight and hedge deadlines,
    /// flattening.
    fn timer_pass<C: Ctx>(&mut self, now: NsTs, ctx: &mut C) {
        self.roll_day(now);
        self.accrue_funding(now);
        let mut p = 0usize;
        while p < self.params.n_pools {
            if self.pools[p].inflight_until != 0 && self.pools[p].inflight_until <= now {
                // The judge cancelled it (or it never landed): no fill.
                self.pools[p].inflight_until = 0;
            }
            p += 1;
        }
        let mut c = 0usize;
        while c < self.params.n_coins {
            let run = self.coins[c];
            if run.pending_1e6 != 0 && run.pending_until <= now {
                self.coins[c].pending_1e6 = 0;
                self.counters.hedges_missed = self.counters.hedges_missed.wrapping_add(1);
            }
            let run = self.coins[c];
            if run.pending_1e6 == 0 && run.inventory_1e6 != 0 {
                // Flatten at the touch of whichever book the selector
                // prefers for this direction right now.
                let sell = run.inventory_1e6 > 0;
                let n = match self.usd_mid_1e6(c as u8) {
                    Some(m) => mul_div_i64(run.inventory_1e6.unsigned_abs() as i64, m, E6),
                    None => 0,
                };
                if let Some(leg) = self.hedge_side(c as u8, sell, n, now) {
                    self.send_hedge(c, -run.inventory_1e6, leg.venue, leg.px_1e6, now, true, ctx);
                }
            }
            c += 1;
        }
        self.check_inventory();
    }
}

// ---------------------------------------------------------------
// The Strategy contract
// ---------------------------------------------------------------

impl StrategyCounters for HyparbStrategy {
    fn orders_emitted(&self) -> u64 {
        self.orders_emitted
    }

    fn orders_dropped(&self) -> u64 {
        self.counters.orders_dropped
    }

    fn strategy_kind(&self) -> &'static str {
        "hyparb"
    }

    fn hyparb_counters(&self) -> HyparbCounters {
        self.counters
    }

    fn hyparb_pools_view(&self, out: &mut [HyparbPoolView]) -> u32 {
        let n = self.params.n_pools.min(out.len());
        let mut p = 0usize;
        while p < n {
            let run = &self.pools[p];
            let b = run.book as usize;
            out[p] = HyparbPoolView::new(
                self.params.pools[p].sym,
                u8::from(self.book.is_live(b)),
                u8::from(run.map_ok),
                run.hedge_venue[0] as u8,
                self.book.judged_fee(b).unwrap_or(0),
                self.book.mid_1e6(b).unwrap_or(0),
                run.basis_bps_1e6,
                run.arbs,
                run.pnl_predicted_usd_1e6,
            );
            p += 1;
        }
        if self.configured {
            self.params.n_pools as u32
        } else {
            0
        }
    }

    fn hyparb_decisions(&self, after: u64, out: &mut [HyparbDecision]) -> u32 {
        let oldest = self
            .decision_seq
            .saturating_sub(HYPARB_DECISION_LOG as u64 - 1)
            .max(1);
        let first = after.saturating_add(1);
        let mut seq = if first > oldest { first } else { oldest };
        let mut n = 0usize;
        while seq <= self.decision_seq && n < out.len() {
            out[n] = self.decisions[(seq % HYPARB_DECISION_LOG as u64) as usize];
            n += 1;
            seq += 1;
        }
        n as u32
    }

    fn hyparb_coins_view(&self, out: &mut [HyparbCoinView]) -> u32 {
        let n = self.params.n_coins.min(out.len());
        let mut c = 0usize;
        while c < n {
            let k = self.params.coins[c];
            let r = &self.coins[c];
            out[c] = HyparbCoinView {
                perp_sym: k.perp_sym,
                spot_sym: k.spot_sym,
                perp_depth_usd_1e6: r.perp.depth_usd_1e6(),
                spot_depth_usd_1e6: r.spot.depth_usd_1e6(),
                perp_cost_bps_1e6: r.last_cost_bps_1e6[0],
                spot_cost_bps_1e6: r.last_cost_bps_1e6[1],
                inventory_1e6: r.inventory_1e6,
                perp_pos_1e6: r.perp_pos_1e6,
                funding_1e9: r.funding_1e9,
            };
            c += 1;
        }
        if self.configured {
            self.params.n_coins as u32
        } else {
            0
        }
    }
}

impl Strategy for HyparbStrategy {
    fn on_start<C: Ctx>(&mut self, ctx: &mut C) -> Result<(), StrategyError> {
        if !self.configured {
            return Err(StrategyError::Config("hyparb: on_start before configure"));
        }
        self.roll_day(ctx.now_ns());
        Ok(())
    }

    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        if !self.configured {
            return;
        }
        let Some((c, spot)) = self.coin_of(tick.sym) else {
            return;
        };
        let t = CoinTouch::of(tick);
        let run = &mut self.coins[c];
        let changed = if spot {
            let ch = !run.spot.same_quote(&t);
            run.spot = t;
            ch
        } else {
            let ch = !run.perp.same_quote(&t);
            run.perp = t;
            ch
        };
        // VT4: a stale book is recorded, never traded against.
        if !changed || t.stale {
            return;
        }
        let now = ctx.now_ns();
        let mut p = 0usize;
        while p < self.params.n_pools {
            let pp = self.params.pools[p];
            if pp.coin0 as usize == c || pp.coin1 as usize == c {
                self.evaluate(p, now, ctx);
            }
            p += 1;
        }
    }

    fn on_signal<C: Ctx>(&mut self, signal: &Signal, ctx: &mut C) {
        if !self.configured || signal.source != SignalSource::HyperEvm as u8 {
            return;
        }
        if let Some(p) = self.apply_pool_signal(signal) {
            let now = ctx.now_ns();
            self.refresh_basis(p, now);
            self.evaluate(p, now, ctx);
        }
    }

    fn on_fill<C: Ctx>(&mut self, fill: &Fill, ctx: &mut C) {
        if !self.configured {
            return;
        }
        let now = ctx.now_ns();
        if let Some(p) = self.pool_of(fill.sym) {
            self.on_amm_fill(p, fill, now, ctx);
        } else if let Some((c, spot)) = self.coin_of(fill.sym) {
            self.on_hedge_fill(c, spot, fill);
        }
    }

    fn on_venue_event<C: Ctx>(&mut self, event: &ChannelEvent, _ctx: &mut C) {
        if !self.configured || event.channel != ChannelId::AssetCtx as u8 {
            return;
        }
        if let Some((c, false)) = self.coin_of(event.sym) {
            self.coins[c].funding_1e9 = event.v0;
        }
    }

    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C) {
        if !self.configured {
            return;
        }
        self.timer_pass(now_ns, ctx);
    }

    fn timer_period_ns(&self) -> u64 {
        if self.configured {
            TIMER_NS
        } else {
            u64::MAX
        }
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests;
