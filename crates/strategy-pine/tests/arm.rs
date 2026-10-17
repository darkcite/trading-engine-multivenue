// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.7 `arm.rs` (T16, 8f): route `Off` never calls the ctx (I36), `enabled = 0` never
//! commits (I37), `reconcile_boot` once before the first commit (I38), the Live boot's venue facts
//! (I39, refused fail-closed), and the four boot-exit policies (I40).

mod common;

use common::*;
use core_config::pine::{BootExitPolicy, PineMode, PineScriptCfg};
use strategy_core::{pine_incident, PineIncident, PineScriptView, Strategy, StrategyCounters};
use strategy_pine::{PineBootCfg, PineStrategy, SlotCaps8, SlotRoute, VenueBootState};

fn rt(file: &str, coin: usize) -> PineScriptCfg {
    let mut s = script(file, coin);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = 0;
    s
}

fn warm_both() -> Vec<strategy_pine::WarmSlice> {
    vec![
        slice(0, 60, warm_bars(60, 50, 0)),
        slice(1, 60, warm_bars(60, 50, 0)),
    ]
}

/// A venue boot state for `coin`: `pos` ×1e6 at 100 USD, today's notional `day` ×1e6.
fn state(coin: usize, pos: i64, day: i64) -> VenueBootState {
    VenueBootState {
        sym: sym(coin),
        position_1e6: pos,
        avg_px_1e6: if pos == 0 { 0 } else { 100_000_000 },
        day_notional_1e6: day,
        resting_swept: true,
    }
}

/// Configure `b` (its `boot_state` must be empty off Live), seed `states` through the test seam,
/// supply `warm`, start.
fn start_seeded(
    b: PineBootCfg,
    states: Vec<VenueBootState>,
    warm: Vec<strategy_pine::WarmSlice>,
) -> PineStrategy {
    let mut p = PineStrategy::new();
    p.configure(b).expect("configure");
    p.seed_boot_state_for_test(states);
    p.supply_warm(warm).expect("warm");
    p.on_start(&mut NoCtx).expect("on_start");
    p
}

/// Drive both coins through `[from, to)` ms: a print then a quote every `step` ms.
fn drive(p: &mut PineStrategy, ctx: &mut RecCtx, from: i64, to: i64, step: i64) {
    let mut t = from;
    while t < to {
        for coin in 0..2 {
            p.on_venue_event(&print(coin, t, 100_000_000, 1_000_000), ctx);
            p.on_tick(&quote(coin, t + 2), ctx);
        }
        t += step;
    }
}

fn incidents(p: &PineStrategy) -> Vec<PineIncident> {
    let mut out = vec![PineIncident::default(); 64];
    let n = p.pine_incidents_view(&mut out, 0) as usize;
    out.truncate(n);
    out
}

/// I36: route `Off` — the ctx is never called (no reconcile, no flatten, no guard), though the
/// scripts evaluate and a fault is still disabled and recorded.
#[test]
fn route_off_never_calls_ctx() {
    let mut b = boot(vec![script("time_fault.pine", 0), rt("rt_entry.pine", 1)]);
    b.route = SlotRoute::Off;
    b.slot.boot_exit_policy = Some(BootExitPolicy::Flatten);
    let mut p = start_seeded(b, vec![state(0, 120_000, 0)], warm_both());
    let mut ctx = RecCtx::new();
    drive(&mut p, &mut ctx, T0 + 10, T0 + 3 * MIN, 5_000);
    assert_eq!(ctx.n, 0, "I36: no ctx verb on Off");
    let k = p.pine_counters();
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.commits, 0);
    assert!(k.bar_evals > 0 && k.tick_evals > 0, "evaluations still run");
    assert_eq!(k.scripts_faulted, 1, "the fault is still a D13 disable");
    assert_eq!(
        k.flattens_started, 0,
        "nothing to dispatch a flatten through"
    );
    assert_eq!(k.boot_guard_flattens, 0);
    assert_eq!(pr.script_boot[0], (false, false), "Off adopts nothing");
    let inc = incidents(&p);
    assert_eq!(inc.len(), 1);
    assert_eq!(inc[0].event, pine_incident::FAULT);
}

