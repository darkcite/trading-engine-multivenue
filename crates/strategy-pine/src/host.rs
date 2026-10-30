// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.4.5 / §4.5.1–§4.5.4 (T12): commit, `reconcile`, and dispatch through
//! `NativeBroker` ONLY (REVIEW S3). The member holds no order state of its own (I30): the broker
//! owns orders and the position; this module translates `EngineIntent`s into the engine's verbs
//! (`Ctx::submit` / `modify` / `cancel`), reports dispositions, and nothing else.
//!
//! **PAPER ONLY on every host the cli builds.** Slot 8 resolves `Paper` or `Off`. `Off`
//! dispatches nothing and commits nothing (§4.4.5). A `Live` route reaches here only with the
//! host's live permit (PL2, pin:pl2-live-permit: `on_start` refuses it otherwise, and `flush`
//! asserts it — fail fast, a defect); its dispositions wait the live settle window (PL1,
//! `settle`).
//!
//! **The risk gate.** Every Submit and Modify goes out through `ctx` — on the engine host the
//! set's `StampCtx` (strategy_id 8) over the `RoutedDispatcher`, whose E6 gate judges it; a cancel
//! is never withheld here (E-1..E-11). Nothing bypasses `ctx`.
//!
//! Hot module (I4): no allocation, no clock (`Order::ts_ns` = E), no `dyn`.
//!
//! Literal readings:
//! - pin:08-reconcile-at-callback-end: a committing evaluation marks its script; `flush` runs
//!   `reconcile` + dispatch once per marked script, manifest order, at the end of the callback
//!   (§4.4.2's "for each dispatching script … not yet reconciled in this callback"), after every
//!   evaluation of the callback has committed.
//! - pin:08-order-flags-set (lead ruling 2026-10-16; replaces pin:08-order-flags-unset; its
//!   POST_ONLY half RETIRED by the 8d–8f risk review): every Submit or Modify whose
//!   `EngineIntent::flags` bit 0 is set (the broker's reduce-only sizing: exits, closes, the D13
//!   flatten, the guards' `CloseAll`) carries `ORDER_FLAG_REDUCE_ONLY`; nothing else sets a flag.
//!   The paper matcher ignores the bit (on paper the broker's sizing is the guard). Live, it is a
//!   second guard against a flip: the exec router reads it on a Hyperliquid perp row (only a
//!   flagged reduction is an exit there, r28 risk review B2), and both arms send it to the venue —
//!   exec-hyperliquid on a perp row, exec-binance on USDⓈ-M (where it is also PB3's minimum
//!   exemption once PB0.2 is recorded, B1). Slot 8 is paper only today.
//! - pin:08-no-post-only-paper (risk review BLOCK): no slot-8 order carries
//!   `ORDER_FLAG_POST_ONLY`. The bit selects the paper QUEUE law, under which a cancel / modify
//!   `Ok` means only "sent" (the disposition arrives as an order event); slot 8 settles on `Ok`
//!   (§4.5.5) and ignores `on_order_event`, would share xmm's 32-order / 8-symbol queue table,
//!   and gets queue fills only when xmm's TradePrint lane is attached. `ORDER_KIND_MAKER` keeps
//!   the strict-cross law the settle rule was written for.
//! - pin:08-submit-nosuch-is-refusal: `NoSuchOrder` on a Submit (never returned by one) is handled
//!   as `Refused` (paper: `on_cancelled` now, the streak counts) — fail closed.
//! - pin:08-modify-nosuch-literal: a Modify `NoSuchOrder` reports `on_cancelled(new)` now (§4.5.4)
//!   and nothing about `prev`; the broker's next reconcile decides again.

use crate::boot::SlotRoute;
use crate::clock;
use crate::oid::encode_client_oid;
use crate::rt::Core;
use crate::settle::{self, kind, Settle};
use core_types::{CancelReq, Order, Price, Qty, Side};
use pine_broker::BrokerFault;
use pine_ir::{EngineIntent, EngineVerb, IntentKind, Primitive};
use strategy_core::{Ctx, PineCounters, SubmitErr};

