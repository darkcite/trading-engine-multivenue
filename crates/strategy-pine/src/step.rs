// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.3.8 / §4.3.9 / §4.4.2 (T10): the live bar path — Trade prints and quote ticks into
//! the rows, every confirm into the sub-program stepping table and the per-confirm step order
//! (I17), the `lookahead` containing bar (A2, CAR-04-1), `bars_seen` (I19), the cold gate (I16).
//! The warm-up merge-walk (`warm`) reuses `step_sub_bar`, `tick_sub` and `eval_script`.
//!
//! Hot module (I4): no allocation, no clock (time arrives as event data, P5), index loops only.
//! The one `dyn` is `&mut dyn BarCtx` (`ctx::PineCtx`). The order path (8d) is not wired: a
//! committing evaluation's intents are counted in `Core::intents_unrouted` and dropped at the one
//! site 8d replaces (`commit`, with `broker.apply_intents(vm.intents(), now_ns)` then
//! `reconcile` + dispatch, §4.3.9 steps 5–6).
//!
//! Literal readings:
//! - pin:08-fault-latch-until-8e: a script fault, or a fault of any instance it reads, latches
//!   the script (never stepped again) and counts `scripts_faulted`; a sub fault also latches every
//!   instance that reads it. The D13 incident, disable and flatten are 8e (T15).
//! - pin:08-cold-check-at-eval: a cold script's I16 test runs at each evaluation, after the bar's
//!   confirm and before `step_bar`: the chart row's confirmed bars (`bar.index + 1`, warm bars
//!   included) ≥ `warmup_bars`, and every instance in its own table holds ≥ `history` confirmed
//!   elements. The first evaluation that passes commits.
//! - pin:08-lookahead-direct-only: the containing-bar `step_tick` runs for the `lookahead`
//!   instances in the confirming script's OWN table (§4.3.8 "each lookahead instance of those
//!   scripts"); a `lookahead` grandchild is stepped at its HTF closes only.
//! - pin:08-sub-now-ms: an instance's `now_ms` is its trigger's (the event time live, the bar's
//!   `close_ms` on warm-up history); pine-screen hands a sub the HTF bar's `close_ms`, the same
//!   value on history.
//! - pin:08-gapped-once-per-bar: `bars_gapped` counts a confirmed bar once when its hole is longer
//!   than the `max_bars_back` of any script charting the row.
//! - pin:08-realtime-ticks-8c: the third stepping-table row (a `Realtime` parent about to
//!   `step_tick`) and every chart `step_tick` are 8c (T11); this run steps bars only.

use crate::bars::{self, Confirmed, TradeOut};
use crate::ctx::PineCtx;
use crate::rt::Core;
use core_time::barclock;
use core_types::{ChannelEvent, Tick};
use pine_ir::{Bar, ExecMode, StepOutcome};
use strategy_core::PineCounters;

/// `E = max(E, ts)`; a regression leaves E and counts (§4.4.1).
#[inline]
fn advance(core: &mut Core, c: &mut PineCounters, ts_ns: core_time::NsTs) {
    if ts_ns < core.e_ns {
        c.event_ts_regressions = c.event_ts_regressions.wrapping_add(1);
    } else {
        core.e_ns = ts_ns;
    }
}

/// `BarCtx::now_ms` live: `ms_from_ns(sat_i64(anchor.wall_of(E)))` (§4.4.1).
#[inline]
fn now_ms(core: &Core) -> i64 {
    pine_ir::scale::ms_from_ns(i64::try_from(core.anchor.wall_of(core.e_ns)).unwrap_or(i64::MAX))
}

/// Whether `sym` feeds any row (a pine symbol).
#[inline]
pub(crate) fn is_pine_sym(core: &Core, sym: core_types::SymbolId) -> bool {
    let mut r = 0usize;
    while r < core.rows.len() {
        if core.rows[r].sym == sym {
            return true;
        }
        r += 1;
    }
    false
}

