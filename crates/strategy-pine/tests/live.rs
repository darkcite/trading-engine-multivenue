// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Binance plan §2.1 PL1 / PL2 (impl-08 §4.5.4–§4.5.5, live): the live settle window and the
//! host-supplied live permit, with the scripted `Ctx` double (`common::RecCtx`). Only a test
//! builds a `Live` route with a permit (pin:pl2-live-permit); the cli never does.
//!
//! The window must hold for a QUEUED `Ok` (Binance: the ACK and the fills come later, no order
//! event) and a SYNCHRONOUS answer (HL: an IoC that found no match is `Err(Refused)`, E7-F2):
//! both scripted here, `submit_res = Ok(())` and `Err(Refused)`.

mod common;

use common::*;
use core_config::pine::{BootExitPolicy, PineLimitPrimitive, PineMode, PineScriptCfg};
use core_types::{Fill, Order, Qty, VenueId, FILL_ORIGIN_VENUE};
use strategy_core::{Strategy, StrategyCounters, StrategyError, SubmitErr};
use strategy_pine::{PineBootCfg, PineStrategy, SlotCaps8, SlotRoute, VenueBootState};

/// `live_settle_ms` default (§4.8), ms.
const WINDOW_MS: i64 = 2_000;

fn rt(file: &str, coin: usize) -> PineScriptCfg {
    let mut s = script(file, coin);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = 0;
    if file.contains("maker") {
        s.limit_primitive = PineLimitPrimitive::Maker;
    }
    s
}

fn warm() -> Vec<strategy_pine::WarmSlice> {
    vec![
        slice(0, 60, warm_bars(60, 50, 0)),
        slice(1, 60, warm_bars(60, 50, 0)),
    ]
}

/// A Live boot that passes every §4.1.6 Live check, with the permit `permit`.
fn live_cfg(scripts: Vec<PineScriptCfg>, permit: Option<VenueId>) -> PineBootCfg {
    let mut b = boot(scripts);
    b.route = SlotRoute::Live;
    b.live_venue = permit;
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

fn live_member(scripts: Vec<PineScriptCfg>) -> PineStrategy {
    start(live_cfg(scripts, Some(VenueId::Hyperliquid)), warm())
}

/// A venue fill of `o` for `qty_1e6` at UTC ms `t`.
fn venue_fill(o: &Order, qty_1e6: i64, t: i64) -> Fill {
    let mut f = Fill::new(
        u64::try_from(t).expect("t") * 1_000_000,
        o.sym,
        o.side,
        o.px,
        Qty::from_raw(qty_1e6),
        o.client_oid,
    );
    f.strategy_id = 8;
    f.origin = FILL_ORIGIN_VENUE;
    f
}

/// The live_entry IoC at E = `T0 + 20` ms; its window ends at `T0 + 20 + WINDOW_MS`.
fn first_ioc(p: &mut PineStrategy, ctx: &mut RecCtx) -> Order {
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), ctx);
    p.on_tick(&quote(0, T0 + 20), ctx);
    assert_eq!(ctx.n, 1, "one IoC");
    ctx.seen()[0].order
}

/// Quotes and settle points (prints) of coin 0 every 100 ms over `[from, to)`.
fn drive(p: &mut PineStrategy, ctx: &mut RecCtx, from: i64, to: i64) {
    let mut t = from;
    while t < to {
        p.on_tick(&quote(0, t), ctx);
        p.on_venue_event(&print(0, t + 1, 100_000_000, 1_000_000), ctx);
        t += 100;
    }
}