/// Whether script `i` commits its evaluations (§4.4.5): enabled, not faulted / disabled, not
/// cold, and its route is not `Off`.
#[inline]
pub(crate) fn commits(core: &Core, i: usize) -> bool {
    let s = &core.scripts[i];
    s.enabled && !s.disabled && !s.cold && core.route != SlotRoute::Off
}

/// A reducing intent kind (§4.4.5: what a refusal trip still commits).
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

/// §4.3.9 step 5 / §4.4.5: `broker.apply_intents(vm.intents(), now_ns)` — every evaluation of a
/// committing script, even an empty slice (it rolls the UTC day). A suppressed script commits only
/// the reducing kinds, through the filter scratch (§4.12 #6). Marks the script for `flush`.
/// `Err(Script)` latches the script (pin:08-fault-latch-until-8e); `Err(Internal)` is ours: panic.
pub(crate) fn commit(core: &mut Core, c: &mut PineCounters, i: usize) {
    let now = clock::now_ns(core);
    let n = core.scripts[i].vm.intents().len();
    let r = if core.scripts[i].suppressed {
        let Core {
            scripts, filter, ..
        } = core;
        let sc = &mut scripts[i];
        let src = sc.vm.intents();
        // The scratch holds `max intent_cap` and the VM never emits more than its own cap.
        debug_assert!(src.len() <= filter.len());
        let mut m = 0usize;
        let mut k = 0usize;
        while k < src.len() {
            if reducing(src[k].kind) && m < filter.len() {
                // COPY: one reducing `OrderIntent` (64 B) into the filter scratch (§4.12 #6) —
                // `apply_intents` takes one contiguous slice and the VM interleaves kinds — a
                // second VM intent buffer rejected: the VM is canonical (never edited).
                filter[m] = src[k];
                m += 1;
            }
            k += 1;
        }
        c.intents_discarded = c.intents_discarded.wrapping_add((n - m) as u64);
        core.intents_committed = core.intents_committed.wrapping_add(m as u64);
        sc.broker.apply_intents(&filter[..m], now)
    } else {
        core.intents_committed = core.intents_committed.wrapping_add(n as u64);
        let sc = &mut core.scripts[i];
        sc.broker.apply_intents(sc.vm.intents(), now)
    };
    core.commits = core.commits.wrapping_add(1);
    match r {
        Ok(()) => core.dirty |= 1u32 << i,
        Err(BrokerFault::Internal(d)) => panic!("pine: broker defect in apply_intents: {d:?}"),
        Err(BrokerFault::Script(r)) => crate::fault::broker_refusal(core, c, i, r),
    }
}

/// End of a callback: `reconcile` + dispatch every marked script, manifest order (§4.5.1); a
/// disabled script's mark is dropped. Then D13's owed `flatten` calls (`fault::run_due`).
pub(crate) fn flush<C: Ctx>(core: &mut Core, c: &mut PineCounters, ctx: &mut C) {
    // Fail closed (PL2, pin:pl2-live-permit): a Live route dispatches only under the host's
    // permit (`on_start` refused it otherwise); `Off` dispatches nothing.
    assert!(
        core.route != SlotRoute::Live || core.live,
        "pine: a Live route without the host's live permit"
    );
    if core.route == SlotRoute::Off {
        // I36: `Off` never calls the ctx — no reconcile, no flatten, no PB6 cancel.
        core.dirty = 0;
        core.flat_due = 0;
        return;
    }
    let mut i = 0usize;
    while i < core.scripts.len() {
        if core.dirty & (1u32 << i) != 0 {
            core.dirty &= !(1u32 << i);
            let s = &core.scripts[i];
            if s.enabled && !s.disabled {
                reconcile(core, c, i, ctx);
            }
        }
        i += 1;
    }
    core.dirty = 0;
    if core.flat_due != 0 {
        crate::fault::run_due(core, c, ctx);
    }
}

