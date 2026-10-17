// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `PendingOrder`, `OrderTable`: commit, modification, cancel, `CloseAll`, OCA (impl-05 §4.3,
//! §4.4.2–§4.4.4). Intents COMMIT (W21): nothing here is ever rolled back (I14); a same-key intent
//! modifies its record (I15). Exit rules live in `exit.rs`'s `ExitTable` (§4.5); `commit_slice` is
//! the one entry point for both. Zero allocation; index loops.

use crate::book::{lot_flag, Book};
use crate::cap;
use crate::exit::ExitTable;
use crate::fault::{BrokerFault, ScriptRefusal};
use crate::intent::validate;
use crate::switch::PriceGrid;
use pine_ir::{
    intent_flag, AllowEntry, BrokerCounters, IntentKind, IntentSide, OcaType, OrderIntent, StrKey,
    Sym,
};

/// `PendingOrder::otype` values.
pub(crate) mod otype {
    /// Market.
    pub(crate) const MARKET: u8 = 0;
    /// Limit.
    pub(crate) const LIMIT: u8 = 1;
    /// Stop.
    pub(crate) const STOP: u8 = 2;
    /// Stop-limit.
    pub(crate) const STOP_LIMIT: u8 = 3;
}

/// `PendingOrder::state` values.
pub(crate) mod state {
    /// Unused slot.
    pub(crate) const FREE: u8 = 0;
    /// Working.
    pub(crate) const WORKING: u8 = 1;
    /// A stop-limit whose stop has triggered (its limit stage is armed).
    pub(crate) const STOP_TRIGGERED: u8 = 2;
    /// Retired; freed at the end of the call.
    pub(crate) const DEAD: u8 = 3;
}

/// `PendingOrder::flags` bits.
pub(crate) mod pflag {
    /// `immediately = true` (§4.3.4).
    pub(crate) const IMMEDIATELY: u8 = 1 << 0;
    /// A close-all created by `allow_entry_in` (§4.7.5).
    pub(crate) const ALLOW_ENTRY_CLOSE: u8 = 1 << 1;
    /// Native: the record's last send was cut short by outstanding reducers (`|pos| − R`,
    /// §4.12.4 rules 2–3), so that send filling is not the record's end — the remainder is sent
    /// after the reducers' dispositions. Kept across a re-commit on the same side.
    pub(crate) const DEFERRED: u8 = 1 << 2;
}

/// One pending entry / `strategy.order` / close / close-all. 128 B.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, align(64))]
pub(crate) struct PendingOrder {
    /// Entry/Order: the id; Close: the entry id (`from_entry`); CloseAll: NONE.
    pub(crate) id: StrKey,
    /// OCA group name, or NONE.
    pub(crate) oca_name: StrKey,
    /// Explicit contracts (`QTY`), else 0.
    pub(crate) qty_1e6: i64,
    /// Close: `QTY_PCT`, else 0.
    pub(crate) qty_pct_1e6: i64,
    /// 0 = none (TV: rounded to mintick at commit).
    pub(crate) limit_1e6: i64,
    /// 0 = none.
    pub(crate) stop_1e6: i64,
    /// Unsigned, cumulative.
    pub(crate) filled_1e6: i64,
    /// OCA-reduce accumulator.
    pub(crate) oca_reduced_1e6: i64,
    /// Evaluation counter at (re)birth.
    pub(crate) born_eval: u32,
    /// Bar at (re)birth.
    pub(crate) born_bar: u32,
    /// Commit order — the tie-break.
    pub(crate) seq: u32,
    /// The id's `Sym` (display).
    pub(crate) id_sym: Sym,
    /// The comment's `Sym` (display).
    pub(crate) comment_sym: Sym,
    /// `IntentKind as u8`: Entry, Order, Close, CloseAll.
    pub(crate) kind: u8,
    /// 0 long/buy, 1 short/sell (Entry/Order); closes resolve at fill.
    pub(crate) side: u8,
    /// [`otype`].
    pub(crate) otype: u8,
    /// [`state`].
    pub(crate) state: u8,
    /// `OcaType as u8`.
    pub(crate) oca_type: u8,
    /// [`pflag`] bits.
    pub(crate) flags: u8,
    /// Native: `SentOrder` index + 1, 0 = none.
    pub(crate) sent: u8,
    /// Native: once-per-episode counter latches (§4.15).
    pub(crate) latch: u8,
    /// TV `pyramiding = GenerationTime`: the open entry-trade count at (re)birth (§4.2).
    pub(crate) gen_open: u32,
    /// TV `pct_equity_at = OrderTime`: equity ×1e12 at (re)birth, low 64 bits (§4.2).
    pub(crate) gen_eq_lo: u64,
    /// … and high 64 bits (split: an `i128` would raise the record's field alignment).
    pub(crate) gen_eq_hi: i64,
    /// Native entry: what this record's own fills opened on its side (past flat), unsigned,
    /// cumulative — the bound of a deferred reversal's remainder (`pin:05-reversal-own-qty`).
    /// Kept across a re-commit on the same side, like `DEFERRED`.
    pub(crate) opened_1e6: i64,
    /// Close / CloseAll: the exit ORDER's id, a closed trade's `exit_id` (`pin:05-exit-signal-text`,
    /// CI-032) — `Close`: the intent's own `id` (TradingView's `Close entry(s) order <id>`, keyed by
    /// the emitter; `from_entry` when the intent left it `NONE`); `CloseAll`:
    /// `signal::CLOSE_ALL`, or the entry's id for the `allow_entry_in` override (E095: the trade
    /// shows `S1`). `NONE` for the other kinds.
    pub(crate) exit_key: StrKey,
}

