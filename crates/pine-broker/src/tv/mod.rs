// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! TV-equivalent mode (impl-05 §3.4, §4.8): `TvStep`, `TvBroker` and the resumable per-bar walk,
//! with every §4.2 switch reading wired (the alternatives reached through the `#[cfg(test)]`
//! `with_switches` constructor, I38).

pub(crate) mod cost;
pub(crate) mod margin;
pub(crate) mod path;
#[cfg(test)]
mod tests_r27b;
#[cfg(test)]
mod tests_trades;

use crate::book::{lot_flag, Book, FillSrc};
use crate::cap;
use crate::decl::{check_tv, decl_hash64};
use crate::exit::{set_state, ExitTable, SyncCtx};
use crate::fault::{BrokerFault, InternalDefect, ScriptRefusal};
use crate::money::{floor_step, mul_div, sat_i64};
use crate::orders::{otype, pflag, state, CommitCtx, OrderTable};
use crate::risk::Risk;
use crate::size::{
    declared_qty, n_rule_entry, n_rule_scale, open_entry_trades, size_entry, unit_qty,
};
use crate::switch::{
    CloseImmediately, CofCloseTick, CofNextTick, ForcedFillSlippage, HistoryTick, PctEquityAt,
    PriceGrid, PyramidCheck, Switches, TrailArmFire, SWITCHES,
};
use cost::{commission_1e12, slip};
use margin::{funds_check, liquidation_price, margin_call_qty};
use path::{before, path, reach, segment, sort_cands, Cand, Level};
use pine_ir::{
    barflag, decl_flag, Bar, BrokerCounters, BrokerView, ChartInfo, DiagCode, Dialect, IntentKind,
    OrderIntent, ProgramMeta, RunRecord, SizingMode, StrKey, StrategyDecl, TradeRec, TvCfg,
};

/// What the host does next. 128 B (a 64-B `Bar` + the tag, 64-aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TvStep {
    /// Evaluate the script against `bar`, then call `submit(vm.intents())`. `intrabar == false`:
    /// the bar-close evaluation → `VmInstance::step_bar`. `intrabar == true`: a
    /// `calc_on_order_fills` execution at a fill tick → `VmInstance::step_tick` (script state rolls
    /// back; the intents it emits commit, W21). `bar` is the **full** bar: on historical bars
    /// TradingView's extra executions see the bar's final OHLCV (§4.3.2) — except a history-tick
    /// run (`history_tick = PathPoints`, §4.8.9), whose `bar` is the developing bar at its path
    /// point with `barflag::CONFIRMED` dropped (`pin:05-hist-tick-bar`).
    Evaluate {
        /// The full bar.
        bar: Bar,
        /// A `calc_on_order_fills` execution at a fill tick.
        intrabar: bool,
    },
    /// The bar is finished; call `open_bar` with the next one.
    Done,
}

const _: () = assert!(core::mem::size_of::<TvStep>() == 128);

/// The walk's cursor state (§4.8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkState {
    Idle,
    /// `hist`: the outstanding evaluation is a history-tick run (§4.8.9).
    AwaitingSubmit {
        intrabar: bool,
        tick_px: i64,
        hist: bool,
    },
    Walking,
}

/// Candidate capacity: every pending record plus three legs per set.
const N_CAND: usize = cap::ORDERS + 3 * cap::LEG_SETS;
/// `Cand::what` bit: a leg-set candidate (else a pending-order index).
const WHAT_SET: u16 = 1 << 15;
/// `Cand::what` leg field shift (0 take-profit, 1 stop-loss, 2 trail).
const LEG_SHIFT: u16 = 13;
/// `Cand::what` index mask.
const IDX_MASK: u16 = (1 << 13) - 1;
/// `Cand::flags` bit: a stop-limit's limit stage re-keyed inside this segment (`a := S`).
pub(crate) const CAND_REKEYED: u16 = 1;
/// `Cand::flags` bit: a record born at an intrabar execution inside this segment
/// (`cof_next_tick = RestOfSegment`): reached from that execution's tick, `late_a`.
const CAND_LATE: u16 = 2;
/// `Cand::flags` bit: a trail armed in this segment, tried at its end point
/// (`trail_arm_fire = SameSegment`).
const CAND_TAIL: u16 = 4;
const LEG_TP: u16 = 0;
const LEG_SL: u16 = 1;
const LEG_TRAIL: u16 = 2;

/// The TV-equivalent broker. One per (script, run), owned by one thread (P1).
pub struct TvBroker {
    decl: StrategyDecl,
    chart: ChartInfo,
    dialect: Dialect,
    cfg: TvCfg,
    sw: Switches,
    decl_hash: u64,
    book: Book,
    orders: OrderTable,
    exits: ExitTable,
    risk: Risk,
    counters: BrokerCounters,
    bar: Bar,
    path: [i64; 4],
    seg: usize,
    cand: [Cand; N_CAND],
    n_cand: usize,
    cur: usize,
    seg_eval: u32,
    state: WalkState,
    latched: Option<BrokerFault>,
    collected: bool,
    evaluated: bool,
    close_work: bool,
    bars: u32,
    last_bar: Option<u32>,
    /// `cof_next_tick = RestOfSegment`: the tick of the latest intrabar execution in this segment.
    late_a: i64,
    /// The current segment's trails have been moved (`after_segment` ran).
    trailed: bool,
    /// v5 orders that died unfilled at their fill attempt — sized to zero, refused by pyramiding,
    /// nothing to close — and so do not count (`pin:05-v5-order-limit`, CI-005 / E110).
    v5_dropped: u64,
    /// The price the latest evaluation saw (its `tick_px`): a market order's sizing price
    /// (`pin:05-qty-reference-price`, CI-035).
    eval_px_1e6: i64,
    /// `history_tick = PathPoints` and the script sets `calc_on_every_history_tick` (§4.8.9).
    hist: bool,
    /// Per bar: the history-tick runs issued, bit = path point 0–2 (§4.8.9); all set on a
    /// realtime bar or when `hist` is off.
    hist_done: u8,
}

/// `hist_done` with every history-tick run issued.
const HIST_ALL: u8 = 0b111;

impl TvBroker {
    /// Validates `meta.decl`, `meta.chart` and `cfg` (§3.7) and allocates the closed-trade ring —
    /// the one allocation. Refuses with the §3.7 code; never panics.
    pub fn new(meta: &ProgramMeta, cfg: TvCfg) -> Result<Self, DiagCode> {
        Self::build(meta, cfg, SWITCHES, BrokerCounters::default())
    }

    /// `new` on the switch reading `sw` (the default `SWITCHES` outside tests, I38); `counters`
    /// the zeroed counters (`BrokerCounters::default()` stays inside a `fn new`, §4.17).
    fn build(
        meta: &ProgramMeta,
        cfg: TvCfg,
        sw: Switches,
        counters: BrokerCounters,
    ) -> Result<Self, DiagCode> {
        let path_points = sw.history_tick == HistoryTick::PathPoints;
        check_tv(meta, &cfg, path_points)?;
        let d = meta.decl;
        // COPY: §4.17 #7 the broker by value, 76 096 B, once at construction — init-time move
        // into the host's slot — an out-parameter or a boxed broker rejected: the frozen signature.
        Ok(Self {
            decl: d,
            chart: meta.chart,
            dialect: meta.dialect,
            cfg,
            sw,
            decl_hash: decl_hash64(&d),
            book: Book::new(
                d.initial_capital_1e6,
                meta.chart.pointvalue_1e6,
                d.flags & decl_flag::CLOSE_ANY != 0,
            ),
            orders: OrderTable::new(),
            exits: ExitTable::new(sw.exit_reservation),
            risk: Risk::new(&d),
            counters,
            bar: Bar::default(),
            path: [0; 4],
            seg: 0,
            cand: [Cand::default(); N_CAND],
            n_cand: 0,
            cur: 0,
            seg_eval: 0,
            state: WalkState::Idle,
            latched: None,
            collected: false,
            evaluated: false,
            close_work: false,
            bars: 0,
            last_bar: None,
            late_a: 0,
            trailed: false,
            v5_dropped: 0,
            eval_px_1e6: 0,
            hist: path_points && d.flags & decl_flag::CALC_ON_EVERY_HISTORY_TICK != 0,
            hist_done: HIST_ALL,
        })
    }

