// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-pm15 — H90 on Polymarket's 15-minute up/down markets (slot 9)
//!
//! The PM15 lane's member (build plan `claude/pm15-build-plan-2026-10-17.md`,
//! the research in the vault's docs 35, 36 v2 and the P0 pre-registration
//! 39). It trades ONE law, the H90 late-window entry that slot 3 trades on
//! HIP-4, on the rolling families the PM15 ingress carries
//! (`ingress_polymarket::pm15_loop`, `core_types::pm_roll`):
//!
//! 1. **The market**: a window's roll (`InstrumentRoll` created) binds one of
//!    the family's two slot pairs; its settled roll closes it.
//! 2. **The strike**: the previous window's settlement — estimated as the
//!    mean of the Chainlink reports over `[open − 60, open − 1]` the moment
//!    the report of `open − 1` arrives. The venue publishes `priceToBeat`
//!    only once the window resolves, so a window that opened before the
//!    boot's reports has no strike and is never traded (`hold_strike`).
//! 3. **The price**: `core_twaplock::report::report_fair` — the reports on
//!    their own second, the samples `[T − 60, T − 1]`, observed through the
//!    newest report, the rest a walk from the fast venue's nowcast.
//! 4. **The decision**, for `τ = T − now` in `[tau_min, tau_max)`: the
//!    favoured side's belief `p ≥ p_min` on `persist` consecutive report
//!    seconds, and its ask `≤ p − e` ⇒ one IoC BUY at `p − e` (on the
//!    leg's tick), sized to the order cap, inside the market cap. One IoC
//!    in flight per market; re-entry while room and belief remain. PMJ J8
//!    (ruling R-PMJ-S24, a documented deviation from doc 39 §4): the ask
//!    must also be at least `price_floor` (0.90) — a favourite the book
//!    prices under it is no H90 entry, the near-tie windows where the
//!    strike estimate and the venue disagree (slot 11's law, and the month
//!    research's).
//! 5. **The book**: positions from fills only; at the settled roll each
//!    leg pays 1 or 0, minus its cost and the venue's taker fee
//!    `0.07·p·(1 − p)` per share — the session's realised P&L. Once it is
//!    at or under `−session_loss`, entries stop (the paper frame equals
//!    the live one, ruling R-PM-8).
//!
//! ## What it does NOT do
//!
//! It never sells, never rests a quote, never trades a window it did not
//! see the strike of (a mid-window boot waits for the next window), and
//! never reads another venue's roll. Live execution is `exec-polymarket`'s.
//!
//! ## Doctrine
//!
//! No allocation after [`Pm15Strategy::new`] (the grids and the Φ table are
//! boxed there). `#[repr(C)]` PODs, no `dyn`, no float, while-index loops,
//! `debug_assert!` on invariants.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core_fill::ORDER_KIND_IOC;
use core_time::{NsTs, WallAnchor};
use core_twaplock::late::{LateHold, LateLut, SEC_NS};
use core_twaplock::report::{report_fair, ReportGrid};
use core_types::pm_roll::{
    pm_instance_parts, pm_rolling_of, pm_window_open_s, PM_OFF_MARK, PM_PAIRS, PM_ROLLING_MAX,
    PM_SETTLED_DOWN, PM_SETTLED_UP,
};
use core_types::{
    ChannelEvent, ChannelId, Order, Price, Qty, Side, SymbolId, Tick, VenueId, SYMBOL_ID_NONE,
};
use strategy_core::{
    Ctx, Pm15Counters, RegimeGate, Strategy, StrategyCounters, StrategyError, SubmitErr,
};

/// Families one member carries (`core_types::pm_roll::PM_ROLLING_MAX`).
pub const PM15_MAX_FAMILIES: usize = PM_ROLLING_MAX;

/// The venue's taker fee rate ×1e6 (0.07).
pub const FEE_RATE_1E6: i64 = 70_000;

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

