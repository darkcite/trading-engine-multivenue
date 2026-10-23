// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-pmj — H90 on Polymarket's 5-minute up/down markets, through
//! Jupiter (slot 11, PAPER)
//!
//! The PMJ lane's member (project doc `claude/pmj-plan-2026-10-18.md`, the
//! session prompt in the vault, risk-policy "PMJ — slot 11"). It trades the
//! H90 late-window law slot 9 trades (`strategy-pm15`), on the 5-MINUTE
//! rolling families only (ruling R-PMJ-3, the family split), and addresses
//! its orders to Jupiter Predict (`VenueId::Jupiter`, R-PMJ-S2): the order
//! keeps the Polymarket leg's sym — the book it is judged against — and
//! carries venue 10, the venue that executes it.
//!
//! The law is `strategy-pm15`'s, provisional until the 5-minute research
//! (R5) freezes its own: the strike from the Chainlink reports over
//! `[open − 60, open − 1]`, the price from `core_twaplock::report`, the
//! decision for `τ` in `[max(tau_min, min_tau), tau_max)`, belief `≥ p_min`
//! on `persist` report seconds, ask `≤ p − e`. PMJ adds:
//!
//! - **`price_floor`** (default 0.90): the favourite's ask must be at least
//!   it — an H90 entry buys a favourite the book prices as one;
//! - **`min_tau`** (default 3 s): Jupiter's keeper needs time to land and
//!   fill the order before the window closes;
//! - **the deposit** (`deposit_usd`, at least the venue's $5): each order
//!   deposits it (inside the market cap; never under $5);
//! - **the paper fee**: `2 × 0.07·p·(1 − p)` per share — Polymarket's taker
//!   fee, which the keeper pays and Jupiter passes on, and Jupiter's own
//!   (the plan's keeper-aware paper law; MEASURED from the venue before any
//!   arming, never read from a doc). The paper FILL is the dispatcher's
//!   held-quote law at the order's submit + `paper_delay_ms`
//!   (`core_fill::held`, the keeper's latency).
//!
//! ## The strike law (PMJ J12, R-PMJ-S61)
//!
//! `strike_law` picks one of two laws at boot. [`STRIKE_LAW_MEAN60`] is the
//! first: the strike is the mean of the reports of `[open − 60, open − 1]`
//! and the price `report_fair`. [`STRIKE_LAW_TRAP62`] is law (iii), the
//! venue's measured law: the trapezoid of `[open − 62, open − 2]` (known one
//! report earlier), priced by `report_fair_trap` with a residual of
//! `max(res_ratio · σ, res_min_ppm)`. Law (iii) reads the reports ×1e9 (v1)
//! and the perp's BBO ×1e9, so a market's `strike_1e6` holds ×1e9 under it.
//!
//! ## What it does NOT do
//!
//! It never sells, never rests a quote, never trades a window it did not
//! see the strike of, and never reads a 15-minute family's records (slot
//! 9's). It builds nothing of the live arm (`exec-jupiter`).
//!
//! ## Doctrine
//!
//! No allocation after [`PmjStrategy::new`] (the grids and the Φ table are
//! boxed there). `#[repr(C)]` PODs, no `dyn`, no float, while-index loops,
//! `debug_assert!` on invariants.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core_fill::ORDER_KIND_IOC;
use core_time::{NsTs, WallAnchor};
use core_twaplock::late::{LateHold, LateLut, SEC_NS};
use core_twaplock::report::{
    report_fair, report_fair_trap, ReportGrid, TrapResidual, FINE_PER_1E6, FINE_PER_V1,
    PM_TRAP_LAST_OFFSET_S, STRIKE_LAW_MEAN60, STRIKE_LAW_TRAP62,
};
use core_types::pm_roll::{
    pm5_instance_parts, pm5_roll_family, pm5_rolling_of, pm5_rolling_sym, pm5_window_open_s,
    PM5_OFF_MARK, PM5_PAIRS, PM5_ROLLING_MAX, PM_SETTLED_DOWN, PM_SETTLED_UP,
};
use core_types::{
    ChannelEvent, ChannelId, Order, Price, Qty, Side, SymbolId, Tick, VenueId, SYMBOL_ID_NONE,
};
use strategy_core::{
    Ctx, Pm15Counters, RegimeGate, Strategy, StrategyCounters, StrategyError, SubmitErr,
};

/// Families one member carries (`core_types::pm_roll::PM5_ROLLING_MAX`).
pub const PMJ_MAX_FAMILIES: usize = PM5_ROLLING_MAX;

/// The paper fee rate ×1e6: Polymarket's taker 0.07, twice (module docs).
pub const FEE_RATE_1E6: i64 = 140_000;

/// The venue's least deposit, ×1e6 USD ($5).
pub const MIN_DEPOSIT_1E6: i64 = 5_000_000;

/// One whole payout ×1e6.
pub const ONE_1E6: i64 = 1_000_000;

/// Share granularity ×1e6 (0.01 share).
pub const SHARE_STEP_1E6: i64 = 10_000;