    /// Starts `bar`: marks at the open, walks the intrabar path filling the orders committed so
    /// far, and returns the first `Evaluate` (a fill tick when `calc_on_order_fills`, else the bar
    /// close). Infallible by signature: a fault met during the walk is latched, the walk stops,
    /// `Evaluate { bar, intrabar: false }` is returned, and the next `submit` returns the latched
    /// `Err` (§4.8.8). Precondition: the previous bar returned `Done`.
    pub fn open_bar(&mut self, bar: &Bar) -> TvStep {
        if self.state != WalkState::Idle {
            self.bar = *bar;
            return self.latch(BrokerFault::Internal(InternalDefect::PathCursor));
        }
        // COPY: §4.17 #5 — the 64-B bar is kept for the walk and every `Evaluate`.
        self.bar = *bar;
        if self.last_bar != Some(bar.index) {
            self.bars += 1;
            self.last_bar = Some(bar.index);
        }
        self.path = path(bar, self.sw.path_tie);
        // §4.11: the exchange day from the bar's open (no clock), or the bar above 1D.
        let day = if self.chart.timeframe_s > 86_400 {
            i64::from(bar.index)
        } else {
            (bar.open_ms.div_euclid(1000) + i64::from(self.cfg.day_offset_s)).div_euclid(86_400)
        };
        let (eq, net) = (
            self.book.view().equity_1e12,
            self.book.view().netprofit_1e12,
        );
        if self.risk.roll_day(day, eq, net, &mut self.counters) {
            if let Err(f) = self.flatten(self.path[0]) {
                return self.latch(f);
            }
        }
        if let Err(f) = self.tick(self.path[0]) {
            return self.latch(f);
        }
        self.seg = 0;
        self.collected = false;
        self.trailed = false;
        self.evaluated = false;
        self.close_work = false;
        // §4.8.9: history-tick runs on history bars only (`pin:05-hist-tick-runs`).
        self.hist_done = if self.hist && bar.flags & barflag::REALTIME == 0 {
            0
        } else {
            HIST_ALL
        };
        self.state = WalkState::Walking;
        self.walk()
    }

    /// Validates and COMMITS one evaluation's intents (W21, §4.3), resumes the walk and returns
    /// the next step. `intents` may be empty. `Err` → the host aborts the run (D13, backtest).
    pub fn submit(&mut self, intents: &[OrderIntent]) -> Result<TvStep, BrokerFault> {
        let (intrabar, tick_px, hist) = match self.state {
            WalkState::AwaitingSubmit {
                intrabar,
                tick_px,
                hist,
            } => (intrabar, tick_px, hist),
            _ => return Err(BrokerFault::Internal(InternalDefect::PathCursor)),
        };
        self.eval_px_1e6 = tick_px;
        if let Some(f) = self.latched {
            return Err(f);
        }
        let ctx = CommitCtx {
            bar: self.bar.index,
            grid: self.price_grid(),
            allow: self.decl.risk.allow_entry_in,
            position_1e6: self.book.view().position_size_1e6,
            halted: self.risk.halted(),
        };
        self.orders.commit_slice(
            &mut self.exits,
            intents,
            &ctx,
            &self.book,
            &mut self.counters,
        )?;
        // `pin:05-v5-order-limit` (CI-005 / E110): the run that creates the 9001st order faults;
        // the slice is committed but the broker is latched, so nothing of it ever fills.
        if let Err(f) = self.v5_order_check() {
            self.latched = Some(f);
            return Err(f);
        }
        let eval = self.orders.eval();
        self.stamp_born(eval);
        // `pin:05-hist-tick-next-point` (provisional): an order born at the history-tick run at
        // point k fills from point k + 1 on, whatever `cof_next_tick` reads.
        if intrabar && !hist && self.sw.cof_next_tick == CofNextTick::RestOfSegment {
            self.admit_late(eval, tick_px);
        }
        let same_tick = self.sw.close_immediately == CloseImmediately::SameTick;
        if intrabar && same_tick {
            // `immediately` closes born now fill at this execution's tick, before the walk resumes
            // — a history-tick run's too, at its path point (`pin:05-hist-tick-immediately`,
            // provisional).
            let mut i = 0;
            while i < self.orders.len() {
                let o = self.orders.get(i);
                if o.is_live() && o.born_eval == eval && o.flags & pflag::IMMEDIATELY != 0 {
                    if let Err(f) = self.execute_counted(i, tick_px, true) {
                        self.latched = Some(f);
                        return Err(f);
                    }
                }
                i += 1;
            }
        } else if !intrabar {
            let on_close = self.decl.flags & decl_flag::ORDERS_ON_CLOSE != 0;
            let mut i = 0;
            while i < self.orders.len() {
                let o = self.orders.get(i);
                if o.is_live()
                    && o.born_eval == eval
                    && (on_close || (same_tick && o.flags & pflag::IMMEDIATELY != 0))
                {
                    self.close_work = true;
                }
                i += 1;
            }
            self.close_work |= on_close && self.exits.live_rules() > 0;
        }
        self.state = WalkState::Walking;
        Ok(self.walk())
    }

    /// Everything a `strategy.*` variable reads; the host's `BarCtx::broker`. Always current.
    #[must_use]
    pub fn view(&self) -> &BrokerView {
        self.book.view()
    }

    /// Open trade `i` (0 = oldest open) — `BarCtx::open_trade`. Borrowed, never copied.
    #[must_use]
    pub fn open_trade(&self, i: u32) -> Option<&TradeRec> {
        self.book.open_trade(i)
    }

    /// Closed trade `i` (0 = oldest **retained**) — `BarCtx::closed_trade`. `None` past
    /// `view().closed_retained`; Pine's absolute index `k` maps to
    /// `i = k - (closedtrades - closed_retained)`, done by `pine-std`.
    #[must_use]
    pub fn closed_trade(&self, i: u32) -> Option<&TradeRec> {
        self.book.closed_trade(i)
    }

    /// Every refusal and adjustment so far (§4.15).
    #[must_use]
    pub fn counters(&self) -> &BrokerCounters {
        &self.counters
    }

