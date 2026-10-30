// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-Bp §3.10 **R6** (impl-08 §3.7, "the one journal"): the delivery journal — slot 8's
//! inputs as paper delivered them, one 32-byte [`PineDeliveryRec`] each, in a preallocated ring
//! the cli drains (`PineStrategy::delivery_view`) into `<run dir>/pine-delivery.bin`.
//!
//! Lead ruling (2026-10-17): a record at every sequencing-tape point — `tick` 0, `event` 1,
//! `fill` 2 — plus kind 3 when an order is cancelled or withheld (every `on_cancelled` the member
//! reports, `settle::report_cancelled`); written only with `--pine-tape`; no timer records
//! (slot 8 never fires a timer).
//!
//! Hot module (I4): one boxed ring sized at `on_start` (empty without `--pine-tape`), POD
//! stores, no allocation, no clock.
//!
//! Literal readings:
//! - pin:08-delivery-rec-in-member: the record type lives here, in the producer's crate (its
//!   consumer, the cli, depends on this crate; a `strategy-core` home would be a trait-crate
//!   change the lead ruled out for this lane).
//! - pin:08-delivery-ordinal: records are numbered by an implicit 0-based ordinal (the record
//!   carries no `seq`); [`view`] returns the ordinal of its first record, so a reader whose
//!   cursor is behind it sees the gap (`first − from`), never a hidden one.
//! - pin:08-delivery-event-seq: kind 1 carries the event's own `venue_seq` whatever its channel
//!   (§3.10 names a Trade's; an event recorded because a settle was pending carries its own).
//! - pin:08-delivery-fill-venue-zero: a `Fill` carries no venue byte; kind 2 and kind 3 record
//!   `venue` 0.
//! - pin:08-delivery-fill-fee-noted: kind 2's `aux` is the fee ×1e12 the host noted for THIS
//!   fill (`note_fill_fee`, R8; the oid must match), saturated to `i64`, as bits; 0 when none.
//! - pin:08-delivery-done-e-ns: kind 3 has no payload; its `ts_ns` is E (the member's event
//!   clock) at the report, its `seq` the engine client oid (`client_oid_base | oid`), its `sym`
//!   the script's chart symbol.

use crate::rt::Core;
use core::cell::Cell;
use core_types::{ChannelEvent, Fill, Tick};

/// One slot-8 input as paper delivered it (impl-Bp §3.10, verbatim). `#[repr(C)]`, 32 B.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct PineDeliveryRec {
    /// The payload's own `ts_ns` (ingress parse time for ticks and events; the fill's stamp).
    pub ts_ns: u64,
    /// `venue_seq` of a Trade, the `client_oid` of a fill / done, 0 for a tick.
    pub seq: u64,
    /// The engine symbol.
    pub sym: u32,
    /// 0 tick · 1 venue event · 2 fill · 3 order done · 4 timer ([`kind`]).
    pub kind: u8,
    /// The payload's venue byte (0 for a fill / done, pin:08-delivery-fill-venue-zero).
    pub venue: u8,
    /// Always zero.
    pub _pad: [u8; 2],
    /// The event's channel for kind 1; the fee ×1e12 the member was handed (R8), as `i64` bits,
    /// for kind 2; 0 otherwise.
    pub aux: u64,
}

const _: () = assert!(
    core::mem::size_of::<PineDeliveryRec>() == 32 && core::mem::align_of::<PineDeliveryRec>() == 8
);

/// [`PineDeliveryRec::kind`] values (impl-Bp §3.10).
pub mod kind {
    /// A `Tick` of a pine symbol.
    pub const TICK: u8 = 0;
    /// A `ChannelEvent` (a pine Trade, or any event while a settle was pending).
    pub const EVENT: u8 = 1;
    /// A `Fill` delivered (foreign and prior-boot ones included, as the sequencing tape).
    pub const FILL: u8 = 2;
    /// An order reported `on_cancelled` (cancelled or withheld).
    pub const DONE: u8 = 3;
    /// A timer — never written by slot 8 (lead ruling: no timer records).
    pub const TIMER: u8 = 4;
}

/// The ring and its cursors (in `Core`; empty without `--pine-tape`).
pub(crate) struct Journal {
    on: bool,
    ring: Box<[PineDeliveryRec]>,
    /// The next record's ordinal (first = 0).
    next: u64,
    /// The reader's cursor: the end of the last view (the member is single-threaded, P1).
    read: Cell<u64>,
    /// Records overwritten before any view returned them.
    dropped: u64,
}

