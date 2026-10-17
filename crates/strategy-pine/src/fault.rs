// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.6 (8e, T15): D13 — disable, flatten, the retries, `flatten_stuck`, the slow phase,
//! and the incident ring (I32–I35).
//!
//! A fault that comes out of a script (its VM, a sub-program's VM, or the broker's
//! `ScriptRefusal`) costs that script and never the process: [`disable`] is the one path. A fault
//! in this crate's own Rust, and `BrokerFault::Internal`, panic (fail fast, I34).
//!
//! Hot module (I4): fixed tables, index loops, POD stores; no allocation, no clock (an
//! incident's `utc_ms` is E through the anchor), no `dyn`.
//!
//! Literal readings:
//! - pin:08-disable-once: a second fault of a disabled script (a sub fault of another instance it
//!   reads, a refusal of its flatten) pushes no second incident: §4.6's "one incident" per
//!   disable, I32.
//! - pin:08-flatten-needs-dispatch: a script whose route is `Off`, or whose `enabled = 0`, is
//!   disabled but never flattened (§4.6 step 6, §4.7.1 `may_dispatch`): nothing is dispatched,
//!   `flattens_started` is not counted, the incident is still pushed.
//! - pin:08-flatten-due: "once per callback of the script's symbol" = a quote tick or a Trade
//!   print of its chart symbol, a fill it took, or a disposition reported for it; the call runs in
//!   `host::flush` at the end of that callback (it needs the `ctx`).
//! - pin:08-flatten-done: the chase ends when `view().position_size_1e6 == 0` and the script
//!   awaits no disposition (no maker rests, no IoC in flight); `flatten_stuck` then drops.
//! - pin:08-flatten-stuck-level: `PineCounters::flatten_stuck` is the LEVEL (scripts stuck now:
//!   +1 at the eighth failure, −1 when flat), the ALERT; `flatten_attempts` counts every call
//!   that wrote a Submit.
//! - pin:08-slow-phase-throttles-submits-only (replaces pin:08-slow-phase-gates-the-call, whose
//!   premise was false: the broker re-sends a cancel only after `quote_stale_ns`): in the slow
//!   phase `flatten` is still called on every due callback while a maker of the script rests, and
//!   its CANCELS are dispatched; only the reduce-only Submit waits for the next confirmed chart
//!   bar — one written before then is withheld (reported `on_cancelled`, never dispatched, not an
//!   attempt). With no maker resting, the call itself waits for the bar.
//! - pin:08-flatten-refusal-retries: a `BrokerFault::Script` out of `flatten` (a full sent table)
//!   changes nothing — the script is already disabled — and the next due callback retries.
//! - pin:08-incident-route-implicit: `PineIncident` has no route field; slot 8 is paper or off,
//!   the boot record names which, and an `Off` disable shows `flattens_started` unchanged.
//! - pin:08-incident-bar-is-last-open: an incident's `bar_index` is the last bar the script's
//!   broker opened (`open_bar`), the bar of the evaluation that faulted.

use crate::clock;
use crate::rt::Core;
use crate::settle::kind;
use crate::{FLATTEN_RETRIES, INCIDENT_CAP, MAX_SETTLES};
use pine_broker::{BrokerFault, ScriptRefusal};
use pine_ir::{EngineIntent, EngineVerb, Primitive};
use strategy_core::{pine_incident, Ctx, PineCounters, PineIncident};

/// `PineIncident::source`.
pub(crate) mod source {
    /// The script's own VM.
    pub(crate) const SCRIPT: u8 = 0;
    /// A sub-program instance's VM.
    pub(crate) const SUB: u8 = 1;
    /// The broker (`ScriptRefusal`).
    pub(crate) const BROKER: u8 = 2;
    /// This host (flatten stuck, the refusal trip, the guards).
    pub(crate) const HOST: u8 = 3;
}

/// What disabled a script (one incident).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cause {
    pub(crate) event: u8,
    pub(crate) source: u8,
    pub(crate) fault_kind: u8,
    pub(crate) pc: u32,
    pub(crate) detail: u32,
}