/// A leg's touch older than this is no book to take from, ns. The ingress
/// emits a touch when it changes and re-asserts every touch at each `PONG`
/// of the market socket (`PM15_MARKET_PING_NS`, 5 s), so a quiet book is
/// still fresh: 5 s + 3 s for the round trip and the poll. A dead socket
/// is the stale tick of its down path, not this bound's business.
pub const TOUCH_FRESH_NS: u64 = 8_000_000_000;

/// The member's configuration (`core_config::pmj`, resolved by the cli's
/// `pmj_boot`).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PmjParams {
    /// Families configured, in `[polymarket] rolling` order.
    pub n_families: usize,
    /// Each family's fast-venue instrument (the nowcast's mid).
    pub perp_sym: [SymbolId; PMJ_MAX_FAMILIES],
    /// The window's floor, ns to the end.
    pub tau_min_ns: u64,
    /// The window's ceiling, ns to the end.
    pub tau_max_ns: u64,
    /// The least belief on the favoured side ×1e6.
    pub p_min_1e6: i64,
    /// The margin the ask must leave under it ×1e6.
    pub e_1e6: i64,
    /// Consecutive report seconds the test must hold on.
    pub persist: u8,
    /// IoCs at most per market.
    pub max_orders_market: u8,
    /// The strike law (PMJ J12): [`STRIKE_LAW_MEAN60`], the first, or
    /// [`STRIKE_LAW_TRAP62`], law (iii) — which reads the reports ×1e9 and
    /// keeps the market's strike in that scale.
    pub strike_law: u8,
    /// The same-side family cap per window (PMJ J13): at most this many
    /// families hold, or are buying, one side of one window; 0 = off.
    pub max_side_families: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 4],
    /// The pricer's σ scale ×1e3.
    pub sigma_k_1e3: i64,
    /// The favourite's ask must be at least this ×1e6 (module docs).
    pub price_floor_1e6: i64,
    /// No order closer to the window's end than this, ns (Jupiter's keeper
    /// lands and fills it): the effective floor is `max(tau_min, min_tau)`.
    pub min_tau_ns: u64,
    /// What one order deposits ×1e6 USD (at least [`MIN_DEPOSIT_1E6`]).
    pub deposit_usd_1e6: i64,
    /// Most one market holds ×1e6 USD (cost of its fills and in-flight).
    pub usd_market_1e6: i64,
    /// The session loss bound ×1e6 USD (positive): entries stop once the
    /// realised P&L is at or under its negative.
    pub session_loss_1e6: i64,
    /// An IoC's time-to-live at the dispatcher, ns.
    pub order_ttl_ns: u64,
    /// Law (iii)'s residual: ρ ×1e3 of the raw σ (F4 §11; 0 under the
    /// first law).
    pub res_ratio_1e3: i64,
    /// Law (iii)'s residual floor per family, ppm of the strike.
    pub res_min_ppm: [i64; PMJ_MAX_FAMILIES],
}

impl Default for PmjParams {
    /// The H90 constants (provisional, the 15-minute law's), the PMJ keys'
    /// defaults, a dust frame ($5 an order, $10 a market, a −$50 session),
    /// no family.
    fn default() -> Self {
        Self {
            n_families: 0,
            perp_sym: [SYMBOL_ID_NONE; PMJ_MAX_FAMILIES],
            tau_min_ns: 2_000_000_000,
            tau_max_ns: 20_000_000_000,
            p_min_1e6: 900_000,
            e_1e6: 20_000,
            persist: 2,
            max_orders_market: 4,
            strike_law: STRIKE_LAW_MEAN60,
            max_side_families: 0,
            _pad: [0; 4],
            sigma_k_1e3: 1_550,
            price_floor_1e6: 900_000,
            min_tau_ns: 3_000_000_000,
            deposit_usd_1e6: 5_000_000,
            usd_market_1e6: 10_000_000,
            session_loss_1e6: 50_000_000,
            order_ttl_ns: 5_000_000_000,
            res_ratio_1e3: 0,
            res_min_ppm: [0; PMJ_MAX_FAMILIES],
        }
    }
}

/// Law (iii)'s keys hold (PMJ J12); the first law needs none.
fn strike_law_ok(p: &PmjParams) -> bool {
    match p.strike_law {
        STRIKE_LAW_MEAN60 => true,
        STRIKE_LAW_TRAP62 => {
            if !(1..=1_000).contains(&p.res_ratio_1e3) {
                return false;
            }
            let mut f = 0usize;
            while f < p.n_families && f < PMJ_MAX_FAMILIES {
                if !(1..=100).contains(&p.res_min_ppm[f]) {
                    return false;
                }
                f += 1;
            }
            true
        }
        _ => false,
    }
}

/// Why [`PmjStrategy::configure`] refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmjConfigErr {
    /// No family, or more than [`PMJ_MAX_FAMILIES`].
    Families,
    /// A family with no fast-venue instrument.
    Perp,
    /// `tau_min < tau_max ≤ 60 s` does not hold.
    Window,
    /// A belief, margin, scale, cap or count out of range.
    Bound,
}