const _: () = assert!(core::mem::size_of::<PendingOrder>() == 128);
const _: () = assert!(core::mem::align_of::<PendingOrder>() == 64);

impl PendingOrder {
    /// Working or a triggered stop-limit.
    pub(crate) const fn is_live(&self) -> bool {
        self.state == state::WORKING || self.state == state::STOP_TRIGGERED
    }

    /// The equity stamped at (re)birth (`pct_equity_at = OrderTime`).
    pub(crate) const fn gen_equity_1e12(&self) -> i128 {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
        let bits = ((self.gen_eq_hi as u128) << 64) | self.gen_eq_lo as u128;
        #[allow(clippy::cast_possible_wrap)]
        let eq = bits as i128;
        eq
    }

    /// Stamp the equity at (re)birth.
    pub(crate) fn set_gen_equity_1e12(&mut self, eq_1e12: i128) {
        #[allow(clippy::cast_sign_loss)]
        let bits = eq_1e12 as u128;
        #[allow(clippy::cast_possible_truncation)]
        let lo = bits as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let hi = (bits >> 64) as u64 as i64;
        self.gen_eq_lo = lo;
        self.gen_eq_hi = hi;
    }
}

/// What a commit needs from its broker.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CommitCtx {
    /// The bar being evaluated.
    pub(crate) bar: u32,
    /// The script-price grid (`pin:05-price-rounding`): TV's `chart.mintick_1e6` under
    /// `price_rounding`; [`PriceGrid::AS_GIVEN`] for native or `price_rounding = None`.
    pub(crate) grid: PriceGrid,
    /// `strategy.risk.allow_entry_in`.
    pub(crate) allow: AllowEntry,
    /// The position at commit (for `allow_entry_in`, §4.4.3).
    pub(crate) position_1e6: i64,
    /// A `strategy.risk.*` halt holds: only `Cancel` / `CancelAll` commit (§4.10).
    pub(crate) halted: bool,
}

/// The pending-record key an intent commits to (§4.3): `(kind, id)`; an `Entry` refused by
/// `allow_entry_in` maps to the `(CloseAll)` record against an opposite position and to nothing
/// otherwise (§4.4.3). `None` also for the kinds that commit no pending record.
fn key_of(it: &OrderIntent, ctx: &CommitCtx) -> Option<(u8, StrKey)> {
    match it.kind {
        IntentKind::Entry => {
            let disallowed = matches!(
                (ctx.allow, it.side),
                (AllowEntry::Long, IntentSide::Short) | (AllowEntry::Short, IntentSide::Long)
            );
            if !disallowed {
                return Some((IntentKind::Entry as u8, it.id));
            }
            let opposite = match it.side {
                IntentSide::Long => ctx.position_1e6 < 0,
                IntentSide::Short => ctx.position_1e6 > 0,
            };
            if opposite {
                Some((IntentKind::CloseAll as u8, StrKey::NONE))
            } else {
                None
            }
        }
        IntentKind::Order => Some((IntentKind::Order as u8, it.id)),
        IntentKind::Close => Some((IntentKind::Close as u8, it.from_entry)),
        IntentKind::CloseAll => Some((IntentKind::CloseAll as u8, StrKey::NONE)),
        IntentKind::Exit | IntentKind::Cancel | IntentKind::CancelAll => None,
    }
}

/// Whether `intents[from..]` commits an exit rule keyed `id` (Ruling (b)).
fn exit_in(intents: &[OrderIntent], from: usize, id: StrKey) -> bool {
    let mut j = from;
    while j < intents.len() {
        if intents[j].kind == IntentKind::Exit && intents[j].id == id {
            return true;
        }
        j += 1;
    }
    false
}

/// Whether `intents[from..]` cancels the exit rule keyed `id` — a `Cancel` of that id or a
/// `CancelAll` (§4.4.3; Ruling (b)).
fn cancelled_from(intents: &[OrderIntent], from: usize, id: StrKey) -> bool {
    let mut j = from;
    while j < intents.len() {
        let it = &intents[j];
        if it.kind == IntentKind::CancelAll || (it.kind == IntentKind::Cancel && it.id == id) {
            return true;
        }
        j += 1;
    }
    false
}

/// The pending-order table (§4.4.2).
pub(crate) struct OrderTable {
    orders: [PendingOrder; cap::ORDERS],
    n: u16,
    next_seq: u32,
    eval: u32,
    /// Pending records created so far — a new key; a modification (a re-send of the same live
    /// order) is not one (`pin:05-v5-order-limit`, CI-005 / E110).
    created: u64,
}

impl OrderTable {
    /// An empty table.
    pub(crate) fn new() -> Self {
        Self {
            orders: [PendingOrder::default(); cap::ORDERS],
            n: 0,
            next_seq: 0,
            eval: 0,
            created: 0,
        }
    }

    /// Pending records created so far (`pin:05-v5-order-limit`): every new key, never a
    /// modification of a live record.
    pub(crate) fn created(&self) -> u64 {
        self.created
    }

    /// Records in use (live or dead-not-yet-freed).
    pub(crate) fn len(&self) -> usize {
        usize::from(self.n)
    }

    /// Record `i`.
    pub(crate) fn get(&self, i: usize) -> &PendingOrder {
        &self.orders[i]
    }

    /// The evaluation counter (bumped once per committed evaluation).
    pub(crate) fn eval(&self) -> u32 {
        self.eval
    }

    /// Start an evaluation's commit: bump the evaluation counter.
    pub(crate) fn begin_eval(&mut self) {
        self.eval = self.eval.wrapping_add(1);
    }

