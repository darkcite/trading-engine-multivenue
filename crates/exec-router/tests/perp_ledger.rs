// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'4 (impl-Bp §3.12, §4.13, §7.9) — the router's Hyperliquid perp rows:
//! the three cap laws (I31), the boot seed (D19) and the drift halt (I32).

use clob_dispatcher::{DispatchError, DispatchStats, OrderDispatch, PaperDispatcher};
use core_types::{
    CancelReq, Fill, ModifyReq, Order, Price, Qty, Side, SymbolId, VenueId, FILL_ORIGIN_VENUE,
};
use exec_router::{
    ExecMode, ExecRoute, HaltLimits, HaltReason, Ledger, LedgerError, PerpVenuePos,
    RoutedDispatcher, SlotCaps, PERP_ROWS,
};

/// A wall stamp inside one UTC day (2025-10-09), used for orders and
/// fills alike: the test anchor is the identity.
const T0: u64 = 1_760_000_000_000_000_000;
const DAY_NS: u64 = 86_400_000_000_000;
const E6: i64 = 1_000_000;
/// The perp slot (impl-08's pine slot).
const SLOT: u8 = 8;
const BTC: SymbolId = 300;
const ETH: SymbolId = 301;

fn anchor() -> core_time::WallAnchor {
    core_time::WallAnchor::new(T0, T0)
}

/// A live arm that accepts everything — the risk gate is under test.
#[derive(Default)]
struct Accept {
    seen: u64,
}

impl OrderDispatch for Accept {
    fn submit(&mut self, _order: &Order) -> Result<(), DispatchError> {
        self.seen += 1;
        Ok(())
    }
    fn cancel(&mut self, _req: &CancelReq) -> Result<(), DispatchError> {
        Ok(())
    }
    fn modify(&mut self, _req: &ModifyReq) -> Result<(), DispatchError> {
        Ok(())
    }
    fn try_next_fill(&mut self) -> Option<Fill> {
        None
    }
    fn stats(&self) -> DispatchStats {
        DispatchStats::default()
    }
}

fn router(caps: SlotCaps, halts: HaltLimits) -> RoutedDispatcher<PaperDispatcher, Accept> {
    let mut r = ExecRoute::all_paper();
    r.set_slot(
        usize::from(SLOT),
        ExecMode::Live,
        &[VenueId::Hyperliquid.to_u8()],
        caps,
        halts,
    )
    .unwrap();
    let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), Accept::default(), anchor());
    d.bind_perp(BTC, SLOT).unwrap();
    d.mark_ledger_seeded();
    d
}

fn order(slot: u8, sym: SymbolId, buy: bool, px: i64, qty: i64, oid: u64) -> Order {
    let mut o = Order::new(
        T0,
        VenueId::Hyperliquid,
        sym,
        if buy { Side::Bid } else { Side::Ask },
        0,
        Price::from_raw(px),
        Qty::from_raw(qty),
        oid,
    );
    o.strategy_id = slot;
    o
}

fn fill_at(ts: u64, sym: SymbolId, buy: bool, px: i64, qty: i64, oid: u64) -> Fill {
    Fill::new(
        ts,
        sym,
        if buy { Side::Bid } else { Side::Ask },
        Price::from_raw(px),
        Qty::from_raw(qty),
        oid,
    )
    .with_attribution(SLOT, FILL_ORIGIN_VENUE)
}

/// I31 — `cap_instance` projects `Σ |pos| × price` and refuses only an
/// increase past the cap: over the cap, every reduction (a flip that
/// lands smaller included) passes.
#[test]
fn perp_instance_cap_projects_and_never_blocks_a_reduction() {
    let big = 1_000_000_000 * E6;
    let mut d = router(
        SlotCaps::new(big, 150_000 * E6, big, 64),
        HaltLimits::none(),
    );
    d.seed_perp(BTC, 2 * E6, 50_000 * E6, 0).unwrap();
    // 2 held + 1 bought at 50k = $150k: at the cap, allowed.
    assert_eq!(
        d.ledger()
            .perp_projection_1e6(SLOT, BTC, Side::Bid, E6, 50_000 * E6),
        (100_000 * E6, 150_000 * E6)
    );
    assert!(d
        .submit(&order(SLOT, BTC, true, 50_000 * E6, E6, 1))
        .is_ok());
    // 2 more would project $200k: refused before the arm.
    assert_eq!(
        d.submit(&order(SLOT, BTC, true, 50_000 * E6, 2 * E6, 2)),
        Err(DispatchError::RiskRefused)
    );
    assert_eq!(d.counters().refused_cap_instance, 1);

    // Over the cap ($250k held): a reduction passes, a flip that lands
    // smaller passes, a flip that lands larger is refused.
    let mut d = router(
        SlotCaps::new(big, 150_000 * E6, big, 64),
        HaltLimits::none(),
    );
    d.seed_perp(BTC, 5 * E6, 50_000 * E6, 0).unwrap();
    assert!(d
        .submit(&order(SLOT, BTC, false, 50_000 * E6, E6, 3))
        .is_ok());
    assert!(d
        .submit(&order(SLOT, BTC, false, 50_000 * E6, 6 * E6, 4))
        .is_ok());
    assert_eq!(
        d.submit(&order(SLOT, BTC, false, 50_000 * E6, 11 * E6, 5)),
        Err(DispatchError::RiskRefused)
    );
    assert_eq!(d.counters().refused_cap_instance, 1);
}

