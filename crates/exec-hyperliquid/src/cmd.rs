// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The engine → gateway command (XH4b): one 64 B cache line.
//!
//! The engine thread never touches a Hyperliquid socket for an order
//! (XMM law, plan §8.1). An arm validates and rounds locally, then
//! pushes ONE [`HlCmd`] into the command ring; the gateway thread
//! coalesces, encodes, signs and posts it. Everything the gateway needs
//! to render the item is in the command — nothing is looked up.
//!
//! `#[repr(C, align(64))]`, `Copy`, no heap field; size, alignment and
//! every field offset const-asserted below (a layout change is a wire
//! change between two threads of one process).

use core::mem::offset_of;

use crate::action::{CancelByCloidWire, ModifyWire, OrderWire, Tif};
use crate::cloid;

/// Place a new order.
pub const CMD_PLACE: u8 = 1;
/// Replace a resting order (LAW E-7: a requote is a modify).
pub const CMD_MODIFY: u8 = 2;
/// Cancel an order by its client id.
pub const CMD_CANCEL: u8 = 3;
/// Control (XH4c): `updateLeverage` for `asset` — `arg` × leverage,
/// cross when [`CMD_FLAG_CROSS`] (O-XH6, at boot).
pub const CMD_LEVERAGE: u8 = 4;
/// Control (XH4c): `scheduleCancel` at `time_ms` (venue wall ms); 0
/// clears it. The venue's dead-man switch, account-wide.
pub const CMD_SCHEDULE_CANCEL: u8 = 5;
/// Control (XH4c): `noop` — spends a nonce.
pub const CMD_NOOP: u8 = 6;
/// Read (XH4d): the account's resting orders, over the WebSocket post
/// channel (`frontendOpenOrders`); answered by `EV_OPEN_ORDER` rows and
/// `EV_READ_DONE`.
pub const CMD_READ_ORDERS: u8 = 7;
/// Read (XH4d): the account's perp sheet (`clearinghouseState`);
/// answered by `EV_POSITION` rows and `EV_READ_DONE`.
pub const CMD_READ_STATE: u8 = 8;
/// Read (XH4e): the address request budget (`userRateLimit`); answered by
/// `EV_READ_DONE` alone.
pub const CMD_READ_LIMIT: u8 = 9;
/// Read (XH4e): the account's fills from `time_ms` to `prev_client_oid`
/// (`userFillsByTime`'s `startTime` and `endTime`, venue wall ms), summed
/// in place for the command's slot; answered by `EV_READ_DONE` alone.
pub const CMD_READ_DAY: u8 = 10;
/// Read (XMM XH5b-1): the account's spot sheet (`spotClearinghouseState`)
/// — the USDC half of an account's worth; answered by `EV_READ_DONE`
/// alone. The last read: [`HlCmd::is_read`] ends here.
pub const CMD_READ_SPOT: u8 = 11;
/// Control (XMM XH5c): `reserveRequestWeight` — buy `arg` requests of
/// address budget (S7-L1's top-up, on the gateway arm). Numbered after the
/// reads: [`HlCmd::is_control`] names it.
pub const CMD_RESERVE: u8 = 12;

/// `flags`: reduce-only.
pub const CMD_FLAG_REDUCE_ONLY: u8 = 1;
/// `flags` ([`CMD_LEVERAGE`]): cross margin (vs isolated).
pub const CMD_FLAG_CROSS: u8 = 1 << 1;

