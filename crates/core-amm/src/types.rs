// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Plain-old-data types: pool state, pool metadata, the tick map, and
//! the results the walk and the arb solve hand back.

use crate::tick_math::{MAX_TICK, MIN_TICK};

/// `PoolMeta::kind` of a concentrated-liquidity (Uniswap V3 ABI) pool.
pub const AMM_KIND_V3: u8 = 0;
/// `PoolMeta::kind` of a constant-product (Uniswap V2 ABI) pool, modelled
/// as ONE full-range position (`L = sqrt(r0·r1)`, no initialised ticks).
pub const AMM_KIND_V2: u8 = 1;

/// `PoolState::flags`: the state is older than the member trusts.
/// Recorded, never traded against.
pub const POOL_FLAG_STALE: u8 = 1;
/// `PoolState::flags`: this state was produced by a walk that ended ON an
/// edge whose liquidity beyond is unknown (the matcher's in-range judge
/// has no tick map). Its `liquidity` is the pre-edge value and must not
/// be walked further before a real update overwrites it.
pub const POOL_FLAG_EDGE: u8 = 2;

/// One pool's live state — exactly the fields a V3 `Swap` event carries,
/// plus where it came from. One cache line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct PoolState {
    /// `sqrtPriceX96`, low 128 bits.
    pub sqrt_price_lo: u128,
    /// In-range liquidity `L`.
    pub liquidity: u128,
    /// `sqrtPriceX96`, high 32 bits of the `uint160`. Zero for every pool
    /// in the universe today; carried so a venue change cannot silently
    /// truncate.
    pub sqrt_price_hi: u32,
    /// Current tick (`tick_at_sqrt(sqrt_price)` except exactly at a
    /// crossed boundary, where it is the contract's value).
    pub tick: i32,
    /// Block of the event that produced this state.
    pub block: u64,
    /// Log index of that event within its block.
    pub log_index: u32,
    /// Index into the member's pool table.
    pub pool: u16,
    /// `POOL_FLAG_*`.
    pub flags: u8,
    _pad0: [u8; 1],
}
const _: () = assert!(core::mem::size_of::<PoolState>() == 64);
const _: () = assert!(core::mem::align_of::<PoolState>() == 64);

impl PoolState {
    /// All-zero state (no price, no liquidity): judged not live.
    pub const ZERO: Self = Self {
        sqrt_price_lo: 0,
        liquidity: 0,
        sqrt_price_hi: 0,
        tick: 0,
        block: 0,
        log_index: 0,
        pool: 0,
        flags: 0,
        _pad0: [0],
    };

    /// A state from its price, tick and liquidity; provenance zeroed.
    #[must_use]
    pub const fn new(sqrt_price_lo: u128, sqrt_price_hi: u32, tick: i32, liquidity: u128) -> Self {
        let mut s = Self::ZERO;
        s.sqrt_price_lo = sqrt_price_lo;
        s.sqrt_price_hi = sqrt_price_hi;
        s.tick = tick;
        s.liquidity = liquidity;
        s
    }

    /// `true` when priced, liquid, and neither stale nor edge-limited.
    #[inline]
    #[must_use]
    pub const fn is_live(&self) -> bool {
        (self.sqrt_price_lo != 0 || self.sqrt_price_hi != 0)
            && self.liquidity != 0
            && self.flags & (POOL_FLAG_STALE | POOL_FLAG_EDGE) == 0
    }
}

impl Default for PoolState {
    fn default() -> Self {
        Self::ZERO
    }
}

/// Static pool facts, resolved once at boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct PoolMeta {
    /// token0 address.
    pub token0: [u8; 20],
    /// token1 address.
    pub token1: [u8; 20],
    /// Pool contract address.
    pub address: [u8; 20],
    /// Swap fee in pips (1e-6): 500 = 0.05 %.
    pub fee_pips: u32,
    /// Tick spacing (> 0).
    pub tick_spacing: i32,
    /// token0 decimals.
    pub dec0: u8,
    /// token1 decimals.
    pub dec1: u8,
    /// HL symbol table index of token0's hedge, `u16::MAX` = USD stable.
    pub sym0: u16,
    /// HL symbol table index of token1's hedge, `u16::MAX` = USD stable.
    pub sym1: u16,
    /// `AMM_KIND_V3` | `AMM_KIND_V2`.
    pub kind: u8,
    _pad: [u8; 3],
}

