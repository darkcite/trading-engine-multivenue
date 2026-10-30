// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.7 (8f, T16): the two levers (route, `enabled`), `reconcile_boot` sequencing (D19,
//! I38), the four boot-exit policies (Q1, §4.7.8, I40) and the host `CloseAll` they share with
//! the late-flip guard (§4.5.8).
//!
//! The levers (§4.7.1) are enforced where they bite: `host::commits` is `may_commit`
//! (`enabled && !disabled && !cold && route != Off`), `host::flush` dispatches only for
//! `enabled` scripts and calls no ctx verb on `Off` (I36), and `fault::disable` flattens only a
//! script that may dispatch. A `Live` route starts only under the host's live permit
//! (pin:pl2-live-permit, which replaced pin:08-live-refused-at-start); the cli never sets the
//! permit, so slot 8 stays paper (fail closed) until the arming change.
//!
//! COPY-DOCTRINE: `adopt_warm`, `boot_commit` and `finish_boot` run inside `on_start` (boot only);
//! `late_flip_guard` is hot (I4: no allocation, no clock, no `dyn`).
//!
//! Literal readings:
//! - pin:08-boot-state-any-route is RETIRED (risk item (a)) by pin:08-boot-state-live-only:
//!   `configure` refuses a non-empty `boot_state` off Live (`6013`, key `boot_state`), so paper and
//!   replay always adopt `(0, 0, 0)` (§4.7.5). A script adopts its chart symbol's entry (zeros
//!   without one). The tests drive non-zero states on paper only through the `test-seams` feature
//!   (`seed_boot_state_for_test`), which no host enables.
//! - pin:08-boot-committing-is-may-dispatch: "each committing script" (§4.7.5) = `enabled` and
//!   route ≠ `Off` and not disabled at that point; cold or warm. An `enabled = 0` script never
//!   reaches its broker (I37).
//! - pin:08-boot-policy-default-accept: `boot_exit_policy` absent (allowed off Live) is `accept`,
//!   the consumer default `core_config::pine` names.
//! - pin:08-boot-guard-only-when-adopted: `protective` and `flatten` commit the host `CloseAll`
//!   only when the adopted position is non-zero ("every adopted position … closed"); a flat
//!   adoption costs nothing and counts nothing.
//! - pin:08-boot-commit-at-boot-instant: the last warm-up evaluation's committed intents (and the
//!   guard's `CloseAll`) are applied at `now_ns = ns_from_ms(boot_utc_ms)` (§4.7.8's text for the
//!   host `CloseAll`, taken for the `standing` / `protective` commit of the same evaluation).
//! - pin:08-boot-protective-protects: the protective set = an `Exit` carrying `STOP`, `LOSS`,
//!   `TRAIL_PRICE` or `TRAIL_POINTS`, or a `Close` / `CloseAll`, among the COMMITTED reducing
//!   intents of the last warm-up evaluation.
//! - pin:08-late-flip-skips-disabled: a disabled script is already being flattened (D13); the
//!   late-flip guard commits nothing for it (it still counts and writes the incident).
//! - pin:08-boot-day-primed is RETIRED: pine-broker keeps the `reconcile_boot` seed at the first
//!   `roll_day` (`pin:05-boot-day-seed-kept`, R-08-T16-1), so no priming call is made.
//! - pin:08-guard-tape-t18 (closed by T18): the boot guard writes `GUARD` 2, the late-flip guard
//!   `GUARD` 3.

use crate::fault::{self, Cause};
use crate::rt::Core;
use crate::VenueBootState;
use core_config::pine::BootExitPolicy;
use pine_broker::BrokerFault;
use pine_ir::{intent_flag, IntentKind, OrderIntent};
use strategy_core::{pine_incident, PineCounters};

/// What `on_start` hands the warm-up walk and `finish_boot` (boot only).
pub(crate) struct BootArm<'a> {
    /// `PineBootCfg::boot_state` (pin:08-boot-state-any-route).
    pub(crate) states: &'a [VenueBootState],
    /// `[exec.slot.8.pine] boot_exit_policy` (pin:08-boot-policy-default-accept).
    pub(crate) policy: BootExitPolicy,
    /// `ns_from_ms(boot_utc_ms)` (pin:08-boot-commit-at-boot-instant).
    pub(crate) boot_ns: u64,
}

