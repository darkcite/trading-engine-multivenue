// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.4.3–§4.4.5 (T11): realtime — coalescing in event time, the per-event instruction
//! budget across scripts, round-robin, and `calc_on_order_fills`.
//!
//! - **A pending update.** A print that updates or opens a chart row's forming bar sets the
//!   row's realtime scripts' `tick_pending` (`mark_updated`); a confirm clears it and re-arms
//!   `first_update` (`on_confirm`).
//! - **Admission** (`run_pending_ticks(sym)`, round-robin from `rr[sym]`, which advances every
//!   callback): `first_update`, or `E − last_tick_e ≥ coalesce_ms × 10⁶`, or (`coalesce_bps > 0`
//!   and `|c − last_eval_close| × 10 000 ≥ coalesce_bps × last_eval_close`, `i128`); and the
//!   callback's budget covers the script's static charge. A refused update stays pending
//!   (`ticks_coalesced` / `ticks_budgeted`) and is re-checked at the next print or quote tick of
//!   the symbol — nothing waits on a timer (I20).
//! - **The budget** (I21): each callback starts at `event_insn_budget`; confirms spend first and
//!   are never refused; tick and `calc_on_order_fills` evaluations are admitted while
//!   `budget ≥ charge`.
//!
//! Hot module (I4): no allocation, no clock — every admission reads E and bar values only.
//!
//! Literal readings:
//! - pin:08-rr-advance-one: `rr[s]` advances by one script per callback of `s` (mod the script
//!   count), so a script deferred by the budget is reached earlier next time.
//! - pin:08-cof-own-state: a `calc_on_order_fills` evaluation is charged like a tick and counted in
//!   `cof_evals`; it does not touch the coalescing state (`last_tick_e`, `last_eval_close`).
//! - pin:08-confirm-budget-floor: confirms spend with a saturating subtraction; a callback whose
//!   confirms cross symbols (a chart close time-confirming another symbol's HTF row) can spend past
//!   the per-symbol load check — the budget floors at 0 and no tick is then admitted. Reported as
//!   a spec gap (the `6011` check is per row symbol).

use crate::clock;
use crate::rt::Core;
use crate::step;
use pine_ir::{ExecMode, StepOutcome};
use strategy_core::PineCounters;

/// `begin_callback`: the per-event budget resets and `cb_seq` advances (§4.4.2).
#[inline]
pub(crate) fn begin_callback(core: &mut Core) {
    core.cb_seq = core.cb_seq.wrapping_add(1);
    core.budget = core.event_budget;
    core.charged = 0;
    core.settle_now = false;
}

/// Confirms spend first, never refused (I21; pin:08-confirm-budget-floor).
#[inline]
pub(crate) fn spend(core: &mut Core, units: u64) {
    core.budget = core.budget.saturating_sub(units);
    charge(core, units);
}

/// The unsaturated witness of what this callback charged.
#[inline]
fn charge(core: &mut Core, units: u64) {
    core.charged = core.charged.saturating_add(units);
    core.charged_max = core.charged_max.max(core.charged);
}

/// A print updated or opened row `r`'s forming bar: its realtime scripts owe an update.
pub(crate) fn mark_updated(core: &mut Core, r: usize) {
    let mut i = 0usize;
    while i < core.scripts.len() {
        let s = &mut core.scripts[i];
        if s.realtime && s.chart_row == r {
            s.tick_pending = true;
        }
        i += 1;
    }
}

/// A chart bar of row `r` confirmed: the bar evaluation supersedes the forming state.
pub(crate) fn on_confirm(core: &mut Core, r: usize) {
    let mut i = 0usize;
    while i < core.scripts.len() {
        let s = &mut core.scripts[i];
        if s.realtime && s.chart_row == r {
            s.tick_pending = false;
            s.first_update = true;
        }
        i += 1;
    }
}

/// §4.4.3 admission's coalescing half for script `i` at forming close `close`.
#[inline]
fn coalesce_admits(core: &Core, i: usize, close: i64) -> bool {
    let s = &core.scripts[i];
    if s.first_update {
        return true;
    }
    let window = u64::from(s.coalesce_ms).saturating_mul(1_000_000);
    if core.e_ns.saturating_sub(s.last_tick_e) >= window {
        return true;
    }
    if s.coalesce_bps > 0 {
        let d = (i128::from(close) - i128::from(s.last_eval_close)).abs();
        return d * 10_000 >= i128::from(s.coalesce_bps) * i128::from(s.last_eval_close);
    }
    false
}

