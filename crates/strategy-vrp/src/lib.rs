// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-vrp — the delta-hedged variance-risk-premium member
//!
//! One short-dated Deribit option per daily expiry, entered only when
//! the venue's own implied vol leaves the band an integer HAR forecast
//! opens around it, delta-hedged on the hour against the venue's own
//! Black-Scholes delta, and exited at `E − ε` rather than held to
//! settlement.
//!
//! ## The doctrine this member is written under
//!
//! These four clauses are load-bearing. A reader who "fixes" any of them
//! without new evidence breaks the strategy in a way no test in this
//! crate can catch, because each one encodes a measurement made
//! elsewhere.
//!
//! 1. **Paper has no fills — SUBMIT is the position event.** The paper
//!    dispatcher never returns a fill, so [`VrpStrategy::on_fill`] is a
//!    documented no-op and every position this member believes it holds
//!    was created by a successful `ctx.submit`. That is an ASSUMPTION,
//!    and the only thing that will ever check it is V8's shadow
//!    reconciliation against the offline replay. Until then, a divergence
//!    between this member's position and the harness's is a real defect
//!    wearing the shape of a rounding difference.
//!
//! 2. **The book is USD-denominated, so the plain unadjusted BS delta
//!    from the venue is the correct hedge ratio.** Deribit reports an
//!    account-level `DeltaTotal` that carries an inverse-contract
//!    adjustment (`Δ − premium`), and a reader who meets that number
//!    will want to "correct" the hedge here. Do not. Our book values
//!    every leg in USD (VRP V2a), and in a USD-denominated book the
//!    correct ratio is the plain `delta_1e9` the venue already sends —
//!    the adjustment exists to express the same hedge in COIN margin
//!    terms (edge spec §2.3). Applying it here would double-count.
//!
//! 3. **Deribit option marks are COIN-denominated.** `mark_px_1e9` on an
//!    [`OptSummary`] from Deribit is a price in BTC, not dollars: a mark
//!    of `0.00038` at BTC = $79,000 is a **$30** premium. Every USD
//!    number in this crate goes through
//!    [`opt_registry::coin_to_usd_1e6`] with the registry's contract
//!    size and the record's own `underlying_px_1e9` — the same function
//!    the harness uses, so a paper submit and its replayed fill cannot
//!    disagree about what a premium is worth.
//!
//! 4. **E1 is the mechanism; E2 is the monetisation.** The member exists
//!    because short-dated crypto ATM implied vol is beaten by a plain
//!    fitted HAR at forecasting realised variance (E1, measured at 4 h
//!    and 8 h, absent by 12 h); the variance risk premium is what turns
//!    that into money (E2). If E1 stops holding, there is nothing left
//!    to harvest and no amount of premium makes up for it — so the QLIKE
//!    comparison is computed beside the bounds and exposed as a LIVE
//!    counter ([`VrpCounters::qlike_har_beats_iv`], edge spec §5.3). The
//!    halt must be something an operator sees on a dashboard, not
//!    something a later session finds in a report.
//!
//! ## The campaign
//!
//! ```text
//! E − τ − selection   pick the ATM call of the next daily expiry
//! E − τ               decide: IV > hi ⇒ short vol; IV < lo ⇒ long vol; else HOLD
//!                     on entry, submit the option IoC + the first hedge
//! every rebalance_ns  re-hedge if |target − current| ≥ band
//! E − ε               unwind both legs
//! ```
//!
//! ## Hot-path rules
//!
//! Zero allocation after [`VrpStrategy::configure`]; no floats anywhere;
//! every transcendental happens at the per-expiry decision boundary, so
//! the per-tick cost is a minute compare and (when a position is open) an
//! hourly compare. `#[repr(C)]` PODs, `debug_assert!` on invariants, no
//! `dyn`, no `unsafe`.
//!
//! ## Fail-closed table
//!
//! Every one of these is a counted, tested branch — the member holds and
//! says why, and never guesses:
//!
//! | condition | behaviour | counter |
//! |---|---|---|
//! | forecast has no bounds (cold ring / < 60 pairs) | no entry | `no_bounds` |
//! | option mark older than `stale_ns` | no entry, no rebalance | `stale_skips` |
//! | no instrument passes the selection law | no campaign | `no_selection` |
//! | mark flag absent, or a non-positive mark / IV | record ignored | `stale_skips` |
//! | sym not in the registry | record ignored | — |
//! | regime gate closed | no entry | `regime_blocked` |
//! | regime gate HARD closed | flatten now | `regime_exits` |

#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc
)]

use core_time::{NsTs, WallAnchor};
use core_types::{
    Order, Price, Qty, RegimeLabelSet, Side, SymbolId, Tick, VenueId, OPT_SUMMARY_FLAG_MARK_PX,
    SYMBOL_ID_NONE,
};
use core_types::{Fill, OptSummary, Signal};
use opt_registry::{OptRegistry, RIGHT_CALL};
use strategy_core::{Ctx, RegimeGate, Strategy, StrategyCounters, StrategyError, SubmitErr};

/// The counters the engine mirrors (defined in `strategy-core` so the
/// cli never names this crate — the icdp/vm precedent).
pub use strategy_core::VrpCounters;

/// Nanoseconds in a minute — the forecast's ingest cadence.
pub const MINUTE_NS: u64 = 60_000_000_000;

/// How long an option mark stays usable. Deribit's ticker cadence is
/// sub-second; a mark this old means the lane is broken, not slow.
pub const MARK_STALE_NS: u64 = 30_000_000_000;

/// TTL on every order this member submits (I1 model rule). One minute:
/// long enough to cross, short enough that an unfilled intent cannot sit
/// across a rebalance boundary and be double-counted.
pub const ORDER_TTL_NS: u64 = 60_000_000_000;

/// `side` value for a flat campaign.
pub const SIDE_FLAT: i8 = 0;
/// `side` value for LONG vol — implied vol was BELOW the band, so we buy
/// the option and hedge its delta away.
pub const SIDE_LONG_VOL: i8 = 1;
/// `side` value for SHORT vol — implied vol was ABOVE the band.
pub const SIDE_SHORT_VOL: i8 = -1;

/// The member's parameters, as parsed from `vrp.toml`
/// (`core_config::vrp`) and handed in at [`VrpStrategy::configure`].
///
/// A plain POD rather than a reference to the config type: this crate
/// must not depend on `core-config` (a strategy that can read files is a
/// strategy that can block a hot path), and the cli owns the translation.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VrpParams {
    /// Band half-width in log space ×1e9 (θ = 0.10 ⇒ `100_000_000`).
    pub theta_1e9: i64,
    /// Hold length ns. Validated against [`core_vol::tenor_of`].
    pub tau_ns: u64,
    /// Exit this many ns before expiry.
    pub epsilon_ns: u64,
    /// Select the strike this many ns before `E − τ`.
    pub selection_ns: u64,
    /// Hedge rebalance cadence ns.
    pub rebalance_ns: u64,
    /// Option position size ×1e6.
    pub qty_1e6: i64,
    /// Hedge rebalance band ×1e6.
    pub band_qty_1e6: i64,
}

