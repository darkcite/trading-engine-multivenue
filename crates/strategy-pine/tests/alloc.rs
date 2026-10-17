// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! I1 at T10–T14: after `on_start` returns (warm-up merge-walk included), the member allocates
//! nothing — prints building bars, quotes confirming them, HTF closes stepping sub-program
//! instances, `lookahead` containing-bar ticks, bar evaluations (cold and committing), realtime
//! tick evaluations under coalescing and the per-event budget, `calc_on_order_fills`, commits,
//! reconcile and dispatch (IoC, maker, modify, cancel) through a scripted ctx, settle points,
//! fills, the counted-and-dropped callbacks and the cold view accessors — 0 B under the
//! workspace's `CountingAllocator`. (The full I1 arms are T23's.)

mod common;

use common::*;
use core_alloc::{AllocGuard, CountingAllocator};
use core_config::pine::{PineLimitPrimitive, PineMode};
use core_types::{Fill, FILL_ORIGIN_PAPER};
use strategy_core::{PineScriptView, Strategy, StrategyCounters};

// Does not count libtest's runner (the main thread), HK14.
#[global_allocator]
static G: CountingAllocator = CountingAllocator::new();

#[test]
fn nothing_allocates_after_on_start() {
    let mut scripts = vec![
        script("htf_close.pine", 0),
        script("htf_la.pine", 0),
        script("sma_always.pine", 1),
        script("rt_entry.pine", 0),
        script("rt_cof.pine", 1),
        script("rt_maker.pine", 1),
    ];
    for s in &mut scripts {
        s.max_open = 2;
    }
    for s in &mut scripts[3..] {
        s.mode = PineMode::Realtime;
        s.coalesce_ms = 20_000;
        s.coalesce_bps = 5;
    }
    scripts[5].limit_primitive = PineLimitPrimitive::Maker;
    scripts[5].cap_instance_1e6 = 48_000_000;
    let mut b = boot(scripts);
    b.slot.event_insn_budget = Some(100_000_000);
    let mut p = start(
        b,
        vec![
            slice(0, 60, warm_bars(60, 300, 0)),
            slice(0, 14_400, warm_bars(14_400, 10, 0)),
            slice(1, 60, warm_bars(60, 300, 0)),
        ],
    );
    let mut v = [PineScriptView::default(); 8];
    let mut c = RecCtx::new();
    let before = p.rt_probe().expect("core");
    let mut done = 0usize;
    let g = AllocGuard::new();
    let mut i = 0i64;
    // 1 500 steps of 15 s: 6 h 15 m of tape, across the 04:00 HTF close.
    while i < 1500 {
        let t = T0 + i * 15_000;
        p.on_venue_event(
            &print(0, t, 100_000_000 + (i % 40) * 10_000, 1_000_000),
            &mut c,
        );
        p.on_venue_event(
            &print(1, t + 3, 50_000_000 + (i % 30) * 10_000, 2_000_000),
            &mut c,
        );
        p.on_tick(&quote(0, t + 7), &mut c);
        p.on_tick(&quote(1, t + 9), &mut c);
        // Fill every third new IoC submit in full (a paper judgment), after the pump.
        while done < c.n {
            let r = c.recs[done % REC_CAP];
            if r.verb == 0 && r.order.kind == core_fill::ORDER_KIND_IOC && done % 3 == 0 {
                let mut f = Fill::new(
                    r.order.ts_ns + 1_000_000,
                    r.order.sym,
                    r.order.side,
                    r.order.px,
                    r.order.qty,
                    r.order.client_oid,
                );
                f.strategy_id = 8;
                f.origin = FILL_ORIGIN_PAPER;
                p.on_fill(&f, &mut c);
            }
            done += 1;
        }
        p.on_timer(1, &mut c);
        let _ = p.pine_counters();
        let _ = p.pine_scripts_view(&mut v);
        i += 1;
    }
    p.on_stop(&mut c);
    g.assert_zero();
    let after = p.rt_probe().expect("core");
    let k = p.pine_counters();
    assert_eq!(k.callbacks_ignored, 1500);
    // The arms ran: bars confirmed, scripts and instances stepped, a containing bar ticked,
    // realtime ticks, commits, every verb dispatched, settle points, fills, cof evaluations.
    assert!(k.bars_confirmed >= 370);
    assert!(after.script_state[0].0 >= before.script_state[0].0 + 370);
    assert!(after.sub_state[0].2 > before.sub_state[0].2);
    assert!(after.sub_run[1].1 > before.sub_run[1].1);
    assert!(after.intents_committed > 0);
    assert!(k.tick_evals > 0 && k.ticks_coalesced > 0);
    assert!(k.submits > 0 && k.cancels > 0 && k.modifies > 0, "{k:?}");
    assert!(k.fills > 0 && k.cof_evals > 0 && k.cancelled_reported > 0);
    assert_eq!(v[0].sym, sym(0));
    fault_disable_flatten_is_zero_alloc();
}

/// I1 (§7.10 `pine_fault_disable_flatten_is_zero_alloc`, T15): a VM fault holding a position —
/// the D13 disable, the incident push, the flatten and its retries through the fast phase into
/// `flatten_stuck` and the slow phase — and the incident view allocate nothing. Run from the one
/// `#[test]` (the counting allocator is process-global: a second test thread would count).
fn fault_disable_flatten_is_zero_alloc() {
    let mut s = script("rt_fault.pine", 0);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = 0;
    let mut p = start(boot(vec![s]), vec![slice(0, 60, warm_bars(60, 50, 0))]);
    let mut c = RecCtx::new();
    let mut inc = [strategy_core::PineIncident::default(); 8];
    let g = AllocGuard::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut c);
    p.on_tick(&quote(0, T0 + 20), &mut c);
    let o = c.recs[0].order;
    let mut f = Fill::new(
        o.ts_ns + 1_000_000,
        o.sym,
        o.side,
        o.px,
        o.qty,
        o.client_oid,
    );
    f.strategy_id = 8;
    f.origin = FILL_ORIGIN_PAPER;
    p.on_fill(&f, &mut c);
    let mut t = T0 + 30;
    while t < T0 + 3 * MIN {
        p.on_venue_event(&print(0, t, 100_000_000, 1_000_000), &mut c);
        p.on_tick(&quote(0, t + 5), &mut c);
        let _ = p.pine_incidents_view(&mut inc, 0);
        t += 2_000;
    }
    g.assert_zero();
    let k = p.pine_counters();
    assert_eq!(k.scripts_faulted, 1);
    assert!(k.flatten_attempts > u64::from(strategy_pine::FLATTEN_RETRIES));
    assert_eq!(k.flatten_stuck, 1);
    assert!(k.incidents >= 2);
}