/// One leg's book and order law.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LegBook {
    /// Best bid ×1e6 (0 = none).
    pub bid_1e6: i64,
    /// Size at the best bid ×1e6.
    pub bid_sz_1e6: i64,
    /// Best ask ×1e6 (0 = none).
    pub ask_1e6: i64,
    /// Size at the best ask ×1e6.
    pub ask_sz_1e6: i64,
    /// When the touch was written, engine ns.
    pub ts_ns: u64,
    /// The price tick ×1e6 (the order law).
    pub tick_1e6: i64,
    /// The minimum order, shares ×1e6.
    pub min_size_1e6: i64,
}

/// The one IoC in flight on a market.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    /// Its client id (0 = none in flight).
    pub oid: u64,
    /// When it is written off, engine ns.
    pub deadline_ns: u64,
    /// Its limit ×1e6.
    pub px_1e6: i64,
    /// Its size ×1e6.
    pub qty_1e6: i64,
    /// Filled so far ×1e6.
    pub filled_1e6: i64,
    /// The leg (0 Up, 1 Down).
    pub leg: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 7],
}

/// One market (one window of one family on one slot pair).
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Market {
    /// The instance id (0 = the slot is free).
    pub instance: u32,
    /// The window index.
    pub window: u32,
    /// The window's open, Unix ns.
    pub open_ns: u64,
    /// The window's end, Unix ns.
    pub end_ns: u64,
    /// The strike in the grid's scale — ×1e6 under the first law, ×1e9
    /// under law (iii) (PMJ J12) — 0 = not known yet.
    pub strike_1e6: i64,
    /// 0 none, 1 estimated from the reports.
    pub strike_src: u8,
    /// The favoured side the persistence counts (0 none, 1 Up, 2 Down).
    pub persist_side: u8,
    /// Consecutive report seconds the test has held.
    pub persist_n: u8,
    /// IoCs sent on this market.
    pub orders: u8,
    /// The strike could not be estimated (counted once).
    pub strike_missed: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 3],
    /// The report second the persistence last counted.
    pub persist_sec: u64,
    /// Committed notional ×1e6 (fills' cost + the in-flight order's).
    pub committed_1e6: i64,
    /// The legs (`[Up, Down]`).
    pub legs: [LegBook; 2],
    /// Shares held per leg ×1e6.
    pub pos_1e6: [i64; 2],
    /// Cost per leg ×1e6 USD.
    pub cost_1e6: [i64; 2],
    /// Fees per leg ×1e6 USD.
    pub fee_1e6: [i64; 2],
    /// The IoC in flight.
    pub pending: Pending,
}

impl Market {
    #[inline(always)]
    fn is_bound(&self) -> bool {
        self.instance != 0
    }

    /// PMJ J13: whether this market holds `leg` or has an IoC in flight on
    /// it — its family counts against the side cap. An IoC that ended
    /// unfilled (its pending cleared, no shares) frees the family.
    #[inline(always)]
    fn on_side(&self, leg: usize) -> bool {
        self.pos_1e6[leg & 1] > 0 || (self.pending.oid != 0 && usize::from(self.pending.leg) == leg)
    }
}

/// The member (module docs).
pub struct PmjStrategy {
    params: PmjParams,
    configured: bool,
    anchor: WallAnchor,
    lut: Box<LateLut>,
    grids: Box<[ReportGrid; PMJ_MAX_FAMILIES]>,
    markets: Box<[[Market; PM5_PAIRS]; PMJ_MAX_FAMILIES]>,
    counters: Pm15Counters,
    oid_seq: u64,
    orders_emitted: u64,
    orders_dropped: u64,
}

impl Default for PmjStrategy {
    fn default() -> Self {
        Self::new()
    }
}

/// `client_oid` bit 48: the leg (0 Up, 1 Down). Bits 0..32 are the
/// instance id — `core_types::OID_INSTANCE_MASK`, the engine-wide
/// convention the execution side reads (LAW E-4).
const OID_LEG_SHIFT: u32 = 48;
/// `client_oid` bits 32..48: the family.
const OID_FAMILY_SHIFT: u32 = 32;
/// `client_oid` bits 49..63: a sequence.
const OID_SEQ_SHIFT: u32 = 49;

/// `[v; N]` built on the heap — boot only. `Box::new([..])` would first
/// build its argument on the stack (≈ 80 KiB for the grids in a debug
/// build), and the strategy set is constructed by value; this keeps the
/// member's inline footprint to its scalars and three pointers.
fn boxed_n<T: Clone, const N: usize>(v: &T) -> Box<[T; N]> {
    let heap: Box<[T]> = vec![v.clone(); N].into_boxed_slice();
    let Ok(arr) = Box::<[T; N]>::try_from(heap) else {
        unreachable!("vec![_; N] holds N elements")
    };
    arr
}

impl PmjStrategy {
    /// An unconfigured member — boot only (the grids and the table are
    /// boxed here). Until [`Self::configure`] it ignores everything.
    #[must_use]
    pub fn new() -> Self {
        Self {
            params: PmjParams::default(),
            configured: false,
            anchor: WallAnchor::new(0, 0),
            lut: Box::new(LateLut::flat()),
            grids: boxed_n(&ReportGrid::new()),
            markets: boxed_n(&[Market::default(); PM5_PAIRS]),
            counters: Pm15Counters::default(),
            oid_seq: 0,
            orders_emitted: 0,
            orders_dropped: 0,
        }
    }

