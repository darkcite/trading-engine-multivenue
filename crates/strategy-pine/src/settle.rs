// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.5.4–§4.5.6 (T13): dispositions — "`on_cancelled` only after every fill" (I27) —
//! and the per-script refusal streak (I29).
//!
//! **Paper (engine host).** An acked IoC is tracked as a `Judge`: the paper matcher judges it once
//! (one fill, or a cancel). A fill ends it (a partial one is reported `on_cancelled` right after
//! `on_fill`, `fills`). Without a fill, a tick of its symbol that is fill evidence at or after
//! `t_active`, or past its expiry, marks it `gone`; the next **settle point** (`on_venue_event`,
//! the only callback after the paper-fill pump) reports it. A maker cancel `Ok` / `NoSuchOrder`
//! (and a `Modify`'s replaced `prev`) is `gone` at once and reported at the next settle point (now
//! when the current callback is one). **Replay:** every `on_tick` / `on_venue_event` is a settle
//! point (§4.5.6). **Live** (PL1, a `Live` route with the host's permit, PL2): the settle window
//! of §4.5.5 — at dispatch `until = E + live_settle_ms × 1e6`; an `Ok` acks at once; an IoC, a
//! cancel and a modify's replaced `prev` are reported `on_cancelled` at the first settle point
//! with `E ≥ until` unless their fills already total their size; a `Refused` / `NoSuchOrder`
//! waits the same window (`RingFull` and `Unsupported` are reported now, §4.5.4). It holds for a
//! QUEUED `Ok` (Binance: the ACK and the fills come later) and a SYNCHRONOUS one (HL) alike —
//! the member never reads what an `Ok` meant. A dark feed freezes E and holds the window.
//!
//! Hot module (I4): fixed tables, index loops, no allocation, no clock.
//!
//! Literal readings:
//! - pin:pl1-window-e-at-resolve: "the first settle point with `E ≥ until`" reads E at the
//!   settle point, i.e. BEFORE the settling event's own `ts_ns` is consumed (`on_venue_event`
//!   resolves first, §4.4.2); the input that carries E past `until` ripens the record for the
//!   NEXT settle point.
//! - pin:pl1-unknown-size-reports: a live wait on an oid with no record (its size unknown) is
//!   tracked with `qty = i64::MAX`, so no fill completes it: the window always reports it.
//! - pin:pl1-redispatch-rearms: a second dispatch on a waiting oid (a re-sent cancel) sets
//!   `until` again from that dispatch's E (step 1 is per dispatch).
//! - pin:08-settle-table-per-script: each script keeps `MAX_SETTLES` (= the broker's sent-table
//!   size) records; a full table is a defect (the broker cannot have more outstanding) and panics.
//! - pin:08-maker-record: an acked maker keeps a `Maker` record (its fee rate and its reduce bit for
//!   the flip guard) until its fills total its size or its cancel is reported.

use crate::rt::Core;
use crate::MAX_SETTLES;
use core_types::Tick;
use strategy_core::PineCounters;

/// Settle record kinds.
pub(crate) mod kind {
    /// Free slot.
    pub(crate) const FREE: u8 = 0;
    /// An acked paper IoC awaiting its one judgment.
    pub(crate) const JUDGE: u8 = 1;
    /// An acked resting maker.
    pub(crate) const MAKER: u8 = 2;
    /// Gone (cancel `Ok` / `NoSuchOrder`, a replaced `prev`): `on_cancelled` at the next settle
    /// point.
    pub(crate) const GONE: u8 = 3;
}

/// One awaited disposition (§4.5.5). 48 B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct Settle {
    /// `t_active = order.ts_ns + Δ` (E domain).
    pub(crate) t_active: u64,
    /// `core_fill::expiry_at(order.ts_ns, ttl_ns)`; 0 = never.
    pub(crate) expiry: u64,
    /// PL1: the live settle window's end, `E + live_settle_ms × 1e6` at dispatch (E domain);
    /// 0 = no window (paper, replay, a resting live maker).
    pub(crate) until: u64,
    /// Ordered quantity ×1e6.
    pub(crate) qty: i64,
    /// Filled so far ×1e6.
    pub(crate) filled: i64,
    /// The broker oid (0 = free).
    pub(crate) oid: u32,
    /// [`kind`].
    pub(crate) kind: u8,
    /// Gone-evidence seen; reported at the next settle point.
    pub(crate) gone: u8,
    /// `EngineIntent::flags` bit 0 (reduce-only).
    pub(crate) reduce: u8,
    /// Always zero.
    pub(crate) _pad: u8,
}

