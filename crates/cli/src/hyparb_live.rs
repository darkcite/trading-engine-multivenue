// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # HYPARB L3 — slot 0's live arm (plan §17; rulings O-HL1..O-HL5)
//!
//! [`HyparbLive`] is the `OrderDispatch` behind which slot 0 trades real
//! money. The router reaches it through `exec_router::SlotSplit`. The
//! member is the SAME one that trades paper: same decisions, same
//! orders. Only the fills differ. A paper fill is the matcher's
//! judgement; a live fill is the venue's word (the E-5 law, both legs).
//!
//! * **The AMM leg** (`ORDER_KIND_AMM_SWAP` on a pool symbol) goes to
//!   the `hyparb-live` thread. That thread signs ONE executor swap from
//!   the slot's wallet X on HyperEVM and sends it. The swap is exact
//!   input, with `minOut` taken from the member's limit
//!   ([`swap_request`]). The thread turns the mined RECEIPT's ERC-20
//!   `Transfer`s into the fill ([`fill_of`]). A revert is no fill. One
//!   swap is in flight at a time; a second is refused, not queued. A
//!   swap the executor could not fund is refused before it leaves (the
//!   worker's own account of the executor's balances, `avail_1e6`).
//! * **The hedge leg** (an IoC on a Hyperliquid perp) goes to X's own
//!   `HlExchange`. X signs as itself, and its `userFills` are the fills.
//!
//! ## What the router reads (slot 0 only, via `halt_signal_for`)
//!
//! * `reconciled` after the first full reconciliation. Until then every
//!   live place of slot 0 is refused.
//! * `recon_drift_usd_1e6`: the executor's tokens and X's perp positions
//!   against what this session's fills say they should be, at mids. It
//!   reports the smaller of the last two readings, so one torn read
//!   never halts (the E7-F3 rule).
//! * `recon_age_ns`, and from X's Hyperliquid arm: `ws_gap_ns`, the
//!   budget floor, `asset_refusal_streak`, and `reject_streak` (plus the
//!   swaps refused, unsent or unconfirmed in a row). A REVERT is a miss,
//!   not a refusal — the pool moved past `minOut`, the E7-F2 reading of
//!   an IoC miss — so it is counted (`swap_misses`) and moves no streak.
//! * Retirements (`try_next_retired`): a swap that ended without a fill,
//!   and every hedge IoC once its fills have had `HEDGE_RETIRE_NS` to
//!   land, leave the router's resting count.
//! * **The P&L bound (O-HL5).** It is judged only while the arm is flat
//!   ([`HyparbLive::pnl_flat`]): nothing is in flight and the last
//!   reconciliation came after the last activity. `pnl_delta_usd_1e6` is
//!   combined equity minus the anchor. Combined equity is the sum of:
//!   - X's perp account value;
//!   - X's spot USDC;
//!   - the executor's tokens at the hedge books' mids;
//!   - X's HYPE at the HYPE mid (gas spent is money lost).
//!
//!   The anchor is the first flat reading, persisted under the slot's
//!   state directory, so +$50 / −$20 bound the real money across
//!   restarts. The router latches a trip into `exec.HALT` (sticky).
//!
//! ## Threads and doctrine
//!
//! The ENGINE thread runs [`HyparbLive`]'s methods. They are
//! zero-alloc: fixed tables, a SPSC ring to the worker, a seqlock
//! snapshot back. Hyperliquid calls block the way slot 3's do. The
//! `hyparb-live` thread owns the HyperEVM arm and the reconciler's
//! reads. It is blocking by design and never touches the engine's
//! state. Boot ([`boot_hyparb_live`]) allocates and copies freely; it
//! runs once.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use clob_dispatcher::{CancelAllState, DispatchError, DispatchStats, HaltSignal, OrderDispatch};
use core_ring::{Consumer, Producer, Ring};
use core_types::{
    CancelReq, ChannelEvent, Fill, ModifyReq, NsTs, Order, Price, Qty, Side, SymbolId, Tick,
    VenueId, FILL_ORIGIN_VENUE,
};
use exec_hyperevm::arm::{EvmArm, PollOutcome, SendOutcome, MAX_BODY, MAX_RESP};
use exec_hyperevm::calldata::{
    encode_addr, ADDR_CALLDATA_LEN, BALANCE_OF_SELECTOR, DECIMALS_SELECTOR, TOKEN0_SELECTOR,
    TOKEN1_SELECTOR,
};
use exec_hyperevm::nonce::WalletState;
use exec_hyperevm::rpc::TokenMove;
use exec_hyperevm::{MainnetAuthority, Network};
use exec_hyperliquid::{HlConfig, HlExchange, PerpPosition, SpotBalance};
use rustls::ClientConfig;
use tracing::{info, warn};

use crate::evm_live::LiveNet;
use crate::evm_shadow::{open_limit_swap, publish_arm, Hex, ShadowStatus};
use crate::evm_testnet::operator_bid;

/// The slot this arm trades (`strategy_set::SLOT_HYPARB`).
pub const SLOT: u8 = 0;
const _: () = assert!(strategy_set::SLOT_HYPARB == SLOT);

/// Pools the live arm may trade.
pub const MAX_LIVE_POOLS: usize = 8;
/// Distinct tokens the executor holds across those pools.
pub const MAX_LIVE_TOKENS: usize = 16;
/// Hedge coins (the member's own bound).
pub const MAX_LIVE_COINS: usize = strategy_hyparb::HYPARB_MAX_COINS;
/// A token or coin priced in USD (the numéraire).
pub const PRICE_USD: u8 = strategy_hyparb::COIN_USD;

/// Swaps the engine hands the worker. One is ever in flight.
const SWAP_RING: usize = 4;
/// AMM fills the worker hands back.
const AMM_FILL_RING: usize = 16;
/// Swaps that ended without a fill, handed back for retirement.
const RETIRE_RING: usize = 16;
/// Hedge IoCs awaiting retirement (≥ any slot's `max_open_orders`).
const HEDGE_DUE_N: usize = 16;
/// An IoC's fills have landed on `userFills` well inside this.
const HEDGE_RETIRE_NS: u64 = 5_000_000_000;
/// Receipt poll cadence while a swap is in flight (blocks are ~1 s).
const POLL_NS: u64 = 500_000_000;
/// Base-fee cache lifetime.
const BASE_FEE_TTL_NS: u64 = 10_000_000_000;
/// Quarantined / unfunded wallet re-sync cadence.
const RESYNC_NS: u64 = 15_000_000_000;
/// Reconciliation cadence (Hyperliquid perp + spot sheets, the
/// executor's balances, X's HYPE).
const RECON_NS: u64 = 15_000_000_000;
/// A reconciliation this long after the last activity reads a settled
/// account (userFills and receipts have landed).
const QUIET_NS: u64 = 2_000_000_000;
/// The engine re-derives drift and equity this often at most.
const EVAL_NS: u64 = 250_000_000;
/// Positions one perp sheet may hold.
const MAX_POSITIONS: usize = 64;
/// The Hyperliquid `/info` answers the reconciler reads.
const INFO_RESP: usize = 64 * 1024;
/// The boot's `meta` answer (the whole perp universe).
const META_RESP: usize = 1024 * 1024;
/// The equity anchor's file, in the slot's state directory.
pub const ANCHOR_FILE: &str = "hyparb-equity-anchor.state";

// ---------------------------------------------------------------
// The book the arm trades, fixed at boot
// ---------------------------------------------------------------

/// One traded pool.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LivePool {
    /// The pool's symbol (what the member's order names).
    pub sym: SymbolId,
    /// The pool contract.
    pub address: [u8; 20],
    /// `tokens` index of token0.
    pub token0: u8,
    /// `tokens` index of token1.
    pub token1: u8,
}

/// One token the executor holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiveToken {
    /// The token contract.
    pub address: [u8; 20],
    /// `decimals()`, checked on chain at boot.
    pub decimals: u8,
    /// Its price: `coins` index (that coin's mid) or [`PRICE_USD`].
    pub price: u8,
}

/// One hedge coin: its book (the mid) and its Hyperliquid name.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiveCoin {
    /// The perp's symbol.
    pub perp_sym: SymbolId,
    /// The venue's coin name (`HYPE`).
    pub name: [u8; 16],
    /// Bytes of `name` in use.
    pub name_len: u8,
}

impl LiveCoin {
    /// The coin's name.
    #[inline]
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

/// Everything the arm trades and marks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveBook {
    /// Traded pools.
    pub pools: Vec<LivePool>,
    /// Their tokens.
    pub tokens: Vec<LiveToken>,
    /// Hedge coins.
    pub coins: Vec<LiveCoin>,
    /// The H9d executor, owned by X.
    pub executor: [u8; 20],
    /// The coin whose mid prices X's HYPE (gas), or [`PRICE_USD`] when
    /// none is configured (then X's HYPE is not marked).
    pub gas_coin: u8,
}

// ---------------------------------------------------------------
// Pure conversions (the member's units ↔ the chain's)
// ---------------------------------------------------------------

/// One swap the engine hands the worker.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SwapReq {
    /// Exact input, raw units of the token going in.
    pub amount_in: u128,
    /// Least output, raw units of the token coming out.
    pub min_out: u128,
    /// The member's `client_oid` (the fill's `order_id`).
    pub client_oid: u64,
    /// `LiveBook::pools` index.
    pub pool: u8,
    /// token0 in.
    pub zero_for_one: bool,
}

/// `10^d`, `d ≤ 38`.
#[inline]
const fn pow10(d: u8) -> u128 {
    let mut v = 1u128;
    let mut i = 0u8;
    while i < d {
        v *= 10;
        i += 1;
    }
    v
}

