// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The member end to end on synthetic but venue-shaped records: a window's
//! roll, Chainlink reports on their seconds, a fast-venue mid, the legs'
//! touches, the take, its fill and the settlement.

use super::*;
use core_twaplock::late::{LATE_PHI_POINTS, LATE_PHI_STEP_1E6};
use core_types::pm_roll::{
    pm_instance_id, pm_mark_sym, pm_pair_of_window, pm_roll_family, pm_rolling_sym,
    pm_window_end_ns, pm_window_of_open, PM_LEG_DOWN, PM_LEG_UP, PM_TWAP_S,
};
use core_types::{make_symbol_id, pack_roll_seq, CancelReq, Fill};

const S: u64 = SEC_NS;
const OPEN: u64 = 1_792_216_800;
const BASE: i64 = 81_000_000_000;

struct TestCtx {
    now: NsTs,
    orders: Vec<Order>,
}

impl Ctx for TestCtx {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        self.orders.push(order);
        Ok(())
    }
    fn cancel(&mut self, _req: CancelReq) -> Result<(), SubmitErr> {
        Err(SubmitErr::Unsupported)
    }
    fn now_ns(&self) -> NsTs {
        self.now
    }
}

fn perp() -> SymbolId {
    make_symbol_id(VenueId::Hyperliquid, 1)
}

/// Gaussian-ish ramp: Φ rises linearly to 1 at z = 4.
fn lut() -> Box<LateLut> {
    let mut l = Box::new(LateLut::flat());
    let mut i = 0usize;
    while i < LATE_PHI_POINTS {
        let z = i as i64 * LATE_PHI_STEP_1E6;
        l.phi[i] = (500_000 + (z.min(4_000_000) / 8)) as u32;
        i += 1;
    }
    l
}

fn member() -> Pm15Strategy {
    let mut perp_sym = [SYMBOL_ID_NONE; PM15_MAX_FAMILIES];
    perp_sym[0] = perp();
    let p = Pm15Params {
        n_families: 1,
        perp_sym,
        ..Pm15Params::default()
    };
    let mut m = Pm15Strategy::new();
    m.configure(p, lut(), WallAnchor::new(0, 0)).expect("valid");
    m
}

fn mark(sec: u64, px: i64) -> ChannelEvent {
    ChannelEvent::new(
        sec * S + 1_300_000_000,
        VenueId::Polymarket,
        ChannelId::Mark,
        pm_mark_sym(0),
        sec,
        sec * 1_000,
        px,
        px * 1_000_000,
    )
}

fn roll(window: u32, settled: bool, v0: i64) -> ChannelEvent {
    let pair = pm_pair_of_window(window);
    ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::InstrumentRoll,
        pm_rolling_sym(0, pair, PM_LEG_UP),
        pack_roll_seq(
            pm_instance_id(window, 0).unwrap(),
            PM_TWAP_S,
            pm_roll_family(0, pair),
            settled,
        ),
        0,
        v0,
        pm_window_end_ns(window) as i64,
    )
}

fn law(window: u32, leg: u32, tick: i64) -> ChannelEvent {
    ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::OutcomeMeta,
        pm_rolling_sym(0, pm_pair_of_window(window), leg),
        u64::from(pm_instance_id(window, 0).unwrap()),
        0,
        tick,
        5_000_000,
    )
}

fn touch(window: u32, leg: u32, at: NsTs, bid: i64, ask: i64) -> Tick {
    Tick::new(
        at,
        VenueId::Polymarket,
        pm_rolling_sym(0, pm_pair_of_window(window), leg),
        1,
        Price::from_raw(bid),
        Qty::from_raw(500_000_000),
        Price::from_raw(ask),
        Qty::from_raw(500_000_000),
    )
}

fn perp_tick(at: NsTs, mid: i64) -> Tick {
    Tick::new(
        at,
        VenueId::Hyperliquid,
        perp(),
        1,
        Price::from_raw(mid - 500_000),
        Qty::from_raw(1),
        Price::from_raw(mid + 500_000),
        Qty::from_raw(1),
    )
}

/// One second of the tape: the perp just before, the report after.
fn second(m: &mut Pm15Strategy, c: &mut TestCtx, sec: u64, px: i64) {
    c.now = sec * S - 200_000_000;
    m.on_tick(&perp_tick(c.now, px), c);
    c.now = sec * S + 1_300_000_000;
    m.on_venue_event(&mark(sec, px), c);
}