/// I31 — the order's coin is priced at the order's LIMIT on both sides of
/// the test, every other perp row at its last fill price.
#[test]
fn perp_caps_price_both_sides_at_the_order_limit() {
    let big = 1_000_000_000 * E6;
    let mut d = router(
        SlotCaps::new(big, 160_000 * E6, big, 64),
        HaltLimits::none(),
    );
    d.bind_perp(ETH, SLOT).unwrap();
    d.seed_perp(BTC, 2 * E6, 50_000 * E6, 0).unwrap();
    d.seed_perp(ETH, 10 * E6, 3_000 * E6, 0).unwrap();
    let l = d.ledger();
    // BTC at the limit on both sides; ETH at its last fill ($30k) on both.
    assert_eq!(
        l.perp_projection_1e6(SLOT, BTC, Side::Ask, E6, 40_000 * E6),
        (110_000 * E6, 70_000 * E6)
    );
    assert_eq!(
        l.perp_projection_1e6(SLOT, BTC, Side::Bid, E6, 60_000 * E6),
        (150_000 * E6, 210_000 * E6)
    );
    // At a 40k limit: 3 × 40k + 30k = $150k ≤ $160k — passes, where the
    // last fill's 50k would have read $180k.
    assert!(d
        .submit(&order(SLOT, BTC, true, 40_000 * E6, E6, 1))
        .is_ok());
    // At a 45k limit: 3 × 45k + 30k = $165k — refused.
    assert_eq!(
        d.submit(&order(SLOT, BTC, true, 45_000 * E6, E6, 2)),
        Err(DispatchError::RiskRefused)
    );
    assert_eq!(d.counters().refused_cap_instance, 1);
}

/// I31 — `cap_day` counts the OPENING notional of fills on either side; a
/// reduction adds nothing and is never refused by it.
#[test]
fn perp_day_cap_counts_opening_fills_on_either_side() {
    let big = 1_000_000_000 * E6;
    let mut d = router(SlotCaps::new(big, big, 500 * E6, 64), HaltLimits::none());
    d.seed_perp(BTC, 0, 0, 0).unwrap();
    // A sell from flat opens a short: $200.
    d.on_fill_booked(&fill_at(T0, BTC, false, 100 * E6, 2 * E6, 1));
    assert_eq!(d.ledger().perp_day_open_1e6(SLOT), 200 * E6);
    // A buy that only reduces adds nothing.
    d.on_fill_booked(&fill_at(T0, BTC, true, 100 * E6, E6, 2));
    assert_eq!(d.ledger().perp_day_open_1e6(SLOT), 200 * E6);
    // A buy that crosses zero adds only the part past it: 1 closes, 2 open.
    d.on_fill_booked(&fill_at(T0, BTC, true, 100 * E6, 3 * E6, 3));
    assert_eq!(d.ledger().perp_day_open_1e6(SLOT), 400 * E6);
    assert_eq!(
        d.ledger().slot_day_turnover_1e6(usize::from(SLOT)),
        400 * E6
    );
    // Long 2: opening 2 × $100 more would read $600 > $500.
    assert_eq!(
        d.submit(&order(SLOT, BTC, true, 100 * E6, 2 * E6, 4)),
        Err(DispatchError::RiskRefused)
    );
    assert_eq!(d.counters().refused_cap_day, 1);
    // A sell of 3 opens 1 short ($100): $500, at the cap — passes; the
    // sell side counts the same way.
    assert_eq!(
        d.ledger()
            .perp_opening_1e6(BTC, Side::Ask, 3 * E6, 100 * E6),
        100 * E6
    );
    assert!(d
        .submit(&order(SLOT, BTC, false, 100 * E6, 3 * E6, 5))
        .is_ok());
    // Once the opening fills, the sell side is spent too…
    d.on_fill_booked(&fill_at(T0, BTC, false, 100 * E6, 3 * E6, 5));
    assert_eq!(d.ledger().perp_day_open_1e6(SLOT), 500 * E6);
    assert_eq!(
        d.submit(&order(SLOT, BTC, false, 100 * E6, E6, 6)),
        Err(DispatchError::RiskRefused)
    );
    // …and the exit is still open.
    assert!(d.submit(&order(SLOT, BTC, true, 100 * E6, E6, 7)).is_ok());
    assert_eq!(d.counters().refused_cap_day, 2);
}

