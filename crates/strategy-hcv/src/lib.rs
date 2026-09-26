// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `strategy-hcv` — the slot-7 HCV member (Hypercall S1; plan `hc9-hc11`
//! §4; rulings O-HC18..O-HC23). **DARK: paper only, in no configured mask.**
//!
//! Sells and buys near-ATM Hypercall options when their implied vol sits
//! a diffusive gap θ away from the HAR forecast σ̂(τ), hedges the delta on
//! the underlying's Hyperliquid perp, and books the venue's own
//! settlement — the median of means of the Hyperliquid oracle over the
//! last 30 minutes — at expiry.
//!
//! ## One timer tick (`timer_ms`, 1 s)
//!
//! 1. **Inputs.** Provider quotes (`on_tick`, Hypercall option symbols;
//!    a quote the ingress marked stale is no quote, and a quiet one stands
//!    while the Hypercall feed is alive — a tick is a BBO CHANGE, so an
//!    unchanged quote the venue keeps re-pushing emits nothing), each
//!    underlying's oracle
//!    (`on_venue_event`, the Hyperliquid `Mark` of its hedge perp: `v1` =
//!    `oraclePx`, settlement's own source) and its hedge perp's touch
//!    (`on_tick`); the HAR view the set pushes at each series epoch
//!    ([`HcvStrategy::set_har_view`], law L4 lifted for slot 7 — O-HC22);
//!    the calendar the cli's reader thread hands over a
//!    [`core_ring::Mailbox`] ([`HcvStrategy::install_events`]).
//! 2. **Settle** what expired: the replicated median of means over the
//!    window this member sampled, else the last oracle when no price
//!    reached the window (a thin or missing window is counted).
//! 3. **Decide**, per option whose quote or underlying moved: inside the
//!    policy (tenor, |ln K/S| band, a live uncrossed quote, a fresh
//!    oracle and a live hedge touch, no event in its life, a forecast) —
//!    `g_sell = σ_bid − σ̂(τ)`, `g_buy = σ̂(τ) − σ_ask`; at `g ≥ θ` ONE IoC
//!    at the quote (a sell at the bid, a buy at the ask), sized by the
//!    displayed size and the clip, then by the premium cap (the entry
//!    basis at stake plus the IoC in flight — a reduce lowers it and is
//!    exempt), the vega cap and the one-day 10σ stress (a reduce may leave
//!    |net vega| and the stress no worse than the cap or than now: net
//!    vega is not the position). ONE IoC in flight per underlying until its
//!    verdict: its fill, the paper law's expiry event, or
//!    [`HCV_PENDING_NS`].
//! 4. **Hedge**: never before a fill (the VRP F7 lesson). Per underlying,
//!    the net delta (the options at their mid IV, else σ̂, plus the perp)
//!    is kept inside ±`hedge_band` PER CONTRACT HELD — O-HC23's "±0.10
//!    delta" read per contract, so the band means the same on a $100 and
//!    a $100 000 underlying — by an IoC across the touch, rounded by the
//!    HL price law and the lot, never below the venue's minimum notional,
//!    its ttl the member's own release. A held option that cannot be
//!    priced holds the hedge (no trade on a partial delta). Over the final
//!    `unwind_min` minutes of an expiry its options' delta decays in
//!    one-minute slices and each slice re-hedges, so the hedge's average
//!    exit tracks the settlement's average (a slice below the minimum
//!    notional carries into the next); at T the residual closes.
//!
//! ## Money and risk
//!
//! Every premium is USD per 1-unit contract (D5 — no coin conversion
//! touches it); τ counts 365.25-day years (the HAR set's). The day's
//! marked P&L below `-day_loss` stops new risk; `kill` stops it at boot; a
//! stale provider, oracle or touch stops it per underlying (the
//! dead-man). Hedging and reduces continue under every stop. Paper: the
//! fill law is the held-quote law (`core_fill::held`, O-HC21).
//!
//! **Known gap (blocks switching it on):** the book lives in memory — the
//! engine's daily restarts drop it, so it must persist before slot 7 goes
//! into any mask (`docs/risk-policy.md`, "HYPERCALL — slot 7").
//!
//! ## Doctrine
//!
//! Zero allocation after [`HcvStrategy::configure`] (which boxes the
//! tables once). The option math is f64 (`bs`); no `dyn`, no `String`.
//! The engine thread never opens a file: the calendar arrives by mailbox.

#![forbid(unsafe_code)]

pub mod bs;

use core_fill::held::{held_index, HELD_SYMS};
use core_ring::MailboxRx;
pub use core_settle::BucketOrder;
use core_settle::{SettleWindow, GRID_POINTS, SETTLE_WINDOW_MS};
use core_time::WallAnchor;
use core_types::{
    hl_px, ChannelEvent, ChannelId, Fill, NsTs, Order, OrderEvent, Price, Qty, Side, Signal,
    SymbolId, Tick, VenueId,
};
use strategy_core::{
    Ctx, HarSeriesView, HcvCounters, Strategy, StrategyCounters, StrategyError, HAR_VIEW_TENORS,
    HAR_VIEW_TENORS_D,
};

/// Underlyings one member trades (the `hcv.toml` table).
pub const HCV_MAX_UND: usize = 10;
/// Options one member tracks: every Hypercall option ordinal.
pub const HCV_MAX_OPTIONS: usize = HELD_SYMS;
/// Events the calendar carries.
pub const HCV_MAX_EVENTS: usize = 64;
/// Expiries sampled for settlement at once.
pub const HCV_SETTLE_SLOTS: usize = 8;
/// An IoC with no verdict is released this long after it was sent: the
/// paper law judges at 2 s and tells a miss; a live IoC answers at once.
pub const HCV_PENDING_NS: u64 = 5_000_000_000;
/// A settlement window with at least this many grid points holding a
/// price is the venue's own estimate; fewer is counted as a fallback.
pub const HCV_SETTLE_FULL_POINTS: usize = GRID_POINTS * 9 / 10;

/// Sampling starts this long before the window, so a print carries in.
const SETTLE_LEAD_MS: u64 = 5_000;
const DAY_MS: u64 = 86_400_000;
const DAY_MS_F: f64 = 86_400_000.0;
/// Days a year — the HAR set annualises over 525 960 minutes (365.25 d),
/// so σ̂ and every τ here share one year.
const YEAR_D: f64 = 365.25;
const YEAR_MS: f64 = YEAR_D * DAY_MS_F;
const MINUTE_MS: u64 = 60_000;
const NAME_MAX: usize = 16;
/// The final-window slice clock before its first slice.
const NO_SLICE: u64 = u64::MAX;

/// The calendar (the cli's reader fills it from `scheduled-events.json`;
/// a `core_ring::Mailbox` slot). Bit `u` of `mask[i]` = the member's
/// underlying `u` is moved by event `i`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HcvEvents {
    /// When the feed was generated, wall ms (0 = none).
    pub generated_ms: u64,
    /// The window the feed vouches for, wall ms.
    pub from_ms: u64,
    /// …its end.
    pub until_ms: u64,
    /// Events held.
    pub n: u32,
    _pad: u32,
    /// Each event's instant, wall ms.
    pub at_ms: [u64; HCV_MAX_EVENTS],
    /// Each event's underlyings.
    pub mask: [u16; HCV_MAX_EVENTS],
}