/// A tape of `secs` reports ending at `last`, alternating ±`step`.
fn tape(m: &mut Pm15Strategy, c: &mut TestCtx, last: u64, secs: u64, base: i64, step: i64) {
    let mut s = last + 1 - secs;
    while s <= last {
        let px = base + if s % 2 == 0 { step } else { -step };
        second(m, c, s, px);
        s += 1;
    }
}

#[test]
fn configure_refuses_every_bad_frame() {
    let mut p = Pm15Params::default();
    let mut m = Pm15Strategy::new();
    assert_eq!(
        m.configure(p, lut(), WallAnchor::new(0, 0)),
        Err(Pm15ConfigErr::Families)
    );
    p.n_families = 1;
    assert_eq!(
        m.configure(p, lut(), WallAnchor::new(0, 0)),
        Err(Pm15ConfigErr::Perp)
    );
    p.perp_sym[0] = perp();
    let mut q = p;
    q.tau_max_ns = q.tau_min_ns;
    assert_eq!(
        m.configure(q, lut(), WallAnchor::new(0, 0)),
        Err(Pm15ConfigErr::Window)
    );
    let mut q = p;
    q.usd_market_1e6 = q.usd_order_1e6 - 1;
    assert_eq!(
        m.configure(q, lut(), WallAnchor::new(0, 0)),
        Err(Pm15ConfigErr::Bound)
    );
    let mut q = p;
    q.p_min_1e6 = 400_000;
    assert_eq!(
        m.configure(q, lut(), WallAnchor::new(0, 0)),
        Err(Pm15ConfigErr::Bound)
    );
    // PMJ J8 (R-PMJ-S24): the floor is 0 (none) or in 0.50..1.
    for bad in [1, 499_999, 1_000_000, -1] {
        let mut q = p;
        q.price_floor_1e6 = bad;
        assert_eq!(m.configure(q, lut(), WallAnchor::new(0, 0)), Err(Pm15ConfigErr::Bound), "{bad}");
    }
    let mut q = p;
    q.price_floor_1e6 = 0;
    assert!(Pm15Strategy::new().configure(q, lut(), WallAnchor::new(0, 0)).is_ok(), "0 = no floor");
    assert!(!m.is_configured());
    assert_eq!(m.configure(p, lut(), WallAnchor::new(0, 0)), Ok(()));
}

