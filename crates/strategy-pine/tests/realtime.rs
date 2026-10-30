// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.5 `realtime.rs` (T11): coalescing in event time (I20), the per-event instruction
//! budget (I21), W21 and `calc_on_order_fills` (I22), and I23.

mod common;

use common::*;
use core_config::pine::{PineMode, PineScriptCfg};
use core_types::{Fill, Side, FILL_ORIGIN_PAPER};
use strategy_core::{Strategy, StrategyCounters};
use strategy_pine::PineStrategy;

/// A realtime script cfg on `coin`.
fn rt(file: &str, coin: usize, coalesce_ms: u32, coalesce_bps: u32) -> PineScriptCfg {
    let mut s = script(file, coin);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = coalesce_ms;
    s.coalesce_bps = coalesce_bps;
    s
}

/// Warm 1-minute rows for both coins (the scripts start warm).
fn warm() -> Vec<strategy_pine::WarmSlice> {
    vec![
        slice(0, 60, warm_bars(60, 50, 0)),
        slice(1, 60, warm_bars(60, 50, 0)),
    ]
}

/// The paper default budget unless `budget` is given.
fn member(scripts: Vec<PineScriptCfg>, budget: Option<u32>) -> PineStrategy {
    let mut b = boot(scripts);
    b.slot.event_insn_budget = budget;
    start(b, warm())
}

/// One fixed input stream: prints every `step` ms over `[from, to)` on coin 0 at a rising price,
/// a quote after every fourth print. Optional wall pacing (`pace`) between events.
fn drive(p: &mut PineStrategy, ctx: &mut RecCtx, from: i64, to: i64, step: i64, pace: bool) {
    let mut t = from;
    let mut k = 0i64;
    while t < to {
        p.on_venue_event(&print(0, t, 100_000_000 + k * 20_000, 1_000_000), ctx);
        if k % 4 == 3 {
            p.on_tick(&quote(0, t + 1), ctx);
        }
        if pace && k % 64 == 0 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        t += step;
        k += 1;
    }
}

/// I20: the admissions (and the order stream) are a function of the input stream alone — the
/// same stream delivered straight and with wall-clock pauses gives identical results.
#[test]
fn coalescing_is_event_time() {
    let run = |pace: bool| {
        let mut p = member(vec![rt("rt_entry.pine", 0, 250, 0)], None);
        let mut ctx = RecCtx::new();
        drive(&mut p, &mut ctx, T0 + 10, T0 + 3 * MIN, 40, pace);
        let k = p.pine_counters();
        let orders: Vec<(u8, u64, i64, i64, u64)> = ctx
            .seen()
            .iter()
            .map(|r| {
                (
                    r.verb,
                    r.order.ts_ns,
                    r.order.px.raw(),
                    r.order.qty.raw(),
                    r.order.client_oid,
                )
            })
            .collect();
        (k.tick_evals, k.ticks_coalesced, k.bar_evals, orders)
    };
    let a = run(false);
    let b = run(true);
    assert_eq!(a, b);
    // 250 ms coalescing over 40 ms prints: most updates wait, every bar's first is admitted.
    assert!(a.1 > a.0, "coalesced {} vs evaluated {}", a.1, a.0);
    // At least one admitted tick per bar (first update), 3 bars.
    assert!(a.0 >= 3);
    assert!(!a.3.is_empty(), "orders flowed");
}

