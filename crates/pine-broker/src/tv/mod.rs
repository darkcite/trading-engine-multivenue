// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! TV-equivalent mode (impl-05 §3.4, §4.8): `TvStep`, `TvBroker` and the resumable per-bar walk.
//! INCREMENT: margin (§4.9), `strategy.risk.*` and the day roll (§4.10–§4.11) land in T13; the
//! `with_switches` constructor and the alternative walk readings (`cof_next_tick`,
//! `close_immediately = NextTick`, `trail_arm_fire = SameSegment`) with their tests in T12/T17.

pub(crate) mod cost;
pub(crate) mod margin;
pub(crate) mod path;

use crate::book::{lot_flag, Book, FillSrc};
use crate::cap;
use crate::decl::{check_tv, decl_hash64};
use crate::exit::{set_state, ExitTable, SyncCtx};
use crate::fault::{BrokerFault, InternalDefect};
use crate::money::{floor_step, mul_div, sat_i64};
use crate::orders::{otype, pflag, state, CommitCtx, OrderTable};
use crate::risk::Risk;
use crate::size::{declared_qty, n_rule_entry, n_rule_scale, size_entry_fill, unit_qty};
use crate::switch::{CloseImmediately, ForcedFillSlippage, PriceRounding, Switches, SWITCHES};
use cost::{commission_1e12, slip};
use margin::{funds_check, liquidation_price, margin_call_qty};
use path::{before, path, reach, segment, sort_cands, Cand, Level};
use pine_ir::{
    decl_flag, Bar, BrokerCounters, BrokerView, ChartInfo, DiagCode, Dialect, IntentKind,
    OrderIntent, ProgramMeta, RunRecord, SizingMode, StrKey, StrategyDecl, TradeRec, TvCfg,
};

/// What the host does next. 128 B (a 64-B `Bar` + the tag, 64-aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TvStep {
    /// Evaluate the script against `bar`, then call `submit(vm.intents())`. `intrabar == false`:
    /// the bar-close evaluation → `VmInstance::step_bar`. `intrabar == true`: a
    /// `calc_on_order_fills` execution at a fill tick → `VmInstance::step_tick` (script state rolls
    /// back; the intents it emits commit, W21). `bar` is always the **full** bar: on historical
    /// bars TradingView's extra executions see the bar's final OHLCV (§4.3.2).
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
    AwaitingSubmit { intrabar: bool, tick_px: i64 },
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
}