impl PoolMeta {
    /// Zeroed metadata; the boot loader fills the public fields.
    pub const ZERO: Self = Self {
        token0: [0; 20],
        token1: [0; 20],
        address: [0; 20],
        fee_pips: 0,
        tick_spacing: 1,
        dec0: 0,
        dec1: 0,
        sym0: u16::MAX,
        sym1: u16::MAX,
        kind: AMM_KIND_V3,
        _pad: [0; 3],
    };
}

impl Default for PoolMeta {
    fn default() -> Self {
        Self::ZERO
    }
}

/// One INITIALISED tick and its signed `liquidityNet`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct TickNode {
    /// The tick (a multiple of the pool's spacing).
    pub tick: i32,
    _pad: [u8; 12],
    /// Liquidity added when the price crosses this tick UPWARD.
    pub liquidity_net: i128,
}
const _: () = assert!(core::mem::size_of::<TickNode>() == 32);

impl TickNode {
    /// The empty node.
    pub const ZERO: Self = Self { tick: 0, _pad: [0; 12], liquidity_net: 0 };

    /// A node from its tick and `liquidityNet`.
    #[must_use]
    pub const fn new(tick: i32, liquidity_net: i128) -> Self {
        Self { tick, _pad: [0; 12], liquidity_net }
    }
}

/// Why a tick map or a state was refused. Boot/decode time only.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AmmError {
    /// More initialised ticks than the map's capacity `N`.
    TickMapFull,
    /// Ticks not strictly ascending.
    TickMapUnsorted,
    /// The same tick twice.
    TickMapDuplicate,
    /// `|liquidityNet|` beyond the contract's per-tick maximum for this
    /// spacing — the signature of a sign-extension decoding error.
    LiquidityNetOverflow,
    /// Spacing, alignment, coverage or range out of domain.
    BadState,
}

impl core::fmt::Display for AmmError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::TickMapFull => "tick map full",
            Self::TickMapUnsorted => "tick map not ascending",
            Self::TickMapDuplicate => "duplicate tick in map",
            Self::LiquidityNetOverflow => "liquidityNet beyond the per-tick maximum (sign-extension?)",
            Self::BadState => "bad pool state or map coverage",
        };
        f.write_str(s)
    }
}

impl std::error::Error for AmmError {}

/// `Uniswap V3 Tick.tickSpacingToMaxLiquidityPerTick`.
#[must_use]
pub const fn max_liquidity_per_tick(tick_spacing: i32) -> u128 {
    if tick_spacing <= 0 {
        return 0;
    }
    let min_t = (MIN_TICK / tick_spacing) * tick_spacing;
    let max_t = (MAX_TICK / tick_spacing) * tick_spacing;
    let num_ticks = ((max_t - min_t) / tick_spacing) as u128 + 1;
    u128::MAX / num_ticks
}

/// The initialised ticks of one pool over a KNOWN coverage
/// `[lo_tick, hi_tick]` (inclusive), ascending.
///
/// **Law — never extrapolate.** A walk clamps its target to the
/// coverage and stops there: liquidity beyond the fetched map is
/// unknown, and guessing it manufactures size that is not on chain.
#[derive(Clone, Debug)]
#[repr(C, align(64))]
pub struct TickMap<const N: usize> {
    nodes: [TickNode; N],
    len: u16,
    /// Lowest tick the map covers (a walk down stops here).
    pub lo_tick: i32,
    /// Highest tick the map covers (a walk up stops here).
    pub hi_tick: i32,
}

impl<const N: usize> TickMap<N> {
    const CAP_OK: () = assert!(N <= u16::MAX as usize, "TickMap capacity must fit u16");

    /// An empty map with EMPTY coverage (`lo > hi`): every walk against
    /// it is refused until `load` sets a coverage.
    pub const EMPTY: Self = {
        let () = Self::CAP_OK;
        Self { nodes: [TickNode::ZERO; N], len: 0, lo_tick: 1, hi_tick: 0 }
    };

    /// Number of initialised ticks held.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// `true` when no initialised tick is held.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `true` when a coverage has been set.
    #[inline]
    #[must_use]
    pub const fn has_coverage(&self) -> bool {
        self.lo_tick <= self.hi_tick
    }