const _: () = assert!(core::mem::size_of::<Settle>() == 48);
const _: () = assert!(core::mem::align_of::<Settle>() == 8);

impl Settle {
    /// A free slot.
    pub(crate) const FREE: Self = Self {
        t_active: 0,
        expiry: 0,
        until: 0,
        qty: 0,
        filled: 0,
        oid: 0,
        kind: kind::FREE,
        gone: 0,
        reduce: 0,
        _pad: 0,
    };
}

/// Retired oids remembered for the late-fill guard (§4.5.8).
pub(crate) const RETIRED_CAP: usize = 64;

/// One oid already reported `on_cancelled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct Retired {
    /// Broker oid (0 = empty).
    pub(crate) oid: u32,
    /// Script index.
    pub(crate) script: u8,
    /// Reduce-only bit of its instruction.
    pub(crate) reduce: u8,
    /// Maker (fee rate).
    pub(crate) maker: u8,
    /// Always zero.
    pub(crate) _pad: u8,
}

const _: () = assert!(core::mem::size_of::<Retired>() == 8);

impl Retired {
    /// Empty.
    pub(crate) const NONE: Self = Self {
        oid: 0,
        script: 0,
        reduce: 0,
        maker: 0,
        _pad: 0,
    };
}

/// Track a new record for script `i` (pin:08-settle-table-per-script).
pub(crate) fn track(core: &mut Core, i: usize, rec: Settle) {
    let t = &mut core.scripts[i].settles;
    let mut k = 0usize;
    while k < MAX_SETTLES {
        if t[k].kind == kind::FREE {
            t[k] = rec;
            return;
        }
        k += 1;
    }
    panic!("pine: settle table full (the broker's sent table bounds it)");
}

/// The record of `oid`, if tracked.
pub(crate) fn find(core: &Core, i: usize, oid: u32) -> Option<usize> {
    let t = &core.scripts[i].settles;
    let mut k = 0usize;
    while k < MAX_SETTLES {
        if t[k].kind != kind::FREE && t[k].oid == oid {
            return Some(k);
        }
        k += 1;
    }
    None
}

/// Report `on_cancelled(oid)` to script `i`'s broker, remember it (the late-fill ring), free its
/// record and owe the script a reconcile (§4.5.1 "after a settle point freed one of its orders").
pub(crate) fn report_cancelled(core: &mut Core, c: &mut PineCounters, i: usize, oid: u32) {
    let (reduce, maker) = match find(core, i, oid) {
        Some(k) => {
            let r = core.scripts[i].settles[k];
            core.scripts[i].settles[k] = Settle::FREE;
            (r.reduce, u8::from(r.kind == kind::MAKER))
        }
        None => (0, 0),
    };
    core.scripts[i].broker.on_cancelled(oid);
    c.cancelled_reported = c.cancelled_reported.wrapping_add(1);
    // impl-Bp §3.10 R6: kind 3 — every cancel or withhold the member reports.
    crate::delivery::done(core, i, oid);
    let h = core.retired_head;
    core.retired[h] = Retired {
        oid,
        script: u8::try_from(i).unwrap_or(u8::MAX),
        reduce,
        maker,
        _pad: 0,
    };
    core.retired_head = (h + 1) % RETIRED_CAP;
    core.dirty |= 1u32 << i;
    // D13: a flattening script's disposition owes a `flatten` call (and may fail its attempt).
    crate::fault::on_reported(core, c, i, oid);
}

/// PL1: the window's end for a dispatch in this callback, `E + live_settle_ms × 1e6`.
#[inline]
pub(crate) fn window_end(core: &Core) -> u64 {
    core.e_ns.saturating_add(core.settle_window_ns)
}

/// PL1 (§4.5.5 live, steps 1–3): `oid` of script `i` is reported `on_cancelled` at the first
/// settle point with `E ≥ until` unless its fills total its size first. An existing record keeps
/// its kind, size and fills (pin:pl1-redispatch-rearms); otherwise `fresh` is tracked.
pub(crate) fn wait_window(core: &mut Core, i: usize, oid: u32, fresh: Settle) {
    debug_assert!(core.live, "pine: a settle window off Live");
    let until = window_end(core);
    match find(core, i, oid) {
        Some(k) => {
            let r = &mut core.scripts[i].settles[k];
            r.gone = 1;
            r.until = until;
        }
        None => track(
            core,
            i,
            Settle {
                oid,
                gone: 1,
                until,
                ..fresh
            },
        ),
    }
}

