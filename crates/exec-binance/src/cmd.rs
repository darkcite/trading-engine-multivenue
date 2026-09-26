// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The two rings' records (plan §3.1, §3.2; §13.3-8).
//!
//! Exactly two threads touch Binance: the **engine thread** (the arm,
//! [`crate::arm::BnArm`]) writes [`BnCmd`] and reads [`BnEvt`]; the
//! **gateway thread** ([`crate::gateway`]) reads [`BnCmd`] and writes
//! [`BnEvt`] — and fills, which cross on the engine's fill lane 4. Each
//! record is ONE cache line, `repr(C, align(64))`, `Copy`, no heap field
//! and no destructor: `core-ring`'s `try_push_ref` copies it into its
//! slot (the designed ring-slot copy) and the reader borrows it in place
//! through `try_pop_ref`.
//!
//! Nothing here allocates, and nothing here is `dyn`.

use core_types::Side;

/// The command ring's capacity (plan §3.1). A full ring refuses the
/// submit with `DispatchError::QueueFull` and the router books nothing.
pub const CMD_RING: usize = 1_024;
/// The event ring's capacity (plan §3.1).
pub const EVT_RING: usize = 1_024;

// -------------------------------------------------------------------------
// BnCmd — engine → gateway
// -------------------------------------------------------------------------

/// Place one order.
pub const VERB_PLACE: u8 = 0;
/// Cancel one resting order (makers only: IoC-only products never build
/// the verb, plan §3.8).
pub const VERB_CANCEL: u8 = 1;
/// Replace one resting order's price and quantity (UM `order.modify`).
pub const VERB_MODIFY: u8 = 2;
/// A halt's sweep: cancel every order of ours on every armed product and
/// report the result (O-BX8, plan §3.11 switch 1). Never a flatten (O-BX9).
pub const VERB_CANCEL_ALL: u8 = 3;
/// `Engine::stop`: the sweep of [`VERB_CANCEL_ALL`], then the gateway
/// leaves its loop.
pub const VERB_SHUTDOWN: u8 = 4;

/// **Engine → gateway, one per verb.** 64 B, one cache line (plan §3.2).
///
/// Prices and quantities are already on the row's tick and step when the
/// arm pushes (BX-5): the gateway renders digits and never rounds. The
/// plan's `flags` byte (@46: spot amend-eligible, options opens-short)
/// belongs to verbs that are not built (spot amend, BX8; options, BX9), so
/// it stays folded into the padding until a phase gives it a reader.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BnCmd {
    /// The member's id for this order (after a modify, the NEW id).
    pub client_oid: u64,
    /// [`VERB_MODIFY`]: the id being replaced. Otherwise 0.
    pub prev_client_oid: u64,
    /// Price ×1e6, on the tick.
    pub px_1e6: i64,
    /// Quantity ×1e6, on the step.
    pub qty_1e6: i64,
    /// `CLOCK_MONOTONIC_RAW` deadline of a maker's TTL (BX-13); 0 = none
    /// (every IoC).
    pub ttl_deadline_ns: u64,
    /// The instrument row ([`crate::inst`]).
    pub row: u16,
    /// `VERB_*`.
    pub verb: u8,
    /// `core_fill::ORDER_KIND_MAKER` / `ORDER_KIND_IOC`.
    pub kind: u8,
    /// `Side` as its `u8` (Bid 0, Ask 1).
    pub side: u8,
    /// The strategy slot (`strategy_id`).
    pub slot: u8,
    _pad: [u8; 18],
}

const _: () = assert!(core::mem::size_of::<BnCmd>() == 64);
const _: () = assert!(core::mem::align_of::<BnCmd>() == 64);

impl BnCmd {
    /// All zero: a `VERB_PLACE` of nothing. Only a starting value.
    pub const ZERO: Self = Self {
        client_oid: 0,
        prev_client_oid: 0,
        px_1e6: 0,
        qty_1e6: 0,
        ttl_deadline_ns: 0,
        row: 0,
        verb: VERB_PLACE,
        kind: 0,
        side: 0,
        slot: 0,
        _pad: [0; 18],
    };

    /// A [`VERB_PLACE`].
    #[inline(always)]
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn place(
        client_oid: u64,
        px_1e6: i64,
        qty_1e6: i64,
        ttl_deadline_ns: u64,
        row: u16,
        kind: u8,
        side: Side,
        slot: u8,
    ) -> Self {
        Self {
            client_oid,
            prev_client_oid: 0,
            px_1e6,
            qty_1e6,
            ttl_deadline_ns,
            row,
            verb: VERB_PLACE,
            kind,
            side: side as u8,
            slot,
            _pad: [0; 18],
        }
    }

