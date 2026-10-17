// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.6 `bridge.rs` (T12–T14) with a scripted `Ctx` double (`common::RecCtx`): the
//! order shape (I24), the client oid (I25), dispatch order (I26), dispositions on paper and
//! replay (I27), the per-script refusal streak (I29), the position only through `on_fill` (I30),
//! and PAPER ONLY — a Live route refuses `on_start` (fail closed).

mod common;

use common::*;
use core_config::pine::{PineLimitPrimitive, PineMode, PineScriptCfg};
use core_types::{Fill, Order, Side, VenueId, FILL_ORIGIN_PAPER, STRATEGY_ID_NONE};
use strategy_core::{Strategy, StrategyCounters, StrategyError};
use strategy_pine::oid::{decode_client_oid, encode_client_oid};
use strategy_pine::{HostKind, PineBootCfg, PineStrategy, SlotRoute};

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

fn member_with(b: PineBootCfg) -> PineStrategy {
    start(b, warm())
}

fn member(scripts: Vec<PineScriptCfg>) -> PineStrategy {
    member_with(boot(scripts))
}

/// A paper fill of `o` for `qty_1e6`.
fn fill_of(o: &Order, qty_1e6: i64, dt_ms: u64) -> Fill {
    let mut f = Fill::new(
        o.ts_ns + dt_ms * 1_000_000,
        o.sym,
        o.side,
        o.px,
        core_types::Qty::from_raw(qty_1e6),
        o.client_oid,
    );
    f.strategy_id = 8;
    f.origin = FILL_ORIGIN_PAPER;
    f
}