/// Human × 1e6 → raw units of a `dec`-decimal token; floor, or ceiling
/// with `up`. `None` past `u128`.
#[inline]
fn to_raw(v_1e6: u128, dec: u8, up: bool) -> Option<u128> {
    if dec >= 6 {
        v_1e6.checked_mul(pow10(dec - 6))
    } else {
        let d = pow10(6 - dec);
        Some(if up { v_1e6.div_ceil(d) } else { v_1e6 / d })
    }
}

/// Raw units → human × 1e6 (floor), saturating into `i64`.
#[inline]
#[must_use]
pub fn to_human_1e6(raw: u128, dec: u8) -> i64 {
    let v = if dec >= 6 {
        raw / pow10(dec - 6)
    } else {
        raw.saturating_mul(pow10(6 - dec))
    };
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// **The swap an AMM order becomes.** Always exact input, because the
/// executor bounds the OUTPUT (`minOut`), never the input:
///
/// * `Ask` sells token0: in = `qty` of token0; `minOut` = `qty × px`
///   of token1.
/// * `Bid` buys token0: in = `qty × px` of token1 (rounded up);
///   `minOut` = `qty` of token0.
///
/// `px` is the member's WORST average price (core-fill's AMM law, token1
/// per token0, human × 1e6). The swap therefore fills only at that
/// price or better, and a better price lands as more output. The
/// receipt states the truth either way.
///
/// # Errors
/// `EncodeOverflow` for a non-positive size or price, or an amount past
/// what the chain's `int256` / this arithmetic carries.
pub fn swap_request(order: &Order, pool: u8, dec0: u8, dec1: u8) -> Result<SwapReq, DispatchError> {
    let (qty, px) = (order.qty.raw(), order.px.raw());
    if qty <= 0 || px <= 0 {
        return Err(DispatchError::EncodeOverflow);
    }
    let q = qty as u128;
    let notional_1e12 = q
        .checked_mul(px as u128)
        .ok_or(DispatchError::EncodeOverflow)?;
    let raw0 = to_raw(q, dec0, false).ok_or(DispatchError::EncodeOverflow)?;
    let (amount_in, min_out, zero_for_one) = if order.side == Side::Ask {
        let out1 =
            to_raw(notional_1e12 / 1_000_000, dec1, false).ok_or(DispatchError::EncodeOverflow)?;
        (raw0, out1, true)
    } else {
        let in1 = to_raw(notional_1e12.div_ceil(1_000_000), dec1, true)
            .ok_or(DispatchError::EncodeOverflow)?;
        (in1, raw0, false)
    };
    if amount_in == 0 || min_out == 0 || amount_in > i128::MAX as u128 {
        return Err(DispatchError::EncodeOverflow);
    }
    Ok(SwapReq {
        amount_in,
        min_out,
        client_oid: order.client_oid,
        pool,
        zero_for_one,
    })
}

/// Whether an exact input of `amount_in` raw units is covered by the
/// executor's `avail_1e6` (human × 1e6, floored) of a `dec`-decimal
/// token. Unknown balances fund nothing.
#[inline]
#[must_use]
pub fn fundable(known: bool, avail_1e6: i64, dec: u8, amount_in: u128) -> bool {
    known && avail_1e6 > 0 && to_raw(avail_1e6 as u128, dec, false).is_some_and(|a| amount_in <= a)
}

/// **The fill a mined swap states** (`moves` are its receipt's
/// `Transfer`s with a = the executor, b = the pool), in the member's
/// units: `qty` token0 × 1e6, `px` the average price token1 per token0
/// × 1e6, derived from the truncated `qty` so `qty × px` is the token1
/// that moved. `None` unless exactly one token0 and one token1 moved, in
/// opposite directions. Anything else is not a swap this arm sent, and
/// the reconciler will see its effect as drift.
#[must_use]
pub fn fill_of(
    sym: SymbolId,
    t0: &LiveToken,
    t1: &LiveToken,
    moves: &[TokenMove],
    client_oid: u64,
    now: NsTs,
) -> Option<Fill> {
    let (mut m0, mut m1): (Option<TokenMove>, Option<TokenMove>) = (None, None);
    let mut i = 0usize;
    while i < moves.len() {
        let m = moves[i];
        i += 1;
        let slot = if m.token == t0.address {
            &mut m0
        } else if m.token == t1.address {
            &mut m1
        } else {
            return None;
        };
        if slot.replace(m).is_some() {
            return None;
        }
    }
    let (m0, m1) = (m0?, m1?);
    if m0.a_to_b == m1.a_to_b {
        return None;
    }
    let qty = to_human_1e6(m0.amount, t0.decimals);
    let human1 = to_human_1e6(m1.amount, t1.decimals);
    if qty <= 0 || human1 <= 0 {
        return None;
    }
    let px = i64::try_from((human1 as i128 * 1_000_000) / qty as i128).ok()?;
    // token0 from the executor (a) to the pool (b): sold.
    let side = if m0.a_to_b { Side::Ask } else { Side::Bid };
    Some(
        Fill::new(
            now,
            sym,
            side,
            Price::from_raw(px),
            Qty::from_raw(qty),
            client_oid,
        )
        .with_attribution(SLOT, FILL_ORIGIN_VENUE),
    )
}

// ---------------------------------------------------------------
// Shared state: the worker writes, the engine reads
// ---------------------------------------------------------------

/// What the worker publishes. Each field has ONE writer. The
/// reconciliation's fields are behind a seqlock (`seq` odd while being
/// written).
pub struct LiveShared {
    /// `engine_hyparb_evm_*` (the same family the testnet shadow uses).
    pub status: ShadowStatus,
    /// A swap is queued or in flight. The engine sets it; the worker
    /// clears it when the swap has concluded and the wallet is Ready.
    busy: AtomicBool,
    /// Swaps in a row refused, unsent or unconfirmed (reverts excluded).
    swap_rejects: AtomicU32,
    /// Swaps that reverted — the pool moved past `minOut` (a miss).
    swap_misses: AtomicU64,
    /// Swaps refused before sending: the executor could not fund them.
    short_inventory: AtomicU64,
    /// `avail_1e6` holds the worker's account of the executor.
    avail_set: AtomicBool,
    /// The executor's balances as this session's swaps left them,
    /// human × 1e6 (floor): the first reconciliation plus every receipt.
    avail_1e6: [AtomicI64; MAX_LIVE_TOKENS],
    seq: AtomicU64,
    reads: AtomicU64,
    taken_ns: AtomicU64,
    perp_value_1e6: AtomicI64,
    spot_usdc_1e6: AtomicI64,
    wallet_gwei: AtomicU64,
    szi_1e6: [AtomicI64; MAX_LIVE_COINS],
    tok_actual_1e6: [AtomicI64; MAX_LIVE_TOKENS],
    tok_expected_1e6: [AtomicI64; MAX_LIVE_TOKENS],
}

impl LiveShared {
    fn new() -> Self {
        Self {
            status: ShadowStatus::new(),
            busy: AtomicBool::new(false),
            swap_rejects: AtomicU32::new(0),
            swap_misses: AtomicU64::new(0),
            short_inventory: AtomicU64::new(0),
            avail_set: AtomicBool::new(false),
            avail_1e6: std::array::from_fn(|_| AtomicI64::new(0)),
            seq: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            taken_ns: AtomicU64::new(0),
            perp_value_1e6: AtomicI64::new(0),
            spot_usdc_1e6: AtomicI64::new(0),
            wallet_gwei: AtomicU64::new(0),
            szi_1e6: std::array::from_fn(|_| AtomicI64::new(0)),
            tok_actual_1e6: std::array::from_fn(|_| AtomicI64::new(0)),
            tok_expected_1e6: std::array::from_fn(|_| AtomicI64::new(0)),
        }
    }

    /// Successful reconciliations so far.
    #[inline]
    #[must_use]
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Acquire)
    }

    /// Whether a swap is queued or in flight.
    #[inline]
    #[must_use]
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// Swaps that reverted (misses).
    #[inline]
    #[must_use]
    pub fn swap_misses(&self) -> u64 {
        self.swap_misses.load(Ordering::Relaxed)
    }

    /// Swaps refused because the executor could not fund them.
    #[inline]
    #[must_use]
    pub fn short_inventory(&self) -> u64 {
        self.short_inventory.load(Ordering::Relaxed)
    }
}

/// One reconciliation as the engine reads it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    seq: u64,
    reads: u64,
    taken_ns: u64,
    perp_value_1e6: i64,
    spot_usdc_1e6: i64,
    wallet_gwei: u64,
    szi_1e6: [i64; MAX_LIVE_COINS],
    tok_actual_1e6: [i64; MAX_LIVE_TOKENS],
    tok_expected_1e6: [i64; MAX_LIVE_TOKENS],
}

impl LiveShared {
    /// The last complete reconciliation, if one is readable now.
    fn read(&self, into: &mut Snapshot) -> bool {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 == 1 {
            return false;
        }
        into.seq = s1;
        into.reads = self.reads.load(Ordering::Relaxed);
        into.taken_ns = self.taken_ns.load(Ordering::Relaxed);
        into.perp_value_1e6 = self.perp_value_1e6.load(Ordering::Relaxed);
        into.spot_usdc_1e6 = self.spot_usdc_1e6.load(Ordering::Relaxed);
        into.wallet_gwei = self.wallet_gwei.load(Ordering::Relaxed);
        let mut i = 0usize;
        while i < MAX_LIVE_COINS {
            into.szi_1e6[i] = self.szi_1e6[i].load(Ordering::Relaxed);
            i += 1;
        }
        i = 0;
        while i < MAX_LIVE_TOKENS {
            into.tok_actual_1e6[i] = self.tok_actual_1e6[i].load(Ordering::Relaxed);
            into.tok_expected_1e6[i] = self.tok_expected_1e6[i].load(Ordering::Relaxed);
            i += 1;
        }
        std::sync::atomic::fence(Ordering::Acquire);
        self.seq.load(Ordering::Relaxed) == s1
    }
}

