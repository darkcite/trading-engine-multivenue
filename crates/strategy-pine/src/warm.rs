// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.3.6 / §4.3.7 (T9): warm-up. `validate` (in `supply_warm`) checks each slice, applies
//! the lag rule and sufficiency, and renumbers + masks the bars of every warm row (I15 warm
//! states, I19); `walk` (in `on_start`, §4.2 step 5) steps every warm bar of every row through
//! one merge-walk, and marks the cold scripts (§4.3.7, I16).
//!
//! COPY-DOCTRINE: boot only (`configure` → `supply_warm` → `on_start`). It may allocate and copy;
//! nothing here runs after `on_start` returns (I1).
//!
//! Literal readings:
//! - pin:08-warm-slice-unknown-or-duplicate: a slice naming no row is dropped (counted with
//!   nothing: no row is named); a second slice for one row makes that row cold (which is the
//!   truth is unknowable). Neither is a refusal (§4.3.6: "makes the row cold").
//! - pin:08-warm-boot-open-gate: the last bar's `close_ms ≤ barclock::open_ms(boot_utc_ms, tf)`;
//!   a slice with no bars is cold.
//! - pin:08-warm-no-reconcile-boot-on-paper: `reconcile_boot` (§4.2 step 5, D19) needs a venue
//!   boot state; slot 8 is paper only and `boot_state` is empty, so no warm-up calls it (8f).
//! - pin:08-warm-ties-subs-first: per `close_ms`, every sub-program instance on a tied row steps
//!   first (instance order — children before parents), then every script charting a tied row
//!   (manifest order); a row that is both a sub row and a chart row steps in both phases.

use crate::bars::NONE_MS;
use crate::boot::WarmSlice;
use crate::rt::{Core, Plan};
use crate::step;
use core_time::barclock;
use pine_ir::{barflag, Bar};
use strategy_core::PineCounters;

/// File-row flags a warm bar keeps (§4.3.6 renumbering).
const KEEP: u32 = barflag::CONFIRMED | barflag::GAP_BEFORE | barflag::VOL_SAT;

/// One slice's checks (§4.3.6 "Validation", the lag rule, sufficiency). `true` = usable.
fn valid(bars: &[Bar], tf: u32, boot_open: i64, max_lag: u32, need: u32) -> bool {
    let Some(last) = bars.last() else {
        return false;
    };
    let mut prev = NONE_MS;
    for b in bars {
        if b.open_ms <= prev
            || barclock::open_ms(b.open_ms, tf) != b.open_ms
            || b.close_ms != barclock::close_ms(b.open_ms, tf)
            || b.flags & barflag::CONFIRMED == 0
        {
            return false;
        }
        prev = b.open_ms;
    }
    if last.close_ms > boot_open {
        return false;
    }
    // Lag: `missing = bars_between(last.open, boot_open) − 1` ≤ `max_warm_lag_bars`.
    let missing = barclock::bars_between(last.open_ms, boot_open, tf) - 1;
    if missing > i64::from(max_lag) {
        return false;
    }
    bars.len() >= need as usize
}

/// `supply_warm`: per plan row, the renumbered, masked bars of a valid slice, or `None` (cold).
pub(crate) fn validate(
    plan: &Plan,
    slices: Vec<WarmSlice>,
    boot_utc_ms: i64,
    max_lag: u32,
) -> Vec<Option<Box<[Bar]>>> {
    let n = plan.rows.len();
    let mut seen = vec![0u8; n];
    let mut out: Vec<Option<Box<[Bar]>>> = (0..n).map(|_| None).collect();
    for sl in slices {
        let Some(r) = plan
            .rows
            .iter()
            .position(|p| p.symbol_hash == sl.symbol_hash && p.tf_s == sl.tf_s)
        else {
            continue;
        };
        seen[r] = seen[r].saturating_add(1);
        let row = &plan.rows[r];
        let boot_open = barclock::open_ms(boot_utc_ms, row.tf_s);
        if seen[r] > 1 || !valid(&sl.bars, row.tf_s, boot_open, max_lag, row.need) {
            out[r] = None;
            continue;
        }
        let mut bars = sl.bars;
        let last = bars.len() - 1;
        for (k, b) in bars.iter_mut().enumerate() {
            b.index = u32::try_from(k).unwrap_or(u32::MAX);
            b.flags &= KEEP;
            if k == last {
                b.flags |= barflag::LAST_CONFIRMED_HISTORY;
            }
        }
        out[r] = Some(bars.into_boxed_slice());
    }
    out
}