/// One incident into the ring (a POD store, §4.6 step 2). The oldest is overwritten.
pub(crate) fn push(core: &mut Core, c: &mut PineCounters, i: usize, cause: Cause) {
    let utc_ms = if core.warming {
        core.warm_ms
    } else {
        clock::now_ms(core)
    };
    let s = &core.scripts[i];
    let rec = PineIncident {
        seq: core.inc_seq,
        utc_ms,
        sha256: s.sha256,
        position_1e6: s.broker.view().position_size_1e6,
        bar_index: s.bar_ix,
        pc: cause.pc,
        coalesce_ms: s.coalesce_ms,
        detail: cause.detail,
        script: u8::try_from(i).unwrap_or(u8::MAX),
        event: cause.event,
        fault_kind: cause.fault_kind,
        source: cause.source,
        ..PineIncident::default()
    };
    // `seq % INCIDENT_CAP` < INCIDENT_CAP (a power-of-two-free modulo; the ring is boxed at
    // `on_start` with exactly `INCIDENT_CAP` slots).
    let at = usize::try_from(core.inc_seq % INCIDENT_CAP as u64).unwrap_or(0);
    // COPY: one `PineIncident` (128 B) into its ring slot (§4.12 #8) — the cold drain copies out
    // later, off the decision path — an in-place build rejected: the slot may be read meanwhile.
    core.incidents[at] = rec;
    core.inc_seq = core.inc_seq.wrapping_add(1);
    c.incidents = c.incidents.wrapping_add(1);
    // §4.9.2: one `FAULT` decision per incident (event, kind, pc, bar, position, detail).
    let mut r = crate::tape::rec(
        strategy_core::pine_tape::FAULT,
        i,
        rec.event,
        rec.fault_kind,
        rec.pc,
    );
    r.bar_index = rec.bar_index;
    r.x0 = rec.position_1e6;
    r.x2 = u64::from(rec.detail);
    crate::tape::dec(core, c, r);
}

