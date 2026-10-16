// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Native mode (impl-05 §3.5, §4.12–§4.14): `NativeBroker` turns committed intents into
//! venue-neutral `pine_ir::EngineIntent`s and books the position from fills only (LAW E-5).
//! INCREMENT (T14): construction, marks, commits, the client-oid lifecycle (`on_ack`,
//! `on_cancelled`), fills with the attribution ring, boot reconciliation and the read-back;
//! (T15 part 1) `reconcile` over the `Ioc` column with the gates and worst-case sizing; (T16
//! part 1) `flatten`. INCREMENT: resting makers (`Maker` column, `Modify` / `Cancel`) are T15
//! part 2.

pub(crate) mod desire;
pub(crate) mod gate;
pub(crate) mod sent;

use crate::book::{lot_flag, Book, FillSrc};
use crate::buf::EngineIntentBuf;
use crate::cap;
use crate::decl::{check_native, decl_hash64};
use crate::exit::{ExitTable, SyncCtx};
use crate::fault::{BrokerFault, InternalDefect};
use crate::money::{floor_step, mul_div, sat_i64, usd_1e6_to_1e12, value_1e12};
use crate::orders::{otype, pflag, CommitCtx, OrderTable};
use crate::risk::Risk;
use crate::size::{declared_qty, unit_qty};
use crate::size::{n_rule_entry, n_rule_scale, size_entry_fill};
use crate::switch::SWITCHES;
use crate::tv::path::{sort_cands, Cand};
use desire::{leg_rest, leg_want, order_rest, order_triggers, order_want, WantIoc, LATCH_EDGE};
use pine_ir::{
    decl_flag, scale, Bar, BrokerCounters, BrokerView, ChartInfo, DiagCode, Dialect, EngineIntent,
    EngineVerb, IntentKind, LimitPrimitive, NativeCfg, OrderIntent, Primitive, ProgramMeta, Quote,
    RunRecord, SizingMode, StrKey, StrategyDecl, TradeRec,
};
use sent::{sent_kind, sent_state, SentOrder, SentTable, OWNER_SET};

/// Milliseconds per UTC day.
const DAY_MS: i64 = 86_400_000;

/// The native broker. One per enabled script; owned by `strategy-pine`'s script slot.
pub struct NativeBroker {
    decl: StrategyDecl,
    chart: ChartInfo,
    dialect: Dialect,
    cfg: NativeCfg,
    decl_hash: u64,
    book: Book,
    orders: OrderTable,
    exits: ExitTable,
    risk: Risk,
    counters: BrokerCounters,
    sent: SentTable,
    cur_bar: u32,
    day: Option<i64>,
    day_notional_1e12: i128,
    halted: bool,
    flatten_pending: bool,
    cof_latch: bool,
    pristine: bool,
    /// This `reconcile`: a close / close-all / reversal wants to fire, so resting reducers are
    /// taken back and none is (re)submitted (§4.12.4 rule 2).
    preempt: bool,
    mark_1e6: i64,
    bars: u32,
    last_bar: Option<u32>,
    cand: [Cand; cap::ORDERS + cap::LEG_SETS],
}

/// `SentOrder::owner` naming no record or set (beyond every table's capacity).
const OWNER_NONE: u16 = 0x7FFF;
// Beyond both owner tables (`LEG_SETS` ≥ `ORDERS`).
const _: () = assert!(cap::LEG_SETS >= cap::ORDERS && (OWNER_NONE as usize) > cap::LEG_SETS);

/// The orphan owner tag for a sent order of `kind`.
const fn owner_none(kind: u8) -> u16 {
    if kind == sent_kind::LEG {
        OWNER_NONE | OWNER_SET
    } else {
        OWNER_NONE
    }
}

/// `Cand::what` bit: a leg-set candidate.
const CAND_SET: u16 = 1 << 15;

impl NativeBroker {
    /// Validates `meta` and `cfg` (§3.7) and allocates the closed-trade ring. Refuses `5108` when
    /// `cfg.live == 1` and `meta.decl.default_qty_type == PercentOfEquity` (D22).
    pub fn new(meta: &ProgramMeta, cfg: NativeCfg) -> Result<Self, DiagCode> {
        check_native(meta, &cfg)?;
        let d = meta.decl;
        // COPY: §4.17 #7 the broker by value, 89 408 B, once at construction — init-time move
        // into the host's slot — an out-parameter or a boxed broker rejected: the frozen signature.
        let mut b = Self {
            decl: d,
            chart: meta.chart,
            dialect: meta.dialect,
            cfg,
            decl_hash: decl_hash64(&d),
            book: Book::new(
                d.initial_capital_1e6,
                meta.chart.pointvalue_1e6,
                d.flags & decl_flag::CLOSE_ANY != 0,
            ),
            orders: OrderTable::new(),
            exits: ExitTable::new(SWITCHES.exit_reservation),
            risk: Risk::new(&d),
            counters: BrokerCounters::default(),
            sent: SentTable::new(),
            cur_bar: 0,
            day: None,
            day_notional_1e12: 0,
            halted: false,
            flatten_pending: false,
            cof_latch: false,
            pristine: true,
            preempt: false,
            mark_1e6: 0,
            bars: 0,
            last_bar: None,
            cand: [Cand::default(); cap::ORDERS + cap::LEG_SETS],
        };
        b.exits.load_boot_dirs(&meta.entry_dirs);
        Ok(b)
    }

    /// Re-marks the book at `bar.close_1e6` and records `bar.index` for trade records and exit
    /// coverage. The host calls it before **every** evaluation, bar or tick.
    pub fn open_bar(&mut self, bar: &Bar) {
        self.cur_bar = bar.index;
        if self.last_bar != Some(bar.index) {
            self.bars += 1;
            self.last_bar = Some(bar.index);
        }
        self.cof_latch = false;
        self.mark_1e6 = bar.close_1e6;
        self.book.mark(bar.close_1e6);
        self.check_risk();
    }

    /// Validates and COMMITS one evaluation's intents (W21). Sends nothing — `reconcile` does.
    /// `now_ns` is the host's event time (UTC day roll, birth stamps).
    pub fn apply_intents(
        &mut self,
        intents: &[OrderIntent],
        now_ns: u64,
    ) -> Result<(), BrokerFault> {
        self.roll_day(scale::ms_from_ns(i64::try_from(now_ns).unwrap_or(i64::MAX)));
        if self.halted {
            // D13: committing stops for good; the structure is still checked (fail fast).
            return crate::intent::validate(intents);
        }
        let ctx = CommitCtx {
            bar: self.cur_bar,
            round_tick_1e6: 0,
            allow: self.decl.risk.allow_entry_in,
            position_1e6: self.book.view().position_size_1e6,
            halted: self.risk.halted(),
        };
        let open = usize::try_from(self.book.view().opentrades).unwrap_or(usize::MAX);
        self.orders
            .commit_slice(&mut self.exits, intents, &ctx, open, &mut self.counters)?;
        if !intents.is_empty() {
            self.pristine = false;
        }
        self.retire_and_relink();
        self.sync_exits()
    }