/// pin:08-boot-committing-is-may-dispatch.
#[inline]
pub(crate) fn committing_at_boot(core: &Core, i: usize) -> bool {
    let s = &core.scripts[i];
    s.enabled && !s.disabled && core.route != crate::SlotRoute::Off
}

/// D19, I38: `reconcile_boot` exactly once, while the broker is pristine.
pub(crate) fn adopt(core: &mut Core, c: &mut PineCounters, i: usize, arm: &BootArm<'_>) {
    if core.scripts[i].adopted {
        return;
    }
    let sym = core.scripts[i].sym;
    let mut st = (0i64, 0i64, 0i64);
    for b in arm.states {
        if b.sym == sym {
            st = (b.position_1e6, b.avg_px_1e6, b.day_notional_1e6);
        }
    }
    let s = &mut core.scripts[i];
    // `reconcile_boot` silently changes nothing on a non-pristine broker, and a flat adoption
    // cannot be told from a skipped one by the position alone (risk review r15): the broker must
    // be pristine before (I38; a sequencing defect of this crate otherwise) …
    assert!(
        s.broker.is_pristine(),
        "pine: reconcile_boot on a non-pristine broker (I38)"
    );
    s.broker.reconcile_boot(st.0, st.1, st.2);
    // … and the adoption must have taken: no longer pristine, the venue's position and (when not
    // flat) its average price on the book.
    let v = s.broker.view();
    assert!(
        !s.broker.is_pristine()
            && v.position_size_1e6 == st.0
            && (st.0 == 0 || v.position_avg_price_1e6 == st.1),
        "pine: reconcile_boot did not adopt (I38)"
    );
    s.adopted = true;
    s.boot_st = st;
    crate::tape::boot_state(core, c, i, st);
}

/// One host `CloseAll` through `apply_intents` (the broker's ordinary close path: a
/// `ReduceOnlyIoc` at the band at the first fresh quote). `true` when committed.
fn host_close_all(core: &mut Core, c: &mut PineCounters, i: usize, now_ns: u64) -> bool {
    // §4.7.8: `OrderIntent { kind: CloseAll, ..Default::default() }` (`seq` 0, `id` NONE), on
    // the stack.
    let intent = [OrderIntent {
        kind: IntentKind::CloseAll,
        ..OrderIntent::default()
    }];
    let r = core.scripts[i].broker.apply_intents(&intent, now_ns);
    core.commits = core.commits.wrapping_add(1);
    match r {
        Ok(()) => {
            core.intents_committed = core.intents_committed.wrapping_add(1);
            core.dirty |= 1u32 << i;
            true
        }
        Err(BrokerFault::Internal(d)) => panic!("pine: broker defect in apply_intents: {d:?}"),
        Err(BrokerFault::Script(r)) => {
            fault::broker_refusal(core, c, i, r);
            false
        }
    }
}

/// §4.7.8: the boot guard's `CloseAll`, at most once per script per boot (I40); counts
/// `boot_guard_flattens` and writes one `BOOT_GUARD` incident.
fn boot_guard(core: &mut Core, c: &mut PineCounters, i: usize, arm: &BootArm<'_>) {
    if core.scripts[i].guard_done || core.scripts[i].broker.view().position_size_1e6 == 0 {
        return; // pin:08-boot-guard-only-when-adopted
    }
    core.scripts[i].guard_done = true;
    if host_close_all(core, c, i, arm.boot_ns) {
        c.boot_guard_flattens = c.boot_guard_flattens.wrapping_add(1);
        crate::tape::guard(core, c, i, 2);
        fault::push(
            core,
            c,
            i,
            Cause {
                event: pine_incident::BOOT_GUARD,
                source: fault::source::HOST,
                fault_kind: 0,
                pc: u32::MAX,
                detail: arm.policy as u32,
            },
        );
    }
}

/// Whether `k` is a reducing intent kind (what `protective` commits).
#[inline]
fn reducing(k: IntentKind) -> bool {
    matches!(
        k,
        IntentKind::Exit
            | IntentKind::Close
            | IntentKind::CloseAll
            | IntentKind::Cancel
            | IntentKind::CancelAll
    )
}

/// pin:08-boot-protective-protects.
#[inline]
fn protects(x: &OrderIntent) -> bool {
    const STOPS: u16 = intent_flag::STOP
        | intent_flag::LOSS
        | intent_flag::TRAIL_PRICE
        | intent_flag::TRAIL_POINTS;
    match x.kind {
        IntentKind::Exit => x.flags & STOPS != 0,
        IntentKind::Close | IntentKind::CloseAll => true,
        _ => false,
    }
}