/// One command. See the module docs.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct HlCmd {
    /// Engine monotonic ns at the push — the origin of the gateway's
    /// `push_to_write` histogram.
    pub ts_ns: u64,
    /// The order's client id (the low 8 bytes of its cloid, LAW E-9).
    pub client_oid: u64,
    /// [`CMD_MODIFY`]: the client id of the order being replaced.
    /// [`CMD_READ_DAY`]: the last fill time read (the read's end), venue
    /// wall ms.
    pub prev_client_oid: u64,
    /// Limit price, 1e8.
    pub px_1e8: i64,
    /// Size, 1e8.
    pub sz_1e8: i64,
    /// Venue asset id.
    pub asset: u32,
    /// Venue `expiresAfter`, ms after the signing instant; 0 = none. An
    /// action expires at the smallest non-zero offset among its items.
    pub expires_after_ms: u16,
    /// `CMD_*`.
    pub kind: u8,
    /// 1 = buy.
    pub is_buy: u8,
    /// [`Tif`] as its discriminant; the gateway carries ALO and IoC only.
    pub tif: u8,
    /// `CMD_FLAG_*`.
    pub flags: u8,
    /// Strategy slot: the cloid's third byte (LAW E-9).
    pub slot: u8,
    _pad: u8,
    /// [`CMD_LEVERAGE`]: the leverage, a whole number. [`CMD_RESERVE`]:
    /// the weight bought (XMM XH5c). [`CMD_PLACE`] of an ALO order (XMM
    /// XH5d): its ALO priority, p ×1e8 of resting notional (0 none) — the
    /// gateway batches places by it; a modify carries none.
    pub arg: u32,
    /// [`CMD_SCHEDULE_CANCEL`]: the venue wall time to cancel at, ms; 0
    /// clears the schedule. [`CMD_READ_DAY`]: the first fill time read,
    /// venue wall ms. [`CMD_PLACE`] (XH4f): the order's TTL, ms — the
    /// gateway cancels it that long after admitting it unless it ended
    /// first (`crate::ttl`); 0 = none. A modify inherits its original's.
    pub time_ms: u64,
}

const _: () = assert!(core::mem::size_of::<HlCmd>() == 64);
const _: () = assert!(core::mem::align_of::<HlCmd>() == 64);
const _: () = assert!(offset_of!(HlCmd, ts_ns) == 0);
const _: () = assert!(offset_of!(HlCmd, client_oid) == 8);
const _: () = assert!(offset_of!(HlCmd, prev_client_oid) == 16);
const _: () = assert!(offset_of!(HlCmd, px_1e8) == 24);
const _: () = assert!(offset_of!(HlCmd, sz_1e8) == 32);
const _: () = assert!(offset_of!(HlCmd, asset) == 40);
const _: () = assert!(offset_of!(HlCmd, expires_after_ms) == 44);
const _: () = assert!(offset_of!(HlCmd, kind) == 46);
const _: () = assert!(offset_of!(HlCmd, is_buy) == 47);
const _: () = assert!(offset_of!(HlCmd, tif) == 48);
const _: () = assert!(offset_of!(HlCmd, flags) == 49);
const _: () = assert!(offset_of!(HlCmd, slot) == 50);
const _: () = assert!(offset_of!(HlCmd, arg) == 52);
const _: () = assert!(offset_of!(HlCmd, time_ms) == 56);

impl HlCmd {
    /// Place `sz_1e8` at `px_1e8` on `asset` for `slot`, as `client_oid`.
    #[inline(always)]
    #[must_use]
    pub const fn place(
        slot: u8,
        client_oid: u64,
        asset: u32,
        is_buy: bool,
        px_1e8: i64,
        sz_1e8: i64,
        tif: Tif,
    ) -> Self {
        Self {
            ts_ns: 0,
            client_oid,
            prev_client_oid: 0,
            px_1e8,
            sz_1e8,
            asset,
            expires_after_ms: 0,
            kind: CMD_PLACE,
            is_buy: is_buy as u8,
            tif: tif as u8,
            flags: 0,
            slot,
            _pad: 0,
            arg: 0,
            time_ms: 0,
        }
    }

    /// The highest ALO priority a place may bid, ×1e8 of resting notional:
    /// 1 % (XMM XH5d — the xmm.toml parser's bound; the arm refuses more
    /// unsent).
    pub const PRIORITY_MAX_1E8: u32 = 1_000_000;

    /// XMM XH5d: this ALO place bidding priority `p_1e8` (p ×1e8 of its
    /// resting notional, carried in `arg`).
    #[inline(always)]
    #[must_use]
    pub const fn with_priority(mut self, p_1e8: u32) -> Self {
        self.arg = p_1e8;
        self
    }