    /// Configure the member (boot only): `params`, the late Φ table
    /// (`late_phi_lut`), and the wall anchor the windows are stated on.
    ///
    /// # Errors
    /// [`PmjConfigErr`] — the member stays unconfigured.
    pub fn configure(
        &mut self,
        params: PmjParams,
        lut: Box<LateLut>,
        anchor: WallAnchor,
    ) -> Result<(), PmjConfigErr> {
        if params.n_families == 0 || params.n_families > PMJ_MAX_FAMILIES {
            return Err(PmjConfigErr::Families);
        }
        let mut f = 0usize;
        while f < params.n_families {
            if params.perp_sym[f] == SYMBOL_ID_NONE {
                return Err(PmjConfigErr::Perp);
            }
            f += 1;
        }
        if params.tau_min_ns == 0
            || params.tau_min_ns >= params.tau_max_ns
            || params.tau_max_ns > 60 * SEC_NS
        {
            return Err(PmjConfigErr::Window);
        }
        let ok = (500_000..ONE_1E6).contains(&params.p_min_1e6)
            && (0..params.p_min_1e6).contains(&params.e_1e6)
            && (1..=8).contains(&params.persist)
            && params.max_orders_market >= 1
            && params.sigma_k_1e3 > 0
            && (500_000..ONE_1E6).contains(&params.price_floor_1e6)
            && params.min_tau_ns < params.tau_max_ns
            && params.deposit_usd_1e6 >= MIN_DEPOSIT_1E6
            && params.usd_market_1e6 >= params.deposit_usd_1e6
            && params.session_loss_1e6 > 0
            && params.order_ttl_ns > 0
            && strike_law_ok(&params)
            && usize::from(params.max_side_families) <= params.n_families.min(PMJ_MAX_FAMILIES);
        if !ok {
            return Err(PmjConfigErr::Bound);
        }
        self.params = params;
        self.lut = lut;
        self.anchor = anchor;
        self.configured = true;
        Ok(())
    }

    /// Whether the member is configured.
    #[inline]
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// The market on family `f`'s pair `pair`.
    #[inline]
    #[must_use]
    pub fn market(&self, f: usize, pair: usize) -> &Market {
        &self.markets[f % PMJ_MAX_FAMILIES][pair % PM5_PAIRS]
    }

    /// The counters, with the levels refreshed.
    #[must_use]
    pub fn counters(&self) -> Pm15Counters {
        let mut c = self.counters;
        let mut live = 0u64;
        let mut open = 0i64;
        let mut f = 0usize;
        while f < self.params.n_families {
            let mut p = 0usize;
            while p < PM5_PAIRS {
                let m = &self.markets[f][p];
                if m.is_bound() {
                    live += 1;
                    open += m.cost_1e6[0] + m.cost_1e6[1];
                }
                p += 1;
            }
            f += 1;
        }
        c.markets_live = live;
        c.open_cost_1e6 = open;
        c.halted = u64::from(self.halted());
        c
    }

    #[inline(always)]
    fn halted(&self) -> bool {
        self.counters.pnl_1e6 <= -self.params.session_loss_1e6
    }

    /// The family whose fast-venue instrument is `sym`, from `from`.
    #[inline]
    fn family_of_perp(&self, sym: SymbolId, from: usize) -> Option<usize> {
        let mut f = from;
        while f < self.params.n_families {
            if self.params.perp_sym[f] == sym {
                return Some(f);
            }
            f += 1;
        }
        None
    }

    // -----------------------------------------------------------------
    // venue events
    // -----------------------------------------------------------------

    fn on_roll(&mut self, ev: &ChannelEvent) {
        let Some((f, off)) = pm5_rolling_of(ev.sym) else {
            self.counters.rolls_refused += 1;
            return;
        };
        let (inst, _twap, roll_family, _) = core_types::unpack_roll_seq(ev.venue_seq);
        let Some(settled) = core_types::roll_kind_strict(ev.venue_seq) else {
            self.counters.rolls_refused += 1;
            return;
        };
        let pair = (off / 2) as usize;
        // A 5-minute instance carries its flag (bit 31); one without is
        // no 5-minute window's.
        let Some((window, inst_f)) = pm5_instance_parts(inst) else {
            self.counters.rolls_refused += 1;
            return;
        };
        if f >= self.params.n_families
            || off % 2 != 0
            || off >= PM5_OFF_MARK
            || inst_f != f
            || roll_family != pm5_roll_family(f, pair)
        {
            self.counters.rolls_refused += 1;
            return;
        }
        let held = self.markets[f][pair].instance;
        if settled {
            if held != inst {
                self.counters.rolls_refused += 1;
                return;
            }
            self.settle(f, pair, ev.v0);
            return;
        }
        if held == inst {
            // A repeat CREATED carries nothing new.
            return;
        }
        if held != 0 {
            // The slot still holds an older market: the ingress retires a
            // window before rebinding its slot, so this is a lost frame —
            // close what is held, unknown.
            self.settle(f, pair, 0);
        }
        let open_ns = pm5_window_open_s(window).saturating_mul(SEC_NS);
        let end_ns = ev.v1.max(0) as u64;
        if end_ns <= open_ns {
            self.counters.rolls_refused += 1;
            return;
        }
        self.markets[f][pair] = Market {
            instance: inst,
            window,
            open_ns,
            end_ns,
            ..Market::default()
        };
        self.counters.rolls += 1;
        // A strike the reports already cover.
        self.try_strike(f, pair);
    }