/// I37: `enabled = 0` — never adopts, never commits, never dispatches, not even a flatten.
#[test]
fn enabled_zero_never_commits() {
    let mut a = rt("rt_entry.pine", 0);
    a.enabled = false;
    let mut f = script("time_fault.pine", 1);
    f.enabled = false;
    let mut b = boot(vec![a, f]);
    b.slot.boot_exit_policy = Some(BootExitPolicy::Flatten);
    let mut p = start_seeded(
        b,
        vec![state(0, 120_000, 0), state(1, 120_000, 0)],
        warm_both(),
    );
    let mut ctx = RecCtx::new();
    drive(&mut p, &mut ctx, T0 + 10, T0 + 3 * MIN, 5_000);
    assert_eq!(ctx.n, 0);
    let k = p.pine_counters();
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.commits, 0);
    assert!(k.intents_discarded > 0, "it evaluated and discarded");
    assert_eq!(k.scripts_faulted, 1);
    assert_eq!((k.flattens_started, k.boot_guard_flattens), (0, 0));
    assert_eq!(pr.script_boot[0], (false, false));
    assert_eq!(pr.script_boot[1], (false, false));
    assert_eq!(
        (pr.script_pos[0], pr.script_pos[1]),
        (0, 0),
        "nothing adopted"
    );
}

/// I38: `reconcile_boot` runs once per committing script, before its first `apply_intents`: the
/// adopted position is what the LAST warm-up evaluation's `strategy.position_size` reads, and the
/// seeded day notional makes a later entry `skipped_cap`. A cold script adopts in `on_start`.
#[test]
fn reconcile_boot_once_before_first_commit() {
    let b = boot(vec![
        script("boot_view.pine", 0),
        rt("rt_entry.pine", 1),
        rt("rt_entry.pine", 0),
    ]);
    // Script 1's day is spent: its 48 USD cap_day is the seeded notional.
    // Coin 1 has no warm slice: script 1 is cold and adopts in `on_start`.
    let mut p = start_seeded(
        b,
        vec![state(0, 120_000, 0), state(1, 0, 48_000_000)],
        vec![slice(0, 60, warm_bars(60, 50, 0))],
    );
    let pr = p.rt_probe().expect("core");
    // `pin:04-qty-float-1e8` (r33f, CI-058): a script reads its quantity as TradingView forms it,
    // `((q_1e6 · 100) as f64) · 1e-8` — the adopted 120 000 ×1e6 reads 0.12000000000000001.
    assert_eq!(
        pr.script_plot0[0],
        (120_000_i64 * 100) as f64 * 1e-8,
        "the last warm-up bar saw the adopted 0.12"
    );
    assert_eq!(pr.script_pos[0], 120_000);
    assert_eq!(
        pr.script_pos[2], 120_000,
        "every committing script adopts its coin's state"
    );
    for i in 0..3 {
        assert_eq!(
            pr.script_boot[i],
            (true, false),
            "script {i} adopted once, no guard"
        );
    }
    assert!(pr.script_run[1].0, "script 1 is cold");
    let mut ctx = RecCtx::new();
    drive(&mut p, &mut ctx, T0 + 10, T0 + 4 * MIN, 5_000);
    // Script 1 warms on live bars and wants to enter: the seeded day notional refuses it.
    let mut v = [PineScriptView::default(); 8];
    let n = p.pine_scripts_view(&mut v);
    assert_eq!(n, 3);
    assert!(v[1].skipped_cap > 0, "the day cap, seeded at boot, binds");
    assert!(
        ctx.seen().iter().all(|r| r.order.sym != sym(1)),
        "no entry on the spent coin"
    );
    // Script 2 (rt_entry, adopted long) holds: it never enters while not flat.
    assert!(ctx
        .seen()
        .iter()
        .all(|r| !(r.order.sym == sym(0) && r.verb == 0 && !r.order.is_reduce_only())));
}

