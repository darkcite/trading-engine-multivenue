// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `PendingOrder`, `OrderTable`: commit, modification, cancel, `CloseAll`, OCA (impl-05 §4.3,
//! §4.4.2–§4.4.4). Intents COMMIT (W21): nothing here is ever rolled back (I14); a same-key intent
//! modifies its record (I15). Exit rules live in `exit.rs`'s `ExitTable` (§4.5); `commit_slice` is
//! the one entry point for both. Zero allocation; index loops.

use crate::cap;
use crate::exit::ExitTable;
use crate::fault::{BrokerFault, ScriptRefusal};
use crate::intent::validate;
use crate::money::round_tick;
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
    /// Always zero.
    pub(crate) _pad: [u32; 9],
}

const _: () = assert!(core::mem::size_of::<PendingOrder>() == 128);
const _: () = assert!(core::mem::align_of::<PendingOrder>() == 64);

impl PendingOrder {
    /// Working or a triggered stop-limit.
    pub(crate) const fn is_live(&self) -> bool {
        self.state == state::WORKING || self.state == state::STOP_TRIGGERED
    }
}

/// What a commit needs from its broker.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CommitCtx {
    /// The bar being evaluated.
    pub(crate) bar: u32,
    /// TV: `chart.mintick_1e6` under `price_rounding = NearestTick`; 0 = prices as given (native,
    /// or `price_rounding = None`).
    pub(crate) round_tick_1e6: i64,
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

/// The pending-order table (§4.4.2).
pub(crate) struct OrderTable {
    orders: [PendingOrder; cap::ORDERS],
    n: u16,
    next_seq: u32,
    eval: u32,
}

