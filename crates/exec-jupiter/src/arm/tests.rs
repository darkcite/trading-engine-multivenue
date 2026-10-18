// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The arm against a scripted jup-gw: the test holds the gateway's end of
//! the duplex.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `arm.rs`): every copy here builds a fixture or reads the rig's rings.

use super::*;
use core_types::pm_roll::{pm5_instance_id, pm5_pair_of_window, pm5_rolling_sym, pm_rolling_sym, PM_SETTLED_UNKNOWN};
use exec_gateway::{duplex, GatewayEnd};

const WINDOW: u32 = 5_974_360;
const SLOT: u8 = 11;

struct Bench {
    arm: JupArm,
    gw: GatewayEnd<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>,
}

fn inst(g: usize) -> u32 {
    pm5_instance_id(WINDOW, g).unwrap()
}

fn sym(g: usize, leg: u32) -> SymbolId {
    pm5_rolling_sym(g, pm5_pair_of_window(WINDOW), leg)
}

fn law(g: usize, leg: u32) -> ChannelEvent {
    ChannelEvent::new(0, VenueId::Polymarket, ChannelId::OutcomeMeta, sym(g, leg), u64::from(inst(g)), 0, 10_000, 5_000_000)
}

fn settle(g: usize, v0: i64) -> ChannelEvent {
    ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::InstrumentRoll,
        sym(g, 0),
        core_types::pack_roll_seq(inst(g), 60, pm5_pair_of_window(WINDOW), true),
        0,
        v0,
        0,
    )
}

fn bench() -> Bench {
    let (eng, gw) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let mut arm = JupArm::new(
        JupArmCfg {
            slot: SLOT,
            live_families: 0b11,
            max_order_micro: 50_000_000,
        },
        eng,
    )
    .expect("boots");
    arm.on_venue_event(&law(0, 0));
    arm.on_venue_event(&law(0, 1));
    arm.on_venue_event(&law(1, 0));
    Bench { arm, gw }
}

fn oid(g: usize, seq: u64) -> u64 {
    (seq << 40) | (u64::from(SLOT) << 32) | u64::from(inst(g))
}

fn order(g: usize, leg: u32, px: i64, qty: i64, seq: u64) -> Order {
    let mut o = Order::new(
        1,
        VenueId::Jupiter,
        sym(g, leg),
        Side::Bid,
        ORDER_KIND_IOC,
        Price::from_raw(px),
        Qty::from_raw(qty),
        oid(g, seq),
    );
    o.strategy_id = SLOT;
    o
}

fn intents(b: &mut Bench) -> Vec<JupIntent> {
    let mut v = Vec::new();
    while let Some(c) = b.gw.cmd.pop() {
        v.push(*c);
    }
    v
}

fn send(b: &mut Bench, kind: u8, cid: u64, f: impl FnOnce(&mut JupEvent)) {
    let mut e = JupEvent::new(kind, cid, 0);
    f(&mut e);
    assert!(b.gw.evt.emit(&e));
}

/// $5 at 0.95: 5 263 158 contracts.
const QTY: i64 = 5_263_158;