    /// Estimate a market's strike once the report of `open − 1` is in.
    fn try_strike(&mut self, f: usize, pair: usize) {
        let m = &self.markets[f][pair];
        if !m.is_bound() || m.strike_src != 0 || m.strike_missed != 0 {
            return;
        }
        let open_s = m.open_ns / SEC_NS;
        let g = &self.grids[f];
        let trap = self.params.strike_law == STRIKE_LAW_TRAP62;
        // The first law waits for the report of `open − 1`, law (iii) for
        // `open − 2` (its last label).
        let lag = if trap { PM_TRAP_LAST_OFFSET_S } else { 1 };
        if g.last_sec() + lag < open_s {
            return;
        }
        let k = if trap {
            g.trap(open_s)
        } else {
            g.mean(open_s - 60, open_s - 1)
        };
        match k {
            Some(k) if k > 0 => {
                let m = &mut self.markets[f][pair];
                m.strike_1e6 = k;
                m.strike_src = 1;
                self.counters.strikes_computed += 1;
            }
            _ => {
                self.markets[f][pair].strike_missed = 1;
                self.counters.strikes_missing += 1;
            }
        }
    }

    /// Close a market: each leg pays 1 or 0 (`winner` ±1; 0 = unknown, a
    /// position then pays nothing — the conservative book).
    fn settle(&mut self, f: usize, pair: usize, winner: i64) {
        let m = &self.markets[f][pair];
        let held = m.pos_1e6[0] > 0 || m.pos_1e6[1] > 0;
        let mut pnl = 0i64;
        let mut leg = 0usize;
        while leg < 2 {
            if m.pos_1e6[leg] > 0 {
                let won = (leg == 0 && winner == PM_SETTLED_UP)
                    || (leg == 1 && winner == PM_SETTLED_DOWN);
                let payout = if won { m.pos_1e6[leg] } else { 0 };
                pnl += payout - m.cost_1e6[leg] - m.fee_1e6[leg];
            }
            leg += 1;
        }
        self.counters.pnl_1e6 += pnl;
        if winner == 0 {
            self.counters.rolls_unknown += 1;
            if held {
                self.counters.settled_unknown += 1;
            }
        } else {
            self.counters.rolls_settled += 1;
            if held {
                if pnl > 0 {
                    self.counters.settled_won += 1;
                } else {
                    self.counters.settled_lost += 1;
                }
            }
        }
        self.markets[f][pair] = Market::default();
    }

    fn on_order_law(&mut self, ev: &ChannelEvent) {
        let Some((f, off)) = pm5_rolling_of(ev.sym) else {
            return;
        };
        if f >= self.params.n_families || off >= PM5_OFF_MARK {
            return;
        }
        let m = &mut self.markets[f][(off / 2) as usize];
        if u64::from(m.instance) != ev.venue_seq || ev.v0 <= 0 {
            return;
        }
        let l = &mut m.legs[(off % 2) as usize];
        l.tick_1e6 = ev.v0;
        l.min_size_1e6 = ev.v1;
    }

    fn on_mark<C: Ctx>(&mut self, ev: &ChannelEvent, ctx: &mut C) {
        let Some((f, off)) = pm5_rolling_of(ev.sym) else {
            return;
        };
        if f >= self.params.n_families || off != PM5_OFF_MARK || ev.v0 <= 0 {
            return;
        }
        // Law (iii) keeps the report's fine scale (×1e9, from v1's ×1e12 —
        // DOGE's ×1e6 steps are 12.6 ppm); the first law keeps v0 ×1e6.
        let px = if self.params.strike_law == STRIKE_LAW_TRAP62 {
            ev.v1 / FINE_PER_V1
        } else {
            ev.v0
        };
        if px <= 0 {
            return;
        }
        self.grids[f].on_report(ev.venue_seq, px);
        let now = ctx.now_ns();
        let mut pair = 0usize;
        while pair < PM5_PAIRS {
            self.try_strike(f, pair);
            self.evaluate(f, pair, now, true, ctx);
            pair += 1;
        }
    }

    // -----------------------------------------------------------------
    // the decision
    // -----------------------------------------------------------------