impl Default for HcvEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl HcvEvents {
    /// No calendar: it vouches for nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            generated_ms: 0,
            from_ms: 0,
            until_ms: 0,
            n: 0,
            _pad: 0,
            at_ms: [0; HCV_MAX_EVENTS],
            mask: [0; HCV_MAX_EVENTS],
        }
    }

    /// Add one event (false when full).
    pub fn push(&mut self, at_ms: u64, mask: u16) -> bool {
        let i = self.n as usize;
        if i >= HCV_MAX_EVENTS {
            return false;
        }
        self.at_ms[i] = at_ms;
        self.mask[i] = mask;
        self.n += 1;
        true
    }

    /// `Some(true)` when an event of underlying `u` falls in `(after,
    /// until]`, `Some(false)` when none does, `None` when the calendar
    /// does not vouch for that span (the event law then refuses).
    #[must_use]
    pub fn any_in(&self, u: usize, after_ms: u64, until_ms: u64) -> Option<bool> {
        if self.generated_ms == 0 || u >= 16 || after_ms < self.from_ms || until_ms > self.until_ms {
            return None;
        }
        let bit = 1u16 << u;
        let n = (self.n as usize).min(HCV_MAX_EVENTS);
        let mut i = 0usize;
        while i < n {
            if self.mask[i] & bit != 0 && self.at_ms[i] > after_ms && self.at_ms[i] <= until_ms {
                return Some(true);
            }
            i += 1;
        }
        Some(false)
    }
}

/// One underlying, bound at boot.
#[derive(Copy, Clone, Debug)]
pub struct HcvUnd {
    /// Hypercall's name (`SP500`) — the HAR series name too.
    pub name: [u8; NAME_MAX],
    /// Live bytes of `name`.
    pub name_len: u8,
    /// The hedge perp (`hyperliquid:xyz:SP500`, `hyperliquid:BTC`).
    pub hedge_sym: SymbolId,
    /// Its `szDecimals` (the dex's own meta, HC10).
    pub hedge_sz_decimals: u8,
}

impl HcvUnd {
    /// An underlying named `name` (≤ 16 B).
    #[must_use]
    pub fn new(name: &[u8], hedge_sym: SymbolId, hedge_sz_decimals: u8) -> Self {
        let mut n = [0u8; NAME_MAX];
        let len = name.len().min(NAME_MAX);
        // COPY: the underlying's name ≤ 16 B, once per underlying at boot —
        // the member owns its table (no borrow outlives `hcv.toml`'s parse)
        // — a `&'static str` field was rejected: the member stays POD.
        n[..len].copy_from_slice(&name[..len]);
        Self {
            name: n,
            name_len: len as u8,
            hedge_sym,
            hedge_sz_decimals,
        }
    }

    /// The live name bytes.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name[..(self.name_len as usize).min(NAME_MAX)]
    }
}

/// One option, from discovery.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HcvOpt {
    /// Its engine symbol (a Hypercall option ordinal).
    pub sym: SymbolId,
    /// Index into the underlyings.
    pub und: u8,
    /// A call (else a put).
    pub call: bool,
    /// Strike, USD ×1e6.
    pub strike_1e6: i64,
    /// Expiry, wall ms.
    pub exp_ms: u64,
}

impl Default for HcvOpt {
    fn default() -> Self {
        Self {
            sym: core_types::SYMBOL_ID_NONE,
            und: 0,
            call: false,
            strike_1e6: 0,
            exp_ms: 0,
        }
    }
}

/// The member's parameters (`hcv.toml` + the boot's bindings).
#[derive(Clone, Debug)]
pub struct HcvParams {
    /// The underlyings traded (bit `u` of a calendar mask is `und[u]`).
    pub und: Vec<HcvUnd>,
    /// The options of those underlyings.
    pub options: Vec<HcvOpt>,
    /// θ, σ ×1e6.
    pub theta_vol_1e6: i64,
    /// |ln K/S| band, bps.
    pub atm_band_bps: u32,
    /// Tenor range, days.
    pub tenor_min_d: u32,
    /// …its end.
    pub tenor_max_d: u32,
    /// Premium per order, USD ×1e6.
    pub clip_usd_1e6: i64,
    /// |net vega| per underlying, USD per vol point ×1e6.
    pub vega_cap_usd_1e6: i64,
    /// Gross premium at stake, USD ×1e6.
    pub premium_cap_usd_1e6: i64,
    /// The day's marked-loss stop, USD ×1e6.
    pub day_loss_usd_1e6: i64,
    /// The one-day 10σ stress loss cap per underlying, USD ×1e6.
    pub tail_loss_usd_1e6: i64,
    /// Option size step, contracts ×1e6.
    pub opt_size_step_1e6: i64,
    /// Hedge band: net delta per contract held, ×1e6 (100 000 = ±0.10).
    pub hedge_band_1e6: i64,
    /// Smallest hedge notional, USD ×1e6.
    pub hedge_min_usd_1e6: i64,
    /// Hedge slippage across the touch, bps.
    pub hedge_slip_bps: u32,
    /// The Hypercall feed is dead — every quote stale — when no quote of
    /// ANY instrument arrived for this long, ms (the ingress emits a tick
    /// only on a change, so one instrument's quiet quote is no evidence).
    pub quote_stale_ms: u32,
    /// An oracle print older than this is stale; the Hyperliquid feed is
    /// dead — every hedge touch stale — when no HL tick arrived for this
    /// long, ms.
    pub oracle_stale_ms: u32,
    /// Final-window unwind, minutes (0 = off).
    pub unwind_min: u32,
    /// Settlement booked this long after expiry, ms.
    pub settle_delay_ms: u32,
    /// The settlement law's bucket order (the HC7 gate's pick).
    pub settle_order: BucketOrder,
    /// The event law.
    pub event_law: bool,
    /// A calendar older than this is unknown, ms.
    pub events_stale_ms: u32,
    /// No new risk.
    pub kill: bool,
    /// The timer, ms.
    pub timer_ms: u32,
    /// Monotonic ↔ wall.
    pub anchor: WallAnchor,
}

impl HcvParams {
    /// Refuse a set the member cannot trade under.
    ///
    /// # Errors
    ///
    /// What is wrong.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.und.is_empty() || self.und.len() > HCV_MAX_UND {
            return Err("hcv: 1..=10 underlyings");
        }
        if self.options.len() > HCV_MAX_OPTIONS {
            return Err("hcv: more options than the member tracks");
        }
        let mut seen = [false; HELD_SYMS];
        let mut i = 0usize;
        while i < self.options.len() {
            let o = self.options[i];
            if o.und as usize >= self.und.len() || o.strike_1e6 <= 0 || o.exp_ms == 0 {
                return Err("hcv: an option names no underlying, strike or expiry");
            }
            let Some(k) = held_index(o.sym) else {
                return Err("hcv: an option symbol is not a Hypercall option ordinal");
            };
            if seen[k] {
                return Err("hcv: an option is listed twice");
            }
            seen[k] = true;
            i += 1;
        }
        if self.theta_vol_1e6 <= 0 || self.clip_usd_1e6 <= 0 || self.opt_size_step_1e6 <= 0 {
            return Err("hcv: θ, the clip and the size step are positive");
        }
        if self.vega_cap_usd_1e6 <= 0
            || self.premium_cap_usd_1e6 <= 0
            || self.day_loss_usd_1e6 <= 0
            || self.tail_loss_usd_1e6 <= 0
        {
            return Err("hcv: every cap is positive");
        }
        if self.tenor_min_d == 0 || self.tenor_max_d < self.tenor_min_d || self.tenor_max_d > 40 {
            return Err("hcv: tenors 1..=40 days");
        }
        if self.hedge_band_1e6 < 0 || self.hedge_min_usd_1e6 < 0 || self.timer_ms == 0 {
            return Err("hcv: the hedge band and minimum are non-negative, the timer positive");
        }
        let mut u = 0usize;
        while u < self.und.len() {
            if self.und[u].hedge_sz_decimals > 6 {
                return Err("hcv: a hedge perp's szDecimals is at most 6");
            }
            u += 1;
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Default)]
struct OptState {
    cfg: HcvOpt,
    bid_1e6: i64,
    bid_qty_1e6: i64,
    ask_1e6: i64,
    ask_qty_1e6: i64,
    quote_ns: u64,
    dirty: bool,
    pending_oid: u64,
    pending_ns: u64,
    /// The IoC in flight: signed contracts ×1e6 (+ a purchase) and limit.
    pending_q_1e6: i64,
    pending_px_1e6: i64,
    pos_1e6: i64,
    /// The position's entry basis, USD ×1e6 (the premium at stake).
    avg_px_1e6: i64,
    /// The last traded price (a mark of last resort).
    last_px_1e6: i64,
}