#[test]
fn admission_takes_a_5m_leg_ioc_buy_on_jupiter_and_refuses_everything_else() {
    let mut b = bench();
    assert_eq!(b.arm.submit(&order(0, 1, 950_000, QTY, 1)), Ok(()));
    let it = intents(&mut b);
    assert_eq!(it.len(), 1);
    assert_eq!((it[0].kind, it[0].outcome, it[0].instance), (INTENT_BUY, 1, inst(0)));
    assert_eq!((it[0].deposit_micro, it[0].limit_px_1e6), (5_000_000, 950_000));
    assert_eq!(it[0].window_end_ns, pm5_window_end_ns(WINDOW));
    let d = 5_300_000; // a $5.035 deposit at 0.95
    // Not slot 11.
    let mut o = order(0, 0, 950_000, d, 2);
    o.strategy_id = 9;
    assert_eq!(b.arm.submit(&o), Err(DispatchError::Unroutable));
    // Not venue 10.
    let mut o = order(0, 0, 950_000, d, 3);
    o.venue = VenueId::Polymarket as u8;
    assert_eq!(b.arm.submit(&o), Err(DispatchError::Unroutable));
    // A 15 m leg (slot 9's family), the 5 m mark, an ask.
    let mut o = order(0, 0, 950_000, d, 4);
    o.sym = pm_rolling_sym(0, 0, 0);
    assert_eq!(b.arm.submit(&o), Err(DispatchError::Unroutable));
    let mut o = order(0, 0, 950_000, d, 5);
    o.sym = core_types::pm_roll::pm5_mark_sym(0);
    assert_eq!(b.arm.submit(&o), Err(DispatchError::Unroutable));
    let mut o = order(0, 0, 950_000, d, 6);
    o.side = Side::Ask;
    assert_eq!(b.arm.submit(&o), Err(DispatchError::Unroutable));
    // A family not live, a leg whose law is not in force.
    assert_eq!(b.arm.submit(&order(2, 0, 950_000, d, 7)), Err(DispatchError::Unroutable));
    assert_eq!(b.arm.counters().refused_family, 1);
    assert_eq!(b.arm.submit(&order(1, 1, 950_000, d, 8)), Err(DispatchError::Unroutable));
    // Prices off the binary law; under $5; over the maximum.
    assert_eq!(b.arm.submit(&order(0, 0, 1_000_000, d, 9)), Err(DispatchError::Unroutable));
    assert_eq!(b.arm.submit(&order(0, 0, 0, d, 10)), Err(DispatchError::Unroutable));
    assert_eq!(b.arm.submit(&order(0, 0, 950_000, 5_000_000, 11)), Err(DispatchError::Unroutable));
    assert_eq!(b.arm.submit(&order(0, 0, 950_000, 60_000_000, 12)), Err(DispatchError::Unroutable));
    assert_eq!(b.arm.counters().refused_size, 2);
    // A duplicate id.
    assert_eq!(b.arm.submit(&order(0, 1, 950_000, QTY, 1)), Err(DispatchError::Unroutable));
    assert!(intents(&mut b).is_empty());
    assert_eq!(b.arm.counters().submitted, 1);
    // No cancel, no modify: a landed keeper order cannot be pulled.
    let req = CancelReq::new(0, VenueId::Jupiter, sym(0, 1), oid(0, 1));
    assert_eq!(b.arm.cancel(&req), Err(DispatchError::Unsupported));
}

#[test]
fn fills_book_the_cumulative_difference_and_retire_after_the_fill() {
    let mut b = bench();
    let cid = oid(0, 1);
    b.arm.submit(&order(0, 1, 960_000, QTY, 1)).unwrap();
    let _ = intents(&mut b);
    send(&mut b, EVT_LANDED, cid, |_| {});
    send(&mut b, EVT_FILL, cid, |e| {
        e.filled_micro = 3_000_000;
        e.avg_px_micro = 940_000;
        e.cost_micro = 2_820_000;
    });
    let f = b.arm.try_next_fill().expect("the first fill");
    assert_eq!((f.qty.raw(), f.px.raw(), f.order_id), (3_000_000, 940_000, cid));
    assert_eq!((f.strategy_id, f.origin), (SLOT, FILL_ORIGIN_VENUE));
    send(&mut b, EVT_FILL, cid, |e| {
        e.filled_micro = QTY as u64;
        e.avg_px_micro = 950_000;
        e.cost_micro = 4_999_999;
    });
    send(&mut b, EVT_DONE, cid, |_| {});
    let f = b.arm.try_next_fill().expect("the difference");
    assert_eq!(f.qty.raw(), QTY - 3_000_000);
    // (4 999 999 − 2 820 000) / 2 263 157 contracts.
    assert_eq!(f.px.raw(), 963_255);
    // The retirement waits one poll behind its fill: this poll releases it.
    assert!(b.arm.try_next_retired().is_none());
    assert!(b.arm.try_next_fill().is_none());
    let r = b.arm.try_next_retired().expect("retired");
    assert_eq!((r.client_oid, r.why), (cid, RETIRED_FILLED));
    assert_eq!(b.arm.open_orders(), 0);
    let c = b.arm.counters();
    assert_eq!((c.landed, c.fills, c.done), (1, 2, 1));
    // A repeated cumulative fill books nothing.
    send(&mut b, EVT_FILL, cid, |e| {
        e.filled_micro = QTY as u64;
        e.avg_px_micro = 950_000;
        e.cost_micro = 4_999_999;
    });
    assert!(b.arm.try_next_fill().is_none());
    assert_eq!(b.arm.counters().fills, 2);
}