impl Journal {
    /// Boot only (`rt::build`): `DELIVERY_CAP` records when `on`, empty otherwise.
    pub(crate) fn new(on: bool, ring: Box<[PineDeliveryRec]>) -> Self {
        debug_assert!(!on || ring.len() == crate::DELIVERY_CAP);
        Self {
            on,
            ring,
            next: 0,
            read: Cell::new(0),
            dropped: 0,
        }
    }

    /// Records overwritten before the reader took them.
    #[inline]
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[inline]
fn push(core: &mut Core, r: PineDeliveryRec) {
    let j = &mut core.delivery;
    if !j.on {
        return;
    }
    let s = j.next;
    if s.wrapping_sub(j.read.get()) >= crate::DELIVERY_CAP as u64 {
        j.dropped = j.dropped.wrapping_add(1);
    }
    let at = usize::try_from(s % crate::DELIVERY_CAP as u64).unwrap_or(0);
    // COPY: one 32 B record into its ring slot — the cli drains it off the decision path —
    // writing the input in place rejected: the input arrives by reference and is not this POD.
    j.ring[at] = r;
    j.next = s.wrapping_add(1);
}

/// Kind 0: a tick of a pine symbol (at the sequencing tape's `TICK` point).
#[inline]
pub(crate) fn tick(core: &mut Core, t: &Tick) {
    push(
        core,
        PineDeliveryRec {
            ts_ns: t.ts_ns,
            seq: 0,
            sym: t.sym,
            kind: kind::TICK,
            venue: t.venue,
            _pad: [0; 2],
            aux: 0,
        },
    );
}

/// Kind 1: a venue event (at the sequencing tape's `EVENT` point; pin:08-delivery-event-seq).
#[inline]
pub(crate) fn event(core: &mut Core, e: &ChannelEvent) {
    push(
        core,
        PineDeliveryRec {
            ts_ns: e.ts_ns,
            seq: e.venue_seq,
            sym: e.sym,
            kind: kind::EVENT,
            venue: e.venue,
            _pad: [0; 2],
            aux: u64::from(e.channel),
        },
    );
}

/// Kind 2: a fill delivered (at the sequencing tape's `FILL` point, before `fills::on_fill`
/// consumes the noted fee; pin:08-delivery-fill-fee-noted).
#[inline]
pub(crate) fn fill(core: &mut Core, f: &Fill) {
    let fee = match core.noted_fee {
        Some((oid, fee)) if oid == f.order_id => {
            let v = i64::try_from(fee).unwrap_or(if fee < 0 { i64::MIN } else { i64::MAX });
            u64::from_ne_bytes(v.to_ne_bytes())
        }
        _ => 0,
    };
    push(
        core,
        PineDeliveryRec {
            ts_ns: f.ts_ns,
            seq: f.order_id,
            sym: f.sym,
            kind: kind::FILL,
            venue: 0,
            _pad: [0; 2],
            aux: fee,
        },
    );
}

/// Kind 3: script `i` reported `oid` cancelled or withheld (pin:08-delivery-done-e-ns).
#[inline]
pub(crate) fn done(core: &mut Core, i: usize, oid: u32) {
    if !core.delivery.on {
        return;
    }
    let base = crate::bootrec::client_oid_base(core.epoch, u8::try_from(i).unwrap_or(u8::MAX));
    let r = PineDeliveryRec {
        ts_ns: core.e_ns,
        seq: base | u64::from(oid),
        sym: core.scripts[i].sym,
        kind: kind::DONE,
        venue: 0,
        _pad: [0; 2],
        aux: 0,
    };
    push(core, r);
}

/// The records with ordinal `>= from` still in the ring, oldest first, into `out`. Returns
/// `(ordinal of out[0], n)` (pin:08-delivery-ordinal). Cold; never allocates.
pub(crate) fn view(core: &Core, out: &mut [PineDeliveryRec], from: u64) -> (u64, u32) {
    let j = &core.delivery;
    if !j.on {
        return (from, 0);
    }
    let oldest = j.next.saturating_sub(crate::DELIVERY_CAP as u64);
    let first = from.max(oldest);
    let mut s = first;
    let mut n = 0usize;
    while s < j.next && n < out.len() {
        // COPY: one 32 B record out per row — the cli's drain owns its buffer.
        out[n] = j.ring[usize::try_from(s % crate::DELIVERY_CAP as u64).unwrap_or(0)];
        n += 1;
        s += 1;
    }
    if n > 0 {
        j.read.set(j.read.get().max(s));
    }
    (first, u32::try_from(n).unwrap_or(u32::MAX))
}
