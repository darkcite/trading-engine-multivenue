// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Binance plan §2.2 PB6 (G5, I-PB5, OD-8) and PB12 (OD-12): venue-originated fills and the live
//! model fee, with the scripted `Ctx` double (`common::RecCtx`). Only a test builds a `Live`
//! route with a permit (pin:pl2-live-permit); the cli never does.
//!
//! - An ADOPTED fill (`FILL_FLAG_ADOPTED`) is counted `fills_adopted`, never booked.
//! - A venue fill with no member oid (`order_id == 0`, origin VENUE: liquidation, ADL, orphan,
//!   settlement) is counted `fills_venue_originated`, never booked, and disables the script on
//!   its symbol WITHOUT a flatten (one `VENUE_FILL` incident) and CANCELS its resting makers
//!   (risk review C3; a failed cancel is re-sent at the next confirmed bar); on an untraded
//!   symbol it is only counted.
//! - A member oid of another boot stays `fills_prior_boot`.
//! - A live fill without a noted fee books the model and counts `fee_modelled_live`.

mod common;

use common::*;
use core_config::pine::{BootExitPolicy, PineLimitPrimitive, PineMode, PineScriptCfg};
use core_types::{
    Fill, Order, Qty, Side, VenueId, FILL_FLAG_ADOPTED, FILL_ORIGIN_PAPER, FILL_ORIGIN_VENUE,
};
use strategy_core::{pine_incident, PineIncident, Strategy, StrategyCounters, SubmitErr};
use strategy_pine::oid::encode_client_oid;
use strategy_pine::{PineBootCfg, PineStrategy, SlotCaps8, SlotRoute, VenueBootState};

fn rt(file: &str, coin: usize) -> PineScriptCfg {
    let mut s = script(file, coin);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = 0;
    s
}

fn warm() -> Vec<strategy_pine::WarmSlice> {
    vec![
        slice(0, 60, warm_bars(60, 50, 0)),
        slice(1, 60, warm_bars(60, 50, 0)),
    ]
}

/// A Live boot that passes every §4.1.6 Live check, booted flat. The permit names the chart
/// venue of `common`'s facts (Hyperliquid); PB6 / PB12 are venue-agnostic in the member.
fn live_cfg(scripts: Vec<PineScriptCfg>) -> PineBootCfg {
    let mut b = boot(scripts);
    b.route = SlotRoute::Live;
    b.live_venue = Some(VenueId::Hyperliquid);
    b.slot.min_coalesce_ms = Some(0);
    b.slot.event_insn_budget = Some(100_000_000);
    b.slot.boot_exit_policy = Some(BootExitPolicy::Accept);
    b.slot_caps = SlotCaps8 {
        max_order_usd_1e6: 100_000_000,
        cap_instance_usd_1e6: 100_000_000,
        cap_day_usd_1e6: 1_000_000_000,
        max_open_orders: 8,
    };
    b.boot_state = (0..COINS.len())
        .map(|c| VenueBootState {
            sym: sym(c),
            position_1e6: 0,
            avg_px_1e6: 0,
            day_notional_1e6: 0,
            resting_swept: true,
        })
        .collect();
    b
}

fn live_member() -> PineStrategy {
    start(live_cfg(vec![rt("live_entry.pine", 0)]), warm())
}

/// The live_entry IoC at E = `T0 + 20` ms.
fn first_ioc(p: &mut PineStrategy, ctx: &mut RecCtx) -> Order {
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), ctx);
    p.on_tick(&quote(0, T0 + 20), ctx);
    assert_eq!(ctx.n, 1, "one IoC");
    ctx.seen()[0].order
}

/// A slot-8 venue fill of `order_id` on `coin` at UTC ms `t`.
fn venue(coin: usize, side: Side, qty_1e6: i64, order_id: u64, t: i64) -> Fill {
    let mut f = Fill::new(
        u64::try_from(t).expect("t") * 1_000_000,
        sym(coin),
        side,
        core_types::Price::from_raw(100_000_000),
        Qty::from_raw(qty_1e6),
        order_id,
    );
    f.strategy_id = 8;
    f.origin = FILL_ORIGIN_VENUE;
    f
}

fn incidents(p: &PineStrategy) -> Vec<PineIncident> {
    let mut out = [PineIncident::default(); 8];
    let n = p.pine_incidents_view(&mut out, 0) as usize;
    out[..n].to_vec()
}