    /// This order as a modify of the resting `prev_client_oid`.
    #[inline(always)]
    #[must_use]
    pub const fn replacing(mut self, prev_client_oid: u64) -> Self {
        self.set_replacing(prev_client_oid);
        self
    }

    /// [`Self::replacing`], in place: a modify of `prev_client_oid`.
    #[inline(always)]
    pub const fn set_replacing(&mut self, prev_client_oid: u64) {
        self.kind = CMD_MODIFY;
        self.prev_client_oid = prev_client_oid;
    }

    /// Cancel `client_oid` of `slot` on `asset`.
    #[inline(always)]
    #[must_use]
    pub const fn cancel(slot: u8, client_oid: u64, asset: u32) -> Self {
        let mut c = Self::place(slot, client_oid, asset, false, 0, 0, Tif::Gtc);
        c.kind = CMD_CANCEL;
        c
    }

    /// Control: set `asset`'s leverage to `leverage` ×, cross or
    /// isolated. `client_oid` only correlates the answer (it is not sent).
    #[inline(always)]
    #[must_use]
    pub const fn leverage(
        slot: u8,
        client_oid: u64,
        asset: u32,
        is_cross: bool,
        leverage: u32,
    ) -> Self {
        let mut c = Self::place(slot, client_oid, asset, false, 0, 0, Tif::Gtc);
        c.kind = CMD_LEVERAGE;
        c.arg = leverage;
        if is_cross {
            c.flags = CMD_FLAG_CROSS;
        }
        c
    }

    /// Control: schedule the account's cancel-all at `time_ms` (venue
    /// wall ms), or clear it with 0.
    #[inline(always)]
    #[must_use]
    pub const fn schedule_cancel(slot: u8, client_oid: u64, time_ms: u64) -> Self {
        let mut c = Self::place(slot, client_oid, 0, false, 0, 0, Tif::Gtc);
        c.kind = CMD_SCHEDULE_CANCEL;
        c.time_ms = time_ms;
        c
    }

    /// Control: a `noop` (spends a nonce).
    #[inline(always)]
    #[must_use]
    pub const fn noop(slot: u8, client_oid: u64) -> Self {
        let mut c = Self::place(slot, client_oid, 0, false, 0, 0, Tif::Gtc);
        c.kind = CMD_NOOP;
        c
    }

    /// Control (XMM XH5c): buy `weight` requests of address budget
    /// (`reserveRequestWeight`). `client_oid` only correlates the answer.
    #[inline(always)]
    #[must_use]
    pub const fn reserve(slot: u8, client_oid: u64, weight: u32) -> Self {
        let mut c = Self::place(slot, client_oid, 0, false, 0, 0, Tif::Gtc);
        c.kind = CMD_RESERVE;
        c.arg = weight;
        c
    }

    /// A control command (leverage, scheduleCancel, noop, a weight
    /// purchase): posted alone.
    #[inline(always)]
    #[must_use]
    pub const fn is_control(&self) -> bool {
        (self.kind >= CMD_LEVERAGE && self.kind <= CMD_NOOP) || self.kind == CMD_RESERVE
    }

    /// Read (XH4d): the account's resting orders. `client_oid` only
    /// correlates the answer.
    #[inline(always)]
    #[must_use]
    pub const fn read_orders(slot: u8, client_oid: u64) -> Self {
        let mut c = Self::place(slot, client_oid, 0, false, 0, 0, Tif::Gtc);
        c.kind = CMD_READ_ORDERS;
        c
    }

    /// Read (XH4d): the account's perp sheet.
    #[inline(always)]
    #[must_use]
    pub const fn read_state(slot: u8, client_oid: u64) -> Self {
        let mut c = Self::place(slot, client_oid, 0, false, 0, 0, Tif::Gtc);
        c.kind = CMD_READ_STATE;
        c
    }