/// I20: inside the window, the first update of a bar and a `coalesce_bps` move are admitted;
/// admitted ticks are otherwise ≥ `coalesce_ms` apart in E.
#[test]
fn first_update_and_bps_admit_inside_the_window() {
    // A 60 s window: only first updates are admitted on a flat tape.
    let mut p = member(vec![rt("rt_entry.pine", 0, 60_000, 0)], None);
    let mut ctx = RecCtx::new();
    let mut t = T0 + 10;
    while t < T0 + 2 * MIN {
        p.on_venue_event(&print(0, t, 100_000_000, 1_000_000), &mut ctx);
        t += 500;
    }
    let k = p.pine_counters();
    assert_eq!(k.tick_evals, 2, "one first update per bar");
    assert!(k.ticks_coalesced > 200);
    // 50 bps: a 1 % jump inside the window is admitted, a 0.1 % one is not.
    let mut p = member(vec![rt("rt_entry.pine", 0, 60_000, 50)], None);
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_venue_event(&print(0, T0 + 20, 100_100_000, 1_000_000), &mut ctx);
    assert_eq!(p.pine_counters().tick_evals, 1);
    p.on_venue_event(&print(0, T0 + 30, 101_000_000, 1_000_000), &mut ctx);
    assert_eq!(p.pine_counters().tick_evals, 2);
    // The window then measures from the last admitted close (101): +0.1 % refused.
    p.on_venue_event(&print(0, T0 + 40, 101_101_000, 1_000_000), &mut ctx);
    assert_eq!(p.pine_counters().tick_evals, 2);
    // A coalesced update stays pending and is re-checked at the next quote tick of the symbol
    // (the trailing edge): inside the window it waits; once 5 s of event time passed it is
    // admitted with no new print.
    let mut p = member(vec![rt("rt_entry.pine", 0, 5_000, 0)], None);
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_venue_event(&print(0, T0 + 20, 100_000_000, 1_000_000), &mut ctx);
    assert_eq!(p.pine_counters().tick_evals, 1);
    p.on_tick(&quote(0, T0 + 3_000), &mut ctx);
    assert_eq!(p.pine_counters().tick_evals, 1);
    p.on_tick(&quote(0, T0 + 5_010), &mut ctx);
    assert_eq!(p.pine_counters().tick_evals, 2);
    assert_eq!(p.pine_counters().ticks_coalesced, 2);
}

/// I21: no callback charges more than `event_insn_budget`; a confirm spends first, and the
/// ticks it leaves no room for are deferred (`ticks_budgeted`), then served round-robin.
#[test]
fn per_event_budget_never_exceeded() {
    // Two realtime scripts on coin 0, 1e6 each: the budget is exactly the load check's Σ.
    let mut p = member(
        vec![rt("rt_entry.pine", 0, 0, 0), rt("rt_cof.pine", 0, 0, 0)],
        Some(2_000_000),
    );
    let mut ctx = RecCtx::new();
    drive(&mut p, &mut ctx, T0 + 10, T0 + 5 * MIN, 1_000, false);
    let k = p.pine_counters();
    let pr = p.rt_probe().expect("core");
    assert!(pr.charged_max <= pr.event_budget, "I21");
    assert_eq!(pr.charged_max, 2_000_000);
    // A print that closes a bar evaluates both bars, leaving nothing for the ticks it opens.
    assert!(k.ticks_budgeted >= 2 * 4, "{}", k.ticks_budgeted);
    // Both scripts were ticked (round-robin), and every deferred update ran at the next print.
    assert!(pr.script_run[0].3 > 50 && pr.script_run[1].3 > 50);
}

/// I21: the budget never skips a confirmed bar — every bar of every script is evaluated even
/// when no tick can be admitted.
#[test]
fn bars_are_never_skipped_by_the_budget() {
    let mut three = vec![
        rt("rt_entry.pine", 0, 0, 0),
        rt("rt_entry.pine", 0, 0, 0),
        script("sma_always.pine", 0),
    ];
    for s in &mut three {
        s.max_open = 2; // Σ max_open ≤ 8 on one symbol (6008)
    }
    let mut p = member(three, Some(3_000_000));
    let mut ctx = RecCtx::new();
    let k0 = p.pine_counters();
    drive(&mut p, &mut ctx, T0 + 10, T0 + 10 * MIN + 10, 30_000, false);
    let k = p.pine_counters();
    // Nine bars closed by prints (the tenth is forming), three scripts each, none skipped.
    assert_eq!(k.bars_confirmed - k0.bars_confirmed, 9);
    assert_eq!(k.bar_evals - k0.bar_evals, 27);
    assert!(
        k.ticks_budgeted > 0,
        "the confirms left no room for the opening ticks"
    );
    let pr = p.rt_probe().expect("core");
    assert!(pr.charged_max <= pr.event_budget);
}