/// PB6: an adopted fill is counted, never booked — no broker position, no fault, no incident.
#[test]
fn an_adopted_fill_is_counted_never_booked() {
    let mut p = live_member();
    let mut ctx = RecCtx::new();
    let mut f = venue(0, Side::Bid, 2_000_000, 0, T0 + 5);
    f.flags |= FILL_FLAG_ADOPTED;
    p.on_fill(&f, &mut ctx);
    let k = p.pine_counters();
    assert_eq!(
        (
            k.fills_adopted,
            k.fills,
            k.fills_prior_boot,
            k.fills_venue_originated
        ),
        (1, 0, 0, 0)
    );
    assert_eq!((k.scripts_faulted, k.incidents), (0, 0));
    let probe = p.rt_probe().expect("core");
    assert_eq!(probe.script_pos[0], 0, "never booked");
    assert!(!probe.script_run[0].1, "not disabled");
}

/// PB6 (OD-8): a liquidation fill (oid 0, origin VENUE) on the script's symbol is never booked
/// and disables the script with ONE `VENUE_FILL` incident and NO flatten — nothing is dispatched
/// for it afterwards, whatever the ticks.
#[test]
fn a_liquidation_fill_disables_its_script_without_a_flatten() {
    let mut p = live_member();
    let mut ctx = RecCtx::new();
    let o = first_ioc(&mut p, &mut ctx);
    // The entry fills whole: the broker holds the position.
    let mut f = venue(0, o.side, o.qty.raw(), o.client_oid, T0 + 25);
    f.px = o.px;
    p.on_fill(&f, &mut ctx);
    let pos = p.rt_probe().expect("core").script_pos[0];
    assert_eq!(pos, o.qty.raw(), "booked through on_fill");
    // The venue liquidates it.
    let liq = venue(0, Side::Ask, o.qty.raw(), 0, T0 + 40);
    p.on_fill(&liq, &mut ctx);
    let k = p.pine_counters();
    assert_eq!((k.fills_venue_originated, k.fills), (1, 1));
    assert_eq!(
        (k.scripts_faulted, k.incidents, k.flattens_started),
        (1, 1, 0),
        "disabled, one incident, no flatten"
    );
    let probe = p.rt_probe().expect("core");
    assert!(probe.script_run[0].1, "disabled");
    assert_eq!(
        probe.script_pos[0], pos,
        "the broker never books the venue's fill"
    );
    let inc = incidents(&p);
    assert_eq!(inc.len(), 1);
    assert_eq!(
        (inc[0].event, inc[0].script, inc[0].position_1e6),
        (pine_incident::VENUE_FILL, 0, pos)
    );
    // Nothing is dispatched afterwards (no flatten, no evaluation).
    let n = ctx.n;
    let mut t = T0 + 100;
    while t < T0 + 5 * MIN {
        p.on_tick(&quote(0, t), &mut ctx);
        p.on_venue_event(&print(0, t + 1, 100_000_000, 1_000_000), &mut ctx);
        t += 500;
    }
    assert_eq!(ctx.n, n, "no flatten order");
    // A second venue fill on the disabled script pushes no second incident (pin:08-disable-once).
    p.on_fill(&venue(0, Side::Bid, 1_000, 0, t), &mut ctx);
    let k = p.pine_counters();
    assert_eq!((k.fills_venue_originated, k.incidents), (2, 1));
}

/// The live_maker script (`limit_primitive = Maker`): its limit entry rests as a maker.
fn live_maker_member() -> PineStrategy {
    let mut s = rt("live_maker.pine", 0);
    s.limit_primitive = PineLimitPrimitive::Maker;
    s.cap_instance_1e6 = 48_000_000;
    start(live_cfg(vec![s]), warm())
}

/// The live_maker entry rests (one Submit, acked) at E = `T0 + 15` ms.
fn resting_maker(p: &mut PineStrategy, ctx: &mut RecCtx) -> Order {
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), ctx);
    p.on_tick(&quote(0, T0 + 15), ctx);
    assert_eq!(ctx.n, 1, "the maker rests");
    assert_eq!(ctx.seen()[0].verb, 0, "a submit");
    assert_eq!(p.rt_probe().expect("core").script_settle[0].2, 1);
    ctx.seen()[0].order
}

