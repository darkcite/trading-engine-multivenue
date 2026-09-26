// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The member's laws, one test each: inputs, the decision and its gates,
//! sizing and the caps, one IoC per instrument, fills, the hedge band and
//! the final-window TWAP, settlement, the stops, the calendar mailbox.

use super::*;
use core_types::make_symbol_id;
use strategy_core::{SubmitErr, HAR_VIEW_NAME_MAX};

/// 2026-09-26T12:00:00Z.
const T0_MS: u64 = 1_790_424_000_000;
/// The engine's monotonic clock at T0.
const MONO0: u64 = 1_000_000_000_000;
const DAY: u64 = 86_400_000;
const S_SP: i64 = 6_600_000_000;
const S_BTC: i64 = 110_000_000_000;
const SP_HEDGE: SymbolId = make_symbol_id(VenueId::Hyperliquid, 40);
const BTC_HEDGE: SymbolId = make_symbol_id(VenueId::Hyperliquid, 1);

/// Monotonic ns at T0 + `ms`.
fn ns(ms: u64) -> NsTs {
    MONO0 + ms * 1_000_000
}

fn opt_sym(k: u32) -> SymbolId {
    make_symbol_id(VenueId::Hypercall, 513 + k)
}

/// k: 0 ATM call, 1 ATM put (both 7 d), 2 far call (out of band), 3 ATM
/// call expiring in 12 h (under the tenor floor), 4 BTC ATM call (7 d).
fn params() -> HcvParams {
    HcvParams {
        und: vec![HcvUnd::new(b"SP500", SP_HEDGE, 2), HcvUnd::new(b"BTC", BTC_HEDGE, 5)],
        options: vec![
            HcvOpt { sym: opt_sym(0), und: 0, call: true, strike_1e6: S_SP, exp_ms: T0_MS + 7 * DAY },
            HcvOpt { sym: opt_sym(1), und: 0, call: false, strike_1e6: S_SP, exp_ms: T0_MS + 7 * DAY },
            HcvOpt { sym: opt_sym(2), und: 0, call: true, strike_1e6: 8_000_000_000, exp_ms: T0_MS + 7 * DAY },
            HcvOpt { sym: opt_sym(3), und: 0, call: true, strike_1e6: S_SP, exp_ms: T0_MS + DAY / 2 },
            HcvOpt { sym: opt_sym(4), und: 1, call: true, strike_1e6: S_BTC, exp_ms: T0_MS + 7 * DAY },
        ],
        theta_vol_1e6: 50_000,
        atm_band_bps: 500,
        tenor_min_d: 1,
        tenor_max_d: 40,
        clip_usd_1e6: 5_000_000,
        vega_cap_usd_1e6: 20_000_000,
        premium_cap_usd_1e6: 50_000_000,
        day_loss_usd_1e6: 20_000_000,
        // Large by default: the stress law has its own test.
        tail_loss_usd_1e6: 1_000_000_000,
        opt_size_step_1e6: 1_000,
        hedge_band_1e6: 100_000,
        hedge_min_usd_1e6: 10_000_000,
        hedge_slip_bps: 10,
        quote_stale_ms: 10_000,
        oracle_stale_ms: 10_000,
        unwind_min: 30,
        settle_delay_ms: 60_000,
        settle_order: BucketOrder::Sorted,
        event_law: true,
        events_stale_ms: 7_200_000,
        kill: false,
        timer_ms: 1_000,
        anchor: WallAnchor::new(MONO0, T0_MS * 1_000_000),
    }
}

#[derive(Default)]
struct Rec {
    orders: Vec<Order>,
    refuse: bool,
}

impl Ctx for Rec {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        if self.refuse {
            return Err(SubmitErr::RingFull);
        }
        self.orders.push(order);
        Ok(())
    }

    fn now_ns(&self) -> NsTs {
        0
    }
}

fn har_row(name: &[u8], sig_1e6: i32) -> HarSeriesView {
    let mut n = [0u8; HAR_VIEW_NAME_MAX];
    // COPY: a test row's ≤ 12 B series name, once per fixture — test-only
    // (this file is the member's `#[cfg(test)]` module) — nothing to borrow.
    n[..name.len()].copy_from_slice(name);
    HarSeriesView {
        name: n,
        name_len: name.len() as u8,
        warm: 1,
        raw_1e6: [sig_1e6; HAR_VIEW_TENORS],
        ..HarSeriesView::default()
    }
}

/// A calendar generated at `gen_ms`, vouching for 30 days back and 60 on.
fn calendar(gen_ms: u64, events: &[(u64, u16)]) -> HcvEvents {
    let mut c = HcvEvents::new();
    c.generated_ms = gen_ms;
    c.from_ms = gen_ms - 30 * DAY;
    c.until_ms = gen_ms + 60 * DAY;
    let mut i = 0usize;
    while i < events.len() {
        assert!(c.push(events[i].0, events[i].1));
        i += 1;
    }
    c
}

/// A configured member at 15 % σ̂ on both underlyings, with an empty
/// calendar generated a minute before T0.
fn member() -> HcvStrategy {
    member_with(&params())
}

fn member_with(p: &HcvParams) -> HcvStrategy {
    let mut s = HcvStrategy::new();
    s.configure(p).expect("the fixture configures");
    s.set_har_view(&[har_row(b"SP500", 150_000), har_row(b"BTC", 450_000)]);
    s.events = calendar(T0_MS - 60_000, &[]);
    s
}

fn hl_tick(sym: SymbolId, bid: i64, ask: i64, at_ms: u64) -> Tick {
    Tick::new_stamped(
        ns(at_ms),
        VenueId::Hyperliquid,
        sym,
        0,
        Price::from_raw(bid),
        Qty::from_raw(50_000_000),
        Price::from_raw(ask),
        Qty::from_raw(50_000_000),
        0,
        0,
    )
}

fn hc_tick(k: u32, bid: i64, ask: i64, at_ms: u64) -> Tick {
    Tick::new_stamped(
        ns(at_ms),
        VenueId::Hypercall,
        opt_sym(k),
        0,
        Price::from_raw(bid),
        Qty::from_raw(if bid > 0 { 1_000_000 } else { 0 }),
        Price::from_raw(ask),
        Qty::from_raw(if ask > 0 { 1_000_000 } else { 0 }),
        0,
        0,
    )
}

fn mark(sym: SymbolId, oracle: i64, at_ms: u64) -> ChannelEvent {
    ChannelEvent::new(ns(at_ms), VenueId::Hyperliquid, ChannelId::Mark, sym, 0, 0, oracle, oracle)
}

/// Fresh SP500 oracle and hedge touch at `at_ms`.
fn feed_sp(s: &mut HcvStrategy, c: &mut Rec, oracle: i64, at_ms: u64) {
    s.on_venue_event(&mark(SP_HEDGE, oracle, at_ms), c);
    s.on_tick(&hl_tick(SP_HEDGE, oracle - 500_000, oracle + 500_000, at_ms), c);
}

/// The premium ×1e6 of option `k`'s shape at vol `sig`, `tau_d` days out.
fn px_at(call: bool, s: i64, k: i64, tau_d: f64, sig: f64) -> i64 {
    let g = bs::price(call, s as f64 / 1e6, k as f64 / 1e6, tau_d / 365.0, sig).expect("on the domain");
    (g.price * 1e6).round() as i64
}

/// The ATM 7-day SP500 call quoted at bid/ask vols `vb`/`va`.
fn quote_call(s: &mut HcvStrategy, c: &mut Rec, vb: f64, va: f64, at_ms: u64) -> (i64, i64) {
    let (b, a) = (px_at(true, S_SP, S_SP, 7.0, vb), px_at(true, S_SP, S_SP, 7.0, va));
    s.on_tick(&hc_tick(0, b, a, at_ms), c);
    (b, a)
}

fn fill(sym: SymbolId, side: Side, px: i64, qty: i64, oid: u64, at_ms: u64) -> Fill {
    Fill::new(ns(at_ms), sym, side, Price::from_raw(px), Qty::from_raw(qty), oid)
}

/// Fill every hedge order from index `from` at its own limit.
fn fill_hedges(s: &mut HcvStrategy, c: &mut Rec, from: usize, at_ms: u64) -> usize {
    let mut i = from;
    while i < c.orders.len() {
        let o = c.orders[i];
        if o.venue == VenueId::Hyperliquid as u8 {
            s.on_fill(&fill(o.sym, o.side, o.px.raw(), o.qty.raw(), o.client_oid, at_ms), c);
        }
        i += 1;
    }
    c.orders.len()
}

// ---- inputs and configuration -----------------------------------------

#[test]
fn an_unconfigured_member_is_inert_and_arms_no_timer() {
    let mut s = HcvStrategy::new();
    let mut c = Rec::default();
    assert_eq!(s.timer_period_ns(), u64::MAX);
    s.on_tick(&hc_tick(0, 1, 2, 0), &mut c);
    s.on_venue_event(&mark(SP_HEDGE, S_SP, 0), &mut c);
    s.on_fill(&fill(opt_sym(0), Side::Bid, 1, 1, 1, 0), &mut c);
    s.on_timer(ns(0), &mut c);
    assert!(c.orders.is_empty());
    assert_eq!(s.counters(), HcvCounters::default());
    assert_eq!(s.strategy_kind(), "hcv");
}

