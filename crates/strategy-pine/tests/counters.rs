// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! T17 (member side): what the cli's 5 s block mirrors — a bare boot reports zeros, and the
//! per-script view carries the script's own counts, position and D13 state.

mod common;

use common::*;
use core_config::pine::PineMode;
use core_types::{Fill, FILL_ORIGIN_PAPER};
use strategy_core::{
    PineCounters, PineIncident, PineScriptView, PineSeqRec, PineTapeRec, Strategy, StrategyCounters,
};
use strategy_pine::PineStrategy;

/// A bare boot (one script, warm, no callback yet): every activity counter is zero; the view
/// names the script and holds zeros; no incident, no tape row.
#[test]
fn bare_boot_reports_zeros() {
    assert_eq!(PineStrategy::new().pine_counters(), PineCounters::default());
    let p = start(
        boot(vec![script("supertrend.pine", 0)]),
        vec![slice(0, 60, warm_bars(60, 50, 0))],
    );
    let k = p.pine_counters();
    let want = PineCounters {
        scripts_loaded: 1,
        scripts_enabled: 1,
        // The boot itself: the warm-up walk's evaluations and discards, and coldness.
        scripts_cold: k.scripts_cold,
        bar_evals: k.bar_evals,
        intents_discarded: k.intents_discarded,
        ..PineCounters::default()
    };
    assert_eq!(k, want);
    let mut v = [PineScriptView::default(); 8];
    assert_eq!(p.pine_scripts_view(&mut v), 1);
    let r = v[0];
    assert_eq!(
        r.sha_lo,
        u64::from_be_bytes(
            script("supertrend.pine", 0).sha256[..8]
                .try_into()
                .expect("8")
        )
    );
    assert_eq!((r.enabled, r.disabled, r.fault_kind), (1, 0, 0));
    assert_eq!(u64::from(r.cold), k.scripts_cold);
    assert_eq!(
        (r.submits, r.submit_errors, r.fills, r.position_1e6),
        (0, 0, 0, 0)
    );
    assert_eq!((r.sym, r.timeframe_s), (sym(0), 60));
    let mut inc = [PineIncident::default(); 4];
    let mut dec = [PineTapeRec::default(); 4];
    let mut seq = [PineSeqRec::default(); 4];
    assert_eq!(p.pine_incidents_view(&mut inc, 0), 0);
    assert_eq!(p.pine_tape_view(&mut dec, 0), 0);
    assert_eq!(p.pine_seq_view(&mut seq, 0), 0);
}

/// The view follows the script: a submit and its fill show up in its row only.
#[test]
fn view_counts_per_script() {
    let mut s = script("rt_entry.pine", 0);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = 0;
    let mut p = start(
        boot(vec![s, script("sma_always.pine", 1)]),
        vec![
            slice(0, 60, warm_bars(60, 50, 0)),
            slice(1, 60, warm_bars(60, 50, 0)),
        ],
    );
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    let o = ctx.seen()[0].order;
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
    p.on_fill(&f, &mut ctx);
    let mut v = [PineScriptView::default(); 8];
    assert_eq!(p.pine_scripts_view(&mut v), 2);
    assert_eq!(
        (v[0].submits, v[0].fills, v[0].position_1e6),
        (1, 1, o.qty.raw())
    );
    assert_eq!(v[0].avg_px_1e6, o.px.raw());
    assert_eq!((v[1].submits, v[1].fills, v[1].position_1e6), (0, 0, 0));
}