/// The member's configuration (`core_config::pm15`, resolved by the cli's
/// `pm15_boot`).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Pm15Params {
    /// Families configured, in `[polymarket] rolling` order.
    pub n_families: usize,
    /// Each family's fast-venue instrument (the nowcast's mid).
    pub perp_sym: [SymbolId; PM15_MAX_FAMILIES],
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
    /// Explicit padding — always zero.
    pub _pad: [u8; 6],
    /// The pricer's σ scale ×1e3.
    pub sigma_k_1e3: i64,
    /// Most one order spends ×1e6 USD.
    pub usd_order_1e6: i64,
    /// Most one market holds ×1e6 USD (cost of its fills and in-flight).
    pub usd_market_1e6: i64,
    /// The session loss bound ×1e6 USD (positive): entries stop once the
    /// realised P&L is at or under its negative.
    pub session_loss_1e6: i64,
    /// An IoC's time-to-live at the dispatcher, ns.
    pub order_ttl_ns: u64,
    /// PMJ J8 (R-PMJ-S24): the least ask an entry takes ×1e6 (0.90; 0 = no
    /// floor, P0's law as frozen).
    pub price_floor_1e6: i64,
}

impl Default for Pm15Params {
    /// The ruled frame (R-PM-8) and the H90 constants, no family.
    fn default() -> Self {
        Self {
            n_families: 0,
            perp_sym: [SYMBOL_ID_NONE; PM15_MAX_FAMILIES],
            tau_min_ns: 2_000_000_000,
            tau_max_ns: 20_000_000_000,
            p_min_1e6: 900_000,
            e_1e6: 20_000,
            persist: 2,
            max_orders_market: 4,
            _pad: [0; 6],
            sigma_k_1e3: 1_550,
            usd_order_1e6: 250_000_000,
            usd_market_1e6: 250_000_000,
            session_loss_1e6: 500_000_000,
            order_ttl_ns: 1_000_000_000,
            price_floor_1e6: 900_000,
        }
    }
}

/// Why [`Pm15Strategy::configure`] refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Pm15ConfigErr {
    /// No family, or more than [`PM15_MAX_FAMILIES`].
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
    /// The strike ×1e6 (0 = not known yet).
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
}

/// The member (module docs).
pub struct Pm15Strategy {
    params: Pm15Params,
    configured: bool,
    anchor: WallAnchor,
    lut: Box<LateLut>,
    grids: Box<[ReportGrid; PM15_MAX_FAMILIES]>,
    markets: Box<[[Market; PM_PAIRS]; PM15_MAX_FAMILIES]>,
    counters: Pm15Counters,
    oid_seq: u64,
    orders_emitted: u64,
    orders_dropped: u64,
}

