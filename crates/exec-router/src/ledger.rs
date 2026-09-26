// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **E6 — what the venue actually did, as the router sees it.**
//!
//! E6 commit 1 made `max_order_usd` stop being decoration. It could,
//! because a single order's notional is entirely contained in the
//! request: nothing has to be remembered to judge it. The other three
//! clamps are not like that. `cap_instance_usd`, `cap_day_usd` and
//! `max_open_orders` all ask a question about the PAST, and the router
//! had no memory of one.
//!
//! This module is that memory. It is fed by
//! [`clob_dispatcher::OrderDispatch::on_fill_booked`] and by the
//! `InstrumentRoll` events the router already forwards, and it is read
//! by `RoutedDispatcher::risk_check` — on the engine thread, by the
//! same object that refuses, with no cross-arm reach and no lock.
//!
//! ## Three ledgers, deliberately not one
//!
//! | Ledger | Question | Fed by | Reset |
//! |---|---|---|---|
//! | exposure | how much is at stake right now | fills | the instance's roll |
//! | turnover | how much has been bought today | fills | 00:00Z |
//! | resting | how many orders are working | submit/cancel/modify **and** fills | the instance's roll |
//!
//! **The first two are computed from fills ALONE and never consult
//! the resting table.** That separation is load-bearing rather than
//! tidy: the resting table is the only part of this module that has to
//! match orders to fills by `client_oid`, which is the one place a key
//! can be ambiguous (see [`Ledger::book_fill`]). Keeping exposure and
//! turnover off that path means a defect in open-order tracking
//! degrades `max_open_orders` loudly and cannot silently corrupt the
//! number the money caps are judged against.
//!
//! ## Why exposure nets per outcome and then SUMS
//!
//! HIP-4 pays $1 to one leg of an outcome and $0 to the other, so
//! holding equal Yes and No is riskless collateral and nets to nothing
//! — [`core_types::net_exposure_1e8`], the reconciler's own rule and
//! the only copy of it.
//!
//! Two DIFFERENT outcomes do not net. They settle on independent
//! events, and a member long Yes on one and long No on another is
//! exposed to both. So the slot's number is
//! `Σ over outcomes |yes − no|`, never `|Σ yes − Σ no|` — the latter
//! would let two unrelated positions cancel each other and report a
//! flat book over real risk. `slot_exposure_1e6` is the only place
//! that sum is taken, and
//! `two_outcomes_do_not_net_against_each_other` is what holds it.
//!
//! ## Scale
//!
//! Everything here is ENGINE scale: quantities ×1e6 contracts, money
//! ×1e6 USD. A contract settles at $0 or $1, so a net exposure of
//! `n` contracts ×1e6 is exactly `n` USD ×1e6 — the conversion is the
//! identity, which is why there is no multiply and no rounding in
//! [`Ledger::slot_exposure_1e6`]. Stated rather than assumed, because
//! an identity nobody wrote down is an identity somebody will later
//! "fix".
//!
//! ## Zero allocation
//!
//! Fixed arrays, no `Vec`, no `dyn`, no iterator adaptors on the fill
//! path. Every table has a hard bound and refuses past it into a
//! counter, because a table that silently drops the 17th outcome is a
//! risk gate that stops seeing the position it was built to stop.

use crate::route::EXEC_SLOTS;
use core_time::WallAnchor;
use core_types::{net_exposure_1e8, Fill, SymbolId, FILL_ORIGIN_VENUE, SYMBOL_ID_NONE};

/// Rows the ledger can hold, **one per FAMILY**.
///
/// ## Why family and not outcome
///
/// Because an outcome id names an INSTANCE, not a market. A BIN15
/// family rolls every fifteen minutes, and
/// `ingress_hyperliquid::run_loop::perform_roll` rebinds the family to
/// a brand-new `HlOutcomeSpec` each time — new outcome id, new coins,
/// new symbols. The FAMILY INDEX is what survives; the outcome id is
/// what changes.
///
/// A table keyed on outcome id would therefore consume a fresh row
/// every roll and be full within two of them, after which every bind
/// is refused, every fill lands as
/// [`LedgerCounters::fills_unbound`], and `cap_instance` silently
/// stops seeing the position it exists to bound — a money clamp
/// failing OPEN about half an hour into a live boot. Keyed on family,
/// a roll REPLACES the row it already owns and the table never grows.
///
/// `ingress_hyperliquid::family::HL_MAX_FAMILIES` and
/// `strategy_bin15::BIN15_MAX_FAMILIES` are both 8, and the family
/// index in the roll seq is the INGRESS's — one table shared by every
/// member, not a per-member ordering. 16 is that doubled, so a second
/// venue's family table could arrive without evicting the first's.
/// Past this a bind is refused into
/// [`LedgerCounters::binds_refused`] rather than overwriting a live
/// row, for the reason `AssetTable` refuses rather than evicts: an
/// overwritten binding books a real position against the wrong
/// market.
pub const LEDGER_ROWS: usize = 16;

/// Live-arm orders the resting table can hold at once.
///
/// `exec.toml`'s shipped `max_open_orders` is 64 per slot and
/// [`EXEC_SLOTS`] is 8, so 512 is exactly "every slot at its cap".
/// Sized to the clamp rather than above it so that the table filling
/// up is not reachable while the clamp is doing its job — if
/// [`LedgerCounters::resting_full`] is ever non-zero, either the
/// clamps are unset or something is not retiring orders, and both are
/// operator-visible facts rather than silent truncation.
pub const LEDGER_RESTING: usize = 512;

/// Nanoseconds in a day. The day cap's epoch, matching
/// `strategy_bin15`'s own `DAY_NS` so the two roll at the same
/// instant — a second opinion that rolled an hour late would read as
/// a disagreement every midnight.
const DAY_NS: u64 = 86_400_000_000_000;

// -----------------------------------------------------------------
// BX3 — instrument rows (plan §3.5, D8, O-BX21, O-BX22)
// -----------------------------------------------------------------

/// **BX3 — instrument rows the ledger can hold**, one per venue
/// instrument bound at boot (Binance spot, USDⓈ-M, COIN-M, options),
/// beside the family rows. The armed set of every product fits; a boot
/// that would bind more refuses (BX6's duty, `InstrumentBindErr::Full`).
pub const LEDGER_INSTRUMENTS: usize = 256;
// `inst_find` masks its index with `LEDGER_INSTRUMENTS - 1`.
const _: () = assert!(LEDGER_INSTRUMENTS.is_power_of_two());

/// Instrument law: spot, bStock, equity. Base units ×1e6, never short
/// (no margin): a sell past the holding is refused.
pub const LAW_SPOT: u8 = 1;
/// Instrument law: a linear perpetual, dated future or TradFi perp. Base
/// units ×1e6, signed.
pub const LAW_LINEAR: u8 = 2;
/// Instrument law: an inverse (COIN-M) contract. CONTRACTS ×1e6,
/// signed; exposure is contracts × the contract's USD face, whatever the
/// price.
pub const LAW_INVERSE: u8 = 3;
/// Instrument law: a European option. CONTRACTS ×1e6, signed; a short
/// only where the venue lets this account write it.
pub const LAW_OPTION: u8 = 4;

/// Instrument flag: an option this account may write (`nakedSell`).
pub const INST_WRITABLE: u8 = 1 << 0;
/// Instrument flag: a call (a put otherwise). Options only.
pub const INST_CALL: u8 = 1 << 1;

/// **One row's exposure for one slot is clamped here**, USD ×1e6 (about
/// $18 bn). Every sum over [`LEDGER_INSTRUMENTS`] rows then fits an
/// `i64` exactly, so the per-slot aggregate is kept by exact adds and
/// subtracts and cannot drift from a full recompute. No cap is near it,
/// so a clamped row still refuses whatever a true one would.
pub const INST_EXPOSURE_MAX: i64 = i64::MAX / 512;

/// What the boot binds one instrument row from (BX6: from discovery).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InstrumentSpec {
    /// The engine id the instrument's orders and fills carry.
    pub sym: SymbolId,
    /// `LAW_*`.
    pub law: u8,
    /// `INST_*` (options only).
    pub flags: u8,
    _pad: [u8; 2],
    /// Option: underlying units per contract ×1e6 (the eapi `unit`).
    /// Inverse: the contract's USD face ×1e6 (`contractSize`). Else 0.
    pub unit_1e6: i64,
    /// Option: the strike ×1e6 (per underlying unit). Else 0.
    pub strike_1e6: i64,
}

const _: () = assert!(core::mem::size_of::<InstrumentSpec>() == 24);

impl InstrumentSpec {
    /// One instrument's binding.
    #[must_use]
    pub const fn new(sym: SymbolId, law: u8, flags: u8, unit_1e6: i64, strike_1e6: i64) -> Self {
        Self {
            sym,
            law,
            flags,
            _pad: [0; 2],
            unit_1e6,
            strike_1e6,
        }
    }
}

/// Why [`Ledger::bind_instrument`] refused. Boot-only; the boot refuses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InstrumentBindErr {
    /// [`LEDGER_INSTRUMENTS`] rows are bound already.
    Full,
    /// That id is bound already.
    Duplicate,
    /// The spec breaks its law: a `NONE` id, an unknown law, no unit on
    /// an inverse or option row, no strike on an option, or an option
    /// flag on a row that is not an option.
    BadSpec,
}

/// One bound instrument: its law, its prices and every slot's signed
/// position and exposure. `#[repr(C)]`, exactly 192 B — three lines, the
/// positions and the cached exposures each one line of their own.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct InstrumentRow {
    sym: SymbolId,
    law: u8,
    flags: u8,
    _pad0: [u8; 2],
    /// See [`InstrumentSpec::unit_1e6`].
    unit_1e6: i64,
    /// See [`InstrumentSpec::strike_1e6`].
    strike_1e6: i64,
    /// The venue's mark, ×1e6 (per contract on an option); 0 = none yet.
    mark_1e6: i64,
    /// An option's underlying index ×1e6; 0 = none yet.
    index_1e6: i64,
    /// The last venue fill's price ×1e6; 0 = none yet.
    last_px_1e6: i64,
    /// Monotonic ns the mark arrived (BX6: a mark that stops arriving
    /// stops pricing, [`Ledger::expire_marks`]); 0 = none.
    mark_ns: u64,
    _pad1: [u8; 8],
    /// Index = slot. Signed position ×1e6 (base units, or contracts).
    pos_1e6: [i64; EXEC_SLOTS],
    /// Index = slot. Exposure at the row's own prices, USD ×1e6 —
    /// the term this row contributes to [`Ledger::inst_exposure_1e6`].
    exp_1e6: [i64; EXEC_SLOTS],
}

const _: () = assert!(core::mem::size_of::<InstrumentRow>() == 192);
const _: () = assert!(core::mem::align_of::<InstrumentRow>() == 64);
const _: () = assert!(core::mem::offset_of!(InstrumentRow, pos_1e6) == 64);
const _: () = assert!(core::mem::offset_of!(InstrumentRow, exp_1e6) == 128);

impl InstrumentRow {
    const FREE: Self = Self {
        sym: SYMBOL_ID_NONE,
        law: 0,
        flags: 0,
        _pad0: [0; 2],
        unit_1e6: 0,
        strike_1e6: 0,
        mark_1e6: 0,
        index_1e6: 0,
        last_px_1e6: 0,
        mark_ns: 0,
        _pad1: [0; 8],
        pos_1e6: [0; EXEC_SLOTS],
        exp_1e6: [0; EXEC_SLOTS],
    };

    /// A long's (or a linear position's) price between orders: the mark
    /// once one arrived, else the last fill's price (O-BX22).
    #[inline]
    const fn px_ctx(&self) -> i64 {
        if self.mark_1e6 > 0 {
            self.mark_1e6
        } else {
            self.last_px_1e6
        }
    }

    /// One slot's exposure at the row's OWN prices — the aggregate's
    /// term. A short option with no index yet is priced at the strike
    /// (the at-the-money IM); the order path never OPENS one without an
    /// index ([`Ledger::probe_instrument`]).
    fn exposure_ctx(&self, pos: i64) -> i64 {
        let mag = pos.saturating_abs();
        match self.law {
            LAW_SPOT | LAW_LINEAR => mul_1e6(mag, self.px_ctx()),
            LAW_INVERSE => mul_1e6(mag, self.unit_1e6),
            LAW_OPTION => {
                if pos >= 0 {
                    mul_1e6(mag, self.px_ctx())
                } else {
                    let index = if self.index_1e6 > 0 {
                        self.index_1e6
                    } else {
                        self.strike_1e6
                    };
                    let im = short_im_per_contract(
                        index,
                        self.strike_1e6,
                        self.px_ctx(),
                        self.unit_1e6,
                        self.flags & INST_CALL != 0,
                    );
                    mul_1e6(mag, im)
                }
            }
            _ => 0,
        }
    }
}

/// `a × b / 1e6` (both ×1e6 → ×1e6), in `i128`, clamped to
/// `[0, INST_EXPOSURE_MAX]`: the operands here are magnitudes.
#[inline]
fn mul_1e6(a: i64, b: i64) -> i64 {
    let p = (a as i128).saturating_mul(b as i128) / 1_000_000;
    if p <= 0 {
        0
    } else if p >= INST_EXPOSURE_MAX as i128 {
        INST_EXPOSURE_MAX
    } else {
        p as i64
    }
}

/// **The venue's initial margin for ONE short option contract**, USD
/// ×1e6 — Binance's formula verbatim (D8):
/// `max(10 %·I, 15 %·I − OTM)·unit + M`, with the index `I` and strike
/// `K` per unit of the underlying, `unit` underlying units per contract
/// and the mark `M` per contract. OTM is `max(K − I, 0)` for a call and
/// `max(I − K, 0)` for a put.
#[inline]
fn short_im_per_contract(index: i64, strike: i64, mark: i64, unit: i64, call: bool) -> i64 {
    let i = index as i128;
    let k = strike as i128;
    let otm = if call { k - i } else { i - k };
    let otm = if otm > 0 { otm } else { 0 };
    let floor = i / 10;
    let rate = 3 * i / 20 - otm;
    let per_unit = if rate > floor { rate } else { floor };
    let per_unit = if per_unit > 0 { per_unit } else { 0 };
    let m = if mark > 0 { mark as i128 } else { 0 };
    let v = per_unit.saturating_mul(unit as i128) / 1_000_000 + m;
    if v >= INST_EXPOSURE_MAX as i128 {
        INST_EXPOSURE_MAX
    } else {
        v as i64
    }
}

/// Why an instrument law refused an order ([`InstProbe::refusal`]).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum InstRefusal {
    /// The law allows it.
    #[default]
    None = 0,
    /// A spot sell past the holding, or a short on a non-writable option.
    Short = 1,
    /// A price at or below zero, a short option with no index, a zero or
    /// negative quantity.
    Unpriced = 2,
}

/// **What the risk gate learns about one order on an instrument row** —
/// every number at ONE price (plan §3.5's one-price rule): the touched
/// row is priced at the order's price (spot, linear), at its mark and
/// index (option) or at its face (inverse) on BOTH sides of the
/// comparison, so a price move can never make a reducing order read as
/// an increase. 40 B, returned by value.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct InstProbe {
    /// A law refusal, or `None`.
    pub refusal: InstRefusal,
    /// The order opposes the position and is no larger than what is left
    /// of it once every working order on its side has filled: it only
    /// reduces risk however they all fill, and no money clamp may refuse
    /// it (O-BX25: a new exit still needs a free open-order place).
    pub exit: bool,
    /// Clamp 1's measure (O-BX21), USD ×1e6: the notional (spot,
    /// linear), the face (inverse), the premium (option buy) or the IM
    /// the order adds (a sell that opens or grows an option short).
    pub measure_1e6: i64,
    /// The slot's exposure now: every other row at its own prices, this
    /// one at the one price.
    pub current_1e6: i64,
    /// The slot's exposure as the order would leave it.
    pub projected_1e6: i64,
    /// The day turnover it would add: the part of the quantity that
    /// increases the position's magnitude, at the law's price.
    pub turnover_add_1e6: i64,
}

const _: () = assert!(core::mem::size_of::<InstProbe>() == 40);

/// One FAMILY's current instance, and the per-slot position in it.
///
/// `#[repr(C)]` and exactly 192 B — three cache lines. A fill touches
/// one of these and nothing else.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct FamilyRow {
    /// The Yes leg's engine symbol, or [`SYMBOL_ID_NONE`] when the row
    /// is free.
    sym_yes: SymbolId,
    /// The No leg's engine symbol. The ingress's boot ordinal law says
    /// this is `sym_yes + 1`, but it is STORED rather than derived:
    /// the roll handler checks the add for the ordinal carry that
    /// would land it in another venue's namespace, and a reader that
    /// recomputed `+ 1` here would skip that check.
    sym_no: SymbolId,
    /// The venue's outcome id for the instance currently bound. `0` is
    /// not a valid outcome. **This changes on every roll** — it names
    /// the instance, not the row.
    outcome: u32,
    /// The ingress's family index — half the row's KEY, stable across
    /// rolls. `0xFF` when the row is free.
    family: u8,
    /// The producing venue (`VenueId` as a raw byte) — **the other
    /// half of the key**.
    ///
    /// A family index is only unique within the ingress that issued
    /// it. Keyed on the byte alone, two venues' family 0 land on one
    /// row and the second `bind` retires the first venue's live
    /// position and drops its resting orders, leaving exposure
    /// reading zero. Doubling the ROW COUNT does nothing about a
    /// colliding KEY — which is what an earlier draft of this file
    /// claimed it did.
    venue: u8,
    /// `1` once this row's instance has SETTLED.
    ///
    /// The binding is kept — the ingress keeps the coins bound too —
    /// so a settlement fill arriving after the roll event still has
    /// somewhere to land rather than counting as unbound. READ on the
    /// fill path ([`Ledger::book_fill`]) and on the settle path
    /// itself, because a settling row's arithmetic is not an ordinary
    /// row's: the venue is closing a position it opened, not trading.
    settled: u8,
    /// Explicit interior padding. The `[i64; _]` arrays below align to
    /// 8, so the header leaves a hole here whether or not it is
    /// named; naming it is what makes the offsets below arithmetic
    /// rather than a thing the compiler decided.
    _pad0: u8,
    /// Slot index = `strategy_id`. Yes contracts held ×1e6.
    pos_yes_1e6: [i64; EXEC_SLOTS],
    /// Slot index = `strategy_id`. No contracts held ×1e6.
    pos_no_1e6: [i64; EXEC_SLOTS],
    /// Explicit tail padding to a whole number of cache lines.
    _pad: [u8; 48],
}