impl Default for VrpParams {
    /// The edge spec's measured configuration: θ = 0.10, τ = 8 h, ε =
    /// 5 min, selection 10 min out, hourly rebalance, one contract, a
    /// band of 10 % of one contract's delta at Δ = 0.5.
    fn default() -> Self {
        Self {
            theta_1e9: 100_000_000,
            tau_ns: 28_800_000_000_000,
            epsilon_ns: 300_000_000_000,
            selection_ns: 600_000_000_000,
            rebalance_ns: 3_600_000_000_000,
            qty_1e6: 1_000_000,
            band_qty_1e6: 50_000,
        }
    }
}

/// The last option mark this member saw, in both denominations.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct OptMarkCache {
    /// Wall ns of the record.
    pub wall_ns: u64,
    /// USD ×1e6 (through [`opt_registry::coin_to_usd_1e6`]).
    pub px_usd_1e6: i64,
    /// Annualised implied vol, fraction ×1e9.
    pub iv_1e9: i64,
    /// The record's underlying reference px ×1e9.
    pub underlying_px_1e9: i64,
    /// Venue BS delta ×1e9 (doctrine clause 2: used unadjusted).
    pub delta_1e9: i32,
    /// Explicit padding — always zero.
    _pad: [u8; 4],
}

/// The delta-hedged VRP member.
#[repr(C, align(64))]
pub struct VrpStrategy {
    /// The forecast. BTC only in v1, so one engine.
    vol: core_vol::VolEngine,
    /// Boot-built option table (one venue per boot).
    registry: OptRegistry,
    params: VrpParams,
    anchor: WallAnchor,
    /// SHA-256 of the `vrp.toml` bytes this member booted with.
    hash: [u8; 32],

    /// The instrument whose minute closes feed the forecast.
    underlying_sym: SymbolId,
    /// The instrument the delta hedge trades.
    hedge_sym: SymbolId,

    /// Minute-roll state for the forecast's ingest.
    minute_id: u64,
    last_underlying_mid_1e6: i64,

    /// The selected option for the current campaign.
    selected_sym: SymbolId,
    /// Expiry of the selected option, wall ns.
    expiry_ns: u64,
    /// The last mark seen for [`Self::selected_sym`].
    last_mark: OptMarkCache,

    /// Campaign state.
    side: i8,
    entry_done: bool,
    /// The campaign is unwinding: every record retries the flatten until
    /// the book is actually flat. Set by the E−ε law and by a
    /// hard-closed regime gate — a submit ring that was full must never
    /// leave a position behind with nothing coming back for it.
    flatten_pending: bool,
    configured: bool,
    regime_open: bool,
    /// Signed option position ×1e6 (positive = long the option).
    opt_pos_qty_1e6: i64,
    /// Signed perp position ×1e6.
    perp_pos_qty_1e6: i64,
    /// Next hedge check, wall ns.
    next_rebalance_wall_ns: u64,

    regime_label: RegimeLabelSet,
    counters: VrpCounters,
    orders_emitted: u64,
    orders_dropped: u64,
    next_oid: u64,
}

impl Default for VrpStrategy {
    fn default() -> Self {
        Self::new()
    }
}