/// `reconcile(&quote[s], now_ns, &mut buf)` then dispatch of `buf` in order (I26).
fn reconcile<C: Ctx>(core: &mut Core, c: &mut PineCounters, i: usize, ctx: &mut C) {
    let now = clock::now_ns(core);
    let r = {
        let Core {
            scripts,
            quotes,
            buf,
            ..
        } = core;
        let sc = &mut scripts[i];
        sc.broker.reconcile(&quotes[sc.sym_ix], now, buf)
    };
    match r {
        Ok(()) => {}
        Err(BrokerFault::Internal(d)) => panic!("pine: broker defect in reconcile: {d:?}"),
        Err(BrokerFault::Script(r)) => {
            crate::fault::broker_refusal(core, c, i, r);
            return;
        }
    }
    let n = core.buf.len();
    let mut k = 0usize;
    while k < n {
        // COPY: one `EngineIntent` (64 B) per instruction out of the shared buffer — dispatch
        // reports dispositions into the broker (`&mut Core`) while the buffer lives in `Core` — a
        // per-script buffer moved out per reconcile rejected: the same bytes, plus 2 KiB × 8.
        let e = core.buf.as_slice()[k];
        dispatch_one(core, c, i, &e, ctx);
        k += 1;
    }
}

/// One instruction (§4.5.2, §4.5.4). Flags per pin:08-order-flags-set.
pub(crate) fn dispatch_one<C: Ctx>(
    core: &mut Core,
    c: &mut PineCounters,
    i: usize,
    e: &EngineIntent,
    ctx: &mut C,
) {
    // §4.5.2: a pine-broker defect fails fast (00-INDEX §3.12).
    assert!(e.side <= 1, "pine: EngineIntent side > 1");
    assert!(
        e.px_1e6 > 0 && e.qty_1e6 > 0,
        "pine: EngineIntent px/qty ≤ 0"
    );
    // §4.6: a broker oid past 2^29 − 1 (or 0) is this crate's invariant broken — fail fast in
    // every build (the oid would alias another script's bits).
    assert!(
        broker_oid_ok(e.client_oid),
        "pine: broker oid outside 1..2^29"
    );
    if e.verb == EngineVerb::Modify {
        assert!(
            broker_oid_ok(e.prev_client_oid),
            "pine: broker prev oid outside 1..2^29"
        );
    }
    let script = u8::try_from(i).unwrap_or(u8::MAX);
    let epoch = core.epoch;
    let (venue, sym, e_ns) = {
        let s = &core.scripts[i];
        (s.venue, s.sym, core.e_ns)
    };
    let side = if e.side == 0 { Side::Bid } else { Side::Ask };
    let reduce = e.flags & 1;
    // §4.5.4 while suppressed: an increasing Submit / Modify is withheld, reported on_cancelled.
    if core.scripts[i].suppressed && e.verb != EngineVerb::Cancel && reduce == 0 {
        c.entries_suppressed = c.entries_suppressed.wrapping_add(1);
        settle::report_cancelled(core, c, i, e.client_oid);
        return;
    }
    match e.verb {
        EngineVerb::Submit => {
            let maker = e.primitive == Primitive::MakerPostOnly;
            let kind = if maker {
                core_fill::ORDER_KIND_MAKER
            } else {
                core_fill::ORDER_KIND_IOC
            };
            // COPY: one `Order` (64 B) per instruction (§4.12 #5) — `EngineIntent` and `Order` are
            // two frozen layouts and `Ctx::submit` takes it by value — none rejected.
            let order = Order::new(
                e_ns,
                venue,
                sym,
                side,
                kind,
                Price::from_raw(e.px_1e6),
                Qty::from_raw(e.qty_1e6),
                encode_client_oid(epoch, script, e.client_oid),
            )
            .with_ttl_ns(e.ttl_ns);
            let order = with_flags(order, e);
            let res = ctx.submit(order);
            crate::tape::dispatch(core, c, i, e, order.flags, crate::tape::result_code(res));
            if res.is_ok() {
                core.scripts[i].n_submits = core.scripts[i].n_submits.wrapping_add(1);
            } else {
                core.scripts[i].n_submit_errors = core.scripts[i].n_submit_errors.wrapping_add(1);
            }
            match res {
                Ok(()) => {
                    c.submits = c.submits.wrapping_add(1);
                    acked(core, c, i, e, maker, e_ns);
                }
                Err(SubmitErr::RingFull) => {
                    c.submit_errors = c.submit_errors.wrapping_add(1);
                    settle::report_cancelled(core, c, i, e.client_oid);
                }
                Err(err) => {
                    // Refused / Unsupported (NoSuchOrder: pin:08-submit-nosuch-is-refusal).
                    c.submit_errors = c.submit_errors.wrapping_add(1);
                    if core.live && err != SubmitErr::Unsupported {
                        // PL1 (§4.5.4 live): the request may have reached the venue — no ack,
                        // `on_cancelled` after the window unless its fills total its size.
                        settle::wait_window(core, i, e.client_oid, err_rec(e, maker));
                    } else {
                        settle::report_cancelled(core, c, i, e.client_oid);
                    }
                    settle::streak_err(core, c, i);
                }
            }
        }
        EngineVerb::Modify => {
            // COPY: the replacement `Order` (64 B) per Modify (§4.12 #5) — as on a submit.
            let order = Order::new(
                e_ns,
                venue,
                sym,
                side,
                core_fill::ORDER_KIND_MAKER,
                Price::from_raw(e.px_1e6),
                Qty::from_raw(e.qty_1e6),
                encode_client_oid(epoch, script, e.client_oid),
            );
            let order = with_flags(order, e);
            let prev = encode_client_oid(epoch, script, e.prev_client_oid);
            let res = ctx.modify(prev, order);
            crate::tape::dispatch(core, c, i, e, order.flags, crate::tape::result_code(res));
            if res.is_ok() {
                core.scripts[i].n_submits = core.scripts[i].n_submits.wrapping_add(1);
            } else {
                core.scripts[i].n_submit_errors = core.scripts[i].n_submit_errors.wrapping_add(1);
            }
            match res {
                Ok(()) => {
                    c.modifies = c.modifies.wrapping_add(1);
                    acked(core, c, i, e, true, e_ns);
                    // The replaced `prev` is gone; its fills (if any) are pumped by the next
                    // settle point, so it is reported then (I27).
                    settle::gone(core, c, i, e.prev_client_oid);
                }
                Err(SubmitErr::RingFull) => {
                    c.submit_errors = c.submit_errors.wrapping_add(1);
                    settle::report_cancelled(core, c, i, e.client_oid);
                }
                Err(err) => {
                    c.submit_errors = c.submit_errors.wrapping_add(1);
                    if core.live && err != SubmitErr::Unsupported {
                        // PL1 (§4.5.4 live): `Refused` / `NoSuchOrder` report `new` after the
                        // window; nothing about `prev` (pin:08-modify-nosuch-literal).
                        settle::wait_window(core, i, e.client_oid, err_rec(e, true));
                    } else {
                        settle::report_cancelled(core, c, i, e.client_oid);
                    }
                    settle::streak_err(core, c, i);
                }
            }
        }
        EngineVerb::Cancel => {
            let _ = cancel_one(core, c, i, e, ctx);
        }
    }
}

