// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The gateway → engine event (XH4b): one 64 B cache line.
//!
//! The gateway reports RAW VENUE FACTS and keeps no order state of its
//! own; the arm maps them onto the order-event lane and the fill lane
//! (XH4d). One event per fact:
//!
//! | kind | source | meaning |
//! |---|---|---|
//! | [`EV_ACK`] | a post reply item | accepted; `flags` says resting / filled (`oid`, and `px`/`sz` = average price / total size when filled) |
//! | [`EV_REJECTED`] | a post reply item, `orderUpdates`, or the gateway | refused, `reason` says why; [`REASON_HALTED`], [`REASON_NOT_READY`], [`REASON_THROTTLED`] and [`REASON_LOCAL`] never left the host, and neither did a REST send refused with [`REASON_TRANSPORT`] (it failed before the request left) |
//! | [`EV_UNANSWERED`] | the gateway | sent and not settled — a timeout, a socket that died under it, an answer that does not scan or does not fit, the venue API layer's error, a refused re-send: LAW E-5 "sent, unanswered", in doubt, never re-signed; `reason` carries what the venue said, if anything |
//! | [`EV_RESTING`] / [`EV_CANCELED`] / [`EV_FILLED`] | `orderUpdates` | the order's status changed (`sz` = remaining size) |
//! | [`EV_FILL`] + [`EV_FILL_META`] | `userFills` | one trade, then its fee (signed, rebates negative) and the crossed flag; `aux` = tid on both |
//! | [`EV_SNAPSHOT`] | `userFills` | the process's first snapshot was absorbed as history (`aux` = rows, `venue_ms` = its latest fill): the arm reads its boot reconciliation after it |
//! | [`EV_GAP`] | the gateway | a stream may have lost rows (`aux` = `GAP_*`): the arm re-checks its open orders |
//! | [`EV_HALT`] | the gateway | the gateway halted (`aux` = the halt reason) |
//! | [`EV_OPEN_ORDER`] | a read (XH4d) | one resting order of the account: `asset`, `oid`, and `slot` / `client_oid` from its cloid ([`SLOT_FOREIGN`] when not ours) |
//! | [`EV_POSITION`] | a read (XH4d) | one perp position: `asset`, `sz_1e8` = signed size, `aux` = its value at the venue's mark (USD ×1e8) |
//! | [`EV_BALANCE`] | a spot read (XH4i-2) | one outcome leg the account holds, on a gateway whose map names outcome legs: `asset` (its `+<enc>`, or [`ASSET_UNKNOWN`] when the name does not parse), `sz_1e8` = the holding (`total`), `px_1e8` = the venue's `entryNtl` (USD ×1e8, raw — `recon::leg_cost_1e6` values it); the read's end then counts the rows in `oid` and carries [`SPOT_LEGS_READ`] |
//! | [`EV_READ_DONE`] / [`EV_READ_FAILED`] | a read (XH4d, XH4e, XH5b-1) | the read's end (`reason` = [`READ_ORDERS`] / [`READ_STATE`] / [`READ_LIMIT`] / [`READ_DAY`] / [`READ_SPOT`]; `aux` = rows, `px_1e8` = the account value and `venue_ms` its time for a state read; the budget, day and spot reads' fields at their constants — a day page's `sz_1e8` counts the rows whose time ran backward) or its failure; `client_oid` / `slot` are the read command's |
//!
//! A post reply's `filled` is an [`EV_ACK`], never [`EV_FILLED`]: the
//! venue's `filled` status arrives on `orderUpdates` and the trades on
//! `userFills`, two channels with no order between them, and the arm
//! joins them (XH4d). A post-only cross arrives twice — the reply item
//! and a `badAloPxRejected` row — and XH-7's counter counts the REPLY
//! items only.
//!
//! `#[repr(C, align(64))]`, `Copy`, no heap field; size, alignment and
//! every field offset const-asserted below.

use core::mem::offset_of;