impl TvBroker {
    /// Validates `meta.decl`, `meta.chart` and `cfg` (§3.7) and allocates the closed-trade ring —
    /// the one allocation. Refuses with the §3.7 code; never panics.
    pub fn new(meta: &ProgramMeta, cfg: TvCfg) -> Result<Self, DiagCode> {
        check_tv(meta, &cfg)?;
        let d = meta.decl;
        Ok(Self {
            decl: d,
            chart: meta.chart,
            dialect: meta.dialect,
            cfg,
            sw: SWITCHES,
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
        self.evaluated = false;
        self.close_work = false;
        self.state = WalkState::Walking;
        self.walk()
    }

    /// Validates and COMMITS one evaluation's intents (W21, §4.3), resumes the walk and returns
    /// the next step. `intents` may be empty. `Err` → the host aborts the run (D13, backtest).
    pub fn submit(&mut self, intents: &[OrderIntent]) -> Result<TvStep, BrokerFault> {
        let (intrabar, tick_px) = match self.state {
            WalkState::AwaitingSubmit { intrabar, tick_px } => (intrabar, tick_px),
            _ => return Err(BrokerFault::Internal(InternalDefect::PathCursor)),
        };
        if let Some(f) = self.latched {
            return Err(f);
        }
        let ctx = CommitCtx {
            bar: self.bar.index,
            round_tick_1e6: if self.sw.price_rounding == PriceRounding::NearestTick {
                self.chart.mintick_1e6
            } else {
                0
            },
            allow: self.decl.risk.allow_entry_in,
            position_1e6: self.book.view().position_size_1e6,
            halted: self.risk.halted(),
        };
        let open = usize::try_from(self.book.view().opentrades).unwrap_or(usize::MAX);
        self.orders
            .commit_slice(&mut self.exits, intents, &ctx, open, &mut self.counters)?;
        let eval = self.orders.eval();
        let same_tick = self.sw.close_immediately == CloseImmediately::SameTick;
        if intrabar && same_tick {
            // `immediately` closes born now fill at this execution's tick, before the walk resumes.
            let mut i = 0;
            while i < self.orders.len() {
                let o = self.orders.get(i);
                if o.is_live() && o.born_eval == eval && o.flags & pflag::IMMEDIATELY != 0 {
                    if let Err(f) = self.execute_order(i, tick_px, true) {
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
        }
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
        let (a, b) = segment(&self.path, self.seg);
        let eval = self.orders.eval();
        let all_on_close = self.decl.flags & decl_flag::ORDERS_ON_CLOSE != 0;
        self.n_cand = 0;
        let mut i = 0;
        while i < self.orders.len() {
            // COPY: 128-B record snapshot: `push` borrows `self` mutably (collection only).
            // COPY: 128-B record snapshot: sizing and `apply_fill` borrow `self` mutably.
            let o = *self.orders.get(i);
            let admitted = match admit_after {
                Some(e) => o.born_eval > e,
                None => true,
            } && (!on_close_only
                || (o.born_eval == eval
                    && (all_on_close
                        || (self.sw.close_immediately == CloseImmediately::SameTick
                            && o.flags & pflag::IMMEDIATELY != 0))));
            if o.is_live() && admitted {
                let lv = self.order_level(i);
                self.push(lv, a, b, o.seq, u16::try_from(i).unwrap_or(IDX_MASK));
            }
            i += 1;
        }
        let mut k = 0;
        while k < self.exits.sets().len() {
            // COPY: 128-B set snapshot: `push` borrows `self` mutably (collection only).
            // COPY: 128-B set snapshot: `fill_set` borrows the table mutably.
            let s = *self.exits.set(k);
            let admitted = match admit_after {
                Some(e) => s.born_eval > e,
                None => true,
            } && (!on_close_only || (all_on_close && s.born_eval == eval));
            if s.state == set_state::LIVE && s.r_1e6 > 0 && admitted {
                let seq = self.exits.rule(usize::from(s.rule)).seq;
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
                if let Some(step) = self.run_ticks(true) {
                    return step;
                }
                if self.seg <= 2 {
                    let (a, b) = segment(&self.path, self.seg);
                    self.exits.after_segment(a, b);
                    if let Err(f) = self.tick(self.path[self.seg + 1]) {
                        return self.latch(f);
                    }
                }
                self.seg += 1;
                self.collected = false;
            } else if !self.evaluated {
                self.evaluated = true;
                self.state = WalkState::AwaitingSubmit {
                    intrabar: false,
                    tick_px: self.path[3],
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
                if let Some(step) = self.run_ticks(false) {
                    return step;
                }
            } else {
                self.state = WalkState::Idle;
                return TvStep::Done;
            }
        }
    }

    /// Process the collected candidates tick by tick (§4.8.7 lines 10–20); `Some` when the walk
    /// must return (an intrabar `Evaluate`, or a latched fault).
    fn run_ticks(&mut self, may_evaluate: bool) -> Option<TvStep> {
        let c = self.path[3];
        while self.cur < self.n_cand {
            let px = self.cand[self.cur].key;
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
            if any
                && may_evaluate
                && self.decl.flags & decl_flag::CALC_ON_ORDER_FILLS != 0
                && !at_close
            {
                self.counters.reevals_on_order_fill += 1;
                self.state = WalkState::AwaitingSubmit {
                    intrabar: true,
                    tick_px: px,
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
        let idx = usize::from(k.what & IDX_MASK);
        if k.what & WHAT_SET != 0 {
            return self.fill_leg(idx, (k.what >> LEG_SHIFT) & 3, a, b);
        }
        if idx >= self.orders.len() {
            return Ok(false);
        }
        // COPY: 128-B record snapshot: the stop-limit stage rewrites `self.cand` and the record.
        let o = *self.orders.get(idx);
        if !o.is_live() || o.born_eval > self.seg_eval {
            return Ok(false);
        }
        let a_eff = if k.flags & CAND_REKEYED != 0 {
            o.stop_1e6
        } else {
            a
        };
        let lv = self.order_level(idx);
        let v = self.verify_for(lv);
        let Some((_, px)) = reach(lv, a_eff, b, v, self.sw.fill_limits_price) else {
            return Ok(false);
        };
        // A re-keyed limit stage is placed in path order after the stop's tick.
        if o.otype == otype::STOP_LIMIT && o.state == state::WORKING {
            // Stage 1: the stop triggers, no fill; the limit stage takes over from `a := S`.
            self.orders.get_mut(idx).state = state::STOP_TRIGGERED;
            let lim = self.order_level(idx);
            let v = self.verify_for(lim);
            if let Some((r, _)) = reach(lim, o.stop_1e6, b, v, self.sw.fill_limits_price) {
                self.cur -= 1;
                self.cand[self.cur] = Cand {
                    key: r,
                    seq: o.seq,
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
        self.execute_order(idx, px, slipped)
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
        // COPY: 256-B view snapshot: `apply_fill` below borrows the book mutably.
        let v = *self.book.view();
        let pos = v.position_size_1e6;
        let m = if pos > 0 {
            self.decl.margin_long_pct_1e6
        } else {
            self.decl.margin_short_pct_1e6
        };
        let pv = self.chart.pointvalue_1e6;
        let (init, net) = (v.initial_capital_1e12, v.netprofit_1e12);
        let size = margin_call_qty(
            pos,
            v.position_avg_price_1e6,
            px,
            init,
            net,
            m,
            pv,
            self.chart.qty_step_1e6,
        );
        if size > 0 {
            let buy = pos < 0;
            let fpx = if self.sw.forced_fill_slippage == ForcedFillSlippage::Slipped {
                slip(px, buy, self.decl.slippage_ticks, self.chart.mintick_1e6)
            } else {
                px
            };
            let fee = commission_1e12(&self.decl, fpx, size, pv, true, self.sw.cash_per_order);
            let src = FillSrc {
                id: StrKey::NONE,
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
        // COPY: 256-B view snapshot after the call (cold: once per tick).
        let v = *self.book.view();
        let pos = v.position_size_1e6;
        let m = if pos > 0 {
            self.decl.margin_long_pct_1e6
        } else {
            self.decl.margin_short_pct_1e6
        };
        let p = liquidation_price(
            pos,
            v.position_avg_price_1e6,
            v.initial_capital_1e12,
            v.netprofit_1e12,
            m,
            pv,
        );
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
            id: StrKey::NONE,
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

    /// Size, price and book order `i` at `px` (before slippage when `slipped`); §4.7, §4.8.6.
    fn execute_order(&mut self, i: usize, px0: i64, slipped: bool) -> Result<bool, BrokerFault> {
        let o = *self.orders.get(i);
        let pos = self.book.view().position_size_1e6;
        let pv = self.chart.pointvalue_1e6;
        let step = self.chart.qty_step_1e6;
        let kind = o.kind;
        let buy = if kind == IntentKind::Close as u8 || kind == IntentKind::CloseAll as u8 {
            pos < 0
        } else {
            o.side == 0
        };
        let px = if slipped {
            slip(px0, buy, self.decl.slippage_ticks, self.chart.mintick_1e6)
        } else {
            px0
        };
        let q_signed: i64 = if kind == IntentKind::Entry as u8 {
            let q = self.resolve_qty(&o, px)?;
            let Some(sz) = size_entry_fill(
                &self.book,
                buy,
                q,
                o.born_eval,
                self.decl.pyramiding,
                self.max_position(px),
                0,
                &mut self.counters,
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
            let q = floor_step(self.resolve_qty(&o, px)?, step);
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
            let total = self.open_qty_of(o.id);
            if total == 0 {
                self.orders.kill(i); // nothing open with that id: it lapses
                return Ok(false);
            }
            let q = if o.qty_1e6 > 0 {
                let q = match self.cfg.sizing {
                    SizingMode::AsDeclared => o.qty_1e6,
                    SizingMode::FixedNotional { notional_1e6 } => {
                        let eq = self.book.view().equity_1e12;
                        let d = declared_qty(&self.decl, px, pv, eq);
                        n_rule_scale(unit_qty(notional_1e6, px, pv), o.qty_1e6, d)
                    }
                };
                q.min(total)
            } else if o.qty_pct_1e6 > 0 {
                let pct = mul_div(
                    i128::from(total),
                    i128::from(o.qty_pct_1e6),
                    100 * 1_000_000,
                );
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
            o.filled_1e6 == 0,
            self.sw.cash_per_order,
        );
        let is_entry = kind == IntentKind::Entry as u8 || kind == IntentKind::Order as u8;
        let mut lot_flags = if o.otype == otype::MARKET {
            0
        } else {
            lot_flag::PRICE_BASED
        };
        if kind == IntentKind::Entry as u8 {
            lot_flags |= lot_flag::PYRAMID;
        }
        let src = FillSrc {
            id: o.id,
            sym: o.id_sym,
            named_entry: if kind == IntentKind::Close as u8 {
                o.id
            } else {
                StrKey::NONE
            },
            born_bar: o.born_bar,
            born_eval: o.born_eval,
            is_entry,
            lot_flags,
        };
        self.book
            .apply_fill(q_signed, px, fee, &src, self.bar.index, self.bar.open_ms, 0)?;
        self.orders.get_mut(i).filled_1e6 = o.filled_1e6.saturating_add(q_abs);
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
        let s = *self.exits.set(k);
        if s.state != set_state::LIVE || s.r_1e6 <= 0 || s.born_eval > self.seg_eval {
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
                s.side == 0,
                self.decl.slippage_ticks,
                self.chart.mintick_1e6,
            ),
            _ => px0,
        };
        let pv = self.chart.pointvalue_1e6;
        let fee = commission_1e12(&self.decl, px, s.r_1e6, pv, true, self.sw.cash_per_order);
        self.exits
            .fill_set(k, px, fee, &mut self.book, self.bar.index, self.bar.open_ms)?;
        self.risk.note_fill();
        self.exits.rederive_now(self.book.view().position_size_1e6);
        Ok(true)
    }

    /// The order's quantity at reference price `px` (§4.7.1–§4.7.2), net of OCA reductions.
    fn resolve_qty(&self, o: &crate::orders::PendingOrder, px: i64) -> Result<i64, BrokerFault> {
        let pv = self.chart.pointvalue_1e6;
        let eq = self.book.view().equity_1e12;
        let explicit = o.qty_1e6 > 0;
        let q = match self.cfg.sizing {
            SizingMode::AsDeclared => {
                if explicit {
                    o.qty_1e6
                } else {
                    declared_qty(&self.decl, px, pv, eq)
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
        let mut b = Self::new(meta, cfg)?;
        b.sw = sw;
        b.exits = ExitTable::new(sw.exit_reservation);
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
}