/// PB6 (risk review C3): a liquidation fill on a script with a resting maker disables it WITHOUT
/// a flatten and CANCELS the maker — exactly one Cancel for its client oid, no Submit, one
/// `VENUE_FILL` incident — and nothing more is dispatched on later ticks, prints and bars.
#[test]
fn a_liquidation_fill_cancels_its_scripts_resting_maker() {
    let mut p = live_maker_member();
    let mut ctx = RecCtx::new();
    let o = resting_maker(&mut p, &mut ctx);
    p.on_fill(&venue(0, Side::Ask, 1_000_000, 0, T0 + 40), &mut ctx);
    assert_eq!(ctx.n, 2, "one cancel");
    let r = ctx.seen()[1];
    assert_eq!(r.verb, 2, "a cancel, never a submit");
    assert_eq!(
        (r.order.client_oid, r.order.sym, r.order.venue),
        (o.client_oid, o.sym, o.venue)
    );
    let k = p.pine_counters();
    assert_eq!((k.fills_venue_originated, k.fills), (1, 0));
    assert_eq!(
        (
            k.scripts_faulted,
            k.incidents,
            k.flattens_started,
            k.cancels
        ),
        (1, 1, 0, 1),
        "disabled, one incident, no flatten, one cancel"
    );
    assert!(p.rt_probe().expect("core").script_run[0].1, "disabled");
    let inc = incidents(&p);
    assert_eq!(inc.len(), 1);
    assert_eq!(
        (inc[0].event, inc[0].script),
        (pine_incident::VENUE_FILL, 0)
    );
    // Nothing more: no re-sent cancel, no flatten, no evaluation, across five chart bars; the
    // live window reports the cancelled maker.
    let mut t = T0 + 100;
    while t < T0 + 5 * MIN {
        p.on_tick(&quote(0, t), &mut ctx);
        p.on_venue_event(&print(0, t + 1, 100_000_000, 1_000_000), &mut ctx);
        t += 500;
    }
    assert_eq!(ctx.n, 2, "nothing dispatched after the cancel");
    let k = p.pine_counters();
    assert_eq!((k.cancelled_reported, k.flattens_started), (1, 0));
    assert_eq!(p.rt_probe().expect("core").script_settle[0].2, 0);
}

/// PB6 (pin:pb6-cancel-retry-per-bar): a refused cancel is not re-sent on every tick; the next
/// confirmed chart bar re-sends it, and once it is answered nothing more goes out.
#[test]
fn a_refused_pb6_cancel_is_resent_at_the_next_bar() {
    let mut p = live_maker_member();
    let mut ctx = RecCtx::new();
    let o = resting_maker(&mut p, &mut ctx);
    ctx.cancel_res = Err(SubmitErr::Refused);
    p.on_fill(&venue(0, Side::Ask, 1_000_000, 0, T0 + 40), &mut ctx);
    assert_eq!((ctx.n, ctx.seen()[1].verb), (2, 2));
    assert_eq!(p.pine_counters().cancel_errors, 1);
    // Ticks inside the bar: nothing re-sent.
    let mut t = T0 + 100;
    while t < T0 + MIN - 1_000 {
        p.on_tick(&quote(0, t), &mut ctx);
        t += 500;
    }
    assert_eq!(ctx.n, 2, "not per tick");
    // The bar confirms: re-sent once, answered.
    ctx.cancel_res = Ok(());
    let mut t = T0 + MIN + 10;
    while t < T0 + 4 * MIN {
        p.on_tick(&quote(0, t), &mut ctx);
        p.on_venue_event(&print(0, t + 1, 100_000_000, 1_000_000), &mut ctx);
        t += 500;
    }
    assert_eq!(ctx.n, 3, "one re-send, then nothing");
    let r = ctx.seen()[2];
    assert_eq!((r.verb, r.order.client_oid), (2, o.client_oid));
    let k = p.pine_counters();
    assert_eq!(
        (k.cancels, k.cancel_errors, k.flattens_started, k.incidents),
        (1, 1, 0, 1)
    );
}