#[test]
fn configure_refuses_what_the_member_cannot_trade() {
    let mut s = HcvStrategy::new();
    let mut p = params();
    p.options[0].sym = make_symbol_id(VenueId::Hypercall, 3);
    assert!(s.configure(&p).is_err(), "an index symbol is no option");
    let mut p = params();
    p.options[1].sym = p.options[0].sym;
    assert!(s.configure(&p).is_err(), "listed twice");
    let mut p = params();
    p.options[0].und = 2;
    assert!(s.configure(&p).is_err(), "no such underlying");
    let mut p = params();
    p.tail_loss_usd_1e6 = 0;
    assert!(s.configure(&p).is_err(), "a cap of zero");
    let mut p = params();
    p.tenor_max_d = 41;
    assert!(s.configure(&p).is_err());
    let mut p = params();
    p.und[1].hedge_sz_decimals = 7;
    assert!(s.configure(&p).is_err());
    assert!(!s.is_configured());
    s.configure(&params()).expect("the fixture");
    assert!(s.is_configured());
    assert_eq!(s.timer_period_ns(), 1_000_000_000);
}

#[test]
fn sigma_hat_interpolates_in_total_variance_and_a_cold_series_forecasts_nothing() {
    let mut s = member();
    let mut row = har_row(b"SP500", 0);
    row.raw_1e6 = [100_000, 200_000, 200_000, 200_000, 200_000, 200_000, 200_000, 200_000, 300_000];
    s.set_har_view(&[row]);
    assert!((s.sigma_hat(0, 0.5).unwrap() - 0.10).abs() < 1e-12, "flat below the first tenor");
    assert!((s.sigma_hat(0, 60.0).unwrap() - 0.30).abs() < 1e-12, "flat beyond the last");
    // 1.5 d: w = (0.01·1 + 0.04·2) / 2 = 0.045 → σ = √(0.045 / 1.5).
    let want = (0.045f64 / 1.5).sqrt();
    assert!((s.sigma_hat(0, 1.5).unwrap() - want).abs() < 1e-12);
    // A fit beats the raw fold where it exists.
    row.fitted = 1;
    row.fit_1e6[0] = 120_000;
    s.set_har_view(&[row]);
    assert!((s.sigma_hat(0, 1.0).unwrap() - 0.12).abs() < 1e-12);
    // BTC is absent from this view: no forecast, and a cold row is none.
    assert!(s.sigma_hat(1, 7.0).is_none());
    row.warm = 0;
    s.set_har_view(&[row]);
    assert!(s.sigma_hat(0, 7.0).is_none());
    assert_eq!(s.counters().har_updates, 4, "the fixture's push and three here");
}

#[test]
fn the_calendar_vouches_only_for_its_window() {
    let c = calendar(T0_MS, &[(T0_MS + 3 * DAY, 0b01), (T0_MS + 9 * DAY, 0b10)]);
    assert_eq!(c.any_in(0, T0_MS, T0_MS + 7 * DAY), Some(true));
    assert_eq!(c.any_in(1, T0_MS, T0_MS + 7 * DAY), Some(false));
    assert_eq!(c.any_in(1, T0_MS, T0_MS + 9 * DAY), Some(true), "(after, until] — the end counts");
    assert_eq!(c.any_in(0, T0_MS + 3 * DAY, T0_MS + 7 * DAY), Some(false), "the start does not");
    assert_eq!(c.any_in(0, T0_MS, T0_MS + 61 * DAY), None, "past the window: unknown");
    assert_eq!(c.any_in(0, T0_MS - 31 * DAY, T0_MS), None);
    assert_eq!(HcvEvents::new().any_in(0, T0_MS, T0_MS + DAY), None, "no calendar: unknown");
    let mut full = HcvEvents::new();
    let mut i = 0usize;
    while i < HCV_MAX_EVENTS {
        assert!(full.push(i as u64, 1));
        i += 1;
    }
    assert!(!full.push(0, 1));
}

#[test]
fn the_calendar_arrives_by_mailbox_and_stays_between_changes() {
    let mut s = member();
    s.events = HcvEvents::new();
    let (mut tx, rx) = core_ring::Mailbox::new(Box::new(HcvEvents::new())).split();
    s.install_events(rx);
    let cal = calendar(T0_MS - 1_000, &[(T0_MS + DAY, 0b11)]);
    {
        let mut f = tx.try_fill().expect("the slot starts free");
        *f = cal;
        f.commit();
    }
    let mut c = Rec::default();
    s.on_timer(ns(0), &mut c);
    assert_eq!(*s.events(), cal);
    assert_eq!(s.counters().calendars, 1);
    s.on_timer(ns(1_000), &mut c);
    assert_eq!(*s.events(), cal, "kept while nothing new arrives");
    assert_eq!(s.counters().calendars, 1);
    assert!(tx.try_fill().is_some(), "the slot went back to the reader");
}

// ---- the decision -------------------------------------------------------

#[test]
fn it_sells_one_ioc_at_the_bid_when_the_bid_is_theta_rich() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    let (bid, _) = quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    assert_eq!(c.orders.len(), 1, "{:?}", s.counters());
    let o = c.orders[0];
    assert_eq!(o.venue, VenueId::Hypercall as u8);
    assert_eq!((o.sym, o.side, o.kind, o.px.raw()), (opt_sym(0), Side::Ask, core_fill::ORDER_KIND_IOC, bid));
    let q = div_1e6(5_000_000, bid);
    assert_eq!(o.qty.raw(), q - q % 1_000, "the clip, on the size step");
    assert_ne!(o.client_oid, 0);
    let k = s.counters();
    assert_eq!((k.sells, k.buys), (1, 0));
    // Judged: the call alone — its IoC is now in flight, so the put (the
    // same underlying) waits for its verdict; the far strike and the 12 h
    // expiry are outside the policy and never judged.
    assert_eq!((k.judged, k.skip_stale), (1, 0));
    assert_eq!(s.emitted, 1);
}

#[test]
fn it_buys_at_the_ask_when_the_ask_is_theta_cheap_and_holds_inside_the_gap() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    let (_, ask) = quote_call(&mut s, &mut c, 0.07, 0.09, 0);
    s.on_timer(ns(500), &mut c);
    assert_eq!(c.orders.len(), 1);
    assert_eq!((c.orders[0].side, c.orders[0].px.raw()), (Side::Bid, ask));
    // Inside the gap on both sides (σ̂ 15 %: bid 16, ask 18): nothing.
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    quote_call(&mut s, &mut c, 0.16, 0.18, 0);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.is_empty());
    assert_eq!(s.counters().judged, 2);
}

#[test]
fn the_event_law_refuses_an_event_in_the_life_an_old_calendar_and_none() {
    let run = |cal: HcvEvents, law: bool| {
        let mut p = params();
        p.event_law = law;
        let mut s = member_with(&p);
        s.events = cal;
        let mut c = Rec::default();
        feed_sp(&mut s, &mut c, S_SP, 0);
        quote_call(&mut s, &mut c, 0.22, 0.24, 0);
        s.on_timer(ns(500), &mut c);
        (c.orders.len(), s.counters().skip_event)
    };
    assert_eq!(run(calendar(T0_MS - 60_000, &[(T0_MS + 3 * DAY, 0b01)]), true).0, 0, "SP500 event inside");
    assert_eq!(run(calendar(T0_MS - 60_000, &[(T0_MS + 3 * DAY, 0b10)]), true).0, 1, "a BTC event only");
    assert_eq!(run(calendar(T0_MS - 60_000, &[(T0_MS + 8 * DAY, 0b01)]), true).0, 1, "after expiry");
    assert_eq!(run(calendar(T0_MS - 3 * 3_600_000, &[]), true).0, 0, "older than 2 h");
    assert_eq!(run(HcvEvents::new(), true).0, 0, "no calendar at all");
    let mut short = calendar(T0_MS - 60_000, &[]);
    short.until_ms = T0_MS + 5 * DAY;
    assert_eq!(run(short, true).0, 0, "the calendar ends before expiry");
    assert_eq!(run(HcvEvents::new(), false).0, 1, "the law off");
    assert_eq!(run(HcvEvents::new(), true).1, 1, "counted once per judged option with a quote");
}

#[test]
fn stale_or_crossed_inputs_are_no_trade() {
    // The quote is 11 s old at the timer.
    let mut s = member();
    let mut c = Rec::default();
    quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    feed_sp(&mut s, &mut c, S_SP, 11_000);
    s.on_timer(ns(11_000), &mut c);
    assert!(c.orders.is_empty());
    // The hedge touch is stale (only the oracle is fresh).
    let mut s = member();
    let mut c = Rec::default();
    s.on_tick(&hl_tick(SP_HEDGE, S_SP - 1, S_SP + 1, 0), &mut c);
    s.on_venue_event(&mark(SP_HEDGE, S_SP, 20_000), &mut c);
    quote_call(&mut s, &mut c, 0.22, 0.24, 20_000);
    s.on_timer(ns(20_000), &mut c);
    assert!(c.orders.is_empty());
    assert!(s.counters().skip_stale >= 1);
    // A crossed quote.
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    let (b, a) = (px_at(true, S_SP, S_SP, 7.0, 0.30), px_at(true, S_SP, S_SP, 7.0, 0.25));
    s.on_tick(&hc_tick(0, b, a, 0), &mut c);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.is_empty());
    // A tick the ingress marked stale quotes nothing.
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    let mut t = hc_tick(0, px_at(true, S_SP, S_SP, 7.0, 0.22), px_at(true, S_SP, S_SP, 7.0, 0.24), 0);
    t.flags = core_types::TICK_FLAG_STALE;
    s.on_tick(&t, &mut c);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.is_empty());
}

