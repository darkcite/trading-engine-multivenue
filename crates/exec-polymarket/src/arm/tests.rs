// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The arm against a scripted pm-gw and pm-user: the test holds the
//! gateway's end of the duplex and the user ring's producer.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `arm.rs`): every copy here builds a fixture or reads the rig's rings.

use super::*;
use crate::cmd::PM_USER_RING;
use crate::userws::{TS_CONFIRMED, TS_FAILED, TS_MATCHED, TS_MINED};
use core_ring::{Producer, Ring};
use core_types::pm_roll::{
    pm_instance_id, pm_mark_sym, pm_pair_of_window, pm_rolling_sym, pm_window_of_open,
    PM_SETTLED_UNKNOWN,
};
use exec_gateway::{duplex, GatewayEnd};

const OPEN: u64 = 1_792_216_800;
const SLOT: u8 = 9;

struct Bench {
    arm: PmArm,
    gw: GatewayEnd<PmCmd, PmEvt, PM_CMD_RING, PM_EVT_RING>,
    user: Producer<PmEvt, PM_USER_RING>,
}

fn window() -> u32 {
    pm_window_of_open(OPEN).unwrap()
}

fn inst(f: usize) -> u32 {
    pm_instance_id(window(), f).unwrap()
}

fn sym(f: usize, leg: u32) -> SymbolId {
    pm_rolling_sym(f, pm_pair_of_window(window()), leg)
}

fn law(f: usize, leg: u32, tick: i64) -> ChannelEvent {
    ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::OutcomeMeta,
        sym(f, leg),
        u64::from(inst(f)),
        0,
        tick,
        5_000_000,
    )
}

fn bench() -> Bench {
    let (eng, gw) = duplex::<PmCmd, PmEvt, PM_CMD_RING, PM_EVT_RING>();
    let (user, user_rx) = Ring::<PmEvt, PM_USER_RING>::new().split();
    let mut arm = PmArm::new(
        PmArmCfg {
            slot: SLOT,
            live_families: 0b11,
        },
        eng,
        user_rx,
    );
    arm.on_venue_event(&law(0, 0, 10_000));
    arm.on_venue_event(&law(0, 1, 10_000));
    arm.on_venue_event(&law(1, 0, 1_000));
    Bench { arm, gw, user }
}

fn oid(f: usize, leg: u64, seq: u64) -> u64 {
    (seq << 49) | (leg << 48) | ((f as u64) << 32) | u64::from(inst(f))
}

fn order(f: usize, leg: u32, px: i64, qty: i64, seq: u64) -> Order {
    let mut o = Order::new(
        1,
        VenueId::Polymarket,
        sym(f, leg),
        Side::Bid,
        ORDER_KIND_IOC,
        Price::from_raw(px),
        Qty::from_raw(qty),
        oid(f, u64::from(leg), seq),
    );
    o.strategy_id = SLOT;
    o
}

fn cmds(b: &mut Bench) -> Vec<PmCmd> {
    let mut v = Vec::new();
    while let Some(c) = b.gw.cmd.pop() {
        v.push(*c);
    }
    v
}

fn gw(b: &mut Bench, e: PmEvt) {
    assert!(b.gw.evt.emit(&e));
}

fn sent(cid: u64, id: [u8; 32]) -> PmEvt {
    let mut e = PmEvt::about(EVT_SENT, cid);
    e.order_id = id;
    e
}

fn ack(cid: u64, id: [u8; 32], trades: i64) -> PmEvt {
    let mut e = PmEvt::about(EVT_ACK, cid);
    e.order_id = id;
    e.b = trades;
    e
}

fn fills(b: &mut Bench) -> Vec<Fill> {
    let mut v = Vec::new();
    while let Some(f) = b.arm.try_next_fill() {
        v.push(f);
    }
    v
}