use core_types::{
    ORDER_EVENT_REASON_BAD_ALO_PX, ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_EVENT_REASON_EXPIRED,
    ORDER_EVENT_REASON_MARGIN, ORDER_EVENT_REASON_MIN_NOTIONAL, ORDER_EVENT_REASON_NONE,
    ORDER_EVENT_REASON_OPEN_INTEREST_CAP, ORDER_EVENT_REASON_OTHER, ORDER_EVENT_REASON_REDUCE_ONLY,
    ORDER_EVENT_REASON_REPLACED, ORDER_EVENT_REASON_SCHEDULED_CANCEL,
    ORDER_EVENT_REASON_SELF_TRADE, ORDER_EVENT_REASON_TICK_OR_LOT,
};

use crate::response::find;

/// Post reply item: accepted.
pub const EV_ACK: u8 = 1;
/// `orderUpdates`: the order rests.
pub const EV_RESTING: u8 = 2;
/// `orderUpdates`: the order is filled out.
pub const EV_FILLED: u8 = 3;
/// `orderUpdates`: the order is cancelled (`reason` says by what).
pub const EV_CANCELED: u8 = 4;
/// Refused (`reason`).
pub const EV_REJECTED: u8 = 5;
/// `userFills`: one trade.
pub const EV_FILL: u8 = 6;
/// `userFills`: the trade's fee (`px_1e8` = fee, signed) and crossed flag.
pub const EV_FILL_META: u8 = 7;
/// Sent, not settled (LAW E-5: in doubt).
pub const EV_UNANSWERED: u8 = 8;
/// The gateway halted (`aux` = reason).
pub const EV_HALT: u8 = 9;
/// The first `userFills` snapshot was absorbed as history (`aux` = rows,
/// `venue_ms` = the latest row's time).
pub const EV_SNAPSHOT: u8 = 10;
/// A stream may have lost rows (`aux` = `GAP_*`).
pub const EV_GAP: u8 = 11;
/// A read's row: one resting order of the account (XH4d).
pub const EV_OPEN_ORDER: u8 = 12;
/// A read's row: one perp position (XH4d).
pub const EV_POSITION: u8 = 13;
/// A read answered in full; its rows came before it (XH4d).
pub const EV_READ_DONE: u8 = 14;
/// A read that did not answer, or answered what does not scan: no rows
/// of it count (XH4d).
pub const EV_READ_FAILED: u8 = 15;
/// A read's row (XH4i-2): one outcome leg the account holds, from a spot
/// read on a gateway whose map names outcome legs — `asset` (its
/// `+<enc>`, or [`ASSET_UNKNOWN`] when the name does not parse), `sz_1e8`
/// = the holding (`total`), `px_1e8` = the venue's `entryNtl` (USD ×1e8,
/// raw: `recon::leg_cost_1e6` values it). A settled husk (a zero holding)
/// and any other token are not rows.
pub const EV_BALANCE: u8 = 16;

/// A read's `reason`: the account's resting orders (`frontendOpenOrders`).
pub const READ_ORDERS: u8 = 1;
/// A read's `reason`: the account's perp sheet (`clearinghouseState`).
pub const READ_STATE: u8 = 2;
/// A read's `reason` (XH4e): the address request budget
/// (`userRateLimit`) — `aux` = `nRequestsUsed`, `px_1e8` = `cumVlm` (USD
/// ×1e8), `sz_1e8` = `nRequestsSurplus`.
pub const READ_LIMIT: u8 = 3;
/// A read's `reason` (XH4e): a page of the account's fills
/// (`userFillsByTime`) — `aux` = rows, `px_1e8` = Σ |px × sz| of the read
/// slot's rows (USD ×1e8), `venue_ms` = the latest row's time.
pub const READ_DAY: u8 = 4;
/// A read's `reason` (XMM XH5b-1): the account's spot sheet
/// (`spotClearinghouseState`) — `aux` = rows, `px_1e8` = the USDC `total`
/// (USD ×1e8), `sz_1e8` = 1 when the sheet lists USDC (0, and `px_1e8` 0,
/// when it lists none). On a gateway whose map names outcome legs
/// (XH4i-2) its [`EV_BALANCE`] rows come first, `oid` counts them and
/// `asset` is [`SPOT_LEGS_READ`].
pub const READ_SPOT: u8 = 5;

/// [`READ_SPOT`]'s end from a gateway whose map names outcome legs
/// (XH4i-2): its `asset`, the HIP-4 id base — the mark of a sheet read
/// for legs. A perps-only gateway's end carries 0, and an outcome arm
/// refuses it: no row could have come, legs held or not. A mark, not a
/// leg: it equals leg `#0`'s id, so a consumer reads it only on an end
/// whose kind and reason say so.
pub const SPOT_LEGS_READ: u32 = crate::asset::ASSET_BASE;

