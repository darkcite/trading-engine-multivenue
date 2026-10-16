// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.4 `warm.rs` (T9): slice validation, the lag rule, renumbering and masking, the
//! merge-walk (I15 warm states, I19), the cold fallback and its gate (I16).

mod common;

use common::*;
use pine_ir::barflag;
use strategy_core::{Strategy, StrategyCounters};

/// I15 + I19: warm bars are renumbered `0..n` and masked to `CONFIRMED | GAP_BEFORE | VOL_SAT`,
/// the last gains `LAST_CONFIRMED_HISTORY`; every one is stepped (`bars_seen == n`), and live
/// bars continue at `n`.
#[test]
fn warm_bars_are_renumbered_and_masked() {
    let mut bars = warm_bars(60, 400, 0);
    bars[5].flags |= barflag::GAP_BEFORE | barflag::VOL_SAT;
    let mut p = start(
        boot(vec![script("supertrend.pine", 0)]),
        vec![slice(0, 60, bars)],
    );
    let pr = p.rt_probe().expect("core");
    let (next, last_open, forming, warm) = pr.row_run[0];
    assert_eq!(
        (next, last_open, forming, warm),
        (400, T0 - MIN, false, true)
    );
    let last = pr.row_last[0];
    assert_eq!(last.index, 399);
    assert_eq!(
        last.flags,
        barflag::CONFIRMED | barflag::LAST_CONFIRMED_HISTORY
    );
    assert_eq!(pr.script_state[0].0, 400, "I19: bars_seen == index + 1");
    assert_eq!((pr.rows_cold, p.pine_counters().scripts_cold), (0, 0));
    assert!(!pr.script_run[0].0);
    assert_eq!(p.pine_counters().bar_evals, 400);
    // The first live bar is index 400, `CONFIRMED | REALTIME`, no history flag.
    tape(&mut p, 0, T0 + 1, T0 + MIN, 7_000);
    p.on_tick(&quote(0, T0 + MIN), &mut NoCtx);
    let pr = p.rt_probe().expect("core");
    let b = pr.row_last[0];
    assert_eq!((b.index, b.open_ms), (400, T0));
    assert_eq!(b.flags, barflag::CONFIRMED | barflag::REALTIME);
    assert_eq!(pr.script_state[0].0, 401);
}

/// The lag rule: a slice whose last bar is more than `max_warm_lag_bars` buckets before the boot
/// bucket makes the row cold; inside the lag the row is warm and the first live bar is gapped.
#[test]
fn stale_bar_file_makes_the_row_cold() {
    let p = start(
        boot(vec![script("supertrend.pine", 0)]),
        vec![slice(0, 60, warm_bars(60, 400, 3))],
    );
    let pr = p.rt_probe().expect("core");
    assert_eq!((pr.rows_cold, pr.row_run[0].3), (1, false));
    assert_eq!(
        (pr.script_state[0].0, p.pine_counters().scripts_cold),
        (0, 1)
    );
    // `max_warm_lag_bars = 3` admits it; the first live bar carries GAP_BEFORE.
    let mut cfg = boot(vec![script("supertrend.pine", 0)]);
    cfg.slot.max_warm_lag_bars = 3;
    let mut p = start(cfg, vec![slice(0, 60, warm_bars(60, 400, 3))]);
    let pr = p.rt_probe().expect("core");
    assert_eq!((pr.rows_cold, pr.script_state[0].0), (0, 400));
    tape(&mut p, 0, T0, T0 + MIN, 10_000);
    p.on_tick(&quote(0, T0 + MIN), &mut NoCtx);
    let b = p.rt_probe().expect("core").row_last[0];
    assert_eq!(b.index, 400);
    assert_ne!(b.flags & barflag::GAP_BEFORE, 0);
}