/// §4.4.2 `on_venue_event`, the Trade arm: one print of a pine symbol into every row of it.
pub(crate) fn on_trade(core: &mut Core, c: &mut PineCounters, ev: &ChannelEvent) {
    advance(core, c, ev.ts_ns);
    core.cb_seq = core.cb_seq.wrapping_add(1);
    let t = bars::t_ms(ev.venue_time_ms, ev.ts_ns, &core.anchor);
    let q = bars::qty_abs(ev.v1);
    let now = now_ms(core);
    let mut r = 0usize;
    while r < core.rows.len() {
        if core.rows[r].sym == ev.sym {
            match core.rows[r].on_trade(ev.v0, q, t) {
                TradeOut::Late => c.prints_late = c.prints_late.wrapping_add(1),
                TradeOut::ConfirmedAndOpened(cf) => confirm_row(core, c, r, &cf, now),
                // 8c (T11): `mark_updated(row)` — the realtime scripts' tick_pending.
                TradeOut::Updated | TradeOut::Opened => {}
            }
        }
        r += 1;
    }
}

/// §4.4.2 `on_tick`, the bar half: a quote tick of a pine symbol confirms its rows by time only
/// (I12). The quote itself (8c/8d) is not kept yet.
pub(crate) fn on_quote(core: &mut Core, c: &mut PineCounters, t: &Tick) {
    advance(core, c, t.ts_ns);
    core.cb_seq = core.cb_seq.wrapping_add(1);
    let tm = bars::t_ms(t.venue_time_ms, t.ts_ns, &core.anchor);
    let now = now_ms(core);
    let mut r = 0usize;
    while r < core.rows.len() {
        if core.rows[r].sym == t.sym {
            if let Some(cf) = core.rows[r].time_confirm(tm) {
                confirm_row(core, c, r, &cf, now);
            }
        }
        r += 1;
    }
}

/// Time-confirm every row in `mask` at `t_ms` (a chart or HTF close proves the bucket over).
fn time_confirm_rows(core: &mut Core, c: &mut PineCounters, mask: u32, t_ms: i64, now: i64) {
    let mut r = 0usize;
    while r < core.rows.len() {
        if mask & (1u32 << r) != 0 {
            if let Some(cf) = core.rows[r].time_confirm(t_ms) {
                confirm_row(core, c, r, &cf, now);
            }
        }
        r += 1;
    }
}

/// §4.4.2 `on_confirm(row, bar)`: the row's sub-program instances (children first — their own
/// children's rows time-confirmed first), then every script charting it, manifest order.
fn confirm_row(core: &mut Core, c: &mut PineCounters, r: usize, cf: &Confirmed, now: i64) {
    c.bars_confirmed = c.bars_confirmed.wrapping_add(1);
    if cf.partial {
        c.bars_partial_boot = c.bars_partial_boot.wrapping_add(1);
    }
    if cf.gap_len > 0 {
        let mut i = 0usize;
        while i < core.scripts.len() {
            let s = &core.scripts[i];
            if s.chart_row == r && cf.gap_len > i64::from(s.program.meta.max_bars_back) {
                c.bars_gapped = c.bars_gapped.wrapping_add(1);
                break;
            }
            i += 1;
        }
    }
    // Rows read by the instances on this row: their buckets ending by this close are over.
    let mut kids = 0u32;
    let mut s = 0usize;
    while s < core.subs.len() {
        if core.subs[s].row == r {
            kids |= core.subs[s].reads_rows;
        }
        s += 1;
    }
    if kids != 0 {
        time_confirm_rows(core, c, kids, cf.bar.close_ms, now);
    }
    s = 0;
    while s < core.subs.len() {
        if core.subs[s].row == r {
            step_sub_bar(core, c, s, &cf.bar, now);
        }
        s += 1;
    }
    let mut i = 0usize;
    while i < core.scripts.len() {
        if core.scripts[i].chart_row == r {
            live_script(core, c, i, &cf.bar, now);
        }
        i += 1;
    }
}