/// [`EV_GAP`]: a `userFills` frame did not scan.
pub const GAP_FILLS_UNREADABLE: u64 = 1;
/// [`EV_GAP`]: an `orderUpdates` frame did not scan.
pub const GAP_UPDATES_UNREADABLE: u64 = 2;
/// [`EV_GAP`]: a socket came up after a spell with none live, or replaced
/// one that had gone silent; `orderUpdates` has no snapshot, so the rows
/// pushed meanwhile are lost.
pub const GAP_RECONNECT: u64 = 3;

/// `flags`: buy side.
pub const EVF_BUY: u8 = 1;
/// `flags`: the fill crossed the spread (taker).
pub const EVF_CROSSED: u8 = 1 << 1;
/// `flags`: the fill came in a `userFills` snapshot (a reconnect's).
pub const EVF_SNAPSHOT: u8 = 1 << 2;
/// `flags`: the event is about an item of a post — its answer, its
/// refusal, or its being unanswered — not a stream row.
pub const EVF_POST_ITEM: u8 = 1 << 3;
/// `flags` bits 4–5: the item's command kind (`CMD_*`: place, modify,
/// cancel), post items only; 0 for a control post's item (leverage,
/// scheduleCancel, noop — the arm correlates those by client id).
pub const EVF_CMD_SHIFT: u8 = 4;
/// `flags`: the reply said the order rests.
pub const EVF_RESTING: u8 = 1 << 6;
/// `flags`: the reply said the order filled.
pub const EVF_FILLED: u8 = 1 << 7;

/// The venue's IoC "could not immediately match": not a refusal.
pub const REASON_IOC_MISS: u8 = 12;
/// A cancel's order was never placed, already cancelled, or filled.
pub const REASON_NOT_FOUND: u8 = 13;
/// The venue's rate limit.
pub const REASON_RATE_LIMIT: u8 = 14;
/// Timeout, socket death after the write, a REST transport failure.
pub const REASON_TRANSPORT: u8 = 15;
/// The answer did not scan, or did not fit its post.
pub const REASON_MALFORMED: u8 = 16;
/// The gateway is halted: a growing command is refused unsent.
pub const REASON_HALTED: u8 = 17;
/// Refused before the socket: a slot this gateway does not serve, a kind
/// or time-in-force it does not carry, the encoder, the signer, the clock.
pub const REASON_LOCAL: u8 = 18;
/// [`EV_FILL`]: the row is the venue SETTLING an outcome instance
/// (HIP-4, `dir: "Settlement"`), not a trade.
pub const REASON_SETTLEMENT: u8 = 19;
/// A growing command before the first `userFills` snapshot was absorbed.
pub const REASON_NOT_READY: u8 = 20;
/// XH4e: a growing command refused unsent — the gateway runs at 80 % of
/// its share of the venue's IP weight or WebSocket messages a minute.
pub const REASON_THROTTLED: u8 = 21;

/// A refusal the gateway gave before the post left the host — its own
/// (slot, class, halt, snapshot gate, rate shares) or a REST send that
/// failed before the request left: no venue reject, and no address
/// request (XH4e: the arm's budget takes it back). One rule for the arm
/// and the dead man.
#[inline(always)]
#[must_use]
pub(crate) const fn never_left(reason: u8) -> bool {
    matches!(
        reason,
        REASON_LOCAL | REASON_HALTED | REASON_NOT_READY | REASON_THROTTLED | REASON_TRANSPORT
    )
}