/// I40: the four policies × {re-issuing, once-at-entry, stop-less, cold} on an adopted long:
/// which scripts the boot guard closes, at most once each, with one `BOOT_GUARD` incident and a
/// reduce-only IoC at the first fresh quote.
#[test]
fn boot_exit_policies_behave() {
    // (policy, guard fired per script [reissue, once, stopless, cold]).
    let table = [
        (BootExitPolicy::Accept, [false, false, false, false]),
        (BootExitPolicy::Standing, [false, false, false, false]),
        (BootExitPolicy::Protective, [false, true, true, true]),
        (BootExitPolicy::Flatten, [true, true, true, true]),
    ];
    for (policy, want) in table {
        let mut b = boot(
            [
                ("exit_reissue.pine", 0),
                ("exit_once.pine", 0),
                ("exit_stopless.pine", 0),
                ("exit_reissue.pine", 1),
            ]
            .iter()
            .map(|(f, c)| {
                let mut s = script(f, *c);
                s.max_open = 2;
                s
            })
            .collect(),
        );
        b.slot.boot_exit_policy = Some(policy);
        // Coin 1 has no warm slice: script 3 is the cold one.
        let mut p = start_seeded(
            b,
            vec![state(0, 120_000, 0), state(1, 120_000, 0)],
            vec![slice(0, 60, warm_bars(60, 50, 0))],
        );
        let pr = p.rt_probe().expect("core");
        let fired: Vec<bool> = (0..4).map(|i| pr.script_boot[i].1).collect();
        assert_eq!(fired, want.to_vec(), "{policy:?}");
        let n = want.iter().filter(|w| **w).count();
        assert_eq!(
            p.pine_counters().boot_guard_flattens,
            n as u64,
            "{policy:?}"
        );
        let inc = incidents(&p);
        assert_eq!(inc.len(), n, "{policy:?}");
        assert!(inc.iter().all(|x| x.event == pine_incident::BOOT_GUARD
            && x.position_1e6 == 120_000
            && x.detail == policy as u32));
        // The first fresh quotes: every guarded script sells its 0.12 reduce-only; a standing /
        // protective stop rests in the broker (a stop fires only when touched).
        let mut ctx = RecCtx::new();
        for coin in 0..2 {
            p.on_tick(&quote(coin, T0 + 20), &mut ctx);
        }
        let sells = ctx
            .seen()
            .iter()
            .filter(|r| r.verb == 0 && r.order.side == core_types::Side::Ask)
            .collect::<Vec<_>>();
        assert_eq!(sells.len(), n, "{policy:?}: one close per guarded script");
        for s in sells {
            assert_eq!(s.order.qty.raw(), 120_000);
            assert!(s.order.is_reduce_only(), "the guard's close is reduce-only");
        }
        // At most once per boot: more quotes, no second guard.
        let mut ctx = RecCtx::new();
        drive(&mut p, &mut ctx, T0 + 30, T0 + 40, 5);
        assert_eq!(p.pine_counters().boot_guard_flattens, n as u64);
    }
}