/// §4.2 step 5 + §4.3.7: the merge-walk over every warm row, then the rows' live start and the
/// cold scripts. Boot only.
pub(crate) fn walk(core: &mut Core, c: &mut PineCounters, warm: &[Option<Box<[Bar]>>]) {
    let n_rows = core.rows.len();
    debug_assert!(warm.len() == n_rows);
    let mut cur = vec![0usize; n_rows];
    loop {
        // The smallest pending close.
        let mut t = i64::MAX;
        for (r, w) in warm.iter().enumerate() {
            if let Some(bars) = w {
                if cur[r] < bars.len() {
                    t = t.min(bars[cur[r]].close_ms);
                }
            }
        }
        if t == i64::MAX {
            break;
        }
        let tied = |r: usize| -> Option<Bar> {
            let bars = warm[r].as_ref()?;
            let b = bars.get(cur[r])?;
            (b.close_ms == t).then_some(*b)
        };
        let mut adv = 0u32;
        for r in 0..n_rows {
            if tied(r).is_some() {
                adv |= 1u32 << r;
            }
        }
        core.cb_seq = core.cb_seq.wrapping_add(1);
        // Sub rows first, children before parents (pin:08-warm-ties-subs-first).
        for s in 0..core.subs.len() {
            if let Some(b) = tied(core.subs[s].row) {
                step::step_sub_bar(core, c, s, &b, b.close_ms);
            }
        }
        // Then chart rows, manifest order.
        for i in 0..core.scripts.len() {
            let Some(b) = tied(core.scripts[i].chart_row) else {
                continue;
            };
            // 1. each `lookahead` instance whose containing HTF bar is in its row's warm slice
            // and closes after this bar: `step_tick(complete HTF bar)`, a confirmed tail.
            for k in 0..usize::from(core.scripts[i].subs.n) {
                let s = usize::from(core.scripts[i].subs.keys[k].1);
                if !core.subs[s].sub.lookahead {
                    continue;
                }
                let hr = core.subs[s].row;
                let Some(h) = warm[hr].as_ref().and_then(|w| w.get(cur[hr]).copied()) else {
                    continue;
                };
                if h.open_ms <= b.open_ms && b.open_ms < h.close_ms && h.close_ms > b.close_ms {
                    step::tick_sub(core, c, s, &h, b.close_ms, true);
                }
            }
            // 2–3. `open_bar`, `step_bar` at `now_ms = close_ms`; the intents are discarded.
            step::eval_script(core, c, i, &b, b.close_ms, true);
        }
        for (r, at) in cur.iter_mut().enumerate() {
            if adv & (1u32 << r) != 0 {
                *at += 1;
            }
        }
    }
    // Live bars continue each warm row at `n`.
    let mut cold_rows = 0u32;
    for (r, w) in warm.iter().enumerate() {
        match w.as_ref().and_then(|b| b.last()) {
            Some(last) => {
                core.rows[r].resume_after(last);
                core.rows[r].warm = true;
            }
            None => cold_rows += 1,
        }
    }
    core.rows_cold = cold_rows;
    // §4.3.7: cold = the chart row or any row its instances read is not warm.
    let mut cold = 0u64;
    for i in 0..core.scripts.len() {
        let s = &core.scripts[i];
        let mut is_cold = !core.rows[s.chart_row].warm;
        for r in 0..n_rows {
            if s.reads_rows & (1u32 << r) != 0 && !core.rows[r].warm {
                is_cold = true;
            }
        }
        let sm = &mut core.scripts[i];
        sm.cold = is_cold && !sm.faulted;
        if sm.cold {
            cold += 1;
        }
    }
    c.scripts_cold = cold;
}
