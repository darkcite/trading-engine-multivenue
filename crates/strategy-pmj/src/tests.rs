// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The member end to end on synthetic but venue-shaped 5-minute records: a
//! window's roll, Chainlink reports on their seconds, a fast-venue mid, the
//! legs' touches, the take on Jupiter, its fill and the settlement — and
//! the PMJ keys: the price floor, the keeper's lead, the $5 deposit, the
//! doubled fee, the family split.

use super::*;
use core_twaplock::late::{LATE_PHI_POINTS, LATE_PHI_STEP_1E6};
use core_types::pm_roll::{
    pm5_instance_id, pm5_mark_sym, pm5_pair_of_window, pm5_window_end_ns, pm5_window_of_open,
    PM_LEG_DOWN, PM_LEG_UP, PM_TWAP_S,
};
use core_types::{make_symbol_id, pack_roll_seq, CancelReq, Fill};

const S: u64 = SEC_NS;
/// A 5-minute (and 15-minute) boundary.
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

fn params() -> PmjParams {
    let mut perp_sym = [SYMBOL_ID_NONE; PMJ_MAX_FAMILIES];
    perp_sym[0] = perp();
    PmjParams {
        n_families: 1,
        perp_sym,
        ..PmjParams::default()
    }
}

fn member_with(p: PmjParams) -> PmjStrategy {
    let mut m = PmjStrategy::new();
    m.configure(p, lut(), WallAnchor::new(0, 0)).expect("valid");
    m
}

fn mark(sec: u64, px: i64) -> ChannelEvent {
    ChannelEvent::new(
        sec * S + 1_300_000_000,
        VenueId::Polymarket,
        ChannelId::Mark,
        pm5_mark_sym(0),
        sec,
        sec * 1_000,
        px,
        px * 1_000_000,
    )
}

fn roll(window: u32, settled: bool, v0: i64) -> ChannelEvent {
    let pair = pm5_pair_of_window(window);
    ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::InstrumentRoll,
        pm5_rolling_sym(0, pair, PM_LEG_UP),
        pack_roll_seq(pm5_instance_id(window, 0).unwrap(), PM_TWAP_S, pm5_roll_family(0, pair), settled),
        0,
        v0,
        pm5_window_end_ns(window) as i64,
    )
}

fn law(window: u32, leg: u32, tick: i64) -> ChannelEvent {
    ChannelEvent::new(
        0,
        VenueId::Polymarket,
        ChannelId::OutcomeMeta,
        pm5_rolling_sym(0, pm5_pair_of_window(window), leg),
        u64::from(pm5_instance_id(window, 0).unwrap()),
        0,
        tick,
        5_000_000,
    )
}