#[derive(Copy, Clone, Default)]
struct UndState {
    hedge_sym: SymbolId,
    sz_dec: u8,
    oracle_1e6: i64,
    oracle_ns: u64,
    hedge_bid_1e6: i64,
    hedge_ask_1e6: i64,
    hedge_ns: u64,
    hedge_pos_1e6: i64,
    hedge_pending_oid: u64,
    hedge_pending_ns: u64,
    /// σ̂ ×1e6 per HAR view tenor (0 = none).
    sig_1e6: [i32; HAR_VIEW_TENORS],
    /// The final window's last slice worked ([`NO_SLICE`] outside it).
    last_slice: u64,
    /// Options of this underlying with an IoC in flight (0 or 1).
    opt_pending: u32,
}

struct SettleSlot {
    live: bool,
    und: u8,
    exp_ms: u64,
    last_sample_ms: u64,
    /// The settlement price once fixed: `> 0` the window's, `-1` none
    /// reached it, `0` not yet fixed.
    px_1e6: i64,
    window: SettleWindow,
}

/// The member (module doc).
pub struct HcvStrategy {
    configured: bool,
    p: Option<Box<HcvParams>>,
    und: [UndState; HCV_MAX_UND],
    und_names: [[u8; NAME_MAX]; HCV_MAX_UND],
    und_name_len: [u8; HCV_MAX_UND],
    n_und: usize,
    opts: Box<[OptState]>,
    n_opts: usize,
    /// Option index + 1 by `held_index` (0 = not ours).
    opt_of: Box<[u16]>,
    events: HcvEvents,
    events_rx: Option<MailboxRx<HcvEvents>>,
    settle: Box<[SettleSlot]>,
    scratch: Box<[i64; GRID_POINTS]>,
    next_oid: u64,
    cash_usd_1e6: i64,
    day: u64,
    day_start_pnl_usd_1e6: i64,
    counters: HcvCounters,
    emitted: u64,
    dropped: u64,
    /// The newest Hypercall tick of any instrument (the feed's liveness).
    hc_alive_ns: u64,
    /// The newest Hyperliquid tick of any coin (the feed's liveness).
    hl_alive_ns: u64,
}