/// A tick is a BBO change: a quiet quote the venue keeps re-pushing emits
/// none, so the Hypercall FEED's liveness vouches for it — and a touch the
/// HL ingress marked stale is none.
#[test]
fn a_quiet_quote_stands_while_the_feed_lives_and_a_stale_touch_is_none() {
    let mut s = member();
    let mut c = Rec::default();
    quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    // 18 s later the call's quote never changed, but another instrument
    // ticked 9 s ago: the feed is alive, the quote stands.
    s.on_tick(&hc_tick(2, 1_000_000, 1_100_000, 9_000), &mut c);
    feed_sp(&mut s, &mut c, S_SP, 18_000);
    s.on_timer(ns(18_000), &mut c);
    assert_eq!(c.orders.len(), 1, "{:?}", s.counters());
    // A stale-flagged touch for the hedge perp: no touch, no new risk.
    let mut s = member();
    let mut c = Rec::default();
    s.on_venue_event(&mark(SP_HEDGE, S_SP, 0), &mut c);
    let mut t = hl_tick(SP_HEDGE, S_SP - 500_000, S_SP + 500_000, 0);
    t.flags = core_types::TICK_FLAG_STALE;
    s.on_tick(&t, &mut c);
    quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.is_empty());
    assert_eq!(s.counters().skip_stale, 2, "the call and the unquoted put");
}

#[test]
fn a_forecast_is_required() {
    let mut s = member();
    s.set_har_view(&[]);
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.is_empty());
    assert_eq!(s.counters().skip_forecast, 1);
}

#[test]
fn one_ioc_per_instrument_until_its_verdict() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    assert_eq!(c.orders.len(), 1);
    let first = c.orders[0];
    // Still rich, a new quote: no second order while the first is out.
    feed_sp(&mut s, &mut c, S_SP, 1_000);
    quote_call(&mut s, &mut c, 0.23, 0.25, 1_000);
    s.on_timer(ns(1_500), &mut c);
    assert_eq!(c.orders.len(), 1);
    // The paper law's miss frees it (an event about another oid does not).
    let ev = |oid| {
        OrderEvent::new(
            ns(2_500),
            VenueId::Hypercall,
            opt_sym(0),
            oid,
            7,
            core_types::ORDER_EVENT_CANCELED,
            core_types::ORDER_EVENT_REASON_EXPIRED,
            0,
        )
    };
    s.on_order_event(&ev(first.client_oid + 99), &mut c);
    feed_sp(&mut s, &mut c, S_SP, 2_500);
    s.on_timer(ns(3_000), &mut c);
    assert_eq!(c.orders.len(), 1);
    s.on_order_event(&ev(first.client_oid), &mut c);
    feed_sp(&mut s, &mut c, S_SP, 3_500);
    s.on_timer(ns(4_000), &mut c);
    assert_eq!(c.orders.len(), 2);
    assert_ne!(c.orders[1].client_oid, first.client_oid);
    // No verdict at all: released after HCV_PENDING_NS.
    feed_sp(&mut s, &mut c, S_SP, 5_000);
    s.on_timer(ns(5_000), &mut c);
    assert_eq!(c.orders.len(), 2);
    feed_sp(&mut s, &mut c, S_SP, 9_000);
    s.on_timer(ns(9_000), &mut c);
    assert_eq!(c.orders.len(), 3);
}

#[test]
fn a_refused_submit_is_counted_and_holds_nothing() {
    let mut s = member();
    let mut c = Rec {
        refuse: true,
        ..Rec::default()
    };
    feed_sp(&mut s, &mut c, S_SP, 0);
    quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    assert_eq!((s.counters().ctx_refused, s.dropped, s.counters().sells), (1, 1, 0));
    c.refuse = false;
    feed_sp(&mut s, &mut c, S_SP, 1_000);
    s.on_timer(ns(1_500), &mut c);
    assert_eq!(c.orders.len(), 1, "nothing pending: the next timer tries again");
}

// ---- sizing and the caps -----------------------------------------------

#[test]
fn the_stress_law_halves_the_size_until_the_10_sigma_day_fits() {
    let mut p = params();
    p.tail_loss_usd_1e6 = 20_000_000;
    let mut s = member_with(&p);
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    let (bid, _) = quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    assert_eq!(c.orders.len(), 1);
    let q = c.orders[0].qty.raw();
    let clip_q = div_1e6(5_000_000, bid) / 1_000 * 1_000;
    assert!(q < clip_q, "the clip alone would lose more than $20 on a 10σ day");
    let now_ms = T0_MS + 500;
    assert!(s.stress_loss(0, 0, -q, now_ms) <= 20_000_000);
    assert!(s.stress_loss(0, 0, -2 * q, now_ms) > 20_000_000, "the halving stopped at the first fit");
    // A cap no size can meet: nothing.
    let mut p = params();
    p.tail_loss_usd_1e6 = 1;
    let mut s = member_with(&p);
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.is_empty());
    assert_eq!(s.counters().skip_caps, 1);
}

#[test]
fn the_vega_and_premium_caps_bind_new_risk_but_a_reduce_passes() {
    // The premium cap at one clip: the second sale finds no room.
    let mut p = params();
    p.premium_cap_usd_1e6 = 5_000_000;
    let mut s = member_with(&p);
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    let (bid, _) = quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    let o = c.orders[0];
    s.on_fill(&fill(o.sym, Side::Ask, bid, o.qty.raw(), o.client_oid, 1_000), &mut c);
    assert_eq!(s.position_1e6(opt_sym(0)), -o.qty.raw());
    feed_sp(&mut s, &mut c, S_SP, 2_000);
    quote_call(&mut s, &mut c, 0.22, 0.24, 2_000);
    let before = c.orders.len();
    s.on_timer(ns(2_000), &mut c);
    let sold_again = c.orders[before..].iter().any(|x| x.venue == VenueId::Hypercall as u8);
    assert!(!sold_again, "no premium room");
    assert!(s.counters().skip_caps >= 1);
    // The quote turns cheap: buying back reduces, so the cap does not bind,
    // and the buy stops at flat.
    feed_sp(&mut s, &mut c, S_SP, 3_000);
    let (_, ask) = quote_call(&mut s, &mut c, 0.07, 0.09, 3_000);
    let before = c.orders.len();
    s.on_timer(ns(3_000), &mut c);
    let buy = c.orders[before..]
        .iter()
        .find(|x| x.venue == VenueId::Hypercall as u8)
        .copied()
        .expect("the buy-back");
    assert_eq!((buy.side, buy.px.raw(), buy.qty.raw()), (Side::Bid, ask, o.qty.raw()));
    // The vega cap: $1/pt holds a fraction of a contract of ~$3.7/pt.
    let mut p = params();
    p.vega_cap_usd_1e6 = 1_000_000;
    p.clip_usd_1e6 = 1_000_000_000;
    p.premium_cap_usd_1e6 = 1_000_000_000;
    let mut s = member_with(&p);
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    let q = c.orders[0].qty.raw();
    let g = bs::price(true, 6600.0, 6600.0, (7.0 - 0.5 / 86_400.0) / 365.0, 0.15).unwrap();
    let vega = q as f64 / 1e6 * g.vega_pt;
    assert!(vega <= 1.0 + 1e-6 && vega > 0.99, "vega ${vega}/pt against a $1 cap");
}

// ---- fills and the hedge -------------------------------------------------