    /// The run header (§4.16): a by-value snapshot (~370 B, cold).
    #[must_use]
    pub fn record(&self) -> RunRecord {
        let v = self.book.view();
        // COPY: §4.17 #6 — the frozen by-value signature, cold.
        RunRecord {
            decl_hash64: self.decl_hash,
            sizing: self.cfg.sizing,
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

    /// Latch `f` (the first one wins) and stop the walk (§4.8.8).
    fn latch(&mut self, f: BrokerFault) -> TvStep {
        if self.latched.is_none() {
            self.latched = Some(f);
        }
        self.state = WalkState::AwaitingSubmit {
            intrabar: false,
            tick_px: self.path[3],
            hist: false,
        };
        TvStep::Evaluate {
            bar: self.bar,
            intrabar: false,
        }
    }

    /// The exit-set context at reference price `px`.
    fn sync_ctx(&self, px: i64) -> SyncCtx {
        let scale = match self.cfg.sizing {
            SizingMode::AsDeclared => None,
            SizingMode::FixedNotional { notional_1e6 } => {
                let pv = self.chart.pointvalue_1e6;
                let eq = self.book.view().equity_1e12;
                Some((
                    unit_qty(notional_1e6, px, pv),
                    declared_qty(&self.decl, px, pv, eq),
                ))
            }
        };
        SyncCtx {
            tick_1e6: self.chart.mintick_1e6,
            // W23 differs by dialect (`ProgramMeta::dialect`, A4.5).
            v6: self.dialect == Dialect::V6,
            step_1e6: self.chart.qty_step_1e6,
            scale,
            grid: self.price_grid(),
        }
    }

    /// The script-price grid (`pin:05-price-rounding`, §4.1): `chart.mintick_1e6` under the
    /// `price_rounding` switch — entries, `strategy.order` and exit legs alike (E113 E129 E130
    /// E131).
    const fn price_grid(&self) -> PriceGrid {
        PriceGrid::tv(self.chart.mintick_1e6, self.sw.price_rounding)
    }

    /// The §4.8.6 verification distance for limit rows.
    fn verify_1e6(&self) -> i64 {
        i64::from(self.decl.fill_limits_ticks).saturating_mul(self.chart.mintick_1e6)
    }

    /// Order `i`'s trigger row on a segment starting at `a` (§4.8.4).
    fn order_level(&self, i: usize) -> Level {
        let o = self.orders.get(i);
        let buy = o.side == 0;
        let stop = if buy {
            Level::BuyStop(o.stop_1e6)
        } else {
            Level::SellStop(o.stop_1e6)
        };
        let limit = if buy {
            Level::BuyLimit(o.limit_1e6)
        } else {
            Level::SellLimit(o.limit_1e6)
        };
        match o.otype {
            otype::LIMIT => limit,
            otype::STOP => stop,
            otype::STOP_LIMIT if o.state == state::WORKING => stop,
            otype::STOP_LIMIT => limit,
            _ => Level::Market,
        }
    }

    /// Set `k`'s `leg` row, or `None` when the leg is absent.
    fn leg_level(&self, k: usize, leg: u16) -> Option<Level> {
        let s = self.exits.set(k);
        let sell = s.side == 1;
        match leg {
            LEG_TP if s.tp_1e6 != 0 => Some(if sell {
                Level::SellLimit(s.tp_1e6)
            } else {
                Level::BuyLimit(s.tp_1e6)
            }),
            LEG_SL if s.sl_1e6 != 0 => Some(if sell {
                Level::SellStop(s.sl_1e6)
            } else {
                Level::BuyStop(s.sl_1e6)
            }),
            LEG_TRAIL if s.legs & crate::exit::LEG_ARMED != 0 => {
                let px = if sell {
                    s.trail_anchor_1e6.saturating_sub(s.trail_off_1e6)
                } else {
                    s.trail_anchor_1e6.saturating_add(s.trail_off_1e6)
                };
                Some(if sell {
                    Level::SellStop(px)
                } else {
                    Level::BuyStop(px)
                })
            }
            _ => None,
        }
    }

    /// `verify` for a row: the distance for limits, 0 otherwise.
    fn verify_for(&self, lv: Level) -> i64 {
        match lv {
            Level::BuyLimit(_) | Level::SellLimit(_) => self.verify_1e6(),
            _ => 0,
        }
    }

    /// Push a candidate if the segment reaches it.
    fn push(&mut self, lv: Level, a: i64, b: i64, seq: u32, what: u16) {
        let v = self.verify_for(lv);
        if let Some((r, _)) = reach(lv, a, b, v, self.sw.fill_limits_price) {
            if self.n_cand < N_CAND {
                self.cand[self.n_cand] = Cand {
                    key: r,
                    seq,
                    what,
                    flags: 0,
                };
                self.n_cand += 1;
            }
        }
    }

    /// Collect segment `seg`'s candidates (§4.8.3): every live record into segments 0–2; into the
    /// close tick (3) or the on-close pass only those `admit` lets through.
    fn collect(&mut self, admit_after: Option<u32>, on_close_only: bool) {
        self.orders.retire_dead();
        let ctx = self.sync_ctx(self.path[0]);
        if let Err(f) = self.exits.sync(&self.book, &ctx) {
            self.latched.get_or_insert(f);
        }
        // `pin:05-v5-order-limit`: exit legs are orders, created when their set is built.
        if let Err(f) = self.v5_order_check() {
            self.latched.get_or_insert(f);
        }
        let (a, b) = segment(&self.path, self.seg);
        let eval = self.orders.eval();
        let all_on_close = self.decl.flags & decl_flag::ORDERS_ON_CLOSE != 0;
        self.n_cand = 0;
        let mut i = 0;
        while i < self.orders.len() {
            // The four scalars the admission reads (`push` borrows `self` mutably).
            let (born_eval, flags, live, seq) = {
                let o = self.orders.get(i);
                (o.born_eval, o.flags, o.is_live(), o.seq)
            };
            let admitted = match admit_after {
                Some(e) => born_eval > e,
                None => true,
            } && (!on_close_only
                || (born_eval == eval
                    && (all_on_close
                        || (self.sw.close_immediately == CloseImmediately::SameTick
                            && flags & pflag::IMMEDIATELY != 0))));
            if live && admitted {
                let lv = self.order_level(i);
                self.push(lv, a, b, seq, u16::try_from(i).unwrap_or(IDX_MASK));
            }
            i += 1;
        }
        let mut k = 0;
        while k < self.exits.sets().len() {
            // The scalars the admission reads (`push` borrows `self` mutably).
            let (born_eval, live, rule) = {
                let s = self.exits.set(k);
                (
                    s.born_eval,
                    s.state == set_state::LIVE && s.r_1e6 > 0,
                    s.rule,
                )
            };
            let admitted = match admit_after {
                Some(e) => born_eval > e,
                None => true,
            } && (!on_close_only || (all_on_close && born_eval == eval));
            if live && admitted {
                let seq = self.exits.rule(usize::from(rule)).seq;
                let mut leg = LEG_TP;
                while leg <= LEG_TRAIL {
                    if let Some(lv) = self.leg_level(k, leg) {
                        let what = WHAT_SET | (leg << LEG_SHIFT) | u16::try_from(k).unwrap_or(0);
                        self.push(lv, a, b, seq, what);
                    }
                    leg += 1;
                }
            }
            k += 1;
        }
        self.seg_eval = eval;
        sort_cands(&mut self.cand[..self.n_cand], b >= a);
        self.cur = 0;
        self.collected = true;
    }

    /// The walk (§4.8.7).
    fn walk(&mut self) -> TvStep {
        loop {
            if let Some(f) = self.latched {
                return self.latch(f);
            }
            if self.seg <= 3 {
                if !self.collected {
                    let after = if self.seg == 3 {
                        Some(self.seg_eval)
                    } else {
                        None
                    };
                    self.collect(after, false);
                }
                // §4.8.9: at path point `seg` (0–2) the fills at the point first, then the
                // history-tick run (`pin:05-hist-tick-fill-first`, provisional).
                if self.seg <= 2 && self.hist_done & (1 << self.seg) == 0 {
                    let at = self.path[self.seg];
                    if let Some(step) = self.run_ticks(true, Some(at)) {
                        return step;
                    }
                    self.hist_done |= 1 << self.seg;
                    self.state = WalkState::AwaitingSubmit {
                        intrabar: true,
                        tick_px: at,
                        hist: true,
                    };
                    return TvStep::Evaluate {
                        bar: self.developing(self.seg),
                        intrabar: true,
                    };
                }
                if let Some(step) = self.run_ticks(true, None) {
                    return step;
                }
                if self.seg <= 2 {
                    if !self.trailed {
                        self.trailed = true;
                        let (a, b) = segment(&self.path, self.seg);
                        let armed = self.exits.after_segment(a, b);
                        if self.sw.trail_arm_fire == TrailArmFire::SameSegment
                            && self.push_tail(armed, b)
                        {
                            continue; // the armed trails' fills at `b`, then the tick
                        }
                    }
                    if let Err(f) = self.tick(self.path[self.seg + 1]) {
                        return self.latch(f);
                    }
                }
                self.seg += 1;
                self.collected = false;
                self.trailed = false;
            } else if !self.evaluated {
                self.evaluated = true;
                self.state = WalkState::AwaitingSubmit {
                    intrabar: false,
                    tick_px: self.path[3],
                    hist: false,
                };
                return TvStep::Evaluate {
                    bar: self.bar,
                    intrabar: false,
                };
            } else if self.close_work {
                self.close_work = false;
                self.seg = 3;
                self.collect(None, true);
                self.seg = 4;
                if let Some(step) = self.run_ticks(false, None) {
                    return step;
                }
            } else if self.cur < self.n_cand {
                // The on-close pass resumed after a `cof_close_tick = Yes` execution.
                if let Some(step) = self.run_ticks(false, None) {
                    return step;
                }
            } else {
                self.state = WalkState::Idle;
                return TvStep::Done;
            }
        }
    }

    /// Process the collected candidates tick by tick (§4.8.7 lines 10–20); `Some` when the walk
    /// must return (an intrabar `Evaluate`, or a latched fault). `stop_at = Some(p)`: only the
    /// tick at `p`, the segment's start point (candidates are in path order, §4.8.9).
    fn run_ticks(&mut self, may_evaluate: bool, stop_at: Option<i64>) -> Option<TvStep> {
        let c = self.path[3];
        while self.cur < self.n_cand {
            let px = self.cand[self.cur].key;
            if stop_at.is_some_and(|p| p != px) {
                return None;
            }
            let mut any = false;
            while self.cur < self.n_cand && self.cand[self.cur].key == px {
                let k = self.cand[self.cur];
                self.cur += 1;
                match self.fill(k) {
                    Ok(filled) => any |= filled,
                    Err(f) => return Some(self.latch(f)),
                }
            }
            if any {
                // "on every … order execution event" (§4.10).
                let (eq, peak) = (self.book.view().equity_1e12, self.book.peak_equity_1e12());
                if self.risk.check(eq, peak, &mut self.counters) {
                    if let Err(f) = self.flatten(px) {
                        return Some(self.latch(f));
                    }
                }
                if let Err(f) = self.margin_check(px) {
                    return Some(self.latch(f));
                }
            }
            let at_close = self.seg >= 2 && px == c;
            // `cof_close_tick` (§4.2): `No` — a fill at the close (segment 2 at `c`, the close
            // tick, the on-close pass) requests no execution; `Yes` — every fill tick does.
            let close_yes = self.sw.cof_close_tick == CofCloseTick::Yes;
            if any
                && (close_yes || (may_evaluate && !at_close))
                && self.decl.flags & decl_flag::CALC_ON_ORDER_FILLS != 0
            {
                self.counters.reevals_on_order_fill += 1;
                self.state = WalkState::AwaitingSubmit {
                    intrabar: true,
                    tick_px: px,
                    hist: false,
                };
                return Some(TvStep::Evaluate {
                    bar: self.bar,
                    intrabar: true,
                });
            }
        }
        None
    }

    /// Try candidate `k`; `Ok(true)` when something filled.
    fn fill(&mut self, k: Cand) -> Result<bool, BrokerFault> {
        let seg = self.seg.min(3);
        let (a, b) = segment(&self.path, seg);
        let a = if k.flags & CAND_TAIL != 0 {
            b
        } else if k.flags & CAND_LATE != 0 {
            self.late_a
        } else {
            a
        };
        let idx = usize::from(k.what & IDX_MASK);
        if k.what & WHAT_SET != 0 {
            return self.fill_leg(idx, (k.what >> LEG_SHIFT) & 3, a, b);
        }
        if idx >= self.orders.len() {
            return Ok(false);
        }
        // The scalars this reads (the stop-limit stage rewrites `self.cand` and the record).
        let (live, born_eval, stop_1e6, ot, st, seq) = {
            let o = self.orders.get(idx);
            (
                o.is_live(),
                o.born_eval,
                o.stop_1e6,
                o.otype,
                o.state,
                o.seq,
            )
        };
        if !live || born_eval > self.seg_eval {
            return Ok(false);
        }
        let a_eff = if k.flags & CAND_REKEYED != 0 {
            stop_1e6
        } else {
            a
        };
        let lv = self.order_level(idx);
        let v = self.verify_for(lv);
        let Some((_, px)) = reach(lv, a_eff, b, v, self.sw.fill_limits_price) else {
            return Ok(false);
        };
        // A re-keyed limit stage is placed in path order after the stop's tick.
        if ot == otype::STOP_LIMIT && st == state::WORKING {
            // Stage 1: the stop triggers, no fill; the limit stage takes over from `a := S`.
            self.orders.get_mut(idx).state = state::STOP_TRIGGERED;
            let lim = self.order_level(idx);
            let v = self.verify_for(lim);
            if let Some((r, _)) = reach(lim, stop_1e6, b, v, self.sw.fill_limits_price) {
                self.cur -= 1;
                self.cand[self.cur] = Cand {
                    key: r,
                    seq,
                    what: k.what,
                    flags: CAND_REKEYED,
                };
                let up = b >= a;
                let mut j = self.cur;
                while j + 1 < self.n_cand && !before(&self.cand[j], &self.cand[j + 1], up) {
                    self.cand.swap(j, j + 1);
                    j += 1;
                }
            }
            return Ok(false);
        }
        let slipped = matches!(lv, Level::Market | Level::BuyStop(_) | Level::SellStop(_));
        self.execute_counted(idx, px, slipped)
    }

    /// `tick(px)` (§4.8.7): mark, then the risk rules at that price.
    fn tick(&mut self, px: i64) -> Result<(), BrokerFault> {
        self.book.mark(px);
        let (eq, peak) = (self.book.view().equity_1e12, self.book.peak_equity_1e12());
        if self.risk.check(eq, peak, &mut self.counters) {
            self.flatten(px)?;
        }
        self.margin_check(px)
    }

    /// The margin call at `px` (§4.9.2; at most one per tick), then the liquidation price (§4.9.3).
    fn margin_check(&mut self, px: i64) -> Result<(), BrokerFault> {
        // The four scalars the call reads (`apply_fill` below borrows the book mutably).
        let (pos, avg, init, net) = {
            let v = self.book.view();
            (
                v.position_size_1e6,
                v.position_avg_price_1e6,
                v.initial_capital_1e12,
                v.netprofit_1e12,
            )
        };
        let m = if pos > 0 {
            self.decl.margin_long_pct_1e6
        } else {
            self.decl.margin_short_pct_1e6
        };
        let pv = self.chart.pointvalue_1e6;
        let size = margin_call_qty(pos, avg, px, init, net, m, pv, self.chart.qty_step_1e6);
        if size > 0 {
            let buy = pos < 0;
            let fpx = if self.sw.forced_fill_slippage == ForcedFillSlippage::Slipped {
                slip(px, buy, self.decl.slippage_ticks, self.chart.mintick_1e6)
            } else {
                px
            };
            let fee = commission_1e12(&self.decl, fpx, size, pv, true, self.sw.cash_per_order);
            let src = FillSrc {
                // `pin:05-exit-signal-text` (CI-032; E071 E072): "Margin call".
                id: crate::signal::MARGIN_CALL,
                sym: pine_ir::Sym::NONE,
                named_entry: StrKey::NONE,
                born_bar: self.bar.index,
                born_eval: self.orders.eval(),
                is_entry: false,
                lot_flags: 0,
            };
            let q = if buy { size } else { -size };
            let cause = pine_ir::trade_flag::LIQUIDATION;
            self.book
                .apply_fill(q, fpx, fee, &src, self.bar.index, self.bar.open_ms, cause)?;
            self.counters.margin_calls += 1;
            self.exits.rederive_now(self.book.view().position_size_1e6);
        }
        // After the call: the same four scalars, re-read.
        let (pos, avg, init, net) = {
            let v = self.book.view();
            (
                v.position_size_1e6,
                v.position_avg_price_1e6,
                v.initial_capital_1e12,
                v.netprofit_1e12,
            )
        };
        let m = if pos > 0 {
            self.decl.margin_long_pct_1e6
        } else {
            self.decl.margin_short_pct_1e6
        };
        let p = liquidation_price(pos, avg, init, net, m, pv);
        self.book.set_liquidation_price(p);
        Ok(())
    }

    /// A risk trip (§4.10): cancel every pending order and exit rule, and close the position with
    /// a market fill at `px` (slipped per `forced_fill_slippage`), its trades flagged
    /// `RISK_FLATTEN`.
    fn flatten(&mut self, px: i64) -> Result<(), BrokerFault> {
        self.orders.kill_all();
        self.exits.cancel(None);
        let pos = self.book.view().position_size_1e6;
        if pos == 0 {
            return Ok(());
        }
        let buy = pos < 0;
        let fpx = if self.sw.forced_fill_slippage == ForcedFillSlippage::Slipped {
            slip(px, buy, self.decl.slippage_ticks, self.chart.mintick_1e6)
        } else {
            px
        };
        let q_abs = pos.saturating_abs();
        let pv = self.chart.pointvalue_1e6;
        let fee = commission_1e12(&self.decl, fpx, q_abs, pv, true, self.sw.cash_per_order);
        let src = FillSrc {
            // `pin:05-exit-signal-text` (CI-032; E092 E093 E094): the tripping rule's text.
            id: self.risk.trip_signal(),
            sym: pine_ir::Sym::NONE,
            named_entry: StrKey::NONE,
            born_bar: self.bar.index,
            born_eval: self.orders.eval(),
            is_entry: false,
            lot_flags: 0,
        };
        let cause = pine_ir::trade_flag::RISK_FLATTEN;
        self.book.apply_fill(
            -pos,
            fpx,
            fee,
            &src,
            self.bar.index,
            self.bar.open_ms,
            cause,
        )?;
        self.exits.rederive_now(0);
        Ok(())
    }

    /// The developing bar at path point `k` (0–2) of a history-tick run (§4.8.9,
    /// `pin:05-hist-tick-bar`, provisional): the open, the extremes the path has reached, the
    /// point's price as the close; the bar's full volume, its times and index unchanged;
    /// `barflag::CONFIRMED` dropped.
    fn developing(&self, k: usize) -> Bar {
        let (mut hi, mut lo) = (self.path[0], self.path[0]);
        let mut j = 1;
        while j <= k && j < 3 {
            hi = hi.max(self.path[j]);
            lo = lo.min(self.path[j]);
            j += 1;
        }
        // COPY: the 64-B bar, once per history-tick run (≤ 3 a bar) — the run's `Evaluate`
        // carries it by value (frozen `TvStep`) — editing `self.bar` in place rejected: the walk
        // and the bar-close evaluation read the full bar.
        Bar {
            high_1e6: hi,
            low_1e6: lo,
            close_1e6: self.path[k.min(2)],
            flags: self.bar.flags & !barflag::CONFIRMED,
            ..self.bar
        }
    }

    /// `pin:05-v5-order-limit` (CI-005 / E110, R-P2): in a v5 compilation the 9001st order faults
    /// `Script(OrderTableFull)` — TradingView's RE10138 "Maximum number of orders (9000) was
    /// reached.", a plain runtime error (ScriptRefused; no new kind, lead answer 2). An order is a
    /// newly created pending record or a new exit leg; a modification (a re-send of the same live
    /// order), an intent refused at commit and an order that dies unfilled at its fill attempt
    /// (zero size, pyramiding, nothing to close) are not counted (lead answer 3). v6 never faults.
    fn v5_order_check(&self) -> Result<(), BrokerFault> {
        if self.dialect != Dialect::V5 {
            return Ok(());
        }
        let n = self
            .orders
            .created()
            .saturating_add(self.exits.legs_created())
            .saturating_sub(self.v5_dropped);
        if n > cap::V5_ORDERS {
            return Err(BrokerFault::Script(ScriptRefusal::OrderTableFull));
        }
        Ok(())
    }

    /// `execute_order`, counting a record that died unfilled (`Ok(false)`: zero size, refused) out
    /// of the v5 orders (`pin:05-v5-order-limit`).
    fn execute_counted(&mut self, i: usize, px: i64, slipped: bool) -> Result<bool, BrokerFault> {
        let filled = self.execute_order(i, px, slipped)?;
        if !filled {
            self.v5_dropped = self.v5_dropped.saturating_add(1);
        }
        Ok(filled)
    }

    /// Size, price and book order `i` at `px` (before slippage when `slipped`); §4.7, §4.8.6.
    /// Every `Ok(false)` kills the record unfilled.
    fn execute_order(&mut self, i: usize, px0: i64, slipped: bool) -> Result<bool, BrokerFault> {
        // The scalars the fill reads (`apply_fill` and the counters borrow `self` mutably).
        let (kind, side, id, id_sym, born_bar, born_eval, gen_open, ot, filled, qty, qty_pct) = {
            let o = self.orders.get(i);
            (
                o.kind,
                o.side,
                o.id,
                o.id_sym,
                o.born_bar,
                o.born_eval,
                o.gen_open,
                o.otype,
                o.filled_1e6,
                o.qty_1e6,
                o.qty_pct_1e6,
            )
        };
        let pos = self.book.view().position_size_1e6;
        let pv = self.chart.pointvalue_1e6;
        let step = self.chart.qty_step_1e6;
        let buy = if kind == IntentKind::Close as u8 || kind == IntentKind::CloseAll as u8 {
            pos < 0
        } else {
            side == 0
        };
        let px = if slipped {
            slip(px0, buy, self.decl.slippage_ticks, self.chart.mintick_1e6)
        } else {
            px0
        };
        let q_signed: i64 = if kind == IntentKind::Entry as u8 {
            let q = self.resolve_qty(self.orders.get(i), px)?;
            let gen_open = match self.sw.pyramiding {
                PyramidCheck::FillTime => None,
                PyramidCheck::GenerationTime => Some(gen_open),
            };
            let Some(sz) = size_entry(
                &self.book,
                buy,
                q,
                born_eval,
                gen_open,
                self.decl.pyramiding,
                self.max_position(px),
                0,
                Some(&mut self.counters),
            ) else {
                self.orders.kill(i);
                return Ok(false);
            };
            // §4.9.1: the funds check on the opening part, before the step floor.
            let m = if buy {
                self.decl.margin_long_pct_1e6
            } else {
                self.decl.margin_short_pct_1e6
            };
            let pos_r = if sz.close_1e6 > 0 {
                0
            } else {
                pos.saturating_abs()
            };
            let eq = self.book.equity_at(px);
            let mode = self.sw.margin_funds;
            let Some(open) = funds_check(sz.open_1e6, pos_r, px, pv, m, eq, step, mode) else {
                self.counters.sized_to_zero += 1;
                self.orders.kill(i);
                return Ok(false);
            };
            let open = floor_step(open, step);
            if open == 0 {
                self.counters.sized_to_zero += 1;
                self.orders.kill(i);
                return Ok(false);
            }
            let total = sz.close_1e6.saturating_add(open);
            if buy {
                total
            } else {
                -total
            }
        } else if kind == IntentKind::Order as u8 {
            let q = floor_step(self.resolve_qty(self.orders.get(i), px)?, step);
            if q == 0 {
                self.counters.sized_to_zero += 1;
                self.orders.kill(i);
                return Ok(false);
            }
            if buy {
                q
            } else {
                -q
            }
        } else if kind == IntentKind::Close as u8 {
            let total = self.open_qty_of(id);
            if total == 0 {
                self.orders.kill(i); // nothing open with that id: it lapses
                return Ok(false);
            }
            let q = if qty > 0 {
                let q = match self.cfg.sizing {
                    SizingMode::AsDeclared => qty,
                    SizingMode::FixedNotional { notional_1e6 } => {
                        let eq = self.book.view().equity_1e12;
                        let d = declared_qty(&self.decl, px, pv, eq);
                        n_rule_scale(unit_qty(notional_1e6, px, pv), qty, d)
                    }
                };
                q.min(total)
            } else if qty_pct > 0 {
                let pct = mul_div(i128::from(total), i128::from(qty_pct), 100 * 1_000_000);
                floor_step(sat_i64(pct), step)
            } else {
                total
            };
            if q == 0 {
                self.counters.sized_to_zero += 1;
                self.orders.kill(i);
                return Ok(false);
            }
            if pos > 0 {
                -q
            } else {
                q
            }
        } else {
            if pos == 0 {
                self.orders.kill(i);
                return Ok(false);
            }
            -pos
        };
        let q_abs = q_signed.saturating_abs();
        let fee = commission_1e12(
            &self.decl,
            px,
            q_abs,
            pv,
            filled == 0,
            self.sw.cash_per_order,
        );
        let is_entry = kind == IntentKind::Entry as u8 || kind == IntentKind::Order as u8;
        let mut lot_flags = if ot == otype::MARKET {
            0
        } else {
            lot_flag::PRICE_BASED
        };
        if kind == IntentKind::Entry as u8 {
            lot_flags |= lot_flag::PYRAMID;
        }
        let closes = kind == IntentKind::Close as u8 || kind == IntentKind::CloseAll as u8;
        let src = FillSrc {
            // `pin:05-exit-signal-text` (CI-032): a close's fill carries its exit order's id.
            id: if closes {
                self.orders.get(i).exit_key
            } else {
                id
            },
            sym: id_sym,
            named_entry: if kind == IntentKind::Close as u8 {
                id
            } else {
                StrKey::NONE
            },
            born_bar,
            born_eval,
            is_entry,
            lot_flags,
        };
        self.book
            .apply_fill(q_signed, px, fee, &src, self.bar.index, self.bar.open_ms, 0)?;
        self.orders.get_mut(i).filled_1e6 = filled.saturating_add(q_abs);
        self.orders.on_member_fill(i, q_abs, &mut self.counters);
        self.orders.kill(i);
        self.risk.note_fill();
        self.exits.rederive_now(self.book.view().position_size_1e6);
        Ok(true)
    }

    /// Fill set `k`'s `leg` on segment `(a, b)` (§4.5.5).
    fn fill_leg(&mut self, k: usize, leg: u16, a: i64, b: i64) -> Result<bool, BrokerFault> {
        if k >= self.exits.sets().len() {
            return Ok(false);
        }
        let (sell_side, r_1e6, live) = {
            let s = self.exits.set(k);
            (
                s.side,
                s.r_1e6,
                s.state == set_state::LIVE && s.r_1e6 > 0 && s.born_eval <= self.seg_eval,
            )
        };
        if !live {
            return Ok(false);
        }
        let Some(lv) = self.leg_level(k, leg) else {
            return Ok(false);
        };
        let v = self.verify_for(lv);
        let Some((_, px0)) = reach(lv, a, b, v, self.sw.fill_limits_price) else {
            return Ok(false);
        };
        let px = match lv {
            Level::BuyStop(_) | Level::SellStop(_) => slip(
                px0,
                sell_side == 0,
                self.decl.slippage_ticks,
                self.chart.mintick_1e6,
            ),
            _ => px0,
        };
        let pv = self.chart.pointvalue_1e6;
        let fee = commission_1e12(&self.decl, px, r_1e6, pv, true, self.sw.cash_per_order);
        self.exits
            .fill_set(k, px, fee, &mut self.book, self.bar.index, self.bar.open_ms)?;
        self.risk.note_fill();
        self.exits.rederive_now(self.book.view().position_size_1e6);
        Ok(true)
    }

    /// Stamp the records (re)born at evaluation `eval` with what the generation-time readings
    /// need (`pyramiding = GenerationTime`, `pct_equity_at = OrderTime`); a no-op on the defaults.
    fn stamp_born(&mut self, eval: u32) {
        let gen_py = self.sw.pyramiding == PyramidCheck::GenerationTime;
        let gen_eq = self.sw.pct_equity_at == PctEquityAt::OrderTime;
        if !gen_py && !gen_eq {
            return;
        }
        let open = open_entry_trades(&self.book);
        let eq = self.book.view().equity_1e12;
        let mut i = 0;
        while i < self.orders.len() {
            let o = self.orders.get_mut(i);
            if o.born_eval == eval {
                if gen_py {
                    o.gen_open = open;
                }
                if gen_eq {
                    o.set_gen_equity_1e12(eq);
                }
            }
            i += 1;
        }
    }

    /// `cof_next_tick = RestOfSegment` (`pin:05-cof-next-tick`, the alternative): the records
    /// (re)born at the intrabar execution `eval` at `tick_px` become candidates for the rest of
    /// the current segment, reached from `tick_px`; their stale slots are dropped first. Exit legs
    /// keep the next-segment rule: sets are derived at segment collection
    /// (`pin:05-cof-next-tick-exits`).
    fn admit_late(&mut self, eval: u32, tick_px: i64) {
        let (a, b) = segment(&self.path, self.seg.min(3));
        let mut w = self.cur;
        let mut r = self.cur;
        while r < self.n_cand {
            let c = self.cand[r];
            let idx = usize::from(c.what & IDX_MASK);
            let reborn = c.what & WHAT_SET == 0
                && idx < self.orders.len()
                && self.orders.get(idx).born_eval == eval;
            if !reborn {
                // COPY: a 16-B candidate record compacts one slot (§4.17: index records).
                self.cand[w] = c;
                w += 1;
            }
            r += 1;
        }
        self.n_cand = w;
        let mut i = 0;
        while i < self.orders.len() {
            let o = self.orders.get(i);
            if o.is_live() && o.born_eval == eval {
                let (seq, lv) = (o.seq, self.order_level(i));
                let v = self.verify_for(lv);
                if let Some((key, _)) = reach(lv, tick_px, b, v, self.sw.fill_limits_price) {
                    if self.n_cand < N_CAND {
                        self.cand[self.n_cand] = Cand {
                            key,
                            seq,
                            what: u16::try_from(i).unwrap_or(IDX_MASK),
                            flags: CAND_LATE,
                        };
                        self.n_cand += 1;
                    }
                }
            }
            i += 1;
        }
        self.late_a = tick_px;
        self.seg_eval = eval;
        sort_cands(&mut self.cand[self.cur..self.n_cand], b >= a);
    }

    /// `trail_arm_fire = SameSegment` (`pin:05-trail-same-segment`, the alternative): the trails
    /// armed by this segment (`armed`, bit = set) whose stop the end point `b` reaches become the
    /// candidates at `b`. `true` when there is one.
    fn push_tail(&mut self, armed: u128, b: i64) -> bool {
        self.n_cand = 0;
        self.cur = 0;
        let mut k = 0;
        while k < self.exits.sets().len() && k < 128 {
            if armed & (1_u128 << k) != 0 {
                if let Some(lv) = self.leg_level(k, LEG_TRAIL) {
                    if reach(lv, b, b, 0, self.sw.fill_limits_price).is_some()
                        && self.n_cand < N_CAND
                    {
                        let seq = self.exits.rule(usize::from(self.exits.set(k).rule)).seq;
                        let what =
                            WHAT_SET | (LEG_TRAIL << LEG_SHIFT) | u16::try_from(k).unwrap_or(0);
                        self.cand[self.n_cand] = Cand {
                            key: b,
                            seq,
                            what,
                            flags: CAND_TAIL,
                        };
                        self.n_cand += 1;
                    }
                }
            }
            k += 1;
        }
        let (a0, _) = segment(&self.path, self.seg);
        sort_cands(&mut self.cand[..self.n_cand], b >= a0);
        self.n_cand > 0
    }

    /// The order's quantity at reference price `px` (§4.7.1–§4.7.2), net of OCA reductions.
    fn resolve_qty(&self, o: &crate::orders::PendingOrder, px: i64) -> Result<i64, BrokerFault> {
        let pv = self.chart.pointvalue_1e6;
        // `pct_equity_at` (`pin:05-pct-equity-time`): `OrderTime` reads the equity stamped at the
        // order's (re)birth.
        let eq = match self.sw.pct_equity_at {
            PctEquityAt::FillTime => self.book.view().equity_1e12,
            PctEquityAt::OrderTime => o.gen_equity_1e12(),
        };
        let explicit = o.qty_1e6 > 0;
        // `pin:05-qty-reference-price` (CI-035; E045 E047 E048 E050): TradingView sizes a default-
        // quantity MARKET order at the price its creating evaluation saw (the bar's close for a
        // bar-close run: E045 trade 1, 10 % of 1 000 000 at the order bar's close 77249.5 →
        // 1.294506, not 77249.6's fill → 1.294505; 45/45 trades of E047 / E048 / E050, 44/45 of
        // E045, where the fill price matches 23), and a limit / stop order at its fill (E046 E049:
        // 39/39 limit fills sized at their fill price; a gapped limit, uncaptured, keeps §4.7.3's
        // fill price). The 1e-6 floor stays `pin:05-qty-rounding`.
        let ref_px = if o.otype == otype::MARKET && self.eval_px_1e6 > 0 {
            self.eval_px_1e6
        } else {
            px
        };
        let q = match self.cfg.sizing {
            SizingMode::AsDeclared => {
                if explicit {
                    o.qty_1e6
                } else {
                    declared_qty(&self.decl, ref_px, pv, eq)
                }
            }
            SizingMode::FixedNotional { notional_1e6 } => {
                let u = unit_qty(notional_1e6, px, pv);
                if explicit && o.kind == IntentKind::Entry as u8 {
                    n_rule_entry(u, o.qty_1e6, declared_qty(&self.decl, px, pv, eq))?
                } else if explicit {
                    n_rule_scale(u, o.qty_1e6, declared_qty(&self.decl, px, pv, eq))
                } else {
                    u
                }
            }
        };
        Ok((q - o.oca_reduced_1e6).max(0))
    }

    /// `strategy.risk.max_position_size` in contracts at `px` (0 = none).
    fn max_position(&self, px: i64) -> i64 {
        let bound = self.decl.risk.max_position_size_1e6;
        match self.cfg.sizing {
            SizingMode::FixedNotional { notional_1e6 } if bound > 0 => {
                let pv = self.chart.pointvalue_1e6;
                let eq = self.book.view().equity_1e12;
                let d = declared_qty(&self.decl, px, pv, eq);
                n_rule_scale(unit_qty(notional_1e6, px, pv), bound, d)
            }
            _ => bound,
        }
    }

    /// Σ|qty| of the open trades whose entry id is `id`.
    fn open_qty_of(&self, id: StrKey) -> i64 {
        let mut t = 0;
        let mut sum = 0_i64;
        while let Some(l) = self.book.open_trade(t) {
            if l.entry_id == id {
                sum = sum.saturating_add(l.qty_1e6.saturating_abs());
            }
            t += 1;
        }
        sum
    }
}

#[cfg(test)]
impl TvBroker {
    /// A broker on another reading of the §4.2 switches (I38).
    pub(crate) fn with_switches(
        meta: &ProgramMeta,
        cfg: TvCfg,
        sw: Switches,
    ) -> Result<Self, DiagCode> {
        let mut b = Self::build(meta, cfg, sw, BrokerCounters::default())?;
        b.exits.set_exit_qty(sw.exit_qty);
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::{TvBroker, TvStep};
    use crate::switch::{PathTie, PriceRounding, SWITCHES};
    use pine_ir::{
        intent_flag as f, Bar, ChartInfo, IntentKind, OrderIntent, ProgramMeta, SizingMode, StrKey,
        StrategyDecl, TvCfg,
    };

    const E: i64 = 1_000_000;

    fn meta() -> ProgramMeta {
        ProgramMeta {
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
        }
    }

    const CFG: TvCfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };

    fn bar(i: u32, o: i64, h: i64, l: i64, c: i64) -> Bar {
        Bar {
            open_1e6: o,
            high_1e6: h,
            low_1e6: l,
            close_1e6: c,
            index: i,
            ..Bar::default()
        }
    }

    fn run(b: &mut TvBroker, x: &Bar, intents: &[OrderIntent]) {
        let mut step = b.open_bar(x);
        let mut first = true;
        while let TvStep::Evaluate { .. } = step {
            let s = if first { intents } else { &[] };
            first = false;
            step = match b.submit(s) {
                Ok(s) => s,
                Err(e) => panic!("{e:?}"),
            };
        }
    }

    fn entry_limit(px: i64) -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(1),
            flags: f::LIMIT,
            limit_1e6: px,
            ..OrderIntent::default()
        }
    }

