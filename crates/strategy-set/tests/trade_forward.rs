// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM B'1 T6 (impl-Bp §3.4, §7.2): a Trade print on the venue-event lane
//! reaches the pine slot and no other member.
//!
//! pin:Bp-slot-pine — the spec's "slot 7" is `SLOT_PINE` (8) on this branch.
//! pin:Bp-I1-observables — strategy-vm's learned wall offset is not public;
//! "identical streams" is asserted on every public counter of the set plus
//! the exact order stream the members emit, with and without interleaved
//! Trade events.

use core_time::NsTs;
use core_types::{make_symbol_id, ChannelEvent, ChannelId, Order, VenueId};
use strategy_core::{Ctx, Strategy, StrategyCounters, SubmitErr};
use strategy_set::{StrategySet, AI_MASK, ALL_MASK, BIT_PINE, SLOT_PINE};

struct RecCtx {
    orders: Vec<Order>,
    now: NsTs,
}

impl Ctx for RecCtx {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        self.orders.push(order);
        Ok(())
    }
    fn now_ns(&self) -> NsTs {
        self.now
    }
}

fn ctx() -> RecCtx {
    RecCtx {
        orders: Vec::new(),
        now: 1_000_000_000_000,
    }
}

const SYM: u32 = make_symbol_id(VenueId::Hyperliquid, 1);

fn trade(i: u64) -> ChannelEvent {
    ChannelEvent::new(
        1_000 + i,
        VenueId::Hyperliquid,
        ChannelId::Trade,
        SYM,
        i,
        1_789_252_941_000 + i,
        64_437_500_000,
        250_000,
    )
}

/// Funding / AssetCtx / Mark: the non-Trade event lane traffic.
fn non_trade(i: u64) -> ChannelEvent {
    let ch = match i % 3 {
        0 => ChannelId::Funding,
        1 => ChannelId::AssetCtx,
        _ => ChannelId::Mark,
    };
    ChannelEvent::new(
        2_000 + i,
        VenueId::Hyperliquid,
        ch,
        SYM,
        0,
        1_789_252_941_000 + i,
        12_500 + i as i64,
        1_000_000,
    )
}

fn ignored(s: &StrategySet) -> u64 {
    s.pine_counters().callbacks_ignored
}

#[test]
fn trade_events_reach_the_pine_slot_and_no_other_member() {
    assert_eq!(SLOT_PINE, 8);
    let mut s = StrategySet::new(AI_MASK | BIT_PINE);
    let mut c = ctx();
    let before = ignored(&s);
    let mut i = 0;
    while i < 5 {
        s.on_venue_event(&trade(i), &mut c);
        i += 1;
    }
    // The unconfigured pine shell counts every callback it is handed:
    // exactly once per Trade event.
    assert_eq!(ignored(&s) - before, 5);
    assert!(c.orders.is_empty());
    assert_eq!(s.orders_emitted(), 0);
}

#[test]
fn trade_to_a_disabled_pine_slot_goes_nowhere() {
    let mut s = StrategySet::new(AI_MASK);
    let mut c = ctx();
    let before = ignored(&s);
    s.on_venue_event(&trade(1), &mut c);
    s.on_venue_event(&trade(2), &mut c);
    assert_eq!(ignored(&s), before, "pine is built but not enabled");
    assert!(c.orders.is_empty());
}

#[test]
fn other_members_see_identical_event_streams_with_and_without_trade_routing() {
    // ALL_MASK excludes pine: every production member, no Trade consumer.
    let mut plain = StrategySet::new(ALL_MASK | BIT_PINE);
    let mut mixed = StrategySet::new(ALL_MASK | BIT_PINE);
    let mut cp = ctx();
    let mut cm = ctx();
    let mut i = 0;
    while i < 60 {
        plain.on_venue_event(&non_trade(i), &mut cp);
        mixed.on_venue_event(&trade(i), &mut cm);
        mixed.on_venue_event(&non_trade(i), &mut cm);
        mixed.on_venue_event(&trade(i + 1_000), &mut cm);
        i += 1;
    }
    let key = |s: &StrategySet| {
        (
            s.orders_emitted(),
            s.orders_dropped(),
            s.enabled_mask(),
            s.vm_rows_active(),
            s.vm_fires(),
            s.vm_orders_emitted(),
            s.vm_orders_dropped(),
            s.vm_commit_dropped(),
            s.vm_regime_blocked(),
            s.fills_unrouted(),
            (
                s.order_events_unrouted(),
                format!("{:?}", s.vrp_counters()),
                format!("{:?}", s.regime_counters()),
            ),
        )
    };
    assert_eq!(key(&plain), key(&mixed));
    assert_eq!(cp.orders.len(), cm.orders.len());
    let mut k = 0;
    while k < cp.orders.len() {
        let (a, b) = (&cp.orders[k], &cm.orders[k]);
        assert_eq!(
            (a.sym, a.side, a.kind, a.px, a.qty, a.strategy_id),
            (b.sym, b.side, b.kind, b.px, b.qty, b.strategy_id)
        );
        k += 1;
    }
    // Only the pine slot saw the 120 prints.
    assert_eq!(ignored(&mixed) - ignored(&plain), 120);
}