#[test]
fn a_fill_books_position_and_cash_and_the_hedge_follows_outside_the_band() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    let (bid, _) = quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.iter().all(|o| o.venue == VenueId::Hypercall as u8), "never a hedge before a fill");
    let o = c.orders[0];
    let q = o.qty.raw();
    s.on_fill(&fill(o.sym, Side::Ask, bid, q, o.client_oid, 2_500), &mut c);
    assert_eq!(s.position_1e6(opt_sym(0)), -q);
    assert_eq!(s.cash_usd_1e6(), mul_1e6(q, bid));
    assert_eq!(s.counters().option_fills, 1);
    // A fair quote (no new sale) and fresh inputs: the short call's delta
    // (≈ −0.5 a contract) is far outside ±0.10 a contract — buy the perp.
    feed_sp(&mut s, &mut c, S_SP, 3_000);
    quote_call(&mut s, &mut c, 0.15, 0.17, 3_000);
    let n0 = c.orders.len();
    s.on_timer(ns(3_000), &mut c);
    assert_eq!(c.orders.len(), n0 + 1);
    let h = c.orders[n0];
    assert_eq!((h.venue, h.sym, h.side, h.kind), (VenueId::Hyperliquid as u8, SP_HEDGE, Side::Bid, core_fill::ORDER_KIND_IOC));
    let lot = hl_px::hl_perp_lot_1e6(2);
    assert_eq!(h.qty.raw() % lot, 0, "on the lot");
    let (delta, _) = s.exposure(0, T0_MS + 3_000, ns(3_000)).expect("priced");
    assert!((h.qty.raw() + delta).abs() < lot, "the hedge flattens the delta to within one lot");
    let want_px = hl_px::hl_perp_round_px(((S_SP + 500_000) as f64 * 1.001) as i64, 2, true);
    assert_eq!(h.px.raw(), want_px, "the touch plus the slip, rounded up by the HL law");
    assert!(hl_px::hl_perp_px_legal(h.px.raw(), 2));
    assert_eq!(s.counters().hedges, 1);
    // The hedge fills: the delta sits inside the band and nothing more goes.
    fill_hedges(&mut s, &mut c, n0, 3_500);
    assert_eq!(s.hedge_position_1e6(0), h.qty.raw());
    assert_eq!(s.cash_usd_1e6(), mul_1e6(q, bid) - mul_1e6(h.qty.raw(), h.px.raw()));
    feed_sp(&mut s, &mut c, S_SP, 4_000);
    let n1 = c.orders.len();
    s.on_timer(ns(4_000), &mut c);
    assert_eq!(c.orders.len(), n1);
    assert_eq!(s.counters().hedge_fills, 1);
}

#[test]
fn a_straddle_sits_inside_the_band_and_is_not_hedged() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 500_000, 1, 0), &mut c);
    s.on_fill(&fill(opt_sym(1), Side::Ask, 80_000_000, 500_000, 2, 0), &mut c);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.iter().all(|o| o.venue != VenueId::Hyperliquid as u8));
    let (delta, _) = s.exposure(0, T0_MS + 500, ns(500)).expect("priced");
    assert!(delta.abs() <= mul_1e6(100_000, s.gross_contracts(0, T0_MS + 500)), "{delta}");
}

#[test]
fn a_hedge_below_the_venue_minimum_is_not_sent() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    // 0.002 contracts short: ~0.001 underlying of delta, ~$6.6 — under $10.
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 2_000, 1, 0), &mut c);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.iter().all(|o| o.venue != VenueId::Hyperliquid as u8));
}

// ---- the final window, settlement ---------------------------------------

/// Long 1 ATM call expiring at T0 + 12 h, hedged, carried through the
/// final window to its settlement; every hedge fills at its limit.
#[test]
fn the_final_window_unwinds_the_hedge_in_slices_and_settlement_books_the_payoff() {
    let mut s = member();
    let mut c = Rec::default();
    let exp = DAY / 2;
    let start = exp - 40 * 60_000;
    feed_sp(&mut s, &mut c, S_SP, start);
    s.on_fill(&fill(opt_sym(3), Side::Bid, 30_000_000, 1_000_000, 1, start), &mut c);
    let cash0 = s.cash_usd_1e6();
    assert_eq!(cash0, -30_000_000);
    let oracle = 6_700_000_000; // settles 100 in the money
    let mut seen = 0usize;
    let mut t = start;
    while t <= exp + 61_000 {
        feed_sp(&mut s, &mut c, oracle, t);
        s.on_timer(ns(t), &mut c);
        seen = fill_hedges(&mut s, &mut c, seen, t);
        if t == exp + 2_000 {
            // At T the option leaves the delta and the band: the last
            // slice's residual is closed there, not at the booking.
            assert_eq!(s.hedge_position_1e6(0), 0, "the residual closes at T");
        }
        t += 1_000;
    }
    let k = s.counters();
    assert_eq!(s.position_1e6(opt_sym(3)), 0, "settled");
    assert_eq!(k.settlements, 1);
    assert_eq!(k.settle_fallbacks, 0, "the member sampled the whole window");
    assert!(k.unwind_slices >= 25, "one-minute slices across the window: {}", k.unwind_slices);
    assert_eq!(s.hedge_position_1e6(0), 0, "the hedge is gone once the option is");
    // The payoff: long 1 × (6700 − 6600) received; what is left is the
    // hedge — one unit sold at the bid less the slip, bought back across
    // the window at the ask plus the slip: ~$14.5 of crossing, no more.
    let hedge_cash = s.cash_usd_1e6() - cash0 - 100_000_000;
    assert!(hedge_cash < 0 && hedge_cash > -20_000_000, "the hedge's crossing only: {hedge_cash}");
    let slot_live = s.settle.iter().any(|x| x.live);
    assert!(!slot_live, "the window is freed");
}

#[test]
fn a_position_never_sampled_settles_on_the_last_oracle_and_is_counted() {
    let mut s = member();
    let mut c = Rec::default();
    let exp = DAY / 2;
    s.on_fill(&fill(opt_sym(3), Side::Ask, 30_000_000, 2_000_000, 1, 0), &mut c);
    // The member sleeps through the window: one timer after it.
    feed_sp(&mut s, &mut c, 6_500_000_000, exp + 70_000);
    s.on_timer(ns(exp + 70_000), &mut c);
    assert_eq!(s.position_1e6(opt_sym(3)), 0);
    let k = s.counters();
    assert_eq!((k.settlements, k.settle_fallbacks), (1, 1));
    // A short 2 × call struck at 6600 settling at 6500: worthless — the
    // premium is kept.
    assert_eq!(s.cash_usd_1e6(), 60_000_000);
}

#[test]
fn a_put_settles_on_the_median_of_means_of_the_window_sampled() {
    let mut p = params();
    p.options[3].call = false;
    let mut s = member_with(&p);
    let mut c = Rec::default();
    let exp = DAY / 2;
    let start = exp - 31 * 60_000;
    s.on_fill(&fill(opt_sym(3), Side::Bid, 10_000_000, 1_000_000, 1, start), &mut c);
    // The oracle walks down 6600 → 6420 across the window, 0.1 a second.
    let mut t = start;
    let mut expect = core_settle::SettleWindow::new(T0_MS + exp);
    while t <= exp + 61_000 {
        let px = if t < exp - 30 * 60_000 {
            S_SP
        } else {
            S_SP - (t - (exp - 30 * 60_000)) as i64 * 100
        };
        let px = px.max(6_420_000_000);
        feed_sp(&mut s, &mut c, px, t);
        if t <= exp {
            let _ = expect.push(T0_MS + t, px);
        }
        // No hedge fills: the hedge stays out of the arithmetic.
        s.on_timer(ns(t), &mut c);
        t += 1_000;
    }
    let mut scratch = [0i64; GRID_POINTS];
    let s_t = expect.settle_1e6(BucketOrder::Sorted, &mut scratch).expect("a price");
    assert!(s_t < S_SP && s_t > 6_420_000_000);
    assert_eq!(s.counters().settlements, 1);
    assert_eq!(s.cash_usd_1e6(), -10_000_000 + (S_SP - s_t), "long 1 put pays K − S_T");
}

// ---- review regressions (HC11 review, 2026-09-26) --------------------------

/// M2: every cap reads the IoC in flight — one per underlying at a time,
/// and its new risk counts in the gross premium until its verdict.
#[test]
fn one_ioc_in_flight_per_underlying_and_its_premium_counts() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    let (bid, _) = quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    // The put is rich too.
    let (pb, pa) = (px_at(false, S_SP, S_SP, 7.0, 0.22), px_at(false, S_SP, S_SP, 7.0, 0.24));
    s.on_tick(&hc_tick(1, pb, pa, 0), &mut c);
    s.on_timer(ns(500), &mut c);
    assert_eq!(c.orders.len(), 1, "one IoC in flight on SP500");
    let q = c.orders[0].qty.raw();
    assert_eq!(s.gross_premium(), mul_1e6(q, bid), "the in-flight sale's premium counts");
    // Its verdict (a miss) frees the underlying: the put goes next.
    let ev = OrderEvent::new(ns(2_600), VenueId::Hypercall, opt_sym(0), c.orders[0].client_oid, 7, core_types::ORDER_EVENT_CANCELED, core_types::ORDER_EVENT_REASON_EXPIRED, 0);
    s.on_order_event(&ev, &mut c);
    assert_eq!(s.gross_premium(), 0);
    feed_sp(&mut s, &mut c, S_SP, 3_000);
    s.on_timer(ns(3_000), &mut c);
    assert_eq!(c.orders.len(), 2);
    assert_eq!(c.orders[1].sym, opt_sym(0), "the call again: first in the table, still rich");
}