#[test]
fn admission_refuses_everything_but_a_live_familys_ioc_buy_on_its_law() {
    let mut b = bench();
    let mut o = order(0, 0, 930_000, 268_810_000, 1);
    o.strategy_id = 3;
    assert_eq!(
        b.arm.submit(&o),
        Err(DispatchError::Unroutable),
        "another slot"
    );
    let mut o = order(0, 0, 930_000, 1_000_000, 1);
    o.sym = pm_mark_sym(0);
    assert_eq!(
        b.arm.submit(&o),
        Err(DispatchError::Unroutable),
        "the mark slot"
    );
    let mut o = order(0, 0, 930_000, 1_000_000, 1);
    o.side = Side::Ask;
    assert_eq!(
        b.arm.submit(&o),
        Err(DispatchError::Unroutable),
        "never a sell"
    );
    let o = order(2, 0, 930_000, 1_000_000, 1);
    assert_eq!(
        b.arm.submit(&o),
        Err(DispatchError::Unroutable),
        "SOL is not live (D-9)"
    );
    let o = order(1, 1, 930_000, 1_000_000, 1);
    assert_eq!(
        b.arm.submit(&o),
        Err(DispatchError::Unroutable),
        "no law on ETH's Down yet"
    );
    assert_eq!(
        (
            b.arm.counters().refused_shape,
            b.arm.counters().refused_family
        ),
        (4, 1)
    );
    assert!(cmds(&mut b).is_empty());
    // BTC Up at 0.93 for 268.81 shares: $249.99 to spend.
    let o = order(0, 0, 930_000, 268_810_000, 1);
    assert_eq!(b.arm.submit(&o), Ok(()));
    let c = cmds(&mut b);
    assert_eq!(c.len(), 1);
    assert_eq!(
        (
            c[0].kind,
            c[0].client_oid,
            c[0].px_1e6,
            c[0].usd_1e6,
            c[0].instance,
            c[0].tick_1e6,
            c[0].leg
        ),
        (
            CMD_SUBMIT,
            o.client_oid,
            930_000,
            249_990_000,
            inst(0),
            10_000,
            0
        )
    );
    assert_eq!(
        b.arm.submit(&o),
        Err(DispatchError::Unroutable),
        "one cid, one order"
    );
    assert_eq!(
        b.arm
            .cancel(&CancelReq::new(1, VenueId::Polymarket, o.sym, o.client_oid)),
        Err(DispatchError::Unsupported)
    );
}

#[test]
fn an_order_books_each_trade_once_and_retires_a_poll_after_its_fills() {
    let mut b = bench();
    let o = order(0, 0, 930_000, 268_810_000, 1);
    b.arm.submit(&o).unwrap();
    let _ = cmds(&mut b);
    let id = [3u8; 32];
    gw(&mut b, sent(o.client_oid, id));
    gw(&mut b, ack(o.client_oid, id, 2));
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id, 11, 920_000, 100_000_000, TS_MATCHED)));
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id, 11, 920_000, 100_000_000, TS_MINED)));
    let f = fills(&mut b);
    assert_eq!(f.len(), 1, "a trade id is booked once");
    assert_eq!(
        (
            f[0].sym,
            f[0].px.raw(),
            f[0].qty.raw(),
            f[0].order_id,
            f[0].strategy_id
        ),
        (o.sym, 920_000, 100_000_000, o.client_oid, SLOT)
    );
    assert_eq!(f[0].origin, FILL_ORIGIN_VENUE);
    assert!(
        b.arm.try_next_retired().is_none(),
        "one of two trades: open"
    );
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id, 12, 930_000, 168_810_000, TS_MATCHED)));
    // Done at that poll; its retirement is released by the next one —
    // which `fills` ran after it had popped the fill.
    assert_eq!(fills(&mut b).len(), 1);
    let r = b.arm.try_next_retired().expect("retired after its fills");
    assert_eq!(
        (r.client_oid, r.slot, r.why),
        (o.client_oid, SLOT, RETIRED_FILLED)
    );
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id, 12, 930_000, 168_810_000, TS_CONFIRMED)));
    assert!(fills(&mut b).is_empty());
    assert_eq!(
        (b.arm.counters().confirmed, b.arm.counters().repeats),
        (1, 2)
    );
    assert!(!b.arm.halted());
    assert_eq!(b.arm.open_orders(), 0);
}