/// The Cancel arm of [`dispatch_one`], shared with PB6 (`fault::run_due`'s venue-fill cancels,
/// which have no broker intent and so skip `dispatch_one`'s px/qty/side asserts): the
/// `CancelReq` for broker oid `e.client_oid`, its §4.9.2 `DISPATCH` row (`e` as given), its
/// disposition. Never gated (I26/I27). Returns the ctx's answer.
pub(crate) fn cancel_one<C: Ctx>(
    core: &mut Core,
    c: &mut PineCounters,
    i: usize,
    e: &EngineIntent,
    ctx: &mut C,
) -> Result<(), SubmitErr> {
    debug_assert!(e.verb == EngineVerb::Cancel);
    // §4.6: fail fast in every build (the oid would alias another script's bits).
    assert!(
        broker_oid_ok(e.client_oid),
        "pine: broker oid outside 1..2^29"
    );
    let script = u8::try_from(i).unwrap_or(u8::MAX);
    let (venue, sym) = (core.scripts[i].venue, core.scripts[i].sym);
    // COPY: one `CancelReq` (24 B) per cancel (§4.12 #5) — `Ctx::cancel` takes it by value.
    let req = CancelReq::new(
        core.e_ns,
        venue,
        sym,
        encode_client_oid(core.epoch, script, e.client_oid),
    );
    let res = ctx.cancel(req);
    crate::tape::dispatch(core, c, i, e, 0, crate::tape::result_code(res));
    match res {
        Ok(()) | Err(SubmitErr::NoSuchOrder) => {
            c.cancels = c.cancels.wrapping_add(1);
            settle::gone(core, c, i, e.client_oid);
        }
        Err(SubmitErr::Unsupported) => {
            c.cancel_errors = c.cancel_errors.wrapping_add(1);
        }
        // RingFull / Refused: nothing — the broker re-emits after `quote_stale_ns` (PB6: the
        // next confirmed chart bar re-sends, pin:pb6-cancel-retry-per-bar).
        Err(_) => {
            c.cancel_errors = c.cancel_errors.wrapping_add(1);
        }
    }
    res
}