impl Default for Pm15Strategy {
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

impl Pm15Strategy {
    /// An unconfigured member — boot only (the grids and the table are
    /// boxed here). Until [`Self::configure`] it ignores everything.
    #[must_use]
    pub fn new() -> Self {
        Self {
            params: Pm15Params::default(),
            configured: false,
            anchor: WallAnchor::new(0, 0),
            lut: Box::new(LateLut::flat()),
            grids: boxed_n(&ReportGrid::new()),
            markets: boxed_n(&[Market::default(); PM_PAIRS]),
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
    /// [`Pm15ConfigErr`] — the member stays unconfigured.
    pub fn configure(
        &mut self,
        params: Pm15Params,
        lut: Box<LateLut>,
        anchor: WallAnchor,
    ) -> Result<(), Pm15ConfigErr> {
        if params.n_families == 0 || params.n_families > PM15_MAX_FAMILIES {
            return Err(Pm15ConfigErr::Families);
        }
        let mut f = 0usize;
        while f < params.n_families {
            if params.perp_sym[f] == SYMBOL_ID_NONE {
                return Err(Pm15ConfigErr::Perp);
            }
            f += 1;
        }
        if params.tau_min_ns == 0
            || params.tau_min_ns >= params.tau_max_ns
            || params.tau_max_ns > 60 * SEC_NS
        {
            return Err(Pm15ConfigErr::Window);
        }
        let ok = (500_000..ONE_1E6).contains(&params.p_min_1e6)
            && (0..params.p_min_1e6).contains(&params.e_1e6)
            && (1..=8).contains(&params.persist)
            && params.max_orders_market >= 1
            && params.sigma_k_1e3 > 0
            && params.usd_order_1e6 > 0
            && params.usd_market_1e6 >= params.usd_order_1e6
            && params.session_loss_1e6 > 0
            && params.order_ttl_ns > 0
            && (params.price_floor_1e6 == 0 || (500_000..ONE_1E6).contains(&params.price_floor_1e6));
        if !ok {
            return Err(Pm15ConfigErr::Bound);
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
        &self.markets[f % PM15_MAX_FAMILIES][pair % PM_PAIRS]
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
            while p < PM_PAIRS {
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
        let Some((f, off)) = pm_rolling_of(ev.sym) else {
            self.counters.rolls_refused += 1;
            return;
        };
        let (inst, _twap, roll_family, _) = core_types::unpack_roll_seq(ev.venue_seq);
        let Some(settled) = core_types::roll_kind_strict(ev.venue_seq) else {
            self.counters.rolls_refused += 1;
            return;
        };
        let pair = (off / 2) as usize;
        let (window, inst_f) = pm_instance_parts(inst);
        if f >= self.params.n_families
            || off % 2 != 0
            || off >= PM_OFF_MARK
            || inst_f != f
            || roll_family != f * PM_PAIRS + pair
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
        let open_ns = pm_window_open_s(window).saturating_mul(SEC_NS);
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
        if g.last_sec() + 1 < open_s {
            return;
        }
        match g.mean(open_s - 60, open_s - 1) {
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
        let Some((f, off)) = pm_rolling_of(ev.sym) else {
            return;
        };
        if f >= self.params.n_families || off >= PM_OFF_MARK {
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
        let Some((f, off)) = pm_rolling_of(ev.sym) else {
            return;
        };
        if f >= self.params.n_families || off != PM_OFF_MARK || ev.v0 <= 0 {
            return;
        }
        self.grids[f].on_report(ev.venue_seq, ev.v0);
        let now = ctx.now_ns();
        let mut pair = 0usize;
        while pair < PM_PAIRS {
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
        if tau < p.tau_min_ns || tau >= p.tau_max_ns {
            return;
        }
        if m.strike_src == 0 {
            self.counters.hold_strike += 1;
            return;
        }
        let fair = match report_fair(
            &self.grids[f],
            &self.lut,
            m.strike_1e6,
            m.end_ns,
            wall,
            p.sigma_k_1e3,
        ) {
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
        // PMJ J8 (R-PMJ-S24): the floor — a favourite the book prices under
        // it is no H90 entry.
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
        let room = p.usd_market_1e6 - m.committed_1e6;
        let budget = if room < p.usd_order_1e6 {
            room
        } else {
            p.usd_order_1e6
        };
        let shares = if budget > 0 {
            (i128::from(budget) * i128::from(ONE_1E6) / i128::from(limit)) as i64
        } else {
            0
        };
        let qty = shares - shares.rem_euclid(SHARE_STEP_1E6);
        if qty <= 0 || qty < book.min_size_1e6 {
            self.counters.skip_cap += 1;
            return;
        }
        let notional = (i128::from(limit) * i128::from(qty) / i128::from(ONE_1E6)) as i64;
        let ttl = p.order_ttl_ns;
        let sym = core_types::pm_roll::pm_rolling_sym(f, pair, leg as u32);
        self.oid_seq = self.oid_seq.wrapping_add(1);
        let oid = ((self.oid_seq & 0x3FFF) << OID_SEQ_SHIFT)
            | ((leg as u64) << OID_LEG_SHIFT)
            | ((f as u64 & 0xFFFF) << OID_FAMILY_SHIFT)
            | (u64::from(m.instance) & core_types::OID_INSTANCE_MASK);
        let order = Order::new(
            now,
            VenueId::Polymarket,
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
            while pair < PM_PAIRS {
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
        let (_, inst_f) = pm_instance_parts(inst);
        if f < self.params.n_families && inst_f == f && inst != 0 {
            let mut pair = 0usize;
            while pair < PM_PAIRS {
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
            while pair < PM_PAIRS {
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

impl StrategyCounters for Pm15Strategy {
    fn orders_emitted(&self) -> u64 {
        self.orders_emitted
    }
    fn orders_dropped(&self) -> u64 {
        self.orders_dropped
    }
    fn strategy_kind(&self) -> &'static str {
        "pm15"
    }
    fn pm15_counters(&self) -> Pm15Counters {
        self.counters()
    }
}

impl Strategy for Pm15Strategy {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        Ok(())
    }

    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        if !self.configured {
            return;
        }
        if let Some((f, off)) = pm_rolling_of(tick.sym) {
            if f >= self.params.n_families || off >= PM_OFF_MARK {
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
                self.grids[f].on_perp(self.anchor.wall_of(tick.ts_ns), b + a);
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
        // PMJ J1b (ruling R-PMJ-3, the family split): a 5-minute family's
        // records are slot 11's. They are not this member's to refuse, so
        // nothing of slot 9's — orders, markets, counters — sees them.
        if core_types::pm_roll::pm5_rolling_of(event.sym).is_some() {
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