    /// Price market `(f, pair)` at engine time `now` and take if the law
    /// says so. `new_second` = a report just arrived (the persistence
    /// counts report seconds).
    fn evaluate<C: Ctx>(
        &mut self,
        f: usize,
        pair: usize,
        now: NsTs,
        new_second: bool,
        ctx: &mut C,
    ) {
        let wall = self.anchor.wall_of(now);
        let p = &self.params;
        let m = &self.markets[f][pair];
        if !m.is_bound() || wall < m.open_ns || wall >= m.end_ns {
            return;
        }
        let tau = m.end_ns - wall;
        let tau_floor = if p.min_tau_ns > p.tau_min_ns { p.min_tau_ns } else { p.tau_min_ns };
        if tau < tau_floor || tau >= p.tau_max_ns {
            return;
        }
        if m.strike_src == 0 {
            self.counters.hold_strike += 1;
            return;
        }
        let priced = if p.strike_law == STRIKE_LAW_TRAP62 {
            report_fair_trap(
                &self.grids[f],
                &self.lut,
                m.strike_1e6,
                m.end_ns,
                wall,
                p.sigma_k_1e3,
                TrapResidual {
                    ratio_1e3: p.res_ratio_1e3,
                    min_ppm: p.res_min_ppm[f],
                },
            )
            .map(|(fair, _)| fair)
        } else {
            report_fair(
                &self.grids[f],
                &self.lut,
                m.strike_1e6,
                m.end_ns,
                wall,
                p.sigma_k_1e3,
            )
        };
        let fair = match priced {
            Ok(fair) => fair,
            Err(h) => {
                match h {
                    LateHold::NoSigma => self.counters.hold_sigma += 1,
                    LateHold::NoNowcast => self.counters.hold_nowcast += 1,
                    LateHold::MarkStale => self.counters.hold_mark += 1,
                    LateHold::ObservedStale => self.counters.hold_observed += 1,
                    LateHold::Shape => self.counters.hold_shape += 1,
                }
                return;
            }
        };
        self.counters.reprices += 1;
        let up = fair.p_yes_1e6 >= 500_000;
        let p_side = if up {
            fair.p_yes_1e6
        } else {
            ONE_1E6 - fair.p_yes_1e6
        };
        let side_code = if up { 1u8 } else { 2u8 };
        let sec = self.grids[f].last_sec();
        let (p_min, persist) = (self.params.p_min_1e6, self.params.persist);
        // The persistence: consecutive REPORT seconds on the same side.
        let mk = &mut self.markets[f][pair];
        if p_side < p_min {
            mk.persist_n = 0;
            mk.persist_side = 0;
            self.counters.skip_belief += 1;
            return;
        }
        if new_second {
            if mk.persist_side == side_code && sec == mk.persist_sec + 1 {
                mk.persist_n = mk.persist_n.saturating_add(1);
            } else if !(mk.persist_side == side_code && sec == mk.persist_sec) {
                mk.persist_n = 1;
            }
            mk.persist_side = side_code;
            mk.persist_sec = sec;
        } else if mk.persist_side != side_code {
            // The side flipped between reports: it starts over.
            mk.persist_n = 0;
            mk.persist_side = 0;
        }
        if mk.persist_n < persist {
            self.counters.skip_persist += 1;
            return;
        }
        if self.halted() {
            self.counters.skip_halt += 1;
            return;
        }
        self.take(f, pair, if up { 0 } else { 1 }, p_side, now, ctx);
    }

    /// PMJ J13: whether `leg` of the window `markets[f][pair]` rides has room
    /// under `max_side_families`. Every family of one window rides the same
    /// pair, so the other families' markets of that pair with the same open
    /// are the window's. The room is there while the families other than
    /// `f` on that side are fewer than the cap; a family already on the side
    /// is never stopped by it (a second IoC of its own adds no family).
    /// Bounded: at most [`PMJ_MAX_FAMILIES`] markets read, no allocation.
    #[inline]
    fn side_room(&self, f: usize, pair: usize, leg: usize) -> bool {
        let own = &self.markets[f][pair];
        if own.on_side(leg) {
            return true;
        }
        let cap = usize::from(self.params.max_side_families);
        let mut n = 0usize;
        let mut g = 0usize;
        while g < self.params.n_families && g < PMJ_MAX_FAMILIES {
            if g != f {
                let o = &self.markets[g][pair];
                if o.is_bound() && o.open_ns == own.open_ns && o.on_side(leg) {
                    n += 1;
                }
            }
            g += 1;
        }
        n < cap
    }