/// M3: a reduce passes the premium cap but not the vega law — selling one
/// leg of a vega-neutral pair may not push |net vega| past the cap.
#[test]
fn a_reduce_may_not_raise_net_vega_past_the_cap() {
    let mut p = params();
    p.vega_cap_usd_1e6 = 500_000; // $0.5/pt
    let mut s = member_with(&p);
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    // Short one ATM call, long one ATM put: net vega ≈ 0.
    s.on_fill(&fill(opt_sym(0), Side::Ask, 55_000_000, 1_000_000, 101, 0), &mut c);
    s.on_fill(&fill(opt_sym(1), Side::Bid, 55_000_000, 1_000_000, 102, 0), &mut c);
    // The put turns rich: selling it is a reduce.
    let (pb, pa) = (px_at(false, S_SP, S_SP, 7.0, 0.22), px_at(false, S_SP, S_SP, 7.0, 0.24));
    s.on_tick(&hc_tick(1, pb, pa, 0), &mut c);
    s.on_timer(ns(500), &mut c);
    let sale = c.orders.iter().find(|o| o.venue == VenueId::Hypercall as u8).copied().expect("a partial reduce");
    assert_eq!((sale.sym, sale.side), (opt_sym(1), Side::Ask));
    let g = bs::price(false, 6600.0, 6600.0, (7.0 - 0.5 / 86_400.0) / YEAR_D, 0.15).unwrap();
    let vega_after = sale.qty.raw() as f64 / 1e6 * g.vega_pt;
    assert!(vega_after <= 0.5 + 1e-3, "net vega after the sale ${vega_after}/pt against $0.5");
    assert!(sale.qty.raw() < 1_000_000, "not the whole put");
}

/// MINOR 1: an expired position waiting for its booking is marked at its
/// intrinsic, never at the premium it traded at — no phantom loss.
#[test]
fn an_expired_unbooked_position_is_marked_at_intrinsic() {
    let mut s = member();
    let mut c = Rec::default();
    let exp = DAY / 2;
    // Sold the 12 h call at $30; it expires out of the money (S 6500).
    s.on_fill(&fill(opt_sym(3), Side::Ask, 30_000_000, 1_000_000, 1, 0), &mut c);
    feed_sp(&mut s, &mut c, 6_500_000_000, exp + 30_000);
    s.on_timer(ns(exp + 30_000), &mut c);
    assert_eq!(s.position_1e6(opt_sym(3)), -1_000_000, "not booked before the delay");
    assert_eq!(s.counters().pnl_usd_1e6, 30_000_000, "worthless at expiry: the premium kept");
}

/// MINOR 2: another venue's ordinal 513 is not a Hypercall option — its
/// unattributed fill is not ours.
#[test]
fn another_venues_ordinal_is_not_an_option_of_ours() {
    let mut s = member();
    let mut c = Rec::default();
    let deribit = make_symbol_id(VenueId::Deribit, 513);
    s.on_fill(&fill(deribit, Side::Bid, 1_000_000, 1_000_000, 1, 0), &mut c);
    assert_eq!((s.position_1e6(opt_sym(0)), s.cash_usd_1e6()), (0, 0));
    let mut p = params();
    p.options[0].sym = deribit;
    assert!(HcvStrategy::new().configure(&p).is_err());
}

/// MINOR 4: the day stop blocks new risk, not a buy-back.
#[test]
fn the_day_stop_lets_a_buy_back_through() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_timer(ns(0), &mut c);
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 100_000, 1, 0), &mut c);
    s.cash_usd_1e6 -= 40_000_000; // deep in the day's loss
    feed_sp(&mut s, &mut c, S_SP, 1_000);
    let (_, ask) = quote_call(&mut s, &mut c, 0.07, 0.09, 1_000);
    s.on_timer(ns(1_000), &mut c);
    let buy = c.orders.iter().find(|o| o.venue == VenueId::Hypercall as u8).copied().expect("the buy-back");
    assert_eq!((buy.side, buy.px.raw(), buy.qty.raw()), (Side::Bid, ask, 100_000));
}

/// MINOR 5: a hedge IoC carries the member's own release as its ttl — the
/// paper law expires it then, so it never fills beside its successor.
#[test]
fn a_hedge_ioc_expires_when_the_member_releases_it() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 1_000_000, 1, 0), &mut c);
    s.on_timer(ns(500), &mut c);
    let h = c.orders.iter().find(|o| o.venue == VenueId::Hyperliquid as u8).copied().expect("a hedge");
    assert_eq!(h.ttl_ns, HCV_PENDING_NS);
}

/// MINOR 6: a held option that cannot be priced (no live two-sided quote,
/// no σ̂) leaves the delta unknown — the hedge is held, not unwound.
#[test]
fn an_unpriceable_held_option_holds_the_hedge() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 1_000_000, 1, 0), &mut c);
    s.on_fill(&fill(SP_HEDGE, Side::Bid, S_SP, 500_000, 2, 0), &mut c);
    s.set_har_view(&[]); // σ̂ cold, and the call has no quote
    feed_sp(&mut s, &mut c, S_SP, 1_000);
    s.on_timer(ns(1_000), &mut c);
    assert!(c.orders.is_empty(), "no trade on a partial delta");
    assert_eq!(s.hedge_position_1e6(0), 500_000);
}

/// The premium at stake is the entry basis: buying back half a short at a
/// higher price lowers it (the old last-price basis raised it).
#[test]
fn the_premium_at_stake_is_the_entry_basis() {
    let mut s = member();
    let mut c = Rec::default();
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 1_000_000, 1, 0), &mut c);
    s.on_fill(&fill(opt_sym(0), Side::Ask, 60_000_000, 1_000_000, 2, 0), &mut c);
    assert_eq!(s.gross_premium(), 140_000_000, "2 contracts at an average $70");
    s.on_fill(&fill(opt_sym(0), Side::Bid, 90_000_000, 1_000_000, 3, 0), &mut c);
    assert_eq!(s.gross_premium(), 70_000_000, "1 contract left, still at $70");
    s.on_fill(&fill(opt_sym(0), Side::Bid, 90_000_000, 2_000_000, 4, 0), &mut c);
    assert_eq!(s.position_1e6(opt_sym(0)), 1_000_000, "flipped long");
    assert_eq!(s.gross_premium(), 90_000_000, "a flip starts the basis at its price");
}

// ---- the stops --------------------------------------------------------------

#[test]
fn the_day_loss_and_the_kill_stop_new_risk_but_never_the_hedge() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_timer(ns(0), &mut c); // the day's start P&L: 0
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 100_000, 1, 0), &mut c);
    s.cash_usd_1e6 -= 30_000_000; // a $30 loss on the day
    feed_sp(&mut s, &mut c, S_SP, 1_000);
    quote_call(&mut s, &mut c, 0.22, 0.24, 1_000);
    s.on_timer(ns(1_000), &mut c);
    assert!(c.orders.iter().all(|o| o.venue == VenueId::Hyperliquid as u8), "no new option risk");
    assert_eq!(c.orders.len(), 1, "the hedge still goes");
    assert!(s.counters().skip_stopped >= 1);
    assert!(s.counters().day_pnl_usd_1e6 <= -20_000_000);
    // kill = 1 at boot: the same.
    let mut p = params();
    p.kill = true;
    let mut s = member_with(&p);
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    quote_call(&mut s, &mut c, 0.22, 0.24, 0);
    s.on_timer(ns(500), &mut c);
    assert!(c.orders.is_empty());
    // The call is stopped (new risk); the unquoted put is stale first.
    assert_eq!((s.counters().skip_stopped, s.counters().skip_stale), (1, 1));
}

#[test]
fn the_gauges_publish_positions_vega_and_pnl() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 1_000_000, 1, 0), &mut c);
    s.on_timer(ns(500), &mut c);
    let k = s.counters();
    assert_eq!(k.positions, 1);
    assert!(k.vega_abs_usd_1e6 > 3_000_000 && k.vega_abs_usd_1e6 < 4_500_000, "{}", k.vega_abs_usd_1e6);
    // Sold at $80 against a 15 % mark (~$55): the mark shows the gain.
    assert!(k.pnl_usd_1e6 > 20_000_000 && k.pnl_usd_1e6 < 30_000_000, "{}", k.pnl_usd_1e6);
    let mut out = HcvCounters::default();
    s.hcv_counters(&mut out);
    assert_eq!(out, k);
}

#[test]
fn fills_on_other_symbols_are_not_ours() {
    let mut s = member();
    let mut c = Rec::default();
    s.on_fill(&fill(make_symbol_id(VenueId::Hyperliquid, 99), Side::Bid, 1_000_000, 1_000_000, 1, 0), &mut c);
    s.on_fill(&fill(opt_sym(9), Side::Bid, 1_000_000, 1_000_000, 1, 0), &mut c);
    assert_eq!(s.cash_usd_1e6(), 0);
    assert_eq!((s.counters().option_fills, s.counters().hedge_fills), (0, 0));
}

#[test]
fn the_money_helpers_saturate() {
    assert_eq!(mul_1e6(2_000_000, 3_000_000), 6_000_000);
    assert_eq!(mul_1e6(i64::MAX, i64::MAX), i64::MAX);
    assert_eq!(div_1e6(5_000_000, 2_000_000), 2_500_000);
    assert_eq!(div_1e6(1, 0), 0);
    assert_eq!(div_1e6(i64::MAX, 1), i64::MAX);
}

// ---- HC11b: the book across a restart ------------------------------------------

/// The fixture's chain numbered from `first`, as the next boot's discovery
/// would number it.
fn renumbered(first: u32) -> HcvParams {
    let mut p = params();
    let mut i = 0usize;
    while i < p.options.len() {
        p.options[i].sym = opt_sym(first + i as u32);
        i += 1;
    }
    p
}

fn rendered(s: &HcvStrategy) -> String {
    let mut t = String::new();
    assert!(s.render_hcv_state(&mut t), "a configured member renders");
    t
}