impl Default for HcvStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl HcvStrategy {
    /// An unconfigured member: it does nothing and arms no timer until
    /// [`Self::configure`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            configured: false,
            p: None,
            und: [UndState::default(); HCV_MAX_UND],
            und_names: [[0; NAME_MAX]; HCV_MAX_UND],
            und_name_len: [0; HCV_MAX_UND],
            n_und: 0,
            opts: Box::new([]),
            n_opts: 0,
            opt_of: Box::new([]),
            events: HcvEvents::new(),
            events_rx: None,
            settle: Box::new([]),
            scratch: Box::new([0; GRID_POINTS]),
            next_oid: 1,
            cash_usd_1e6: 0,
            day: 0,
            day_start_pnl_usd_1e6: 0,
            counters: HcvCounters::default(),
            emitted: 0,
            dropped: 0,
            hc_alive_ns: 0,
            hl_alive_ns: 0,
        }
    }

    /// Boot: install the parameters and allocate the tables (once).
    ///
    /// # Errors
    ///
    /// [`HcvParams::validate`]'s.
    pub fn configure(&mut self, p: &HcvParams) -> Result<(), &'static str> {
        p.validate()?;
        self.n_und = p.und.len();
        let mut u = 0usize;
        while u < HCV_MAX_UND {
            self.und[u] = UndState {
                last_slice: NO_SLICE,
                ..UndState::default()
            };
            if u < self.n_und {
                self.und[u].hedge_sym = p.und[u].hedge_sym;
                self.und[u].sz_dec = p.und[u].hedge_sz_decimals;
                self.und_names[u] = p.und[u].name;
                self.und_name_len[u] = p.und[u].name_len.min(NAME_MAX as u8);
            }
            u += 1;
        }
        let mut opts = vec![OptState::default(); p.options.len()].into_boxed_slice();
        let mut opt_of = vec![0u16; HELD_SYMS].into_boxed_slice();
        let mut i = 0usize;
        while i < p.options.len() {
            opts[i].cfg = p.options[i];
            if let Some(k) = held_index(p.options[i].sym) {
                opt_of[k] = (i + 1) as u16;
            }
            i += 1;
        }
        self.opts = opts;
        self.opt_of = opt_of;
        self.n_opts = p.options.len();
        let mut slots = Vec::with_capacity(HCV_SETTLE_SLOTS);
        let mut k = 0usize;
        while k < HCV_SETTLE_SLOTS {
            slots.push(SettleSlot {
                live: false,
                und: 0,
                exp_ms: 0,
                last_sample_ms: 0,
                px_1e6: 0,
                window: SettleWindow::new(0),
            });
            k += 1;
        }
        self.settle = slots.into_boxed_slice();
        self.p = Some(Box::new(p.clone()));
        self.configured = true;
        Ok(())
    }

    /// Boot: the calendar's receiving end (the cli's reader thread owns
    /// the other).
    pub fn install_events(&mut self, rx: MailboxRx<HcvEvents>) {
        self.events_rx = Some(rx);
    }

    /// Is the member configured?
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.configured
    }

    /// The counters (`/state`, `/metrics`, tests).
    #[must_use]
    pub const fn counters(&self) -> HcvCounters {
        self.counters
    }

    /// The option position on `sym`, contracts ×1e6.
    #[must_use]
    pub fn position_1e6(&self, sym: SymbolId) -> i64 {
        self.opt_index(sym).map_or(0, |i| self.opts[i].pos_1e6)
    }

    /// The hedge perp position of underlying `u`, units ×1e6.
    #[must_use]
    pub fn hedge_position_1e6(&self, u: usize) -> i64 {
        if u < self.n_und {
            self.und[u].hedge_pos_1e6
        } else {
            0
        }
    }

    /// Cash: premiums received less paid, hedge flows and payoffs, USD ×1e6.
    #[must_use]
    pub const fn cash_usd_1e6(&self) -> i64 {
        self.cash_usd_1e6
    }

    /// The calendar in force.
    #[must_use]
    pub const fn events(&self) -> &HcvEvents {
        &self.events
    }

    /// The set pushes the HAR view at boot and at each series epoch (law
    /// L4 lifted for slot 7 — O-HC22): σ̂ per tenor, the fit where the fit
    /// exists, the raw fold otherwise; a cold series forecasts nothing.
    pub fn set_har_view(&mut self, rows: &[HarSeriesView]) {
        self.counters.har_updates = self.counters.har_updates.wrapping_add(1);
        let mut u = 0usize;
        while u < self.n_und {
            let name = &self.und_names[u][..self.und_name_len[u] as usize];
            let mut sig = [0i32; HAR_VIEW_TENORS];
            let mut r = 0usize;
            while r < rows.len() {
                let row = &rows[r];
                if row.name() == name {
                    if row.warm != 0 {
                        let mut t = 0usize;
                        while t < HAR_VIEW_TENORS {
                            let fitted = row.fitted & (1u16 << t) != 0 && row.fit_1e6[t] > 0;
                            sig[t] = if fitted { row.fit_1e6[t] } else { row.raw_1e6[t].max(0) };
                            t += 1;
                        }
                    }
                    break;
                }
                r += 1;
            }
            self.und[u].sig_1e6 = sig;
            u += 1;
        }
    }

    /// σ̂(τ), annualised, interpolated in total variance between the
    /// view's tenors (flat beyond its ends); `None` without a forecast.
    fn sigma_hat(&self, u: usize, tau_d: f64) -> Option<f64> {
        let s = &self.und[u].sig_1e6;
        let last = HAR_VIEW_TENORS - 1;
        if tau_d <= f64::from(HAR_VIEW_TENORS_D[0]) {
            return (s[0] > 0).then(|| f64::from(s[0]) / 1e6);
        }
        if tau_d >= f64::from(HAR_VIEW_TENORS_D[last]) {
            return (s[last] > 0).then(|| f64::from(s[last]) / 1e6);
        }
        let mut k = 0usize;
        while k < last {
            let t1 = f64::from(HAR_VIEW_TENORS_D[k + 1]);
            if tau_d <= t1 {
                if s[k] <= 0 || s[k + 1] <= 0 {
                    return None;
                }
                let t0 = f64::from(HAR_VIEW_TENORS_D[k]);
                let (v0, v1) = (f64::from(s[k]) / 1e6, f64::from(s[k + 1]) / 1e6);
                let (w0, w1) = (v0 * v0 * t0, v1 * v1 * t1);
                let w = w0 + (w1 - w0) * (tau_d - t0) / (t1 - t0);
                return (w > 0.0).then(|| (w / tau_d).sqrt());
            }
            k += 1;
        }
        None
    }

    fn opt_index(&self, sym: SymbolId) -> Option<usize> {
        let k = held_index(sym)?;
        match self.opt_of.get(k) {
            Some(&v) if v != 0 => Some(v as usize - 1),
            _ => None,
        }
    }

    fn und_of_hedge(&self, sym: SymbolId) -> Option<usize> {
        let mut u = 0usize;
        while u < self.n_und {
            if self.und[u].hedge_sym == sym {
                return Some(u);
            }
            u += 1;
        }
        None
    }

    fn wall_ms(&self, now_ns: NsTs) -> u64 {
        self.p.as_ref().map_or(0, |p| p.anchor.wall_of(now_ns) / 1_000_000)
    }

    fn fresh(ts_ns: u64, now_ns: NsTs, ms: u32) -> bool {
        ts_ns != 0 && now_ns.saturating_sub(ts_ns) <= u64::from(ms) * 1_000_000
    }

    /// An option's quote stands while the Hypercall feed is alive (the
    /// module doc: a quiet quote emits no tick).
    fn quote_live(&self, o: &OptState, now_ns: NsTs, ms: u32) -> bool {
        o.quote_ns != 0 && Self::fresh(self.hc_alive_ns, now_ns, ms)
    }

    /// A hedge touch stands while the Hyperliquid feed is alive and it
    /// quotes both sides.
    fn touch_live(&self, st: &UndState, now_ns: NsTs, ms: u32) -> bool {
        st.hedge_ns != 0
            && st.hedge_bid_1e6 > 0
            && st.hedge_ask_1e6 > 0
            && Self::fresh(self.hl_alive_ns, now_ns, ms)
    }

    /// An option's greeks at its mid IV (a live two-sided quote), else
    /// at σ̂; `None` when neither exists (or the option has expired).
    fn greeks(&self, i: usize, now_ms: u64, now_ns: NsTs) -> Option<bs::Greeks> {
        let o = &self.opts[i];
        let u = o.cfg.und as usize;
        let s = self.und[u].oracle_1e6 as f64 / 1e6;
        if s <= 0.0 || o.cfg.exp_ms <= now_ms {
            return None;
        }
        let tau_ms = (o.cfg.exp_ms - now_ms) as f64;
        let k = o.cfg.strike_1e6 as f64 / 1e6;
        let p = self.p.as_deref()?;
        let two_sided = o.bid_1e6 > 0 && o.ask_1e6 > 0 && o.bid_1e6 <= o.ask_1e6;
        let mid_iv = if two_sided && self.quote_live(o, now_ns, p.quote_stale_ms) {
            bs::implied_vol(o.cfg.call, s, k, tau_ms / YEAR_MS, (o.bid_1e6 + o.ask_1e6) as f64 / 2e6)
        } else {
            None
        };
        let sig = match mid_iv {
            Some(v) => v,
            None => self.sigma_hat(u, tau_ms / DAY_MS_F)?,
        };
        bs::price(o.cfg.call, s, k, tau_ms / YEAR_MS, sig)
    }

    /// The settlement price fixed for `(u, exp)`, once its window closed.
    fn fixed_settlement(&self, u: usize, exp_ms: u64) -> Option<i64> {
        let mut k = 0usize;
        while k < self.settle.len() {
            let s = &self.settle[k];
            if s.live && s.und as usize == u && s.exp_ms == exp_ms && s.px_1e6 > 0 {
                return Some(s.px_1e6);
            }
            k += 1;
        }
        None
    }

    /// The marked P&L, USD ×1e6: cash plus every position at its mark — a
    /// live option at its greeks' price, an expired one not yet booked at
    /// its intrinsic on the fixed settlement (else the oracle), never at
    /// a stale premium.
    fn marked_pnl(&self, now_ms: u64, now_ns: NsTs) -> i64 {
        let mut v = self.cash_usd_1e6 as f64;
        let mut i = 0usize;
        while i < self.n_opts {
            let o = &self.opts[i];
            if o.pos_1e6 != 0 {
                let mark = if o.cfg.exp_ms <= now_ms {
                    let u = o.cfg.und as usize;
                    let s_t = self.fixed_settlement(u, o.cfg.exp_ms).unwrap_or(self.und[u].oracle_1e6);
                    intrinsic_1e6(&o.cfg, s_t) as f64
                } else {
                    match self.greeks(i, now_ms, now_ns) {
                        Some(g) => g.price * 1e6,
                        None => o.last_px_1e6 as f64,
                    }
                };
                v += o.pos_1e6 as f64 / 1e6 * mark;
            }
            i += 1;
        }
        let mut u = 0usize;
        while u < self.n_und {
            v += self.und[u].hedge_pos_1e6 as f64 / 1e6 * self.und[u].oracle_1e6 as f64;
            u += 1;
        }
        v as i64
    }

    /// Underlying `u`: (net delta in underlying units ×1e6 with the final
    /// window's decay applied, perp included; net vega USD/pt ×1e6). `None`
    /// when a held, unexpired option cannot be priced (no live two-sided
    /// quote and no σ̂) — a partial delta is no delta: nothing is hedged or
    /// sized on it.
    fn exposure(&self, u: usize, now_ms: u64, now_ns: NsTs) -> Option<(i64, i64)> {
        let unwind_ms = self.p.as_ref().map_or(0, |p| u64::from(p.unwind_min) * MINUTE_MS);
        let mut d = 0f64;
        let mut v = 0f64;
        let mut i = 0usize;
        while i < self.n_opts {
            let o = &self.opts[i];
            if o.pos_1e6 != 0 && o.cfg.und as usize == u && o.cfg.exp_ms > now_ms {
                let g = self.greeks(i, now_ms, now_ns)?;
                let q = o.pos_1e6 as f64 / 1e6;
                let left = o.cfg.exp_ms - now_ms;
                // The final window: the delta decays in whole one-minute
                // slices — what is left of the settlement's average.
                let f = if unwind_ms > 0 && left < unwind_ms {
                    (left / MINUTE_MS + 1) as f64 / (unwind_ms / MINUTE_MS) as f64
                } else {
                    1.0
                };
                d += q * g.delta * f;
                v += q * g.vega_pt;
            }
            i += 1;
        }
        Some(((d * 1e6) as i64 + self.und[u].hedge_pos_1e6, (v * 1e6) as i64))
    }

    /// Unexpired contracts held on underlying `u`, gross, ×1e6 — the hedge
    /// band's base (an expired, unbooked position no longer has a delta).
    fn gross_contracts(&self, u: usize, now_ms: u64) -> i64 {
        let mut g = 0i64;
        let mut i = 0usize;
        while i < self.n_opts {
            let o = &self.opts[i];
            if o.cfg.und as usize == u && o.cfg.exp_ms > now_ms {
                g = g.saturating_add(o.pos_1e6.abs());
            }
            i += 1;
        }
        g
    }

    /// The one-day 10σ stress loss on underlying `u` with `add_1e6`
    /// contracts of option `add_i` added, USD ×1e6 (≥ 0): the worse of a
    /// 10σ one-day move down and up, every option repriced at σ̂.
    fn stress_loss(&self, u: usize, add_i: usize, add_1e6: i64, now_ms: u64) -> i64 {
        let s = self.und[u].oracle_1e6 as f64 / 1e6;
        if s <= 0.0 {
            return 0;
        }
        let sig1 = self.sigma_hat(u, 1.0).unwrap_or(1.0);
        let mv = 10.0 * sig1 * (1.0f64 / YEAR_D).sqrt();
        let mut worst = 0f64;
        let mut side = 0usize;
        while side < 2 {
            let s2 = s * if side == 0 { (-mv).exp() } else { mv.exp() };
            let mut pnl = self.und[u].hedge_pos_1e6 as f64 / 1e6 * (s2 - s);
            let mut i = 0usize;
            while i < self.n_opts {
                let o = &self.opts[i];
                let q = o.pos_1e6 + if i == add_i { add_1e6 } else { 0 };
                if q != 0 && o.cfg.und as usize == u && o.cfg.exp_ms > now_ms {
                    let tau_d = (o.cfg.exp_ms - now_ms) as f64 / DAY_MS_F;
                    let k = o.cfg.strike_1e6 as f64 / 1e6;
                    let sig = self.sigma_hat(u, tau_d).unwrap_or(1.0);
                    let tau_y = tau_d / YEAR_D;
                    if let (Some(a), Some(b)) =
                        (bs::price(o.cfg.call, s, k, tau_y, sig), bs::price(o.cfg.call, s2, k, tau_y, sig))
                    {
                        pnl += q as f64 / 1e6 * (b.price - a.price);
                    }
                }
                i += 1;
            }
            if -pnl > worst {
                worst = -pnl;
            }
            side += 1;
        }
        (worst * 1e6) as i64
    }

    /// Gross premium at stake, USD ×1e6: every position at its entry basis
    /// (a reduce lowers it), plus the new risk of each IoC still in flight
    /// at its limit — so a second order is never sized against headroom
    /// the first one already took.
    fn gross_premium(&self) -> i64 {
        let mut g = 0i64;
        let mut i = 0usize;
        while i < self.n_opts {
            let o = &self.opts[i];
            g = g.saturating_add(mul_1e6(o.pos_1e6.abs(), o.avg_px_1e6));
            if o.pending_q_1e6 != 0 {
                let grow = o.pos_1e6.saturating_add(o.pending_q_1e6).abs() - o.pos_1e6.abs();
                if grow > 0 {
                    g = g.saturating_add(mul_1e6(grow, o.pending_px_1e6));
                }
            }
            i += 1;
        }
        g
    }

    fn submit<C: Ctx>(&mut self, ctx: &mut C, o: Order) -> bool {
        match ctx.submit(o) {
            Ok(()) => {
                self.emitted = self.emitted.wrapping_add(1);
                true
            }
            Err(_) => {
                self.dropped = self.dropped.wrapping_add(1);
                self.counters.ctx_refused = self.counters.ctx_refused.wrapping_add(1);
                false
            }
        }
    }

    fn next_oid(&mut self) -> u64 {
        let oid = self.next_oid;
        self.next_oid = self.next_oid.wrapping_add(1).max(1);
        oid
    }

    /// Clear option `i`'s IoC in flight (its fill, its event, or the
    /// timeout) — the underlying may send its next one.
    fn clear_pending(&mut self, i: usize) {
        let o = &mut self.opts[i];
        if o.pending_oid == 0 {
            return;
        }
        o.pending_oid = 0;
        o.pending_q_1e6 = 0;
        o.pending_px_1e6 = 0;
        let u = o.cfg.und as usize;
        self.und[u].opt_pending = self.und[u].opt_pending.saturating_sub(1);
    }

    /// Sample the oracle into each held expiry's window (1 s grid,
    /// sample-and-hold — `core_settle`'s law), from just before the
    /// window opens until expiry.
    fn sample_settlement(&mut self, now_ms: u64) {
        let mut i = 0usize;
        while i < self.n_opts {
            let at = i;
            i += 1;
            let (pos, und, exp) = (self.opts[at].pos_1e6, self.opts[at].cfg.und, self.opts[at].cfg.exp_ms);
            let opens = exp.saturating_sub(SETTLE_WINDOW_MS + SETTLE_LEAD_MS);
            if pos == 0 || now_ms < opens || now_ms > exp {
                continue;
            }
            let u = und as usize;
            let mut slot = HCV_SETTLE_SLOTS;
            let mut free = HCV_SETTLE_SLOTS;
            let mut k = 0usize;
            while k < self.settle.len() {
                let s = &self.settle[k];
                if s.live && s.und as usize == u && s.exp_ms == exp {
                    slot = k;
                    break;
                }
                if !s.live && free == HCV_SETTLE_SLOTS {
                    free = k;
                }
                k += 1;
            }
            if slot == HCV_SETTLE_SLOTS {
                if free == HCV_SETTLE_SLOTS {
                    continue; // every slot busy: this expiry settles on the fallback
                }
                let s = &mut self.settle[free];
                s.live = true;
                s.und = und;
                s.exp_ms = exp;
                s.last_sample_ms = 0;
                s.px_1e6 = 0;
                s.window.reset(exp);
                slot = free;
            }
            let px = self.und[u].oracle_1e6;
            let s = &mut self.settle[slot];
            if px > 0 && s.last_sample_ms != now_ms {
                let _ = s.window.push(now_ms, px);
                s.last_sample_ms = now_ms;
            }
        }
    }

    /// Settle every position whose expiry is `settle_delay` past: the
    /// replicated median of means over the window sampled (fixed once per
    /// window), else the last oracle — both counted when not the full
    /// window.
    fn settle_expired(&mut self, now_ms: u64) {
        let Some(p) = self.p.as_deref() else { return };
        let delay = u64::from(p.settle_delay_ms);
        let order = p.settle_order;
        // Fix each due window's price, once.
        let mut k = 0usize;
        while k < self.settle.len() {
            let s = &mut self.settle[k];
            if s.live && s.px_1e6 == 0 && now_ms >= s.exp_ms.saturating_add(delay) {
                s.px_1e6 = match s.window.settle_1e6(order, &mut self.scratch) {
                    Some(v) if v > 0 => v,
                    _ => -1,
                };
                if s.window.points() < HCV_SETTLE_FULL_POINTS {
                    self.counters.settle_fallbacks = self.counters.settle_fallbacks.wrapping_add(1);
                }
            }
            k += 1;
        }
        // Book every position past its expiry.
        let mut i = 0usize;
        while i < self.n_opts {
            let at = i;
            i += 1;
            // COPY: one option's config (40 B) — the booking below writes
            // the member's cash and position while reading it.
            let cfg = self.opts[at].cfg;
            let pos = self.opts[at].pos_1e6;
            if pos == 0 || now_ms < cfg.exp_ms.saturating_add(delay) {
                continue;
            }
            let u = cfg.und as usize;
            let mut s_t = 0i64;
            let mut found = false;
            let mut k = 0usize;
            while k < self.settle.len() {
                let s = &self.settle[k];
                if s.live && s.und as usize == u && s.exp_ms == cfg.exp_ms {
                    s_t = s.px_1e6;
                    found = true;
                    break;
                }
                k += 1;
            }
            if s_t <= 0 {
                if !found {
                    // No window at all (every slot was busy).
                    self.counters.settle_fallbacks = self.counters.settle_fallbacks.wrapping_add(1);
                }
                s_t = self.und[u].oracle_1e6;
            }
            if s_t <= 0 {
                continue; // no price anywhere yet: booked at the first oracle
            }
            // A long receives the payoff, a short pays it.
            self.cash_usd_1e6 = self.cash_usd_1e6.saturating_add(mul_1e6(pos, intrinsic_1e6(&cfg, s_t)));
            self.opts[at].pos_1e6 = 0;
            self.opts[at].avg_px_1e6 = 0;
            self.counters.settlements = self.counters.settlements.wrapping_add(1);
        }
        // Free the windows whose expiry holds nothing any more.
        let mut k = 0usize;
        while k < self.settle.len() {
            if self.settle[k].live && self.settle[k].px_1e6 != 0 {
                let (u, e) = (self.settle[k].und, self.settle[k].exp_ms);
                let mut held = false;
                let mut j = 0usize;
                while j < self.n_opts {
                    let o = &self.opts[j];
                    held |= o.pos_1e6 != 0 && o.cfg.und == u && o.cfg.exp_ms == e;
                    j += 1;
                }
                if !held {
                    self.settle[k].live = false;
                }
            }
            k += 1;
        }
    }

    fn decide<C: Ctx>(&mut self, now_ns: NsTs, now_ms: u64, stopped: bool, ctx: &mut C) {
        let Some(p) = self.p.as_deref() else { return };
        let theta = p.theta_vol_1e6 as f64 / 1e6;
        let band = f64::from(p.atm_band_bps) / 1e4;
        let (tmin, tmax) = (f64::from(p.tenor_min_d), f64::from(p.tenor_max_d));
        let (qstale, ostale) = (p.quote_stale_ms, p.oracle_stale_ms);
        let (ev_law, ev_stale) = (p.event_law, u64::from(p.events_stale_ms));
        let (clip, vcap, pcap, tcap, step) = (
            p.clip_usd_1e6,
            p.vega_cap_usd_1e6,
            p.premium_cap_usd_1e6,
            p.tail_loss_usd_1e6,
            p.opt_size_step_1e6,
        );
        let cal_fresh =
            self.events.generated_ms != 0 && now_ms.saturating_sub(self.events.generated_ms) <= ev_stale;
        let mut i = 0usize;
        while i < self.n_opts {
            let k = i;
            i += 1;
            // COPY: one option's state (≤ 144 B) per judged option per
            // 1 s timer — cold; a borrow could not outlive the `&mut self`
            // calls below (the submit, the counters).
            let o = self.opts[k];
            if !o.dirty || o.pending_oid != 0 {
                continue;
            }
            let u = o.cfg.und as usize;
            // One IoC in flight per underlying: its verdict moves the book
            // every cap below reads (the dirty flag waits for it).
            if self.und[u].opt_pending != 0 {
                continue;
            }
            self.opts[k].dirty = false;
            if o.cfg.exp_ms <= now_ms {
                continue;
            }
            let tau_d = (o.cfg.exp_ms - now_ms) as f64 / DAY_MS_F;
            let s = self.und[u].oracle_1e6 as f64 / 1e6;
            let strike = o.cfg.strike_1e6 as f64 / 1e6;
            if tau_d < tmin || tau_d > tmax || s <= 0.0 || (s / strike).ln().abs() > band {
                continue;
            }
            self.counters.judged = self.counters.judged.wrapping_add(1);
            // COPY: the underlying's state (≤ 160 B), for the liveness
            // checks — cold, as above.
            let st = self.und[u];
            let crossed = o.bid_1e6 > 0 && o.ask_1e6 > 0 && o.bid_1e6 > o.ask_1e6;
            if crossed
                || !self.quote_live(&o, now_ns, qstale)
                || !Self::fresh(st.oracle_ns, now_ns, ostale)
                || !self.touch_live(&st, now_ns, ostale)
            {
                self.counters.skip_stale = self.counters.skip_stale.wrapping_add(1);
                continue;
            }
            if ev_law && (!cal_fresh || self.events.any_in(u, now_ms, o.cfg.exp_ms) != Some(false)) {
                self.counters.skip_event = self.counters.skip_event.wrapping_add(1);
                continue;
            }
            let Some(sig_hat) = self.sigma_hat(u, tau_d) else {
                self.counters.skip_forecast = self.counters.skip_forecast.wrapping_add(1);
                continue;
            };
            let tau_y = tau_d / YEAR_D;
            let sig_bid = if o.bid_1e6 > 0 {
                bs::implied_vol(o.cfg.call, s, strike, tau_y, o.bid_1e6 as f64 / 1e6)
            } else {
                None
            };
            let sig_ask = if o.ask_1e6 > 0 {
                bs::implied_vol(o.cfg.call, s, strike, tau_y, o.ask_1e6 as f64 / 1e6)
            } else {
                None
            };
            let (sell, px, shown) = match (sig_bid, sig_ask) {
                (Some(b), _) if b - sig_hat >= theta => (true, o.bid_1e6, o.bid_qty_1e6),
                (_, Some(a)) if sig_hat - a >= theta => (false, o.ask_1e6, o.ask_qty_1e6),
                _ => continue,
            };
            // A reduce closes risk the book holds: the stops and the
            // premium cap never block it, and it never flips through zero.
            let reducing = (sell && o.pos_1e6 > 0) || (!sell && o.pos_1e6 < 0);
            if stopped && !reducing {
                self.counters.skip_stopped = self.counters.skip_stopped.wrapping_add(1);
                continue;
            }
            let Some((_, vega_now)) = self.exposure(u, now_ms, now_ns) else {
                // A held option cannot be priced: no vega to size against.
                self.counters.skip_forecast = self.counters.skip_forecast.wrapping_add(1);
                continue;
            };
            let mut q = shown.min(div_1e6(clip, px));
            if reducing {
                q = q.min(o.pos_1e6.abs());
            } else {
                let room = pcap.saturating_sub(self.gross_premium());
                q = q.min(div_1e6(room.max(0), px));
            }
            // |net vega| after the trade: within the cap — or, for a
            // reduce, no worse than now (net vega is not the position:
            // selling one leg of a hedged pair can raise it).
            if let Some(g) = bs::price(o.cfg.call, s, strike, tau_y, sig_hat) {
                let vpc = (g.vega_pt * 1e6) as i64; // USD/pt ×1e6 per contract
                if vpc > 0 {
                    let lim = if reducing { vcap.max(vega_now.abs()) } else { vcap };
                    // A sale adds −vega, a purchase +vega.
                    let vroom = if sell { lim + vega_now } else { lim - vega_now };
                    q = q.min(div_1e6(vroom.max(0), vpc));
                }
            }
            q -= q % step;
            // The one-day 10σ stress: new risk fits the tail cap or lowers
            // the stress; a reduce may not raise it past the cap either.
            // Halved until it fits.
            if q > 0 {
                let base = self.stress_loss(u, k, 0, now_ms);
                let mut fits = false;
                let mut tries = 0u32;
                while q > 0 && tries < 16 {
                    let s_loss = self.stress_loss(u, k, if sell { -q } else { q }, now_ms);
                    if s_loss <= tcap || s_loss < base || (reducing && s_loss == base) {
                        fits = true;
                        break;
                    }
                    q /= 2;
                    q -= q % step;
                    tries += 1;
                }
                if !fits {
                    q = 0;
                }
            }
            if q <= 0 {
                self.counters.skip_caps = self.counters.skip_caps.wrapping_add(1);
                continue;
            }
            let oid = self.next_oid();
            let order = Order::new(
                now_ns,
                VenueId::Hypercall,
                o.cfg.sym,
                if sell { Side::Ask } else { Side::Bid },
                core_fill::ORDER_KIND_IOC,
                Price::from_raw(px),
                Qty::from_raw(q),
                oid,
            );
            if self.submit(ctx, order) {
                let x = &mut self.opts[k];
                x.pending_oid = oid;
                x.pending_ns = now_ns;
                x.pending_q_1e6 = if sell { -q } else { q };
                x.pending_px_1e6 = px;
                // Judged again after its verdict (the book moved).
                x.dirty = true;
                self.und[u].opt_pending = self.und[u].opt_pending.saturating_add(1);
                if sell {
                    self.counters.sells = self.counters.sells.wrapping_add(1);
                } else {
                    self.counters.buys = self.counters.buys.wrapping_add(1);
                }
            }
        }
    }

    fn hedge<C: Ctx>(&mut self, now_ns: NsTs, now_ms: u64, ctx: &mut C) {
        let Some(p) = self.p.as_deref() else { return };
        let (band, min_usd, slip, ostale, unwind_ms) = (
            p.hedge_band_1e6,
            p.hedge_min_usd_1e6,
            f64::from(p.hedge_slip_bps) / 1e4,
            p.oracle_stale_ms,
            u64::from(p.unwind_min) * MINUTE_MS,
        );
        let mut u = 0usize;
        while u < self.n_und {
            let k = u;
            u += 1;
            // COPY: the underlying's state (≤ 160 B) per underlying per
            // 1 s timer — cold; the order below writes into `self`.
            let st = self.und[k];
            if st.hedge_pending_oid != 0 || st.oracle_1e6 <= 0 {
                continue;
            }
            let gross = self.gross_contracts(k, now_ms);
            // Never before a fill: nothing held, nothing to hedge.
            if gross == 0 && st.hedge_pos_1e6 == 0 {
                continue;
            }
            if !self.touch_live(&st, now_ns, ostale) || !Self::fresh(st.oracle_ns, now_ns, ostale) {
                continue;
            }
            let mut near_exp = u64::MAX;
            let mut i = 0usize;
            while i < self.n_opts {
                let o = &self.opts[i];
                if o.pos_1e6 != 0 && o.cfg.und as usize == k && o.cfg.exp_ms > now_ms && o.cfg.exp_ms < near_exp {
                    near_exp = o.cfg.exp_ms;
                }
                i += 1;
            }
            // A slice boundary inside the final window re-hedges whatever
            // the band says — that is the TWAP. At expiry the options leave
            // the delta and the band (their base is unexpired contracts), so
            // the last slice's residual is closed at T.
            let mut slice_due = false;
            if unwind_ms > 0 && near_exp != u64::MAX && near_exp - now_ms < unwind_ms {
                let slice = (near_exp - now_ms) / MINUTE_MS;
                if slice != st.last_slice {
                    slice_due = true;
                    self.und[k].last_slice = slice;
                }
            } else {
                self.und[k].last_slice = NO_SLICE;
            }
            // A held option that cannot be priced leaves the delta unknown:
            // hold the hedge as it is rather than trade on part of it.
            let Some((delta, _)) = self.exposure(k, now_ms, now_ns) else {
                continue;
            };
            if delta.abs() <= mul_1e6(band, gross) && !slice_due {
                continue;
            }
            let lot = hl_px::hl_perp_lot_1e6(st.sz_dec).max(1);
            let mut q = -delta;
            q -= q % lot;
            if q == 0 || mul_1e6(q.abs(), st.oracle_1e6) < min_usd {
                continue;
            }
            let buy = q > 0;
            let touch = if buy { st.hedge_ask_1e6 } else { st.hedge_bid_1e6 };
            if touch <= 0 {
                continue;
            }
            let raw = (touch as f64 * if buy { 1.0 + slip } else { 1.0 - slip }) as i64;
            let px = hl_px::hl_perp_round_px(raw, st.sz_dec, buy);
            if px <= 0 {
                continue;
            }
            let oid = self.next_oid();
            // The paper strict-cross law expires an IoC at its ttl: without
            // one, a hedge the member released at `HCV_PENDING_NS` could
            // still fill beside its successor.
            let order = Order::new(
                now_ns,
                VenueId::Hyperliquid,
                st.hedge_sym,
                if buy { Side::Bid } else { Side::Ask },
                core_fill::ORDER_KIND_IOC,
                Price::from_raw(px),
                Qty::from_raw(q.abs()),
                oid,
            )
            .with_ttl_ns(HCV_PENDING_NS);
            if self.submit(ctx, order) {
                self.und[k].hedge_pending_oid = oid;
                self.und[k].hedge_pending_ns = now_ns;
                self.counters.hedges = self.counters.hedges.wrapping_add(1);
                if slice_due {
                    self.counters.unwind_slices = self.counters.unwind_slices.wrapping_add(1);
                }
            }
        }
    }

    fn release_stale_pending(&mut self, now_ns: NsTs) {
        let mut i = 0usize;
        while i < self.n_opts {
            if self.opts[i].pending_oid != 0 && now_ns.saturating_sub(self.opts[i].pending_ns) >= HCV_PENDING_NS {
                self.clear_pending(i);
            }
            i += 1;
        }
        let mut u = 0usize;
        while u < self.n_und {
            let st = &mut self.und[u];
            if st.hedge_pending_oid != 0 && now_ns.saturating_sub(st.hedge_pending_ns) >= HCV_PENDING_NS {
                st.hedge_pending_oid = 0;
            }
            u += 1;
        }
    }

    fn publish_gauges(&mut self, now_ms: u64, now_ns: NsTs, pnl: i64) {
        let mut n = 0i64;
        let mut i = 0usize;
        while i < self.n_opts {
            n += i64::from(self.opts[i].pos_1e6 != 0);
            i += 1;
        }
        let mut vabs = 0i64;
        let mut u = 0usize;
        while u < self.n_und {
            if let Some((_, v)) = self.exposure(u, now_ms, now_ns) {
                vabs = vabs.saturating_add(v.abs());
            }
            u += 1;
        }
        self.counters.positions = n;
        self.counters.vega_abs_usd_1e6 = vabs;
        self.counters.pnl_usd_1e6 = pnl;
        self.counters.day_pnl_usd_1e6 = pnl.saturating_sub(self.day_start_pnl_usd_1e6);
    }
}