    /// The take: one IoC BUY of `leg` at `p_side − e` on its tick, if the
    /// ask allows and the caps leave room.
    fn take<C: Ctx>(
        &mut self,
        f: usize,
        pair: usize,
        leg: usize,
        p_side: i64,
        now: NsTs,
        ctx: &mut C,
    ) {
        let p = &self.params;
        let m = &self.markets[f][pair];
        let book = &m.legs[leg];
        if book.tick_1e6 <= 0 {
            self.counters.skip_law += 1;
            return;
        }
        if book.ask_1e6 <= 0 || book.ts_ns == 0 {
            self.counters.skip_book += 1;
            return;
        }
        // The floor: a favourite the book prices under it is no H90 entry.
        if book.ask_1e6 < p.price_floor_1e6 {
            self.counters.skip_price += 1;
            return;
        }
        if now.saturating_sub(book.ts_ns) >= TOUCH_FRESH_NS {
            self.counters.skip_book_old += 1;
            return;
        }
        // The limit: `p − e`, down onto the leg's tick.
        let raw = p_side - p.e_1e6;
        let limit = raw - raw.rem_euclid(book.tick_1e6);
        if limit <= 0 || book.ask_1e6 > limit {
            self.counters.skip_price += 1;
            return;
        }
        if m.pending.oid != 0 || m.orders >= p.max_orders_market {
            self.counters.skip_cap += 1;
            return;
        }
        // PMJ J13: the same-side family cap (off at 0).
        if p.max_side_families != 0 && !self.side_room(f, pair, leg) {
            self.counters.skip_side += 1;
            return;
        }
        // The deposit, inside the market cap, never under the venue's $5:
        // whole hundredths of a share, one more when the rounding left the
        // deposit under $5.
        let room = p.usd_market_1e6 - m.committed_1e6;
        let budget = if room < p.deposit_usd_1e6 {
            room
        } else {
            p.deposit_usd_1e6
        };
        let shares = if budget > 0 {
            (i128::from(budget) * i128::from(ONE_1E6) / i128::from(limit)) as i64
        } else {
            0
        };
        let mut qty = shares - shares.rem_euclid(SHARE_STEP_1E6);
        let at = |q: i64| (i128::from(limit) * i128::from(q) / i128::from(ONE_1E6)) as i64;
        if qty > 0 && at(qty) < MIN_DEPOSIT_1E6 {
            qty += SHARE_STEP_1E6;
        }
        let notional = at(qty);
        if qty <= 0 || qty < book.min_size_1e6 || notional < MIN_DEPOSIT_1E6 || notional > room {
            self.counters.skip_cap += 1;
            return;
        }
        let ttl = p.order_ttl_ns;
        let sym = pm5_rolling_sym(f, pair, leg as u32);
        self.oid_seq = self.oid_seq.wrapping_add(1);
        let oid = ((self.oid_seq & 0x3FFF) << OID_SEQ_SHIFT)
            | ((leg as u64) << OID_LEG_SHIFT)
            | ((f as u64 & 0xFFFF) << OID_FAMILY_SHIFT)
            | (u64::from(m.instance) & core_types::OID_INSTANCE_MASK);
        let order = Order::new(
            now,
            VenueId::Jupiter,
            sym,
            Side::Bid,
            ORDER_KIND_IOC,
            Price::from_raw(limit),
            Qty::from_raw(qty),
            oid,
        )
        .with_ttl_ns(ttl);
        match ctx.submit(order) {
            Ok(()) => {
                self.orders_emitted += 1;
                self.counters.takes_submitted += 1;
                let mk = &mut self.markets[f][pair];
                mk.orders += 1;
                mk.committed_1e6 += notional;
                mk.pending = Pending {
                    oid,
                    deadline_ns: now.saturating_add(ttl.saturating_mul(2)),
                    px_1e6: limit,
                    qty_1e6: qty,
                    filled_1e6: 0,
                    leg: leg as u8,
                    _pad: [0; 7],
                };
            }
            Err(
                SubmitErr::RingFull
                | SubmitErr::Unsupported
                | SubmitErr::NoSuchOrder
                | SubmitErr::Refused,
            ) => {
                self.orders_dropped += 1;
                self.counters.submit_refused += 1;
            }
        }
    }

    /// Book one fill: against a market's pending IoC, or — a live fill the
    /// taker hold delayed past the pending's write-off (PM15-6b) — by its
    /// client id's family, instance and leg.
    fn book_fill(&mut self, fill: &core_types::Fill) {
        let qty = fill.qty.raw();
        if qty <= 0 || fill.is_settlement() {
            return;
        }
        let oid = fill.order_id;
        let px = fill.px.raw();
        let cost = (i128::from(px) * i128::from(qty) / i128::from(ONE_1E6)) as i64;
        let fee = (i128::from(FEE_RATE_1E6)
            * i128::from(px)
            * i128::from(ONE_1E6 - px)
            * i128::from(qty)
            / 1_000_000_000_000_000_000) as i64;
        let mut f = 0usize;
        while f < self.params.n_families {
            let mut pair = 0usize;
            while pair < PM5_PAIRS {
                let m = &mut self.markets[f][pair];
                if m.pending.oid != 0 && m.pending.oid == oid {
                    let leg = (m.pending.leg & 1) as usize;
                    let first = m.pending.filled_1e6 == 0;
                    m.pos_1e6[leg] += qty;
                    m.cost_1e6[leg] += cost;
                    m.fee_1e6[leg] += fee;
                    m.pending.filled_1e6 += qty;
                    // The commitment follows the fill price, not the limit.
                    let at_limit = (i128::from(m.pending.px_1e6) * i128::from(qty)
                        / i128::from(ONE_1E6)) as i64;
                    m.committed_1e6 += cost - at_limit;
                    if m.pending.filled_1e6 >= m.pending.qty_1e6 {
                        m.pending = Pending::default();
                    }
                    self.counters.fills += 1;
                    self.counters.fees_1e6 += fee;
                    self.counters.bought_1e6 += cost;
                    if first {
                        self.counters.takes_filled += 1;
                    }
                    return;
                }
                pair += 1;
            }
            f += 1;
        }
        // A late fill: no pending names it, its client id still does.
        let inst = (oid & core_types::OID_INSTANCE_MASK) as u32;
        let f = ((oid >> OID_FAMILY_SHIFT) & 0xFFFF) as usize;
        let leg = ((oid >> OID_LEG_SHIFT) & 1) as usize;
        let (_, inst_f) = pm5_instance_parts(inst).unwrap_or((0, usize::MAX));
        if f < self.params.n_families && inst_f == f && inst != 0 {
            let mut pair = 0usize;
            while pair < PM5_PAIRS {
                let m = &mut self.markets[f][pair];
                if m.instance == inst {
                    m.pos_1e6[leg] += qty;
                    m.cost_1e6[leg] += cost;
                    m.fee_1e6[leg] += fee;
                    m.committed_1e6 += cost;
                    self.counters.fills += 1;
                    self.counters.late_fills += 1;
                    self.counters.fees_1e6 += fee;
                    self.counters.bought_1e6 += cost;
                    return;
                }
                pair += 1;
            }
        }
        self.counters.unknown_fills += 1;
    }