// ---------------------------------------------------------------
// The engine-thread arm
// ---------------------------------------------------------------

/// Slot 0's live `OrderDispatch` (module doc).
pub struct HyparbLive<const FILL_N: usize> {
    book: LiveBook,
    hl: HlExchange<FILL_N>,
    hl_fills: Consumer<Fill, FILL_N>,
    swaps: Producer<SwapReq, SWAP_RING>,
    amm_fills: Consumer<Fill, AMM_FILL_RING>,
    retired: Consumer<u64, RETIRE_RING>,
    /// Hedge IoCs to retire: `(client_oid, due_ns)`, `client_oid` 0 = free.
    hedge_due: [(u64, u64); HEDGE_DUE_N],
    shared: Arc<LiveShared>,
    mids_1e6: [i64; MAX_LIVE_COINS],
    szi_expected_1e6: [i64; MAX_LIVE_COINS],
    baseline: bool,
    snap: Snapshot,
    drift_prev_1e6: i64,
    drift_now_1e6: i64,
    last_activity_ns: u64,
    last_hedge_fill_ns: u64,
    equity_1e6: i64,
    equity_ok: bool,
    flat: bool,
    anchor_1e6: i64,
    anchor_path: PathBuf,
    anchor_address: [u8; 20],
    last_eval_ns: u64,
    /// Wall clock at boot, unix ns: X's `userFills` opens with a
    /// SNAPSHOT of its recent fills, and a fill older than this process
    /// is not one of its orders (plan §17.5).
    boot_unix_ns: u64,
    /// Snapshot fills dropped by that rule.
    stale_fills: u64,
}

/// Whether a venue fill stamped `fill_ts_ns` (unix ns) can belong to a
/// process that booted at `boot_unix_ns`: X's `userFills` replays its
/// recent history on every connect, and on a perp — a coin bound for
/// good, unlike a HIP-4 instance — that history resolves, so without
/// this a restart books the last session's hedges again (the member's
/// hedge book, the router's ledger and the drift all double).
#[inline]
#[must_use]
pub const fn from_this_session(fill_ts_ns: u64, boot_unix_ns: u64) -> bool {
    fill_ts_ns >= boot_unix_ns
}

/// Wall clock now, unix ns (0 if the clock is before 1970).
fn unix_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