    /// A [`VERB_CANCEL`] of the order the member knows as `client_oid`.
    #[inline(always)]
    #[must_use]
    pub const fn cancel(client_oid: u64, row: u16, slot: u8) -> Self {
        Self {
            client_oid,
            prev_client_oid: 0,
            px_1e6: 0,
            qty_1e6: 0,
            ttl_deadline_ns: 0,
            row,
            verb: VERB_CANCEL,
            kind: 0,
            side: 0,
            slot,
            _pad: [0; 18],
        }
    }

    /// A [`VERB_MODIFY`]: `prev_client_oid` becomes `client_oid` at a new
    /// price and quantity, with a new TTL deadline (0 keeps none).
    #[inline(always)]
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn modify(
        client_oid: u64,
        prev_client_oid: u64,
        px_1e6: i64,
        qty_1e6: i64,
        ttl_deadline_ns: u64,
        row: u16,
        side: Side,
        slot: u8,
    ) -> Self {
        Self {
            client_oid,
            prev_client_oid,
            px_1e6,
            qty_1e6,
            ttl_deadline_ns,
            row,
            verb: VERB_MODIFY,
            kind: core_fill::ORDER_KIND_MAKER,
            side: side as u8,
            slot,
            _pad: [0; 18],
        }
    }

    /// A [`VERB_CANCEL_ALL`] or [`VERB_SHUTDOWN`].
    #[inline(always)]
    #[must_use]
    pub const fn sweep(verb: u8) -> Self {
        let mut c = Self::ZERO;
        c.verb = verb;
        c
    }

    /// The side, decoded.
    #[inline(always)]
    #[must_use]
    pub const fn side(&self) -> Side {
        if self.side == Side::Ask as u8 {
            Side::Ask
        } else {
            Side::Bid
        }
    }
}

// -------------------------------------------------------------------------
// BnEvt — gateway → engine
// -------------------------------------------------------------------------

/// The venue accepted the order (the ACK, BX-2). `a` = venue `orderId`.
pub const EVT_ACK: u8 = 1;
/// The venue refused the order. `code` = the Binance code (negative) or
/// the HTTP status; `why` = `RETIRED_REJECTED`. The order is terminal.
pub const EVT_REJECT: u8 = 2;
/// The order ended without a further fill (F9). `why` = `RETIRED_*`;
/// `flags` bit [`EVT_F_HAD_FILL`] if any quantity filled. Pushed AFTER
/// every fill of the order is on lane 4 (BX3 obligation 1).
pub const EVT_RETIRED: u8 = 3;
/// A cancel the venue refused (`code`). The order stays counted until its
/// own terminal event arrives (BX3 obligation 2).
pub const EVT_CANCEL_FAILED: u8 = 4;
/// A modify the venue applied: `client_oid` is the NEW id, `a` the
/// replaced one, `b` the new quantity ×1e6, `c` the new price ×1e6.
pub const EVT_MODIFIED: u8 = 5;
/// A modify the venue refused (`code`): `client_oid` is the refused new
/// id, `a` the order's id, which keeps working unchanged.
pub const EVT_MODIFY_FAILED: u8 = 6;
/// The gateway's pulse, every 100 ms: `a` = the stream gap ns (the older
/// of the order session's and the user stream's last frame), `b` = the
/// measured clock offset ms, `c` = the in-doubt count; `flags` =
/// `EVT_F_ORDER_UP | EVT_F_USER_UP | EVT_F_DEADMAN_OK | EVT_F_CLOCK_OK`.
pub const EVT_STATUS: u8 = 7;
/// A reconciliation verdict (plan §3.10): `a` = drift USD ×1e6 (legs that
/// differed across two cycles), `b` = unseen venue legs, `c` = foreign
/// orders on owned instruments; `flags` bit [`EVT_F_RECONCILED`].
pub const EVT_RECON: u8 = 8;
/// A margin sample (BX-20): `product`, `a` = the ratio ×1e6
/// (maintenance / equity), `b` = the session equity USD ×1e6; `flags` bit
/// [`EVT_F_MARGIN_CALL`] when the venue sent `MARGIN_CALL`.
pub const EVT_MARGIN: u8 = 9;
/// A budget observation (§3.9: `-1015`, 429, `-1003`): `code`; `a` = the
/// monotonic ns until which the venue's window stays breached.
pub const EVT_BUDGET: u8 = 10;
/// A venue lock (418, `-4400`…`-4402`, `RISK_LEVEL_CHANGE → REDUCE_ONLY`,
/// a mid-session mode or permission change; BX-12, BX-19): `code`. Sticky
/// for the life of the boot.
pub const EVT_LOCK: u8 = 11;
/// A sweep's result (plan §3.11): `a` = our orders still open after the
/// confirmation query, `b` = cancels sent; `flags` bit [`EVT_F_SWEEP_DONE`]
/// when confirmed clear, [`EVT_F_SWEEP_STRANDED`] when it gave up.
pub const EVT_SWEEP: u8 = 12;
/// The day's turnover per slot, read from the venue (BX3 obligation 8):
/// `slot`, `a` = UTC day (`wall_ms / 86 400 000`), `b` = the increasing
/// part of the slot's fills today, USD ×1e6.
pub const EVT_DAY: u8 = 13;
/// A user-data or order-session frame that did not scan (BX-15): a halt
/// observation, never a skip. `code` = the scanner (`SCAN_*`).
pub const EVT_SCAN_FAIL: u8 = 14;
/// A row's net position changed sign or left zero (breadth, BX-7):
/// `row`, `a` = the net position ×1e6 the gateway has booked.
pub const EVT_POSITION: u8 = 15;