// The gateway's reasons share the byte with core-types' order-event
// reasons: those must stay below 12 (or be 255).
const _: () = {
    let core = [
        ORDER_EVENT_REASON_NONE,
        ORDER_EVENT_REASON_CANCEL_REQUESTED,
        ORDER_EVENT_REASON_REPLACED,
        ORDER_EVENT_REASON_EXPIRED,
        ORDER_EVENT_REASON_BAD_ALO_PX,
        ORDER_EVENT_REASON_SELF_TRADE,
        ORDER_EVENT_REASON_MARGIN,
        ORDER_EVENT_REASON_OPEN_INTEREST_CAP,
        ORDER_EVENT_REASON_SCHEDULED_CANCEL,
        ORDER_EVENT_REASON_REDUCE_ONLY,
        ORDER_EVENT_REASON_TICK_OR_LOT,
        ORDER_EVENT_REASON_MIN_NOTIONAL,
    ];
    let mut i = 0usize;
    while i < core.len() {
        assert!(core[i] < REASON_IOC_MISS, "a core-types reason collides");
        i += 1;
    }
    assert!(ORDER_EVENT_REASON_OTHER == 255);
};

/// `slot` of a row that carries no cloid of ours.
pub const SLOT_FOREIGN: u8 = 0xFF;
/// `asset` when the coin is not in the gateway's table.
pub const ASSET_UNKNOWN: u32 = u32::MAX;

/// One event. See the module docs.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct ExecEvent {
    /// Gateway monotonic ns at emission.
    pub ts_ns: u64,
    /// The venue's timestamp, ms (fill time, status time); 0 = none.
    pub venue_ms: u64,
    /// The client id from our cloid; 0 = none.
    pub client_oid: u64,
    /// The venue order id; 0 = none. A read's end: see its `READ_*`
    /// ([`READ_SPOT`]: the [`EV_BALANCE`] rows before it).
    pub oid: u64,
    /// Price, 1e8 (fill, limit, or average fill); [`EV_FILL_META`]: the
    /// fee, 1e8, signed (a rebate is negative).
    pub px_1e8: i64,
    /// Size, 1e8 (fill, remaining, or total filled).
    pub sz_1e8: i64,
    /// Tid ([`EV_FILL`], [`EV_FILL_META`]); post id (post items; 0 for a
    /// command refused before it had one); halt reason ([`EV_HALT`]); rows
    /// ([`EV_SNAPSHOT`]); `GAP_*` ([`EV_GAP`]).
    pub aux: u64,
    /// Venue asset id, or [`ASSET_UNKNOWN`]. A read's end: see its
    /// `READ_*` ([`READ_SPOT`]: [`SPOT_LEGS_READ`], a mark, not a leg).
    pub asset: u32,
    /// `EV_*`.
    pub kind: u8,
    /// `ORDER_EVENT_REASON_*` (core-types) or `REASON_*` (this module).
    pub reason: u8,
    /// Strategy slot from the cloid, or [`SLOT_FOREIGN`].
    pub slot: u8,
    /// `EVF_*`.
    pub flags: u8,
}

const _: () = assert!(core::mem::size_of::<ExecEvent>() == 64);
const _: () = assert!(core::mem::align_of::<ExecEvent>() == 64);
const _: () = assert!(offset_of!(ExecEvent, ts_ns) == 0);
const _: () = assert!(offset_of!(ExecEvent, venue_ms) == 8);
const _: () = assert!(offset_of!(ExecEvent, client_oid) == 16);
const _: () = assert!(offset_of!(ExecEvent, oid) == 24);
const _: () = assert!(offset_of!(ExecEvent, px_1e8) == 32);
const _: () = assert!(offset_of!(ExecEvent, sz_1e8) == 40);
const _: () = assert!(offset_of!(ExecEvent, aux) == 48);
const _: () = assert!(offset_of!(ExecEvent, asset) == 56);
const _: () = assert!(offset_of!(ExecEvent, kind) == 60);
const _: () = assert!(offset_of!(ExecEvent, reason) == 61);
const _: () = assert!(offset_of!(ExecEvent, slot) == 62);
const _: () = assert!(offset_of!(ExecEvent, flags) == 63);

impl ExecEvent {
    /// The post item's command kind (`CMD_*`: place, modify, cancel), or
    /// 0 for a stream event or a control post's item.
    #[inline(always)]
    #[must_use]
    pub const fn cmd_kind(&self) -> u8 {
        (self.flags >> EVF_CMD_SHIFT) & 3
    }
}