    /// The held nodes, ascending.
    #[inline]
    #[must_use]
    pub fn nodes(&self) -> &[TickNode] {
        let n = if (self.len as usize) <= N { self.len as usize } else { N };
        &self.nodes[..n]
    }

    /// Drop every node and the coverage.
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
        self.lo_tick = 1;
        self.hi_tick = 0;
    }

    /// Replace the map with `nodes` over the coverage `[lo_tick, hi_tick]`.
    ///
    /// Refuses (and leaves the map CLEARED) on: more than `N` nodes, a
    /// non-ascending or duplicate tick, a tick outside the coverage or
    /// not a multiple of `tick_spacing`, a coverage not aligned to the
    /// spacing, and any `|liquidity_net|` beyond
    /// [`max_liquidity_per_tick`] — the fingerprint of an `int128`
    /// decoded with the wrong sign extension.
    pub fn load(&mut self, nodes: &[TickNode], lo_tick: i32, hi_tick: i32, tick_spacing: i32) -> Result<(), AmmError> {
        self.clear();
        if tick_spacing <= 0
            || lo_tick > hi_tick
            || lo_tick < MIN_TICK
            || hi_tick > MAX_TICK
            || lo_tick % tick_spacing != 0
            || hi_tick % tick_spacing != 0
        {
            return Err(AmmError::BadState);
        }
        if nodes.len() > N {
            return Err(AmmError::TickMapFull);
        }
        let cap = max_liquidity_per_tick(tick_spacing);
        let mut i = 0;
        while i < nodes.len() {
            let n = nodes[i];
            if i > 0 {
                let prev = nodes[i - 1].tick;
                if n.tick == prev {
                    return Err(AmmError::TickMapDuplicate);
                }
                if n.tick < prev {
                    return Err(AmmError::TickMapUnsorted);
                }
            }
            if n.tick < lo_tick || n.tick > hi_tick || n.tick % tick_spacing != 0 {
                return Err(AmmError::BadState);
            }
            if n.liquidity_net.unsigned_abs() > cap {
                return Err(AmmError::LiquidityNetOverflow);
            }
            self.nodes[i] = n;
            i += 1;
        }
        self.len = nodes.len() as u16;
        self.lo_tick = lo_tick;
        self.hi_tick = hi_tick;
        Ok(())
    }
}

/// Which way an arb trades the POOL.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ArbSide {
    /// No trade clears costs.
    None = 0,
    /// Buy token0 from the pool (price up), sell it on the hedge venue.
    BuyToken0 = 1,
    /// Sell token0 into the pool (price down), buy it on the hedge venue.
    SellToken0 = 2,
}

/// `ArbQuote::flags`: the size was cut by `max_notional` before the
/// pool reached the optimal price.
pub const ARB_FLAG_SIZE_CAPPED: u8 = 1;
/// `ArbQuote::flags`: the walk stopped at the tick map's coverage edge.
pub const ARB_FLAG_MAP_EDGE: u8 = 2;
/// `ArbQuote::flags`: the pool state was not live (stale, edge-limited,
/// unpriced or illiquid) — nothing was solved.
pub const ARB_FLAG_NOT_LIVE: u8 = 4;
/// `ArbQuote::flags`: positive before gas, not after.
pub const ARB_FLAG_BELOW_GAS: u8 = 8;
/// `ArbQuote::flags`: an arithmetic domain error stopped the solve.
pub const ARB_FLAG_MATH: u8 = 16;

/// The profit-maximising single-block arb against one pool.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct ArbQuote {
    /// Which way the pool trades; `None` ⇒ every amount is zero.
    pub side: ArbSide,
    /// `ARB_FLAG_*`.
    pub flags: u8,
    pub(crate) _pad: [u8; 2],
    /// Marginal edge at the pre-trade price, bps × 1e6, after the pool
    /// fee and the hedge fees folded into the bounds; before gas.
    pub gross_bps_1e6: i64,
    /// token0 amount, raw units (OUT of the pool when buying, IN — fee
    /// included — when selling).
    pub token0_raw: u128,
    /// token1 amount, raw units (IN — fee included — when buying, OUT
    /// when selling).
    pub token1_raw: u128,
    /// Net P&L in USD × 1e6 after the pool fee, the hedge fees and gas.
    /// Never positive with `side == None`.
    pub pnl_usd_1e6: i64,
    /// token0 notional in USD × 1e6.
    pub notional_usd_1e6: i64,
    /// Pool state AFTER our own swap. **The caller MUST carry this** —
    /// otherwise a standing gap is re-harvested every block.
    pub after: PoolState,
}