/// I31 — a perp fill books to its row, never to `fills_unbound`.
#[test]
fn a_perp_fill_books_to_its_row_never_unbound() {
    let mut l = Ledger::new(anchor());
    l.bind_perp(BTC, SLOT).unwrap();
    l.book_fill(&fill_at(T0, BTC, true, 100 * E6, E6, 1));
    assert_eq!(l.counters().fills_unbound, 0);
    assert_eq!(l.counters().fills_booked, 1);
    assert_eq!(l.instrument_position_1e6(usize::from(SLOT), BTC), Some(E6));
    assert_eq!(l.slot_exposure_1e6(usize::from(SLOT)), 100 * E6);
    // The same fill on an id no row binds is the unbound case.
    l.book_fill(&fill_at(T0, ETH, true, 100 * E6, E6, 2));
    assert_eq!(l.counters().fills_unbound, 1);
}

/// I31 / D19 — the boot seed restores the venue's position and today's
/// opening notional before the first order; it is one per row, refused on
/// an unbound id, and the day part rolls at 00:00Z.
#[test]
fn boot_seed_restores_position_and_day_open_notional() {
    let mut l = Ledger::new(anchor());
    l.bind_perp(BTC, SLOT).unwrap();
    l.seed_perp(BTC, -3 * E6, 2_000 * E6, 1_500 * E6).unwrap();
    let s = usize::from(SLOT);
    assert_eq!(l.instrument_position_1e6(s, BTC), Some(-3 * E6));
    assert_eq!(l.slot_exposure_1e6(s), 6_000 * E6);
    assert_eq!(l.perp_exposure_1e6(SLOT), 6_000 * E6);
    assert_eq!(l.perp_day_open_1e6(SLOT), 1_500 * E6);
    assert_eq!(l.slot_day_turnover_1e6(s), 1_500 * E6);
    assert_eq!(l.seed_perp(BTC, 0, 0, 0), Err(LedgerError::SeededTwice));
    assert_eq!(l.seed_perp(ETH, 0, 0, 0), Err(LedgerError::NotPerp));
    l.bind_perp(ETH, SLOT).unwrap();
    assert_eq!(l.seed_perp(ETH, E6, 0, 0), Err(LedgerError::BadSeed));
    assert_eq!(l.seed_perp(ETH, 0, 0, -1), Err(LedgerError::BadSeed));
    // Today's seed rolls with the day: a fill tomorrow resets it.
    l.book_fill(&fill_at(T0, BTC, true, 2_000 * E6, E6, 1));
    l.book_fill(&fill_at(T0 + DAY_NS, BTC, true, 2_000 * E6, E6, 2));
    assert_eq!(l.perp_day_open_1e6(SLOT), 0);
    assert_eq!(l.instrument_position_1e6(s, BTC), Some(-E6));
    // The table bound: PERP_ROWS rows, then refused.
    let mut k = 2u32;
    while (k as usize) < PERP_ROWS {
        l.bind_perp(BTC + k, SLOT).unwrap();
        k += 1;
    }
    assert_eq!(l.bind_perp(BTC + k, SLOT), Err(LedgerError::PerpRowsFull));
}

/// I32 — drift `Σ |ledger pos − szi| × px` at or above
/// `halt_on_recon_drift_usd_1e6` latches the slot's sticky halt.
#[test]
fn perp_position_drift_halts_the_slot() {
    let big = 1_000_000_000 * E6;
    let halts = HaltLimits::new(5, 5 * E6, 30_000, 3, 300_000);
    let mut d = router(SlotCaps::new(big, big, big, 64), halts);
    d.seed_perp(BTC, E6, 100 * E6, 0).unwrap();
    let s = usize::from(SLOT);
    let venue = |szi: i64| {
        [PerpVenuePos {
            sym: BTC,
            szi_1e6: szi,
            px_1e6: 100 * E6,
        }]
    };
    assert_eq!(d.reconcile_perps(SLOT, &venue(E6)), 0);
    assert_eq!(d.reconcile_perps(SLOT, &venue(1_040_000)), 4 * E6);
    assert!(!d.halt().is_halted(s));
    assert_eq!(d.reconcile_perps(SLOT, &venue(1_050_000)), 5 * E6);
    assert!(d.halt().is_halted(s));
    assert_eq!(d.halt().reason(s), HaltReason::ReconDrift);
    // Sticky: the venue agreeing again does not lift it.
    d.reconcile_perps(SLOT, &venue(E6));
    assert!(d.halt().is_halted(s));
    assert_eq!(
        d.submit(&order(SLOT, BTC, true, 100 * E6, E6, 1)),
        Err(DispatchError::RiskRefused)
    );
    // A coin the venue no longer lists is held against zero.
    assert_eq!(d.reconcile_perps(SLOT, &[]), 100 * E6);
}