impl OrderTable {
    /// An empty table.
    pub(crate) fn new() -> Self {
        Self {
            orders: [PendingOrder::default(); cap::ORDERS],
            n: 0,
            next_seq: 0,
            eval: 0,
        }
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
                Some((k, _)) if k == IntentKind::CloseAll as u8 => {
                    counters.entries_refused_allow_entry_in += 1;
                    let close = OrderIntent {
                        kind: IntentKind::CloseAll,
                        side: IntentSide::Long,
                        flags: 0,
                        oca_type: OcaType::None,
                        id: StrKey::NONE,
                        from_entry: StrKey::NONE,
                        oca_name: StrKey::NONE,
                        qty_1e6: 0,
                        limit_1e6: 0,
                        stop_1e6: 0,
                        ..*it
                    };
                    self.upsert(
                        &close,
                        StrKey::NONE,
                        pflag::ALLOW_ENTRY_CLOSE,
                        ctx,
                        counters,
                    )
                }
                Some(_) => self.upsert(it, it.id, 0, ctx, counters),
                None => {
                    counters.entries_refused_allow_entry_in += 1;
                    Ok(())
                }
            },
            IntentKind::Order => self.upsert(it, it.id, 0, ctx, counters),
            IntentKind::Close => self.upsert(it, it.from_entry, 0, ctx, counters),
            IntentKind::CloseAll => self.upsert(it, StrKey::NONE, 0, ctx, counters),
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
        open_trades: usize,
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
        // Ruling (b): leg sets. CONSERVATIVE upper bound — every live rule (after the slice) times
        // every trade that is open or could open (pending entries and the slice's entries); a slice
        // whose bound exceeds `cap::LEG_SETS` is refused whole, though fewer sets may materialise.
        if new_rules > 0 || new_entries > 0 {
            let mut pending = 0;
            let mut i = 0;
            while i < self.len() {
                let o = &self.orders[i];
                let entry = o.kind == IntentKind::Entry as u8 || o.kind == IntentKind::Order as u8;
                pending += usize::from(o.is_live() && entry);
                i += 1;
            }
            let rules = exits.live_rules() + new_rules;
            if rules.saturating_mul(open_trades + pending + new_entries) > cap::LEG_SETS {
                return full;
            }
        }
        Ok(())
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
        open_trades: usize,
        counters: &mut BrokerCounters,
    ) -> Result<(), BrokerFault> {
        validate(intents)?;
        self.check_capacity(exits, intents, ctx, open_trades)?;
        self.begin_eval();
        let n = intents.len();
        let mut i = 0;
        while i < n {
            self.commit(exits, &intents[i], ctx, counters)?;
            i += 1;
        }
        Ok(())
    }

    /// Create or modify the record keyed `(kind, key)` (§4.3 "Modification").
    fn upsert(
        &mut self,
        it: &OrderIntent,
        key: StrKey,
        extra: u8,
        ctx: &CommitCtx,
        counters: &mut BrokerCounters,
    ) -> Result<(), BrokerFault> {
        let kind = it.kind as u8;
        let existing = self.find(kind, key);
        let skip = existing.unwrap_or(usize::MAX);
        if it.oca_type != OcaType::None
            && !self.group_live(it.oca_name, it.oca_type as u8, skip)
            && self.live_groups(skip) >= cap::OCA_GROUPS
        {
            return Err(BrokerFault::Script(ScriptRefusal::OcaGroupsFull));
        }
        let (i, seq, filled, reduced, sent) = match existing {
            Some(i) => {
                counters.intents_modified += 1;
                let o = &self.orders[i];
                // A modification keeps its venue order (native requotes it, §4.3).
                (i, o.seq, o.filled_1e6, o.oca_reduced_1e6, o.sent)
            }
            None => {
                if self.len() == cap::ORDERS {
                    return Err(BrokerFault::Script(ScriptRefusal::OrderTableFull));
                }
                let s = self.next_seq;
                self.next_seq = self.next_seq.wrapping_add(1);
                self.n += 1;
                (self.len() - 1, s, 0, 0, 0)
            }
        };
        let has_l = it.flags & intent_flag::LIMIT != 0;
        let has_s = it.flags & intent_flag::STOP != 0;
        let ot = match (has_l, has_s) {
            (true, true) => otype::STOP_LIMIT,
            (true, false) => otype::LIMIT,
            (false, true) => otype::STOP,
            (false, false) => otype::MARKET,
        };
        let imm = if it.flags & intent_flag::IMMEDIATELY != 0 {
            pflag::IMMEDIATELY
        } else {
            0
        };
        self.orders[i] = PendingOrder {
            id: key,
            oca_name: it.oca_name,
            qty_1e6: it.qty_1e6,
            qty_pct_1e6: it.qty_pct_1e6,
            limit_1e6: if has_l {
                round_tick(it.limit_1e6, ctx.round_tick_1e6)
            } else {
                0
            },
            stop_1e6: if has_s {
                round_tick(it.stop_1e6, ctx.round_tick_1e6)
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
            side: it.side as u8,
            otype: ot,
            state: state::WORKING,
            oca_type: it.oca_type as u8,
            flags: imm | extra,
            sent,
            latch: 0,
            _pad: [0; 9],
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
    use crate::exit::ExitTable;
    use crate::fault::{BrokerFault, ScriptRefusal};
    use crate::switch::ExitReservation;
    use pine_ir::{
        intent_flag as f, AllowEntry, BrokerCounters, IntentKind, IntentSide, OcaType, OrderIntent,
        StrKey,
    };

    fn ctx() -> CommitCtx {
        CommitCtx {
            bar: 7,
            round_tick_1e6: 250_000,
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
                round_tick_1e6: 0,
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
        assert_eq!(t.commit_slice(&mut x, &slice, &ctx(), 0, &mut c), full);
        assert_eq!((t.len(), c.intents_committed), (0, 0));
        assert_eq!(
            t.commit_slice(&mut x, &slice[..64], &ctx(), 0, &mut c),
            Ok(())
        );
        assert_eq!(t.len(), 64);
        // Same-key re-emissions need no new slot.
        assert_eq!(
            t.commit_slice(&mut x, &slice[..64], &ctx(), 0, &mut c),
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
        assert_eq!(t.commit_slice(&mut x, &g, &ctx(), 0, &mut c), oca);
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
        assert_eq!(t.commit_slice(&mut x, &e, &ctx(), 0, &mut c), full);
        assert_eq!(x.free_rules(), 32);
        assert_eq!(t.commit_slice(&mut x, &e[..32], &ctx(), 0, &mut c), Ok(()));
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
        t.commit_slice(&mut x, &[ex], &ctx(), 0, &mut c).unwrap();
        assert!(x.find(StrKey(9)).is_some());
        t.commit_slice(&mut x, &[ex], &ctx(), 0, &mut c).unwrap();
        assert_eq!(c.intents_modified, 1);
        let cx = OrderIntent {
            kind: IntentKind::Cancel,
            seq: 0,
            ..entry(9)
        };
        t.commit_slice(&mut x, &[cx], &ctx(), 0, &mut c).unwrap();
        assert!(x.find(StrKey(9)).is_none());
    }
}