impl VrpStrategy {
    /// An unconfigured member: no artifact, no registry, no forecast.
    /// [`Strategy::on_start`] refuses a boot in this state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            vol: core_vol::VolEngine::new(),
            registry: OptRegistry::new(),
            params: VrpParams::default(),
            anchor: WallAnchor::new(0, 0),
            hash: [0; 32],
            underlying_sym: SYMBOL_ID_NONE,
            hedge_sym: SYMBOL_ID_NONE,
            minute_id: 0,
            last_underlying_mid_1e6: 0,
            selected_sym: SYMBOL_ID_NONE,
            expiry_ns: 0,
            last_mark: OptMarkCache::default(),
            side: SIDE_FLAT,
            entry_done: false,
            flatten_pending: false,
            configured: false,
            regime_open: true,
            opt_pos_qty_1e6: 0,
            perp_pos_qty_1e6: 0,
            next_rebalance_wall_ns: 0,
            regime_label: RegimeLabelSet::ANY,
            counters: VrpCounters::default(),
            orders_emitted: 0,
            orders_dropped: 0,
            next_oid: 1,
        }
    }

    /// Boot configuration. Allocation is fine here and nowhere else.
    ///
    /// Refuses anything the evidence does not support: an untradeable τ
    /// (kill criterion 4 — the 12 h and 24 h cells are not to be
    /// traded), a non-positive size or band, an ε that would exit before
    /// the entry, or an empty option table.
    pub fn configure(
        &mut self,
        params: VrpParams,
        registry: OptRegistry,
        underlying_sym: SymbolId,
        hedge_sym: SymbolId,
        anchor: WallAnchor,
        hash: [u8; 32],
    ) -> Result<(), StrategyError> {
        if core_vol::tenor_of(params.tau_ns).is_none() {
            return Err(StrategyError::Config(
                "vrp: tau_ns is not a tradeable tenor (E1 lives at 4 h and 8 h)",
            ));
        }
        if params.epsilon_ns >= params.tau_ns {
            return Err(StrategyError::Config("vrp: epsilon_ns must be < tau_ns"));
        }
        if params.qty_1e6 <= 0 || params.band_qty_1e6 <= 0 || params.theta_1e9 <= 0 {
            return Err(StrategyError::Config(
                "vrp: qty_1e6, band_qty_1e6 and theta_1e9 must all be > 0",
            ));
        }
        if registry.is_empty() {
            return Err(StrategyError::Config("vrp: the option registry is empty"));
        }
        if underlying_sym == SYMBOL_ID_NONE || hedge_sym == SYMBOL_ID_NONE {
            return Err(StrategyError::Config(
                "vrp: the underlying and hedge descriptors must resolve",
            ));
        }
        self.params = params;
        self.registry = registry;
        self.underlying_sym = underlying_sym;
        self.hedge_sym = hedge_sym;
        self.anchor = anchor;
        self.hash = hash;
        self.configured = true;
        Ok(())
    }

    /// Replay one boot-seed pair into the forecast (V5). Boot path.
    pub fn seed_pair(&mut self, x_1e9: i64, y_1e9: i64) {
        self.vol.seed_pair(x_1e9, y_1e9);
    }

    /// The artifact hash this member booted with.
    #[must_use]
    pub const fn hash(&self) -> [u8; 32] {
        self.hash
    }

    /// Fitted pairs held by the forecast.
    #[must_use]
    pub const fn n_pairs(&self) -> usize {
        self.vol.n_pairs()
    }

    /// The current campaign's side.
    #[must_use]
    pub const fn side(&self) -> i8 {
        self.side
    }

    /// Signed option position ×1e6.
    #[must_use]
    pub const fn opt_pos_qty_1e6(&self) -> i64 {
        self.opt_pos_qty_1e6
    }

    /// Signed perp position ×1e6.
    #[must_use]
    pub const fn perp_pos_qty_1e6(&self) -> i64 {
        self.perp_pos_qty_1e6
    }

    /// The option currently selected for the campaign.
    #[must_use]
    pub const fn selected_sym(&self) -> SymbolId {
        self.selected_sym
    }

    // -----------------------------------------------------------
    // Hedge arithmetic (doctrine clause 2)
    // -----------------------------------------------------------

    /// The perp quantity that flattens the option leg's delta:
    /// `−pos_qty × delta`. `i128` intermediate; floor division, so the
    /// hedge is never rounded UP into more risk than we hold.
    ///
    /// The delta is the venue's plain unadjusted BS delta. See doctrine
    /// clause 2 for why the inverse-contract adjustment does not belong
    /// here.
    #[inline]
    #[must_use]
    pub fn hedge_target_1e6(opt_pos_qty_1e6: i64, delta_1e9: i32) -> i64 {
        let t = -(opt_pos_qty_1e6 as i128) * delta_1e9 as i128;
        core_regime::math::floor_div(t, 1_000_000_000) as i64
    }

    /// Whether a hedge move is worth paying the spread for.
    #[inline]
    #[must_use]
    pub const fn hedge_band_breached(target_1e6: i64, current_1e6: i64, band_1e6: i64) -> bool {
        let d = target_1e6 - current_1e6;
        let d = if d < 0 { -d } else { d };
        d >= band_1e6
    }

    // -----------------------------------------------------------
    // Order emission
    // -----------------------------------------------------------

    #[inline]
    fn submit<C: Ctx>(&mut self, ctx: &mut C, order: Order) -> bool {
        match ctx.submit(order) {
            Ok(()) => {
                self.orders_emitted = self.orders_emitted.wrapping_add(1);
                true
            }
            Err(SubmitErr::RingFull) => {
                self.orders_dropped = self.orders_dropped.wrapping_add(1);
                false
            }
        }
    }

    #[inline]
    fn next_oid(&mut self) -> u64 {
        let o = self.next_oid;
        self.next_oid = self.next_oid.wrapping_add(1).max(1);
        o
    }

    /// Build one IoC. `qty_1e6` is SIGNED: the sign picks the side and
    /// the magnitude is the size.
    #[inline]
    fn ioc(&mut self, sym: SymbolId, px_1e6: i64, qty_1e6: i64, now: NsTs) -> Option<Order> {
        if qty_1e6 == 0 || px_1e6 <= 0 {
            return None;
        }
        let side = if qty_1e6 > 0 { Side::Bid } else { Side::Ask };
        let venue = VenueId::from_u8(core_types::symbol_venue_byte(sym))?;
        let oid = self.next_oid();
        Some(
            Order::new(
                now,
                venue,
                sym,
                side,
                1, // IoC
                Price::from_raw(px_1e6),
                Qty::from_raw(qty_1e6.abs()),
                oid,
            )
            .with_ttl_ns(ORDER_TTL_NS),
        )
    }

    /// Move the perp position to `target_1e6`, paper-accounting the
    /// move on a successful submit (doctrine clause 1).
    fn move_hedge<C: Ctx>(&mut self, ctx: &mut C, target_1e6: i64, now: NsTs) -> bool {
        let delta = target_1e6 - self.perp_pos_qty_1e6;
        if delta == 0 {
            return false;
        }
        let px = self.last_mark.underlying_px_1e9 / 1_000;
        let Some(order) = self.ioc(self.hedge_sym, px, delta, now) else {
            return false;
        };
        if !self.submit(ctx, order) {
            return false;
        }
        self.perp_pos_qty_1e6 = target_1e6;
        self.counters.hedges = self.counters.hedges.wrapping_add(1);
        true
    }

    // -----------------------------------------------------------
    // The campaign
    // -----------------------------------------------------------

    /// Reset campaign state for the next expiry. Position accounting is
    /// NOT cleared here — [`Self::flatten`] does that, and a reset that
    /// silently dropped a position would hide a bug rather than fix one.
    fn end_campaign(&mut self) {
        debug_assert_eq!(self.opt_pos_qty_1e6, 0, "campaign ended holding options");
        debug_assert_eq!(self.perp_pos_qty_1e6, 0, "campaign ended holding a hedge");
        self.selected_sym = SYMBOL_ID_NONE;
        self.expiry_ns = 0;
        self.side = SIDE_FLAT;
        self.entry_done = false;
        self.flatten_pending = false;
        self.last_mark = OptMarkCache::default();
        self.next_rebalance_wall_ns = 0;
    }

    /// Unwind both legs at the last known marks.
    fn flatten<C: Ctx>(&mut self, ctx: &mut C, now: NsTs) -> bool {
        let mut any = false;
        if self.opt_pos_qty_1e6 != 0 && self.last_mark.px_usd_1e6 > 0 {
            let closing = -self.opt_pos_qty_1e6;
            let px = self.last_mark.px_usd_1e6;
            let sym = self.selected_sym;
            if let Some(order) = self.ioc(sym, px, closing, now) {
                if self.submit(ctx, order) {
                    self.opt_pos_qty_1e6 = 0;
                    self.counters.exits = self.counters.exits.wrapping_add(1);
                    any = true;
                }
            }
        }
        if self.perp_pos_qty_1e6 != 0 && self.move_hedge(ctx, 0, now) {
            any = true;
        }
        any
    }

    /// The E−ε exit, plus the settlement fold that keeps the forecast
    /// learning. Returns true when the campaign ended.
    fn maybe_exit<C: Ctx>(&mut self, ctx: &mut C, wall_ns: u64, now: NsTs) -> bool {
        if self.selected_sym == SYMBOL_ID_NONE {
            return false;
        }
        if !self.flatten_pending && wall_ns + self.params.epsilon_ns < self.expiry_ns {
            return false;
        }
        self.flatten_pending = true;
        self.flatten(ctx, now);
        if self.opt_pos_qty_1e6 != 0 || self.perp_pos_qty_1e6 != 0 {
            // The submit ring was full. Keep the campaign open and retry
            // on the next record: abandoning a live position because one
            // submit failed is strictly worse than trying again.
            return false;
        }
        // Fold the settled hold back into the forecast. `observe_settlement`
        // ignores an unarmed engine, so a campaign that never entered
        // contributes nothing — which is right: there was no hold.
        if self.vol.is_armed() {
            if let Some(rv) = self.realised_rv_1e9() {
                self.vol.observe_settlement(rv);
                self.counters.settlements = self.counters.settlements.wrapping_add(1);
                self.refresh_qlike();
            }
        }
        self.end_campaign();
        true
    }

    /// Realised vol over the hold that just ended, raw bps ×1e9 — the
    /// forecast's own `rv` over τ, read straight off the ring so the
    /// `y` this member forms is the same quantity the seed cutter forms.
    #[inline]
    fn realised_rv_1e9(&self) -> Option<i64> {
        self.vol.har_1e9(self.params.tau_ns)
    }

    #[inline]
    fn refresh_qlike(&mut self) {
        let q = self.vol.qlike_counters();
        self.counters.qlike_iv_1e6 = q.iv_mean_1e9 / 1_000;
        self.counters.qlike_har_1e6 = q.har_mean_1e9 / 1_000;
        self.counters.qlike_har_beats_iv = u64::from(q.har_beats_iv);
    }

    /// The selection law: at the first summary inside the selection
    /// window, the nearest-strike CALL of the nearest expiry still ahead
    /// of us. Nearest strike to the record's own underlying reference —
    /// the venue's number, not one we derived.
    fn select<C: Ctx>(&mut self, _ctx: &mut C, wall_ns: u64, underlying_px_1e6: i64) {
        let open_from = self.params.tau_ns + self.params.selection_ns;
        let mut best: Option<(SymbolId, u64, i64)> = None;
        for row in self.registry.rows() {
            if row.right != RIGHT_CALL || row.expiry_ns <= wall_ns {
                continue;
            }
            let lead = row.expiry_ns - wall_ns;
            if lead > open_from || lead < self.params.tau_ns {
                continue;
            }
            let d = (row.strike_1e6 - underlying_px_1e6).abs();
            match best {
                // Nearest expiry first, then nearest strike — a later
                // expiry is a different campaign, never a substitute.
                Some((_, e, bd)) if (row.expiry_ns, d) >= (e, bd) => {}
                _ => best = Some((row.sym, row.expiry_ns, d)),
            }
        }
        match best {
            Some((sym, expiry_ns, _)) => {
                self.selected_sym = sym;
                self.expiry_ns = expiry_ns;
                self.entry_done = false;
                self.flatten_pending = false;
            }
            None => self.counters.no_selection = self.counters.no_selection.wrapping_add(1),
        }
    }

    /// The two-compare decision at `E − τ`, and the entry it authorises.
    fn decide<C: Ctx>(&mut self, ctx: &mut C, wall_ns: u64, now: NsTs) {
        if self.entry_done || self.flatten_pending || self.selected_sym == SYMBOL_ID_NONE {
            return;
        }
        if wall_ns + self.params.tau_ns < self.expiry_ns {
            return; // not yet at the entry instant
        }
        // One decision per campaign, whatever it decides.
        self.entry_done = true;
        self.counters.decisions = self.counters.decisions.wrapping_add(1);

        if !self.regime_open {
            self.counters.regime_blocked = self.counters.regime_blocked.wrapping_add(1);
            return;
        }
        if self.last_mark.px_usd_1e6 <= 0
            || self.last_mark.iv_1e9 <= 0
            || wall_ns.saturating_sub(self.last_mark.wall_ns) > MARK_STALE_NS
        {
            self.counters.stale_skips = self.counters.stale_skips.wrapping_add(1);
            return;
        }
        let Some((lo, hi)) = self.vol.bounds(self.params.tau_ns, self.params.theta_1e9) else {
            self.counters.no_bounds = self.counters.no_bounds.wrapping_add(1);
            return;
        };
        // THE decision: two i64 compares. Everything transcendental
        // already happened, at the per-expiry boundary.
        let iv = self.last_mark.iv_1e9;
        let side = if iv > hi {
            SIDE_SHORT_VOL
        } else if iv < lo {
            SIDE_LONG_VOL
        } else {
            self.counters.holds = self.counters.holds.wrapping_add(1);
            return;
        };
        // Arm the forecast BEFORE the position exists: the regressor
        // must be formed from minutes strictly before the hold.
        if self.vol.arm_hold(self.params.tau_ns, iv).is_none() {
            self.counters.no_bounds = self.counters.no_bounds.wrapping_add(1);
            return;
        }
        let qty = self.params.qty_1e6 * side as i64;
        let sym = self.selected_sym;
        let px = self.last_mark.px_usd_1e6;
        let Some(order) = self.ioc(sym, px, qty, now) else {
            return;
        };
        if !self.submit(ctx, order) {
            return;
        }
        self.side = side;
        self.opt_pos_qty_1e6 = qty;
        self.counters.entries = self.counters.entries.wrapping_add(1);
        self.next_rebalance_wall_ns = wall_ns + self.params.rebalance_ns;
        // The first hedge goes out on the same instant as the entry.
        let target = Self::hedge_target_1e6(self.opt_pos_qty_1e6, self.last_mark.delta_1e9);
        self.move_hedge(ctx, target, now);
    }

    /// The hourly hedge check.
    fn maybe_rebalance<C: Ctx>(&mut self, ctx: &mut C, wall_ns: u64, now: NsTs) {
        if self.opt_pos_qty_1e6 == 0 || wall_ns < self.next_rebalance_wall_ns {
            return;
        }
        self.next_rebalance_wall_ns = wall_ns + self.params.rebalance_ns;
        if wall_ns.saturating_sub(self.last_mark.wall_ns) > MARK_STALE_NS
            || self.last_mark.underlying_px_1e9 <= 0
        {
            // A stale mark holds the hedge at its last target rather
            // than re-deriving it from a number nobody stands behind.
            self.counters.stale_skips = self.counters.stale_skips.wrapping_add(1);
            return;
        }
        let target = Self::hedge_target_1e6(self.opt_pos_qty_1e6, self.last_mark.delta_1e9);
        if Self::hedge_band_breached(target, self.perp_pos_qty_1e6, self.params.band_qty_1e6) {
            self.move_hedge(ctx, target, now);
        }
    }
}