/// §4.3.9 for one script on a live confirmed chart bar.
fn live_script(core: &mut Core, c: &mut PineCounters, i: usize, bar: &Bar, now: i64) {
    if core.scripts[i].faulted {
        return;
    }
    // 1. sub-programs: the chart-close time-confirm first (the table's second row) …
    let reads = core.scripts[i].reads_rows;
    if reads != 0 {
        time_confirm_rows(core, c, reads, bar.close_ms, now);
    }
    // … then each `lookahead` instance whose containing HTF bar is not confirmed: its developing
    // bar, `step_tick` → tail `last_confirmed = false` (A2, CAR-04-1).
    let n = usize::from(core.scripts[i].subs.n);
    let mut k = 0usize;
    while k < n {
        let s = usize::from(core.scripts[i].subs.keys[k].1);
        k += 1;
        if !core.subs[s].sub.lookahead || core.subs[s].faulted {
            continue;
        }
        let row = &core.rows[core.subs[s].row];
        let contain = barclock::open_ms(bar.open_ms, row.tf_s);
        if row.last_open_ms != bars::NONE_MS && row.last_open_ms >= contain {
            continue; // the containing bar is confirmed and published
        }
        let Some(dev) = row.forming_bar() else {
            continue; // no print of it yet: nothing developing to step
        };
        if dev.open_ms != contain {
            continue;
        }
        tick_sub(core, c, s, &dev, now, false);
    }
    if core.scripts[i].faulted {
        return;
    }
    // 2–5.
    eval_script(core, c, i, bar, now, false);
}

/// One sub-program instance `step_bar` on its row's confirmed bar, then `publish_confirmed`.
pub(crate) fn step_sub_bar(core: &mut Core, c: &mut PineCounters, s: usize, bar: &Bar, now: i64) {
    if core.subs[s].faulted {
        return;
    }
    let Core { subs, series, .. } = core;
    let sub = &mut subs[s];
    let out = {
        let mut ctx = PineCtx {
            series,
            subs: sub.table.entries(),
            broker: None,
            now_ms: now,
        };
        sub.vm.step_bar(bar, &mut ctx)
    };
    if let StepOutcome::Fault(_) = out {
        sub_fault(core, c, s);
        return;
    }
    // I19 / impl-03 F17: fail fast.
    assert!(
        sub.vm.bars_seen() == u64::from(bar.index) + 1,
        "pine: sub-program bars_seen != bar.index + 1 (I19)"
    );
    let n_out = usize::from(sub.sub.n_out);
    series.publish_confirmed(s, &sub.vm.plots()[..n_out], bar.open_ms, bar.close_ms);
}

/// One `lookahead` instance `step_tick` on its containing HTF bar, published as the tail
/// (`last_confirmed`: `false` live, `true` on warm-up history). Once per callback (`ticked_cb`).
pub(crate) fn tick_sub(
    core: &mut Core,
    c: &mut PineCounters,
    s: usize,
    bar: &Bar,
    now: i64,
    last_confirmed: bool,
) {
    let cb = core.cb_seq;
    let Core { subs, series, .. } = core;
    let sub = &mut subs[s];
    if sub.faulted || sub.ticked_cb == cb {
        return;
    }
    // I23: only a `Realtime` instance is ever ticked (A2 compiles `lookahead` cones `Realtime`).
    debug_assert!(sub.sub.program.mode == ExecMode::Realtime);
    if sub.sub.program.mode != ExecMode::Realtime {
        return;
    }
    sub.ticked_cb = cb;
    let out = {
        let mut ctx = PineCtx {
            series,
            subs: sub.table.entries(),
            broker: None,
            now_ms: now,
        };
        sub.vm.step_tick(bar, &mut ctx)
    };
    if let StepOutcome::Fault(_) = out {
        sub_fault(core, c, s);
        return;
    }
    let n_out = usize::from(sub.sub.n_out);
    series.publish_tail(
        s,
        &sub.vm.plots()[..n_out],
        bar.open_ms,
        bar.close_ms,
        last_confirmed,
    );
}