#[test]
fn a_refusal_retires_rejected_and_counts_the_streak() {
    let mut b = bench();
    let cid = oid(0, 2);
    b.arm.submit(&order(0, 0, 950_000, QTY, 2)).unwrap();
    send(&mut b, EVT_REFUSED, cid, |e| e.reason = reason::GUARD);
    let _ = b.arm.on_idle();
    let _ = b.arm.on_idle();
    let r = b.arm.try_next_retired().expect("retired");
    assert_eq!(r.why, RETIRED_REJECTED);
    assert_eq!(b.arm.halt_signal(), HaltSignal::new(0, 0, 1, 0, false, false, 0).with_pnl(false, 0).with_gateway_halted(false));
    // An event of no order halts the arm.
    send(&mut b, EVT_LANDED, 999, |_| {});
    let _ = b.arm.on_idle();
    assert!(b.arm.halted());
    assert_eq!(b.arm.submit(&order(0, 0, 950_000, QTY, 3)), Err(DispatchError::SlotDisabled));
}

#[test]
fn the_worth_is_usdc_plus_legs_at_cost_and_a_lost_leg_leaves_it() {
    let mut b = bench();
    let (c0, c1) = (oid(0, 3), oid(0, 4));
    b.arm.submit(&order(0, 0, 950_000, QTY, 3)).unwrap();
    b.arm.submit(&order(0, 1, 950_000, QTY, 4)).unwrap();
    for cid in [c0, c1] {
        send(&mut b, EVT_FILL, cid, |e| {
            e.filled_micro = QTY as u64;
            e.avg_px_micro = 950_000;
            e.cost_micro = 4_999_999;
        });
    }
    send(&mut b, EVT_BALANCE, 0, |e| e.aux = 90_000_000);
    let _ = b.arm.on_idle();
    assert_eq!(b.arm.worth(), (99_999_998, Some(99_999_998)));
    // Up won: the Down leg is lost (worth 0, out of the ledger), the Up
    // leg stays at cost and its claim is asked.
    b.arm.on_venue_event(&settle(0, PM_SETTLED_UP));
    assert_eq!(b.arm.worth().0, 94_999_999);
    assert_eq!(b.arm.counters().legs_lost, 1);
    let it = intents(&mut b);
    assert_eq!(it.iter().filter(|i| i.kind == INTENT_CLAIM).count(), 1);
    let s = b.arm.halt_signal();
    assert_eq!(s, HaltSignal::new(0, 0, 0, 0, false, true, s.recon_age_ns).with_pnl(true, -4_999_999).with_gateway_halted(false));
    // An unknown settlement keeps the leg, at cost.
    let mut b2 = bench();
    b2.arm.submit(&order(0, 0, 950_000, QTY, 5)).unwrap();
    send(&mut b2, EVT_FILL, oid(0, 5), |e| {
        e.filled_micro = QTY as u64;
        e.avg_px_micro = 950_000;
        e.cost_micro = 4_999_999;
    });
    send(&mut b2, EVT_BALANCE, 0, |e| e.aux = 1_000_000);
    let _ = b2.arm.on_idle();
    b2.arm.on_venue_event(&settle(0, PM_SETTLED_UNKNOWN));
    assert_eq!(b2.arm.worth().0, 5_999_999);
}