#[test]
fn a_fill_before_its_sent_is_read_finds_its_order_and_a_stranger_halts() {
    let mut b = bench();
    let o = order(0, 1, 900_000, 10_000_000, 2);
    b.arm.submit(&o).unwrap();
    let id = [8u8; 32];
    // The channel's trade is in the user ring before the arm read SENT.
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id, 5, 900_000, 10_000_000, TS_MATCHED)));
    gw(&mut b, sent(o.client_oid, id));
    // (SENT is read first anyway: the gw ring is drained before the user
    // ring; and again on a trade that finds no order.)
    assert_eq!(fills(&mut b).len(), 1);
    assert!(!b.arm.halted());
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill([9; 32], 6, 900_000, 1_000_000, TS_MATCHED)));
    assert!(fills(&mut b).is_empty());
    assert!(b.arm.halted(), "a trade of no order of ours");
    assert_eq!(
        b.arm.submit(&order(0, 0, 930_000, 1_000_000, 3)),
        Err(DispatchError::SlotDisabled)
    );
}

#[test]
fn a_reversal_after_a_booking_halts() {
    let mut b = bench();
    let o = order(0, 0, 930_000, 10_000_000, 1);
    b.arm.submit(&o).unwrap();
    let id = [4u8; 32];
    gw(&mut b, sent(o.client_oid, id));
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id, 21, 930_000, 10_000_000, TS_MATCHED)));
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id, 21, 930_000, 10_000_000, TS_FAILED)));
    assert_eq!(fills(&mut b).len(), 1);
    assert!(b.arm.halted());
    assert_eq!(b.arm.counters().reversed, 1);
}

#[test]
fn refusals_retire_and_doubt_goes_to_recon_which_books_and_retires() {
    let mut b = bench();
    let o = order(0, 0, 930_000, 10_000_000, 1);
    b.arm.submit(&o).unwrap();
    let _ = cmds(&mut b);
    let mut r = PmEvt::about(EVT_REFUSED, o.client_oid);
    r.why = crate::response::Refusal::NoMatch as u8;
    gw(&mut b, r);
    b.arm.on_idle();
    b.arm.on_idle();
    let ret = b.arm.try_next_retired().unwrap();
    assert_eq!(ret.why, RETIRED_REJECTED);
    assert_eq!(b.arm.counters().no_match, 1);
    // In doubt: recon after DOUBT_RECON_NS, keyed by the order's id.
    let o2 = order(0, 0, 930_000, 10_000_000, 2);
    b.arm.submit(&o2).unwrap();
    let _ = cmds(&mut b);
    let id = [6u8; 32];
    gw(&mut b, sent(o2.client_oid, id));
    let mut d = PmEvt::about(EVT_IN_DOUBT, o2.client_oid);
    d.order_id = id;
    gw(&mut b, d);
    let t0 = core_time::now_ns();
    b.arm.pump(t0);
    assert!(cmds(&mut b).is_empty(), "not yet");
    b.arm.pump(t0 + DOUBT_RECON_NS + SCAN_EVERY_NS);
    let c = cmds(&mut b);
    assert_eq!(
        (c.len(), c[0].kind, c[0].order_id, c[0].instance),
        (1, CMD_RECON, id, inst(0))
    );
    // Recon's fill comes on the gw ring, then DONE.
    gw(
        &mut b,
        PmEvt::fill(id, 31, 925_000, 10_000_000, TS_CONFIRMED),
    );
    let mut done = PmEvt::about(EVT_DONE, o2.client_oid);
    done.b = 1;
    gw(&mut b, done);
    let f = fills(&mut b);
    assert_eq!((f.len(), f[0].px.raw()), (1, 925_000));
    b.arm.on_idle();
    assert_eq!(b.arm.try_next_retired().unwrap().why, RETIRED_FILLED);
    // A recon that could not read the trades: the slot stops.
    let o3 = order(0, 0, 930_000, 10_000_000, 3);
    b.arm.submit(&o3).unwrap();
    let mut done = PmEvt::about(EVT_DONE, o3.client_oid);
    done.b = -1;
    gw(&mut b, done);
    b.arm.on_idle();
    assert!(b.arm.halted());
}