#[test]
fn a_decided_window_takes_once_fills_and_settles_into_the_session_pnl() {
    let w = pm_window_of_open(OPEN).unwrap();
    let end = OPEN + 900;
    let mut m = member();
    let mut c = TestCtx {
        now: 0,
        orders: Vec::new(),
    };
    // Twenty minutes of a quiet tape before the open, then the window opens.
    tape(&mut m, &mut c, OPEN - 2, 1_100, BASE, 2_000_000);
    c.now = OPEN * S;
    m.on_venue_event(&roll(w, false, 0), &mut c);
    m.on_venue_event(&law(w, PM_LEG_UP, 10_000), &mut c);
    m.on_venue_event(&law(w, PM_LEG_DOWN, 10_000), &mut c);
    assert_eq!(m.counters().rolls, 1);
    assert_eq!(
        m.market(0, pm_pair_of_window(w)).strike_src,
        0,
        "open − 1 not reported yet"
    );
    // The report of open − 1 arrives: the strike is the mean over [open−60, open−1].
    // (OPEN − 1 is odd: the tape's −step second.)
    second(&mut m, &mut c, OPEN - 1, BASE - 2_000_000);
    let mk = *m.market(0, pm_pair_of_window(w));
    assert_eq!(mk.strike_src, 1);
    assert_eq!(
        mk.strike_1e6, BASE,
        "the alternating tape averages to its base"
    );
    assert_eq!(m.counters().strikes_computed, 1);
    // The window runs; the price climbs $400 above the strike. The last
    // report outside the late window is end − 22 (it lands 1.3 s after its
    // second: τ = 20.7 s).
    tape(
        &mut m,
        &mut c,
        end - 22,
        900 - 21,
        BASE + 400_000_000,
        2_000_000,
    );
    // The books: Up asks 0.93, Down asks 0.08 — still outside the window.
    c.now = (end - 22) * S + 1_400_000_000;
    m.on_tick(&touch(w, PM_LEG_UP, c.now, 920_000, 930_000), &mut c);
    m.on_tick(&touch(w, PM_LEG_DOWN, c.now, 70_000, 80_000), &mut c);
    assert!(c.orders.is_empty(), "outside the window: τ ≥ 20 s");
    assert_eq!(m.counters().reprices, 0);
    // τ = 19.7 s: the first report second inside — persistence 1, no take.
    second(&mut m, &mut c, end - 21, BASE + 400_000_000);
    m.on_tick(&touch(w, PM_LEG_UP, c.now, 920_000, 930_000), &mut c);
    assert!(c.orders.is_empty(), "one second of belief is not two");
    assert!(m.counters().skip_persist > 0, "{:?}", m.counters());
    // PMJ J8 (R-PMJ-S24): the book drops under the 0.90 floor before the
    // next report second — two seconds of belief, and still no take: an
    // ask under the floor is no H90 entry, however far under the belief.
    m.on_tick(&touch(w, PM_LEG_UP, c.now, 840_000, 850_000), &mut c);
    let skip_price = m.counters().skip_price;
    second(&mut m, &mut c, end - 20, BASE + 400_000_000);
    assert!(c.orders.is_empty(), "an ask under the floor: no take");
    assert!(m.counters().skip_price > skip_price, "{:?}", m.counters());
    // The book comes back over the floor: the take.
    m.on_tick(&touch(w, PM_LEG_UP, c.now, 920_000, 930_000), &mut c);
    assert_eq!(c.orders.len(), 1, "{:?}", m.counters());
    let o = c.orders[0];
    assert_eq!(o.sym, pm_rolling_sym(0, pm_pair_of_window(w), PM_LEG_UP));
    assert_eq!(o.side, Side::Bid);
    assert_eq!(o.kind, ORDER_KIND_IOC);
    assert_eq!(o.venue, VenueId::Polymarket as u8);
    assert_eq!(o.px.raw() % 10_000, 0, "on the leg's 0.01 tick");
    assert!(
        o.px.raw() >= 930_000 && o.px.raw() <= 980_000,
        "limit {}",
        o.px.raw()
    );
    assert_eq!(
        o.client_oid & core_types::OID_INSTANCE_MASK,
        u64::from(pm_instance_id(w, 0).unwrap())
    );
    // $250 at the limit, whole hundredths of a share.
    let notional = o.px.raw() as i128 * o.qty.raw() as i128 / 1_000_000;
    assert!(
        notional <= 250_000_000 && notional > 249_000_000,
        "notional {notional}"
    );
    // In flight: no second order.
    second(&mut m, &mut c, end - 19, BASE + 400_000_000);
    m.on_tick(&touch(w, PM_LEG_UP, c.now, 920_000, 930_000), &mut c);
    assert_eq!(c.orders.len(), 1);
    // It fills at the ask, all of it.
    let fill = Fill::new(
        c.now,
        o.sym,
        Side::Bid,
        Price::from_raw(930_000),
        o.qty,
        o.client_oid,
    );
    m.on_fill(&fill, &mut c);
    let mk = *m.market(0, pm_pair_of_window(w));
    assert_eq!(mk.pos_1e6[0], o.qty.raw());
    assert_eq!(mk.pending.oid, 0);
    assert_eq!(m.counters().takes_filled, 1);
    // The fill came in under the limit: the room it left is topped up,
    // and no more than that — the $250 market cap holds at the fill price.
    second(&mut m, &mut c, end - 18, BASE + 400_000_000);
    m.on_tick(&touch(w, PM_LEG_UP, c.now, 920_000, 930_000), &mut c);
    assert_eq!(c.orders.len(), 2);
    let top = c.orders[1];
    let cost0 = 930_000i128 * o.qty.raw() as i128 / 1_000_000;
    let top_notional = top.px.raw() as i128 * top.qty.raw() as i128 / 1_000_000;
    assert!(
        cost0 + top_notional <= 250_000_000,
        "{cost0} + {top_notional}"
    );
    // The venue resolves Up: the leg pays $1 a share, less cost and fee.
    m.on_venue_event(&roll(w, true, PM_SETTLED_UP), &mut c);
    let q = o.qty.raw() as i128;
    let cost = 930_000i128 * q / 1_000_000;
    let fee = 70_000i128 * 930_000 * 70_000 * q / 1_000_000_000_000_000_000;
    let ct = m.counters();
    assert_eq!(ct.pnl_1e6 as i128, q - cost - fee);
    assert_eq!(
        (ct.settled_won, ct.rolls_settled, ct.markets_live),
        (1, 1, 0)
    );
    assert!(ct.pnl_1e6 > 0);
}