/// The reason for a venue error message, by the venue's own wording
/// (a prefix or a phrase it has used verbatim; anything else is
/// `ORDER_EVENT_REASON_OTHER`). Bytes, not a decoded string: the message
/// is a span of the reply, scanned in place.
#[must_use]
pub fn reason_of_error(msg: &[u8]) -> u8 {
    const TABLE: [(&[u8], u8); 11] = [
        (
            b"Post only order would have immediately matched",
            ORDER_EVENT_REASON_BAD_ALO_PX,
        ),
        (b"Order could not immediately match", REASON_IOC_MISS),
        (
            b"never placed, already canceled, or filled",
            REASON_NOT_FOUND,
        ),
        (b"Too many cumulative requests", REASON_RATE_LIMIT),
        (b"rate limit", REASON_RATE_LIMIT),
        (b"minimum value of $10", ORDER_EVENT_REASON_MIN_NOTIONAL),
        (b"divisible by tick size", ORDER_EVENT_REASON_TICK_OR_LOT),
        (b"invalid size", ORDER_EVENT_REASON_TICK_OR_LOT),
        (b"Insufficient margin", ORDER_EVENT_REASON_MARGIN),
        (
            b"Reduce only order would increase position",
            ORDER_EVENT_REASON_REDUCE_ONLY,
        ),
        (b"open interest", ORDER_EVENT_REASON_OPEN_INTEREST_CAP),
    ];
    let mut i = 0usize;
    while i < TABLE.len() {
        if find(msg, TABLE[i].0, 0).is_some() {
            return TABLE[i].1;
        }
        i += 1;
    }
    ORDER_EVENT_REASON_OTHER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_kind_rides_bits_four_and_five() {
        let e = ExecEvent {
            flags: EVF_POST_ITEM | (2 << EVF_CMD_SHIFT) | EVF_RESTING,
            ..ExecEvent::default()
        };
        assert_eq!(e.cmd_kind(), 2);
        assert_eq!(ExecEvent::default().cmd_kind(), 0);
    }

    /// XH4e: one rule for the arm's refund and the dead man's retry — the
    /// gateway's own refusals and a REST send that failed before it left
    /// never left the host; no other reason byte is one of them.
    #[test]
    fn never_left_holds_for_the_gateway_s_refusals_only() {
        let unsent = [
            REASON_LOCAL,
            REASON_HALTED,
            REASON_NOT_READY,
            REASON_THROTTLED,
            REASON_TRANSPORT,
        ];
        let mut i = 0;
        while i < unsent.len() {
            assert!(never_left(unsent[i]), "reason {}", unsent[i]);
            i += 1;
        }
        let mut n = 0u32;
        let mut r = 0u16;
        while r <= u16::from(u8::MAX) {
            n += u32::from(never_left(r as u8));
            r += 1;
        }
        assert_eq!(n, 5, "those five of the 256 reason bytes, no other");
    }

    #[test]
    fn venue_errors_map_to_reasons_by_their_wording() {
        let cases: [(&[u8], u8); 10] = [
            (
                b"Post only order would have immediately matched, bbo was 100@101. asset=0",
                ORDER_EVENT_REASON_BAD_ALO_PX,
            ),
            (
                b"Order could not immediately match against any resting orders. asset=0",
                REASON_IOC_MISS,
            ),
            (
                b"Order was never placed, already canceled, or filled. asset=0",
                REASON_NOT_FOUND,
            ),
            (
                b"Too many cumulative requests sent (1301 > 1200) for cumulative volume traded",
                REASON_RATE_LIMIT,
            ),
            (
                b"Order must have minimum value of $10. asset=0",
                ORDER_EVENT_REASON_MIN_NOTIONAL,
            ),
            (
                b"Price must be divisible by tick size. asset=0",
                ORDER_EVENT_REASON_TICK_OR_LOT,
            ),
            (b"Order has invalid size.", ORDER_EVENT_REASON_TICK_OR_LOT),
            (
                b"Insufficient margin to place order. asset=0",
                ORDER_EVENT_REASON_MARGIN,
            ),
            (
                b"Reduce only order would increase position. asset=0",
                ORDER_EVENT_REASON_REDUCE_ONLY,
            ),
            (b"something the venue never said", ORDER_EVENT_REASON_OTHER),
        ];
        let mut i = 0usize;
        while i < cases.len() {
            assert_eq!(reason_of_error(cases[i].0), cases[i].1, "case {i}");
            i += 1;
        }
        assert_eq!(reason_of_error(b""), ORDER_EVENT_REASON_OTHER);
    }
}