    /// Clears `out`, then writes the instructions that move what the venue holds toward what the
    /// committed orders want, given the touch `q` (§4.12.5): at most one IoC in flight, sized
    /// worst-case-safe (§4.12.4) and gated (§4.12.3). Called after every `apply_intents` and on
    /// every quote event.
    pub fn reconcile(
        &mut self,
        q: &Quote,
        now_ns: u64,
        out: &mut EngineIntentBuf,
    ) -> Result<(), BrokerFault> {
        out.clear();
        self.roll_day(scale::ms_from_ns(i64::try_from(now_ns).unwrap_or(i64::MAX)));
        if self.halted || self.flatten_pending {
            return self.flatten_step(q, now_ns, out);
        }
        let any = self.orders.len() > 0 || !self.exits.sets().is_empty();
        let fresh = gate::fresh(q, now_ns, self.cfg.quote_stale_ns);
        if fresh {
            // Step 5's trails move with every fresh touch, before the cancels read them: a set
            // whose take-profit rests still arms and runs its trail (§4.12.4 rule 2).
            self.step_trails(q);
        }
        // §4.12.4 rule 2: a close, a close-all or a reversal that wants to fire takes back every
        // resting reducer, so it is never covered by them forever.
        self.preempt = fresh && self.reducers_preempted(q);
        // Step 3: cancels are never gated (I26, I27).
        self.maker_cancels(q, now_ns, fresh, out)?;
        if !fresh {
            if any {
                self.counters.skipped_stale += 1;
            }
            return Ok(());
        }
        self.maker_modifies(q, now_ns, out)?;
        let mut ioc_sent = self.ioc_in_flight();
        // Candidates in creation order (orders and rules share one counter).
        let mut n = 0;
        let mut i = 0;
        while i < self.orders.len() {
            let o = self.orders.get_mut(i);
            if o.is_live() {
                order_triggers(o, q);
                let seq = o.seq;
                self.cand[n] = Cand {
                    key: i64::from(seq),
                    seq,
                    what: u16::try_from(i).unwrap_or(0),
                    flags: 0,
                };
                n += 1;
            }
            i += 1;
        }
        let mut k = 0;
        while k < self.exits.sets().len() {
            let s = self.exits.set(k);
            if s.state == crate::exit::set_state::LIVE && s.r_1e6 > 0 {
                let seq = self.exits.rule(usize::from(s.rule)).seq;
                self.cand[n] = Cand {
                    key: i64::from(seq),
                    seq: s.seq,
                    what: u16::try_from(k).unwrap_or(0) | CAND_SET,
                    flags: 0,
                };
                n += 1;
            }
            k += 1;
        }
        sort_cands(&mut self.cand[..n], true);
        let mut c = 0;
        while c < n {
            let what = self.cand[c].what;
            c += 1;
            let idx = usize::from(what & !CAND_SET);
            let set = what & CAND_SET != 0;
            if self.owner_sent(set, idx) != 0 {
                continue; // a sent order (or a Cancelling maker) speaks for it
            }
            if self.try_maker(set, idx, q, now_ns, out)? {
                continue;
            }
            if ioc_sent {
                continue; // one IoC in flight (§4.12.4 rule 1)
            }
            ioc_sent = if set {
                self.try_leg(idx, q, now_ns, out)?
            } else {
                self.try_order(idx, q, now_ns, out)?
            };
        }
        self.retire_and_relink();
        Ok(())
    }

    /// D13 disable-and-flatten: clears `out`, kills every pending order and exit rule, writes one
    /// `ReduceOnlyIoc` for the position not already covered by an outstanding reducer, and stops
    /// committing intents for good. Idempotent; the retry schedule is the host's.
    pub fn flatten(
        &mut self,
        q: &Quote,
        now_ns: u64,
        out: &mut EngineIntentBuf,
    ) -> Result<(), BrokerFault> {
        out.clear();
        self.halted = true;
        self.flatten_step(q, now_ns, out)
    }

    /// The dispatcher accepted `client_oid` for judgment. No book effect; retires nothing: a
    /// `Modify`'s replacement becomes its record's maker and its `prev` is marked replaced, still
    /// outstanding until `on_cancelled(prev)` (§3.6.3, §4.12.5).
    pub fn on_ack(&mut self, client_oid: u32) {
        debug_assert!(
            client_oid != 0 && client_oid < self.sent.next_oid(),
            "on_ack: never issued"
        );
        let Some(i) = self.sent.find(client_oid) else {
            return; // already retired: a late report
        };
        let o = self.sent.get_mut(i);
        o.acked = 1;
        if o.state == sent_state::SUBMITTING {
            o.state = sent_state::LIVE;
        }
        let prev = o.prev_oid;
        if prev != 0 {
            if let Some(p) = self.sent.find(prev) {
                self.sent.get_mut(p).state = sent_state::REPLACED;
            }
            self.link_owner(i);
        }
    }

    /// A fill — the only writer of the position (E-5). `qty_1e6` signed (`> 0` bought);
    /// `fee_1e12` booked as given; `time_ms` the fill's event time. `Err(Internal(UnknownFill))`
    /// for an oid never issued (book untouched).
    pub fn on_fill(
        &mut self,
        client_oid: u32,
        qty_1e6: i64,
        px_1e6: i64,
        fee_1e12: i128,
        bar_index: u32,
        time_ms: i64,
    ) -> Result<(), BrokerFault> {
        if client_oid == 0 || client_oid >= self.sent.next_oid() {
            self.counters.unknown_fills += 1;
            return Err(BrokerFault::Internal(InternalDefect::UnknownFill));
        }
        if qty_1e6 == 0 {
            return Ok(());
        }
        self.pristine = false;
        self.roll_day(time_ms);
        let q_abs = qty_1e6.saturating_abs();
        let pv = self.chart.pointvalue_1e6;
        self.day_notional_1e12 = self
            .day_notional_1e12
            .saturating_add(value_1e12(px_1e6, q_abs, pv).abs());
        let mut src = FillSrc {
            id: StrKey::NONE,
            sym: pine_ir::Sym::NONE,
            named_entry: StrKey::NONE,
            born_bar: bar_index,
            born_eval: self.orders.eval(),
            // The book mirrors the venue: a late reducer fill that crosses zero opens a trade.
            is_entry: true,
            lot_flags: 0,
        };
        let Some(i) = self.sent.find(client_oid) else {
            // Retired: the ring attributes it; evicted from the ring: booked unattributed.
            let cause = match self.sent.retired(client_oid) {
                Some(r) => {
                    src.id = r.id;
                    if r.kind == sent_kind::RISK_FLATTEN {
                        pine_ir::trade_flag::RISK_FLATTEN
                    } else {
                        0
                    }
                }
                None => {
                    self.counters.unknown_fills += 1;
                    0
                }
            };
            self.book
                .apply_fill(qty_1e6, px_1e6, fee_1e12, &src, bar_index, time_ms, cause)?;
            return self.after_fill();
        };
        // The scalars the fill reads (the book and the owners are written below).
        let (o_id, o_kind, o_owner, o_side, o_filled, o_qty, o_maker) = {
            let o = self.sent.get(i);
            (
                o.id,
                o.kind,
                o.owner,
                o.side,
                o.filled_1e6,
                o.qty_1e6,
                o.primitive == Primitive::MakerPostOnly as u8,
            )
        };
        src.id = o_id;
        let mut owner_order = None;
        let mut owner_set = None;
        match o_kind {
            sent_kind::ORDER => {
                let k = usize::from(o_owner);
                if k < self.orders.len() && usize::from(self.orders.get(k).sent) == i + 1 {
                    let p = self.orders.get(k);
                    src.sym = p.id_sym;
                    if p.kind == IntentKind::Close as u8 {
                        src.named_entry = p.id;
                    }
                    if p.kind == IntentKind::Entry as u8 {
                        src.lot_flags |= lot_flag::PYRAMID;
                    }
                    if p.otype != otype::MARKET {
                        src.lot_flags |= lot_flag::PRICE_BASED;
                    }
                    owner_order = Some(k);
                }
            }
            sent_kind::LEG => {
                let k = usize::from(o_owner & !OWNER_SET);
                if k < self.exits.sets().len() && usize::from(self.exits.set(k).sent) == i + 1 {
                    src.named_entry = self.exits.set(k).entry_id;
                    owner_set = Some(k);
                }
            }
            _ => {}
        }
        let cause = if o_kind == sent_kind::RISK_FLATTEN {
            pine_ir::trade_flag::RISK_FLATTEN
        } else {
            0
        };
        let wrong_side = (qty_1e6 > 0) != (o_side == 0);
        let over = o_filled.saturating_add(q_abs) > o_qty;
        let pos_before = self.book.view().position_size_1e6;
        self.book
            .apply_fill(qty_1e6, px_1e6, fee_1e12, &src, bar_index, time_ms, cause)?;
        if wrong_side || over {
            return Err(BrokerFault::Internal(InternalDefect::BookDesync));
        }
        if o_filled == 0 {
            if o_maker {
                self.counters.maker_filled += 1;
            } else {
                self.counters.ioc_filled += 1;
            }
        }
        let filled = o_filled + q_abs;
        self.sent.get_mut(i).filled_1e6 = filled;
        if let Some(k) = owner_order {
            // What this fill opened past flat on its own side (`pin:05-reversal-own-qty`).
            let pos_after = self.book.view().position_size_1e6;
            let (before, after) = if qty_1e6 > 0 {
                (pos_before, pos_after)
            } else {
                (pos_before.saturating_neg(), pos_after.saturating_neg())
            };
            let opened = after.max(0).saturating_sub(before.max(0)).max(0);
            let p = self.orders.get_mut(k);
            p.filled_1e6 = p.filled_1e6.saturating_add(q_abs);
            if p.kind == IntentKind::Entry as u8 {
                p.opened_1e6 = p.opened_1e6.saturating_add(opened);
            }
            self.orders.on_member_fill(k, q_abs, &mut self.counters);
            // Done at its size (§4.13 line 6) — the size it WANTS: a send cut short by outstanding
            // reducers (`DEFERRED`, §4.12.4 rules 2–3) leaves the record alive for the remainder,
            // sized again after the reducers' dispositions.
            if filled >= o_qty && self.orders.get(k).flags & pflag::DEFERRED == 0 {
                self.orders.kill(k);
            }
        }
        if let Some(k) = owner_set {
            let s = self.exits.set_mut(k);
            s.filled_1e6 = s.filled_1e6.saturating_add(q_abs);
            if s.filled_1e6 >= s.r_1e6 {
                s.state = crate::exit::set_state::DONE;
            }
        }
        if filled >= o_qty {
            self.unlink_owner(i);
            self.sent.retire(i);
        }
        self.after_fill()
    }

