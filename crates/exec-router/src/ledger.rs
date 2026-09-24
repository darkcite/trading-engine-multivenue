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
    _pad: [u8; 2],
}

const _: () = assert!(core::mem::size_of::<RestingOrder>() == 24);

impl RestingOrder {
    #[inline]
    const fn free() -> Self {
        Self {
            client_oid: 0,
            remaining_1e6: 0,
            sym: SYMBOL_ID_NONE,
            slot: 0,
            live: 0,
            _pad: [0; 2],
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
}

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
    #[must_use]
    pub fn slot_exposure_1e6(&self, slot: usize) -> i64 {
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
        total
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
    fn roll_day(&mut self, wall_ns: u64) {
        if wall_ns == 0 {
            return;
        }
        let epoch = wall_ns / DAY_NS;
        if self.day_epoch == 0 {
            self.day_epoch = epoch;
        } else if epoch != self.day_epoch {
            self.day_epoch = epoch;
            self.day_turnover_1e6 = [0; EXEC_SLOTS];
            self.counters.day_rollovers = self.counters.day_rollovers.saturating_add(1);
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
    pub fn on_submit(&mut self, client_oid: u64, slot: usize, sym: SymbolId, qty_1e6: i64) {
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
            _pad: [0; 2],
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
    ) {
        match self.find(prev_client_oid, slot) {
            Found::One(i) => {
                self.resting[i].client_oid = client_oid;
                self.resting[i].remaining_1e6 = qty_1e6;
                self.resting[i].sym = sym;
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
            Found::None => self.on_submit(client_oid, slot, sym, qty_1e6),
            // **Two rows under one key: leave the table alone.** Same
            // ruling as `consume_resting`, and it has to be the same
            // one: adding a THIRD row under an already-ambiguous key
            // is the opposite of "decrementing an arbitrary one is a
            // guess". `find` counts it.
            Found::Many => {}
        }
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
        l.on_submit(11, SLOT, sym(100), 5_000_000);
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
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        l.on_submit(12, SLOT, sym(100), 5_000_000);
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
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        l.on_submit(11, SLOT + 1, sym(100), 5_000_000);
        l.on_cancel(11, SLOT + 1);
        assert_eq!(l.slot_resting(SLOT), 1, "the other slot's cancel took mine");
        assert_eq!(l.slot_resting(SLOT + 1), 0);
    }

    #[test]
    fn a_modify_keeps_the_count_and_moves_the_id() {
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        l.on_modify(11, 12, SLOT, sym(100), 7_000_000);
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
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        l.book_fill(&fill(T0, sym(100), true, 500_000, 2_000_000, 11));
        assert_eq!(l.slot_resting(SLOT), 1, "3 contracts still working");
        l.book_fill(&fill(T0, sym(100), true, 500_000, 3_000_000, 11));
        assert_eq!(l.slot_resting(SLOT), 0);
    }

    #[test]
    fn an_ambiguous_key_leaves_the_count_alone_and_says_so() {
        let mut l = bound();
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        l.on_submit(11, SLOT, sym(100), 5_000_000);
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
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        l.on_submit(11, SLOT, sym(100), 5_000_000);
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
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        l.on_submit(12, SLOT, sym(101), 5_000_000);
        assert_eq!(l.slot_resting(SLOT), 2);
        l.settle(VENUE, FAM, 20_182);
        assert_eq!(l.slot_resting(SLOT), 0);
    }

    #[test]
    fn a_roll_leaves_another_outcomes_orders_alone() {
        let mut l = Ledger::new(anchor());
        l.bind(VENUE, 0, 1, sym(100));
        l.bind(VENUE, 1, 2, sym(200));
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        l.on_submit(12, SLOT, sym(200), 5_000_000);
        l.settle(VENUE, 0, 1);
        assert_eq!(l.slot_resting(SLOT), 1);
    }

    #[test]
    fn the_resting_table_refuses_rather_than_overwriting_when_full() {
        let mut l = bound();
        for i in 0..LEDGER_RESTING {
            l.on_submit(i as u64 + 1, SLOT, sym(100), 1);
        }
        assert_eq!(l.slot_resting(SLOT), LEDGER_RESTING as u32);
        l.on_submit(999_999, SLOT, sym(100), 1);
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
        l.on_submit(11, EXEC_SLOTS, sym(100), 5_000_000);
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
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        l.on_submit(11, SLOT, sym(100), 5_000_000);
        assert_eq!(l.slot_resting(SLOT), 2);
        l.on_modify(11, 12, SLOT, sym(100), 5_000_000);
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
        l.on_modify(11, 12, SLOT, sym(100), 5_000_000);
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
            l.on_submit(i as u64 + 1, SLOT, sym(100), 1);
        }
        l.on_submit(999_999, SLOT, sym(100), 1);
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
        l.on_modify(11, 12, SLOT, sym(100), 5_000_000);
        assert_eq!(l.slot_resting(SLOT), 1);
    }
}
