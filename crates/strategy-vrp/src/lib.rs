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
//! E                   still holding? European CASH SETTLE at intrinsic
//! ```
//!
//! The settle rung (VX, operator ruling O-D4) is the last word on a
//! position, not an alternative exit. The member aims to be flat by
//! E − ε; the rung exists because "aims to" is not "is" — a submit ring
//! that stayed full, a data gap that swallowed the ε instant, an option
//! lane that went quiet. At expiry the instrument stops being tradeable
//! and becomes `max(0, S − K)` in cash, so the member books that and
//! nothing else. An expiry that passes with no records at all settles on
//! the first record after it, tick or summary, rather than being missed.
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
//! | an entry (or a growing hedge) over a risk-policy cap | refused | `caps_rejected` |
//! | kill criterion 3: a full trailing-60 window where the forecast stopped beating IV | HALT for the life of the process | `killed` |
//! | at expiry with no usable index | settlement DEFERRED to the next record | `stale_skips` |
//! | at expiry ITM with the contract rolled off the chain (restored campaign) | position closed, value NOT recorded — reconcile by hand | `settled_unpriced` |

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
use opt_registry::{OptRegistry, RIGHT_CALL, RIGHT_PUT};
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

// The caps come from `strategy_core::risk` — ONE table, read by every
// coded member, so a second order-submission path cannot quietly run to
// a different policy than the first. Deribit is SIZE-capped at one whole
// coin per order and per symbol (operator amendment 2026-09-10), which
// is the venue's own unit: a Deribit inverse contract IS a coin, so the
// cap does not move with the index.
//
// The BINDING check here is the hedge, not the option. At Δ = 1 a
// one-contract option's hedge is a one-coin perp order, so the member
// refuses an ENTRY whose WORST-CASE hedge would breach a cap rather
// than entering and clamping the hedge later: a half hedge is a naked
// option position wearing a hedged one's name.

/// VX: Deribit's settlement schedule, for the record — the member emits
/// the settling order and the harness's fill engine charges the fee, so
/// these numbers live in `cli::backtest::fill` and are repeated here
/// only so a reader of this crate knows what an expiry costs:
/// `min(0.00015 × index, 0.125 × settlement value)` per contract, and
/// **an option expiring out of the money is free** — which is why an OTM
/// expiry here emits no order at all rather than a zero-priced one.
///
/// `vrp-state.tsv` format version. Bump on any row-shape change.
///
/// **1 → 2 (W2, 2026-09-11):** added the `R` rows that carry the HAR's
/// rolling minute returns. Without them `minutes` restarted at zero on
/// every boot, and the restart lane fires five times a UTC day with a
/// longest gap of 7 h 35 m against a 24 h warm-up — so the forecast
/// could never exist and the member could never trade. That is not a
/// hypothetical: it is what the first live campaign did.
///
/// **2 → 3 (W6, 2026-09-11):** the `C` row gained an eighth field,
/// `entry_done`. It had been DERIVED from `opt_qty != 0`, so a campaign
/// that decided and HELD left nothing to restore and the next boot
/// decided it again. On 2026-09-11 that put on a live position at
/// 06:01Z against an 08:00Z expiry — a 1 h 56 m hold gated by an 8 h
/// forecast.
///
/// A reader accepts every version at or below its own and REFUSES
/// anything above it rather than guessing at rows it does not know. A
/// v2 `C` row (seven fields) still loads, with `entry_done` derived as
/// v2 meant it; a v1 file has no `R` rows and is simply a cold window,
/// which is what v1 always meant. An older binary refuses a newer file
/// outright, which is the point of the bump — a binary rolled back
/// below a change must not silently read state it cannot account for.
pub const VRP_STATE_VERSION: u32 = 3;