impl ArbQuote {
    /// The no-trade quote against `state`.
    #[must_use]
    pub const fn none(state: &PoolState, flags: u8, pnl_usd_1e6: i64) -> Self {
        Self {
            side: ArbSide::None,
            flags,
            _pad: [0; 2],
            gross_bps_1e6: 0,
            token0_raw: 0,
            token1_raw: 0,
            pnl_usd_1e6: if pnl_usd_1e6 > 0 { 0 } else { pnl_usd_1e6 },
            notional_usd_1e6: 0,
            after: *state,
        }
    }
}

/// `SwapResult::flags`: stopped at the requested price limit.
pub const SWAP_FLAG_LIMIT: u8 = 1;
/// `SwapResult::flags`: stopped at the map/range coverage edge before the
/// requested limit or amount — the walk refused to extrapolate.
pub const SWAP_FLAG_EDGE: u8 = 2;
/// `SwapResult::flags`: the step budget ran out.
pub const SWAP_FLAG_STEP_CAP: u8 = 4;
/// `SwapResult::flags`: an arithmetic domain error (the contract would
/// revert). Amounts are those accumulated before it.
pub const SWAP_FLAG_MATH: u8 = 8;
/// `SwapResult::flags`: refused before the first step (bad input, a
/// state outside the map's coverage, a limit on the wrong side).
pub const SWAP_FLAG_REFUSED: u8 = 16;
/// `SwapResult::flags`: a downward cross would have taken liquidity
/// negative (an inconsistent map); clamped at zero.
pub const SWAP_FLAG_LIQ_CLAMP: u8 = 32;
/// `SwapResult::flags`: an amount exceeded `u128` and was saturated.
pub const SWAP_FLAG_SATURATED: u8 = 64;

/// What one swap did, amounts in raw token units.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct SwapResult {
    /// Input paid INCLUDING the pool fee.
    pub amount_in: u128,
    /// Output received.
    pub amount_out: u128,
    /// The fee part of `amount_in`.
    pub fee: u128,
    /// Steps walked (ticks crossed + word boundaries + the final partial).
    pub steps: u16,
    /// `SWAP_FLAG_*`.
    pub flags: u8,
    _pad: [u8; 13],
    /// The pool after the swap.
    pub after: PoolState,
}

impl SwapResult {
    pub(crate) const fn refused(state: &PoolState) -> Self {
        Self {
            amount_in: 0,
            amount_out: 0,
            fee: 0,
            steps: 0,
            flags: SWAP_FLAG_REFUSED,
            _pad: [0; 13],
            after: *state,
        }
    }

    pub(crate) const fn new(amount_in: u128, amount_out: u128, fee: u128, steps: u16, flags: u8, after: PoolState) -> Self {
        Self { amount_in, amount_out, fee, steps, flags, _pad: [0; 13], after }
    }
}

/// What to swap: direction, amount semantics, the price limit and fee.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct SwapSpec {
    /// Exact amount: input (fee included) when `exact_in`, else output.
    pub amount: u128,
    /// Price limit `uint160` low 128 bits.
    pub limit_lo: u128,
    /// Price limit `uint160` high 32 bits.
    pub limit_hi: u32,
    /// Fee in pips.
    pub fee_pips: u32,
    /// `true`: token0 in, price down.
    pub zero_for_one: bool,
    /// `true`: `amount` is the input; `false`: the output.
    pub exact_in: bool,
}

/// What `solve_arb` needs from the hedge side, all integers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct ArbParams {
    /// Effective hedge BID for token0, in token1 per token0 × 1e18, with
    /// the hedge venue's taker fee already subtracted.
    pub eff_bid_1e18: u128,
    /// Effective hedge ASK for token0, in token1 per token0 × 1e18, with
    /// the hedge venue's taker fee already added.
    pub eff_ask_1e18: u128,
    /// USD price of one token0, × 1e6.
    pub px0_usd_1e6: i64,
    /// Notional cap in USD × 1e6, ALREADY clamped to the hedge venue's
    /// live top-of-book by the caller.
    pub max_notional_usd_1e6: i64,
    /// Gas charged per attempt, USD × 1e6.
    pub gas_usd_1e6: i64,
}
