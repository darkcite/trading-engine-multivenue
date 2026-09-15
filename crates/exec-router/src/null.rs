// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The live arm E1 does not have yet.
//!
//! E1 builds the routing *mechanism*; E2/E3 build the Hyperliquid
//! exchange arm that goes inside it. Until then
//! [`RoutedDispatcher`](crate::RoutedDispatcher) still needs a
//! concrete live type, and this is it: a zero-sized dispatcher that
//! **refuses every order** with [`DispatchError::NoLiveRoute`].
//!
//! ## Why a refusing stub and not `PaperDispatcher`
//!
//! Filling the live type parameter with a second paper matcher would
//! mean a routing bug lands an order in the paper book and produces a
//! *modelled* fill wearing live semantics — precisely what **LAW E-1**
//! exists to forbid. A stub that can only refuse makes that outcome
//! unrepresentable, and makes LAW E-1 testable today rather than in E3.
//!
//! ## Why it is unreachable in a booted engine
//!
//! `core_config::exec` refuses at boot any artifact that marks a slot
//! `live` while no live arm is compiled in, so a running engine never
//! has a `Live` slot pointing here. The stub exists for the type
//! system and for the tests; if one ever *does* reach it, refusing is
//! the right answer and the counter says so.

use clob_dispatcher::{DispatchError, DispatchStats, OrderDispatch};
use core_types::{Fill, Order};

/// A live arm that refuses everything. Zero-sized apart from its
/// counters; constructing one costs nothing and allocates nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullLiveDispatcher {
    stats: DispatchStatsShim,
}

/// `DispatchStats` is not `PartialEq`, so the stub keeps the one
/// number it can honestly report and synthesizes the rest.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DispatchStatsShim {
    refused: u64,
}

impl NullLiveDispatcher {
    /// Construct the stub.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stats: DispatchStatsShim { refused: 0 },
        }
    }

    /// How many orders this stub has refused. Should be zero on a
    /// correctly-configured boot; a non-zero value means something
    /// routed `Live` to a venue with no arm, which is a bug worth
    /// seeing.
    #[inline]
    #[must_use]
    pub const fn refused(&self) -> u64 {
        self.stats.refused
    }
}

impl OrderDispatch for NullLiveDispatcher {
    /// Always refuses. Never touches a network, never allocates.
    #[inline]
    fn submit(&mut self, _order: &Order) -> Result<(), DispatchError> {
        self.stats.refused = self.stats.refused.saturating_add(1);
        Err(DispatchError::NoLiveRoute)
    }

    /// No venue, no fills.
    #[inline]
    fn try_next_fill(&mut self) -> Option<Fill> {
        None
    }

    /// Every refusal is a routing rejection.
    #[inline]
    fn stats(&self) -> DispatchStats {
        DispatchStats {
            rejected: self.stats.refused,
            rejected_routing: self.stats.refused,
            ..DispatchStats::default()
        }
    }

    // `observe_tick`, `matcher_counters` and `open_paper_orders` take
    // the trait defaults: a live arm has no business modelling fills
    // from a book (LAW E-2).
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty, Side, VenueId};

    fn order() -> Order {
        let mut o = Order::new(
            1_000,
            VenueId::Hyperliquid,
            42,
            Side::Bid,
            0,
            Price::from_raw(500_000),
            Qty::from_raw(1_000_000),
            7,
        );
        o.strategy_id = 3;
        o
    }

    #[test]
    fn it_refuses_every_order_and_counts_them() {
        let mut d = NullLiveDispatcher::new();
        assert_eq!(d.refused(), 0);
        for _ in 0..3 {
            assert_eq!(d.submit(&order()), Err(DispatchError::NoLiveRoute));
        }
        assert_eq!(d.refused(), 3);
        let s = d.stats();
        assert_eq!(s.rejected, 3);
        assert_eq!(s.rejected_routing, 3);
        assert_eq!(s.accepted, 0);
    }

    #[test]
    fn it_never_invents_a_fill_and_never_models_a_book() {
        let mut d = NullLiveDispatcher::new();
        let _ = d.submit(&order());
        assert!(d.try_next_fill().is_none());
        // LAW E-2: a live arm reports no matcher activity.
        assert_eq!(d.matcher_counters(), Default::default());
        assert_eq!(d.open_paper_orders(), 0);
    }

    #[test]
    fn it_is_zero_sized_apart_from_its_counter() {
        assert_eq!(core::mem::size_of::<NullLiveDispatcher>(), 8);
    }
}