    /// Read (XH4e): the address request budget.
    #[inline(always)]
    #[must_use]
    pub const fn read_limit(slot: u8, client_oid: u64) -> Self {
        let mut c = Self::place(slot, client_oid, 0, false, 0, 0, Tif::Gtc);
        c.kind = CMD_READ_LIMIT;
        c
    }

    /// Read (XH4e): the account's fills from `start_ms` to `end_ms` (venue
    /// wall ms, both inclusive), summed for `slot`.
    #[inline(always)]
    #[must_use]
    pub const fn read_day(slot: u8, client_oid: u64, start_ms: u64, end_ms: u64) -> Self {
        let mut c = Self::place(slot, client_oid, 0, false, 0, 0, Tif::Gtc);
        c.kind = CMD_READ_DAY;
        c.time_ms = start_ms;
        c.prev_client_oid = end_ms;
        c
    }

    /// Read (XMM XH5b-1): the account's spot sheet.
    #[inline(always)]
    #[must_use]
    pub const fn read_spot(slot: u8, client_oid: u64) -> Self {
        let mut c = Self::place(slot, client_oid, 0, false, 0, 0, Tif::Gtc);
        c.kind = CMD_READ_SPOT;
        c
    }

    /// A read (posted alone, unsigned, WebSocket only).
    #[inline(always)]
    #[must_use]
    pub const fn is_read(&self) -> bool {
        self.kind >= CMD_READ_ORDERS && self.kind <= CMD_READ_SPOT
    }

    /// Reduce-only.
    #[inline(always)]
    #[must_use]
    pub const fn reduce_only(mut self) -> Self {
        self.set_reduce_only();
        self
    }

    /// [`Self::reduce_only`], in place.
    #[inline(always)]
    pub const fn set_reduce_only(&mut self) {
        self.flags |= CMD_FLAG_REDUCE_ONLY;
    }

    /// Venue expiry, `ms` after signing.
    #[inline(always)]
    #[must_use]
    pub const fn expiring_after(mut self, ms: u16) -> Self {
        self.expires_after_ms = ms;
        self
    }

    /// Stamped with the push time.
    #[inline(always)]
    #[must_use]
    pub const fn at(mut self, ts_ns: u64) -> Self {
        self.ts_ns = ts_ns;
        self
    }

    /// The order's own cloid (LAW E-9).
    #[inline(always)]
    #[must_use]
    pub fn cloid(&self) -> [u8; 16] {
        cloid::encode(self.slot, self.client_oid)
    }

    /// The order item, as the encoders take it. The gateway admits only
    /// ALO and IoC (`gateway::class_of`) before it converts.
    #[inline]
    #[must_use]
    pub fn order_wire(&self) -> OrderWire {
        debug_assert!(
            self.tif <= Tif::Alo as u8,
            "an unknown tif reached the encoder"
        );
        let tif = match self.tif {
            1 => Tif::Ioc,
            2 => Tif::Alo,
            _ => Tif::Gtc,
        };
        let o = OrderWire::new(self.asset, self.is_buy != 0, self.px_1e8, self.sz_1e8, tif)
            .with_cloid(self.cloid());
        if self.flags & CMD_FLAG_REDUCE_ONLY != 0 {
            o.reduce_only()
        } else {
            o
        }
    }

    /// Write the modify item into `out` — the resting order named by its
    /// cloid, replaced by [`Self::order_wire`] (which carries the NEW
    /// cloid). Written in place: the item is 80 B.
    #[inline]
    pub fn modify_into(&self, out: &mut ModifyWire) {
        out.order = self.order_wire();
        out.oid = 0;
        out.oid_cloid = cloid::encode(self.slot, self.prev_client_oid);
        out.oid_is_cloid = true;
    }