/// PL1: a live IoC is reported `on_cancelled` only at the first settle point with
/// `E ≥ E_dispatch + live_settle_ms` — gone-evidence inside the window judges nothing — for a
/// queued `Ok` and a synchronous `Err(Refused)` alike; then the entry is free to fire again.
#[test]
fn live_ioc_is_cancelled_only_after_the_window() {
    for res in [Ok(()), Err(SubmitErr::Refused)] {
        let mut p = live_member(vec![rt("live_entry.pine", 0)]);
        let mut ctx = RecCtx::new();
        ctx.submit_res = res;
        let _ = first_ioc(&mut p, &mut ctx);
        let k = p.pine_counters();
        assert_eq!(
            (k.acks, k.submit_errors),
            if res.is_ok() { (1, 0) } else { (0, 1) },
            "{res:?}"
        );
        // Fresh two-sided ticks (paper's gone-evidence) and settle points inside the window.
        drive(&mut p, &mut ctx, T0 + 30, T0 + 20 + WINDOW_MS);
        assert_eq!(p.pine_counters().cancelled_reported, 0, "{res:?}");
        assert_eq!(ctx.n, 1, "{res:?}: nothing re-sent while in flight");
        assert_eq!(p.rt_probe().expect("core").script_settle[0].2, 1);
        // E reaches `until` on a quote (not a settle point): still nothing.
        p.on_tick(&quote(0, T0 + 20 + WINDOW_MS), &mut ctx);
        assert_eq!(p.pine_counters().cancelled_reported, 0, "{res:?}");
        // The next settle point reports it, and the entry re-fires.
        p.on_venue_event(
            &print(0, T0 + 25 + WINDOW_MS, 100_000_000, 1_000_000),
            &mut ctx,
        );
        assert_eq!(p.pine_counters().cancelled_reported, 1, "{res:?}");
        assert_eq!(ctx.n, 2, "{res:?}: re-fired after the disposition");
        assert_eq!(p.rt_probe().expect("core").script_pos[0], 0);
    }
}

/// PL1: fills that total the IoC's size inside the window settle it — no `on_cancelled`, no late
/// fill; a partial one is NOT terminal on Live (paper's one judgment) and the window reports the
/// remainder.
#[test]
fn a_fill_inside_the_window_settles_the_ioc() {
    let mut p = live_member(vec![rt("live_entry.pine", 0)]);
    let mut ctx = RecCtx::new();
    let o = first_ioc(&mut p, &mut ctx);
    p.on_fill(&venue_fill(&o, o.qty.raw(), T0 + 500), &mut ctx);
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.script_pos[0], o.qty.raw());
    assert_eq!(pr.script_settle[0].2, 0, "settled by its fills");
    drive(&mut p, &mut ctx, T0 + 600, T0 + 100 + 2 * WINDOW_MS);
    let k = p.pine_counters();
    assert_eq!(
        (k.fills, k.cancelled_reported, k.late_fills_after_cancel),
        (1, 0, 0)
    );
    // A partial fill inside the window.
    let mut p = live_member(vec![rt("live_entry.pine", 0)]);
    let mut ctx = RecCtx::new();
    let o = first_ioc(&mut p, &mut ctx);
    p.on_fill(&venue_fill(&o, 50_000, T0 + 500), &mut ctx);
    assert_eq!(p.pine_counters().cancelled_reported, 0, "not terminal live");
    assert_eq!(p.rt_probe().expect("core").script_settle[0].2, 1);
    drive(&mut p, &mut ctx, T0 + 600, T0 + 20 + WINDOW_MS);
    assert_eq!(p.pine_counters().cancelled_reported, 0);
    drive(&mut p, &mut ctx, T0 + 20 + WINDOW_MS, T0 + 220 + WINDOW_MS);
    let k = p.pine_counters();
    assert_eq!((k.fills, k.cancelled_reported), (1, 1));
    assert_eq!(p.rt_probe().expect("core").script_pos[0], 50_000);
}

/// PL1: a modify's replaced `prev` is reported only after the window (it may still fill on the
/// venue: a queued modify, R10); the replacement rests.
#[test]
fn live_modify_prev_waits_the_window() {
    let mut s = rt("live_maker.pine", 0);
    s.cap_instance_1e6 = 48_000_000;
    let mut p = live_member(vec![s]);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 15), &mut ctx);
    assert_eq!(ctx.n, 1, "the maker rests");
    // The close moves: the maker is requoted (E-7, a Modify).
    p.on_venue_event(&print(0, T0 + 1_000, 100_090_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 1_005), &mut ctx);
    let m = ctx
        .seen()
        .iter()
        .position(|r| r.verb == 1)
        .expect("a modify");
    let tm = ctx.seen()[m].order.ts_ns;
    let k = p.pine_counters();
    assert_eq!((k.modifies, k.cancelled_reported), (1, 0));
    assert_eq!(
        p.rt_probe().expect("core").script_settle[0].2,
        2,
        "prev + new"
    );
    // Settle points at an unchanged close inside prev's window: nothing reported.
    let end = i64::try_from(tm / 1_000_000).expect("ms") + WINDOW_MS;
    let mut t = T0 + 1_100;
    while t < end {
        p.on_venue_event(&print(0, t, 100_090_000, 1_000_000), &mut ctx);
        p.on_tick(&quote(0, t + 5), &mut ctx);
        t += 100;
    }
    assert_eq!(p.pine_counters().cancelled_reported, 0);
    assert_eq!(p.pine_counters().modifies, 1, "no requote at one close");
    p.on_venue_event(&print(0, end + 10, 100_090_000, 1_000_000), &mut ctx);
    p.on_venue_event(&print(0, end + 20, 100_090_000, 1_000_000), &mut ctx);
    assert_eq!(
        p.pine_counters().cancelled_reported,
        1,
        "prev, after the window"
    );
    assert_eq!(
        p.rt_probe().expect("core").script_settle[0].2,
        1,
        "new rests"
    );
}