/// I29's router half — another slot's order on a bound perp is refused
/// before any number is read.
#[test]
fn a_perp_row_refuses_another_slots_order() {
    let big = 1_000_000_000 * E6;
    let mut r = ExecRoute::all_paper();
    let caps = SlotCaps::new(big, big, big, 64);
    for s in [3usize, usize::from(SLOT)] {
        r.set_slot(
            s,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            caps,
            HaltLimits::none(),
        )
        .unwrap();
    }
    let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), Accept::default(), anchor());
    d.bind_perp(BTC, SLOT).unwrap();
    d.mark_ledger_seeded();
    assert_eq!(
        d.submit(&order(3, BTC, true, 100 * E6, E6, 1)),
        Err(DispatchError::RiskRefused)
    );
    assert_eq!(d.counters().refused_foreign_row, 1);
    assert!(d.submit(&order(SLOT, BTC, true, 100 * E6, E6, 2)).is_ok());
}

/// r28 risk review B2 (pin:bp4-perp-exit-exempt-clamp-1) — a perp EXIT is a
/// reduction that carries `ORDER_FLAG_REDUCE_ONLY` (the arm sends it to the
/// venue, so stacked exits can never flip the position): it alone skips
/// clamp 1. A reduction without the flag is judged by clamp 1 like any order,
/// and clamp 2 counts every PLACE, stacked exits too.
#[test]
fn stacked_perp_sells_are_exits_only_when_reduce_only() {
    let big = 1_000_000_000 * E6;
    // Max order $60k; two working orders at most.
    let mut d = router(SlotCaps::new(60_000 * E6, big, big, 2), HaltLimits::none());
    d.seed_perp(BTC, 2 * E6, 50_000 * E6, 0).unwrap();
    // The whole 2 BTC ($100k) without the flag: a reduction, but not an
    // exit — clamp 1 refuses it.
    assert_eq!(
        d.submit(&order(SLOT, BTC, false, 50_000 * E6, 2 * E6, 1)),
        Err(DispatchError::RiskRefused)
    );
    assert_eq!(d.counters().refused_max_order, 1);
    // Reduce-only: an exit, past clamp 1 — and a second one stacked on the
    // same unchanged position passes too (the venue bounds the pair).
    let exit = |oid| order(SLOT, BTC, false, 50_000 * E6, 2 * E6, oid).with_reduce_only();
    assert!(d.submit(&exit(2)).is_ok());
    assert!(d.submit(&exit(3)).is_ok());
    assert_eq!(d.ledger().slot_resting(usize::from(SLOT)), 2);
    // A third stacked exit: clamp 2 counts exits (O-BX25).
    assert_eq!(d.submit(&exit(4)), Err(DispatchError::RiskRefused));
    assert_eq!(d.counters().refused_open_orders, 1);
    assert_eq!(d.counters().refused_max_order, 1);
    // An opening sell past the position is never an exit, flag or not.
    let mut d = router(SlotCaps::new(60_000 * E6, big, big, 8), HaltLimits::none());
    d.seed_perp(BTC, E6, 50_000 * E6, 0).unwrap();
    assert_eq!(
        d.submit(&order(SLOT, BTC, false, 50_000 * E6, 3 * E6, 5).with_reduce_only()),
        Err(DispatchError::RiskRefused)
    );
    assert_eq!(d.counters().refused_max_order, 1);
}

/// PB13 (Binance plan §2.2 G8): the router answers the cli's Pine-vs-ledger alarm from its own
/// ledger row — a perp row is an instrument row; an unbound symbol has no row (`None`).
#[test]
fn the_ledger_answers_the_pine_alarm_from_its_own_row() {
    let big = 1_000_000_000 * E6;
    let mut d = router(SlotCaps::new(big, big, big, 8), HaltLimits::none());
    d.seed_perp(BTC, 2 * E6, 50_000 * E6, 0).unwrap();
    assert_eq!(d.slot_instrument_position_1e6(SLOT, BTC), Some(2 * E6));
    assert_eq!(d.slot_instrument_position_1e6(SLOT, ETH), None, "no row");
}