/// I22: every evaluation of a committing script — bar and tick — is followed by `apply_intents`.
#[test]
fn every_evaluation_commits() {
    let mut p = member(vec![rt("rt_entry.pine", 0, 0, 0)], None);
    let mut ctx = RecCtx::new();
    let b0 = p.pine_counters().bar_evals;
    drive(&mut p, &mut ctx, T0 + 10, T0 + 3 * MIN, 700, false);
    let k = p.pine_counters();
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.commits, (k.bar_evals - b0) + k.tick_evals);
    assert!(k.tick_evals > 100);
}

/// I22: unchanged intents over 40 ticks leave one order — 40 tick evaluations re-emit the same
/// `(Entry, "L")`, and the first fresh quote sends exactly one IoC.
#[test]
fn unchanged_intents_over_40_ticks_keep_one_order() {
    let mut p = member(vec![rt("rt_entry.pine", 0, 0, 0)], None);
    let mut ctx = RecCtx::new();
    let mut k = 0i64;
    while k < 40 {
        p.on_venue_event(
            &print(0, T0 + 10 + k * 100, 100_000_000, 1_000_000),
            &mut ctx,
        );
        k += 1;
    }
    assert_eq!(p.pine_counters().tick_evals, 40);
    assert_eq!(ctx.n, 0, "no fresh quote yet: nothing sent");
    p.on_tick(&quote(0, T0 + 4_100), &mut ctx);
    assert_eq!(ctx.n, 1);
    assert_eq!(ctx.seen()[0].verb, 0);
    // More ticks with the order in flight: still one.
    p.on_venue_event(&print(0, T0 + 4_200, 100_000_000, 1_000_000), &mut ctx);
    assert_eq!(ctx.n, 1);
}

/// I22: `calc_on_order_fills` runs one extra `step_tick` per fill callback.
#[test]
fn cof_runs_one_extra_tick_per_fill_callback() {
    let mut p = member(vec![rt("rt_cof.pine", 0, 60_000, 0)], None);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    assert_eq!(ctx.n, 1);
    let o = ctx.seen()[0].order;
    let ticks = p.pine_counters().tick_evals;
    let mut f = Fill::new(
        o.ts_ns + 5_000_000,
        o.sym,
        Side::Bid,
        o.px,
        o.qty,
        o.client_oid,
    );
    f.strategy_id = 8;
    f.origin = FILL_ORIGIN_PAPER;
    p.on_fill(&f, &mut ctx);
    let k = p.pine_counters();
    assert_eq!((k.fills, k.cof_evals), (1, 1));
    assert_eq!(
        k.tick_evals, ticks,
        "a cof evaluation is not a coalesced tick"
    );
    assert_eq!(p.rt_probe().expect("core").script_pos[0], o.qty.raw());
}

/// I23: a `BarClose` program never receives `step_tick`.
#[test]
fn barclose_programs_never_step_tick() {
    let mut p = member(
        vec![script("sma_always.pine", 0), script("htf_close.pine", 1)],
        None,
    );
    let mut ctx = RecCtx::new();
    drive(&mut p, &mut ctx, T0 + 10, T0 + 5 * MIN, 500, false);
    let pr = p.rt_probe().expect("core");
    assert_eq!((pr.script_run[0].3, pr.script_run[1].3), (0, 0));
    assert_eq!(p.pine_counters().tick_evals, 0);
    assert!(pr.script_run[0].2 > 0);
}