#[test]
fn a_gap_and_a_deadline_send_open_orders_to_recon_and_a_late_trade_still_books() {
    let mut b = bench();
    let o = order(0, 0, 930_000, 10_000_000, 1);
    b.arm.submit(&o).unwrap();
    let _ = cmds(&mut b);
    let id = [2u8; 32];
    gw(&mut b, sent(o.client_oid, id));
    gw(&mut b, ack(o.client_oid, id, 1));
    assert!(b.user.try_push_ref(&PmEvt::about(EVT_GAP, 0)));
    b.arm.on_idle();
    let c = cmds(&mut b);
    assert_eq!(
        (c.len(), c[0].kind, c[0].client_oid),
        (1, CMD_RECON, o.client_oid)
    );
    // Recon found nothing; DONE retires it. A trade the venue sends later
    // still finds the kept row.
    let mut done = PmEvt::about(EVT_DONE, o.client_oid);
    done.b = 0;
    gw(&mut b, done);
    b.arm.on_idle();
    b.arm.on_idle();
    assert_eq!(
        b.arm.try_next_retired().unwrap().why,
        RETIRED_CANCELED_VENUE
    );
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id, 41, 930_000, 4_000_000, TS_MATCHED)));
    assert_eq!(fills(&mut b).len(), 1, "a late trade of a done order");
    assert!(!b.arm.halted());
    // The deadline: an order with no answer goes to recon.
    let o2 = order(0, 0, 930_000, 10_000_000, 2);
    b.arm.submit(&o2).unwrap();
    let _ = cmds(&mut b);
    gw(&mut b, sent(o2.client_oid, [1; 32]));
    let t0 = core_time::now_ns();
    b.arm.pump(t0 + DEADLINE_NS + SCAN_EVERY_NS);
    let c = cmds(&mut b);
    assert_eq!((c.len(), c[0].kind, c[0].order_id), (1, CMD_RECON, [1; 32]));
}

fn read(b: &mut Bench, balance: i64, legs: &[(u32, u8, i64)]) {
    let mut e = PmEvt::about(EVT_BALANCE, 0);
    e.b = balance;
    e.c = i64::MAX;
    gw(b, e);
    for &(inst, leg, shares) in legs {
        let mut p = PmEvt::about(EVT_POSITION, (u64::from(inst) << 8) | u64::from(leg));
        p.b = shares;
        gw(b, p);
    }
    gw(b, PmEvt::about(EVT_READ_DONE, 0));
}