const _: () = assert!(core::mem::size_of::<FamilyRow>() == 192);
const _: () = assert!(core::mem::align_of::<FamilyRow>() == 64);
const _: () = assert!(core::mem::offset_of!(FamilyRow, pos_yes_1e6) == 16);
const _: () = assert!(core::mem::offset_of!(FamilyRow, pos_no_1e6) == 80);

/// A free row's family byte. Not a valid index — the roll seq carries
/// the family in 8 bits and every real one is below [`LEDGER_ROWS`].
const FAMILY_NONE: u8 = 0xFF;

impl FamilyRow {
    #[inline]
    const fn free() -> Self {
        Self {
            sym_yes: SYMBOL_ID_NONE,
            sym_no: SYMBOL_ID_NONE,
            outcome: 0,
            family: FAMILY_NONE,
            venue: 0,
            settled: 0,
            _pad0: 0,
            pos_yes_1e6: [0; EXEC_SLOTS],
            pos_no_1e6: [0; EXEC_SLOTS],
            _pad: [0; 48],
        }
    }

    #[inline]
    const fn is_bound(&self) -> bool {
        self.family != FAMILY_NONE
    }
}

/// One live-arm order the router believes is working at the venue.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct RestingOrder {
    /// The MEMBER's id, not the venue's — `Fill::order_id` carries the
    /// same thing (`exec_hyperliquid::userws` decodes it out of the
    /// cloid, LAW E-9), which is what makes a fill matchable here.
    client_oid: u64,
    /// What is still working ×1e6. A fill decrements it; at or below
    /// zero the order has retired.
    remaining_1e6: i64,
    /// The leg. Kept so a roll can retire this slot's orders on the
    /// instance that ended without asking the venue.
    sym: SymbolId,
    /// Owning slot. **Part of the key** — `client_oid` alone is not
    /// unique across slots, because every member counts its own ids
    /// from 1. Keying on the oid alone is the exact defect the E5
    /// commit-3 review found in the paper matcher's `find_resting`.
    slot: u8,
    /// `1` when the row holds an order.
    live: u8,
    /// **BX3** — [`RESTING_BUY`] or [`RESTING_SELL`]: what the order does
    /// to the position if it fills. The instrument exit test sums the
    /// working quantity on one side ([`Ledger::probe_instrument`]).
    side: u8,
    _pad: [u8; 1],
}

const _: () = assert!(core::mem::size_of::<RestingOrder>() == 24);

/// [`RestingOrder::side`] of a buy.
const RESTING_BUY: u8 = 1;
/// [`RestingOrder::side`] of a sell.
const RESTING_SELL: u8 = 2;

/// The resting side byte of an order.
#[inline(always)]
const fn resting_side(buy: bool) -> u8 {
    if buy {
        RESTING_BUY
    } else {
        RESTING_SELL
    }
}

impl RestingOrder {
    #[inline]
    const fn free() -> Self {
        Self {
            client_oid: 0,
            remaining_1e6: 0,
            sym: SYMBOL_ID_NONE,
            slot: 0,
            live: 0,
            side: 0,
            _pad: [0; 1],
        }
    }
}

/// What the ledger had to refuse or could not make sense of.
///
/// Every field here is a number an operator acts on. None of them is
/// reachable in a healthy boot, which is exactly why they are counted
/// rather than asserted: a `debug_assert!` is absent from the release
/// build that matters.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LedgerCounters {
    /// Venue fills booked into the exposure and turnover ledgers.
    pub fills_booked: u64,
    /// Fills whose `sym` matched no bound outcome. A fill the router
    /// cannot place is a position it is not counting, so this must be
    /// zero on a live boot; non-zero means a roll was missed or
    /// refused.
    pub fills_unbound: u64,
    /// Fills carrying `STRATEGY_ID_NONE`, or a slot at or above
    /// [`EXEC_SLOTS`]. Not attributable to a cap.
    pub fills_unattributed: u64,
    /// Rolls that could not be bound — the table was full, the event
    /// named outcome 0 or a family at or above [`LEDGER_ROWS`], the No
    /// leg's ordinal would have carried out of the symbol's venue
    /// namespace, or the kind byte was neither CREATED nor SETTLED.
    pub binds_refused: u64,
    /// A SETTLED roll naming a family and outcome the ledger holds no
    /// row for. Its create-roll was missed or refused, so this
    /// instance's position was never being counted.
    pub settles_unmatched: u64,
    /// Sells that would have taken a leg below zero, truncated to
    /// zero. **The only lossy arithmetic in this module.** A HIP-4 leg
    /// cannot be shorted, so a non-zero value is the router's position
    /// disagreeing with the venue's — and because a leg can never go
    /// negative, the error is absorbed and never self-heals. The tell
    /// for a pre-boot position or a missed fill.
    pub sells_below_zero: u64,
    /// Outcomes bound by a roll.
    pub binds: u64,
    /// Instances retired by a roll (the row's positions cleared).
    pub instances_cleared: u64,
    /// Orders the resting table could not hold. See
    /// [`LEDGER_RESTING`] — non-zero means the clamps are unset or
    /// something is not retiring.
    pub resting_full: u64,
    /// A fill whose `(client_oid, slot)` matched MORE THAN ONE resting
    /// row. The position and turnover are still booked; only the
    /// open-order count is left alone, because decrementing an
    /// arbitrary one of two identical keys is a guess.
    pub resting_ambiguous: u64,
    /// A fill that matched no resting row. Normal for a fill on an
    /// order placed before this boot, or on the paper arm's side of a
    /// mixed table; a stream of them on a live-only slot means the
    /// count is drifting upward.
    pub resting_unmatched: u64,
    /// Day-cap epochs crossed (00:00Z rollovers observed).
    pub day_rollovers: u64,
    /// **BX6 (obligation 6)** — marks refused by the sanity law: no index,
    /// or a mark more than [`MARK_INDEX_MAX_DEV_1E6`] away from it.
    pub marks_refused: u64,
    /// **BX6** — marks dropped for staleness ([`MARK_STALE_NS`]): the row
    /// is priced at its last fill until a fresh one arrives.
    pub marks_expired: u64,
}

/// **BX6 (BX3 obligation 6)** — a mark further than this from its index
/// (×1e6 of the index: 10 %) is refused, never priced.
pub const MARK_INDEX_MAX_DEV_1E6: i64 = 100_000;
/// **BX6** — a mark older than this (30 s; the venue pushes one every 1–3
/// s) no longer prices its row.
pub const MARK_STALE_NS: u64 = 30_000_000_000;

/// What `(client_oid, slot)` matched in the resting table.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Found {
    /// Exactly one row, at this index.
    One(usize),
    /// No row holds that key.
    None,
    /// More than one does — counted, and never acted on.
    Many,
}

/// The router's memory of what the venue has done.
///
/// Owned by `RoutedDispatcher`, mutated only on the engine thread.
#[derive(Debug)]
pub struct Ledger {
    rows: [FamilyRow; LEDGER_ROWS],
    resting: [RestingOrder; LEDGER_RESTING],
    /// Index = slot. Filled BUY notional today, USD ×1e6.
    day_turnover_1e6: [i64; EXEC_SLOTS],
    /// Index = slot. Rows in `resting` owned by that slot.
    resting_by_slot: [u32; EXEC_SLOTS],
    /// `wall_ns / DAY_NS` of the day `day_turnover_1e6` counts. `0`
    /// means "no day adopted yet", matching `strategy_bin15`'s own
    /// `day_epoch` convention so the two adopt on the same event.
    day_epoch: u64,
    /// **Monotonic → wall, and the reason this field has to exist.**
    ///
    /// `core_time::now_ns` is `CLOCK_MONOTONIC_RAW`: nanoseconds since
    /// an arbitrary origin, NOT since the Unix epoch. An `Order`'s
    /// `ts_ns` comes from the engine's clock and is therefore
    /// monotonic; a VENUE `Fill`'s `ts_ns` is stamped by the
    /// Hyperliquid arm from `SystemTime` and is wall.
    ///
    /// Feeding both to `wall_ns / DAY_NS` puts two different epochs in
    /// one field, so every alternation between an order and a fill
    /// looks like a midnight crossing and ZEROES the day's turnover —
    /// `cap_day` disabled outright, failing open, with nothing to see
    /// but a climbing `day_rollovers`. `strategy_bin15` solves it with
    /// exactly this anchor (`self.bar.anchor.wall_of(now_ns)`), and
    /// this is the same mechanism rather than a second one.
    anchor: WallAnchor,
    /// **Has anything told this ledger what the venue already holds?**
    ///
    /// A fresh `Ledger` reads zero exposure, zero turnover and zero
    /// resting orders. After a RESTART that is not the truth: the
    /// venue still holds whatever the previous boot left, and all
    /// three clamps would fail OPEN — a full `cap_instance` addable on
    /// top of an existing position, a fresh `cap_day` on top of the
    /// day's real spend, and `max_open_orders` more orders on top of
    /// the ones already working.
    ///
    /// So a live PLACE is refused until something has reconciled this
    /// ledger against the venue. Nothing does yet — the reconciler
    /// wiring is E6 commit 3 — which means **arming a live slot on
    /// this commit alone gets a slot that refuses every order**. That
    /// is the intended reading: a risk gate with no memory of the
    /// venue must not pass orders, and "it refuses everything" is a
    /// failure an operator sees in the first second rather than a cap
    /// that was never really there.
    ///
    /// **Per slot since HYPARB L4** (bit `s` = slot `s`): with two
    /// live arms, each slot is seeded by the reconciler of the arm
    /// that trades it, so slot 0's first reconcile never admits slot
    /// 3's orders, nor the reverse.
    seeded: u8,
    counters: LedgerCounters,
    /// **BX3** — bound instrument ids, sorted ascending; the free tail
    /// is [`SYMBOL_ID_NONE`] (`u32::MAX`), which sorts last.
    inst_keys: [SymbolId; LEDGER_INSTRUMENTS],
    /// **BX3** — the rows, in `inst_keys`' order.
    inst: [InstrumentRow; LEDGER_INSTRUMENTS],
    /// **BX3** — rows bound. `0` on every boot without a Binance slot, and
    /// then every instrument lookup is one compare.
    inst_n: u32,
    /// **BX3** — index = slot: Σ over instrument rows of their cached
    /// exposure (`InstrumentRow::exp_1e6`). Exact by construction
    /// ([`INST_EXPOSURE_MAX`]); added to the family sum wherever the
    /// slot's exposure is read.
    inst_exposure_1e6: [i64; EXEC_SLOTS],
}

// **No `Default`.** A `Ledger` needs the boot's monotonic→wall
// anchor, and there is no sensible default for a clock: a zeroed
// anchor maps every engine timestamp to 1970, which is one fixed day
// epoch and would look exactly like a day cap that never rolls.
// `Ledger::new` takes the anchor so that forgetting it is a compile
// error rather than a cap that quietly stops resetting.

impl Ledger {
    /// An empty, UNSEEDED ledger: nothing bound, nothing held,
    /// nothing resting, and refusing live places until reconciled.
    ///
    /// `anchor` converts the engine's monotonic stamps to wall time
    /// for the day epoch — see [`Ledger::anchor`]. Take it at boot
    /// with `WallAnchor::now()`, the same way `strategy_bin15` does.
    #[must_use]
    pub fn new(anchor: WallAnchor) -> Self {
        Self {
            rows: [FamilyRow::free(); LEDGER_ROWS],
            resting: [RestingOrder::free(); LEDGER_RESTING],
            day_turnover_1e6: [0; EXEC_SLOTS],
            resting_by_slot: [0; EXEC_SLOTS],
            day_epoch: 0,
            anchor,
            seeded: 0,
            counters: LedgerCounters::default(),
            inst_keys: [SYMBOL_ID_NONE; LEDGER_INSTRUMENTS],
            inst: [InstrumentRow::FREE; LEDGER_INSTRUMENTS],
            inst_n: 0,
            inst_exposure_1e6: [0; EXEC_SLOTS],
        }
    }

    /// **Something has reconciled this ledger against the venue.**
    ///
    /// Until this is called, [`Self::is_seeded`] is false and the risk
    /// gate refuses every live PLACE. E6 commit 3's reconciler is what
    /// will call it; it is public now so that the interlock is a
    /// thing the code states rather than a thing commit 3 remembers.
    ///
    /// Every slot at once: one venue relationship reconciled for all
    /// of them. [`Self::mark_slot_seeded`] is the per-arm form.
    #[inline]
    pub fn mark_seeded(&mut self) {
        self.seeded = u8::MAX;
    }

    /// HYPARB L4: slot `slot`'s arm has reconciled. A slot at or above
    /// [`EXEC_SLOTS`] is ignored (it can never be live).
    #[inline]
    pub fn mark_slot_seeded(&mut self, slot: usize) {
        if slot < EXEC_SLOTS {
            self.seeded |= 1 << slot;
        }
    }

    /// Whether ANY slot has been reconciled against its venue since
    /// boot (the `/state` byte). See [`Ledger::seeded`].
    #[inline]
    #[must_use]
    pub const fn is_seeded(&self) -> bool {
        self.seeded != 0
    }

    /// Whether slot `slot` has been reconciled — what the risk gate
    /// asks before a live PLACE.
    #[inline]
    #[must_use]
    pub const fn is_slot_seeded(&self, slot: usize) -> bool {
        slot < EXEC_SLOTS && self.seeded & (1 << slot) != 0
    }