#[test]
fn exec_halt_reaches_the_gateway_and_the_slot_is_clear_once_its_orders_end() {
    let mut b = bench();
    let cid = oid(0, 6);
    b.arm.submit(&order(0, 0, 950_000, QTY, 6)).unwrap();
    assert_eq!(b.arm.cancel_slot(SLOT), Ok(()));
    assert_eq!(b.gw.stats.halt_reason(), HALT_OPERATOR);
    assert_eq!(b.arm.cancel_slot_state(SLOT), CancelAllState::Working);
    assert_eq!(b.arm.submit(&order(0, 1, 950_000, QTY, 7)), Err(DispatchError::SlotDisabled));
    send(&mut b, EVT_REFUSED, cid, |e| e.reason = reason::HALTED);
    let _ = b.arm.on_idle();
    assert_eq!(b.arm.cancel_slot_state(SLOT), CancelAllState::Clear);
    // A halt refusal is not a venue reject.
    assert_eq!(b.arm.halt_signal().reject_streak, 0);
}

#[test]
fn the_arm_boots_only_for_slot_11_and_its_own_halt_stops_the_gateway() {
    let (eng, _gw) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let cfg = JupArmCfg {
        slot: 9,
        live_families: 1,
        max_order_micro: 50_000_000,
    };
    assert_eq!(JupArm::new(cfg, eng).err(), Some(ArmBootErr::Slot));
    let (eng, _gw) = duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let cfg = JupArmCfg {
        slot: SLOT,
        live_families: 1,
        max_order_micro: 4_999_999,
    };
    assert_eq!(JupArm::new(cfg, eng).err(), Some(ArmBootErr::MaxOrder));
    // An event of no order halts the arm AND jup-gw: an intent already
    // queued is refused unbuilt.
    let mut b = bench();
    send(&mut b, EVT_LANDED, 12_345, |_| {});
    let _ = b.arm.on_idle();
    assert!(b.arm.halted());
    assert_eq!(b.gw.stats.halt_reason(), HALT_ARM);
}

#[test]
fn a_claimed_leg_leaves_the_ledger_and_the_next_read_values_the_payout() {
    let mut b = bench();
    let cid = oid(0, 9);
    b.arm.submit(&order(0, 0, 950_000, QTY, 9)).unwrap();
    send(&mut b, EVT_FILL, cid, |e| {
        e.filled_micro = QTY as u64;
        e.avg_px_micro = 950_000;
        e.cost_micro = 4_999_999;
    });
    send(&mut b, EVT_BALANCE, 0, |e| e.aux = 10_000_000);
    let _ = b.arm.on_idle();
    assert_eq!(b.arm.worth(), (14_999_999, Some(14_999_999)));
    b.arm.on_venue_event(&settle(0, PM_SETTLED_UP));
    // The venue settled it on its own: the positions read says claimed.
    send(&mut b, EVT_CLAIMED, 0, |e| {
        e.aux = u64::from(inst(0));
        e.reason = 0;
        e.cost_micro = 4_999_999;
    });
    send(&mut b, EVT_BALANCE, 0, |e| e.aux = 15_263_158);
    let _ = b.arm.on_idle();
    assert_eq!(b.arm.counters().legs_claimed, 1);
    assert_eq!(b.arm.worth().0, 15_263_158, "the payout is in the USDC, the leg out of the worth");
}

/// Review round 2: execute-stage refusals reach the reject streak (the
/// router's `halt_on_reject_streak`); a landing resets it.
#[test]
fn execute_stage_refusals_count_toward_the_reject_streak() {
    let mut b = bench();
    let mut seq = 20u64;
    while seq < 23 {
        b.arm.submit(&order(0, 0, 950_000, QTY, seq)).unwrap();
        send(&mut b, EVT_IN_DOUBT, oid(0, seq), |e| e.reason = reason::VENUE);
        seq += 1;
    }
    // An unanswered execute (reason 0) is no refusal.
    b.arm.submit(&order(0, 0, 950_000, QTY, seq)).unwrap();
    send(&mut b, EVT_IN_DOUBT, oid(0, seq), |_| {});
    let _ = b.arm.on_idle();
    assert_eq!(b.arm.halt_signal().reject_streak, 3);
    assert_eq!(b.arm.open_orders(), 4, "in doubt, not retired");
    send(&mut b, EVT_LANDED, oid(0, 20), |_| {});
    let _ = b.arm.on_idle();
    assert_eq!(b.arm.halt_signal().reject_streak, 0);
}