    /// The cancel item.
    #[inline]
    #[must_use]
    pub fn cancel_wire(&self) -> CancelByCloidWire {
        CancelByCloidWire {
            asset: self.asset,
            cloid: self.cloid(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wires_carry_the_slot_cloids() {
        let c = HlCmd::place(6, 42, 3, true, 123_450_000_000, 1_000_000, Tif::Alo)
            .reduce_only()
            .at(9);
        assert_eq!(c.kind, CMD_PLACE);
        assert_eq!(c.ts_ns, 9);
        let o = c.order_wire();
        assert!(o.is_buy && o.reduce_only && o.has_cloid);
        assert_eq!(o.tif, Tif::Alo as u8);
        assert_eq!(o.cloid, cloid::encode(6, 42));

        let m = HlCmd::place(6, 43, 3, false, 123_460_000_000, 1_000_000, Tif::Alo).replacing(42);
        assert_eq!(m.kind, CMD_MODIFY);
        let mut w = ModifyWire {
            order: OrderWire::new(0, true, 0, 0, Tif::Gtc),
            oid: 7,
            oid_cloid: [0xFF; 16],
            oid_is_cloid: false,
        };
        m.modify_into(&mut w);
        assert!(w.oid_is_cloid);
        assert_eq!(w.oid, 0);
        assert_eq!(
            w.oid_cloid,
            cloid::encode(6, 42),
            "the modify names the RESTING cloid"
        );
        assert_eq!(
            w.order.cloid,
            cloid::encode(6, 43),
            "and carries the new one"
        );
        assert!(!w.order.is_buy);

        let x = HlCmd::cancel(6, 43, 3);
        assert_eq!(x.kind, CMD_CANCEL);
        assert_eq!(x.cancel_wire().cloid, cloid::encode(6, 43));
        assert_eq!(x.cancel_wire().asset, 3);
    }

    #[test]
    fn control_commands_carry_their_arguments() {
        let l = HlCmd::leverage(6, 9, 1, true, 3);
        assert_eq!(
            (l.kind, l.asset, l.arg, l.flags),
            (CMD_LEVERAGE, 1, 3, CMD_FLAG_CROSS)
        );
        assert!(l.is_control());
        let i = HlCmd::leverage(6, 9, 1, false, 10);
        assert_eq!(i.flags & CMD_FLAG_CROSS, 0);
        let s = HlCmd::schedule_cancel(6, 10, 1_789_000_005_000);
        assert_eq!(
            (s.kind, s.time_ms),
            (CMD_SCHEDULE_CANCEL, 1_789_000_005_000)
        );
        assert!(HlCmd::noop(6, 11).is_control());
        let r = HlCmd::reserve(6, 12, 10_000);
        assert_eq!((r.kind, r.arg), (CMD_RESERVE, 10_000));
        assert!(r.is_control() && !r.is_read());
        assert!(!HlCmd::cancel(6, 1, 0).is_control());
        assert!(!HlCmd::place(6, 1, 0, true, 1, 1, Tif::Alo).is_control());
    }

    #[test]
    fn the_tif_byte_matches_the_action_enum() {
        assert_eq!(Tif::Gtc as u8, 0);
        assert_eq!(Tif::Ioc as u8, 1);
        assert_eq!(Tif::Alo as u8, 2);
        let c = HlCmd::place(0, 1, 0, true, 1, 1, Tif::Ioc);
        assert_eq!(c.order_wire().tif, Tif::Ioc as u8);
        assert_eq!(c.expiring_after(750).expires_after_ms, 750);
    }

    /// XH5b-1: the spot sheet is the last read — every read kind is one,
    /// nothing past it.
    #[test]
    fn every_read_kind_is_a_read_and_none_past_the_spot_sheet() {
        let s = HlCmd::read_spot(6, 9);
        assert_eq!((s.kind, s.slot, s.client_oid), (CMD_READ_SPOT, 6, 9));
        assert!(s.is_read() && !s.is_control());
        assert!(HlCmd::read_orders(6, 1).is_read());
        assert!(HlCmd::read_state(6, 1).is_read());
        assert!(HlCmd::read_limit(6, 1).is_read());
        assert!(HlCmd::read_day(6, 1, 0, 0).is_read());
        let mut past = s;
        past.kind = CMD_READ_SPOT + 1;
        assert!(!past.is_read());
        assert!(!HlCmd::noop(6, 1).is_read());
        assert!(!HlCmd::cancel(6, 1, 0).is_read());
    }
}