    /// What the ledger had to refuse. Cold; `/metrics` and `/state`.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> &LedgerCounters {
        &self.counters
    }

    // -----------------------------------------------------------------
    // reading — what the clamps ask
    // -----------------------------------------------------------------

    /// **The slot's net exposure, USD ×1e6.**
    ///
    /// `Σ over bound outcomes |yes − no|`. See the module docs for why
    /// the netting is per-outcome and the sum is across them, and why
    /// contracts ×1e6 ARE dollars ×1e6 here.
    ///
    /// A slot at or above [`EXEC_SLOTS`] reports `0`: it can never be
    /// `Live` (`ExecRoute::mode` resolves every out-of-range id to
    /// `Paper`), so there is no clamp for it to answer.
    ///
    /// **BX3** — plus the slot's instrument rows, each at its own prices
    /// (`inst_exposure_1e6`; zero when none is bound), so one
    /// `cap_instance` spans a slot's every venue.
    #[must_use]
    pub fn slot_exposure_1e6(&self, slot: usize) -> i64 {
        if slot >= EXEC_SLOTS {
            return 0;
        }
        self.family_exposure_1e6(slot)
            .saturating_add(self.inst_exposure_1e6[slot])
    }

    /// The family rows' share of [`Self::slot_exposure_1e6`].
    fn family_exposure_1e6(&self, slot: usize) -> i64 {
        // The guard lives HERE, not only in the callers: it is what lets
        // the per-slot indexing below compile without bounds checks
        // whether or not this is inlined.
        if slot >= EXEC_SLOTS {
            return 0;
        }
        let mut total = 0i64;
        let mut i = 0usize;
        while i < LEDGER_ROWS {
            let r = &self.rows[i];
            i += 1;
            if !r.is_bound() {
                continue;
            }
            total =
                total.saturating_add(net_exposure_1e8(r.pos_yes_1e6[slot], r.pos_no_1e6[slot]));
        }
        total
    }

    /// **The slot's net exposure AS ONE MORE ORDER WOULD LEAVE IT**,
    /// USD ×1e6.
    ///
    /// The clamp has to project, not merely read. A slot flat at zero
    /// passes any `slot_exposure_1e6` test, so a cap read off the
    /// CURRENT position would never refuse a slot's first order however
    /// large — it would start biting only on the second, which is one
    /// order too late and exactly the hole `max_order_usd` alone
    /// already leaves.
    ///
    /// The projection is EXACT rather than conservative wherever the
    /// leg is bound: buying the shorter leg of an outcome *reduces*
    /// `|yes − no|`, and a clamp that assumed every order adds risk
    /// would refuse the order that closes a position. **A risk-
    /// reducing order is never refused by this number** —
    /// `the_exposure_clamp_never_refuses_an_order_that_reduces_it` is
    /// what holds that, and it is the property that keeps a cap from
    /// trapping a member inside a position it is trying to leave.
    ///
    /// For a leg the ledger has no binding for, a BUY is assumed to add
    /// its full size and a SELL to add nothing. That is the
    /// conservative reading in both directions, and it is the right one
    /// while the position behind an unbound leg is also missing from
    /// the sum ([`LedgerCounters::fills_unbound`]).
    #[must_use]
    pub fn projected_exposure_1e6(
        &self,
        slot: usize,
        sym: SymbolId,
        qty_1e6: i64,
        buy: bool,
    ) -> i64 {
        if slot >= EXEC_SLOTS {
            return 0;
        }
        let mut total = 0i64;
        let mut hit = false;
        let mut i = 0usize;
        while i < LEDGER_ROWS {
            let r = &self.rows[i];
            i += 1;
            if !r.is_bound() {
                continue;
            }
            let mut yes = r.pos_yes_1e6[slot];
            let mut no = r.pos_no_1e6[slot];
            if sym == r.sym_yes {
                yes = Self::apply(yes, qty_1e6, buy).0;
                hit = true;
            } else if sym == r.sym_no {
                no = Self::apply(no, qty_1e6, buy).0;
                hit = true;
            }
            total = total.saturating_add(net_exposure_1e8(yes, no));
        }
        if !hit && buy {
            total = total.saturating_add(qty_1e6);
        }
        // BX3: the slot's instrument rows, unchanged by a family order.
        total.saturating_add(self.inst_exposure_1e6[slot])
    }

    /// One leg after a fill or a projected order, and **whether the
    /// floor was hit**.
    ///
    /// A sell never takes a leg below zero — the venue has no short on
    /// a HIP-4 outcome, and a negative holding would INFLATE
    /// `|yes − no|` rather than reduce it.
    ///
    /// The floor is the only lossy arithmetic in this module, so it
    /// reports. A real sell that would go below zero means the
    /// router's position disagrees with the venue's — a pre-boot
    /// position, or a fill that never reached lane 3 — and because a
    /// leg can never go negative the error is absorbed here and never
    /// self-heals. [`LedgerCounters::sells_below_zero`] is the only
    /// tell it leaves. (A PROJECTION hitting the floor is ordinary:
    /// that is just an over-sized close, so the projection discards
    /// the flag.)
    #[inline]
    const fn apply(leg_1e6: i64, qty_1e6: i64, buy: bool) -> (i64, bool) {
        if buy {
            (leg_1e6.saturating_add(qty_1e6), false)
        } else {
            let v = leg_1e6.saturating_sub(qty_1e6);
            if v < 0 {
                (0, true)
            } else {
                (v, false)
            }
        }
    }

    /// **The slot's filled BUY turnover for the current day, USD ×1e6.**
    ///
    /// Sells are not counted. The day cap asks how much was committed,
    /// and selling a position back does not un-commit it — a ledger
    /// that netted sells off would let a member round-trip an
    /// unbounded notional under a fixed cap.
    #[inline]
    #[must_use]
    pub fn slot_day_turnover_1e6(&self, slot: usize) -> i64 {
        if slot >= EXEC_SLOTS {
            return 0;
        }
        self.day_turnover_1e6[slot]
    }

    /// **Orders the router believes this slot has working.**
    #[inline]
    #[must_use]
    pub fn slot_resting(&self, slot: usize) -> u32 {
        if slot >= EXEC_SLOTS {
            return 0;
        }
        self.resting_by_slot[slot]
    }

    /// **L1** — what the slot HOLDS on one leg, contracts ×1e6: the
    /// bound row's Yes or No position for `sym`, `0` when no bound row
    /// carries that leg (an unbound leg holds nothing the router can
    /// see, so nothing it could sell is exempt).
    ///
    /// The risk gate's exit test reads it: a sell no larger than this
    /// only returns premium, and the order caps are never allowed to
    /// block one (`RoutedDispatcher::risk_check`).
    #[must_use]
    pub fn held_on_sym_1e6(&self, slot: usize, sym: SymbolId) -> i64 {
        if slot >= EXEC_SLOTS {
            return 0;
        }
        let mut i = 0usize;
        while i < LEDGER_ROWS {
            let r = &self.rows[i];
            i += 1;
            if !r.is_bound() {
                continue;
            }
            if sym == r.sym_yes {
                return r.pos_yes_1e6[slot];
            }
            if sym == r.sym_no {
                return r.pos_no_1e6[slot];
            }
        }
        0
    }

    /// The slot's position in one outcome, `(yes, no)` ×1e6, or `None`
    /// when the outcome is not bound. Test and `/state` only.
    #[must_use]
    pub fn position_1e6(&self, slot: usize, outcome: u32) -> Option<(i64, i64)> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        let mut i = 0usize;
        while i < LEDGER_ROWS {
            let r = &self.rows[i];
            i += 1;
            if r.is_bound() && r.outcome == outcome {
                return Some((r.pos_yes_1e6[slot], r.pos_no_1e6[slot]));
            }
        }
        None
    }

    // -----------------------------------------------------------------
    // the day epoch
    // -----------------------------------------------------------------

    /// Roll the day cap at 00:00Z. **Wall nanoseconds since the Unix
    /// epoch, always** — a venue fill's `ts_ns` directly (the
    /// Hyperliquid arm stamps it from `SystemTime`), an order's
    /// through [`Self::observe_mono_clock`].
    ///
    /// Called from BOTH the fill path and the clamp, and that is not
    /// belt-and-braces. A boot that fills nothing after midnight would
    /// otherwise judge the first order of the new day against the old
    /// day's turnover — the cap would stay shut until a fill happened
    /// to roll it, which is precisely the order it should have let
    /// through.
    ///
    /// `wall_ns == 0` is ignored rather than adopted as epoch 0: a
    /// zero timestamp is an unstamped record, and adopting it would
    /// make the next real timestamp look like a rollover.
    ///
    /// **Forward only (S7-L1).** A stamp from an EARLIER day — a fill
    /// stamped before midnight booked after an order stamped after it,
    /// or a stale venue figure — never rolls the day back: rolling back
    /// would wipe the new day's turnover, and the late fill is counted
    /// in the day it lands in instead, the conservative direction.
    fn roll_day(&mut self, wall_ns: u64) {
        if wall_ns == 0 {
            return;
        }
        let epoch = wall_ns / DAY_NS;
        if self.day_epoch == 0 {
            self.day_epoch = epoch;
        } else if epoch > self.day_epoch {
            self.day_epoch = epoch;
            self.day_turnover_1e6 = [0; EXEC_SLOTS];
            self.counters.day_rollovers = self.counters.day_rollovers.saturating_add(1);
        }
    }

    /// **S7-L1 (gap A) — adopt the venue's own count of a slot's day
    /// spend.** `day` is the UTC day number (`wall / DAY`) the figure
    /// is for; `bought_1e6` is the slot's filled BUY notional since that
    /// day's 00:00Z as the venue's fill history reports it.
    ///
    /// Only ever RAISES the turnover: a fill this ledger booked that
    /// the venue's answer had not caught up with is never un-counted,
    /// and the adoption is idempotent, so the router offers it on every
    /// poll. A figure for a LATER day rolls the ledger first (the first
    /// read after midnight); one for an EARLIER day is a stale answer
    /// from before a rollover and is ignored — letting it through would
    /// roll the day BACKWARDS and wipe it.
    pub fn adopt_venue_day_turnover(&mut self, slot: usize, day: u64, bought_1e6: i64) {
        if slot >= EXEC_SLOTS || day == 0 {
            return;
        }
        if self.day_epoch != 0 && day < self.day_epoch {
            return;
        }
        self.roll_day(day.saturating_mul(DAY_NS));
        if bought_1e6 > self.day_turnover_1e6[slot] {
            self.day_turnover_1e6[slot] = bought_1e6;
        }
    }

    /// Roll the day cap from a MONOTONIC stamp — an `Order`'s
    /// `ts_ns`, which the engine takes from `core_time::now_ns` and
    /// which is `CLOCK_MONOTONIC_RAW`, not Unix time.
    ///
    /// Converted through the boot anchor before it reaches the epoch.
    /// The two paths into `roll_day` must agree about what "now"
    /// means or the epoch thrashes and the day's turnover is wiped on
    /// every order/fill alternation — see [`Ledger::anchor`]. That the
    /// argument is monotonic is stated in the NAME, because the type
    /// is `u64` either way and nothing else would catch it.
    #[inline]
    pub fn observe_mono_clock(&mut self, mono_ns: u64) {
        // **Guarded on the INPUT, not on the converted value.**
        // `roll_day` refuses a zero wall stamp as "an unstamped
        // record", but the conversion runs first and `wall_of(0)` is
        // `wall0 − mono0` — on a machine up eleven days, eleven days
        // before the anchor, a perfectly plausible wall time in a
        // DIFFERENT day epoch. One unstamped order would roll the day
        // and wipe the turnover; alternating stamped and unstamped
        // ones would disable `cap_day` outright, which is the exact
        // failure the anchor was introduced to prevent, re-entered
        // through the one input it does not map faithfully.
        if mono_ns == 0 {
            return;
        }
        self.roll_day(self.anchor.wall_of(mono_ns));
    }

    // -----------------------------------------------------------------
    // writing — rolls
    // -----------------------------------------------------------------

    /// A roll frame this ledger would not act on — a kind byte that
    /// is neither CREATED nor SETTLED, so the frame says nothing the
    /// binding table can use. Counted with the other refused binds.
    #[inline]
    pub fn refuse_roll(&mut self) {
        self.counters.binds_refused = self.counters.binds_refused.saturating_add(1);
    }

    /// **LAW E-4 — bind a family's new instance from a roll event.**
    ///
    /// `family` is the row's KEY and `outcome` names the instance
    /// currently in it. A created roll for a family the table already
    /// holds REPLACES that row's instance and CLEARS what the old one
    /// held — a roll of a family is a new instance of it, and carrying
    /// the old instance's position into the new one is exactly what
    /// `cap_instance` exists to stop. It also drops the orders that
    /// were resting on the retired legs: LAW E-8 says the roll sends a
    /// real cancel-all, so they are gone.
    ///
    /// `sym_yes` is what the wire carries; the No leg is the next
    /// ordinal. The add is CHECKED, for the reason
    /// `exec_hyperliquid`'s own roll handler checks it: release builds
    /// set `overflow-checks = false`, the ordinal field is 24 bits and
    /// `SYMBOL_ID_NONE` is `u32::MAX`, so an unchecked `+ 1` could
    /// carry out of the ordinal and into the venue byte, binding a leg
    /// in another venue's namespace.
    pub fn bind(&mut self, venue: u8, family: usize, outcome: u32, sym_yes: SymbolId) {
        if outcome == 0 || sym_yes == SYMBOL_ID_NONE || family >= LEDGER_ROWS {
            self.counters.binds_refused = self.counters.binds_refused.saturating_add(1);
            return;
        }
        let ord = core_types::symbol_ordinal(sym_yes);
        if ord >= core_types::SYMBOL_ORDINAL_MASK {
            self.counters.binds_refused = self.counters.binds_refused.saturating_add(1);
            return;
        }
        let sym_no = sym_yes + 1;

        // The row this family already owns is REUSED, so successive
        // instances can never occupy two rows.
        let mut free = LEDGER_ROWS;
        let mut i = 0usize;
        while i < LEDGER_ROWS {
            if self.rows[i].is_bound() {
                if self.rows[i].family as usize == family && self.rows[i].venue == venue {
                    // A REPEAT of the roll that is already live binds
                    // nothing: the venue re-sends `outcomeCreated` on
                    // a reconnect and the ingress dedups it, but this
                    // ledger is the risk gate's memory and does not
                    // get to rely on a layer above — retiring the row
                    // here would zero a real position and drop its
                    // resting orders under a flat reading (the guard
                    // `settle` already has, applied to `bind`; E7
                    // review 2026-09-19).
                    if self.rows[i].outcome == outcome && self.rows[i].settled == 0 {
                        return;
                    }
                    self.retire_row(i, outcome, sym_yes, sym_no);
                    return;
                }
            } else if free == LEDGER_ROWS {
                free = i;
            }
            i += 1;
        }
        if free == LEDGER_ROWS {
            self.counters.binds_refused = self.counters.binds_refused.saturating_add(1);
            return;
        }
        self.rows[free] = FamilyRow {
            sym_yes,
            sym_no,
            outcome,
            family: family as u8,
            venue,
            settled: 0,
            _pad0: 0,
            pos_yes_1e6: [0; EXEC_SLOTS],
            pos_no_1e6: [0; EXEC_SLOTS],
            _pad: [0; 48],
        };
        self.counters.binds = self.counters.binds.saturating_add(1);
    }

    /// **The instance this family held has SETTLED.**
    ///
    /// Drops every order resting on the two legs — LAW E-8 says the
    /// roll sends a real cancel-all — marks the row settled, and
    /// **keeps the binding**. The ingress keeps the coins bound and
    /// subscribed on a settle for the same reason
    /// (`perform_roll`'s "A SETTLED instance keeps its coins bound"),
    /// and the venue reports SETTLEMENT down `userFills` like any
    /// other fill. A row freed here would leave that settlement fill
    /// with nowhere to land, counting as
    /// [`LedgerCounters::fills_unbound`] — a counter whose whole
    /// meaning is "a position the router is not tracking" — on the one
    /// event guaranteed to end every instance.
    ///
    /// **It does NOT zero the position.** Until the settlement cash
    /// arrives the contracts are still held, so zeroing here would
    /// report a flat book over a real one for the length of that
    /// window. It would also make every settlement fill land on a
    /// zero leg and trip [`LedgerCounters::sells_below_zero`] — eight
    /// families rolling four times an hour is on the order of 768 a
    /// day — burying the one signal that counter exists to carry. The
    /// settlement fill reduces the position naturally, and
    /// [`Self::retire_row`] clears any residue when the family
    /// rebinds.
    ///
    /// The OUTCOME is the identity here and the family byte is not
    /// consulted, exactly as `strategy_bin15::on_roll` rules: settle
    /// the row that actually HOLDS this instance, so a duplicate or
    /// reordered frame can never clear a successor that has already
    /// bound. A frame naming outcome 0 names no instance and is
    /// REFUSED rather than falling back to the weaker family key —
    /// that fallback would clear whatever the family currently holds,
    /// which after a reorder is the successor's live position.
    ///
    /// A second SETTLED frame for a row already settled is a no-op.
    pub fn settle(&mut self, venue: u8, family: usize, outcome: u32) {
        let _ = family;
        if outcome == 0 {
            self.counters.settles_unmatched = self.counters.settles_unmatched.saturating_add(1);
            return;
        }
        let mut target = LEDGER_ROWS;
        let mut i = 0usize;
        while i < LEDGER_ROWS {
            let r = &self.rows[i];
            if r.is_bound() && r.outcome == outcome && r.venue == venue {
                target = i;
                break;
            }
            i += 1;
        }
        if target == LEDGER_ROWS {
            self.counters.settles_unmatched = self.counters.settles_unmatched.saturating_add(1);
            return;
        }
        if self.rows[target].settled == 1 {
            return;
        }
        let (y, n) = (self.rows[target].sym_yes, self.rows[target].sym_no);
        self.drop_resting_on(y, n);
        self.rows[target].settled = 1;
        self.counters.instances_cleared = self.counters.instances_cleared.saturating_add(1);
    }

    /// Re-point a bound row at a new instance's legs, clearing what
    /// the old instance held.
    ///
    /// This is also where a settled instance's RESIDUE goes. `settle`
    /// deliberately leaves the position alone so the settlement fill
    /// can reduce it; if that fill never arrived, the successor's
    /// bind is what clears it, and `cap_instance` means THIS instance
    /// either way.
    fn retire_row(&mut self, i: usize, outcome: u32, sym_yes: SymbolId, sym_no: SymbolId) {
        let (old_y, old_n) = (self.rows[i].sym_yes, self.rows[i].sym_no);
        self.drop_resting_on(old_y, old_n);
        self.rows[i].sym_yes = sym_yes;
        self.rows[i].sym_no = sym_no;
        self.rows[i].outcome = outcome;
        self.rows[i].settled = 0;
        self.rows[i].pos_yes_1e6 = [0; EXEC_SLOTS];
        self.rows[i].pos_no_1e6 = [0; EXEC_SLOTS];
        self.counters.instances_cleared = self.counters.instances_cleared.saturating_add(1);
        self.counters.binds = self.counters.binds.saturating_add(1);
    }

    fn drop_resting_on(&mut self, sym_yes: SymbolId, sym_no: SymbolId) {
        let mut j = 0usize;
        while j < LEDGER_RESTING {
            // Three fields read before `release` mutates the row — a
            // reference would hold `self` across that call.
            let (live, sym) = (self.resting[j].live, self.resting[j].sym);
            j += 1;
            if live == 0 || (sym != sym_yes && sym != sym_no) {
                continue;
            }
            self.release(j - 1);
        }
    }

    // -----------------------------------------------------------------
    // BX3 — instrument rows
    // -----------------------------------------------------------------

    /// **Bind one instrument row.** Boot-only: a sorted insert, so the
    /// order path's lookup is a fixed eight-step search.
    ///
    /// # Errors
    /// [`InstrumentBindErr`] — the table is full, the id is bound
    /// already, or the spec breaks its law. The boot refuses on any.
    pub fn bind_instrument(&mut self, spec: &InstrumentSpec) -> Result<(), InstrumentBindErr> {
        let option = spec.law == LAW_OPTION;
        let bad = spec.sym == SYMBOL_ID_NONE
            || spec.law < LAW_SPOT
            || spec.law > LAW_OPTION
            || ((spec.law == LAW_INVERSE || option) && spec.unit_1e6 <= 0)
            || (option && spec.strike_1e6 <= 0)
            || (!option && (spec.flags != 0 || spec.strike_1e6 != 0));
        if bad {
            return Err(InstrumentBindErr::BadSpec);
        }
        if self.inst_find(spec.sym).is_some() {
            return Err(InstrumentBindErr::Duplicate);
        }
        let n = self.inst_n as usize;
        if n >= LEDGER_INSTRUMENTS {
            return Err(InstrumentBindErr::Full);
        }
        let mut at = n;
        while at > 0 && self.inst_keys[at - 1] > spec.sym {
            // COPY: one 192 B row and its 4 B key, one place right, per
            // row after the insertion point — boot-only, at most 255 rows
            // for one bind — rejected: an unsorted table, whose lookup on
            // the order path would be a linear scan of 256 rows.
            self.inst_keys[at] = self.inst_keys[at - 1];
            self.inst[at] = self.inst[at - 1];
            at -= 1;
        }
        self.inst_keys[at] = spec.sym;
        self.inst[at] = InstrumentRow {
            sym: spec.sym,
            law: spec.law,
            flags: spec.flags,
            unit_1e6: spec.unit_1e6,
            strike_1e6: spec.strike_1e6,
            ..InstrumentRow::FREE
        };
        self.inst_n += 1;
        Ok(())
    }

    /// Instrument rows bound. Cold.
    #[inline]
    #[must_use]
    pub const fn instruments_bound(&self) -> u32 {
        self.inst_n
    }

    /// **The row bound for `sym`.** One compare when nothing is bound
    /// (every boot without a Binance slot); otherwise a branchless lower
    /// bound over the fixed, sorted key array — eight steps for 256.
    #[inline]
    #[must_use]
    fn inst_find(&self, sym: SymbolId) -> Option<usize> {
        if self.inst_n == 0 || sym == SYMBOL_ID_NONE {
            return None;
        }
        let mut base = 0usize;
        let mut size = LEDGER_INSTRUMENTS;
        while size > 1 {
            let half = size / 2;
            let mid = base + half;
            // SAFETY: `base + size <= LEDGER_INSTRUMENTS` holds before
            // every step (it starts equal, and each step moves `base` up
            // by at most `half` while `size` shrinks by exactly `half`),
            // so `mid = base + half < base + size <= LEDGER_INSTRUMENTS`.
            let key = unsafe { *self.inst_keys.get_unchecked(mid) };
            if key <= sym {
                base = mid;
            }
            size -= half;
        }
        // `base < LEDGER_INSTRUMENTS` already (the invariant above); the
        // mask says so to the compiler, so every `self.inst[i]` a caller
        // indexes with the result carries no bounds check.
        let i = base & (LEDGER_INSTRUMENTS - 1);
        if self.inst_keys[i] == sym {
            Some(i)
        } else {
            None
        }
    }

    /// Whether `sym` has an instrument row. The router's routing test.
    #[inline]
    #[must_use]
    pub fn has_instrument(&self, sym: SymbolId) -> bool {
        self.inst_find(sym).is_some()
    }

    /// A slot's signed position on an instrument, ×1e6, or `None` when
    /// no row is bound for it. Cold; `/state` and tests.
    #[must_use]
    pub fn instrument_position_1e6(&self, slot: usize, sym: SymbolId) -> Option<i64> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        self.inst_find(sym).map(|i| self.inst[i].pos_1e6[slot])
    }

    /// The instrument rows' share of a slot's exposure, USD ×1e6. Cold.
    #[inline]
    #[must_use]
    pub fn instrument_exposure_1e6(&self, slot: usize) -> i64 {
        if slot >= EXEC_SLOTS {
            return 0;
        }
        self.inst_exposure_1e6[slot]
    }

    /// **Judge one order on an instrument row by its law** — the numbers
    /// the router's clamps compare, all at one price (see [`InstProbe`]).
    /// `px` is the order's price ×1e6 (per contract on an option);
    /// `exclude` is the resting order a modify replaces (`None` for a
    /// fresh order).
    ///
    /// **Judged from the worst position the slot's working orders allow**
    /// (BX3 risk review). An order that opposes the position is judged
    /// as if every working order on its side had filled first — they
    /// reduce the same position, and on a signed law N working exits of
    /// the whole position would otherwise each read as an exit and, all
    /// filled, flip it by (N−1)× with no clamp measuring the flip. An
    /// order that does not oppose the position is judged from the filled
    /// position itself: working orders that reduce can only help it, and
    /// ones that increase are bounded by `max_open_orders × max_order`,
    /// the stated worst case.
    #[must_use]
    pub fn probe_instrument(
        &self,
        sym: SymbolId,
        slot: usize,
        qty_1e6: i64,
        buy: bool,
        px_1e6: i64,
        exclude: Option<u64>,
    ) -> Option<InstProbe> {
        let i = self.inst_find(sym)?;
        let mut p = InstProbe::default();
        if slot >= EXEC_SLOTS || qty_1e6 <= 0 {
            p.refusal = InstRefusal::Unpriced;
            return Some(p);
        }
        let r = &self.inst[i];
        let filled = r.pos_1e6[slot];
        let pos = if (filled > 0 && !buy) || (filled < 0 && buy) {
            let working = self.working_1e6(slot, sym, buy, exclude);
            if buy {
                filled.saturating_add(working)
            } else {
                filled.saturating_sub(working)
            }
        } else {
            filled
        };
        let mag = pos.saturating_abs();
        let opposes = (pos > 0 && !buy) || (pos < 0 && buy);
        if opposes && qty_1e6 <= mag {
            // Reduces the position without crossing zero, however the
            // working orders on its side fill. Every law is monotone in
            // the magnitude within a sign, so no clamp can read it as an
            // increase — and it is never priced, so an exit needs no
            // mark, no index and no price.
            p.exit = true;
            return Some(p);
        }
        let inc = if opposes { qty_1e6 - mag } else { qty_1e6 };
        let next = if buy {
            pos.saturating_add(qty_1e6)
        } else {
            pos.saturating_sub(qty_1e6)
        };
        // **O-BX24 — the one price a SELL is judged at: the higher of its
        // limit and the row's reference** (the mark, else the last fill).
        // A marketable sell fills at or above its limit, so a low limit
        // alone would understate what a short-opening sell adds — by as
        // much as the venue's price band allows. A buy keeps its own
        // price: it fills at or below it. Used on both sides of every
        // comparison below, so the one-price rule holds. A non-positive
        // LIMIT is refused before this, whatever the reference.
        let px_at = if buy { px_1e6 } else { px_1e6.max(r.px_ctx()) };
        let (e_now, e_next, measure, turnover) = match r.law {
            LAW_SPOT | LAW_LINEAR => {
                if r.law == LAW_SPOT && next < 0 {
                    p.refusal = InstRefusal::Short;
                    return Some(p);
                }
                if px_1e6 <= 0 {
                    p.refusal = InstRefusal::Unpriced;
                    return Some(p);
                }
                (
                    mul_1e6(mag, px_at),
                    mul_1e6(next.saturating_abs(), px_at),
                    mul_1e6(qty_1e6, px_at),
                    mul_1e6(inc, px_at),
                )
            }
            LAW_INVERSE => (
                mul_1e6(mag, r.unit_1e6),
                mul_1e6(next.saturating_abs(), r.unit_1e6),
                mul_1e6(qty_1e6, r.unit_1e6),
                mul_1e6(inc, r.unit_1e6),
            ),
            LAW_OPTION => {
                if px_1e6 <= 0 {
                    p.refusal = InstRefusal::Unpriced;
                    return Some(p);
                }
                // The long side's one price: the mark, or — before the
                // first summary — the order's own price (O-BX24: a sell's
                // is the higher of its limit and the last fill), never
                // zero (a zero would read a first long as no exposure).
                let m = if r.mark_1e6 > 0 { r.mark_1e6 } else { px_at };
                let opens_short = !buy && next < 0;
                if opens_short {
                    if r.flags & INST_WRITABLE == 0 {
                        p.refusal = InstRefusal::Short;
                        return Some(p);
                    }
                    if r.index_1e6 <= 0 {
                        p.refusal = InstRefusal::Unpriced;
                        return Some(p);
                    }
                }
                // The short IM, only where a short is on either side of
                // the order (two i128 divides a pure long never needs).
                let im = if pos < 0 || next < 0 {
                    let index = if r.index_1e6 > 0 {
                        r.index_1e6
                    } else {
                        r.strike_1e6
                    };
                    short_im_per_contract(
                        index,
                        r.strike_1e6,
                        m,
                        r.unit_1e6,
                        r.flags & INST_CALL != 0,
                    )
                } else {
                    0
                };
                let e_now = if pos >= 0 {
                    mul_1e6(mag, m)
                } else {
                    mul_1e6(mag, im)
                };
                let e_next = if next >= 0 {
                    mul_1e6(next, m)
                } else {
                    mul_1e6(next.saturating_abs(), im)
                };
                // O-BX21: a buy is measured by its premium, a sell that
                // opens or grows a short by the IM it adds.
                let measure = if buy {
                    mul_1e6(qty_1e6, px_1e6)
                } else {
                    mul_1e6(inc, im)
                };
                (e_now, e_next, measure, mul_1e6(inc, px_at))
            }
            _ => {
                p.refusal = InstRefusal::Unpriced;
                return Some(p);
            }
        };
        let others = self
            .family_exposure_1e6(slot)
            .saturating_add(self.inst_exposure_1e6[slot] - r.exp_1e6[slot]);
        p.measure_1e6 = measure;
        p.current_1e6 = others.saturating_add(e_now);
        p.projected_1e6 = others.saturating_add(e_next);
        p.turnover_add_1e6 = turnover;
        Some(p)
    }

    /// Re-price one row for every slot and keep the aggregate exact.
    fn refresh_instrument(&mut self, i: usize) {
        let r = &mut self.inst[i];
        let mut s = 0usize;
        while s < EXEC_SLOTS {
            // A flat slot with nothing cached has nothing to re-price —
            // and skipping it skips the i128 divides a mark would
            // otherwise cost every slot on every row.
            if r.pos_1e6[s] == 0 && r.exp_1e6[s] == 0 {
                s += 1;
                continue;
            }
            let e = r.exposure_ctx(r.pos_1e6[s]);
            self.inst_exposure_1e6[s] = self.inst_exposure_1e6[s] - r.exp_1e6[s] + e;
            r.exp_1e6[s] = e;
            s += 1;
        }
    }

    /// **The price feed — a venue `Mark`** (`v0` = mark ×1e6, `v1` = the
    /// index ×1e6, `ts_ns` when it was read): a spot, linear or inverse
    /// row's mark. Options take theirs from [`Self::on_opt_summary`].
    /// Unbound ids are ignored. **BX6 (obligation 6): the sanity law** — a
    /// mark with no index, a non-positive one, or one further than
    /// [`MARK_INDEX_MAX_DEV_1E6`] from its index is refused and counted;
    /// the row keeps its previous price.
    pub fn on_mark(&mut self, sym: SymbolId, mark_1e6: i64, index_1e6: i64, ts_ns: u64) {
        let Some(i) = self.inst_find(sym) else {
            return;
        };
        if self.inst[i].law == LAW_OPTION {
            return;
        }
        let dev = (mark_1e6 as i128 - index_1e6 as i128).abs();
        if mark_1e6 <= 0 || index_1e6 <= 0 || dev * 1_000_000 > MARK_INDEX_MAX_DEV_1E6 as i128 * index_1e6 as i128 {
            self.counters.marks_refused = self.counters.marks_refused.saturating_add(1);
            return;
        }
        self.inst[i].mark_1e6 = mark_1e6;
        self.inst[i].mark_ns = ts_ns;
        self.refresh_instrument(i);
    }

    /// **BX6 (obligation 6): the staleness law** — a mark older than
    /// [`MARK_STALE_NS`] at `now_ns` stops pricing its row (the last fill
    /// prices it, O-BX22) until a fresh one arrives. The router calls this
    /// once a second.
    pub fn expire_marks(&mut self, now_ns: u64) {
        let n = self.inst_n as usize;
        let mut i = 0;
        while i < n {
            let r = &self.inst[i];
            if r.law != LAW_OPTION && r.mark_1e6 > 0 && now_ns.saturating_sub(r.mark_ns) > MARK_STALE_NS {
                self.inst[i].mark_1e6 = 0;
                self.counters.marks_expired = self.counters.marks_expired.saturating_add(1);
                self.refresh_instrument(i);
            }
            i += 1;
        }
    }

    /// **The price feed — an options summary**: an option row's mark (when
    /// the summary carries one) and its underlying's index, both ×1e9 on
    /// the lane, ×1e6 here.
    pub fn on_opt_summary(&mut self, summary: &core_types::OptSummary) {
        let Some(i) = self.inst_find(summary.sym) else {
            return;
        };
        if self.inst[i].law != LAW_OPTION {
            return;
        }
        let r = &mut self.inst[i];
        let mut moved = false;
        if summary.flags & core_types::OPT_SUMMARY_FLAG_MARK_PX != 0 && summary.mark_px_1e9 > 0 {
            r.mark_1e6 = summary.mark_px_1e9 / 1_000;
            moved = true;
        }
        if summary.underlying_px_1e9 > 0 {
            r.index_1e6 = summary.underlying_px_1e9 / 1_000;
            moved = true;
        }
        if moved {
            self.refresh_instrument(i);
        }
    }

    /// **Book one venue fill on an instrument row.** The position moves by
    /// the fill (a spot row floors at zero and reports it); the day
    /// turnover adds the INCREASING part at the law's price; a
    /// settlement (options expiry, delivery) zeroes the slot's position,
    /// adds no turnover and matches no resting order.
    fn book_instrument_fill(&mut self, i: usize, fill: &Fill, slot: usize) {
        let qty = fill.qty.raw();
        let px = fill.px.raw();
        let buy = fill.side == core_types::Side::Bid;
        let settlement = fill.is_settlement();
        let r = &mut self.inst[i];
        if settlement {
            r.pos_1e6[slot] = 0;
        } else {
            let pos = r.pos_1e6[slot];
            let mag = pos.saturating_abs();
            let opposes = (pos > 0 && !buy) || (pos < 0 && buy);
            let inc = if opposes {
                if qty > mag {
                    qty - mag
                } else {
                    0
                }
            } else {
                qty
            };
            let mut next = if buy {
                pos.saturating_add(qty)
            } else {
                pos.saturating_sub(qty)
            };
            if r.law == LAW_SPOT && next < 0 {
                next = 0;
                self.counters.sells_below_zero = self.counters.sells_below_zero.saturating_add(1);
            }
            r.pos_1e6[slot] = next;
            if px > 0 {
                r.last_px_1e6 = px;
            }
            let add = if r.law == LAW_INVERSE {
                mul_1e6(inc, r.unit_1e6)
            } else {
                mul_1e6(inc, px)
            };
            self.day_turnover_1e6[slot] = self.day_turnover_1e6[slot].saturating_add(add);
        }
        self.refresh_instrument(i);
        self.counters.fills_booked = self.counters.fills_booked.saturating_add(1);
        if !settlement {
            self.consume_resting(fill.order_id, slot, qty);
        }
    }

    // -----------------------------------------------------------------
    // writing — fills
    // -----------------------------------------------------------------

    /// **Book one fill.**
    ///
    /// PAPER fills are ignored outright: these ledgers exist to bound
    /// real money, and a modelled trade puts none at risk. The test is
    /// on [`FILL_ORIGIN_VENUE`] and it is made HERE, once, so that the
    /// engine can call the hook from both of its drains without either
    /// call site having to know the rule.
    pub fn book_fill(&mut self, fill: &Fill) {
        if fill.origin != FILL_ORIGIN_VENUE {
            return;
        }
        self.roll_day(fill.ts_ns);

        let slot = fill.strategy_id as usize;
        if slot >= EXEC_SLOTS {
            self.counters.fills_unattributed = self.counters.fills_unattributed.saturating_add(1);
            return;
        }
        // BX3: an instrument row books by its law.
        if let Some(i) = self.inst_find(fill.sym) {
            self.book_instrument_fill(i, fill, slot);
            return;
        }
        let qty = fill.qty.raw();
        let buy = fill.side == core_types::Side::Bid;

        // ---- exposure -------------------------------------------------
        // Deliberately first, and deliberately independent of the
        // resting table below: the money caps must be right even when
        // order matching is not.
        let mut placed = false;
        let mut below_zero = false;
        let mut on_settled_row = false;
        let mut i = 0usize;
        while i < LEDGER_ROWS {
            let r = &mut self.rows[i];
            i += 1;
            if !r.is_bound() {
                continue;
            }
            let settled_row = r.settled == 1;
            let leg = if fill.sym == r.sym_yes {
                &mut r.pos_yes_1e6[slot]
            } else if fill.sym == r.sym_no {
                &mut r.pos_no_1e6[slot]
            } else {
                continue;
            };
            // The SAME rule the clamp projects with. One function, so
            // "what this order would do" and "what this fill did" can
            // never drift apart — a projection that rounded a leg
            // differently from the booking would let an order pass a
            // cap it then breached on landing.
            let (next, floored) = Self::apply(*leg, qty, buy);
            *leg = next;
            below_zero = floored;
            on_settled_row = settled_row;
            placed = true;
            break;
        }
        if !placed {
            self.counters.fills_unbound = self.counters.fills_unbound.saturating_add(1);
        }
        // **Counted on a settled row too**, and that is deliberate.
        //
        // The review that asked for this counter also asked to
        // suppress it here, because at the time `settle` ZEROED the
        // position and every settlement therefore sold against an
        // empty leg — hundreds of floor hits a day, burying the
        // signal. Not zeroing was the better of the two fixes, and
        // both were applied. With the zeroing gone the suppression
        // had nothing left to suppress: break-and-watch removed the
        // `!settled` test and NOT ONE TEST FAILED, which is how an
        // unreachable guard announces itself.
        //
        // And it was worse than dead. A settlement that sells MORE
        // than this ledger booked is precisely the router disagreeing
        // with the venue — a buy fill that never reached lane 3 — and
        // it is the single most informative moment to hear about it.
        // The guard would have swallowed exactly that.
        if below_zero {
            self.counters.sells_below_zero = self.counters.sells_below_zero.saturating_add(1);
        }

        // ---- turnover -------------------------------------------------
        // Booked whether or not the leg was bound. An unbound fill is
        // a position the router cannot place, but the money left the
        // account either way, and a day cap that ignored it would be
        // most generous exactly when the ledger is least trustworthy.
        // **Not on a settled row** — this guard IS load-bearing (see
        // `a_settlement_never_charges_the_day_cap`, and the
        // break-and-watch run that fails without it). A settlement is
        // the venue paying out, not the member committing capital, so
        // charging it to the day cap would spend a budget on money
        // that never left.
        // The venue prints both sides of a settlement as `side: "A"`
        // (measured, testnet 2026-09-15), so this branch should not be
        // reachable for one — which is exactly why it is guarded
        // rather than assumed.
        if buy && !on_settled_row {
            // `i128` product, then a SATURATING narrow — `as i64` would
            // wrap a product past ~9.2e18 negative and let the day cap
            // read a spend as a refund.
            let notional_1e6 = i64::try_from(
                (fill.px.raw() as i128).saturating_mul(qty as i128) / 1_000_000,
            )
            .unwrap_or(i64::MAX);
            self.day_turnover_1e6[slot] =
                self.day_turnover_1e6[slot].saturating_add(notional_1e6);
        }

        self.counters.fills_booked = self.counters.fills_booked.saturating_add(1);

        // ---- resting --------------------------------------------------
        self.consume_resting(fill.order_id, slot, qty);
    }

    /// Decrement the resting order this fill belongs to, retiring it
    /// when nothing is left.
    fn consume_resting(&mut self, client_oid: u64, slot: usize, qty: i64) {
        // ONE lookup rule — `find` — not a second copy of the scan.
        // Two rows under one key is a guess, and a guess that retires
        // the wrong order makes the NEXT fill unmatched too: `find`
        // counts it and this leaves the table alone.
        let found = match self.find(client_oid, slot) {
            Found::None => {
                self.counters.resting_unmatched =
                    self.counters.resting_unmatched.saturating_add(1);
                return;
            }
            Found::Many => return,
            Found::One(i) => i,
        };
        let rem = self.resting[found].remaining_1e6.saturating_sub(qty);
        if rem <= 0 {
            self.release(found);
        } else {
            self.resting[found].remaining_1e6 = rem;
        }
    }

    // -----------------------------------------------------------------
    // writing — the order lifecycle
    // -----------------------------------------------------------------

    /// A live submit was accepted by the arm. Starts tracking it.
    pub fn on_submit(
        &mut self,
        client_oid: u64,
        slot: usize,
        sym: SymbolId,
        qty_1e6: i64,
        buy: bool,
    ) {
        if slot >= EXEC_SLOTS {
            return;
        }
        let mut free = LEDGER_RESTING;
        let mut i = 0usize;
        while i < LEDGER_RESTING {
            if self.resting[i].live == 0 {
                free = i;
                break;
            }
            i += 1;
        }
        if free == LEDGER_RESTING {
            // **Fail CLOSED.** The count is still incremented even
            // though no row was taken, because the order IS working at
            // the venue — the same reading `on_modify`'s unmatched
            // branch takes. Returning without it would freeze
            // `resting_by_slot` and leave `max_open_orders` passing
            // every subsequent order for the rest of the boot: a clamp
            // silently switched off by the table it depends on.
            //
            // The cost is that the untracked order can never be
            // retired, so the slot ratchets toward refusing
            // everything. That is the right direction.
            //
            // `core_config::exec` refuses at boot any live slot whose
            // `max_open_orders` exceeds its equal share of this table,
            // which bounds the SUBMIT path. It does NOT make this
            // unreachable: a modify is exempt from `max_open_orders`
            // (LAW E-7), and an unmatched modify tracks its
            // replacement here, so the count is not bounded by the
            // clamp. Reaching it needs a stream of modifies naming
            // orders this ledger never saw, which is a disagreement
            // with the venue that `resting_full` is now the tell
            // for — not a configuration error.
            self.counters.resting_full = self.counters.resting_full.saturating_add(1);
            self.resting_by_slot[slot] = self.resting_by_slot[slot].saturating_add(1);
            return;
        }
        self.resting[free] = RestingOrder {
            client_oid,
            remaining_1e6: qty_1e6,
            sym,
            slot: slot as u8,
            live: 1,
            side: resting_side(buy),
            _pad: [0; 1],
        };
        self.resting_by_slot[slot] = self.resting_by_slot[slot].saturating_add(1);
    }

    /// **E6 commit 3 — every slot's orders were cancelled at the
    /// venue.**
    ///
    /// A halt's cancel-all is venue-wide, so the resting count is
    /// stale for EVERY slot — including the healthy ones that keep
    /// running and will simply re-quote. Leaving them counted would
    /// have `max_open_orders` refuse a slot holding nothing.
    ///
    /// The exposure and turnover ledgers are untouched: a cancel
    /// removes an order, not a position.
    pub fn clear_resting(&mut self) {
        self.resting = [RestingOrder::free(); LEDGER_RESTING];
        self.resting_by_slot = [0; EXEC_SLOTS];
    }

    /// A live cancel was accepted by the arm. Stops tracking it.
    ///
    /// A key that matches nothing is silent: the arm returns
    /// `NoSuchOrder` for an order that was already gone, and the
    /// router forwards that to the member — there is nothing here to
    /// report that the caller does not already know.
    pub fn on_cancel(&mut self, client_oid: u64, slot: usize) {
        if let Found::One(i) = self.find(client_oid, slot) {
            self.release(i);
        }
    }

    /// A live modify was accepted by the arm. **The count does not
    /// change** — LAW E-7 says a requote is one order replaced in
    /// place — but the id and the size both do, and a ledger that
    /// kept the old id would fail to match the replacement's fills.
    pub fn on_modify(
        &mut self,
        prev_client_oid: u64,
        client_oid: u64,
        slot: usize,
        sym: SymbolId,
        qty_1e6: i64,
        buy: bool,
    ) {
        match self.find(prev_client_oid, slot) {
            Found::One(i) => {
                self.resting[i].client_oid = client_oid;
                self.resting[i].remaining_1e6 = qty_1e6;
                self.resting[i].sym = sym;
                self.resting[i].side = resting_side(buy);
            }
            // The arm accepted a modify of an order the ledger is not
            // holding. Tracking the replacement is the conservative
            // reading: it IS working at the venue, and the count that
            // matters is the one that does not run low.
            //
            // **`sym` is carried, not `SYMBOL_ID_NONE`.** A row with
            // no symbol matches no leg, so `drop_resting_on` could
            // never release it and no roll — no LAW E-8 cancel-all —
            // would ever retire it. It would survive the whole boot,
            // leaking one place in a shared table per unmatched
            // modify, which is the very condition that makes the table
            // fill.
            Found::None => self.on_submit(client_oid, slot, sym, qty_1e6, buy),
            // **Two rows under one key: leave the table alone.** Same
            // ruling as `consume_resting`, and it has to be the same
            // one: adding a THIRD row under an already-ambiguous key
            // is the opposite of "decrementing an arbitrary one is a
            // guess". `find` counts it.
            Found::Many => {}
        }
    }

    /// **BX3 (risk review) — what `slot` has WORKING on `sym` on one side**,
    /// ×1e6: the remaining quantity of its resting orders there, less the
    /// order a modify replaces (`exclude`). The instrument exit test reads
    /// it: N working exits of the whole position are one exit, not N.
    ///
    /// One pass over the table by reference, like [`Self::find`]; asked
    /// only for an order that opposes a position on an instrument row.
    fn working_1e6(&self, slot: usize, sym: SymbolId, buy: bool, exclude: Option<u64>) -> i64 {
        let side = resting_side(buy);
        let (skip, skip_oid) = match exclude {
            Some(oid) => (true, oid),
            None => (false, 0),
        };
        let mut total = 0i64;
        let mut i = 0usize;
        while i < LEDGER_RESTING {
            let r = &self.resting[i];
            i += 1;
            if r.live == 1
                && r.slot as usize == slot
                && r.sym == sym
                && r.side == side
                && !(skip && r.client_oid == skip_oid)
            {
                total = total.saturating_add(r.remaining_1e6.max(0));
            }
        }
        total
    }

    /// Resolve `(client_oid, slot)` against the resting table.
    ///
    /// Three outcomes, not two. `on_modify` must tell an ABSENT key
    /// from an AMBIGUOUS one — it tracks the replacement for the
    /// first and refuses to touch the table for the second — and a
    /// function returning `Option` collapses exactly that
    /// distinction, which is how the two cases came to share a branch
    /// that was right for only one of them.
    fn find(&mut self, client_oid: u64, slot: usize) -> Found {
        if slot >= EXEC_SLOTS {
            return Found::None;
        }
        let mut found = LEDGER_RESTING;
        let mut n = 0usize;
        let mut i = 0usize;
        while i < LEDGER_RESTING {
            // By reference: 24 B × 512 rows per venue fill would be a
            // 12 KiB memcpy for a three-field compare.
            let r = &self.resting[i];
            if r.live == 1 && r.client_oid == client_oid && r.slot as usize == slot {
                n += 1;
                if found == LEDGER_RESTING {
                    found = i;
                }
            }
            i += 1;
        }
        if n > 1 {
            self.counters.resting_ambiguous =
                self.counters.resting_ambiguous.saturating_add(1);
            return Found::Many;
        }
        if n == 0 {
            return Found::None;
        }
        Found::One(found)
    }

    #[inline]
    fn release(&mut self, i: usize) {
        let slot = self.resting[i].slot as usize;
        self.resting[i] = RestingOrder::free();
        if slot < EXEC_SLOTS {
            self.resting_by_slot[slot] = self.resting_by_slot[slot].saturating_sub(1);
        }
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty, Side, FILL_ORIGIN_PAPER};

    /// Hyperliquid namespace, so the symbols look like the ones a roll
    /// really carries.
    const VENUE: u8 = 4;
    const SLOT: usize = 3;
    /// The ingress family index every single-family test binds.
    const FAM: usize = 0;

    const fn sym(ord: u32) -> SymbolId {
        ((VENUE as u32) << core_types::SYMBOL_VENUE_SHIFT) | ord
    }

    /// 2026-09-19T00:00:01Z, well inside one day epoch.
    const T0: u64 = 1_789_776_001_000_000_000;

    fn fill(at: u64, s: SymbolId, buy: bool, px_1e6: i64, qty_1e6: i64, oid: u64) -> Fill {
        Fill::new(
            at,
            s,
            if buy { Side::Bid } else { Side::Ask },
            Price::from_raw(px_1e6),
            Qty::from_raw(qty_1e6),
            oid,
        )
        .with_attribution(SLOT as u8, FILL_ORIGIN_VENUE)
    }

    /// The anchor every test builds its ledger with. `T0` is both the
    /// monotonic and the wall stamp here, so `wall_of(T0) == T0` and a
    /// test can use one constant for both clocks. Production cannot —
    /// see `Ledger::anchor` — which is why
    /// `the_two_clocks_do_not_thrash_the_day_epoch` builds the anchor
    /// the way boot does instead.
    fn anchor() -> WallAnchor {
        WallAnchor::new(T0, T0)
    }

    /// Family 0's first instance, bound and SEEDED.
    fn bound() -> Ledger {
        let mut l = Ledger::new(anchor());
        l.mark_seeded();
        l.bind(VENUE, FAM, 20_182, sym(100));
        l
    }

    // -----------------------------------------------------------------
    // binding
    // -----------------------------------------------------------------

    #[test]
    fn a_roll_binds_both_legs_and_the_no_leg_is_the_next_ordinal() {
        let l = bound();
        assert_eq!(l.counters().binds, 1);
        assert_eq!(l.position_1e6(SLOT, 20_182), Some((0, 0)));
        // Both legs reachable: a fill on either must place.
        let mut l = l;
        l.book_fill(&fill(T0, sym(100), true, 500_000, 2_000_000, 1));
        l.book_fill(&fill(T0, sym(101), true, 500_000, 3_000_000, 2));
        assert_eq!(l.position_1e6(SLOT, 20_182), Some((2_000_000, 3_000_000)));
        assert_eq!(l.counters().fills_unbound, 0);
    }

    #[test]
    fn outcome_zero_and_a_none_symbol_are_refused_not_bound() {
        let mut l = Ledger::new(anchor());
        l.bind(VENUE, FAM, 0, sym(100));
        l.bind(VENUE, FAM, 20_182, SYMBOL_ID_NONE);
        assert_eq!(l.counters().binds, 0);
        assert_eq!(l.counters().binds_refused, 2);
    }

    #[test]
    fn a_no_leg_that_would_carry_out_of_the_venue_namespace_is_refused() {
        // The ordinal field is 24 bits. `+ 1` on the last ordinal
        // carries into the VENUE byte and binds a leg in another
        // venue's namespace — the same unchecked add
        // `exec_hyperliquid`'s roll handler refuses.
        let mut l = Ledger::new(anchor());
        l.bind(VENUE, FAM, 20_182, sym(core_types::SYMBOL_ORDINAL_MASK));
        assert_eq!(l.counters().binds, 0);
        assert_eq!(l.counters().binds_refused, 1);
    }

    #[test]
    fn the_table_refuses_rather_than_evicting_when_full() {
        let mut l = Ledger::new(anchor());
        for i in 0..LEDGER_ROWS {
            l.bind(VENUE, i, 1000 + i as u32, sym(100 + (i as u32) * 2));
        }
        assert_eq!(l.counters().binds, LEDGER_ROWS as u64);
        // A family the table has no room for. (A family index at or
        // above LEDGER_ROWS is refused by the bound check; this one is
        // refused because every row is taken.)
        l.bind(VENUE, LEDGER_ROWS - 1 + 1, 9999, sym(900));
        assert_eq!(l.counters().binds_refused, 1);
        // The FIRST binding is still there. An evicting table would
        // book a real position against the wrong market.
        assert!(l.position_1e6(SLOT, 1000).is_some());
    }

    /// **The defect that keying on outcome id would have shipped.**
    ///
    /// A family rolls every fifteen minutes and every roll carries a
    /// BRAND-NEW outcome id — `ingress_hyperliquid::perform_roll`
    /// rebinds the family to a fresh `HlOutcomeSpec`. A table keyed on
    /// outcome id takes a new row each time and is full within two
    /// rolls of eight families, after which every bind is refused,
    /// every fill is `fills_unbound`, and `cap_instance` stops seeing
    /// the position it exists to bound. Keyed on FAMILY, the row is
    /// replaced and the table never grows.
    #[test]
    fn successive_instances_of_a_family_reuse_one_row() {
        let mut l = Ledger::new(anchor());
        l.mark_seeded();
        // Eight families, a hundred rolls each — a day of BIN15.
        for roll in 0..100u32 {
            for fam in 0..8usize {
                let outcome = 20_000 + roll * 8 + fam as u32;
                l.bind(VENUE, fam, outcome, sym(1_000 + roll * 32 + (fam as u32) * 2));
            }
        }
        assert_eq!(
            l.counters().binds_refused,
            0,
            "the table filled — rows are being keyed on the instance, not the family"
        );
        // And there is still room for eight more families.
        for fam in 8..LEDGER_ROWS {
            l.bind(VENUE, fam, 90_000 + fam as u32, sym(500_000 + (fam as u32) * 2));
        }
        assert_eq!(l.counters().binds_refused, 0);
    }

    #[test]
    fn a_new_instance_of_a_family_does_not_inherit_the_old_position() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 900_000, 5_000_000, 1));
        assert_eq!(l.slot_exposure_1e6(SLOT), 5_000_000);
        // The roll of the successor — a NEW outcome id on the SAME
        // family, which is what a real roll carries.
        l.bind(VENUE, FAM, 20_183, sym(200));
        assert_eq!(
            l.slot_exposure_1e6(SLOT),
            0,
            "cap_instance means THIS instance"
        );
        assert_eq!(l.counters().instances_cleared, 1);
    }

    /// **A repeated CREATED frame for the LIVE instance binds nothing.**
    /// The venue re-sends `outcomeCreated` on a reconnect; the ledger
    /// must not zero a real position on it.
    #[test]
    fn a_repeated_created_frame_for_the_live_instance_keeps_the_position() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 900_000, 5_000_000, 1));
        assert_eq!(l.slot_exposure_1e6(SLOT), 5_000_000);
        let cleared = l.counters().instances_cleared;
        l.bind(VENUE, FAM, 20_182, sym(100));
        assert_eq!(l.slot_exposure_1e6(SLOT), 5_000_000, "a repeat is not a roll");
        assert_eq!(l.counters().instances_cleared, cleared);
        // The SUCCESSOR still rolls the row.
        l.bind(VENUE, FAM, 20_183, sym(200));
        assert_eq!(l.slot_exposure_1e6(SLOT), 0);
        assert_eq!(l.counters().instances_cleared, cleared + 1);
    }

    /// **A settle drops the orders and keeps everything else.**
    ///
    /// An earlier cut zeroed the position here. Two things were wrong
    /// with that: the contracts ARE still held until the settlement
    /// cash arrives, so it reported a flat book over a real one; and
    /// it made every settlement fill land on a zero leg and trip
    /// `sells_below_zero` — the counter added to report the router
    /// DISAGREEING with the venue — hundreds of times a day, burying
    /// the one signal it exists to carry.
    #[test]
    fn a_settle_drops_the_resting_orders_and_leaves_the_position_to_the_fill() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 900_000, 5_000_000, 1));
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        assert_eq!(l.slot_exposure_1e6(SLOT), 5_000_000);

        l.settle(VENUE, FAM, 20_182);
        assert_eq!(l.slot_resting(SLOT), 0, "LAW E-8: the roll cancelled them");
        assert_eq!(
            l.slot_exposure_1e6(SLOT),
            5_000_000,
            "the contracts are held until the settlement pays out"
        );
        assert_eq!(l.counters().instances_cleared, 1);

        // The settlement itself: a sale of the whole position, which
        // the venue prints Ask-side for both legs.
        l.book_fill(&fill(T0, sym(100), false, 1_000_000, 5_000_000, 2));
        assert_eq!(l.slot_exposure_1e6(SLOT), 0);
        assert_eq!(l.counters().fills_unbound, 0, "the settlement fill landed");
        assert_eq!(
            l.counters().sells_below_zero,
            0,
            "a settlement is not a disagreement with the venue"
        );
    }

    #[test]
    fn a_settlement_that_sells_more_than_we_booked_is_reported() {
        // The most informative moment to hear that a buy fill never
        // reached lane 3: the venue closes a position bigger than the
        // one this ledger is counting. Suppressing the floor on a
        // settled row would swallow exactly this.
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 500_000, 3_000_000, 1));
        l.settle(VENUE, FAM, 20_182);
        l.book_fill(&fill(T0, sym(100), false, 1_000_000, 8_000_000, 2));
        assert_eq!(l.counters().sells_below_zero, 1);
    }

    #[test]
    fn a_settlement_never_charges_the_day_cap() {
        // A settlement is the venue paying out, not the member
        // committing capital. Ask-side in practice — guarded rather
        // than assumed, because a budget spent on money that never
        // left is unrecoverable until midnight.
        let mut l = bound();
        l.settle(VENUE, FAM, 20_182);
        let before = l.slot_day_turnover_1e6(SLOT);
        l.book_fill(&fill(T0, sym(100), true, 1_000_000, 5_000_000, 2));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), before);
    }

    #[test]
    fn a_duplicate_settle_is_a_no_op() {
        let mut l = bound();
        l.settle(VENUE, FAM, 20_182);
        l.settle(VENUE, FAM, 20_182);
        l.settle(VENUE, FAM, 20_182);
        assert_eq!(l.counters().instances_cleared, 1);
    }

    #[test]
    fn a_settle_naming_no_outcome_is_refused_rather_than_clearing_a_successor() {
        // The weaker family key would clear whatever the family holds
        // NOW, which after a reordered frame is the successor's live
        // position — the very thing the outcome-first search exists to
        // prevent.
        let mut l = bound();
        l.bind(VENUE, FAM, 20_183, sym(200)); // the successor
        l.book_fill(&fill(T0, sym(200), true, 500_000, 4_000_000, 1));
        l.settle(VENUE, FAM, 0);
        assert_eq!(l.counters().settles_unmatched, 1);
        assert_eq!(
            l.slot_exposure_1e6(SLOT),
            4_000_000,
            "a late settle must not clear a successor that has bound"
        );
    }

    #[test]
    fn two_venues_do_not_share_a_family_row() {
        // A family index is only unique within the ingress that
        // issued it. Keyed on the byte alone, the second venue's bind
        // retires the first venue's live position and exposure reads
        // zero — fail open, from a key collision no row count fixes.
        let mut l = Ledger::new(anchor());
        l.mark_seeded();
        l.bind(VENUE, 0, 1, sym(100));
        l.book_fill(&fill(T0, sym(100), true, 500_000, 4_000_000, 1));
        assert_eq!(l.slot_exposure_1e6(SLOT), 4_000_000);
        l.bind(VENUE + 1, 0, 2, ((VENUE as u32 + 1) << 24) | 300);
        assert_eq!(
            l.slot_exposure_1e6(SLOT),
            4_000_000,
            "another venue's family 0 evicted ours"
        );
    }

    #[test]
    fn an_unstamped_order_clock_does_not_roll_the_day() {
        // `roll_day` guards a zero WALL stamp, but the conversion runs
        // first and `wall_of(0)` is `wall0 - mono0` — a plausible wall
        // time in a DIFFERENT epoch. The guard has to be on the input.
        const MONO0: u64 = 11 * 86_400_000_000_000;
        let mut l = Ledger::new(WallAnchor::new(MONO0, T0));
        l.mark_seeded();
        l.bind(VENUE, FAM, 20_182, sym(100));
        l.book_fill(&fill(T0, sym(100), true, 400_000, 5_000_000, 1));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 2_000_000);
        l.observe_mono_clock(0);
        assert_eq!(l.counters().day_rollovers, 0);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 2_000_000);
    }

    #[test]
    fn a_settle_for_a_family_the_ledger_does_not_hold_is_counted() {
        let mut l = bound();
        l.settle(VENUE, FAM, 99_999);
        assert_eq!(l.counters().settles_unmatched, 1);
        assert_eq!(l.counters().instances_cleared, 0);
    }

    // -----------------------------------------------------------------
    // exposure
    // -----------------------------------------------------------------

    #[test]
    fn equal_legs_net_to_nothing() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 400_000, 4_000_000, 1));
        l.book_fill(&fill(T0, sym(101), true, 600_000, 4_000_000, 2));
        assert_eq!(
            l.slot_exposure_1e6(SLOT),
            0,
            "yes+no of an outcome is riskless collateral"
        );
    }

    #[test]
    fn two_outcomes_do_not_net_against_each_other() {
        let mut l = Ledger::new(anchor());
        l.bind(VENUE, 0, 1, sym(100));
        l.bind(VENUE, 1, 2, sym(200));
        // Long Yes on outcome 1, long No on outcome 2. `|Σyes − Σno|`
        // would report ZERO here; the truth is 8 contracts at stake on
        // two independent events.
        l.book_fill(&fill(T0, sym(100), true, 500_000, 4_000_000, 1));
        l.book_fill(&fill(T0, sym(201), true, 500_000, 4_000_000, 2));
        assert_eq!(l.slot_exposure_1e6(SLOT), 8_000_000);
    }

    #[test]
    fn one_slots_fills_do_not_move_another_slots_exposure() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 500_000, 4_000_000, 1));
        assert_eq!(l.slot_exposure_1e6(SLOT), 4_000_000);
        assert_eq!(l.slot_exposure_1e6(SLOT + 1), 0);
    }

    #[test]
    fn a_sell_reduces_a_leg_but_never_takes_it_negative() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 500_000, 3_000_000, 1));
        l.book_fill(&fill(T0, sym(100), false, 500_000, 9_000_000, 2));
        assert_eq!(
            l.position_1e6(SLOT, 20_182),
            Some((0, 0)),
            "the venue has no short; a negative leg would INFLATE |yes-no|"
        );
        assert_eq!(l.slot_exposure_1e6(SLOT), 0);
    }

    #[test]
    fn a_paper_fill_never_reaches_a_live_ledger() {
        let mut l = bound();
        let f = fill(T0, sym(100), true, 500_000, 4_000_000, 1)
            .with_attribution(SLOT as u8, FILL_ORIGIN_PAPER);
        l.book_fill(&f);
        assert_eq!(l.slot_exposure_1e6(SLOT), 0);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 0);
        assert_eq!(l.counters().fills_booked, 0);
    }

    #[test]
    fn an_unattributed_fill_is_counted_and_booked_nowhere() {
        let mut l = bound();
        let f = fill(T0, sym(100), true, 500_000, 4_000_000, 1)
            .with_attribution(core_types::STRATEGY_ID_NONE, FILL_ORIGIN_VENUE);
        l.book_fill(&f);
        assert_eq!(l.counters().fills_unattributed, 1);
        assert_eq!(l.counters().fills_booked, 0);
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            assert_eq!(l.slot_exposure_1e6(i), 0);
            i += 1;
        }
    }

    #[test]
    fn a_fill_on_an_unbound_leg_is_counted_but_its_money_still_is() {
        let mut l = Ledger::new(anchor());
        l.book_fill(&fill(T0, sym(100), true, 500_000, 4_000_000, 1));
        assert_eq!(l.counters().fills_unbound, 1);
        assert_eq!(l.slot_exposure_1e6(SLOT), 0, "nowhere to put the position");
        assert_eq!(
            l.slot_day_turnover_1e6(SLOT),
            2_000_000,
            "the money left the account whether or not we could place it"
        );
    }

    // -----------------------------------------------------------------
    // the projection
    // -----------------------------------------------------------------

    #[test]
    fn the_projection_of_a_first_order_is_the_order_itself() {
        // The whole reason the clamp projects: a flat slot passes any
        // test on its CURRENT exposure, so a cap read off the position
        // would never see a slot's first order.
        let l = bound();
        assert_eq!(l.slot_exposure_1e6(SLOT), 0);
        assert_eq!(
            l.projected_exposure_1e6(SLOT, sym(100), 7_000_000, true),
            7_000_000
        );
    }

    #[test]
    fn the_exposure_clamp_never_refuses_an_order_that_reduces_it() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 500_000, 6_000_000, 1));
        let now = l.slot_exposure_1e6(SLOT);
        assert_eq!(now, 6_000_000);
        // Selling the long leg.
        assert!(l.projected_exposure_1e6(SLOT, sym(100), 4_000_000, false) < now);
        // Buying the SHORT leg — also risk-reducing, and the reason
        // the projection is exact rather than "every order adds".
        assert!(l.projected_exposure_1e6(SLOT, sym(101), 4_000_000, true) < now);
        // And an over-sized close cannot go below flat.
        assert_eq!(
            l.projected_exposure_1e6(SLOT, sym(100), 99_000_000, false),
            0
        );
    }

    #[test]
    fn an_unbound_leg_projects_conservatively_for_a_buy_and_not_at_all_for_a_sell() {
        let l = Ledger::new(anchor());
        assert_eq!(l.projected_exposure_1e6(SLOT, sym(100), 5_000_000, true), 5_000_000);
        assert_eq!(l.projected_exposure_1e6(SLOT, sym(100), 5_000_000, false), 0);
    }

    #[test]
    fn the_projection_and_the_booking_apply_the_same_rule() {
        // Not a tautology: they are two call sites of `apply`, and the
        // test fails the moment one of them grows its own arithmetic.
        // A projection that rounded differently from the booking would
        // let an order pass a cap it then breached on landing.
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 500_000, 3_000_000, 1));
        for (qty, buy) in [(2_000_000i64, true), (2_000_000, false), (9_000_000, false)] {
            let projected = l.projected_exposure_1e6(SLOT, sym(100), qty, buy);
            let mut probe = Ledger::new(anchor());
            probe.bind(VENUE, FAM, 20_182, sym(100));
            probe.book_fill(&fill(T0, sym(100), true, 500_000, 3_000_000, 1));
            probe.book_fill(&fill(T0, sym(100), buy, 500_000, qty, 2));
            assert_eq!(
                projected,
                probe.slot_exposure_1e6(SLOT),
                "projection disagreed with the booking for qty={qty} buy={buy}"
            );
        }
    }

    // -----------------------------------------------------------------
    // turnover
    // -----------------------------------------------------------------

    #[test]
    fn turnover_counts_buys_and_ignores_sells() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 400_000, 5_000_000, 1));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 2_000_000, "$2");
        l.book_fill(&fill(T0, sym(100), false, 900_000, 5_000_000, 2));
        assert_eq!(
            l.slot_day_turnover_1e6(SLOT),
            2_000_000,
            "selling back does not un-commit what was committed"
        );
    }

    #[test]
    fn turnover_rolls_at_midnight_utc_and_not_before() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 400_000, 5_000_000, 1));
        // 23 hours later — same UTC day.
        l.book_fill(&fill(
            T0 + 23 * 3_600_000_000_000,
            sym(100),
            true,
            400_000,
            5_000_000,
            2,
        ));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 4_000_000);
        assert_eq!(l.counters().day_rollovers, 0);
        // Across 00:00Z.
        l.book_fill(&fill(
            T0 + 24 * 3_600_000_000_000,
            sym(100),
            true,
            400_000,
            5_000_000,
            3,
        ));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 2_000_000, "a fresh day");
        assert_eq!(l.counters().day_rollovers, 1);
    }

    #[test]
    fn the_clock_rolls_the_day_even_when_nothing_fills() {
        // The reason `observe_clock` exists. A boot that fills nothing
        // after midnight would otherwise judge the new day's first
        // order against the old day's turnover — the cap would stay
        // shut until a fill happened to open it.
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 400_000, 5_000_000, 1));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 2_000_000);
        l.observe_mono_clock(T0 + 24 * 3_600_000_000_000);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 0);
        assert_eq!(l.counters().day_rollovers, 1);
    }

    #[test]
    fn an_unstamped_record_does_not_adopt_epoch_zero() {
        // Adopting 0 would make the next real timestamp look like a
        // rollover and wipe a day of turnover on the first fill.
        let mut l = bound();
        l.observe_mono_clock(0);
        l.book_fill(&fill(T0, sym(100), true, 400_000, 5_000_000, 1));
        l.observe_mono_clock(0);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 2_000_000);
        assert_eq!(l.counters().day_rollovers, 0);
    }

    // -----------------------------------------------------------------
    // resting
    // -----------------------------------------------------------------

    #[test]
    fn a_submit_counts_and_a_cancel_uncounts() {
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.on_submit(12, SLOT, sym(100), 5_000_000, true);
        assert_eq!(l.slot_resting(SLOT), 2);
        l.on_cancel(11, SLOT);
        assert_eq!(l.slot_resting(SLOT), 1);
    }

    #[test]
    fn the_key_is_the_oid_and_the_slot_together() {
        // Every member counts its own client_oids from 1, so two slots
        // share oid 11 routinely. Keying on the oid alone is the exact
        // defect the E5 commit-3 review found in the paper matcher.
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.on_submit(11, SLOT + 1, sym(100), 5_000_000, true);
        l.on_cancel(11, SLOT + 1);
        assert_eq!(l.slot_resting(SLOT), 1, "the other slot's cancel took mine");
        assert_eq!(l.slot_resting(SLOT + 1), 0);
    }

    #[test]
    fn a_modify_keeps_the_count_and_moves_the_id() {
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.on_modify(11, 12, SLOT, sym(100), 7_000_000, true);
        assert_eq!(l.slot_resting(SLOT), 1, "LAW E-7: replaced in place");
        // The replacement's fills must match: the OLD id no longer does.
        l.book_fill(&fill(T0, sym(100), true, 500_000, 7_000_000, 11));
        assert_eq!(l.counters().resting_unmatched, 1);
        assert_eq!(l.slot_resting(SLOT), 1);
        l.book_fill(&fill(T0, sym(100), true, 500_000, 7_000_000, 12));
        assert_eq!(l.slot_resting(SLOT), 0);
    }

    #[test]
    fn a_full_fill_retires_the_order_and_a_partial_one_does_not() {
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.book_fill(&fill(T0, sym(100), true, 500_000, 2_000_000, 11));
        assert_eq!(l.slot_resting(SLOT), 1, "3 contracts still working");
        l.book_fill(&fill(T0, sym(100), true, 500_000, 3_000_000, 11));
        assert_eq!(l.slot_resting(SLOT), 0);
    }

    #[test]
    fn an_ambiguous_key_leaves_the_count_alone_and_says_so() {
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        assert_eq!(l.slot_resting(SLOT), 2);
        l.book_fill(&fill(T0, sym(100), true, 500_000, 5_000_000, 11));
        assert_eq!(l.counters().resting_ambiguous, 1);
        assert_eq!(l.slot_resting(SLOT), 2, "retiring an arbitrary one is a guess");
    }

    #[test]
    fn an_ambiguous_key_does_not_stop_the_money_ledgers() {
        // The separation the module docs claim, asserted. A defect in
        // order matching must not be able to blind the caps.
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.book_fill(&fill(T0, sym(100), true, 500_000, 5_000_000, 11));
        assert_eq!(l.counters().resting_ambiguous, 1);
        assert_eq!(l.slot_exposure_1e6(SLOT), 5_000_000);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 2_500_000);
    }

    #[test]
    fn a_roll_drops_the_orders_resting_on_the_instance_that_ended() {
        // LAW E-8 — the roll sends a real cancel-all, so an order on a
        // retired leg is gone. Keeping them would leak the count
        // upward by one instance's quotes every roll, and
        // `max_open_orders` would eventually refuse everything for
        // ever.
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.on_submit(12, SLOT, sym(101), 5_000_000, true);
        assert_eq!(l.slot_resting(SLOT), 2);
        l.settle(VENUE, FAM, 20_182);
        assert_eq!(l.slot_resting(SLOT), 0);
    }

    #[test]
    fn a_roll_leaves_another_outcomes_orders_alone() {
        let mut l = Ledger::new(anchor());
        l.bind(VENUE, 0, 1, sym(100));
        l.bind(VENUE, 1, 2, sym(200));
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.on_submit(12, SLOT, sym(200), 5_000_000, true);
        l.settle(VENUE, 0, 1);
        assert_eq!(l.slot_resting(SLOT), 1);
    }

    #[test]
    fn the_resting_table_refuses_rather_than_overwriting_when_full() {
        let mut l = bound();
        for i in 0..LEDGER_RESTING {
            l.on_submit(i as u64 + 1, SLOT, sym(100), 1, true);
        }
        assert_eq!(l.slot_resting(SLOT), LEDGER_RESTING as u32);
        l.on_submit(999_999, SLOT, sym(100), 1, true);
        assert_eq!(l.counters().resting_full, 1);
        // No ROW was taken from an existing order — the first one is
        // still there to be cancelled. (The COUNT rises anyway, which
        // is `a_full_resting_table_still_counts_the_order_that_did_not_fit`.)
        l.on_cancel(1, SLOT);
        assert_eq!(l.slot_resting(SLOT), LEDGER_RESTING as u32);
    }

    #[test]
    fn an_out_of_range_slot_answers_zero_rather_than_reading_past_the_array() {
        let mut l = bound();
        l.on_submit(11, EXEC_SLOTS, sym(100), 5_000_000, true);
        assert_eq!(l.slot_resting(EXEC_SLOTS), 0);
        assert_eq!(l.slot_exposure_1e6(EXEC_SLOTS), 0);
        assert_eq!(l.slot_day_turnover_1e6(EXEC_SLOTS), 0);
        assert_eq!(l.position_1e6(EXEC_SLOTS, 20_182), None);
        assert_eq!(l.projected_exposure_1e6(EXEC_SLOTS, sym(100), 9, true), 0);
    }

    /// **The two clocks, and the day epoch between them.**
    ///
    /// `core_time::now_ns` is `CLOCK_MONOTONIC_RAW` — ns since an
    /// arbitrary origin — and that is what stamps an `Order`. A VENUE
    /// `Fill` is stamped by the Hyperliquid arm from `SystemTime` and
    /// is ns since 1970. The two differ by DECADES.
    ///
    /// Fed to one `wall_ns / DAY_NS` epoch, every alternation between
    /// an order and a fill looks like a midnight crossing and wipes
    /// the day's turnover — `cap_day` disabled outright, failing open,
    /// with nothing to show but a climbing `day_rollovers`.
    ///
    /// Every other test here builds an anchor where the two stamps
    /// coincide, so none of them can catch it. This one builds the
    /// anchor the way boot does, with a realistic monotonic origin,
    /// and alternates the two sources inside one day.
    #[test]
    fn the_two_clocks_do_not_thrash_the_day_epoch() {
        // A machine up for eleven days: monotonic ns nowhere near
        // Unix ns.
        const MONO0: u64 = 11 * 86_400_000_000_000;
        let mut l = Ledger::new(WallAnchor::new(MONO0, T0));
        l.mark_seeded();
        l.bind(VENUE, FAM, 20_182, sym(100));

        let mut i = 0u64;
        while i < 64 {
            // An order's clock: MONOTONIC, minutes apart.
            l.observe_mono_clock(MONO0 + i * 60_000_000_000);
            // A venue fill's clock: WALL, the same instants.
            l.book_fill(&fill(
                T0 + i * 60_000_000_000,
                sym(100),
                true,
                400_000,
                1_000_000,
                i + 1,
            ));
            i += 1;
        }
        assert_eq!(
            l.counters().day_rollovers,
            0,
            "the epoch thrashed — two clocks are reaching one day field"
        );
        assert_eq!(
            l.slot_day_turnover_1e6(SLOT),
            64 * 400_000,
            "turnover was wiped by a phantom midnight"
        );
    }

    /// **S7-L1 (gap A).** A restart's ledger starts the day at zero;
    /// the venue's figure lifts it. Adoption never LOWERS the turnover
    /// (a local fill the venue had not caught up with stays counted),
    /// is idempotent, rolls forward on a later day, and ignores an
    /// earlier day rather than rolling backwards over it.
    #[test]
    fn the_venue_day_spend_lifts_the_turnover_and_never_lowers_it() {
        let day = T0 / DAY_NS;
        let mut l = bound();
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 0, "a fresh boot's day");
        l.adopt_venue_day_turnover(SLOT, day, 30_000_000);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 30_000_000, "the venue's day");
        l.adopt_venue_day_turnover(SLOT, day, 30_000_000);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 30_000_000, "idempotent");

        // A buy booked here that the venue's answer has not caught up
        // with: the stale figure must not un-count it.
        l.book_fill(&fill(T0 + 1, sym(100), true, 500_000, 4_000_000, 1));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 32_000_000);
        l.adopt_venue_day_turnover(SLOT, day, 30_000_000);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 32_000_000, "never lowered");

        // Another slot's figure is that slot's.
        l.adopt_venue_day_turnover(1, day, 7_000_000);
        assert_eq!(l.slot_day_turnover_1e6(1), 7_000_000);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 32_000_000);

        // An EARLIER day is a stale answer: ignored, not a rollback.
        let rollovers = l.counters().day_rollovers;
        l.adopt_venue_day_turnover(SLOT, day - 1, 99_000_000);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 32_000_000);
        assert_eq!(l.counters().day_rollovers, rollovers);

        // The next day's first read rolls the day and adopts it.
        l.adopt_venue_day_turnover(SLOT, day + 1, 5_000_000);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 5_000_000);
        assert_eq!(l.slot_day_turnover_1e6(1), 0, "the roll zeroed every slot");
        assert_eq!(l.counters().day_rollovers, rollovers + 1);

        // Out of range and day 0 are nothing.
        l.adopt_venue_day_turnover(EXEC_SLOTS, day + 1, 1);
        l.adopt_venue_day_turnover(SLOT, 0, 1_000_000_000);
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 5_000_000);
    }

    /// **S7-L1 — the day only rolls forward.** A fill stamped before
    /// midnight that is booked after the day has rolled counts in the
    /// day it lands in; it never rolls the day back over the new day's
    /// turnover.
    #[test]
    fn a_late_stamp_never_rolls_the_day_back() {
        let mut l = bound();
        l.book_fill(&fill(T0 + DAY_NS, sym(100), true, 500_000, 2_000_000, 1));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 1_000_000);
        let rolls = l.counters().day_rollovers;
        l.book_fill(&fill(T0, sym(100), true, 500_000, 2_000_000, 2));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 2_000_000, "counted in the day it landed in");
        assert_eq!(l.counters().day_rollovers, rolls, "and nothing rolled");
    }

    #[test]
    fn an_unseeded_ledger_says_so() {
        let l = Ledger::new(anchor());
        assert!(!l.is_seeded(), "a fresh ledger knows nothing of the venue");
        let mut l = l;
        l.mark_slot_seeded(0);
        assert!(l.is_seeded() && l.is_slot_seeded(0) && !l.is_slot_seeded(3));
        l.mark_slot_seeded(EXEC_SLOTS);
        assert!(!l.is_slot_seeded(EXEC_SLOTS), "no slot past the table");
        l.mark_seeded();
        assert!(l.is_seeded() && l.is_slot_seeded(3) && l.is_slot_seeded(7));
    }

    #[test]
    fn a_modify_of_an_ambiguous_key_does_not_add_a_third_row() {
        // The mirror of `an_ambiguous_key_leaves_the_count_alone_and_says_so`.
        // Two rows under one key are already a guess; adding a third
        // under the same key is the opposite of the ruling
        // `consume_resting` follows, and it was what the first cut of
        // `on_modify` did, because it could not tell ABSENT from
        // AMBIGUOUS.
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        l.on_submit(11, SLOT, sym(100), 5_000_000, true);
        assert_eq!(l.slot_resting(SLOT), 2);
        l.on_modify(11, 12, SLOT, sym(100), 5_000_000, true);
        assert_eq!(l.slot_resting(SLOT), 2, "a third row was created");
        assert_eq!(l.counters().resting_ambiguous, 1);
    }

    #[test]
    fn a_tracked_replacement_is_still_droppable_by_a_roll() {
        // The row `on_modify`'s unmatched branch creates must carry a
        // real `sym`. With `SYMBOL_ID_NONE` it matches no leg, so
        // `drop_resting_on` can never release it and no roll — no LAW
        // E-8 cancel-all — ever retires it: one place leaked out of a
        // shared table per unmatched modify, for the life of the boot.
        let mut l = bound();
        l.on_modify(11, 12, SLOT, sym(100), 5_000_000, true);
        assert_eq!(l.slot_resting(SLOT), 1);
        l.settle(VENUE, FAM, 20_182);
        assert_eq!(l.slot_resting(SLOT), 0, "the phantom row survived the roll");
    }

    #[test]
    fn a_full_resting_table_still_counts_the_order_that_did_not_fit() {
        // Fail CLOSED. Freezing `resting_by_slot` here would leave
        // `max_open_orders` passing every later order for the rest of
        // the boot — the clamp switched off by the table it depends
        // on.
        let mut l = bound();
        for i in 0..LEDGER_RESTING {
            l.on_submit(i as u64 + 1, SLOT, sym(100), 1, true);
        }
        l.on_submit(999_999, SLOT, sym(100), 1, true);
        assert_eq!(l.counters().resting_full, 1);
        assert_eq!(
            l.slot_resting(SLOT),
            LEDGER_RESTING as u32 + 1,
            "the count must keep rising, not freeze"
        );
    }

    #[test]
    fn a_sell_that_would_go_below_zero_is_reported_not_just_absorbed() {
        // The only lossy arithmetic here, and the only tell it leaves.
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 500_000, 3_000_000, 1));
        l.book_fill(&fill(T0, sym(100), false, 500_000, 9_000_000, 2));
        assert_eq!(l.counters().sells_below_zero, 1);
        // An ordinary sell does not trip it.
        l.book_fill(&fill(T0, sym(100), true, 500_000, 3_000_000, 3));
        l.book_fill(&fill(T0, sym(100), false, 500_000, 1_000_000, 4));
        assert_eq!(l.counters().sells_below_zero, 1);
    }

    #[test]
    fn a_refused_roll_is_counted_with_the_other_refused_binds() {
        let mut l = bound();
        let before = l.counters().binds_refused;
        l.refuse_roll();
        assert_eq!(l.counters().binds_refused, before + 1);
    }

    #[test]
    fn a_modify_of_an_order_the_ledger_does_not_hold_starts_tracking_it() {
        // The arm accepted it, so it IS working at the venue. Tracking
        // the replacement is the reading that does not run the count
        // LOW — a count that under-reports is a clamp that lets
        // through the order it exists to stop.
        let mut l = bound();
        l.on_modify(11, 12, SLOT, sym(100), 5_000_000, true);
        assert_eq!(l.slot_resting(SLOT), 1);
    }

    // -----------------------------------------------------------------
    // BX3 — instrument rows
    // -----------------------------------------------------------------

    /// A Binance-namespace id.
    const fn bn(ord: u32) -> SymbolId {
        (1u32 << core_types::SYMBOL_VENUE_SHIFT) | ord
    }
    const E6: i64 = 1_000_000;
    const SPOT: SymbolId = bn(1);
    const LIN: SymbolId = bn(2);
    const INV: SymbolId = bn(3);
    /// BTC call, strike 100 000, unit 1, writable.
    const CALL: SymbolId = bn(4);
    /// DOGE put, strike 0.084, unit 1 000, not writable.
    const PUT: SymbolId = bn(5);

    /// A seeded ledger with one row per law.
    fn inst() -> Ledger {
        let mut l = Ledger::new(anchor());
        l.mark_seeded();
        l.bind_instrument(&InstrumentSpec::new(SPOT, LAW_SPOT, 0, 0, 0))
            .unwrap();
        l.bind_instrument(&InstrumentSpec::new(LIN, LAW_LINEAR, 0, 0, 0))
            .unwrap();
        l.bind_instrument(&InstrumentSpec::new(INV, LAW_INVERSE, 0, 100 * E6, 0))
            .unwrap();
        l.bind_instrument(&InstrumentSpec::new(
            CALL,
            LAW_OPTION,
            INST_WRITABLE | INST_CALL,
            E6,
            100_000 * E6,
        ))
        .unwrap();
        l.bind_instrument(&InstrumentSpec::new(PUT, LAW_OPTION, 0, 1_000 * E6, 84_000))
            .unwrap();
        l
    }

    /// BX6 (obligation 6): a mark with no index, or too far from it, is
    /// refused and counted; a stale one stops pricing (the last fill
    /// prices the row again) until a fresh one arrives.
    #[test]
    fn marks_are_sane_and_fresh_or_they_do_not_price() {
        let mut l = Ledger::new(anchor());
        l.bind_instrument(&InstrumentSpec::new(LIN, LAW_LINEAR, 0, E6, 0)).unwrap();
        let i = l.inst_find(LIN).unwrap();
        l.on_mark(LIN, 100 * E6, 0, 1);
        l.on_mark(LIN, 100 * E6, 89 * E6, 1);
        assert_eq!((l.inst[i].mark_1e6, l.counters().marks_refused), (0, 2));
        l.on_mark(LIN, 100 * E6, 91 * E6, 5);
        assert_eq!(l.inst[i].mark_1e6, 100 * E6, "9 % from the index is sane");
        l.expire_marks(5 + MARK_STALE_NS);
        assert_eq!(l.inst[i].mark_1e6, 100 * E6, "not stale yet");
        l.expire_marks(6 + MARK_STALE_NS);
        assert_eq!((l.inst[i].mark_1e6, l.counters().marks_expired), (0, 1));
        l.on_mark(LIN, 101 * E6, 101 * E6, 7 + MARK_STALE_NS);
        assert_eq!(l.inst[i].mark_1e6, 101 * E6, "a fresh mark prices again");
    }

    fn summary(s: SymbolId, mark_1e6: i64, index_1e6: i64) -> core_types::OptSummary {
        core_types::OptSummary::new(
            T0,
            core_types::VenueId::Binance,
            s,
            if mark_1e6 > 0 {
                core_types::OPT_SUMMARY_FLAG_MARK_PX
            } else {
                0
            },
            mark_1e6 * 1_000,
            0,
            index_1e6 * 1_000,
            0,
            0,
            0,
            0,
            0,
        )
    }

    /// Deterministic xorshift for the randomised properties below.
    fn rng(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    #[test]
    fn bind_instrument_refuses_duplicates_bad_specs_and_the_257th() {
        let mut l = Ledger::new(anchor());
        let bad = [
            InstrumentSpec::new(SYMBOL_ID_NONE, LAW_SPOT, 0, 0, 0),
            InstrumentSpec::new(bn(9), 0, 0, 0, 0),
            InstrumentSpec::new(bn(9), 5, 0, 0, 0),
            InstrumentSpec::new(bn(9), LAW_INVERSE, 0, 0, 0),
            InstrumentSpec::new(bn(9), LAW_OPTION, 0, E6, 0),
            InstrumentSpec::new(bn(9), LAW_OPTION, 0, 0, E6),
            InstrumentSpec::new(bn(9), LAW_LINEAR, INST_WRITABLE, 0, 0),
            InstrumentSpec::new(bn(9), LAW_SPOT, 0, 0, E6),
        ];
        for b in bad {
            assert_eq!(
                l.bind_instrument(&b),
                Err(InstrumentBindErr::BadSpec),
                "{b:?}"
            );
        }
        l.bind_instrument(&InstrumentSpec::new(bn(9), LAW_SPOT, 0, 0, 0))
            .unwrap();
        assert_eq!(
            l.bind_instrument(&InstrumentSpec::new(bn(9), LAW_LINEAR, 0, 0, 0)),
            Err(InstrumentBindErr::Duplicate)
        );
        let mut k = 10u32;
        while l.instruments_bound() < LEDGER_INSTRUMENTS as u32 {
            l.bind_instrument(&InstrumentSpec::new(bn(k), LAW_LINEAR, 0, 0, 0))
                .unwrap();
            k += 1;
        }
        assert_eq!(
            l.bind_instrument(&InstrumentSpec::new(bn(k), LAW_LINEAR, 0, 0, 0)),
            Err(InstrumentBindErr::Full)
        );
    }

    #[test]
    fn the_branchless_search_agrees_with_a_linear_scan() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut round = 0;
        while round < 40 {
            let mut l = Ledger::new(anchor());
            let n = (rng(&mut x) % LEDGER_INSTRUMENTS as u64) as usize;
            let mut bound: Vec<SymbolId> = Vec::new();
            while bound.len() < n {
                let id = (rng(&mut x) % 5_000) as u32 + 1;
                if l.bind_instrument(&InstrumentSpec::new(id, LAW_LINEAR, 0, 0, 0))
                    .is_ok()
                {
                    bound.push(id);
                }
            }
            let mut q = 0;
            while q < 2_000 {
                let id = (rng(&mut x) % 5_200) as u32;
                assert_eq!(l.has_instrument(id), bound.contains(&id), "id {id}");
                q += 1;
            }
            for id in &bound {
                assert!(l.has_instrument(*id));
            }
            assert!(!l.has_instrument(SYMBOL_ID_NONE));
            round += 1;
        }
    }

    #[test]
    fn an_unbound_ledger_reads_exactly_as_before() {
        let mut l = bound();
        l.book_fill(&fill(T0, sym(100), true, 500_000, 3 * E6, 1));
        let before = (
            l.slot_exposure_1e6(SLOT),
            l.projected_exposure_1e6(SLOT, sym(100), E6, true),
            l.slot_day_turnover_1e6(SLOT),
        );
        l.on_mark(bn(1), 5 * E6, 5 * E6, 1);
        l.on_opt_summary(&summary(bn(4), E6, E6));
        assert_eq!(l.probe_instrument(sym(100), SLOT, E6, true, E6, None), None);
        assert_eq!(l.instrument_exposure_1e6(SLOT), 0);
        assert_eq!(
            (
                l.slot_exposure_1e6(SLOT),
                l.projected_exposure_1e6(SLOT, sym(100), E6, true),
                l.slot_day_turnover_1e6(SLOT),
            ),
            before
        );
    }

    #[test]
    fn a_spot_sell_beyond_the_holding_is_refused_as_short() {
        let mut l = inst();
        l.book_fill(&fill(T0, SPOT, true, 100 * E6, 2 * E6, 1));
        let p = l
            .probe_instrument(SPOT, SLOT, 3 * E6, false, 100 * E6, None)
            .unwrap();
        assert_eq!(p.refusal, InstRefusal::Short);
        let p = l
            .probe_instrument(SPOT, SLOT, 2 * E6, false, 100 * E6, None)
            .unwrap();
        assert!(
            p.exit && p.refusal == InstRefusal::None,
            "selling the holding is an exit"
        );
        // A fill that oversells anyway floors at zero and is reported.
        l.book_fill(&fill(T0, SPOT, false, 100 * E6, 3 * E6, 2));
        assert_eq!(l.instrument_position_1e6(SLOT, SPOT), Some(0));
        assert_eq!(l.counters().sells_below_zero, 1);
    }

    /// **BX3 risk review — working exits of the whole position are ONE
    /// exit.** Each further sell is judged from the position the working
    /// ones would leave: flat, so it opens a short and meets every clamp.
    /// A requote of the working exit is judged beside the OTHER orders
    /// only; buys working on the far side change nothing for a sell; a
    /// partial fill moves quantity from working into the position; a
    /// spot sell past the holding net of working sells is a short.
    /// Break-and-watch: judging from the filled position alone reads the
    /// second sell as an exit — both filled, the position flips 5 → −5
    /// with no clamp measuring it.
    #[test]
    fn concurrent_exits_are_judged_from_the_position_they_leave() {
        let mut l = inst();
        l.book_fill(&fill(T0, LIN, true, 100 * E6, 5 * E6, 1));
        let p = l
            .probe_instrument(LIN, SLOT, 5 * E6, false, 100 * E6, None)
            .unwrap();
        assert!(p.exit, "the first full-size sell is an exit");
        l.on_submit(21, SLOT, LIN, 5 * E6, false);
        let p = l
            .probe_instrument(LIN, SLOT, 5 * E6, false, 100 * E6, None)
            .unwrap();
        assert!(!p.exit, "a second one would flip the position");
        assert_eq!((p.current_1e6, p.projected_1e6), (0, 500 * E6));
        assert_eq!(p.turnover_add_1e6, 500 * E6);
        let p = l
            .probe_instrument(LIN, SLOT, 5 * E6, false, 99 * E6, Some(21))
            .unwrap();
        assert!(p.exit, "a requote of the working exit is still one");
        l.on_submit(22, SLOT, LIN, 3 * E6, true);
        let p = l
            .probe_instrument(LIN, SLOT, 2 * E6, false, 100 * E6, Some(21))
            .unwrap();
        assert!(p.exit, "working buys do not reduce what a sell may close");
        // 2 of the working exit fill: long 3, 3 still working to sell.
        l.book_fill(&fill(T0, LIN, false, 100 * E6, 2 * E6, 21));
        assert_eq!(l.instrument_position_1e6(SLOT, LIN), Some(3 * E6));
        let p = l
            .probe_instrument(LIN, SLOT, E6, false, 100 * E6, None)
            .unwrap();
        assert!(!p.exit);
        assert_eq!(p.turnover_add_1e6, 100 * E6, "all of it opens the short");
        // Another slot's working orders are its own.
        let p = l
            .probe_instrument(LIN, SLOT + 1, E6, true, 100 * E6, None)
            .unwrap();
        assert!(!p.exit && p.turnover_add_1e6 == 100 * E6);
        // Spot: a working sell of the whole holding leaves nothing to sell.
        l.book_fill(&fill(T0, SPOT, true, 100 * E6, 2 * E6, 3));
        l.on_submit(31, SLOT, SPOT, 2 * E6, false);
        let p = l
            .probe_instrument(SPOT, SLOT, E6, false, 100 * E6, None)
            .unwrap();
        assert_eq!(p.refusal, InstRefusal::Short);
        l.on_cancel(31, SLOT);
        let p = l
            .probe_instrument(SPOT, SLOT, E6, false, 100 * E6, None)
            .unwrap();
        assert!(p.exit, "cancelled, it no longer counts");
    }

    /// **O-BX24 — a sell is judged at the higher of its limit and the
    /// row's reference.** A marketable sell at a low limit fills at the
    /// market, so it is measured there: flat, a sell of 5 limited at $10
    /// under a $100 mark opens $500 of short, not $50. A buy keeps its own
    /// price; with no reference yet a sell has only its limit; the last
    /// fill stands in for a missing mark. Break-and-watch: pricing the
    /// sell at its limit reads $50 on every measure.
    #[test]
    fn a_sell_is_judged_at_the_higher_of_its_limit_and_the_reference() {
        let mut l = inst();
        let p = l
            .probe_instrument(LIN, SLOT, 5 * E6, false, 10 * E6, None)
            .unwrap();
        assert_eq!(p.measure_1e6, 50 * E6, "no reference yet: the limit");
        l.on_mark(LIN, 100 * E6, 100 * E6, 1);
        let p = l
            .probe_instrument(LIN, SLOT, 5 * E6, false, 10 * E6, None)
            .unwrap();
        assert_eq!(
            (p.measure_1e6, p.projected_1e6, p.turnover_add_1e6),
            (500 * E6, 500 * E6, 500 * E6)
        );
        let p = l
            .probe_instrument(LIN, SLOT, 5 * E6, false, 120 * E6, None)
            .unwrap();
        assert_eq!(p.measure_1e6, 600 * E6, "a limit above the mark is its own");
        let p = l
            .probe_instrument(LIN, SLOT, 5 * E6, true, 10 * E6, None)
            .unwrap();
        assert_eq!(p.measure_1e6, 50 * E6, "a buy fills at or below its limit");
        // An option with no mark: the last fill stands in for it.
        l.on_opt_summary(&summary(CALL, 0, 100_000 * E6));
        l.book_fill(&fill(T0, CALL, true, 400 * E6, E6, 1));
        let low = l
            .probe_instrument(CALL, SLOT, 3 * E6, false, E6, None)
            .unwrap();
        let at = l
            .probe_instrument(CALL, SLOT, 3 * E6, false, 400 * E6, None)
            .unwrap();
        assert_eq!(low, at, "a $1 limit is judged at the $400 last fill");
    }

    /// Long 5, sell 8: only the 3 that open the short are turnover.
    /// Break-and-watch: counting the whole quantity makes it 8.
    #[test]
    fn turnover_counts_only_the_increasing_part() {
        let mut l = inst();
        l.book_fill(&fill(T0, LIN, true, 10 * E6, 5 * E6, 1));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 50 * E6);
        let p = l
            .probe_instrument(LIN, SLOT, 8 * E6, false, 10 * E6, None)
            .unwrap();
        assert!(!p.exit);
        assert_eq!(p.turnover_add_1e6, 30 * E6);
        assert_eq!(p.measure_1e6, 80 * E6, "clamp 1 still sees the whole order");
        l.book_fill(&fill(T0, LIN, false, 10 * E6, 8 * E6, 2));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), 80 * E6);
        assert_eq!(l.instrument_position_1e6(SLOT, LIN), Some(-3 * E6));
    }

    #[test]
    fn a_settlement_zeroes_the_row_and_adds_no_turnover() {
        let mut l = inst();
        l.book_fill(&fill(T0, CALL, true, 500 * E6, 2 * E6, 1));
        let turnover = l.slot_day_turnover_1e6(SLOT);
        let resting = l.slot_resting(SLOT);
        let s = fill(T0, CALL, false, 0, 2 * E6, 9).with_flags(core_types::FILL_FLAG_SETTLEMENT);
        l.book_fill(&s);
        assert_eq!(l.instrument_position_1e6(SLOT, CALL), Some(0));
        assert_eq!(l.slot_day_turnover_1e6(SLOT), turnover);
        assert_eq!(l.slot_resting(SLOT), resting);
        assert_eq!(l.instrument_exposure_1e6(SLOT), 0);
    }

    #[test]
    fn a_short_option_with_no_index_is_refused_until_one_arrives() {
        let mut l = inst();
        let p = l
            .probe_instrument(CALL, SLOT, E6, false, 500 * E6, None)
            .unwrap();
        assert_eq!(p.refusal, InstRefusal::Unpriced);
        l.on_opt_summary(&summary(CALL, 500 * E6, 100_000 * E6));
        let p = l
            .probe_instrument(CALL, SLOT, E6, false, 500 * E6, None)
            .unwrap();
        assert_eq!(p.refusal, InstRefusal::None);
        // ATM: 15 % of the index plus the mark, per contract.
        assert_eq!(p.measure_1e6, 15_000 * E6 + 500 * E6);
        // A buy-back of an existing short needs no index at all.
        let mut l = inst();
        l.book_fill(&fill(T0, CALL, false, 500 * E6, E6, 1));
        let p = l
            .probe_instrument(CALL, SLOT, E6, true, 500 * E6, None)
            .unwrap();
        assert!(p.exit && p.refusal == InstRefusal::None);
    }

    #[test]
    fn a_non_writable_option_cannot_be_shorted() {
        let mut l = inst();
        l.on_opt_summary(&summary(PUT, 10 * E6, 100_000));
        let p = l
            .probe_instrument(PUT, SLOT, E6, false, 10 * E6, None)
            .unwrap();
        assert_eq!(p.refusal, InstRefusal::Short);
        // Buying it is fine, and selling what was bought is an exit.
        let p = l
            .probe_instrument(PUT, SLOT, E6, true, 10 * E6, None)
            .unwrap();
        assert_eq!(p.refusal, InstRefusal::None);
        l.book_fill(&fill(T0, PUT, true, 10 * E6, E6, 1));
        let p = l
            .probe_instrument(PUT, SLOT, E6, false, 10 * E6, None)
            .unwrap();
        assert!(p.exit);
    }

    /// Binance's short-option IM (D8): OTM on either side lowers it to
    /// the 10 % floor; the unit scales the index term, not the mark.
    #[test]
    fn option_im_follows_the_venue_formula() {
        let (i, m) = (100_000 * E6, 500 * E6);
        assert_eq!(
            short_im_per_contract(i, 100_000 * E6, m, E6, true),
            15_000 * E6 + m
        );
        assert_eq!(
            short_im_per_contract(i, 110_000 * E6, m, E6, true),
            10_000 * E6 + m
        );
        assert_eq!(
            short_im_per_contract(i, 90_000 * E6, m, E6, false),
            10_000 * E6 + m
        );
        assert_eq!(
            short_im_per_contract(i, 97_000 * E6, m, E6, true),
            15_000 * E6 + m
        );
        assert_eq!(
            short_im_per_contract(i, 103_000 * E6, m, E6, true),
            12_000 * E6 + m
        );
        // DOGE: unit 1 000, index 0.1, mark 16 per contract.
        let im = short_im_per_contract(100_000, 84_000, 16 * E6, 1_000 * E6, true);
        assert_eq!(im, 15 * E6 + 16 * E6);
    }

    /// COIN-M: contracts × face, whatever the price.
    #[test]
    fn inverse_exposure_ignores_price() {
        let mut l = inst();
        l.book_fill(&fill(T0, INV, true, 80_000 * E6, 3 * E6, 1));
        assert_eq!(l.instrument_exposure_1e6(SLOT), 300 * E6);
        l.on_mark(INV, 120_000 * E6, 120_000 * E6, 1);
        assert_eq!(l.instrument_exposure_1e6(SLOT), 300 * E6);
        let p = l
            .probe_instrument(INV, SLOT, 2 * E6, true, 1, None)
            .unwrap();
        assert_eq!((p.current_1e6, p.projected_1e6), (300 * E6, 500 * E6));
        assert_eq!(p.measure_1e6, 200 * E6);
    }

    /// Before the first summary a long is priced at its own order, never
    /// at zero — zero would read a first long as no exposure at all.
    #[test]
    fn a_first_long_option_with_no_mark_is_priced_at_the_order() {
        let l = inst();
        let p = l
            .probe_instrument(CALL, SLOT, 2 * E6, true, 700 * E6, None)
            .unwrap();
        assert_eq!(p.projected_1e6, 1_400 * E6);
        assert_eq!(p.measure_1e6, 1_400 * E6);
    }

    #[test]
    fn a_mark_reprices_the_rows_exposure() {
        let mut l = inst();
        l.book_fill(&fill(T0, LIN, true, 100 * E6, 2 * E6, 1));
        assert_eq!(l.instrument_exposure_1e6(SLOT), 200 * E6, "the last fill");
        l.on_mark(LIN, 150 * E6, 150 * E6, 1);
        assert_eq!(l.instrument_exposure_1e6(SLOT), 300 * E6);
        assert_eq!(
            l.slot_exposure_1e6(SLOT),
            300 * E6,
            "part of the slot's sum"
        );
        l.on_mark(LIN, 0, 0, 1);
        assert_eq!(
            l.instrument_exposure_1e6(SLOT),
            300 * E6,
            "a zero mark is ignored"
        );
    }

    /// Every exit on every signed law is recognised whatever the prices,
    /// and a crossing order never reads as an increase of what it
    /// reduced: the touched row is priced at one price on both sides.
    /// Break-and-watch: pricing `current` at the cached mark and
    /// `projected` at the order price fails the crossing check.
    #[test]
    fn a_reducing_order_is_never_refused_on_any_signed_law() {
        let mut x = 0xD1B5_4A32_D192_ED03u64;
        let mut n = 0;
        while n < 20_000 {
            let mut l = inst();
            l.on_opt_summary(&summary(CALL, 400 * E6, 100_000 * E6));
            let syms = [SPOT, LIN, INV, CALL];
            let s = syms[(rng(&mut x) % 4) as usize];
            let open = (rng(&mut x) % 50 + 1) as i64 * E6;
            let long = s == SPOT || rng(&mut x) & 1 == 0;
            let fpx = (rng(&mut x) % 100_000 + 1) as i64 * E6;
            l.book_fill(&fill(T0, s, long, fpx, open, 1));
            l.on_mark(s, (rng(&mut x) % 100_000 + 1) as i64 * E6, (rng(&mut x) % 100_000 + 1) as i64 * E6, 1);
            let px = (rng(&mut x) % 100_000 + 1) as i64 * E6;
            let q = (rng(&mut x) % (open as u64 / E6 as u64) + 1) as i64 * E6;
            let p = l.probe_instrument(s, SLOT, q, !long, px, None).unwrap();
            assert!(p.exit, "an exit on {s:#x}: open {open} q {q}");
            assert_eq!(p.refusal, InstRefusal::None);
            if s == LIN || s == INV {
                let over = open + (rng(&mut x) % 2 + 1) as i64 * E6;
                if over < 2 * open {
                    let p = l.probe_instrument(s, SLOT, over, !long, px, None).unwrap();
                    assert!(
                        p.projected_1e6 <= p.current_1e6,
                        "a crossing that shrinks the magnitude read as an increase"
                    );
                }
            }
            n += 1;
        }
    }

    /// The per-slot aggregate is exact: after any mix of fills, marks and
    /// summaries on any rows and slots it equals a full recompute.
    #[test]
    fn the_instrument_aggregate_matches_a_full_recompute() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut l = inst();
        let syms = [SPOT, LIN, INV, CALL, PUT];
        let mut step = 0;
        while step < 50_000 {
            let s = syms[(rng(&mut x) % 5) as usize];
            match rng(&mut x) % 4 {
                0 | 1 => {
                    let mut f = fill(
                        T0,
                        s,
                        rng(&mut x) & 1 == 0,
                        (rng(&mut x) % 200_000 + 1) as i64 * E6,
                        (rng(&mut x) % 20 + 1) as i64 * E6,
                        step,
                    );
                    f.strategy_id = (rng(&mut x) % EXEC_SLOTS as u64) as u8;
                    l.book_fill(&f);
                }
                2 => {
                    let m = (rng(&mut x) % 200_000) as i64 * E6;
                    l.on_mark(s, m, m, 1);
                }
                _ => l.on_opt_summary(&summary(
                    s,
                    (rng(&mut x) % 2_000) as i64 * E6,
                    (rng(&mut x) % 200_000) as i64 * E6,
                )),
            }
            let mut slot = 0usize;
            while slot < EXEC_SLOTS {
                let mut sum = 0i64;
                let mut i = 0usize;
                while i < l.inst_n as usize {
                    let r = &l.inst[i];
                    let e = r.exposure_ctx(r.pos_1e6[slot]);
                    assert_eq!(r.exp_1e6[slot], e, "a stale cached term");
                    sum += e;
                    i += 1;
                }
                assert_eq!(
                    l.instrument_exposure_1e6(slot),
                    sum,
                    "step {step} slot {slot}"
                );
                slot += 1;
            }
            step += 1;
        }
    }
}