/// I39: a Live boot is refused `6013` without venue facts — a symbol not read, an enabled coin
/// without a swept boot state, a slot-8 position no enabled script trades — and a Live route that
/// passes every check is still refused at `on_start` (PAPER ONLY, fail closed).
#[test]
fn live_boot_needs_venue_state() {
    fn live(scripts: Vec<PineScriptCfg>) -> PineBootCfg {
        let mut b = boot(scripts);
        b.route = SlotRoute::Live;
        b.slot.min_coalesce_ms = Some(100);
        b.slot.event_insn_budget = Some(100_000_000);
        b.slot.boot_exit_policy = Some(BootExitPolicy::Protective);
        b.slot_caps = SlotCaps8 {
            max_order_usd_1e6: 100_000_000,
            cap_instance_usd_1e6: 100_000_000,
            cap_day_usd_1e6: 1_000_000_000,
            max_open_orders: 8,
        };
        b.boot_state = vec![state(0, 0, 0)];
        b
    }
    let code = |b: PineBootCfg| PineStrategy::new().configure(b).err().map(|e| e.code.0);
    let s = || vec![script("supertrend.pine", 0)];
    let mut b = live(s());
    b.symbols[0].venue_read = false;
    assert_eq!(code(b), Some(6013), "facts not read from the venue");
    let mut b = live(s());
    b.boot_state.clear();
    assert_eq!(code(b), Some(6013), "no boot state for an enabled coin");
    let mut b = live(s());
    b.boot_state[0].resting_swept = false;
    assert_eq!(code(b), Some(6013), "the E-8 sweep did not confirm");
    let mut b = live(s());
    b.boot_state.push(state(1, 50_000, 0));
    assert_eq!(code(b), Some(6013), "an orphan venue position");
    // Every check passes: configure accepts, `on_start` refuses (no live arming in this crate).
    let mut p = PineStrategy::new();
    p.configure(live(s())).expect("live checks pass");
    p.supply_warm(warm_both()).expect("warm");
    assert!(p.on_start(&mut NoCtx).is_err());
    assert_eq!(p.last_boot_error().map(|e| e.code.0), Some(6005));
    assert!(p.rt_probe().is_none());
}

/// Risk item (a): off Live a non-empty venue boot state is refused at `configure` (6013) — paper
/// and replay boot flat; the cli builds it empty (`pine_boot`).
#[test]
fn boot_state_off_live_is_refused() {
    for route in [SlotRoute::Paper, SlotRoute::Off] {
        let mut b = boot(vec![script("supertrend.pine", 0)]);
        b.route = route;
        b.boot_state = vec![state(0, 0, 0)];
        let e = PineStrategy::new().configure(b).expect_err("refused");
        assert_eq!((e.code.0, e.key), (6013, "boot_state"), "{route:?}");
    }
}

/// pin:08-epoch-zero-refused: `oid_epoch == 0` is a load refusal (every own fill would decode as
/// a prior boot's and paper would re-fire without bound).
#[test]
fn oid_epoch_zero_is_refused() {
    let mut b = boot(vec![script("supertrend.pine", 0)]);
    b.oid_epoch = 0;
    let e = PineStrategy::new().configure(b).expect_err("refused");
    assert_eq!((e.code.0, e.key), (6006, "oid_epoch"));
}

/// R1 (impl-Bp §3.9, verbatim): the manifest's chart symbols and `[script.symbols]` targets,
/// manifest order, deduplicated, resolved without booting; an unresolved key is `6006`.
#[test]
fn route_symbols_resolves_the_manifest_without_booting() {
    let mut a = script("supertrend.pine", 1);
    a.enabled = false;
    let mut b = script("htf_close.pine", 0);
    b.symbols = vec![("BINANCE:ETHUSDT".to_string(), "hyperliquid:eth".to_string())];
    let m = core_config::pine::PineManifest {
        scripts: vec![a, b, script("sma_always.pine", 1)],
    };
    // A fn item: the output borrow is tied to the input (the R1 bound is higher-ranked).
    fn resolve(k: &str) -> Option<(u32, &str)> {
        match k {
            "hyperliquid:btc" => Some((sym(0), "hyperliquid:BTC")),
            "hyperliquid:eth" => Some((sym(1), "hyperliquid:ETH")),
            _ => None,
        }
    }
    let r = strategy_pine::route_symbols(&m, resolve).expect("resolved");
    assert_eq!(
        r,
        vec![
            (sym(1), "hyperliquid:ETH".to_string()),
            (sym(0), "hyperliquid:BTC".to_string())
        ]
    );
    let mut bad = m.clone();
    bad.scripts[2].symbol = "hyperliquid:doge".to_string();
    assert_eq!(
        strategy_pine::route_symbols(&bad, resolve).map_err(|d| d.0),
        Err(6006)
    );
}