#[test]
fn the_ledger_watches_values_at_cost_asks_a_won_leg_redeemed_and_drops_it_once_it_is() {
    let mut b = bench();
    let o = order(0, 0, 930_000, 100_000_000, 1);
    b.arm.submit(&o).unwrap();
    let _ = cmds(&mut b);
    let id = [3u8; 32];
    gw(&mut b, sent(o.client_oid, id));
    gw(&mut b, ack(o.client_oid, id, 1));
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id, 1, 930_000, 100_000_000, TS_MATCHED)));
    assert_eq!(fills(&mut b).len(), 1);
    let c = cmds(&mut b);
    assert_eq!(
        (c.len(), c[0].kind, c[0].instance, c[0].leg),
        (1, CMD_WATCH, inst(0), 0)
    );
    assert_eq!(
        b.arm.halt_signal().reconciled,
        0,
        "not read yet: the router refuses places"
    );
    // The first read: $1 000 pUSD and the leg as booked — seeded, anchored
    // at $1 093 (the leg at cost).
    read(&mut b, 1_000_000_000, &[(inst(0), 0, 100_000_000)]);
    b.arm.on_idle();
    let s = b.arm.halt_signal();
    assert_eq!(
        (
            s.reconciled,
            s.pnl_judged,
            s.pnl_delta_usd_1e6,
            s.recon_drift_usd_1e6
        ),
        (1, 1, 0, 0)
    );
    assert_eq!(b.arm.anchor_1e6(), Some(1_093_000_000));
    // The roll settles Up: the Up leg is won — still worth its cost until
    // it is redeemed, and the first read showing it asks the redemption,
    // once.
    b.arm.on_venue_event(&settle(0, PM_SETTLED_UP));
    read(&mut b, 1_000_000_000, &[(inst(0), 0, 100_000_000)]);
    b.arm.on_idle();
    assert_eq!(b.arm.halt_signal().pnl_delta_usd_1e6, 0);
    let c = cmds(&mut b);
    assert_eq!(
        (c.len(), c[0].kind, c[0].instance),
        (1, CMD_REDEEM, inst(0))
    );
    read(&mut b, 1_000_000_000, &[(inst(0), 0, 100_000_000)]);
    b.arm.on_idle();
    assert!(cmds(&mut b).is_empty(), "asked once");
    assert_eq!(b.arm.counters().redeem_asks, 1);
    // Redeemed: the venue holds none — the leg leaves, its watch stops,
    // and that read is not judged; the next one is: $1 100 against $1 093.
    read(&mut b, 1_100_000_000, &[(inst(0), 0, 0)]);
    b.arm.on_idle();
    assert_eq!(b.arm.counters().legs_redeemed, 1);
    assert_eq!(b.arm.counters().reads_disagreed, 1);
    let c = cmds(&mut b);
    assert_eq!((c[0].kind, c[0].leg), (CMD_WATCH, crate::gw::WATCH_STOP));
    read(&mut b, 1_100_000_000, &[]);
    b.arm.on_idle();
    assert_eq!(b.arm.halt_signal().pnl_delta_usd_1e6, 7_000_000);
    // A drift: a leg whose venue balance disagrees (the fill old enough).
    let o2 = order(0, 1, 900_000, 50_000_000, 2);
    b.arm.on_venue_event(&law(0, 1, 10_000));
    b.arm.submit(&o2).unwrap();
    let id2 = [4u8; 32];
    gw(&mut b, sent(o2.client_oid, id2));
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill(id2, 2, 900_000, 50_000_000, TS_MATCHED)));
    let _ = fills(&mut b);
    b.arm.legs[0].last_fill_ns = 0;
    b.arm.legs[1].last_fill_ns = 0;
    read(&mut b, 955_000_000, &[(inst(0), 1, 40_000_000)]);
    b.arm.on_idle();
    assert_eq!(
        b.arm.halt_signal().recon_drift_usd_1e6,
        9_000_000,
        "10 shares at $0.90"
    );
    // A failed read is not an agreement: the age grows from the last one.
    let mut e = PmEvt::about(EVT_BALANCE, 0);
    e.why = 1;
    gw(&mut b, e);
    gw(&mut b, PmEvt::about(EVT_READ_DONE, 0));
    b.arm.on_idle();
    assert_eq!(
        b.arm.counters().reads_disagreed,
        2,
        "the redemption's and this"
    );
}

/// A roll settling `v0` for family 0's window.
fn settle(f: usize, v0: i64) -> ChannelEvent {
    ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::InstrumentRoll,
        sym(f, 0),
        core_types::pack_roll_seq(inst(f), 60, pm_pair_of_window(window()), true),
        0,
        v0,
        0,
    )
}

/// PM15-6d: a leg its roll settled against leaves the ledger at once —
/// worth 0, nothing to redeem, its watch stopped; a roll settled unknown
/// keeps its leg at 0 and asks the redemption (the adapter pays what the
/// chain says).
#[test]
fn a_lost_leg_leaves_at_settlement_and_an_unknown_one_is_redeemed() {
    let mut b = bench();
    let o = order(0, 0, 930_000, 100_000_000, 1);
    b.arm.submit(&o).unwrap();
    let _ = cmds(&mut b);
    gw(&mut b, sent(o.client_oid, [3; 32]));
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill([3; 32], 1, 930_000, 100_000_000, TS_MATCHED)));
    let _ = fills(&mut b);
    let _ = cmds(&mut b);
    read(&mut b, 1_000_000_000, &[(inst(0), 0, 100_000_000)]);
    b.arm.on_idle();
    assert_eq!(b.arm.anchor_1e6(), Some(1_093_000_000));
    b.arm.on_venue_event(&settle(0, PM_SETTLED_DOWN));
    assert_eq!(b.arm.counters().legs_lost, 1);
    let c = cmds(&mut b);
    assert_eq!(
        (c.len(), c[0].kind, c[0].instance, c[0].leg),
        (1, CMD_WATCH, inst(0), crate::gw::WATCH_STOP)
    );
    read(&mut b, 1_000_000_000, &[(inst(0), 0, 100_000_000)]);
    b.arm.on_idle();
    assert_eq!(b.arm.halt_signal().pnl_delta_usd_1e6, -93_000_000);
    assert!(cmds(&mut b).is_empty(), "a loser is never redeemed");

    // Family 1, settled unknown: at 0, and asked redeemed.
    let o = order(1, 0, 900_000, 10_000_000, 2);
    b.arm.on_venue_event(&law(1, 0, 10_000));
    b.arm.submit(&o).unwrap();
    let _ = cmds(&mut b);
    gw(&mut b, sent(o.client_oid, [5; 32]));
    assert!(b
        .user
        .try_push_ref(&PmEvt::fill([5; 32], 2, 900_000, 10_000_000, TS_MATCHED)));
    let _ = fills(&mut b);
    let _ = cmds(&mut b);
    b.arm.on_venue_event(&settle(1, PM_SETTLED_UNKNOWN));
    assert_eq!(b.arm.counters().legs_lost, 1);
    read(&mut b, 991_000_000, &[(inst(1), 0, 10_000_000)]);
    b.arm.on_idle();
    let c = cmds(&mut b);
    assert_eq!(
        (c.len(), c[0].kind, c[0].instance),
        (1, CMD_REDEEM, inst(1))
    );
    assert_eq!(b.arm.halt_signal().pnl_delta_usd_1e6, -102_000_000);
}