fn touch(window: u32, leg: u32, at: NsTs, bid: i64, ask: i64) -> Tick {
    Tick::new(
        at,
        VenueId::Polymarket,
        pm5_rolling_sym(0, pm5_pair_of_window(window), leg),
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
fn second(m: &mut PmjStrategy, c: &mut TestCtx, sec: u64, px: i64) {
    c.now = sec * S - 200_000_000;
    m.on_tick(&perp_tick(c.now, px), c);
    c.now = sec * S + 1_300_000_000;
    m.on_venue_event(&mark(sec, px), c);
}

/// A tape of `secs` reports ending at `last`, alternating ±`step`.
fn tape(m: &mut PmjStrategy, c: &mut TestCtx, last: u64, secs: u64, base: i64, step: i64) {
    let mut s = last + 1 - secs;
    while s <= last {
        let px = base + if s % 2 == 0 { step } else { -step };
        second(m, c, s, px);
        s += 1;
    }
}

/// A window that opens at OPEN, its strike known, its price $400 above the
/// strike until `end − 22`: the late window is next. Returns (window, end).
fn decided(m: &mut PmjStrategy, c: &mut TestCtx) -> (u32, u64) {
    let w = pm5_window_of_open(OPEN).unwrap();
    let end = OPEN + 300;
    tape(m, c, OPEN - 2, 1_100, BASE, 2_000_000);
    c.now = OPEN * S;
    m.on_venue_event(&roll(w, false, 0), c);
    m.on_venue_event(&law(w, PM_LEG_UP, 10_000), c);
    m.on_venue_event(&law(w, PM_LEG_DOWN, 10_000), c);
    second(m, c, OPEN - 1, BASE - 2_000_000);
    tape(m, c, end - 22, 300 - 21, BASE + 400_000_000, 2_000_000);
    (w, end)
}

/// The two report seconds that make the persistence, each followed by the
/// legs' touches (`up_ask` for the favourite).
fn late_seconds(m: &mut PmjStrategy, c: &mut TestCtx, w: u32, end: u64, up_ask: i64) {
    let mut s = end - 21;
    while s <= end - 20 {
        second(m, c, s, BASE + 400_000_000);
        m.on_tick(&touch(w, PM_LEG_UP, c.now, up_ask - 10_000, up_ask), c);
        m.on_tick(&touch(w, PM_LEG_DOWN, c.now, 70_000, 80_000), c);
        s += 1;
    }
}

#[test]
fn configure_refuses_every_bad_frame() {
    let mut m = PmjStrategy::new();
    let w = |m: &mut PmjStrategy, p: PmjParams| m.configure(p, lut(), WallAnchor::new(0, 0));
    assert_eq!(w(&mut m, PmjParams::default()), Err(PmjConfigErr::Families));
    let mut p = params();
    p.perp_sym[0] = SYMBOL_ID_NONE;
    assert_eq!(w(&mut m, p), Err(PmjConfigErr::Perp));
    let mut p = params();
    p.tau_max_ns = p.tau_min_ns;
    assert_eq!(w(&mut m, p), Err(PmjConfigErr::Window));
    for bad in [
        PmjParams { deposit_usd_1e6: MIN_DEPOSIT_1E6 - 1, ..params() },
        PmjParams { usd_market_1e6: 4_999_999, ..params() },
        PmjParams { price_floor_1e6: 1_000_000, ..params() },
        PmjParams { price_floor_1e6: 400_000, ..params() },
        PmjParams { min_tau_ns: 20_000_000_000, ..params() },
        PmjParams { p_min_1e6: 400_000, ..params() },
    ] {
        assert_eq!(w(&mut m, bad), Err(PmjConfigErr::Bound), "{bad:?}");
    }
    assert!(!m.is_configured());
    assert_eq!(w(&mut m, params()), Ok(()));
}

#[test]
fn a_decided_window_takes_on_jupiter_at_five_dollars_and_settles_with_the_doubled_fee() {
    let mut m = member_with(params());
    let mut c = TestCtx { now: 0, orders: Vec::new() };
    let (w, end) = decided(&mut m, &mut c);
    assert_eq!(m.counters().rolls, 1);
    assert_eq!(m.market(0, pm5_pair_of_window(w)).strike_1e6, BASE);
    assert!(c.orders.is_empty(), "outside the late window");
    late_seconds(&mut m, &mut c, w, end, 930_000);
    assert_eq!(c.orders.len(), 1, "{:?}", m.counters());
    let o = c.orders[0];
    assert_eq!(o.sym, pm5_rolling_sym(0, pm5_pair_of_window(w), PM_LEG_UP));
    assert_eq!(o.venue, VenueId::Jupiter as u8, "the order addresses Jupiter");
    assert_eq!((o.side, o.kind), (Side::Bid, ORDER_KIND_IOC));
    assert_eq!(o.px.raw() % 10_000, 0, "on the leg's tick");
    assert!(o.px.raw() >= 930_000 && o.px.raw() < 1_000_000, "limit {}", o.px.raw());
    assert_eq!(o.client_oid & core_types::OID_INSTANCE_MASK, u64::from(pm5_instance_id(w, 0).unwrap()));
    // At least the venue's $5, at most one share step over it.
    let notional = i128::from(o.px.raw()) * i128::from(o.qty.raw()) / 1_000_000;
    assert!(notional >= 5_000_000, "notional {notional}");
    assert!(notional < 5_000_000 + i128::from(o.px.raw()) / 100, "notional {notional}");
    assert_eq!(o.qty.raw() % SHARE_STEP_1E6, 0);
    // It fills at the ask; the venue resolves Up.
    let fill = Fill::new(c.now, o.sym, Side::Bid, Price::from_raw(930_000), o.qty, o.client_oid);
    m.on_fill(&fill, &mut c);
    m.on_venue_event(&roll(w, true, PM_SETTLED_UP), &mut c);
    let q = i128::from(o.qty.raw());
    let cost = 930_000i128 * q / 1_000_000;
    // 2 × 0.07 · p · (1 − p) per share.
    let fee = 140_000i128 * 930_000 * 70_000 * q / 1_000_000_000_000_000_000;
    let ct = m.counters();
    assert_eq!(i128::from(ct.fees_1e6), fee);
    assert_eq!(i128::from(ct.pnl_1e6), q - cost - fee);
    assert_eq!((ct.settled_won, ct.rolls_settled, ct.markets_live), (1, 1, 0));
}

#[test]
fn the_price_floor_refuses_a_favourite_the_book_prices_under_it() {
    let mut m = member_with(params());
    let mut c = TestCtx { now: 0, orders: Vec::new() };
    let (w, end) = decided(&mut m, &mut c);
    late_seconds(&mut m, &mut c, w, end, 890_000);
    assert!(c.orders.is_empty(), "an ask of 0.89 is under the 0.90 floor");
    assert!(m.counters().skip_price > 0);
}

#[test]
fn the_keepers_lead_moves_the_window_floor() {
    // min_tau 19.5 s: the two report seconds (τ ≈ 19.7, 18.7) never both
    // count — the second one is inside the keeper's lead.
    let p = PmjParams { min_tau_ns: 19_500_000_000, ..params() };
    let mut m = member_with(p);
    let mut c = TestCtx { now: 0, orders: Vec::new() };
    let (w, end) = decided(&mut m, &mut c);
    late_seconds(&mut m, &mut c, w, end, 930_000);
    assert!(c.orders.is_empty(), "{:?}", m.counters());
}

#[test]
fn a_market_cap_that_cannot_hold_five_dollars_at_the_limit_sends_nothing() {
    // $5 of room: the share rounding that reaches $5 overshoots it.
    let p = PmjParams { usd_market_1e6: 5_000_000, ..params() };
    let mut m = member_with(p);
    let mut c = TestCtx { now: 0, orders: Vec::new() };
    let (w, end) = decided(&mut m, &mut c);
    late_seconds(&mut m, &mut c, w, end, 930_000);
    // At any limit in [0.93, 0.98], whole hundredths of a share land under
    // $5 or one step over it — never on it: nothing is sent.
    assert!(c.orders.is_empty(), "{:?}", c.orders);
    assert!(m.counters().skip_cap > 0);
}

#[test]
fn fifteen_minute_records_leave_slot_11_untouched() {
    use core_types::pm_roll::{
        pm_instance_id, pm_mark_sym, pm_pair_of_window, pm_roll_family, pm_rolling_sym, pm_window_end_ns,
        pm_window_of_open,
    };
    let mut m = member_with(params());
    let mut c = TestCtx { now: OPEN * S, orders: Vec::new() };
    let before = m.counters();
    let w = pm_window_of_open(OPEN).unwrap();
    let pair = pm_pair_of_window(w);
    let inst = pm_instance_id(w, 0).unwrap();
    for (ch, sym, seq, v0, v1) in [
        (ChannelId::InstrumentRoll, pm_rolling_sym(0, pair, PM_LEG_UP), pack_roll_seq(inst, PM_TWAP_S, pm_roll_family(0, pair), false), 0, pm_window_end_ns(w) as i64),
        (ChannelId::OutcomeMeta, pm_rolling_sym(0, pair, PM_LEG_DOWN), u64::from(inst), 10_000, 5_000_000),
        (ChannelId::Mark, pm_mark_sym(0), OPEN, BASE, BASE * 1_000_000),
        (ChannelId::InstrumentRoll, pm_rolling_sym(0, pair, PM_LEG_UP), pack_roll_seq(inst, PM_TWAP_S, pm_roll_family(0, pair), true), PM_SETTLED_DOWN, pm_window_end_ns(w) as i64),
    ] {
        m.on_venue_event(&ChannelEvent::new(OPEN * S, VenueId::Polymarket, ch, sym, seq, 0, v0, v1), &mut c);
    }
    let t = Tick::new(
        OPEN * S,
        VenueId::Polymarket,
        pm_rolling_sym(0, pair, PM_LEG_UP),
        1,
        Price::from_raw(950_000),
        Qty::from_raw(500_000_000),
        Price::from_raw(960_000),
        Qty::from_raw(500_000_000),
    );
    m.on_tick(&t, &mut c);
    assert_eq!(m.counters(), before, "a 15 m record changed slot 11's counters");
    assert!(c.orders.is_empty());
}

#[test]
fn the_member_names_itself_pmj_and_reports_on_its_own_family() {
    let m = member_with(params());
    assert_eq!(m.strategy_kind(), "pmj");
    assert_eq!(m.pm15_counters(), Pm15Counters::default(), "slot 9's family stays slot 9's");
    assert_eq!(m.pmj_counters(), m.counters());
}
