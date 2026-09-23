// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `core-amm` — the reusable AMM engine (HYPARB H1)
//!
//! Concentrated-liquidity (Uniswap V3 ABI) and constant-product swap
//! simulation, and the single-block arb solve against a hedge venue.
//! Pure math: no network, no config, no I/O, no allocation, no floats.
//! This crate is what makes chain #2 cheap — nothing in it knows it is
//! on HyperEVM.
//!
//! ## What it guarantees
//!
//! * **Bit-exact with the contracts.** `TickMath`, `SqrtPriceMath`,
//!   `SwapMath` and the pool's swap loop are ported with their rounding
//!   directions and their bitmap-word step decomposition, over a
//!   256-bit integer ([`u256`] is crate-private). A replayed on-chain
//!   swap reproduces the chain's amounts to the wei — see
//!   `tests/replay.rs`, the H1 gate.
//! * **Never extrapolates.** A [`TickMap`] carries the coverage it was
//!   fetched over; a walk clamps to it and stops, flagged. Liquidity
//!   beyond the map is unknown, and guessing it manufactures size that
//!   is not on chain.
//! * **Carries its own impact.** Every walk returns the pool state AFTER
//!   the swap; [`solve_arb`] hands it back in [`ArbQuote::after`] and the
//!   caller must carry it, or a standing gap is harvested every block.
//! * **The matcher's judge is conservative by construction.**
//!   [`swap_in_range`] / [`swap_exact_in_range`] walk the ACTIVE range
//!   only (constant liquidity) and stop at its edge: they can fill less
//!   than the chain would, never more.
//!
//! ## Units
//!
//! Raw token amounts are `u128` in the token's own decimals. A `uint160`
//! sqrt price is carried as `(low 128 bits, high 32 bits)`. Human prices
//! are token1 per token0 × 1e18; USD is × 1e6; edges are bps × 1e6.

#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

mod arb;
mod price;
mod sqrt_price_math;
mod swap;
mod tick_math;
mod types;
mod u256;

pub use arb::solve_arb;
pub use price::{price_1e18_from_sqrt, range_bounds, sqrt_from_price_1e18};
pub use swap::{swap_exact, swap_exact_in_range, swap_in_range, swap_to_target, MAX_STEPS};
pub use tick_math::{sqrt_at_tick, tick_at_sqrt, MAX_SQRT_HI, MAX_SQRT_LO, MAX_TICK, MIN_SQRT_LO, MIN_TICK};
pub use types::{
    max_liquidity_per_tick, AmmError, ArbParams, ArbQuote, ArbSide, PoolMeta, PoolState, SwapResult, SwapSpec,
    TickMap, TickNode, AMM_KIND_V2, AMM_KIND_V3, ARB_FLAG_BELOW_GAS, ARB_FLAG_MAP_EDGE, ARB_FLAG_MATH,
    ARB_FLAG_NOT_LIVE, ARB_FLAG_SIZE_CAPPED, POOL_FLAG_EDGE, POOL_FLAG_STALE, SWAP_FLAG_EDGE, SWAP_FLAG_LIMIT,
    SWAP_FLAG_LIQ_CLAMP, SWAP_FLAG_MATH, SWAP_FLAG_REFUSED, SWAP_FLAG_SATURATED, SWAP_FLAG_STEP_CAP,
};
