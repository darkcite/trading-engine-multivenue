// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The member's tests: every public fn a happy path and a failure mode,
//! the decision against a real V3 pool walked by `core_amm`, and the
//! hedge / inventory lifecycle. Test code: allocation is fine.

use super::*;
use core_amm::payload::{
    encode_gap, encode_head, encode_liquidity, encode_snapshot, encode_state, encode_tick,
    FAMILY_ALGEBRA, FAMILY_V3,
};
use core_amm::{price_1e18_from_sqrt, sqrt_at_tick};
use core_types::{make_symbol_id, LatencyClass, TICK_FLAG_STALE};

const POOL: SymbolId = make_symbol_id(VenueId::HyperEvm, 1);
const POOL2: SymbolId = make_symbol_id(VenueId::HyperEvm, 2);
const PERP: SymbolId = make_symbol_id(VenueId::Hyperliquid, 5);
const SPOT: SymbolId = make_symbol_id(VenueId::Hyperliquid, 10_107);
/// ≈ 97.6 USDC per WHYPE at 18 / 6 decimals.
const TICK0: i32 = -230_543;
const LO: i32 = -240_000;
const HI: i32 = -220_000;
const L: u128 = 50_000_000_000_000_000_000;
const T0: u64 = 1_000_000_000_000;
const WALL0: u64 = 1_789_192_800 * 1_000_000_000;

struct RecCtx {
    orders: Vec<Order>,
    full: bool,
    now: NsTs,
}

impl Ctx for RecCtx {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        if self.full {
            return Err(SubmitErr::RingFull);
        }
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
        full: false,
        now: T0,
    }
}

fn params() -> HyparbParams {
    let mut p = HyparbParams::EMPTY;
    p.coins[0] = CoinParams {
        perp_sym: PERP,
        spot_sym: SPOT,
        lot_1e6: 10_000,
        min_notional_usd_1e6: 10_000_000,
    };
    p.n_coins = 1;
    p.pools[0] = PoolParams {
        sym: POOL,
        coin0: 0,
        coin1: COIN_USD,
        trade: true,
        max_notional_usd_1e6: 1_000_000_000,
    };
    p.n_pools = 1;
    p.lag_ns = 500_000_000;
    p.basis_window_ns = 60_000_000_000;
    p.depth_cap_enabled = true;
    p.gas_p50_usd_1e6 = 10_000;
    p.gas_p99_usd_1e6 = 3_910_000;
    p.max_order_usd_1e6 = 1_000_000_000;
    p.cap_day_usd_1e6 = 10_000_000_000;
    p.min_net_bps_1e6 = 5_000_000;
    p.inventory_cap_usd_1e6 = 5_000_000_000;
    p.hedge_switch_hysteresis_bps_1e6 = 1_000_000;
    p.perp_taker_bps_1e6 = 4_500_000;
    p.spot_taker_bps_1e6 = 7_000_000;
    p.funding_window_ns = 3_600_000_000_000;
    p.cooldown_ns = 1_000_000_000;
    p
}

fn member_with(p: HyparbParams) -> HyparbStrategy {
    let mut m = HyparbStrategy::new();
    m.configure(p, core_time::WallAnchor::new(0, WALL0))
        .expect("configure");
    let mut c = ctx();
    m.on_start(&mut c).expect("on_start");
    m
}

fn member() -> HyparbStrategy {
    member_with(params())
}

fn sig(sym: SymbolId, payload: [u8; 40]) -> Signal {
    Signal::new(
        T0,
        sym,
        LatencyClass::Warm,
        SignalSource::HyperEvm as u8,
        payload,
    )
}

/// A full-coverage range position: `+L` at `LO`, `−L` at `HI`.
fn snapshot(m: &mut HyparbStrategy, c: &mut RecCtx, sym: SymbolId, nodes: u16, ticks: usize) {
    let (lo, hi) = sqrt_at_tick(TICK0);
    m.on_signal(
        &sig(
            sym,
            encode_snapshot(7, FAMILY_V3, LO, HI, nodes, 500, 10, 18, 6).unwrap(),
        ),
        c,
    );
    let t = [
        encode_tick(LO, L as i128, L).unwrap(),
        encode_tick(HI, -(L as i128), L).unwrap(),
    ];
    let mut i = 0;
    while i < ticks {
        m.on_signal(&sig(sym, t[i]), c);
        i += 1;
    }
    m.on_signal(&sig(sym, encode_state(TICK0, lo, hi, L, true).unwrap()), c);
}

/// The pool's mid, USD per WHYPE × 1e6.
fn pool_mid() -> i64 {
    let (lo, hi) = sqrt_at_tick(TICK0);
    (price_1e18_from_sqrt(lo, hi, 18, 6) / 1_000_000_000_000) as i64
}

fn bbo(sym: SymbolId, bid: i64, ask: i64, qty: i64, now: NsTs) -> Tick {
    Tick::new(
        now,
        VenueId::Hyperliquid,
        sym,
        1,
        Price::from_raw(bid),
        Qty::from_raw(qty),
        Price::from_raw(ask),
        Qty::from_raw(qty),
    )
}