    /// The one "done" signal (§3.6.3): `client_oid` is gone and every fill it had was delivered.
    /// No book effect; a report for an oid already retired is ignored.
    pub fn on_cancelled(&mut self, client_oid: u32) {
        debug_assert!(
            client_oid != 0 && client_oid < self.sent.next_oid(),
            "on_cancelled: never issued"
        );
        let Some(i) = self.sent.find(client_oid) else {
            return;
        };
        let o = self.sent.get(i);
        let ioc = o.primitive != Primitive::MakerPostOnly as u8;
        if ioc && o.acked == 1 && o.state == sent_state::LIVE {
            self.counters.ioc_remainder_cancelled += 1;
        }
        self.unlink_owner(i);
        self.sent.retire(i);
    }

    /// D19: the venue's position, average price and today's notional are the truth. Replaces the
    /// book with one open trade and seeds the UTC-day notional. Call once while pristine; on a
    /// non-pristine broker it changes nothing (I34).
    pub fn reconcile_boot(&mut self, position_1e6: i64, avg_px_1e6: i64, day_notional_1e6: i64) {
        if !self.pristine {
            return;
        }
        // In place: `new` made the crate's one allocation; a fresh `Book` here would be a second.
        self.book.reset(
            self.decl.initial_capital_1e6,
            self.chart.pointvalue_1e6,
            self.decl.flags & decl_flag::CLOSE_ANY != 0,
        );
        if position_1e6 != 0 {
            let src = FillSrc {
                id: StrKey::NONE,
                sym: pine_ir::Sym::NONE,
                named_entry: StrKey::NONE,
                born_bar: self.cur_bar,
                born_eval: 0,
                is_entry: true,
                lot_flags: lot_flag::BOOT,
            };
            // A boot position is booked without a fee: the venue charged it in an earlier boot.
            let r = self
                .book
                .apply_fill(position_1e6, avg_px_1e6, 0, &src, self.cur_bar, 0, 0);
            debug_assert!(r.is_ok(), "boot position");
        }
        if self.mark_1e6 != 0 {
            self.book.mark(self.mark_1e6);
        }
        self.day_notional_1e12 = usd_1e6_to_1e12(day_notional_1e6);
        self.pristine = false;
    }

    /// Everything a `strategy.*` variable reads; the host's `BarCtx::broker`. Always current.
    #[must_use]
    pub fn view(&self) -> &BrokerView {
        self.book.view()
    }

    /// Open trade `i` (0 = oldest open). Borrowed, never copied.
    #[must_use]
    pub fn open_trade(&self, i: u32) -> Option<&TradeRec> {
        self.book.open_trade(i)
    }

    /// Closed trade `i` (0 = oldest retained).
    #[must_use]
    pub fn closed_trade(&self, i: u32) -> Option<&TradeRec> {
        self.book.closed_trade(i)
    }

    /// Every refusal and adjustment so far (§4.15).
    #[must_use]
    pub fn counters(&self) -> &BrokerCounters {
        &self.counters
    }

    /// The run header (§4.16); native sizing is always the D22 unit notional.
    #[must_use]
    pub fn record(&self) -> RunRecord {
        let v = self.book.view();
        // COPY: §4.17 #6 — the frozen by-value signature, cold.
        RunRecord {
            decl_hash64: self.decl_hash,
            sizing: SizingMode::FixedNotional {
                notional_1e6: self.cfg.unit_notional_1e6,
            },
            initial_capital_1e12: v.initial_capital_1e12,
            final_equity_1e12: v.equity_1e12,
            net_profit_1e12: v.netprofit_1e12,
            gross_profit_1e12: v.grossprofit_1e12,
            gross_loss_1e12: v.grossloss_1e12,
            commission_paid_1e12: self.book.commission_1e12(),
            max_drawdown_1e12: v.max_drawdown_1e12,
            bars: self.bars,
            fills: self.book.fills(),
            wins: v.wintrades,
            losses: v.losstrades,
            counters: self.counters,
        }
    }

    /// The owner's current sent-order tag (slot + 1), 0 = none.
    fn owner_sent(&self, set: bool, idx: usize) -> u8 {
        if set {
            self.exits.set(idx).sent
        } else {
            self.orders.get(idx).sent
        }
    }

    /// The resting maker the owner of sent order `o` wants, with its size: `None` when it
    /// wants none (or the owner is gone).
    fn rest_of(
        &mut self,
        set: bool,
        idx: usize,
        q: &Quote,
    ) -> Result<Option<(WantIoc, i64)>, BrokerFault> {
        if self.cfg.limit_primitive != LimitPrimitive::Maker {
            return Ok(None);
        }
        if set {
            if idx >= self.exits.sets().len() {
                return Ok(None);
            }
            let s = self.exits.set(idx);
            if s.state != crate::exit::set_state::LIVE || s.r_1e6 <= 0 {
                return Ok(None);
            }
            let Some(w) = leg_rest(s, q) else {
                return Ok(None);
            };
            let qty = floor_step((s.r_1e6 - s.filled_1e6).max(0), self.cfg.qty_step_1e6);
            return Ok(Some((w, qty)));
        }
        if idx >= self.orders.len() || !self.orders.get(idx).is_live() {
            return Ok(None);
        }
        let Some(w) = order_rest(self.orders.get(idx), q) else {
            return Ok(None);
        };
        let px = w.limit_1e6.unwrap_or(0);
        let qty = self.size_order(idx, w, px, false)?;
        Ok(Some((w, qty)))
    }

    /// The maker price: `L` on the venue tick toward the protective side (buy down, sell up).
    fn maker_px(&self, w: WantIoc) -> i64 {
        let l = w.limit_1e6.unwrap_or(0);
        let px = if w.buy {
            crate::money::floor_tick(l, self.cfg.tick_1e6)
        } else {
            crate::money::ceil_tick(l, self.cfg.tick_1e6)
        };
        px.max(self.cfg.tick_1e6)
    }

    /// A requote of sent maker `i` to `qty` fits beside it (§4.12.4 rule 4): reducing — the new
    /// size within `|pos| − R` with `prev` still in `R`; increasing — the size check already ran
    /// the gates with `prev` outstanding (`qty > 0`); and fewer than `MAX_OPEN` replaced.
    fn requote_fits(&self, reduce: bool, qty: i64) -> bool {
        let mut replaced = 0;
        let mut k = 0;
        while k < self.sent.slots() {
            replaced += u32::from(self.sent.get(k).state == sent_state::REPLACED);
            k += 1;
        }
        if qty <= 0 || replaced >= cap::MAX_OPEN {
            return false;
        }
        !reduce
            || qty
                <= self.book.view().position_size_1e6.saturating_abs() - self.outstanding_reducing()
    }

    /// Whether sent maker `oid` already has a replacement awaiting its ack.
    fn has_pending_replacement(&self, oid: u32) -> bool {
        let mut k = 0;
        while k < self.sent.slots() {
            let o = self.sent.get(k);
            if o.state == sent_state::SUBMITTING && o.prev_oid == oid {
                return true;
            }
            k += 1;
        }
        false
    }

    /// Write a `Cancel` for sent slot `i` (its last sent side, price, quantity, primitive and
    /// reduce bit) and mark it `Cancelling`.
    fn emit_cancel(
        &mut self,
        i: usize,
        now_ns: u64,
        out: &mut EngineIntentBuf,
    ) -> Result<(), BrokerFault> {
        let o = self.sent.get_mut(i);
        o.state = sent_state::CANCELLING;
        o.emit_ns = now_ns;
        let primitive = if o.primitive == Primitive::MakerPostOnly as u8 {
            Primitive::MakerPostOnly
        } else if o.reduce == 1 {
            Primitive::ReduceOnlyIoc
        } else {
            Primitive::TakerIoc
        };
        let e = EngineIntent {
            verb: EngineVerb::Cancel,
            primitive,
            side: o.side,
            flags: o.reduce,
            client_oid: o.client_oid,
            prev_client_oid: 0,
            _pad0: 0,
            px_1e6: o.px_1e6,
            qty_1e6: o.qty_1e6,
            ttl_ns: 0,
            id: o.id,
            _pad1: [0; 2],
        };
        out.push(e)
    }