/// What a [`VrpStrategy::restore_state`] replay put back, for the boot
/// tell. An operator reading `pairs=0 campaign=false` after a restart
/// knows the member came up cold.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct VrpRestored {
    /// Fitted pairs replayed.
    pub pairs: usize,
    /// QLIKE observations replayed.
    pub qlike: usize,
    /// A campaign was carried across the restart.
    pub campaign: bool,
    /// That campaign's contract is still in the boot chain. `false`
    /// means the expiry has rolled off and the settle rung will close
    /// the position out at the first index it sees.
    pub campaign_resolved: bool,
    /// Kill criterion 3 was already armed before the restart.
    pub killed: bool,
    /// W2: `R` rows seen. They are validated here and replayed by the
    /// boot path, so this is what the FILE carried, not what the window
    /// ended up with — the merge with the worker's seed decides that.
    pub returns: usize,
    /// W2: the newest `R` row's minute, ms since the epoch.
    pub last_return_ts_ms: u64,
}

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
    /// The edge spec's measured configuration, at the size the risk
    /// policy permits: θ = 0.10, τ = 8 h, ε = 5 min, selection 10 min
    /// out, hourly rebalance, **one contract**, and a band of 10 % of
    /// one contract's delta at Δ = 0.5.
    ///
    /// One contract is one whole coin of underlying exposure, which is
    /// exactly the Deribit per-order cap after the operator's
    /// 2026-09-10 amendment ([`strategy_core::CAPS_DERIBIT`]). The
    /// worst-case hedge at Δ = 1 is therefore precisely at the line, not
    /// over it — a size above this refuses at the decision and is
    /// counted.
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
    /// Wall instant of that mid, so the settle rung can pick whichever
    /// of the two index sources is FRESHER.
    last_underlying_wall_ns: u64,

    /// The selected option for the current campaign.
    selected_sym: SymbolId,
    /// Expiry of the selected option, wall ns.
    expiry_ns: u64,
    /// Strike and right of the selected option, held HERE rather than
    /// looked up per use. Two reasons: it takes a registry lookup off
    /// the settle path, and it lets a campaign restored from persisted
    /// state settle even when the instrument itself is gone from the
    /// chain — which is exactly the case that state exists for.
    selected_strike_1e6: i64,
    selected_right: u8,
    /// The last mark seen for [`Self::selected_sym`].
    last_mark: OptMarkCache,

    /// Campaign state.
    side: i8,
    entry_done: bool,
    /// Kill criterion 3 fired: a FULL trailing-60 window in which the
    /// forecast no longer beat implied vol. E1 is the whole mechanism;
    /// without it there is nothing to harvest and premium does not make
    /// up for it, so once this is set the member never enters again.
    ///
    /// **Scope: this process.** The QLIKE window lives in
    /// [`core_vol::VolEngine`] and is zeroed by `new()`; the V5 boot seed
    /// restores the fitted pairs but NOT the window. With the standing
    /// restart cadence (`scripts/daily-restart.sh`, five slots a day)
    /// and an 8 h campaign, sixty settlements cannot accumulate inside
    /// one process — so as deployed today this flag cannot arm, and the
    /// halt is a control the lane does not yet have. Persisting the
    /// window across restarts is a named V8 precondition
    /// (`docs/risk-policy.md`, kill-switch trigger 7). The code is here,
    /// tested and correct, so that closing that gap is a seed change and
    /// not a strategy change.
    killed: bool,
    /// The campaign is unwinding: every record retries the flatten until
    /// the book is actually flat. Set by the E−ε law and by a
    /// hard-closed regime gate — a submit ring that was full must never
    /// leave a position behind with nothing coming back for it.
    /// Y1: no NEW hedge inside ε of expiry (see `maybe_freeze_hedge`).
    hedge_frozen: bool,
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
    /// V8a: bumped whenever persisted state changes — a campaign
    /// opening, a hedge moving, a settlement folding in, the kill
    /// arming. The cli watches it and rewrites `vrp-state.tsv`; it never
    /// has to guess when something happened.
    state_epoch: u64,
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
            last_underlying_wall_ns: 0,
            selected_sym: SYMBOL_ID_NONE,
            expiry_ns: 0,
            selected_strike_1e6: 0,
            selected_right: RIGHT_CALL,
            last_mark: OptMarkCache::default(),
            side: SIDE_FLAT,
            killed: false,
            entry_done: false,
            hedge_frozen: false,
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
            state_epoch: 0,
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

    /// [`Self::seed_pair`], carrying the expiry the pair came from so
    /// the member can write its own history back out (V8a).
    pub fn seed_pair_at(&mut self, expiry_ts_ms: u64, x_1e9: i64, y_1e9: i64) {
        self.vol.seed_pair_at(expiry_ts_ms, x_1e9, y_1e9);
    }

    /// W2: replay the merged rolling window into the forecast. Boot
    /// path, once, BEFORE any live minute close.
    ///
    /// `rows` must be chronological and contiguous — `cli::vrp_boot`
    /// guarantees both when it reconciles the worker's seed with the
    /// engine's own last state. Returns how many minutes went in.
    ///
    /// This is the whole of the W2 fix. Everything else in this commit
    /// exists to get a correct `rows` to this call.
    pub fn seed_returns(&mut self, rows: &[(u64, i64)]) -> usize {
        let mut i = 0usize;
        while i < rows.len() {
            let (ts, r) = rows[i];
            self.vol.seed_return(r, ts);
            i += 1;
        }
        rows.len()
    }

    /// W2: minutes in the rolling window right now.
    #[must_use]
    pub const fn vol_minutes(&self) -> u64 {
        self.vol.minutes()
    }

    /// W2: whether the HAR can forecast at all.
    ///
    /// False is the state this member spent its entire first live day
    /// in without ever saying so — `no_bounds` was the only tell, and it
    /// is indistinguishable from every other cause. Every boot reports
    /// this now.
    #[must_use]
    pub const fn vol_is_warm(&self) -> bool {
        self.vol.is_warm()
    }

    /// W2: minutes still needed before the forecast exists; `0` = warm.
    #[must_use]
    pub const fn vol_short_by(&self) -> u64 {
        core_vol::HAR_WARM_MINUTES.saturating_sub(self.vol.minutes())
    }

    /// W2: the minute of the newest return in the window, ms since the
    /// epoch; `0` when nothing stamped has been seen.
    #[must_use]
    pub const fn vol_last_min_ts_ms(&self) -> u64 {
        self.vol.last_min_ts_ms()
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

    /// Whether kill criterion 3 has halted the member.
    #[must_use]
    pub const fn is_killed(&self) -> bool {
        self.killed
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
    // V8a: persisted state
    // -----------------------------------------------------------

    #[inline]
    fn bump_state(&mut self) {
        self.state_epoch = self.state_epoch.wrapping_add(1);
    }

    /// Bumped whenever persisted state changes. The cli watches this and
    /// rewrites `vrp-state.tsv`; it never has to guess when something
    /// happened, and an unchanged epoch means an unchanged file.
    #[inline]
    #[must_use]
    pub const fn state_epoch(&self) -> u64 {
        self.state_epoch
    }

    /// Render the member's whole persisted state into `out`.
    ///
    /// **Why this exists.** Two things the member owns outlive a
    /// process: the QLIKE window kill criterion 3 is measured over
    /// (sixty settled expiries — twenty days of live running, across
    /// every restart in between), and an OPEN campaign. Without the
    /// first, the halt can never arm. Without the second, a restart in
    /// the middle of an 8 h hold orphans a live position: the next boot
    /// knows nothing about it, never hedges it again and never settles
    /// it.
    ///
    /// The campaign is keyed by `(expiry_ns, strike, right)` rather than
    /// by symbol, because Deribit option ordinals reshuffle at every
    /// boot — a persisted `SymbolId` would name a different instrument
    /// tomorrow. Those three fields name the contract itself.
    ///
    /// Cold path: called at most once per state change, off the tick
    /// loop, so a `String` is fine here and nowhere else in this crate.
    pub fn render_state(&self, out: &mut String) -> bool {
        use core::fmt::Write as _;
        if !self.configured {
            return false;
        }
        out.clear();
        out.push_str(
            "# vrp-state.tsv — written by the engine, read at boot. Not for hand editing.\n\
             # V version | P expiry_ts_ms x_1e9 y_1e9 | Q qlike_iv_1e9 qlike_har_1e9\n\
             # K killed | C expiry_ns strike_1e6 right side opt_qty_1e6 perp_qty_1e6 next_rebalance_ns\n\
             # R min_ts_ms r_1e9 (W2: the HAR's rolling window, oldest first)\n\
             # C gained an 8th field, entry_done, in v3 (W6).\n",
        );
        let _ = writeln!(out, "V\t{VRP_STATE_VERSION}");
        if self.killed {
            out.push_str("K\t1\n");
        }
        let mut i = 0usize;
        while let Some((ts, x, y)) = self.vol.pair_at(i) {
            let _ = writeln!(out, "P\t{ts}\t{x}\t{y}");
            i += 1;
        }
        i = 0;
        while let Some((iv, har)) = self.vol.qlike_at(i) {
            let _ = writeln!(out, "Q\t{iv}\t{har}");
            i += 1;
        }
        // W2: the rolling window, oldest first — the order
        // `seed_return` needs them back in. Unstamped returns (the
        // parity fixture, a unit test) are skipped rather than written
        // with a 0 stamp: a row the merge cannot place in time is worse
        // than one fewer minute.
        let mut m = 0usize;
        let n_ret = self.vol.n_returns();
        let last_ts = self.vol.last_min_ts_ms();
        if last_ts > 0 {
            let oldest_ts = last_ts.saturating_sub((n_ret.saturating_sub(1)) as u64 * 60_000);
            while m < n_ret {
                if let Some(r) = self.vol.ret_chrono(m) {
                    let _ = writeln!(out, "R\t{}\t{r}", oldest_ts + m as u64 * 60_000);
                }
                m += 1;
            }
        }
        if self.selected_sym != SYMBOL_ID_NONE {
            let _ = writeln!(
                out,
                "C\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                self.expiry_ns,
                self.selected_strike_1e6,
                self.selected_right,
                self.side,
                self.opt_pos_qty_1e6,
                self.perp_pos_qty_1e6,
                self.next_rebalance_wall_ns,
                u8::from(self.entry_done)
            );
        }
        true
    }

    /// Replay a state file rendered by [`Self::render_state`].
    ///
    /// Boot path — call after [`Self::configure`] and before
    /// `on_start`. Fail-closed: an unknown tag, a bad version, a
    /// malformed field or a campaign the registry cannot place is an
    /// error, and the caller refuses the boot. A state file the engine
    /// cannot read EXACTLY is a position nobody is tracking.
    ///
    /// A campaign whose instrument is no longer in the chain is still
    /// restored — `selected_sym` stays [`SYMBOL_ID_NONE`] and the
    /// settle rung, which reads the persisted strike and right rather
    /// than the registry, closes it out at the first index. That is the
    /// engine-down-across-expiry path, and it is the whole reason the
    /// campaign is keyed by contract and not by symbol.
    pub fn restore_state(&mut self, text: &str) -> Result<VrpRestored, &'static str> {
        if !self.configured {
            return Err("vrp: restore before configure");
        }
        let mut seen_version = false;
        let mut st = VrpRestored::default();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut f = line.split('\t');
            let tag = f.next().unwrap_or("");
            let num = |t: Option<&str>| -> Result<i64, &'static str> {
                t.ok_or("vrp state: short row")?
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| "vrp state: not an integer")
            };
            match tag {
                "V" => {
                    // Accept anything this binary fully understands and
                    // refuse what it does not. A v1 file has no `R`
                    // rows, which is exactly the cold window v1 always
                    // meant — readable. A v3 file has rows this code has
                    // never seen, and reading it would mean guessing.
                    let v = num(f.next())?;
                    if v < 1 || v > i64::from(VRP_STATE_VERSION) {
                        return Err("vrp state: unsupported version");
                    }
                    seen_version = true;
                }
                // W2: validated and COUNTED here, replayed by the boot
                // path. The rolling window is the one thing with two
                // sources — this file and the worker's seed — and
                // reconciling them needs both series in hand before any
                // of them is pushed, which a line-at-a-time parser
                // cannot do. `cli::vrp_boot` owns that merge.
                "R" => {
                    let ts = num(f.next())?;
                    let _r = num(f.next())?;
                    if ts <= 0 {
                        return Err("vrp state: return min_ts_ms must be > 0");
                    }
                    if ts <= st.last_return_ts_ms as i64 && st.last_return_ts_ms != 0 {
                        return Err("vrp state: return rows must increase in min_ts_ms");
                    }
                    st.last_return_ts_ms = ts as u64;
                    st.returns += 1;
                }
                "K" => {
                    if num(f.next())? != 0 {
                        self.killed = true;
                        self.counters.killed = 1;
                        st.killed = true;
                    }
                }
                "P" => {
                    let ts = num(f.next())?;
                    let x = num(f.next())?;
                    let y = num(f.next())?;
                    if ts < 0 {
                        return Err("vrp state: negative expiry stamp");
                    }
                    self.vol.seed_pair_at(ts as u64, x, y);
                    st.pairs += 1;
                }
                "Q" => {
                    let iv = num(f.next())?;
                    let har = num(f.next())?;
                    self.vol.seed_qlike(iv, har);
                    st.qlike += 1;
                }
                "C" => {
                    let expiry_ns = num(f.next())?;
                    let strike = num(f.next())?;
                    let right = num(f.next())?;
                    let side = num(f.next())?;
                    let opt_qty = num(f.next())?;
                    let perp_qty = num(f.next())?;
                    let next_reb = num(f.next())?;
                    // W6: eighth field. ABSENT on a v2 row, where it
                    // was derived — and that derivation is exactly the
                    // defect, because a campaign that decided and HELD
                    // has no position to derive it from.
                    let entry_done = match f.next() {
                        Some(s) => {
                            s.trim()
                                .parse::<i64>()
                                .map_err(|_| "vrp state: not an integer")?
                                != 0
                        }
                        None => opt_qty != 0,
                    };
                    if expiry_ns <= 0 || strike <= 0 {
                        return Err("vrp state: campaign expiry/strike must be positive");
                    }
                    if right != i64::from(RIGHT_CALL) && right != i64::from(RIGHT_PUT) {
                        return Err("vrp state: campaign right is neither call nor put");
                    }
                    if !(SIDE_SHORT_VOL..=SIDE_LONG_VOL).contains(&(side as i8)) {
                        return Err("vrp state: campaign side out of range");
                    }
                    self.expiry_ns = expiry_ns as u64;
                    self.selected_strike_1e6 = strike;
                    self.selected_right = right as u8;
                    self.side = side as i8;
                    self.opt_pos_qty_1e6 = opt_qty;
                    self.perp_pos_qty_1e6 = perp_qty;
                    self.next_rebalance_wall_ns = next_reb.max(0) as u64;
                    self.entry_done = entry_done;
                    self.selected_sym = self.find_contract(self.expiry_ns, strike, right as u8);
                    st.campaign = true;
                    st.campaign_resolved = self.selected_sym != SYMBOL_ID_NONE;
                }
                _ => return Err("vrp state: unknown row tag"),
            }
        }
        if !seen_version {
            return Err("vrp state: no version row");
        }
        self.refresh_qlike();
        Ok(st)
    }

    /// The chain row for a contract, by the three fields that name it
    /// across boots. `SYMBOL_ID_NONE` when the chain no longer carries
    /// it — an expiry that has already rolled off.
    fn find_contract(&self, expiry_ns: u64, strike_1e6: i64, right: u8) -> SymbolId {
        for row in self.registry.rows() {
            if row.expiry_ns == expiry_ns && row.strike_1e6 == strike_1e6 && row.right == right {
                return row.sym;
            }
        }
        SYMBOL_ID_NONE
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

    /// USD ×1e6 notional of `qty_1e6` units at `px_1e6`. `i128`
    /// intermediate, saturating — a notional that cannot be represented
    /// is treated as infinite, which refuses rather than admits.
    #[inline]
    #[must_use]
    pub fn notional_1e6(px_1e6: i64, qty_1e6: i64) -> i64 {
        let n = (px_1e6 as i128 * qty_1e6.unsigned_abs() as i128) / 1_000_000;
        i64::try_from(n).unwrap_or(i64::MAX)
    }

    /// Whether one order of `qty_1e6` units and `notional_1e6` dollars
    /// is inside `caps`, in whichever unit that venue is capped in.
    ///
    /// A venue capped in the OTHER unit refuses — a `0` there means
    /// "this unit does not apply here", never "unlimited", and reading
    /// it as unlimited is the one mistake this shape exists to prevent.
    #[inline]
    #[must_use]
    pub fn size_ok(caps: strategy_core::VenueCaps, qty_1e6: i64, notional_1e6: i64) -> bool {
        let q = qty_1e6.unsigned_abs();
        if caps.leg_qty_1e6 > 0 {
            return q <= caps.leg_qty_1e6.unsigned_abs()
                && q <= caps.sym_qty_1e6.unsigned_abs();
        }
        if caps.leg_usd_1e6 > 0 {
            return notional_1e6 <= caps.leg_usd_1e6 && notional_1e6 <= caps.sym_usd_1e6;
        }
        false
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
        // Belt and braces against a corrupt frame. The entry gate proved
        // this position is legal at Δ = 1, so these can only bind on a
        // |delta| > 1 the venue should never send — but `delta_1e9` is a
        // saturating `i32` off the wire with no range check anywhere in
        // the tree, so "should never" is not a guard.
        //
        let caps = strategy_core::caps_for_sym(self.hedge_sym);
        // The SINGLE-ORDER cap applies to the ORDER, unconditionally.
        // Sizing it off the position's magnitude was the hole: a move
        // from +0.1 to −0.09 shrinks the position and still emits a
        // 0.19-coin order. A genuine reduction can never exceed the
        // position it unwinds, so an unconditional test on `delta`
        // cannot block an exit.
        if !Self::size_ok(caps, delta, Self::notional_1e6(px, delta)) {
            self.counters.caps_rejected = self.counters.caps_rejected.wrapping_add(1);
            return false;
        }
        // The per-symbol and table caps bound the RESULTING position, so
        // they apply only to a move that grows it — a cap must never be
        // the reason a position cannot be closed (the ICDP precedent,
        // where `exit_position` is exempt).
        let grows = target_1e6.unsigned_abs() > self.perp_pos_qty_1e6.unsigned_abs();
        if grows {
            let hedge_notional = Self::notional_1e6(px, target_1e6);
            let opt_notional = Self::notional_1e6(self.last_mark.px_usd_1e6, self.opt_pos_qty_1e6);
            if !Self::size_ok(caps, target_1e6, hedge_notional)
                || hedge_notional.saturating_add(opt_notional) > caps.table_usd_1e6
            {
                self.counters.caps_rejected = self.counters.caps_rejected.wrapping_add(1);
                return false;
            }
        }
        let Some(order) = self.ioc(self.hedge_sym, px, delta, now) else {
            return false;
        };
        if !self.submit(ctx, order) {
            return false;
        }
        self.perp_pos_qty_1e6 = target_1e6;
        self.counters.hedges = self.counters.hedges.wrapping_add(1);
        self.bump_state();
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
        self.selected_strike_1e6 = 0;
        self.selected_right = RIGHT_CALL;
        self.side = SIDE_FLAT;
        self.entry_done = false;
        self.hedge_frozen = false;
        self.last_mark = OptMarkCache::default();
        self.next_rebalance_wall_ns = 0;
        self.bump_state();
    }

    /// Unwind both legs at the last known marks.
    /// Y1: close BOTH legs now, whatever it costs. The RISK exit, and
    /// the only path that still crosses the option spread to get out.
    ///
    /// This is not the campaign's economic exit — that is settlement
    /// (`maybe_freeze_hedge`). This runs when the regime gate slams
    /// shut, where the question is not "is this the best price" but
    /// "get out". Paying the spread twice is the correct price of an
    /// unplanned exit; the whole X0 finding is that paying it on a
    /// PLANNED one destroys the edge.
    ///
    /// Returns true once both legs are flat.
    fn flatten<C: Ctx>(&mut self, ctx: &mut C, now: NsTs) -> bool {
        if self.opt_pos_qty_1e6 != 0 && self.last_mark.px_usd_1e6 > 0 {
            let closing = -self.opt_pos_qty_1e6;
            let px = self.last_mark.px_usd_1e6;
            let sym = self.selected_sym;
            if let Some(order) = self.ioc(sym, px, closing, now) {
                if self.submit(ctx, order) {
                    self.opt_pos_qty_1e6 = 0;
                    self.counters.exits = self.counters.exits.wrapping_add(1);
                    self.bump_state();
                }
            }
        }
        if self.perp_pos_qty_1e6 != 0 {
            self.move_hedge(ctx, 0, now);
        }
        self.opt_pos_qty_1e6 == 0 && self.perp_pos_qty_1e6 == 0
    }

    /// Y1: freeze the hedge ε before expiry. NOT an exit — settlement is
    /// the only way a campaign ends now.
    ///
    /// **Why the E−ε unwind is gone.** It bought the option back five
    /// minutes before expiry, crossing the option spread a SECOND time.
    /// The lane plan's §3.4 ruled that acceptable — *"economically
    /// near-identical to cash settlement"* — and at the 0 % spread it
    /// then assumed, it was: −0.73 bps. V2a measured the real book the
    /// next day (near-ATM 4–12 h calls, median 25 % crossed) and that
    /// gap became the whole edge. Re-measured 2026-09-11 at the ATM
    /// rung, per expiry:
    ///
    /// ```text
    ///   settle    +3.658 bps   (t +1.67)
    ///   fair_eps  −2.198 bps   (t −0.79)   <- what this rung used to do
    /// ```
    ///
    /// Holding to expiry crosses the spread ONCE. The venue settles an
    /// ITM option at intrinsic and an OTM one at nothing, and
    /// [`Self::maybe_settle`] already books both.
    ///
    /// The hedge still runs to expiry — that is what the measured arm
    /// does — but no NEW hedge goes on inside ε, because a hedge put on
    /// in the last minutes cannot be unwound before settlement and
    /// delta is least stable exactly there. The measured arm never
    /// rebalances that late either (the schedule is hourly), so this
    /// changes no economics; it is a rail, not a policy.
    fn maybe_freeze_hedge(&mut self, wall_ns: u64) {
        if self.selected_sym == SYMBOL_ID_NONE || self.hedge_frozen {
            return;
        }
        if wall_ns + self.params.epsilon_ns < self.expiry_ns {
            return;
        }
        self.hedge_frozen = true;
        self.bump_state();
    }

    /// W6: the last instant at which this campaign's decision is still
    /// the trade the forecast describes. `E − τ + selection`.
    #[inline]
    #[must_use]
    fn entry_deadline_ns(&self) -> u64 {
        self.expiry_ns
            .saturating_sub(self.params.tau_ns)
            .saturating_add(self.params.selection_ns)
    }

    /// The best index this member can stand behind right now: whichever
    /// of its two sources is FRESHER — the last option record's own
    /// `underlying_px_1e9`, or the last fresh mid of the underlying's
    /// tick lane.
    ///
    /// Freshness, not preference: the option lane can go quiet for hours
    /// while the perp keeps printing, and settling an expiry against an
    /// eight-hour-old forward would book a payoff the option did not
    /// have. `None` when there is neither, at which point the member
    /// defers rather than settling against a number it made up.
    #[inline]
    fn settle_index_1e6(&self) -> Option<i64> {
        let from_opt = (self.last_mark.underlying_px_1e9 > 0)
            .then_some((self.last_mark.wall_ns, self.last_mark.underlying_px_1e9 / 1_000));
        let from_tick = (self.last_underlying_mid_1e6 > 0)
            .then_some((self.last_underlying_wall_ns, self.last_underlying_mid_1e6));
        match (from_opt, from_tick) {
            (Some((wo, po)), Some((wt, pt))) => Some(if wt >= wo { pt } else { po }),
            (Some((_, po)), None) => Some(po),
            (None, Some((_, pt))) => Some(pt),
            (None, None) => None,
        }
    }

    /// European cash settlement value of one unit at index `s_1e6`:
    /// `max(0, S − K)` for a call, `max(0, K − S)` for a put.
    #[inline]
    #[must_use]
    pub const fn intrinsic_1e6(s_1e6: i64, strike_1e6: i64, right: u8) -> i64 {
        let v = if right == RIGHT_CALL {
            s_1e6 - strike_1e6
        } else {
            strike_1e6 - s_1e6
        };
        if v > 0 {
            v
        } else {
            0
        }
    }

    /// VX: the European cash settle. Returns true when the campaign
    /// ended here.
    ///
    /// Runs on ANY callback, so an expiry that passed while no record
    /// arrived settles on the first one after it. It runs BEFORE the
    /// E−ε law on the same instant, because at or after expiry there is
    /// no mark to unwind against — the instrument is cash now.
    fn maybe_settle<C: Ctx>(&mut self, ctx: &mut C, wall_ns: u64, now: NsTs) -> bool {
        // Keyed on the EXPIRY, not the symbol: a campaign restored
        // across a restart may have no symbol at all, because Deribit
        // drops an instrument from the chain once it expires — which is
        // exactly the case this rung exists for.
        if self.expiry_ns == 0 || wall_ns < self.expiry_ns {
            return false;
        }
        // Nothing held: the E−ε law already did its job, so expiry is
        // just the end of the campaign.
        if self.opt_pos_qty_1e6 == 0 && self.perp_pos_qty_1e6 == 0 {
            self.settle_forecast();
            self.end_campaign();
            return true;
        }
        let Some(s_1e6) = self.settle_index_1e6() else {
            // No index we can stand behind. DEFER — a settlement priced
            // off a number the member invented is worse than a late one.
            self.counters.stale_skips = self.counters.stale_skips.wrapping_add(1);
            return false;
        };
        let value_1e6 =
            Self::intrinsic_1e6(s_1e6, self.selected_strike_1e6, self.selected_right);
        if self.opt_pos_qty_1e6 != 0 {
            if self.selected_sym == SYMBOL_ID_NONE {
                // The contract has rolled off the chain, so there is no
                // symbol to submit against — and submitting against the
                // PERSISTED one would be worse than not submitting:
                // Deribit option ordinals reshuffle at every boot, so
                // that number names a different instrument today.
                //
                // Close the position out of our own book and count it as
                // what it is: an expiry whose value we could not record.
                // An operator reconciles it by hand. OTM needs no order,
                // so this only ever bites in the money.
                self.opt_pos_qty_1e6 = 0;
                self.counters.settled_unpriced =
                    self.counters.settled_unpriced.wrapping_add(1);
                self.bump_state();
            } else if value_1e6 > 0 {
                // ITM: book the intrinsic. A closing order at the
                // settlement price is how a paper member says "this
                // position became cash at this value".
                let closing = -self.opt_pos_qty_1e6;
                let sym = self.selected_sym;
                let Some(order) = self.ioc(sym, value_1e6, closing, now) else {
                    return false;
                };
                if !self.submit(ctx, order) {
                    return false; // ring full — retry on the next record
                }
                self.opt_pos_qty_1e6 = 0;
                self.counters.settled_itm = self.counters.settled_itm.wrapping_add(1);
                self.bump_state();
            } else {
                // OTM: the option is worth nothing and the venue charges
                // nothing. There is no order to place — a zero-priced
                // one would be a fiction, and a mark-priced one would
                // book value that expired.
                self.opt_pos_qty_1e6 = 0;
                self.counters.settled_otm = self.counters.settled_otm.wrapping_add(1);
                self.bump_state();
            }
        }
        if self.perp_pos_qty_1e6 != 0 {
            self.last_mark.underlying_px_1e9 = s_1e6.saturating_mul(1_000);
            if !self.move_hedge(ctx, 0, now) {
                return false; // the hedge still stands; retry
            }
        }
        self.settle_forecast();
        self.end_campaign();
        true
    }

    /// Fold a finished hold back into the forecast. Shared by the E−ε
    /// exit and the settle rung so a campaign contributes exactly once
    /// however it ended.
    fn settle_forecast(&mut self) {
        if !self.vol.is_armed() {
            return;
        }
        if let Some(rv) = self.realised_rv_1e9() {
            self.vol.observe_settlement(rv);
            self.counters.settlements = self.counters.settlements.wrapping_add(1);
            self.refresh_qlike();
            self.bump_state();
        }
    }

    /// Realised vol over the hold that just ended, raw bps ×1e9 — the
    /// forecast's own `rv` over τ, read straight off the ring so the
    /// `y` this member forms is the same quantity the seed cutter forms.
    #[inline]
    fn realised_rv_1e9(&self) -> Option<i64> {
        self.vol.har_1e9(self.params.tau_ns)
    }

    /// Refresh the kill-criterion-3 tell, and ARM the halt when it
    /// fires.
    ///
    /// Edge spec §5.3: "HAR no longer beats IV out of sample at 4–8 h
    /// over a trailing 60 expiries ⇒ halt". Not "warn", not "put a
    /// number on a dashboard" — halt. `har_beats_iv` is false while the
    /// window is still filling, so the halt is armed only on a FULL
    /// window; once armed it holds for the life of the process (see
    /// [`VrpStrategy::killed`] for why that is not yet the same thing as
    /// sticky, and what closes the gap).
    #[inline]
    fn refresh_qlike(&mut self) {
        let q = self.vol.qlike_counters();
        self.counters.qlike_iv_1e6 = q.iv_mean_1e9 / 1_000;
        self.counters.qlike_har_1e6 = q.har_mean_1e9 / 1_000;
        self.counters.qlike_har_beats_iv = u64::from(q.har_beats_iv);
        if q.n as usize >= core_vol::QLIKE_RING && !q.har_beats_iv {
            self.killed = true;
            self.counters.killed = 1;
            self.bump_state();
        }
    }

    /// The selection law: at the first summary inside the selection
    /// window, the nearest-strike CALL of the nearest expiry still ahead
    /// of us. Nearest strike to the record's own underlying reference —
    /// the venue's number, not one we derived.
    fn select<C: Ctx>(&mut self, _ctx: &mut C, wall_ns: u64, underlying_px_1e6: i64) {
        let open_from = self.params.tau_ns + self.params.selection_ns;
        let mut best: Option<(SymbolId, u64, i64)> = None;
        // Whether ANY expiry is inside the selection window right now.
        // For ~23 h 50 m of every day none is, and that is the member
        // working — not a failure to select. Counting a "no selection"
        // on every option record in between would run the counter into
        // the millions and bury the one case that matters: an expiry
        // was due and nothing in the chain was tradeable for us.
        let mut any_due = false;
        for row in self.registry.rows() {
            if row.expiry_ns <= wall_ns {
                continue;
            }
            let lead = row.expiry_ns - wall_ns;
            if lead > open_from || lead < self.params.tau_ns {
                continue;
            }
            // An expiry IS due. Everything below is about whether THIS
            // instrument is one we may trade.
            any_due = true;
            // THE UNDERLYING FILTER, and it is load-bearing. The live
            // Deribit ladder is on for BTC **and** ETH
            // (`options_underlyings`), so without this the member can
            // select an ETH call and hedge it with the BTC perp — a
            // cross-asset naked position that every other check in this
            // crate would wave through, because every other check is
            // about size, staleness or timing. `underlying_sym` is the
            // instrument's OWN hedge leg, set per row at boot.
            //
            // The evidence covers BTC only (edge spec §7: no ETH), so
            // this is also where that restriction is enforced rather
            // than assumed.
            if row.underlying_sym != self.hedge_sym || row.right != RIGHT_CALL {
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
                if let Some(row) = self.registry.get(sym) {
                    self.selected_strike_1e6 = row.strike_1e6;
                    self.selected_right = row.right;
                }
                self.entry_done = false;
                self.hedge_frozen = false;
                self.bump_state();
            }
            // An expiry was due and nothing in the chain was tradeable
            // for us — the chain rolled without our currency, or it
            // carries no calls at that expiry. THAT is worth a counter.
            None if any_due => {
                self.counters.no_selection = self.counters.no_selection.wrapping_add(1);
            }
            // No expiry due. The normal state, all day.
            None => {}
        }
    }

    /// The two-compare decision at `E − τ`, and the entry it authorises.
    fn decide<C: Ctx>(&mut self, ctx: &mut C, wall_ns: u64, now: NsTs) {
        if self.entry_done || self.hedge_frozen || self.selected_sym == SYMBOL_ID_NONE {
            return;
        }
        if wall_ns + self.params.tau_ns < self.expiry_ns {
            return; // not yet at the entry instant
        }
        // W6: and not LATE. `τ` is not a start line, it is the HORIZON
        // — `bounds` is a variance forecast for a τ-long hold and the
        // edge was measured on one. Coming back from a restart with two
        // hours left and entering on an eight-hour forecast is a
        // different trade wearing the same gate.
        //
        // The grace is the selection window, so the decision band
        // `[E−τ, E−τ+selection]` mirrors the selection band that ends
        // at `E−τ`. A restart takes 10–40 s; ten minutes is room for
        // several.
        if wall_ns > self.entry_deadline_ns() {
            // The campaign's one decision is SPENT, not deferred: left
            // undecided it would be retried on every record until
            // expiry, and each retry is the same wrong trade.
            self.entry_done = true;
            self.counters.decisions_late = self.counters.decisions_late.wrapping_add(1);
            self.bump_state();
            return;
        }
        // One decision per campaign, whatever it decides.
        self.entry_done = true;
        self.counters.decisions = self.counters.decisions.wrapping_add(1);

        if self.killed {
            // Kill criterion 3, sticky. No counter here: `killed` is
            // already 1 and the entries counter simply stops moving.
            return;
        }
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
        let qty = self.params.qty_1e6 * side as i64;
        // The policy gate, at the ONE instant that can grow the book.
        // Worst case, not current case: the hedge is sized off delta,
        // and delta walks to 1 as the option goes in the money, so an
        // entry is legal only if the position it commits us to is still
        // legal at Δ = 1 — where the hedge is exactly `qty_1e6` coins.
        let underlying_1e6 = self.last_mark.underlying_px_1e9 / 1_000;
        let opt_notional = Self::notional_1e6(self.last_mark.px_usd_1e6, qty);
        let hedge_worst_1e6 = Self::notional_1e6(underlying_1e6, self.params.qty_1e6);
        let opt_caps = strategy_core::caps_for_sym(self.selected_sym);
        let hedge_caps = strategy_core::caps_for_sym(self.hedge_sym);
        if !Self::size_ok(opt_caps, self.params.qty_1e6, opt_notional)
            || !Self::size_ok(hedge_caps, self.params.qty_1e6, hedge_worst_1e6)
            || opt_notional.saturating_add(hedge_worst_1e6) > hedge_caps.table_usd_1e6
        {
            self.counters.caps_rejected = self.counters.caps_rejected.wrapping_add(1);
            return;
        }
        // Arm only once the trade is authorised: an armed forecast with
        // no position would pair an x with a hold that never happened.
        let sym = self.selected_sym;
        let px = self.last_mark.px_usd_1e6;
        let Some(order) = self.ioc(sym, px, qty, now) else {
            return;
        };
        // The regressor must be formed from minutes strictly BEFORE the
        // hold, so arm before the position exists — but only once the
        // trade is authorised, or an armed forecast would pair an `x`
        // with a hold that never happened.
        if self
            .vol
            .arm_hold_at(self.expiry_ns / 1_000_000, self.params.tau_ns, iv)
            .is_none()
        {
            self.counters.no_bounds = self.counters.no_bounds.wrapping_add(1);
            return;
        }
        if !self.submit(ctx, order) {
            return;
        }
        self.side = side;
        self.opt_pos_qty_1e6 = qty;
        self.counters.entries = self.counters.entries.wrapping_add(1);
        self.bump_state();
        self.next_rebalance_wall_ns = wall_ns + self.params.rebalance_ns;
        // The first hedge goes out on the same instant as the entry.
        let target = Self::hedge_target_1e6(self.opt_pos_qty_1e6, self.last_mark.delta_1e9);
        self.move_hedge(ctx, target, now);
    }

    /// The hourly hedge check.
    fn maybe_rebalance<C: Ctx>(&mut self, ctx: &mut C, wall_ns: u64, now: NsTs) {
        if self.opt_pos_qty_1e6 == 0 || self.hedge_frozen || wall_ns < self.next_rebalance_wall_ns {
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
    #[inline]
    fn vrp_state_epoch(&self) -> u64 {
        self.state_epoch
    }
    #[inline]
    fn render_vrp_state(&self, out: &mut String) -> bool {
        self.render_state(out)
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
                self.last_underlying_wall_ns = wall_ns;
            } else if minute != self.minute_id {
                // The CLOSE of a minute is its last mid, so the roll
                // publishes the value carried across the boundary, not
                // the first quote of the new minute.
                // W2: stamped, so the close can be written to
                // `vrp-state.tsv` and lined up against the worker's
                // seed at the next boot. `minute_id` is the minute
                // that just CLOSED, not the one opening.
                self.vol.on_minute_close_at(
                    self.last_underlying_mid_1e6,
                    self.minute_id.saturating_mul(MINUTE_NS / 1_000_000),
                );
                self.minute_id = minute;
                // W2: the window CHANGED, so the state file has to be
                // rewritten. Without this the `R` rows would only ever
                // be persisted when a campaign happened to move the
                // epoch — which is a few times a day at most, and would
                // have left the whole fix inert. Once a minute, on the
                // observability cadence, off the tick path.
                self.bump_state();
            }
            self.last_underlying_mid_1e6 = mid;
            self.last_underlying_wall_ns = wall_ns;
        }
        if self.maybe_settle(ctx, wall_ns, now) {
            return;
        }
        self.maybe_freeze_hedge(wall_ns);
        self.maybe_rebalance(ctx, wall_ns, now);
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

        // `expiry_ns`, not the symbol: a restored campaign whose
        // contract has rolled off the chain still OWNS the member until
        // the settle rung closes it, and starting a new campaign on top
        // of it would lose the old position.
        if self.expiry_ns == 0 {
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
        if self.maybe_settle(ctx, wall_ns, now) {
            return;
        }
        self.decide(ctx, wall_ns, now);
        self.maybe_freeze_hedge(wall_ns);
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
        self.hedge_frozen = true;
        self.counters.regime_exits = self.counters.regime_exits.wrapping_add(1);
        if self.selected_sym != SYMBOL_ID_NONE && self.flatten(ctx, now) {
            // Flat: fold the hold back into the forecast and close the
            // campaign. Not flat (the submit ring was full) — the next
            // record retries, exactly as the old exit rung did.
            self.settle_forecast();
            self.end_campaign();
        }
        let _ = wall_ns;
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
        registry_with_foreign(false)
    }

    /// The same chain, optionally with another currency's options mixed
    /// in — an ETH ladder at ETH strikes and an EARLIER expiry, which is
    /// what `universe.toml`'s `options_underlyings = ["BTC", "ETH"]`
    /// actually produces.
    fn registry_with_foreign(foreign: bool) -> OptRegistry {
        let mut r = OptRegistry::new();
        let perp = perp_sym();
        if foreign {
            // Nearest expiry wins before nearest strike, so an ETH call
            // expiring SOONER beats every BTC row on the first tie-break
            // — this is the row that would have been selected.
            let eth_perp = make_symbol_id(VenueId::Deribit, 2);
            let mut j = 0u32;
            while j < 4 {
                r.insert(OptInstrument::new(
                    opt_sym(40 + j),
                    eth_perp,
                    VenueId::Deribit as u8,
                    // Five minutes earlier — INSIDE the selection
                    // window, so the TAU/selection filters let it
                    // through and only the underlying filter stops it.
                    EXPIRY - 300_000_000_000,
                    (3_000 + 100 * j as i64) * 1_000_000,
                    RIGHT_CALL,
                    1_000_000_000,
                ))
                .expect("eth chain");
                j += 1;
            }
        }
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

        // --- E−ε freezes the hedge; it does NOT exit (Y1) ---
        ctx.orders.clear();
        let eps_wall = EXPIRY - params.epsilon_ns;
        ctx.now = mono_of(eps_wall);
        m.on_tick(&tick(eps_wall, 79_000_000_000, false), &mut ctx);
        assert!(ctx.orders.is_empty(), "E−ε trades NOTHING now");
        assert_eq!(m.opt_pos_qty_1e6(), -params.qty_1e6, "still short into expiry");
        assert_ne!(m.selected_sym(), SYMBOL_ID_NONE, "campaign still open");

        // --- settlement is the terminal rung ---
        // Well above the ATM strike, so the short call settles IN the
        // money and the venue's cash settlement is a real number.
        let itm = 90_000_000_000i64;
        ctx.now = mono_of(EXPIRY);
        m.on_tick(&tick(EXPIRY, itm, false), &mut ctx);
        assert_eq!(m.opt_pos_qty_1e6(), 0, "the option became cash");
        assert_eq!(m.perp_pos_qty_1e6(), 0, "and the hedge came off");
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE, "the campaign ended");
        assert_eq!(m.vrp_counters().settled_itm, 1, "settled, not unwound");
        assert_eq!(
            m.vrp_counters().exits,
            0,
            "`exits` is the RISK exit only — a planned campaign never crosses \
             the option spread to get out"
        );
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

    // ---------------- the risk-policy caps ----------------

    #[test]
    fn the_notional_helper_saturates_rather_than_wraps() {
        assert_eq!(VrpStrategy::notional_1e6(300_200_000, 100_000), 30_020_000);
        assert_eq!(VrpStrategy::notional_1e6(79_000_000_000, 100_000), 7_900_000_000);
        // Sign of the quantity is irrelevant — a short is as much
        // notional as a long.
        assert_eq!(
            VrpStrategy::notional_1e6(79_000_000_000, -100_000),
            7_900_000_000
        );
        // A notional that cannot be represented reads as infinite, so a
        // cap test refuses rather than admits.
        assert_eq!(VrpStrategy::notional_1e6(i64::MAX, i64::MAX), i64::MAX);
    }

    #[test]
    fn the_default_size_is_exactly_the_deribit_cap() {
        // One contract = one whole coin = exactly the Deribit per-order
        // and per-symbol cap (operator amendment 2026-09-10). The cap is
        // in the venue's own unit, so this holds at any index — which is
        // the point of capping a coin-denominated instrument in coins.
        let p = VrpParams::default();
        assert_eq!(p.qty_1e6, 1_000_000);
        let caps = strategy_core::caps_for_venue(VenueId::Deribit as u8);
        assert_eq!(caps.leg_qty_1e6, 1_000_000);
        assert_eq!(caps.sym_qty_1e6, 1_000_000);
        assert_eq!(caps.leg_usd_1e6, 0, "Deribit is size-capped, not notional-capped");
        for idx in [50_000_000_000i64, 79_000_000_000, 250_000_000_000] {
            let n = VrpStrategy::notional_1e6(idx, p.qty_1e6);
            assert!(VrpStrategy::size_ok(caps, p.qty_1e6, n), "one coin fits at ${idx}");
            assert!(
                !VrpStrategy::size_ok(caps, p.qty_1e6 + 1, n),
                "a hair over one coin does not, at ${idx}"
            );
        }
        // Every other venue keeps the base tier, in dollars.
        let base = strategy_core::caps_for_venue(VenueId::Binance as u8);
        assert_eq!(base.leg_usd_1e6, 10_000_000_000);
        assert_eq!(base.leg_qty_1e6, 0, "size-capping is Deribit-only");
        assert!(VrpStrategy::size_ok(base, 999, 10_000_000_000));
        assert!(!VrpStrategy::size_ok(base, 999, 10_000_000_001));
    }

    #[test]
    fn a_venue_capped_in_the_other_unit_refuses() {
        // `0` means "this unit does not apply here", never "unlimited".
        let neither = strategy_core::VenueCaps {
            leg_usd_1e6: 0,
            leg_qty_1e6: 0,
            sym_usd_1e6: 0,
            sym_qty_1e6: 0,
            table_usd_1e6: 0,
        };
        assert!(!VrpStrategy::size_ok(neither, 1, 1));
        assert!(!VrpStrategy::size_ok(neither, 0, 0));
    }

    #[test]
    fn an_entry_whose_worst_case_hedge_breaches_a_cap_is_refused() {
        // The gate is on the WORST case, not the current one: delta
        // walks to 1 as the option goes in the money, so an entry is
        // legal only if the position is still legal there. A half hedge
        // is a naked option wearing a hedged one's name.
        let mut ctx = RecCtx::new();
        let params = VrpParams {
            qty_1e6: 1_500_000, // one and a half coins — over the cap
            band_qty_1e6: 50_000,
            ..VrpParams::default()
        };
        let (mut m, _) = member(&mut ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        // A quoted IV far above the band: the decision WANTS to trade.
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.vrp_counters().decisions, 1, "the decision ran");
        assert_eq!(m.vrp_counters().caps_rejected, 1);
        assert_eq!(m.vrp_counters().entries, 0);
        assert_eq!(m.side(), SIDE_FLAT);
        assert!(ctx.orders.is_empty(), "nothing reached the dispatcher");
        // And nothing was armed: an armed forecast with no position
        // would pair an x with a hold that never happened.
        assert!(!m.vol.is_armed());
    }

    #[test]
    fn the_default_size_passes_the_same_gate() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.vrp_counters().caps_rejected, 0);
        assert_eq!(m.vrp_counters().entries, 1);
        assert_eq!(ctx.orders.len(), 2);
        // The hedge itself is inside the single-order cap.
        let h = ctx.orders[1];
        let caps = strategy_core::caps_for_sym(perp_sym());
        assert!(
            VrpStrategy::size_ok(caps, h.qty.raw(), VrpStrategy::notional_1e6(h.px.raw(), h.qty.raw())),
            "hedge of {} over the cap",
            h.qty.raw()
        );
    }

    #[test]
    fn a_cap_never_blocks_an_exit() {
        // A cap must never be the reason a position cannot be closed.
        // Drive the hedge to a size the cap would refuse to GROW to,
        // then confirm the unwind still goes out.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_ne!(m.perp_pos_qty_1e6(), 0);
        ctx.orders.clear();
        // Y1: the unwind happens at SETTLEMENT, not at E−ε. E−ε only
        // freezes the hedge, so the position is still on there.
        let eps = EXPIRY - params.epsilon_ns;
        ctx.now = mono_of(eps);
        m.on_tick(&tick(eps, 79_000_000_000, false), &mut ctx);
        assert_ne!(m.perp_pos_qty_1e6(), 0, "still hedged into expiry");
        ctx.now = mono_of(EXPIRY);
        m.on_tick(&tick(EXPIRY, 79_000_000_000, false), &mut ctx);
        assert_eq!(m.perp_pos_qty_1e6(), 0, "the hedge unwound at settlement");
        assert_eq!(m.opt_pos_qty_1e6(), 0);
    }

    // ---------------- kill criterion 3 ----------------

    #[test]
    fn a_full_window_without_the_edge_halts_the_member_for_good() {
        // Edge spec §5.3: "HAR no longer beats IV out of sample at 4–8 h
        // over a trailing 60 expiries ⇒ halt". Not warn — halt, because
        // E1 is the entire mechanism. (The window is per-process today;
        // see `VrpStrategy::killed` and the V8 precondition.)
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        assert!(!m.is_killed());

        // Settle a full window in which the IV was the better forecast
        // every time: quote an implied vol AT the realisation while the
        // member's own fit is far away.
        let mut settled = 0usize;
        while settled < core_vol::QLIKE_RING {
            let rv = 1_200_000_000_000i64;
            let iv = (rv as i128 * 33_102_114_736i128 / 10_000_000_000_000i128) as i64;
            m.vol.arm_hold(TAU, iv).expect("armed");
            m.vol.observe_settlement(rv);
            m.refresh_qlike();
            settled += 1;
        }
        assert_eq!(m.vrp_counters().qlike_har_beats_iv, 0);
        assert_eq!(m.vrp_counters().killed, 1);
        assert!(m.is_killed(), "a full window without the edge halts");

        // And the halt REFUSES entries, whatever the quote says.
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.vrp_counters().entries, 0);
        assert!(ctx.orders.is_empty());
        assert!(m.is_killed(), "nothing clears it inside this process");
    }

    // ---------------- VX: European cash settlement ----------------

    #[test]
    fn intrinsic_is_the_european_payoff_and_never_negative() {
        // A call is worth S − K when that is positive and nothing when
        // it is not; a put is the mirror. There is no time value at
        // expiry, which is the whole point of settling rather than
        // marking.
        let k = 79_000_000_000i64;
        assert_eq!(VrpStrategy::intrinsic_1e6(80_000_000_000, k, RIGHT_CALL), 1_000_000_000);
        assert_eq!(VrpStrategy::intrinsic_1e6(78_000_000_000, k, RIGHT_CALL), 0);
        assert_eq!(VrpStrategy::intrinsic_1e6(k, k, RIGHT_CALL), 0, "ATM is worthless");
        assert_eq!(VrpStrategy::intrinsic_1e6(78_000_000_000, k, RIGHT_PUT), 1_000_000_000);
        assert_eq!(VrpStrategy::intrinsic_1e6(80_000_000_000, k, RIGHT_PUT), 0);
        assert_eq!(VrpStrategy::intrinsic_1e6(k, k, RIGHT_PUT), 0);
    }

    /// Drive a campaign to a held position and then jump straight past
    /// expiry without ever passing E−ε — the "engine was down / the
    /// lane went quiet" path the settle rung exists for.
    fn campaign_at_expiry(ctx: &mut RecCtx, params: VrpParams) -> VrpStrategy {
        let (mut m, _) = member(ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), ctx);
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), ctx);
        assert_eq!(m.opt_pos_qty_1e6(), -params.qty_1e6, "the campaign is open");
        ctx.orders.clear();
        m
    }

    #[test]
    fn an_in_the_money_expiry_settles_at_intrinsic() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let mut m = campaign_at_expiry(&mut ctx, params);
        // The index closes $1,500 above the 79,000 strike.
        let s_1e6 = 80_500_000_000i64;
        ctx.now = mono_of(EXPIRY);
        let mut o = summary(EXPIRY, opt_sym(4), 5_000_000_000, 500_000_000);
        o.underlying_px_1e9 = s_1e6.saturating_mul(1_000);
        m.on_opt_summary(&o, &mut ctx);

        assert_eq!(m.vrp_counters().settled_itm, 1);
        assert_eq!(m.vrp_counters().settled_otm, 0);
        assert_eq!(ctx.orders.len(), 2, "the option's cash value and the hedge");
        // The option leg books max(0, S − K) = $1,500, not a mark.
        assert_eq!(ctx.orders[0].sym, opt_sym(4));
        assert_eq!(ctx.orders[0].px.raw(), 1_500_000_000);
        assert_eq!(ctx.orders[0].side, Side::Bid, "buying back the short");
        assert_eq!(ctx.orders[0].qty.raw(), params.qty_1e6);
        assert_eq!(ctx.orders[1].sym, perp_sym());
        assert_eq!(m.opt_pos_qty_1e6(), 0);
        assert_eq!(m.perp_pos_qty_1e6(), 0);
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE, "the campaign ended");
        // And the hold still fed the forecast.
        assert_eq!(m.vrp_counters().settlements, 1);
    }

    #[test]
    fn an_out_of_the_money_expiry_emits_no_order_at_all() {
        // The option is worth nothing and the venue charges nothing for
        // an OTM expiry, so there is no fill to price. A zero-priced
        // order would be a fiction and a mark-priced one would book
        // value that expired.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let mut m = campaign_at_expiry(&mut ctx, params);
        let s_1e6 = 77_500_000_000i64; // below the 79,000 strike
        ctx.now = mono_of(EXPIRY);
        let mut o = summary(EXPIRY, opt_sym(4), 5_000_000_000, 500_000_000);
        o.underlying_px_1e9 = s_1e6.saturating_mul(1_000);
        m.on_opt_summary(&o, &mut ctx);

        assert_eq!(m.vrp_counters().settled_otm, 1);
        assert_eq!(m.vrp_counters().settled_itm, 0);
        assert_eq!(ctx.orders.len(), 1, "ONLY the hedge unwinds");
        assert_eq!(ctx.orders[0].sym, perp_sym());
        assert_eq!(m.opt_pos_qty_1e6(), 0, "the option expired worthless");
        assert_eq!(m.perp_pos_qty_1e6(), 0);
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE);
    }

    #[test]
    fn an_at_the_money_expiry_is_out_of_the_money() {
        // S == K pays nothing. The boundary is worth pinning: a `>=`
        // here would book a zero-priced order every time the index
        // closes exactly on the strike.
        let mut ctx = RecCtx::new();
        let mut m = campaign_at_expiry(&mut ctx, VrpParams::default());
        ctx.now = mono_of(EXPIRY);
        let mut o = summary(EXPIRY, opt_sym(4), 5_000_000_000, 500_000_000);
        o.underlying_px_1e9 = 79_000_000_000i64.saturating_mul(1_000);
        m.on_opt_summary(&o, &mut ctx);
        assert_eq!(m.vrp_counters().settled_otm, 1);
        assert_eq!(ctx.orders.len(), 1, "hedge only");
    }

    #[test]
    fn an_expiry_that_passed_unseen_settles_on_the_next_record() {
        // The E−ε instant went by with no record at all — a data gap,
        // or the process was elsewhere. The next record after expiry is
        // an underlying TICK, not an option summary, so the settle rung
        // has to work off the tick lane's own mid.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let mut m = campaign_at_expiry(&mut ctx, params);
        let s_1e6 = 81_000_000_000i64;
        let late = EXPIRY + 6 * 3_600_000_000_000; // six hours late
        ctx.now = mono_of(late);
        m.on_tick(&tick(late, s_1e6, false), &mut ctx);

        assert_eq!(m.vrp_counters().settled_itm, 1, "settled on a TICK");
        assert_eq!(ctx.orders.len(), 2);
        // Priced off the FRESHER of the two index sources: the tick
        // that arrived six hours after the option lane went quiet.
        // 81,000 − 79,000 = $2,000.
        assert_eq!(ctx.orders[0].px.raw(), 2_000_000_000);
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE);
    }

    #[test]
    fn an_expiry_with_no_index_defers_rather_than_inventing_one() {
        // A settlement priced off a number the member made up is worse
        // than a late one.
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
        // Force a campaign into existence with a position but no index:
        // hand-set the state the way a restart-with-persistence would.
        m.selected_sym = opt_sym(4);
        m.expiry_ns = EXPIRY;
        m.opt_pos_qty_1e6 = -params.qty_1e6;
        ctx.now = mono_of(EXPIRY + 1);
        let before = m.vrp_counters().stale_skips;
        assert!(!m.maybe_settle(&mut ctx, EXPIRY + 1, mono_of(EXPIRY + 1)));
        assert_eq!(m.vrp_counters().stale_skips, before + 1);
        assert_eq!(m.opt_pos_qty_1e6(), -params.qty_1e6, "still held");
        assert!(ctx.orders.is_empty());
        assert_ne!(m.selected_sym(), SYMBOL_ID_NONE, "campaign still open");
    }

    #[test]
    fn a_flat_campaign_at_expiry_just_ends() {
        // Y1: a campaign that decided and HELD carries no position, so
        // expiry is pure bookkeeping — it closes the campaign and must
        // not emit an order for something that is not there.
        //
        // (Under the old E−ε law this case was reached by unwinding
        // first. There is no unwind now, so the flat campaign has to be
        // a genuinely flat one.)
        let mut ctx = RecCtx::new();
        let mut m = fresh_member(&mut ctx);
        let held = format!("V\t3\nC\t{EXPIRY}\t79000000000\t0\t0\t0\t0\t0\t1\n");
        m.restore_state(&held).expect("restores");
        assert_eq!(m.opt_pos_qty_1e6(), 0);
        assert_eq!(m.perp_pos_qty_1e6(), 0);

        let eps = EXPIRY - VrpParams::default().epsilon_ns;
        ctx.now = mono_of(eps);
        m.on_tick(&tick(eps, 79_000_000_000, false), &mut ctx);
        assert_ne!(
            m.selected_sym(),
            SYMBOL_ID_NONE,
            "E−ε freezes the hedge; it does NOT end the campaign"
        );
        ctx.orders.clear();
        ctx.now = mono_of(EXPIRY);
        m.on_tick(&tick(EXPIRY, 79_000_000_000, false), &mut ctx);
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE, "settlement ended it");
        assert!(ctx.orders.is_empty(), "nothing held ⇒ nothing to trade");
        assert_eq!(m.vrp_counters().settled_itm, 0);
        assert_eq!(m.vrp_counters().settled_otm, 0);
    }

    // ---------------- V8a: persisted state ----------------

    fn fresh_member(ctx: &mut RecCtx) -> VrpStrategy {
        let mut m = VrpStrategy::new();
        m.configure(
            VrpParams::default(),
            registry(),
            perp_sym(),
            perp_sym(),
            WallAnchor::new(MONO0, WALL0),
            [7u8; 32],
        )
        .expect("configure");
        let _ = ctx;
        m
    }

    #[test]
    fn a_live_campaign_round_trips_through_the_state_file() {
        // The defect this exists to prevent: a restart in the middle of
        // an 8 h hold. The next boot knows nothing about the position,
        // never hedges it again and never settles it — the member is
        // flat in its own head while the venue says otherwise.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let m = campaign_at_expiry(&mut ctx, params);
        // Push the forecast forward so there is a QLIKE row too.
        let mut text = String::new();
        assert!(m.render_state(&mut text));
        assert!(text.contains("V\t3"), "{text}");

        let mut boot = fresh_member(&mut ctx);
        let r = boot.restore_state(&text).expect("restores");
        // W2: `restore_state` validates and COUNTS the `R` rows; the
        // boot path replays them, because the rolling window is the one
        // thing with two sources and reconciling them needs both series
        // in hand. A round trip therefore has to do what the boot does.
        assert_eq!(boot.seed_returns(&r_rows(&text)), r.returns);
        assert!(r.campaign, "the campaign came back");
        assert!(r.campaign_resolved, "and the chain still carries it");
        assert_eq!(r.pairs, m.n_pairs());
        assert_eq!(boot.n_pairs(), m.n_pairs());
        assert_eq!(boot.selected_sym(), m.selected_sym());
        assert_eq!(boot.opt_pos_qty_1e6(), m.opt_pos_qty_1e6());
        assert_eq!(boot.perp_pos_qty_1e6(), m.perp_pos_qty_1e6());
        assert_eq!(boot.side(), m.side());
        assert_eq!(boot.expiry_ns, m.expiry_ns);
        assert_eq!(boot.selected_strike_1e6, m.selected_strike_1e6);
        // And the restored member renders the SAME state — a round trip
        // that drifted would compound at every restart.
        let mut again = String::new();
        assert!(boot.render_state(&mut again));
        assert_eq!(again, text, "render → restore → render must be a fixpoint");
    }

    #[test]
    fn a_restored_campaign_settles_even_when_its_contract_is_gone() {
        // The engine-down-across-expiry path, and the reason the
        // campaign is keyed by (expiry, strike, right) rather than by
        // SymbolId: option ordinals reshuffle at every boot, and by the
        // time the process comes back the expiry may have rolled off the
        // chain entirely. The persisted strike and right are enough to
        // settle it.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let m = campaign_at_expiry(&mut ctx, params);
        let mut text = String::new();
        assert!(m.render_state(&mut text));

        // A boot whose chain no longer holds that expiry at all.
        let mut boot = VrpStrategy::new();
        let mut reg = OptRegistry::new();
        reg.insert(OptInstrument::new(
            opt_sym(30),
            perp_sym(),
            VenueId::Deribit as u8,
            EXPIRY + 7 * 86_400_000_000_000_000 / 1_000,
            79_000_000_000,
            RIGHT_CALL,
            1_000_000_000,
        ))
        .expect("next week's chain");
        boot.configure(
            params,
            reg,
            perp_sym(),
            perp_sym(),
            WallAnchor::new(MONO0, WALL0),
            [0; 32],
        )
        .expect("configure");
        let r = boot.restore_state(&text).expect("restores");
        assert!(r.campaign);
        assert!(!r.campaign_resolved, "the contract is gone from the chain");
        assert_eq!(boot.selected_sym(), SYMBOL_ID_NONE);
        assert_eq!(boot.opt_pos_qty_1e6(), m.opt_pos_qty_1e6(), "still held");

        // The first tick after expiry closes it. The option leg cannot
        // be PRICED — there is no symbol to submit against, and the
        // persisted one names a different instrument on this boot — so
        // it is counted as a reconciliation item rather than booked at a
        // number nobody can trade. The hedge, which trades a perp whose
        // sym IS stable, unwinds normally.
        let late = EXPIRY + 3_600_000_000_000;
        ctx.orders.clear();
        ctx.now = mono_of(late);
        boot.on_tick(&tick(late, 81_000_000_000, false), &mut ctx);
        assert_eq!(boot.vrp_counters().settled_unpriced, 1);
        assert_eq!(boot.vrp_counters().settled_itm, 0);
        assert_eq!(ctx.orders.len(), 1, "the hedge only");
        assert_eq!(ctx.orders[0].sym, perp_sym());
        assert_eq!(boot.opt_pos_qty_1e6(), 0);
        assert_eq!(boot.perp_pos_qty_1e6(), 0);
        assert_eq!(boot.selected_sym(), SYMBOL_ID_NONE);
        assert_eq!(boot.expiry_ns, 0, "the campaign ended");
    }

    #[test]
    fn a_restored_campaign_settles_normally_when_its_contract_is_still_there() {
        // The common case: a restart between the entry and the expiry.
        // The contract is still in the chain, so the settle rung prices
        // it exactly as it would have without the restart.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let m = campaign_at_expiry(&mut ctx, params);
        let mut text = String::new();
        assert!(m.render_state(&mut text));

        let mut boot = fresh_member(&mut ctx);
        let r = boot.restore_state(&text).expect("restores");
        assert!(r.campaign_resolved);
        ctx.orders.clear();
        ctx.now = mono_of(EXPIRY);
        boot.on_tick(&tick(EXPIRY, 81_000_000_000, false), &mut ctx);
        assert_eq!(boot.vrp_counters().settled_itm, 1);
        assert_eq!(boot.vrp_counters().settled_unpriced, 0);
        assert_eq!(ctx.orders.len(), 2);
        assert_eq!(ctx.orders[0].px.raw(), 2_000_000_000, "81,000 − 79,000");
    }

    #[test]
    fn the_kill_survives_a_restart() {
        // Kill criterion 3 is measured over SIXTY settled expiries —
        // twenty days at an 8 h campaign, across every restart in
        // between. A window that starts empty at every boot can never
        // fill, so the halt it feeds could never arm. This is the test
        // that says it now can.
        let mut ctx = RecCtx::new();
        let mut m = fresh_member(&mut ctx);
        let mut i = 0usize;
        while i < core_vol::QLIKE_RING {
            m.vol.seed_qlike(1_000_000, 900_000_000); // IV wins every time
            i += 1;
        }
        m.refresh_qlike();
        assert!(m.is_killed());
        let mut text = String::new();
        assert!(m.render_state(&mut text));
        assert!(text.contains("K\t1"), "{text}");

        let mut boot = fresh_member(&mut ctx);
        assert!(!boot.is_killed(), "a fresh member is not halted");
        let r = boot.restore_state(&text).expect("restores");
        assert!(r.killed);
        assert_eq!(r.qlike, core_vol::QLIKE_RING);
        assert!(boot.is_killed(), "the halt survived the restart");
        assert_eq!(boot.vrp_counters().qlike_har_beats_iv, 0);
    }

    #[test]
    fn a_malformed_state_file_refuses_rather_than_guessing() {
        // A state file the engine cannot read EXACTLY is a position
        // nobody is tracking. Every one of these is a boot refusal.
        let mut ctx = RecCtx::new();
        for bad in [
            "P\t1\t2\t3\n",                              // no version row
            "V\t4\n",                                      // a version we do not know
            "V\t0\n",                                      // nor is 0 a version
            "V\t1\nZ\t1\n",                               // an unknown tag
            "V\t1\nP\t1\t2\n",                           // a short pair row
            "V\t1\nP\t1\tx\t3\n",                       // not an integer
            "V\t1\nP\t-1\t2\t3\n",                      // a negative expiry stamp
            "V\t1\nC\t0\t1\t0\t1\t1\t1\t1\n",        // expiry 0
            "V\t1\nC\t1\t0\t0\t1\t1\t1\t1\n",        // strike 0
            "V\t1\nC\t1\t1\t9\t1\t1\t1\t1\n",        // right is neither
            "V\t1\nC\t1\t1\t0\t7\t1\t1\t1\n",        // side out of range
        ] {
            let mut m = fresh_member(&mut ctx);
            assert!(
                m.restore_state(bad).is_err(),
                "must refuse {bad:?} — a state file the engine cannot read exactly \
                 is a position nobody is tracking"
            );
        }
        // Comments and blank lines are skipped, and a bare version row
        // is a legal empty history.
        let mut m = fresh_member(&mut ctx);
        let r = m.restore_state("# hello\n\nV\t1\n").expect("legal");
        assert_eq!(r.pairs, 0);
        assert!(!r.campaign);
        assert!(!r.killed);
    }

    #[test]
    fn the_state_epoch_moves_only_when_something_persistable_happens() {
        // The cli writes the file when this moves and never otherwise.
        //
        // W2 CHANGED THIS LAW, deliberately. A minute close is now
        // persistable, because the rolling window is persisted state —
        // if the epoch stayed put through a minute roll, the `R` rows
        // would only ever reach disk when a campaign happened to move
        // it, which is a few times a day, and the whole warm-up fix
        // would be inert. The write is once a minute on the
        // observability cadence, off the tick path.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, mut wall) = member(&mut ctx, params);
        // `member` leaves `wall` one minute past its last tick, so the
        // first tick here closes that minute. Absorb it, THEN measure.
        m.on_tick(&tick(wall, 79_000_000_000, false), &mut ctx);
        let quiet = m.state_epoch();
        // Many ticks INSIDE one minute are still not state.
        let mut i = 0usize;
        while i < 100 {
            m.on_tick(&tick(wall, 79_000_000_000 + i as i64, false), &mut ctx);
            i += 1;
        }
        assert_eq!(m.state_epoch(), quiet, "ticks inside a minute are not state");

        // Crossing a minute boundary IS.
        let before_minutes = m.vol_minutes();
        wall += MINUTE_NS;
        m.on_tick(&tick(wall, 79_000_000_000, false), &mut ctx);
        assert_eq!(m.vol_minutes(), before_minutes + 1, "a return was pushed");
        assert!(m.state_epoch() > quiet, "a minute close is state (W2)");
        let quiet = m.state_epoch();

        // Advance FIRST, so every one of these ticks crosses a
        // boundary and the count is exact.
        let mut i = 0usize;
        while i < 1_000 {
            wall += MINUTE_NS;
            m.on_tick(&tick(wall, 79_000_000_000, false), &mut ctx);
            i += 1;
        }
        assert_eq!(
            m.state_epoch(),
            quiet + 1_000,
            "exactly one epoch per minute closed — no more, no less"
        );
        let quiet = m.state_epoch();

        // Selecting IS.
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        assert!(m.state_epoch() > quiet, "a selection is state");
        let after_select = m.state_epoch();

        // So is an entry, and so is a hedge.
        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert!(m.state_epoch() > after_select + 1, "entry AND hedge");
    }


    // ---------------------------------------------------------------
    // W2 — the rolling window has to survive a restart
    // ---------------------------------------------------------------

    /// Pull the `R` rows back out of a rendered state file, the way
    /// `cli::vrp_boot` does through `core_config::vrp::parse_returns`.
    /// Spelled out here so this crate stays free of that dependency.
    fn r_rows(text: &str) -> Vec<(u64, i64)> {
        let mut out = Vec::new();
        for line in text.lines() {
            let mut f = line.split('\t');
            if f.next() != Some("R") {
                continue;
            }
            let ts: u64 = f.next().expect("ts").parse().expect("ts int");
            let r: i64 = f.next().expect("r").parse().expect("r int");
            assert!(f.next().is_none(), "an R row has exactly two fields");
            out.push((ts, r));
        }
        out
    }

    // ---------------------------------------------------------------
    // W6 — a decision is spent once, and only near E−τ
    // ---------------------------------------------------------------

    /// A campaign whose decision instant is reached LATE is spent
    /// unused, not taken.
    ///
    /// `τ` is the horizon, not a start line: `bounds` is a variance
    /// forecast for a τ-long hold and the edge was measured on one.
    /// On 2026-09-11 a restart re-reached this campaign at 06:01Z
    /// against an 08:00Z expiry and entered — a 1 h 56 m hold gated by
    /// an 8 h forecast.
    #[test]
    fn a_late_decision_is_spent_not_taken() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);

        // Select inside the proper window.
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
        assert_ne!(m.selected_sym(), SYMBOL_ID_NONE, "selected");

        // Then come back one second past the deadline.
        let late = EXPIRY - TAU + params.selection_ns + 1_000_000_000;
        ctx.now = mono_of(late);
        let orders_before = ctx.orders.len();
        m.on_opt_summary(&summary(late, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);

        assert_eq!(m.vrp_counters().decisions_late, 1, "counted as late");
        assert_eq!(m.vrp_counters().decisions, 0, "and never decided");
        assert_eq!(m.vrp_counters().entries, 0, "no position on a stale horizon");
        assert_eq!(ctx.orders.len(), orders_before, "and no order at all");

        // SPENT, not deferred: further records must not retry it, or
        // every one of them is the same wrong trade.
        let later = late + 60_000_000_000;
        ctx.now = mono_of(later);
        m.on_opt_summary(&summary(later, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.vrp_counters().decisions_late, 1, "counted once, not per record");
        assert_eq!(m.vrp_counters().entries, 0);
    }

    /// Just inside the deadline still trades — the bound is a grace
    /// window for a restart, not a new refusal.
    #[test]
    fn a_decision_just_inside_the_deadline_is_still_taken() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);

        let edge = EXPIRY - TAU + params.selection_ns;
        ctx.now = mono_of(edge);
        m.on_opt_summary(&summary(edge, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.vrp_counters().decisions_late, 0, "on the line is inside it");
        assert_eq!(m.vrp_counters().decisions, 1, "and it decided");
    }

    /// THE W6 state test. A campaign that decided and HELD carries no
    /// position, so v2 had nothing to derive `entry_done` from and the
    /// next boot decided it all over again. v3 persists the flag.
    #[test]
    fn a_spent_decision_survives_a_restart() {
        let mut ctx = RecCtx::new();
        // A held campaign: selected, decided, no position.
        let v3 = format!("V\t3\nC\t{EXPIRY}\t79000000000\t0\t0\t0\t0\t0\t1\n");
        let mut m = fresh_member(&mut ctx);
        let r = m.restore_state(&v3).expect("restores");
        assert!(r.campaign);

        let entry = EXPIRY - TAU;
        ctx.now = mono_of(entry);
        m.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(m.vrp_counters().decisions, 0, "the decision was already spent");
        assert_eq!(m.vrp_counters().entries, 0, "so no position is opened");

        // The v2 shape — seven fields, flag derived — is what produced
        // the live defect, and it still loads for readability.
        let v2 = format!("V\t2\nC\t{EXPIRY}\t79000000000\t0\t0\t0\t0\t0\n");
        let mut old = fresh_member(&mut ctx);
        old.restore_state(&v2).expect("v2 still readable");
        ctx.now = mono_of(entry);
        old.on_opt_summary(&summary(entry, opt_sym(4), 5_000_000_000, 500_000_000), &mut ctx);
        assert_eq!(
            old.vrp_counters().decisions,
            1,
            "v2 re-decides — this is the defect v3 exists to end"
        );
    }

    /// THE W2 test. Warm a member the slow way, write its state, and
    /// boot a fresh one from it — the second member is warm IMMEDIATELY,
    /// with the same forecast.
    ///
    /// Before W2 the second member started at `minutes = 0` against a
    /// 24 h warm-up, and the restart lane never left it 24 h. That is
    /// not a hypothetical: it is exactly what the 2026-09-11 campaign
    /// did — `decisions=1 entries=0 holds=0 no_bounds=1`.
    #[test]
    fn a_restart_keeps_the_forecast_warm() {
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, mut wall) = member(&mut ctx, params);

        let mut px = 79_000_000_000i64;
        let mut s = 20_260_911i64;
        let mut i = 0usize;
        while i < 1_500 {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
            m.on_tick(&tick(wall, px, false), &mut ctx);
            wall += MINUTE_NS;
            i += 1;
        }
        assert!(m.vol_is_warm(), "the slow way works — it just takes 24 h");
        assert_eq!(m.vol_short_by(), 0);

        let mut text = String::new();
        assert!(m.render_state(&mut text));
        let rows = r_rows(&text);
        // RETAINED, not observed: the ring holds MINUTE_RING at most,
        // and `member()` already walked it past that before this test
        // added 1_500 more.
        assert_eq!(rows.len(), core_vol::MINUTE_RING, "every retained minute");
        assert!(m.vol_minutes() as usize > rows.len(), "the ring wrapped");
        // Stamped and contiguous — the merge depends on both.
        let mut j = 1usize;
        while j < rows.len() {
            assert_eq!(rows[j].0 - rows[j - 1].0, 60_000, "contiguous at row {j}");
            j += 1;
        }
        assert_eq!(rows[rows.len() - 1].0, m.vol_last_min_ts_ms());

        // The restart. `fresh_member` is what a boot actually looks
        // like: configured, and knowing nothing.
        let mut boot = fresh_member(&mut ctx);
        assert!(!boot.vol_is_warm(), "a fresh member is cold — that is the defect");
        assert_eq!(boot.vol_short_by(), core_vol::HAR_WARM_MINUTES);

        let restored = boot.restore_state(&text).expect("restores");
        assert_eq!(restored.returns, rows.len(), "counted, not replayed");
        assert!(!boot.vol_is_warm(), "restore_state does NOT replay them");
        assert_eq!(boot.seed_returns(&rows), rows.len());

        assert!(boot.vol_is_warm(), "warm on the FIRST tick after a restart");
        // The restored member has only what the ring carried, which is
        // the point: the forecast reads at most the last 1440 returns,
        // so a wrapped ring is a COMPLETE restore of everything that
        // can affect it.
        assert_eq!(boot.vol_minutes() as usize, rows.len());
        assert_eq!(boot.vol_last_min_ts_ms(), m.vol_last_min_ts_ms());
        // And it is the same forecast, not merely a warm one.
        assert_eq!(
            boot.vol.har_1e9(params.tau_ns),
            m.vol.har_1e9(params.tau_ns),
            "identical forecast"
        );
        assert_eq!(
            boot.vol.x_1e9(params.tau_ns),
            m.vol.x_1e9(params.tau_ns),
            "identical regressor"
        );
    }

    /// A v1 file — every file written before 2026-09-11 — still loads.
    /// It simply carries no window, which is what v1 always meant.
    #[test]
    fn a_v1_state_file_still_loads_as_a_cold_window() {
        let mut ctx = RecCtx::new();
        let mut m = fresh_member(&mut ctx);
        let r = m
            .restore_state("V\t1\nP\t1789000000000\t1000\t2000\n")
            .expect("v1 is readable");
        assert_eq!(r.pairs, 1);
        assert_eq!(r.returns, 0);
        assert!(!m.vol_is_warm());
    }

    /// A file from a FUTURE writer is refused rather than half-read.
    /// That is the whole reason the version moved: a binary rolled back
    /// below W2 must not silently read a window it cannot account for.
    #[test]
    fn a_future_state_version_is_refused() {
        let mut ctx = RecCtx::new();
        let mut m = fresh_member(&mut ctx);
        assert!(m.restore_state("V\t4\n").is_err(), "v4 is not readable here");
        assert!(m.restore_state("V\t0\n").is_err(), "nor is a nonsense version");
        assert!(m.restore_state("V\t3\n").is_ok(), "v3 is this binary's own");
        assert!(m.restore_state("V\t2\n").is_ok(), "and it still reads v2");
    }

    /// Out-of-order `R` rows are refused. The ring's eviction arm
    /// assumes chronological order, so a shuffled file does not produce
    /// a cosmetically odd window — it produces a DIFFERENT one, quietly.
    #[test]
    fn out_of_order_returns_are_refused() {
        let mut ctx = RecCtx::new();
        let mut m = fresh_member(&mut ctx);
        assert!(m
            .restore_state("V\t3\nR\t1789000060000\t10\nR\t1789000000000\t20\n")
            .is_err());
        assert!(m.restore_state("V\t3\nR\t0\t10\n").is_err(), "a 0 stamp is not a minute");
    }

    #[test]
    fn no_selection_counts_expiries_not_records() {
        // For ~23 h 50 m of every day no expiry is inside the selection
        // window, and that is the member working. Counting a "no
        // selection" on every option record in between runs the counter
        // into the millions and buries the one case that matters.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let (mut m, _) = member(&mut ctx, params);

        // Far from any expiry: a hundred records, no counter movement.
        let quiet = EXPIRY - 10 * 3_600_000_000_000; // 10 h out
        let mut i = 0usize;
        while i < 100 {
            ctx.now = mono_of(quiet);
            m.on_opt_summary(&summary(quiet, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);
            i += 1;
        }
        assert_eq!(m.vrp_counters().no_selection, 0, "a quiet market is not a failure");
        assert_eq!(m.selected_sym(), SYMBOL_ID_NONE);

        // An expiry IS due, but the chain carries nothing we may trade —
        // every row belongs to another underlying. THAT counts.
        let mut foreign = VrpStrategy::new();
        let mut reg = OptRegistry::new();
        let eth_perp = make_symbol_id(VenueId::Deribit, 2);
        let mut k = 0u32;
        while k < 4 {
            reg.insert(OptInstrument::new(
                opt_sym(40 + k),
                eth_perp,
                VenueId::Deribit as u8,
                EXPIRY,
                (3_000 + 100 * k as i64) * 1_000_000,
                RIGHT_CALL,
                1_000_000_000,
            ))
            .expect("eth chain");
            k += 1;
        }
        foreign
            .configure(
                params,
                reg,
                perp_sym(),
                perp_sym(),
                WallAnchor::new(MONO0, WALL0),
                [0; 32],
            )
            .expect("configure");
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        foreign.on_opt_summary(&summary(sel, opt_sym(40), 700_000_000, 500_000_000), &mut ctx);
        assert_eq!(foreign.vrp_counters().no_selection, 1, "an expiry rolled without us");
        assert_eq!(foreign.selected_sym(), SYMBOL_ID_NONE);
    }

    #[test]
    fn another_currencys_option_is_never_selected() {
        // The live ladder is on for BTC AND ETH. Without the underlying
        // filter the member selects an ETH call — it expires sooner, and
        // "nearest expiry" is the first tie-break — and then hedges it
        // with the BTC perp. That is a cross-asset naked position, and
        // no size, staleness or timing check in this crate would catch
        // it, because none of them is about WHAT the instrument is.
        let mut ctx = RecCtx::new();
        let params = VrpParams::default();
        let mut m = VrpStrategy::new();
        m.configure(
            params,
            registry_with_foreign(true),
            perp_sym(),
            perp_sym(),
            WallAnchor::new(MONO0, WALL0),
            [0; 32],
        )
        .expect("configure");
        // Warm enough to select; the decision itself is not the point.
        let mut wall = WALL0;
        let mut i = 0usize;
        while i < 1_442 {
            m.on_tick(&tick(wall, 79_000_000_000, false), &mut ctx);
            wall += MINUTE_NS;
            i += 1;
        }
        let sel = EXPIRY - TAU - params.selection_ns / 2;
        ctx.now = mono_of(sel);
        m.on_opt_summary(&summary(sel, opt_sym(4), 700_000_000, 500_000_000), &mut ctx);

        let picked = m.selected_sym();
        assert_ne!(picked, SYMBOL_ID_NONE, "a BTC call is still selectable");
        let row = m.registry.get(picked).expect("registered");
        assert_eq!(
            row.underlying_sym,
            perp_sym(),
            "the selected option's own hedge leg must BE our hedge leg"
        );
        assert_eq!(row.expiry_ns, EXPIRY, "not the ETH row's earlier expiry");
        // Prove the ETH row really was a candidate: the same law with
        // the underlying filter removed would have taken it, because
        // nearest expiry is the first tie-break.
        let eth = m
            .registry
            .rows()
            .iter()
            .find(|r| r.underlying_sym != perp_sym())
            .expect("the ETH ladder is in the table");
        assert!(eth.expiry_ns < row.expiry_ns, "and it expires sooner");
        let lead = eth.expiry_ns - sel;
        assert!(
            lead >= params.tau_ns && lead <= params.tau_ns + params.selection_ns,
            "inside the selection window, so nothing else would have refused it"
        );
        assert_eq!(row.strike_1e6, 79_000_000_000, "the BTC ATM strike");
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

        // Y1: the terminal rung is SETTLEMENT, so that is where a full
        // ring has to be survived. Expiry arrives with it full.
        ctx.full = true;
        ctx.orders.clear();
        ctx.now = mono_of(EXPIRY);
        m.on_tick(&tick(EXPIRY, 79_000_000_000, false), &mut ctx);
        assert_ne!(m.selected_sym(), SYMBOL_ID_NONE, "campaign still open");
        assert!(m.orders_dropped() > 0);
        assert!(
            m.opt_pos_qty_1e6() != 0 || m.perp_pos_qty_1e6() != 0,
            "a refused submit must NOT be taken as a closed position"
        );

        // The ring drains; the very next record completes the settlement.
        ctx.full = false;
        m.on_tick(&tick(EXPIRY + 1_000_000_000, 79_000_000_000, false), &mut ctx);
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