/// `run_pending_ticks(sym)`: the realtime scripts on `sym`, round-robin from `rr[sym]`; owed
/// `calc_on_order_fills` evaluations first within each script's turn.
pub(crate) fn run_pending_ticks(core: &mut Core, c: &mut PineCounters, sym: core_types::SymbolId) {
    let n = core.scripts.len();
    if n == 0 {
        return;
    }
    let mut k = 0usize;
    while k < core.syms.len() && core.syms[k] != sym {
        k += 1;
    }
    if k == core.syms.len() {
        return;
    }
    let start = usize::from(core.rr[k]) % n;
    let mut j = 0usize;
    while j < n {
        let i = (start + j) % n;
        j += 1;
        if core.scripts[i].sym != sym || !core.scripts[i].realtime || core.scripts[i].disabled {
            continue;
        }
        if core.scripts[i].cof_pending {
            run_cof(core, c, i);
        }
        if !core.scripts[i].tick_pending || core.scripts[i].disabled {
            continue;
        }
        let Some(forming) = core.rows[core.scripts[i].chart_row].forming_bar() else {
            continue;
        };
        if !coalesce_admits(core, i, forming.close_1e6) {
            c.ticks_coalesced = c.ticks_coalesced.wrapping_add(1);
            continue;
        }
        let charge = core.scripts[i].charge_tick;
        if core.budget < charge {
            c.ticks_budgeted = c.ticks_budgeted.wrapping_add(1);
            continue;
        }
        core.budget -= charge;
        self::charge(core, charge);
        if tick_eval(core, c, i, &forming) {
            let s = &mut core.scripts[i];
            s.tick_pending = false;
            s.first_update = false;
            s.last_tick_e = core.e_ns;
            s.last_eval_close = forming.close_1e6;
            c.tick_evals = c.tick_evals.wrapping_add(1);
        }
    }
    let next = (usize::from(core.rr[k]) + 1) % n;
    core.rr[k] = u8::try_from(next).unwrap_or(0);
}

/// §4.4.5: one owed `calc_on_order_fills` evaluation of script `i`, if a forming bar exists and
/// the budget covers it; otherwise it stays owed (the print that opens a bar re-checks it).
pub(crate) fn run_cof(core: &mut Core, c: &mut PineCounters, i: usize) {
    if !core.scripts[i].realtime || core.scripts[i].disabled {
        core.scripts[i].cof_pending = false;
        return;
    }
    let Some(forming) = core.rows[core.scripts[i].chart_row].forming_bar() else {
        return;
    };
    let charge = core.scripts[i].charge_tick;
    if core.budget < charge {
        c.ticks_budgeted = c.ticks_budgeted.wrapping_add(1);
        return;
    }
    core.budget -= charge;
    self::charge(core, charge);
    core.scripts[i].cof_pending = false;
    if tick_eval(core, c, i, &forming) {
        c.cof_evals = c.cof_evals.wrapping_add(1);
    }
}

/// Every owed `calc_on_order_fills` evaluation (the end of a fill callback).
pub(crate) fn run_all_cof(core: &mut Core, c: &mut PineCounters) {
    let mut i = 0usize;
    while i < core.scripts.len() {
        if core.scripts[i].cof_pending {
            run_cof(core, c, i);
        }
        i += 1;
    }
}

/// Evaluating an admitted update (§4.4.3): the script's `Realtime` sub-programs `step_tick`ed
/// once per callback (children first), `open_bar(&forming)`, `step_tick(&forming)`, then commit
/// (or discard). `false` when the script faulted.
fn tick_eval(core: &mut Core, c: &mut PineCounters, i: usize, forming: &pine_ir::Bar) -> bool {
    debug_assert!(core.scripts[i].program.mode == ExecMode::Realtime, "I23");
    let now = clock::now_ms(core);
    let reads = core.scripts[i].reads_subs;
    let mut s = 0usize;
    while s < core.subs.len() {
        if reads & (1u32 << s) != 0 && core.subs[s].sub.program.mode == ExecMode::Realtime {
            if let Some(dev) = core.rows[core.subs[s].row].forming_bar() {
                step::tick_sub(core, c, s, &dev, now, false);
            }
        }
        s += 1;
    }
    if core.scripts[i].disabled {
        return false;
    }
    let out = step::step_script_tick(core, i, forming, now);
    let tick = strategy_core::pine_tape::EVAL_TICK;
    if let StepOutcome::Fault(k) = out {
        crate::tape::eval(core, c, i, tick, forming, k as u8, 0);
        crate::fault::vm_fault(core, c, i);
        return false;
    }
    let committing = crate::host::commits(core, i);
    crate::tape::eval(core, c, i, tick, forming, 0, u8::from(committing) << 3);
    if committing {
        crate::host::commit(core, c, i);
    } else {
        let n = core.scripts[i].vm.intents().len() as u64;
        c.intents_discarded = c.intents_discarded.wrapping_add(n);
    }
    true
}