/// §4.3.9 steps 2–5 for script `i` on `bar`: `open_bar`, `step_bar` (the one `dyn`), the I19
/// check, then commit — or discard (`history`: warm-up; a cold script, I16).
pub(crate) fn eval_script(
    core: &mut Core,
    c: &mut PineCounters,
    i: usize,
    bar: &Bar,
    now: i64,
    history: bool,
) {
    if core.scripts[i].faulted {
        return;
    }
    // I16: a cold script's gate, before the evaluation (pin:08-cold-check-at-eval).
    let committing = !history && (!core.scripts[i].cold || turns_warm(core, c, i, bar));
    let Core {
        scripts, series, ..
    } = core;
    let sc = &mut scripts[i];
    sc.broker.open_bar(bar);
    let out = {
        let mut ctx = PineCtx {
            series,
            subs: sc.subs.entries(),
            broker: Some(&sc.broker),
            now_ms: now,
        };
        sc.vm.step_bar(bar, &mut ctx)
    };
    c.bar_evals = c.bar_evals.wrapping_add(1);
    if let StepOutcome::Fault(_) = out {
        script_fault(core, c, i);
        return;
    }
    // §4.3.9 step 4, I19 (impl-03 F17): fail fast.
    assert!(
        sc.vm.bars_seen() == u64::from(bar.index) + 1,
        "pine: bars_seen != bar.index + 1 (I19)"
    );
    let n = sc.vm.intents().len() as u64;
    if committing {
        commit(core, n);
    } else {
        c.intents_discarded = c.intents_discarded.wrapping_add(n);
    }
}

/// The 8d site (§4.3.9 steps 5–6): `broker.apply_intents(vm.intents(), now_ns)?`, then
/// `reconcile` + dispatch. Until 8d the intents are counted and dropped here.
#[inline]
fn commit(core: &mut Core, n: u64) {
    core.intents_unrouted = core.intents_unrouted.wrapping_add(n);
}

/// I16: whether cold script `i` turns warm at this evaluation; flips it when so.
fn turns_warm(core: &mut Core, c: &mut PineCounters, i: usize, bar: &Bar) -> bool {
    let sc = &core.scripts[i];
    if u64::from(bar.index) + 1 < u64::from(sc.program.meta.warmup_bars) {
        return false;
    }
    let n = usize::from(sc.subs.n);
    let mut k = 0usize;
    while k < n {
        let s = usize::from(sc.subs.keys[k].1);
        if core.series.confirmed_len(s) < core.series.history(s) {
            return false;
        }
        k += 1;
    }
    core.scripts[i].cold = false;
    c.scripts_cold = c.scripts_cold.saturating_sub(1);
    // T18: the GUARD tape record is written here.
    true
}

/// pin:08-fault-latch-until-8e.
fn script_fault(core: &mut Core, c: &mut PineCounters, i: usize) {
    if core.scripts[i].faulted {
        return;
    }
    core.scripts[i].faulted = true;
    c.scripts_faulted = c.scripts_faulted.wrapping_add(1);
    if core.scripts[i].cold {
        core.scripts[i].cold = false;
        c.scripts_cold = c.scripts_cold.saturating_sub(1);
    }
}

/// §4.3.8: a sub fault latches, and every reader (instances and scripts) is latched with it.
fn sub_fault(core: &mut Core, c: &mut PineCounters, s: usize) {
    core.subs[s].faulted = true;
    let bit = 1u32 << s;
    let mut k = 0usize;
    while k < core.subs.len() {
        if core.subs[k].reads_subs & bit != 0 {
            core.subs[k].faulted = true;
        }
        k += 1;
    }
    let mut i = 0usize;
    while i < core.scripts.len() {
        if core.scripts[i].reads_subs & bit != 0 {
            script_fault(core, c, i);
        }
        i += 1;
    }
}