    /// Step 3 (§4.12.5): `Cancel` every venue maker whose owner died, lost its reservation, no
    /// longer wants to rest, changed side, or cannot be requoted in place; re-emit a `Cancel` not
    /// confirmed within `quote_stale_ns`. On a stale quote only the first two (owner gone) and the
    /// re-emissions are decided — wants need the touch.
    fn maker_cancels(
        &mut self,
        q: &Quote,
        now_ns: u64,
        fresh: bool,
        out: &mut EngineIntentBuf,
    ) -> Result<(), BrokerFault> {
        let stale = u64::try_from(self.cfg.quote_stale_ns).unwrap_or(0);
        let mut i = 0;
        while i < self.sent.slots() {
            // The scalars the decision reads (`rest_of` borrows `self` mutably).
            let (maker, state, emit_ns, set, idx, reduce, side, remaining, o_px, oid) = {
                let o = self.sent.get(i);
                (
                    o.primitive == Primitive::MakerPostOnly as u8,
                    o.state,
                    o.emit_ns,
                    o.kind == sent_kind::LEG,
                    usize::from(o.owner & !OWNER_SET),
                    o.reduce,
                    o.side,
                    o.qty_1e6 - o.filled_1e6,
                    o.px_1e6,
                    o.client_oid,
                )
            };
            if maker && state == sent_state::CANCELLING && emit_ns.saturating_add(stale) <= now_ns {
                self.emit_cancel(i, now_ns, out)?;
            } else if maker && state == sent_state::LIVE {
                let linked = usize::from(self.owner_sent_checked(set, idx)) == i + 1;
                let closing_side = u8::from(self.book.view().position_size_1e6 > 0);
                let cancel = if !linked {
                    true
                } else if !fresh {
                    !self.owner_alive(set, idx)
                } else if self.preempt && reduce == 1 && side == closing_side {
                    true
                } else {
                    match self.rest_of(set, idx, q)? {
                        None => true,
                        Some((w, qty)) => {
                            let same_side = u8::from(!w.buy) == side;
                            let px = self.maker_px(w);
                            let changed = px != o_px || qty != remaining;
                            !same_side
                                || (changed
                                    && !self.has_pending_replacement(oid)
                                    && !self.requote_fits(w.reduce, qty))
                        }
                    }
                };
                if cancel {
                    self.emit_cancel(i, now_ns, out)?;
                }
            }
            i += 1;
        }
        Ok(())
    }

    /// The owner's sent tag, 0 when the owner index is out of range.
    fn owner_sent_checked(&self, set: bool, idx: usize) -> u8 {
        if set {
            if idx < self.exits.sets().len() {
                self.exits.set(idx).sent
            } else {
                0
            }
        } else if idx < self.orders.len() {
            self.orders.get(idx).sent
        } else {
            0
        }
    }

    /// The owner still wants something (live record / live set with a reservation).
    fn owner_alive(&self, set: bool, idx: usize) -> bool {
        if set {
            let s = self.exits.set(idx);
            s.state == crate::exit::set_state::LIVE && s.r_1e6 > 0
        } else {
            self.orders.get(idx).is_live()
        }
    }

    /// `Modify` (E-7) every live maker whose wanted price or size changed and whose replacement
    /// fits beside it (§4.12.4 rule 4).
    fn maker_modifies(
        &mut self,
        q: &Quote,
        now_ns: u64,
        out: &mut EngineIntentBuf,
    ) -> Result<(), BrokerFault> {
        let mut i = 0;
        while i < self.sent.slots() {
            // The scalars the decision reads (`rest_of` borrows `self` mutably).
            let (live_maker, prev, set, idx, side, remaining, o_px) = {
                let o = self.sent.get(i);
                (
                    o.primitive == Primitive::MakerPostOnly as u8 && o.state == sent_state::LIVE,
                    o.client_oid,
                    o.kind == sent_kind::LEG,
                    usize::from(o.owner & !OWNER_SET),
                    o.side,
                    o.qty_1e6 - o.filled_1e6,
                    o.px_1e6,
                )
            };
            if live_maker && !self.has_pending_replacement(prev) {
                if let Some((w, qty)) = self.rest_of(set, idx, q)? {
                    let px = self.maker_px(w);
                    let changed = px != o_px || qty != remaining;
                    if changed && u8::from(!w.buy) == side && self.requote_fits(w.reduce, qty) {
                        // COPY: prev's SentOrder → its replacement's record, 64 B, once per
                        // Modify — the replacement IS prev's identity (id, owner, side, kind,
                        // primitive, reduce) with a new price / size — re-listing those six
                        // fields one by one rejected: the same bytes, written field-wise.
                        let so = SentOrder {
                            prev_oid: prev,
                            px_1e6: px,
                            qty_1e6: qty,
                            emit_ns: now_ns,
                            ..*self.sent.get(i)
                        };
                        let (id, reduce) = (so.id, so.reduce);
                        let (_, oid) = self.sent.issue(so)?;
                        out.push(EngineIntent {
                            verb: EngineVerb::Modify,
                            primitive: Primitive::MakerPostOnly,
                            side,
                            flags: reduce,
                            client_oid: oid,
                            prev_client_oid: prev,
                            _pad0: 0,
                            px_1e6: px,
                            qty_1e6: qty,
                            ttl_ns: 0,
                            id,
                            _pad1: [0; 2],
                        })?;
                        self.counters.modifies += 1;
                    }
                }
            }
            i += 1;
        }
        Ok(())
    }

    /// Submit a resting maker for an owner with nothing sent, within the venue-slot budget
    /// (§4.12.3); `Ok(true)` when the owner is handled as a maker (sent, or an increasing maker
    /// refused for want of a slot). A take-profit with no free slot is held (`Ok(false)`).
    fn try_maker(
        &mut self,
        set: bool,
        idx: usize,
        q: &Quote,
        now_ns: u64,
        out: &mut EngineIntentBuf,
    ) -> Result<bool, BrokerFault> {
        let Some((w, qty)) = self.rest_of(set, idx, q)? else {
            return Ok(false);
        };
        if w.reduce && self.preempt {
            return Ok(false); // a close / reversal is taking the position back (rule 2)
        }
        let mut venue = 0;
        let mut makers = 0;
        let mut k = 0;
        while k < self.sent.slots() {
            let o = self.sent.get(k);
            let facing = o.state == sent_state::SUBMITTING
                || o.state == sent_state::LIVE
                || o.state == sent_state::CANCELLING;
            venue += u32::from(facing);
            makers += u32::from(facing && o.primitive == Primitive::MakerPostOnly as u8);
            k += 1;
        }
        if makers + 1 > self.cfg.max_open - 1 || venue + 1 > self.cfg.max_open {
            if w.reduce {
                return Ok(false); // held: fired as a ReduceOnlyIoc on the touch
            }
            self.counters.skipped_max_open += 1;
            self.orders.kill(idx);
            return Ok(true);
        }
        // The real sizing pass (refusals kill and count); a reducer within |pos| − R.
        let qty = if set {
            let room = (self.book.view().position_size_1e6.saturating_abs()
                - self.outstanding_reducing())
            .max(0);
            floor_step(qty.min(room), self.cfg.qty_step_1e6)
        } else {
            self.size_order(idx, w, w.limit_1e6.unwrap_or(0), true)?
        };
        if qty <= 0 {
            return Ok(true);
        }
        let px = self.maker_px(w);
        let (id, kind) = if set {
            (self.exits.set(idx).exit_id, sent_kind::LEG)
        } else {
            (self.orders.get(idx).id, sent_kind::ORDER)
        };
        let owner = u16::try_from(idx).unwrap_or(0) | if set { OWNER_SET } else { 0 };
        let so = SentOrder {
            id,
            px_1e6: px,
            qty_1e6: qty,
            emit_ns: now_ns,
            owner,
            primitive: Primitive::MakerPostOnly as u8,
            side: u8::from(!w.buy),
            reduce: u8::from(w.reduce),
            kind,
            ..SentOrder::default()
        };
        let (slot, oid) = self.sent.issue(so)?;
        out.push(EngineIntent {
            verb: EngineVerb::Submit,
            primitive: Primitive::MakerPostOnly,
            side: u8::from(!w.buy),
            flags: u8::from(w.reduce),
            client_oid: oid,
            prev_client_oid: 0,
            _pad0: 0,
            px_1e6: px,
            qty_1e6: qty,
            ttl_ns: 0,
            id,
            _pad1: [0; 2],
        })?;
        self.counters.maker_sent += 1;
        self.pristine = false;
        let tag = u8::try_from(slot + 1).unwrap_or(0);
        if set {
            self.exits.set_mut(idx).sent = tag;
        } else {
            self.orders.get_mut(idx).sent = tag;
        }
        Ok(true)
    }