/// A perp touch `bps` away from the pool's mid (1 bp wide).
fn perp_at(m: &mut HyparbStrategy, c: &mut RecCtx, bps: i64) {
    let mid = pool_mid() * (10_000 + bps) / 10_000;
    m.on_tick(
        &bbo(
            PERP,
            mid - mid / 20_000,
            mid + mid / 20_000,
            100_000_000,
            c.now,
        ),
        c,
    );
}

/// The member after a perp 1 % above the pool, then the snapshot: one
/// AMM buy in flight.
fn armed() -> (HyparbStrategy, RecCtx) {
    let mut m = member();
    let mut c = ctx();
    perp_at(&mut m, &mut c, 100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    assert_eq!(c.orders.len(), 1, "{:?}", m.counters());
    (m, c)
}

// ---------------------------------------------------------------
// Params and configuration
// ---------------------------------------------------------------

#[test]
fn validate_accepts_the_reference_shape() {
    assert_eq!(params().validate(), Ok(()));
}

#[test]
fn validate_refuses_every_malformed_shape() {
    let mut cases: Vec<HyparbParams> = Vec::new();
    cases.push(HyparbParams::EMPTY);
    let mut p = params();
    p.n_coins = HYPARB_MAX_COINS + 1;
    cases.push(p.clone());
    let mut p = params();
    p.coins[0].perp_sym = make_symbol_id(VenueId::Binance, 1);
    cases.push(p.clone());
    let mut p = params();
    p.coins[0].perp_sym = SYMBOL_ID_NONE;
    p.coins[0].spot_sym = SYMBOL_ID_NONE;
    cases.push(p.clone());
    let mut p = params();
    p.coins[0].lot_1e6 = 0;
    cases.push(p.clone());
    let mut p = params();
    p.pools[0].sym = PERP;
    cases.push(p.clone());
    let mut p = params();
    p.pools[0].coin0 = 3;
    cases.push(p.clone());
    let mut p = params();
    p.pools[0].coin0 = COIN_USD;
    cases.push(p.clone());
    let mut p = params();
    p.pools[0].max_notional_usd_1e6 = 0;
    cases.push(p.clone());
    let mut p = params();
    p.gas_p99_usd_1e6 = p.gas_p50_usd_1e6 - 1;
    cases.push(p.clone());
    let mut p = params();
    p.perp_taker_bps_1e6 = ONE_BPS_1E6;
    cases.push(p.clone());
    let mut p = params();
    p.lag_ns = 0;
    cases.push(p.clone());
    let mut p = params();
    p.basis_window_ns = 0;
    cases.push(p.clone());
    let mut p = params();
    p.gas_coin = 1; // one coin configured
    cases.push(p);
    let mut i = 0;
    while i < cases.len() {
        assert!(cases[i].validate().is_err(), "case {i} must be refused");
        i += 1;
    }
}

#[test]
fn configure_refuses_invalid_params_and_a_pool_named_twice() {
    let mut m = HyparbStrategy::new();
    let a = core_time::WallAnchor::new(0, WALL0);
    assert!(m.configure(HyparbParams::EMPTY, a).is_err());
    let mut p = params();
    p.pools[1] = p.pools[0];
    p.n_pools = 2;
    assert!(matches!(
        HyparbStrategy::new().configure(p, a),
        Err(StrategyError::Config(_))
    ));
    assert!(m.configure(params(), a).is_ok());
}

#[test]
fn an_unconfigured_member_refuses_to_start_and_does_nothing() {
    let mut m = HyparbStrategy::new();
    let mut c = ctx();
    assert!(matches!(m.on_start(&mut c), Err(StrategyError::Config(_))));
    assert_eq!(m.timer_period_ns(), u64::MAX);
    m.on_tick(&bbo(PERP, 1, 2, 1, T0), &mut c);
    m.on_signal(&sig(POOL, encode_head(1, 1, 1).unwrap()), &mut c);
    m.on_timer(T0, &mut c);
    m.on_fill(
        &Fill::new(T0, POOL, Side::Bid, Price::from_raw(1), Qty::from_raw(1), 1),
        &mut c,
    );
    assert!(c.orders.is_empty());
    assert_eq!(m.counters(), HyparbCounters::default());
    let mut out = [HyparbPoolView::default(); 2];
    assert_eq!(m.hyparb_pools_view(&mut out), 0);
    assert_eq!(m.strategy_kind(), "hyparb");
    assert_eq!(m.inventory_1e6(0), None);
}

#[test]
fn a_configured_member_starts_and_runs_a_one_second_timer() {
    let m = member();
    assert_eq!(m.timer_period_ns(), 1_000_000_000);
    assert!(!m.is_halted());
    assert_eq!(m.inventory_1e6(0), Some(0));
    assert_eq!(m.inventory_1e6(1), None);
    assert_eq!(m.book().head_block(), 0);
}

// ---------------------------------------------------------------
// Pools and maps
// ---------------------------------------------------------------

#[test]
fn a_complete_snapshot_loads_the_map_and_the_view_shows_it() {
    let mut m = member();
    let mut c = ctx();
    snapshot(&mut m, &mut c, POOL, 2, 2);
    let k = m.counters();
    assert_eq!(k.maps_loaded, 1);
    assert_eq!(k.maps_refused, 0);
    assert_eq!(k.pool_events, 4);
    assert!(m.book().is_live(0));
    let mut out = [HyparbPoolView::default(); 1];
    assert_eq!(m.hyparb_pools_view(&mut out), 1);
    assert_eq!(out[0].sym, POOL);
    assert_eq!(out[0].live, 1);
    assert_eq!(out[0].map_ok, 1);
    assert_eq!(out[0].fee_pips, 500);
    assert!(out[0].mid_1e6 > 0);
    // No hedge book: evaluated, but nothing to hedge with.
    assert!(c.orders.is_empty());
    assert_eq!(k.skipped_no_hedge, 1);
    // A short out slice is filled only as far as it goes.
    let mut none: [HyparbPoolView; 0] = [];
    assert_eq!(m.hyparb_pools_view(&mut none), 1);
}

#[test]
fn a_short_snapshot_is_refused_and_the_pool_is_never_traded() {
    let mut m = member();
    let mut c = ctx();
    perp_at(&mut m, &mut c, 100);
    snapshot(&mut m, &mut c, POOL, 3, 2);
    let k = m.counters();
    assert_eq!(k.maps_loaded, 0);
    assert_eq!(k.maps_refused, 1);
    assert!(!m.book().is_live(0), "the member stales its own view");
    assert!(c.orders.is_empty());
    let mut out = [HyparbPoolView::default(); 1];
    m.hyparb_pools_view(&mut out);
    assert_eq!(out[0].map_ok, 0);
}

#[test]
fn an_algebra_snapshot_loads_its_map_with_spacing_one() {
    let mut m = member();
    let mut c = ctx();
    let (lo, hi) = sqrt_at_tick(TICK0);
    // Coverage and nodes off any 10-grid: only a spacing-1 map takes them.
    m.on_signal(
        &sig(
            POOL,
            encode_snapshot(7, FAMILY_ALGEBRA, LO - 3, HI + 7, 2, 500, 60, 18, 6).unwrap(),
        ),
        &mut c,
    );
    m.on_signal(
        &sig(POOL, encode_tick(LO - 3, L as i128, L).unwrap()),
        &mut c,
    );
    m.on_signal(
        &sig(POOL, encode_tick(HI + 7, -(L as i128), L).unwrap()),
        &mut c,
    );
    m.on_signal(
        &sig(POOL, encode_state(TICK0, lo, hi, L, true).unwrap()),
        &mut c,
    );
    assert_eq!(m.counters().maps_loaded, 1);
    assert_eq!(m.pools[0].map_spacing, 1);
}

#[test]
fn a_new_snapshot_mid_staging_abandons_the_old_one() {
    let mut m = member();
    let mut c = ctx();
    m.on_signal(
        &sig(
            POOL,
            encode_snapshot(7, FAMILY_V3, LO, HI, 2, 500, 10, 18, 6).unwrap(),
        ),
        &mut c,
    );
    snapshot(&mut m, &mut c, POOL, 2, 2);
    let k = m.counters();
    assert_eq!(k.maps_refused, 1);
    assert_eq!(k.maps_loaded, 1);
}

#[test]
fn a_mint_keeps_the_map_and_an_impossible_burn_refuses_it() {
    let mut m = member();
    let mut c = ctx();
    snapshot(&mut m, &mut c, POOL, 2, 2);
    m.on_signal(
        &sig(
            POOL,
            encode_liquidity(8, false, -231_000, -230_000, L).unwrap(),
        ),
        &mut c,
    );
    assert!(m.pools[0].map_ok);
    assert_eq!(m.maps[0].len(), 4);
    m.on_signal(
        &sig(POOL, encode_liquidity(9, true, LO, HI, 3 * L).unwrap()),
        &mut c,
    );
    assert!(!m.pools[0].map_ok);
    assert_eq!(m.counters().maps_refused, 1);
    assert!(!m.book().is_live(0));
}

#[test]
fn a_gap_invalidates_every_map_and_a_refused_payload_is_counted() {
    let mut m = member();
    let mut c = ctx();
    snapshot(&mut m, &mut c, POOL, 2, 2);
    m.on_signal(&sig(SYMBOL_ID_NONE, encode_gap(9).unwrap()), &mut c);
    assert!(!m.pools[0].map_ok);
    perp_at(&mut m, &mut c, 100);
    assert!(c.orders.is_empty());
    m.on_signal(&sig(POOL, [0xff; 40]), &mut c);
    assert_eq!(m.counters().pool_refused, 1);
    // Another source's signal is not a pool event at all.
    let mut other = sig(POOL, [0; 40]);
    other.source = SignalSource::HyperEvm as u8 + 1;
    let before = m.counters().pool_events;
    m.on_signal(&other, &mut c);
    assert_eq!(m.counters().pool_events, before);
}

#[test]
fn a_pool_the_member_does_not_trade_is_tracked_by_the_book_alone() {
    let mut m = member();
    let mut c = ctx();
    snapshot(&mut m, &mut c, POOL2, 2, 2);
    assert!(m.book().is_live(1));
    assert_eq!(m.counters().maps_loaded, 0);
}

// ---------------------------------------------------------------
// The decision
// ---------------------------------------------------------------

#[test]
fn a_cheap_pool_is_bought_at_the_quotes_own_average_price() {
    let (m, c) = armed();
    let o = c.orders[0];
    assert_eq!(o.venue, VenueId::HyperEvm as u8);
    assert_eq!(o.sym, POOL);
    assert_eq!(o.side, Side::Bid);
    assert_eq!(o.kind, core_fill::ORDER_KIND_AMM_SWAP);
    assert!(o.qty.raw() > 0);
    let mid = pool_mid();
    assert!(
        o.px.raw() > mid && o.px.raw() < mid * 10_100 / 10_000,
        "{o:?}"
    );
    let k = m.counters();
    assert_eq!(k.arbs_submitted, 1);
    assert_eq!(k.gas_charged_usd_1e6, 10_000);
    assert!(k.pnl_predicted_usd_1e6 > 0);
    assert_eq!(k.size_capped, 1, "$1,000 caps a deep pool");
    // The member carried its own impact: the gap is not harvested twice.
    assert!(m.book().mid_1e6(0).unwrap() > mid);
    assert_eq!(m.orders_emitted(), 1);
}

/// H8: every submitted decision is logged with the solver's edge and
/// the gas coin's mid — what the write path's shadow bids against.
#[test]
fn a_submitted_decision_is_logged_for_the_write_path() {
    let mut p = params();
    p.gas_coin = 0;
    let mut m = member_with(p);
    let mut c = ctx();
    assert_eq!(m.hyparb_decision_log().1, 0, "nothing decided yet");
    perp_at(&mut m, &mut c, 100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    assert_eq!(c.orders.len(), 1);
    let (log, newest) = m.hyparb_decision_log();
    assert_eq!((log.len(), newest), (strategy_core::HYPARB_DECISION_LOG, 1));
    let d = log[1];
    assert_eq!((d.seq, d.pool_sym, d.buy), (1, POOL, 1));
    assert_eq!(d.edge_usd_1e6, m.counters().pnl_predicted_usd_1e6);
    assert!(d.edge_usd_1e6 > 0 && d.notional_usd_1e6 > 0);
    let perp_mid = pool_mid() * 10_100 / 10_000;
    assert!((d.gas_px_usd_1e6 - perp_mid).abs() <= 1, "{d:?}");
    // Without a gas coin the mid is unknown, never guessed.
    let (m2, _) = armed();
    let (log2, newest2) = m2.hyparb_decision_log();
    assert_eq!(newest2, 1);
    assert_eq!(log2[1].gas_px_usd_1e6, 0);
}

/// The log is a ring of `HYPARB_DECISION_LOG`, borrowed in place: the
/// newest 64 are kept, each at `seq % 64`; an overwritten slot carries a
/// newer `seq` — which is how a late reader tells the lost ones.
#[test]
fn the_decision_log_keeps_the_last_64_and_a_late_reader_sees_the_gap() {
    let mut m = member();
    let mut q = core_amm::ArbQuote::none(&core_amm::PoolState::ZERO, 0, 0);
    let mut k = 0i64;
    while k < 70 {
        q.pnl_usd_1e6 = k + 1;
        m.record_decision(T0 + k as u64, POOL, k % 2 == 0, &q);
        k += 1;
    }
    let (log, newest) = m.hyparb_decision_log();
    assert_eq!((log.len(), newest), (64, 70));
    assert_eq!((log[7].seq, log[6].seq), (7, 70), "slot 6 now holds 70");
    assert_eq!(log[6].edge_usd_1e6, 70);
    let mut s = 1u64;
    while s <= 6 {
        assert_ne!(log[(s % 64) as usize].seq, s, "seq {s} is gone");
        s += 1;
    }
    let mut s = 7u64;
    while s <= 70 {
        assert_eq!(log[(s % 64) as usize].seq, s, "seq {s} is kept");
        s += 1;
    }
}

#[test]
fn a_rich_pool_is_sold_into() {
    let mut m = member();
    let mut c = ctx();
    perp_at(&mut m, &mut c, -100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    assert_eq!(c.orders.len(), 1);
    assert_eq!(c.orders[0].side, Side::Ask);
    assert!(c.orders[0].px.raw() < pool_mid());
}

#[test]
fn a_pool_in_line_with_the_hedge_is_not_traded() {
    let mut m = member();
    let mut c = ctx();
    perp_at(&mut m, &mut c, 0);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    assert!(c.orders.is_empty());
    assert_eq!(m.counters().skipped_below_min, 1);
}

#[test]
fn an_arb_in_flight_and_the_cooldown_gate_the_next() {
    let (mut m, mut c) = armed();
    perp_at(&mut m, &mut c, 120);
    assert_eq!(c.orders.len(), 1);
    assert_eq!(m.counters().skipped_inflight, 1);
    // The judge never answered: the timer frees the pool; the cooldown
    // has also run.
    c.now = T0 + INFLIGHT_NS;
    m.on_timer(c.now, &mut c);
    perp_at(&mut m, &mut c, 140);
    assert_eq!(c.orders.len(), 2);
}

#[test]
fn a_stale_or_old_hedge_book_is_never_traded_against() {
    let mut m = member();
    let mut c = ctx();
    snapshot(&mut m, &mut c, POOL, 2, 2);
    let mid = pool_mid() * 101 / 100;
    let mut t = bbo(PERP, mid - 1_000, mid + 1_000, 100_000_000, c.now);
    t.flags = TICK_FLAG_STALE;
    m.on_tick(&t, &mut c);
    assert!(c.orders.is_empty());
    // A sound tick, then the feed dies: past the age bound nothing trades.
    perp_at(&mut m, &mut c, 100);
    assert_eq!(c.orders.len(), 1);
    let mut m = member();
    let mut c = ctx();
    perp_at(&mut m, &mut c, 100);
    c.now = T0 + hedge::TOUCH_MAX_AGE_NS + 1;
    snapshot(&mut m, &mut c, POOL, 2, 2);
    assert!(c.orders.is_empty());
    assert_eq!(m.counters().skipped_no_hedge, 1);
}

#[test]
fn the_depth_cap_bounds_the_size_to_the_touch() {
    let mut m = member();
    let mut c = ctx();
    let mid = pool_mid() * 101 / 100;
    // One coin at the bid: ~$98 of depth.
    m.on_tick(
        &bbo(PERP, mid - 1_000, mid + 1_000, 1_000_000, c.now),
        &mut c,
    );
    snapshot(&mut m, &mut c, POOL, 2, 2);
    assert_eq!(c.orders.len(), 1);
    assert!(c.orders[0].qty.raw() <= 1_000_000, "{:?}", c.orders[0]);
    // Without the cap the same book is harvested to the $1,000 cap.
    let mut p = params();
    p.depth_cap_enabled = false;
    let mut m = member_with(p);
    let mut c = ctx();
    m.on_tick(
        &bbo(PERP, mid - 1_000, mid + 1_000, 1_000_000, c.now),
        &mut c,
    );
    snapshot(&mut m, &mut c, POOL, 2, 2);
    assert!(c.orders[0].qty.raw() > 5_000_000);
}

#[test]
fn basis_control_de_means_a_persistent_deviation() {
    let mut p = params();
    p.basis_enabled = true;
    let mut m = member_with(p);
    let mut c = ctx();
    perp_at(&mut m, &mut c, 100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    // The first sample IS the pool's basis: no edge left.
    assert!(c.orders.is_empty());
    assert_eq!(m.counters().skipped_below_min, 1);
    let mut out = [HyparbPoolView::default(); 1];
    m.hyparb_pools_view(&mut out);
    assert!(
        out[0].basis_bps_1e6 < -90 * 1_000_000,
        "{}",
        out[0].basis_bps_1e6
    );
}

#[test]
fn the_day_cap_and_the_inventory_cap_halt_new_arbs() {
    let mut p = params();
    p.cap_day_usd_1e6 = 1_000_000_000;
    let (mut m, mut c) = {
        let mut m = member_with(p);
        let mut c = ctx();
        perp_at(&mut m, &mut c, 100);
        snapshot(&mut m, &mut c, POOL, 2, 2);
        (m, c)
    };
    assert_eq!(c.orders.len(), 1);
    c.now = T0 + INFLIGHT_NS;
    m.on_timer(c.now, &mut c);
    perp_at(&mut m, &mut c, 150);
    assert_eq!(c.orders.len(), 1);
    assert_eq!(m.counters().skipped_halted, 1, "{:?}", m.counters());

    // Inventory: a fill that leaves more than the cap unhedged halts.
    let mut p = params();
    p.inventory_cap_usd_1e6 = 100_000_000;
    let mut m = member_with(p);
    let mut c = ctx();
    perp_at(&mut m, &mut c, 100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    let o = c.orders[0];
    m.on_fill(
        &Fill::new(c.now, POOL, Side::Bid, o.px, o.qty, o.client_oid),
        &mut c,
    );
    assert!(m.is_halted());
    assert_eq!(m.counters().inventory_breaches, 1);
    // The hedge fills: back under half the cap, trading resumes.
    let h = c.orders[1];
    m.on_fill(
        &Fill::new(c.now, PERP, Side::Ask, h.px, h.qty, h.client_oid),
        &mut c,
    );
    assert!(!m.is_halted());
}

#[test]
fn a_full_ring_drops_the_order_and_counts_it() {
    let mut m = member();
    let mut c = ctx();
    c.full = true;
    perp_at(&mut m, &mut c, 100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    assert!(c.orders.is_empty());
    assert_eq!(m.counters().orders_dropped, 1);
    assert_eq!(m.orders_dropped(), 1);
    assert_eq!(m.counters().arbs_submitted, 0);
}

// ---------------------------------------------------------------
// Fills, hedges, inventory
// ---------------------------------------------------------------

#[test]
fn an_amm_fill_is_hedged_on_the_perp_and_the_hedge_fill_closes_it() {
    let (mut m, mut c) = armed();
    let o = c.orders[0];
    m.on_fill(
        &Fill::new(c.now, POOL, Side::Bid, o.px, o.qty, o.client_oid),
        &mut c,
    );
    assert_eq!(m.inventory_1e6(0), Some(o.qty.raw()));
    assert_eq!(c.orders.len(), 2);
    let h = c.orders[1];
    assert_eq!(h.venue, VenueId::Hyperliquid as u8);
    assert_eq!(h.sym, PERP);
    assert_eq!(h.side, Side::Ask);
    assert_eq!(h.kind, core_fill::ORDER_KIND_IOC);
    assert_eq!(h.ttl_ns, 500_000_000);
    assert_eq!(
        h.qty.raw(),
        o.qty.raw() - o.qty.raw() % 10_000,
        "lot-rounded"
    );
    let k = m.counters();
    assert_eq!((k.amm_fills, k.hedges_submitted, k.hedges_perp), (1, 1, 1));
    assert!(k.amm_notional_usd_1e6 > 0);
    m.on_fill(
        &Fill::new(c.now, PERP, Side::Ask, h.px, h.qty, h.client_oid),
        &mut c,
    );
    assert_eq!(m.inventory_1e6(0), Some(o.qty.raw() % 10_000));
    assert_eq!(m.counters().hedge_fills, 1);
    // The sub-lot residue is below the venue minimum: the timer sends
    // nothing and misses nothing.
    c.now += 10 * TIMER_NS;
    m.on_timer(c.now, &mut c);
    assert_eq!(c.orders.len(), 2);
    assert_eq!(m.counters().hedges_missed, 0);
}

#[test]
fn a_missed_hedge_is_counted_and_the_timer_flattens_the_residue() {
    let (mut m, mut c) = armed();
    let o = c.orders[0];
    m.on_fill(
        &Fill::new(c.now, POOL, Side::Bid, o.px, o.qty, o.client_oid),
        &mut c,
    );
    // Before the deadline: still pending, nothing re-sent.
    c.now += 500_000_000;
    m.on_timer(c.now, &mut c);
    assert_eq!(c.orders.len(), 2);
    c.now += HEDGE_GRACE_NS + 1;
    perp_at(&mut m, &mut c, 0);
    m.on_timer(c.now, &mut c);
    let k = m.counters();
    assert_eq!(k.hedges_missed, 1);
    assert_eq!(k.flattens_submitted, 1);
    let f = c.orders[c.orders.len() - 1];
    assert_eq!((f.sym, f.side), (PERP, Side::Ask));
}

#[test]
fn a_sold_pool_leg_is_hedged_by_buying_the_coin() {
    let mut m = member();
    let mut c = ctx();
    perp_at(&mut m, &mut c, -100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    let o = c.orders[0];
    m.on_fill(
        &Fill::new(c.now, POOL, Side::Ask, o.px, o.qty, o.client_oid),
        &mut c,
    );
    assert_eq!(m.inventory_1e6(0), Some(-o.qty.raw()));
    assert_eq!(c.orders[1].side, Side::Bid);
}

#[test]
fn hedge_venue_perp_and_spot_both_run() {
    let mut i = 0;
    while i < 2 {
        let mut p = params();
        p.hedge_mode = if i == 0 {
            HedgeMode::Perp
        } else {
            HedgeMode::Spot
        };
        let mut m = member_with(p);
        let mut c = ctx();
        let mid = pool_mid() * 101 / 100;
        m.on_tick(
            &bbo(PERP, mid - 1_000, mid + 1_000, 100_000_000, c.now),
            &mut c,
        );
        m.on_tick(
            &bbo(SPOT, mid - 1_000, mid + 1_000, 100_000_000, c.now),
            &mut c,
        );
        snapshot(&mut m, &mut c, POOL, 2, 2);
        let o = c.orders[0];
        m.on_fill(
            &Fill::new(c.now, POOL, Side::Bid, o.px, o.qty, o.client_oid),
            &mut c,
        );
        let want = if i == 0 { PERP } else { SPOT };
        assert_eq!(c.orders[1].sym, want, "mode {i}");
        let k = m.counters();
        assert_eq!(
            (k.hedges_perp, k.hedges_spot),
            if i == 0 { (1, 0) } else { (0, 1) }
        );
        i += 1;
    }
}

#[test]
fn funding_moves_the_auto_selector_by_side() {
    let mut p = params();
    p.spot_taker_bps_1e6 = 1_000_000;
    let mut m = member_with(p);
    let mut c = ctx();
    let mid = 100_000_000;
    m.on_tick(
        &bbo(PERP, mid - 5_000, mid + 5_000, 100_000_000, c.now),
        &mut c,
    );
    m.on_tick(
        &bbo(SPOT, mid - 5_000, mid + 5_000, 100_000_000, c.now),
        &mut c,
    );
    // Spot is cheaper by the taker difference.
    assert_eq!(
        m.hedge_side(0, true, 1_000_000_000, c.now).unwrap().venue,
        HedgeVenue::Spot
    );
    // 0.05 %/h over one hour pays a short 5 bps: the perp wins the sale,
    // and the purchase stays on spot.
    m.on_venue_event(
        &ChannelEvent::new(
            c.now,
            VenueId::Hyperliquid,
            ChannelId::AssetCtx,
            PERP,
            0,
            0,
            500_000,
            0,
        ),
        &mut c,
    );
    assert_eq!(
        m.hedge_side(0, true, 1_000_000_000, c.now).unwrap().venue,
        HedgeVenue::Perp
    );
    assert_eq!(
        m.hedge_side(0, false, 1_000_000_000, c.now).unwrap().venue,
        HedgeVenue::Spot
    );
    // The USD side needs no leg; an unknown coin has none.
    assert_eq!(
        m.hedge_side(COIN_USD, true, 1, c.now).unwrap().venue,
        HedgeVenue::None
    );
    assert!(m.hedge_side(5, true, 1, c.now).is_none());
}

// ---------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------

#[test]
fn choose_venue_forces_prefers_the_cheaper_and_holds_under_hysteresis() {
    let p = VenueCost {
        venue: HedgeVenue::Perp,
        cost_bps_1e6: 10,
    };
    let s = VenueCost {
        venue: HedgeVenue::Spot,
        cost_bps_1e6: 8,
    };
    assert_eq!(
        choose_venue(HedgeMode::Perp, Some(p), Some(s), HedgeVenue::None, 0),
        Some(p)
    );
    assert_eq!(
        choose_venue(HedgeMode::Spot, Some(p), Some(s), HedgeVenue::None, 0),
        Some(s)
    );
    assert_eq!(
        choose_venue(HedgeMode::Spot, Some(p), None, HedgeVenue::None, 0),
        None
    );
    assert_eq!(
        choose_venue(HedgeMode::Auto, Some(p), Some(s), HedgeVenue::None, 0),
        Some(s)
    );
    // Last was perp; spot is cheaper by 2, less than the hysteresis 5.
    assert_eq!(
        choose_venue(HedgeMode::Auto, Some(p), Some(s), HedgeVenue::Perp, 5),
        Some(p)
    );
    assert_eq!(
        choose_venue(HedgeMode::Auto, Some(p), Some(s), HedgeVenue::Perp, 1),
        Some(s)
    );
    assert_eq!(
        choose_venue(HedgeMode::Auto, None, Some(s), HedgeVenue::Perp, 5),
        Some(s)
    );
    assert_eq!(
        choose_venue(HedgeMode::Auto, Some(p), None, HedgeVenue::Spot, 5),
        Some(p)
    );
    assert_eq!(
        choose_venue(HedgeMode::Auto, None, None, HedgeVenue::Spot, 5),
        None
    );
    // A tie with no history goes to the perp.
    let tie = VenueCost {
        venue: HedgeVenue::Spot,
        cost_bps_1e6: 10,
    };
    assert_eq!(
        choose_venue(HedgeMode::Auto, Some(p), Some(tie), HedgeVenue::None, 0),
        Some(p)
    );
}

#[test]
fn coin_touch_reads_a_tick_and_refuses_an_unsound_book() {
    let t = CoinTouch::of(&bbo(PERP, 99_000_000, 101_000_000, 5, 7));
    assert_eq!(t.mid_1e6(), Some(100_000_000));
    assert!(t.is_usable(7 + hedge::TOUCH_MAX_AGE_NS));
    assert!(!t.is_usable(8 + hedge::TOUCH_MAX_AGE_NS));
    assert!(t.same_quote(&CoinTouch::of(&bbo(PERP, 99_000_000, 101_000_000, 5, 9))));
    assert!(!t.same_quote(&CoinTouch::of(&bbo(PERP, 99_000_000, 101_000_000, 6, 7))));
    let crossed = CoinTouch::of(&bbo(PERP, 101_000_000, 99_000_000, 5, 7));
    assert_eq!(crossed.mid_1e6(), None);
    let one_sided = CoinTouch::of(&bbo(PERP, 0, 101_000_000, 5, 7));
    assert_eq!(one_sided.mid_1e6(), None);
    let mut stale = bbo(PERP, 99_000_000, 101_000_000, 5, 7);
    stale.flags = TICK_FLAG_STALE;
    assert!(CoinTouch::of(&stale).stale);
    assert_eq!(CoinTouch::of(&stale).mid_1e6(), None);
    assert_eq!(CoinTouch::EMPTY.mid_1e6(), None);
}

#[test]
fn ratio_and_mul_div_round_and_refuse_as_documented() {
    assert_eq!(
        ratio_1e18(2_000_000, 1_000_000, 0),
        Some(2_000_000_000_000_000_000)
    );
    // +1 % basis lifts the bound 1 %.
    assert_eq!(
        ratio_1e18(1_000_000, 1_000_000, ONE_BPS_1E6 / 100),
        Some(1_010_000_000_000_000_000)
    );
    assert_eq!(ratio_1e18(0, 1_000_000, 0), None);
    assert_eq!(ratio_1e18(1_000_000, 0, 0), None);
    assert_eq!(ratio_1e18(1_000_000, 1_000_000, -ONE_BPS_1E6), None);
    assert_eq!(mul_div_i64(7, 3, 2), 10);
    assert_eq!(mul_div_i64(7, 3, 0), 0);
    assert_eq!(mul_div_i64(i64::MAX, i64::MAX, 1), i64::MAX);
}

// ---------------------------------------------------------------
// H6 observables
// ---------------------------------------------------------------

#[test]
fn the_side_balance_and_per_pool_prediction_are_counted() {
    let (m, _c) = armed();
    let k = m.counters();
    assert_eq!((k.arbs_buy, k.arbs_sell), (1, 0));
    let mut out = [HyparbPoolView::default(); 1];
    m.hyparb_pools_view(&mut out);
    assert_eq!(out[0].pnl_predicted_usd_1e6, k.pnl_predicted_usd_1e6);
    assert!(out[0].pnl_predicted_usd_1e6 > 0);
    let mut m = member();
    let mut c = ctx();
    perp_at(&mut m, &mut c, -100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    let k = m.counters();
    assert_eq!((k.arbs_buy, k.arbs_sell), (0, 1));
}

#[test]
fn the_coin_view_carries_depth_costs_positions_and_funding() {
    let (mut m, mut c) = armed();
    let mut out = [HyparbCoinView::default(); 2];
    assert_eq!(m.hyparb_coins_view(&mut out), 1);
    assert_eq!((out[0].perp_sym, out[0].spot_sym), (PERP, SPOT));
    assert!(
        out[0].perp_depth_usd_1e6 > 1_000_000_000,
        "100 coins at ~$98"
    );
    assert_eq!(out[0].spot_depth_usd_1e6, 0, "no spot book");
    assert!(out[0].perp_cost_bps_1e6 > 4_500_000, "taker + half spread");
    // The AMM fill, then the perp hedge fill: a short perp position.
    let o = c.orders[0];
    m.on_fill(
        &Fill::new(c.now, POOL, Side::Bid, o.px, o.qty, o.client_oid),
        &mut c,
    );
    let h = c.orders[1];
    m.on_fill(
        &Fill::new(c.now, PERP, Side::Ask, h.px, h.qty, h.client_oid),
        &mut c,
    );
    m.hyparb_coins_view(&mut out);
    assert_eq!(out[0].perp_pos_1e6, -h.qty.raw());
    // 0.05 %/h for one hour on a short earns ≈ notional × 5 bps.
    m.on_venue_event(
        &ChannelEvent::new(
            c.now,
            VenueId::Hyperliquid,
            ChannelId::AssetCtx,
            PERP,
            0,
            0,
            500_000,
            0,
        ),
        &mut c,
    );
    m.on_timer(c.now, &mut c);
    // Keep the book fresh across the hour (a stale touch has no mid).
    c.now += 3_600_000_000_000;
    perp_at(&mut m, &mut c, 100);
    m.on_timer(c.now, &mut c);
    let earned = m.counters().funding_earned_usd_1e6;
    let mid = pool_mid() * 10_100 / 10_000;
    let want = (h.qty.raw() as i128 * mid as i128 / 1_000_000 * 5 / 10_000) as i64;
    assert!(
        (earned - want).abs() <= want / 100 + 1,
        "earned {earned} want {want}"
    );
    m.hyparb_coins_view(&mut out);
    assert_eq!(out[0].funding_1e9, 500_000);
    // An unconfigured member reports no coins.
    assert_eq!(HyparbStrategy::new().hyparb_coins_view(&mut out), 0);
}

#[test]
fn funding_accrues_nothing_on_the_first_pass_or_a_backward_clock() {
    let mut m = member();
    m.accrue_funding(T0);
    assert_eq!(m.counters().funding_earned_usd_1e6, 0);
    m.accrue_funding(T0 - 1);
    assert_eq!(m.counters().funding_earned_usd_1e6, 0);
}

#[test]
fn a_hedge_book_that_dies_never_lifts_the_inventory_halt() {
    let mut p = params();
    p.inventory_cap_usd_1e6 = 100_000_000;
    let mut m = member_with(p);
    let mut c = ctx();
    perp_at(&mut m, &mut c, 100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    let o = c.orders[0];
    m.on_fill(
        &Fill::new(c.now, POOL, Side::Bid, o.px, o.qty, o.client_oid),
        &mut c,
    );
    assert!(m.is_halted());
    let inv = m.inventory_1e6(0).expect("coin 0");
    assert_ne!(inv, 0);
    // The perp book goes stale: the coin has no usable mid. The
    // exposure is valued at its last mid — still over half the cap —
    // so the halt holds (it used to value it at $0 and lift).
    let mid = pool_mid() * 101 / 100;
    let mut t = bbo(PERP, mid - 1_000, mid + 1_000, 100_000_000, c.now);
    t.flags = TICK_FLAG_STALE;
    m.on_tick(&t, &mut c);
    c.now += 1_000_000_000;
    m.on_timer(c.now, &mut c);
    assert!(
        m.is_halted(),
        "an unusable book does not make the exposure $0"
    );
}

#[test]
fn inventory_that_was_never_valued_halts_and_never_lifts_a_halt() {
    let mut p = params();
    p.inventory_cap_usd_1e6 = 100_000_000;
    let mut m = member_with(p.clone());
    m.halted = true;
    m.coins[0].inventory_1e6 = 5_000_000;
    m.coins[0].last_mid_1e6 = 0;
    m.check_inventory();
    assert!(m.is_halted(), "no mid ever: unvalued, the halt holds");
    // …and sets it: an exposure of unknown size is under no cap.
    let mut m = member_with(p);
    m.coins[0].inventory_1e6 = 5_000_000;
    m.check_inventory();
    assert!(m.is_halted(), "unvalued inventory halts new arbs");
    assert_eq!(m.counters().inventory_breaches, 1);
}

#[test]
fn the_halted_level_follows_the_inventory_cap() {
    let mut p = params();
    p.inventory_cap_usd_1e6 = 100_000_000;
    let mut m = member_with(p);
    let mut c = ctx();
    perp_at(&mut m, &mut c, 100);
    snapshot(&mut m, &mut c, POOL, 2, 2);
    let o = c.orders[0];
    m.on_fill(
        &Fill::new(c.now, POOL, Side::Bid, o.px, o.qty, o.client_oid),
        &mut c,
    );
    assert_eq!(m.counters().halted, 1);
    let h = c.orders[1];
    m.on_fill(
        &Fill::new(c.now, PERP, Side::Ask, h.px, h.qty, h.client_oid),
        &mut c,
    );
    assert_eq!(m.counters().halted, 0);
}