#[test]
fn the_session_loss_bound_stops_entries_and_other_venues_rolls_are_not_ours() {
    let w = pm_window_of_open(OPEN).unwrap();
    let mut m = member();
    let mut c = TestCtx {
        now: OPEN * S,
        orders: Vec::new(),
    };
    // A Hyperliquid roll on the same channel binds nothing.
    let mut hl = roll(w, false, 0);
    hl.venue = VenueId::Hyperliquid as u8;
    m.on_venue_event(&hl, &mut c);
    assert_eq!(m.counters().rolls, 0);
    // A Polymarket roll on a slot of a family this member does not carry.
    let mut other = roll(w, false, 0);
    other.sym = pm_rolling_sym(3, pm_pair_of_window(w), PM_LEG_UP);
    m.on_venue_event(&other, &mut c);
    assert_eq!(m.counters().rolls_refused, 1);
    // Bind, buy at a loss, settle Down: the bound is spent.
    m.on_venue_event(&roll(w, false, 0), &mut c);
    assert_eq!(m.market(0, pm_pair_of_window(w)).strike_src, 0);
    let mk = &mut m.markets[0][pm_pair_of_window(w)];
    mk.pos_1e6[0] = 600_000_000;
    mk.cost_1e6[0] = 560_000_000;
    m.on_venue_event(&roll(w, true, PM_SETTLED_DOWN), &mut c);
    let ct = m.counters();
    assert_eq!(ct.pnl_1e6, -560_000_000);
    assert_eq!((ct.settled_lost, ct.halted), (1, 1));
}

#[test]
fn an_unfilled_ioc_gives_its_room_back_at_its_deadline() {
    let mut m = member();
    let mut c = TestCtx {
        now: 0,
        orders: Vec::new(),
    };
    let w = pm_window_of_open(OPEN).unwrap();
    m.on_venue_event(&roll(w, false, 0), &mut c);
    let pair = pm_pair_of_window(w);
    m.markets[0][pair].pending = Pending {
        oid: 7,
        deadline_ns: 10 * S,
        px_1e6: 950_000,
        qty_1e6: 100_000_000,
        filled_1e6: 0,
        leg: 0,
        _pad: [0; 7],
    };
    m.markets[0][pair].committed_1e6 = 95_000_000;
    m.on_timer(9 * S, &mut c);
    assert_eq!(m.market(0, pair).pending.oid, 7);
    m.on_timer(10 * S, &mut c);
    assert_eq!(m.market(0, pair).pending.oid, 0);
    assert_eq!(m.market(0, pair).committed_1e6, 0);
    assert_eq!(m.counters().takes_unfilled, 1);
    // A fill nobody owns is counted, not booked.
    m.on_fill(
        &Fill::new(
            0,
            pm_rolling_sym(0, pair, PM_LEG_UP),
            Side::Bid,
            Price::from_raw(1),
            Qty::from_raw(1),
            99,
        ),
        &mut c,
    );
    assert_eq!(m.counters().unknown_fills, 1);
}

/// PM15-5c: each book gate names its reason, and a quiet book — no change
/// and no re-assertion for 7.9 s — is still a book to take from.
#[test]
fn the_book_gates_name_their_reason_and_a_quiet_book_still_takes() {
    let mut m = member();
    let now = 100 * S;
    let mut c = TestCtx {
        now,
        orders: Vec::new(),
    };
    let w = pm_window_of_open(OPEN).unwrap();
    let pair = pm_pair_of_window(w);
    m.on_venue_event(&roll(w, false, 0), &mut c);
    m.take(0, pair, 0, 990_000, now, &mut c);
    assert_eq!(m.counters().skip_law, 1, "no order law yet");
    m.on_venue_event(&law(w, PM_LEG_UP, 10_000), &mut c);
    m.take(0, pair, 0, 990_000, now, &mut c);
    assert_eq!(m.counters().skip_book, 1, "no ask yet");
    m.on_tick(
        &touch(w, PM_LEG_UP, now - 7_900_000_000, 920_000, 930_000),
        &mut c,
    );
    m.take(0, pair, 0, 990_000, now, &mut c);
    assert_eq!(c.orders.len(), 1, "{:?}", m.counters());
    m.markets[0][pair].pending = Pending::default();
    m.take(0, pair, 0, 990_000, now + 100_000_000, &mut c);
    assert_eq!(m.counters().skip_book_old, 1, "8 s without a word");
    // The socket went down: its stale tick takes the book away.
    let mut down = touch(w, PM_LEG_UP, now, 0, 0);
    down.flags = core_types::TICK_FLAG_STALE;
    m.on_tick(&down, &mut c);
    m.take(0, pair, 0, 990_000, now, &mut c);
    assert_eq!(m.counters().skip_book, 2);
    assert_eq!(c.orders.len(), 1);
}