    /// Write off pendings past their deadline: the unfilled remainder's
    /// commitment comes back.
    fn sweep(&mut self, now: NsTs) {
        let mut f = 0usize;
        while f < self.params.n_families {
            let mut pair = 0usize;
            while pair < PM5_PAIRS {
                let m = &mut self.markets[f][pair];
                if m.pending.oid != 0 && now >= m.pending.deadline_ns {
                    let left = m.pending.qty_1e6 - m.pending.filled_1e6;
                    if left > 0 {
                        m.committed_1e6 -= (i128::from(m.pending.px_1e6) * i128::from(left)
                            / i128::from(ONE_1E6))
                            as i64;
                    }
                    if m.pending.filled_1e6 == 0 {
                        self.counters.takes_unfilled += 1;
                    }
                    m.pending = Pending::default();
                }
                pair += 1;
            }
            f += 1;
        }
    }
}

impl StrategyCounters for PmjStrategy {
    fn orders_emitted(&self) -> u64 {
        self.orders_emitted
    }
    fn orders_dropped(&self) -> u64 {
        self.orders_dropped
    }
    fn strategy_kind(&self) -> &'static str {
        "pmj"
    }
    fn pmj_counters(&self) -> Pm15Counters {
        self.counters()
    }
}

impl Strategy for PmjStrategy {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        Ok(())
    }

    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        if !self.configured {
            return;
        }
        if let Some((f, off)) = pm5_rolling_of(tick.sym) {
            if f >= self.params.n_families || off >= PM5_OFF_MARK {
                return;
            }
            let pair = (off / 2) as usize;
            let l = &mut self.markets[f][pair].legs[(off % 2) as usize];
            l.bid_1e6 = tick.bid_px.raw();
            l.bid_sz_1e6 = tick.bid_qty.raw();
            l.ask_1e6 = tick.ask_px.raw();
            l.ask_sz_1e6 = tick.ask_qty.raw();
            l.ts_ns = if tick.is_stale() { 0 } else { tick.ts_ns };
            self.evaluate(f, pair, ctx.now_ns(), false, ctx);
            return;
        }
        // A fast-venue BBO: every family it is the nowcast of.
        let mut from = 0usize;
        while let Some(f) = self.family_of_perp(tick.sym, from) {
            let (b, a) = (tick.bid_px.raw(), tick.ask_px.raw());
            if b > 0 && a > 0 && !tick.is_stale() {
                // The nowcast in the grid's scale (law (iii): ×1e9).
                let x2 = if self.params.strike_law == STRIKE_LAW_TRAP62 {
                    (b + a).saturating_mul(FINE_PER_1E6)
                } else {
                    b + a
                };
                self.grids[f].on_perp(self.anchor.wall_of(tick.ts_ns), x2);
            }
            from = f + 1;
        }
    }

    fn on_signal<C: Ctx>(&mut self, _signal: &core_types::Signal, _ctx: &mut C) {}

    fn on_fill<C: Ctx>(&mut self, fill: &core_types::Fill, _ctx: &mut C) {
        if self.configured {
            self.book_fill(fill);
        }
    }

    fn on_venue_event<C: Ctx>(&mut self, event: &ChannelEvent, ctx: &mut C) {
        if !self.configured || event.venue != VenueId::Polymarket as u8 {
            return;
        }
        // Ruling R-PMJ-3, the family split: a 15-minute family's records
        // are slot 9's. They are not this member's to refuse, so nothing of
        // slot 11's — orders, markets, counters — sees them.
        if pm5_rolling_of(event.sym).is_none() {
            return;
        }
        match ChannelId::from_u8(event.channel) {
            Some(ChannelId::InstrumentRoll) => self.on_roll(event),
            Some(ChannelId::OutcomeMeta) => self.on_order_law(event),
            Some(ChannelId::Mark) => self.on_mark(event, ctx),
            _ => {}
        }
    }

    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, _ctx: &mut C) {
        if self.configured {
            self.sweep(now_ns);
        }
    }

    fn timer_period_ns(&self) -> u64 {
        if self.configured {
            1_000_000_000
        } else {
            u64::MAX
        }
    }

    fn on_regime<C: Ctx>(&mut self, _gate: RegimeGate, _ctx: &mut C) {}

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests;