    #[test]
    fn price_rounding_nearest_tick_and_none() {
        // A buy limit at 98.123456 on a 0.01 tick: rounded to 98.12 by default; kept as given
        // under `price_rounding = None`.
        let x0 = bar(0, 100 * E, 101 * E, 99 * E, 100 * E);
        let x1 = bar(1, 100 * E, 100 * E, 97 * E, 98 * E);
        let mut b = TvBroker::new(&meta(), CFG).unwrap_or_else(|_| unreachable!());
        run(&mut b, &x0, &[entry_limit(98_123_456)]);
        run(&mut b, &x1, &[]);
        assert_eq!(b.open_trade(0).map(|t| t.entry_px_1e6), Some(98_120_000));
        let sw = crate::switch::Switches {
            price_rounding: PriceRounding::None,
            ..SWITCHES
        };
        let mut b = TvBroker::with_switches(&meta(), CFG, sw).unwrap_or_else(|_| unreachable!());
        run(&mut b, &x0, &[entry_limit(98_123_456)]);
        run(&mut b, &x1, &[]);
        assert_eq!(b.open_trade(0).map(|t| t.entry_px_1e6), Some(98_123_456));
    }

    #[test]
    fn path_tie_high_first() {
        // Open equidistant from high and low (`pin:05-path-tie`): LowFirst fills the 98 buy limit
        // first and pyramiding = 1 refuses the 102 buy stop; HighFirst fills the stop first.
        let x0 = bar(0, 100 * E, 101 * E, 99 * E, 100 * E);
        let x1 = bar(1, 100 * E, 102 * E, 98 * E, 100 * E);
        let stop = OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(2),
            flags: f::STOP,
            stop_1e6: 102 * E,
            seq: 1,
            ..OrderIntent::default()
        };
        let lim = entry_limit(98 * E);
        let mut b = TvBroker::new(&meta(), CFG).unwrap_or_else(|_| unreachable!());
        run(&mut b, &x0, &[lim, stop]);
        run(&mut b, &x1, &[]);
        assert_eq!(b.open_trade(0).map(|t| t.entry_id), Some(StrKey(1)));
        let sw = crate::switch::Switches {
            path_tie: PathTie::HighFirst,
            ..SWITCHES
        };
        let mut b = TvBroker::with_switches(&meta(), CFG, sw).unwrap_or_else(|_| unreachable!());
        run(&mut b, &x0, &[lim, stop]);
        run(&mut b, &x1, &[]);
        assert_eq!(b.open_trade(0).map(|t| t.entry_id), Some(StrKey(2)));
    }

    fn close_now() -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Close,
            from_entry: StrKey(1),
            flags: f::IMMEDIATELY,
            ..OrderIntent::default()
        }
    }

    fn market_entry() -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(1),
            ..OrderIntent::default()
        }
    }

    #[test]
    fn close_immediately_same_tick_and_next_tick() {
        // `pin:U11-close-immediately`: SameTick fills at this bar's close (103); NextTick at the
        // next bar's open (105).
        let x0 = bar(0, 100 * E, 101 * E, 99 * E, 100 * E);
        let x2 = bar(2, 100 * E, 104 * E, 99 * E, 103 * E);
        let x3 = bar(3, 105 * E, 106 * E, 104 * E, 105 * E);
        let exit_px = |sw: crate::switch::Switches| {
            let mut b =
                TvBroker::with_switches(&meta(), CFG, sw).unwrap_or_else(|_| unreachable!());
            run(&mut b, &x0, &[market_entry()]);
            run(&mut b, &bar(1, 100 * E, 101 * E, 99 * E, 100 * E), &[]);
            run(&mut b, &x2, &[close_now()]);
            run(&mut b, &x3, &[]);
            b.closed_trade(0).map(|t| t.exit_px_1e6)
        };
        assert_eq!(exit_px(SWITCHES), Some(103 * E));
        let next = crate::switch::Switches {
            close_immediately: crate::switch::CloseImmediately::NextTick,
            ..SWITCHES
        };
        assert_eq!(exit_px(next), Some(105 * E));
    }

    #[test]
    fn forced_fill_slippage_slipped_and_unslipped() {
        // `pin:U10-risk-rules`: the risk flatten at 97 slips 10 ticks (96.90) by default.
        let mut m = meta();
        m.decl.slippage_ticks = 10;
        m.decl.risk.max_drawdown_1e6 = 2 * E;
        let flat_px = |sw: crate::switch::Switches| {
            let mut b = TvBroker::with_switches(&m, CFG, sw).unwrap_or_else(|_| unreachable!());
            run(
                &mut b,
                &bar(0, 100 * E, 101 * E, 99 * E, 100 * E),
                &[market_entry()],
            );
            run(&mut b, &bar(1, 100 * E, 101 * E, 97 * E, 98 * E), &[]);
            b.closed_trade(0).map(|t| t.exit_px_1e6)
        };
        assert_eq!(flat_px(SWITCHES), Some(96_900_000));
        let un = crate::switch::Switches {
            forced_fill_slippage: crate::switch::ForcedFillSlippage::Unslipped,
            ..SWITCHES
        };
        assert_eq!(flat_px(un), Some(97 * E));
    }
    fn with(sw: crate::switch::Switches, m: &ProgramMeta) -> TvBroker {
        TvBroker::with_switches(m, CFG, sw).unwrap_or_else(|_| unreachable!())
    }

    #[test]
    fn pyramiding_generation_time() {
        // `pin:05-pyramiding-check`: pyramiding 1, A (limit 98) born on bar 0, B (limit 96) on
        // bar 1, both reached on bar 2. FillTime counts A's trade at B's fill → B refused;
        // GenerationTime counts the trades open at B's birth (none) → B fills.
        let a = entry_limit(98 * E);
        let b2 = OrderIntent {
            id: StrKey(2),
            limit_1e6: 96 * E,
            seq: 1,
            ..entry_limit(0)
        };
        let trades = |sw: crate::switch::Switches| {
            let mut b = with(sw, &meta());
            run(&mut b, &bar(0, 100 * E, 101 * E, 99 * E, 100 * E), &[a]);
            run(&mut b, &bar(1, 100 * E, 101 * E, 99 * E, 100 * E), &[b2]);
            run(&mut b, &bar(2, 100 * E, 100 * E, 95 * E, 96 * E), &[]);
            (b.view().opentrades, b.counters().entries_refused_pyramiding)
        };
        assert_eq!(trades(SWITCHES), (1, 1));
        let gen = crate::switch::Switches {
            pyramiding: crate::switch::PyramidCheck::GenerationTime,
            ..SWITCHES
        };
        assert_eq!(trades(gen), (2, 0));
    }

    #[test]
    fn pct_equity_at_order_time() {
        // `pin:05-pct-equity-time`: 10 % of equity; B is generated at bar 1's close (equity
        // 100 000, the 100-lot A marked at 100) and fills at bar 2's open, 110 (equity 101 000).
        // `pin:05-qty-reference-price` (CI-035): the market order B sizes at its creating
        // evaluation's price, bar 1's close 100, not at the 110 fill.
        let mut m = meta();
        m.decl.pyramiding = 2;
        m.decl.default_qty_type = pine_ir::QtyType::PercentOfEquity;
        m.decl.default_qty_value_1e6 = 10 * E;
        let a = OrderIntent {
            flags: f::QTY,
            qty_1e6: 100 * E,
            ..market_entry()
        };
        let b2 = OrderIntent {
            id: StrKey(2),
            seq: 1,
            ..market_entry()
        };
        let qty_b = |sw: crate::switch::Switches| {
            let mut b = with(sw, &m);
            run(&mut b, &bar(0, 100 * E, 101 * E, 99 * E, 100 * E), &[a]);
            run(&mut b, &bar(1, 100 * E, 101 * E, 99 * E, 100 * E), &[b2]);
            run(&mut b, &bar(2, 110 * E, 111 * E, 109 * E, 110 * E), &[]);
            b.open_trade(1).map(|t| t.qty_1e6)
        };
        assert_eq!(qty_b(SWITCHES), Some(101 * E)); // 10 100 / 100 (was 10 100 / 110)
        let ot = crate::switch::Switches {
            pct_equity_at: crate::switch::PctEquityAt::OrderTime,
            ..SWITCHES
        };
        assert_eq!(qty_b(ot), Some(100 * E)); // 10 000 / 100
    }

    #[test]
    fn exit_qty_per_call() {
        // `pin:05-exit-qty-per-trade`: two 3-lot trades, one exit `qty = 4` at 105. PerTrade:
        // each set takes min(4, 3) → flat. PerCall: 3 + 1 → 2 left.
        let mut m = meta();
        m.decl.pyramiding = 2;
        let e = |id: u64, seq: u32| OrderIntent {
            id: StrKey(id),
            flags: f::QTY,
            qty_1e6: 3 * E,
            seq,
            ..market_entry()
        };
        let x = OrderIntent {
            kind: IntentKind::Exit,
            id: StrKey(9),
            flags: f::QTY | f::LIMIT,
            qty_1e6: 4 * E,
            limit_1e6: 105 * E,
            seq: 2,
            ..OrderIntent::default()
        };
        let left = |sw: crate::switch::Switches| {
            let mut b = with(sw, &m);
            run(
                &mut b,
                &bar(0, 100 * E, 101 * E, 99 * E, 100 * E),
                &[e(1, 0), e(2, 1), x],
            );
            run(&mut b, &bar(1, 100 * E, 101 * E, 99 * E, 100 * E), &[]);
            run(&mut b, &bar(2, 104 * E, 106 * E, 103 * E, 105 * E), &[]);
            b.view().position_size_1e6
        };
        assert_eq!(left(SWITCHES), 0);
        let pc = crate::switch::Switches {
            exit_qty: crate::switch::ExitQty::PerCall,
            ..SWITCHES
        };
        assert_eq!(left(pc), 2 * E);
    }

    #[test]
    fn trail_arm_fire_same_segment() {
        // `pin:05-trail-same-segment` (zero offset): bar 2's path 104→105→95→103 arms the 103
        // trail on its last segment. NextSegment: the close tick does not admit it, bar 3's open
        // (101) gaps through → 101. SameSegment: fires at the arming segment's end, 103.
        let x = OrderIntent {
            kind: IntentKind::Exit,
            id: StrKey(9),
            flags: f::TRAIL_PRICE | f::TRAIL_OFFSET,
            trail_price_1e6: 103 * E,
            trail_offset_1e6: 0,
            seq: 1,
            ..OrderIntent::default()
        };
        let exit_px = |sw: crate::switch::Switches| {
            let mut b = with(sw, &meta());
            run(
                &mut b,
                &bar(0, 100 * E, 101 * E, 99 * E, 100 * E),
                &[market_entry(), x],
            );
            run(&mut b, &bar(1, 100 * E, 101 * E, 99 * E, 100 * E), &[]);
            run(&mut b, &bar(2, 104 * E, 105 * E, 95 * E, 103 * E), &[]);
            run(&mut b, &bar(3, 101 * E, 102 * E, 100 * E, 101 * E), &[]);
            b.closed_trade(0).map(|t| t.exit_px_1e6)
        };
        assert_eq!(exit_px(SWITCHES), Some(101 * E));
        let same = crate::switch::Switches {
            trail_arm_fire: crate::switch::TrailArmFire::SameSegment,
            ..SWITCHES
        };
        assert_eq!(exit_px(same), Some(103 * E));
    }

    fn cof_meta() -> ProgramMeta {
        let mut m = meta();
        m.decl.flags |= pine_ir::decl_flag::CALC_ON_ORDER_FILLS;
        m.uses_intrabar = true;
        m
    }

    #[test]
    fn cof_close_tick_yes() {
        // `pin:05-cof-close-tick`: the 99 buy limit fills at bar 1's close (path
        // 100→99.5→102→99). No: no execution; Yes: one.
        let m = cof_meta();
        let execs = |sw: crate::switch::Switches| {
            let mut b = with(sw, &m);
            run(
                &mut b,
                &bar(0, 100 * E, 101 * E, 99_500_000, 100 * E),
                &[entry_limit(99 * E)],
            );
            run(&mut b, &bar(1, 100 * E, 102 * E, 99_500_000, 99 * E), &[]);
            assert_eq!(b.view().opentrades, 1);
            b.counters().reevals_on_order_fill
        };
        assert_eq!(execs(SWITCHES), 0);
        let yes = crate::switch::Switches {
            cof_close_tick: crate::switch::CofCloseTick::Yes,
            ..SWITCHES
        };
        assert_eq!(execs(yes), 1);
    }

    #[test]
    fn cof_next_tick_rest_of_segment() {
        // `pin:05-cof-next-tick`: the entry fills at bar 1's open (100); the execution there
        // closes it. NextPoint: from segment 1's start, 99. RestOfSegment: at once, 100.
        let m = cof_meta();
        let close = OrderIntent {
            kind: IntentKind::Close,
            from_entry: StrKey(1),
            ..OrderIntent::default()
        };
        let exit_px = |sw: crate::switch::Switches| {
            let mut b = with(sw, &m);
            run(
                &mut b,
                &bar(0, 100 * E, 101 * E, 99 * E, 100 * E),
                &[market_entry()],
            );
            let mut step = b.open_bar(&bar(1, 100 * E, 104 * E, 99 * E, 103 * E));
            let mut sent = false;
            while let TvStep::Evaluate { intrabar, .. } = step {
                let s: &[OrderIntent] = if intrabar && !sent { &[close] } else { &[] };
                sent |= intrabar;
                step = b.submit(s).unwrap_or_else(|e| panic!("{e:?}"));
            }
            b.closed_trade(0).map(|t| t.exit_px_1e6)
        };
        assert_eq!(exit_px(SWITCHES), Some(99 * E));
        let rest = crate::switch::Switches {
            cof_next_tick: crate::switch::CofNextTick::RestOfSegment,
            ..SWITCHES
        };
        assert_eq!(exit_px(rest), Some(100 * E));
    }
}