/// The gateway's cumulative tallies, three per event: `why` = the group
/// (0: fills booked, foreign, unowned; 1: fills deferred, unresolved, recon
/// ok; 2: recon failed, drift legs, unseen legs; 3: reconnects, connect
/// failures, sweep orders left), `a` / `b` / `c` = the values.
pub const EVT_TALLY: u8 = 16;

/// [`EVT_RETIRED`]: some quantity filled before the order ended.
pub const EVT_F_HAD_FILL: u8 = 1 << 0;
/// [`EVT_RETIRED`] / [`EVT_REJECT`]: the order was an IoC.
pub const EVT_F_IOC: u8 = 1 << 1;
/// [`EVT_RETIRED`]: a maker the member cancelled within 5 s of placement
/// (the UM ICR rule counts it).
pub const EVT_F_ICR: u8 = 1 << 2;
/// [`EVT_STATUS`]: the order session is logged on.
pub const EVT_F_ORDER_UP: u8 = 1 << 0;
/// [`EVT_STATUS`]: the user-data stream is open.
pub const EVT_F_USER_UP: u8 = 1 << 1;
/// [`EVT_STATUS`]: the venue dead-man (UM `countdownCancelAll`) answered
/// its last heartbeat in time (BX-17).
pub const EVT_F_DEADMAN_OK: u8 = 1 << 2;
/// [`EVT_STATUS`]: the clock offset is measured (BX-6).
pub const EVT_F_CLOCK_OK: u8 = 1 << 3;
/// [`EVT_STATUS`] flag: an output was lost (both rings and the held queue
/// full) — the arm raises unbounded drift.
pub const EVT_F_LOST: u8 = 1 << 4;
/// [`EVT_STATUS`] flag: the E7 session anchor is not persisted yet — the
/// journal ring has not taken it, or its store failed (S8, F2): a restart
/// now would re-anchor at its own equity. No member may arm while it is
/// raised; the arm reports it ([`crate::arm::BnArm::anchor_unsaved`]).
pub const EVT_F_ANCHOR_UNSAVED: u8 = 1 << 5;
/// [`EVT_RECON`]: drift and unseen are both zero (plan §3.10).
pub const EVT_F_RECONCILED: u8 = 1 << 0;
/// [`EVT_MARGIN`]: a `MARGIN_CALL` arrived.
pub const EVT_F_MARGIN_CALL: u8 = 1 << 0;
/// [`EVT_SWEEP`]: confirmed clear.
pub const EVT_F_SWEEP_DONE: u8 = 1 << 0;
/// [`EVT_SWEEP`]: gave up with orders left (`Stranded`).
pub const EVT_F_SWEEP_STRANDED: u8 = 1 << 1;