/// §4.6: a broker oid the engine client oid can carry: `1 ≤ oid < 2^29`.
#[inline(always)]
const fn broker_oid_ok(oid: u32) -> bool {
    oid != 0 && oid < (1u32 << crate::oid::BROKER_OID_BITS)
}

/// pin:08-order-flags-set: the reduce-only bit from the broker's sizing; nothing else — never
/// the post-only bit (pin:08-no-post-only-paper).
#[inline]
fn with_flags(order: Order, e: &EngineIntent) -> Order {
    // The broker marks every `ReduceOnlyIoc` reducing (impl-05 §4.12.4); a mismatch is its defect.
    debug_assert!(e.primitive != Primitive::ReduceOnlyIoc || e.flags & 1 == 1);
    if e.flags & 1 == 1 {
        order.with_reduce_only()
    } else {
        order
    }
}

/// PL1: the record a live `Refused` / `NoSuchOrder` waits the window under — its kind by
/// primitive (the fee rate of a fill that still comes), its size and reduce bit.
#[inline]
fn err_rec(e: &EngineIntent, maker: bool) -> Settle {
    Settle {
        oid: e.client_oid,
        kind: if maker { kind::MAKER } else { kind::JUDGE },
        qty: e.qty_1e6,
        reduce: e.flags & 1,
        ..Settle::FREE
    }
}

/// `Ok` for a Submit or Modify: `on_ack`, reset the streak, track the disposition.
fn acked(core: &mut Core, c: &mut PineCounters, i: usize, e: &EngineIntent, maker: bool, ts: u64) {
    core.scripts[i].broker.on_ack(e.client_oid);
    c.acks = c.acks.wrapping_add(1);
    settle::streak_ok(core, i);
    if core.live && !maker {
        // PL1 (§4.5.5 live step 2): the IoC is reported after the window unless its fills total
        // its size first; no gone-evidence judges it (only the venue does).
        settle::wait_window(core, i, e.client_oid, err_rec(e, false));
        return;
    }
    let rec = if maker {
        Settle {
            oid: e.client_oid,
            kind: kind::MAKER,
            qty: e.qty_1e6,
            reduce: e.flags & 1,
            ..Settle::FREE
        }
    } else {
        let v = usize::from(core.scripts[i].venue as u8);
        // Δ by venue byte; a byte past the table never activates (fail closed: only the expiry
        // ends it).
        let delta = core.activation_ns.get(v).copied().unwrap_or(u64::MAX);
        Settle {
            t_active: ts.saturating_add(delta),
            expiry: core_fill::expiry_at(ts, e.ttl_ns),
            qty: e.qty_1e6,
            oid: e.client_oid,
            kind: kind::JUDGE,
            reduce: e.flags & 1,
            ..Settle::FREE
        }
    };
    settle::track(core, i, rec);
}

#[cfg(test)]
mod tests {
    use super::broker_oid_ok;

    /// §4.6 (risk review FIX-NOW): the 2^29 bound `dispatch_one` asserts in every build.
    #[test]
    fn broker_oid_bound_is_exact() {
        assert!(!broker_oid_ok(0));
        assert!(broker_oid_ok(1));
        assert!(broker_oid_ok((1 << 29) - 1));
        assert!(!broker_oid_ok(1 << 29));
        assert!(!broker_oid_ok(u32::MAX));
    }
}