/// An option's payoff per contract at `s_t`, USD ×1e6.
#[inline]
fn intrinsic_1e6(o: &HcvOpt, s_t: i64) -> i64 {
    if o.call {
        (s_t - o.strike_1e6).max(0)
    } else {
        (o.strike_1e6 - s_t).max(0)
    }
}

/// `a × b / 1e6` in i128, saturating.
#[inline]
fn mul_1e6(a: i64, b: i64) -> i64 {
    let v = i128::from(a) * i128::from(b) / 1_000_000;
    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// `a × 1e6 / b` (b > 0), saturating; 0 for b ≤ 0.
#[inline]
fn div_1e6(a: i64, b: i64) -> i64 {
    if b <= 0 {
        return 0;
    }
    let v = i128::from(a) * 1_000_000 / i128::from(b);
    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

impl StrategyCounters for HcvStrategy {
    fn orders_emitted(&self) -> u64 {
        self.emitted
    }

    fn orders_dropped(&self) -> u64 {
        self.dropped
    }

    fn strategy_kind(&self) -> &'static str {
        "hcv"
    }

    fn hcv_counters(&self, out: &mut HcvCounters) {
        *out = self.counters;
    }
}

impl Strategy for HcvStrategy {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        Ok(())
    }

    fn on_tick<C: Ctx>(&mut self, tick: &Tick, _ctx: &mut C) {
        if !self.configured {
            return;
        }
        if tick.venue == VenueId::Hypercall as u8 {
            // Any Hypercall tick vouches for the feed (module doc).
            self.hc_alive_ns = tick.ts_ns;
            if let Some(i) = self.opt_index(tick.sym) {
                let o = &mut self.opts[i];
                let live = !tick.is_stale();
                let (b, bq, a, aq) = (tick.bid_px.raw(), tick.bid_qty.raw(), tick.ask_px.raw(), tick.ask_qty.raw());
                let (bid_ok, ask_ok) = (live && b > 0 && bq > 0, live && a > 0 && aq > 0);
                o.bid_1e6 = if bid_ok { b } else { 0 };
                o.bid_qty_1e6 = if bid_ok { bq } else { 0 };
                o.ask_1e6 = if ask_ok { a } else { 0 };
                o.ask_qty_1e6 = if ask_ok { aq } else { 0 };
                o.quote_ns = tick.ts_ns;
                o.dirty = true;
            }
            return;
        }
        if tick.venue == VenueId::Hyperliquid as u8 {
            self.hl_alive_ns = tick.ts_ns;
            if let Some(u) = self.und_of_hedge(tick.sym) {
                // A touch the ingress marked stale is no touch.
                let st = &mut self.und[u];
                let live = !tick.is_stale();
                let (b, a) = (tick.bid_px.raw(), tick.ask_px.raw());
                st.hedge_bid_1e6 = if live && b > 0 { b } else { 0 };
                st.hedge_ask_1e6 = if live && a > 0 { a } else { 0 };
                st.hedge_ns = tick.ts_ns;
            }
        }
    }

    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}

    fn on_venue_event<C: Ctx>(&mut self, event: &ChannelEvent, _ctx: &mut C) {
        if !self.configured
            || event.venue != VenueId::Hyperliquid as u8
            || event.channel != ChannelId::Mark as u8
            || event.v1 <= 0
        {
            return;
        }
        if let Some(u) = self.und_of_hedge(event.sym) {
            // `v1` = oraclePx — what Hypercall settles on, and S.
            self.und[u].oracle_1e6 = event.v1;
            self.und[u].oracle_ns = event.ts_ns;
            // A new S re-judges every option of the underlying.
            let mut i = 0usize;
            while i < self.n_opts {
                if self.opts[i].cfg.und as usize == u {
                    self.opts[i].dirty = true;
                }
                i += 1;
            }
        }
    }

    fn on_fill<C: Ctx>(&mut self, fill: &Fill, _ctx: &mut C) {
        let q = fill.qty.raw();
        if !self.configured || q <= 0 {
            return;
        }
        let px = fill.px.raw();
        let buy = fill.side == Side::Bid;
        let flow = mul_1e6(q, px);
        if let Some(i) = self.opt_index(fill.sym) {
            let signed = if buy { q } else { -q };
            let o = &mut self.opts[i];
            let old = o.pos_1e6;
            let new = old.saturating_add(signed);
            // The entry basis: an add averages in, a reduce keeps it, a
            // flip starts it at this price, flat clears it.
            if new == 0 {
                o.avg_px_1e6 = 0;
            } else if old == 0 || (old > 0) == (signed > 0) {
                let tot = i128::from(old.abs()) + i128::from(q);
                let w = i128::from(old.abs()) * i128::from(o.avg_px_1e6) + i128::from(q) * i128::from(px);
                o.avg_px_1e6 = (w / tot.max(1)) as i64;
            } else if (new > 0) != (old > 0) {
                o.avg_px_1e6 = px;
            }
            o.pos_1e6 = new;
            o.last_px_1e6 = px;
            let mine = o.pending_oid != 0 && o.pending_oid == fill.order_id;
            if mine {
                self.clear_pending(i);
            }
            self.cash_usd_1e6 = self.cash_usd_1e6.saturating_add(if buy { -flow } else { flow });
            self.counters.option_fills = self.counters.option_fills.wrapping_add(1);
            return;
        }
        if let Some(u) = self.und_of_hedge(fill.sym) {
            let st = &mut self.und[u];
            st.hedge_pos_1e6 = st.hedge_pos_1e6.saturating_add(if buy { q } else { -q });
            if st.hedge_pending_oid == fill.order_id {
                st.hedge_pending_oid = 0;
            }
            // The hedge's cash flow; its position is marked at the oracle.
            self.cash_usd_1e6 = self.cash_usd_1e6.saturating_add(if buy { -flow } else { flow });
            self.counters.hedge_fills = self.counters.hedge_fills.wrapping_add(1);
        }
    }

    /// The paper law's miss (and, once armed, a venue's refusal) frees the
    /// instrument at once.
    fn on_order_event<C: Ctx>(&mut self, event: &OrderEvent, _ctx: &mut C) {
        if !self.configured
            || (event.kind != core_types::ORDER_EVENT_CANCELED && event.kind != core_types::ORDER_EVENT_REJECTED)
        {
            return;
        }
        if let Some(i) = self.opt_index(event.sym) {
            if self.opts[i].pending_oid != 0 && self.opts[i].pending_oid == event.client_oid {
                self.clear_pending(i);
            }
            return;
        }
        if let Some(u) = self.und_of_hedge(event.sym) {
            if self.und[u].hedge_pending_oid == event.client_oid {
                self.und[u].hedge_pending_oid = 0;
            }
        }
    }

    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C) {
        if !self.configured {
            return;
        }
        if let Some(rx) = self.events_rx.as_mut() {
            if let Some(cal) = rx.try_take() {
                // COPY: the ≤ 1.1 KiB calendar out of its mailbox slot,
                // once per file change — the slot goes back to the
                // reader, and the member keeps its calendar between changes
                // (a lent slot cannot be held across timers).
                self.events = *cal;
                self.counters.calendars = self.counters.calendars.wrapping_add(1);
            }
        }
        let now_ms = self.wall_ms(now_ns);
        self.release_stale_pending(now_ns);
        self.sample_settlement(now_ms);
        self.settle_expired(now_ms);
        let pnl = self.marked_pnl(now_ms, now_ns);
        let day = now_ms / DAY_MS;
        if day != self.day {
            self.day = day;
            self.day_start_pnl_usd_1e6 = pnl;
        }
        let (kill, day_loss) = self.p.as_deref().map_or((true, 0), |p| (p.kill, p.day_loss_usd_1e6));
        let stopped = kill || pnl.saturating_sub(self.day_start_pnl_usd_1e6) <= -day_loss;
        self.decide(now_ns, now_ms, stopped, ctx);
        self.hedge(now_ns, now_ms, ctx);
        self.publish_gauges(now_ms, now_ns, pnl);
    }

    fn timer_period_ns(&self) -> u64 {
        match self.p.as_deref() {
            Some(p) if self.configured => u64::from(p.timer_ms) * 1_000_000,
            _ => u64::MAX,
        }
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests;