/// Every §4.3.6 validation failure makes the row cold, never a refusal.
#[test]
fn invalid_slice_makes_the_row_cold() {
    let cases: [fn(&mut Vec<pine_ir::Bar>); 6] = [
        |b| b[3].flags = 0,       // not CONFIRMED
        |b| b.swap(3, 4),         // not ascending
        |b| b[3].open_ms += 1,    // misaligned open
        |b| b[3].close_ms += MIN, // close ≠ barclock::close_ms
        |b| b.truncate(0),        // empty
        |b| {
            // the last bar reaches into the boot bucket
            let l = b.len() - 1;
            b[l].open_ms = T0;
            b[l].close_ms = T0 + MIN;
        },
    ];
    for (k, f) in cases.iter().enumerate() {
        let mut bars = warm_bars(60, 400, 0);
        f(&mut bars);
        let p = start(
            boot(vec![script("supertrend.pine", 0)]),
            vec![slice(0, 60, bars)],
        );
        let pr = p.rt_probe().expect("core");
        assert_eq!((pr.rows_cold, pr.script_state[0].0), (1, 0), "case {k}");
        assert_eq!(p.pine_counters().scripts_cold, 1, "case {k}");
    }
    // A short slice (fewer than `warmup_bars`) is cold too; a duplicate slice for one row is.
    let p = start(
        boot(vec![script("sma_always.pine", 0)]),
        vec![slice(0, 60, warm_bars(60, 2, 0))],
    );
    assert_eq!(p.rt_probe().expect("core").rows_cold, 1);
    let p = start(
        boot(vec![script("supertrend.pine", 0)]),
        vec![
            slice(0, 60, warm_bars(60, 400, 0)),
            slice(0, 60, warm_bars(60, 400, 0)),
        ],
    );
    assert_eq!(p.rt_probe().expect("core").rows_cold, 1);
}

/// I16: a cold script evaluates every live bar but commits nothing until its chart row holds
/// `warmup_bars` confirmed bars; from that evaluation on it commits (counted until 8d).
#[test]
fn cold_script_discards_until_warm_then_commits() {
    let mut p = start(boot(vec![script("sma_always.pine", 0)]), Vec::new());
    assert_eq!(p.pine_counters().scripts_cold, 1);
    let mut turned = None;
    let mut k = 0i64;
    while k < 12 {
        tape(&mut p, 0, T0 + k * MIN, T0 + (k + 1) * MIN, 20_000);
        p.on_tick(&quote(0, T0 + (k + 1) * MIN), &mut NoCtx);
        let c = p.pine_counters();
        let pr = p.rt_probe().expect("core");
        match turned {
            None if c.scripts_cold == 0 => {
                turned = Some(k);
                assert!(pr.intents_unrouted > 0, "the turning evaluation commits");
            }
            None => assert_eq!(
                pr.intents_unrouted, 0,
                "bar {k}: a cold script commits nothing"
            ),
            Some(_) => {}
        }
        k += 1;
    }
    let t = turned.expect("the script warms on the live tape");
    assert!(t >= 1, "sma(5) is not warm on its first bar");
    let c = p.pine_counters();
    assert!(c.intents_discarded > 0);
    assert_eq!(c.bar_evals, 12);
    assert_eq!(p.rt_probe().expect("core").script_state[0].0, 12);
}

/// The merge-walk (§4.3.6): sub rows first at a tie, and on history a `lookahead` instance steps
/// its containing HTF bar complete (`step_tick`, a confirmed tail) before the chart bar; the
/// HTF bar's own close supersedes it. Both rows warm → the script is warm; I19 on both.
#[test]
fn merge_walk_steps_subs_and_the_lookahead_history_bar() {
    let p = start(
        boot(vec![script("htf_la.pine", 0)]),
        vec![
            slice(0, 60, warm_bars(60, 300, 0)),
            slice(0, 14_400, warm_bars(14_400, 10, 0)),
        ],
    );
    let pr = p.rt_probe().expect("core");
    assert_eq!((pr.rows_cold, p.pine_counters().scripts_cold), (0, 0));
    assert_eq!(pr.script_state[0].0, 300);
    let (bar_evals, tick_evals, len, tail, faulted, la) = pr.sub_run[0];
    assert!(la && !faulted);
    assert_eq!(pr.sub_state[0].2, 10, "I19 on the instance");
    assert_eq!(bar_evals, 10);
    // The 1 m bars inside 4 h bars that close after them: 300 bars − the ones closing at a 4 h
    // close (two: T0 − 4 h and T0 itself) each tick their containing bar once.
    assert_eq!(tick_evals, 298);
    assert!(len >= 2);
    assert!(!tail.0, "the last 4 h close superseded the history tail");
    assert_eq!(pr.row_last[1].index, 9);
    // Warm-up commits nothing (§4.3.6 step 3).
    assert_eq!(pr.intents_unrouted, 0);
}