/// §4.7.8, a WARM script right after its last warm-up evaluation (adopted just before it): the
/// policy's commit, then (`protective`, `flatten`) the guard. `intents` were discarded by the walk.
pub(crate) fn boot_commit(core: &mut Core, c: &mut PineCounters, i: usize, arm: &BootArm<'_>) {
    if core.scripts[i].disabled {
        return; // its last warm-up evaluation faulted: D13 owns it now
    }
    match arm.policy {
        BootExitPolicy::Accept => {}
        BootExitPolicy::Standing => {
            let n = core.scripts[i].vm.intents().len();
            let r = {
                let sc = &mut core.scripts[i];
                sc.broker.apply_intents(sc.vm.intents(), arm.boot_ns)
            };
            core.commits = core.commits.wrapping_add(1);
            core.intents_committed = core.intents_committed.wrapping_add(n as u64);
            c.intents_discarded = c.intents_discarded.saturating_sub(n as u64);
            match r {
                Ok(()) => core.dirty |= 1u32 << i,
                Err(BrokerFault::Internal(d)) => {
                    panic!("pine: broker defect in apply_intents: {d:?}")
                }
                Err(BrokerFault::Script(r)) => fault::broker_refusal(core, c, i, r),
            }
        }
        BootExitPolicy::Protective => {
            let (m, protected) = {
                let Core {
                    scripts, filter, ..
                } = &mut *core;
                let src = scripts[i].vm.intents();
                let mut m = 0usize;
                let mut protected = false;
                let mut k = 0usize;
                while k < src.len() {
                    if reducing(src[k].kind) && m < filter.len() {
                        // COPY: one reducing `OrderIntent` (128 B) into the filter scratch —
                        // boot only, as `host::commit`'s suppressed path (§4.12 #6).
                        filter[m] = src[k];
                        protected |= protects(&src[k]);
                        m += 1;
                    }
                    k += 1;
                }
                (m, protected)
            };
            let r = {
                let Core {
                    scripts, filter, ..
                } = &mut *core;
                scripts[i].broker.apply_intents(&filter[..m], arm.boot_ns)
            };
            core.commits = core.commits.wrapping_add(1);
            core.intents_committed = core.intents_committed.wrapping_add(m as u64);
            c.intents_discarded = c.intents_discarded.saturating_sub(m as u64);
            match r {
                Ok(()) => core.dirty |= 1u32 << i,
                Err(BrokerFault::Internal(d)) => {
                    panic!("pine: broker defect in apply_intents: {d:?}")
                }
                Err(BrokerFault::Script(r)) => {
                    fault::broker_refusal(core, c, i, r);
                    return;
                }
            }
            if !protected {
                boot_guard(core, c, i, arm);
            }
        }
        BootExitPolicy::Flatten => boot_guard(core, c, i, arm),
    }
}

/// `on_start`, after the walk: every committing script not adopted at a last warm-up evaluation
/// (the COLD ones) adopts now; `protective` and `flatten` close an adopted position, `accept` and
/// `standing` hold it until the script is warm (§4.7.8 "Cold scripts").
pub(crate) fn finish_boot(core: &mut Core, c: &mut PineCounters, arm: &BootArm<'_>) {
    let mut i = 0usize;
    while i < core.scripts.len() {
        if committing_at_boot(core, i) && !core.scripts[i].adopted {
            adopt(core, c, i, arm);
            if matches!(
                arm.policy,
                BootExitPolicy::Protective | BootExitPolicy::Flatten
            ) {
                boot_guard(core, c, i, arm);
            }
        }
        i += 1;
    }
}

/// §4.5.8: a late reducer fill flipped script `i`'s position — a `LATE_FLIP` incident and one host
/// `CloseAll` at E (the script stays enabled). The caller counted `late_reducer_flips`.
pub(crate) fn late_flip_guard(core: &mut Core, c: &mut PineCounters, i: usize) {
    fault::push(
        core,
        c,
        i,
        Cause {
            event: pine_incident::LATE_FLIP,
            source: fault::source::HOST,
            fault_kind: 0,
            pc: u32::MAX,
            detail: 0,
        },
    );
    if core.scripts[i].disabled {
        return; // pin:08-late-flip-skips-disabled
    }
    let now = crate::clock::now_ns(core);
    if host_close_all(core, c, i, now) {
        crate::tape::guard(core, c, i, 3);
    }
}