/// PL1: the window runs in event time — a dark feed (no tick or print of a pine symbol; other
/// events keep arriving, far past the window in wall time) freezes E and holds the IoC in
/// flight. The first pine input past `until` ripens it; then any venue event is a settle point.
#[test]
fn a_dark_feed_holds_the_window() {
    let mut p = live_member(vec![rt("live_entry.pine", 0)]);
    let mut ctx = RecCtx::new();
    let _ = first_ioc(&mut p, &mut ctx);
    // A print of a symbol no script trades (coin index 5), 10 windows later: E does not move.
    let mut t = T0 + 100;
    while t < T0 + 10 * WINDOW_MS {
        p.on_venue_event(&print(5, t, 100_000_000, 1_000_000), &mut ctx);
        t += 250;
    }
    assert_eq!(p.pine_counters().cancelled_reported, 0);
    assert_eq!(ctx.n, 1, "no IoC into a market it cannot see");
    // The feed returns: E jumps past `until`; the next (foreign) venue event settles it.
    p.on_tick(&quote(0, T0 + 10 * WINDOW_MS), &mut ctx);
    assert_eq!(p.pine_counters().cancelled_reported, 0);
    p.on_venue_event(
        &print(5, T0 + 10 * WINDOW_MS + 1, 100_000_000, 1_000_000),
        &mut ctx,
    );
    assert_eq!(p.pine_counters().cancelled_reported, 1);
}

/// PL1 / §4.5.5: a fill after the window reported its order is a LATE fill — booked by the
/// broker (its attribution ring), counted `late_fills_after_cancel`.
#[test]
fn a_late_fill_after_cancel_is_booked_and_counted() {
    let mut p = live_member(vec![rt("live_entry.pine", 0)]);
    let mut ctx = RecCtx::new();
    let o = first_ioc(&mut p, &mut ctx);
    drive(&mut p, &mut ctx, T0 + 30, T0 + 220 + WINDOW_MS);
    assert_eq!(p.pine_counters().cancelled_reported, 1);
    p.on_fill(&venue_fill(&o, o.qty.raw(), T0 + 230 + WINDOW_MS), &mut ctx);
    let k = p.pine_counters();
    assert_eq!((k.fills, k.late_fills_after_cancel), (1, 1));
    assert_eq!(p.rt_probe().expect("core").script_pos[0], o.qty.raw());
}

/// PL2: a Live route without the host's permit refuses `on_start` (`6005`, nothing starts); so
/// do a permit off Live and a permit naming another venue; the permit for the scripts' venue
/// starts the member.
#[test]
fn a_live_route_without_a_permit_refuses_on_start() {
    let refused = |b: PineBootCfg| {
        let mut p = PineStrategy::new();
        p.configure(b).expect("the §4.1.6 checks pass");
        p.supply_warm(warm()).expect("warm");
        let r = p.on_start(&mut NoCtx);
        assert!(matches!(r, Err(StrategyError::Config(_))));
        assert!(p.rt_probe().is_none(), "no core: nothing dispatches");
        let e = p.last_boot_error().expect("refusal");
        (e.code.0, e.key)
    };
    let s = || vec![rt("live_entry.pine", 0)];
    assert_eq!(refused(live_cfg(s(), None)), (6005, "[exec.slot.8]"));
    assert_eq!(
        refused(live_cfg(s(), Some(VenueId::Binance))),
        (6005, "live_venue")
    );
    let mut paper = boot(s());
    paper.live_venue = Some(VenueId::Hyperliquid);
    assert_eq!(refused(paper), (6005, "live_venue"));
    let p = live_member(s());
    assert!(p.rt_probe().is_some(), "the permit starts the Live member");
}
