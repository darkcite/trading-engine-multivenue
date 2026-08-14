//! # strategy-news
//!
//! News-driven probability shifts. Consumes `Signal`s whose
//! `LatencyClass == Slow` (RSS feeds) or `Warm`/`Hot` (X/Benzinga when
//! unlocked in Phase 6).

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use core_time::NsTs;
use core_types::{Fill, Signal, Tick};
use strategy_core::{Ctx, Strategy, StrategyError};

/// News-driven strategy skeleton. Phase 0: counts callbacks, trades nothing.
pub struct NewsStrategy {
    /// Callback tally.
    pub callbacks: u64,
}

impl NewsStrategy {
    /// Construct empty.
    pub const fn new() -> Self {
        Self { callbacks: 0 }
    }
}

impl Default for NewsStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl strategy_core::StrategyCounters for NewsStrategy {}

impl Strategy for NewsStrategy {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        Ok(())
    }
    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, _tick: &Tick, _ctx: &mut C) {}
    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {
        self.callbacks = self.callbacks.wrapping_add(1);
    }
    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}
    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}
    #[inline(always)]
    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }
    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn constructor_is_const() {
        const S: NewsStrategy = NewsStrategy::new();
        assert_eq!(S.callbacks, 0);
    }
}
