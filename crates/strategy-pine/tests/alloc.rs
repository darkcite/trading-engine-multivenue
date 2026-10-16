// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! I1 at T10: after `on_start` returns (warm-up merge-walk included), the member allocates
//! nothing on the bar path — prints building bars, quotes confirming them, HTF closes stepping
//! sub-program instances, `lookahead` containing-bar ticks, script evaluations (cold and
//! committing), the counted-and-dropped callbacks and the cold view accessors — 0 B under the
//! workspace's `CountingAllocator`. (The full I1 arms, with dispatch and fills, are T23's.)

mod common;

use common::*;
use core_alloc::{AllocGuard, CountingAllocator};
use strategy_core::{PineScriptView, Strategy, StrategyCounters};

// Does not count libtest's runner (the main thread), HK14.
#[global_allocator]
static G: CountingAllocator = CountingAllocator::new();

#[test]
fn nothing_allocates_after_on_start() {
    let mut p = start(
        boot(vec![
            script("htf_close.pine", 0),
            script("htf_la.pine", 0),
            script("sma_always.pine", 1),
        ]),
        vec![
            slice(0, 60, warm_bars(60, 300, 0)),
            slice(0, 14_400, warm_bars(14_400, 10, 0)),
        ],
    );
    let mut v = [PineScriptView::default(); 8];
    let mut c = NoCtx;
    let before = p.rt_probe().expect("core");
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
    // intents committed (counted until 8d).
    assert!(k.bars_confirmed >= 370);
    assert!(after.script_state[0].0 >= before.script_state[0].0 + 370);
    assert!(after.sub_state[0].2 > before.sub_state[0].2);
    assert!(after.sub_run[1].1 > before.sub_run[1].1);
    assert!(after.intents_unrouted > 0);
    assert_eq!(v[0].sym, sym(0));
}