impl<const FILL_N: usize> HyparbLive<FILL_N> {
    /// The pool index `sym` trades as, if any.
    #[inline]
    fn pool_of(&self, sym: SymbolId) -> Option<usize> {
        let mut i = 0usize;
        while i < self.book.pools.len() {
            if self.book.pools[i].sym == sym {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// The coin whose perp `sym` is, if any.
    #[inline]
    fn coin_of(&self, sym: SymbolId) -> Option<usize> {
        let mut i = 0usize;
        while i < self.book.coins.len() {
            if self.book.coins[i].perp_sym == sym {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn submit_swap(&mut self, order: &Order) -> Result<(), DispatchError> {
        let Some(p) = self.pool_of(order.sym) else {
            return Err(DispatchError::NoLiveRoute);
        };
        if self.shared.busy() {
            return Err(DispatchError::QueueFull);
        }
        let pool = self.book.pools[p];
        let (d0, d1) = (
            self.book.tokens[pool.token0 as usize].decimals,
            self.book.tokens[pool.token1 as usize].decimals,
        );
        let req = swap_request(order, p as u8, d0, d1)?;
        // The executor funds exact input or reverts: refuse here what
        // it cannot fund (conservative: the published balance is floored).
        let (t_in, d_in) = if req.zero_for_one {
            (pool.token0 as usize, d0)
        } else {
            (pool.token1 as usize, d1)
        };
        let known = self.shared.avail_set.load(Ordering::Acquire);
        let avail = self.shared.avail_1e6[t_in].load(Ordering::Acquire);
        if !fundable(known, avail, d_in, req.amount_in) {
            self.shared.short_inventory.fetch_add(1, Ordering::Relaxed);
            return Err(DispatchError::RiskRefused);
        }
        self.shared.busy.store(true, Ordering::Release);
        if self.swaps.try_push(req).is_err() {
            self.shared.busy.store(false, Ordering::Release);
            return Err(DispatchError::QueueFull);
        }
        self.last_activity_ns = core_time::now_ns();
        Ok(())
    }

    /// The price of token `t`, USD × 1e6 per whole token (0 = unknown).
    #[inline]
    fn token_px_1e6(&self, t: usize) -> i64 {
        let price = self.book.tokens[t].price;
        if price == PRICE_USD {
            1_000_000
        } else {
            self.mids_1e6[price as usize]
        }
    }

    /// Re-derive drift, equity and flatness from the newest
    /// reconciliation and the mids; set the anchor at the first flat
    /// reading.
    fn evaluate(&mut self, now: u64) {
        let mut s = Snapshot::default();
        let fresh = self.shared.read(&mut s) && s.reads > 0;
        if fresh && s.seq != self.snap.seq {
            if !self.baseline {
                // The session's first reading is the baseline: nothing
                // traded before it (slot 0 is unseeded until it lands).
                self.szi_expected_1e6 = s.szi_1e6;
                self.baseline = true;
            }
            // COPY: one reconciliation (≈ 380 B) into the arm, once per
            // RECON_NS — rejected: reading the atomics on every judgement.
            self.snap = s;
            // A read taken while a hedge fill may still be landing is
            // not compared (the next one is).
            if s.taken_ns > self.last_hedge_fill_ns.saturating_add(QUIET_NS) {
                if let Some(d) = self.drift_1e6() {
                    self.drift_prev_1e6 = self.drift_now_1e6;
                    self.drift_now_1e6 = d;
                }
            }
        }
        if self.snap.reads == 0 {
            return;
        }
        match self.equity() {
            Some(e) => {
                self.equity_1e6 = e;
                self.equity_ok = true;
            }
            None => self.equity_ok = false,
        }
        self.flat = !self.shared.busy()
            && self.snap.taken_ns > self.last_activity_ns.saturating_add(QUIET_NS)
            && now.saturating_sub(self.snap.taken_ns) < 2 * RECON_NS;
        if self.anchor_1e6 == 0 && self.flat && self.equity_ok && self.equity_1e6 > 0 {
            self.anchor_1e6 = self.equity_1e6;
            let a = exec_hyperliquid::anchor::PnlAnchor {
                address: self.anchor_address,
                usdc_1e6: self.equity_1e6,
                set_unix_s: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
            };
            match exec_hyperliquid::anchor::store(&self.anchor_path, a) {
                Ok(()) => info!(
                    equity_usd_1e6 = self.equity_1e6,
                    path = %self.anchor_path.display(),
                    "hyparb live: the session P&L bound is ANCHORED on combined equity"
                ),
                Err(e) => warn!(
                    error = %e,
                    "hyparb live: anchor NOT persisted — the bound holds this session only"
                ),
            }
        }
    }

    /// Σ |actual − expected| at prices, USD × 1e6; `None` while a price
    /// it needs is unknown.
    fn drift_1e6(&self) -> Option<i64> {
        let mut d = 0i64;
        let mut t = 0usize;
        while t < self.book.tokens.len() {
            let gap = (self.snap.tok_actual_1e6[t] - self.snap.tok_expected_1e6[t]).abs();
            if gap != 0 {
                let px = self.token_px_1e6(t);
                if px <= 0 {
                    return None;
                }
                d = d.saturating_add(mul_1e6(gap, px));
            }
            t += 1;
        }
        let mut c = 0usize;
        while c < self.book.coins.len() {
            let gap = (self.snap.szi_1e6[c] - self.szi_expected_1e6[c]).abs();
            if gap != 0 {
                let px = self.mids_1e6[c];
                if px <= 0 {
                    return None;
                }
                d = d.saturating_add(mul_1e6(gap, px));
            }
            c += 1;
        }
        Some(d)
    }

    /// Combined equity, USD × 1e6; `None` while a price it needs is
    /// unknown.
    fn equity(&self) -> Option<i64> {
        let mut e = self
            .snap
            .perp_value_1e6
            .saturating_add(self.snap.spot_usdc_1e6);
        let mut t = 0usize;
        while t < self.book.tokens.len() {
            let held = self.snap.tok_actual_1e6[t];
            if held != 0 {
                let px = self.token_px_1e6(t);
                if px <= 0 {
                    return None;
                }
                e = e.saturating_add(mul_1e6(held, px));
            }
            t += 1;
        }
        if self.book.gas_coin != PRICE_USD && self.snap.wallet_gwei > 0 {
            let px = self.mids_1e6[self.book.gas_coin as usize];
            if px <= 0 {
                return None;
            }
            // gwei × (USD × 1e6 per HYPE) / 1e9.
            let v = (self.snap.wallet_gwei as i128 * px as i128) / 1_000_000_000;
            e = e.saturating_add(i64::try_from(v).unwrap_or(i64::MAX));
        }
        Some(e)
    }

    /// Retire hedge `oid` at `due`. A full table leaves the row counted
    /// (fail-closed: the slot tends toward refusing, never toward
    /// placing past its cap); the router's `max_open_orders` keeps the
    /// table from filling.
    fn retire_later(&mut self, oid: u64, due: u64) {
        let mut i = 0usize;
        while i < HEDGE_DUE_N {
            if self.hedge_due[i].0 == 0 {
                self.hedge_due[i] = (oid, due);
                return;
            }
            i += 1;
        }
        debug_assert!(false, "hedge retirement table full");
    }

    /// Whether the session bound may be judged now: flat (nothing in
    /// flight, a reconciliation after the last activity), every price
    /// known, and anchored.
    #[inline]
    #[must_use]
    pub const fn pnl_flat(&self) -> bool {
        self.flat && self.equity_ok && self.anchor_1e6 != 0
    }

    /// `userFills` snapshot rows older than this process, dropped.
    #[inline]
    #[must_use]
    pub const fn stale_fills(&self) -> u64 {
        self.stale_fills
    }

    /// Coin `c`'s perp size on X's account at the last reconciliation,
    /// coin × 1e6 (0 before the first).
    #[inline]
    #[must_use]
    pub fn venue_position_1e6(&self, c: usize) -> i64 {
        self.snap.szi_1e6.get(c).copied().unwrap_or(0)
    }

    /// The equity anchor (0 = not anchored yet), USD × 1e6.
    #[inline]
    #[must_use]
    pub const fn anchor_usd_1e6(&self) -> i64 {
        self.anchor_1e6
    }

    /// Combined equity at the last evaluation, USD × 1e6.
    #[inline]
    #[must_use]
    pub const fn equity_usd_1e6(&self) -> i64 {
        self.equity_1e6
    }

    /// The shared status (metrics, `/state`).
    #[inline]
    #[must_use]
    pub fn shared(&self) -> &Arc<LiveShared> {
        &self.shared
    }
}

/// `a × b / 1e6` in `i128`, saturated into `i64`.
#[inline]
fn mul_1e6(a: i64, b: i64) -> i64 {
    i64::try_from((a as i128 * b as i128) / 1_000_000).unwrap_or(i64::MAX)
}

impl<const FILL_N: usize> OrderDispatch for HyparbLive<FILL_N> {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        if order.venue == VenueId::HyperEvm.to_u8() && order.kind == core_fill::ORDER_KIND_AMM_SWAP
        {
            return self.submit_swap(order);
        }
        if order.venue == VenueId::Hyperliquid.to_u8() && self.coin_of(order.sym).is_some() {
            let now = core_time::now_ns();
            self.last_activity_ns = now;
            self.hl.submit(order)?;
            if order.kind == core_fill::ORDER_KIND_IOC {
                self.retire_later(order.client_oid, now.saturating_add(HEDGE_RETIRE_NS));
            }
            return Ok(());
        }
        Err(DispatchError::NoLiveRoute)
    }

    /// A swap cannot be recalled once handed to the chain; a hedge IoC
    /// never rests. Cancels reach X's Hyperliquid arm for completeness.
    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        if req.venue == VenueId::Hyperliquid.to_u8() {
            return self.hl.cancel(req);
        }
        Err(DispatchError::Unsupported)
    }

    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        if req.order().venue == VenueId::Hyperliquid.to_u8() {
            return self.hl.modify(req);
        }
        Err(DispatchError::Unsupported)
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        if let Some(f) = self.amm_fills.try_pop() {
            self.last_activity_ns = core_time::now_ns();
            return Some(f);
        }
        let f = loop {
            let f = self.hl_fills.try_pop()?;
            if from_this_session(f.ts_ns, self.boot_unix_ns) {
                break f;
            }
            self.stale_fills = self.stale_fills.wrapping_add(1);
        };
        if let Some(c) = self.coin_of(f.sym) {
            let q = f.qty.raw();
            self.szi_expected_1e6[c] += if f.side == Side::Bid { q } else { -q };
        }
        let now = core_time::now_ns();
        self.last_activity_ns = now;
        self.last_hedge_fill_ns = now;
        Some(f)
    }

    /// A swap that ended without a fill, else a hedge IoC whose fills
    /// have had their time to land (its remainder, if any, is gone).
    fn try_next_retired(&mut self) -> Option<(u64, u8)> {
        if let Some(oid) = self.retired.try_pop() {
            return Some((oid, SLOT));
        }
        let now = core_time::now_ns();
        let mut i = 0usize;
        while i < HEDGE_DUE_N {
            let (oid, due) = self.hedge_due[i];
            if oid != 0 && due <= now {
                self.hedge_due[i] = (0, 0);
                return Some((oid, SLOT));
            }
            i += 1;
        }
        None
    }

    fn stats(&self) -> DispatchStats {
        DispatchStats::default()
    }

    /// The hedge books' mids mark the account.
    #[inline]
    fn observe_tick(&mut self, tick: &Tick, _now_ns: NsTs) {
        if let Some(c) = self.coin_of(tick.sym) {
            let (b, a) = (tick.bid_px.raw(), tick.ask_px.raw());
            if b > 0 && a > 0 {
                self.mids_1e6[c] = (b + a) / 2;
            }
        }
    }

    fn on_idle(&mut self) -> bool {
        let worked = self.hl.on_idle();
        let now = core_time::now_ns();
        if now >= self.last_eval_ns.saturating_add(EVAL_NS) {
            self.last_eval_ns = now;
            self.evaluate(now);
        }
        worked
    }

    /// X's account never binds HIP-4 rolls: nothing here trades them.
    #[inline]
    fn on_venue_event(&mut self, _event: &ChannelEvent) {}

    fn halt_signal(&self) -> HaltSignal {
        let h = self.hl.halt_signal();
        let reads = self.snap.reads;
        let age = if reads > 0 {
            core_time::now_ns().saturating_sub(self.snap.taken_ns)
        } else {
            0
        };
        let rejects = h
            .reject_streak
            .saturating_add(self.shared.swap_rejects.load(Ordering::Relaxed));
        HaltSignal::new(
            h.ws_gap_ns,
            self.drift_now_1e6.min(self.drift_prev_1e6),
            rejects,
            h.asset_refusal_streak,
            h.budget_floor_breached != 0,
            reads > 0,
            age,
        )
        .with_pnl(
            self.pnl_flat(),
            self.equity_1e6.saturating_sub(self.anchor_1e6),
        )
    }

    fn cancel_all(&mut self) -> Result<(), DispatchError> {
        self.hl.cancel_all()
    }

    fn cancel_all_state(&self) -> CancelAllState {
        self.hl.cancel_all_state()
    }
}

// ---------------------------------------------------------------
// The `hyparb-live` thread
// ---------------------------------------------------------------

struct Worker {
    arm: EvmArm,
    info: core_net::HttpsPost,
    hl_user: [u8; 20],
    book: LiveBook,
    swaps: Consumer<SwapReq, SWAP_RING>,
    fills: Producer<Fill, AMM_FILL_RING>,
    retired: Producer<u64, RETIRE_RING>,
    shared: Arc<LiveShared>,
    base_fee: u128,
    base_fee_at: u64,
    next_poll: u64,
    next_resync: u64,
    next_recon: u64,
    inflight: Option<SwapReq>,
    expected_raw: [u128; MAX_LIVE_TOKENS],
    expected_set: bool,
    moves: [TokenMove; 4],
    positions: [PerpPosition; MAX_POSITIONS],
    balances: Box<[SpotBalance]>,
    rejects: u32,
    recon_fails: u64,
    /// A popped swap concluded while the wallet was not Ready (a
    /// timeout, a quarantine): `busy` clears when it is Ready again.
    release_when_ready: bool,
}

/// Why a reconciliation read failed (logged, retried next cycle).
#[derive(Debug)]
enum ReadErr {
    Evm(exec_hyperevm::arm::ArmErr),
    Info(&'static str),
}

impl core::fmt::Display for ReadErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Evm(e) => write!(f, "{e}"),
            Self::Info(s) => f.write_str(s),
        }
    }
}

impl Worker {
    /// One pass. `true` if anything was done.
    fn step(&mut self, now: u64) -> bool {
        let mut worked = false;
        if self.arm.nonces().state(0) == WalletState::InFlight && now >= self.next_poll {
            self.next_poll = now + POLL_NS;
            worked = true;
            self.poll(now);
        }
        let state = self.arm.nonces().state(0);
        if (state == WalletState::Quarantined || state == WalletState::Unfunded)
            && now >= self.next_resync
        {
            self.next_resync = now + RESYNC_NS;
            worked = true;
            if let Err(e) = self.arm.sync(0) {
                warn!(error = %e, "hyparb live: wallet re-sync failed");
            }
        }
        let ready = self.arm.nonces().state(0) == WalletState::Ready;
        if ready && self.release_when_ready {
            self.release_when_ready = false;
            self.shared.busy.store(false, Ordering::Release);
        }
        if ready && self.inflight.is_none() {
            if let Some(req) = self.swaps.try_pop() {
                worked = true;
                self.send(&req, now);
            }
        }
        if ready && self.inflight.is_none() && !self.shared.busy() && now >= self.next_recon {
            self.next_recon = now + RECON_NS;
            worked = true;
            match self.reconcile(now) {
                Ok(()) => self.recon_fails = 0,
                Err(e) => {
                    self.recon_fails += 1;
                    if self.recon_fails == 1 || self.recon_fails % 20 == 0 {
                        warn!(
                            error = %e,
                            fails = self.recon_fails,
                            "hyparb live: reconciliation failed — retried; the router halts \
                             slot 0 on staleness"
                        );
                    }
                }
            }
        }
        publish_arm(&self.shared.status, &self.arm);
        worked
    }

