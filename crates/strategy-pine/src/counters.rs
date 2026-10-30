// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §3.4 / T17: the observables' assembly — `PineCounters` live in the member (`lib.rs`,
//! counted where they happen); this module assembles the per-script `PineScriptView` rows the
//! cli's 5 s block mirrors into `/metrics` and `/state`.
//!
//! Cold (5 s cadence) but on the engine thread: no allocation, index loops, POD stores (I4).
//!
//! Literal readings:
//! - pin:08-view-fault-kind: `fault_kind` is the VM's `FaultKind` for a script fault, `0xFE` for
//!   a broker refusal, `0xFD` for a sub-program fault (strategy-core's field doc), 0 when none.
//! - pin:08-view-submits-are-ctx-calls: `submits` / `submit_errors` count this script's
//!   `ctx.submit` + `ctx.modify` results (Ok / Err); `fills` the fills booked by its broker.
//! - pin:08-view-equity-floor: `equity_1e6 = floor(equity_1e12 / 1e6)`, saturated to `i64`.

use crate::rt::Core;
use strategy_core::PineScriptView;

/// One `PineScriptView` per loaded script into `out` (as many as fit); returns the script count.
pub(crate) fn scripts_view(core: &Core, out: &mut [PineScriptView]) -> u32 {
    let n = core.scripts.len().min(out.len());
    let mut i = 0usize;
    while i < n {
        let s = &core.scripts[i];
        let bc = s.broker.counters();
        let v = s.broker.view();
        let vc = s.vm.counters();
        let mut sha = [0u8; 8];
        // COPY: the SHA-256's first 8 B into a word (`sha_lo`) — cold, 5 s — `from_be_bytes`
        // takes an array by value; a pointer cast rejected (alignment).
        sha.copy_from_slice(&s.sha256[..8]);
        let eq = v.equity_1e12.div_euclid(1_000_000);
        let equity_1e6 = i64::try_from(eq).unwrap_or(if eq < 0 { i64::MIN } else { i64::MAX });
        // COPY: one 128 B view row per script (§4.12 #9) — cold, every 5 s — the cli must not
        // name the member.
        out[i] = PineScriptView {
            sha_lo: u64::from_be_bytes(sha),
            bar_evals: vc.bar_evals,
            tick_evals: vc.tick_evals,
            submits: s.n_submits,
            submit_errors: s.n_submit_errors,
            fills: s.n_fills,
            skipped_cap: bc.skipped_cap,
            skipped_max_open: bc.skipped_max_open,
            ioc_remainder_cancelled: bc.ioc_remainder_cancelled,
            position_1e6: v.position_size_1e6,
            avg_px_1e6: v.position_avg_price_1e6,
            equity_1e6,
            timeframe_s: s.timeframe_s,
            coalesce_ms: s.coalesce_ms,
            max_insns: vc.max_insns,
            fault_pc: if s.disabled { s.fault.1 } else { 0 },
            fault_bar: if s.disabled { s.fault.2 } else { 0 },
            sym: s.sym,
            script: u8::try_from(i).unwrap_or(u8::MAX),
            enabled: u8::from(s.enabled),
            disabled: u8::from(s.disabled),
            realtime: u8::from(s.program.mode == pine_ir::ExecMode::Realtime),
            cold: u8::from(s.cold),
            fault_kind: if s.disabled { s.fault.0 } else { 0 },
            flatten_stuck: u8::from(s.flat_stuck),
            entries_suppressed: u8::from(s.suppressed),
        };
        i += 1;
    }
    u32::try_from(core.scripts.len()).unwrap_or(u32::MAX)
}
