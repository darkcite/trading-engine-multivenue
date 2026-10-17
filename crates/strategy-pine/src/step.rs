// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.3.8 / §4.3.9 / §4.4.2 (T10): the live bar path — Trade prints and quote ticks into
//! the rows, every confirm into the sub-program stepping table and the per-confirm step order
//! (I17), the `lookahead` containing bar (A2, CAR-04-1), `bars_seen` (I19), the cold gate (I16).
//! The warm-up merge-walk (`warm`) reuses `step_sub_bar`, `tick_sub` and `eval_script`.
//!
//! Hot module (I4): no allocation, no clock (time arrives as event data, P5), index loops only.
//! The one `dyn` is `&mut dyn BarCtx` (`ctx::PineCtx`). A committing evaluation hands its intents
//! to `host::commit` (`broker.apply_intents(vm.intents(), now_ns)`, §4.3.9 step 5); `reconcile` +
//! dispatch (step 6) run in `host::flush` at the end of the callback
//! (pin:08-reconcile-at-callback-end). Realtime ticks are `coalesce` (8c, T11).
//!
//! Literal readings:
//! - pin:08-fault-latch-until-8e (RETIRED by T15): a script fault, or a fault of any instance it
//!   reads, goes through `fault::disable` (the D13 incident, the sticky disable, the flatten); a
//!   sub fault also latches every instance that reads it (§4.3.8) and disables every parent (I35).
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

use crate::bars::{self, Confirmed, TradeOut};
use crate::clock::{advance, now_ms};
use crate::coalesce;
use crate::ctx::PineCtx;
use crate::rt::Core;
use crate::settle;
use core_time::barclock;
use core_types::{ChannelEvent, Tick};
use pine_ir::{Bar, ExecMode, StepOutcome};
use strategy_core::{pine_tape, PineCounters};

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
    let t = bars::t_ms(ev.venue_time_ms, ev.ts_ns, &core.anchor);
    let q = bars::qty_abs(ev.v1);
    let now = now_ms(core);
    let mut r = 0usize;
    while r < core.rows.len() {
        if core.rows[r].sym == ev.sym {
            match core.rows[r].on_trade(ev.v0, q, t) {
                TradeOut::Late => c.prints_late = c.prints_late.wrapping_add(1),
                TradeOut::ConfirmedAndOpened(cf) => {
                    confirm_row(core, c, r, &cf, now);
                    coalesce::mark_updated(core, r);
                }
                TradeOut::Updated | TradeOut::Opened => coalesce::mark_updated(core, r),
            }
        }
        r += 1;
    }
    coalesce::run_pending_ticks(core, c, ev.sym);
}

/// §4.4.2 `on_tick`: E, the symbol's `Quote`, new gone-evidence, the rows confirmed by time only
/// (I12), the pending ticks, and a reconcile owed to every script on the symbol (synthetic stops
/// and held limits live on the touch, §4.5.1).
pub(crate) fn on_quote(core: &mut Core, c: &mut PineCounters, t: &Tick) {
    advance(core, c, t.ts_ns);
    let q = crate::clock::quote_of(core, t);
    let mut k = 0usize;
    while k < core.syms.len() {
        if core.syms[k] == t.sym {
            core.quotes[k] = q;
        }
        k += 1;
    }
    settle::observe_tick(core, t);
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
    coalesce::run_pending_ticks(core, c, t.sym);
    let mut i = 0usize;
    while i < core.scripts.len() {
        if core.scripts[i].sym == t.sym {
            core.dirty |= 1u32 << i;
        }
        i += 1;
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
            // R7: the bar's mark after its evaluation (pin:08-bar-mark-every-chart-confirm).
            crate::marks::record(core, i, &cf.bar);
        }
        i += 1;
    }
    coalesce::on_confirm(core, r);
}

/// §4.3.9 for one script on a live confirmed chart bar.
fn live_script(core: &mut Core, c: &mut PineCounters, i: usize, bar: &Bar, now: i64) {
    if core.scripts[i].disabled {
        // D13's slow phase: one flatten attempt per confirmed chart bar (§4.6 step 5).
        crate::fault::on_chart_confirm(core, i);
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
        // Part of the confirm: spends first, never refused (I21). Inside a tick evaluation the
        // instance is part of the script's `charge_tick` instead.
        coalesce::spend(core, core.subs[s].charge);
        tick_sub(core, c, s, &dev, now, false);
    }
    if core.scripts[i].disabled {
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
    coalesce::spend(core, core.subs[s].charge);
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
    if core.scripts[i].disabled {
        return;
    }
    // I16: a cold script's gate, before the evaluation (pin:08-cold-check-at-eval); then W21's
    // other conditions (enabled, not faulted, route not `Off`, §4.4.5).
    let committing = !history
        && (!core.scripts[i].cold || turns_warm(core, c, i, bar))
        && crate::host::commits(core, i);
    coalesce::spend(core, core.scripts[i].charge_bar);
    let Core {
        scripts, series, ..
    } = core;
    let sc = &mut scripts[i];
    sc.broker.open_bar(bar);
    sc.bar_ix = bar.index;
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
    if let StepOutcome::Fault(k) = out {
        crate::tape::eval(
            core,
            c,
            i,
            pine_tape::EVAL_BAR,
            bar,
            k as u8,
            u8::from(history),
        );
        crate::fault::vm_fault(core, c, i);
        return;
    }
    // §4.3.9 step 4, I19 (impl-03 F17): fail fast.
    assert!(
        sc.vm.bars_seen() == u64::from(bar.index) + 1,
        "pine: bars_seen != bar.index + 1 (I19)"
    );
    let n = sc.vm.intents().len() as u64;
    let flags = u8::from(history) | (u8::from(committing) << 3);
    crate::tape::eval(core, c, i, pine_tape::EVAL_BAR, bar, 0, flags);
    if committing {
        crate::host::commit(core, c, i);
    } else if !core.boot_eval {
        c.intents_discarded = c.intents_discarded.wrapping_add(n);
    }
}

/// §4.4.3 step 2 for script `i`: `broker.open_bar(&forming)`, then `step_tick(&forming, ctx)` —
/// the one `dyn`. Only a `Realtime` script is ever ticked (I23; `coalesce` guards it).
pub(crate) fn step_script_tick(core: &mut Core, i: usize, forming: &Bar, now: i64) -> StepOutcome {
    let Core {
        scripts, series, ..
    } = core;
    let sc = &mut scripts[i];
    debug_assert!(sc.program.mode == ExecMode::Realtime, "I23");
    sc.broker.open_bar(forming);
    sc.bar_ix = forming.index;
    let mut ctx = PineCtx {
        series,
        subs: sc.subs.entries(),
        broker: Some(&sc.broker),
        now_ms: now,
    };
    sc.vm.step_tick(forming, &mut ctx)
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

/// §4.3.8: a sub fault latches, and every reader (instances and scripts) is latched with it; every
/// parent script is disabled through D13's one path, one `SUB_FAULT` incident each (I35).
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
            crate::fault::sub_fault_parent(core, c, i, s);
        }
        i += 1;
    }
}