    fn send(&mut self, req: &SwapReq, now: u64) {
        if self.base_fee == 0 || now >= self.base_fee_at + BASE_FEE_TTL_NS {
            if let Ok(b) = self.arm.next_base_fee() {
                self.base_fee = b;
                self.base_fee_at = now;
            }
        }
        if self.base_fee == 0 {
            self.conclude_rejected(
                req.client_oid,
                "no base fee readable — the swap was not sent",
            );
            return;
        }
        let pool = self.book.pools[req.pool as usize];
        let call = open_limit_swap(pool.address, req.zero_for_one, req.amount_in, req.min_out);
        let bid = operator_bid(self.base_fee);
        match self
            .arm
            .send_swap(0, &self.book.executor, &call, bid, req.client_oid, now)
        {
            SendOutcome::Sent { nonce, hash, .. } => {
                info!(
                    oid = req.client_oid,
                    nonce,
                    pool = %Hex(&pool.address),
                    amount_in = req.amount_in,
                    min_out = req.min_out,
                    tx = %Hex(&hash),
                    "hyparb live: swap SENT"
                );
                self.inflight = Some(*req);
                self.next_poll = now + POLL_NS;
            }
            SendOutcome::MaybeSent { nonce, hash, .. } => {
                warn!(
                    oid = req.client_oid,
                    nonce,
                    tx = %Hex(&hash),
                    "hyparb live: swap left the host without an answer — tracking its hash"
                );
                self.inflight = Some(*req);
                self.next_poll = now + POLL_NS;
            }
            SendOutcome::Refused { why, .. } => {
                warn!(oid = req.client_oid, why = ?why, "hyparb live: the node refused the swap");
                self.conclude_rejected(req.client_oid, "refused");
            }
            SendOutcome::NotSent { err, .. } => {
                warn!(oid = req.client_oid, error = %err, "hyparb live: swap not sent");
                self.conclude_rejected(req.client_oid, "not sent");
            }
            SendOutcome::Halted => {
                warn!(cause = ?self.arm.halted(), "hyparb live: the EVM arm is HALTED");
                self.conclude_rejected(req.client_oid, "halted");
            }
        }
    }

    /// A swap ended without a fill because something is wrong (refused,
    /// unsent, unconfirmed): it counts toward the reject streak.
    fn conclude_rejected(&mut self, oid: u64, _why: &'static str) {
        self.rejects = self.rejects.saturating_add(1);
        self.shared
            .swap_rejects
            .store(self.rejects, Ordering::Relaxed);
        self.retire(oid);
        self.conclude();
    }

    /// A swap reverted: the pool moved past `minOut` — a MISS, counted,
    /// never a streak (the E7-F2 reading of an IoC that did not cross).
    fn conclude_missed(&mut self, oid: u64) {
        self.shared.swap_misses.fetch_add(1, Ordering::Relaxed);
        self.retire(oid);
        self.conclude();
    }

    /// Swap `oid` left no fill: its resting row goes.
    fn retire(&mut self, oid: u64) {
        if self.retired.try_push(oid).is_err() {
            warn!("hyparb live: the retirement ring is full — a resting row stays counted");
        }
    }

    /// The popped swap is over: the engine may hand the next one once
    /// the wallet is Ready (the only place `busy` clears).
    fn conclude(&mut self) {
        self.inflight = None;
        if self.arm.nonces().state(0) == WalletState::Ready {
            self.shared.busy.store(false, Ordering::Release);
        } else {
            self.release_when_ready = true;
        }
    }

    fn poll(&mut self, now: u64) {
        let Some(req) = self.inflight else {
            return;
        };
        match self.arm.poll(0, now) {
            PollOutcome::Mined { .. } => {
                let rc = *self.arm.last_receipt();
                let pool = self.book.pools[req.pool as usize];
                if rc.status != 1 {
                    warn!(
                        oid = req.client_oid,
                        block = rc.block,
                        tx = %Hex(&rc.tx_hash),
                        "hyparb live: swap REVERTED (below minOut, or the pool moved) — no fill"
                    );
                    self.conclude_missed(req.client_oid);
                    return;
                }
                let n = self.arm.last_receipt_transfers(
                    &self.book.executor,
                    &pool.address,
                    &mut self.moves,
                );
                let n = match n {
                    Ok(n) => n,
                    Err(e) => {
                        warn!(
                            error = ?e,
                            tx = %Hex(&rc.tx_hash),
                            "hyparb live: mined swap's logs unreadable — no fill; the \
                             reconciler will see its effect"
                        );
                        self.conclude_rejected(req.client_oid, "unreadable receipt");
                        return;
                    }
                };
                self.apply_moves(n);
                self.publish_avail();
                let t0 = self.book.tokens[pool.token0 as usize];
                let t1 = self.book.tokens[pool.token1 as usize];
                match fill_of(pool.sym, &t0, &t1, &self.moves[..n], req.client_oid, now) {
                    Some(f) => {
                        info!(
                            oid = req.client_oid,
                            block = rc.block,
                            side = ?f.side,
                            qty_1e6 = f.qty.raw(),
                            px_1e6 = f.px.raw(),
                            gas_used = rc.gas_used,
                            tx = %Hex(&rc.tx_hash),
                            "hyparb live: swap MINED — the receipt is the fill"
                        );
                        if self.fills.try_push(f).is_err() {
                            warn!("hyparb live: the fill ring is full — fill DROPPED");
                            self.retire(req.client_oid);
                        }
                        self.rejects = 0;
                        self.shared.swap_rejects.store(0, Ordering::Relaxed);
                    }
                    None => {
                        warn!(
                            tx = %Hex(&rc.tx_hash),
                            "hyparb live: mined swap moved tokens this arm cannot read as one fill"
                        );
                        self.retire(req.client_oid);
                    }
                }
                self.conclude();
            }
            PollOutcome::TimedOut { nonce, .. } => {
                warn!(
                    oid = req.client_oid,
                    nonce,
                    "hyparb live: no receipt within the timeout — wallet quarantined; the \
                     reconciler will see whatever happened"
                );
                self.conclude_rejected(req.client_oid, "timed out");
            }
            PollOutcome::Err(e) => warn!(error = %e, "hyparb live: receipt poll failed"),
            PollOutcome::Pending | PollOutcome::Idle => {}
        }
    }

    /// The executor's expected balances after a mined swap (its first
    /// `n` receipt moves).
    fn apply_moves(&mut self, n: usize) {
        let mut i = 0usize;
        while i < n {
            let m = self.moves[i];
            i += 1;
            let mut t = 0usize;
            while t < self.book.tokens.len() {
                if self.book.tokens[t].address == m.token {
                    // a = the executor: a→b left it.
                    self.expected_raw[t] = if m.a_to_b {
                        self.expected_raw[t].saturating_sub(m.amount)
                    } else {
                        self.expected_raw[t].saturating_add(m.amount)
                    };
                }
                t += 1;
            }
        }
    }

    /// The executor's balances as the engine may spend them (floored).
    fn publish_avail(&self) {
        let mut t = 0usize;
        while t < self.book.tokens.len() {
            let v = to_human_1e6(self.expected_raw[t], self.book.tokens[t].decimals);
            self.shared.avail_1e6[t].store(v, Ordering::Relaxed);
            t += 1;
        }
        self.shared.avail_set.store(true, Ordering::Release);
    }

    /// One `/info` POST; the answer's body range.
    fn info_post(&mut self, n: usize) -> Result<core::ops::Range<usize>, ReadErr> {
        let (status, r) = self
            .info
            .post(n)
            .map_err(|_| ReadErr::Info("hyperliquid /info unreachable"))?;
        if status != 200 {
            return Err(ReadErr::Info("hyperliquid /info answered non-200"));
        }
        Ok(r)
    }