/// Every row kind but a window: short 3 SP500 calls at two prices, long
/// half a BTC call, the SP500 hedge, the day's opening mark.
fn traded_member() -> HcvStrategy {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_timer(ns(0), &mut c);
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 2_000_000, 1, 100), &mut c);
    s.on_fill(&fill(opt_sym(0), Side::Ask, 82_000_000, 1_000_000, 2, 200), &mut c);
    s.on_fill(&fill(opt_sym(4), Side::Bid, 2_000_000_000, 500_000, 3, 300), &mut c);
    s.on_fill(&fill(SP_HEDGE, Side::Bid, S_SP, 1_500_000, 4, 400), &mut c);
    s
}

fn is_blank(s: &HcvStrategy) -> bool {
    let mut held = false;
    let mut i = 0usize;
    while i < s.n_opts {
        held |= s.opts[i].pos_1e6 != 0;
        i += 1;
    }
    !held && s.cash_usd_1e6() == 0 && s.hedge_position_1e6(0) == 0 && s.counters().restored == 0 && s.n_opts == 5
}

#[test]
fn the_book_round_trips_by_contract_whatever_the_next_chain_numbers_it() {
    let a = traded_member();
    let text = rendered(&a);
    // The next boot's discovery numbered the same chain differently.
    let p2 = renumbered(40);
    let mut b = member_with(&p2);
    let r = b.restore_state(&text, T0_MS + 1_000).expect("restores");
    let want = HcvRestored {
        positions: 2,
        hedges: 1,
        cash_usd_1e6: a.cash_usd_1e6(),
        ..HcvRestored::default()
    };
    assert_eq!(r, want);
    assert_eq!(b.position_1e6(p2.options[0].sym), -3_000_000);
    assert_eq!(b.position_1e6(opt_sym(0)), 0, "the old ordinal names nothing now");
    assert_eq!(b.position_1e6(p2.options[4].sym), 500_000);
    assert_eq!(b.opts[0].avg_px_1e6, 80_666_666, "the entry basis");
    assert_eq!(b.opts[0].last_px_1e6, 82_000_000);
    assert_eq!(b.hedge_position_1e6(0), 1_500_000);
    assert_eq!(b.cash_usd_1e6(), 160_000_000 + 82_000_000 - 1_000_000_000 - 9_900_000_000);
    assert_eq!((b.day, b.day_start_pnl_usd_1e6), (a.day, a.day_start_pnl_usd_1e6));
    assert_eq!(b.gross_premium(), a.gross_premium(), "the premium cap reads the same book");
    assert_eq!(b.counters().restored, 2);
    assert_eq!(b.state_epoch, 0, "restoring is not a change");
    // Written back once the feeds are in, the book reads the same (the
    // marks come back with the feeds, never from the file).
    b.on_venue_event(&mark(SP_HEDGE, S_SP, 1_000), &mut Rec::default());
    assert_eq!(rendered(&b), text);
    assert!(b.restore_state(&text, T0_MS).is_err(), "once per boot");
}

#[test]
fn a_contract_the_new_chain_lacks_is_carried_hedged_and_settled_but_never_traded() {
    let mut a = member();
    let mut c = Rec::default();
    let exp = DAY / 2;
    a.on_fill(&fill(opt_sym(3), Side::Bid, 30_000_000, 1_000_000, 1, 0), &mut c);
    let text = rendered(&a);
    // The next chain lost it (its strike left the capped chain).
    let mut p2 = params();
    p2.options.remove(3);
    let mut b = member_with(&p2);
    let r = b.restore_state(&text, T0_MS + 1_000).expect("restores");
    assert_eq!((r.positions, r.orphans), (1, 1));
    assert_eq!(b.n_opts, 5, "the chain's four and the orphan");
    assert_eq!(b.opts[4].cfg.sym, SYMBOL_ID_NONE);
    // Carried: the gauges count it, its delta is hedged at σ̂ (no quote
    // reaches it), and nothing is ever sent on it.
    let mut c = Rec::default();
    feed_sp(&mut b, &mut c, S_SP, 1_000);
    b.on_timer(ns(1_000), &mut c);
    assert_eq!((b.counters().positions, b.counters().orphans), (1, 1));
    assert_eq!(c.orders.len(), 1, "the long call's delta, hedged");
    assert_eq!((c.orders[0].sym, c.orders[0].side), (SP_HEDGE, Side::Ask));
    // Sampled and settled at its expiry.
    let mut seen = fill_hedges(&mut b, &mut c, 0, 1_500);
    let mut t = exp - 31 * 60_000;
    while t <= exp + 61_000 {
        feed_sp(&mut b, &mut c, 6_700_000_000, t);
        b.on_timer(ns(t), &mut c);
        seen = fill_hedges(&mut b, &mut c, seen, t);
        t += 1_000;
    }
    let k = b.counters();
    assert_eq!((k.settlements, k.settle_fallbacks), (1, 0), "on its own window");
    assert_eq!((b.opts[4].pos_1e6, k.orphans, k.positions), (0, 0, 0));
    assert_eq!(b.hedge_position_1e6(0), 0, "the hedge left with it");
    assert!(c.orders.iter().all(|o| o.sym == SP_HEDGE), "only ever hedged");
}

#[test]
fn a_position_that_expired_while_the_engine_was_down_is_booked_at_the_first_oracle() {
    let mut a = member();
    let mut c = Rec::default();
    a.on_fill(&fill(opt_sym(3), Side::Ask, 30_000_000, 2_000_000, 1, 0), &mut c);
    let text = rendered(&a);
    let mut b = member();
    let back = DAY; // twelve hours after its expiry
    let r = b.restore_state(&text, T0_MS + back).expect("restores");
    assert_eq!((r.positions, r.expired, r.windows), (1, 1, 0));
    feed_sp(&mut b, &mut c, 6_500_000_000, back);
    b.on_timer(ns(back), &mut c);
    assert_eq!(b.position_1e6(opt_sym(3)), 0);
    assert_eq!((b.counters().settlements, b.counters().settle_fallbacks), (1, 1), "no window: counted");
    // Short 2 calls struck at 6600, settling at 6500: the premium is kept.
    assert_eq!(b.cash_usd_1e6(), 60_000_000);
}

/// Long 1 call through its final window; the engine goes down with ten
/// minutes to go and is back a minute later.
#[test]
fn a_restart_inside_the_final_window_keeps_its_grid_and_holds_nothing_across_the_gap() {
    let exp = DAY / 2;
    let start = exp - 31 * 60_000;
    let down = exp - 10 * 60_000;
    // An oracle that moves every 3 s (HL's cadence); no hedge touch, so
    // the member only samples.
    let px = |t: u64| S_SP + ((t / 3_000) % 97) as i64 * 1_000_000;
    let mut a = member();
    let mut c = Rec::default();
    a.on_fill(&fill(opt_sym(3), Side::Bid, 30_000_000, 1_000_000, 1, start), &mut c);
    let mut t = start;
    while t < down {
        a.on_venue_event(&mark(SP_HEDGE, px(t), t), &mut c);
        a.on_timer(ns(t), &mut c);
        t += 1_000;
    }
    // COPY: the crashed process's grid (≤ 14.4 KiB), once — test-only (this
    // file is the member's `#[cfg(test)]` module) — the window it lends
    // goes on being written.
    let kept = a.settle.iter().find(|x| x.live).expect("a window").window.samples().to_vec();
    // Instants T−30 min + 1 s … the one before the last print (a print
    // fills its instant at the next one).
    assert_eq!(kept.len(), 1_198);
    let text = rendered(&a);
    let runs = text.lines().filter(|l| l.starts_with("S\t")).count();
    assert!(runs > 0 && runs < kept.len(), "run-length: {runs} rows");
    let mut b = member();
    let back = down + 60_000;
    let r = b.restore_state(&text, T0_MS + back).expect("restores");
    assert_eq!((r.positions, r.windows), (1, 1));
    let mut t = back;
    while t <= exp + 61_000 {
        b.on_venue_event(&mark(SP_HEDGE, px(t), t), &mut c);
        b.on_timer(ns(t), &mut c);
        t += 1_000;
    }
    let k = b.counters();
    assert_eq!((k.settlements, k.settle_fallbacks), (1, 0), "29 minutes of 30 is the venue's estimate");
    let w = &b.settle.iter().find(|x| x.exp_ms == T0_MS + exp).expect("the window").window;
    let grid = w.samples();
    // The dark minute is missing — not held — and so is the instant the
    // last print before the crash was waiting to fill.
    assert_eq!(grid.len(), GRID_POINTS - 61);
    assert_eq!(grid[..kept.len()], kept[..]);
    let mut scratch = [0i64; GRID_POINTS];
    let s_t = core_settle::median_of_means(grid, BucketOrder::Sorted, &mut scratch).expect("a price");
    assert_eq!(b.cash_usd_1e6(), -30_000_000 + (s_t - S_SP).max(0), "booked on that grid");
}