/// PM15-6b: a live fill the taker hold delayed past the pending's
/// write-off still books — by its client id's family, instance and leg.
#[test]
fn a_late_fill_books_by_its_client_id() {
    let mut m = member();
    let now = 100 * S;
    let mut c = TestCtx {
        now,
        orders: Vec::new(),
    };
    let w = pm_window_of_open(OPEN).unwrap();
    let pair = pm_pair_of_window(w);
    m.on_venue_event(&roll(w, false, 0), &mut c);
    m.on_venue_event(&law(w, PM_LEG_DOWN, 10_000), &mut c);
    m.on_tick(&touch(w, PM_LEG_DOWN, now, 920_000, 930_000), &mut c);
    m.take(0, pair, 1, 990_000, now, &mut c);
    let o = c.orders[0];
    m.on_timer(now + 10 * S, &mut c);
    assert_eq!(m.market(0, pair).pending.oid, 0, "written off");
    m.on_fill(
        &Fill::new(now, o.sym, Side::Bid, Price::from_raw(930_000), Qty::from_raw(50_000_000), o.client_oid),
        &mut c,
    );
    let mk = *m.market(0, pair);
    assert_eq!((mk.pos_1e6[1], mk.cost_1e6[1]), (50_000_000, 46_500_000));
    assert_eq!((m.counters().late_fills, m.counters().unknown_fills), (1, 0));
    // Another instance's: unknown.
    m.on_fill(
        &Fill::new(now, o.sym, Side::Bid, Price::from_raw(930_000), Qty::from_raw(1), o.client_oid + 8),
        &mut c,
    );
    assert_eq!(m.counters().unknown_fills, 1);
}

/// PMJ J1b (R-PMJ-3): the 5-minute families are slot 11's. Their rolls,
/// order laws, marks and touches reach slot 9 on the same lane once the
/// 5-minute ingress runs; none of them may change an order, a market or a
/// counter of this member.
#[test]
fn five_minute_records_leave_slot_9_untouched() {
    use core_types::pm_roll::{
        pm5_instance_id, pm5_mark_sym, pm5_pair_of_window, pm5_roll_family, pm5_rolling_sym,
        pm5_window_end_ns, pm5_window_of_open,
    };
    let w = pm_window_of_open(OPEN).unwrap();
    let mut m = member();
    let mut c = TestCtx {
        now: OPEN * S,
        orders: Vec::new(),
    };
    m.on_venue_event(&roll(w, false, 0), &mut c);
    let before = m.counters();
    let w5 = pm5_window_of_open(OPEN).unwrap();
    let pair = pm5_pair_of_window(w5);
    let inst = pm5_instance_id(w5, 0).unwrap();
    let created = ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::InstrumentRoll,
        pm5_rolling_sym(0, pair, PM_LEG_UP),
        pack_roll_seq(inst, PM_TWAP_S, pm5_roll_family(0, pair), false),
        0,
        0,
        pm5_window_end_ns(w5) as i64,
    );
    let settled = ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::InstrumentRoll,
        pm5_rolling_sym(0, pair, PM_LEG_UP),
        pack_roll_seq(inst, PM_TWAP_S, pm5_roll_family(0, pair), true),
        0,
        PM_SETTLED_DOWN,
        pm5_window_end_ns(w5) as i64,
    );
    let law5 = ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::OutcomeMeta,
        pm5_rolling_sym(0, pair, PM_LEG_DOWN),
        u64::from(inst),
        0,
        10_000,
        5_000_000,
    );
    let mark5 = ChannelEvent::new(
        OPEN * S,
        VenueId::Polymarket,
        ChannelId::Mark,
        pm5_mark_sym(0),
        OPEN,
        OPEN * 1_000,
        BASE,
        BASE * 1_000_000,
    );
    m.on_venue_event(&created, &mut c);
    m.on_venue_event(&law5, &mut c);
    m.on_venue_event(&mark5, &mut c);
    let t5 = Tick::new(
        OPEN * S,
        VenueId::Polymarket,
        pm5_rolling_sym(0, pair, PM_LEG_UP),
        1,
        Price::from_raw(950_000),
        Qty::from_raw(500_000_000),
        Price::from_raw(960_000),
        Qty::from_raw(500_000_000),
    );
    m.on_tick(&t5, &mut c);
    m.on_venue_event(&settled, &mut c);
    assert_eq!(m.counters(), before, "a 5 m record changed slot 9's counters");
    assert!(c.orders.is_empty(), "a 5 m record made slot 9 order");
    assert_eq!(m.market(0, pm_pair_of_window(w)).instance, pm_instance_id(w, 0).unwrap());
}
