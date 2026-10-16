// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.4 `subs.rs` + the member half of `bars.rs` (T8, T10): the per-confirm step order
//! (I17), the `lookahead` containing bar (A2), the series never ahead of the bar (I18), `BarClose`
//! never ticked (I23), and the member's bar counters (I14, §4.3.4). The builder's own I12–I15
//! tests are `bars::tests`.

mod common;

use common::*;
use strategy_core::{Strategy, StrategyCounters};

/// The `tape` price of the print at `t`, `from` + k × step.
fn px_at(from: i64, t: i64, step: i64) -> i64 {
    100_000_000 + ((t - from) / step) * 10_000
}

/// I17: at the 1 m bar that closes at 04:00, the 4 h row is time-confirmed and its instance
/// stepped and published BEFORE the parent's `step_bar` — the parent's plot is that 4 h close.
/// I18: no confirmed element closes after the evaluating bar; I23: nothing is ticked.
#[test]
fn confirm_step_order() {
    let mut p = start(boot(vec![script("htf_close.pine", 0)]), Vec::new());
    let pr = p.rt_probe().expect("core");
    assert_eq!((pr.n_rows, pr.n_subs), (2, 1));
    let step = 20_000;
    let mut t = T0;
    while t < T0 + 240 * MIN {
        p.on_venue_event(&print(0, t, px_at(T0, t, step), 1_000_000), &mut NoCtx);
        let pr = p.rt_probe().expect("core");
        // I18: the sub series (row 1) never closes past the chart's last confirmed bar.
        if pr.sub_run[0].2 > 0 {
            assert!(pr.row_last[1].close_ms <= pr.row_last[0].close_ms);
        }
        assert!(!pr.sub_run[0].3 .0, "a lookahead_off instance has no tail");
        t += step;
    }
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.sub_run[0].2, 0, "the 4 h bar is still forming");
    // The print at 04:00:00 confirms the 03:59 bar; the chart close time-confirms 00:00–04:00.
    p.on_venue_event(
        &print(
            0,
            T0 + 240 * MIN,
            px_at(T0, T0 + 240 * MIN, step),
            1_000_000,
        ),
        &mut NoCtx,
    );
    let pr = p.rt_probe().expect("core");
    let htf = pr.row_last[1];
    assert_eq!(
        (htf.open_ms, htf.close_ms, htf.index),
        (T0, T0 + 240 * MIN, 0)
    );
    assert_eq!(pr.sub_run[0].2, 1, "published before the parent ran");
    assert_eq!(pr.row_last[0].close_ms, T0 + 240 * MIN);
    #[allow(clippy::cast_precision_loss)]
    let want = htf.close_1e6 as f64 / 1e6;
    assert!(
        (pr.script_plot0[0] - want).abs() < 1e-9,
        "the parent read the HTF close {want}, got {}",
        pr.script_plot0[0]
    );
    // I23: a BarClose parent and its non-lookahead instance are never ticked.
    assert_eq!((pr.script_run[0].3, pr.sub_run[0].1), (0, 0));
    assert_eq!((pr.script_state[0].0, pr.sub_state[0].2), (240, 1));
}

/// I18: after every callback, no confirmed element of any instance closes after the chart's
/// last confirmed bar; only a `lookahead` tail (`last_confirmed = false`) may reach past it.
#[test]
fn series_never_ahead_of_the_bar() {
    // Five instances on btc: 5 × 700k fits the paper default (pin:08-sub-charge-own-bound).
    let (mut a, mut b) = (script("htf_two.pine", 0), script("htf_la.pine", 0));
    a.insn_budget = 700_000;
    b.insn_budget = 700_000;
    let mut p = start(boot(vec![a, b]), Vec::new());
    let mut t = T0;
    while t < T0 + 490 * MIN {
        p.on_venue_event(&print(0, t, 100_000_000, 1_000_000), &mut NoCtx);
        let pr = p.rt_probe().expect("core");
        let chart_close = pr.row_last[0].close_ms;
        for k in 0..pr.n_subs {
            let (_, _, len, tail, _, la) = pr.sub_run[k];
            let row = pr.sub_state[k].0;
            if len > 0 {
                assert!(pr.row_last[row].close_ms <= chart_close, "instance {k}");
            }
            assert!(
                !tail.0 || (la && !tail.1),
                "only a lookahead tail, developing"
            );
        }
        t += 25_000;
    }
}