impl StrategyCounters for VrpStrategy {
    #[inline]
    fn orders_emitted(&self) -> u64 {
        self.orders_emitted
    }
    #[inline]
    fn orders_dropped(&self) -> u64 {
        self.orders_dropped
    }
    #[inline]
    fn strategy_kind(&self) -> &'static str {
        "vrp"
    }
    #[inline]
    fn vrp_counters(&self) -> VrpCounters {
        self.counters
    }
}

impl Strategy for VrpStrategy {
    /// Enabled without an artifact ⇒ refuse the boot (the icdp
    /// precedent; the cli only sets the bit when `vrp.toml` resolved).
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if !self.configured {
            return Err(StrategyError::Config(
                "vrp: enabled without a resolved vrp.toml",
            ));
        }
        Ok(())
    }

    /// Underlying maintenance: the minute roll that feeds the forecast,
    /// the hourly hedge check, and the E−ε exit. Everything else this
    /// member does happens on an [`OptSummary`].
    #[inline]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        if !self.configured || tick.sym != self.underlying_sym {
            return;
        }
        let now = tick.ts_ns;
        let wall_ns = self.anchor.wall_of(now);
        let bid = tick.bid_px.raw();
        let ask = tick.ask_px.raw();
        if !tick.is_stale() && bid > 0 && ask > 0 {
            // Integer mid, floored — the same mid the ICDP feature law
            // and the regime law use.
            let mid = (bid + ask) >> 1;
            let minute = wall_ns / MINUTE_NS;
            if self.minute_id == 0 {
                self.minute_id = minute;
                self.last_underlying_mid_1e6 = mid;
            } else if minute != self.minute_id {
                // The CLOSE of a minute is its last mid, so the roll
                // publishes the value carried across the boundary, not
                // the first quote of the new minute.
                self.vol.on_minute_close(self.last_underlying_mid_1e6);
                self.minute_id = minute;
            }
            self.last_underlying_mid_1e6 = mid;
        }
        self.maybe_rebalance(ctx, wall_ns, now);
        self.maybe_exit(ctx, wall_ns, now);
    }

    /// The option lane: cache the mark, select at the window, decide at
    /// the entry instant.
    #[inline]
    fn on_opt_summary<C: Ctx>(&mut self, opt: &OptSummary, ctx: &mut C) {
        if !self.configured {
            return;
        }
        let Some(row) = self.registry.get(opt.sym) else {
            return;
        };
        if opt.flags & OPT_SUMMARY_FLAG_MARK_PX == 0
            || opt.mark_px_1e9 <= 0
            || opt.mark_iv_1e9 <= 0
            || opt.underlying_px_1e9 <= 0
        {
            self.counters.stale_skips = self.counters.stale_skips.wrapping_add(1);
            return;
        }
        // Doctrine clause 3: the ONE conversion, shared with the harness.
        let Some(px_usd_1e6) = opt_registry::coin_to_usd_1e6(
            opt.mark_px_1e9,
            opt.underlying_px_1e9,
            row.contract_size_1e9,
        ) else {
            self.counters.stale_skips = self.counters.stale_skips.wrapping_add(1);
            return;
        };
        let now = opt.ts_ns;
        let wall_ns = self.anchor.wall_of(now);
        let underlying_px_1e6 = opt.underlying_px_1e9 / 1_000;

        if self.selected_sym == SYMBOL_ID_NONE {
            self.select(ctx, wall_ns, underlying_px_1e6);
        }
        if opt.sym != self.selected_sym {
            return;
        }
        self.last_mark = OptMarkCache {
            wall_ns,
            px_usd_1e6,
            iv_1e9: opt.mark_iv_1e9,
            underlying_px_1e9: opt.underlying_px_1e9,
            delta_1e9: opt.delta_1e9,
            _pad: [0; 4],
        };
        self.decide(ctx, wall_ns, now);
        self.maybe_exit(ctx, wall_ns, now);
    }

    #[inline]
    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}

    /// Doctrine clause 1: paper has no fills. SUBMIT is the position
    /// event, and this callback is deliberately empty — not forgotten.
    #[inline]
    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}

    #[inline]
    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}

    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    #[inline]
    fn regime_label(&self) -> RegimeLabelSet {
        self.regime_label
    }

    #[inline]
    fn set_regime_label(&mut self, set: RegimeLabelSet) -> bool {
        self.regime_label = set;
        true
    }

    /// Closed ⇒ no new entries (the campaign's own exit law still
    /// drains). Hard-closed ⇒ flatten both legs NOW.
    fn on_regime<C: Ctx>(&mut self, gate: RegimeGate, ctx: &mut C) {
        self.regime_open = gate.open;
        if !gate.hard_closed() || !self.configured {
            return;
        }
        if self.selected_sym == SYMBOL_ID_NONE {
            return;
        }
        let now = ctx.now_ns();
        let wall_ns = self.anchor.wall_of(now);
        // Unwind NOW rather than at E−ε, through the same retrying path
        // the exit law uses.
        self.flatten_pending = true;
        self.counters.regime_exits = self.counters.regime_exits.wrapping_add(1);
        self.maybe_exit(ctx, wall_ns, now);
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, RegimeWord, TICK_FLAG_STALE};
    use opt_registry::{OptInstrument, RIGHT_PUT};

    const MONO0: NsTs = 3_191_000_000_000_000;
    /// 2026-09-10 08:00:00 UTC — a Deribit daily settle.
    const EXPIRY: u64 = 1_789_027_200_000_000_000;
    /// The anchor's wall instant: two days before that expiry.
    const WALL0: u64 = EXPIRY - 2 * 86_400_000_000_000_000 / 1_000;
    const TAU: u64 = 28_800_000_000_000;
    const OPT_BASE: u32 = 513;

    struct RecCtx {
        orders: Vec<Order>,
        full: bool,
        now: NsTs,
    }

    impl RecCtx {
        fn new() -> Self {
            Self {
                orders: Vec::new(),
                full: false,
                now: MONO0,
            }
        }
    }

    impl Ctx for RecCtx {
        fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
            if self.full {
                return Err(SubmitErr::RingFull);
            }
            self.orders.push(order);
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    fn perp_sym() -> SymbolId {
        make_symbol_id(VenueId::Deribit, 1)
    }

    fn opt_sym(k: u32) -> SymbolId {
        make_symbol_id(VenueId::Deribit, OPT_BASE + k)
    }

    /// A chain around $79,000: 8 calls and 8 puts at today's expiry, and
    /// 8 calls at tomorrow's — so the selection law has to choose, not
    /// merely accept what it is handed.
    fn registry() -> OptRegistry {
        let mut r = OptRegistry::new();
        let perp = perp_sym();
        let mut k = 0u32;
        while k < 8 {
            let strike = (77_000 + 500 * k as i64) * 1_000_000;
            r.insert(OptInstrument::new(
                opt_sym(k),
                perp,
                VenueId::Deribit as u8,
                EXPIRY,
                strike,
                RIGHT_CALL,
                1_000_000_000,
            ))
            .expect("call");
            r.insert(OptInstrument::new(
                opt_sym(8 + k),
                perp,
                VenueId::Deribit as u8,
                EXPIRY,
                strike,
                RIGHT_PUT,
                1_000_000_000,
            ))
            .expect("put");
            r.insert(OptInstrument::new(
                opt_sym(16 + k),
                perp,
                VenueId::Deribit as u8,
                EXPIRY + 86_400_000_000_000_000 / 1_000,
                strike,
                RIGHT_CALL,
                1_000_000_000,
            ))
            .expect("next-day call");
            k += 1;
        }
        r
    }

    fn mono_of(wall_ns: u64) -> NsTs {
        MONO0.wrapping_add(wall_ns.wrapping_sub(WALL0))
    }

    fn tick(wall_ns: u64, px_1e6: i64, stale: bool) -> Tick {
        let mut t = Tick::new(
            mono_of(wall_ns),
            VenueId::Deribit,
            perp_sym(),
            0,
            Price::from_raw(px_1e6 - 500_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(px_1e6 + 500_000),
            Qty::from_raw(1_000_000),
        );
        if stale {
            t.flags |= TICK_FLAG_STALE;
        }
        t
    }

    fn summary(wall_ns: u64, sym: SymbolId, iv_1e9: i64, delta_1e9: i64) -> OptSummary {
        OptSummary::new(
            mono_of(wall_ns),
            VenueId::Deribit,
            sym,
            OPT_SUMMARY_FLAG_MARK_PX,
            3_800_000, // 0.0038 BTC
            iv_1e9,
            79_000_000_000_000, // $79,000 ×1e9
            0,
            delta_1e9,
            1,
            1,
            -1,
        )
    }

    /// A configured member with a warm forecast ring and a fitted line,
    /// positioned two days before the expiry. `walk_bias` shifts the
    /// tape so the caller can move the forecast relative to a quoted IV.
    fn member(ctx: &mut RecCtx, params: VrpParams) -> (VrpStrategy, u64) {
        let mut m = VrpStrategy::new();
        m.configure(
            params,
            registry(),
            perp_sym(),
            perp_sym(),
            WallAnchor::new(MONO0, WALL0),
            [7u8; 32],
        )
        .expect("configure");

        // Warm the ring: 1441 minute closes ending well before the
        // selection window opens.
        let mut wall = WALL0;
        let mut px = 79_000_000_000i64;
        let mut s = 20_260_910i64;
        let mut i = 0usize;
        while i < 1_442 {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
            m.on_tick(&tick(wall, px, false), ctx);
            wall += MINUTE_NS;
            i += 1;
        }
        // Fit a line through 60 seeded pairs around the live regressor.
        let x = m.vol.x_1e9(params.tau_ns).expect("warm ring");
        let mut k = 0i64;
        while k < 60 {
            m.seed_pair(x - 300_000_000 + k * 11_000_000, x + k * 9_000_000);
            k += 1;
        }
        assert!(m.vol.fit().is_some(), "the test needs a fitted member");
        ctx.orders.clear();
        (m, wall)
    }

    // ---------------- the hedge arithmetic ----------------

    #[test]
    fn the_hedge_is_the_plain_unadjusted_delta() {
        // Doctrine clause 2: the book is USD-denominated, so the venue's
        // plain BS delta IS the hedge ratio. One long contract at
        // Δ = 0.5 is hedged by 0.5 SHORT perp.
        assert_eq!(
            VrpStrategy::hedge_target_1e6(1_000_000, 500_000_000),
            -500_000
        );
        // One SHORT contract at Δ = 0.5 is hedged by 0.5 LONG perp.
        assert_eq!(
            VrpStrategy::hedge_target_1e6(-1_000_000, 500_000_000),
            500_000
        );
        // A put's negative delta flips both.
        assert_eq!(
            VrpStrategy::hedge_target_1e6(1_000_000, -300_000_000),
            300_000
        );
        // Flat is flat, and Δ = 0 needs no hedge.
        assert_eq!(VrpStrategy::hedge_target_1e6(0, 900_000_000), 0);
        assert_eq!(VrpStrategy::hedge_target_1e6(1_000_000, 0), 0);
        // Floor division: the hedge is never rounded UP into more risk
        // than we hold.
        assert_eq!(VrpStrategy::hedge_target_1e6(1, 1), -1);
    }

    #[test]
    fn the_band_gates_the_rebalance_both_ways() {
        assert!(VrpStrategy::hedge_band_breached(600_000, 500_000, 50_000));
        assert!(VrpStrategy::hedge_band_breached(400_000, 500_000, 50_000));
        assert!(VrpStrategy::hedge_band_breached(550_000, 500_000, 50_000));
        assert!(!VrpStrategy::hedge_band_breached(549_999, 500_000, 50_000));
        assert!(!VrpStrategy::hedge_band_breached(500_000, 500_000, 50_000));
    }

    // ---------------- configure refusals ----------------

    #[test]
    fn configure_refuses_what_the_evidence_does_not_support() {
        let anchor = WallAnchor::new(MONO0, WALL0);
        let bad = |p: VrpParams| {
            let mut m = VrpStrategy::new();
            m.configure(p, registry(), perp_sym(), perp_sym(), anchor, [0; 32])
                .is_err()
        };
        // Kill criterion 4: the 12 h and 24 h cells are not tradeable.
        assert!(bad(VrpParams {
            tau_ns: 43_200_000_000_000,
            ..VrpParams::default()
        }));
        assert!(bad(VrpParams {
            tau_ns: 86_400_000_000_000,
            ..VrpParams::default()
        }));
        // An exit that precedes the entry.
        assert!(bad(VrpParams {
            epsilon_ns: TAU,
            ..VrpParams::default()
        }));
        // Sizes and the band must be real.
        assert!(bad(VrpParams {
            qty_1e6: 0,
            ..VrpParams::default()
        }));
        assert!(bad(VrpParams {
            band_qty_1e6: 0,
            ..VrpParams::default()
        }));
        assert!(bad(VrpParams {
            theta_1e9: 0,
            ..VrpParams::default()
        }));
        // An empty registry, and unresolved descriptors.
        let mut m = VrpStrategy::new();
        assert!(m
            .configure(
                VrpParams::default(),
                OptRegistry::new(),
                perp_sym(),
                perp_sym(),
                anchor,
                [0; 32]
            )
            .is_err());
        assert!(m
            .configure(
                VrpParams::default(),
                registry(),
                SYMBOL_ID_NONE,
                perp_sym(),
                anchor,
                [0; 32]
            )
            .is_err());
        // 4 h IS measured, so it configures.
        let mut ok = VrpStrategy::new();
        assert!(ok
            .configure(
                VrpParams {
                    tau_ns: 14_400_000_000_000,
                    rebalance_ns: 1_800_000_000_000,
                    selection_ns: 300_000_000_000,
                    ..VrpParams::default()
                },
                registry(),
                perp_sym(),
                perp_sym(),
                anchor,
                [0; 32]
            )
            .is_ok());
    }

    #[test]
    fn an_unconfigured_member_refuses_the_boot_and_does_nothing() {
        let mut ctx = RecCtx::new();
        let mut m = VrpStrategy::new();
        assert!(m.on_start(&mut ctx).is_err());
        m.on_tick(&tick(WALL0, 79_000_000_000, false), &mut ctx);
        m.on_opt_summary(&summary(WALL0, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        assert!(ctx.orders.is_empty(), "an unconfigured member never trades");
    }

    // ---------------- the full campaign ----------------

    #[test]
    fn one_campaign_selects_enters_hedges_and_exits() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        assert!(m.on_start(&mut ctx).is_ok());

        // --- selection: the first summary inside the window ---
        let sel_wall = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel_wall);
        m.on_opt_summary(&summary(sel_wall, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        // $79,000 with strikes 77,000..80,500 in 500s ⇒ k = 4 (79,000).
        assert_eq!(m.selected_sym(), opt_sym(4), "the ATM call of TODAY's expiry");
        assert_eq!(m.expiry_ns, EXPIRY);
        assert!(ctx.orders.is_empty(), "selection never trades");

        // --- the decision at E−τ ---
        // Quote an implied vol far above anything the fit can produce,
        // so the member is SHORT vol.
        let entry_wall = EXPIRY - TAU;
        ctx.now = mono_of(entry_wall);
        m.on_opt_summary(
            &summary(entry_wall, opt_sym(4), 5_000_000_000, 500_000_000),
            &mut ctx,
        );
        assert_eq!(m.side(), SIDE_SHORT_VOL, "IV above the band ⇒ sell vol");
        assert_eq!(m.vrp_counters().decisions, 1);
        assert_eq!(m.vrp_counters().entries, 1);
        assert_eq!(m.vrp_counters().holds, 0);
        assert_eq!(ctx.orders.len(), 2, "the option leg and its first hedge");

        // The option submit, field by field.
        let o = ctx.orders[0];
        assert_eq!(o.sym, opt_sym(4));
        assert_eq!(o.side, Side::Ask, "short vol sells the option");
        assert_eq!(o.kind, 1, "IoC");
        assert_eq!(o.qty.raw(), params.qty_1e6);
        assert_eq!(o.ttl_ns, ORDER_TTL_NS);
        // Doctrine clause 3: the price is USD, through the shared law —
        // 0.0038 BTC × $79,000 × 1.0 = $300.20.
        assert_eq!(
            o.px.raw(),
            opt_registry::coin_to_usd_1e6(3_800_000, 79_000_000_000_000, 1_000_000_000).unwrap()
        );
        assert_eq!(o.px.raw(), 300_200_000);

        // The hedge: short 1 contract at Δ = 0.5 ⇒ LONG 0.5 perp.
        let h = ctx.orders[1];
        assert_eq!(h.sym, perp_sym());
        assert_eq!(h.side, Side::Bid);
        assert_eq!(h.qty.raw(), 500_000);
        assert_eq!(h.kind, 1);
        assert_eq!(h.ttl_ns, ORDER_TTL_NS);
        assert_eq!(h.px.raw(), 79_000_000_000, "hedged at the venue's own underlying");
        assert_eq!(m.opt_pos_qty_1e6(), -params.qty_1e6);
        assert_eq!(m.perp_pos_qty_1e6(), 500_000);

        // --- three hourly rebalances, each on a moved delta ---
        ctx.orders.clear();
        let mut hedges = 0usize;
        let mut hour = 1u64;
        while hour <= 3 {
            let wall = entry_wall + hour * params.rebalance_ns;
            // A summary moves the delta, then a tick crosses the hour.
            ctx.now = mono_of(wall);
            let delta = 500_000_000 + hour as i64 * 100_000_000;
            m.on_opt_summary(&summary(wall, opt_sym(4), 5_000_000_000, delta), &mut ctx);
            m.on_tick(&tick(wall, 79_000_000_000, false), &mut ctx);
            assert_eq!(
                m.perp_pos_qty_1e6(),
                VrpStrategy::hedge_target_1e6(-params.qty_1e6, delta as i32),
                "hour {hour}: the hedge tracks the venue's delta"
            );
            hedges += 1;
            hour += 1;
        }
        assert_eq!(hedges, 3);
        assert_eq!(ctx.orders.len(), 3, "one hedge per breached band");
        for o in &ctx.orders {
            assert_eq!(o.sym, perp_sym());
            assert_eq!(o.kind, 1);
            assert_eq!(o.ttl_ns, ORDER_TTL_NS);
        }
        assert_eq!(m.vrp_counters().hedges, 4, "the entry hedge plus three");

        // --- the E−ε exit ---
        ctx.orders.clear();
        let exit_wall = EXPIRY - params.epsilon_ns;
        ctx.now = mono_of(exit_wall);
        m.on_tick(&tick(exit_wall, 79_000_000_000, false), &mut ctx);
        assert_eq!(ctx.orders.len(), 2, "both legs unwind");
        assert_eq!(ctx.orders[0].sym, opt_sym(4));
        assert_eq!(ctx.orders[0].side, Side::Bid, "buying back the short");
        assert_eq!(ctx.orders[0].qty.raw(), params.qty_1e6);
        assert_eq!(ctx.orders[1].sym, perp_sym());
        assert_eq!(ctx.orders[1].side, Side::Ask, "selling the long hedge");
        assert_eq!(m.opt_pos_qty_1e6(), 0);
        assert_eq!(m.perp_pos_qty_1e6(), 0);
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE, "the campaign ended");
        assert_eq!(m.vrp_counters().exits, 1);
        // And the settled hold went back into the forecast.
        assert_eq!(m.vrp_counters().settlements, 1);
        assert_eq!(m.n_pairs(), 61);
    }

    #[test]
    fn implied_vol_inside_the_band_holds() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        let (lo, hi) = m.vol.bounds(TAU, params.theta_1e9).expect("bounds");
        // Quote exactly the midpoint of the band.
        let iv = lo + (hi - lo) / 2;
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), iv, 500_000_000), &mut ctx);
        assert_eq!(m.side(), SIDE_FLAT);
        assert_eq!(m.vrp_counters().holds, 1);
        assert_eq!(m.vrp_counters().entries, 0);
        assert!(ctx.orders.is_empty(), "a hold submits nothing");
    }

    #[test]
    fn implied_vol_below_the_band_buys_vol() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), 1_000, 500_000_000), &mut ctx);
        assert_eq!(m.side(), SIDE_LONG_VOL);
        assert_eq!(m.opt_pos_qty_1e6(), params.qty_1e6);
        assert_eq!(ctx.orders[0].side, Side::Bid, "long vol buys the option");
        // Long 1 contract at Δ = 0.5 ⇒ SHORT 0.5 perp.
        assert_eq!(m.perp_pos_qty_1e6(), -500_000);
        assert_eq!(ctx.orders[1].side, Side::Ask);
    }

    // ---------------- the fail-closed table ----------------

    #[test]
    fn no_fit_means_no_entry() {
        // ABSENT DATA HOLDS: a warm ring with no fitted pairs produces
        // no bounds, and a member with no bounds does not trade.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let mut m = VrpStrategy::new();
        m.configure(
            params,
            registry(),
            perp_sym(),
            perp_sym(),
            WallAnchor::new(MONO0, WALL0),
            [0; 32],
        )
        .expect("configure");
        let mut wall = WALL0;
        let mut i = 0usize;
        while i < 1_442 {
            m.on_tick(&tick(wall, 79_000_000_000 + (i as i64 % 7) * 3_000_000, false), &mut ctx);
            wall += MINUTE_NS;
            i += 1;
        }
        assert!(m.vol.har_1e9(TAU).is_some(), "the ring IS warm");
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        let entry = EXPIRY - TAU;
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.vrp_counters().decisions, 1);
        assert_eq!(m.vrp_counters().no_bounds, 1);
        assert_eq!(m.vrp_counters().entries, 0);
        assert!(ctx.orders.is_empty());
    }

    #[test]
    fn a_stale_mark_blocks_the_entry() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        // The entry instant arrives on a TICK, with the last mark now
        // older than MARK_STALE_NS.
        let entry = EXPIRY - TAU + MARK_STALE_NS * 2;
        ctx.now = mono_of(entry);
        m.on_tick(&tick(entry, 79_000_000_000, false), &mut ctx);
        // The decision has not run — it runs on the option lane — so
        // drive it with a summary carrying no mark flag.
        let mut stale = summary(entry, opt_sym(4), 5_000_000_000, 500_000_000);
        stale.flags = 0;
        m.on_opt_summary(&stale, &mut ctx);
        assert_eq!(m.vrp_counters().entries, 0);
        assert!(m.vrp_counters().stale_skips >= 1);
        assert!(ctx.orders.is_empty());
    }

    #[test]
    fn a_record_with_no_usable_mark_is_counted_not_guessed() {
        let mut ctx = RecCtx::new();
        let (mut m, _) = member(&mut ctx, VrpParams::default());
        let w = EXPIRY - TAU - 300_000_000_000;
        let mut o = summary(w, opt_sym(4), 700_000_000, 500_000_000);
        o.mark_px_1e9 = 0;
        m.on_opt_summary(&o, &mut ctx);
        let mut o2 = summary(w, opt_sym(4), 0, 500_000_000);
        m.on_opt_summary(&o2, &mut ctx);
        o2.mark_iv_1e9 = 700_000_000;
        o2.underlying_px_1e9 = 0;
        m.on_opt_summary(&o2, &mut ctx);
        assert_eq!(m.vrp_counters().stale_skips, 3);
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE, "nothing was selected");
        assert!(ctx.orders.is_empty());
    }

    #[test]
    fn an_unregistered_sym_is_ignored() {
        let mut ctx = RecCtx::new();
        let (mut m, _) = member(&mut ctx, VrpParams::default());
        let w = EXPIRY - TAU - 300_000_000_000;
        let unknown = make_symbol_id(VenueId::Deribit, OPT_BASE + 200);
        m.on_opt_summary(&summary(w, unknown, 700_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.vrp_counters().stale_skips, 0, "not a mark problem");
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE);
        assert!(ctx.orders.is_empty());
    }

    #[test]
    fn a_closed_regime_gate_blocks_the_entry_and_a_hard_one_flattens() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);

        // Soft close: the decision runs and refuses.
        m.on_regime(
            RegimeGate::new([RegimeWord(0); 4], false, core_types::REGIME_OFF_SOFT),
            &mut ctx,
        );
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.vrp_counters().regime_blocked, 1);
        assert_eq!(m.vrp_counters().entries, 0);
        assert!(ctx.orders.is_empty());

        // Now open the gate, run a fresh campaign to a position, then
        // hard-close it.
        let mut ctx2 = RecCtx::new();
        let (mut m2, _) = member(&mut ctx2, params);
        ctx2.now = mono_of(sel);
        m2.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx2);
        ctx2.now = mono_of(entry);
        m2.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx2);
        assert_eq!(m2.opt_pos_qty_1e6(), -params.qty_1e6);
        ctx2.orders.clear();
        ctx2.now = mono_of(entry + 60_000_000_000);
        m2.on_regime(
            RegimeGate::new([RegimeWord(0); 4], false, core_types::REGIME_OFF_HARD),
            &mut ctx2,
        );
        assert_eq!(ctx2.orders.len(), 2, "a hard gate unwinds both legs NOW");
        assert_eq!(m2.opt_pos_qty_1e6(), 0);
        assert_eq!(m2.perp_pos_qty_1e6(), 0);
        assert_eq!(m2.selected_sym(), SYMBOL_ID_NONE);
        assert_eq!(m2.vrp_counters().regime_exits, 1);
    }

    #[test]
    fn a_full_submit_ring_never_abandons_a_position() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.opt_pos_qty_1e6(), -params.qty_1e6);

        // The exit instant arrives with the ring full.
        ctx.full = true;
        ctx.orders.clear();
        let exit = EXPIRY - params.epsilon_ns;
        ctx.now = mono_of(exit);
        m.on_tick(&tick(exit, 79_000_000_000, false), &mut ctx);
        assert_eq!(m.opt_pos_qty_1e6(), -params.qty_1e6, "still holding");
        assert_ne!(m.selected_sym(), SYMBOL_ID_NONE, "campaign still open");
        assert!(m.orders_dropped() > 0);

        // The ring drains; the very next tick completes the unwind.
        ctx.full = false;
        m.on_tick(&tick(exit + 1_000_000_000, 79_000_000_000, false), &mut ctx);
        assert_eq!(m.opt_pos_qty_1e6(), 0);
        assert_eq!(m.perp_pos_qty_1e6(), 0);
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE);
    }

    #[test]
    fn the_selection_law_takes_the_nearest_expiry_then_the_nearest_strike() {
        let mut ctx = RecCtx::new();
        let (mut m, _) = member(&mut ctx, VrpParams::default());
        // A summary from an instrument that is NOT the ATM call still
        // opens the selection, and selection picks by the law, not by
        // whichever record happened to arrive.
        let sel = EXPIRY - TAU - 300_000_000_000;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(0), 700_000_000, 200_000_000), &mut ctx);
        assert_eq!(m.selected_sym(), opt_sym(4), "ATM, not the arriving sym");
        let row = m.registry.get(m.selected_sym()).expect("registered");
        assert_eq!(row.expiry_ns, EXPIRY, "today's expiry, not tomorrow's");
        assert_eq!(row.right, RIGHT_CALL);
        assert_eq!(row.strike_1e6, 79_000_000_000);
    }

    #[test]
    fn a_stale_underlying_tick_never_feeds_the_forecast() {
        let mut ctx = RecCtx::new();
        let mut m = VrpStrategy::new();
        m.configure(
            VrpParams::default(),
            registry(),
            perp_sym(),
            perp_sym(),
            WallAnchor::new(MONO0, WALL0),
            [0; 32],
        )
        .expect("configure");
        let mut wall = WALL0;
        let mut i = 0usize;
        while i < 100 {
            m.on_tick(&tick(wall, 79_000_000_000, true), &mut ctx);
            wall += MINUTE_NS;
            i += 1;
        }
        assert_eq!(m.vol.minutes(), 0, "100 stale minutes fed nothing");
        // And a foreign sym is ignored outright.
        let other = make_symbol_id(VenueId::Deribit, 2);
        let mut t = tick(wall, 79_000_000_000, false);
        t.sym = other;
        m.on_tick(&t, &mut ctx);
        assert_eq!(m.vol.minutes(), 0);
    }
}