#[test]
fn a_fixed_window_keeps_only_its_price() {
    let exp = DAY / 2;
    let mut a = member();
    let mut c = Rec::default();
    a.on_fill(&fill(opt_sym(3), Side::Bid, 30_000_000, 1_000_000, 1, 0), &mut c);
    let mut t = exp - 60_000;
    while t < exp - 50_000 {
        a.on_venue_event(&mark(SP_HEDGE, S_SP, t), &mut c);
        a.on_timer(ns(t), &mut c);
        t += 1_000;
    }
    // Fixed, not yet booked (an oracle that has not come back, say).
    let k = a.settle.iter().position(|x| x.live).expect("a window");
    a.settle[k].px_1e6 = 6_650_000_000;
    let text = rendered(&a);
    assert!(!text.lines().any(|l| l.starts_with("S\t")), "no grid for a fixed price");
    let mut b = member();
    b.restore_state(&text, T0_MS + exp).expect("restores");
    let s = b.settle.iter().find(|x| x.live).expect("the window");
    assert_eq!((s.exp_ms, s.px_1e6, s.window.points()), (T0_MS + exp, 6_650_000_000, 0));
    // Booked on the kept price once due.
    feed_sp(&mut b, &mut c, 6_600_000_000, exp + 61_000);
    b.on_timer(ns(exp + 61_000), &mut c);
    assert_eq!(b.cash_usd_1e6(), -30_000_000 + 50_000_000);
    assert_eq!(b.counters().settle_fallbacks, 0);
}

#[test]
fn the_epoch_moves_only_with_what_outlives_the_process() {
    let mut s = member();
    let mut c = Rec::default();
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_timer(ns(0), &mut c);
    let e = s.state_epoch;
    assert_eq!(e, 1, "the day's opening mark");
    // Quotes, oracles and quiet timers are not state.
    quote_call(&mut s, &mut c, 0.15, 0.17, 500);
    feed_sp(&mut s, &mut c, S_SP + 1_000_000, 1_000);
    s.on_timer(ns(1_000), &mut c);
    assert_eq!(s.state_epoch, e);
    // Fills are.
    s.on_fill(&fill(opt_sym(3), Side::Bid, 30_000_000, 1_000_000, 1, 1_000), &mut c);
    s.on_fill(&fill(SP_HEDGE, Side::Ask, S_SP, 500_000, 2, 1_000), &mut c);
    assert_eq!(s.state_epoch, e + 2);
    // A window opening is (T − 30 min − 5 s); a sample in the same minute
    // is not; the first sample of the next minute is.
    let open = DAY / 2 - 30 * 60_000 - 5_000;
    let mut t = open;
    while t < open + 5_000 {
        feed_sp(&mut s, &mut c, S_SP, t);
        s.on_timer(ns(t), &mut c);
        t += 1_000;
    }
    assert_eq!(s.state_epoch, e + 3, "opened, then four samples inside its first minute");
    feed_sp(&mut s, &mut c, S_SP, open + 5_000);
    s.on_timer(ns(open + 5_000), &mut c);
    assert_eq!(s.state_epoch, e + 4, "a new minute of the window");
}

#[test]
fn the_book_leaves_by_mailbox_when_it_moved_and_a_held_slot_is_offered_again() {
    let mut s = member();
    let mut c = Rec::default();
    let (tx, mut rx) = core_ring::Mailbox::new(HcvStateSnap::new_boxed()).split();
    s.install_state_outbox(tx);
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_timer(ns(0), &mut c);
    assert_eq!(rx.try_take().expect("the day's mark, handed").n_pos, 0);
    s.on_timer(ns(1_000), &mut c);
    assert!(rx.try_take().is_none(), "nothing moved, nothing handed");
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 1_000_000, 1, 1_500), &mut c);
    s.on_timer(ns(2_000), &mut c);
    // The writer is slow: the slot is still FULL when the next fill lands.
    s.on_fill(&fill(opt_sym(0), Side::Ask, 80_000_000, 1_000_000, 2, 2_500), &mut c);
    s.on_timer(ns(3_000), &mut c);
    {
        let t = rx.try_take().expect("the first book");
        assert_eq!((t.n_pos, t.pos[0].pos_1e6), (1, -1_000_000), "the older one");
    }
    s.on_timer(ns(4_000), &mut c);
    let t = rx.try_take().expect("offered again: the newest");
    assert_eq!((t.epoch, t.pos[0].pos_1e6), (s.state_epoch, -2_000_000));
    // What the writer renders is what the shutdown's forced write renders.
    let mut via_writer = String::new();
    render_state(&t, &mut via_writer);
    assert_eq!(via_writer, rendered(&s));
}

#[test]
fn a_file_the_member_cannot_read_exactly_refuses_and_changes_nothing() {
    let good = rendered(&traded_member());
    let exp = T0_MS + 7 * DAY;
    let p_row = format!("P\tSP500\t{exp}\t{S_SP}\tC\t1000000\t1\t1\n");
    let win = |px: i64, n: u32, rows: &str| format!("{good}W\tSP500\t{exp}\t{px}\t{n}\t{n}\t{}\n{rows}", exp - 1);
    let mut nine = good.clone();
    let mut k = 0u64;
    while k < 9 {
        nine.push_str(&format!("W\tSP500\t{}\t0\t0\t0\t0\n", exp + k * DAY));
        k += 1;
    }
    let mut orphans = good.clone();
    let mut k = 0i64;
    while k <= HCV_MAX_ORPHANS as i64 {
        orphans.push_str(&format!("P\tSP500\t{exp}\t{}\tC\t1000000\t1\t1\n", 1_000_000 + k));
        k += 1;
    }
    let first_p = good.lines().find(|l| l.starts_with("P\t")).expect("a P row");
    let d_line = good.lines().find(|l| l.starts_with("D\t")).expect("a D row");
    let bad: [(&str, String); 29] = [
        ("a version it does not read", good.replace("V\t1\n", "V\t2\n")),
        ("no V row", good.replace("V\t1\n", "")),
        ("V after C", good.replace("V\t1\n", "").replace("D\t", "V\t1\nD\t")),
        ("an unknown tag", format!("{good}Q\t1\n")),
        ("a second C", format!("{good}C\t5\n")),
        ("no D row", good.lines().filter(|l| !l.starts_with("D\t")).map(|l| format!("{l}\n")).collect()),
        ("a contract twice", format!("{good}{first_p}\n")),
        ("an untraded underlying", format!("{good}{}", p_row.replace("SP500", "BABA"))),
        ("a bad right", format!("{good}{}", p_row.replace("\tC\t", "\tX\t"))),
        ("a zero size", format!("{good}{}", p_row.replace("\t1000000\t", "\t0\t"))),
        ("a hedge twice", format!("{good}H\tSP500\t5\t0\n")),
        ("a run outside a window", format!("{good}S\t5\t1\n")),
        ("a grid short at the end", win(0, 3, "S\t5\t2\n")),
        ("a grid short before the next row", win(0, 3, "S\t5\t2\nH\tBTC\t1\n")),
        ("a run past its window", win(0, 2, "S\t5\t3\n")),
        ("a fixed window with a grid", win(6_600_000_000, 2, "S\t5\t2\n")),
        ("a window twice", format!("{}{}", win(0, 0, ""), &win(0, 0, "")[good.len()..])),
        ("more windows than the member samples", nine),
        ("a field too many", good.replacen("C\t", "C\t0\t", 1)),
        ("not an integer", good.replacen("C\t", "C\tabc", 1)),
        ("more orphans than the member carries", orphans),
        // Plausibility (HC11b review): nothing the arithmetic cannot carry.
        ("a size of i64::MIN", format!("{good}{}", p_row.replace("\t1000000\t1\t1", "\t-9223372036854775808\t1\t1"))),
        ("a size past 1e15", format!("{good}{}", p_row.replace("\t1000000\t1\t1", "\t1000000000000001\t1\t1"))),
        ("a strike past 1e15", format!("{good}{}", p_row.replace(&format!("\t{S_SP}\t"), "\t1000000000000001\t"))),
        ("an expiry past 2100", format!("{good}{}", p_row.replace(&format!("\t{exp}\t"), "\t4102444800001\t"))),
        ("a hedge of i64::MIN", good.replacen("H\tSP500\t1500000\t", "H\tSP500\t-9223372036854775808\t", 1)),
        ("an opening mark of i64::MIN", good.replace(d_line, "D\t1\t-9223372036854775808")),
        ("a real hedge on an untraded underlying", format!("{good}H\tBABA\t1000000\t150000000\n")),
        ("a window on an untraded underlying", format!("{good}W\tBABA\t{exp}\t0\t0\t0\t0\n")),
    ];
    for (why, text) in &bad {
        let mut s = member();
        let e = s.restore_state(text, T0_MS).expect_err(why);
        assert!(is_blank(&s), "{why}: the member is unchanged ({e})");
        assert!(!s.state_restored, "{why}");
    }
    // The line is named (the header's six comment lines, then V C D P P H).
    let mut s = member();
    let e = s.restore_state(&format!("{good}Q\t1\n"), T0_MS).unwrap_err();
    assert_eq!(e.line, 13);
    assert_eq!(e.to_string(), "hcv-state.tsv line 13: an unknown row tag");
    let e = HcvStrategy::new().restore_state(&good, T0_MS).unwrap_err();
    assert_eq!((e.line, e.to_string().as_str()), (0, "hcv-state.tsv: the book is restored after configure"));
}

// ---- HC11b review regressions ------------------------------------------------