    /// Move every live set's trail with the touch (§4.5.6; bid for a long's trail, ask for a
    /// short's). Idempotent for one touch, so `leg_want`'s own step changes nothing after it.
    fn step_trails(&mut self, q: &Quote) {
        let mut k = 0;
        while k < self.exits.sets().len() {
            let s = self.exits.set_mut(k);
            if s.state == crate::exit::set_state::LIVE && s.r_1e6 > 0 {
                let touch = if s.side == 1 { q.bid_1e6 } else { q.ask_1e6 };
                crate::exit::trail_step(s, touch);
            }
            k += 1;
        }
    }

    /// A live `Close` with open trades of its id, a live `CloseAll`, or a live entry against the
    /// position that wants to fire at the touch (a reversal) — §4.12.4 rules 2–3.
    /// `pin:05-preempt-reducers`: the literal "cancel every resting reducer", whether or not the
    /// close's own room suffices; a reversal is included (rule 3 alone could wait forever on a
    /// take-profit that never fills). Reducing makers are held while it holds.
    /// `pin:05-stop-limit-preempt-lag`: this runs before step 5's `order_triggers`, so a
    /// stop-limit entry still `Working` wants nothing here; on the quote that triggers a reversing
    /// stop-limit no reducer is taken back, and its send is rule 3's `q_open + |pos| − R`, which
    /// never passes the target. It preempts at a later reconcile that finds it triggered and
    /// wanting to fire (a plain stop entry, judged from the touch alone, preempts at once).
    fn reducers_preempted(&self, q: &Quote) -> bool {
        let pos = self.book.view().position_size_1e6;
        if pos == 0 {
            return false;
        }
        let mut i = 0;
        while i < self.orders.len() {
            let o = self.orders.get(i);
            if o.is_live() {
                if o.kind == IntentKind::CloseAll as u8 {
                    return true;
                }
                if o.kind == IntentKind::Close as u8 && self.open_qty_of(o.id) > 0 {
                    return true;
                }
                if o.kind == IntentKind::Entry as u8
                    && (o.side == 0) != (pos > 0)
                    && order_want(o, q, pos).is_some()
                {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    /// An IoC of this script awaits its disposition.
    fn ioc_in_flight(&self) -> bool {
        let mut i = 0;
        while i < self.sent.slots() {
            let o = self.sent.get(i);
            if o.state != sent_state::FREE && o.primitive != Primitive::MakerPostOnly as u8 {
                return true;
            }
            i += 1;
        }
        false
    }

    /// `R`: the unfilled quantity of outstanding reducing orders (§4.12.4 rule 2).
    fn outstanding_reducing(&self) -> i64 {
        let mut r = 0_i64;
        let mut i = 0;
        while i < self.sent.slots() {
            let o = self.sent.get(i);
            if o.state != sent_state::FREE && o.reduce == 1 {
                r = r.saturating_add(o.qty_1e6 - o.filled_1e6);
            }
            i += 1;
        }
        r
    }

    /// Unfilled outstanding increasing quantity on side `buy`, and its notional at `px`.
    fn outstanding_increasing(&self, buy: bool, px_1e6: i64) -> (i64, i128) {
        let pv = self.chart.pointvalue_1e6;
        let (mut q, mut v) = (0_i64, 0_i128);
        let mut i = 0;
        while i < self.sent.slots() {
            let o = self.sent.get(i);
            if o.state != sent_state::FREE && o.reduce == 0 && (o.side == 0) == buy {
                let u = o.qty_1e6 - o.filled_1e6;
                q = q.saturating_add(u);
                v = v.saturating_add(value_1e12(px_1e6, u, pv));
            }
            i += 1;
        }
        (q, v)
    }

    /// Gates 3, 5, 6 on an increasing quantity at reference price `px` (§4.12.3); `false` refuses
    /// (counted).
    fn gates_pass(
        &mut self,
        buy: bool,
        held_1e6: i64,
        inc_1e6: i64,
        px_1e6: i64,
        count: bool,
    ) -> bool {
        let pv = self.chart.pointvalue_1e6;
        let n = u64::from(count);
        if value_1e12(px_1e6, inc_1e6, pv) < usd_1e6_to_1e12(self.cfg.min_notional_1e6) {
            self.counters.skipped_min_notional += n;
            return false;
        }
        let (out_q, out_v) = self.outstanding_increasing(buy, px_1e6);
        let hi = held_1e6.saturating_add(out_q).saturating_add(inc_1e6);
        if value_1e12(px_1e6, hi, pv) > usd_1e6_to_1e12(self.cfg.cap_instance_1e6) {
            self.counters.skipped_cap += n;
            return false;
        }
        let day = self
            .day_notional_1e12
            .saturating_add(out_v)
            .saturating_add(value_1e12(px_1e6, inc_1e6, pv));
        if day > usd_1e6_to_1e12(self.cfg.cap_day_1e6) {
            self.counters.skipped_cap += n;
            return false;
        }
        true
    }

    /// Size pending order `i` for want `w` at reference price `px` (§4.7, §4.12.3–§4.12.4).
    /// `commit`: refusals kill the record and count, and the record's `pflag::DEFERRED` bit is set
    /// when outstanding reducers cut the send short of what it wants (§4.12.4 rules 2–3);
    /// otherwise a dry run (requote checks) that changes nothing. `Ok(0)`: nothing to send now.
    fn size_order(
        &mut self,
        i: usize,
        w: WantIoc,
        px: i64,
        commit: bool,
    ) -> Result<i64, BrokerFault> {
        let pos = self.book.view().position_size_1e6;
        // The scalars sizing reads (sizing borrows `self` mutably, so no borrow of the record).
        let (kind, id, qty, qty_pct, filled, reduced, born_eval, deferred) = {
            let o = self.orders.get(i);
            (
                o.kind,
                o.id,
                o.qty_1e6,
                o.qty_pct_1e6,
                o.filled_1e6,
                o.oca_reduced_1e6,
                o.born_eval,
                o.flags & pflag::DEFERRED != 0,
            )
        };
        let pv = self.chart.pointvalue_1e6;
        let step = self.cfg.qty_step_1e6;
        let room = (pos.saturating_abs() - self.outstanding_reducing()).max(0);
        if w.reduce {
            let want = if kind == IntentKind::CloseAll as u8 {
                pos.saturating_abs()
            } else {
                let total = self.open_qty_of(id);
                if total == 0 {
                    if commit {
                        self.orders.kill(i);
                    }
                    return Ok(0);
                }
                // What the close wants is judged against the id's quantity before its own fills.
                let base = total.saturating_add(filled);
                let w = if qty > 0 {
                    self.scaled(qty, px).min(base)
                } else if qty_pct > 0 {
                    let p = mul_div(i128::from(base), i128::from(qty_pct), 100 * 1_000_000);
                    floor_step(sat_i64(p), step)
                } else {
                    base
                };
                (w - filled).max(0)
            };
            let want = floor_step(want, step);
            if want == 0 && kind == IntentKind::Close as u8 {
                if commit {
                    self.orders.kill(i); // it filled what it wanted
                }
                return Ok(0);
            }
            let send = floor_step(want.min(room), step);
            if commit {
                self.set_deferred(i, send < want);
            }
            return Ok(send);
        }
        let u = unit_qty(self.cfg.unit_notional_1e6, px, pv);
        let eq = self.book.view().equity_1e12;
        let d = declared_qty(&self.decl, px, pv, eq);
        let mut want = if qty > 0 {
            if kind == IntentKind::Entry as u8 {
                n_rule_entry(u, qty, d)?
            } else {
                n_rule_scale(u, qty, d)
            }
        } else {
            u
        };
        if deferred && kind == IntentKind::Entry as u8 {
            return Ok(self.size_deferred_reversal(i, w, (want - reduced).max(0), px, commit));
        }
        want = (want - filled - reduced).max(0);
        let adds = pos == 0 || (pos > 0) == w.buy;
        let (close, open, reversal) = if kind == IntentKind::Entry as u8 {
            let bound = self.decl.risk.max_position_size_1e6;
            let max_pos = if bound > 0 {
                n_rule_scale(u, bound, d)
            } else {
                0
            };
            let entries = self.outstanding_entries(w.buy, i);
            // A dry run counts nothing (and zeroes no scratch counters).
            let counters = if commit {
                Some(&mut self.counters)
            } else {
                None
            };
            let sized = size_entry_fill(
                &self.book,
                w.buy,
                want,
                born_eval,
                self.decl.pyramiding,
                max_pos,
                entries,
                counters,
            );
            let Some(sz) = sized else {
                if commit {
                    self.orders.kill(i);
                }
                return Ok(0);
            };
            (sz.close_1e6, sz.open_1e6, sz.close_1e6 > 0)
        } else if adds {
            (0, want, false)
        } else {
            let c = want.min(room);
            (c, want - c, false)
        };
        let open = floor_step(open, step);
        if open == 0 && close == 0 {
            if commit {
                self.counters.sized_to_zero += 1;
                self.orders.kill(i);
            }
            return Ok(0);
        }
        let held = if adds { pos.saturating_abs() } else { 0 };
        if open > 0 && !self.gates_pass(w.buy, held, open, px, commit) {
            if commit {
                self.orders.kill(i);
            }
            return Ok(0);
        }
        if reversal {
            // Rule 3: `|pos| + q_open − R` — if every outstanding reducer also fills, the
            // position lands on the target; if some do not, the remainder is sent after their
            // dispositions. A reversal is done when the position reached its target, not when
            // a send filled (a partial IoC, or `R` > 0): it stays alive, `DEFERRED`.
            let r_dir = self.outstanding_reducing_dir(w.buy);
            let send = floor_step(
                close.saturating_add(open).saturating_sub(r_dir).max(0),
                step,
            );
            if commit && send > 0 {
                self.set_deferred(i, true);
            }
            return Ok(send);
        }
        if commit {
            self.set_deferred(i, false);
        }
        Ok(close.saturating_add(open))
    }

    /// A deferred reversal (§4.12.4 rule 3): the entry wants the position at `q_open` on its side.
    /// What remains is `q_open − pos·dir`, of which the outstanding reducers moving that way
    /// (`R` on the entry's side) are already in flight: it sends the rest, waits while they cover
    /// it, and is done (killed) once the position reached the target — by its own fills or by a
    /// late reducer fill crossing zero. Its `pyramiding` was judged at its first send.
    /// `pin:05-reversal-target`: the target is `q_open` on the entry's side at the current
    /// sizing; a position already past it (other fills) ends the record without a send.
    /// `pin:05-reversal-own-qty` (lead ruling N1): the part of the remainder past flat is bounded
    /// by `q_open − opened`, what this record's own fills have not opened yet — so a fill of the
    /// new trade's own exit while an old reducer's cancel is pending is never re-sold; a record
    /// that opened its whole `q_open` is done. The gates judge the worst case: the reducers in
    /// flight that move its way fill first, so the send opens `send + R_dir − flat` (review minor).
    fn size_deferred_reversal(
        &mut self,
        i: usize,
        w: WantIoc,
        q_open: i64,
        px: i64,
        commit: bool,
    ) -> i64 {
        let pos = self.book.view().position_size_1e6;
        let toward = if w.buy { pos } else { pos.saturating_neg() };
        let remaining = q_open.saturating_sub(toward);
        let left = q_open.saturating_sub(self.orders.get(i).opened_1e6);
        if remaining <= 0 || left <= 0 {
            if commit {
                self.orders.kill(i); // on target, or it opened all it may
            }
            return 0;
        }
        let flat_dist = toward.saturating_neg().max(0);
        let bounded = remaining.min(flat_dist.saturating_add(left));
        let r_dir = self.outstanding_reducing_dir(w.buy);
        let send = floor_step(bounded.saturating_sub(r_dir).max(0), self.cfg.qty_step_1e6);
        if send == 0 {
            return 0; // the reducers in flight cover it: wait for their dispositions
        }
        let open = send.saturating_add(r_dir).saturating_sub(flat_dist).max(0);
        if open > 0 && !self.gates_pass(w.buy, toward.max(0), open, px, commit) {
            if commit {
                self.orders.kill(i);
            }
            return 0;
        }
        send
    }

    /// `R` on one side: the unfilled quantity of outstanding reducing orders that buy (`buy`) or
    /// sell — those that move the position the way an order on that side does.
    fn outstanding_reducing_dir(&self, buy: bool) -> i64 {
        let mut r = 0_i64;
        let mut i = 0;
        while i < self.sent.slots() {
            let o = self.sent.get(i);
            if o.state != sent_state::FREE && o.reduce == 1 && (o.side == 0) == buy {
                r = r.saturating_add(o.qty_1e6 - o.filled_1e6);
            }
            i += 1;
        }
        r
    }

    /// Set or clear record `i`'s `pflag::DEFERRED` bit.
    fn set_deferred(&mut self, i: usize, on: bool) {
        let o = self.orders.get_mut(i);
        if on {
            o.flags |= pflag::DEFERRED;
        } else {
            o.flags &= !pflag::DEFERRED;
        }
    }

    /// Outstanding increasing entry orders on side `buy`, other than order `skip`'s (native
    /// pyramiding counts them, §4.7.4).
    fn outstanding_entries(&self, buy: bool, skip: usize) -> u32 {
        let mut n = 0;
        let mut i = 0;
        while i < self.sent.slots() {
            let o = self.sent.get(i);
            let entry = o.kind == sent_kind::ORDER
                && usize::from(o.owner) < self.orders.len()
                && usize::from(o.owner) != skip
                && self.orders.get(usize::from(o.owner)).kind == IntentKind::Entry as u8;
            if o.state != sent_state::FREE && o.reduce == 0 && (o.side == 0) == buy && entry {
                n += 1;
            }
            i += 1;
        }
        n
    }

    /// Size, gate and fire pending order `i` if it wants an IoC; `Ok(true)` when emitted.
    fn try_order(
        &mut self,
        i: usize,
        q: &Quote,
        now_ns: u64,
        out: &mut EngineIntentBuf,
    ) -> Result<bool, BrokerFault> {
        let pos = self.book.view().position_size_1e6;
        let o = self.orders.get(i);
        let (kind, id) = (o.kind, o.id);
        let Some(w) = order_want(o, q, pos) else {
            if (kind == IntentKind::Close as u8 || kind == IntentKind::CloseAll as u8) && pos == 0 {
                self.orders.kill(i); // nothing open: it lapses
            }
            return Ok(false);
        };
        let px = if w.buy { q.ask_1e6 } else { q.bid_1e6 };
        let qty = self.size_order(i, w, px, true)?;
        if qty <= 0 {
            return Ok(false); // covered by outstanding reducers: wait for their dispositions
        }
        let tag = self.emit_ioc(w, qty, q, now_ns, id, sent_kind::ORDER, i, out)?;
        let p = self.orders.get_mut(i);
        p.sent = tag;
        if w.edge {
            p.latch |= LATCH_EDGE;
        }
        Ok(true)
    }

    /// Fire leg set `k` if a leg wants it; `Ok(true)` when emitted.
    fn try_leg(
        &mut self,
        k: usize,
        q: &Quote,
        now_ns: u64,
        out: &mut EngineIntentBuf,
    ) -> Result<bool, BrokerFault> {
        let Some(w) = leg_want(self.exits.set_mut(k), q) else {
            return Ok(false);
        };
        let s = self.exits.set(k);
        let (id, want) = (s.exit_id, (s.r_1e6 - s.filled_1e6).max(0));
        let room = (self.book.view().position_size_1e6.saturating_abs()
            - self.outstanding_reducing())
        .max(0);
        let qty = floor_step(want.min(room), self.cfg.qty_step_1e6);
        if qty <= 0 {
            return Ok(false);
        }
        let tag = self.emit_ioc(w, qty, q, now_ns, id, sent_kind::LEG, k, out)?;
        let s = self.exits.set_mut(k);
        s.sent = tag;
        if w.edge {
            s.latch |= LATCH_EDGE;
        }
        Ok(true)
    }

    /// Write one IoC into `out` and the sent table (§4.12.2); returns the owner's link tag.
    fn emit_ioc(
        &mut self,
        w: WantIoc,
        qty_1e6: i64,
        q: &Quote,
        now_ns: u64,
        id: StrKey,
        kind: u8,
        owner: usize,
        out: &mut EngineIntentBuf,
    ) -> Result<u8, BrokerFault> {
        let tick = self.cfg.tick_1e6;
        let px = gate::ioc_px(
            w.buy,
            gate::band(w.buy, q, self.cfg.cross_ticks, tick),
            w.limit_1e6,
            tick,
        );
        let primitive = if w.reduce {
            Primitive::ReduceOnlyIoc
        } else {
            Primitive::TakerIoc
        };
        let owner_tag =
            u16::try_from(owner).unwrap_or(0) | if kind == sent_kind::LEG { OWNER_SET } else { 0 };
        let so = SentOrder {
            id,
            px_1e6: px,
            qty_1e6,
            emit_ns: now_ns,
            owner: owner_tag,
            primitive: primitive as u8,
            side: u8::from(!w.buy),
            reduce: u8::from(w.reduce),
            kind,
            ..SentOrder::default()
        };
        let (slot, oid) = self.sent.issue(so)?;
        out.push(EngineIntent {
            verb: EngineVerb::Submit,
            primitive,
            side: u8::from(!w.buy),
            flags: u8::from(w.reduce),
            client_oid: oid,
            prev_client_oid: 0,
            _pad0: 0,
            px_1e6: px,
            qty_1e6,
            ttl_ns: u64::try_from(self.cfg.quote_stale_ns).unwrap_or(0),
            id,
            _pad1: [0; 2],
        })?;
        self.counters.ioc_sent += 1;
        self.pristine = false;
        Ok(u8::try_from(slot + 1).unwrap_or(0))
    }

    /// The flatten (§4.14.2, and a risk trip's, §4.10): kill every pending order and rule, then
    /// one `ReduceOnlyIoc` at the band for `|pos| − R` when the quote is fresh and no IoC awaits.
    fn flatten_step(
        &mut self,
        q: &Quote,
        now_ns: u64,
        out: &mut EngineIntentBuf,
    ) -> Result<(), BrokerFault> {
        self.orders.kill_all();
        self.exits.cancel(None);
        self.retire_and_relink();
        // A `Cancel` for every live maker (I33), never gated; unconfirmed ones re-emitted.
        let stale = u64::try_from(self.cfg.quote_stale_ns).unwrap_or(0);
        let mut i = 0;
        while i < self.sent.slots() {
            let (maker, live, again) = {
                let o = self.sent.get(i);
                (
                    o.primitive == Primitive::MakerPostOnly as u8,
                    o.state == sent_state::LIVE,
                    o.state == sent_state::CANCELLING && o.emit_ns.saturating_add(stale) <= now_ns,
                )
            };
            if maker && (live || again) {
                self.emit_cancel(i, now_ns, out)?;
            }
            i += 1;
        }
        let pos = self.book.view().position_size_1e6;
        if pos == 0 {
            self.flatten_pending = false;
            return Ok(());
        }
        if !gate::fresh(q, now_ns, self.cfg.quote_stale_ns) {
            self.counters.skipped_stale += 1;
            return Ok(());
        }
        if self.ioc_in_flight() {
            return Ok(());
        }
        let qty = (pos.saturating_abs() - self.outstanding_reducing()).max(0);
        if qty == 0 {
            return Ok(());
        }
        let kind = if self.halted {
            sent_kind::FLATTEN
        } else {
            sent_kind::RISK_FLATTEN
        };
        let w = WantIoc {
            buy: pos < 0,
            reduce: true,
            limit_1e6: None,
            edge: false,
        };
        self.emit_ioc(w, qty, q, now_ns, StrKey::NONE, kind, 0, out)?;
        Ok(())
    }

    /// An explicit close / exit quantity under the unit notional (§4.7.2) at `px`.
    fn scaled(&self, q_1e6: i64, px_1e6: i64) -> i64 {
        let pv = self.chart.pointvalue_1e6;
        let eq = self.book.view().equity_1e12;
        let u = unit_qty(self.cfg.unit_notional_1e6, px_1e6, pv);
        n_rule_scale(u, q_1e6, declared_qty(&self.decl, px_1e6, pv, eq))
    }

    /// Σ|qty| of the open trades a `strategy.close(id)` closes: those whose entry id is `id`, and
    /// the D19 boot trade when the script opens `id` in exactly the boot position's direction
    /// (amendment A6.3, `pin:05-boot-close-dir`, operator ruling 2026-10-16; reduce-only; signed
    /// off by the risk re-review with N1–N3).
    fn open_qty_of(&self, id: StrKey) -> i64 {
        let mut t = 0;
        let mut sum = 0_i64;
        while let Some(l) = self.book.open_trade(t) {
            let boot = self.book.lot_flags(t).unwrap_or(0) & lot_flag::BOOT != 0
                && self.exits.boot_dirs().reaches(id, l.qty_1e6 > 0);
            if l.entry_id == id || boot {
                sum = sum.saturating_add(l.qty_1e6.saturating_abs());
            }
            t += 1;
        }
        sum
    }

    /// Roll the UTC day on event time `ms` (§4.11): the day notional resets.
    fn roll_day(&mut self, ms: i64) {
        let day = ms.div_euclid(DAY_MS);
        if self.day != Some(day) {
            self.day = Some(day);
            self.day_notional_1e12 = 0;
            let (eq, net) = (
                self.book.view().equity_1e12,
                self.book.view().netprofit_1e12,
            );
            if self.risk.roll_day(day, eq, net, &mut self.counters) {
                self.flatten_pending = true;
            }
        }
    }

    /// The risk rules at a mark or fill (§4.10): a trip sets `flatten_pending`.
    fn check_risk(&mut self) {
        let (eq, peak) = (self.book.view().equity_1e12, self.book.peak_equity_1e12());
        if self.risk.check(eq, peak, &mut self.counters) {
            self.flatten_pending = true;
        }
        if self.risk.halted() && self.book.view().position_size_1e6 != 0 {
            self.flatten_pending = true; // §4.10: late flips under a halt
        }
    }

    /// Common tail of every booked fill (§4.13 lines 6–9).
    fn after_fill(&mut self) -> Result<(), BrokerFault> {
        self.risk.note_fill();
        self.check_risk();
        if self.decl.flags & decl_flag::CALC_ON_ORDER_FILLS != 0 && !self.cof_latch {
            self.cof_latch = true;
            self.counters.reevals_on_order_fill += 1;
        }
        self.retire_and_relink();
        self.sync_exits()
    }

    /// The exit-set context at the last mark (N-rule scale under the unit notional, D22).
    fn sync_ctx(&self) -> SyncCtx {
        let px = self.mark_1e6;
        let pv = self.chart.pointvalue_1e6;
        let eq = self.book.view().equity_1e12;
        let scale = Some((
            unit_qty(self.cfg.unit_notional_1e6, px, pv),
            declared_qty(&self.decl, px, pv, eq),
        ));
        // W23 differs by dialect (`ProgramMeta::dialect`, A4.5).
        SyncCtx {
            tick_1e6: self.chart.mintick_1e6,
            v6: self.dialect == Dialect::V6,
            step_1e6: self.cfg.qty_step_1e6,
            scale,
        }
    }

    /// Rebuild the exit sets and point every leg-owned sent order at its set's new index.
    fn sync_exits(&mut self) -> Result<(), BrokerFault> {
        let ctx = self.sync_ctx();
        self.exits.sync(&self.book, &ctx)?;
        self.orphan_owners(sent_kind::LEG);
        let mut k = 0;
        while k < self.exits.sets().len() {
            let s = usize::from(self.exits.set(k).sent);
            if s != 0 && s <= self.sent.slots() {
                self.sent.get_mut(s - 1).owner = u16::try_from(k).unwrap_or(0) | OWNER_SET;
            }
            k += 1;
        }
        self.relink_replacements(sent_kind::LEG);
        Ok(())
    }

    /// Free retired pending records and re-point order-owned sent orders at their new index.
    fn retire_and_relink(&mut self) {
        self.orders.retire_dead();
        self.orphan_owners(sent_kind::ORDER);
        let mut k = 0;
        while k < self.orders.len() {
            let s = usize::from(self.orders.get(k).sent);
            if s != 0 && s <= self.sent.slots() {
                self.sent.get_mut(s - 1).owner = u16::try_from(k).unwrap_or(0);
            }
            k += 1;
        }
        self.relink_replacements(sent_kind::ORDER);
    }

    /// Point every outstanding `kind` sent order at no owner; the relink pass then re-points the
    /// linked ones. An owner that died (swap-removed, or a set rebuilt away) leaves its orders
    /// orphaned instead of naming whatever record now sits at its old index.
    fn orphan_owners(&mut self, kind: u8) {
        let none = owner_none(kind);
        let mut i = 0;
        while i < self.sent.slots() {
            let o = self.sent.get_mut(i);
            if o.state != sent_state::FREE && o.kind == kind {
                o.owner = none;
            }
            i += 1;
        }
    }

    /// A `Modify`'s replacement awaiting its ack is not linked yet (its `prev` is): it follows
    /// `prev`'s freshly relinked owner, so `on_ack` links the right record (§4.12.5).
    fn relink_replacements(&mut self, kind: u8) {
        let mut j = 0;
        while j < self.sent.slots() {
            let (state, k, prev) = {
                let o = self.sent.get(j);
                (o.state, o.kind, o.prev_oid)
            };
            if state == sent_state::SUBMITTING && k == kind && prev != 0 {
                let owner = match self.sent.find(prev) {
                    Some(p) => self.sent.get(p).owner,
                    None => owner_none(kind),
                };
                self.sent.get_mut(j).owner = owner;
            }
            j += 1;
        }
    }

    /// Make sent slot `i` its owner's current venue order.
    fn link_owner(&mut self, i: usize) {
        let (kind, owner) = (self.sent.get(i).kind, self.sent.get(i).owner);
        let tag = u8::try_from(i + 1).unwrap_or(0);
        match kind {
            sent_kind::ORDER if usize::from(owner) < self.orders.len() => {
                self.orders.get_mut(usize::from(owner)).sent = tag;
            }
            sent_kind::LEG if usize::from(owner & !OWNER_SET) < self.exits.sets().len() => {
                self.exits.set_mut(usize::from(owner & !OWNER_SET)).sent = tag;
            }
            _ => {}
        }
    }

    /// Clear the owner's link to sent slot `i` if it still points there.
    fn unlink_owner(&mut self, i: usize) {
        let (kind, owner) = (self.sent.get(i).kind, self.sent.get(i).owner);
        let tag = u8::try_from(i + 1).unwrap_or(0);
        match kind {
            sent_kind::ORDER if usize::from(owner) < self.orders.len() => {
                let p = self.orders.get_mut(usize::from(owner));
                if p.sent == tag {
                    p.sent = 0;
                }
            }
            sent_kind::LEG if usize::from(owner & !OWNER_SET) < self.exits.sets().len() => {
                let s = self.exits.set_mut(usize::from(owner & !OWNER_SET));
                if s.sent == tag {
                    s.sent = 0;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! T14: the client-oid lifecycle and fills (I25, I32). Orders are issued straight into the
    //! sent table here; `reconcile` (T15) is their producer in production.

    use super::sent::{sent_kind, sent_state, SentOrder};
    use super::NativeBroker;
    use crate::fault::{BrokerFault, InternalDefect};
    use pine_ir::{
        Bar, ChartInfo, IntentKind, LimitPrimitive, NativeCfg, OrderIntent, Primitive, ProgramMeta,
        StrKey, StrategyDecl,
    };

    const E: i64 = 1_000_000;

    fn broker() -> NativeBroker {
        let meta = ProgramMeta {
            chart: ChartInfo {
                symbol_hash: 1,
                timeframe_s: 60,
                venue: 0,
                _pad: [0; 3],
                mintick_1e6: 10_000,
                pointvalue_1e6: E,
                qty_step_1e6: 0,
                min_notional_1e6: 0,
            },
            decl: StrategyDecl {
                initial_capital_1e6: 100_000 * E,
                default_qty_value_1e6: E,
                pyramiding: 1,
                ..StrategyDecl::default()
            },
            ..ProgramMeta::default()
        };
        let cfg = NativeCfg {
            unit_notional_1e6: 100 * E,
            cap_instance_1e6: 10_000 * E,
            cap_day_1e6: 100_000 * E,
            min_notional_1e6: 0,
            qty_step_1e6: 0,
            tick_1e6: 10_000,
            quote_stale_ns: 2_000_000_000,
            cross_ticks: 5,
            max_open: 4,
            limit_primitive: LimitPrimitive::Ioc,
            live: 0,
            _pad: [0; 6],
        };
        let mut b = NativeBroker::new(&meta, cfg).unwrap_or_else(|_| unreachable!());
        b.open_bar(&Bar {
            close_1e6: 100 * E,
            index: 1,
            ..Bar::default()
        });
        b
    }

    /// Commit a market entry and issue a buy IoC of `q` for it (what `reconcile` will do).
    fn entry_ioc(b: &mut NativeBroker, q: i64) -> u32 {
        let it = OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(1),
            ..OrderIntent::default()
        };
        b.apply_intents(&[it], 1_000).unwrap();
        let so = SentOrder {
            id: StrKey(1),
            px_1e6: 101 * E,
            qty_1e6: q,
            owner: 0,
            primitive: Primitive::TakerIoc as u8,
            side: 0,
            kind: sent_kind::ORDER,
            ..SentOrder::default()
        };
        let (i, oid) = b.sent.issue(so).unwrap();
        b.link_owner(i);
        oid
    }

    #[test]
    fn unknown_fill_fails_fast_and_leaves_the_book() {
        let mut b = broker();
        let before = *b.view();
        let r = b.on_fill(1, E, 100 * E, 0, 1, 0);
        assert_eq!(r, Err(BrokerFault::Internal(InternalDefect::UnknownFill)));
        assert_eq!(b.on_fill(0, E, 100 * E, 0, 1, 0), r);
        assert_eq!(*b.view(), before);
        assert_eq!(b.counters().unknown_fills, 2);
    }

    #[test]
    fn ack_and_cancel_never_touch_the_book() {
        let mut b = broker();
        let oid = entry_ioc(&mut b, 2 * E);
        let before = *b.view();
        b.on_ack(oid);
        assert_eq!(
            b.sent.get(b.sent.find(oid).unwrap_or(0)).state,
            sent_state::LIVE
        );
        assert_eq!(*b.view(), before);
        b.on_cancelled(oid);
        assert_eq!(*b.view(), before, "I25");
        assert_eq!(b.counters().ioc_remainder_cancelled, 1);
        assert!(b.sent.find(oid).is_none());
        // The record keeps its remainder: the entry is still pending.
        assert_eq!(b.orders.len(), 1);
        // A repeated report is ignored.
        b.on_cancelled(oid);
        assert_eq!(b.counters().ioc_remainder_cancelled, 1);
    }

    #[test]
    fn fills_book_the_position_and_retire_at_size() {
        let mut b = broker();
        let oid = entry_ioc(&mut b, 2 * E);
        b.on_ack(oid);
        b.on_fill(oid, E, 100 * E, 1_000, 1, 60_000).unwrap();
        assert_eq!(b.view().position_size_1e6, E);
        assert!(b.sent.find(oid).is_some());
        b.on_fill(oid, E, 101 * E, 1_000, 1, 60_000).unwrap();
        assert_eq!(b.view().position_size_1e6, 2 * E);
        assert!(b.sent.find(oid).is_none(), "retired at its size");
        assert_eq!(b.counters().ioc_filled, 1);
        assert_eq!(b.orders.len(), 0, "the entry is done");
        assert_eq!(b.open_trade(0).map(|t| t.entry_id), Some(StrKey(1)));
        assert_eq!(b.record().commission_paid_1e12, 2_000);
    }

    #[test]
    fn a_late_fill_is_attributed_by_the_ring() {
        let mut b = broker();
        let oid = entry_ioc(&mut b, 2 * E);
        b.on_ack(oid);
        b.on_cancelled(oid);
        b.on_fill(oid, E, 100 * E, 0, 1, 0).unwrap();
        assert_eq!(b.view().position_size_1e6, E);
        assert_eq!(b.open_trade(0).map(|t| t.entry_id), Some(StrKey(1)));
        assert_eq!(b.counters().unknown_fills, 0);
    }

    #[test]
    fn a_fill_contradicting_its_order_is_booked_then_a_desync() {
        let mut b = broker();
        let oid = entry_ioc(&mut b, E);
        let r = b.on_fill(oid, -E, 100 * E, 0, 1, 0);
        assert_eq!(r, Err(BrokerFault::Internal(InternalDefect::BookDesync)));
        assert_eq!(
            b.view().position_size_1e6,
            -E,
            "the book still mirrors the venue"
        );
        let oid = entry_ioc(&mut b, E);
        let r = b.on_fill(oid, 2 * E, 100 * E, 0, 1, 0);
        assert_eq!(r, Err(BrokerFault::Internal(InternalDefect::BookDesync)));
    }

    #[test]
    fn an_acked_replacement_marks_prev_replaced_until_its_cancel() {
        let mut b = broker();
        let a = entry_ioc(&mut b, E);
        b.on_ack(a);
        let so = SentOrder {
            prev_oid: a,
            qty_1e6: E,
            side: 0,
            primitive: Primitive::MakerPostOnly as u8,
            kind: sent_kind::ORDER,
            ..SentOrder::default()
        };
        let (_, r) = b.sent.issue(so).unwrap();
        b.on_ack(r);
        let pa = b.sent.find(a).unwrap_or(0);
        assert_eq!(b.sent.get(pa).state, sent_state::REPLACED);
        b.on_cancelled(a);
        assert!(b.sent.find(a).is_none());
        assert!(b.sent.find(r).is_some());
        assert_eq!(b.counters().ioc_remainder_cancelled, 0);
    }

    #[test]
    fn reconcile_boot_seeds_one_open_trade() {
        let mut b = broker();
        b.reconcile_boot(2 * E, 95 * E, 500 * E);
        assert_eq!(b.view().position_size_1e6, 2 * E);
        assert_eq!(b.view().opentrades, 1);
        assert_eq!(b.view().position_avg_price_1e6, 95 * E);
        // Marked at the last bar's close (100): open profit 10.
        assert_eq!(b.view().openprofit_1e12, 10 * i128::from(E) * i128::from(E));
        assert_eq!(b.day_notional_1e12, 500 * i128::from(E) * i128::from(E));
    }
}