/// A2 / CAR-04-1: a `lookahead` instance steps the HTF bar containing the chart bar's open, as a
/// developing tail (`last_confirmed = false`), and its own close supersedes it.
#[test]
fn lookahead_containing_bar_is_stepped_developing() {
    let mut p = start(boot(vec![script("htf_la.pine", 0)]), Vec::new());
    let pr = p.rt_probe().expect("core");
    assert!(pr.sub_run[0].5, "the instance is the lookahead one");
    tape(&mut p, 0, T0, T0 + 3 * MIN + 1, 20_000);
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.sub_run[0].3, (true, false), "a developing tail");
    assert_eq!(pr.sub_run[0].2, 0, "nothing confirmed yet");
    assert!(
        pr.sub_run[0].1 >= 3,
        "one containing-bar tick per chart confirm"
    );
    assert_eq!(pr.sub_run[0].0, 0);
    // The last print lands at 04:00:00.001: the 03:59 bar confirms, the chart close confirms
    // 00:00–04:00, and the containing bar of 03:59 is that confirmed bar (no new tick).
    tape(&mut p, 0, T0 + 3 * MIN + 1, T0 + 240 * MIN + 2, 20_000);
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.sub_run[0].2, 1, "the 4 h close is published confirmed");
    assert!(!pr.sub_run[0].3 .0, "and replaced the tail");
    assert_eq!(pr.sub_run[0].0, 1);
    // The 04:00 chart bar's containing bar is the new forming 4 h bar: a tail again.
    tape(&mut p, 0, T0 + 240 * MIN + 2, T0 + 241 * MIN + 3, 20_000);
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.sub_run[0].3, (true, false));
    assert_eq!(pr.sub_run[0].2, 1);
    // The parent itself (BarClose) is never ticked (I23).
    assert_eq!(pr.script_run[0].3, 0);
}

/// I18: identical `(key, mode)` instances share one series (the T7 half is in `load.rs`), and a
/// shared instance is stepped once per HTF bar however many parents read it.
#[test]
fn identical_subprograms_share_one_series() {
    let mut p = start(
        boot(vec![
            script("htf_close.pine", 0),
            script("htf_close.pine", 0),
        ]),
        Vec::new(),
    );
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.n_subs, 1);
    assert_eq!(pr.script_series[0][0].0, pr.script_series[1][0].0);
    tape(&mut p, 0, T0, T0 + 240 * MIN + 1, 30_000);
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.sub_state[0].2, 1, "one step_bar for two parents");
    assert_eq!(pr.script_plot0[0].to_bits(), pr.script_plot0[1].to_bits());
}

/// I23: a BarClose script and its non-lookahead sub-programs never receive `step_tick`, through
/// prints, quotes and HTF closes.
#[test]
fn barclose_parent_subs_are_never_ticked() {
    let mut p = start(boot(vec![script("htf_two.pine", 0)]), Vec::new());
    let mut t = T0;
    while t < T0 + 300 * MIN {
        p.on_venue_event(&print(0, t, 100_000_000, 1_000_000), &mut NoCtx);
        p.on_tick(&quote(0, t + 1), &mut NoCtx);
        t += 45_000;
    }
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.script_run[0].3, 0);
    for k in 0..pr.n_subs {
        assert_eq!(pr.sub_run[k].1, 0, "instance {k} ticked");
        assert!(pr.sub_run[k].0 > 0, "instance {k} stepped at its closes");
    }
}

/// The member's bar counters: a late print is dropped and counted (I14), the boot bucket's bar
/// is counted partial (§4.3.4), a quote of another symbol is ignored and uncounted, and E only
/// moves forward (§4.4.1).
#[test]
fn member_counts_late_partial_and_regressions() {
    let mut p = start(boot(vec![script("supertrend.pine", 0)]), Vec::new());
    p.on_venue_event(&print(0, T0 + 5_000, 100_000_000, 1_000_000), &mut NoCtx);
    p.on_venue_event(&print(0, T0 + MIN + 5, 100_000_000, 1_000_000), &mut NoCtx);
    let c = p.pine_counters();
    assert_eq!((c.bars_confirmed, c.bars_partial_boot), (1, 1));
    // A print back in the confirmed bucket, ingress-ordered after: late, and E regresses.
    p.on_venue_event(&print(0, T0 + 6_000, 1, 1), &mut NoCtx);
    let c = p.pine_counters();
    assert_eq!((c.prints_late, c.event_ts_regressions), (1, 1));
    assert_eq!(
        p.rt_probe().expect("core").row_last[0].high_1e6,
        100_000_000
    );
    // eth is a configured symbol but no row reads it: ignored, uncounted.
    p.on_tick(&quote(1, T0 + 2 * MIN), &mut NoCtx);
    assert_eq!(p.pine_counters().callbacks_ignored, 0);
    // A hole: the next bar after two missing minutes carries GAP_BEFORE.
    p.on_venue_event(&print(0, T0 + 4 * MIN, 100_000_000, 1_000_000), &mut NoCtx);
    p.on_tick(&quote(0, T0 + 5 * MIN), &mut NoCtx);
    let b = p.rt_probe().expect("core").row_last[0];
    assert_eq!(b.index, 2);
    assert_ne!(b.flags & pine_ir::barflag::GAP_BEFORE, 0);
}