/// **Gateway → engine.** 64 B, one cache line. Field meanings per kind are
/// on the `EVT_*` constants.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BnEvt {
    /// Order events: the order's CURRENT member id. Otherwise 0.
    pub client_oid: u64,
    /// Kind-specific.
    pub a: i64,
    /// Kind-specific.
    pub b: i64,
    /// Kind-specific.
    pub c: i64,
    /// Gateway `CLOCK_MONOTONIC_RAW` at the observation.
    pub ts_ns: u64,
    /// The venue code (Binance negative codes) or HTTP status; 0 = none.
    pub code: i32,
    /// The instrument row, where one applies.
    pub row: u16,
    /// `EVT_*`.
    pub kind: u8,
    /// The strategy slot, where one applies.
    pub slot: u8,
    /// `RETIRED_*` for [`EVT_RETIRED`] / [`EVT_REJECT`].
    pub why: u8,
    /// `Side` as its `u8`, where one applies.
    pub side: u8,
    /// `PRODUCT_*`, where one applies.
    pub product: u8,
    /// `EVT_F_*`.
    pub flags: u8,
    _pad: [u8; 12],
}

const _: () = assert!(core::mem::size_of::<BnEvt>() == 64);
const _: () = assert!(core::mem::align_of::<BnEvt>() == 64);

impl BnEvt {
    /// All zero (kind 0 is no kind: the arm counts it unknown).
    pub const ZERO: Self = Self {
        client_oid: 0,
        a: 0,
        b: 0,
        c: 0,
        ts_ns: 0,
        code: 0,
        row: 0,
        kind: 0,
        slot: 0,
        why: 0,
        side: 0,
        product: 0,
        flags: 0,
        _pad: [0; 12],
    };

    /// An event of `kind` at `ts_ns`, every other field zero.
    #[inline(always)]
    #[must_use]
    pub const fn new(kind: u8, ts_ns: u64) -> Self {
        let mut e = Self::ZERO;
        e.kind = kind;
        e.ts_ns = ts_ns;
        e
    }

    /// An order event: `kind` for the order the member knows as
    /// `client_oid`.
    #[inline(always)]
    #[must_use]
    pub const fn order(kind: u8, ts_ns: u64, client_oid: u64, row: u16, slot: u8) -> Self {
        let mut e = Self::new(kind, ts_ns);
        e.client_oid = client_oid;
        e.row = row;
        e.slot = slot;
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_one_cache_line_with_the_planned_offsets() {
        assert_eq!(core::mem::offset_of!(BnCmd, client_oid), 0);
        assert_eq!(core::mem::offset_of!(BnCmd, prev_client_oid), 8);
        assert_eq!(core::mem::offset_of!(BnCmd, px_1e6), 16);
        assert_eq!(core::mem::offset_of!(BnCmd, qty_1e6), 24);
        assert_eq!(core::mem::offset_of!(BnCmd, ttl_deadline_ns), 32);
        assert_eq!(core::mem::offset_of!(BnCmd, row), 40);
        assert_eq!(core::mem::offset_of!(BnCmd, verb), 42);
        assert_eq!(core::mem::offset_of!(BnCmd, kind), 43);
        assert_eq!(core::mem::offset_of!(BnCmd, side), 44);
        assert_eq!(core::mem::offset_of!(BnCmd, slot), 45);
        assert_eq!(core::mem::offset_of!(BnEvt, ts_ns), 32);
        assert_eq!(core::mem::offset_of!(BnEvt, code), 40);
        assert_eq!(core::mem::offset_of!(BnEvt, row), 44);
        assert_eq!(core::mem::offset_of!(BnEvt, kind), 46);
        assert_eq!(core::mem::offset_of!(BnEvt, flags), 51);
    }

    #[test]
    fn constructors_stamp_the_verb_and_side() {
        let p = BnCmd::place(9, 100, 2, 7, 3, core_fill::ORDER_KIND_IOC, Side::Ask, 5);
        assert_eq!((p.verb, p.side(), p.slot, p.row), (VERB_PLACE, Side::Ask, 5, 3));
        let m = BnCmd::modify(10, 9, 101, 1, 0, 3, Side::Bid, 5);
        assert_eq!((m.verb, m.prev_client_oid, m.side()), (VERB_MODIFY, 9, Side::Bid));
        assert_eq!(m.kind, core_fill::ORDER_KIND_MAKER);
        assert_eq!(BnCmd::cancel(9, 3, 5).verb, VERB_CANCEL);
        assert_eq!(BnCmd::sweep(VERB_SHUTDOWN).verb, VERB_SHUTDOWN);
        let e = BnEvt::order(EVT_ACK, 1, 9, 3, 5);
        assert_eq!((e.kind, e.client_oid, e.row, e.slot), (EVT_ACK, 9, 3, 5));
    }
}