/// Reported at this settle point: gone, and its window (if any) has run out at E.
#[inline(always)]
fn ripe(r: &Settle, e_ns: u64) -> bool {
    r.gone == 1 && (r.until == 0 || e_ns >= r.until)
}

/// Mark `oid` gone: reported now when this callback is a settle point, else at the next one.
/// Live (PL1): after the window, never now.
pub(crate) fn gone(core: &mut Core, c: &mut PineCounters, i: usize, oid: u32) {
    if core.live {
        // pin:pl1-unknown-size-reports: no record → the size is unknown, so no fill completes it.
        wait_window(
            core,
            i,
            oid,
            Settle {
                kind: kind::GONE,
                qty: i64::MAX,
                ..Settle::FREE
            },
        );
        return;
    }
    if core.settle_now {
        report_cancelled(core, c, i, oid);
        return;
    }
    match find(core, i, oid) {
        Some(k) => {
            let r = &mut core.scripts[i].settles[k];
            r.kind = kind::GONE;
            r.gone = 1;
        }
        None => track(
            core,
            i,
            Settle {
                oid,
                kind: kind::GONE,
                gone: 1,
                ..Settle::FREE
            },
        ),
    }
}

/// §4.4.2 `settle::observe_tick`: new gone-evidence for the open IoCs on `t.sym`. Resolved at a
/// LATER settle point.
pub(crate) fn observe_tick(core: &mut Core, t: &Tick) {
    let evidence = core_fill::is_fill_evidence(t);
    let mut i = 0usize;
    while i < core.scripts.len() {
        if core.scripts[i].sym == t.sym {
            let tab = &mut core.scripts[i].settles;
            let mut k = 0usize;
            while k < MAX_SETTLES {
                let r = &mut tab[k];
                if r.kind == kind::JUDGE
                    && r.gone == 0
                    && ((evidence && t.ts_ns >= r.t_active)
                        || core_fill::expired_at(t.ts_ns, r.expiry))
                {
                    r.gone = 1;
                }
                k += 1;
            }
        }
        i += 1;
    }
}

/// `settle::resolve_point`: every gone record is reported `on_cancelled` (the pump is done, so no
/// fill of it can still come on paper); a live record only once `E ≥ until` (PL1,
/// pin:pl1-window-e-at-resolve). Marks the callback a settle point.
pub(crate) fn resolve_point(core: &mut Core, c: &mut PineCounters) {
    core.settle_now = true;
    c.settle_points = c.settle_points.wrapping_add(1);
    let e = core.e_ns;
    let mut i = 0usize;
    while i < core.scripts.len() {
        let mut k = 0usize;
        while k < MAX_SETTLES {
            let r = core.scripts[i].settles[k];
            if r.kind != kind::FREE && ripe(&r, e) {
                report_cancelled(core, c, i, r.oid);
            }
            k += 1;
        }
        i += 1;
    }
}

/// Whether any disposition is pending (an `on_venue_event` that is not a pine Trade is then still
/// a settle point; otherwise it is ignored, untaped — §4.4.2). A live record counts once its
/// window has run out (such an event leaves E unchanged, so an unripe one cannot ripen in it).
pub(crate) fn any_pending(core: &Core) -> bool {
    let e = core.e_ns;
    let mut i = 0usize;
    while i < core.scripts.len() {
        let mut k = 0usize;
        while k < MAX_SETTLES {
            if ripe(&core.scripts[i].settles[k], e) {
                return true;
            }
            k += 1;
        }
        i += 1;
    }
    false
}

/// §4.5.4: one Submit/Modify error (RingFull excluded) of script `i`. At `refusal_streak_max`
/// its entries are suppressed, sticky (I29); no other script is touched.
pub(crate) fn streak_err(core: &mut Core, c: &mut PineCounters, i: usize) {
    let s = &mut core.scripts[i];
    s.streak = s.streak.saturating_add(1);
    if !s.suppressed && s.streak >= core.refusal_max {
        s.suppressed = true;
        c.refusal_trips = c.refusal_trips.wrapping_add(1);
        let detail = s.streak;
        crate::tape::guard(core, c, i, 4);
        crate::fault::push(
            core,
            c,
            i,
            crate::fault::Cause {
                event: strategy_core::pine_incident::REFUSAL_TRIP,
                source: crate::fault::source::HOST,
                fault_kind: 0,
                pc: u32::MAX,
                detail,
            },
        );
    }
}

/// §4.5.4: an `Ok` or a fill of script `i` resets its streak (the trip stays).
#[inline]
pub(crate) fn streak_ok(core: &mut Core, i: usize) {
    core.scripts[i].streak = 0;
}