    /// Read everything the bound and the drift are judged on, and
    /// publish it (seqlock).
    fn reconcile(&mut self, now: u64) -> Result<(), ReadErr> {
        let n = exec_hyperliquid::perp_state_request(self.info.body_mut(), &self.hl_user)
            .map_err(|_| ReadErr::Info("request did not fit"))?;
        let r = self.info_post(n)?;
        let (value_1e8, np) =
            exec_hyperliquid::scan_perp_state(&self.info.resp()[r.clone()], &mut self.positions)
                .map_err(|_| ReadErr::Info("unreadable clearinghouseState"))?;
        let mut szi = [0i64; MAX_LIVE_COINS];
        let mut c = 0usize;
        while c < self.book.coins.len() {
            let mut k = 0usize;
            while k < np {
                if self.positions[k].coin.of(&self.info.resp()[r.clone()])
                    == self.book.coins[c].name()
                {
                    szi[c] = self.positions[k].szi_1e8 / 100;
                }
                k += 1;
            }
            c += 1;
        }
        let n = exec_hyperliquid::spot_state_request(self.info.body_mut(), &self.hl_user)
            .map_err(|_| ReadErr::Info("request did not fit"))?;
        let r = self.info_post(n)?;
        let nb =
            exec_hyperliquid::scan_spot_state(&self.info.resp()[r.clone()], &mut self.balances)
                .map_err(|_| ReadErr::Info("unreadable spotClearinghouseState"))?;
        let usdc_1e6 = spot_usdc_1e6(&self.balances[..nb], &self.info.resp()[r]);
        let wallet_wei = self.arm.sync(0).map_err(ReadErr::Evm)?.balance_wei;
        let mut actual = [0u128; MAX_LIVE_TOKENS];
        let mut cd = [0u8; ADDR_CALLDATA_LEN];
        encode_addr(BALANCE_OF_SELECTOR, &self.book.executor, &mut cd);
        let mut t = 0usize;
        while t < self.book.tokens.len() {
            let w = self
                .arm
                .call_word(&self.book.tokens[t].address, &cd)
                .map_err(ReadErr::Evm)?;
            actual[t] = word_u128(&w).ok_or(ReadErr::Info("a balance past 128 bits"))?;
            t += 1;
        }
        if !self.expected_set {
            self.expected_raw = actual;
            self.expected_set = true;
            self.publish_avail();
        }
        let s = &self.shared;
        s.seq.fetch_add(1, Ordering::AcqRel);
        s.taken_ns.store(now, Ordering::Relaxed);
        s.perp_value_1e6.store(value_1e8 / 100, Ordering::Relaxed);
        s.spot_usdc_1e6.store(usdc_1e6, Ordering::Relaxed);
        s.wallet_gwei.store(
            u64::try_from(wallet_wei / 1_000_000_000).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let mut c = 0usize;
        while c < MAX_LIVE_COINS {
            s.szi_1e6[c].store(szi[c], Ordering::Relaxed);
            c += 1;
        }
        t = 0;
        while t < self.book.tokens.len() {
            let d = self.book.tokens[t].decimals;
            s.tok_actual_1e6[t].store(to_human_1e6(actual[t], d), Ordering::Relaxed);
            s.tok_expected_1e6[t].store(to_human_1e6(self.expected_raw[t], d), Ordering::Relaxed);
            t += 1;
        }
        s.reads.fetch_add(1, Ordering::Relaxed);
        s.seq.fetch_add(1, Ordering::Release);
        Ok(())
    }
}

/// X's spot USDC on a scanned `spotClearinghouseState`, USD × 1e6
/// (the scanner is × 1e8). Read here rather than through slot 3's
/// session view, whose outcome-leg reading is not slot 0's business.
fn spot_usdc_1e6(bal: &[SpotBalance], body: &[u8]) -> i64 {
    let mut i = 0usize;
    while i < bal.len() {
        if bal[i].coin.of(body) == b"USDC" {
            return bal[i].total_1e8 / 100;
        }
        i += 1;
    }
    0
}

/// A 32-byte word's value, if it fits 128 bits.
#[inline]
fn word_u128(w: &[u8; 32]) -> Option<u128> {
    let mut i = 0usize;
    while i < 16 {
        if w[i] != 0 {
            return None;
        }
        i += 1;
    }
    let mut v = 0u128;
    while i < 32 {
        v = (v << 8) | u128::from(w[i]);
        i += 1;
    }
    Some(v)
}

fn worker_loop(mut w: Worker) {
    while !crate::sigint::shutdown_requested() {
        if !w.step(core_time::now_ns()) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

// ---------------------------------------------------------------
// Boot
// ---------------------------------------------------------------

/// One traded pool as the artifact and the universe describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolSpec {
    /// The pool's symbol.
    pub sym: SymbolId,
    /// The pool contract.
    pub address: [u8; 20],
    /// The universe's token0 decimals claim (checked on chain).
    pub dec0: u8,
    /// The universe's token1 decimals claim.
    pub dec1: u8,
    /// token0's price: a `coins` index or [`PRICE_USD`].
    pub coin0: u8,
    /// token1's price.
    pub coin1: u8,
}

/// One hedge coin as the artifact describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoinSpec {
    /// The perp's symbol.
    pub perp_sym: SymbolId,
    /// The venue's coin name.
    pub name: String,
    /// The member's size step, coin × 1e6 (must be the venue's).
    pub lot_1e6: i64,
}

/// What [`boot_hyparb_live`] needs.
pub struct LiveSpec<'a> {
    /// Mainnet (real money) or testnet (the rehearsal).
    pub net: LiveNet,
    /// The operator's authority, required on mainnet.
    pub authority: Option<&'a MainnetAuthority>,
    /// The HyperEVM JSON-RPC endpoint.
    pub evm_endpoint: &'a str,
    /// The H9d executor.
    pub executor: [u8; 20],
    /// Traded pools.
    pub pools: Vec<PoolSpec>,
    /// Hedge coins (index = the member's coin index).
    pub coins: Vec<CoinSpec>,
    /// The coin pricing gas (HYPE), or [`PRICE_USD`].
    pub gas_coin: u8,
    /// Where the slot's own state files live (budget, anchors).
    pub state_dir: PathBuf,
    /// X's Hyperliquid request-budget floor.
    pub budget_floor: u64,
}

/// A booted live arm.
pub struct LiveBoot<const FILL_N: usize> {
    /// The arm the router trades slot 0 through.
    pub arm: HyparbLive<FILL_N>,
    /// The `hyparb-live` thread.
    pub handle: JoinHandle<()>,
    /// One line for the boot log.
    pub tell: String,
}

/// X's keys and addresses for `net`.
fn keys_for(
    net: LiveNet,
) -> Result<
    (
        core_config::SecretKeyBytes,
        [u8; 20],
        HlConfig,
        &'static str,
    ),
    String,
> {
    match net {
        LiveNet::Mainnet => {
            let k = core_config::SecretKeyBytes::from_hex_env(crate::evm_live::KEY_ENV)
                .map_err(|e| format!("{e} — the operator writes it into the repo .env"))?;
            let x = signer_eip712::address_from_private_key(k.bytes())
                .map_err(|e| format!("{}: not a valid key ({e:?})", crate::evm_live::KEY_ENV))?;
            crate::evm_live::check_wallet_env(&x, k.bytes())?;
            // O-HL3: X signs its own Hyperliquid orders as itself.
            let cfg = HlConfig::new(
                exec_hyperliquid::Scope::Live,
                exec_hyperliquid::config::HOST_MAINNET,
                'a',
                *k.bytes(),
                x,
            )
            .map_err(|e| format!("hyperliquid config for X: {e}"))?;
            Ok((k, x, cfg, crate::evm_live::KEY_ENV))
        }
        LiveNet::Testnet => {
            let mut w = crate::evm_testnet::wallet_keys_from_env(1)?;
            let k = w.keys.swap_remove(0);
            let x = signer_eip712::address_from_private_key(k.bytes())
                .map_err(|e| format!("{}: not a valid key ({e:?})", w.source))?;
            let cfg = HlConfig::from_env(exec_hyperliquid::Scope::Testnet)
                .map_err(|e| format!("hyperliquid TESTNET config: {e}"))?;
            Ok((k, x, cfg, w.source))
        }
    }
}

/// POST `body` to `https://host/info` on a fresh boot-only client; the
/// answer's bytes.
pub(crate) fn info_once(
    host: &str,
    body: &[u8],
    tls: Arc<ClientConfig>,
) -> Result<Vec<u8>, String> {
    let mut http = core_net::HttpsPost::new(host, 443, "/info", tls, 256, META_RESP)
        .map_err(|e| format!("{host}/info: {e}"))?;
    let b = http.body_mut();
    if body.len() > b.len() {
        return Err(String::from("request too large"));
    }
    // COPY: a ≤ 256 B boot request into the client's body window — cold,
    // once per boot — rejected: rendering JSON in place for a constant.
    b[..body.len()].copy_from_slice(body);
    let (status, r) = http
        .post(body.len())
        .map_err(|e| format!("{host}/info: {e}"))?;
    if status != 200 {
        return Err(format!("{host}/info: http status {status}"));
    }
    // COPY: the boot's `meta` answer out of the client (≤ 1 MiB, once) —
    // rejected: holding the client alive past boot for one parse.
    Ok(http.resp()[r].to_vec())
}

/// One address-returning view, at boot.
fn addr_at(arm: &mut EvmArm, to: &[u8; 20], selector: [u8; 4]) -> Result<[u8; 20], String> {
    let w = arm
        .call_word(to, &selector)
        .map_err(|e| format!("{}: {e}", Hex(to)))?;
    if w[..12] != [0u8; 12] {
        return Err(format!("{}: not an address word", Hex(to)));
    }
    let mut a = [0u8; 20];
    // COPY: 20 B out of a 32 B word, boot only.
    a.copy_from_slice(&w[12..]);
    Ok(a)
}

/// **Boot slot 0's live arm.** Every refusal names what to fix; nothing
/// is sent. Checks, in order:
///
/// 1. X's keys: on mainnet, X must be `HYPERLIQUID_HYPARB_MASTER_ADDR`
///    when that is set, and never slot 3's key or master (O-HL3).
/// 2. The EVM arm: mainnet only with the authority; the endpoint's
///    chain; X funded and Ready; the executor's `owner()` is X.
/// 3. Every traded pool's `token0()` / `token1()` and their
///    `decimals()`, checked against the universe's claim.
/// 4. X's Hyperliquid arm: its budget (state files under
///    `state_dir`), each coin bound to its perp asset id from `meta`,
///    and the member's lot equal to the venue's size step.
/// 5. The equity anchor, if an earlier session persisted one for X.
pub fn boot_hyparb_live<const FILL_N: usize>(
    spec: &LiveSpec<'_>,
    tls: Arc<ClientConfig>,
) -> Result<LiveBoot<FILL_N>, String> {
    if spec.pools.is_empty() || spec.pools.len() > MAX_LIVE_POOLS {
        return Err(format!(
            "hyparb live: 1..={MAX_LIVE_POOLS} traded pools (got {})",
            spec.pools.len()
        ));
    }
    if spec.coins.is_empty() || spec.coins.len() > MAX_LIVE_COINS {
        return Err(String::from("hyparb live: 1..=8 hedge coins"));
    }
    let (key, x_evm, hl_cfg, key_source) = keys_for(spec.net)?;

    // 2. The EVM arm.
    let (host, port, path) = core_net::parse_https_url(spec.evm_endpoint)
        .ok_or_else(|| format!("`{}` is not an https URL", spec.evm_endpoint))?;
    let http = core_net::HttpsPost::new(host, port, path, tls.clone(), MAX_BODY, MAX_RESP)
        .map_err(|e| format!("{}: {e}", spec.evm_endpoint))?;
    let keys = core::slice::from_ref(&key);
    let arm = match (spec.net, spec.authority) {
        (LiveNet::Mainnet, Some(a)) => EvmArm::new_mainnet(http, a, keys),
        (LiveNet::Mainnet, None) => {
            return Err(exec_hyperevm::ChainRefusal::Unconfirmed.to_string())
        }
        (LiveNet::Testnet, _) => EvmArm::new(http, Network::Testnet, keys),
    };
    let mut arm = arm.map_err(|e| e.to_string())?;
    arm.verify_chain()
        .map_err(|e| format!("{}: {e}", spec.evm_endpoint))?;
    let sync = arm.sync(0).map_err(|e| format!("wallet X sync: {e}"))?;
    if sync.state != WalletState::Ready {
        return Err(format!(
            "wallet X {} is {:?} ({} wei) — fund it with HYPE on HyperEVM",
            Hex(&x_evm),
            sync.state,
            sync.balance_wei
        ));
    }
    let owner = arm
        .owner_of(&spec.executor)
        .map_err(|e| format!("owner() of the executor {}: {e}", Hex(&spec.executor)))?;
    if owner != x_evm {
        return Err(format!(
            "the executor {} is owned by {}, not X {} — its swap accepts its owner only",
            Hex(&spec.executor),
            Hex(&owner),
            Hex(&x_evm)
        ));
    }

    // 3. Pools and tokens.
    let mut tokens: Vec<LiveToken> = Vec::new();
    let mut pools: Vec<LivePool> = Vec::new();
    let mut i = 0usize;
    while i < spec.pools.len() {
        let p = &spec.pools[i];
        let t0 = addr_at(&mut arm, &p.address, TOKEN0_SELECTOR)?;
        let t1 = addr_at(&mut arm, &p.address, TOKEN1_SELECTOR)?;
        let mut idx = [0u8; 2];
        let legs = [(t0, p.dec0, p.coin0), (t1, p.dec1, p.coin1)];
        let mut k = 0usize;
        while k < 2 {
            let (addr, dec, price) = legs[k];
            let w = arm
                .call_word(&addr, &DECIMALS_SELECTOR)
                .map_err(|e| format!("decimals() of {}: {e}", Hex(&addr)))?;
            let on_chain = word_u128(&w).unwrap_or(u128::MAX);
            if on_chain != u128::from(dec) {
                return Err(format!(
                    "pool {}: token {} has {on_chain} decimals on chain, the universe says {dec}",
                    Hex(&p.address),
                    Hex(&addr)
                ));
            }
            idx[k] = match tokens.iter().position(|t| t.address == addr) {
                Some(j) => {
                    if tokens[j].price != price {
                        return Err(format!(
                            "token {} is priced as two different coins across pools",
                            Hex(&addr)
                        ));
                    }
                    j as u8
                }
                None => {
                    if tokens.len() == MAX_LIVE_TOKENS {
                        return Err(String::from("hyparb live: too many distinct tokens"));
                    }
                    tokens.push(LiveToken {
                        address: addr,
                        decimals: dec,
                        price,
                    });
                    (tokens.len() - 1) as u8
                }
            };
            k += 1;
        }
        pools.push(LivePool {
            sym: p.sym,
            address: p.address,
            token0: idx[0],
            token1: idx[1],
        });
        i += 1;
    }

    // 4. X's Hyperliquid arm, its perps bound.
    std::fs::create_dir_all(&spec.state_dir)
        .map_err(|e| format!("{}: {e}", spec.state_dir.display()))?;
    let (fill_prod, hl_fills) = Ring::<Fill, FILL_N>::new().split();
    let mut hl = HlExchange::new(
        &hl_cfg,
        tls.clone(),
        fill_prod,
        spec.state_dir
            .join(exec_hyperliquid::budget::DEFAULT_STATE_PATH),
        spec.budget_floor,
    )
    .map_err(|e| format!("hyperliquid arm for X: {e}"))?;
    hl.seed_budget_from_venue();
    let meta = info_once(&hl_cfg.host, br#"{"type":"meta"}"#, tls.clone())?;
    let mut disc = ingress_hyperliquid::discovery::HlDiscovery::new();
    disc.ingest_meta(&meta)
        .map_err(|e| format!("hyperliquid meta: {e:?}"))?;
    let mut coins: Vec<LiveCoin> = Vec::new();
    let mut c = 0usize;
    while c < spec.coins.len() {
        let cs = &spec.coins[c];
        let info = disc
            .resolve(cs.name.as_bytes())
            .filter(|a| a.kind == ingress_hyperliquid::discovery::HlAssetKind::Perp)
            .ok_or_else(|| format!("coin {}: not a Hyperliquid perp", cs.name))?;
        let venue_lot = i64::try_from(pow10(6u8.saturating_sub(info.sz_decimals))).unwrap_or(0);
        if info.sz_decimals > 6 || cs.lot_1e6 != venue_lot {
            return Err(format!(
                "coin {}: lot_1e6 {} is not the venue's size step (szDecimals {} → {})",
                cs.name, cs.lot_1e6, info.sz_decimals, venue_lot
            ));
        }
        hl.assets_mut()
            .bind(cs.perp_sym, info.asset_id, 0, cs.name.as_bytes())
            .map_err(|e| format!("coin {}: bind: {e:?}", cs.name))?;
        let mut name = [0u8; 16];
        let nb = cs.name.as_bytes();
        if nb.len() > name.len() {
            return Err(format!("coin {}: name too long", cs.name));
        }
        // COPY: a ≤ 16 B coin name into its fixed slot, boot only.
        name[..nb.len()].copy_from_slice(nb);
        coins.push(LiveCoin {
            perp_sym: cs.perp_sym,
            name,
            name_len: nb.len() as u8,
        });
        c += 1;
    }
    let info = core_net::HttpsPost::new(&hl_cfg.host, 443, "/info", tls, 128, INFO_RESP)
        .map_err(|e| format!("{}/info: {e}", hl_cfg.host))?;

    // 5. The anchor.
    let anchor_path = spec.state_dir.join(ANCHOR_FILE);
    let anchor = exec_hyperliquid::anchor::load(&anchor_path, hl_cfg.master_addr);
    let book = LiveBook {
        pools,
        tokens,
        coins,
        executor: spec.executor,
        gas_coin: spec.gas_coin,
    };
    let shared = Arc::new(LiveShared::new());
    let (swap_prod, swap_cons) = Ring::<SwapReq, SWAP_RING>::new().split();
    let (fill_prod, amm_fills) = Ring::<Fill, AMM_FILL_RING>::new().split();
    let (retire_prod, retired) = Ring::<u64, RETIRE_RING>::new().split();
    let worker = Worker {
        arm,
        info,
        hl_user: hl_cfg.master_addr,
        book: book.clone(),
        swaps: swap_cons,
        fills: fill_prod,
        retired: retire_prod,
        shared: shared.clone(),
        base_fee: 0,
        base_fee_at: 0,
        next_poll: 0,
        next_resync: 0,
        next_recon: 0,
        inflight: None,
        expected_raw: [0; MAX_LIVE_TOKENS],
        expected_set: false,
        moves: [TokenMove::default(); 4],
        positions: [PerpPosition::default(); MAX_POSITIONS],
        balances: vec![SpotBalance::default(); exec_hyperliquid::MAX_SPOT_BALANCES]
            .into_boxed_slice(),
        rejects: 0,
        recon_fails: 0,
        release_when_ready: false,
    };
    let handle = std::thread::Builder::new()
        .name("hyparb-live".into())
        .spawn(move || worker_loop(worker))
        .map_err(|e| format!("spawn hyparb-live: {e}"))?;
    let tell = format!(
        "hyparb: LIVE ARM ARMED — {} — wallet X {} ({key_source}) owns executor {}; \
         Hyperliquid account {} on {}; pools={} tokens={} coins={}; equity anchor {} ({})",
        match spec.net {
            LiveNet::Mainnet => "HyperEVM MAINNET (chain 999) + Hyperliquid MAINNET: REAL MONEY",
            LiveNet::Testnet => "HyperEVM TESTNET (chain 998) + Hyperliquid TESTNET (rehearsal)",
        },
        Hex(&x_evm),
        Hex(&spec.executor),
        Hex(&hl_cfg.master_addr),
        hl_cfg.host,
        book.pools.len(),
        book.tokens.len(),
        book.coins.len(),
        anchor.map_or(0, |a| a.usdc_1e6),
        anchor_path.display(),
    );
    Ok(LiveBoot {
        arm: HyparbLive {
            book,
            hl,
            hl_fills,
            swaps: swap_prod,
            amm_fills,
            retired,
            hedge_due: [(0, 0); HEDGE_DUE_N],
            shared,
            mids_1e6: [0; MAX_LIVE_COINS],
            szi_expected_1e6: [0; MAX_LIVE_COINS],
            baseline: false,
            snap: Snapshot::default(),
            drift_prev_1e6: 0,
            drift_now_1e6: 0,
            last_activity_ns: 0,
            last_hedge_fill_ns: 0,
            equity_1e6: 0,
            equity_ok: false,
            flat: false,
            anchor_1e6: anchor.map_or(0, |a| a.usdc_1e6),
            anchor_path,
            anchor_address: hl_cfg.master_addr,
            last_eval_ns: 0,
            boot_unix_ns: unix_ns(),
            stale_fills: 0,
        },
        handle,
        tell,
    })
}

/// The traded pools and the hedge coins of a booted artifact, as the
/// live arm needs them. `universe` and `parsed` are the allocated
/// `[hyperevm]` instruments and their parsed entries (index-parallel).
pub fn pools_and_coins(
    hb: &crate::hyparb_boot::HyparbBoot,
    universe: &[core_config::universe::Instrument],
    parsed: &[core_config::universe::HyperEvmPool],
) -> Result<(Vec<PoolSpec>, Vec<CoinSpec>), String> {
    let p = &hb.params;
    let mut pools = Vec::new();
    let mut i = 0usize;
    while i < p.n_pools {
        let pp = p.pools[i];
        i += 1;
        if !pp.trade {
            continue;
        }
        let Some(u) = universe.iter().position(|x| x.sym == pp.sym) else {
            return Err(format!("pool symbol {:#x} is not in the universe", pp.sym));
        };
        let Some(e) = parsed.get(u) else {
            return Err(String::from(
                "the universe's pool table is shorter than its instruments",
            ));
        };
        pools.push(PoolSpec {
            sym: pp.sym,
            address: e.address,
            dec0: e.dec0,
            dec1: e.dec1,
            coin0: pp.coin0,
            coin1: pp.coin1,
        });
    }
    let mut coins = Vec::new();
    let mut c = 0usize;
    while c < p.n_coins {
        let k = p.coins[c];
        let Some(name) = hb.coin_names.get(c) else {
            return Err(String::from("coin names and coin parameters disagree"));
        };
        if k.perp_sym == core_types::SYMBOL_ID_NONE {
            return Err(format!(
                "coin {name}: live mode hedges on the perp, which it lacks"
            ));
        }
        coins.push(CoinSpec {
            perp_sym: k.perp_sym,
            name: name.clone(),
            lot_1e6: k.lot_1e6,
        });
        c += 1;
    }
    Ok((pools, coins))
}

/// The slot's state directory: `<exec.toml dir>/hyparb/` — its own
/// budget and anchors, never slot 3's files.
#[must_use]
pub fn state_dir(exec_toml: &Path) -> PathBuf {
    exec_toml
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("hyparb")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(side: Side, qty: i64, px: i64) -> Order {
        let mut o = Order::new(
            1,
            VenueId::HyperEvm,
            7,
            side,
            core_fill::ORDER_KIND_AMM_SWAP,
            Price::from_raw(px),
            Qty::from_raw(qty),
            5 << 32,
        );
        o.strategy_id = SLOT;
        o
    }

    /// WHYPE (18) / USDC (6) at $92.70: 0.2 HYPE.
    #[test]
    fn a_sell_is_exact_token0_in_with_min_out_at_the_limit() {
        let r = swap_request(&order(Side::Ask, 200_000, 92_700_000), 3, 18, 6).unwrap();
        assert_eq!(r.amount_in, 200_000_000_000_000_000, "0.2 WHYPE raw");
        assert_eq!(r.min_out, 18_540_000, "$18.54 of USDC raw");
        assert!(r.zero_for_one);
        assert_eq!((r.pool, r.client_oid), (3, 5 << 32));
    }

    #[test]
    fn a_buy_is_exact_token1_in_rounded_up_with_min_out_the_size() {
        let r = swap_request(&order(Side::Bid, 200_000, 92_700_001), 0, 18, 6).unwrap();
        // 0.2 × 92.700001 = 18.5400002 → 18_540_001 raw USDC (ceiling).
        assert_eq!(r.amount_in, 18_540_001);
        assert_eq!(r.min_out, 200_000_000_000_000_000);
        assert!(!r.zero_for_one);
    }

    #[test]
    fn a_token_with_fewer_than_six_decimals_rounds_the_right_way() {
        // UBTC-like token0 with 8 decimals, a 5-decimal token1.
        let r = swap_request(&order(Side::Bid, 1_000, 84_000_000_000), 0, 8, 5).unwrap();
        assert_eq!(r.min_out, 100_000, "0.001 of an 8-decimal token");
        assert_eq!(r.amount_in, 8_400_000, "84.0 at 5 decimals, ceiling");
        assert_eq!(
            swap_request(&order(Side::Ask, 0, 1), 0, 18, 6),
            Err(DispatchError::EncodeOverflow)
        );
        assert_eq!(
            swap_request(&order(Side::Ask, 1, -1), 0, 18, 6),
            Err(DispatchError::EncodeOverflow)
        );
        assert_eq!(
            swap_request(&order(Side::Ask, i64::MAX, i64::MAX), 0, 38, 38),
            Err(DispatchError::EncodeOverflow)
        );
    }

    fn tok(a: u8, decimals: u8) -> LiveToken {
        LiveToken {
            address: [a; 20],
            decimals,
            price: PRICE_USD,
        }
    }

    #[test]
    fn a_fill_older_than_the_process_is_the_last_sessions() {
        let boot = 1_790_234_000_000_000_000;
        assert!(!from_this_session(boot - 1, boot), "a snapshot row from before boot");
        assert!(from_this_session(boot, boot));
        assert!(from_this_session(boot + 5_000_000_000, boot));
        // A fill the arm stamped with its monotonic receive time (a bad
        // venue stamp) is far below any unix boot time: dropped, and the
        // reconciler sees the position.
        assert!(!from_this_session(12_345_678_901, boot));
        assert!(unix_ns() > boot);
    }

    #[test]
    fn a_swap_the_executor_cannot_fund_is_refused() {
        // 30 WHYPE (18 dp) known: 30e18 raw funds, one wei more does not.
        assert!(fundable(true, 30_000_000, 18, 30 * 10u128.pow(18)));
        assert!(!fundable(true, 30_000_000, 18, 30 * 10u128.pow(18) + 1));
        // 25.5 USDC (6 dp).
        assert!(fundable(true, 25_500_000, 6, 25_500_000));
        assert!(!fundable(true, 25_500_000, 6, 25_500_001));
        // Unknown, empty or negative funds nothing.
        assert!(!fundable(false, 30_000_000, 18, 1));
        assert!(!fundable(true, 0, 6, 1));
        assert!(!fundable(true, -5, 6, 1));
        // An 8-dp token floors: 0.000001 human is 100 raw.
        assert!(fundable(true, 1, 8, 100));
        assert!(!fundable(true, 1, 8, 101));
    }

    /// The L1 dry run's real receipt (see `exec_hyperevm::rpc` tests):
    /// 25 108 724 raw token0 out of the pool, 1e11 raw token1 in.
    #[test]
    fn a_receipt_becomes_the_members_fill() {
        let (t0, t1) = (tok(1, 8), tok(2, 18));
        let moves = [
            TokenMove {
                amount: 25_108_724,
                token: [1; 20],
                a_to_b: false,
            },
            TokenMove {
                amount: 100_000_000_000,
                token: [2; 20],
                a_to_b: true,
            },
        ];
        // 1e11 raw at 18 decimals is 1e-7 of a token — below the
        // member's 1e-6 unit: no fill rather than a zero price.
        assert!(fill_of(9, &t0, &t1, &moves, 42, 77).is_none());
        let t1 = tok(2, 6);
        let f = fill_of(9, &t0, &t1, &moves, 42, 77).unwrap();
        assert_eq!(f.side, Side::Bid, "token0 into the executor: bought");
        assert_eq!(f.qty.raw(), 251_087, "0.25108724 → 0.251087 × 1e6");
        // Cash-exact at the member's unit: qty × px is the 100 000 paid.
        assert_eq!(f.px.raw(), 398_268_329_304, "100000 / 0.251087");
        assert_eq!(
            (f.order_id, f.strategy_id, f.origin),
            (42, SLOT, FILL_ORIGIN_VENUE)
        );
        // A sale; a move of a third token; two moves of one; same side.
        let sold = [
            TokenMove {
                a_to_b: true,
                ..moves[0]
            },
            TokenMove {
                a_to_b: false,
                ..moves[1]
            },
        ];
        assert_eq!(fill_of(9, &t0, &t1, &sold, 1, 1).unwrap().side, Side::Ask);
        let third = [
            moves[0],
            TokenMove {
                token: [3; 20],
                ..moves[1]
            },
        ];
        assert!(fill_of(9, &t0, &t1, &third, 1, 1).is_none());
        assert!(fill_of(9, &t0, &t1, &[moves[0], moves[0]], 1, 1).is_none());
        let same = [
            moves[0],
            TokenMove {
                a_to_b: false,
                ..moves[1]
            },
        ];
        assert!(fill_of(9, &t0, &t1, &same, 1, 1).is_none());
        assert!(fill_of(9, &t0, &t1, &moves[..1], 1, 1).is_none());
    }

    #[test]
    fn units_convert_both_ways() {
        assert_eq!(to_human_1e6(1_500_000_000_000_000_000, 18), 1_500_000);
        assert_eq!(to_human_1e6(150_000_000, 8), 1_500_000);
        assert_eq!(to_human_1e6(15, 5), 150);
        assert_eq!(
            to_raw(1_500_000, 18, false),
            Some(1_500_000_000_000_000_000)
        );
        assert_eq!(to_raw(1_500_001, 5, false), Some(150_000));
        assert_eq!(to_raw(1_500_001, 5, true), Some(150_001));
        assert_eq!(word_u128(&[0; 32]), Some(0));
        let mut w = [0u8; 32];
        w[15] = 1;
        assert_eq!(word_u128(&w), None, "past 128 bits");
        w[15] = 0;
        w[31] = 7;
        assert_eq!(word_u128(&w), Some(7));
        assert_eq!(pow10(0), 1);
        assert_eq!(pow10(18), 1_000_000_000_000_000_000);
    }

    #[test]
    fn the_seqlock_reads_a_whole_reconciliation_or_nothing() {
        let s = LiveShared::new();
        let mut snap = Snapshot::default();
        assert!(s.read(&mut snap), "an idle seq reads");
        assert_eq!(snap.reads, 0);
        s.seq.fetch_add(1, Ordering::AcqRel);
        assert!(!s.read(&mut snap), "a write in progress is not read");
        s.perp_value_1e6.store(40_000_000, Ordering::Relaxed);
        s.reads.fetch_add(1, Ordering::Relaxed);
        s.seq.fetch_add(1, Ordering::Release);
        assert!(s.read(&mut snap));
        assert_eq!(
            (snap.reads, snap.perp_value_1e6, snap.seq),
            (1, 40_000_000, 2)
        );
    }

    #[test]
    fn the_state_dir_is_the_slots_own() {
        assert_eq!(
            state_dir(Path::new("/x/multivenue/exec.toml")),
            PathBuf::from("/x/multivenue/hyparb")
        );
    }
}