/// PB6 on a paper route: the same cancel goes out (the paper matcher's resting maker would fill
/// into the broker too) and the next settle point reports it.
#[test]
fn a_paper_venue_fill_cancels_the_resting_maker_too() {
    let mut s = rt("rt_maker.pine", 0);
    s.limit_primitive = PineLimitPrimitive::Maker;
    let mut p = start(boot(vec![s]), warm());
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 15), &mut ctx);
    assert_eq!((ctx.n, ctx.seen()[0].verb), (1, 0), "the maker rests");
    let oid = ctx.seen()[0].order.client_oid;
    p.on_fill(&venue(0, Side::Ask, 1_000_000, 0, T0 + 40), &mut ctx);
    assert_eq!(ctx.n, 2);
    assert_eq!(
        (ctx.seen()[1].verb, ctx.seen()[1].order.client_oid),
        (2, oid)
    );
    p.on_venue_event(&print(0, T0 + 50, 100_000_000, 1_000_000), &mut ctx);
    let k = p.pine_counters();
    assert_eq!(
        (k.cancels, k.cancelled_reported, k.flattens_started),
        (1, 1, 0)
    );
    assert_eq!(ctx.n, 2);
}

/// PB6: a member oid of another boot stays `fills_prior_boot` — not venue-originated, no fault.
#[test]
fn a_prior_boot_member_fill_stays_fills_prior_boot() {
    let mut p = live_member();
    let mut ctx = RecCtx::new();
    // The test boot's epoch is 1 (`common::boot`); 2 is a previous boot's.
    let f = venue(0, Side::Bid, 1_000_000, encode_client_oid(2, 0, 1), T0 + 5);
    p.on_fill(&f, &mut ctx);
    let k = p.pine_counters();
    assert_eq!(
        (
            k.fills_prior_boot,
            k.fills_venue_originated,
            k.fills_adopted
        ),
        (1, 0, 0)
    );
    assert_eq!((k.scripts_faulted, k.incidents), (0, 0));
    // A paper fill with oid 0 is not venue-originated either (origin PAPER).
    let mut g = venue(0, Side::Bid, 1_000_000, 0, T0 + 6);
    g.origin = FILL_ORIGIN_PAPER;
    p.on_fill(&g, &mut ctx);
    let k = p.pine_counters();
    assert_eq!((k.fills_prior_boot, k.fills_venue_originated), (2, 0));
}

/// PB6: a venue-originated fill on a symbol no script trades is counted only — no fault, no
/// incident; the trading script keeps running.
#[test]
fn a_venue_fill_on_an_untraded_symbol_is_counted_only() {
    let mut p = live_member();
    let mut ctx = RecCtx::new();
    p.on_fill(&venue(1, Side::Ask, 1_000_000, 0, T0 + 5), &mut ctx);
    let k = p.pine_counters();
    assert_eq!(k.fills_venue_originated, 1);
    assert_eq!(
        (k.scripts_faulted, k.incidents, k.flattens_started),
        (0, 0, 0)
    );
    assert!(!p.rt_probe().expect("core").script_run[0].1);
    let _ = first_ioc(&mut p, &mut ctx);
}

/// PB12 (OD-12): a live fill without a noted fee books the model and counts
/// `fee_modelled_live`; a noted (measured) fee is not counted; paper never counts.
#[test]
fn live_fills_book_the_model_fee_and_count_it() {
    let mut p = live_member();
    let mut ctx = RecCtx::new();
    let o = first_ioc(&mut p, &mut ctx);
    let half = o.qty.raw() / 2;
    let mut f = venue(0, o.side, half, o.client_oid, T0 + 25);
    f.px = o.px;
    p.on_fill(&f, &mut ctx);
    assert_eq!(p.pine_counters().fee_modelled_live, 1, "the model, counted");
    p.note_fill_fee(o.client_oid, 1_000);
    let mut g = venue(0, o.side, o.qty.raw() - half, o.client_oid, T0 + 26);
    g.px = o.px;
    p.on_fill(&g, &mut ctx);
    let k = p.pine_counters();
    assert_eq!(
        (k.fills, k.fee_modelled_live),
        (2, 1),
        "a measured fee is not"
    );
    // Paper: the same fill books the model and is never counted.
    let mut q = start(boot(vec![rt("rt_entry.pine", 0)]), warm());
    let mut c2 = RecCtx::new();
    q.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut c2);
    q.on_tick(&quote(0, T0 + 20), &mut c2);
    let o2 = c2.seen()[0].order;
    let mut h = venue(0, o2.side, o2.qty.raw(), o2.client_oid, T0 + 25);
    h.px = o2.px;
    h.origin = FILL_ORIGIN_PAPER;
    q.on_fill(&h, &mut c2);
    let k2 = q.pine_counters();
    assert_eq!((k2.fills, k2.fee_modelled_live), (1, 0));
}