/// The incidents with `seq >= from_seq` still in the ring, oldest first, into `out`; the count.
pub(crate) fn view(core: &Core, out: &mut [PineIncident], from_seq: u64) -> u32 {
    let oldest = core.inc_seq.saturating_sub(INCIDENT_CAP as u64);
    let mut seq = from_seq.max(oldest);
    let mut n = 0usize;
    while seq < core.inc_seq && n < out.len() {
        let at = usize::try_from(seq % INCIDENT_CAP as u64).unwrap_or(0);
        // COPY: one `PineIncident` (128 B) out per row (§4.12 #8) — the cli's 5 s drain owns its
        // buffer — a borrow of the ring rejected: the engine thread keeps writing it.
        out[n] = core.incidents[at];
        n += 1;
        seq += 1;
    }
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// §4.6, the one path: disable script `i` (sticky), push its incident, start its flatten.
pub(crate) fn disable(core: &mut Core, c: &mut PineCounters, i: usize, cause: Cause) {
    disable_with(core, c, i, cause, true);
}

/// PB6 (OD-8): a venue-originated fill (liquidation, ADL, orphan, settlement — `order_id` 0,
/// origin VENUE, not adopted) on `sym`: every script whose chart symbol is `sym` is disabled
/// WITHOUT a flatten (its broker no longer holds the venue's position, so a broker flatten would
/// trade on a wrong size) and gets one `VENUE_FILL` incident; a symbol no script trades is only
/// counted (by the caller). Its resting makers are CANCELLED (risk review C3: they could still
/// fill into the broker) — never a submit, never a flatten: the cancels are owed to this
/// callback's `host::flush` (`run_due` → [`pb6_cancel`]). Returns how many scripts it disabled
/// now.
/// pin:pb6-venue-fill-chart-symbol: "the script that trades `f.sym`" is the script whose CHART
/// symbol is `f.sym` (the symbol its orders go to, pin:08-traded-is-every-chart-symbol).
/// pin:pb6-cancel-needs-dispatch: as the flatten (pin:08-flatten-needs-dispatch), the cancels
/// are owed only when the script may dispatch (`enabled`, route not `Off`): such a script never
/// sent a maker, and `Off` never calls the ctx (I36).
pub(crate) fn venue_fill(core: &mut Core, c: &mut PineCounters, sym: core_types::SymbolId) -> u32 {
    let mut n = 0u32;
    let mut i = 0usize;
    while i < core.scripts.len() {
        if core.scripts[i].sym == sym && !core.scripts[i].disabled {
            let may_dispatch = core.scripts[i].enabled && core.route != crate::SlotRoute::Off;
            if may_dispatch && maker_uncancelled(core, i) {
                core.scripts[i].pb6_cancel = true;
                core.flat_due |= 1u32 << i;
            }
            disable_with(
                core,
                c,
                i,
                Cause {
                    event: pine_incident::VENUE_FILL,
                    source: source::HOST,
                    fault_kind: 0,
                    pc: u32::MAX,
                    detail: 0,
                },
                false,
            );
            n += 1;
        }
        i += 1;
    }
    n
}

/// [`disable`], with the flatten (step 4) only when `flatten` (PB6 disables without one).
fn disable_with(core: &mut Core, c: &mut PineCounters, i: usize, cause: Cause, flatten: bool) {
    if core.scripts[i].disabled {
        return; // pin:08-disable-once
    }
    // 1. Sticky for the process: nothing assigns `false` (I33).
    core.scripts[i].disabled = true;
    c.scripts_faulted = c.scripts_faulted.wrapping_add(1);
    if core.scripts[i].cold {
        core.scripts[i].cold = false;
        c.scripts_cold = c.scripts_cold.saturating_sub(1);
    }
    // 2. The fault and its incident.
    let bar = core.scripts[i].bar_ix;
    // pin:08-view-fault-kind: the view's encoding (0xFE broker refusal, 0xFD sub fault).
    let kind = match cause.source {
        source::BROKER => 0xFE,
        source::SUB => 0xFD,
        _ => cause.fault_kind,
    };
    core.scripts[i].fault = (kind, cause.pc, bar);
    // R4 `fault()`: the VM's own `FaultKind` byte (0 for a broker refusal).
    core.scripts[i].fault_vm = cause.fault_kind;
    push(core, c, i, cause);
    // 3. Never evaluated or committed again: no pending tick, no owed evaluation, no reconcile.
    let s = &mut core.scripts[i];
    s.tick_pending = false;
    s.cof_pending = false;
    core.dirty &= !(1u32 << i);
    // 4. Flatten (pin:08-flatten-needs-dispatch): `may_dispatch` = enabled && route != Off; never
    // for a venue-originated fill (PB6, OD-8).
    if flatten && core.scripts[i].enabled && core.route != crate::SlotRoute::Off {
        core.scripts[i].flat_active = true;
        c.flattens_started = c.flattens_started.wrapping_add(1);
        core.flat_due |= 1u32 << i;
    }
}

/// A VM fault of script `i` (`FAULT`, or `WARM_FAULT` during the warm-up walk).
pub(crate) fn vm_fault(core: &mut Core, c: &mut PineCounters, i: usize) {
    // I4: a `match`, not a closure.
    let (k, pc) = match core.scripts[i].vm.last_fault() {
        Some((k, pc)) => (k as u8, pc),
        None => (0, u32::MAX),
    };
    let event = if core.warming {
        pine_incident::WARM_FAULT
    } else {
        pine_incident::FAULT
    };
    disable(
        core,
        c,
        i,
        Cause {
            event,
            source: source::SCRIPT,
            fault_kind: k,
            pc,
            detail: 0,
        },
    );
}

/// `BrokerFault::Script(r)` of script `i` (`BROKER_REFUSAL`, `detail = r as u32`).
pub(crate) fn broker_refusal(core: &mut Core, c: &mut PineCounters, i: usize, r: ScriptRefusal) {
    disable(
        core,
        c,
        i,
        Cause {
            event: pine_incident::BROKER_REFUSAL,
            source: source::BROKER,
            fault_kind: 0,
            pc: u32::MAX,
            detail: r as u32,
        },
    );
}

/// A fault of instance `s` disables parent `i` (`SUB_FAULT`, `detail` = the instance, I35).
pub(crate) fn sub_fault_parent(core: &mut Core, c: &mut PineCounters, i: usize, s: usize) {
    // I4: a `match`, not a closure.
    let (k, pc) = match core.subs[s].vm.last_fault() {
        Some((k, pc)) => (k as u8, pc),
        None => (0, u32::MAX),
    };
    disable(
        core,
        c,
        i,
        Cause {
            event: pine_incident::SUB_FAULT,
            source: source::SUB,
            fault_kind: k,
            pc,
            detail: u32::try_from(s).unwrap_or(u32::MAX),
        },
    );
}

/// pin:08-flatten-due: a quote tick or Trade print of `sym` owes every flattening script on it a
/// call.
#[inline]
pub(crate) fn due_sym(core: &mut Core, sym: core_types::SymbolId) {
    let mut i = 0usize;
    while i < core.scripts.len() {
        if core.scripts[i].flat_active && core.scripts[i].sym == sym {
            core.flat_due |= 1u32 << i;
        }
        i += 1;
    }
}

/// pin:08-flatten-due: a fill of script `i` owes it a call (when it is flattening).
#[inline]
pub(crate) fn due_script(core: &mut Core, i: usize) {
    if core.scripts[i].flat_active {
        core.flat_due |= 1u32 << i;
    }
}

/// `on_cancelled(oid)` was reported to script `i`'s broker: a call is owed, and when `oid` is the
/// attempt in flight with the position still open, the attempt FAILED (a miss, a router refusal,
/// `RingFull`). The eighth failure raises `flatten_stuck` (an ALERT) and the slow phase.
pub(crate) fn on_reported(core: &mut Core, c: &mut PineCounters, i: usize, oid: u32) {
    if !core.scripts[i].flat_active {
        return;
    }
    core.flat_due |= 1u32 << i;
    let s = &mut core.scripts[i];
    if s.flat_oid == 0 || s.flat_oid != oid {
        return;
    }
    s.flat_oid = 0;
    if s.broker.view().position_size_1e6 == 0 {
        return;
    }
    s.flat_fails = s.flat_fails.saturating_add(1);
    if s.flat_fails >= FLATTEN_RETRIES && !s.flat_stuck {
        s.flat_stuck = true;
        s.flat_slow_ok = false;
        c.flatten_stuck = c.flatten_stuck.wrapping_add(1);
        let detail = u32::from(s.flat_fails);
        push(
            core,
            c,
            i,
            Cause {
                event: pine_incident::FLATTEN_STUCK,
                source: source::HOST,
                fault_kind: 0,
                pc: u32::MAX,
                detail,
            },
        );
    }
}

/// A live confirmed bar of disabled script `i`'s chart row: the slow phase owes one attempt; a
/// PB6 cancel that failed is re-sent (pin:pb6-cancel-retry-per-bar).
#[inline]
pub(crate) fn on_chart_confirm(core: &mut Core, i: usize) {
    let s = &mut core.scripts[i];
    if s.flat_active && s.flat_stuck {
        s.flat_slow_ok = true;
        core.flat_due |= 1u32 << i;
    }
    if s.pb6_cancel {
        core.flat_due |= 1u32 << i;
    }
}

/// pin:08-flatten-done.
fn finish_if_flat(core: &mut Core, c: &mut PineCounters, i: usize) -> bool {
    let s = &core.scripts[i];
    if s.broker.view().position_size_1e6 != 0 {
        return false;
    }
    let mut k = 0usize;
    while k < MAX_SETTLES {
        if s.settles[k].kind != kind::FREE {
            return false;
        }
        k += 1;
    }
    let s = &mut core.scripts[i];
    s.flat_active = false;
    s.flat_oid = 0;
    if s.flat_stuck {
        s.flat_stuck = false;
        c.flatten_stuck = c.flatten_stuck.saturating_sub(1);
    }
    true
}

/// End of a callback (`host::flush`, route not `Off`): one `flatten` call per due script, then
/// its instructions dispatched in buffer order (cancels never withheld); or, for a script PB6
/// disabled, the cancels of its resting makers ([`pb6_cancel`]).
pub(crate) fn run_due<C: Ctx>(core: &mut Core, c: &mut PineCounters, ctx: &mut C) {
    let mut i = 0usize;
    while i < core.scripts.len() {
        let bit = 1u32 << i;
        if core.flat_due & bit != 0 {
            core.flat_due &= !bit;
            // PB6 disables without a flatten and a disable is sticky: never both.
            debug_assert!(!(core.scripts[i].flat_active && core.scripts[i].pb6_cancel));
            if core.scripts[i].flat_active {
                call(core, c, i, ctx);
            } else if core.scripts[i].pb6_cancel {
                pb6_cancel(core, c, i, ctx);
            }
        }
        i += 1;
    }
    core.flat_due = 0;
}

/// pin:pb6-cancel-side-unknown: the `DISPATCH` row's side byte of a PB6 cancel — the settle
/// record keeps no side (nor the limit price or the Pine id), so the row says "unknown" (neither
/// 0 = bid nor 1 = ask) instead of inventing one.
pub(crate) const PB6_SIDE_UNKNOWN: u8 = 0xFF;

/// PB6 (risk review C3): one `Cancel` per acked resting maker of disabled script `i` — never a
/// submit, never a flatten, never gated (I26/I27) — through `host::cancel_one` (the
/// `dispatch_one` Cancel arm: the `CancelReq`, its tape row, its disposition). The script stays
/// owed (`pb6_cancel`) while a maker rests uncancelled (a `RingFull` / `Refused` /
/// `Unsupported` answer); the next confirmed chart bar re-sends.
/// pin:pb6-cancel-acked-makers: "every working order" = every `kind::MAKER` record not yet gone
/// (`gone == 0`): an acked maker resting. A record already gone (a cancel answered `Ok` /
/// `NoSuchOrder`, a refused submit, a modify's replaced `prev`, all live-window waits) was
/// answered by the venue and waits its window as before; an IoC never rests.
/// pin:pb6-cancel-retry-per-bar: a failed cancel is re-sent at each later confirmed chart bar of
/// the script (`on_chart_confirm`, the slow phase's cadence), not on every tick: the broker,
/// which would re-emit after `quote_stale_ns`, never reconciles a disabled script.
/// pin:pb6-cancel-tape-row: the row records what the record knows — verb `Cancel`, primitive
/// `MakerPostOnly` (only a maker rests), side [`PB6_SIDE_UNKNOWN`], flags 0, the result, the
/// broker oid, px 0 (unknown), qty = the maker's ordered size, id 0 (none), prev 0.
fn pb6_cancel<C: Ctx>(core: &mut Core, c: &mut PineCounters, i: usize, ctx: &mut C) {
    let mut k = 0usize;
    while k < MAX_SETTLES {
        let (rk, gone, oid, qty) = {
            let r = &core.scripts[i].settles[k];
            (r.kind, r.gone, r.oid, r.qty)
        };
        if rk == kind::MAKER && gone == 0 {
            let e = EngineIntent {
                verb: EngineVerb::Cancel,
                primitive: Primitive::MakerPostOnly,
                side: PB6_SIDE_UNKNOWN,
                client_oid: oid,
                qty_1e6: qty,
                ..EngineIntent::default()
            };
            let _ = crate::host::cancel_one(core, c, i, &e, ctx);
        }
        k += 1;
    }
    core.scripts[i].pb6_cancel = maker_uncancelled(core, i);
}

/// Whether script `i` has an acked maker resting with no cancel answered (pin:pb6-cancel-acked-
/// makers).
fn maker_uncancelled(core: &Core, i: usize) -> bool {
    let t = &core.scripts[i].settles;
    let mut k = 0usize;
    while k < MAX_SETTLES {
        if t[k].kind == kind::MAKER && t[k].gone == 0 {
            return true;
        }
        k += 1;
    }
    false
}

/// §4.6 steps 4–5: one idempotent `flatten` call of script `i`.
fn call<C: Ctx>(core: &mut Core, c: &mut PineCounters, i: usize, ctx: &mut C) {
    if finish_if_flat(core, c, i) {
        return;
    }
    // pin:08-slow-phase-throttles-submits-only.
    let throttled = core.scripts[i].flat_stuck && !core.scripts[i].flat_slow_ok;
    if throttled && !maker_rests(core, i) {
        return;
    }
    let now = clock::now_ns(core);
    let r = {
        let Core {
            scripts,
            quotes,
            buf,
            ..
        } = core;
        let sc = &mut scripts[i];
        sc.broker.flatten(&quotes[sc.sym_ix], now, buf)
    };
    match r {
        Ok(()) => {}
        Err(BrokerFault::Internal(d)) => panic!("pine: broker defect in flatten: {d:?}"),
        Err(BrokerFault::Script(_)) => return, // pin:08-flatten-refusal-retries
    }
    // An attempt = a call that writes a Submit (the reduce-only IoC) — unless throttled.
    let n = core.buf.len();
    let mut k = 0usize;
    while k < n {
        let e = &core.buf.as_slice()[k];
        if e.verb == EngineVerb::Submit && !throttled {
            let s = &mut core.scripts[i];
            s.flat_oid = e.client_oid;
            s.flat_slow_ok = false;
            c.flatten_attempts = c.flatten_attempts.wrapping_add(1);
        }
        k += 1;
    }
    let mut k = 0usize;
    while k < n {
        // COPY: one `EngineIntent` (64 B) per instruction out of the shared buffer — as in
        // `host::reconcile` (dispatch reports into the broker while the buffer lives in `Core`).
        let e = core.buf.as_slice()[k];
        if throttled && e.verb == EngineVerb::Submit {
            // Withheld until the next confirmed bar: the broker hears it is gone.
            crate::settle::report_cancelled(core, c, i, e.client_oid);
        } else {
            crate::host::dispatch_one(core, c, i, &e, ctx);
        }
        k += 1;
    }
    finish_if_flat(core, c, i);
}

/// Whether script `i` has an acked resting maker (a `Maker` disposition record outstanding).
fn maker_rests(core: &Core, i: usize) -> bool {
    let t = &core.scripts[i].settles;
    let mut k = 0usize;
    while k < MAX_SETTLES {
        if t[k].kind == kind::MAKER {
            return true;
        }
        k += 1;
    }
    false
}