/// MAJOR: right after a restore no Hyperliquid Mark has arrived, and a hedge
/// at a zero price is no mark. The day waits for a known one — and so does
/// new risk; once every held underlying's oracle is in, the day rolls on it.
#[test]
fn an_unknown_mark_after_a_restore_holds_the_day_and_stops_new_risk() {
    let mut a = member();
    let mut c = Rec::default();
    a.on_venue_event(&mark(BTC_HEDGE, S_BTC, 0), &mut c);
    feed_sp(&mut a, &mut c, S_SP, 0);
    a.on_timer(ns(0), &mut c);
    // A short BTC hedge residual: +$11 000 of cash, −$11 000 at its mark.
    a.on_fill(&fill(BTC_HEDGE, Side::Ask, S_BTC, 100_000, 1, 100), &mut c);
    let text = rendered(&a);
    // Back the next day: SP500's oracle and a rich quote arrive first.
    let mut b = member();
    b.restore_state(&text, T0_MS + DAY).expect("restores");
    b.events = calendar(T0_MS + DAY - 60_000, &[]);
    let mut c = Rec::default();
    feed_sp(&mut b, &mut c, S_SP, DAY);
    quote_call(&mut b, &mut c, 0.22, 0.24, DAY);
    b.on_timer(ns(DAY), &mut c);
    assert!(c.orders.is_empty(), "no new risk on an unknown mark: {:?}", c.orders);
    assert_eq!(b.counters().skip_stopped, 1);
    assert_eq!((b.day, b.day_start_pnl_usd_1e6), (a.day, a.day_start_pnl_usd_1e6), "the day waits");
    assert_eq!(b.counters().pnl_usd_1e6, 0, "no mark published on an unknown one");
    assert_eq!(b.counters().marks_unknown, 1, "BTC: a hedge and no oracle yet");
    // BTC's oracle arrives: the mark is known, the day rolls on it and the
    // sale goes.
    b.on_venue_event(&mark(BTC_HEDGE, S_BTC, DAY + 1_000), &mut c);
    feed_sp(&mut b, &mut c, S_SP, DAY + 1_000);
    quote_call(&mut b, &mut c, 0.22, 0.24, DAY + 1_000);
    b.on_timer(ns(DAY + 1_000), &mut c);
    assert_eq!(b.day, (T0_MS + DAY) / DAY);
    assert_eq!(b.day_start_pnl_usd_1e6, 0, "cash +11 000 against a hedge marked −11 000");
    assert_eq!(b.counters().marks_unknown, 0);
    assert_eq!(c.orders.iter().filter(|o| o.venue == VenueId::Hypercall as u8).count(), 1);
}

/// MINOR: an orphan with no σ̂ (its series went cold) no longer freezes its
/// underlying's hedge while its expiry is quoted: it borrows the nearest
/// quoted strike's implied vol. With nothing quoted, the hedge is held.
#[test]
fn an_orphan_without_a_forecast_is_priced_off_its_expirys_nearest_quote() {
    let exp = DAY / 2;
    let mut a = member();
    let mut c = Rec::default();
    a.on_fill(&fill(opt_sym(3), Side::Bid, 30_000_000, 1_000_000, 1, 0), &mut c);
    let text = rendered(&a);
    // The next chain lists the same expiry at another strike only.
    let mut p2 = params();
    p2.options[3].strike_1e6 = S_SP + 100_000_000;
    let mut b = member_with(&p2);
    b.restore_state(&text, T0_MS).expect("restores");
    assert_eq!(b.counters().restored, 1);
    b.set_har_view(&[]);
    let mut c = Rec::default();
    feed_sp(&mut b, &mut c, S_SP, 1_000);
    b.on_timer(ns(1_000), &mut c);
    assert!(c.orders.is_empty(), "nothing quoted and no σ̂: the hedge is held");
    // The neighbour strike quotes (~20 % vol, 12 h).
    let (bid, ask) = (
        px_at(true, S_SP, S_SP + 100_000_000, 0.5, 0.19),
        px_at(true, S_SP, S_SP + 100_000_000, 0.5, 0.21),
    );
    b.on_tick(&hc_tick(3, bid, ask, 2_000), &mut c);
    feed_sp(&mut b, &mut c, S_SP, 2_000);
    b.on_timer(ns(2_000), &mut c);
    assert_eq!(c.orders.len(), 1, "the long call's delta, hedged at its neighbour's vol");
    assert_eq!((c.orders[0].sym, c.orders[0].side), (SP_HEDGE, Side::Ask));
    let (delta, _) = b.exposure(0, T0_MS + 2_000, ns(2_000)).expect("priced now");
    assert!(delta > 400_000 && delta < 600_000, "an ATM call's delta: {delta}");
    assert!(exp > 2_000);
}

/// MINOR: a hedge residual under the venue's minimum on an underlying
/// hcv.toml no longer trades (one the member could never close) is
/// dropped and counted; one above it still refuses the boot.
#[test]
fn a_dust_hedge_on_an_underlying_no_longer_traded_is_dropped_but_a_real_one_refuses() {
    let good = rendered(&traded_member());
    let mut s = member();
    // 0.00005 BTC at $110 000: $5.50, under the $10 minimum.
    let dust = format!("{good}H\tETH\t50\t110000000000\n");
    let r = s.restore_state(&dust, T0_MS).expect("restores");
    assert_eq!((r.hedges, r.hedges_dropped), (1, 1));
    // The same size with no mark, or worth $11, refuses.
    for bad in [format!("{good}H\tETH\t50\t0\n"), format!("{good}H\tETH\t100\t110000000000\n")] {
        let mut s = member();
        let e = s.restore_state(&bad, T0_MS).unwrap_err();
        assert!(e.what.contains("a hedge on an underlying"), "{e}");
    }
}

/// MINOR: a book the writer stops taking — its file falls behind — stops new
/// risk after `HCV_BOOK_STALE_NS`; hedging goes on, and a taken book clears it.
#[test]
fn a_book_the_writer_stops_taking_stops_new_risk() {
    let mut s = member();
    let mut c = Rec::default();
    let (tx, mut rx) = core_ring::Mailbox::new(HcvStateSnap::new_boxed()).split();
    s.install_state_outbox(tx);
    feed_sp(&mut s, &mut c, S_SP, 0);
    s.on_timer(ns(0), &mut c);
    // The writer took the day's mark, then stopped: the slot stays FULL.
    drop(rx.try_take().expect("handed"));
    s.on_fill(&fill(opt_sym(1), Side::Ask, 80_000_000, 1_000_000, 1, 500), &mut c);
    s.on_timer(ns(1_000), &mut c);
    s.on_fill(&fill(opt_sym(1), Side::Ask, 80_000_000, 1_000_000, 2, 1_500), &mut c);
    let stale_at = 2_000 + HCV_BOOK_STALE_NS / 1_000_000;
    s.on_timer(ns(2_000), &mut c);
    assert!(!s.book_stale(ns(stale_at - 1)));
    assert!(s.book_stale(ns(stale_at)));
    // Rich quote after the stale point: no sale; the hedge still goes.
    feed_sp(&mut s, &mut c, S_SP, stale_at);
    quote_call(&mut s, &mut c, 0.22, 0.24, stale_at);
    let n0 = c.orders.len();
    s.on_timer(ns(stale_at), &mut c);
    assert!(c.orders[n0..].iter().all(|o| o.venue == VenueId::Hyperliquid as u8), "hedges only");
    assert_eq!(s.counters().book_stale, 1);
    assert!(s.counters().skip_stopped >= 1);
    // The writer takes it again: the newest book goes and the stop lifts.
    drop(rx.try_take().expect("the first book"));
    s.on_timer(ns(stale_at + 1_000), &mut c);
    assert_eq!(rx.try_take().expect("the newest book").pos[0].pos_1e6, -2_000_000);
    assert!(!s.book_stale(ns(stale_at + 1_000)));
    s.on_timer(ns(stale_at + 2_000), &mut c);
    assert_eq!(s.counters().book_stale, 0);
}

/// Re-verification: a restart before an underlying's first Mark writes back
/// the mark the book was restored with — never 0 — so the residual rule
/// still has its measure at the boot after; the live oracle replaces it.
#[test]
fn a_restored_hedge_keeps_its_mark_until_the_feed_brings_one() {
    let a = traded_member();
    let text = rendered(&a);
    let h_row = format!("H\tSP500\t1500000\t{S_SP}\n");
    assert!(text.contains(&h_row), "{text}");
    let mut b = member();
    b.restore_state(&text, T0_MS + 1_000).expect("restores");
    assert!(rendered(&b).contains(&h_row), "the kept mark, before any oracle");
    b.on_venue_event(&mark(SP_HEDGE, S_SP + 7_000_000, 2_000), &mut Rec::default());
    assert!(rendered(&b).contains(&format!("H\tSP500\t1500000\t{}\n", S_SP + 7_000_000)), "the live one");
    assert_eq!(b.und[0].oracle_1e6, S_SP + 7_000_000);
    // A mark kept is never a mark for the P&L.
    let mut c = member();
    c.restore_state(&text, T0_MS + 1_000).expect("restores");
    assert_eq!(c.marked_pnl(T0_MS + 1_000, ns(1_000)), None);
}
