// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-hyparb — the slot-0 member (HYPARB, O-H1/O-H2)
//!
//! HyperEVM concentrated-liquidity pools against Hyperliquid spot and
//! perp books: the single-block CEX-DEX arb, paper-first.
//!
//! **H0 stub.** The member is linked into `strategy-set` at slot 0 so the
//! set, the mask table and the boot resolve `hyparb`; it emits nothing
//! and is never in the live engine's mask (O-H8, land dark). H4 fills
//! it in.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core_types::{Fill, NsTs, Signal, Tick};
use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError};

/// The slot-0 member.
#[derive(Debug, Default)]
pub struct HyparbStrategy {
    _private: (),
}

impl HyparbStrategy {
    /// An unconfigured member: every callback is a no-op.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl StrategyCounters for HyparbStrategy {
    fn strategy_kind(&self) -> &'static str {
        "hyparb"
    }
}

impl Strategy for HyparbStrategy {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        Ok(())
    }

    fn on_tick<C: Ctx>(&mut self, _tick: &Tick, _ctx: &mut C) {}

    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}

    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}

    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}

    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}