    /// The live record with key `(kind, id)`.
    fn find(&self, kind: u8, id: StrKey) -> Option<usize> {
        let n = self.len();
        let mut i = 0;
        while i < n {
            let o = &self.orders[i];
            if o.is_live() && o.kind == kind && o.id == id {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Whether the group `(name, ty)` has a live member other than `skip`.
    fn group_live(&self, name: StrKey, ty: u8, skip: usize) -> bool {
        let n = self.len();
        let mut i = 0;
        while i < n {
            let o = &self.orders[i];
            if i != skip && o.is_live() && o.oca_name == name && o.oca_type == ty {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Distinct live OCA groups, excluding record `skip` (O(n²) over ≤ 64 records, commit only).
    fn live_groups(&self, skip: usize) -> usize {
        let n = self.len();
        let mut count = 0;
        let mut i = 0;
        while i < n {
            let o = &self.orders[i];
            if i != skip && o.is_live() && o.oca_type != OcaType::None as u8 {
                let mut first = true;
                let mut k = 0;
                while k < i {
                    let p = &self.orders[k];
                    if k != skip
                        && p.is_live()
                        && p.oca_name == o.oca_name
                        && p.oca_type == o.oca_type
                    {
                        first = false;
                        break;
                    }
                    k += 1;
                }
                if first {
                    count += 1;
                }
            }
            i += 1;
        }
        count
    }

    /// Commit one validated intent (§4.4.3): pending orders here, exit rules in `exits`; `Cancel`
    /// and `CancelAll` reach both.
    pub(crate) fn commit(
        &mut self,
        exits: &mut ExitTable,
        it: &OrderIntent,
        ctx: &CommitCtx,
        counters: &mut BrokerCounters,
    ) -> Result<(), BrokerFault> {
        if ctx.halted && !matches!(it.kind, IntentKind::Cancel | IntentKind::CancelAll) {
            return Ok(()); // the trip's counter is the record
        }
        counters.intents_committed += 1;
        match it.kind {
            IntentKind::Entry => match key_of(it, ctx) {
                Some((k, key)) if k == IntentKind::CloseAll as u8 => {
                    counters.entries_refused_allow_entry_in += 1;
                    // The entry is read in place as a bare market close-all (`upsert`'s override).
                    self.upsert(it, k, key, pflag::ALLOW_ENTRY_CLOSE, ctx, counters)
                }
                Some((k, key)) => self.upsert(it, k, key, 0, ctx, counters),
                None => {
                    counters.entries_refused_allow_entry_in += 1;
                    Ok(())
                }
            },
            IntentKind::Order => self.upsert(it, it.kind as u8, it.id, 0, ctx, counters),
            IntentKind::Close => self.upsert(it, it.kind as u8, it.from_entry, 0, ctx, counters),
            IntentKind::CloseAll => self.upsert(it, it.kind as u8, StrKey::NONE, 0, ctx, counters),
            IntentKind::Cancel => {
                self.cancel(Some(it.id));
                exits.cancel(Some(it.id));
                Ok(())
            }
            IntentKind::CancelAll => {
                self.cancel(None);
                exits.cancel(None);
                Ok(())
            }
            IntentKind::Exit => {
                // One creation counter for orders and rules: ties order by creation.
                if exits.commit(it, ctx.bar, self.eval, self.next_seq, counters)? {
                    self.next_seq = self.next_seq.wrapping_add(1);
                }
                Ok(())
            }
        }
    }

    /// Ruling Q4: the slots the whole slice needs — pending records, exit rules, OCA groups — are
    /// counted before any commit, so a capacity refusal commits nothing (I35). Conservative: a key
    /// after a `Cancel` / `CancelAll` in the slice counts as new.
    fn check_capacity(
        &self,
        exits: &ExitTable,
        intents: &[OrderIntent],
        ctx: &CommitCtx,
        book: &Book,
    ) -> Result<(), BrokerFault> {
        let full = Err(BrokerFault::Script(ScriptRefusal::OrderTableFull));
        let mut new_entries = 0;
        let mut new_orders = 0;
        let mut new_rules = 0;
        let mut new_groups = 0;
        let mut cancel_seen = false;
        let n = intents.len();
        let mut i = 0;
        while i < n {
            let it = &intents[i];
            match it.kind {
                IntentKind::Cancel | IntentKind::CancelAll => cancel_seen = true,
                IntentKind::Exit => {
                    let mut new = cancel_seen || exits.find(it.id).is_none();
                    let mut j = 0;
                    while new && !cancel_seen && j < i {
                        new = !(intents[j].kind == IntentKind::Exit && intents[j].id == it.id);
                        j += 1;
                    }
                    new_rules += usize::from(new);
                }
                _ => {
                    if it.kind == IntentKind::Entry || it.kind == IntentKind::Order {
                        new_entries += 1;
                    }
                    if let Some((k, key)) = key_of(it, ctx) {
                        let mut new = cancel_seen || self.find(k, key).is_none();
                        let mut j = 0;
                        while new && !cancel_seen && j < i {
                            new = key_of(&intents[j], ctx) != Some((k, key));
                            j += 1;
                        }
                        new_orders += usize::from(new);
                    }
                    if it.oca_type != OcaType::None
                        && !self.group_live(it.oca_name, it.oca_type as u8, usize::MAX)
                    {
                        let mut first = true;
                        let mut j = 0;
                        while first && j < i {
                            let p = &intents[j];
                            first = !(p.oca_name == it.oca_name && p.oca_type == it.oca_type);
                            j += 1;
                        }
                        new_groups += usize::from(first);
                    }
                }
            }
            i += 1;
        }
        if self.len() + new_orders > cap::ORDERS || new_rules > exits.free_rules() {
            return full;
        }
        if self.live_groups(usize::MAX) + new_groups > cap::OCA_GROUPS {
            return Err(BrokerFault::Script(ScriptRefusal::OcaGroupsFull));
        }
        // Ruling (b): leg sets. CONSERVATIVE upper bound on what the next `sync` can build (one set
        // per rule × covered trade, §4.5.1), summed over the rules live AFTER the slice; a slice
        // whose bound exceeds `cap::LEG_SETS` is refused whole, though fewer sets may materialise.
        // `pin:05-leg-bound-coverage`: the bound follows coverage — a rule retired by a later
        // `Cancel` / `CancelAll` of the slice counts nothing, and a `from_entry = X` rule counts
        // only what can carry entry id X (`coverable`). The v1 bound (every live rule before the
        // slice + every re-commit after its `cancel_all`, times every trade) refused a five-level
        // grid at three open trades (pvm7_grid).
        if new_rules > 0 || new_entries > 0 {
            let mut bound = 0_usize;
            // Rules already in the table and not re-committed by the slice.
            let mut r = 0;
            while r < exits.slots() {
                let rule = exits.rule(r);
                let id = rule.intent.id;
                let live = rule.state == crate::exit::rule_state::LIVE
                    && !exit_in(intents, 0, id)
                    && !cancelled_from(intents, 0, id);
                if live {
                    let c = self.coverable(rule.intent.from_entry, intents, ctx, book);
                    bound = bound.saturating_add(c);
                }
                r += 1;
            }
            // The slice's rules: the last commit of each id, unless a later intent cancels it.
            let mut j = 0;
            while j < n {
                let it = &intents[j];
                if it.kind == IntentKind::Exit
                    && !exit_in(intents, j + 1, it.id)
                    && !cancelled_from(intents, j + 1, it.id)
                {
                    let c = self.coverable(it.from_entry, intents, ctx, book);
                    bound = bound.saturating_add(c);
                }
                j += 1;
            }
            if bound > cap::LEG_SETS {
                return full;
            }
        }
        Ok(())
    }

    /// Ruling (b): the trades a rule with `from_entry` can cover at the next `sync` — an upper bound
    /// (§4.5.1; the `born_bar` horizon is ignored). `NONE`: every open trade, every live pending
    /// entry / order and every entry / order of the slice. `X`: the open trades whose entry id is
    /// `X` (and a D19 boot trade, which A6.3 may route to any id), plus one per pending key
    /// `(Entry, X)` / `(Order, X)` live now or committed by the slice — a record fills into at most
    /// one trade before the next `sync`.
    fn coverable(
        &self,
        from_entry: StrKey,
        intents: &[OrderIntent],
        ctx: &CommitCtx,
        book: &Book,
    ) -> usize {
        let any = from_entry == StrKey::NONE;
        let mut c = 0_usize;
        let mut t = 0_u32;
        while let Some(lot) = book.open_trade(t) {
            let boot = book.lot_flags(t).unwrap_or(0) & lot_flag::BOOT != 0;
            c += usize::from(any || boot || lot.entry_id == from_entry);
            t += 1;
        }
        const KINDS: [u8; 2] = [IntentKind::Entry as u8, IntentKind::Order as u8];
        let mut k = 0;
        while k < KINDS.len() {
            let kind = KINDS[k];
            if any {
                // Every pending record of this kind, and every slice intent of this kind.
                let mut i = 0;
                while i < self.len() {
                    let o = &self.orders[i];
                    c += usize::from(o.is_live() && o.kind == kind);
                    i += 1;
                }
                let mut j = 0;
                while j < intents.len() {
                    c += usize::from(intents[j].kind as u8 == kind);
                    j += 1;
                }
            } else {
                let mut has = self.find(kind, from_entry).is_some();
                let mut j = 0;
                while !has && j < intents.len() {
                    has = key_of(&intents[j], ctx) == Some((kind, from_entry));
                    j += 1;
                }
                c += usize::from(has);
            }
            k += 1;
        }
        c
    }

    /// Commit one evaluation's intents (W21): validate the whole slice (§4.4.1) and its capacity
    /// (Q4), then commit every intent in order. All or nothing (I35). Retired records are freed by
    /// the caller's `retire_dead` at a point where no walk candidate names them; the caller
    /// re-syncs the exit sets against its book.
    pub(crate) fn commit_slice(
        &mut self,
        exits: &mut ExitTable,
        intents: &[OrderIntent],
        ctx: &CommitCtx,
        book: &Book,
        counters: &mut BrokerCounters,
    ) -> Result<(), BrokerFault> {
        validate(intents)?;
        self.check_capacity(exits, intents, ctx, book)?;
        self.begin_eval();
        let n = intents.len();
        let mut i = 0;
        while i < n {
            self.commit(exits, &intents[i], ctx, counters)?;
            i += 1;
        }
        Ok(())
    }

    /// Create or modify the record keyed `(kind, key)` (§4.3 "Modification"). `kind` other than
    /// `it.kind` is the `allow_entry_in` override (§4.4.3): the entry `it` commits as a bare market
    /// close-all — long side, no OCA, no quantity, no prices, no flags — read in place (its
    /// `qty_pct_1e6`, `id_sym`, `comment_sym` kept), never rebuilt as a local intent.
    fn upsert(
        &mut self,
        it: &OrderIntent,
        kind: u8,
        key: StrKey,
        extra: u8,
        ctx: &CommitCtx,
        counters: &mut BrokerCounters,
    ) -> Result<(), BrokerFault> {
        let bare = kind != it.kind as u8;
        let (side, flags, oca_type, oca_name) = if bare {
            (IntentSide::Long as u8, 0, OcaType::None, StrKey::NONE)
        } else {
            (it.side as u8, it.flags, it.oca_type, it.oca_name)
        };
        let existing = self.find(kind, key);
        let skip = existing.unwrap_or(usize::MAX);
        if oca_type != OcaType::None
            && !self.group_live(oca_name, oca_type as u8, skip)
            && self.live_groups(skip) >= cap::OCA_GROUPS
        {
            return Err(BrokerFault::Script(ScriptRefusal::OcaGroupsFull));
        }
        let (i, seq, filled, reduced, sent, deferred, opened) = match existing {
            Some(i) => {
                counters.intents_modified += 1;
                let o = &self.orders[i];
                // A modification keeps its venue order (native requotes it, §4.3), and a deferred
                // remainder on the same side with what the record already opened (§4.12.4 rule 3).
                let (keep, opened) = if o.side == side {
                    (o.flags & pflag::DEFERRED, o.opened_1e6)
                } else {
                    (0, 0)
                };
                (
                    i,
                    o.seq,
                    o.filled_1e6,
                    o.oca_reduced_1e6,
                    o.sent,
                    keep,
                    opened,
                )
            }
            None => {
                if self.len() == cap::ORDERS {
                    return Err(BrokerFault::Script(ScriptRefusal::OrderTableFull));
                }
                let s = self.next_seq;
                self.next_seq = self.next_seq.wrapping_add(1);
                self.n += 1;
                self.created = self.created.saturating_add(1);
                (self.len() - 1, s, 0, 0, 0, 0, 0)
            }
        };
        let has_l = flags & intent_flag::LIMIT != 0;
        let has_s = flags & intent_flag::STOP != 0;
        let ot = match (has_l, has_s) {
            (true, true) => otype::STOP_LIMIT,
            (true, false) => otype::LIMIT,
            (false, true) => otype::STOP,
            (false, false) => otype::MARKET,
        };
        let imm = if flags & intent_flag::IMMEDIATELY != 0 {
            pflag::IMMEDIATELY
        } else {
            0
        };
        // `pin:05-price-rounding` (r31: E113 buy limit floor 45/45, E129 sell limit ceil 45/45,
        // E131 buy stop ceil 45/45): each script price on the grid by its own kind and side.
        let buy = side == IntentSide::Long as u8;
        self.orders[i] = PendingOrder {
            id: key,
            oca_name,
            qty_1e6: if bare { 0 } else { it.qty_1e6 },
            qty_pct_1e6: it.qty_pct_1e6,
            limit_1e6: if has_l {
                ctx.grid.limit(it.limit_1e6, buy)
            } else {
                0
            },
            stop_1e6: if has_s {
                ctx.grid.stop(it.stop_1e6, buy)
            } else {
                0
            },
            filled_1e6: filled,
            oca_reduced_1e6: reduced,
            born_eval: self.eval,
            born_bar: ctx.bar,
            seq,
            id_sym: it.id_sym,
            comment_sym: it.comment_sym,
            kind,
            side,
            otype: ot,
            state: state::WORKING,
            oca_type: oca_type as u8,
            flags: imm | extra | deferred,
            sent,
            latch: 0,
            gen_open: 0,
            gen_eq_lo: 0,
            gen_eq_hi: 0,
            opened_1e6: opened,
            exit_key: if kind == IntentKind::CloseAll as u8 {
                if bare {
                    it.id
                } else {
                    crate::signal::CLOSE_ALL
                }
            } else if kind == IntentKind::Close as u8 && it.id != StrKey::NONE {
                it.id
            } else if kind == IntentKind::Close as u8 {
                key
            } else {
                StrKey::NONE
            },
        };
        Ok(())
    }

    /// `Cancel` (`Some(id)`: working `Entry` / `Order` records with that id) or `CancelAll`
    /// (`None`: every working record). A market record is killed only by the evaluation that
    /// created it (§4.4.3).
    fn cancel(&mut self, id: Option<StrKey>) {
        let n = self.len();
        let mut i = 0;
        while i < n {
            let o = &mut self.orders[i];
            let hit = match id {
                Some(k) => {
                    o.id == k
                        && (o.kind == IntentKind::Entry as u8 || o.kind == IntentKind::Order as u8)
                }
                None => true,
            };
            let killable = o.otype != otype::MARKET || o.born_eval == self.eval;
            if hit && o.is_live() && killable {
                o.state = state::DEAD;
            }
            i += 1;
        }
    }

    /// OCA effects of a fill of `q_1e6` (unsigned) on record `i` (§4.4.4).
    pub(crate) fn on_member_fill(&mut self, i: usize, q_1e6: i64, counters: &mut BrokerCounters) {
        let (name, ty) = (self.orders[i].oca_name, self.orders[i].oca_type);
        if ty == OcaType::None as u8 {
            return;
        }
        let n = self.len();
        let mut k = 0;
        while k < n {
            let o = &mut self.orders[k];
            if k != i && o.is_live() && o.oca_name == name && o.oca_type == ty {
                if ty == OcaType::Cancel as u8 {
                    o.state = state::DEAD;
                    counters.orders_cancelled_oca += 1;
                } else {
                    o.oca_reduced_1e6 = o.oca_reduced_1e6.saturating_add(q_1e6);
                    counters.orders_reduced_oca += 1;
                    // An explicit member dies at ≤ 0; a default-sized one when its size resolves.
                    if o.qty_1e6 > 0 && o.qty_1e6 - o.filled_1e6 - o.oca_reduced_1e6 <= 0 {
                        o.state = state::DEAD;
                    }
                }
            }
            k += 1;
        }
    }

    /// Record `i`, mutable (the walk's fill bookkeeping).
    pub(crate) fn get_mut(&mut self, i: usize) -> &mut PendingOrder {
        &mut self.orders[i]
    }

    /// Kill every live record (a risk flatten, §4.10).
    pub(crate) fn kill_all(&mut self) {
        let mut i = 0;
        while i < self.len() {
            if self.orders[i].is_live() {
                self.orders[i].state = state::DEAD;
            }
            i += 1;
        }
    }

    /// Kill record `i`.
    pub(crate) fn kill(&mut self, i: usize) {
        self.orders[i].state = state::DEAD;
    }

    /// Free every dead record (swap-remove); `seq`, not position, orders every walk.
    pub(crate) fn retire_dead(&mut self) {
        let mut i = 0;
        while i < self.len() {
            if self.orders[i].state == state::DEAD {
                let last = self.len() - 1;
                // COPY: §4.17 #3 — swap-remove of a freed 128-B record keeps the table dense.
                self.orders[i] = self.orders[last];
                self.orders[last] = PendingOrder {
                    state: state::FREE,
                    ..PendingOrder::default()
                };
                self.n -= 1;
            } else {
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{otype, pflag, state, CommitCtx, OrderTable, PendingOrder};
    use crate::book::Book;
    use crate::exit::ExitTable;
    use crate::fault::{BrokerFault, ScriptRefusal};
    use crate::switch::{ExitReservation, PriceGrid, PriceRounding};
    use pine_ir::{
        intent_flag as f, AllowEntry, BrokerCounters, IntentKind, IntentSide, OcaType, OrderIntent,
        StrKey,
    };

    /// A flat book (the capacity checks read its open trades).
    fn flat() -> Book {
        Book::new(1_000_000 * 1_000_000, 1_000_000, false)
    }

    fn ctx() -> CommitCtx {
        CommitCtx {
            bar: 7,
            grid: PriceGrid {
                tick_1e6: 250_000,
                rounding: PriceRounding::NearestTick,
            },
            allow: AllowEntry::All,
            position_1e6: 0,
            halted: false,
        }
    }

    fn entry(id: u64) -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(id),
            ..OrderIntent::default()
        }
    }

    #[test]
    fn pending_order_is_two_cache_lines() {
        assert_eq!(core::mem::size_of::<PendingOrder>(), 128);
        assert_eq!(core::mem::align_of::<PendingOrder>(), 64);
    }

    #[test]
    fn committed_orders_persist_and_same_key_modifies() {
        let mut t = OrderTable::new();
        let mut c = BrokerCounters::default();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        t.begin_eval();
        let lim = OrderIntent {
            flags: f::LIMIT,
            limit_1e6: 1_125_000,
            ..entry(1)
        };
        t.commit(&mut x, &lim, &ctx(), &mut c).unwrap();
        assert_eq!(t.len(), 1);
        let o = *t.get(0);
        // TV price rounding at commit, half away from zero.
        assert_eq!(
            (o.otype, o.limit_1e6, o.born_bar),
            (otype::LIMIT, 1_250_000, 7)
        );
        // I14: later evaluations that do not touch it leave it pending.
        let mut k = 0;
        while k < 5 {
            t.begin_eval();
            k += 1;
        }
        assert_eq!(t.get(0).state, state::WORKING);
        // I15: a same-key intent modifies, keeps seq, re-births.
        let stp = OrderIntent {
            flags: f::LIMIT | f::STOP,
            limit_1e6: 1,
            stop_1e6: 2,
            ..entry(1)
        };
        t.commit(
            &mut x,
            &stp,
            &CommitCtx {
                bar: 9,
                grid: PriceGrid::AS_GIVEN,
                ..ctx()
            },
            &mut c,
        )
        .unwrap();
        assert_eq!(t.len(), 1);
        let m = *t.get(0);
        assert_eq!(
            (m.otype, m.seq, m.born_bar, m.born_eval),
            (otype::STOP_LIMIT, o.seq, 9, 6)
        );
        assert_eq!((c.intents_committed, c.intents_modified), (2, 1));
        // A different kind with the same id is a different key.
        let ord = OrderIntent {
            kind: IntentKind::Order,
            ..entry(1)
        };
        t.commit(&mut x, &ord, &ctx(), &mut c).unwrap();
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn allow_entry_in_turns_an_opposite_entry_into_a_close_all() {
        let mut t = OrderTable::new();
        let mut c = BrokerCounters::default();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        t.begin_eval();
        let long_only = CommitCtx {
            allow: AllowEntry::Long,
            ..ctx()
        };
        let short = OrderIntent {
            side: IntentSide::Short,
            ..entry(1)
        };
        // Flat: dropped.
        t.commit(&mut x, &short, &long_only, &mut c).unwrap();
        assert_eq!(t.len(), 0);
        // Long position: a pending close-all flagged ALLOW_ENTRY_CLOSE.
        t.commit(
            &mut x,
            &short,
            &CommitCtx {
                position_1e6: 1,
                ..long_only
            },
            &mut c,
        )
        .unwrap();
        assert_eq!(t.len(), 1);
        let o = t.get(0);
        assert_eq!(o.kind, IntentKind::CloseAll as u8);
        assert_eq!(o.flags & pflag::ALLOW_ENTRY_CLOSE, pflag::ALLOW_ENTRY_CLOSE);
        assert_eq!(c.entries_refused_allow_entry_in, 2);
    }

    #[test]
    fn close_all_keeps_pending_and_cancel_kills_by_id() {
        let mut t = OrderTable::new();
        let mut c = BrokerCounters::default();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        t.begin_eval();
        let lim = OrderIntent {
            flags: f::LIMIT,
            limit_1e6: 1,
            ..entry(1)
        };
        t.commit(&mut x, &lim, &ctx(), &mut c).unwrap();
        let ca = OrderIntent {
            kind: IntentKind::CloseAll,
            flags: f::IMMEDIATELY,
            ..entry(0)
        };
        let ca = OrderIntent {
            id: StrKey::NONE,
            ..ca
        };
        t.commit(&mut x, &ca, &ctx(), &mut c).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t.get(0).state, state::WORKING);
        assert_eq!(t.get(1).flags & pflag::IMMEDIATELY, pflag::IMMEDIATELY);
        // A later evaluation cannot cancel the market close-all, but cancels the limit.
        t.begin_eval();
        let all = OrderIntent {
            kind: IntentKind::CancelAll,
            ..entry(0)
        };
        t.commit(
            &mut x,
            &OrderIntent {
                id: StrKey::NONE,
                ..all
            },
            &ctx(),
            &mut c,
        )
        .unwrap();
        assert_eq!(
            (t.get(0).state, t.get(1).state),
            (state::DEAD, state::WORKING)
        );
        t.retire_dead();
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(0).kind, IntentKind::CloseAll as u8);
        // Cancel by id kills a market entry of the same evaluation.
        t.commit(&mut x, &entry(5), &ctx(), &mut c).unwrap();
        let cx = OrderIntent {
            kind: IntentKind::Cancel,
            ..entry(5)
        };
        t.commit(&mut x, &cx, &ctx(), &mut c).unwrap();
        t.retire_dead();
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn oca_cancel_and_reduce() {
        let mut t = OrderTable::new();
        let mut c = BrokerCounters::default();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        t.begin_eval();
        let g = |id, ty, q| OrderIntent {
            flags: f::QTY | f::LIMIT,
            qty_1e6: q,
            limit_1e6: 1,
            oca_type: ty,
            oca_name: StrKey(99),
            ..entry(id)
        };
        t.commit(&mut x, &g(1, OcaType::Cancel, 5), &ctx(), &mut c)
            .unwrap();
        t.commit(&mut x, &g(2, OcaType::Cancel, 5), &ctx(), &mut c)
            .unwrap();
        t.commit(&mut x, &g(3, OcaType::Reduce, 5), &ctx(), &mut c)
            .unwrap();
        t.commit(&mut x, &g(4, OcaType::Reduce, 3), &ctx(), &mut c)
            .unwrap();
        t.on_member_fill(0, 5, &mut c);
        assert_eq!(t.get(1).state, state::DEAD);
        assert_eq!(
            t.get(2).state,
            state::WORKING,
            "another type is another group"
        );
        t.on_member_fill(2, 3, &mut c);
        assert_eq!((t.get(3).state, t.get(3).oca_reduced_1e6), (state::DEAD, 3));
        assert_eq!((c.orders_cancelled_oca, c.orders_reduced_oca), (1, 1));
    }

    #[test]
    fn the_17th_oca_group_and_the_65th_order_are_refused() {
        let mut t = OrderTable::new();
        let mut c = BrokerCounters::default();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        t.begin_eval();
        let mut k = 0_u64;
        while k < 16 {
            let it = OrderIntent {
                oca_type: OcaType::Cancel,
                oca_name: StrKey(100 + k),
                ..entry(k + 1)
            };
            t.commit(&mut x, &it, &ctx(), &mut c).unwrap();
            k += 1;
        }
        let it = OrderIntent {
            oca_type: OcaType::Cancel,
            oca_name: StrKey(500),
            ..entry(77)
        };
        assert_eq!(
            t.commit(&mut x, &it, &ctx(), &mut c),
            Err(BrokerFault::Script(ScriptRefusal::OcaGroupsFull))
        );
        while k < 64 {
            t.commit(&mut x, &entry(k + 1), &ctx(), &mut c).unwrap();
            k += 1;
        }
        assert_eq!(
            t.commit(&mut x, &entry(1000), &ctx(), &mut c),
            Err(BrokerFault::Script(ScriptRefusal::OrderTableFull))
        );
    }

    #[test]
    fn a_capacity_refusal_commits_nothing_of_its_slice() {
        let mut t = OrderTable::new();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        let mut c = BrokerCounters::default();
        let mut slice = [OrderIntent::default(); 65];
        let mut k = 0;
        while k < 65 {
            slice[k] = OrderIntent {
                seq: u32::try_from(k).unwrap_or(0),
                ..entry(k as u64 + 1)
            };
            k += 1;
        }
        let full = Err(BrokerFault::Script(ScriptRefusal::OrderTableFull));
        assert_eq!(
            t.commit_slice(&mut x, &slice, &ctx(), &flat(), &mut c),
            full
        );
        assert_eq!((t.len(), c.intents_committed), (0, 0));
        assert_eq!(
            t.commit_slice(&mut x, &slice[..64], &ctx(), &flat(), &mut c),
            Ok(())
        );
        assert_eq!(t.len(), 64);
        // Same-key re-emissions need no new slot.
        assert_eq!(
            t.commit_slice(&mut x, &slice[..64], &ctx(), &flat(), &mut c),
            Ok(())
        );
        // The 17th OCA group, anywhere in the slice, refuses the whole slice.
        let mut t = OrderTable::new();
        let mut g = [OrderIntent::default(); 17];
        let mut k = 0;
        while k < 17 {
            g[k] = OrderIntent {
                oca_type: OcaType::Cancel,
                oca_name: StrKey(100 + k as u64),
                seq: u32::try_from(k).unwrap_or(0),
                ..entry(k as u64 + 1)
            };
            k += 1;
        }
        let oca = Err(BrokerFault::Script(ScriptRefusal::OcaGroupsFull));
        assert_eq!(t.commit_slice(&mut x, &g, &ctx(), &flat(), &mut c), oca);
        assert_eq!(t.len(), 0);
        // The 33rd exit rule.
        let mut t = OrderTable::new();
        let mut e = [OrderIntent::default(); 33];
        let mut k = 0;
        while k < 33 {
            e[k] = OrderIntent {
                kind: IntentKind::Exit,
                seq: u32::try_from(k).unwrap_or(0),
                ..entry(k as u64 + 1)
            };
            k += 1;
        }
        assert_eq!(t.commit_slice(&mut x, &e, &ctx(), &flat(), &mut c), full);
        assert_eq!(x.free_rules(), 32);
        assert_eq!(
            t.commit_slice(&mut x, &e[..32], &ctx(), &flat(), &mut c),
            Ok(())
        );
        assert_eq!(x.free_rules(), 0);
    }

    #[test]
    fn exit_and_cancel_reach_the_exit_table() {
        let mut t = OrderTable::new();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        let mut c = BrokerCounters::default();
        let ex = OrderIntent {
            kind: IntentKind::Exit,
            seq: 0,
            ..entry(9)
        };
        t.commit_slice(&mut x, &[ex], &ctx(), &flat(), &mut c)
            .unwrap();
        assert!(x.find(StrKey(9)).is_some());
        t.commit_slice(&mut x, &[ex], &ctx(), &flat(), &mut c)
            .unwrap();
        assert_eq!(c.intents_modified, 1);
        let cx = OrderIntent {
            kind: IntentKind::Cancel,
            seq: 0,
            ..entry(9)
        };
        t.commit_slice(&mut x, &[cx], &ctx(), &flat(), &mut c)
            .unwrap();
        assert!(x.find(StrKey(9)).is_none());
    }

    /// Ruling (b), `pin:05-leg-bound-coverage`: the leg-set bound sums, over the rules live after
    /// the slice, the trades each can cover — a `from_entry = X` rule only X's.
    #[test]
    fn the_leg_set_bound_follows_coverage_and_the_slice_cancels() {
        let mut book = flat();
        let src = crate::book::FillSrc {
            id: StrKey(1),
            sym: pine_ir::Sym::NONE,
            named_entry: StrKey::NONE,
            born_bar: 0,
            born_eval: 0,
            is_entry: true,
            lot_flags: 0,
        };
        let mut k = 0;
        while k < 30 {
            book.apply_fill(1_000_000, 100_000_000, 0, &src, 0, 0, 0)
                .unwrap();
            k += 1;
        }
        let rule = |id: u64, from: u64, seq: u32| OrderIntent {
            kind: IntentKind::Exit,
            id: StrKey(id),
            from_entry: StrKey(from),
            seq,
            ..OrderIntent::default()
        };
        let cancel_all = OrderIntent {
            kind: IntentKind::CancelAll,
            seq: 0,
            ..OrderIntent::default()
        };
        let mut t = OrderTable::new();
        let mut x = ExitTable::new(ExitReservation::RuleOrder);
        let mut c = BrokerCounters::default();
        // Four rules on id 1 over its 30 trades: ≤ 120 sets.
        let four = [
            rule(11, 1, 0),
            rule(12, 1, 1),
            rule(13, 1, 2),
            rule(14, 1, 3),
        ];
        assert_eq!(t.commit_slice(&mut x, &four, &ctx(), &book, &mut c), Ok(()));
        // `cancel_all` then the same four: the cancelled four count nothing (v1: 8 × 30).
        let again = [
            cancel_all,
            rule(11, 1, 1),
            rule(12, 1, 2),
            rule(13, 1, 3),
            rule(14, 1, 4),
        ];
        assert_eq!(
            t.commit_slice(&mut x, &again, &ctx(), &book, &mut c),
            Ok(())
        );
        // Re-committed, then cancelled later in the slice: nothing live, nothing counted.
        let gone = [
            rule(15, 1, 0),
            rule(16, 1, 1),
            OrderIntent {
                seq: 2,
                ..cancel_all
            },
        ];
        assert_eq!(t.commit_slice(&mut x, &gone, &ctx(), &book, &mut c), Ok(()));
        // A fifth rule on id 1 could cover 150 > 128: refused whole, nothing committed.
        let full = Err(BrokerFault::Script(ScriptRefusal::OrderTableFull));
        assert_eq!(
            t.commit_slice(
                &mut x,
                &[
                    rule(11, 1, 0),
                    rule(12, 1, 1),
                    rule(13, 1, 2),
                    rule(14, 1, 3),
                    rule(15, 1, 4)
                ],
                &ctx(),
                &book,
                &mut c
            ),
            full
        );
        assert!(x.find(StrKey(15)).is_none());
        // A fifth rule on id 2 covers no open trade (and one pending key at most): accepted.
        let other = [
            rule(11, 1, 0),
            rule(12, 1, 1),
            rule(13, 1, 2),
            rule(14, 1, 3),
            rule(15, 2, 4),
        ];
        assert_eq!(
            t.commit_slice(&mut x, &other, &ctx(), &book, &mut c),
            Ok(())
        );
        // A `from_entry = NONE` rule covers every trade: 120 + 0 + 30 > 128, refused.
        let any = [rule(16, 0, 0)];
        assert_eq!(t.commit_slice(&mut x, &any, &ctx(), &book, &mut c), full);
    }
}