/// PM15-8c: the tallies reach `/metrics`' block at the first pass and
/// then once a second — never on every pass of the engine loop.
#[test]
fn the_tallies_reach_the_metrics_block_once_a_second() {
    use crate::stats::{PmExecStats, ARM_NAMES, PUBLISH_EVERY_NS};
    let mut b = bench();
    let s = std::sync::Arc::new(PmExecStats::new());
    b.arm.set_stats(std::sync::Arc::clone(&s));
    assert_eq!(ARM_NAMES[0], "submitted");
    b.arm.submit(&order(0, 0, 930_000, 100_000_000, 1)).unwrap();
    b.arm.pump(1);
    assert_eq!(s.arm(0), 1);
    b.arm.submit(&order(0, 0, 930_000, 100_000_000, 2)).unwrap();
    b.arm.pump(2);
    assert_eq!(s.arm(0), 1, "not again inside the second");
    b.arm.pump(1 + PUBLISH_EVERY_NS);
    assert_eq!(s.arm(0), 2);
}

/// PM15-6f: legs restored at boot are valued at cost (a settled one as it
/// was left, its redemption not asked again by the arm); a leg pm-gw
/// expires is booked at 0, its redemption already asked.
#[test]
fn restored_legs_are_valued_at_cost_and_an_expired_one_at_zero() {
    use crate::held::{HeldLeg, HELD_OPEN, HELD_WON};
    let mut b = bench();
    let mut open = HeldLeg::EMPTY;
    open.instance = inst(0);
    open.state = HELD_OPEN;
    open.shares_1e6 = 100_000_000;
    open.cost_1e6 = 93_000_000;
    let mut won = open;
    won.instance = inst(1);
    won.state = HELD_WON;
    won.cost_1e6 = 50_000_000;
    b.arm.restore_legs(&[open, won]);
    assert_eq!(b.arm.counters().legs_restored, 2);
    read(
        &mut b,
        1_000_000_000,
        &[(inst(0), 0, 100_000_000), (inst(1), 0, 100_000_000)],
    );
    b.arm.on_idle();
    assert_eq!(
        b.arm.anchor_1e6(),
        Some(1_143_000_000),
        "pUSD and both legs at cost"
    );
    assert!(
        cmds(&mut b).is_empty(),
        "the won leg was asked before the restart"
    );
    gw(&mut b, PmEvt::about(EVT_EXPIRED, u64::from(inst(0)) << 8));
    read(
        &mut b,
        1_000_000_000,
        &[(inst(0), 0, 100_000_000), (inst(1), 0, 100_000_000)],
    );
    b.arm.on_idle();
    assert_eq!(b.arm.counters().legs_expired, 1);
    assert_eq!(b.arm.halt_signal().pnl_delta_usd_1e6, -93_000_000);
    assert!(cmds(&mut b).is_empty(), "its redemption is pm-gw's already");
}