/// I24: kind by primitive, `px` / `qty` unscaled, `ts_ns == E`, the chart venue and symbol, the
/// broker's TTL, no flags; `strategy_id` is left for the set's `StampCtx` to stamp.
#[test]
fn orders_are_stamped_kinded_and_unscaled() {
    let mut p = member(vec![rt("rt_entry.pine", 0), rt("rt_maker.pine", 1)]);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    assert_eq!(ctx.n, 1);
    let o = ctx.seen()[0].order;
    let e = p.rt_probe().expect("core").e_ns;
    assert_eq!(o.ts_ns, e, "ts_ns == E");
    assert_eq!(e, u64::try_from(T0 + 20).expect("t") * 1_000_000);
    assert_eq!(o.kind, core_fill::ORDER_KIND_IOC);
    assert_eq!(o.side, Side::Bid);
    // The band: ask 101 + 2 ticks of 0.01, unscaled; 12 USD / 101.02 floored to the 0.001 step.
    assert_eq!(o.px.raw(), 101_020_000);
    assert_eq!(o.qty.raw(), 118_000);
    assert_eq!(o.ttl_ns, 700_000_000, "the broker's TTL (HL stale window)");
    assert_eq!(
        (o.venue, o.sym, o.flags),
        (VenueId::Hyperliquid as u8, sym(0), 0)
    );
    assert_eq!(
        o.strategy_id, STRATEGY_ID_NONE,
        "stamped by the set, not the member"
    );
    assert_eq!(decode_client_oid(1, o.client_oid), Some((0, 1)));
    // A maker: `ORDER_KIND_MAKER`, no TTL.
    p.on_venue_event(&print(1, T0 + 30, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(1, T0 + 40), &mut ctx);
    assert_eq!(ctx.n, 2);
    let m = ctx.seen()[1].order;
    assert_eq!(
        (m.kind, m.ttl_ns, m.sym),
        (core_fill::ORDER_KIND_MAKER, 0, sym(1))
    );
    assert_eq!(m.px.raw(), 99_500_000, "close − 0.5 on the tick, unscaled");
    assert_eq!(decode_client_oid(1, m.client_oid), Some((1, 1)));
}

/// I25: the round trip, and a fill from another boot (epoch) never reaches a broker.
#[test]
fn client_oid_roundtrip_and_epoch_filter() {
    for (e, s, o) in [
        (1u32, 0u8, 1u32),
        (u32::MAX, 7, (1 << 29) - 1),
        (1_760_000_000, 3, 77),
    ] {
        assert_eq!(
            decode_client_oid(e, encode_client_oid(e, s, o)),
            Some((s, o))
        );
        assert_eq!(decode_client_oid(e ^ 1, encode_client_oid(e, s, o)), None);
    }
    assert_eq!(decode_client_oid(5, 5u64 << 32), None, "oid 0");
    assert_eq!(decode_client_oid(0, 0), None, "epoch 0");
    let mut p = member(vec![rt("rt_entry.pine", 0)]);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    let o = ctx.seen()[0].order;
    let mut f = fill_of(&o, o.qty.raw(), 5);
    f.order_id = encode_client_oid(2, 0, 1); // a previous boot's oid 1
    p.on_fill(&f, &mut ctx);
    let mut g = fill_of(&o, o.qty.raw(), 5);
    g.strategy_id = 3; // another slot's
    p.on_fill(&g, &mut ctx);
    let k = p.pine_counters();
    assert_eq!((k.fills_prior_boot, k.fills_foreign, k.fills), (1, 1, 0));
    assert_eq!(p.rt_probe().expect("core").script_pos[0], 0);
}

/// I26: within one callback the instructions go out in the broker's buffer order — Cancels,
/// then Modifies, then Submits.
#[test]
fn dispatch_follows_buffer_order() {
    // cap_instance above one unit: with cap == unit the broker's dry-run requote sizing counts
    // the resting maker itself and cancels it (request R-08-T12-1).
    let mut s = rt("rt_maker.pine", 0);
    s.cap_instance_1e6 = 48_000_000;
    let mut p = member(vec![s]);
    let mut ctx = RecCtx::new();
    let mut t = T0 + 10;
    let mut k = 0i64;
    while t < T0 + 12 * MIN {
        p.on_venue_event(
            &print(0, t, 100_000_000 + (k % 9) * 30_000, 1_000_000),
            &mut ctx,
        );
        p.on_tick(&quote(0, t + 5), &mut ctx);
        t += 7_000;
        k += 1;
    }
    let rank = |v: u8| match v {
        2 => 0, // cancel
        1 => 1, // modify
        _ => 2, // submit
    };
    let seen = ctx.seen();
    let (mut cancels, mut modifies) = (0, 0);
    let mut i = 1usize;
    while i < seen.len() {
        if seen[i].order.ts_ns == seen[i - 1].order.ts_ns {
            assert!(rank(seen[i - 1].verb) <= rank(seen[i].verb), "I26 at {i}");
        }
        i += 1;
    }
    for r in seen {
        cancels += u32::from(r.verb == 2);
        modifies += u32::from(r.verb == 1);
    }
    assert!(
        cancels > 0 && modifies > 0,
        "cancels {cancels} modifies {modifies}"
    );
    let c = p.pine_counters();
    assert_eq!(
        (c.cancels, c.modifies),
        (u64::from(cancels), u64::from(modifies))
    );
}

/// I27 (paper): an IoC is reported `on_cancelled` only at a settle point after gone-evidence
/// (a fresh tick at/after `t_active`), or right after a partial fill — never before its fill.
#[test]
fn paper_ioc_cancelled_after_its_fill_or_evidence_and_settle() {
    let mut p = member(vec![rt("rt_entry.pine", 0)]);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    assert_eq!(ctx.n, 1);
    // Evidence (a fresh two-sided tick at/after t_active, Δ = 0): marked gone, not reported.
    p.on_tick(&quote(0, T0 + 30), &mut ctx);
    assert_eq!(p.pine_counters().cancelled_reported, 0);
    assert_eq!(
        ctx.n, 1,
        "one IoC in flight: nothing re-sent before its disposition"
    );
    // The settle point: reported, the entry is free to fire again on a fresh touch.
    p.on_venue_event(&print(0, T0 + 40, 100_000_000, 1_000_000), &mut ctx);
    assert_eq!(p.pine_counters().cancelled_reported, 1);
    assert_eq!(ctx.n, 2);
    let o = ctx.seen()[1].order;
    assert_eq!(decode_client_oid(1, o.client_oid), Some((0, 2)));
    assert_eq!(
        p.rt_probe().expect("core").script_pos[0],
        0,
        "I30: no fill, no position"
    );
    // A partial fill is terminal for an IoC: on_cancelled right after on_fill.
    p.on_fill(&fill_of(&o, 50_000, 1), &mut ctx);
    let k = p.pine_counters();
    assert_eq!((k.fills, k.cancelled_reported), (1, 2));
    let pr = p.rt_probe().expect("core");
    assert_eq!(
        pr.script_pos[0], 50_000,
        "I30: the position moved through on_fill only"
    );
    assert_eq!(pr.script_settle[0], (0, false, 0));
}

/// I27 (replay): every `on_tick` / `on_venue_event` is a settle point resolved FIRST, so
/// evidence seen at one record is reported at the next callback; unattributed fills accepted.
#[test]
fn replay_settles_on_the_next_callback() {
    let mut b = boot(vec![rt("rt_entry.pine", 0)]);
    b.host = HostKind::Replay;
    let mut p = member_with(b);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    assert_eq!(ctx.n, 1);
    p.on_tick(&quote(0, T0 + 30), &mut ctx); // evidence
    assert_eq!(p.pine_counters().cancelled_reported, 0);
    p.on_tick(&quote(0, T0 + 40), &mut ctx); // the next callback settles it, then re-sends
    assert_eq!(p.pine_counters().cancelled_reported, 1);
    assert_eq!(ctx.n, 2);
    let o = ctx.seen()[1].order;
    let mut f = fill_of(&o, o.qty.raw(), 1);
    f.strategy_id = STRATEGY_ID_NONE; // replay fills are unattributed (§4.5.6)
    p.on_fill(&f, &mut ctx);
    assert_eq!(p.pine_counters().fills, 1);
    assert_eq!(p.rt_probe().expect("core").script_pos[0], o.qty.raw());
}

/// I29: the refusal streak is per script — at `refusal_streak_max` that script's increasing
/// orders are withheld (counted, reported `on_cancelled`); the other script is untouched.
#[test]
fn refusal_streak_is_per_script_and_spares_exits() {
    let mut b = boot(vec![rt("rt_entry.pine", 0), rt("rt_entry.pine", 1)]);
    b.slot.refusal_streak_max = 3;
    let mut p = member_with(b);
    let mut ctx = RecCtx::new();
    ctx.refuse_sym = sym(0);
    let mut k = 0i64;
    while k < 8 {
        let t = T0 + 10 + k * 1_000;
        p.on_venue_event(&print(0, t, 100_000_000, 1_000_000), &mut ctx);
        p.on_tick(&quote(0, t + 5), &mut ctx);
        p.on_venue_event(&print(1, t + 6, 100_000_000, 1_000_000), &mut ctx);
        p.on_tick(&quote(1, t + 7), &mut ctx);
        // Script 1's IoC is judged without a fill; the next settle point frees it.
        p.on_tick(&quote(1, t + 8), &mut ctx);
        k += 1;
    }
    let c = p.pine_counters();
    let pr = p.rt_probe().expect("core");
    assert_eq!(c.refusal_trips, 1);
    assert_eq!(pr.script_settle[0].0, 3);
    assert!(pr.script_settle[0].1, "script 0 suppressed");
    assert_eq!(pr.script_settle[1].0, 0);
    assert!(!pr.script_settle[1].1, "script 1 untouched");
    assert!(c.entries_suppressed > 0, "entries withheld after the trip");
    let sent0 = ctx.seen().iter().filter(|r| r.order.sym == sym(0)).count();
    let sent1 = ctx.seen().iter().filter(|r| r.order.sym == sym(1)).count();
    assert_eq!(
        sent0, 3,
        "three refusals reached the ctx, then nothing increasing"
    );
    assert!(sent1 >= 7, "script 1 kept trading: {sent1}");
}

/// pin:pl2-live-permit (replaced pin:08-live-refused-at-start): a Live route WITHOUT the host's
/// live permit that reaches `on_start` is refused — the cli never sets the permit.
#[test]
fn live_route_refuses_on_start() {
    let mut b = boot(vec![rt("rt_entry.pine", 0)]);
    b.route = SlotRoute::Live;
    let mut p = PineStrategy::new();
    // `configure` runs the §4.1.6 Live checks; force past them by building the core directly is
    // not possible — so assert whichever refuses first refuses, and nothing starts.
    let configured = p.configure(b).is_ok();
    if configured {
        p.supply_warm(warm()).expect("warm");
        let r = p.on_start(&mut NoCtx);
        assert!(matches!(r, Err(StrategyError::Config(_))));
        let e = p.last_boot_error().expect("refusal");
        assert_eq!(e.code.0, 6005);
    }
    assert!(p.rt_probe().is_none(), "no core: nothing dispatches");
}

/// An `Off` route dispatches nothing and commits nothing (§4.4.5).
#[test]
fn off_route_dispatches_nothing() {
    let mut b = boot(vec![rt("rt_entry.pine", 0)]);
    b.route = SlotRoute::Off;
    let mut p = member_with(b);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    assert_eq!(ctx.n, 0);
    assert_eq!(p.rt_probe().expect("core").commits, 0);
    assert!(p.pine_counters().intents_discarded > 0);
}

/// I30 (grep): the member keeps no position, average price or day notional of its own — no
/// such field in any run-time module.
#[test]
fn member_keeps_no_position_state() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for f in [
        "rt.rs",
        "host.rs",
        "settle.rs",
        "fills.rs",
        "coalesce.rs",
        "step.rs",
        "clock.rs",
    ] {
        let text = std::fs::read_to_string(src.join(f)).expect("src");
        for line in text.lines() {
            let l = line.trim_start();
            if !l.starts_with("pub(crate) ") && !l.starts_with("pub ") {
                continue;
            }
            for bad in ["position_1e6", "position_size", "avg_px", "day_notional"] {
                assert!(
                    !(l.contains(bad) && l.contains(':') && !l.contains("fn ")),
                    "{f}: `{l}` holds position state (I30)"
                );
            }
        }
    }
}
